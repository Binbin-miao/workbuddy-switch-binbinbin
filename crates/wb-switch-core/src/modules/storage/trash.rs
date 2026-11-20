//! 回收站：移入 / 列举 / 还原 / 彻底删除。
//!
//! 磁盘布局（`<trash_root>` = `store_dir()/trash`）：
//! ```text
//! trash/<YYYYMMDD-HHMMSS>/
//!   items/0/...        被移入的文件或目录（整棵原样搬过来）
//!   manifest.json      durable_write 写入的还原清单
//! ```
//!
//! 三条不可动摇的语义：
//! - 移入用 `rename`（同卷瞬时）；跨卷回落 `copy + fsync + remove`。
//! - `manifest.json` 是**唯一**还原依据；清单缺失或 JSON 坏了 → `manifest_ok = false`，
//!   仍可彻底删除，但**禁止 restore**（B16）。
//! - `purge` 只允许作用于回收站目录内部，是唯一不可恢复动作。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::protected::{guard_delete_at, EntryKind};
use super::roots::store_root_at;
use crate::modules::config;
use crate::modules::session_backup::durable_write_str;

/// 清单文件名（恢复与自检都只认这一个名字）。
pub const MANIFEST_NAME: &str = "manifest.json";
const MANIFEST_VERSION: u32 = 1;

/// 回收站根（真实家目录）。
pub fn trash_root() -> PathBuf {
    config::store_dir().join("trash")
}

/// 注入家目录的回收站根。
pub fn trash_root_at(home: &Path) -> PathBuf {
    store_root_at(home).join("trash")
}

/// 由回收站根反推家目录（`<home>/.wb-switch/trash`）；形状不符时回落真实家目录。
///
/// 这样 [`move_to_trash_at`] 这类「只给根不给家目录」的入口仍然能做 P1 保护判据，
/// 而测试又可以用临时家目录完整覆盖删除链路。
fn home_for_trash_root(root: &Path) -> PathBuf {
    root.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(config::home_dir)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    original: String,
    rel: String,
    kind: String,
    bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    created_at: i64,
    reason: String,
    entries: Vec<ManifestEntry>,
}

/// 一条回收站条目。`manifest_ok = false` 的条目可彻底删除但不允许还原。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrashEntry {
    pub id: String,
    pub created_at: i64,
    pub bytes: u64,
    pub items: usize,
    pub reason: String,
    pub manifest_ok: bool,
}

/// 一次移入的明细结果（B3：逐条 `skipped` 原因）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashMoveOutcome {
    pub entry: Option<TrashEntry>,
    pub moved: usize,
    pub bytes: u64,
    pub skipped: Vec<CleanSkipLite>,
}

/// 与 `clean::CleanSkip` 同形，避免 trash 反向依赖 clean。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanSkipLite {
    pub path: PathBuf,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreOutcome {
    pub id: String,
    pub restored: usize,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PurgeOutcome {
    pub purged: usize,
    pub bytes: u64,
}

/// 列举回收站（真实家目录）。
pub fn list_trash() -> Vec<TrashEntry> {
    list_trash_at(&trash_root())
}

/// 列举回收站条目；顺序按 `created_at` 倒序（新的在前）。
pub fn list_trash_at(root: &Path) -> Vec<TrashEntry> {
    let mut entries: Vec<TrashEntry> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|dir_entry| {
            let path = dir_entry.path();
            path.is_dir().then(|| read_entry(&path))
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    entries
}

fn read_entry(entry_dir: &Path) -> TrashEntry {
    let id = entry_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let items_dir = entry_dir.join("items");
    let manifest = read_manifest(entry_dir);
    let (created_at, reason, items, bytes, manifest_ok) = match manifest {
        Some(manifest) => (
            manifest.created_at,
            manifest.reason,
            manifest.entries.len(),
            manifest.entries.iter().map(|entry| entry.bytes).sum::<u64>(),
            true,
        ),
        None => (
            std::fs::metadata(entry_dir)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(0),
            String::new(),
            count_children(&items_dir),
            measure_bytes(&items_dir),
            false,
        ),
    };
    TrashEntry {
        id,
        created_at,
        bytes,
        items,
        reason,
        manifest_ok,
    }
}

fn read_manifest(entry_dir: &Path) -> Option<Manifest> {
    let raw = std::fs::read_to_string(entry_dir.join(MANIFEST_NAME)).ok()?;
    let manifest: Manifest = serde_json::from_str(&raw).ok()?;
    (manifest.version == MANIFEST_VERSION).then_some(manifest)
}

fn count_children(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir() || entry.path().is_file())
        .count()
}

fn measure_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            let Ok(child_meta) = std::fs::symlink_metadata(&child) else {
                continue;
            };
            if child_meta.file_type().is_symlink() {
                continue;
            }
            if child_meta.is_dir() {
                stack.push(child);
            } else if child_meta.is_file() {
                total = total.saturating_add(child_meta.len());
            }
        }
    }
    total
}

