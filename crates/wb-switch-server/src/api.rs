//! HTTP API 层：把 wb-switch-core 暴露为本地 REST 接口，供 webui（浏览器）调用。
//!
//! 路由设计对应 Python 版 server.py 与桌面端 commands.rs。仅绑定 127.0.0.1，
//! token 不出本机。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, RawQuery};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rust_embed::RustEmbed;
use serde::Serialize;
use serde_json::{json, Value};

use wb_switch_core::modules::{
    account, archive, auth_file, checkin, codebuddy_cli, codebuddy_cn_ide, codebuddy_ide,
    codebuddy_ide_session, codebuddy_ide_session_sync, config, credit_usage, credits,
    export_import, jetbrains, limits, memory, mirror, notifications, oauth, process,
    rate_limit_events, rate_limit_hook, refresh, rotate, session, session_groups, switch,
    token_stats, travel, update, variant::WbVariant, vscode_ext, vscode_session,
    vscode_session_sync, web_activity,
};

/// 归档接口的请求体上限。
///
/// 取值由 core 的限额倒推：`archive::zip::MAX_TOTAL_BYTES` 是 **64 MiB 原始字节**，
/// 而浏览器只能传 base64（体积 ×4/3 ≈ 85.3 MiB），再留一点 JSON 包装余量 → 96 MiB。
/// 若这里比「64 MiB × 4/3」小，就会出现「导出成功、同一个包导不回来」：请求在
/// JSON 解析前就被 axum 413 掉，前端只能看到「导入失败」。本机真实数据（session-links
/// 的 baselines 整包约 37 MiB）已经踩过这条线，所以别再往下调。
///
/// 只挂在这三条 POST 上，不动全局上限——切号这类小接口仍是 axum 默认的 2 MiB，
/// 免得大 body 也被无谓地读进内存。代价是导入时单请求峰值内存约 96 MiB（body）
/// + 解码后 zip（≤64 MiB），可接受：同一时刻只会有一个浏览器在导入。
const ARCHIVE_BODY_LIMIT: usize = 96 * 1024 * 1024;

/// WorkBuddy 运行状态缓存：Windows 上检测要跑 tasklist（慢），缓存几秒避免
/// 前端切 tab 频繁触发命令行导致卡顿/闪窗。按档位分别缓存。
#[cfg(target_os = "windows")]
static RUNNING_CACHE: Mutex<Option<(Instant, bool, WbVariant)>> = Mutex::new(None);

fn cached_workbuddy_running(variant: WbVariant) -> bool {
    #[cfg(target_os = "windows")]
    {
        let mut cache = RUNNING_CACHE.lock().unwrap();
        if let Some((t, v, cached_variant)) = cache.as_ref() {
            if *cached_variant == variant && t.elapsed() < Duration::from_secs(3) {
                return *v;
            }
        }
        let v = process::is_workbuddy_running(variant);
        *cache = Some((Instant::now(), v, variant));
        v
    }
    #[cfg(not(target_os = "windows"))]
    {
        process::is_workbuddy_running(variant)
    }
}

#[derive(RustEmbed)]
#[folder = "../../dist/"]
struct Assets;

/// 切换进度缓存：webui 通过 GET /api/switch/progress 轮询。
static SWITCH_PROGRESS: Mutex<Option<String>> = Mutex::new(None);
static SWITCH_RUNNING: Mutex<bool> = Mutex::new(false);
/// 关联会话预览进度：`(已核对组数, 待核对总组数)`。webui 通过
/// GET /api/session-links/progress 轮询；`None` 表示当前没有在跑的预览。
static LINKS_PREVIEW_PROGRESS: Mutex<Option<(usize, usize)>> = Mutex::new(None);
/// 跨档预览进度：与普通预览分开计数。两路预览共用一个前端轮询端点，共用一个槽时
/// 一路结束/被取消会把手写 `= None` 落到另一路正在跑的进度上。
static LINKS_PREVIEW_CROSS_PROGRESS: Mutex<Option<(usize, usize)>> = Mutex::new(None);
/// Token 统计扫描进度：`(已扫描文件数, 待扫描文件总数)`。webui 通过
/// GET /api/token-stats/progress 轮询；`None` 表示当前没有在跑的扫描。
static TOKEN_STATS_PROGRESS: Mutex<Option<(usize, usize)>> = Mutex::new(None);
/// 关联会话列表加载进度：`(已组装组数, 待组装总组数)`。webui 通过
/// GET /api/session-groups/progress 轮询；`None` 表示当前没有在跑的加载。
static SESSION_GROUPS_PROGRESS: Mutex<Option<(usize, usize)>> = Mutex::new(None);

/// 进度槽的 RAII 复位。
///
/// handler 被 `.await` 期间取消（客户端断开、页面切走）时，写在 await 之后的
/// `= None` 永远不会执行，static 会永久停在最后一帧，之后任何页面一进加载态
/// 就显示上一次的陈旧进度。用 Drop 兜住这条路径。
struct ProgressReset(&'static Mutex<Option<(usize, usize)>>);

impl Drop for ProgressReset {
    fn drop(&mut self) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// 切号状态的 RAII 守卫：把「判重 → 置位 → 复位」收进一个接口极简的深模块。
///
/// 客户端在 `spawn_blocking(...).await` 期间断开时 handler future 被 drop，写在
/// await 之后的手动 `= false` 永不执行，static 会永久停在 running=true，之后所有
/// `/api/switch` 恒 409 直到重启进程。Drop 兜住这条取消路径。
struct SwitchStateGuard;

impl SwitchStateGuard {
    /// 原子抢占切号状态；已有任务在跑时返回 None，由调用方回 409。
    ///
    /// 判重必须留在构造函数内：若先构造 guard 再判重，409 早返回时 Drop 会把
    /// **别人正在跑**的 running 误清成 false，反而放行第二个切号。
    fn acquire() -> Option<Self> {
        let mut running = SWITCH_RUNNING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *running {
            return None;
        }
        *running = true;
        *SWITCH_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some("开始切换账号…".to_string());
        Some(Self)
    }
}

impl Drop for SwitchStateGuard {
    fn drop(&mut self) {
        // running 与 progress 是两条独立复位路径：漏前者恒 409，漏后者留陈旧文案。
        *SWITCH_RUNNING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        *SWITCH_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

pub fn router() -> Router {
    Router::new()
        .route("/api/status", get(api_status))
        .route("/api/accounts", get(api_accounts))
        .route("/api/codebuddy-cli/status", get(api_codebuddy_cli_status))
        .route(
            "/api/codebuddy-cli/install-helper",
            post(api_codebuddy_cli_install_helper),
        )
        .route("/api/codebuddy-cli/switch", post(api_codebuddy_cli_switch))
        .route(
            "/api/codebuddy-cn-ide/status",
            get(api_codebuddy_cn_ide_status),
        )
        .route(
            "/api/codebuddy-cn-ide/switch",
            post(api_codebuddy_cn_ide_switch),
        )
        .route(
            "/api/codebuddy-cn-ide/detect",
            post(api_codebuddy_cn_ide_detect),
        )
        .route(
            "/api/codebuddy-cn-ide/sessions",
            get(api_codebuddy_cn_ide_sessions),
        )
        .route(
            "/api/codebuddy-cn-ide/session-links",
            post(api_codebuddy_cn_ide_session_links_preview),
        )
        .route("/api/codebuddy-ide/status", get(api_codebuddy_ide_status))
        .route("/api/codebuddy-ide/switch", post(api_codebuddy_ide_switch))
        .route("/api/codebuddy-ide/detect", post(api_codebuddy_ide_detect))
        .route(
            "/api/codebuddy-ide/sessions",
            get(api_codebuddy_intl_ide_sessions),
        )
        .route(
            "/api/codebuddy-ide/session-links",
            post(api_codebuddy_intl_ide_session_links_preview),
        )
        .route("/api/jetbrains/status", get(api_jetbrains_status))
        .route("/api/jetbrains/switch", post(api_jetbrains_switch))
        .route("/api/jetbrains/detect", post(api_jetbrains_detect))
        .route("/api/vscode-ext/status", get(api_vscode_ext_status))
        .route("/api/vscode-ext/sessions", get(api_vscode_ext_sessions))
        .route("/api/vscode-ext/switch", post(api_vscode_ext_switch))
        .route("/api/vscode-ext/detect", post(api_vscode_ext_detect))
        .route(
            "/api/vscode-ext/restart-precheck",
            post(api_vscode_restart_precheck),
        )
        .route(
            "/api/vscode-ext/session-links",
            post(api_vscode_ext_session_links_preview),
        )
        .route("/api/delete", post(api_delete))
        .route(
            "/api/update-account-display",
            post(api_update_account_display),
        )
        .route("/api/oauth/start", post(api_oauth_start))
        .route("/api/oauth/status", post(api_oauth_status))
        .route("/api/import-local", post(api_import_local))
        .route("/api/export-accounts", post(api_export_accounts))
        .route(
            "/api/export-accounts-to-path",
            post(api_export_accounts_to_path),
        )
        .route("/api/import/preview", post(api_preview_import))
        .route("/api/import", post(api_import))
        .route("/api/switch", post(api_switch))
        .route("/api/switch/progress", get(api_switch_progress))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/account", get(api_account_sessions))
        .route("/api/sessions/copy", post(api_copy_sessions))
        .route("/api/sessions/copy-cross", post(api_copy_sessions_cross))
        .route(
            "/api/session-links/preview",
            post(api_session_links_preview),
        )
        .route("/api/mirror/preview", post(api_mirror_preview))
        .route("/api/mirror/restore", post(api_mirror_restore))
        .route(
            "/api/sessions/deleted-sync",
            get(api_deleted_sync_preview),
        )
        .route(
            "/api/session-links/progress",
            get(api_session_links_progress),
        )
        .route(
            "/api/session-links/preview-cross",
            post(api_session_links_preview_cross),
        )
        .route("/api/session-sync/cross", post(api_session_sync_cross))
        .route("/api/session-groups/list", post(api_list_session_groups))
        .route(
            "/api/session-groups/progress",
            get(api_session_groups_progress),
        )
        .route("/api/session-groups/detail", post(api_get_session_group))
        .route(
            "/api/session-groups/preview",
            post(api_preview_session_group_pair),
        )
        .route(
            "/api/session-groups/sync",
            post(api_sync_session_group_pair),
        )
        .route(
            "/api/session-groups/unify",
            post(api_sync_session_group_unify),
        )
        .route(
            "/api/session-groups/sync-safe",
            post(api_sync_session_group_safe_batch),
        )
        .route(
            "/api/session-groups/add",
            post(api_add_session_group_member),
        )
        .route(
            "/api/session-groups/unlink",
            post(api_unlink_session_group_member),
        )
        .route("/api/session-groups/delete", post(api_delete_session_group))
        .route(
            "/api/session-groups/copy-linked",
            post(api_copy_linked_sessions),
        )
        .route("/api/checkin/status", get(api_checkin_status))
        .route("/api/credits", post(api_credits))
        .route("/api/credits/stats", get(api_credit_statistics))
        .route("/api/token-stats", get(api_token_statistics))
        .route(
            "/api/token-stats/progress",
            get(api_token_statistics_progress),
        )
        .route("/api/rate-limits", get(api_rate_limits))
        .route(
            "/api/rate-limits/hook-status",
            get(api_rate_limit_hook_status),
        )
        .route(
            "/api/rate-limits/install-hook",
            post(api_install_rate_limit_hook),
        )
        .route(
            "/api/rate-limits/uninstall-hook",
            post(api_uninstall_rate_limit_hook),
        )
        .route(
            "/api/rate-limits/config",
            get(api_rate_limit_config).post(api_save_rate_limit_config),
        )
        .route("/api/checkin", post(api_checkin))
        .route("/api/checkin/all", post(api_checkin_all))
        // 国际版每日活跃会话（网页通道）：与签到完全独立的一条链路。
        .route("/api/activity/daily", post(api_daily_activity))
        .route("/api/activity/daily/all", post(api_daily_activity_all))
        .route(
            "/api/activity/config",
            get(api_activity_config).post(api_save_activity_config),
        )
        .route("/api/activity/status", get(api_activity_status))
        .route(
            "/api/checkin/config",
            get(api_checkin_config).post(api_save_checkin_config),
        )
        .route("/api/checkin/logs", get(api_checkin_logs))
        .route("/api/notifications", get(api_notifications))
        .route("/api/notifications/record", post(api_record_notification))
        .route("/api/notifications/clear", post(api_clear_notifications))
        .route("/api/travel/status", get(api_travel_status))
        .route(
            "/api/travel/config",
            get(api_travel_config).post(api_save_travel_config),
        )
        .route(
            "/api/rotate/config",
            get(api_rotate_config).post(api_save_rotate_config),
        )
        .route("/api/rotate/status", get(api_rotate_status))
        .route("/api/rotate/run", post(api_rotate_run))
        .route("/api/rotate/logs", get(api_rotate_logs))
        .route("/api/refresh-token", post(api_refresh_token))
        .route("/api/update/check", get(api_update_check))
        .route(
            "/api/update/config",
            get(api_update_config).post(api_save_update_config),
        )
        .route("/api/archive/groups", get(api_archive_groups))
        .route(
            "/api/archive/export",
            post(api_archive_export).layer(DefaultBodyLimit::max(ARCHIVE_BODY_LIMIT)),
        )
        .route(
            "/api/archive/inspect",
            post(api_archive_inspect).layer(DefaultBodyLimit::max(ARCHIVE_BODY_LIMIT)),
        )
        .route(
            "/api/archive/import",
            post(api_archive_import).layer(DefaultBodyLimit::max(ARCHIVE_BODY_LIMIT)),
        )
        .fallback(static_handler)
}

fn json_ok<T: Serialize>(v: T) -> Response {
    Json(v).into_response()
}

fn json_err(e: String, code: StatusCode) -> Response {
    (code, Json(json!({ "ok": false, "error": e }))).into_response()
}

/// 从 query string 解析档位（缺省国内版）。与 Tauri 命令的可选 `variant` 参数同义。
fn query_variant(query: Option<&str>) -> WbVariant {
    let raw = query.unwrap_or("").split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == "variant").then_some(value)
    });
    WbVariant::parse(raw)
}

/// 从请求体解析档位（缺省国内版）。与 Tauri 命令的可选 `variant` 参数同义。
fn body_variant(body: &Value) -> WbVariant {
    WbVariant::parse(body.get("variant").and_then(Value::as_str))
}

/// 从 query string 取指定键的原值（键值均为账号 id 这类 ASCII 串；
/// 与 `query_variant` 同一解析口径，不做百分号解码）。
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query.unwrap_or("").split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name == key).then_some(value.to_string())
    })
}

