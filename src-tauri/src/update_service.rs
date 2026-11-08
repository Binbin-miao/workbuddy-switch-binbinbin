//! 统一更新服务：检查 / 下载 / 安装重启的单一状态机。
//!
//! 托盘与前端弹窗共用同一份快照：状态变更统一经 [`set_phase`] 写快照 → emit
//! `update-state` → 刷新托盘菜单；前端只订阅事件，不再自己调用 JS 版 updater。
//! 主窗口关闭（轻量模式、隐藏到托盘）时托盘仍可独立走完 检查 → 下载 → 重启。
//!
//! 检查复用 `wb_switch_core::modules::update::update_check`（release 资产端点 + 302 兜底，
//! 不占 GitHub API 配额）；下载自己走镜像 + 分段并发（见 `update_download`），
//! 只把验签后的字节交给 `Update::install`；代理仅在用户显式开启时才生效。
//!
//! **安装时机**：下载完成的包暂存在内存（`PACKAGE`），由用户点「重启以完成升级」时才
//! 安装。Windows NSIS 安装器要求应用退出后才能完成安装，下载即安装等于强制自动重启；
//! macOS 安装可能触发 `/Applications` 写授权，放在用户主动动作时语义才自然。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_updater::UpdaterExt;

use wb_switch_core::modules::config::now_secs;
use wb_switch_core::modules::update;
use wb_switch_core::modules::update_download;

/// 后台首次检查延迟：启动 15 秒后（替代前端 30 分钟轮询的首屏延迟）。
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(5);
/// 后台检查间隔（30 分钟）。
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);

const BUSY_MESSAGE: &str = "更新任务正在进行中，请稍候";
const NO_PACKAGE_MESSAGE: &str = "没有可安装的更新包，请先下载更新包";
const DEMO_DOWNLOAD_MESSAGE: &str = "演示模式已跳过更新下载";
const DEMO_RESTART_MESSAGE: &str = "演示模式已跳过更新安装";
/// 用户取消下载后的提示文案（spec [D4] 边界⑤）。
const CANCELLED_MESSAGE: &str = "已取消下载";
/// 涓流采样节拍：1 秒一次，比 [D1] 的 15 秒窗口细得多，判定延迟 ≈ 窗口 + 1 秒。
const STALL_SAMPLE: Duration = Duration::from_secs(1);

/// 更新阶段。前端与托盘菜单都只读这一份状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdatePhase {
    /// 尚未检查 / 未开始。
    Idle,
    /// 正在检查更新源。
    Checking,
    /// 已是最新版本。
    UpToDate,
    /// 有新版且可下载。
    Available,
    /// 正在后台下载更新包。
    Downloading,
    /// 包已下载完成，等待用户重启（安装发生在重启时）。
    ReadyToRestart,
    /// 检查 / 下载 / 安装失败，可按文案重试。
    Error,
}

/// 更新状态快照：命令返回值、`update-state` 事件负载、托盘菜单文案的唯一来源。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSnapshot {
    pub phase: UpdatePhase,
    /// 目标版本号（无 `v` 前缀）；检查失败或已是最新时为 None。
    pub latest: Option<String>,
    /// 下载进度（0-100）；总量未知（无 `Content-Length`）时为 None。
    pub percent: Option<u8>,
    /// 错误 / 提示文案。
    pub message: Option<String>,
    /// 最近一次检查完成时刻（秒）。
    pub checked_at: Option<i64>,
}

impl UpdateSnapshot {
    fn idle() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            latest: None,
            percent: None,
            message: None,
            checked_at: None,
        }
    }
}

/// 已下载的更新包：bytes 交给 `Update::install`，`update` 提供安装上下文
/// （安装目标路径、安装器参数等，无法在外部重建）。
struct DownloadedPackage {
    version: String,
    bytes: Vec<u8>,
    update: tauri_plugin_updater::Update,
}

static SNAPSHOT: OnceLock<Mutex<UpdateSnapshot>> = OnceLock::new();
static PACKAGE: OnceLock<Mutex<Option<DownloadedPackage>>> = OnceLock::new();
/// 「检查 / 下载 / 安装」共用互斥：任一任务在跑时重复点击直接拒绝。
static BUSY: AtomicBool = AtomicBool::new(false);
/// 托盘菜单重建档位（每 10% 一档）；`u8::MAX` 表示当前无下载进度。
static MENU_PROGRESS_BUCKET: AtomicU8 = AtomicU8::new(u8::MAX);
/// 本轮下载是否已打过「进度开始上报」日志（排障用，一次下载只打一行）。
static PROGRESS_REPORTED: AtomicBool = AtomicBool::new(false);

/// 没有总量（服务端不回 Content-Length）时，是否已经写过「正在下载…」文案。
static NO_TOTAL_TOOLTIP_SET: AtomicBool = AtomicBool::new(false);

/// 自动下载连续失败次数。**只统计 `auto=true` 的下载**，手动点击会归零。
///
/// 为什么必须单独计数：`update_check` 只拉 1 KB 清单，能通不代表包能下
/// （镜像/直连对二进制与 JSON 的策略可能不同）。只看检查失败的退避，会在
/// 「清单通、下载一直失败」的机器上每 30 分钟重试一次并弹出两条通知。
static AUTO_DOWNLOAD_FAILURES: AtomicU32 = AtomicU32::new(0);
/// 最近一次自动下载失败的时间（Unix 秒，与 core 的 `now_secs` 同为 i64），用于计算退避窗口。
static AUTO_DOWNLOAD_FAILED_AT: AtomicI64 = AtomicI64::new(0);

/// 自动下载是否仍在退避窗口内（失败后不立刻重试，也不发通知）。
fn auto_download_backoff_active() -> bool {
    let failures = AUTO_DOWNLOAD_FAILURES.load(Ordering::Acquire);
    if failures == 0 {
        return false;
    }
    let failed_at = AUTO_DOWNLOAD_FAILED_AT.load(Ordering::Acquire);
    let elapsed = now_secs().saturating_sub(failed_at);
    // 系统时间被回拨时 elapsed 可能为负：按"仍在退避"处理，避免立刻重试。
    elapsed < check_delay(failures).as_secs() as i64
}

// ---------------------------------------------------------------------------
// 互斥
// ---------------------------------------------------------------------------

/// 更新任务互斥 guard：覆盖「检查 / 下载 / 安装」全过程，drop 即释放。
struct BusyGuard;