/// 把目标移入回收站（真实家目录）。
pub fn move_to_trash(paths: &[PathBuf], reason: &str) -> Result<TrashEntry, String> {
    move_to_trash_at(&trash_root(), paths, reason)
}

/// 把目标移入回收站；一个都没移成功时返回 `Err`（零值不当成功）。
pub fn move_to_trash_at(
    root: &Path,
    paths: &[PathBuf],
    reason: &str,
) -> Result<TrashEntry, String> {
    let outcome = move_to_trash_detailed_at(root, paths, reason)?;
    outcome
        .entry
        .ok_or_else(|| "没有可移入回收站的目标".to_string())
}

/// [`move_to_trash_at`] 的明细版本：逐个目标记录 `skipped` 原因，供 `execute_clean` 汇总。
pub fn move_to_trash_detailed_at(
    root: &Path,
    paths: &[PathBuf],
    reason: &str,
) -> Result<TrashMoveOutcome, String> {
    let home = home_for_trash_root(root);
    let mut outcome = TrashMoveOutcome::default();
    if paths.is_empty() {
        return Ok(outcome);
    }
    std::fs::create_dir_all(root).map_err(|error| format!("创建回收站目录失败：{error}"))?;
    let id = unique_id(root);
    let entry_dir = root.join(&id);
    let items_dir = entry_dir.join("items");
    std::fs::create_dir_all(&items_dir)
        .map_err(|error| format!("创建回收站条目失败：{error}"))?;

    // B4：先落一个占位清单，随后任何失败都能清理掉半截目录。
    let created_at = config::now_ms();
    let mut manifest = Manifest {
        version: MANIFEST_VERSION,
        created_at,
        reason: reason.to_string(),
        entries: Vec::new(),
    };
    if let Err(error) = write_manifest(&entry_dir, &manifest) {
        let _ = std::fs::remove_dir_all(&entry_dir);
        return Err(error);
    }

    for (index, path) in paths.iter().enumerate() {
        if !path.exists() {
            outcome.skipped.push(CleanSkipLite {
                path: path.clone(),
                why: "目标不存在".to_string(),
            });
            continue;
        }
        let Some(kind) = EntryKind::of_path(path) else {
            outcome.skipped.push(CleanSkipLite {
                path: path.clone(),
                why: "目标既不是普通文件也不是目录".to_string(),
            });
            continue;
        };
        if let Err(why) = guard_delete_at(&home, path, kind) {
            outcome.skipped.push(CleanSkipLite {
                path: path.clone(),
                why,
            });
            continue;
        }
        let bytes = measure_bytes(path);
        let destination = items_dir.join(index.to_string());
        if let Err(error) = move_path(path, &destination) {
            outcome.skipped.push(CleanSkipLite {
                path: path.clone(),
                why: format_io_error("移入回收站", path, &error),
            });
            // B4：磁盘不足时中止后续项，避免留下更多半截目录。
            if is_no_space(&error) {
                break;
            }
            continue;
        }
        manifest.entries.push(ManifestEntry {
            original: path.to_string_lossy().to_string(),
            rel: format!("items/{index}"),
            kind: kind.id().to_string(),
            bytes,
        });
        outcome.moved += 1;
        outcome.bytes = outcome.bytes.saturating_add(bytes);
    }

    if outcome.moved == 0 {
        let _ = std::fs::remove_dir_all(&entry_dir);
        return Ok(outcome);
    }
    if let Err(error) = write_manifest(&entry_dir, &manifest) {
        // 清单写不进去 = 没有还原依据：把已移入的条目搬回原位，别让它们凭空消失。
        rollback_moved_entries(&entry_dir, &manifest);
        return Err(error);
    }
    outcome.entry = Some(TrashEntry {
        id,
        created_at,
        bytes: outcome.bytes,
        items: manifest.entries.len(),
        reason: reason.to_string(),
        manifest_ok: true,
    });
    Ok(outcome)
}

fn write_manifest(entry_dir: &Path, manifest: &Manifest) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(manifest)
        .map_err(|error| format!("序列化回收站清单失败：{error}"))?;
    durable_write_str(&entry_dir.join(MANIFEST_NAME), &payload)
        .map_err(|error| format!("写入回收站清单失败：{error}"))
}

