//! 签到后的「日常对话」：给每个账号每天真发一条消息，点亮活跃地图。
//!
//! 为什么需要它：服务端的月活跃地图要求「当天完成一次对话」才记录，光签到不算。
//! 多账号用户不可能逐个去聊天，所以签到跑完后由这里代劳 —— 每个账号一条短消息，
//! 内容从按时段分组的词库里随机取，不用同一句。
//!
//! 三条硬约束：
//! 1. **每号每天最多一条**：记录落 `daily_chat_logs.json`，重启后不会重复发、不重复烧额度；
//! 2. **串行 + 间隔**：账号之间隔 3~8 秒逐个发，且本地 9 点前整轮不发 —— 同一秒多号齐发
//!    既点不亮活跃地图，也是最明显的自动化指纹（见 `spec-daily-chat-active-map.md`）；
//! 3. **失败不影响签到**：对话失败只记一行，签到结果照原样返回。

use std::path::PathBuf;

use chrono::{Local, Timelike};
use serde_json::{json, Value};

use crate::modules::config::{atomic_write, now_ms, store_dir, RunFlagGuard};

/// 并发发送的互斥：签到与定时轮次可能在同一进程里前后脚触发。
static CHAT_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 日志保留天数。
const KEEP_DAYS: i64 = 30;

/// 早上（5:00–10:59）。
const MORNING: &[&str] = &[
    "早",
    "早上好",
    "早安",
    "滴，打卡",
    "滴～上班卡",
    "新的一天",
    "起床啦",
    "今天也早起",
];

/// 中午（11:00–13:59）。
const NOON: &[&str] = &["中午好", "午安", "吃了没", "午休时间", "打卡", "歇会儿", "午饭吃啥"];

/// 下午（14:00–17:59）。
const AFTERNOON: &[&str] = &[
    "下午好",
    "下午茶时间",
    "在吗",
    "hi",
    "hello",
    "来杯咖啡",
    "还有半天",
];

/// 晚上（18:00–22:59）。
const EVENING: &[&str] = &[
    "晚上好",
    "晚安",
    "今天也辛苦啦",
    "滴，下班卡",
    "收工",
    "晚饭吃啥",
];

/// 深夜（23:00–4:59）。
const LATE_NIGHT: &[&str] = &["夜深了", "还没睡呀", "晚安", "早点休息"];

/// 全天通用（不含时段暗示，任何时间点都不违和）。
const COMMON: &[&str] = &[
    "你好", "嗨", "报个到", "来都来了", "今天也在", "打卡", "在忙吗", "冒个泡", "随便聊聊",
];

/// 账号标识：优先稳定 id，缺失时退回展示名。
fn account_key(account: &Value) -> String {
    account
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(String::from)
        .or_else(|| {
            account
                .get("email")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default()
}

fn local_hour(now: i64) -> u32 {
    chrono::DateTime::from_timestamp_millis(now)
        .map(|dt| dt.with_timezone(&Local).hour())
        .unwrap_or(12)
}

/// 是否到了可以发「日常对话」的时刻。
///
/// `after_hour = 0` 表示不限制（旧行为，可一键关掉时间窗）；否则要求本地小时已过下界。
/// 窗口**只有下界**：本地 0~8:59 不发，9:00 到午夜都发，跨过午夜再拦 —— 因为 8:00 之后
/// UTC 才跨日，9:00~23:59 都落在安全区（spec [D1] 边界②）。
pub fn should_chat_now(hour: u32, after_hour: u32) -> bool {
    after_hour == 0 || hour >= after_hour
}

/// 账号之间的发送间隔（毫秒）：`3000 + fnv1a(key + day) % 5001` → 区间 [3000, 8000]。
///
/// 用哈希而非随机数：同一天、同一账号的间隔恒定，既让多账号不挤在同一毫秒，
/// 又保证结果可复现（spec [D2]）。
pub fn chat_gap_ms(key: &str, day: &str) -> u64 {
    3000 + fnv1a(&format!("{key}{day}")) % 5001
}

/// 某个小时对应的候选词库（时段词 + 通用词）。
fn pool_for(hour: u32) -> Vec<&'static str> {
    let bucket: &[&str] = match hour {
        5..=10 => MORNING,
        11..=13 => NOON,
        14..=17 => AFTERNOON,
        18..=22 => EVENING,
        _ => LATE_NIGHT,
    };
    bucket.iter().chain(COMMON.iter()).copied().collect()
}

/// FNV-1a：把账号 id 摊成一个稳定的散列，避免每个号都抽到同一句。
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 纳秒时间戳参与随机，保证同一账号每天换句子。
fn nanos_salt() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_nanos()))
        .unwrap_or(0)
}

