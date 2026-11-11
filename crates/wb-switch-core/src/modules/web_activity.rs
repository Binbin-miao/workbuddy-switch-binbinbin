//! 国际版每日活跃会话（网页通道）。
//!
//! 对照 88lin/workbuddy-auto-signin 与 workbuddy2api-hub 的实测结论：国际版**没有
//! 签到**（`/billing/meter/daily-checkin` 恒回 `10001 签到活动未开启`），但有一条
//! 独立链路能拿到官方「每日活跃」奖励——在网页端建一条 agent 会话并把它真正跑完。
//!
//! 关键点（踩过的坑，照抄结论）：
//! - 桌面端身份发 `chat/completions` **不计入**每日活跃，只有跑完的网页会话算；
//! - 只建会话不接沙箱，会话会永远停在 `CREATING`、零输出，同样不算；
//! - 上游对单账号的活跃会话（`status=working`）有硬上限 10 条，建了必须收尾，
//!   否则攒满后建会话直接 429 / 10105。**本模块自己不清名额**：命中上限就当天放弃，
//!   因为无法区分「本模块建的 Hi 会话」与「用户此刻正在用的会话」；
//! - 必须补一条 `session/set_model`，否则跑的是账号默认模型（Auto，按付费计费）。
//!
//! 与签到的关系：签到走 `billing` 体系、由服务端 `active` 开关决定；每日活跃走
//! `/console/as/conversations/` + ACP，没有 `active` 概念，跑完即计分。两者互不影响，
//! 本模块不触碰任何签到接口。
//!
//! 风险声明：这条链路是**社区逆向**出来的非官方接口，路径与报文随时可能变更；
//! `session/set_model` 已从 ACP 规范（`agentclientprotocol/agent-client-protocol`
//! 的 `docs/rfds/updates.mdx`）移除，调用失败时只能按会话默认模型跑完这一轮。

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use chrono::Timelike;
use serde_json::{json, Value};

use crate::modules::account::{find_account, variant_of};
use crate::modules::config::{now_ms, store_dir, RunFlagGuard};
use crate::modules::variant::WbVariant;

/// 网页端（控制台）origin。
const WEB_ORIGIN: &str = "https://www.workbuddy.ai";
/// 会话集合端点；单条会话的读/动作都在它后面拼 id。
const CONVERSATIONS_URL: &str = "https://www.workbuddy.ai/console/as/conversations/";
/// 免费模型：恒在候选列表里。
///
/// 官方国际版模型的积分倍率（`wb_catalog` 的 `credits` 字段，实测 2026-11）：
/// - `hy3`                 x0.00（免费，192k 上下文）→ 实测一次 40 段输出，成本 0.00
/// - `deepseek-v4.1-flash` x0.11（300k，effort=high）→ 实测一次 115 段输出，成本 9.36
///
/// 打卡只需要一句「Hi」加一段短回复，没必要用付费模型；`hy3` 不可用（限流 /
/// 下线）时才顺延到付费备选，且必须由用户显式授权（`allow_paid_model`）。
/// 注意网页通道**无法**单独下调思考档位：ACP 只提供 `session/set_model`，没有
/// reasoning effort 方法，所以 `deepseek-v4.1-flash` 的 `onlyReasoning: true /
/// defaultEffort: high` 改不动——省钱只能换模型，不能调参。
const FREE_MODEL: &str = "hy3";
/// 付费备用模型：仅在 `allow_paid_model == true` 时进入候选列表。
const PAID_MODEL: &str = "deepseek-v4.1-flash";
/// 沙箱域名后缀（E2B on 腾讯云 CloudStudio）。
const SANDBOX_SUFFIX: &str = ".sandbox.cloudstudio.club";
/// 单轮最长等待。
const TURN_TIMEOUT: Duration = Duration::from_secs(120);
/// 轮询会话状态的间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// 单账号同时存在的活跃会话上限（上游硬限制）。
const ACTIVE_LIMIT: usize = 10;
/// 网页端 UA（与官方 web app 一致，不带桌面端指纹）。
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// ACP 协议版本。
const PROTOCOL_VERSION: i64 = 1;
/// 每日活跃状态文件（记录各 uid 最近一次成功的本地日期）。
const STATE_FILE: &str = "daily_activity_state.json";
/// 每日活跃开关文件。
const ACTIVITY_CONFIG_FILE: &str = "daily_activity_config.json";
/// 调度轮询间隔：靠状态文件做当天去重，轮询只为「窗口内尽快补上」。
const CYCLE_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// 每日活跃的防重入标志：手动触发与后台轮次共用，避免同一账号被并发跑两次。
static ACTIVITY_RUNNING: AtomicBool = AtomicBool::new(false);

