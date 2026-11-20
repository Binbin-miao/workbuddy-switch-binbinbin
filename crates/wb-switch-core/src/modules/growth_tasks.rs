//! 成长任务自动完成：任务动作表与执行器。
//!
//! 一个任务动作的完整闭环是四步：**接受 → 触发行为 → 回读进度 → 领奖**。
//!
//! * **接受**：`POST /v2/activity/growth/tasks/accept`。小程序专属任务必须带
//!   `X-Client-Platform: miniprogram`，否则 accept 返回 task not found。
//! * **触发行为**：要么上报行为事件（桌面 / web / 小程序三套指纹），
//!   要么调用真实接口（Buddy 领养、设置主题），要么真发一次对话
//!   （专家类任务——事件里的 requestId 必须是服务端返回的真实 id）。
//! * **回读**：上游计分是**异步**的，事件发出去后进度要几秒才刷新。
//!   一次性回读会误判「未达标」从而跳过领奖，故需要有界轮询。
//! * **领奖**：`POST /activity/growth/tasks/{code}/claim`。chat 域 400 时降级到
//!   web 域重试（部分任务/租户形态只在 web 域可领）。
//!
//! 事件按天幂等：同一天重复跑不会重复扣资源，已达标的任务自动跳过。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::modules::account::{build_auth_headers, envelope_token_error};
use crate::modules::config::{http_request, http_request_raw};
use crate::modules::growth_auto::{
    desktop_automation_create_event, desktop_buddy_app_sequence, desktop_chat_sequence,
    desktop_expert_actual_use_event, desktop_expert_summon_sequence,
    desktop_playbook_prompt_sequence, desktop_template_use_sequence, mini_chat_send_event,
    mini_expert_use_event, new_conversation_id, new_message_id, report_desktop_events,
    report_mp_events, report_web_event, MarketExpert,
};

/// 成长任务列表路径。
const TASKS_PATH: &str = "/v2/activity/growth/tasks";
/// 接受任务路径。
const TASKS_ACCEPT_PATH: &str = "/v2/activity/growth/tasks/accept";
/// 小程序口径标记。
const MP_PLATFORM: &str = "miniprogram";
/// Buddy 领养前置：同意协议。
const BUDDY_AGREEMENT_PATH: &str = "/activity/growth/buddy/agreement";
/// Buddy 领养接口。
const BUDDY_FIRST_PATH: &str = "/activity/growth/buddy/first";
/// 外观设置接口。
const APPEARANCE_SET_PATH: &str = "/v2/user-asset/appearance/set";
/// 专家市场列表接口。
const MARKET_EXPERT_PATH: &str = "/portal/operation-platform/market/expert/list";

/// 连续上报之间的间隔（对齐官方脚本实测的 1.05 秒口径，避免风控）。
const REPORT_GAP: Duration = Duration::from_millis(1050);
/// 专家召唤链的间隔（真实使用节奏）。
const EXPERT_SUMMON_GAP: Duration = Duration::from_secs(6);
/// 小程序对话事件的真人节奏间隔。
///
/// 上游对「5 次有效对话」有反作弊校验：数秒级连发的事件会先被计入进度，
/// 随后被判无效整体回滚（进度回落、claim 报 task not completed）。
/// 实测 45 秒间隔逐条上报全存活。
const MP_CHAT_GAP: Duration = Duration::from_secs(45);
/// 达标回读的有界轮询参数：约 12 秒预算。
const CLAIM_POLL_ATTEMPTS: usize = 4;
const CLAIM_POLL_GAP: Duration = Duration::from_secs(3);

/// 某个任务的执行结果。
#[derive(Debug, Clone)]
pub struct TaskRunResult {
    pub code: String,
    pub title: String,
    /// `done`=已领到奖励；`progressed`=行为已触发但进度未落账；`skipped`=已达标或已完成无需动作。
    pub status: &'static str,
    pub message: String,
    pub credit: i64,
}

impl TaskRunResult {
    fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "title": self.title,
            "status": self.status,
            "message": self.message,
            "credit": self.credit,
        })
    }
}

/// 成长域 base URL：国内版 `www.workbuddy.cn`，国际版 `www.workbuddy.ai`。
fn growth_base(account: &Value) -> String {
    if crate::modules::growth_auto::is_global(account) {
        "https://www.workbuddy.ai".to_string()
    } else {
        "https://www.workbuddy.cn".to_string()
    }
}

/// 发一次成长域请求，返回信封解开后的 data。
async fn growth_json(
    account: &Value,
    method: &str,
    path: &str,
    body: Option<Value>,
    mp: bool,
) -> Result<Value, String> {
    let mut headers = build_auth_headers(account);
    headers.insert(
        "X-Client-Platform".to_string(),
        if mp {
            MP_PLATFORM.to_string()
        } else {
            "web".to_string()
        },
    );
    let url = format!("{}{}", growth_base(account), path);
    let resp = http_request(&url, method, body, Some(&headers)).await;
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
    Ok(resp.get("data").cloned().unwrap_or_else(|| resp.clone()))
}

/// 上游把业务错误放在 `code`/`msg` 里，HTTP 200 也可能是失败。
fn envelope_error(resp: &Value) -> Result<(), String> {
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
    if let Some(error) = resp.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    Ok(())
}

/// 任务列表项（只保留判定需要的字段）。
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub code: String,
    pub title: String,
    pub current: i64,
    pub target: i64,
    pub progress_known: bool,
    pub accept_status: String,
    pub claimed: bool,
    /// 客户端跳转协议。专家类任务的目标专家 id 藏在里面，不能硬编码。
    pub jump_url: String,
}

impl TaskEntry {
    /// 进度已达标且尚未领取。
    pub fn claimable(&self) -> bool {
        !self.claimed && self.progress_known && self.target > 0 && self.current >= self.target
    }
}

/// 解析任务列表。
fn parse_tasks(data: &Value) -> Vec<TaskEntry> {
    let tasks = data
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    tasks.iter().filter_map(parse_task).collect()
}