impl BusyGuard {
    /// 抢占更新任务；已有任务在跑时返回 None（调用方按各自契约拒绝）。
    fn acquire() -> Option<Self> {
        BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// 下载控制块：取消 / 涓流标志的唯一宿主（core 只读它，宿主只写它）
// ---------------------------------------------------------------------------

/// 当前正在跑的下载控制块。取消命令靠它找到任务；任务结束必须清空，
/// 否则用户下次点取消会打到一块早已结束的僵尸块上（阶段却不动，像点坏了）。
static ACTIVE_CONTROL: OnceLock<Mutex<Option<Arc<update_download::DownloadControl>>>> =
    OnceLock::new();

fn active_control_cell() -> &'static Mutex<Option<Arc<update_download::DownloadControl>>> {
    ACTIVE_CONTROL.get_or_init(|| Mutex::new(None))
}

/// 把控制块挂到全局槽位上，Drop 即清空。
///
/// 用 guard 而不是手写三次清理：成功 / 失败 / 取消三条路都要清，漏一条就是「取消打到僵尸块」。
struct ActiveControlGuard;

impl ActiveControlGuard {
    fn install(control: Arc<update_download::DownloadControl>) -> Self {
        *active_control_cell()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(control);
        Self
    }
}

impl Drop for ActiveControlGuard {
    fn drop(&mut self) {
        *active_control_cell()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// 换候选通道：给下一条通道装一块干净的控制块。
///
/// 为什么必须换块：停滞标志是「**这条**通道涓流」的一次性判定，带进下一条会让
/// `check()` 立刻返回「速度过低」，好通道连试都试不上。取消则是「**整条**任务结束」
/// 的意图，必须跨候选保留 —— 所以整段持锁并与 [`cancel_download`] 互斥，取消不会落空。
fn install_candidate_control(
    previous: &Arc<update_download::DownloadControl>,
) -> Arc<update_download::DownloadControl> {
    let mut slot = active_control_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fresh = Arc::new(update_download::DownloadControl::new());
    if previous.is_cancelled() {
        fresh.cancel();
    }
    *slot = Some(Arc::clone(&fresh));
    fresh
}

/// 取消当前下载：幂等 —— 没有任务在跑时返回 `Ok(())`，不报错、不改阶段（spec [D4] 边界④）。
pub fn cancel_download() -> Result<(), String> {
    let slot = active_control_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(control) = slot.as_ref() else {
        eprintln!("[更新] 取消下载：当前没有下载任务（幂等返回）");
        return Ok(());
    };
    // 持锁置位：与「换候选」互斥，避免取消落在正被替换掉的旧控制块上。
    control.cancel();
    eprintln!("[更新] 取消下载：已通知当前通道中止，阶段将回到可升级");
    Ok(())
}

// ---------------------------------------------------------------------------
// 快照读写（无宿主依赖：单测直接驱动这一层）
// ---------------------------------------------------------------------------

fn snapshot_cell() -> &'static Mutex<UpdateSnapshot> {
    SNAPSHOT.get_or_init(|| Mutex::new(UpdateSnapshot::idle()))
}

fn package_cell() -> &'static Mutex<Option<DownloadedPackage>> {
    PACKAGE.get_or_init(|| Mutex::new(None))
}

fn with_snapshot<T>(edit: impl FnOnce(&mut UpdateSnapshot) -> T) -> T {
    let mut guard = snapshot_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    edit(&mut guard)
}

/// 读当前快照（`update_state` 命令与托盘菜单共用）。
pub fn snapshot() -> UpdateSnapshot {
    with_snapshot(|snapshot| snapshot.clone())
}

/// 进入检查阶段。
///
/// 下载中 / 已就绪不被后台检查改写：否则托盘菜单会从「正在下载 / 重启以完成升级」
/// 闪回「正在检查…」，用户正准备点的入口会消失。
fn begin_check() -> UpdateSnapshot {
    with_snapshot(|snapshot| {
        if matches!(
            snapshot.phase,
            UpdatePhase::Downloading | UpdatePhase::ReadyToRestart
        ) {
            return snapshot.clone();
        }
        snapshot.phase = UpdatePhase::Checking;
        snapshot.percent = None;
        snapshot.message = None;
        snapshot.clone()
    })
}

/// 写入检查结果（`ok=false` 或 `hasUpdate=false` 分别落到 Error / UpToDate）。
fn apply_check_result(result: &Value) -> UpdateSnapshot {
    with_snapshot(|snapshot| {
        if matches!(
            snapshot.phase,
            UpdatePhase::Downloading | UpdatePhase::ReadyToRestart
        ) {
            return snapshot.clone();
        }
        snapshot.percent = None;
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            // 已有明确目标版本时（Available 被后台复查带进 Checking、或下载失败后
            // 再检查），失败不得清掉升级入口：否则 30 分钟后台检查一旦网络失败，
            // 托盘会从「升级到 vX」变成「检查更新失败」。
            if snapshot.latest.is_some()
                && matches!(
                    snapshot.phase,
                    UpdatePhase::Checking | UpdatePhase::Available
                )
            {
                snapshot.phase = UpdatePhase::Available;
                snapshot.message = None;
                return snapshot.clone();
            }
            snapshot.phase = UpdatePhase::Error;
            snapshot.latest = None;
            snapshot.message = result
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| result.get("error").and_then(Value::as_str))
                .map(str::to_string);
            return snapshot.clone();
        }
        let latest = result
            .get("latest")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let has_update =
            result.get("hasUpdate").and_then(Value::as_bool) == Some(true) && !latest.is_empty();
        snapshot.phase = if has_update {
            UpdatePhase::Available
        } else {
            UpdatePhase::UpToDate
        };
        snapshot.latest = has_update.then_some(latest);
        snapshot.message = None;
        snapshot.checked_at = result
            .get("checkedAt")
            .and_then(Value::as_i64)
            .or_else(|| Some(now_secs()));
        snapshot.clone()
    })
}

/// 进入下载阶段（进度百分比由首个 chunk 回调填入，总量未知时保持 None）。
fn begin_download(latest: &str) -> UpdateSnapshot {
    MENU_PROGRESS_BUCKET.store(0, Ordering::Release);
    PROGRESS_REPORTED.store(false, Ordering::Release);
    NO_TOTAL_TOOLTIP_SET.store(false, Ordering::Release);
    with_snapshot(|snapshot| {
        snapshot.phase = UpdatePhase::Downloading;
        snapshot.latest = Some(latest.to_string());
        snapshot.percent = None;
        snapshot.message = None;
        snapshot.clone()
    })
}

/// 下载完成：包已存入 `PACKAGE`，菜单转为「重启以完成升级」。
fn mark_ready_to_restart(latest: &str) -> UpdateSnapshot {
    MENU_PROGRESS_BUCKET.store(u8::MAX, Ordering::Release);
    with_snapshot(|snapshot| {
        snapshot.phase = UpdatePhase::ReadyToRestart;
        snapshot.latest = Some(latest.to_string());
        snapshot.percent = Some(100);
        snapshot.message = None;
        snapshot.clone()
    })
}

