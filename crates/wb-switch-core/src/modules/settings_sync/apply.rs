//! 同步编排：定位 → 逐键合并 → 备份前置 → 写入目标 → 记状态。
//!
//! 顺序是安全契约（M5）：**备份没成功就不许写目标**。任何一步返回 `Err` 时目标文件
//! 必须保持原样——`backup_failure_yields_zero_write` 用字节比对把这条钉住。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::backups::{self, BackupFile};
use super::merge::{merge_preferences, MergeReport};
use super::{locate_at, PreferencesLocation, SyncOutcome, SyncPreview, SyncTarget};
use crate::modules::session_backup::{durable_write, durable_write_str};

const GLOBAL_REL: &str = "global/preferences.json";

/// 跨档位拒绝对话（国内版 ↔ 国际版）。
fn cross_variant_message() -> &'static str {
    "国内版与国际版的设置不互通"
}

struct ScopePlan {
    target: PathBuf,
    rel: String,
    merged: Map<String, Value>,
    report: MergeReport,
    target_exists: bool,
}

fn scope_hash(path: &Path) -> Option<String> {
    path.parent()?
        .file_name()?
        .to_str()
        .map(|name| name.to_string())
}

fn read_object(path: &Path, role: &str) -> Result<Map<String, Value>, String> {
    if !path.is_file() {
        return Ok(Map::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("读取{role}失败（{}）：{error}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(format!("{role}不是 JSON 对象（{}）", path.display())),
        Err(error) => Err(format!("{role}无法解析（{}）：{error}", path.display())),
    }
}

fn filter_keys(source: &Map<String, Value>, keys: Option<&[String]>) -> Map<String, Value> {
    match keys {
        None => source.clone(),
        Some(wanted) => source
            .iter()
            .filter(|(key, _)| wanted.iter().any(|candidate| candidate == *key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    }
}

fn union<'a>(items: impl Iterator<Item = String> + 'a) -> Vec<String> {
    let set: BTreeSet<String> = items.collect();
    set.into_iter().collect()
}

fn build_plans(
    from_loc: &PreferencesLocation,
    to_loc: &PreferencesLocation,
    keys: Option<&[String]>,
    now_ms: i64,
) -> Result<Vec<ScopePlan>, String> {
    let mut plans = Vec::with_capacity(1 + to_loc.scoped.len());
    let source_map = read_object(&from_loc.global, "源设置文件")?;
    let target_map = read_object(&to_loc.global, "目标设置文件")?;
    let (merged, report) = merge_preferences(
        &filter_keys(&source_map, keys),
        &target_map,
        &to_loc.uid,
        now_ms,
    );
    plans.push(ScopePlan {
        target: to_loc.global.clone(),
        rel: GLOBAL_REL.to_string(),
        merged,
        report,
        target_exists: to_loc.global.is_file(),
    });
    for target in &to_loc.scoped {
        let Some(hash) = scope_hash(target) else {
            continue;
        };
        let source_map = match from_loc
            .scoped
            .iter()
            .find(|path| scope_hash(path).as_deref() == Some(hash.as_str()))
        {
            Some(path) => read_object(path, "源设置文件")?,
            // 源账号没有这个 scope：按空对象参与合并，目标独有键全部保留。
            None => Map::new(),
        };
        let target_map = read_object(target, "目标设置文件")?;
        let (merged, report) = merge_preferences(
            &filter_keys(&source_map, keys),
            &target_map,
            &to_loc.uid,
            now_ms,
        );
        plans.push(ScopePlan {
            target: target.clone(),
            rel: format!("scoped/{hash}/preferences.json"),
            merged,
            report,
            target_exists: target.is_file(),
        });
    }
    Ok(plans)
}

/// 预览两个账号的差异；不能同步时把原因放进 `blocked`（不抛错，方便前端展示）。
pub fn preview_between(
    home: &Path,
    from: &SyncTarget,
    to: &SyncTarget,
) -> Result<SyncPreview, String> {
    let mut preview = SyncPreview {
        from_uid: from.uid.clone(),
        to_uid: to.uid.clone(),
        variant: from.variant,
        keys_changed: Vec::new(),
        keys_kept: Vec::new(),
        blocked: None,
    };
    if from.variant != to.variant {
        preview.blocked = Some(cross_variant_message().to_string());
        return Ok(preview);
    }
    let from_loc = match locate_at(home, &from.uid, from.variant) {
        Some(loc) if loc.global.is_file() => loc,
        _ => {
            preview.blocked = Some("源账号没有可同步的设置".to_string());
            return Ok(preview);
        }
    };
    let Some(to_loc) = locate_at(home, &to.uid, to.variant) else {
        preview.blocked = Some("目标账号还没有设置目录，无法同步".to_string());
        return Ok(preview);
    };
    let plans = build_plans(&from_loc, &to_loc, None, super::current_ms())?;
    preview.keys_changed = union(plans.iter().flat_map(|plan| plan.report.changed.iter().cloned()));
    preview.keys_kept = union(plans.iter().flat_map(|plan| plan.report.kept.iter().cloned()));
    Ok(preview)
}

/// 执行同步。
pub fn apply_between(
    home: &Path,
    store_root: &Path,
    from: &SyncTarget,
    to: &SyncTarget,
    keys: Option<&[String]>,
) -> Result<SyncOutcome, String> {
    // B6：跨档直接拒绝，零写入。
    if from.variant != to.variant {
        return Err(cross_variant_message().to_string());
    }
    let from_loc = locate_at(home, &from.uid, from.variant)
        .filter(|loc| loc.global.is_file())
        .ok_or_else(|| "源账号没有可同步的设置".to_string())?;
    let Some(to_loc) = locate_at(home, &to.uid, to.variant) else {
        // 目标目录不存在 → 跳过（不创建）。
        return Ok(SyncOutcome {
            backup_dir: PathBuf::new(),
            keys_written: Vec::new(),
            keys_kept: Vec::new(),
            note: Some("目标账号还没有设置目录，已跳过".to_string()),
        });
    };
    let now = super::current_ms();
    let plans = build_plans(&from_loc, &to_loc, keys, now)?;
    let keys_written = union(plans.iter().flat_map(|plan| plan.report.changed.iter().cloned()));
    let keys_kept = union(plans.iter().flat_map(|plan| plan.report.kept.iter().cloned()));
    if keys_written.is_empty() {
        return Ok(SyncOutcome {
            backup_dir: PathBuf::new(),
            keys_written,
            keys_kept,
            note: Some("设置已是最新，本次未写入也未备份".to_string()),
        });
    }

    // M5 备份前置：先把目标现状完整落盘，全部成功才动目标。
    let files: Vec<BackupFile> = plans
        .iter()
        .filter(|plan| plan.target_exists)
        .filter_map(|plan| {
            std::fs::read(&plan.target)
                .ok()
                .map(|bytes| BackupFile {
                    rel: plan.rel.clone(),
                    bytes,
                })
        })
        .collect();
    let meta = json!({
        "version": 1,
        "at": now,
        "from_uid": from.uid,
        "to_uid": to.uid,
        "variant": from.variant.as_str(),
        "keys_written": keys_written.clone(),
        "keys_kept": keys_kept.clone(),
    })
    .to_string();
    let backup_dir = backups::create_backup_at(store_root, &to.uid, &files, &meta)?;

    for plan in &plans {
        if plan.report.changed.is_empty() {
            continue;
        }
        let payload = serde_json::to_string_pretty(&plan.merged)
            .map_err(|error| format!("序列化目标设置失败：{error}"))?;
        if let Some(parent) = plan.target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("创建目标目录失败：{error}"))?;
        }
        durable_write_str(&plan.target, &payload)
            .map_err(|error| format!("写入目标设置失败（{}）：{error}", plan.target.display()))?;
    }

    super::append_state_at(
        store_root,
        json!({
            "at": now,
            "from_uid": from.uid,
            "to_uid": to.uid,
            "keys_written": keys_written.clone(),
            "backup": backup_dir.to_string_lossy(),
        }),
    );
    let mut config = super::load_config_at(store_root);
    config.last_run_at = Some(now);
    let _ = super::save_config_at(store_root, &config);

    Ok(SyncOutcome {
        backup_dir,
        keys_written,
        keys_kept,
        note: None,
    })
}

/// 从备份还原；`ts` 只允许单层目录名（防越权读到备份根之外）。
pub fn restore_between(
    home: &Path,
    store_root: &Path,
    target: &SyncTarget,
    ts: &str,
) -> Result<SyncOutcome, String> {
    if ts.is_empty() || ts.contains('/') || ts.contains('\\') || ts.contains("..") {
        return Err("备份时间戳非法".to_string());
    }
    let Some(location) = locate_at(home, &target.uid, target.variant) else {
        return Err("目标账号没有设置目录，无法还原".to_string());
    };
    let backup_dir = backups::backup_root_for_at(store_root, &target.uid).join(ts);
    if !backup_dir.is_dir() {
        return Err("备份不存在".to_string());
    }
    let mut written: Vec<String> = Vec::new();
    restore_one(&backup_dir, GLOBAL_REL, &location.global, &mut written)?;
    for path in &location.scoped {
        let Some(hash) = scope_hash(path) else {
            continue;
        };
        let rel = format!("scoped/{hash}/preferences.json");
        restore_one(&backup_dir, &rel, path, &mut written)?;
    }
    if written.is_empty() {
        return Err("备份里没有可还原的设置".to_string());
    }
    super::append_state_at(
        store_root,
        json!({
            "at": super::current_ms(),
            "to_uid": target.uid,
            "restored": written,
        }),
    );
    Ok(SyncOutcome {
        backup_dir,
        keys_written: written,
        keys_kept: Vec::new(),
        note: Some("已从备份还原".to_string()),
    })
}

fn restore_one(
    backup_dir: &Path,
    rel: &str,
    target: &Path,
    written: &mut Vec<String>,
) -> Result<(), String> {
    let Some(bytes) = backups::read_backup_file(backup_dir, rel) else {
        return Ok(());
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建目标目录失败：{error}"))?;
    }
    durable_write(target, &bytes)
        .map_err(|error| format!("还原目标设置失败（{}）：{error}", target.display()))?;
    written.push(rel.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::variant::WbVariant;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-settings-apply-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn prefs_path(home: &Path, uid: &str, scope: Option<&str>) -> PathBuf {
        let data = crate::modules::storage::data_root_at(WbVariant::Cn, home);
        let base = data.join("storage").join(format!("user-{uid}-personal"));
        match scope {
            Some(hash) => base.join("scoped").join(hash).join("preferences.json"),
            None => base.join("global").join("preferences.json"),
        }
    }

    fn write_prefs(home: &Path, uid: &str, scope: Option<&str>, payload: &str) {
        let path = prefs_path(home, uid, scope);
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(path, payload).expect("写 preferences");
    }

    fn cn_target(uid: &str) -> SyncTarget {
        SyncTarget {
            uid: uid.to_string(),
            variant: WbVariant::Cn,
        }
    }

    #[test]
    fn cross_variant_apply_is_rejected_with_zero_writes() {
        let home = temp_dir("cross");
        let store = temp_dir("cross-store");
        write_prefs(&home, "uid-a", None, r#"{"theme":"dark"}"#);
        write_prefs(&home, "uid-b", None, r#"{"theme":"light"}"#);
        let before = std::fs::read(prefs_path(&home, "uid-b", None)).expect("读目标");

        let from = SyncTarget {
            uid: "uid-a".to_string(),
            variant: WbVariant::Cn,
        };
        let to = SyncTarget {
            uid: "uid-b".to_string(),
            variant: WbVariant::Ai,
        };
        let error = apply_between(&home, &store, &from, &to, None).expect_err("必须拒绝");
        assert!(error.contains("不互通"), "{error}");
        assert_eq!(std::fs::read(prefs_path(&home, "uid-b", None)).expect("读目标"), before);
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn apply_overwrites_shared_keys_and_keeps_target_only_keys() {
        let home = temp_dir("apply");
        let store = temp_dir("apply-store");
        write_prefs(
            &home,
            "uid-a",
            None,
            r#"{"theme":"dark","language":"en"}"#,
        );
        write_prefs(
            &home,
            "uid-b",
            None,
            r#"{"theme":"light","myOwn":7}"#,
        );
        let outcome =
            apply_between(&home, &store, &cn_target("uid-a"), &cn_target("uid-b"), None)
                .expect("同步成功");
        assert!(outcome.keys_written.contains(&"theme".to_string()));
        assert!(outcome.keys_kept.contains(&"myOwn".to_string()));
        assert!(outcome.backup_dir.is_dir(), "必须先备份");

        let merged: Value = serde_json::from_str(
            &std::fs::read_to_string(prefs_path(&home, "uid-b", None)).expect("读目标"),
        )
        .expect("解析");
        assert_eq!(merged.get("theme"), Some(&json!("dark")));
        assert_eq!(merged.get("myOwn"), Some(&json!(7)));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn second_apply_is_idempotent() {
        let home = temp_dir("idem");
        let store = temp_dir("idem-store");
        write_prefs(&home, "uid-a", None, r#"{"theme":"dark"}"#);
        write_prefs(&home, "uid-b", None, r#"{"theme":"light"}"#);
        apply_between(&home, &store, &cn_target("uid-a"), &cn_target("uid-b"), None)
            .expect("首次同步");
        let second = apply_between(&home, &store, &cn_target("uid-a"), &cn_target("uid-b"), None)
            .expect("二次同步");
        assert!(
            second.keys_written.is_empty(),
            "二次同步不得再报写入：{:?}",
            second.keys_written
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn backup_failure_yields_zero_write() {
        let home = temp_dir("nobackup");
        let store = temp_dir("nobackup-store");
        write_prefs(&home, "uid-a", None, r#"{"theme":"dark"}"#);
        write_prefs(&home, "uid-b", None, r#"{"theme":"light"}"#);
        let target = prefs_path(&home, "uid-b", None);
        let before = std::fs::read(&target).expect("读目标");

        // 把「工具存储根」换成普通文件 → 备份目录建不出来 → 必须零写入。
        let blocked_store = store.join("blocked");
        std::fs::write(&blocked_store, b"x").expect("写占位文件");

        let result = apply_between(
            &home,
            &blocked_store,
            &cn_target("uid-a"),
            &cn_target("uid-b"),
            None,
        );
        assert!(result.is_err(), "备份失败必须中断同步");
        assert_eq!(std::fs::read(&target).expect("读目标"), before, "备份失败时目标零写入");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn scoped_scopes_are_synced_without_inventing_missing_ones() {
        let home = temp_dir("scoped");
        let store = temp_dir("scoped-store");
        write_prefs(&home, "uid-a", None, r#"{"theme":"dark"}"#);
        write_prefs(&home, "uid-a", Some("hash1"), r#"{"language":"en"}"#);
        write_prefs(&home, "uid-b", None, r#"{"theme":"light"}"#);
        write_prefs(&home, "uid-b", Some("hash1"), r#"{"language":"zh-CN"}"#);

        apply_between(&home, &store, &cn_target("uid-a"), &cn_target("uid-b"), None)
            .expect("同步成功");
        let scoped: Value = serde_json::from_str(
            &std::fs::read_to_string(prefs_path(&home, "uid-b", Some("hash1"))).expect("读 scoped"),
        )
        .expect("解析");
        assert_eq!(scoped.get("language"), Some(&json!("en")));
        // 源没有 hash2，目标也没有 → 不得凭空创建。
        assert!(!prefs_path(&home, "uid-b", Some("hash2")).exists());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn restore_brings_back_the_backed_up_bytes() {
        let home = temp_dir("restore");
        let store = temp_dir("restore-store");
        write_prefs(&home, "uid-a", None, r#"{"theme":"dark"}"#);
        let original = r#"{"theme":"light","keep":true}"#;
        write_prefs(&home, "uid-b", None, original);
        let outcome = apply_between(&home, &store, &cn_target("uid-a"), &cn_target("uid-b"), None)
            .expect("同步成功");
        let ts = outcome
            .backup_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .expect("备份目录名");

        restore_between(&home, &store, &cn_target("uid-b"), &ts).expect("还原成功");
        let restored: Value = serde_json::from_str(
            &std::fs::read_to_string(prefs_path(&home, "uid-b", None)).expect("读目标"),
        )
        .expect("解析");
        assert_eq!(restored, serde_json::from_str::<Value>(original).expect("原始 JSON"));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn restore_rejects_traversal_timestamp() {
        let home = temp_dir("restore-bad");
        let store = temp_dir("restore-bad-store");
        write_prefs(&home, "uid-b", None, "{}");
        assert!(restore_between(&home, &store, &cn_target("uid-b"), "../etc").is_err());
        assert!(restore_between(&home, &store, &cn_target("uid-b"), "").is_err());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn missing_source_settings_is_reported() {
        let home = temp_dir("nosrc");
        let store = temp_dir("nosrc-store");
        write_prefs(&home, "uid-b", None, "{}");
        let error = apply_between(&home, &store, &cn_target("uid-a"), &cn_target("uid-b"), None)
            .expect_err("源不存在必须报错");
        assert!(error.contains("源账号没有可同步的设置"), "{error}");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&store).ok();
    }
}
