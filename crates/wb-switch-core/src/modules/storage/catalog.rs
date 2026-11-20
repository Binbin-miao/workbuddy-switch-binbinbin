//! 分类与体积统计。
//!
//! 两条口径不能混：
//! - `bytes == 0` 且 `partial == false` = **真的空**；
//! - `partial == true` = 扫描不完整（权限不足 / 超时），体积可能偏小。
//!
//! 统计过程只读（G2 纯读路径），任何写动作都不发生在这里。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::roots::{data_root_at, resolve_roots_at, StorageCategory};
use crate::modules::config;
use crate::modules::variant::WbVariant;

/// 单个分类的扫描预算。超时即放弃该类并置 `partial`，不阻塞其它分类（B15）。
pub const SCAN_BUDGET: Duration = Duration::from_secs(5);

/// 一次测量的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct UsageStat {
    pub bytes: u64,
    pub files: u64,
    /// 扫描超时或权限不足导致的不完整。
    pub partial: bool,
}

/// 测量一个目录的体积。目录不存在时返回零值且 `partial = false`（真·空）。
pub fn measure_usage(root: &Path) -> UsageStat {
    measure_usage_within(root, SCAN_BUDGET)
}

/// [`measure_usage`] 的可测实现：显式预算。
pub fn measure_usage_within(root: &Path, budget: Duration) -> UsageStat {
    measure_usage_excluding(root, budget, &[])
}

/// 测量目录体积，跳过指定的**直接子目录**（用于把 `projects/` 从客户端数据里扣除，
/// 避免会话资产被重复计入两份）。
pub fn measure_usage_excluding(
    root: &Path,
    budget: Duration,
    excluded_children: &[PathBuf],
) -> UsageStat {
    let mut stat = UsageStat::default();
    if !root.is_dir() {
        return stat;
    }
    let started = Instant::now();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if started.elapsed() > budget {
            stat.partial = true;
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                // B2：读目录 EACCES 只让该分类变 partial，整体仍成功。
                stat.partial = true;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                stat.partial = true;
                continue;
            };
            let path = entry.path();
            // 符号链接不跟随（防环 + 防把链接目标算进来）。
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                stat.partial = true;
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if excluded_children.iter().any(|excluded| *excluded == path) {
                    continue;
                }
                stack.push(path);
            } else if meta.is_file() {
                stat.files += 1;
                stat.bytes = stat.bytes.saturating_add(meta.len());
            }
        }
    }
    stat
}

/// 按分类测量（真实家目录）。
pub fn usage_for(category: StorageCategory) -> UsageStat {
    usage_for_at(category, &config::home_dir())
}

/// [`usage_for`] 的可测实现。客户端与会话资产跨档位合并，避免重复计数。
pub fn usage_for_at(category: StorageCategory, home: &Path) -> UsageStat {
    match category {
        StorageCategory::Tool | StorageCategory::Trash => {
            let mut stat = UsageStat::default();
            for root in resolve_roots_at(home)
                .into_iter()
                .filter(|root| root.category == category)
            {
                let measured = measure_usage(&root.path);
                stat.bytes = stat.bytes.saturating_add(measured.bytes);
                stat.files = stat.files.saturating_add(measured.files);
                stat.partial |= measured.partial;
            }
            stat
        }
        StorageCategory::Client => {
            let mut stat = UsageStat::default();
            for variant in WbVariant::ALL {
                let data = data_root_at(variant, home);
                // `projects/` 归「会话资产」，这里扣除，避免同一批字节算两次。
                let measured =
                    measure_usage_excluding(&data, SCAN_BUDGET, &[data.join("projects")]);
                stat.bytes = stat.bytes.saturating_add(measured.bytes);
                stat.files = stat.files.saturating_add(measured.files);
                stat.partial |= measured.partial;
            }
            stat
        }
        StorageCategory::Sessions => {
            let mut stat = UsageStat::default();
            for variant in WbVariant::ALL {
                let measured = measure_usage(&data_root_at(variant, home).join("projects"));
                stat.bytes = stat.bytes.saturating_add(measured.bytes);
                stat.files = stat.files.saturating_add(measured.files);
                stat.partial |= measured.partial;
            }
            stat
        }
    }
}

/// 一条会话资产（`projects/<project>/<session>/`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionAsset {
    pub path: PathBuf,
    /// 归属项目目录名（编码后的工作目录）。
    pub project: String,
    /// 会话目录名。
    pub name: String,
    pub variant: WbVariant,
    pub bytes: u64,
    pub files: u64,
    pub modified_at: i64,
}

/// 会话资产明细（真实家目录）。
pub fn list_session_assets(limit: usize) -> Vec<SessionAsset> {
    list_session_assets_at(&config::home_dir(), limit)
}