/// 本次可以尝试的模型序列：免费模型恒在，付费模型需显式授权。
fn model_candidates(allow_paid_model: bool) -> Vec<&'static str> {
    let mut candidates = vec![FREE_MODEL];
    if allow_paid_model {
        candidates.push(PAID_MODEL);
    }
    candidates
}

/// 抢每日活跃的运行权；占用中返回 `None`。
fn try_acquire_activity() -> Option<RunFlagGuard<'static>> {
    RunFlagGuard::try_acquire(&ACTIVITY_RUNNING)
}

/// 已有实例在跑时的返回体。
fn busy_result() -> Value {
    json!({"ok": false, "busy": true, "error": "每日活跃正在运行，请稍后"})
}

// ---------------------------------------------------------------------------
// 客户端构造：沙箱域名走 DoH 兜底
// ---------------------------------------------------------------------------

/// 用阿里 DoH 查 A 记录。返回第一个 IPv4。
///
/// 为什么要这一步：部分代理以 fake-ip 模式把 `*.sandbox.cloudstudio.club`
/// 解析成保留段地址，TCP 能连上但 TLS 握手被直接掐断
/// （`UNEXPECTED_EOF_WHILE_READING`）。真实地址在腾讯云 EdgeOne，直连正常。
async fn resolve_via_doh(host: &str) -> Option<IpAddr> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?;
    let url = format!("https://dns.alidns.com/resolve?name={host}&type=A");
    let resp = client
        .get(&url)
        .header("Accept", "application/dns-json")
        .send()
        .await
        .ok()?;
    let body: Value = resp.json().await.ok()?;
    body.get("Answer")?
        .as_array()?
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_i64) == Some(1))
        .filter_map(|item| item.get("data").and_then(Value::as_str))
        .find_map(|ip| ip.parse::<IpAddr>().ok())
}

/// 为沙箱地址构造专用客户端：把 host 固定解析到 DoH 查到的真实 IP。
///
/// `reqwest::ClientBuilder::resolve` 的覆盖优先于系统解析器，因此 fake-ip 不再生效；
/// 证书校验仍按原 host 名进行（SNI 不变）。DoH 也拿不到地址时不再静默降级——那种
/// 情况下走系统解析必然又落到 fake-ip，连接注定被掐断，明确报错比无声失败有用。
async fn sandbox_client(link: &str) -> Result<reqwest::Client, String> {
    let parsed = reqwest::Url::parse(link).map_err(|e| format!("沙箱地址非法: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "沙箱地址没有 host".to_string())?
        .to_string();
    let mut builder = reqwest::Client::builder()
        .timeout(TURN_TIMEOUT)
        .user_agent(WEB_USER_AGENT);
    if host.ends_with(SANDBOX_SUFFIX) {
        let Some(ip) = resolve_via_doh(&host).await else {
            return Err(
                "沙箱域名 DNS 解析失败（可能是代理的 fake-ip 模式），请更换网络后重试".to_string(),
            );
        };
        // reqwest 0.12 的 resolve 签名是 (domain: &str, addr: SocketAddr)。
        builder = builder.resolve(host.as_str(), SocketAddr::new(ip, sandbox_port(&parsed)));
    }
    builder.build().map_err(|e| format!("构造沙箱客户端失败: {e}"))
}

/// 沙箱连接的端口：地址里写死了就用它，否则按 scheme 的默认端口（https=443）。
fn sandbox_port(parsed: &reqwest::Url) -> u16 {
    parsed.port_or_known_default().unwrap_or(443)
}

/// 控制台 API 的客户端（走系统解析，域名是 www.workbuddy.ai）。
fn console_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(WEB_USER_AGENT)
        .build()
        .map_err(|e| format!("构造控制台客户端失败: {e}"))
}

/// 网页端出站头：只有 bearer 与 X-User-Id，没有桌面端指纹。
fn web_headers(account: &Value) -> Vec<(String, String)> {
    let token = account
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let uid = account.get("uid").and_then(Value::as_str).unwrap_or_default();
    vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("X-User-Id".to_string(), uid.to_string()),
        ("Accept".to_string(), "application/json, text/plain, */*".to_string()),
        ("Origin".to_string(), WEB_ORIGIN.to_string()),
        ("Referer".to_string(), format!("{WEB_ORIGIN}/app")),
    ]
}

