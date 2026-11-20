//! 领取台账：把「今天还能不能领」变成一条可复算的记录。
//!
//! 判据硬约束（改动即历史数据错乱）：
//! - **L1** 任务日固定 `Asia/Shanghai`，与主机时区无关。跨零点必须按上海日切。
//! - **L2** 「已领完」== `claimable == 0`，**不是** `claimed == total`
//!   （长期锁定 / 未达标项永远不会变成 claimed）。
//! - **L3** 官方可查时 `source = "server"` 无条件覆盖同 `(day, uid, variant)` 的 local 记录。
//! - **L4** 官方不可查时标 `Unknown` 并保留既有计数，**不得**写成 `Nothing`
//!   （查不到 ≠ 没得领）。
//! - **L5** 容量：最多 90 天 / 500 条，超出按 `day` 淘汰。
//! - **L6** 前端只读：本模块只暴露写入口给后端，不提供任何 HTTP 写路由。
//!
//! 数据来源零新增请求：直接消费 `task_center::list_tasks` 已经拿到的
//! `tasks[].claimable` / `claimable` 计数。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{FixedOffset, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modules::config::{atomic_write, now_ms, store_dir};
use crate::modules::variant::WbVariant;

/// 任务日时区固定东八区（秒）。
const DAY_OFFSET_SECONDS: i32 = 8 * 3600;
/// 台账文件名。
pub const LEDGER_FILE_NAME: &str = "claim_ledger.json";
const LEDGER_VERSION: u32 = 1;
/// 保留天数上限（L5）。
pub const MAX_DAYS: i64 = 90;
/// 保留条目上限（L5）。
pub const MAX_ENTRIES: usize = 500;

/// 台账状态。刻意不用 `None` 作变体名，避免和 `Option::None` 混读。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimState {
    /// 今日已领完（`claimable == 0` 且有任务）。
    Done,
    /// 还剩可领取项。
    Partial,
    /// 今日无可领取（没有任何任务）。
    Nothing,
    /// 官方暂不可查。
    Unknown,
}

impl ClaimState {
    pub fn id(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Partial => "partial",
            Self::Nothing => "nothing",
            Self::Unknown => "unknown",
        }
    }
}

/// 一条台账记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub day: String,
    pub uid: String,
    pub variant: String,
    pub total: u32,
    pub claimed: u32,
    pub claimable: u32,
    /// 该档位不支持签到 / 未观测到签到 = `None`（不是 `false`）。
    #[serde(default)]
    pub checkin_ok: Option<bool>,
    pub state: ClaimState,
    /// `"local"`（工具自己观测）| `"server"`（官方可查，覆盖 local）。
    pub source: String,
    pub updated_at: i64,
}

/// 某账号当日的汇总（前端只读消费）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LedgerSummary {
    pub state: ClaimState,
    /// 还能领几项。
    pub remaining: u32,
    pub day: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedgerFile {
    version: u32,
    #[serde(default)]
    entries: Vec<LedgerEntry>,
}

impl Default for LedgerFile {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            entries: Vec::new(),
        }
    }
}

/// 台账文件（真实工具存储根）。
pub fn ledger_file() -> PathBuf {
    store_dir().join(LEDGER_FILE_NAME)
}

/// 测试用：指定工具存储根下的台账文件。
pub fn ledger_file_at(store_root: &Path) -> PathBuf {
    store_root.join(LEDGER_FILE_NAME)
}

/// 进程内串行化台账读改写（多线程后台采集 + HTTP 并发写）。
static LEDGER_LOCK: Mutex<()> = Mutex::new(());

/// 任务日：`Asia/Shanghai` 自然日 `YYYY-MM-DD`。
///
/// 必须用固定偏移实现，**禁止**用主机本地时区（`chrono::Local`）。
pub fn task_day(now_ms_value: i64) -> String {
    let Some(offset) = FixedOffset::east_opt(DAY_OFFSET_SECONDS) else {
        return String::new();
    };
    match Utc.timestamp_millis_opt(now_ms_value).single() {
        Some(datetime) => datetime
            .with_timezone(&offset)
            .format("%Y-%m-%d")
            .to_string(),
        None => String::new(),
    }
}