/// 取一句话：同一时段内按 `key` + 当天日期 + `salt` 决定，可复现（测试用固定 salt）。
pub fn phrase_for_key(key: &str, now: i64, salt: u64) -> &'static str {
    let pool = pool_for(local_hour(now));
    let day = now / 86_400_000;
    let seed = fnv1a(key) ^ (day as u64).rotate_left(24) ^ salt.rotate_left(7);
    pool[(seed % pool.len() as u64) as usize]
}

/// 账号维度的取词入口：salt 取当前纳秒，多账号同一时刻不会撞同一句。
pub fn phrase_for(account: &Value, now: i64) -> &'static str {
    phrase_for_key(&account_key(account), now, nanos_salt())
}

pub fn logs_file() -> PathBuf {
    store_dir().join("daily_chat_logs.json")
}

pub fn load_logs() -> Vec<Value> {
    std::fs::read_to_string(logs_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
        .unwrap_or_default()
}

/// 今天这个账号是否已经发过（只看成功记录）。
pub fn sent_today(rows: &[Value], key: &str, day: &str) -> bool {
    rows.iter().any(|row| {
        row.get("day").and_then(Value::as_str) == Some(day)
            && row.get("accountKey").and_then(Value::as_str) == Some(key)
            && row.get("ok").and_then(Value::as_bool) == Some(true)
    })
}

/// 只保留最近 [`KEEP_DAYS`] 天的记录（`YYYY-MM-DD` 字典序即时间序）。
fn prune_rows(rows: Vec<Value>, day: &str) -> Vec<Value> {
    let cutoff = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.checked_sub_signed(chrono::Duration::days(KEEP_DAYS)))
        .map(|date| date.format("%Y-%m-%d").to_string());
    let Some(cutoff) = cutoff else {
        return rows;
    };
    rows.into_iter()
        .filter(|row| {
            row.get("day")
                .and_then(Value::as_str)
                .is_some_and(|value| value >= cutoff.as_str())
        })
        .collect()
}

fn today_str() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// 成长域 base URL。主机名与上报通道共用一张表，不再多一处 `.cn` / `.ai` 分叉。
fn growth_base(account: &Value) -> String {
    format!(
        "https://{}",
        crate::modules::growth_auto::report_host(account)
    )
}

/// 从 `/activity/growth/streak` 的响应里取连登天数。
///
/// `code != 0`、`data` / `streak.days` 缺失或不是整数一律 `Err`：回读只用于自证，
/// 读不到就当「不知道」，绝不影响发送结果（spec [D3] 边界⑦）。
fn parse_streak_days(resp: &Value) -> Result<i64, String> {
    if let Some(code) = resp.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let msg = resp
                .get("msg")
                .or_else(|| resp.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("上游返回错误");
            return Err(format!("{msg}（code {code}）"));
        }
    }
    resp.get("data")
        .and_then(|data| data.get("streak"))
        .and_then(|streak| streak.get("days"))
        .and_then(Value::as_i64)
        .ok_or_else(|| "响应缺少 data.streak.days".to_string())
}

/// 读服务端连登天数：`GET {growth_base}/activity/growth/streak`。
async fn read_streak_days(account: &Value) -> Result<i64, String> {
    let url = format!("{}/activity/growth/streak", growth_base(account));
    let headers = crate::modules::account::build_auth_headers(account);
    let resp = crate::modules::config::http_request(&url, "GET", None, Some(&headers)).await;
    parse_streak_days(&resp)
}