/// 清单落盘失败时把已移入的条目搬回原位；只有全部搬回成功才清掉半截条目目录。
///
/// 不能在这里直接 `remove_dir_all(entry_dir)`：此刻用户文件只存在于 `items/` 里，
/// 删目录等于**永久删除**，违反「删除必须先落回收站」。任一文件搬不回去就保留条目目录
/// （文件至少还在，仍可彻底删除），绝不替用户做主销毁。
fn rollback_moved_entries(entry_dir: &Path, manifest: &Manifest) {
    let mut rolled_back = true;
    for entry in &manifest.entries {
        if move_path(&entry_dir.join(&entry.rel), Path::new(&entry.original)).is_err() {
            rolled_back = false;
        }
    }
    if rolled_back {
        let _ = std::fs::remove_dir_all(entry_dir);
    }
}

/// 生成不冲突的条目 id（同一秒内连续两次清理加 `-2`、`-3`）。
fn unique_id(root: &Path) -> String {
    let base = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    if !root.join(&base).exists() {
        return base;
    }
    for suffix in 2..1000u32 {
        let candidate = format!("{base}-{suffix}");
        if !root.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("{base}-{}", uuid::Uuid::new_v4().simple())
}

fn move_path(src: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        // 跨卷 rename 会失败（Windows 不同盘符 / Linux EXDEV），回落复制。
        Err(_) => {
            copy_tree(src, dest)?;
            remove_path(src)
        }
    }
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dest)?;
        // 复制路径也要持久化证明，否则崩在 rename 之前会得到 0 字节文件。
        crate::modules::session_backup::sync_file(dest)
    }
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn is_no_space(error: &std::io::Error) -> bool {
    // ENOSPC(28) / Windows ERROR_DISK_FULL(112)；不依赖尚未稳定的 ErrorKind 变体。
    matches!(error.raw_os_error(), Some(28) | Some(112))
        || error.to_string().to_lowercase().contains("space")
}

fn format_io_error(action: &str, path: &Path, error: &std::io::Error) -> String {
    if is_no_space(error) {
        return format!("磁盘空间不足：{action} {} 失败", path.display());
    }
    format!("{action} {} 失败：{error}", path.display())
}

/// 校验回收站条目 id：只允许单层名字，任何分隔符或 `..` 一律拒绝。
fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.contains('/')
        || id.contains('\\')
        || id.contains("..")
        || id == "."
    {
        return Err("回收站条目 id 非法".to_string());
    }
    Ok(())
}

/// 还原回收站条目（真实家目录）。
pub fn restore_trash(id: &str, overwrite: bool) -> Result<RestoreOutcome, String> {
    restore_trash_at(&trash_root(), id, overwrite)
}

/// 还原回收站条目。`overwrite = false` 时遇同名目标跳过（不覆盖），并计入 `skipped`。
pub fn restore_trash_at(root: &Path, id: &str, overwrite: bool) -> Result<RestoreOutcome, String> {
    validate_id(id)?;
    let entry_dir = root.join(id);
    if !entry_dir.is_dir() {
        return Err("回收站条目不存在".to_string());
    }
    let Some(manifest) = read_manifest(&entry_dir) else {
        // B16：没有可用的还原依据就绝不动原位置。
        return Err("回收站条目损坏（清单缺失或无法解析），禁止还原".to_string());
    };
    let mut restored = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let replaced_dir = entry_dir.join("replaced");
    for (index, entry) in manifest.entries.iter().enumerate() {
        let source = entry_dir.join(&entry.rel);
        if !source.exists() {
            skipped.push(format!("{}：回收站内文件已丢失", entry.original));
            continue;
        }
        let target = PathBuf::from(&entry.original);
        if target.exists() {
            if !overwrite {
                skipped.push(format!("{}：目标已存在，已跳过", entry.original));
                continue;
            }
            let backup = replaced_dir.join(index.to_string());
            if let Err(error) = move_path(&target, &backup) {
                skipped.push(format!(
                    "{}：{}",
                    entry.original,
                    format_io_error("备份同名目标", &target, &error)
                ));
                continue;
            }
        }
        match move_path(&source, &target) {
            Ok(()) => restored += 1,
            Err(error) => skipped.push(format!(
                "{}：{}",
                entry.original,
                format_io_error("还原", &target, &error)
            )),
        }
    }
    Ok(RestoreOutcome {
        id: id.to_string(),
        restored,
        skipped,
    })
}