fn with_headers(
    mut req: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (key, value) in headers {
        req = req.header(key.as_str(), value.as_str());
    }
    req
}

// ---------------------------------------------------------------------------
// 会话动作
// ---------------------------------------------------------------------------

/// 建一条网页会话，返回会话 id。
///
/// 建会话只是**排队**：不接沙箱、不请求这一轮，agent 永远不会跑。
async fn create_conversation(
    client: &reqwest::Client,
    account: &Value,
    prompt: &str,
    model: &str,
) -> Result<String, Value> {
    let body = json!({
        "prompt": prompt,
        "model": model,
        // 网页端建会话时固定带上这两项（抓包所得），保持请求形态一致。
        "conversationOrigin": "workbuddy-app",
        "plugins": [{"name": "weixinpay", "marketplace": "codebuddy-builtin"}],
    });
    let req = with_headers(
        client.post(CONVERSATIONS_URL).json(&body),
        &web_headers(account),
    );
    let resp = req
        .send()
        .await
        .map_err(|e| json!({"error": format!("建会话请求失败: {e}"), "phase": "create"}))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        let limit = status == 429
            && (text.contains("10105") || text.to_lowercase().contains("maximum number of active"));
        return Err(json!({
            "error": format!("建会话 HTTP {status}: {}", text.chars().take(160).collect::<String>()),
            "phase": "create",
            "limit": limit,
        }));
    }
    let payload: Value = serde_json::from_str(&text).unwrap_or(json!({}));
    let code = payload.get("code").and_then(Value::as_i64);
    let conversation = payload
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !matches!(code, Some(0) | None) || conversation.is_empty() {
        return Err(json!({
            "error": format!("建会话被拒: code={code:?} msg={}",
                payload.get("msg").and_then(Value::as_str).unwrap_or("")),
            "phase": "create",
            "retryable": false,
        }));
    }
    Ok(conversation)
}

/// 读一条会话；`suffix` 为空拿会话本身，`"/session"` 拿沙箱信息。
async fn get_conversation(
    client: &reqwest::Client,
    account: &Value,
    conversation: &str,
    suffix: &str,
) -> Result<Value, String> {
    let url = format!("{CONVERSATIONS_URL}{conversation}{suffix}");
    let req = with_headers(client.get(&url), &web_headers(account));
    let resp = req.send().await.map_err(|e| format!("查询会话失败: {e}"))?;
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).map_err(|e| format!("会话响应不是 JSON: {e}"))
}