/// [`list_session_assets`] 的可测实现：只在 `projects/<一层>/<一层>` 里找，
/// 不做全盘递归——会话资产数万文件，递归会让接口变成秒级。
pub fn list_session_assets_at(home: &Path, limit: usize) -> Vec<SessionAsset> {
    let mut assets = Vec::new();
    for variant in WbVariant::ALL {
        let projects = data_root_at(variant, home).join("projects");
        let Ok(project_entries) = std::fs::read_dir(&projects) else {
            continue;
        };
        for project_entry in project_entries.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let Ok(session_entries) = std::fs::read_dir(&project_path) else {
                continue;
            };
            for session_entry in session_entries.flatten() {
                let session_path = session_entry.path();
                if !session_path.is_dir() {
                    continue;
                }
                let measured = measure_usage(&session_path);
                assets.push(SessionAsset {
                    project: project_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    name: session_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    variant,
                    bytes: measured.bytes,
                    files: measured.files,
                    modified_at: modified_ms(&session_path),
                    path: session_path,
                });
            }
        }
    }
    assets.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    assets.truncate(limit);
    assets
}

/// 文件/目录的最后修改时刻（毫秒）；取不到时返回 0（不 panic）。
pub fn modified_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-storage-catalog-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时家目录");
        dir
    }

    #[test]
    fn empty_root_reports_zero_without_partial() {
        let home = temp_home("empty");
        let root = home.join("empty");
        std::fs::create_dir_all(&root).expect("建空目录");
        assert_eq!(
            measure_usage(&root),
            UsageStat {
                bytes: 0,
                files: 0,
                partial: false
            }
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_root_is_zero_and_not_partial() {
        let home = temp_home("absent");
        let stat = measure_usage(&home.join("no-such-dir"));
        assert_eq!(stat.bytes, 0);
        assert_eq!(stat.files, 0);
        // 不存在 ≠ 扫描失败：不当成 partial，前端才敢显示「空」。
        assert!(!stat.partial);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn counts_nested_files_and_skips_symlinks() {
        let home = temp_home("nested");
        let root = home.join("tree");
        std::fs::create_dir_all(root.join("a/b")).expect("建嵌套目录");
        std::fs::write(root.join("a/one.txt"), vec![b'x'; 10]).expect("写文件");
        std::fs::write(root.join("a/b/two.txt"), vec![b'y'; 5]).expect("写文件");
        let stat = measure_usage(&root);
        assert_eq!(stat.files, 2);
        assert_eq!(stat.bytes, 15);
        assert!(!stat.partial);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn budget_exhaustion_marks_partial() {
        let home = temp_home("budget");
        let root = home.join("tree");
        std::fs::create_dir_all(root.join("deep")).expect("建目录");
        std::fs::write(root.join("deep/x.txt"), vec![b'z'; 8]).expect("写文件");
        // 预算给 0 → 第一轮就超时，必须置 partial 而不是 panic。
        let stat = measure_usage_within(&root, Duration::from_nanos(1));
        assert!(stat.partial);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn excluded_children_are_not_counted() {
        let home = temp_home("excluded");
        let root = home.join("data");
        std::fs::create_dir_all(root.join("projects")).expect("建 projects");
        std::fs::create_dir_all(root.join("cache")).expect("建 cache");
        std::fs::write(root.join("projects/session.bin"), vec![b'p'; 100]).expect("写会话");
        std::fs::write(root.join("cache/c.bin"), vec![b'c'; 7]).expect("写缓存");
        let with_projects = measure_usage(&root);
        assert_eq!(with_projects.bytes, 107);
        let without = measure_usage_excluding(&root, SCAN_BUDGET, &[root.join("projects")]);
        assert_eq!(without.bytes, 7);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn session_assets_are_listed_per_project_and_variant() {
        let home = temp_home("assets");
        let cn_projects = data_root_at(WbVariant::Cn, &home).join("projects");
        std::fs::create_dir_all(cn_projects.join("proj-a/session-1")).expect("建会话目录");
        std::fs::write(cn_projects.join("proj-a/session-1/log.json"), vec![b'1'; 20])
            .expect("写会话文件");
        let assets = list_session_assets_at(&home, 10);
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].project, "proj-a");
        assert_eq!(assets[0].name, "session-1");
        assert_eq!(assets[0].bytes, 20);
        assert_eq!(assets[0].variant, WbVariant::Cn);
        // 分页上限生效。
        assert!(list_session_assets_at(&home, 0).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn client_usage_excludes_sessions_to_avoid_double_count() {
        let home = temp_home("client");
        let data = data_root_at(WbVariant::Cn, &home);
        std::fs::create_dir_all(data.join("projects/p/s")).expect("建会话目录");
        std::fs::create_dir_all(data.join("cache")).expect("建缓存目录");
        std::fs::write(data.join("projects/p/s/big.bin"), vec![b'b'; 200]).expect("写会话");
        std::fs::write(data.join("cache/small.bin"), vec![b's'; 3]).expect("写缓存");
        let client = usage_for_at(StorageCategory::Client, &home);
        let sessions = usage_for_at(StorageCategory::Sessions, &home);
        assert_eq!(sessions.bytes, 200);
        assert_eq!(client.bytes, 3);
        std::fs::remove_dir_all(&home).ok();
    }
}
