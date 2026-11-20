//! 保护根白名单与唯一删除闸门。
//!
//! 这是整个存储模块的安全底线（风险 R1「误删」）。判据分五条：
//! - **P1** 保护根：`store_dir()` / `data_root()` / 账号个人档 / `binaries` / `plugins` /
//!   `app` / `storage` —— 这些根**自身**一律拒绝删除（只匹配自身，不牵连后代，
//!   否则 `~/.workbuddy` 的整棵子树都会被误判）。
//! - **P2** `blobs`：只允许删单个文件，整目录删会丢掉所有附件。
//! - **P4** 符号链接：一律拒绝（防目录穿越）。
//! - **P5** 路径穿越：由 [`ensure_within_root`] 提供，调用方传入目标所属根。
//! - **P3** 客户端运行中：由 [`ensure_variant_idle`] 提供，属分类语义，不在此处。
//!
//! 另外把「工具备份」与三个账号库文件列为不可删：删掉即丢全部账号与回滚能力。

use std::path::{Path, PathBuf};

use crate::modules::config;
use crate::modules::variant::WbVariant;

use super::roots::{data_root_at, store_root_at};

/// 目标条目类型。删除判据必须知道这一点：同一路径「删目录」与「删其中的单个文件」
/// 的安全等级完全不同（见 P2 `blobs`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Dir,
    File,
}

impl EntryKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
        }
    }

    /// 由实际文件系统状态判定（拒绝符号链接之后才可用）。
    pub fn of_path(path: &Path) -> Option<Self> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if meta.is_dir() {
            Some(Self::Dir)
        } else if meta.is_file() {
            Some(Self::File)
        } else {
            None
        }
    }
}

/// 一条保护根（含原因，前端与日志都直接展示这句）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedRoot {
    pub path: PathBuf,
    pub reason: &'static str,
}

const REASON_TOOL_ROOT: &str = "工具存储根，禁止整目录删除";
const REASON_DATA_ROOT: &str = "客户端数据根，禁止整目录删除";
const REASON_PERSONAL: &str = "账号个人档目录，禁止整目录删除";
const REASON_BINARIES: &str = "客户端可执行组件目录，禁止整目录删除";
const REASON_PLUGINS: &str = "客户端插件目录，禁止整目录删除";
const REASON_APP: &str = "客户端应用目录，禁止整目录删除";
const REASON_STORAGE: &str = "客户端设置存储目录，禁止整目录删除";
const REASON_BLOBS: &str = "附件目录只允许逐文件删除，禁止整目录删除";
const REASON_DB: &str = "客户端会话数据库，禁止删除";
const REASON_ACCOUNTS: &str = "账号库，禁止删除";
const REASON_SESSION_LINKS: &str = "会话关联表，禁止删除";
const REASON_BACKUPS: &str = "工具备份目录，禁止删除";
const REASON_LOCK: &str = "并发锁文件，禁止删除";

/// 静态保护根清单（真实家目录）。
pub fn protected_roots() -> Vec<ProtectedRoot> {
    protected_roots_at(&config::home_dir())
}

/// [`protected_roots`] 的可测实现。
pub fn protected_roots_at(home: &Path) -> Vec<ProtectedRoot> {
    let store = store_root_at(home);
    let mut roots = vec![
        ProtectedRoot {
            path: store.clone(),
            reason: REASON_TOOL_ROOT,
        },
        ProtectedRoot {
            path: store.join("backups"),
            reason: REASON_BACKUPS,
        },
        ProtectedRoot {
            path: store.join("accounts.json"),
            reason: REASON_ACCOUNTS,
        },
        ProtectedRoot {
            path: store.join("accounts.lock"),
            reason: REASON_LOCK,
        },
        ProtectedRoot {
            path: store.join("auth-state.lock"),
            reason: REASON_LOCK,
        },
        ProtectedRoot {
            path: store.join("session_links.json"),
            reason: REASON_SESSION_LINKS,
        },
    ];
    for variant in WbVariant::ALL {
        let data = data_root_at(variant, home);
        roots.push(ProtectedRoot {
            path: data.clone(),
            reason: REASON_DATA_ROOT,
        });
        for (name, reason) in [
            ("binaries", REASON_BINARIES),
            ("plugins", REASON_PLUGINS),
            ("app", REASON_APP),
            ("storage", REASON_STORAGE),
            ("blobs", REASON_BLOBS),
        ] {
            roots.push(ProtectedRoot {
                path: data.join(name),
                reason,
            });
        }
        for name in ["workbuddy.db", "workbuddy.db-shm", "workbuddy.db-wal"] {
            roots.push(ProtectedRoot {
                path: data.join(name),
                reason: REASON_DB,
            });
        }
    }
    roots
}