// ---------------------------------------------------------------------------
// 状态 / 账号
// ---------------------------------------------------------------------------

async fn api_status(RawQuery(query): RawQuery) -> Response {
    let variant = query_variant(query.as_deref());
    let auth = auth_file::read_auth_file(variant);
    let current = auth.as_ref().map(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        json!({
            "uid": account::display_value(&acct, "uid"),
            "nickname": account::display_value(&acct, "nickname"),
            "email": account::display_value(&acct, "email"),
        })
    });
    json_ok(json!({
        "running": cached_workbuddy_running(variant),
        "authFile": auth_file::auth_file_path(variant).to_string_lossy(),
        "current": current,
        "appPath": auth_file::workbuddy_app_path(variant).to_string_lossy(),
        "version": update::APP_VERSION,
        "variant": variant.as_str(),
    }))
}

/// GET /api/accounts —— 返回全部档位的账号，`current` 取请求档位的登录态。
async fn api_accounts(RawQuery(query): RawQuery) -> Response {
    let variant = query_variant(query.as_deref());
    json_ok(json!({
        "accounts": account::load_accounts()
            .iter()
            .map(account::account_meta)
            .collect::<Vec<_>>(),
        "current": auth_file::read_auth_file(variant)
            .and_then(|a| a.get("account").and_then(|x| x.get("uid")).and_then(|x| x.as_str()).map(String::from)),
        "variant": variant.as_str(),
    }))
}

async fn api_codebuddy_cli_status() -> Response {
    json_ok(codebuddy_cli::status())
}