/// 失败：置 Error（保留 `latest`，托盘据此区分「检查失败」与「下载失败」两种重试文案）。
fn fail(message: String) -> UpdateSnapshot {
    MENU_PROGRESS_BUCKET.store(u8::MAX, Ordering::Release);
    with_snapshot(|snapshot| {
        snapshot.phase = UpdatePhase::Error;
        snapshot.percent = None;
        snapshot.message = Some(message);
        snapshot.clone()
    })
}

/// 用户取消：回到「可升级」并保留 `latest`，用户可以再点一次下载（spec [D4] 边界⑤）。
///
/// 取消**不算失败**：stage 不进 Error、不弹通知、不进退避（边界⑥），
/// 否则用户手滑点一次取消就要等 30 分钟才有下一次自动下载。
fn mark_cancelled() -> UpdateSnapshot {
    MENU_PROGRESS_BUCKET.store(u8::MAX, Ordering::Release);
    with_snapshot(|snapshot| {
        snapshot.phase = UpdatePhase::Available;
        snapshot.percent = None;
        snapshot.message = Some(CANCELLED_MESSAGE.to_string());
        snapshot.clone()
    })
}

/// 换源时把进度打回 0 并广播：不能让前端停在上一通道留下的百分比（spec [D5]）。
fn rewind_progress() -> UpdateSnapshot {
    MENU_PROGRESS_BUCKET.store(0, Ordering::Release);
    with_snapshot(|snapshot| {
        snapshot.percent = Some(0);
        snapshot.clone()
    })
}

/// 下载进度换算：总量未知或为 0 → None；超出总量按 100 截断。
fn percent_of(downloaded: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|total| *total > 0)?;
    let percent = downloaded.saturating_mul(100) / total;
    Some(percent.min(100) as u8)
}

/// 记录一次进度回调：整数百分比变化才写快照并返回新值（未变化返回 None）。
fn advance_progress(downloaded: u64, total: Option<u64>) -> Option<u8> {
    let percent = percent_of(downloaded, total)?;
    with_snapshot(|snapshot| {
        if snapshot.percent == Some(percent) {
            return None;
        }
        snapshot.percent = Some(percent);
        Some(percent)
    })
}

/// 菜单重建判定：跨 10% 档时才重建（每 1% 重建会让打开的菜单闪掉）。
fn should_rebuild_menu(percent: u8) -> bool {
    let bucket = percent / 10;
    MENU_PROGRESS_BUCKET.swap(bucket, Ordering::AcqRel) != bucket
}

// ---------------------------------------------------------------------------
// 对外能力
// ---------------------------------------------------------------------------

/// 检查更新：走状态机（Checking → Available / UpToDate / Error），返回与旧
/// `check_update` 完全同形的结果（前端展示与 webui 侧契约不变）。
///
/// 已有更新任务在跑时直接返回忙碌错误，不并发触发第二次检查。
pub async fn check<R: Runtime>(app: &AppHandle<R>, proxy: Option<&str>, force: bool) -> Value {
    let Some(_busy) = BusyGuard::acquire() else {
        // 互斥拒绝不写日志到 stderr 之外：托盘文案已表达同一件事。
        eprintln!("[更新] 已有任务在跑，跳过本次检查");
        let mut value = json!({
            "ok": false,
            "error": BUSY_MESSAGE,
            "message": BUSY_MESSAGE,
        });
        value["releaseUrl"] = json!(release_url());
        return value;
    };
    eprintln!("[更新] 检查更新（force={force}）");
    set_phase(app, begin_check());
    let result = update::update_check(proxy, force).await;
    let snapshot = set_phase(app, apply_check_result(&result));
    eprintln!(
        "[更新] 检查结果：{:?} latest={:?}",
        snapshot.phase, snapshot.latest
    );
    result
}

/// 启动下载：立即返回，进度走 `update-state` 事件与托盘 tooltip。
///
/// 下载在后台任务里进行（含重新 check 一次以拿到 `Update` 句柄），因此托盘与前端
/// 都不需要等网络；重复调用由 `BUSY` 拒绝。
pub fn start_download<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    start_download_inner(app, false)
}

/// 下载入口的共用实现。`auto=true` 表示由定时检查触发：失败计入退避计数。
fn start_download_inner<R: Runtime>(app: &AppHandle<R>, auto: bool) -> Result<(), String> {
    if crate::is_screenshot_demo() {
        return Err(DEMO_DOWNLOAD_MESSAGE.to_string());
    }
    let Some(busy) = BusyGuard::acquire() else {
        eprintln!("[更新] 已有任务在跑，跳过本次下载");
        return Err(BUSY_MESSAGE.to_string());
    };
    // 手动点击等同"用户主动重试"：清掉自动重试的失败计数，退避立即结束。
    if !auto {
        AUTO_DOWNLOAD_FAILURES.store(0, Ordering::Release);
    }
    eprintln!("[更新] 启动下载任务（auto={auto}）");
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _busy = busy;
        match download_package(&app).await {
            // 下载完成：包先暂存，等用户点「重启以完成升级」时才安装。
            Ok(Some(package)) => {
                AUTO_DOWNLOAD_FAILURES.store(0, Ordering::Release);
                let version = package.version.clone();
                eprintln!("[更新] 下载完成 v{version}（{} 字节）", package.bytes.len());
                store_package(package);
                set_phase(&app, mark_ready_to_restart(&version));
                reset_tooltip(&app);
                notify(
                    &app,
                    "更新已就绪",
                    &format!("v{version} 已下载完成，点「重启以完成升级」后生效"),
                );
            }
            // 更新源已无可用包（版本已追平）。
            Ok(None) => {
                AUTO_DOWNLOAD_FAILURES.store(0, Ordering::Release);
                eprintln!("[更新] 更新源无可用更新包，转为已是最新");
                set_phase(
                    &app,
                    apply_check_result(&json!({"ok": true, "hasUpdate": false})),
                );
                reset_tooltip(&app);
            }
            // 用户主动取消：回到「可升级」，不计数、不通知、不进退避（spec [D4] 边界⑤⑥）。
            // 必须排在通用失败分支之前，否则会被当成一次下载失败计入自动重试退避。
            Err(message) if message == update_download::CANCELLED_ERROR => {
                eprintln!("[更新] 下载已被用户取消，阶段回到可升级");
                set_phase(&app, mark_cancelled());
                reset_tooltip(&app);
            }
            Err(message) => {
                if auto {
                    let failures = AUTO_DOWNLOAD_FAILURES.fetch_add(1, Ordering::AcqRel) + 1;
                    AUTO_DOWNLOAD_FAILED_AT.store(now_secs(), Ordering::Release);
                    eprintln!("[更新] 自动下载连续失败 {failures} 次，进入退避");
                }
                eprintln!("[更新] 下载失败：{message}");
                set_phase(&app, fail(message.clone()));
                reset_tooltip(&app);
                notify(&app, "更新下载失败", &message);
            }
        }
    });
    Ok(())
}