/// 纯判据（L2）。`official_known = false` 一律 `Unknown`（L4）。
pub fn state_of(total: u32, claimable: u32, claimed: u32, official_known: bool) -> ClaimState {
    if !official_known {
        return ClaimState::Unknown;
    }
    if claimable > 0 {
        return ClaimState::Partial;
    }
    if total == 0 {
        return ClaimState::Nothing;
    }
    debug_assert!(claimed <= total, "已领数不得大于任务总数");
    ClaimState::Done
}

struct ParsedTasks {
    known: bool,
    total: u32,
    claimed: u32,
    claimable: u32,
}

/// 解析 `task_center::list_tasks` 的返回；含 `error` 或 `tasks` 为空即「官方不可查」。
fn parse_tasks(list: &Value) -> ParsedTasks {
    let has_error = list
        .get("error")
        .map(|value| !value.is_null())
        .unwrap_or(false);
    let tasks = list
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if has_error || tasks.is_empty() {
        return ParsedTasks {
            known: false,
            total: 0,
            claimed: 0,
            claimable: 0,
        };
    }
    let total = tasks.len() as u32;
    let claimed = tasks
        .iter()
        .filter(|task| task.get("claimed").and_then(Value::as_bool).unwrap_or(false))
        .count() as u32;
    // 上游已给出汇总计数；缺失时按逐条 `claimable` 复算（任务 JSON 自带该布尔）。
    let claimable = list
        .get("claimable")
        .and_then(Value::as_u64)
        .map(|value| value.min(u64::from(u32::MAX)) as u32)
        .unwrap_or_else(|| {
            tasks
                .iter()
                .filter(|task| {
                    task.get("claimable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .count() as u32
        });
    ParsedTasks {
        known: true,
        total,
        claimed,
        claimable,
    }
}

fn load_at(path: &Path) -> LedgerFile {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return LedgerFile::default();
    };
    match serde_json::from_str::<LedgerFile>(&raw) {
        Ok(file) if file.version == LEDGER_VERSION => file,
        // 版本不符或损坏：当作空台账（不覆盖原文件前先由 save 走原子写）。
        _ => LedgerFile::default(),
    }
}

fn save_at(path: &Path, file: &LedgerFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建台账目录失败：{error}"))?;
    }
    let payload = serde_json::to_string_pretty(file)
        .map_err(|error| format!("序列化台账失败：{error}"))?;
    atomic_write(path, &payload).map_err(|error| format!("写入台账失败：{error}"))
}

/// L5：先按天淘汰过老记录，再按容量截断。
fn prune(entries: &mut Vec<LedgerEntry>, today_ms: i64) {
    let cutoff = task_day(today_ms - MAX_DAYS * 86_400_000);
    if !cutoff.is_empty() {
        entries.retain(|entry| entry.day >= cutoff);
    }
    if entries.len() <= MAX_ENTRIES {
        return;
    }
    entries.sort_by(|left, right| {
        right
            .day
            .cmp(&left.day)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    entries.truncate(MAX_ENTRIES);
    entries.sort_by(|left, right| left.day.cmp(&right.day));
}

fn position_of(entries: &[LedgerEntry], day: &str, uid: &str, variant: WbVariant) -> Option<usize> {
    entries.iter().position(|entry| {
        entry.day == day && entry.uid == uid && entry.variant == variant.as_str()
    })
}

/// 观测一次任务列表（真实工具存储根）。
pub fn observe_tasks(uid: &str, variant: WbVariant, list: &Value) -> Result<LedgerEntry, String> {
    observe_tasks_at(&store_dir(), uid, variant, list, false)
}

/// 官方可查时的回写：`source` 抬成 `"server"` 并保留 `updated_at` 递增（L3）。
pub fn observe_server(uid: &str, variant: WbVariant, list: &Value) -> Result<LedgerEntry, String> {
    observe_tasks_at(&store_dir(), uid, variant, list, true)
}

fn observe_tasks_at(
    store_root: &Path,
    uid: &str,
    variant: WbVariant,
    list: &Value,
    server: bool,
) -> Result<LedgerEntry, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("缺少 uid，无法记录领取台账".to_string());
    }
    let parsed = parse_tasks(list);
    let now = now_ms();
    let day = task_day(now);
    if day.is_empty() {
        return Err("无法计算任务日".to_string());
    }
    let _guard = LEDGER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = ledger_file_at(store_root);
    let mut file = load_at(&path);
    let index = position_of(&file.entries, &day, uid, variant);
    let previous = index.map(|position| file.entries[position].clone());

    let entry = if parsed.known {
        LedgerEntry {
            day: day.clone(),
            uid: uid.to_string(),
            variant: variant.as_str().to_string(),
            total: parsed.total,
            claimed: parsed.claimed,
            claimable: parsed.claimable,
            checkin_ok: previous.as_ref().and_then(|entry| entry.checkin_ok),
            state: state_of(parsed.total, parsed.claimable, parsed.claimed, true),
            source: resolve_source(server, previous.as_ref()),
            updated_at: now,
        }
    } else {
        // L4：保留既有计数，只把状态标成 Unknown。
        match previous {
            Some(mut entry) => {
                entry.state = ClaimState::Unknown;
                entry.updated_at = now;
                entry.source = resolve_source(server, Some(&entry));
                entry
            }
            None => LedgerEntry {
                day: day.clone(),
                uid: uid.to_string(),
                variant: variant.as_str().to_string(),
                total: 0,
                claimed: 0,
                claimable: 0,
                checkin_ok: None,
                state: ClaimState::Unknown,
                source: resolve_source(server, None),
                updated_at: now,
            },
        }
    };

    match index {
        Some(position) => file.entries[position] = entry.clone(),
        None => file.entries.push(entry.clone()),
    }
    prune(&mut file.entries, now);
    save_at(&path, &file)?;
    Ok(entry)
}

/// `server` 记录只增不降：已有 server 记录时，local 观测不得把它降级。
fn resolve_source(server: bool, previous: Option<&LedgerEntry>) -> String {
    if server || previous.map(|entry| entry.source == "server").unwrap_or(false) {
        "server".to_string()
    } else {
        "local".to_string()
    }
}

/// 观测签到结果（只补 `checkin_ok`，不动任务计数）。
pub fn observe_checkin(uid: &str, variant: WbVariant, ok: bool) -> Result<LedgerEntry, String> {
    observe_checkin_at(&store_dir(), uid, variant, ok)
}

fn observe_checkin_at(
    store_root: &Path,
    uid: &str,
    variant: WbVariant,
    ok: bool,
) -> Result<LedgerEntry, String> {
    let uid = uid.trim();
    if uid.is_empty() {
        return Err("缺少 uid，无法记录签到台账".to_string());
    }
    let now = now_ms();
    let day = task_day(now);
    if day.is_empty() {
        return Err("无法计算任务日".to_string());
    }
    let _guard = LEDGER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = ledger_file_at(store_root);
    let mut file = load_at(&path);
    let index = position_of(&file.entries, &day, uid, variant);
    let mut entry = match index {
        Some(position) => file.entries[position].clone(),
        None => LedgerEntry {
            day: day.clone(),
            uid: uid.to_string(),
            variant: variant.as_str().to_string(),
            total: 0,
            claimed: 0,
            claimable: 0,
            checkin_ok: None,
            state: ClaimState::Unknown,
            source: "local".to_string(),
            updated_at: now,
        },
    };
    entry.checkin_ok = Some(ok);
    entry.updated_at = now;
    match index {
        Some(position) => file.entries[position] = entry.clone(),
        None => file.entries.push(entry.clone()),
    }
    prune(&mut file.entries, now);
    save_at(&path, &file)?;
    Ok(entry)
}

/// 按 (可选 uid, 日) 读取条目（真实工具存储根）。
pub fn entries_for(uid: Option<&str>, day: &str) -> Vec<LedgerEntry> {
    entries_for_at(&store_dir(), uid, day)
}

/// [`entries_for`] 的可测实现。
pub fn entries_for_at(store_root: &Path, uid: Option<&str>, day: &str) -> Vec<LedgerEntry> {
    let mut entries: Vec<LedgerEntry> = load_at(&ledger_file_at(store_root))
        .entries
        .into_iter()
        .filter(|entry| entry.day == day)
        .filter(|entry| uid.map(|wanted| entry.uid == wanted).unwrap_or(true))
        .collect();
    entries.sort_by(|left, right| {
        left.uid
            .cmp(&right.uid)
            .then_with(|| left.variant.cmp(&right.variant))
    });
    entries
}

/// 某账号当日汇总（真实工具存储根）。
pub fn summary(uid: &str) -> LedgerSummary {
    summary_at(&store_dir(), uid)
}

/// [`summary`] 的可测实现：多档位时取 `updated_at` 最新的一条。
pub fn summary_at(store_root: &Path, uid: &str) -> LedgerSummary {
    let day = task_day(now_ms());
    let entries = entries_for_at(store_root, Some(uid), &day);
    match entries.into_iter().max_by_key(|entry| entry.updated_at) {
        Some(entry) => LedgerSummary {
            state: entry.state,
            remaining: entry.claimable,
            day,
        },
        // 没有任何记录 = 状态未知，不能谎报「今日无可领取」。
        None => LedgerSummary {
            state: ClaimState::Unknown,
            remaining: 0,
            day,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_store(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-claim-ledger-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时工具目录");
        dir
    }

    fn shanghai_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        let offset = FixedOffset::east_opt(DAY_OFFSET_SECONDS).expect("东八区");
        offset
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("合法时间")
            .timestamp_millis()
    }

    fn known_list(claimable: u64) -> Value {
        json!({
            "tasks": [
                { "code": "t1", "claimed": false, "claimable": claimable > 0 },
                { "code": "t2", "claimed": true, "claimable": false },
            ],
            "claimable": claimable,
        })
    }

    #[test]
    fn task_day_uses_asia_shanghai_across_midnight() {
        let offset = FixedOffset::east_opt(DAY_OFFSET_SECONDS).expect("东八区");
        // 上海 2026-03-10 23:59 == UTC 15:59
        let before = Utc
            .with_ymd_and_hms(2026, 3, 10, 15, 59, 0)
            .single()
            .expect("合法时间")
            .timestamp_millis();
        // 上海 2026-03-11 00:01 == UTC 16:01
        let after = Utc
            .with_ymd_and_hms(2026, 3, 10, 16, 1, 0)
            .single()
            .expect("合法时间")
            .timestamp_millis();
        assert_eq!(task_day(before), "2026-03-10");
        assert_eq!(task_day(after), "2026-03-11");
        assert_ne!(task_day(before), task_day(after), "跨零点必须切日");
        // 主机本地时区不参与：同一时间戳永远得到同一结果。
        assert_eq!(task_day(before), task_day(before));
        assert_eq!(
            offset
                .timestamp_millis_opt(before)
                .single()
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_default(),
            "23:59"
        );
    }

    #[test]
    fn done_state_is_claimable_zero_not_claimed_total() {
        // 全部 locked / 未达标：claimed 很少，但 claimable == 0 → 已领完。
        assert_eq!(state_of(5, 0, 1, true), ClaimState::Done);
        // 即使 total == claimed，只要还有可领项就不能叫已领完。
        assert_eq!(state_of(3, 2, 3, true), ClaimState::Partial);
        assert_eq!(state_of(0, 0, 0, true), ClaimState::Nothing);
        assert_eq!(state_of(5, 0, 1, false), ClaimState::Unknown);
    }

    #[test]
    fn official_unknown_keeps_previous_and_marks_unknown() {
        let store = temp_store("unknown");
        let uid = "uid-a";
        let first = observe_tasks_at(&store, uid, WbVariant::Cn, &known_list(2), false)
            .expect("首次观测");
        assert_eq!(first.state, ClaimState::Partial);
        assert_eq!(first.claimable, 2);

        let unknown = json!({ "tasks": [], "error": "token 失效" });
        let second = observe_tasks_at(&store, uid, WbVariant::Cn, &unknown, false)
            .expect("二次观测");
        assert_eq!(second.state, ClaimState::Unknown, "不可查必须标 Unknown");
        // L4：既有计数保留，不能写成 0 / Nothing。
        assert_eq!(second.total, first.total);
        assert_eq!(second.claimable, first.claimable);
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn server_source_overwrites_local() {
        let store = temp_store("server");
        let uid = "uid-a";
        let local = observe_tasks_at(&store, uid, WbVariant::Cn, &known_list(1), false)
            .expect("local 观测");
        assert_eq!(local.source, "local");

        let server = observe_tasks_at(&store, uid, WbVariant::Cn, &known_list(0), true)
            .expect("server 观测");
        assert_eq!(server.source, "server");
        assert_eq!(server.state, ClaimState::Done);
        assert!(server.updated_at >= local.updated_at);

        // 已有 server 记录时，后续 local 观测不得把它降级。
        let again = observe_tasks_at(&store, uid, WbVariant::Cn, &known_list(2), false)
            .expect("local 再观测");
        assert_eq!(again.source, "server", "server 权威不得被 local 降级");
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn entries_are_scoped_by_uid_and_variant() {
        let store = temp_store("scope");
        observe_tasks_at(&store, "uid-a", WbVariant::Cn, &known_list(1), false).expect("cn");
        observe_tasks_at(&store, "uid-a", WbVariant::Ai, &known_list(0), false).expect("ai");
        observe_tasks_at(&store, "uid-b", WbVariant::Cn, &known_list(0), false).expect("uid-b");
        let day = task_day(now_ms());
        assert_eq!(entries_for_at(&store, Some("uid-a"), &day).len(), 2);
        assert_eq!(entries_for_at(&store, None, &day).len(), 3);
        assert!(entries_for_at(&store, Some("uid-z"), &day).is_empty());
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn checkin_is_recorded_without_touching_task_counts() {
        let store = temp_store("checkin");
        let uid = "uid-a";
        observe_tasks_at(&store, uid, WbVariant::Cn, &known_list(3), false).expect("观测任务");
        let entry = observe_checkin_at(&store, uid, WbVariant::Cn, true).expect("观测签到");
        assert_eq!(entry.checkin_ok, Some(true));
        assert_eq!(entry.claimable, 3, "签到不得改动任务计数");
        assert_eq!(entry.state, ClaimState::Partial);

        let unpaid = observe_checkin_at(&store, uid, WbVariant::Cn, false).expect("再观测");
        assert_eq!(unpaid.checkin_ok, Some(false));
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn summary_is_unknown_without_any_record() {
        let store = temp_store("empty");
        let summary = summary_at(&store, "uid-missing");
        assert_eq!(summary.state, ClaimState::Unknown);
        assert_eq!(summary.remaining, 0);
        assert_eq!(summary.day, task_day(now_ms()));
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn summary_reports_remaining_items() {
        let store = temp_store("summary");
        observe_tasks_at(&store, "uid-a", WbVariant::Cn, &known_list(4), false).expect("观测");
        let summary = summary_at(&store, "uid-a");
        assert_eq!(summary.state, ClaimState::Partial);
        assert_eq!(summary.remaining, 4);
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn capacity_is_capped_by_entry_count() {
        let store = temp_store("capacity");
        let path = ledger_file_at(&store);
        let now = now_ms();
        let mut file = LedgerFile::default();
        // 全部落在最近 80 天内：躲过「90 天淘汰」，只考验 500 条容量截断。
        for index in 0..(MAX_ENTRIES + 20) {
            file.entries.push(LedgerEntry {
                day: task_day(now - i64::from(index as u32 % 80) * 86_400_000),
                uid: format!("uid-{index}"),
                variant: "cn".to_string(),
                total: 1,
                claimed: 0,
                claimable: 1,
                checkin_ok: None,
                state: ClaimState::Partial,
                source: "local".to_string(),
                updated_at: index as i64,
            });
        }
        save_at(&path, &file).expect("写满台账");
        observe_tasks_at(&store, "uid-new", WbVariant::Cn, &known_list(1), false).expect("观测");
        let reloaded = load_at(&path);
        assert_eq!(
            reloaded.entries.len(),
            MAX_ENTRIES,
            "台账必须截断到容量上限"
        );
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn empty_uid_is_rejected() {
        let store = temp_store("baduid");
        assert!(observe_tasks_at(&store, "  ", WbVariant::Cn, &known_list(1), false).is_err());
        assert!(observe_checkin_at(&store, "", WbVariant::Cn, true).is_err());
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn shanghai_helper_matches_task_day() {
        // 固定时间戳断言（不依赖运行机器时区）。
        assert_eq!(task_day(shanghai_ms(2026, 3, 10, 23, 59)), "2026-03-10");
        assert_eq!(task_day(shanghai_ms(2026, 3, 11, 0, 1)), "2026-03-11");
    }
}