fn parse_task(task: &Value) -> Option<TaskEntry> {
    let code = task
        .get("task_code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| task.get("code").and_then(Value::as_str))
        .filter(|s| !s.is_empty())?
        .to_string();
    let title = task
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(&code)
        .to_string();
    let (current, target, progress_known) = read_progress(task);
    let accept_status = task
        .get("accept_status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(TaskEntry {
        code,
        title,
        current,
        target,
        progress_known,
        claimed: accept_status == "claimed",
        accept_status,
        jump_url: task
            .get("jump_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// 进度读取：`progress` 对象优先，其次平铺的 current / target。
///
/// 判据是「两个键都在」而非「大于 0」——0 是合法进度值。
fn read_progress(task: &Value) -> (i64, i64, bool) {
    if let Some(progress) = task.get("progress") {
        if progress.get("current").is_some() && progress.get("target").is_some() {
            return (
                int_of(progress, "current"),
                int_of(progress, "target"),
                true,
            );
        }
    }
    if task.get("current").is_some() && task.get("target").is_some() {
        return (int_of(task, "current"), int_of(task, "target"), true);
    }
    (0, 0, false)
}

fn int_of(value: &Value, key: &str) -> i64 {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// 是否需要提交 accept。
fn needs_accept(status: &str) -> bool {
    status.is_empty() || status == "not_accepted"
}

/// 从任务的 `jump_url` 里解析目标专家 id。
///
/// 专家类任务的判据要求专家是**平台上真实存在的那个**（活动指定的那一位）。
/// 专家 id 藏在深链里：`...deeplink=workbuddy%3A%2F%2Fexperts%3FexpertId%3Dex_xxx`。
///
/// 不能硬编码：活动换专家时硬编码的 id 会失效，任务就再也点不亮。
fn expert_id_from_jump_url(jump_url: &str) -> Option<String> {
    let decoded = percent_decode(jump_url);
    // 深链在 URL 里是百分号编码的，先解一层再找 expertId。
    let marker = "expertId=";
    let position = decoded.find(marker)? + marker.len();
    let rest = &decoded[position..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let id = &rest[..end];
    if id.starts_with("ex_") {
        Some(id.to_string())
    } else {
        None
    }
}

/// URL 百分号解码。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 拉取任务列表（指定口径）。
pub async fn list_tasks(account: &Value, mp: bool) -> Result<Vec<TaskEntry>, String> {
    let data = growth_json(account, "GET", TASKS_PATH, None, mp).await?;
    Ok(parse_tasks(&data))
}

/// 在任务列表里定位某个任务。
async fn find_task(account: &Value, code: &str, mp: bool) -> Result<Option<TaskEntry>, String> {
    let tasks = list_tasks(account, mp).await?;
    Ok(tasks.into_iter().find(|t| t.code == code))
}

/// 有界轮询等待任务达标（上游计分异步）。
async fn wait_until_claimable(
    account: &Value,
    code: &str,
    mp: bool,
) -> Result<Option<TaskEntry>, String> {
    let mut task = find_task(account, code, mp).await?;
    if let Some(t) = &task {
        if t.claimable() || t.claimed {
            return Ok(task);
        }
    }
    for _ in 1..CLAIM_POLL_ATTEMPTS {
        tokio::time::sleep(CLAIM_POLL_GAP).await;
        task = find_task(account, code, mp).await?;
        if let Some(t) = &task {
            if t.claimable() || t.claimed {
                return Ok(task);
            }
        }
    }
    Ok(task)
}

/// 接受一个任务（幂等）。
///
/// 上游存在 200 + OK 但 accept 未真正登记的形态——此时后续事件全部不归账。
/// 故接受后回读一次，以 `accept_status` 为准；未生效重试一次。
async fn accept_task(account: &Value, code: &str, mp: bool) -> Result<(), String> {
    for attempt in 1..=2 {
        let result = growth_json(
            account,
            "POST",
            TASKS_ACCEPT_PATH,
            Some(json!({ "task_codes": [code] })),
            mp,
        )
        .await;
        if let Err(error) = result {
            if attempt == 2 {
                return Err(format!("接受失败：{error}"));
            }
            continue;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Ok(Some(task)) = find_task(account, code, mp).await {
            if !needs_accept(&task.accept_status) {
                return Ok(());
            }
        }
    }
    Err("上游未确认任务已接受".to_string())
}

/// 领取一条奖励：chat 域失败且是 400 时降级到 web 域。
async fn claim_one(account: &Value, code: &str, mp: bool) -> Result<i64, String> {
    let path = format!("/activity/growth/tasks/{}/claim", percent_encode(code));
    match growth_json(account, "POST", &path, None, mp).await {
        Ok(data) => Ok(reward_credit(&data)),
        Err(error) => {
            if error.contains("400") {
                let retry = growth_json(account, "POST", &path, None, !mp).await?;
                return Ok(reward_credit(&retry));
            }
            Err(error)
        }
    }
}

fn reward_credit(data: &Value) -> i64 {
    for key in ["credit", "reward_credit", "gain_credit", "total_credit"] {
        if data.get(key).is_some() {
            return int_of(data, key);
        }
    }
    0
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 真实动作
// ---------------------------------------------------------------------------

pub async fn chat_for_request_id_public(
    account: &Value,
    expert_id: &str,
) -> Result<(String, String), String> {
    chat_for_request_id(account, expert_id).await
}

/// 成长任务的默认开场白：短、确定、模型一句话就能答完，省额度。
const DEFAULT_CHAT_PROMPT: &str = "1+1等于几？直接回答。";

/// 成长任务沿用的通用模型。
const GROWTH_CHAT_MODEL: &str = "fast-model";

/// 日常对话（签到后补的那条）用的模型：Hy4 flash 当前限时免费，消耗倍率 0.00x。
///
/// 旧值 `hy3` 已从模型清单下线；`hy4flash`（同模型付费档，x0.03）可作直接替代。
pub const DAILY_CHAT_MODEL: &str = "hy4flash-free";

/// 免费模型下线 / 被拒时的兜底模型，保证签到流程不受影响。
const FALLBACK_CHAT_MODEL: &str = GROWTH_CHAT_MODEL;

/// 发一次真实对话，从 SSE 流里取服务端返回的 requestId。
///
/// 专家类任务的事件必须携带**服务端返回的真实 id**——自造 UUID 服务端不计数，
/// 所以这里必须真发一次请求。
async fn chat_for_request_id(account: &Value, expert_id: &str) -> Result<(String, String), String> {
    chat_with_prompt_inner(account, expert_id, DEFAULT_CHAT_PROMPT, GROWTH_CHAT_MODEL).await
}

/// 用指定开场白发一次真实对话（签到后的「日常对话」走这里）。
///
/// 默认走限时免费的 [`DAILY_CHAT_MODEL`]；服务端不认这个模型时退回通用模型，
/// 只降级一次，不会无限重试。
pub async fn chat_with_prompt(
    account: &Value,
    prompt: &str,
) -> Result<(String, String), String> {
    match chat_with_prompt_inner(account, "", prompt, DAILY_CHAT_MODEL).await {
        Ok(outcome) => Ok(outcome),
        Err(error) if model_rejected(&error) => {
            eprintln!("[日常对话] 模型 {DAILY_CHAT_MODEL} 不可用（{error}），改用 {FALLBACK_CHAT_MODEL}");
            chat_with_prompt_inner(account, "", prompt, FALLBACK_CHAT_MODEL).await
        }
        Err(error) => Err(error),
    }
}

/// 400/422 才视为「模型不被接受」；401/429/5xx 换模型也没用，直接返回原错误。
fn model_rejected(error: &str) -> bool {
    error.contains("HTTP 400") || error.contains("HTTP 422")
}

async fn chat_with_prompt_inner(
    account: &Value,
    expert_id: &str,
    prompt: &str,
    model: &str,
) -> Result<(String, String), String> {
    let conversation_id = new_conversation_id();
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant. 当前处于中文环境，使用简体中文回答。"},
            {"role": "user", "content": prompt}
        ],
        "agent": "cli",
        "temperature": 1,
        "stream": true,
        "stream_options": {"include_usage": true},
    });

    let mut headers = build_auth_headers(account);
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("X-Product".to_string(), "SaaS".to_string());
    headers.insert(
        "User-Agent".to_string(),
        crate::modules::growth_auto::desktop_ua(account),
    );
    headers.insert("X-Requested-With".to_string(), "XMLHttpRequest".to_string());
    headers.insert("X-Conversation-ID".to_string(), conversation_id.clone());
    headers.insert(
        "X-Domain".to_string(),
        crate::modules::growth_auto::report_host(account).to_string(),
    );
    if !expert_id.is_empty() {
        headers.insert("X-Expert-Id".to_string(), expert_id.to_string());
    }

    let url = format!("{}/v2/chat/completions", growth_base(account));
    let (status, _resp_headers, text) =
        http_request_raw(&url, "POST", Some(body), Some(&headers), None, true).await;
    if status != 200 {
        return Err(format!("对话失败（HTTP {status}）：{}", text.chars().take(200).collect::<String>()));
    }
    match extract_request_id(&text) {
        Some(id) => Ok((conversation_id, id)),
        None => Err("对话返回里没有服务端 requestId".to_string()),
    }
}

/// 从 SSE 文本里提取服务端 requestId（`cmb-` 前缀 32 位十六进制或裸 32 位）。
fn extract_request_id(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(pos) = rest.find("\"id\":\"") {
        let after = &rest[pos + 6..];
        if let Some(end) = after.find('"') {
            let id = &after[..end];
            if is_server_request_id(id) {
                return Some(id.to_string());
            }
        }
        rest = &after[..after.len().min(1)];
    }
    None
}

/// 服务端 requestId 形状判定。
fn is_server_request_id(value: &str) -> bool {
    let hex = value.strip_prefix("cmb-").unwrap_or(value);
    hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// 拉取专家市场列表。
///
/// 专家 id 必须真实存在（编造 id 不计数），故任务动作要先取市场列表。
pub async fn market_experts_public(
    account: &Value,
    expert_type: &str,
    keyword: Option<&str>,
) -> Result<Vec<MarketExpert>, String> {
    market_experts(account, expert_type, keyword).await
}

/// 拉取专家市场列表（公开包装）。
async fn market_experts(
    account: &Value,
    expert_type: &str,
    keyword: Option<&str>,
) -> Result<Vec<MarketExpert>, String> {
    let mut body = Map::new();
    body.insert("page".into(), json!(1));
    body.insert("page_size".into(), json!(20));
    match keyword {
        Some(word) if !word.trim().is_empty() => {
            body.insert("keyword".into(), json!(word.trim()));
        }
        _ => {
            body.insert("sort_by".into(), json!("reco_rank"));
            body.insert("sort_order".into(), json!("desc"));
        }
    }
    if !expert_type.is_empty() {
        body.insert("expert_type".into(), json!(expert_type));
    }

    let mut headers = build_auth_headers(account);
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert(
        "User-Agent".to_string(),
        crate::modules::growth_auto::desktop_ua(account),
    );
    headers.insert(
        "X-Domain".to_string(),
        crate::modules::growth_auto::report_host(account).to_string(),
    );
    headers.insert("X-Product".to_string(), "SaaS".to_string());

    let url = format!("{}{}", growth_base(account), MARKET_EXPERT_PATH);
    let resp = http_request(
        &url,
        "POST",
        Some(Value::Object(body)),
        Some(&headers),
    )
    .await;
    envelope_error(&resp)?;
    let experts = resp
        .get("data")
        .and_then(|d| d.get("experts"))
        .and_then(Value::as_array)
        .or_else(|| resp.get("experts").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();

    Ok(experts
        .iter()
        .filter_map(|e| {
            let expert_id = e.get("expert_id").and_then(Value::as_str)?;
            if expert_id.is_empty() {
                return None;
            }
            let categories = e.get("categories").and_then(Value::as_array);
            let category = categories
                .and_then(|c| c.first())
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(MarketExpert {
                expert_id: expert_id.to_string(),
                expert_type: e
                    .get("expert_type")
                    .and_then(Value::as_str)
                    .unwrap_or("agent")
                    .to_string(),
                display_name: e
                    .get("display_name_zh")
                    .and_then(Value::as_str)
                    .unwrap_or(expert_id)
                    .to_string(),
                profession: e
                    .get("profession_zh")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                version: e
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                category,
            })
        })
        .collect())
}

/// 领养第一只 Buddy：先同意协议，再领养。
///
/// 领养门槛要求「当日活跃」，未达标时上游返回 400 —— 属预期行为，报告为
/// 「稍后重试」而不是失败。
async fn adopt_first_buddy(account: &Value) -> Result<String, String> {
    let mut headers = build_auth_headers(account);
    headers.insert(
        "X-Client-Platform".to_string(),
        "web".to_string(),
    );
    let url = format!("{}{}", growth_base(account), BUDDY_AGREEMENT_PATH);
    let resp = http_request(
        &url,
        "POST",
        Some(json!({ "agree": true })),
        Some(&headers),
    )
    .await;
    envelope_error(&resp)?;

    tokio::time::sleep(REPORT_GAP).await;

    let url = format!("{}{}", growth_base(account), BUDDY_FIRST_PATH);
    let resp = http_request(&url, "POST", Some(json!({})), Some(&headers)).await;
    match envelope_error(&resp) {
        Ok(()) => Ok("已领取 Buddy".to_string()),
        Err(error) if error.contains("400") => {
            Ok("前置已上报，但领养门槛未过（上游要求当日活跃），请稍后重试".to_string())
        }
        Err(error) => Err(error),
    }
}

/// 设置外观主题并上报皮肤生效事件。
async fn apply_appearance(account: &Value) -> Result<String, String> {
    const THEME_KEY: &str = "theme-tkmw7j";
    let mut headers = build_auth_headers(account);
    headers.insert(
        "Content-Type".to_string(),
        "application/json".to_string(),
    );
    headers.insert(
        "User-Agent".to_string(),
        crate::modules::growth_auto::desktop_ua(account),
    );
    headers.insert("X-Product".to_string(), "SaaS".to_string());
    let url = format!("{}{}", growth_base(account), APPEARANCE_SET_PATH);
    let resp = http_request(
        &url,
        "POST",
        Some(json!({ "kind": "theme", "resource_key": THEME_KEY })),
        Some(&headers),
    )
    .await;
    envelope_error(&resp)?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut event = Map::new();
    event.insert("eventCode".into(), json!("appearance_skin_apply"));
    event.insert("action".into(), json!("apply"));
    event.insert("source".into(), json!("settings_close"));
    event.insert("id".into(), json!(THEME_KEY));
    event.insert("vipLevel".into(), json!(0));
    event.insert("series".into(), json!(""));
    event.insert("type".into(), json!("unknown"));
    report_desktop_events(account, vec![event]).await?;

    Ok("已设置主题并上报皮肤生效事件".to_string())
}

// ---------------------------------------------------------------------------
// 任务动作表
// ---------------------------------------------------------------------------

/// 任务动作的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    /// 对话活跃事件（可多条补足差额）。
    ChatActivity,
    /// 桌面端对话链事件。
    DesktopChat,
    /// Buddy 应用五连事件。
    BuddyApp,
    /// 定时任务创建事件。
    Automation,
    /// 资料库阅读事件。
    LibraryRead,
    /// 模板使用事件组。
    Template,
    /// 灵感案例事件组。
    Playbook,
    /// 批量专家召唤+使用。
    ExpertBatch,
    /// 单个专家召唤+使用。
    ExpertSingle,
    /// 外观主题。
    Appearance,
    /// 领养第一只 Buddy。
    FirstBuddy,
    /// 技能尝鲜：真实对话 + 技能加载事件。
    SkillFresh,
    /// 小程序对话任务。
    MiniChat,
    /// 小程序专家任务。
    MiniExpert,
    /// 小程序定时任务。
    MiniAutomation,
    /// 小程序指定模型对话。
    MiniModelChat,
    /// 小程序灵感事件。
    MiniPlaybook,
}

/// 一个任务动作定义。
struct Action {
    code: &'static str,
    kind: ActionKind,
    /// 展示用说明。
    desc: &'static str,
    /// 该任务是否只在小程序口径下发。
    mp: bool,
    /// 目标次数（补足差额用）。
    count: i64,
    /// 单个专家任务用的专家 id（兜底；优先从任务深链取真实 id）。
    expert_id: &'static str,
    /// 单个专家任务用的专家**昵称**。
    ///
    /// 判据事件里 name 取昵称、expertTitle 取职业名，两者都参与服务端校验、
    /// 不可互换（参考仓库 2026-10-23 实测）。
    expert_name: &'static str,
    /// 单个专家任务用的专家**职业名**。
    expert_profession: &'static str,
    /// 单个专家任务的专家版本号。
    expert_version: &'static str,
}

/// 动作表。顺序即执行顺序：先解锁依赖项（活跃上报 → 领养 → 其余）。
///
/// 不含 `Expert_Philanthropy`（需真实捐款，服务端校验捐赠回执，无法绕过）。
const ACTIONS: &[Action] = &[
    Action {
        code: "chat_5",
        kind: ActionKind::ChatActivity,
        desc: "补足对话次数上报",
        mp: false,
        count: 5,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "first_buddy",
        kind: ActionKind::FirstBuddy,
        desc: "同意协议并领养第一只 Buddy",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "RichMeow_Chat",
        kind: ActionKind::DesktopChat,
        desc: "上报桌面端对话事件链",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Buddy_App",
        kind: ActionKind::BuddyApp,
        desc: "上报进入 Buddy 应用事件链",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Buddy_App_QQ",
        kind: ActionKind::BuddyApp,
        desc: "上报企鹅教师助手进入事件链",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "automation_1",
        kind: ActionKind::Automation,
        desc: "上报定时任务创建事件",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Library_read",
        kind: ActionKind::LibraryRead,
        desc: "上报资料库介绍阅读事件",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "template_5",
        kind: ActionKind::Template,
        desc: "上报模板使用事件组 ×5",
        mp: false,
        count: 5,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "playbook_prompt",
        kind: ActionKind::Playbook,
        desc: "上报灵感案例做同款事件组",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Hp_Appearance",
        kind: ActionKind::Appearance,
        desc: "设置主题并上报皮肤生效事件",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "expert_5",
        kind: ActionKind::ExpertBatch,
        desc: "真实专家召唤+使用链 ×5",
        mp: false,
        count: 5,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Expert_team_use_3",
        kind: ActionKind::ExpertBatch,
        desc: "真实专家团召唤+使用链 ×3",
        mp: false,
        count: 3,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "skill_1",
        kind: ActionKind::SkillFresh,
        desc: "真实对话 + 技能加载事件",
        mp: false,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Expert_lighthouse",
        kind: ActionKind::ExpertSingle,
        desc: "轻量云专家召唤+使用链",
        mp: false,
        count: 1,
        expert_id: "ex_2cvvUZQhDyeJ",
        expert_name: "腾讯轻量云专家",
        expert_profession: "腾讯轻量云专家",
        expert_version: "1.0.2",
    },
    Action {
        code: "WorkBuddy_Valorant",
        kind: ActionKind::ExpertSingle,
        desc: "无畏契约拼豆专家召唤+使用链",
        mp: false,
        count: 1,
        expert_id: "ex_xQHHD1u5Jkua",
        expert_name: "无畏契约拼豆专家",
        expert_profession: "无畏契约拼豆专家",
        expert_version: "1.2.1",
    },
    Action {
        code: "expert_use_maidehao",
        kind: ActionKind::ExpertSingle,
        desc: "中国电商运营专家召唤+使用链",
        mp: false,
        count: 1,
        expert_id: "ex_GDl31WahyIH0",
        expert_name: "卖得好",
        expert_profession: "中国电商运营专家",
        expert_version: "1.0.1",
    },
    Action {
        code: "expert_use_douyinyin",
        kind: ActionKind::ExpertSingle,
        desc: "抖音策略师召唤+使用链",
        mp: false,
        count: 1,
        expert_id: "ex_FbY5nlWTqDod",
        expert_name: "斗音音",
        expert_profession: "抖音策略师",
        expert_version: "1.0.2",
    },
    Action {
        code: "expert_use_shangjia_xuanchuan",
        kind: ActionKind::ExpertSingle,
        desc: "商家宣传页与海报设计师召唤+使用链",
        mp: false,
        count: 1,
        expert_id: "ex_Weo3x2EZuSDm",
        expert_name: "商家宣传设计师",
        expert_profession: "商家宣传页与海报设计师",
        expert_version: "1.6.0",
    },
    Action {
        code: "expert_use_bowangwang",
        kind: ActionKind::ExpertSingle,
        desc: "直播电商教练召唤+使用链",
        mp: false,
        count: 1,
        expert_id: "ex_WZY136wV5aMt",
        expert_name: "播旺旺",
        expert_profession: "直播电商教练",
        expert_version: "1.0.2",
    },
    Action {
        code: "Sequential_Tasks_1",
        kind: ActionKind::MiniChat,
        desc: "小程序首次对话",
        mp: true,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Sequential_Tasks_2",
        kind: ActionKind::MiniExpert,
        desc: "小程序选中专家并完成对话",
        mp: true,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Sequential_Tasks_3",
        kind: ActionKind::MiniChat,
        desc: "小程序完成 5 次对话",
        mp: true,
        count: 5,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Sequential_Tasks_4",
        kind: ActionKind::MiniAutomation,
        desc: "小程序创建定时任务",
        mp: true,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Sequential_Tasks_5",
        kind: ActionKind::MiniModelChat,
        desc: "小程序使用 GLM5.2",
        mp: true,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Sequential_Tasks_6",
        kind: ActionKind::MiniChat,
        desc: "小程序完成 10 次对话",
        mp: true,
        count: 10,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
    Action {
        code: "Sequential_Tasks_7",
        kind: ActionKind::MiniPlaybook,
        desc: "小程序体验灵感功能",
        mp: true,
        count: 1,
        expert_id: "",
        expert_name: "",
        expert_profession: "",
        expert_version: "",
    },
];

/// 查任务对应的动作定义。
fn action_for(code: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|a| a.code == code)
}

/// 小程序口径专属的任务码。
fn is_mp_code(code: &str) -> bool {
    code.starts_with("Sequential_Tasks_")
}

// ---------------------------------------------------------------------------
// 执行器
// ---------------------------------------------------------------------------

/// 执行一个任务动作，返回描述性结果（不含领奖）。
async fn run_action(account: &Value, action: &Action) -> Result<String, String> {
    match action.kind {
        ActionKind::ChatActivity => {
            let task = find_task(account, action.code, action.mp).await?;
            let (current, target) = match &task {
                Some(t) if t.progress_known => (t.current, t.target),
                _ => (0, action.count),
            };
            let target = if target > 0 { target } else { action.count };
            let need = target - current;
            if need <= 0 {
                return Ok("进度已达标，无需上报".to_string());
            }
            for index in 0..need {
                let conversation = new_conversation_id();
                let mut event = Map::new();
                event.insert("eventCode".into(), json!("chat_request_send"));
                event.insert("mode".into(), json!("craft"));
                event.insert("conversationId".into(), json!(conversation));
                event.insert("requestId".into(), json!(new_message_id()));
                event.insert("inputLength".into(), json!(12));
                event.insert("requestModelId".into(), json!("deepseek-v4.1-flash"));
                event.insert("requestModelName".into(), json!("DeepSeek-V4.1-Flash"));
                event.insert("isPlan".into(), json!(false));
                event.insert("isAutoExecuteTerminal".into(), json!(false));
                event.insert("isAutoModify".into(), json!(false));
                event.insert("codebaseEnable".into(), json!(false));
                event.insert("isMaxMode".into(), json!(true));
                event.insert("source".into(), json!("work"));
                event.insert("mentionContexts".into(), json!([]));
                event.insert("knowledgeId".into(), json!([]));
                event.insert("knowledgeName".into(), json!([]));
                event.insert("codebaseId".into(), json!(""));
                event.insert("mentionContextCount".into(), json!(0));
                event.insert("command".into(), json!(""));
                event.insert("inputAttemptId".into(), json!(new_conversation_id()));
                event.insert("recommendId".into(), json!(""));
                event.insert("skillId".into(), json!(""));
                event.insert("traceId".into(), json!(new_message_id()));
                event.insert("rootRequestId".into(), json!(new_message_id()));
                event.insert("parentConversationId".into(), json!(conversation));
                event.insert("agentName".into(), json!("cli"));
                event.insert("agentType".into(), json!("main"));
                report_desktop_events(account, vec![event]).await?;
                if index < need - 1 {
                    tokio::time::sleep(REPORT_GAP).await;
                }
            }
            Ok(format!("已补报 {need} 条对话事件"))
        }
        ActionKind::DesktopChat => {
            let conversation = new_conversation_id();
            let request = new_message_id();
            let message = new_message_id();
            let events = desktop_chat_sequence(
                &conversation,
                &request,
                &message,
                "fast-model",
                "fast-model",
            );
            report_desktop_events(account, events).await?;
            Ok("已上报桌面端对话事件链".to_string())
        }
        ActionKind::BuddyApp => {
            let events = desktop_buddy_app_sequence("cb_y5Dy46tPQGGWtueMxXbe", "企鹅教师助手");
            report_desktop_events(account, events).await?;
            Ok("已上报 Buddy 应用进入事件链".to_string())
        }
        ActionKind::Automation => {
            report_desktop_events(account, vec![desktop_automation_create_event("自动化")]).await?;
            Ok("已上报定时任务创建事件".to_string())
        }
        ActionKind::LibraryRead => {
            report_web_event(
                account,
                "web_element_click",
                "https://www.workbuddy.cn/space/d/o0KWYeynteVv06UnAZqIFm",
                "library_doc_intro_click",
                "WorkBuddy资料库介绍",
            )
            .await?;
            Ok("已上报资料库介绍阅读事件".to_string())
        }
        ActionKind::Template => {
            let templates = [
                ("1", "深度研究"),
                ("2", "周报生成"),
                ("3", "竞品分析"),
                ("4", "活动策划"),
                ("5", "代码评审"),
            ];
            for (index, (id, name)) in templates.iter().enumerate() {
                let conversation = new_conversation_id();
                let request = new_message_id();
                let events = desktop_template_use_sequence(&conversation, &request, id, name);
                report_desktop_events(account, events).await?;
                if index + 1 < templates.len() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
            Ok("已上报 template_used ×5".to_string())
        }
        ActionKind::Playbook => {
            let conversation = new_conversation_id();
            let request = new_message_id();
            let events = desktop_playbook_prompt_sequence(
                &conversation,
                &request,
                "pm-gtm-launch-plan",
                "新产品上市 GTM 发布计划一页纸",
            );
            report_desktop_events(account, events).await?;
            Ok("已上报灵感案例事件组".to_string())
        }
        ActionKind::Appearance => apply_appearance(account).await,
        ActionKind::FirstBuddy => {
            // 前置：活跃上报（解锁领养）。
            let conversation = new_conversation_id();
            let mut event = Map::new();
            event.insert("eventCode".into(), json!("chat_request_send"));
            event.insert("mode".into(), json!("craft"));
            event.insert("conversationId".into(), json!(conversation));
            event.insert("requestId".into(), json!(new_message_id()));
            event.insert("inputLength".into(), json!(12));
            event.insert("requestModelId".into(), json!("deepseek-v4.1-flash"));
            event.insert("requestModelName".into(), json!("DeepSeek-V4.1-Flash"));
            event.insert("source".into(), json!("work"));
            event.insert("agentName".into(), json!("cli"));
            event.insert("agentType".into(), json!("main"));
            report_desktop_events(account, vec![event]).await?;
            tokio::time::sleep(REPORT_GAP).await;
            adopt_first_buddy(account).await
        }
        ActionKind::SkillFresh => {
            let (conversation, request) = chat_for_request_id(account, "").await?;
            let tail: String = request
                .chars()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let mut events =
                desktop_chat_sequence(&conversation, &request, &format!("msg-{tail}"), "fast-model", "fast-model");
            for event in events.iter_mut() {
                if event["eventCode"] == json!("chat_message_response") {
                    event.insert("finishReason".into(), json!("tool_calls"));
                }
            }
            let mut skill = Map::new();
            skill.insert("eventCode".into(), json!("skill_info"));
            skill.insert("id".into(), json!("润泽小馆·日报撰写"));
            skill.insert("skillId".into(), json!("skill_2097350077599879168"));
            skill.insert("skillVersion".into(), json!("1.0.0"));
            skill.insert("toolStatus".into(), json!("success"));
            skill.insert("fileCount".into(), json!(56));
            skill.insert("source".into(), json!("workbuddy-desktop"));
            skill.insert("conversationId".into(), json!(conversation));
            skill.insert("requestId".into(), json!(request));
            skill.insert("messageId".into(), json!(format!("msg-{tail}")));
            skill.insert("requestModelId".into(), json!("fast-model"));
            skill.insert("requestModelName".into(), json!("fast-model"));
            skill.insert("traceId".into(), json!(request));
            events.push(skill);
            report_desktop_events(account, events).await?;
            Ok("已上报真实对话 + 技能加载事件".to_string())
        }
        ActionKind::ExpertBatch => {
            let expert_type = if action.code == "Expert_team_use_3" {
                "team"
            } else {
                "agent"
            };
            let experts = market_experts(account, expert_type, None).await?;
            if experts.is_empty() {
                return Err("专家市场列表为空".to_string());
            }
            let mut done = 0;
            for (index, expert) in experts.iter().enumerate() {
                if done >= action.count {
                    break;
                }
                if let Ok((conversation, request)) = chat_for_request_id(account, &expert.expert_id).await {
                    let tail: String = request
                        .chars()
                        .rev()
                        .take(8)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    let mut events = desktop_chat_sequence(
                        &conversation,
                        &request,
                        &format!("msg-{tail}"),
                        "fast-model",
                        "fast-model",
                    );
                    let summon = desktop_expert_summon_sequence(expert);
                    events.splice(0..0, summon);
                    events.push(desktop_expert_actual_use_event(
                        expert,
                        &conversation,
                        &request,
                        "craft",
                    ));
                    if report_desktop_events(account, events).await.is_ok() {
                        done += 1;
                    }
                }
                if index + 1 < experts.len() && done < action.count {
                    tokio::time::sleep(EXPERT_SUMMON_GAP).await;
                }
            }
            if done < action.count {
                return Err(format!("仅成功 {done}/{} 位专家", action.count));
            }
            Ok(format!("已对 {done} 位专家完成召唤+使用链"))
        }
        ActionKind::ExpertSingle => {
            // 目标专家 id 优先从任务自身的深链里取——活动指定的那一位才是判据，
            // 硬编码的 id 在活动换专家后就会失效。
            let jump_expert = find_task(account, action.code, action.mp)
                .await
                .ok()
                .flatten()
                .and_then(|task| expert_id_from_jump_url(&task.jump_url));

            let expert_id = jump_expert
                .clone()
                .unwrap_or_else(|| action.expert_id.to_string());
            if expert_id.is_empty() {
                return Err("无法确定目标专家（深链与内置表都没有 expert id）".to_string());
            }
            let mut expert = MarketExpert {
                expert_id: expert_id.clone(),
                expert_type: "agent".to_string(),
                display_name: action.expert_name.to_string(),
                profession: action.expert_profession.to_string(),
                version: action.expert_version.to_string(),
                category: String::new(),
            };
            // 市场列表里命中同 id 就用服务端的版本号与名称（以服务端为准）。
            // 昵称与职业名只在服务端给了非空值时才覆盖——判据事件里这两个字段
            // 都参与校验，任一为空都会被判不认。
            if let Ok(list) = market_experts(account, "agent", Some(&expert.profession)).await {
                for candidate in list {
                    if candidate.expert_id != expert.expert_id {
                        continue;
                    }
                    if !candidate.display_name.is_empty() {
                        expert.display_name = candidate.display_name;
                    }
                    if !candidate.profession.is_empty() {
                        expert.profession = candidate.profession;
                    }
                    if !candidate.version.is_empty() {
                        expert.version = candidate.version;
                    }
                    if !candidate.category.is_empty() {
                        expert.category = candidate.category;
                    }
                    break;
                }
            }
            report_desktop_events(account, desktop_expert_summon_sequence(&expert)).await?;
            let (conversation, request) = chat_for_request_id(account, &expert.expert_id).await?;
            let tail: String = request
                .chars()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let mut events = desktop_chat_sequence(
                &conversation,
                &request,
                &format!("msg-{tail}"),
                "fast-model",
                "fast-model",
            );
            for event in events.iter_mut() {
                if event["eventCode"] == json!("agent_task_created") {
                    event.insert("has_expert".into(), json!(true));
                    event.insert("expert_id".into(), json!(expert.expert_id));
                    event.insert("expert_name".into(), json!(expert.display_name));
                    event.insert("expert_industry_id".into(), json!(""));
                }
            }
            let mut use_event =
                desktop_expert_actual_use_event(&expert, &conversation, &request, "LOCAL");
            // 专家页直连召唤的真实样本：type 为空、cost 为 0。
            use_event.insert("type".into(), json!(""));
            use_event.insert("cost".into(), json!(0));
            events.push(use_event);
            report_desktop_events(account, events).await?;
            Ok(format!("已上报「{}」召唤+使用链", expert.display_name))
        }
        ActionKind::MiniChat => {
            let task = find_task(account, action.code, true).await?;
            let (current, target) = match &task {
                Some(t) if t.progress_known => (t.current, t.target),
                _ => (0, action.count),
            };
            let target = if target > 0 { target } else { action.count };
            let need = target - current;
            if need <= 0 {
                return Ok("进度已达标".to_string());
            }
            for index in 0..need {
                // 真人节奏间隔：连发会被上游反作弊判无效（进度先涨后退）。
                tokio::time::sleep(MP_CHAT_GAP).await;
                let conversation = new_conversation_id();
                report_mp_events(account, vec![mini_chat_send_event(&conversation)]).await?;
                if index < need - 1 {
                    // 额外抖动，贴近真人操作节奏。
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
            Ok(format!("已上报 {need} 次小程序对话"))
        }
        ActionKind::MiniExpert => {
            let experts = market_experts(account, "agent", None).await?;
            let expert = experts
                .first()
                .ok_or_else(|| "专家市场列表为空".to_string())?;
            report_mp_events(
                account,
                vec![mini_expert_use_event(&expert.expert_id, &expert.display_name)],
            )
            .await?;
            Ok("已上报小程序专家使用事件".to_string())
        }
        ActionKind::MiniAutomation => {
            report_desktop_events(account, vec![desktop_automation_create_event("自动化")]).await?;
            Ok("已上报定时任务创建事件".to_string())
        }
        ActionKind::MiniModelChat => {
            let conversation = new_conversation_id();
            let mut event = mini_chat_send_event(&conversation);
            event.insert("requestModelId".into(), json!("glm-5.2"));
            event.insert("requestModelName".into(), json!("GLM-5.2"));
            report_mp_events(account, vec![event]).await?;
            Ok("已上报小程序模型对话事件".to_string())
        }
        ActionKind::MiniPlaybook => {
            let mut cta = Map::new();
            cta.insert("eventCode".into(), json!("playbook_cta_click"));
            cta.insert("source".into(), json!("discover"));
            cta.insert("position".into(), json!(1));
            cta.insert("extVersion".into(), json!("2.2.8"));
            cta.insert("id".into(), json!("pm-gtm-launch-plan"));
            cta.insert("name".into(), json!("新产品上市 GTM 发布计划一页纸"));
            cta.insert("type".into(), json!("document"));
            cta.insert("categoryId".into(), json!(""));
            cta.insert("categoryName".into(), json!(""));
            cta.insert("skills".into(), json!(""));
            cta.insert("skillNames".into(), json!(""));

            let mut send = Map::new();
            send.insert("eventCode".into(), json!("playbook_prompt_send"));
            send.insert("source".into(), json!("discover"));
            send.insert("promptLength".into(), json!(96));
            send.insert("isOfficial".into(), json!(1));
            send.insert("conversationId".into(), json!(new_conversation_id()));
            send.insert("extVersion".into(), json!("2.2.8"));
            send.insert("id".into(), json!("pm-gtm-launch-plan"));
            send.insert("name".into(), json!("新产品上市 GTM 发布计划一页纸"));
            send.insert("type".into(), json!("document"));
            send.insert("categoryId".into(), json!(""));
            send.insert("categoryName".into(), json!(""));
            send.insert("skills".into(), json!(""));
            send.insert("skillNames".into(), json!(""));

            report_mp_events(account, vec![cta, send]).await?;
            Ok("已上报小程序灵感事件组".to_string())
        }
    }
}

/// 执行一个任务的完整闭环：接受 → 动作 → 回读 → 领奖。
async fn execute_task(account: &Value, action: &Action) -> TaskRunResult {
    let title = action.code.to_string();

    // 上游没下发这个任务（活动结束 / 前置未完成）：跳过，不算失败。
    let task = match find_task(account, action.code, action.mp).await {
        Ok(Some(task)) => task,
        // 小程序专属任务在默认口径列表查不到，需回落到小程序口径再找一次。
        Ok(None) if is_mp_code(action.code) => {
            match find_task(account, action.code, true).await {
                Ok(Some(task)) => task,
                Ok(None) => {
                    return TaskRunResult {
                        code: action.code.to_string(),
                        title,
                        status: "skipped",
                        message: "上游未下发该任务（活动可能已结束）".to_string(),
                        credit: 0,
                    }
                }
                Err(error) => {
                    return TaskRunResult {
                        code: action.code.to_string(),
                        title,
                        status: "failed",
                        message: format!("查询任务失败：{error}"),
                        credit: 0,
                    }
                }
            }
        }
        Ok(None) => {
            return TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "skipped",
                message: "上游未下发该任务（活动可能已结束）".to_string(),
                credit: 0,
            }
        }
        Err(error) => {
            return TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "failed",
                message: format!("查询任务失败：{error}"),
                credit: 0,
            }
        }
    };

    if task.claimed {
        return TaskRunResult {
            code: action.code.to_string(),
            title,
            status: "skipped",
            message: "已领取".to_string(),
            credit: 0,
        };
    }

    // 已达标但没领：直接领奖，不重复触发动作。
    if task.claimable() {
        return match claim_one(account, action.code, action.mp).await {
            Ok(credit) => TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "done",
                message: format!("已领取奖励（+{credit} 积分）"),
                credit,
            },
            Err(error) => TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "failed",
                message: format!("领奖失败：{error}"),
                credit: 0,
            },
        };
    }

    // 接受任务。
    if needs_accept(&task.accept_status) {
        if let Err(error) = accept_task(account, action.code, action.mp).await {
            return TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "failed",
                message: error,
                credit: 0,
            };
        }
    }

    // 触发行为。
    let action_message = match run_action(account, action).await {
        Ok(message) => message,
        Err(error) => {
            return TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "failed",
                message: error,
                credit: 0,
            }
        }
    };

    // 回读进度（异步计分，有界轮询）。
    let after = match wait_until_claimable(account, action.code, action.mp).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            return TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "progressed",
                message: format!("{action_message}；但上游未回读到该任务"),
                credit: 0,
            }
        }
        Err(error) => {
            return TaskRunResult {
                code: action.code.to_string(),
                title,
                status: "progressed",
                message: format!("{action_message}；回读失败：{error}"),
                credit: 0,
            }
        }
    };

    if after.claimed {
        return TaskRunResult {
            code: action.code.to_string(),
            title,
            status: "done",
            message: format!("{action_message}；本轮已入账"),
            credit: 0,
        };
    }

    if !after.claimable() {
        return TaskRunResult {
            code: action.code.to_string(),
            title,
            status: "progressed",
            message: format!(
                "{action_message}；但进度未达标（{}/{}），异步计分未归账，下次重试",
                after.current, after.target
            ),
            credit: 0,
        };
    }

    match claim_one(account, action.code, action.mp).await {
        Ok(credit) => TaskRunResult {
            code: action.code.to_string(),
            title,
            status: "done",
            message: format!("{action_message}；已领取奖励（+{credit} 积分）"),
            credit,
        },
        Err(error) => TaskRunResult {
            code: action.code.to_string(),
            title,
            status: "failed",
            message: format!("{action_message}；领奖失败：{error}"),
            credit: 0,
        },
    }
}