/// 彻底删除回收站条目（真实家目录）。
pub fn purge_trash(id: Option<&str>) -> Result<PurgeOutcome, String> {
    purge_trash_at(&trash_root(), id)
}

/// 彻底删除：`id = None` 清空全部，`Some(id)` 只删该条目。唯一不可恢复动作。
pub fn purge_trash_at(root: &Path, id: Option<&str>) -> Result<PurgeOutcome, String> {
    if !root.is_dir() {
        return Err("回收站目录不存在".to_string());
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("回收站路径不可用：{error}"))?;
    let targets: Vec<PathBuf> = match id {
        Some(id) => {
            validate_id(id)?;
            let target = root.join(id);
            if !target.is_dir() {
                return Err("回收站条目不存在".to_string());
            }
            vec![target]
        }
        None => std::fs::read_dir(root)
            .map_err(|error| format!("读取回收站失败：{error}"))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect(),
    };
    let mut purged = 0usize;
    let mut bytes = 0u64;
    for target in targets {
        // 纵深防御：规范化后必须仍在回收站内（不信任拼出来的 id）。
        let canonical = match target.canonicalize() {
            Ok(path) => path,
            Err(_) => continue,
        };
        if canonical == canonical_root || !canonical.starts_with(&canonical_root) {
            continue;
        }
        bytes = bytes.saturating_add(measure_bytes(&canonical));
        match std::fs::remove_dir_all(&canonical) {
            Ok(()) => purged += 1,
            Err(error) => return Err(format_io_error("彻底删除", &canonical, &error)),
        }
    }
    Ok(PurgeOutcome { purged, bytes })
}