async fn conversation_status(
    client: &reqwest::Client,
    account: &Value,
    conversation: &str,
) -> String {
    match get_conversation(client, account, conversation, "").await {
        Ok(payload) => payload
            .get("data")
            .and_then(|d| d.get("status"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Err(_) => String::new(),
    }
}

/// 对会话执行动作（`stop` / `delete`）。收尾失败不改判本轮结果。
async fn conversation_action(
    client: &reqwest::Client,
    account: &Value,
    conversation: &str,
    action: &str,
) -> bool {
    let url = format!("{CONVERSATIONS_URL}{conversation}/{action}");
    let req = with_headers(client.post(&url).json(&json!({})), &web_headers(account));
    match req.send().await {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(payload) => matches!(
                payload.get("code").and_then(Value::as_i64),
                Some(0) | None
            ),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// ACP：接上沙箱把这一轮跑完
// ---------------------------------------------------------------------------

/// 接上沙箱、依次发 `initialize` → `session/load` → `session/set_model` →
/// `session/prompt`，并在后台持续读 SSE 流（否则连接会被判空闲）。
///
/// 返回 `(connection_id, sse_task)`；调用方负责轮询会话状态到 `completed`。
async fn acp_open_turn(
    link: &str,
    token: &str,
    session_id: &str,
    cwd: &str,
    prompt: &str,
    model: &str,
) -> Result<(), String> {
    let client = sandbox_client(link).await?;
    // 1) 建立 SSE 通道，响应头里带 Acp-Connection-Id。
    let resp = client
        .get(link)
        .header("Accept", "text/event-stream")
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("SSE 连接失败: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("SSE 通道返回 HTTP {}", resp.status().as_u16()));
    }
    let connection_id = resp
        .headers()
        .get("Acp-Connection-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if connection_id.is_empty() {
        return Err("SSE 通道没有返回 Acp-Connection-Id".to_string());
    }
    // 后台读流：不消费事件，只保持连接存活。流关闭后任务自然退出。
    // 把 client 的句柄一起 move 进去，避免函数返回后连接池被回收。
    let keepalive = client.clone();
    tokio::spawn(async move {
        let _keep = keepalive;
        let mut stream = resp;
        while let Ok(Some(_)) = stream.chunk().await {}
    });

    // 2) 按网页端顺序发请求。每条都是独立 POST，带 Acp-Connection-Id。
    let calls: [(&str, Value, i64); 4] = [
        (
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
            }),
            1,
        ),
        (
            "session/load",
            json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []}),
            2,
        ),
        (
            "session/set_model",
            json!({"sessionId": session_id, "modelId": model}),
            3,
        ),
        (
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": prompt}],
            }),
            4,
        ),
    ];
    for (method, params, id) in calls {
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = client
            .post(link)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Acp-Connection-Id", &connection_id)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("{method} 请求失败: {e}"))?;
        let status = resp.status().as_u16();
        // set_model 失败不致命：本轮仍按会话默认模型跑，只是日志里留一句。
        if status != 200 && status != 202 {
            if method == "session/set_model" {
                eprintln!("[活跃] 设置模型 {model} 失败（HTTP {status}），本轮按会话默认模型跑");
                continue;
            }
            return Err(format!("{method} 返回 HTTP {status}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 状态持久化：避免同一天重复消耗额度
// ---------------------------------------------------------------------------

fn state_file() -> std::path::PathBuf {
    store_dir().join(STATE_FILE)
}

fn load_state() -> Value {
    std::fs::read_to_string(state_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}))
}

/// 该 uid 今天是否已经成功跑过一次。
fn done_today(uid: &str) -> bool {
    let state = load_state();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    state
        .get(uid)
        .and_then(Value::as_str)
        .map(|date| date == today)
        .unwrap_or(false)
}

fn mark_done(uid: &str) {
    let mut state = load_state();
    if let Some(map) = state.as_object_mut() {
        map.insert(
            uid.to_string(),
            json!(chrono::Local::now().format("%Y-%m-%d").to_string()),
        );
    }
    let _ = std::fs::create_dir_all(store_dir());
    if let Ok(text) = serde_json::to_string_pretty(&state) {
        // 原子写：写坏状态文件会让当天重复建会话、重复消耗额度。
        let _ = crate::modules::config::atomic_write(&state_file(), &text);
    }
}

// ---------------------------------------------------------------------------
// 开关与时间段
// ---------------------------------------------------------------------------

fn activity_config_file() -> std::path::PathBuf {
    store_dir().join(ACTIVITY_CONFIG_FILE)
}

/// 配置默认值：**默认关闭**。
///
/// 默认关是有意的：这条链路是社区逆向的非官方接口，且会真的建一条网页会话，所以
/// 必须由用户显式打开。`allow_paid_model` 同样默认 false：`hy3` 免费，付费备选
/// （`deepseek-v4.1-flash`，倍率 0.11）需要单独授权。
fn activity_config_default() -> Value {
    json!({"enabled": false, "allow_paid_model": false})
}

/// 解析配置文本；文件缺失、JSON 损坏、字段类型不对一律回落默认值（false）。
fn parse_activity_config(text: Option<&str>) -> Value {
    let mut cfg = activity_config_default();
    let Some(map) = text
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
    else {
        return cfg;
    };
    if let Some(enabled) = map.get("enabled").and_then(Value::as_bool) {
        cfg["enabled"] = json!(enabled);
    }
    if let Some(allow_paid_model) = map.get("allow_paid_model").and_then(Value::as_bool) {
        cfg["allow_paid_model"] = json!(allow_paid_model);
    }
    cfg
}

/// 每日活跃开关配置（默认关闭），返回 `{"enabled": bool, "allow_paid_model": bool}`。
pub fn load_activity_config() -> Value {
    let text = std::fs::read_to_string(activity_config_file()).ok();
    parse_activity_config(text.as_deref())
}

pub fn activity_enabled() -> bool {
    load_activity_config()
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 是否已授权使用付费备用模型。
fn activity_allow_paid_model() -> bool {
    load_activity_config()
        .get("allow_paid_model")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 各账号今日活跃状态，供账号页渲染 chip。
///
/// 返回 `{"today": "YYYY-MM-DD", "accounts": {"<uid>": "<最后成功日期>"}}`。
/// 前端只需判断 `accounts[uid] == today`。
pub fn activity_status() -> Value {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let state = load_state();
    let accounts = state
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    json!({"today": today, "accounts": accounts})
}

/// 落盘开关配置；缺失的字段沿用当前值（前端只发改动过的字段）。
///
/// 前端开关只提交 `enabled`，所以 `allow_paid_model` 不传时必须保住原值，否则每次
/// 切换开关都会把付费授权悄悄抹成 false。
pub fn save_activity_config(input: &Value) -> std::io::Result<()> {
    let current = load_activity_config();
    let flag = |key: &str| -> bool {
        input
            .get(key)
            .and_then(Value::as_bool)
            .or_else(|| current.get(key).and_then(Value::as_bool))
            .unwrap_or(false)
    };
    let cfg = json!({
        "enabled": flag("enabled"),
        "allow_paid_model": flag("allow_paid_model"),
    });
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("序列化活跃配置失败: {e}"),
        )
    })?;
    let _ = std::fs::create_dir_all(store_dir());
    // 原子写：半截 JSON 会让用户设置丢失（断电/崩溃时）。
    crate::modules::config::atomic_write(&activity_config_file(), &text)
}

/// 复用签到的时间段（`checkin_start` / `checkin_end`），本地时区的分钟区间。
///
/// 与 `checkin::window_minutes` 同一套语义：两端都合法且 `start < end` 才算生效，
/// 否则视为不限制。这里单独实现而不复用，是为了不改 checkin 模块的可见性。
fn activity_window(cfg: &Value) -> Option<(u32, u32)> {
    let (start_hour, start_minute) = cfg
        .get("checkin_start")
        .and_then(Value::as_str)
        .and_then(crate::modules::config::parse_clock)?;
    let (end_hour, end_minute) = cfg
        .get("checkin_end")
        .and_then(Value::as_str)
        .and_then(crate::modules::config::parse_clock)?;
    let start = start_hour * 60 + start_minute;
    let end = end_hour * 60 + end_minute;
    (start < end).then_some((start, end))
}

fn clock_label(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// 时间窗下界的小时：取签到配置的 `chat_after_hour`，缺省/越界一律 9。
fn effective_after_hour(cfg: &Value) -> u32 {
    match cfg.get("chat_after_hour").and_then(Value::as_i64) {
        Some(hour) if (0..=23).contains(&hour) => hour as u32,
        _ => 9,
    }
}

/// 当前时刻是否允许跑：显式窗口优先，窗口未生效时退化为「到点之后」的下界。
///
/// 窗口未配置时**不能**当作「完全不限」——那会让调度一启动就发请求（凌晨也算活跃，
/// 上游对账日界不确定），所以至少拦到下界小时。
fn activity_time_gate(window: Option<(u32, u32)>, hour: u32, minute: u32, after_hour: u32) -> bool {
    match window {
        Some((start, end)) => {
            let minutes = hour * 60 + minute;
            minutes >= start && minutes < end
        }
        None => crate::modules::daily_chat::should_chat_now(hour, after_hour),
    }
}

// ---------------------------------------------------------------------------
// 编排
// ---------------------------------------------------------------------------

/// 用指定模型完整跑一次：建会话 → 取沙箱 → ACP → 轮询 → 收尾。
///
/// 返回里 `ok=true` 表示这一轮跑完；`limitBlocked=true` 表示活跃名额满了，换模型也
/// 无济于事，调用方应直接终止候选循环。**命中上限时不清理任何既有会话**：那些会话
/// 可能是用户此刻正在用的，宁可当天活跃失败也不得中断用户会话。
async fn attempt_with_model(client: &reqwest::Client, account: &Value, model: &str) -> Value {
    let prompt = "Hi";
    let started = now_ms();
    let mut result = json!({"ok": false, "model": model, "phase": "create"});

    // 1) 建会话。
    let conversation = match create_conversation(client, account, prompt, model).await {
        Ok(id) => id,
        Err(failure) => {
            if failure.get("limit").and_then(Value::as_bool) == Some(true) {
                result["limitBlocked"] = json!(true);
                result["error"] = json!(format!(
                    "活跃会话已达上限（{ACTIVE_LIMIT}），请到网页端结束旧对话后重试"
                ));
            } else {
                result["error"] = failure
                    .get("error")
                    .cloned()
                    .unwrap_or_else(|| json!("建会话失败"));
            }
            return result;
        }
    };
    result["conversation"] = json!(conversation);
    result["phase"] = json!("session");

    // 2) 取沙箱 link / token。
    let session = match get_conversation(client, account, &conversation, "/session").await {
        Ok(payload) => payload.get("data").cloned().unwrap_or_else(|| json!({})),
        Err(error) => {
            conversation_action(client, account, &conversation, "delete").await;
            result["error"] = json!(format!("取沙箱失败: {error}"));
            return result;
        }
    };
    let link = session
        .get("link")
        .or_else(|| session.get("endpoint"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let token = session
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session_id = session
        .get("sessionId")
        .or_else(|| session.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| conversation.clone());
    let cwd = session
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("/workspace")
        .to_string();
    if link.is_empty() || token.is_empty() {
        conversation_action(client, account, &conversation, "delete").await;
        result["error"] = json!("沙箱未就绪（没有 link/token）");
        return result;
    }

    // 3) 接上沙箱并请求这一轮。
    result["phase"] = json!("turn");
    if let Err(error) = acp_open_turn(&link, &token, &session_id, &cwd, prompt, model).await {
        conversation_action(client, account, &conversation, "delete").await;
        result["error"] = json!(error);
        result["elapsedMs"] = json!(now_ms() - started);
        return result;
    }

    // 4) 轮询到 completed。
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    let mut status = String::new();
    let mut error = String::new();
    loop {
        if tokio::time::Instant::now() >= deadline {
            let shown = if status.is_empty() { "未知" } else { status.as_str() };
            error = format!(
                "会话在 {}s 内没有跑完（状态={shown}）",
                TURN_TIMEOUT.as_secs()
            );
            break;
        }
        status = conversation_status(client, account, &conversation).await;
        if status == "completed" {
            break;
        }
        if status == "failed" || status == "error" {
            error = format!("会话状态={status}");
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // 5) 收尾：成功保留（stop 只改状态），失败删掉免得网页端堆满「Hi」。
    let ok = error.is_empty() && status == "completed";
    conversation_action(client, account, &conversation, "stop").await;
    if !ok {
        conversation_action(client, account, &conversation, "delete").await;
    }

    result["ok"] = json!(ok);
    result["status"] = json!(status);
    result["elapsedMs"] = json!(now_ms() - started);
    if ok {
        result["msg"] = json!(format!(
            "每日活跃会话完成（{model}，{} ms）",
            now_ms() - started
        ));
    } else {
        result["error"] = json!(error);
    }
    result
}

/// 对单个账号跑一次每日活跃会话。
///
/// 按 [`model_candidates`] 从便宜到贵依次尝试，成功即停；只有「活跃名额满」会提前
/// 终止（换模型也建不出会话）。
///
/// 返回 `{"ok": bool, "status": ..., "conversation": ..., "model": ...,
/// "modelsTried": [...], "elapsedMs": ..., "error": ...}`。
/// `skipped` 表示今天已经做过；`busy` 表示已有一个实例在跑。
pub async fn run_daily_activity(account_id: &str) -> Value {
    // 防重入：手动触发与后台轮次不能并发进同一账号（会双倍消耗额度）。
    let Some(_guard) = try_acquire_activity() else {
        return busy_result();
    };
    let Some(account) = find_account(account_id) else {
        return json!({"ok": false, "error": "账号不存在"});
    };
    if variant_of(&account) != WbVariant::Ai {
        return json!({
            "ok": false,
            "error": "每日活跃会话仅国际版可用；国内版走签到",
            "unsupportedVariant": true,
        });
    }
    let uid = account
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if done_today(&uid) {
        return json!({"ok": true, "skipped": true, "msg": "今日活跃已完成，跳过"});
    }
    let client = match console_client() {
        Ok(client) => client,
        Err(error) => return json!({"ok": false, "error": error}),
    };

    let mut tried: Vec<&str> = Vec::new();
    let mut last = json!({"ok": false, "error": "没有可用的模型候选"});
    for model in model_candidates(activity_allow_paid_model()) {
        tried.push(model);
        let outcome = attempt_with_model(&client, &account, model).await;
        if outcome.get("ok").and_then(Value::as_bool) == Some(true) {
            mark_done(&uid);
            let mut done = outcome;
            done["modelsTried"] = json!(tried);
            return done;
        }
        let blocked = outcome
            .get("limitBlocked")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        last = outcome;
        if blocked {
            break;
        }
    }
    last["modelsTried"] = json!(tried);
    last
}

/// 后台调度入口：每天对每个国际版账号跑一次，时间段跟随签到配置。
///
/// 去重靠 `daily_activity_state.json`（uid → 成功日期），所以这个函数可以被高频
/// 调用：当天已完成的账号在 `run_daily_activity` 里直接 `skipped`，不会发任何
/// 请求。轮询只是为了让「窗口内失败后还能补一次」。
///
/// 时间段的语义与签到一致：两端都合法且 `start < end` 才生效；未设置时退化为
/// 「`chat_after_hour` 之后」（默认 9 点），不会被当成完全不限。
pub async fn run_daily_activity_cycle() -> Value {
    if !activity_enabled() {
        return json!({"status": "skipped", "reason": "disabled"});
    }
    let checkin_cfg = crate::modules::config::load_checkin_config();
    let now = chrono::Local::now();
    let window = activity_window(&checkin_cfg);
    let allowed =
        activity_time_gate(window, now.hour(), now.minute(), effective_after_hour(&checkin_cfg));
    if !allowed {
        return match window {
            Some((start, end)) => json!({
                "status": "skipped",
                "reason": "outside_activity_window",
                "window": format!("{}-{}", clock_label(start), clock_label(end)),
            }),
            None => json!({
                "status": "skipped",
                "reason": "before_activity_hour",
                "afterHour": effective_after_hour(&checkin_cfg),
            }),
        };
    }
    let result = run_daily_activity_all().await;
    // 后台轮询每 30 分钟一次，只记「这一轮真跑了」和「这一轮失败」两种，
    // skipped 不刷日志（否则一天几十行噪音）。
    if let Some(rows) = result.get("accounts").and_then(Value::as_array) {
        for row in rows {
            let inner = row.get("result").unwrap_or(&Value::Null);
            let ok = inner.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let skipped = inner.get("skipped").and_then(Value::as_bool).unwrap_or(false);
            let who = row
                .get("uid")
                .and_then(Value::as_str)
                .map(|uid| uid.chars().take(8).collect::<String>())
                .unwrap_or_else(|| "?".to_string());
            if ok && !skipped {
                let msg = inner.get("msg").and_then(Value::as_str).unwrap_or("完成");
                eprintln!("[活跃] {who} {msg}");
            } else if !ok {
                let err = inner
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                eprintln!("[活跃] {who} 失败：{err}");
            }
        }
    }
    result
}

/// 调度轮询间隔，供宿主直接使用。
pub fn cycle_interval() -> Duration {
    CYCLE_INTERVAL
}

/// 给调用方看的入口：遍历所有国际版账号跑一轮（手动触发，不受开关与时间段限制）。
///
/// 账号之间按 `chat_gap_ms` 错开（3~8 秒，同一天同一账号恒定），最后一个不等待：
/// 连续建会话容易触发上游风控，间隔也让日志可读。
pub async fn run_daily_activity_all() -> Value {
    let accounts = crate::modules::account::load_accounts();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let eligible: Vec<&Value> = accounts
        .iter()
        .filter(|acc| variant_of(acc) == WbVariant::Ai)
        .collect();
    let mut rows = Vec::new();
    for (index, account) in eligible.iter().enumerate() {
        let uid = account.get("uid").and_then(Value::as_str).unwrap_or_default();
        if index > 0 {
            let gap = crate::modules::daily_chat::chat_gap_ms(uid, &today);
            tokio::time::sleep(Duration::from_millis(gap)).await;
        }
        let id = account.get("id").and_then(Value::as_str).unwrap_or_default();
        let row = run_daily_activity(id).await;
        rows.push(json!({
            "accountId": id,
            "uid": account.get("uid").cloned().unwrap_or(Value::Null),
            "result": row,
        }));
    }
    json!({"accounts": rows})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把配置序列化成磁盘形态，用于读写回环。
    fn 配置文本(enabled: bool, allow_paid_model: bool) -> String {
        json!({"enabled": enabled, "allow_paid_model": allow_paid_model}).to_string()
    }

    #[test]
    fn 配置_默认关闭且解析失败也关闭() {
        let 缺文件 = parse_activity_config(None);
        assert_eq!(缺文件["enabled"], json!(false));
        assert_eq!(缺文件["allow_paid_model"], json!(false));

        let 坏文本 = parse_activity_config(Some("{ 这不是 JSON"));
        assert_eq!(坏文本["enabled"], json!(false));

        // 字段类型不对（字符串 "true" / 数字 1）同样回落 false，不得猜成 true。
        let 非布尔 = parse_activity_config(Some(r#"{"enabled":"true","allow_paid_model":1}"#));
        assert_eq!(非布尔["enabled"], json!(false));
        assert_eq!(非布尔["allow_paid_model"], json!(false));
    }

    #[test]
    fn 模型候选_默认只含免费模型() {
        assert_eq!(model_candidates(false), vec!["hy3"]);
    }

    #[test]
    fn 模型候选_开启付费授权后追加付费模型() {
        assert_eq!(model_candidates(true), vec!["hy3", "deepseek-v4.1-flash"]);
    }

    #[test]
    fn 防重入_标志占用时直接返回busy() {
        let 第一把 = try_acquire_activity();
        assert!(第一把.is_some(), "首次获取应当成功");
        assert!(try_acquire_activity().is_none(), "占用中第二次必须失败");
        assert_eq!(busy_result()["busy"], json!(true));
        assert_eq!(busy_result()["ok"], json!(false));
        drop(第一把);
        assert!(try_acquire_activity().is_some(), "释放后应能再次获取");
    }

    #[test]
    fn 时间窗_未配置窗口时九点前不跑() {
        // 窗口未生效（None）→ 退化为「到点之后」的下界，默认 9 点。
        assert!(!activity_time_gate(None, 8, 59, 9), "8:59 早于下界不该跑");
        assert!(activity_time_gate(None, 9, 0, 9), "9:00 到点应跑");
        assert!(!activity_time_gate(None, 0, 30, 9), "0:30 也不该跑");
        // 显式窗口生效时按窗口判定，与下界无关（窗口 9:00-12:00，下界令为 23 点）。
        assert!(activity_time_gate(Some((540, 720)), 9, 0, 23));
        assert!(!activity_time_gate(Some((540, 720)), 12, 0, 9));
    }

    #[test]
    fn 时间窗_下界配置越界回落到九点() {
        assert_eq!(effective_after_hour(&json!({})), 9);
        assert_eq!(effective_after_hour(&json!({"chat_after_hour": 25})), 9);
        assert_eq!(effective_after_hour(&json!({"chat_after_hour": -1})), 9);
        assert_eq!(effective_after_hour(&json!({"chat_after_hour": 7})), 7);
    }

    #[test]
    fn 配置_原子写后开关与付费授权可读回() {
        let path = std::env::temp_dir().join(format!("wb-activity-cfg-{}.json", std::process::id()));
        crate::modules::config::atomic_write(&path, &配置文本(true, true)).expect("原子写失败");
        let text = std::fs::read_to_string(&path).expect("读回失败");
        let 读回 = parse_activity_config(Some(&text));
        assert_eq!(读回["enabled"], json!(true));
        assert_eq!(读回["allow_paid_model"], json!(true));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn 沙箱_端口缺省回落443() {
        let 默认地址 =
            reqwest::Url::parse("https://x.sandbox.cloudstudio.club/acp").expect("URL 非法");
        assert_eq!(sandbox_port(&默认地址), 443);
        let 显式端口 = reqwest::Url::parse("https://h:8443/acp").expect("URL 非法");
        assert_eq!(sandbox_port(&显式端口), 8443);
    }
}