/// 一键完成当前账号的成长任务。
///
/// 逐个任务走完整闭环，单条失败不影响其余。返回每条任务的结果与积分合计。
pub async fn run_all(account: &Value) -> Value {
    if let Some(error) = envelope_token_error(account) {
        return json!({ "results": [], "error": error });
    }

    let mut results: Vec<Value> = Vec::new();
    let mut credit_total = 0i64;
    let mut notes: Vec<String> = Vec::new();

    for action in ACTIONS {
        let result = execute_task(account, action).await;
        credit_total += result.credit;
        results.push(result.to_json());
    }

    // 白名单外的任务：进度已达标却没人领奖时，积分会永久卡住。补领一次。
    // 两个口径都要扫——小程序专属任务只在 mp 列表里下发。
    for mp in [false, true] {
        let tasks = match list_tasks(account, mp).await {
            Ok(tasks) => tasks,
            Err(error) => {
                notes.push(format!("补领阶段{}口径任务列表未取到：{error}", if mp { "小程序" } else { "默认" }));
                continue;
            }
        };
        for task in tasks {
            if task.claimable() && action_for(&task.code).is_none() {
                match claim_one(account, &task.code, mp).await {
                    Ok(credit) => {
                        credit_total += credit;
                        results.push(json!({
                            "code": task.code,
                            "title": task.title,
                            "status": "done",
                            "message": format!("补领已达标奖励（+{credit} 积分）"),
                            "credit": credit,
                        }));
                    }
                    Err(error) => notes.push(format!("补领 {} 失败：{error}", task.code)),
                }
            }
        }
    }

    let mut out = json!({
        "results": results,
        "creditTotal": credit_total,
    });
    if !notes.is_empty() {
        out["note"] = json!(notes.join("；"));
    }
    out
}