/// 安装已下载的更新包并重启应用（用户点「重启以完成升级」时调用）。
///
/// 无包时返回可读错误（不 panic）；安装失败置 Error + 通知，并释放已暂存的包。
pub async fn restart<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    if crate::is_screenshot_demo() {
        return Err(DEMO_RESTART_MESSAGE.to_string());
    }
    // 无包时返回可读错误（不 panic）。
    let package = take_package_for_install()?;
    let Some(_busy) = BusyGuard::acquire() else {
        store_package(package);
        return Err(BUSY_MESSAGE.to_string());
    };

    let DownloadedPackage {
        version,
        bytes,
        update,
    } = package;
    eprintln!("[更新] 开始安装 v{version}");
    // 安装会解压整包并替换应用（macOS 未授权时经 AppleScript 在主线程弹授权框），
    // 放 blocking 线程，避免占住 async worker。
    let installed = tauri::async_runtime::spawn_blocking(move || update.install(bytes)).await;
    match installed {
        Ok(Ok(())) => match crate::commands::relaunch_app_inner(app) {
            // macOS / Linux：安装已完成，由本进程拉起新版本。Windows 侧安装器会先让
            // 本进程退出（`install` 内部 `std::process::exit`），到不了这里。
            Ok(()) => Ok(()),
            Err(error) => {
                // 包已装好，重启失败只剩「手动重启」一条路：明确告知，避免用户以为要重下。
                let message = format!("v{version} 已安装，请手动重启应用完成升级：{error}");
                eprintln!("[更新] 重启失败：{message}");
                set_phase(app, fail(message.clone()));
                notify(app, "更新已安装", &message);
                Err(message)
            }
        },
        Ok(Err(error)) => {
            let message = format!("安装 v{version} 失败：{error}");
            eprintln!("[更新] {message}");
            set_phase(app, fail(message.clone()));
            notify(app, "更新安装失败", &message);
            Err(message)
        }
        Err(error) => {
            let message = format!("安装 v{version} 失败：{error}");
            eprintln!("[更新] {message}");
            set_phase(app, fail(message.clone()));
            notify(app, "更新安装失败", &message);
            Err(message)
        }
    }
}

/// 后台定时检查：首次延迟 [`FIRST_CHECK_DELAY`]，之后每 [`CHECK_INTERVAL`] 一次。
///
/// 常驻循环随进程存活，轻量模式（WebView 销毁）与主窗口关闭时同样在跑；
/// 未带 force 走 core 的 6 小时缓存，因此多数轮次不发网络请求。
///
/// 发现新版本且用户没跳过时**自动转后台下载**（用户诉求："不是说后台下载吗"）。
pub fn spawn_periodic_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        // 连续失败计数：断网机器不该每 30 分钟重试一次 + 每轮发一条"发现新版本"。
        let mut consecutive_failures: u32 = 0;
        loop {
            // 注意顺序：`check` 内部持 BusyGuard 并在返回时释放；自动下载必须在它
            // 释放**之后**另起一次获取。BusyGuard 不可重入，在 check 内部调用
            // start_download 会 100% 被 BUSY 拒绝。
            let result = check(&app, None, false).await;
            let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if ok {
                consecutive_failures = 0;
                maybe_auto_download(&app, &result).await;
            } else {
                consecutive_failures = consecutive_failures.saturating_add(1);
                eprintln!("[更新] 第 {consecutive_failures} 次连续失败，进入退避");
            }
            tokio::time::sleep(check_delay(consecutive_failures)).await;
        }
    });
}

/// 下一轮检查的等待时长：正常 30 分钟；连续失败按 30/60/120/240/360 分钟退避，上限 6 小时。
///
/// 没有退避时，一台断网的机器会每 30 分钟做一次注定失败的请求；一旦哪天出错路径
/// 带上通知，就变成周期性骚扰。成功一次即归零（调用方负责重置计数）。
fn check_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return CHECK_INTERVAL;
    }
    // 1/2/4/8/16 倍增，再夹到 6 小时上限；`.min(5)` 保证移位不超过 16 倍。
    let doublings = consecutive_failures.min(5) - 1;
    let minutes = 30u64.saturating_mul(1u64 << doublings).min(6 * 60);
    Duration::from_secs(minutes * 60)
}

/// 检查结果说「有新版」且用户没跳过，就自动进后台下载。
///
/// 只做决策，不做下载：真正的下载仍走 [`start_download`]（含 BusyGuard、通知、托盘）。
async fn maybe_auto_download<R: Runtime>(app: &AppHandle<R>, result: &Value) {
    if crate::is_screenshot_demo() {
        return;
    }
    if !update::auto_download_enabled() {
        return;
    }
    // 下载连续失败后先退避：清单能拉通不代表包能下（镜像对 JSON 与二进制策略可能不同）。
    // 没有这道闸，"检查通、下载一直失败"的机器会每 30 分钟重试一次并弹两条通知。
    if auto_download_backoff_active() {
        eprintln!("[更新] 自动下载处于退避期，跳过本次");
        return;
    }
    let has_update = result
        .get("hasUpdate")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(latest) = result
        .get("latest")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    if !has_update {
        return;
    }
    if update::skipped_version().as_deref() == Some(latest) {
        eprintln!("[更新] v{latest} 已被用户跳过，不自动下载");
        return;
    }
    // 阶段必须是 Available：检查刚失败或是"已最新"时不该下载。
    // 注意 `UpdatePhase` 是枚举（序列化后才变字符串），必须用变体比较。
    if snapshot().phase != UpdatePhase::Available {
        return;
    }
    match start_download_inner(app, true) {
        Ok(()) => {
            eprintln!("[更新] v{latest} 自动后台下载已启动");
            notify(
                app,
                "发现新版本",
                &format!("v{latest} 正在后台下载，完成后会提示你重启安装"),
            );
        }
        // 用户可能刚在弹窗里手点了下载：BUSY 属正常拒绝，不重复、不报错。
        Err(error) => eprintln!("[更新] 自动下载未启动：{error}"),
    }
}

// ---------------------------------------------------------------------------
// 下载 / 安装内部实现
// ---------------------------------------------------------------------------