/// `user-<uid>-personal` 这类按账号命名的目录（名字动态，无法写进静态表）。
fn is_personal_scope_at(home: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !(name.starts_with("user-") && name.ends_with("-personal")) {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    WbVariant::ALL.iter().any(|variant| {
        let data = data_root_at(*variant, home);
        // 实测落盘位置多一层 `storage/`；不带 `storage/` 的老布局也一并保护。
        parent == data || parent == data.join("storage")
    })
}

/// 目标是否落在工具备份目录内（`backups/**` 整棵保护）。
fn is_under_backups_at(home: &Path, path: &Path) -> bool {
    path.starts_with(store_root_at(home).join("backups"))
}

/// 目标是否落在某个档位的 `blobs/` 内（P2 判据的口径来源）。
fn is_under_blobs_at(home: &Path, path: &Path) -> bool {
    WbVariant::ALL
        .iter()
        .any(|variant| path.starts_with(data_root_at(*variant, home).join("blobs")))
}

/// `Some(reason)` = 拒绝删除；`None` = 路径形状上允许（仍须过 trash 与 P3/P5）。
pub fn is_protected(path: &Path) -> Option<&'static str> {
    is_protected_at(&config::home_dir(), path)
}

/// [`is_protected`] 的可测实现。
pub fn is_protected_at(home: &Path, path: &Path) -> Option<&'static str> {
    if let Some(root) = protected_roots_at(home)
        .into_iter()
        .find(|root| root.path == path)
    {
        return Some(root.reason);
    }
    if is_personal_scope_at(home, path) {
        return Some(REASON_PERSONAL);
    }
    if is_under_backups_at(home, path) {
        return Some(REASON_BACKUPS);
    }
    None
}

/// P5 路径穿越闸门：规范化后必须仍在 `root` 之内。
///
/// `root` 自身传进来时一律拒绝（保护根整目录删已在 P1 拦住，这里再兜一层）。
pub fn ensure_within_root(path: &Path, root: &Path) -> Result<(), String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("根路径不可用：{error}"))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|error| format!("目标路径不可用：{error}"))?;
    if canonical_path == canonical_root {
        return Err("不允许对整个根目录执行删除".to_string());
    }
    if !canonical_path.starts_with(&canonical_root) {
        return Err("目标路径不在所属根目录内，已拒绝".to_string());
    }
    Ok(())
}

/// P3 闸门：客户端运行中一律拒绝删除其数据。
pub fn ensure_variant_idle(variant: WbVariant) -> Result<(), String> {
    if crate::modules::process::is_workbuddy_running(variant) {
        return Err("请先退出 WorkBuddy 客户端".to_string());
    }
    Ok(())
}

/// 统一删除闸门：先保护判据，再按 kind 判「目录删 vs 单文件删」，返回 `Err` 即拒绝。
pub fn guard_delete(path: &Path, kind: EntryKind) -> Result<(), String> {
    guard_delete_at(&config::home_dir(), path, kind)
}

