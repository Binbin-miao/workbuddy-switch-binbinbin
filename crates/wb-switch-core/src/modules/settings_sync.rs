//! 设置跨账号同步（只碰 `preferences.json`）。
//!
//! 落盘位置（**本机实测**，不是推测）：
//! ```text
//! {data_root}/storage/user-<uid>-personal/global/preferences.json
//! {data_root}/storage/user-<uid>-personal/scoped/<hash>/preferences.json
//! ```
//! 目录名必须**扫盘取实际值**，不拼死 uid；`scoped/` 下本机实测同时存在多个 hash
//! 子目录（`0cf2de2d…` / `502188ee…` / `c7389fe0…`），所以 [`PreferencesLocation::scoped`]
//! 是**列表**而不是单一路径——猜哪个是「当前」会把另一个 scope 的语言 / 模型设置丢掉。
//!
//! 显式不碰：`experts.json`、`skill-picker.json`、`{data_root}/settings.json`、
//! 任何 `storage/` 下的使用痕迹。跨档位（国内版 ↔ 国际版）直接拒绝。

mod apply;
mod backups;
mod merge;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::modules::account;
use crate::modules::config::{atomic_write, home_dir, now_ms, store_dir};
use crate::modules::variant::WbVariant;

pub use backups::{BackupEntry, BackupFile};
pub use merge::{merge_preferences, MergeReport, THEME_KEY};

/// 设置同步开关；缺省**开**（用户决策 D：默认开）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    pub enabled: bool,
    #[serde(default)]
    pub last_run_at: Option<i64>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            last_run_at: None,
        }
    }
}

const CONFIG_FILE_NAME: &str = "settings_sync_config.json";

fn config_file_at(store_root: &Path) -> PathBuf {
    store_root.join(CONFIG_FILE_NAME)
}

/// 读取配置；文件缺失或损坏都回落「开启」（缺省即开，不能让损坏文件把功能关掉）。
pub fn load_config() -> SyncConfig {
    load_config_at(&store_dir())
}

/// [`load_config`] 的可测实现。
pub fn load_config_at(store_root: &Path) -> SyncConfig {
    let raw = std::fs::read_to_string(config_file_at(store_root)).ok();
    raw.and_then(|text| serde_json::from_str::<SyncConfig>(&text).ok())
        .unwrap_or_default()
}

/// 保存配置（原子写）。
pub fn save_config(cfg: &SyncConfig) -> Result<(), String> {
    save_config_at(&store_dir(), cfg)
}

/// [`save_config`] 的可测实现。
pub fn save_config_at(store_root: &Path, cfg: &SyncConfig) -> Result<(), String> {
    std::fs::create_dir_all(store_root)
        .map_err(|error| format!("创建工具目录失败：{error}"))?;
    let payload = serde_json::to_string_pretty(cfg)
        .map_err(|error| format!("序列化设置同步配置失败：{error}"))?;
    atomic_write(&config_file_at(store_root), &payload)
        .map_err(|error| format!("写入设置同步配置失败：{error}"))
}

/// 一个账号的偏好文件位置。
///
/// `scoped` 为**多个** hash 子目录下的 `preferences.json`；目录不存在时整个
/// `PreferencesLocation` 为 `None`（不创建）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferencesLocation {
    pub variant: WbVariant,
    pub uid: String,
    pub global: PathBuf,
    pub scoped: Vec<PathBuf>,
}

/// 解析偏好位置（真实家目录）。
pub fn locate(uid: &str, variant: WbVariant) -> Option<PreferencesLocation> {
    locate_at(&home_dir(), uid, variant)
}