/// 下载更新包：`Ok(None)` 表示更新源当前没有可用包（版本已追平）。
async fn download_package<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<DownloadedPackage>, String> {
    let proxy = configured_proxy();
    let Some(update) = check_package(app, proxy.as_deref()).await? else {
        return Ok(None);
    };

    let version = update.version.clone();
    let pubkey = updater_pubkey(app)?;
    set_phase(app, begin_download(&version));

    let control = Arc::new(update_download::DownloadControl::new());
    // 挂上全局槽位：取消命令从这一刻起有效；成功 / 失败 / 取消都由 Drop 清空。
    let _active = ActiveControlGuard::install(Arc::clone(&control));

    // 整包大小由 core 在首个进度回调里给出（探测阶段才知道）；涓流判据需要它 ——
    // total 未知就不判（spec [D1] 边界③）。0 表示「还不知道」，包大小不可能为 0。
    let total = Arc::new(AtomicU64::new(0));
    let progress: update_download::Progress = {
        let app = app.clone();
        let total = Arc::clone(&total);
        Arc::new(move |downloaded, total_bytes| {
            if let Some(size) = total_bytes {
                total.store(size, Ordering::Relaxed);
            }
            on_download_chunk(&app, downloaded, total_bytes)
        })
    };
    let bytes =
        download_verified(app, &update, proxy.as_deref(), &pubkey, &control, &total, &progress)
            .await?;
    Ok(Some(DownloadedPackage {
        version,
        bytes,
        update,
    }))
}

/// 检查更新包：清单按「镜像 → GitHub」拉，失败再换另一条代理通道。
///
/// 插件的 `check()` 默认走系统代理且**没有超时**：系统代理指向死端口时会一直挂着，
/// 用户看到的就是「检查说有新版，点下载却永远停在 0%」。这里换成与
/// [`update::update_check`] 同一份候选（镜像优先），并给 20 秒上限。
async fn check_package<R: Runtime>(
    app: &AppHandle<R>,
    proxy: Option<&str>,
) -> Result<Option<tauri_plugin_updater::Update>, String> {
    // 有显式代理就先用它；没有则先直连——系统代理会把镜像流量带去海外节点。
    let first_with_proxy = proxy.is_some();
    let first_error = match build_updater(app, proxy, first_with_proxy)?.check().await {
        Ok(update) => return Ok(update),
        Err(error) => error,
    };
    eprintln!("[更新] 清单检查失败（{first_error}），换另一条通道重试");
    build_updater(app, proxy, !first_with_proxy)?
        .check()
        .await
        .map_err(|error| format!("检查更新包失败：{first_error}；重试：{error}"))
}

/// 构造 updater：清单端点复用下载候选（镜像优先），并按需决定走不走代理。
fn build_updater<R: Runtime>(
    app: &AppHandle<R>,
    proxy: Option<&str>,
    use_proxy: bool,
) -> Result<tauri_plugin_updater::Updater, String> {
    let cfg = update::load_github_config();
    let owner = cfg.get("owner").and_then(Value::as_str).unwrap_or("");
    let repo = cfg.get("repo").and_then(Value::as_str).unwrap_or("");
    let base = format!("https://github.com/{owner}/{repo}/releases/latest/download/latest.json");
    let endpoints: Vec<tauri::Url> = update_download::download_candidates(&base)
        .iter()
        .filter_map(|url| tauri::Url::parse(url).ok())
        .collect();

    let mut builder = app.updater_builder().timeout(Duration::from_secs(20));
    if !endpoints.is_empty() {
        builder = builder
            .endpoints(endpoints)
            .map_err(|error| format!("更新清单地址无效：{error}"))?;
    }
    builder = match (use_proxy, proxy) {
        (true, Some(proxy)) => builder.proxy(
            tauri::Url::parse(proxy).map_err(|error| format!("更新代理地址无效：{error}"))?,
        ),
        (true, None) => builder,
        (false, _) => builder.no_proxy(),
    };
    builder
        .build()
        .map_err(|error| format!("初始化更新器失败：{error}"))
}