/// 仅补领已达标但未领取的奖励（不触发任何动作）。
pub async fn claim_only(account: &Value) -> Value {
    if let Some(error) = envelope_token_error(account) {
        return json!({ "claimed": [], "error": error });
    }
    let mut claimed = Vec::new();
    let mut errors = Vec::new();
    let mut credit_total = 0i64;

    for mp in [false, true] {
        let tasks = match list_tasks(account, mp).await {
            Ok(tasks) => tasks,
            Err(error) => {
                errors.push(json!({ "scope": if mp { "小程序" } else { "默认" }, "error": error }));
                continue;
            }
        };
        for task in tasks {
            if !task.claimable() {
                continue;
            }
            match claim_one(account, &task.code, mp).await {
                Ok(credit) => {
                    credit_total += credit;
                    claimed.push(json!({ "code": task.code, "title": task.title, "credit": credit }));
                }
                Err(error) => errors.push(json!({ "code": task.code, "error": error })),
            }
        }
    }

    json!({ "claimed": claimed, "errors": errors, "creditTotal": credit_total })
}

/// 任务编号到展示名的映射（供前端显示）。
pub fn known_task_codes() -> HashMap<String, String> {
    ACTIONS
        .iter()
        .map(|a| (a.code.to_string(), a.desc.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_table_covers_expected_tasks() {
        for code in [
            "chat_5",
            "first_buddy",
            "RichMeow_Chat",
            "Buddy_App",
            "automation_1",
            "Library_read",
            "template_5",
            "playbook_prompt",
            "expert_5",
            "Expert_team_use_3",
            "Hp_Appearance",
            "skill_1",
            "Expert_lighthouse",
            "Sequential_Tasks_1",
            "Sequential_Tasks_3",
            "Sequential_Tasks_6",
        ] {
            assert!(action_for(code).is_some(), "缺少动作定义：{code}");
        }
    }

    #[test]
    fn mp_tasks_are_flagged() {
        assert!(is_mp_code("Sequential_Tasks_3"));
        assert!(!is_mp_code("chat_5"));
        assert!(action_for("Sequential_Tasks_3").unwrap().mp);
        assert!(!action_for("chat_5").unwrap().mp);
    }

    #[test]
    fn extract_request_id_finds_server_id() {
        let sse = "data: {\"id\":\"cmb-ead473f6d61f11f18c4d46ce91afba4a\",\"model\":\"glm-5.3-flash\"}";
        assert_eq!(
            extract_request_id(sse),
            Some("cmb-ead473f6d61f11f18c4d46ce91afba4a".to_string())
        );
    }

    #[test]
    fn extract_request_id_skips_non_server_ids() {
        // 消息 id 形态与 requestId 不同，不能误取。
        let sse = "data: {\"id\":\"msg-12345\",\"x\":1}";
        assert_eq!(extract_request_id(sse), None);
    }

    #[test]
    fn server_request_id_shape() {
        assert!(is_server_request_id("cmb-ead473f6d61f11f18c4d46ce91afba4a"));
        assert!(is_server_request_id("ead473f6d61f11f18c4d46ce91afba4a"));
        assert!(!is_server_request_id("short"));
        assert!(!is_server_request_id("cmb-EAD473F6D61F11F18C4D46CE91AFBA4A"));
    }

    #[test]
    fn parses_task_progress_variants() {
        let tasks = parse_tasks(&json!({ "tasks": [
            { "task_code": "a", "title": "A", "current": 2, "target": 5 },
            { "task_code": "b", "title": "B", "progress": { "current": 3, "target": 3 } },
            { "code": "c", "title": "C" }
        ]}));
        assert_eq!(tasks.len(), 3);
        assert_eq!((tasks[0].current, tasks[0].target), (2, 5));
        assert!(tasks[0].progress_known);
        assert!(tasks[1].claimable());
        assert!(!tasks[2].progress_known);
    }

    #[test]
    fn claimed_task_is_never_claimable() {
        let tasks = parse_tasks(&json!({ "tasks": [
            { "task_code": "t", "title": "x", "current": 1, "target": 1, "accept_status": "claimed" }
        ]}));
        assert!(tasks[0].claimed);
        assert!(!tasks[0].claimable());
    }

    #[test]
    fn needs_accept_matches_upstream_values() {
        assert!(needs_accept("not_accepted"));
        assert!(needs_accept(""));
        assert!(!needs_accept("accepted"));
        assert!(!needs_accept("claimed"));
    }

    #[test]
    fn known_task_codes_lists_actions() {
        let map = known_task_codes();
        assert!(map.contains_key("chat_5"));
        assert!(map.contains_key("Sequential_Tasks_6"));
    }

    #[test]
    fn parses_expert_id_from_jump_url() {
        // 实测深链形态：expertId 在百分号编码的 deeplink 参数里。
        let url = "https://www.workbuddy.cn/work/launch/?deeplink=workbuddy%3A%2F%2Fexperts%3FexpertId%3Dex_Weo3x2EZuSDm&buddy_type=workbuddy";
        assert_eq!(
            expert_id_from_jump_url(url),
            Some("ex_Weo3x2EZuSDm".to_string())
        );
    }

    #[test]
    fn parses_expert_id_from_plain_jump_url() {
        assert_eq!(
            expert_id_from_jump_url("https://x/?expertId=ex_abc123"),
            Some("ex_abc123".to_string())
        );
    }

    #[test]
    fn jump_url_without_expert_id_yields_none() {
        assert_eq!(expert_id_from_jump_url("workbuddy://expert"), None);
        assert_eq!(expert_id_from_jump_url(""), None);
        // 非 ex_ 前缀不算专家 id。
        assert_eq!(expert_id_from_jump_url("https://x/?expertId=abc"), None);
    }

    #[test]
    fn model_fallback_only_triggers_on_rejection_statuses() {
        assert!(model_rejected("对话失败（HTTP 400）：model not found"));
        assert!(model_rejected("对话失败（HTTP 422）：invalid model"));
        // 换模型解决不了的问题：直接返回原错误，别白烧一次请求。
        assert!(!model_rejected("对话失败（HTTP 401）：unauthorized"));
        assert!(!model_rejected("对话失败（HTTP 429）：rate limited"));
        assert!(!model_rejected("对话失败（HTTP 500）：server error"));
        assert!(!model_rejected("连接失败：dns error"));
    }

    #[test]
    fn daily_chat_uses_free_model() {
        assert_eq!(DAILY_CHAT_MODEL, "hy4flash-free");
    }

    #[test]
    fn task_entry_keeps_jump_url() {
        let tasks = parse_tasks(&json!({ "tasks": [
            { "task_code": "t", "title": "x", "jump_url": "workbuddy://expert" }
        ]}));
        assert_eq!(tasks[0].jump_url, "workbuddy://expert");
    }
}