/// [`locate`] 的可测实现。
pub fn locate_at(home: &Path, uid: &str, variant: WbVariant) -> Option<PreferencesLocation> {
    let uid = uid.trim();
    if uid.is_empty() {
        return None;
    }
    let dir_name = format!("user-{uid}-personal");
    let data = crate::modules::storage::data_root_at(variant, home);
    // 实测在 `storage/` 下；不带 `storage/` 的老布局一并兼容。
    let personal = [data.join("storage").join(&dir_name), data.join(&dir_name)]
        .into_iter()
        .find(|path| path.is_dir())?;
    let mut scoped: Vec<PathBuf> = std::fs::read_dir(personal.join("scoped"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("preferences.json"))
        .filter(|path| path.is_file())
        .collect();
    scoped.sort();
    Some(PreferencesLocation {
        variant,
        uid: uid.to_string(),
        global: personal.join("global").join("preferences.json"),
        scoped,
    })
}

/// 参与同步的账号（`variant` 已从账号记录解析出来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTarget {
    pub uid: String,
    pub variant: WbVariant,
}

/// 从账号 id / uid 解析同步目标；账号不存在或缺 uid 时给出可读错误。
pub fn resolve_target(account_id: &str) -> Result<SyncTarget, String> {
    let account = account::find_account(account_id)
        .ok_or_else(|| format!("账号不存在：{account_id}"))?;
    let uid = account::get_str(&account, "uid")
        .map(|uid| uid.trim().to_string())
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| "该账号缺少 uid，无法定位设置目录".to_string())?;
    Ok(SyncTarget {
        uid,
        variant: WbVariant::from_account(&account),
    })
}

/// 同步预览。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncPreview {
    pub from_uid: String,
    pub to_uid: String,
    pub variant: WbVariant,
    pub keys_changed: Vec<String>,
    pub keys_kept: Vec<String>,
    /// 非空表示「这次不能同步」，前端直接展示这句话。
    pub blocked: Option<String>,
}

/// 同步结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncOutcome {
    pub backup_dir: PathBuf,
    pub keys_written: Vec<String>,
    pub keys_kept: Vec<String>,
    /// 附加说明（例如「目标账号还没有设置目录，已跳过」）。
    pub note: Option<String>,
}

/// 预览两个账号的差异（真实家目录）。
pub fn preview(from_account: &str, to_account: &str) -> Result<SyncPreview, String> {
    preview_at(&home_dir(), from_account, to_account)
}

/// [`preview`] 的可测实现：账号解析仍走真实账号库，路径落到注入家目录。
pub fn preview_at(home: &Path, from_account: &str, to_account: &str) -> Result<SyncPreview, String> {
    let from = resolve_target(from_account)?;
    let to = resolve_target(to_account)?;
    apply::preview_between(home, &from, &to)
}

/// 执行同步（真实家目录 + 真实工具存储根）。
pub fn apply(
    from_account: &str,
    to_account: &str,
    keys: Option<&[String]>,
) -> Result<SyncOutcome, String> {
    apply_at(&home_dir(), &store_dir(), from_account, to_account, keys)
}

/// [`apply`] 的可测实现：家目录与工具存储根都可注入，测试不碰真实数据。
pub fn apply_at(
    home: &Path,
    store_root: &Path,
    from_account: &str,
    to_account: &str,
    keys: Option<&[String]>,
) -> Result<SyncOutcome, String> {
    let from = resolve_target(from_account)?;
    let to = resolve_target(to_account)?;
    apply::apply_between(home, store_root, &from, &to, keys)
}

/// 列出某账号的备份（真实工具存储根）。
pub fn list_backups(account_id: &str) -> Vec<BackupEntry> {
    list_backups_at(&store_dir(), account_id)
}

/// [`list_backups`] 的可测实现。
pub fn list_backups_at(store_root: &Path, account_id: &str) -> Vec<BackupEntry> {
    match resolve_target(account_id) {
        Ok(target) => backups::list_backups_at(store_root, &target.uid),
        Err(_) => Vec::new(),
    }
}

/// 从备份还原（真实家目录 + 真实工具存储根）。
pub fn restore_backup(account_id: &str, ts: &str) -> Result<SyncOutcome, String> {
    restore_backup_at(&home_dir(), &store_dir(), account_id, ts)
}

/// [`restore_backup`] 的可测实现。
pub fn restore_backup_at(
    home: &Path,
    store_root: &Path,
    account_id: &str,
    ts: &str,
) -> Result<SyncOutcome, String> {
    let target = resolve_target(account_id)?;
    apply::restore_between(home, store_root, &target, ts)
}