/// 按 [`update_download::rank_candidates`] 的实测速度排序，依次尝试、每个都验签，
/// 任一成功即返回；排序只是"先试谁"，不是跳过验签的理由。
///
/// 三道闸门（spec [D1]/[D2]/[D3]）：
/// - 单候选 `CANDIDATE_BUDGET`，到点换下一条；
/// - 每秒采样窗口吞吐判涓流，判真即给控制块置停滞、core 在 500ms 内放弃这条通道；
/// - 整段（测速 + 换源 + 验签）`DOWNLOAD_DEADLINE`，超时按 [D3] 文案收尾。
///
/// 涓流与超预算只换候选，**取消则终止整条任务**（[D4] 边界⑥）。
///
/// 镜像只负责「把字节搬快」，可信度由 `verify_signature` 兜底：任何一个镜像返回
/// 脏数据或半截文件都会在这里被拒，然后自动换下一个通道。
async fn download_verified<R: Runtime>(
    app: &AppHandle<R>,
    update: &tauri_plugin_updater::Update,
    proxy: Option<&str>,
    pubkey: &str,
    control: &Arc<update_download::DownloadControl>,
    total: &Arc<AtomicU64>,
    progress: &update_download::Progress,
) -> Result<Vec<u8>, String> {
    // 总预算从测速之前开始算：测速本身也要吃候选的 5 秒，不能让它游离在兜底之外。
    let deadline = tokio::time::Instant::now() + update_download::DOWNLOAD_DEADLINE;
    let candidates = update_download::download_candidates(update.download_url.as_str());
    // 候选顺序不再写死：镜像与直连谁快完全取决于用户网络（实测过镜像 20 KB/s、
    // 代理直连 3.5 MB/s 的机器，也见过相反的）。按小样本实测速度重排，慢的沉底兜底。
    let candidates = update_download::rank_candidates(&candidates, proxy).await;
    let mut current = Arc::clone(control);
    let mut failures: Vec<String> = Vec::new();
    let mut timed_out = false;

    for (index, candidate) in candidates.iter().enumerate() {
        // 取消是「整条任务结束」：换源继续下载等于没取消。
        if current.is_cancelled() {
            return Err(update_download::CANCELLED_ERROR.to_string());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        let label = source_label(candidate);
        if index == 0 {
            eprintln!("[更新] 下载尝试：{label}");
        } else {
            eprintln!("[更新] 换源重试：{label}（已尝试 {index} 条通道）");
            // 进度归零 + 停滞标志不复用 + 取消延续，三件事都在这里落地。
            current = install_candidate_control(&current);
            set_phase(app, rewind_progress());
        }

        let budget = remaining.min(update_download::CANDIDATE_BUDGET);
        let sampler = spawn_stall_watch(Arc::clone(&current), Arc::clone(total));
        let attempt = tokio::time::timeout(
            budget,
            update_download::fetch(candidate, proxy, &current, Arc::clone(progress)),
        )
        .await;
        // 采样任务只活在这一条候选的生命期里，超时 / 提前返回都要停掉。
        sampler.abort();

        match attempt {
            Ok(Ok(bytes)) => match update_download::verify_signature(&bytes, &update.signature, pubkey)
            {
                Ok(()) => {
                    eprintln!(
                        "[更新] 下载完成并验签通过：{label}（{} 字节）",
                        bytes.len()
                    );
                    return Ok(bytes);
                }
                Err(error) => {
                    eprintln!("[更新] {label} 验签失败：{error}");
                    failures.push(format!("{label}：{error}"));
                }
            },
            // 取消直接上抛：让 start_download_inner 走「回到可升级」分支，而不是当失败重试。
            Ok(Err(error)) if error == update_download::CANCELLED_ERROR => return Err(error),
            Ok(Err(error)) => {
                eprintln!("[更新] {label} 下载失败：{error}");
                failures.push(format!("{label}：{error}"));
            }
            // 预算到点：整段下载也到期了就直接收尾，否则只记这一条通道超预算。
            Err(_) if tokio::time::Instant::now() >= deadline => {
                failures.push(format!("{label}：预算耗尽"));
                timed_out = true;
                break;
            }
            Err(_) => {
                eprintln!(
                    "[更新] {label} 超出单候选预算（{} 秒），换下一个候选",
                    budget.as_secs()
                );
                failures.push(format!("{label}：{} 秒内没下完", budget.as_secs()));
            }
        }
    }

    if timed_out {
        eprintln!(
            "[更新] 下载超时（6 分钟），各通道失败原因：{}",
            failures.join("；")
        );
        // 文案按 spec [D3]：说清试过几条通道，用户才知道不是卡死，也知道还能重试。
        return Err(format!(
            "下载超时（6 分钟），已尝试 {} 条通道",
            failures.len()
        ));
    }
    Err(format!("所有下载通道都失败了（{}）", failures.join("；")))
}

/// 每秒量一次窗口吞吐：低于地板就给控制块置停滞，core 在 500ms 内放弃这条通道。
///
/// `bytes` 传窗口内的**增量**而不是累计量：整段均速会被前一段的快跑掩盖，
/// 用户看到的「卡在 85% 再也不动」正是后段涓流，只有窗口增量判得出来。
///
/// **首判要比常规窗口晚 `STALL_WARMUP`（即 20 秒）**：`probe()` 的 1.5 MiB 测速与建链
/// 时间只进分母、不进分子（它们不经过 `control.record_bytes`）。若首窗仍是 15 秒，
/// 一条稳态 110 KB/s 的正常通道会被稀释成 ≈73 KB/s 而误判涓流，换源后还会按同一
/// 机制连续误杀。之后每轮恢复 15 秒滑动窗口。
fn spawn_stall_watch(
    control: Arc<update_download::DownloadControl>,
    total: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut window_started = std::time::Instant::now();
        let mut window_bytes = control.downloaded();
        let mut window_length = update_download::STALL_WINDOW + update_download::STALL_WARMUP;
        loop {
            tokio::time::sleep(STALL_SAMPLE).await;
            let elapsed = window_started.elapsed();
            // 不足一个窗口就没有判据（首窗是 20 秒，之后 15 秒）。
            if elapsed < window_length {
                continue;
            }
            let downloaded = control.downloaded();
            let window_bytes_now = downloaded.saturating_sub(window_bytes);
            // 0 是「总量还不知道」的哨兵：未知就不判涓流（[D1] 边界③）。
            let known_total = match total.load(Ordering::Relaxed) {
                0 => None,
                size => Some(size),
            };
            if update_download::throughput_is_stalled(elapsed, window_bytes_now, known_total) {
                let throughput = window_bytes_now / elapsed.as_secs().max(1) / 1024;
                control.stall(&format!(
                    "窗口 {} 秒只下了 {window_bytes_now} 字节（约 {throughput} KB/s，累计 {downloaded} 字节），低于 80 KB/s 地板",
                    elapsed.as_secs()
                ));
                return;
            }
            // 滑动窗口：判完把窗口起点挪到此刻，下一轮只看新的 15 秒。
            window_started = std::time::Instant::now();
            window_bytes = downloaded;
            window_length = update_download::STALL_WINDOW;
        }
    })
}

/// 下载通道的人类可读标签，用于日志与失败文案。
fn source_label(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .filter(|host| !host.is_empty())
        .unwrap_or(url);
    let is_mirror = update_download::MIRROR_PREFIXES
        .iter()
        .any(|prefix| url.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')));
    if is_mirror {
        format!("镜像 {host}")
    } else {
        "GitHub 直连".to_string()
    }
}

/// 读取 `tauri.conf.json → plugins.updater.pubkey`，与插件共用同一份配置。
///
/// 自己下载意味着要自己验签，公钥绝不能在这里写死第二份。
fn updater_pubkey<R: Runtime>(app: &AppHandle<R>) -> Result<String, String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|value| value.get("pubkey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "缺少更新公钥配置（tauri.conf.json → plugins.updater.pubkey）".to_string())
}

/// 单个下载分包的回调：tooltip 实时更新；快照与事件按整数百分比；菜单跨 10% 才重建。
fn on_download_chunk<R: Runtime>(app: &AppHandle<R>, downloaded: u64, total: Option<u64>) {
    // 先判百分比有没有变：8.8 MB 的包会回调上千次，每次都改托盘 tooltip
    // 等于把下载线程拖去做上千次 UI 调用。
    let Some(percent) = percent_of(downloaded, total) else {
        // 极少数服务端不回 Content-Length：没有百分比，但至少把文案推进到「正在下载」。
        if !NO_TOTAL_TOOLTIP_SET.swap(true, Ordering::AcqRel) {
            set_tooltip(app, &download_tooltip(None));
        }
        return;
    };
    if advance_progress(downloaded, total).is_none() {
        return;
    }
    set_tooltip(app, &download_tooltip(Some(percent)));
    if !PROGRESS_REPORTED.swap(true, Ordering::AcqRel) {
        eprintln!("[更新] 进度开始上报：{percent}%（total={total:?}）");
    }
    let _ = app.emit("update-state", snapshot());
    if should_rebuild_menu(percent) {
        eprintln!("[更新] 下载进度 {percent}%（重建托盘菜单）");
        refresh_tray(app);
    }
}

fn download_tooltip(percent: Option<u8>) -> String {
    match percent {
        Some(percent) => format!("正在下载更新 {percent}%"),
        None => "正在下载更新…".to_string(),
    }
}

/// 更新代理：只有用户在设置里显式开启时才注入 updater builder。
///
/// 读的是 `update::effective_proxy()`（带 `proxy_enabled` 判定），不是裸的
/// `proxy` 字段——历史地址失效时不能让整条更新链跟着挂掉。
fn configured_proxy() -> Option<String> {
    update::effective_proxy()
}

fn store_package(package: DownloadedPackage) {
    *package_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(package);
}

/// 取出待安装的包；无包时返回可读错误（不 panic）。
fn take_package_for_install() -> Result<DownloadedPackage, String> {
    take_package().ok_or_else(|| NO_PACKAGE_MESSAGE.to_string())
}

fn take_package() -> Option<DownloadedPackage> {
    package_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

/// 公开更新页地址（按当前更新源配置拼装，与 core 的 `update_check` 同源）。
fn release_url() -> String {
    let config = update::load_github_config();
    let owner = config
        .get("owner")
        .and_then(Value::as_str)
        .filter(|owner| !owner.is_empty())
        .unwrap_or(update::GITHUB_OWNER);
    let repo = config
        .get("repo")
        .and_then(Value::as_str)
        .filter(|repo| !repo.is_empty())
        .unwrap_or(update::GITHUB_REPO);
    format!("https://github.com/{owner}/{repo}/releases/latest")
}

// ---------------------------------------------------------------------------
// 宿主出口：广播 / 托盘刷新 / tooltip / 通知
// ---------------------------------------------------------------------------

/// 统一状态出口：写好的快照经 emit + 托盘刷新对外可见。
fn set_phase<R: Runtime>(app: &AppHandle<R>, snapshot: UpdateSnapshot) -> UpdateSnapshot {
    let _ = app.emit("update-state", &snapshot);
    refresh_tray(app);
    snapshot
}

fn refresh_tray<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(desktop)]
    crate::tray::refresh_tray_menu(app);
    #[cfg(not(desktop))]
    let _ = app;
}

