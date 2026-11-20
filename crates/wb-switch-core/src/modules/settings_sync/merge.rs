//! `preferences.json` 的逐键合并（纯函数，零 IO）。
//!
//! 合并规则（对应规格 M1..M4）：
//! - **M1** 逐键覆盖：源有的键覆盖目标同名键；数组整体替换（不做深合并，避免半新半旧）。
//! - **M2** 目标独有键原样保留，并如实列进 [`MergeReport::kept`]。
//! - **M3** `appearance.themeSelection` 复制后必须把 `uid` 改成目标账号，并把
//!   `revision` 抬到 `max(now_ms, 源 revision + 1)`；漏改会被客户端判成「不是我的主题」
//!   而回落默认主题——这是本项最隐蔽的坑。
//! - **M4** 设备本地状态（窗口几何 / `lastUpdateCheck` / 任何本机绝对路径）不同步，
//!   判据集中在静态表 [`DENYLIST_KEYS`] 与 [`looks_like_local_path`]，不许散落 if。

use serde_json::{json, Map, Value};

/// 主题选择键。实测客户端把它作为**扁平点号键**写在 `preferences.json` 顶层。
pub const THEME_KEY: &str = "appearance.themeSelection";

/// 设备本地状态键（静态表，禁止散落判断）。
const DENYLIST_KEYS: [&str; 6] = [
    "window.bounds",
    "window.geometry",
    "windowBounds",
    "windowState",
    "lastUpdateCheck",
    "appearance.windowState",
];

/// 一次合并的结论。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// 值确实发生变化的键。
    pub changed: Vec<String>,
    /// 目标独有、未被覆盖的键（前端要能看见「哪些没被覆盖」）。
    pub kept: Vec<String>,
}

/// 逐键覆盖合并。
pub fn merge_preferences(
    source: &Map<String, Value>,
    target: &Map<String, Value>,
    to_uid: &str,
    now_ms: i64,
) -> (Map<String, Value>, MergeReport) {
    let mut merged = target.clone();
    let mut changed = Vec::new();
    for (key, value) in source {
        let Some(next) = rewritten_value(key, value, target.get(key), to_uid, now_ms) else {
            continue;
        };
        if merged.get(key) != Some(&next) {
            changed.push(key.clone());
        }
        merged.insert(key.clone(), next);
    }
    let mut kept: Vec<String> = target
        .keys()
        .filter(|key| !source.contains_key(*key))
        .cloned()
        .collect();
    changed.sort();
    kept.sort();
    (merged, MergeReport { changed, kept })
}

/// 计算单键的落盘值；返回 `None` 表示该键不同步（denylist 命中）。
fn rewritten_value(
    key: &str,
    value: &Value,
    target_value: Option<&Value>,
    to_uid: &str,
    now_ms: i64,
) -> Option<Value> {
    if is_denied(key, value) {
        return None;
    }
    if key == THEME_KEY {
        return Some(merge_theme(value, target_value, to_uid, now_ms));
    }
    // 防御：万一上游把主题塞进 `appearance` 对象里，也要改写 uid / 抬 revision。
    if key == "appearance" {
        let object = value.as_object()?;
        if object.contains_key("themeSelection") {
            let mut next = object.clone();
            let existing = target_value
                .and_then(Value::as_object)
                .and_then(|target| target.get("themeSelection"));
            if let Some(theme) = object.get("themeSelection") {
                next.insert(
                    "themeSelection".to_string(),
                    merge_theme(theme, existing, to_uid, now_ms),
                );
            }
            return Some(Value::Object(next));
        }
    }
    Some(value.clone())
}

/// 主题对象改写：`uid` 改目标账号，`revision` 抬到 `max(now_ms, 源 + 1)`。
///
/// 幂等性处理：目标已经是同一个 uid，且除 `revision` 外内容完全一致时，保留目标现有的
/// （更新的）revision。否则第二次同步会把 revision 一路顶到「当前时间」，`keys_changed`
/// 永远非空，用户会看到「同步成功但其实什么都没变」的假动作。
fn merge_theme(source: &Value, target: Option<&Value>, to_uid: &str, now_ms: i64) -> Value {
    let Some(source_object) = source.as_object() else {
        return source.clone();
    };
    let mut next = source_object.clone();
    next.insert("uid".to_string(), Value::String(to_uid.to_string()));
    if let Some(existing) = target.and_then(Value::as_object) {
        let same_uid = existing.get("uid").and_then(Value::as_str) == Some(to_uid);
        if same_uid && equal_except_revision(existing, &next) {
            return Value::Object(existing.clone());
        }
    }
    let source_revision = source_object
        .get("revision")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    next.insert(
        "revision".to_string(),
        json!(now_ms.max(source_revision.saturating_add(1))),
    );
    Value::Object(next)
}

fn equal_except_revision(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    let strip = |map: &Map<String, Value>| {
        let mut clone = map.clone();
        clone.remove("revision");
        clone
    };
    strip(left) == strip(right)
}

