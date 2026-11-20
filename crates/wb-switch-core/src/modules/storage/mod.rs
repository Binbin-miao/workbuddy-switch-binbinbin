//! 存储管理：分类扫描 / 分类清理 / 回收站（工具数据、客户端数据、会话资产、回收站）。
//!
//! 模块边界（三条硬约束，改动前先读）：
//! 1. 所有根路径只来自 `config` / `variant` / `session` 的既有解析，本模块不新增
//!    任何 `~` / `.workbuddy` / `.wb-switch` 字面量，也不拼平台专属前缀。
//! 2. 一切真实删除必须过 [`protected::guard_delete`]（保护根 + `blobs` 单文件 + 符号链接），
//!    且必须先把目标移入 [`trash`] 再删，`purge` 是唯一不可恢复动作。
//! 3. 扫描结果是共享快照（[`snapshot::SNAPSHOT_TTL_MS`] 60 秒），不装文件监听：
//!    会话资产目录动辄数万文件，监听会持续吃 CPU；改为 focus/visibility 触发
//!    [`snapshot::revalidate_page`]。

pub mod catalog;
pub mod clean;
pub mod protected;
pub mod reveal;
pub mod roots;
pub mod snapshot;
pub mod trash;

pub use catalog::{list_session_assets, list_session_assets_at, measure_usage, SessionAsset, UsageStat};
pub use clean::{
    execute_clean, execute_clean_in, plan_clean, plan_clean_in, CleanEntry, CleanOutcome,
    CleanPlan, CleanSkip,
};
pub use protected::{
    guard_delete, guard_delete_at, is_protected, is_protected_at, protected_roots,
    protected_roots_at, EntryKind, ProtectedRoot,
};
pub use reveal::{reveal, reveal_supported};
pub use roots::{
    data_root_at, resolve_roots, resolve_roots_at, store_root_at, StorageCategory, StorageRoot,
};
pub use snapshot::{
    build_snapshot_at, invalidate_snapshot, page_signature, revalidate_page, shared_snapshot,
    CategoryUsage, RevalidateReport, StorageSnapshot, REVALIDATE_DEBOUNCE_MS, SNAPSHOT_TTL_MS,
};
pub use trash::{
    list_trash, list_trash_at, move_to_trash, move_to_trash_at, purge_trash, purge_trash_at,
    restore_trash, restore_trash_at, trash_root, trash_root_at, PurgeOutcome, RestoreOutcome,
    TrashEntry,
};