fn set_tooltip<R: Runtime>(app: &AppHandle<R>, text: &str) {
    #[cfg(desktop)]
    crate::tray::set_update_tooltip(app, text);
    #[cfg(not(desktop))]
    let _ = (app, text);
}

/// 更新流程结束（完成 / 失败）后把 tooltip 复位，避免长期停留在进度文案上。
fn reset_tooltip<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(desktop)]
    crate::tray::reset_tray_tooltip(app);
    #[cfg(not(desktop))]
    let _ = app;
}

fn notify<R: Runtime>(app: &AppHandle<R>, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// 快照 / BUSY / PACKAGE 都是进程级单例，用例之间必须串行。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset() {
        *snapshot_cell()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = UpdateSnapshot::idle();
        BUSY.store(false, Ordering::Release);
    }

    #[test]
    fn phases_flow_idle_available_downloading_ready_to_restart() {
        let _guard = lock();
        reset();
        assert_eq!(snapshot().phase, UpdatePhase::Idle);

        assert_eq!(begin_check().phase, UpdatePhase::Checking);

        let available = apply_check_result(&json!({
            "ok": true,
            "latest": "9.9.9",
            "hasUpdate": true,
            "checkedAt": 1_700_000_000,
        }));
        assert_eq!(available.phase, UpdatePhase::Available);
        assert_eq!(available.latest.as_deref(), Some("9.9.9"));
        assert_eq!(available.checked_at, Some(1_700_000_000));

        let downloading = begin_download("9.9.9");
        assert_eq!(downloading.phase, UpdatePhase::Downloading);
        assert_eq!(downloading.latest.as_deref(), Some("9.9.9"));
        assert_eq!(downloading.percent, None);

        assert_eq!(advance_progress(30, Some(100)), Some(30));
        assert_eq!(snapshot().percent, Some(30));

        let ready = mark_ready_to_restart("9.9.9");
        assert_eq!(ready.phase, UpdatePhase::ReadyToRestart);
        assert_eq!(ready.percent, Some(100));
        assert_eq!(ready.latest.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn check_without_update_lands_on_up_to_date() {
        let _guard = lock();
        reset();
        let snapshot =
            apply_check_result(&json!({"ok": true, "latest": "0.1.0", "hasUpdate": false}));
        assert_eq!(snapshot.phase, UpdatePhase::UpToDate);
        assert_eq!(snapshot.latest, None);
        assert!(snapshot.checked_at.is_some());
    }

    #[test]
    fn failed_check_lands_on_error_with_message() {
        let _guard = lock();
        reset();
        let snapshot = apply_check_result(&json!({
            "ok": false,
            "error": "网络请求失败",
            "message": "网络请求失败（code=-1）",
        }));
        assert_eq!(snapshot.phase, UpdatePhase::Error);
        assert_eq!(snapshot.message.as_deref(), Some("网络请求失败（code=-1）"));
        assert_eq!(snapshot.latest, None);
    }

    /// 已有目标版本时，复查失败必须回到 Available，不能清掉「升级到 vX」入口。
    #[test]
    fn failed_recheck_keeps_available_target() {
        let _guard = lock();
        reset();
        apply_check_result(&json!({
            "ok": true,
            "latest": "9.9.9",
            "hasUpdate": true,
        }));
        assert_eq!(begin_check().phase, UpdatePhase::Checking);
        assert_eq!(snapshot().latest.as_deref(), Some("9.9.9"));
        let snapshot = apply_check_result(&json!({
            "ok": false,
            "error": "网络请求失败",
            "message": "网络请求失败（code=-1）",
        }));
        assert_eq!(snapshot.phase, UpdatePhase::Available);
        assert_eq!(snapshot.latest.as_deref(), Some("9.9.9"));
        assert_eq!(snapshot.message, None);
    }

    #[test]
    fn failed_check_from_idle_still_errors() {
        let _guard = lock();
        reset();
        begin_check();
        let snapshot = apply_check_result(&json!({
            "ok": false,
            "error": "网络请求失败",
            "message": "网络请求失败（code=-1）",
        }));
        assert_eq!(snapshot.phase, UpdatePhase::Error);
        assert_eq!(snapshot.latest, None);
        assert_eq!(snapshot.message.as_deref(), Some("网络请求失败（code=-1）"));
    }

    /// 下载中 / 待重启不被后台检查改写：托盘升级入口不能闪掉。
    #[test]
    fn check_does_not_downgrade_downloading_or_ready_to_restart() {
        let _guard = lock();
        reset();
        begin_download("9.9.9");
        assert_eq!(begin_check().phase, UpdatePhase::Downloading);
        assert_eq!(
            apply_check_result(&json!({"ok": true, "latest": "9.9.10", "hasUpdate": true})).phase,
            UpdatePhase::Downloading
        );

        mark_ready_to_restart("9.9.9");
        assert_eq!(begin_check().phase, UpdatePhase::ReadyToRestart);
        assert_eq!(
            apply_check_result(&json!({"ok": true, "latest": "9.9.10", "hasUpdate": true})).phase,
            UpdatePhase::ReadyToRestart
        );
        assert_eq!(snapshot().phase, UpdatePhase::ReadyToRestart);
    }

    #[test]
    fn failed_download_keeps_target_version_for_retry() {
        let _guard = lock();
        reset();
        apply_check_result(&json!({"ok": true, "latest": "9.9.9", "hasUpdate": true}));
        begin_download("9.9.9");
        let snapshot = fail("下载更新包失败：连接超时".to_string());
        assert_eq!(snapshot.phase, UpdatePhase::Error);
        assert_eq!(snapshot.latest.as_deref(), Some("9.9.9"));
        assert_eq!(snapshot.percent, None);
    }

    #[test]
    fn busy_guard_rejects_second_entry_until_released() {
        let _guard = lock();
        reset();
        let first = BusyGuard::acquire();
        assert!(first.is_some(), "首次进入应拿到互斥");
        assert!(BusyGuard::acquire().is_none(), "并发进入必须被拒");
        drop(first);
        let second = BusyGuard::acquire();
        assert!(second.is_some(), "释放后应重新可进入");
        drop(second);
    }

    #[test]
    fn restart_without_package_returns_error_instead_of_panicking() {
        let _guard = lock();
        *package_cell()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let error = match take_package_for_install() {
            Ok(_) => panic!("无包时不应返回可安装的更新包"),
            Err(error) => error,
        };
        assert!(error.contains("没有可安装的更新包"), "{error}");
    }

    #[test]
    fn percent_conversion_clamps_and_handles_unknown_total() {
        assert_eq!(percent_of(0, Some(100)), Some(0));
        assert_eq!(percent_of(1, Some(3)), Some(33));
        assert_eq!(percent_of(99, Some(100)), Some(99));
        assert_eq!(percent_of(100, Some(100)), Some(100));
        assert_eq!(percent_of(150, Some(100)), Some(100), "超出总量按 100 截断");
        assert_eq!(percent_of(10, None), None, "无 Content-Length 时不报百分比");
        assert_eq!(percent_of(10, Some(0)), None, "总量为 0 不除零");
    }

    #[test]
    fn progress_snapshot_written_only_on_integer_change() {
        let _guard = lock();
        reset();
        begin_download("9.9.9");
        assert_eq!(advance_progress(1, Some(1000)), Some(0));
        assert_eq!(
            advance_progress(2, Some(1000)),
            None,
            "同一整数百分比不重复写"
        );
        assert_eq!(advance_progress(10, Some(1000)), Some(1));
        assert_eq!(snapshot().percent, Some(1));
        assert_eq!(advance_progress(10, None), None, "总量未知不写百分比");
    }

    #[test]
    fn menu_rebuilds_once_per_ten_percent_bucket() {
        let _guard = lock();
        MENU_PROGRESS_BUCKET.store(0, Ordering::Release);
        assert!(!should_rebuild_menu(0));
        assert!(!should_rebuild_menu(3));
        assert!(!should_rebuild_menu(9));
        assert!(should_rebuild_menu(10));
        assert!(!should_rebuild_menu(11));
        assert!(should_rebuild_menu(100), "跨档必须重建");
        assert!(!should_rebuild_menu(100), "同档不重复重建");
    }

    /// 事件 / 命令负载的字段契约（前端 `UpdateSnapshot` 按此解析）。
    #[test]
    fn snapshot_serializes_camel_case_contract() {
        let value = serde_json::to_value(UpdateSnapshot::idle()).unwrap();
        assert_eq!(value["phase"], json!("idle"));
        assert_eq!(value["latest"], json!(null));
        assert_eq!(value["percent"], json!(null));
        assert_eq!(value["message"], json!(null));
        assert_eq!(value["checkedAt"], json!(null));

        let value = serde_json::to_value(mark_ready_to_restart("1.2.3")).unwrap();
        assert_eq!(value["phase"], json!("readyToRestart"));
        assert_eq!(value["latest"], json!("1.2.3"));
        assert_eq!(value["percent"], json!(100));
    }

    #[test]
    fn download_tooltip_reports_percent_or_unknown() {
        assert_eq!(download_tooltip(Some(42)), "正在下载更新 42%");
        assert_eq!(download_tooltip(None), "正在下载更新…");
    }

    // ---------------------------------------------------------------------
    // 取消下载（spec [D4]）：幂等、阶段回退、控制块槽位生命周期
    // ---------------------------------------------------------------------

    fn clear_active_control() {
        *active_control_cell()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// 边界④：没有下载在跑时点取消必须返回 Ok（前端按钮可能被连点），且不改阶段。
    #[test]
    fn 取消下载_没有任务时幂等返回_ok() {
        let _guard = lock();
        reset();
        clear_active_control();
        assert!(cancel_download().is_ok());
        assert!(cancel_download().is_ok(), "重复取消也必须 Ok");
        assert_eq!(snapshot().phase, UpdatePhase::Idle, "无任务时取消不改阶段");
    }

    /// 有任务在跑时取消要真的置位；任务结束（guard drop）必须清空全局槽位。
    #[test]
    fn 取消下载_置位当前控制块且结束后清空槽位() {
        let _guard = lock();
        reset();
        let control = Arc::new(update_download::DownloadControl::new());
        let active = ActiveControlGuard::install(Arc::clone(&control));
        assert!(cancel_download().is_ok());
        assert!(control.is_cancelled(), "取消必须落到当前控制块上");
        drop(active);
        let slot = active_control_cell()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(slot.is_none(), "任务结束必须清空全局槽位，否则下次取消打到僵尸块");
    }

    /// 边界⑤：取消后回到「可升级」并保留 latest，percent 必须清空，提示文案是「已取消下载」。
    #[test]
    fn 取消后阶段回到可升级且百分比清空() {
        let _guard = lock();
        reset();
        apply_check_result(&json!({"ok": true, "latest": "9.9.9", "hasUpdate": true}));
        begin_download("9.9.9");
        assert_eq!(advance_progress(85, Some(100)), Some(85));
        let snapshot = mark_cancelled();
        assert_eq!(snapshot.phase, UpdatePhase::Available);
        assert_eq!(snapshot.latest.as_deref(), Some("9.9.9"));
        assert_eq!(snapshot.percent, None);
        assert_eq!(snapshot.message.as_deref(), Some("已取消下载"));
    }

    /// 换候选：停滞标志（单通道判定）必须清掉，取消意图必须带进下一条通道。
    #[test]
    fn 换候选清空停滞标志但保留取消意图() {
        let _guard = lock();
        reset();
        clear_active_control();
        let first = Arc::new(update_download::DownloadControl::new());
        let _active = ActiveControlGuard::install(Arc::clone(&first));
        first.stall("窗口吞吐 16 KB/s 低于地板");
        assert_eq!(first.check().expect_err("停滞必须拦住"), update_download::STALLED_ERROR);

        let second = install_candidate_control(&first);
        assert!(!second.is_stalled(), "停滞是单通道判定，不能带进下一条通道");
        assert!(second.check().is_ok(), "新通道必须能真的开工");

        second.cancel();
        let third = install_candidate_control(&second);
        assert!(third.is_cancelled(), "取消是整条任务的意图，必须跨候选保留");
        drop(_active);
    }
}