/// [`guard_delete`] 的可测实现。
pub fn guard_delete_at(home: &Path, path: &Path, kind: EntryKind) -> Result<(), String> {
    // P4：符号链接先拦（`canonicalize` 会跟随链接指向真实目标，先拦更安全）。
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => "目标不存在".to_string(),
            _ => format!("无法读取目标信息：{error}"),
        })?;
    if meta.file_type().is_symlink() {
        return Err("符号链接一律拒绝删除（防目录穿越）".to_string());
    }
    // P1：保护根自身。
    if let Some(reason) = is_protected_at(home, path) {
        return Err(reason.to_string());
    }
    // P2：`blobs/**` 只允许删单个文件。
    if kind == EntryKind::Dir && is_under_blobs_at(home, path) {
        return Err("附件目录只允许逐文件删除，禁止整目录删除".to_string());
    }
    // Parse, Don't Validate：调用方声明的 kind 必须与实际文件系统状态一致。
    match (EntryKind::of_path(path), kind) {
        (Some(actual), declared) if actual == declared => Ok(()),
        (Some(_), EntryKind::Dir) => Err("目标是文件，不能按目录删除".to_string()),
        (Some(_), EntryKind::File) => Err("目标是目录，不能按单文件删除".to_string()),
        (None, _) => Err("目标既不是普通文件也不是目录，已拒绝".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-storage-protected-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时家目录");
        dir
    }

    #[test]
    fn rejects_every_p1_root_as_directory() {
        let home = temp_home("p1");
        let store = store_root_at(&home);
        let mut targets = vec![store.clone()];
        for variant in WbVariant::ALL {
            let data = data_root_at(variant, &home);
            targets.extend([
                data.clone(),
                data.join("binaries"),
                data.join("plugins"),
                data.join("app"),
                data.join("storage"),
                data.join("blobs"),
            ]);
        }
        for target in targets {
            std::fs::create_dir_all(&target).expect("建保护根");
            assert!(
                is_protected_at(&home, &target).is_some(),
                "保护根未被识别：{}",
                target.display()
            );
            assert!(
                guard_delete_at(&home, &target, EntryKind::Dir).is_err(),
                "保护根竟然允许目录删除：{}",
                target.display()
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn blobs_directory_delete_is_rejected_but_file_allowed() {
        let home = temp_home("blobs");
        let cn = data_root_at(WbVariant::Cn, &home);
        let nested = cn.join("blobs").join("ab").join("cd");
        std::fs::create_dir_all(&nested).expect("建 blobs 子目录");
        let file = nested.join("payload.bin");
        std::fs::write(&file, b"x").expect("写附件");

        assert!(guard_delete_at(&home, &nested, EntryKind::Dir).is_err());
        assert!(guard_delete_at(&home, &cn.join("blobs"), EntryKind::Dir).is_err());
        assert!(guard_delete_at(&home, &file, EntryKind::File).is_ok());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn personal_scope_directory_is_protected() {
        let home = temp_home("personal");
        let personal = data_root_at(WbVariant::Cn, &home)
            .join("storage")
            .join("user-abc-personal");
        std::fs::create_dir_all(&personal).expect("建个人档");
        assert!(is_protected_at(&home, &personal).is_some());
        assert!(guard_delete_at(&home, &personal, EntryKind::Dir).is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn backups_tree_and_account_files_are_protected() {
        let home = temp_home("backups");
        let deep = store_root_at(&home).join("backups").join("20260101");
        std::fs::create_dir_all(&deep).expect("建备份目录");
        assert!(is_protected_at(&home, &deep).is_some());
        let accounts = store_root_at(&home).join("accounts.json");
        std::fs::write(&accounts, b"[]").expect("写账号库");
        assert_eq!(guard_delete_at(&home, &accounts, EntryKind::File), Err(REASON_ACCOUNTS.to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn symlink_is_rejected_even_when_target_is_plain() {
        let home = temp_home("symlink");
        let real = home.join("cache");
        std::fs::create_dir_all(&real).expect("建真实目录");
        let link = home.join("cache-link");
        let created = make_symlink(&real, &link);
        if created {
            assert!(guard_delete_at(&home, &link, EntryKind::Dir).is_err());
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_target_is_reported_not_silently_allowed() {
        let home = temp_home("missing");
        assert_eq!(
            guard_delete_at(&home, &home.join("nope"), EntryKind::File),
            Err("目标不存在".to_string())
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn kind_mismatch_is_rejected() {
        let home = temp_home("kind");
        let dir = home.join("plain-dir");
        std::fs::create_dir_all(&dir).expect("建普通目录");
        let file = home.join("plain.txt");
        std::fs::write(&file, b"y").expect("写普通文件");
        assert_eq!(
            guard_delete_at(&home, &dir, EntryKind::File),
            Err("目标是目录，不能按单文件删除".to_string())
        );
        assert_eq!(
            guard_delete_at(&home, &file, EntryKind::Dir),
            Err("目标是文件，不能按目录删除".to_string())
        );
        assert!(guard_delete_at(&home, &dir, EntryKind::Dir).is_ok());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ensure_within_root_rejects_escape() {
        let home = temp_home("escape");
        let root = home.join("root");
        std::fs::create_dir_all(&root).expect("建根");
        let inside = root.join("a.txt");
        std::fs::write(&inside, b"a").expect("写文件");
        let outside = home.join("b.txt");
        std::fs::write(&outside, b"b").expect("写文件");
        assert!(ensure_within_root(&inside, &root).is_ok());
        assert!(ensure_within_root(&outside, &root).is_err());
        assert!(ensure_within_root(&root, &root).is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    fn make_symlink(src: &Path, dst: &Path) -> bool {
        std::os::unix::fs::symlink(src, dst).is_ok()
    }

    #[cfg(windows)]
    fn make_symlink(src: &Path, dst: &Path) -> bool {
        std::os::windows::fs::symlink_dir(src, dst).is_ok()
    }
}