async fn api_codebuddy_cli_install_helper() -> Response {
    match codebuddy_cli::install_helper() {
        Ok(result) => json_ok(result),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cli_switch(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    // 入参 `closeRunningCli` 已废弃：后端一律先关闭正在运行的 CLI 再写状态，忽略该值。
    // 无头模式不投递系统通知，切号结果（含关闭数量）照常返回给调用方。
    match codebuddy_cli::switch_active_account(id) {
        Ok(result) => json_ok(result),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_cn_ide_status() -> Response {
    json_ok(codebuddy_cn_ide::status())
}

async fn api_codebuddy_cn_ide_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // 可选：切换前把勾选会话复制到目标账号（与 /api/vscode-ext/switch 同形）。
    // 任一条目非法即整包拒绝（与 Tauri 侧 `Option<Vec<CopyItem>>` 的 serde 整包报错同形），
    // 避免「部分成功 + 静默丢弃」让用户误以为全部复制成功。
    let copy_items: Vec<vscode_session::CopyItem> = match body
        .get("copySessions")
        .and_then(|v| v.as_array())
        .map(|array| {
            array
                .iter()
                .map(|item| serde_json::from_value::<vscode_session::CopyItem>(item.clone()))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
    {
        Ok(items) => items.unwrap_or_default(),
        Err(error) => {
            return json_err(
                format!("copySessions 条目非法：{error}"),
                StatusCode::BAD_REQUEST,
            )
        }
    };
    // 同步选择与桌面端同形（[{groupId, previewToken, mode}]），形状由 core 校验。
    let sync_selections = match session::parse_sync_selections(body.get("syncSelections")) {
        Ok(selections) => selections,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let result = if copy_items.is_empty() && sync_selections.is_empty() {
        codebuddy_cn_ide::switch_account(account_id, restart)
    } else {
        codebuddy_ide_session::switch_codebuddy_cn_ide_with_copy(
            account_id,
            restart,
            &copy_items,
            &sync_selections,
        )
    };
    match result {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// GET /api/codebuddy-cn-ide/sessions —— 当前 IDE 账号可复制的会话（未登录返回空列表）。
async fn api_codebuddy_cn_ide_sessions() -> Response {
    let result =
        tokio::task::spawn_blocking(codebuddy_ide_session::list_current_codebuddy_ide_sessions)
            .await;
    match result {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/codebuddy-cn-ide/session-links —— 预览当前 IDE 账号 → 目标账号的关联会话同步项。
///
/// 与桌面端 `codebuddy_ide_session_links_preview` 同形：直接返回 core 的只读预览。
async fn api_codebuddy_cn_ide_session_links_preview(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if target_account_id.trim().is_empty() {
        return json_err("缺少 targetAccountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let result = tokio::task::spawn_blocking(move || {
        let target = account::find_account(&target_account_id).ok_or("目标账号不存在")?;
        codebuddy_ide_session_sync::links_preview(&target)
    })
    .await;
    match result {
        Ok(Ok(value)) => json_ok(value),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_codebuddy_cn_ide_detect() -> Response {
    match codebuddy_cn_ide::detect_current_account() {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_vscode_ext_status() -> Response {
    json_ok(vscode_ext::status())
}

/// GET /api/vscode-ext/sessions —— 当前 VS Code 扩展账号可复制的会话（未登录返回空列表）。
async fn api_vscode_ext_sessions() -> Response {
    let result = tokio::task::spawn_blocking(|| match vscode_ext::active_ext_uid() {
        Some(uid) => vscode_session::list_vscode_sessions(&uid),
        None => json!({ "sourceUid": null, "sessions": [], "skipped": 0 }),
    })
    .await;
    match result {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_vscode_ext_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // 默认重启（= 自动关闭并重开）：VS Code 运行时由后端先优雅退出再写入。
    // 显式传 restart=false 时退回「请先完全退出 VS Code」的手动模式。
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // 可选：切换前把勾选会话复制到目标账号（与 /api/vscode-ext/* 命名风格一致）。
    // 任一条目非法即整包拒绝（与 Tauri 侧 `Option<Vec<CopyItem>>` 的 serde 整包报错同形），
    // 避免「部分成功 + 静默丢弃」让用户误以为全部复制成功。
    let copy_items: Vec<vscode_session::CopyItem> = match body
        .get("copySessions")
        .and_then(|v| v.as_array())
        .map(|array| {
            array
                .iter()
                .map(|item| serde_json::from_value::<vscode_session::CopyItem>(item.clone()))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
    {
        Ok(items) => items.unwrap_or_default(),
        Err(error) => {
            return json_err(
                format!("copySessions 条目非法：{error}"),
                StatusCode::BAD_REQUEST,
            )
        }
    };

    // 同步选择与桌面端同形（[{groupId, previewToken, mode}]），形状由 core 校验。
    let sync_selections = match session::parse_sync_selections(body.get("syncSelections")) {
        Ok(selections) => selections,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };

    let result = if copy_items.is_empty() && sync_selections.is_empty() {
        vscode_ext::switch_account(account_id, restart)
    } else {
        vscode_session::switch_vscode_ext_with_copy(
            account_id,
            restart,
            &copy_items,
            &sync_selections,
        )
    };
    match result {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/vscode-ext/session-links —— 预览当前插件账号 → 目标账号的关联会话同步项。
///
/// 与桌面端 `vscode_session_links_preview` 同形：直接返回 core 的只读预览
/// （`supported` / `storeStatus` / `groups`），每组的 `defaultChecked` 与 `availableModes`
/// 是前端的勾选权限来源。
async fn api_vscode_ext_session_links_preview(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if target_account_id.trim().is_empty() {
        return json_err("缺少 targetAccountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let result = tokio::task::spawn_blocking(move || {
        let target = account::find_account(&target_account_id).ok_or("目标账号不存在")?;
        vscode_session_sync::links_preview(&target)
    })
    .await;
    match result {
        Ok(Ok(value)) => json_ok(value),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_codebuddy_ide_status() -> Response {
    json_ok(codebuddy_ide::status())
}

/// POST /api/codebuddy-ide/switch —— 注入凭证到 CodeBuddy IDE（国际版），可选复制 / 同步会话。
///
/// `copySessions` / `syncSelections` 与国内版（`/api/codebuddy-cn-ide/switch`）同形：
/// 任一条目非法即整包拒绝，两者都为空时行为与纯切换逐字一致。
async fn api_codebuddy_ide_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let copy_items: Vec<vscode_session::CopyItem> = match body
        .get("copySessions")
        .and_then(|v| v.as_array())
        .map(|array| {
            array
                .iter()
                .map(|item| serde_json::from_value::<vscode_session::CopyItem>(item.clone()))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
    {
        Ok(items) => items.unwrap_or_default(),
        Err(error) => {
            return json_err(
                format!("copySessions 条目非法：{error}"),
                StatusCode::BAD_REQUEST,
            )
        }
    };
    // 同步选择与桌面端同形（[{groupId, previewToken, mode}]），形状由 core 校验。
    let sync_selections = match session::parse_sync_selections(body.get("syncSelections")) {
        Ok(selections) => selections,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let result = if copy_items.is_empty() && sync_selections.is_empty() {
        codebuddy_ide::switch_account(account_id, restart)
    } else {
        codebuddy_ide_session::switch_codebuddy_intl_ide_with_copy(
            account_id,
            restart,
            &copy_items,
            &sync_selections,
        )
    };
    match result {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// GET /api/codebuddy-ide/sessions —— 当前国际版 IDE 账号可复制的会话（未登录返回空列表）。
async fn api_codebuddy_intl_ide_sessions() -> Response {
    let result =
        tokio::task::spawn_blocking(codebuddy_ide_session::list_current_intl_ide_sessions).await;
    match result {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/codebuddy-ide/session-links —— 预览当前国际版 IDE 账号 → 目标账号的关联会话同步项。
///
/// 与桌面端 `codebuddy_intl_ide_session_links_preview` 同形：直接返回 core 的只读预览。
async fn api_codebuddy_intl_ide_session_links_preview(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if target_account_id.trim().is_empty() {
        return json_err("缺少 targetAccountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let result = tokio::task::spawn_blocking(move || {
        let target = account::find_account(&target_account_id).ok_or("目标账号不存在")?;
        codebuddy_ide_session_sync::links_preview_intl(&target)
    })
    .await;
    match result {
        Ok(Ok(value)) => json_ok(value),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_vscode_ext_detect() -> Response {
    match vscode_ext::detect_current_account() {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_codebuddy_ide_detect() -> Response {
    match codebuddy_ide::detect_current_account() {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_jetbrains_status() -> Response {
    json_ok(jetbrains::status())
}

async fn api_jetbrains_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .or_else(|| body.get("account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // 可选：目标配置目录名列表（如 ["PyCharm2026.2"]）。缺省 / 空数组 = 全部装了插件的 IDE。
    let config_dirs: Option<Vec<String>> = body
        .get("configDirs")
        .and_then(|v| v.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|list| !list.is_empty());
    match jetbrains::switch_account(account_id, restart, config_dirs.as_deref()) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_jetbrains_detect() -> Response {
    match jetbrains::detect_current_account() {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_delete(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    match account::delete_account(id) {
        Ok(()) => json_ok(json!({ "ok": true })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/update-account-display —— 更新账号本地展示字段（备注 / 显示选择）。
///
/// body：`{ accountId, patch }`，patch 可含 `note`（字符串或 null）与 `displayField`。
async fn api_update_account_display(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let patch = body.get("patch").cloned().unwrap_or_else(|| json!({}));
    match account::update_account_display(id, &patch) {
        Ok(meta) => json_ok(json!({ "ok": true, "account": meta })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/import-local —— 导入本机当前账号（body 可选 `variant`，缺省国内版）。
///
/// body 允许缺失，保持改造前的调用方式可用。
async fn api_import_local(body: Option<Json<Value>>) -> Response {
    let variant = body
        .as_ref()
        .map(|Json(value)| body_variant(value))
        .unwrap_or_else(|| WbVariant::parse(None));
    match account::import_local(variant) {
        Ok(acc) => json_ok(json!({ "ok": true, "account": acc })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 导出 / 导入账号
// ---------------------------------------------------------------------------

async fn api_export_accounts(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match export_import::export_accounts(&ids) {
        Ok(records) => json_ok(json!({ "ok": true, "accounts": records })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_export_accounts_to_path(Json(body): Json<Value>) -> Response {
    let ids: Vec<String> = body
        .get("accountIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::export_accounts_to_path(&ids, &path) {
        Ok(path) => json_ok(json!({ "ok": true, "path": path })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_preview_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match export_import::preview_accounts(&text) {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

async fn api_import(Json(body): Json<Value>) -> Response {
    let text = body
        .get("fileText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let indexes: Vec<usize> = body
        .get("indexes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as usize))
                .collect()
        })
        .unwrap_or_default();
    match export_import::import_accounts(&text, &indexes) {
        Ok(result) => json_ok(json!({
            "ok": true,
            "imported": result.imported,
            "skipped": result.skipped,
            "overwritten": result.overwritten,
        })),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 整包备份 / 迁移（archive）
//
// 浏览器没有文件系统：导出把 zip 读成 base64 塞进 JSON 由前端下载，导入把
// base64 写成临时文件再交给 core。`data` 体积可能很大，任何错误信息里都不带它。
// ---------------------------------------------------------------------------

/// 每次请求独占一个临时目录。
///
/// 不用 `{temp}/固定名.zip`：那个路径可以被提前摆好的符号链接劫持，`fs::write`
/// 会跟着写到别处。目录名带 uuid，且用 `create_dir`（不是 `create_dir_all`）
/// 保证「必须是新建的」；`DROP` 时整目录删除，成功、失败、请求中途取消都不留垃圾。
#[derive(Debug)]
struct TempArchive {
    dir: PathBuf,
    file: PathBuf,
}

impl TempArchive {
    fn new(tag: &str) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-archive-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&dir)
            .map_err(|error| format!("创建临时目录失败（{}）：{error}", dir.display()))?;
        let file = dir.join("archive.zip");
        Ok(Self { dir, file })
    }

    fn path(&self) -> &Path {
        &self.file
    }
}

impl Drop for TempArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 请求体里的 `selected`：缺失或非数组一律当「全部」（core 的语义）。
fn archive_selection(body: &Value) -> Vec<String> {
    body.get("selected")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// 把前端传来的 base64 落成临时 zip；空串直接拒绝，别名见 `TempArchive`。
fn decode_archive(tag: &str, data: &str) -> Result<TempArchive, String> {
    if data.is_empty() {
        return Err("请先选择要导入的归档文件".to_string());
    }
    let bytes = BASE64
        .decode(data)
        .map_err(|error| format!("归档数据不是合法的 base64：{error}"))?;
    let temp = TempArchive::new(tag)?;
    // create_new：目标文件必须由我们新建，不会跟随已有的符号链接。
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp.path())
        .map_err(|error| format!("创建临时归档失败（{}）：{error}", temp.path().display()))?;
    use std::io::Write;
    file.write_all(&bytes)
        .map_err(|error| format!("写入临时归档失败（{}）：{error}", temp.path().display()))?;
    Ok(temp)
}

/// 浏览器下载用的文件名：`wb-switch-backup-YYYYMMDD.zip`。
fn backup_file_name() -> String {
    format!(
        "wb-switch-backup-{}.zip",
        chrono::Local::now().format("%Y%m%d")
    )
}

/// 阻塞段：导出到临时文件 → 读回 → base64。临时文件由 `TempArchive` 兜底删除。
fn export_archive(selected: &[String]) -> Result<Value, String> {
    let temp = TempArchive::new("export")?;
    let outcome = archive::export(selected, temp.path())?;
    let bytes = std::fs::read(temp.path())
        .map_err(|error| format!("读取临时归档失败（{}）：{error}", temp.path().display()))?;
    Ok(json!({
        "fileName": backup_file_name(),
        "bytes": outcome.bytes,
        "groups": outcome.groups,
        "data": BASE64.encode(bytes),
    }))
}

/// GET /api/archive/groups —— 各数据分组的文件数与体积（勾选面板用）。
async fn api_archive_groups() -> Response {
    json_ok(json!({ "groups": archive::groups() }))
}

/// POST /api/archive/export —— body `{ selected: [...] }`，回 base64 归档。
async fn api_archive_export(Json(body): Json<Value>) -> Response {
    let selected = archive_selection(&body);
    match tokio::task::spawn_blocking(move || export_archive(&selected)).await {
        Ok(Ok(value)) => json_ok(value),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("导出任务异常终止：{error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/archive/inspect —— body `{ data: "<base64>" }`，只读清单。
async fn api_archive_inspect(Json(body): Json<Value>) -> Response {
    let data = body
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match tokio::task::spawn_blocking(move || {
        let temp = decode_archive("inspect", &data)?;
        archive::inspect(temp.path())
    })
    .await
    {
        Ok(Ok(info)) => json_ok(info),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("读取归档信息失败：{error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/archive/import —— body `{ data: "<base64>", selected: [...] }`。
async fn api_archive_import(Json(body): Json<Value>) -> Response {
    let data = body
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let selected = archive_selection(&body);
    match tokio::task::spawn_blocking(move || {
        let temp = decode_archive("import", &data)?;
        archive::import(temp.path(), &selected)
    })
    .await
    {
        Ok(Ok(outcome)) => json_ok(outcome),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("导入任务异常终止：{error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

// ---------------------------------------------------------------------------
// OAuth 登录
// ---------------------------------------------------------------------------

/// POST /api/oauth/start —— 发起扫码登录（body 可选 `variant`，缺省国内版）。
async fn api_oauth_start(body: Option<Json<Value>>) -> Response {
    let variant = body
        .as_ref()
        .map(|Json(value)| body_variant(value))
        .unwrap_or_else(|| WbVariant::parse(None));
    match oauth::oauth_start(variant).await {
        Ok(v) => json_ok(v),
        Err(e) => json_err(e, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/oauth/status —— 轮询采集结果（档位取发起时记录，无需传参）。
async fn api_oauth_status(Json(body): Json<Value>) -> Response {
    let login_id = body
        .get("loginId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    json_ok(oauth::oauth_poll(&login_id).await)
}

// ---------------------------------------------------------------------------
// 切换
// ---------------------------------------------------------------------------

async fn api_switch(Json(body): Json<Value>) -> Response {
    let account_id = body
        .get("accountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if account_id.trim().is_empty() {
        return json_err("缺少 accountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let restart = body
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let share_sessions = body
        .get("shareSessions")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // 边界一次性归一化：非法 memoryMode 直接 400，不静默回退成「不同步」。
    let memory_sync = match memory::MemorySyncMode::parse(
        body.get("memoryMode").and_then(|v| v.as_str()),
    ) {
        Ok(mode) => mode,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let memory_source_uid = body
        .get("memorySourceUid")
        .and_then(|v| v.as_str())
        .map(String::from);
    let merge_experts = body
        .get("mergeExperts")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // 人设同步独立于记忆合并：两个副作用差异很大，绑在一个开关上会误伤。
    let sync_persona = body
        .get("syncPersona")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let persona_mode = memory::PersonaMode::parse(body.get("personaMode").and_then(|v| v.as_str()));
    let copy_ids: Vec<String> = body
        .get("copySessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    // 同步选择与桌面端同形（[{groupId, previewToken, mode}]），形状由 core 校验。
    let sync_selections = match session::parse_sync_selections(body.get("syncSelections")) {
        Ok(selections) => selections,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    // 镜像计划：与 core 参数逐字对称；未传为 None，解析失败明确 400（不静默降级）。
    let mirror_plan = match body.get("mirrorPlan") {
        Some(value) if !value.is_null() => {
            match serde_json::from_value::<mirror::MirrorPlan>(value.clone()) {
                Ok(plan) => Some(plan),
                Err(error) => {
                    return json_err(
                        format!("mirrorPlan 解析失败：{error}"),
                        StatusCode::BAD_REQUEST,
                    )
                }
            }
        }
        _ => None,
    };
    // 删除同步计划：同样与 core 逐字对称，解析失败 400。
    let deleted_sync_plan = match body.get("deletedSyncPlan") {
        Some(value) if !value.is_null() => {
            match serde_json::from_value::<mirror::DeletedSyncPlan>(value.clone()) {
                Ok(plan) => Some(plan),
                Err(error) => {
                    return json_err(
                        format!("deletedSyncPlan 解析失败：{error}"),
                        StatusCode::BAD_REQUEST,
                    )
                }
            }
        }
        _ => None,
    };

    let Some(_switch_guard) = SwitchStateGuard::acquire() else {
        return json_err("已有切换任务进行中".to_string(), StatusCode::CONFLICT);
    };

    let progress: switch::ProgressFn = Box::new(|msg| {
        *SWITCH_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(msg.to_string());
    });

    let result = tokio::task::spawn_blocking(move || {
        switch::switch_account(
            Some(&progress),
            &account_id,
            restart,
            share_sessions,
            &copy_ids,
            &sync_selections,
            memory_sync,
            memory_source_uid.as_deref(),
            merge_experts,
            sync_persona,
            persona_mode,
            mirror_plan,
            deleted_sync_plan,
        )
    })
    .await;

    match result {
        Ok(Ok(v)) => json_ok(v),
        Ok(Err(e)) => json_err(e, StatusCode::BAD_REQUEST),
        Err(e) => json_err(e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn api_switch_progress() -> Response {
    let p = SWITCH_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let running = *SWITCH_RUNNING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    json_ok(json!({ "running": running, "progress": p }))
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

/// GET /api/sessions —— 当前账号的会话列表（query 可选 `variant`，缺省国内版）。
async fn api_sessions(RawQuery(query): RawQuery) -> Response {
    let variant = query_variant(query.as_deref());
    match session::current_user_uid(variant) {
        Some(uid) => json_ok(json!({
            "sessions": session::list_sessions_for_user(variant, &uid),
            "current": uid,
            "variant": variant.as_str(),
        })),
        None => json_ok(json!({ "sessions": [], "current": null, "variant": variant.as_str() })),
    }
}

/// GET /api/sessions/account —— 指定账号名下的会话列表（会话管理页的源账号视角）。
///
/// 与 `GET /api/sessions` 同形，但来源是显式账号（`accountId` query）：账号不存在
/// 返回 400；账号缺 uid 时返回空列表 + `current: null`（与现有容错一致）。
async fn api_account_sessions(RawQuery(query): RawQuery) -> Response {
    let account_id = query_param(query.as_deref(), "accountId").unwrap_or_default();
    let client = query_param(query.as_deref(), "client").unwrap_or_else(|| "workbuddy".to_string());
    let Some(account) = account::find_account(&account_id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let client = match session_groups::SessionClient::parse(&client) {
        Ok(client) => client,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    match client {
        session_groups::SessionClient::VscodeExt => {
            let Some(uid) = account::get_str(&account, "uid")
                .map(|uid| uid.trim().to_string())
                .filter(|uid| !uid.is_empty())
            else {
                return json_err("账号缺少 uid".to_string(), StatusCode::BAD_REQUEST);
            };
            json_ok(vscode_session::list_vscode_sessions(&uid))
        }
        session_groups::SessionClient::Workbuddy => {
            json_ok(session::list_sessions_for_account(&account))
        }
        session_groups::SessionClient::CodebuddyIde => json_err(
            "当前客户端暂不支持列出账号会话".to_string(),
            StatusCode::BAD_REQUEST,
        ),
    }
}

async fn api_copy_sessions(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_ids: Vec<String> = body
        .get("sessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    // 档位取目标账号自身（源 uid 也从该档位的登录态读）。
    let variant = account::variant_of(&target);
    // 与桌面端同形：直接返回 core 的复制报告（copied / alreadyLinked / errors / needsRecovery）。
    let mut report = match session::copy_sessions_for_switch(&target, &session_ids) {
        Ok(report) => report,
        Err(error) => {
            return json_err(error, StatusCode::BAD_REQUEST);
        }
    };
    report["variant"] = json!(variant.as_str());
    json_ok(report)
}

/// POST /api/sessions/copy-cross —— 跨档复制：源与目标账号都显式给出（会话管理页用）。
///
/// 与 `POST /api/sessions/copy` 同形，多一个 `sourceAccountId`：源 uid 取自该账号，
/// 不再从目标档登录态读取，支持国内版 ↔ 国际版。报告与桌面端 `copy_sessions_cross`
/// 同形（含 `sourceVariant` / `targetVariant`）。
async fn api_copy_sessions_cross(Json(body): Json<Value>) -> Response {
    let source_account_id = body
        .get("sourceAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_ids: Vec<String> = body
        .get("sessionIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(source) = account::find_account(&source_account_id) else {
        return json_err("源账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let target_variant = account::variant_of(&target);
    let mut report = match session::copy_sessions_cross(&source, &target, &session_ids) {
        Ok(report) => report,
        Err(error) => {
            return json_err(error, StatusCode::BAD_REQUEST);
        }
    };
    // 与同档端点同形：`variant` 恒为目标档（前端按目标账号渲染）。
    report["variant"] = json!(target_variant.as_str());
    json_ok(report)
}

/// POST /api/session-links/preview —— 预览当前账号 → 目标账号的关联会话同步项。
///
/// 与桌面端 `session_links_preview` 同形：直接返回 core 的只读预览（`supported` /
/// `storeStatus` / `groups`），每组的 `defaultChecked` 与 `availableModes` 是前端的
/// 勾选权限来源。`variant` 缺省取目标账号自身档位。
async fn api_session_links_preview(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if target_account_id.trim().is_empty() {
        return json_err("缺少 targetAccountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let variant = match body.get("variant").and_then(Value::as_str) {
        Some(raw) => WbVariant::parse(Some(raw)),
        None => account::variant_of(&target),
    };
    // 逐组核对是真 IO：放 blocking 线程，别占住 async worker，否则进度轮询排不上队。
    // Drop 复位兜住取消路径：写在 await 之后的 `= None` 不会执行，会留下陈旧帧。
    let _reset = ProgressReset(&LINKS_PREVIEW_PROGRESS);
    let report_progress = |done: usize, total: usize| {
        *LINKS_PREVIEW_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((done, total));
    };
    let result = tokio::task::spawn_blocking(move || {
        session::session_links_preview_with_progress(variant, &target, Some(&report_progress))
    })
    .await;
    match result {
        Ok(Ok(report)) => json_ok(report),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("读取关联会话预览失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/mirror/preview —— 只读预览整份镜像清单（Wave 2 批次①）。
///
/// 以「当前登录账号」为母本，逐条列出目标账号将被软删的会话；零会话/记忆数据写入。
/// 母本 uid 一律由登录态文件判定，**绝不接受前端传入的 motherUid**（该字段仅用于界面回显）。
/// 与桌面端 `mirror_preview` 参数逐字对称：只读 `targetAccountId` + 可选 `variant`。
async fn api_mirror_preview(Json(body): Json<Value>) -> Response {
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if target_account_id.trim().is_empty() {
        return json_err("缺少 targetAccountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let variant = match body.get("variant").and_then(Value::as_str) {
        Some(raw) => WbVariant::parse(Some(raw)),
        None => account::variant_of(&target),
    };
    let result =
        tokio::task::spawn_blocking(move || mirror::mirror_preview(variant, &target)).await;
    match result {
        Ok(Ok(report)) => json_ok(report),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("读取镜像预览失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/mirror/restore —— 还原镜像切换的备份（写端点，自己持 auth 锁）。
///
/// 与桌面端 `restore_mirror_backup` 逐字对称；前端 `restoreMirrorBackup` 指向该路由。
/// 「持锁 + 精确白名单回滚」下沉在 `switch::restore_mirror_backup`，锁冲突返回 409。
async fn api_mirror_restore(Json(body): Json<Value>) -> Response {
    let backup = body
        .get("backup")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if backup.trim().is_empty() {
        return json_err("缺少 backup".to_string(), StatusCode::BAD_REQUEST);
    }
    let result = tokio::task::spawn_blocking(move || {
        let path = PathBuf::from(&backup);
        let report = switch::restore_mirror_backup(&path)?;
        let restored = report.deleted_sessions;
        let report_value = serde_json::to_value(&report).map_err(|error| error.to_string())?;
        Ok::<Value, String>(json!({ "ok": true, "restored": restored, "report": report_value }))
    })
    .await;
    match result {
        Ok(Ok(value)) => json_ok(value),
        Ok(Err(error))
            if error.starts_with(wb_switch_core::modules::auth_transaction::BUSY_PREFIX) =>
        {
            json_err(error, StatusCode::CONFLICT)
        }
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// GET /api/sessions/deleted-sync —— 扫描「别处已删、这里还在」的会话（只读，不写任何数据）。
///
/// 与桌面端 `deleted_sync_preview` 逐字对称：无入参，档位固定为国内版（与桌面命令一致）。
async fn api_deleted_sync_preview() -> Response {
    let result = tokio::task::spawn_blocking(|| {
        let paths = session::SessionPaths::for_variant(WbVariant::Cn);
        let preview = mirror::deleted_sync_preview_at(&paths)?;
        serde_json::to_value(preview).map_err(|error| error.to_string())
    })
    .await;
    match result {
        Ok(Ok(value)) => json_ok(value),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("扫描已删除会话失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// 两路预览共用一个轮询端点：取任一在跑的进度槽，都空闲才报 running=false。
/// 前端不带请求标识，只能汇总；两路各写各槽，一路结束不会清掉另一路。
fn links_preview_progress_snapshot() -> Option<(usize, usize)> {
    [&LINKS_PREVIEW_PROGRESS, &LINKS_PREVIEW_CROSS_PROGRESS]
        .iter()
        .find_map(|slot| *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// GET /api/session-links/progress —— 关联会话预览的逐组进度（webui 轮询用）。
async fn api_session_links_progress() -> Response {
    let progress = links_preview_progress_snapshot();
    match progress {
        Some((done, total)) => json_ok(json!({ "running": true, "done": done, "total": total })),
        None => json_ok(json!({ "running": false, "done": 0, "total": 0 })),
    }
}

/// POST /api/session-links/preview-cross —— 预览「显式来源账号 → 显式目标账号」（跨档支持）。
///
/// 与 `POST /api/session-links/preview` 同形，多一个 `sourceAccountId`；
/// 成员内容按成员自身档位读取（跨档组的源读源档、目标读目标档）。
async fn api_session_links_preview_cross(Json(body): Json<Value>) -> Response {
    let source_account_id = body
        .get("sourceAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if source_account_id.trim().is_empty() {
        return json_err("缺少 sourceAccountId".to_string(), StatusCode::BAD_REQUEST);
    }
    let Some(source) = account::find_account(&source_account_id) else {
        return json_err("源账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let _reset = ProgressReset(&LINKS_PREVIEW_CROSS_PROGRESS);
    let report_progress = |done: usize, total: usize| {
        *LINKS_PREVIEW_CROSS_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((done, total));
    };
    let result = tokio::task::spawn_blocking(move || {
        session::session_links_preview_cross_with_progress(
            &source,
            &target,
            Some(&report_progress),
        )
    })
    .await;
    match result {
        Ok(Ok(report)) => json_ok(report),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("读取跨档关联会话预览失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/session-sync/cross —— 把显式来源账号的新增同步到显式目标账号（跨档支持）。
///
/// `syncSelections` 与切号弹窗同形（core 校验缺 groupId / previewToken / mode）。
async fn api_session_sync_cross(Json(body): Json<Value>) -> Response {
    let source_account_id = body
        .get("sourceAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let target_account_id = body
        .get("targetAccountId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let Some(source) = account::find_account(&source_account_id) else {
        return json_err("源账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let Some(target) = account::find_account(&target_account_id) else {
        return json_err("目标账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    let selections = match session::parse_sync_selections(body.get("syncSelections")) {
        Ok(selections) => selections,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    match session::sync_sessions_cross(&source, &target, &selections) {
        Ok(report) => json_ok(report),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

fn session_group_request(
    body: &Value,
) -> Result<(session_groups::SessionClient, Option<WbVariant>), String> {
    let client = body
        .get("client")
        .and_then(Value::as_str)
        .ok_or_else(|| "缺少 client".to_string())?;
    let client = session_groups::SessionClient::parse(client)?;
    let scope =
        session_groups::parse_variant_scope(body.get("variantScope").and_then(Value::as_str))?;
    Ok((client, scope))
}

async fn api_list_session_groups(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    // 组内是真磁盘遍历：放 blocking 线程，别占住 async worker，否则进度轮询排不上队。
    let _reset = ProgressReset(&SESSION_GROUPS_PROGRESS);
    let report_progress = |done: usize, total: usize| {
        *SESSION_GROUPS_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((done, total));
    };
    let result = tokio::task::spawn_blocking(move || {
        session_groups::list_with_progress(client, scope, Some(&report_progress))
    })
    .await;
    match result {
        Ok(Ok(value)) => json_ok(value),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(
            format!("读取会话组失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// GET /api/session-groups/progress —— 列表加载进度；无加载在跑时 running=false。
async fn api_session_groups_progress() -> Response {
    let progress = *SESSION_GROUPS_PROGRESS.lock().unwrap();
    match progress {
        Some((done, total)) => json_ok(json!({ "running": true, "done": done, "total": total })),
        None => json_ok(json!({ "running": false, "done": 0, "total": 0 })),
    }
}

async fn api_get_session_group(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let group_id = body.get("groupId").and_then(Value::as_str).unwrap_or("");
    match session_groups::detail(client, scope, group_id) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_preview_session_group_pair(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let args = (
        body.get("groupId").and_then(Value::as_str).unwrap_or(""),
        body.get("sourceMemberId")
            .and_then(Value::as_str)
            .unwrap_or(""),
        body.get("targetMemberId")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    match session_groups::preview_pair(client, scope, args.0, args.1, args.2) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_sync_session_group_pair(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let args = (
        body.get("groupId").and_then(Value::as_str).unwrap_or(""),
        body.get("sourceMemberId")
            .and_then(Value::as_str)
            .unwrap_or(""),
        body.get("targetMemberId")
            .and_then(Value::as_str)
            .unwrap_or(""),
        body.get("previewToken")
            .and_then(Value::as_str)
            .unwrap_or(""),
        body.get("mode").and_then(Value::as_str).unwrap_or(""),
    );
    let restart = body
        .get("restart")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match session_groups::sync_pair(
        client, scope, args.0, args.1, args.2, args.3, args.4, restart,
    ) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_sync_session_group_unify(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let targets = match serde_json::from_value::<Vec<session_groups::GroupUnifyTarget>>(
        body.get("targets").cloned().unwrap_or(Value::Null),
    ) {
        Ok(value) => value,
        Err(error) => {
            return json_err(
                format!("目标副本参数无效：{error}"),
                StatusCode::BAD_REQUEST,
            )
        }
    };
    let group_id = body.get("groupId").and_then(Value::as_str).unwrap_or("");
    let source_member_id = body
        .get("sourceMemberId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let restart = body
        .get("restart")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match session_groups::sync_unify_batch(
        client,
        scope,
        group_id,
        source_member_id,
        &targets,
        restart,
    ) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_sync_session_group_safe_batch(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let group_id = body.get("groupId").and_then(Value::as_str).unwrap_or("");
    let restart = body
        .get("restart")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match session_groups::sync_safe_batch(client, scope, group_id, restart) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_add_session_group_member(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let args = (
        body.get("groupId").and_then(Value::as_str).unwrap_or(""),
        body.get("sourceMemberId")
            .and_then(Value::as_str)
            .unwrap_or(""),
        body.get("targetAccountId")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let restart = body
        .get("restart")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match session_groups::add_member(client, scope, args.0, args.1, args.2, restart) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_copy_linked_sessions(Json(body): Json<Value>) -> Response {
    let (client, _scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let source_account_id = body
        .get("sourceAccountId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let target_account_id = body
        .get("targetAccountId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let session_ids: Vec<String> = body
        .get("sessionIds")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let restart = body
        .get("restart")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match session_groups::copy_linked_sessions(
        client,
        source_account_id,
        target_account_id,
        &session_ids,
        restart,
    ) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

async fn api_vscode_restart_precheck(Json(body): Json<Value>) -> Response {
    let target_account_ids: Vec<String> = body
        .get("targetAccountIds")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    json_ok(session_groups::vscode_restart_precheck(&target_account_ids))
}

/// 取消关联：只解除管理关系，不触碰账号内的会话内容。
async fn api_unlink_session_group_member(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let group_id = body.get("groupId").and_then(Value::as_str).unwrap_or("");
    let member_id = body.get("memberId").and_then(Value::as_str).unwrap_or("");
    match session_groups::remove_member(client, scope, group_id, member_id) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

/// 删除会话组：组内所有成员一起解除关联，只解除管理关系，不触碰账号内的会话内容。
async fn api_delete_session_group(Json(body): Json<Value>) -> Response {
    let (client, scope) = match session_group_request(&body) {
        Ok(value) => value,
        Err(error) => return json_err(error, StatusCode::BAD_REQUEST),
    };
    let group_id = body.get("groupId").and_then(Value::as_str).unwrap_or("");
    match session_groups::delete_group(client, scope, group_id) {
        Ok(value) => json_ok(value),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 签到 / 保活
// ---------------------------------------------------------------------------

/// GET /api/checkin/status —— 传 accountId 时只查询该账号；缺省保留旧批量响应。
/// 两种形式都遵守单账号自动签到开关，避免展示状态时触发已关闭账号的请求。
async fn api_checkin_status(Query(query): Query<HashMap<String, String>>) -> Response {
    if let Some(id) = query.get("accountId") {
        let Some(acc) = account::find_account(id) else {
            return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
        };
        let status = checkin::get_checkin_status_for_display(&acc).await;
        return json_ok(checkin_status_item(&acc, status));
    }
    let list = account::load_accounts();
    let mut items = Vec::new();
    for acc in &list {
        let status = checkin::get_checkin_status_for_display(acc).await;
        items.push(checkin_status_item(acc, status));
    }
    json_ok(json!({ "accounts": items }))
}

fn checkin_status_item(account: &Value, mut status: Value) -> Value {
    status["accountId"] = account.get("id").cloned().unwrap_or(Value::Null);
    status["email"] = json!(account::account_display_name(account));
    status["variant"] = json!(account::variant_of(account).as_str());
    status
}

async fn api_credits(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(credits::get_credit_expiry(&acc).await)
}

fn query_flag_enabled(query: Option<&str>, name: &str) -> bool {
    query.unwrap_or("").split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, "true"));
        key == name && matches!(value, "" | "1" | "true" | "yes")
    })
}

async fn api_credit_statistics(RawQuery(query): RawQuery) -> Response {
    json_ok(credit_usage::get_statistics(query_flag_enabled(query.as_deref(), "refresh")).await)
}

async fn api_token_statistics(RawQuery(query): RawQuery) -> Response {
    let days = query.as_deref().and_then(|value| {
        value
            .split('&')
            .find_map(|part| part.strip_prefix("days=")?.parse::<i64>().ok())
    });
    let _reset = ProgressReset(&TOKEN_STATS_PROGRESS);
    let report_progress = |done: usize, total: usize| {
        *TOKEN_STATS_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((done, total));
    };
    let result = tokio::task::spawn_blocking(move || {
        token_stats::get_statistics_with_progress(days, Some(&report_progress))
    })
    .await;
    match result {
        Ok(statistics) => json_ok(statistics),
        Err(error) => json_err(
            format!("扫描 Token 统计失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// GET /api/token-stats/progress —— 扫描进度 `(done, total)`；无扫描在跑时 running=false。
async fn api_token_statistics_progress() -> Response {
    let progress = *TOKEN_STATS_PROGRESS.lock().unwrap();
    match progress {
        Some((done, total)) => json_ok(json!({ "running": true, "done": done, "total": total })),
        None => json_ok(json!({ "running": false, "done": 0, "total": 0 })),
    }
}

/// GET /api/rate-limits —— 模型限额台账（全部账号当前受限的模型与官方恢复时刻）。
///
/// 扫描本机日志文件，放 blocking 线程避免占用运行时线程；无受限模型时返回空数组。
async fn api_rate_limits() -> Response {
    match tokio::task::spawn_blocking(limits::get_rate_limits).await {
        Ok(payload) => json_ok(payload),
        Err(error) => json_err(
            format!("扫描模型限额失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// hook 状态 + 运行期字段（最近一次 hook 事件时刻）。
fn rate_limit_hook_status() -> Value {
    let mut status = rate_limit_hook::hook_status();
    status["lastEventAt"] = json!(rate_limit_events::last_event_at());
    status
}

/// GET /api/rate-limits/hook-status —— hook 安装状态（脚本 + 三处客户端配置逐项结果）。
async fn api_rate_limit_hook_status() -> Response {
    match tokio::task::spawn_blocking(rate_limit_hook_status).await {
        Ok(status) => json_ok(status),
        Err(error) => json_err(
            format!("查询限额 hook 状态失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

/// POST /api/rate-limits/install-hook —— 安装 hook（幂等，写前备份；同时清除「卸载过」标记）。
async fn api_install_rate_limit_hook() -> Response {
    match tokio::task::spawn_blocking(|| {
        let result = rate_limit_hook::install_hook();
        // 扫描范围随安装结果变化（只对未注册的来源扫日志），缓存必须作废。
        limits::invalidate_scan_cache();
        result.map(|_| rate_limit_hook_status())
    })
    .await
    {
        Ok(Ok(status)) => json_ok(status),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/rate-limits/uninstall-hook —— 卸载 hook（移除注册条目，尽量逐字节还原）。
///
/// 卸载即用户拒绝自动接入（`hookOptOut`），与安装逻辑同处 core，两个宿主共用同一语义。
async fn api_uninstall_rate_limit_hook() -> Response {
    match tokio::task::spawn_blocking(|| {
        let result = rate_limit_hook::uninstall_hook();
        limits::invalidate_scan_cache();
        result.map(|_| rate_limit_hook_status())
    })
    .await
    {
        Ok(Ok(status)) => json_ok(status),
        Ok(Err(error)) => json_err(error, StatusCode::BAD_REQUEST),
        Err(error) => json_err(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// webui 把配置包在 `{ config: {...} }` 里，桌面端 Tauri 直接把裸 config 作为命令入参。
/// 在边界一次性解包，让 core 拿到与桌面端同形的顶层字段；否则 enabled/proxy 会被
/// 当成缺省值写回，前端拿到的又是错误的外层包装。
fn submitted_config(body: &Value) -> &Value {
    body.get("config").unwrap_or(body)
}

async fn api_rate_limit_config() -> Response {
    json_ok(config::load_rate_limit_config())
}

/// POST /api/rate-limits/config —— 保存限额监听配置。
///
/// 与桌面端同语义：`scanIdeLogs` 变化时作废扫描缓存，下一次按当前来源范围重算。
async fn api_save_rate_limit_config(Json(body): Json<Value>) -> Response {
    let submitted = submitted_config(&body);
    match limits::save_rate_limit_config(submitted) {
        Ok(()) => json_ok(config::load_rate_limit_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_checkin(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(checkin::checkin_account(&acc).await)
}

/// POST /api/activity/daily —— 国际版每日活跃会话（网页通道）。
///
/// body: `{"accountId": "..."}`。与签到是两条独立链路：签到走 billing 体系、
/// 由服务端 `active` 开关决定；每日活跃走 `/console/as/conversations/` + ACP，
/// 跑完一轮即计分。国际版没有签到活动，能拿到官方奖励的只有这一条。
async fn api_daily_activity(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    json_ok(web_activity::run_daily_activity(id).await)
}

/// POST /api/activity/daily/all —— 对所有国际版账号各跑一次。
async fn api_daily_activity_all() -> Response {
    json_ok(web_activity::run_daily_activity_all().await)
}

/// GET /api/activity/config —— 每日活跃开关（默认开启）。
async fn api_activity_config() -> Response {
    json_ok(web_activity::load_activity_config())
}

/// GET /api/activity/status —— 各账号今日活跃状态（账号页 chip 用）。
async fn api_activity_status() -> Response {
    json_ok(web_activity::activity_status())
}

/// POST /api/activity/config —— 保存每日活跃开关。
///
/// 时间段不在这里配：它跟随签到配置的 `checkin_start` / `checkin_end`，一处设置
/// 同时管两个功能。
async fn api_save_activity_config(Json(body): Json<Value>) -> Response {
    match web_activity::save_activity_config(&body) {
        Ok(()) => json_ok(web_activity::load_activity_config()),
        Err(error) => json_err(
            format!("保存每日活跃配置失败: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

async fn api_checkin_all(body: Option<Json<Value>>) -> Response {    // 缺省（无 body / 无 variant）= 全部档位，保持与桌面端 set 前的行为一致。
    let variant = body
        .as_ref()
        .and_then(|Json(value)| value.get("variant"))
        .and_then(|value| value.as_str())
        .map(|raw| WbVariant::parse(Some(raw)));
    // `respectWindow`（camelCase，与 `accountId` 先例一致）只由账号页「刷新并签到」
    // 传 true：窗口生效且当前不在时间段内时整轮跳过；缺省 false 保持设置页 /
    // 托盘 / 旧客户端的立即签到语义。
    let respect_window = body
        .as_ref()
        .and_then(|Json(value)| value.get("respectWindow"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    json_ok(checkin::run_checkin_all(variant, respect_window).await)
}

async fn api_checkin_config() -> Response {
    json_ok(config::load_checkin_config())
}

async fn api_save_checkin_config(Json(body): Json<Value>) -> Response {
    let submitted = submitted_config(&body);
    match config::save_checkin_config(submitted) {
        Ok(()) => json_ok(config::load_checkin_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

/// GET /api/checkin/logs —— 签到日志（每行带 `variant`，便于前端按档位过滤）。
async fn api_checkin_logs() -> Response {
    json_ok(json!({ "logs": checkin::load_checkin_logs_with_variant() }))
}

async fn api_travel_status() -> Response {
    travel::reconcile_due_travel(None).await;
    let items = account::load_accounts()
        .iter()
        .map(|acc| {
            let id = acc.get("id").and_then(Value::as_str).unwrap_or("");
            let mut value = travel::travel_display(id);
            value["accountId"] = acc.get("id").cloned().unwrap_or(Value::Null);
            value["email"] = json!(account::account_display_name(acc));
            value
        })
        .collect::<Vec<_>>();
    json_ok(json!({ "accounts": items }))
}

async fn api_travel_config() -> Response {
    json_ok(config::load_travel_config())
}

async fn api_save_travel_config(Json(body): Json<Value>) -> Response {
    let submitted = submitted_config(&body);
    match config::save_travel_config(submitted) {
        Ok(()) => {
            let saved = config::load_travel_config();
            if saved.get("enabled").and_then(Value::as_bool) == Some(true) {
                tokio::spawn(async {
                    let _ = travel::run_travel_cycle().await;
                    let _ = travel::run_travel_claim_cycle().await;
                });
            }
            json_ok(saved)
        }
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_refresh_token(Json(body): Json<Value>) -> Response {
    let id = body.get("accountId").and_then(|v| v.as_str()).unwrap_or("");
    let Some(acc) = account::find_account(id) else {
        return json_err("账号不存在".to_string(), StatusCode::BAD_REQUEST);
    };
    json_ok(refresh::refresh_account_token(acc).await)
}

// ---------------------------------------------------------------------------
// 自动轮换（CodeBuddy CLI）
// ---------------------------------------------------------------------------

async fn api_rotate_config() -> Response {
    json_ok(config::load_auto_rotate_config())
}

async fn api_save_rotate_config(Json(body): Json<Value>) -> Response {
    match config::save_auto_rotate_config(submitted_config(&body)) {
        Ok(()) => json_ok(config::load_auto_rotate_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

async fn api_rotate_status() -> Response {
    json_ok(rotate::rotate_status())
}

async fn api_rotate_run() -> Response {
    json_ok(rotate::run_rotate_cycle().await)
}

async fn api_rotate_logs() -> Response {
    json_ok(json!({ "logs": rotate::rotate_logs() }))
}

// ---------------------------------------------------------------------------
// 更新
// ---------------------------------------------------------------------------

/// GET 的 query 一次归一化：前端 `checkUpdate(proxy, force)` 把参数拼在 URL 上，
/// 桌面端由 Tauri 直接透传同名入参；两条路径都必须到达 `update::update_check`。
fn update_check_args(query: &HashMap<String, String>) -> (Option<&str>, bool) {
    let proxy = query
        .get("proxy")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let force = query.get("force").map_or(false, |value| value == "true");
    (proxy, force)
}

async fn api_update_check(Query(query): Query<HashMap<String, String>>) -> Response {
    let (proxy, force) = update_check_args(&query);
    json_ok(update::update_check(proxy, force).await)
}

async fn api_update_config() -> Response {
    json_ok(update::load_github_config())
}

async fn api_save_update_config(Json(body): Json<Value>) -> Response {
    match update::save_github_config(submitted_config(&body)) {
        Ok(()) => json_ok(update::load_github_config()),
        Err(e) => json_err(e.to_string(), StatusCode::BAD_REQUEST),
    }
}

// ---------------------------------------------------------------------------
// 静态前端
// ---------------------------------------------------------------------------

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

async fn static_handler(uri: Uri) -> Response {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() || path == "index.html" {
        path = "index.html".to_string();
    }
    // 前端路由回退到 index.html。Content-Type 必须按**实际命中的文件**给：按请求路径算
    // 会把 `/sessions` 这类前端路由标成 application/octet-stream，浏览器直接当下载。
    let (data, served) = match Assets::get(&path) {
        Some(f) => (f, path),
        None => match Assets::get("index.html") {
            Some(f) => (f, "index.html".to_string()),
            None => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("not found"))
                    .unwrap()
            }
        },
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(&served))
        .body(Body::from(data.data.into_owned()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// 通知存档（toast 事后可查）
// ---------------------------------------------------------------------------

/// GET /api/notifications —— 最近的应用内提示（新的在前，最多 100 条）。
async fn api_notifications() -> Response {
    match notifications::list() {
        Ok(items) => json_ok(json!({ "items": items })),
        Err(error) => json_err(error, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// POST /api/notifications/record —— 记录一条提示（前端 toast 同步写一份）。
async fn api_record_notification(Json(body): Json<Value>) -> Response {
    let level = body.get("level").and_then(|v| v.as_str()).unwrap_or("info");
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let description = body.get("description").and_then(|v| v.as_str());
    match notifications::record(level, title, description) {
        Ok(()) => json_ok(json!({ "recorded": true })),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

/// POST /api/notifications/clear —— 清空通知存档。
async fn api_clear_notifications() -> Response {
    match notifications::clear() {
        Ok(()) => json_ok(json!({ "cleared": true })),
        Err(error) => json_err(error, StatusCode::BAD_REQUEST),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        archive_selection, backup_file_name, body_variant, checkin_status_item, decode_archive,
        links_preview_progress_snapshot, query_variant, static_handler, submitted_config,
        temp_archive_path, update_check_args, ProgressReset, SwitchStateGuard, TempArchive, BASE64,
        ARCHIVE_BODY_LIMIT, LINKS_PREVIEW_CROSS_PROGRESS, LINKS_PREVIEW_PROGRESS, SWITCH_PROGRESS,
        SWITCH_RUNNING,
    };
    use axum::http::{header, StatusCode, Uri};
    use axum::Json;
    use base64::Engine as _;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use wb_switch_core::modules::variant::WbVariant;

    // 全局状态用例各自独占一把锁：Rust 测试默认并行，避免互相踩 static。
    static SWITCH_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static LINKS_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 非法 memoryMode 必须在边界被拒（400），绝不静默回退成「不同步」后闷头切号。
    /// 校验发生在 SwitchStateGuard 与任何会话/记忆写入之前，因此无需抢全局状态锁。
    #[tokio::test]
    async fn illegal_memory_mode_is_rejected_before_any_side_effect() {
        for illegal in ["MERGE", "sync", "overwrite-all", ""] {
            let body: Value = json!({ "accountId": "acc-1", "memoryMode": illegal });
            let response = super::api_switch(Json(body)).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{illegal:?}");
        }
    }

    /// SPA 回退必须按**命中的文件**给 Content-Type：`/sessions` 这类前端路由曾按请求路径
    /// 被标成 application/octet-stream，浏览器直接当文件下载、页面打不开（真机验收发现）。
    #[tokio::test]
    async fn spa_fallback_serves_html_content_type() {
        for path in ["/", "/sessions", "/accounts"] {
            let resp = static_handler(Uri::from_static(path)).await;
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let content_type = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert!(
                content_type.starts_with("text/html"),
                "{path} 的 Content-Type 应为 text/html，实际 {content_type}"
            );
        }
    }

    /// 缺省档位必须与改造前一致（不传 variant 即国内版）。
    #[test]
    fn variant_query_defaults_to_cn() {
        assert_eq!(query_variant(None), WbVariant::Cn);
        assert_eq!(query_variant(Some("")), WbVariant::Cn);
        assert_eq!(query_variant(Some("refresh=true")), WbVariant::Cn);
        assert_eq!(
            query_variant(Some("refresh=true&variant=cn")),
            WbVariant::Cn
        );
        assert_eq!(query_variant(Some("variant=ai")), WbVariant::Ai);
        assert_eq!(
            query_variant(Some("variant=ai&refresh=true")),
            WbVariant::Ai
        );
        assert_eq!(query_variant(Some("variant=unknown")), WbVariant::Cn);
    }

    #[test]
    fn variant_body_defaults_to_cn() {
        assert_eq!(body_variant(&json!({})), WbVariant::Cn);
        assert_eq!(body_variant(&json!({"variant": null})), WbVariant::Cn);
        assert_eq!(body_variant(&json!({"variant": "ai"})), WbVariant::Ai);
        assert_eq!(body_variant(&json!({"accountId": "x"})), WbVariant::Cn);
    }

    #[test]
    fn web_checkin_status_keeps_account_identity() {
        let item = checkin_status_item(
            &json!({"id": "account-1", "email": "user@example.com"}),
            json!({"ok": true, "todayCheckedIn": true}),
        );

        assert_eq!(item["accountId"], "account-1");
        assert_eq!(item["email"], "user@example.com");
        assert_eq!(item["todayCheckedIn"], true);
        assert_eq!(item["variant"], "cn");
    }

    #[test]
    fn web_checkin_status_preserves_failure_state() {
        let item = checkin_status_item(
            &json!({"id": "account-2"}),
            json!({"ok": false, "todayCheckedIn": false, "error": "status failed"}),
        );

        assert_eq!(item["accountId"], "account-2");
        assert_eq!(item["ok"], false);
        assert_eq!(item["error"], "status failed");
    }

    #[test]
    fn web_checkin_status_preserves_exclusion_without_inventing_today_status() {
        let item = checkin_status_item(
            &json!({"id": "excluded"}),
            json!({"ok": false, "result": "skipped", "reason": "auto_checkin_disabled"}),
        );
        assert_eq!(item["accountId"], "excluded");
        assert_eq!(item["reason"], "auto_checkin_disabled");
        assert_eq!(item["result"], "skipped");
        assert!(item.get("todayCheckedIn").is_none());
    }

    #[test]
    fn web_checkin_status_row_carries_variant() {
        let item = checkin_status_item(
            &json!({"id": "ai-1", "variant": "ai"}),
            json!({"ok": false, "statusUnsupported": true}),
        );

        assert_eq!(item["variant"], "ai");
        assert_eq!(item["statusUnsupported"], true);
    }

    /// 一次断连不得永久 409：guard 无论正常收尾还是被取消，都要复位两个独立槽。
    #[test]
    fn switch_state_guard_resets_running_and_progress_on_cancel() {
        let _serial = SWITCH_STATE_TEST_LOCK.lock().unwrap();
        *SWITCH_RUNNING.lock().unwrap() = false;
        *SWITCH_PROGRESS.lock().unwrap() = None;

        let guard = SwitchStateGuard::acquire().expect("首次切号应抢占成功");
        assert!(*SWITCH_RUNNING.lock().unwrap(), "抢占后 running=true");
        assert_eq!(
            SWITCH_PROGRESS.lock().unwrap().as_deref(),
            Some("开始切换账号…")
        );

        // 连点/并发：判重仍生效，且被拒绝的抢占不得复位正在跑的那一轮。
        assert!(SwitchStateGuard::acquire().is_none(), "并发切号必须 409");
        assert!(
            *SWITCH_RUNNING.lock().unwrap(),
            "被拒绝的抢占不得把别人的 running 清掉"
        );
        assert!(
            SWITCH_PROGRESS.lock().unwrap().is_some(),
            "被拒绝的抢占不得清掉别人的进度"
        );

        // 模拟客户端断开：handler future 被 drop，await 之后的手动复位永不执行。
        drop(guard);
        assert!(
            !*SWITCH_RUNNING.lock().unwrap(),
            "取消后 running 不得恒 true"
        );
        assert!(
            SWITCH_PROGRESS.lock().unwrap().is_none(),
            "取消后 progress 必须复位为 null（独立于 running 的第二条路径）"
        );

        // 组合态：标志复位后再次切号不得 409。
        let again = SwitchStateGuard::acquire().expect("复位后应能再次切号");
        drop(again);
        assert!(!*SWITCH_RUNNING.lock().unwrap());
        assert!(SWITCH_PROGRESS.lock().unwrap().is_none());
    }

    /// 两路预览并发各写各槽：一路结束/取消不得清掉另一路；取消后无陈旧帧。
    #[test]
    fn links_preview_slots_do_not_clear_each_other() {
        let _serial = LINKS_STATE_TEST_LOCK.lock().unwrap();
        *LINKS_PREVIEW_PROGRESS.lock().unwrap() = None;
        *LINKS_PREVIEW_CROSS_PROGRESS.lock().unwrap() = None;

        let plain = ProgressReset(&LINKS_PREVIEW_PROGRESS);
        let cross = ProgressReset(&LINKS_PREVIEW_CROSS_PROGRESS);
        *LINKS_PREVIEW_PROGRESS.lock().unwrap() = Some((3, 10));
        *LINKS_PREVIEW_CROSS_PROGRESS.lock().unwrap() = Some((2, 7));

        // 普通预览先结束：跨档那一路的进度必须还在。
        drop(plain);
        assert!(LINKS_PREVIEW_PROGRESS.lock().unwrap().is_none());
        assert_eq!(links_preview_progress_snapshot(), Some((2, 7)));

        // 跨档那一路被取消：槽清空，端点不得留下陈旧帧。
        drop(cross);
        assert_eq!(links_preview_progress_snapshot(), None);

        // 只有跨档槽在跑时，单一轮询端点也必须能读到（前端契约不变）。
        let cross_only = ProgressReset(&LINKS_PREVIEW_CROSS_PROGRESS);
        *LINKS_PREVIEW_CROSS_PROGRESS.lock().unwrap() = Some((1, 4));
        assert_eq!(links_preview_progress_snapshot(), Some((1, 4)));
        drop(cross_only);
        assert_eq!(links_preview_progress_snapshot(), None);
    }

    /// 三宿主同形：webui 的 `{config:{...}}` 与桌面端的裸 config 都解成顶层字段。
    #[test]
    fn submitted_config_normalizes_nested_and_bare_payloads() {
        let cases = [
            (
                json!({"config": {"enabled": true}}),
                json!({"enabled": true}),
            ),
            (
                json!({"enabled": false, "check_interval_minutes": 15}),
                json!({"enabled": false, "check_interval_minutes": 15}),
            ),
            (
                json!({"config": {"owner": "o", "repo": "r", "proxy": "http://127.0.0.1:7897"}}),
                json!({"owner": "o", "repo": "r", "proxy": "http://127.0.0.1:7897"}),
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(submitted_config(&body), &expected);
        }
        // config::save_auto_rotate_config 只读顶层 enabled：解包后落盘值必须是 true。
        assert_eq!(
            submitted_config(&json!({"config": {"enabled": true}}))["enabled"],
            true
        );
    }

    /// `checkUpdate(proxy, force)` 的 GET query 归一化：proxy/force 不再被硬编码丢弃。
    #[test]
    fn update_check_args_keeps_proxy_and_force() {
        let cases: [(HashMap<String, String>, Option<&str>, bool); 5] = [
            (HashMap::new(), None, false),
            (
                HashMap::from([("force".to_string(), "true".to_string())]),
                None,
                true,
            ),
            (
                HashMap::from([
                    ("proxy".to_string(), "http://127.0.0.1:7897".to_string()),
                    ("force".to_string(), "true".to_string()),
                ]),
                Some("http://127.0.0.1:7897"),
                true,
            ),
            (
                HashMap::from([
                    ("proxy".to_string(), String::new()),
                    ("force".to_string(), "false".to_string()),
                ]),
                None,
                false,
            ),
            (
                HashMap::from([("force".to_string(), "1".to_string())]),
                None,
                false,
            ),
        ];
        for (query, expected_proxy, expected_force) in cases {
            let (proxy, force) = update_check_args(&query);
            assert_eq!(proxy, expected_proxy);
            assert_eq!(force, expected_force);
        }
    }

    /// `selected` 缺省 / 非数组 / 混入非字符串 → 只保留字符串，其余当「全部」。
    #[test]
    fn archive_selection_tolerates_missing_and_dirty_input() {
        assert!(archive_selection(&json!({})).is_empty());
        assert!(archive_selection(&json!({ "selected": "accounts" })).is_empty());
        assert_eq!(
            archive_selection(&json!({ "selected": ["accounts", 1, null, "logs"] })),
            vec!["accounts".to_string(), "logs".to_string()]
        );
    }

    /// 同一瞬间的并发请求不能撞到同一个临时文件名（否则互相删/覆盖）。
    #[test]
    fn temp_archive_paths_are_unique() {
        let first = temp_archive_path("export");
        let second = temp_archive_path("export");
        assert_ne!(first, second);
        assert!(first.to_string_lossy().contains("wb-switch-archive-export-"));
    }

    /// 临时归档必须「用完即删」：无论正常返回还是提前出错都不留垃圾。
    #[test]
    fn temp_archive_is_removed_on_drop() {
        let guarded = TempArchive::new("test");
        let path = guarded.path().to_path_buf();
        std::fs::write(&path, b"zip").unwrap();
        drop(guarded);
        assert!(!path.exists());
    }

    /// base64 → 临时文件 → core 读回，字节必须原样；空串与坏 base64 要给中文错误。
    #[test]
    fn decode_archive_round_trips_bytes_and_rejects_bad_input() {
        assert!(decode_archive("test", "").unwrap_err().contains("请先选择"));
        assert!(decode_archive("test", "not base64!!")
            .unwrap_err()
            .contains("base64"));

        let raw = b"PK\x03\x04not-a-real-zip";
        let encoded = BASE64.encode(raw);
        let temp = decode_archive("test", &encoded).unwrap();
        assert_eq!(std::fs::read(temp.path()).unwrap(), raw);
    }

    /// 下载文件名必须带日期且是 zip，前端直接用它在浏览器落盘。
    #[test]
    fn backup_file_name_is_dated_zip() {
        let name = backup_file_name();
        assert!(name.starts_with("wb-switch-backup-"), "{name}");
        assert!(name.ends_with(".zip"), "{name}");
        assert_eq!(name.len(), "wb-switch-backup-YYYYMMDD.zip".len(), "{name}");
    }

    /// body 上限必须装得下「core 允许的最大归档 × base64 膨胀」，否则会出现
    /// 「导出成功、同一个包导不回来」（真正卡在 axum 413，而不是 core 的限额提示）。
    /// core 的 `MAX_TOTAL_BYTES` 是 `pub(crate)`，这里只能写死 64 MiB 并留注释对齐。
    #[test]
    fn archive_body_limit_fits_largest_base64_archive() {
        const CORE_MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
        // base64 最坏膨胀 4/3（向上取整），再加 JSON 外层与转义余量。
        let needed = CORE_MAX_TOTAL_BYTES / 3 * 4 + 4096;
        assert!(
            ARCHIVE_BODY_LIMIT >= needed,
            "ARCHIVE_BODY_LIMIT={ARCHIVE_BODY_LIMIT} 小于 base64 后的最大归档 {needed}"
        );
    }
}