fn is_denied(key: &str, value: &Value) -> bool {
    DENYLIST_KEYS.contains(&key) || looks_like_local_path(value)
}

/// 值是否像「本机绝对路径」（这类键换机即失效，同步过去只会指向不存在的目录）。
fn looks_like_local_path(value: &Value) -> bool {
    let Value::String(text) = value else {
        return false;
    };
    let trimmed = text.trim();
    if trimmed.starts_with('/') || trimmed.starts_with("\\\\") || trimmed.starts_with("//") {
        return true;
    }
    let bytes = trimmed.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), value.clone());
        }
        map
    }

    #[test]
    fn keeps_target_only_keys() {
        let source = object(&[("theme", json!("dark"))]);
        let target = object(&[("theme", json!("light")), ("myOwnKey", json!(42))]);
        let (merged, report) = merge_preferences(&source, &target, "uid-b", 1_000);
        assert_eq!(merged.get("theme"), Some(&json!("dark")));
        assert_eq!(merged.get("myOwnKey"), Some(&json!(42)));
        assert_eq!(report.kept, vec!["myOwnKey".to_string()]);
        assert_eq!(report.changed, vec!["theme".to_string()]);
    }

    #[test]
    fn theme_selection_rewrites_uid_and_bumps_revision() {
        let source = object(&[(
            THEME_KEY,
            json!({ "resourceKey": "dark", "uid": "uid-a", "revision": 99 }),
        )]);
        let target = object(&[(THEME_KEY, json!({ "resourceKey": "light", "uid": "uid-b", "revision": 5 }))]);
        let now = 1_000_000;
        let (merged, report) = merge_preferences(&source, &target, "uid-b", now);
        let theme = merged.get(THEME_KEY).expect("主题键");
        assert_eq!(theme.get("uid"), Some(&json!("uid-b")));
        // M3：max(now, 源 + 1) = now。
        assert_eq!(theme.get("revision"), Some(&json!(now)));
        assert_eq!(theme.get("resourceKey"), Some(&json!("dark")));
        assert_eq!(report.changed, vec![THEME_KEY.to_string()]);

        // 源 revision 超前（时钟偏差）时取 源 + 1，不能用更小的 now 把 revision 往回压。
        let source_ahead = object(&[(
            THEME_KEY,
            json!({ "resourceKey": "dark", "uid": "uid-a", "revision": 5_000 }),
        )]);
        let (merged, _) = merge_preferences(&source_ahead, &object(&[]), "uid-b", 1_000);
        assert_eq!(
            merged.get(THEME_KEY).and_then(|v| v.get("revision")),
            Some(&json!(5_001))
        );
    }

    #[test]
    fn second_merge_is_idempotent() {
        let source = object(&[
            ("theme", json!("dark")),
            (THEME_KEY, json!({ "resourceKey": "dark", "uid": "uid-a", "revision": 7 })),
        ]);
        let empty: Map<String, Value> = Map::new();
        let (first, _) = merge_preferences(&source, &empty, "uid-b", 1_000);
        let (second, report) = merge_preferences(&source, &first, "uid-b", 2_000);
        assert!(report.changed.is_empty(), "第二次合并不得再报变化：{:?}", report.changed);
        assert_eq!(second, first);
    }

    #[test]
    fn arrays_are_replaced_wholesale_not_deep_merged() {
        let source = object(&[("recent", json!([1, 2, 3]))]);
        let target = object(&[("recent", json!([9]))]);
        let (merged, _) = merge_preferences(&source, &target, "uid-b", 1);
        assert_eq!(merged.get("recent"), Some(&json!([1, 2, 3])));
    }

    #[test]
    fn local_device_state_is_denied() {
        let source = object(&[
            ("window.bounds", json!({ "x": 1 })),
            ("lastUpdateCheck", json!(123)),
            ("workspaceRoot", json!("C:\\Users\\someone\\proj")),
            ("unixRoot", json!("/home/someone/proj")),
            ("theme", json!("dark")),
        ]);
        let (merged, report) = merge_preferences(&source, &object(&[]), "uid-b", 1);
        assert_eq!(merged.keys().collect::<Vec<_>>(), vec!["theme"]);
        assert_eq!(report.changed, vec!["theme".to_string()]);
    }

    #[test]
    fn nested_appearance_theme_is_also_rewritten() {
        let source = object(&[(
            "appearance",
            json!({ "themeSelection": { "resourceKey": "dark", "uid": "uid-a", "revision": 3 } }),
        )]);
        let (merged, _) = merge_preferences(&source, &object(&[]), "uid-b", 500);
        let theme = merged
            .get("appearance")
            .and_then(|v| v.get("themeSelection"))
            .expect("嵌套主题");
        assert_eq!(theme.get("uid"), Some(&json!("uid-b")));
        assert_eq!(theme.get("revision"), Some(&json!(500)));
    }
}
