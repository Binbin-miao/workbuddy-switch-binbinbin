//! 共享快照与轻量校验。
//!
//! 为什么不做文件监听（N8）：会话资产动辄数万文件，`notify`/`inotify` 会持续吃
//! CPU。改用「60 秒共享快照 + 页面 focus/visibility 触发 [`revalidate_page`]」，
//! 且 revalidate 只算当前页的一层签名，不重扫全盘。3 秒防抖在 server 层做。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use super::catalog::usage_for_at;
use super::roots::{resolve_roots_at, StorageCategory};
use super::trash::{list_trash_at, trash_root_at, TrashEntry};
use crate::modules::config;

/// 快照有效期（毫秒）。
pub const SNAPSHOT_TTL_MS: i64 = 60_000;
/// revalidate 防抖窗口（毫秒），由 server 层持有节流状态。
pub const REVALIDATE_DEBOUNCE_MS: i64 = 3_000;

/// 单个分类的体积汇总。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryUsage {
    pub category: StorageCategory,
    pub bytes: u64,
    pub files: u64,
    /// 扫描超时或权限不足导致的不完整。
    pub partial: bool,
    /// 该分类的根是否存在。「不存在的根」与「空目录」必须能区分开。
    pub exists: bool,
    /// 该分类的代表根路径（「打开目录」直接用它，前端不拼路径）。
    pub path: PathBuf,
}

/// 存储扫描结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageSnapshot {
    pub generated_at: i64,
    pub usage: Vec<CategoryUsage>,
    pub trash: Vec<TrashEntry>,
    pub trash_bytes: u64,
}

/// 校验结果。`signature` 由前端回传，用于判断「这一页有没有变」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RevalidateReport {
    pub changed: bool,
    pub signature: String,
}

static SNAPSHOT: Mutex<Option<StorageSnapshot>> = Mutex::new(None);

/// 命中 TTL 直接返回缓存，否则重扫并替换（真实家目录）。
pub fn shared_snapshot() -> StorageSnapshot {
    let now = config::now_ms();
    if let Ok(guard) = SNAPSHOT.lock() {
        if let Some(snapshot) = guard.as_ref() {
            if now.saturating_sub(snapshot.generated_at) < SNAPSHOT_TTL_MS {
                return snapshot.clone();
            }
        }
    }
    let fresh = build_snapshot_at(&config::home_dir());
    if let Ok(mut guard) = SNAPSHOT.lock() {
        *guard = Some(fresh.clone());
    }
    fresh
}

/// 所有写动作成功后必须调用，否则用户要等 60 秒才能看到新体积。
pub fn invalidate_snapshot() {
    if let Ok(mut guard) = SNAPSHOT.lock() {
        *guard = None;
    }
}

/// 构建快照（可测实现：注入家目录，禁止触碰真实 home）。
pub fn build_snapshot_at(home: &Path) -> StorageSnapshot {
    let roots = resolve_roots_at(home);
    let mut usage = Vec::with_capacity(StorageCategory::ALL.len());
    for category in StorageCategory::ALL {
        let measured = usage_for_at(category, home);
        let category_roots: Vec<&super::roots::StorageRoot> = roots
            .iter()
            .filter(|root| root.category == category)
            .collect();
        usage.push(CategoryUsage {
            category,
            bytes: measured.bytes,
            files: measured.files,
            partial: measured.partial,
            exists: category_roots.iter().any(|root| root.exists),
            // 优先给「存在」的那个根，其次给第一个（不存在也给出路径，前端显示禁用态）。
            path: category_roots
                .iter()
                .find(|root| root.exists)
                .or_else(|| category_roots.first())
                .map(|root| root.path.clone())
                .unwrap_or_default(),
        });
    }
    let trash = list_trash_at(&trash_root_at(home));
    StorageSnapshot {
        generated_at: config::now_ms(),
        trash_bytes: trash.iter().map(|entry| entry.bytes).sum(),
        trash,
        usage,
    }
}

/// 当前页的轻量签名（只算一层，不递归、不重扫）。
pub fn page_signature(page: &str) -> String {
    page_signature_at(&config::home_dir(), page)
}