/// 回收站总览（`GET /api/storage/trash` 的负载形状）。
pub fn trash_summary() -> serde_json::Value {
    let entries = list_trash();
    json!({
        "entries": entries,
        "count": entries.len(),
        "bytes": entries.iter().map(|entry| entry.bytes).sum::<u64>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::storage::roots::data_root_at;
    use crate::modules::variant::WbVariant;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-storage-trash-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时家目录");
        dir
    }

    fn pick_cache_dir(home: &Path) -> PathBuf {
        // 用客户端数据根下的缓存子目录：既不触发 P1，也不需要真实账号。
        let dir = data_root_at(WbVariant::Cn, home).join("cache");
        std::fs::create_dir_all(&dir).expect("建缓存目录");
        dir
    }

    #[test]
    fn roundtrip_restores_original_path() {
        let home = temp_home("roundtrip");
        let root = trash_root_at(&home);
        let cache = pick_cache_dir(&home);
        let file = cache.join("payload.bin");
        std::fs::write(&file, vec![b'a'; 12]).expect("写文件");

        let entry = move_to_trash_at(&root, std::slice::from_ref(&file), "storage_clean")
            .expect("移入回收站");
        assert_eq!(entry.items, 1);
        assert_eq!(entry.bytes, 12);
        assert!(!file.exists(), "原文件必须已被移走");
        assert_eq!(list_trash_at(&root).len(), 1);

        let outcome = restore_trash_at(&root, &entry.id, false).expect("还原");
        assert_eq!(outcome.restored, 1);
        assert!(outcome.skipped.is_empty());
        assert_eq!(std::fs::read(&file).expect("读回文件"), vec![b'a'; 12]);
        std::fs::remove_dir_all(&home).ok();
    }

    fn manual_manifest(entries: Vec<ManifestEntry>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            created_at: 0,
            reason: "storage_clean".to_string(),
            entries,
        }
    }

    fn moved_entry(original: &Path, rel: &str) -> ManifestEntry {
        ManifestEntry {
            original: original.to_string_lossy().to_string(),
            rel: rel.to_string(),
            kind: "file".to_string(),
            bytes: 10,
        }
    }

    #[test]
    fn rollback_brings_moved_entries_back_when_manifest_write_fails() {
        // 直接构造「文件已搬进 items/、清单还没落盘」这个中间态：真实写失败无法在单测里
        // 注入，但回滚要保证的是「文件不凭空消失」，这一点可以在这里如实验证。
        let home = temp_home("rollback");
        let entry_dir = trash_root_at(&home).join("20260101-000000");
        let items_dir = entry_dir.join("items");
        std::fs::create_dir_all(&items_dir).expect("建条目目录");
        std::fs::write(items_dir.join("0"), b"still-here").expect("写搬运后的文件");
        let original = home.join("payload.bin");
        let manifest = manual_manifest(vec![moved_entry(&original, "items/0")]);

        rollback_moved_entries(&entry_dir, &manifest);

        assert_eq!(
            std::fs::read(&original).expect("文件必须回到原位置"),
            b"still-here"
        );
        assert!(!items_dir.join("0").exists(), "items/ 不得残留副本");
        assert!(!entry_dir.exists(), "全部搬回后应清掉半截条目目录");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn rollback_keeps_entry_dir_when_any_entry_cannot_move_back() {
        let home = temp_home("rollbackpartial");
        let entry_dir = trash_root_at(&home).join("20260101-000000");
        let items_dir = entry_dir.join("items");
        std::fs::create_dir_all(&items_dir).expect("建条目目录");
        std::fs::write(items_dir.join("0"), b"rescued").expect("写搬运后的文件");
        let rescued = home.join("kept.bin");
        let lost = home.join("missing.bin");
        let manifest = manual_manifest(vec![
            moved_entry(&rescued, "items/0"),
            moved_entry(&lost, "items/7"),
        ]);

        rollback_moved_entries(&entry_dir, &manifest);

        assert_eq!(std::fs::read(&rescued).expect("能搬回的要搬回"), b"rescued");
        assert!(entry_dir.exists(), "还有条目没搬回，条目目录必须保留给用户处理");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn damaged_manifest_blocks_restore_but_allows_purge() {
        let home = temp_home("damaged");
        let root = trash_root_at(&home);
        let cache = pick_cache_dir(&home);
        let file = cache.join("broken.bin");
        std::fs::write(&file, b"z").expect("写文件");
        let entry = move_to_trash_at(&root, &[file], "storage_clean").expect("移入");

        std::fs::write(root.join(&entry.id).join(MANIFEST_NAME), b"{ not json").expect("写坏清单");
        let listed = list_trash_at(&root);
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].manifest_ok, "坏清单必须标 manifest_ok=false");
        assert!(restore_trash_at(&root, &entry.id, false).is_err());

        let purged = purge_trash_at(&root, Some(&entry.id)).expect("彻底删除");
        assert_eq!(purged.purged, 1);
        assert!(list_trash_at(&root).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn restore_skips_existing_target_without_overwrite() {
        let home = temp_home("conflict");
        let root = trash_root_at(&home);
        let cache = pick_cache_dir(&home);
        let file = cache.join("dup.txt");
        std::fs::write(&file, b"old").expect("写文件");
        let entry = move_to_trash_at(&root, &[file.clone()], "storage_clean").expect("移入");
        std::fs::write(&file, b"new").expect("重建同名文件");

        let outcome = restore_trash_at(&root, &entry.id, false).expect("还原");
        assert_eq!(outcome.restored, 0);
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(std::fs::read(&file).expect("读回"), b"new");

        let outcome = restore_trash_at(&root, &entry.id, true).expect("覆盖还原");
        assert_eq!(outcome.restored, 1);
        assert_eq!(std::fs::read(&file).expect("读回"), b"old");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn protected_targets_are_skipped_not_moved() {
        let home = temp_home("protected");
        let root = trash_root_at(&home);
        let accounts = crate::modules::storage::roots::store_root_at(&home).join("accounts.json");
        std::fs::create_dir_all(accounts.parent().expect("父目录")).expect("建 store");
        std::fs::write(&accounts, b"[]").expect("写账号库");

        let outcome = move_to_trash_detailed_at(&root, &[accounts.clone()], "storage_clean")
            .expect("明细结果");
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.skipped.len(), 1);
        assert!(accounts.exists(), "受保护文件不得被移动");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn purge_rejects_traversal_ids() {
        let home = temp_home("traversal");
        let root = trash_root_at(&home);
        std::fs::create_dir_all(&root).expect("建回收站");
        assert!(purge_trash_at(&root, Some("../escape")).is_err());
        assert!(restore_trash_at(&root, "../escape", false).is_err());
        assert!(purge_trash_at(&root, Some("")).is_err());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn purge_all_clears_every_entry() {
        let home = temp_home("purgeall");
        let root = trash_root_at(&home);
        let cache = pick_cache_dir(&home);
        for name in ["a.bin", "b.bin"] {
            let file = cache.join(name);
            std::fs::write(&file, vec![b'q'; 4]).expect("写文件");
            move_to_trash_at(&root, &[file], "storage_clean").expect("移入");
        }
        assert_eq!(list_trash_at(&root).len(), 2);
        let purged = purge_trash_at(&root, None).expect("清空");
        assert_eq!(purged.purged, 2);
        // 彻底删除的字节数含清单本身，只断言「至少把两批 4 字节内容都算进去了」。
        assert!(purged.bytes >= 8, "统计字节数偏小：{}", purged.bytes);
        assert!(list_trash_at(&root).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}