/// 追加一条同步状态（最多保留 [`STATE_LIMIT`] 条，按时间淘汰）。
pub const STATE_LIMIT: usize = 20;
const STATE_FILE_NAME: &str = "settings_sync_state.json";

pub fn append_state_at(store_root: &Path, record: serde_json::Value) {
    let path = store_root.join(STATE_FILE_NAME);
    let mut items: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(&raw).ok())
        .unwrap_or_default();
    items.push(record);
    if items.len() > STATE_LIMIT {
        let overflow = items.len() - STATE_LIMIT;
        items.drain(0..overflow);
    }
    let Ok(payload) = serde_json::to_string_pretty(&items) else {
        return;
    };
    let _ = std::fs::create_dir_all(store_root);
    let _ = atomic_write(&path, &payload);
}

/// 最近一次同步时刻（无记录返回 `None`）。
pub fn last_run_at(store_root: &Path) -> Option<i64> {
    let path = store_root.join(STATE_FILE_NAME);
    let items: Vec<serde_json::Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())?;
    items
        .last()
        .and_then(|item| item.get("at").and_then(serde_json::Value::as_i64))
}

/// 当前时间戳（毫秒），供调用方构造 `now_ms` 而不必直接依赖 `config`。
pub fn current_ms() -> i64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-settings-sync-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时家目录");
        dir
    }

    fn write_prefs(home: &Path, uid: &str, scope: Option<&str>, payload: &str) {
        let data = crate::modules::storage::data_root_at(WbVariant::Cn, home);
        let base = data.join("storage").join(format!("user-{uid}-personal"));
        let path = match scope {
            Some(hash) => base.join("scoped").join(hash).join("preferences.json"),
            None => base.join("global").join("preferences.json"),
        };
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(path, payload).expect("写 preferences");
    }

    #[test]
    fn locate_finds_global_and_every_scoped_hash() {
        let home = temp_home("locate");
        write_prefs(&home, "uid-a", None, "{}");
        write_prefs(&home, "uid-a", Some("hash1"), "{}");
        write_prefs(&home, "uid-a", Some("hash2"), "{}");
        let location = locate_at(&home, "uid-a", WbVariant::Cn).expect("应能定位");
        assert!(location.global.ends_with("global/preferences.json"));
        assert_eq!(location.scoped.len(), 2, "多个 hash 必须全部返回");
        assert!(location.scoped.iter().all(|p| p.is_file()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn locate_returns_none_for_unknown_uid_instead_of_creating_dirs() {
        let home = temp_home("absent");
        assert!(locate_at(&home, "missing-uid", WbVariant::Cn).is_none());
        assert!(locate_at(&home, "   ", WbVariant::Cn).is_none());
        // 不得顺手创建目录。
        assert!(!crate::modules::storage::data_root_at(WbVariant::Cn, &home).exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn config_defaults_to_enabled_and_round_trips() {
        let store = temp_home("config");
        assert!(load_config_at(&store).enabled, "缺省必须开启");
        save_config_at(
            &store,
            &SyncConfig {
                enabled: false,
                last_run_at: Some(42),
            },
        )
        .expect("保存配置");
        let loaded = load_config_at(&store);
        assert!(!loaded.enabled);
        assert_eq!(loaded.last_run_at, Some(42));
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn corrupted_config_falls_back_to_enabled() {
        let store = temp_home("corrupt");
        std::fs::write(store.join(CONFIG_FILE_NAME), b"{ not json").expect("写坏配置");
        assert!(load_config_at(&store).enabled, "损坏配置不得把功能关掉");
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn state_log_is_capped() {
        let store = temp_home("state");
        for index in 0..(STATE_LIMIT + 5) {
            append_state_at(&store, serde_json::json!({ "at": index as i64 }));
        }
        let raw = std::fs::read_to_string(store.join(STATE_FILE_NAME)).expect("读状态文件");
        let items: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("解析");
        assert_eq!(items.len(), STATE_LIMIT);
        assert_eq!(last_run_at(&store), Some((STATE_LIMIT as i64) + 4));
        std::fs::remove_dir_all(&store).ok();
    }
}