/// [`page_signature`] 的可测实现。
///
/// 目前只有存储页需要它：签名 = 各分类根的存在位 + 回收站条目 id 列表。
/// 未登记的页面返回空串（表示「该页不支持 revalidate」，不假装有变化）。
pub fn page_signature_at(home: &Path, page: &str) -> String {
    match page.trim() {
        "storage" => {
            let exists: String = resolve_roots_at(home)
                .iter()
                .map(|root| if root.exists { '1' } else { '0' })
                .collect();
            let trash = list_trash_at(&trash_root_at(home));
            let ids: Vec<&str> = trash.iter().map(|entry| entry.id.as_str()).collect();
            format!("{exists}|{}|{}", trash.len(), ids.join(","))
        }
        _ => String::new(),
    }
}

/// 只校验当前页的一层；节流（3 秒）在 server 层做。
pub fn revalidate_page(page: &str, signature: &str) -> RevalidateReport {
    revalidate_page_at(&config::home_dir(), page, signature)
}

/// [`revalidate_page`] 的可测实现。
pub fn revalidate_page_at(home: &Path, page: &str, signature: &str) -> RevalidateReport {
    let current = page_signature_at(home, page);
    if current.is_empty() {
        // 未登记的页面：不宣称有变化，也不返回假签名。
        return RevalidateReport {
            changed: false,
            signature: String::new(),
        };
    }
    RevalidateReport {
        changed: current != signature,
        signature: current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::storage::roots::{data_root_at, store_root_at};
    use crate::modules::variant::WbVariant;
    use std::path::PathBuf;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-storage-snapshot-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时家目录");
        dir
    }

    #[test]
    fn snapshot_reports_all_four_categories_even_when_empty() {
        let home = temp_home("empty");
        let snapshot = build_snapshot_at(&home);
        assert_eq!(snapshot.usage.len(), 4);
        let ids: Vec<&'static str> = snapshot.usage.iter().map(|u| u.category.id()).collect();
        assert_eq!(ids, vec!["tool", "client", "sessions", "trash"]);
        // 零值必须显式：不存在 → bytes 0 / partial false / exists false。
        assert!(snapshot.usage.iter().all(|u| u.bytes == 0));
        assert!(snapshot.usage.iter().all(|u| !u.partial));
        assert!(snapshot.usage.iter().all(|u| !u.exists));
        assert!(snapshot.trash.is_empty());
        assert_eq!(snapshot.trash_bytes, 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn snapshot_counts_tool_and_client_bytes() {
        let home = temp_home("bytes");
        let store = store_root_at(&home);
        std::fs::create_dir_all(&store).expect("建工具目录");
        std::fs::write(store.join("notifications.json"), vec![b'n'; 6]).expect("写工具文件");
        let data = data_root_at(WbVariant::Cn, &home);
        std::fs::create_dir_all(data.join("cache")).expect("建缓存");
        std::fs::write(data.join("cache/x.bin"), vec![b'x'; 4]).expect("写缓存文件");

        let snapshot = build_snapshot_at(&home);
        let tool = snapshot
            .usage
            .iter()
            .find(|u| u.category == StorageCategory::Tool)
            .expect("tool");
        let client = snapshot
            .usage
            .iter()
            .find(|u| u.category == StorageCategory::Client)
            .expect("client");
        assert_eq!(tool.bytes, 6);
        assert!(tool.exists);
        assert_eq!(client.bytes, 4);
        assert!(client.exists);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn page_signature_changes_only_when_the_page_changes() {
        let home = temp_home("signature");
        let first = revalidate_page_at(&home, "storage", "");
        assert!(first.changed, "首次校验应提示需要刷新");
        assert!(!first.signature.is_empty());

        let second = revalidate_page_at(&home, "storage", &first.signature);
        assert!(!second.changed, "没有变化时不得谎报 changed");
        assert_eq!(second.signature, first.signature);

        let data = data_root_at(WbVariant::Cn, &home);
        std::fs::create_dir_all(&data).expect("建数据根");
        let third = revalidate_page_at(&home, "storage", &second.signature);
        assert!(third.changed, "新增根目录后签名必须变化");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unknown_page_is_not_claimed_as_changed() {
        let home = temp_home("unknown");
        let report = revalidate_page_at(&home, "sessions", "whatever");
        assert!(!report.changed);
        assert!(report.signature.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn trash_ids_participate_in_the_signature() {
        let home = temp_home("trashsig");
        let before = revalidate_page_at(&home, "storage", "").signature;
        let entry_dir = trash_root_at(&home).join("20260101-000000/items/0");
        std::fs::create_dir_all(&entry_dir).expect("造回收站条目");
        let after = revalidate_page_at(&home, "storage", &before).signature;
        assert_ne!(before, after, "回收站新增条目必须改变签名");
        std::fs::remove_dir_all(&home).ok();
    }
}