/// 回读自证三态：`new` 本次点亮、`same` 天数没变、`unknown` 回读失败。
fn verify_state(before: Option<i64>, after: Option<i64>) -> &'static str {
    match (before, after) {
        (Some(before), Some(after)) if after > before => "new",
        (Some(_), Some(_)) => "same",
        _ => "unknown",
    }
}

/// 给这批账号各发一条日常对话（每号每天最多一条）。
///
/// `accounts` 由调用方先过滤好（国内版、未被排除）；`after_hour` 是本地发送下界小时
/// （`0` = 不限时）。返回汇总：
/// `{"sent": n, "skipped": m, "failed": k, "results": [...]}`。
///
/// shortcut: App 白天没运行 → 当天不会发（无法远程唤醒）。这是 [D1] 边界④ 的已知边界，
/// 不试图用后台服务补，只保证「运行时一定发得对」。
pub async fn ensure_sent_today(accounts: Vec<Value>, after_hour: u32) -> Value {
    if accounts.is_empty() {
        return json!({"sent": 0, "skipped": 0, "failed": 0, "results": []});
    }
    let now = now_ms();
    let hour = local_hour(now);
    // 时间窗拦截放在写日志、发请求之前：整轮跳过不是失败，留给当天后续轮次。
    if !should_chat_now(hour, after_hour) {
        return json!({
            "sent": 0, "skipped": 0, "failed": 0, "results": [],
            "reason": "before_chat_hour",
            "hour": hour,
        });
    }
    let Some(_guard) = RunFlagGuard::try_acquire(&CHAT_RUNNING) else {
        return json!({"sent": 0, "skipped": 0, "failed": 0, "results": [], "reason": "already_running"});
    };

    let day = today_str();
    let rows = load_logs();

    let mut pending: Vec<(String, Value, &'static str)> = Vec::new();
    let mut skipped = 0usize;
    for account in accounts {
        let key = account_key(&account);
        if key.is_empty() || sent_today(&rows, &key, &day) {
            skipped += 1;
            continue;
        }
        let phrase = phrase_for(&account, now);
        pending.push((key, account, phrase));
    }

    if pending.is_empty() {
        return json!({"sent": 0, "skipped": skipped, "failed": 0, "results": []});
    }

    // 串行发：账号之间留 3~8 秒间隔，避开「同一秒多号齐发」这种自动化指纹（spec [D2]）。
    //
    // 每个账号先过一遍 `ensure_fresh_token`：签到路径刷新的是它的副本，这里拿到的可能
    // 还是旧 token；不刷新的话，最需要补对话的那批账号反而会 401 失败。
    let cfg = crate::modules::config::load_checkin_config();
    let last_index = pending.len().saturating_sub(1);
    let mut results: Vec<Value> = Vec::new();
    let mut sent = 0usize;
    let mut failed = 0usize;
    let mut new_rows: Vec<Value> = Vec::new();

    for (index, (key, account, phrase)) in pending.into_iter().enumerate() {
        let email = crate::modules::account::account_display_name(&account);
        let account = crate::modules::refresh::ensure_fresh_token(account, &cfg).await;
        // 发前、发后各回读一次连登天数：只用于自证今天有没有点亮，不参与 `ok` 判定。
        let days_before = read_streak_days(&account).await.ok();
        let outcome = crate::modules::growth_tasks::chat_with_prompt(&account, phrase).await;
        let days_after = read_streak_days(&account).await.ok();
        let verify = verify_state(days_before, days_after);

        let mut row = json!({
            "ts": now_ms(),
            "day": day,
            "accountKey": key,
            "email": email,
            "phrase": phrase,
            "daysBefore": days_before,
            "daysAfter": days_after,
            "verify": verify,
        });
        match outcome {
            Ok((conversation_id, _request_id)) => {
                sent += 1;
                results.push(json!({
                    "email": email, "phrase": phrase, "ok": true,
                    "verify": verify, "daysAfter": days_after,
                }));
                row["ok"] = json!(true);
                row["conversationId"] = json!(conversation_id);
            }
            Err(error) => {
                failed += 1;
                results.push(json!({
                    "email": email, "phrase": phrase, "ok": false, "error": error,
                    "verify": verify, "daysAfter": days_after,
                }));
                // 失败也落盘（`ok: false`）：`sent_today` 只看成功记录，下一轮会自动重试，
                // 但日志里能看到到底为什么没发出去。
                row["ok"] = json!(false);
                row["error"] = json!(error);
            }
        }
        new_rows.push(row);

        // 最后一个账号之后不 sleep：间隔只为隔开相邻两次发送。
        if index < last_index {
            let gap = chat_gap_ms(&key, &day);
            tokio::time::sleep(std::time::Duration::from_millis(gap)).await;
        }
    }

    // 统一一次写入：逐账号写盘会互相覆盖。
    let mut merged = prune_rows(load_logs(), &day);
    merged.append(&mut new_rows);
    let _ = std::fs::create_dir_all(store_dir());
    let _ = atomic_write(
        &logs_file(),
        &serde_json::to_string_pretty(&merged).unwrap_or_default(),
    );

    json!({"sent": sent, "skipped": skipped, "failed": failed, "results": results})
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 固定时间点：2026-11-06 09:30 / 12:30 / 15:30 / 20:30 / 02:00（本地时区无关的近似值，
    /// 只用来区分时段桶，不参与断言具体小时）。
    fn at(hour: u32) -> i64 {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let time = chrono::NaiveTime::from_hms_opt(hour, 30, 0).unwrap();
        Local
            .from_local_datetime(&date.and_time(time))
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn pools_are_time_aware_and_never_empty() {
        assert!(pool_for(8).contains(&"早上好"));
        assert!(pool_for(12).contains(&"中午好"));
        assert!(pool_for(15).contains(&"下午好"));
        assert!(pool_for(20).contains(&"晚上好"));
        assert!(pool_for(2).contains(&"夜深了"));
        for hour in 0..24 {
            assert!(!pool_for(hour).is_empty(), "{hour} 点的词库不该为空");
        }
    }

    #[test]
    fn phrase_always_comes_from_the_matching_bucket() {
        for hour in [8u32, 12, 15, 20, 2] {
            let pool = pool_for(hour);
            for salt in 0..40u64 {
                let phrase = phrase_for_key("acct", at(hour), salt);
                assert!(pool.contains(&phrase), "{hour} 点抽到越界词：{phrase}");
            }
        }
    }

    /// 词库不能退化成一句话：同一时段连续取 40 次，至少要出现 5 种不同的句子。
    #[test]
    fn library_is_not_monotonous() {
        let mut seen = std::collections::HashSet::new();
        for salt in 0..40u64 {
            seen.insert(phrase_for_key("acct", at(9), salt));
        }
        assert!(seen.len() >= 5, "词库过于单一，只出现 {} 种", seen.len());
    }

    /// 同一时刻不同账号不能全撞同一句（这是「多号同时发」最容易暴露的问题）。
    #[test]
    fn different_accounts_get_different_phrases() {
        let mut seen = std::collections::HashSet::new();
        for index in 0..12 {
            seen.insert(phrase_for_key(&format!("uid-{index}"), at(9), 7));
        }
        assert!(seen.len() >= 3, "不同账号只抽到 {} 种句子", seen.len());
    }

    #[test]
    fn sent_today_only_counts_successful_rows() {
        let rows = vec![
            json!({"day": "2026-11-06", "accountKey": "a", "ok": true}),
            json!({"day": "2026-11-06", "accountKey": "b", "ok": false}),
            json!({"day": "2026-11-05", "accountKey": "c", "ok": true}),
        ];
        assert!(sent_today(&rows, "a", "2026-11-06"));
        assert!(!sent_today(&rows, "b", "2026-11-06"), "失败记录不算今天的额度");
        assert!(!sent_today(&rows, "c", "2026-11-06"));
    }

    #[test]
    fn prune_keeps_recent_rows_only() {
        let rows = vec![
            json!({"day": "2026-11-06", "accountKey": "a"}),
            json!({"day": "2026-10-07", "accountKey": "b"}),
            json!({"day": "2026-09-01", "accountKey": "c"}),
            json!({"accountKey": "no-day"}),
        ];
        let kept = prune_rows(rows, "2026-11-06");
        let keys: Vec<&str> = kept
            .iter()
            .filter_map(|row| row.get("accountKey").and_then(Value::as_str))
            .collect();
        assert_eq!(keys, vec!["a", "b"], "30 天内的保留，超期的和缺日期的丢弃");
    }

    #[test]
    fn empty_input_short_circuits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let value = runtime.block_on(ensure_sent_today(Vec::new(), 0));
        assert_eq!(value["sent"], 0);
        assert_eq!(value["results"].as_array().map(Vec::len), Some(0));
    }

    /// [D1] 时间窗：`after_hour = 0` 退回旧行为（不限时），否则按本地小时判下界。
    #[test]
    fn should_chat_now_respects_floor_hour() {
        assert!(!should_chat_now(8, 9));
        assert!(should_chat_now(9, 9));
        assert!(should_chat_now(23, 9));
        assert!(should_chat_now(0, 0), "after_hour=0 时 0 点也要发");
        assert!(should_chat_now(5, 0));
    }

    /// 窗口只有下界：`after_hour = 9` 时 0~8 点全不发、9~23 点全发，覆盖全部 24 小时。
    #[test]
    fn should_chat_now_covers_every_hour() {
        for hour in 0..24 {
            assert_eq!(should_chat_now(hour, 9), hour >= 9, "{hour} 点判断错误");
        }
    }

    /// [D2] 间隔恒定落在 [3000, 8000] ms，同一天同一账号结果不变。
    #[test]
    fn chat_gap_stays_within_three_to_eight_seconds() {
        for (key, day) in [
            ("uid-1", "2026-11-08"),
            ("uid-2", "2026-11-08"),
            ("uid-3", "2026-11-09"),
            ("user@example.com", "2026-11-09"),
            ("", "2026-11-09"),
        ] {
            let gap = chat_gap_ms(key, day);
            assert!((3000..=8000).contains(&gap), "{key}/{day} 间隔越界：{gap}");
            assert_eq!(gap, chat_gap_ms(key, day), "{key}/{day} 两次结果不一致");
        }
    }

    /// [D3] 回读只接受 `code == 0` + `data.streak.days` 整数，其余一律 Err（不 panic）。
    #[test]
    fn parse_streak_days_accepts_only_well_formed_payloads() {
        assert_eq!(
            parse_streak_days(&json!({"code": 0, "data": {"streak": {"days": 3}}})),
            Ok(3)
        );
        assert_eq!(
            parse_streak_days(&json!({"data": {"streak": {"days": 0}}})),
            Ok(0),
            "上游省略 code 时按成功处理（与 growth_tasks 同口径）"
        );
        for bad in [
            json!({"code": 5, "message": "token 失效"}),
            json!({"code": 0, "data": {}}),
            json!({"code": 0, "data": {"streak": {}}}),
            json!({"code": 0, "data": {"streak": {"days": "3"}}}),
            json!({"code": -1, "message": "网络错误"}),
        ] {
            assert!(parse_streak_days(&bad).is_err(), "{bad} 应当报错");
        }
    }

    /// [D3] 三态：只有回读变大才算点亮；变少也按 same（不谎报）。
    #[test]
    fn verify_state_reports_new_same_and_unknown() {
        assert_eq!(verify_state(Some(0), Some(1)), "new");
        assert_eq!(verify_state(Some(2), Some(2)), "same");
        assert_eq!(verify_state(Some(3), Some(2)), "same");
        assert_eq!(verify_state(None, Some(2)), "unknown");
        assert_eq!(verify_state(Some(2), None), "unknown");
        assert_eq!(verify_state(None, None), "unknown");
    }
}
