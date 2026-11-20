//! workbuddy-switch CLI：npm 安装形态的入口。
//!
//! ```bash
//! workbuddy-switch              # 启动本地服务 + 打开浏览器 webui
//! workbuddy-switch serve        # 只起服务不开浏览器（--port / --no-open）
//! workbuddy-switch status       # 终端输出当前账号
//! workbuddy-switch archive list                          # 各分组的数据量
//! workbuddy-switch archive export --groups accounts,logs --out ~/bk.zip
//! workbuddy-switch archive import --in ~/bk.zip
//! workbuddy-switch version      # 版本号
//! ```

mod api;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use wb_switch_core::modules::{
    account, archive, auth_file, checkin, config, process, rotate, storage, token_stats, travel,
    update, variant::WbVariant,
};

fn default_port() -> u16 {
    57890
}

/// 后台任务：自动签到启动即核验，之后按 core 计算的下一轮延迟睡眠（未设置
/// 签到时间段时固定 30 分钟）；自动轮换按配置间隔执行；
/// 限额 hook 信号每秒轮询一次、启动时后台默认接入。
fn spawn_background_loops() {
    tokio::spawn(async move {
        if let Err(error) = config::compact_checkin_logs() {
            eprintln!("[签到] 历史日志整理失败: {error}");
        }
        let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::StartupVerify).await;
        loop {
            tokio::time::sleep(checkin::next_cycle_delay()).await;
            let _ = checkin::run_checkin_cycle(checkin::CheckinCycleMode::PeriodicRecovery).await;
        }
    });

    tokio::spawn(async move {
        let mut last_cycle_at: i64 = 0;
        loop {
            let cfg = config::load_auto_rotate_config();
            if cfg.get("enabled").and_then(|v| v.as_bool()) == Some(true) {
                let interval_minutes = cfg
                    .get("check_interval_minutes")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(5)
                    .max(1);
                let now = config::now_ms();
                if now - last_cycle_at >= interval_minutes * 60_000 {
                    last_cycle_at = now;
                    let _ = rotate::run_rotate_cycle().await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // 国际版每日活跃：启动即查一轮，之后按固定间隔轮询（靠状态文件做当天去重，
    // 时间段跟随签到配置）。不切换账号，直接用账号库里的 token 直连网页通道。
    tokio::spawn(async move {
        let _ = wb_switch_core::modules::web_activity::run_daily_activity_cycle().await;
        loop {
            tokio::time::sleep(wb_switch_core::modules::web_activity::cycle_interval()).await;
            let _ = wb_switch_core::modules::web_activity::run_daily_activity_cycle().await;
        }
    });

    // 派猫猫旅行：启动即派发，之后周期性补派（并重试 no-buddy / 瞬时错误）。
    tokio::spawn(async move {
        let _ = travel::run_travel_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_RETRY_INTERVAL).await;
            let _ = travel::run_travel_cycle().await;
        }
    });

    // 旅行领取：启动立刻查一轮（避免重启后空等 15 分钟漏领），之后按周期检查。
    tokio::spawn(async move {
        let _ = travel::run_travel_claim_cycle().await;
        loop {
            tokio::time::sleep(travel::TRAVEL_CLAIM_INTERVAL).await;
            let _ = travel::run_travel_claim_cycle().await;
        }
    });

    // 限额 hook 信号：轮询 `~/.wb-switch/hook-events.jsonl`，入账后由前端下次拉取可见。
    // webui 没有 Tauri 事件通道，因此不需要推送回调（桌面端见 src-tauri/src/lib.rs）。
    wb_switch_core::modules::rate_limit_events::spawn_watcher(|| {});

    // 默认接入：后台线程自动安装 hook（幂等、非阻塞、失败静默）；
    // 装上了就作废扫描缓存——扫描范围从全量收窄到「未注册的来源」。
    std::thread::spawn(|| {
        if wb_switch_core::modules::rate_limit_hook::auto_install_on_startup() {
            wb_switch_core::modules::limits::invalidate_scan_cache();
        }
    });
}

/// CLI 档位参数：`--variant ai` / `--variant=ai`；缺省国内版。
///
/// 与 Tauri 命令的可选 `variant` 参数、HTTP 路由的 query/body 字段同义。
fn variant_arg(args: &[String]) -> WbVariant {
    let raw = args.iter().enumerate().find_map(|(index, arg)| {
        if let Some(value) = arg.strip_prefix("--variant=") {
            return Some(value.to_string());
        }
        arg.eq("--variant")
            .then(|| args.get(index + 1).cloned().unwrap_or_default())
    });
    WbVariant::parse(raw.as_deref())
}

fn print_status(variant: WbVariant) {
    let auth = auth_file::read_auth_file(variant);
    let current = auth.as_ref().map(|a| {
        let acct = a.get("account").cloned().unwrap_or_else(|| json!({}));
        json!({
            "uid": account::display_value(&acct, "uid"),
            "nickname": account::display_value(&acct, "nickname"),
            "email": account::display_value(&acct, "email"),
        })
    });
    let running = process::is_workbuddy_running(variant);
    println!("workbuddy-switch v{}", update::APP_VERSION);
    println!("WorkBuddy 运行中: {}", if running { "是" } else { "否" });
    match current {
        Some(c) => {
            let name = c
                .get("nickname")
                .and_then(|v| v.as_str())
                .or_else(|| c.get("email").and_then(|v| v.as_str()))
                .unwrap_or("未知");
            println!("当前账号: {name}");
        }
        None => println!("当前账号: 未登录"),
    }
    println!("账号数: {}", account::load_accounts().len());
}

// ---------------------------------------------------------------------------
// 新增子命令：accounts / switch --dry-run / token-stats / operations / help
//
// C1 零业务逻辑：下面每个函数**只**调用 core 的既有只读函数并打印结果，
// 不判断「能不能切」、不算额度、不比较档位规则。
// ---------------------------------------------------------------------------

/// 一项 CLI 能力。`host` 明确区分「CLI 能做」与「只有桌面宿主能做」（C3），
/// 避免把关窗重启、开机自启、原生权限窗、悬浮窗伪装成 CLI 能力。
struct Operation {
    name: &'static str,
    summary: &'static str,
    usage: &'static str,
    /// `"cli"` | `"desktop"`。
    host: &'static str,
}

const OPERATIONS: &[Operation] = &[
    Operation {
        name: "status",
        summary: "查看当前登录账号与客户端运行状态",
        usage: "workbuddy-switch status [--variant ai]",
        host: "cli",
    },
    Operation {
        name: "accounts",
        summary: "列出全部账号（--json 输出机器可读原样 JSON）",
        usage: "workbuddy-switch accounts [--json]",
        host: "cli",
    },
    Operation {
        name: "switch",
        summary: "只读预演切换；不写任何文件，也不验证凭据",
        usage: "workbuddy-switch switch <id|uid> --dry-run",
        host: "cli",
    },
    Operation {
        name: "token-stats",
        summary: "Token 用量统计（按数据源分开，不混算）",
        usage: "workbuddy-switch token-stats [--days 7|30|90] [--json]",
        host: "cli",
    },
    Operation {
        name: "operations",
        summary: "列出本 CLI 支持的操作与桌面专属边界",
        usage: "workbuddy-switch operations [--json]",
        host: "cli",
    },
    Operation {
        name: "archive",
        summary: "整包备份 / 迁移",
        usage: "workbuddy-switch archive list|export --out <zip>|import --in <zip>",
        host: "cli",
    },
    Operation {
        name: "serve",
        summary: "启动本地 WebUI 服务",
        usage: "workbuddy-switch serve [--port N] [--no-open]",
        host: "cli",
    },
    Operation {
        name: "version",
        summary: "打印版本号",
        usage: "workbuddy-switch version",
        host: "cli",
    },
    // 以下只在桌面宿主内成立，不提供 CLI 等价物。
    Operation {
        name: "restart-client",
        summary: "重启 WorkBuddy 客户端（需要 GUI 会话）",
        usage: "（仅桌面端设置页）",
        host: "desktop",
    },
    Operation {
        name: "launch-at-login",
        summary: "注册开机自启（需要系统登录项权限）",
        usage: "（仅桌面端设置页）",
        host: "desktop",
    },
    Operation {
        name: "reveal-in-file-manager",
        summary: "在文件管理器中定位（需要桌面宿主）",
        usage: "（仅桌面端存储页）",
        host: "desktop",
    },
    Operation {
        name: "companion-window",
        summary: "会话悬浮窗开关（需要桌面 WebView）",
        usage: "（仅桌面端设置页）",
        host: "desktop",
    },
];

/// 展示型字段转字符串：对象/数组（加密信封）按 `-` 处理，避免把 JSON 塞进表格。
fn text_of(record: &Value, key: &str) -> String {
    match account::display_value(record, key) {
        Value::String(text) => text,
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => "-".to_string(),
    }
}

fn json_flag(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--json")
}

/// `accounts --json` 的负载（纯函数，便于断言机器可读性）。
fn accounts_json_payload(accounts: &[Value]) -> Result<String, String> {
    serde_json::to_string_pretty(accounts).map_err(|error| format!("序列化账号列表失败：{error}"))
}

fn accounts_command(args: &[String]) -> Result<(), String> {
    let accounts = account::load_accounts();
    if json_flag(args) {
        println!("{}", accounts_json_payload(&accounts)?);
        return Ok(());
    }
    if accounts.is_empty() {
        // B11：没有账号不是错误，exit 0。
        println!("暂无账号");
        return Ok(());
    }
    println!(
        "{:<38} {:<16} {:<30} {}",
        "ID", "昵称", "邮箱", "档位"
    );
    for record in &accounts {
        println!(
            "{:<38} {:<16} {:<30} {}",
            text_of(record, "id"),
            text_of(record, "nickname"),
            text_of(record, "email"),
            WbVariant::from_account(record).as_str()
        );
    }
    Ok(())
}

/// 账号 id / uid 双键匹配（与 core 的 `account::find_account` 同口径）。
fn matches_account(record: &Value, target: &str) -> bool {
    let target = target.trim();
    !target.is_empty()
        && (record.get("id").and_then(Value::as_str) == Some(target)
            || record.get("uid").and_then(Value::as_str) == Some(target))
}

/// `switch --dry-run` 的纯逻辑部分：只读账号元数据，产出人话报告。
///
/// 参数全部由调用方注入，因此本函数**没有**任何写盘能力（R4 的结构性保证）。
fn switch_dry_run_report(
    target: &str,
    accounts: &[Value],
    running: bool,
    current_uid: Option<&str>,
) -> Result<String, String> {
    let Some(record) = accounts.iter().find(|record| matches_account(record, target)) else {
        return Err(format!("未找到该账号：{target}"));
    };
    let variant = WbVariant::from_account(record);
    let nickname = text_of(record, "nickname");
    let uid = text_of(record, "uid");
    let mut lines = vec![
        format!("目标账号: {nickname}（{uid}）"),
        format!("目标档位: {}", variant.as_str()),
    ];
    if running {
        // B12：只报告，不关闭、不重启任何进程。
        lines.push(
            "客户端运行中，真实切换需要先退出 WorkBuddy（本命令不会关闭或重启任何进程）"
                .to_string(),
        );
    }
    match current_uid {
        Some(current) if current == uid => {
            lines.push("该账号已经是当前登录账号，无需切换".to_string());
        }
        Some(current) => lines.push(format!("当前登录账号: {current}")),
        None => lines.push("当前档位未登录任何账号".to_string()),
    }
    lines.push("--dry-run：未写入任何文件，也未验证凭据".to_string());
    Ok(lines.join("\n"))
}

fn switch_command(args: &[String]) -> Result<(), String> {
    if !args.iter().any(|arg| arg == "--dry-run") {
        return Err(
            "switch 目前只支持 --dry-run（真实切换请用 WebUI 或桌面端，CLI 不做写入）"
                .to_string(),
        );
    }
    let Some(target) = args
        .get(2)
        .filter(|arg| !arg.starts_with("--"))
        .cloned()
    else {
        return Err(
            "缺少账号 id 或 uid\n用法：workbuddy-switch switch <id|uid> --dry-run".to_string(),
        );
    };
    let accounts = account::load_accounts();
    // 档位判定只用 core：先解析出目标账号，再按同一档位查运行态与登录态。
    let variant = accounts
        .iter()
        .find(|record| matches_account(record, &target))
        .map(WbVariant::from_account)
        .unwrap_or(WbVariant::Cn);
    let running = process::is_workbuddy_running(variant);
    let current_uid = auth_file::read_auth_file(variant)
        .and_then(|auth| auth.get("account").and_then(|a| a.get("uid")).cloned())
        .and_then(|uid| uid.as_str().map(String::from));
    let report = switch_dry_run_report(&target, &accounts, running, current_uid.as_deref())?;
    println!("{report}");
    Ok(())
}

fn token_stats_command(args: &[String]) -> Result<(), String> {
    let days = flag_value(args, "--days").and_then(|raw| raw.parse::<i64>().ok());
    let statistics = token_stats::get_statistics(days);
    if json_flag(args) {
        let payload = serde_json::to_string_pretty(&statistics)
            .map_err(|error| format!("序列化 Token 统计失败：{error}"))?;
        println!("{payload}");
        return Ok(());
    }
    match days {
        Some(days) => println!("Token 统计（最近 {days} 天，按数据源分开）"),
        None => println!("Token 统计（全部时间，按数据源分开）"),
    }
    let sources = statistics
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if sources.is_empty() {
        println!("暂无数据源");
        return Ok(());
    }
    for source in &sources {
        let id = source.get("source").and_then(Value::as_str).unwrap_or("-");
        let summary = source.get("summary").cloned().unwrap_or_else(|| json!({}));
        let total = summary.get("total").and_then(Value::as_i64).unwrap_or(0);
        let requests = summary.get("records").and_then(Value::as_i64).unwrap_or(0);
        println!("{id:<22} token {total:>12}    记录 {requests:>6}");
    }
    Ok(())
}

/// `operations --json` 的负载（纯函数；`host` 字段是 C3 的显式边界）。
fn operations_json_payload() -> Result<String, String> {
    let items: Vec<Value> = OPERATIONS
        .iter()
        .map(|operation| {
            json!({
                "name": operation.name,
                "summary": operation.summary,
                "usage": operation.usage,
                "host": operation.host,
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({ "operations": items }))
        .map_err(|error| format!("序列化操作列表失败：{error}"))
}

fn operations_command(args: &[String]) -> Result<(), String> {
    if json_flag(args) {
        println!("{}", operations_json_payload()?);
        return Ok(());
    }
    println!("{:<26} {:<8} {}", "操作", "宿主", "说明");
    for operation in OPERATIONS {
        println!(
            "{:<26} {:<8} {}",
            operation.name, operation.host, operation.summary
        );
    }
    println!("\n桌面专属操作（host=desktop）没有 CLI 等价物，请到桌面端设置页操作。");
    Ok(())
}

fn print_help() {
    println!("workbuddy-switch v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("用法：workbuddy-switch [子命令] [选项]");
    println!();
    println!("子命令：");
    for operation in OPERATIONS.iter().filter(|item| item.host == "cli") {
        println!("  {:<14} {}", operation.name, operation.summary);
        println!("  {:<14} {}", "", operation.usage);
    }
    println!();
    println!("不传子命令等价于 `serve`（启动本地服务并打开浏览器）。");
    println!("`switch --dry-run` 只读账号元数据，不验证凭据、不写任何文件。");
}

// ---------------------------------------------------------------------------
// archive 子命令：整包备份 / 迁移（与 webui、桌面端同一套 core 实现）
// ---------------------------------------------------------------------------

/// `--key value` / `--key=value` 取值；空值当作没传。
fn flag_value(args: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    args.iter()
        .enumerate()
        .find_map(|(index, arg)| {
            if let Some(value) = arg.strip_prefix(&prefix) {
                return Some(value.to_string());
            }
            arg.eq(key)
                .then(|| args.get(index + 1).cloned().unwrap_or_default())
        })
        .filter(|value| !value.is_empty())
}

/// `--groups a,b --groups c` 合并成列表；一个都不传 = 全部（core 的语义）。
fn groups_arg(args: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let raw = if let Some(value) = arg.strip_prefix("--groups=") {
            value.to_string()
        } else if arg == "--groups" {
            args.get(index + 1).cloned().unwrap_or_default()
        } else {
            continue;
        };
        ids.extend(
            raw.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(String::from),
        );
    }
    ids
}

fn archive_list() {
    let groups = archive::groups();
    if groups.is_empty() {
        println!("没有可备份的数据分组。");
        return;
    }
    for group in groups {
        let flag = if group.sensitive {
            " [含明文凭据]"
        } else {
            ""
        };
        println!(
            "{:<9} {:<10} {:>3} 个文件 {:>10} 字节{}",
            group.id, group.title, group.files, group.bytes, flag
        );
    }
}

fn archive_command(args: &[String]) -> Result<(), String> {
    match args.get(2).map(String::as_str).unwrap_or("") {
        "list" => {
            archive_list();
            Ok(())
        }
        "export" => {
            let selected = groups_arg(args);
            let dest = match flag_value(args, "--out") {
                Some(path) => PathBuf::from(path),
                // 缺省落在主目录，文件名带日期，和 webui 下载的文件名一致。
                None => config::home_dir().join(format!(
                    "wb-switch-backup-{}.zip",
                    chrono::Local::now().format("%Y%m%d")
                )),
            };
            let outcome = archive::export(&selected, &dest)?;
            println!(
                "已导出 {} 个分组 → {}（{} 字节）",
                outcome.groups.len(),
                outcome.path,
                outcome.bytes
            );
            Ok(())
        }
        "import" => {
            let path = flag_value(args, "--in").ok_or_else(|| {
                "缺少 --in 参数：workbuddy-switch archive import --in <归档路径>".to_string()
            })?;
            let outcome = archive::import(Path::new(&path), &groups_arg(args))?;
            println!("已恢复分组：{}", outcome.groups.join(", "));
            println!(
                "账号：新增 {} 个，更新 {} 个",
                outcome.accounts_added, outcome.accounts_updated
            );
            println!("导入前的文件已备份到 {}", outcome.backup_dir);
            Ok(())
        }
        other => Err(format!(
            "未知的 archive 子命令：{other}\n用法：archive list | export [--groups a,b] [--out 路径] | import --in 路径 [--groups a,b]"
        )),
    }
}

/// 子命令分派结果（纯函数产出，便于把「未知子命令不得静默回落 serve」钉成单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CliCommand {
    Status,
    Accounts,
    Switch,
    TokenStats,
    Operations,
    Archive,
    Help,
    Version,
    Serve,
    /// C5：未知子命令必须报错并指向 `help`。
    Unknown(String),
}

/// 分派规则：
/// - 无参 = `serve`（LP1 既有行为，不得被新子命令抢走）；
/// - 以 `-` 开头的第一个参数（如 `--port 1234`）= `serve`（既有用法）；
/// - 其它未登记的名字 = `Unknown`，不再兜底成 `serve`。
fn classify_command(raw: Option<&str>) -> CliCommand {
    match raw.unwrap_or("serve") {
        "status" => CliCommand::Status,
        "accounts" => CliCommand::Accounts,
        "switch" => CliCommand::Switch,
        "token-stats" => CliCommand::TokenStats,
        "operations" => CliCommand::Operations,
        "archive" => CliCommand::Archive,
        "help" | "--help" | "-h" => CliCommand::Help,
        "version" | "--version" | "-V" => CliCommand::Version,
        "serve" => CliCommand::Serve,
        option if option.starts_with('-') => CliCommand::Serve,
        other => CliCommand::Unknown(other.to_string()),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match classify_command(args.get(1).map(String::as_str)) {
        CliCommand::Status => print_status(variant_arg(&args)),
        CliCommand::Accounts => run_or_exit(accounts_command(&args)),
        CliCommand::Switch => run_or_exit(switch_command(&args)),
        CliCommand::TokenStats => run_or_exit(token_stats_command(&args)),
        CliCommand::Operations => run_or_exit(operations_command(&args)),
        CliCommand::Archive => run_or_exit(archive_command(&args)),
        CliCommand::Help => print_help(),
        CliCommand::Version => {
            println!("workbuddy-switch {}", env!("CARGO_PKG_VERSION"));
        }
        CliCommand::Serve => serve(&args).await,
        CliCommand::Unknown(other) => {
            eprintln!("未知子命令：{other}\n用法：workbuddy-switch help（列出全部子命令）");
            std::process::exit(1);
        }
    }
}

/// 子命令失败统一 `eprintln!` + `exit(1)`（C4 输出契约）。
fn run_or_exit(result: Result<(), String>) {
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn serve(args: &[String]) {
    // 服务端没有桌面宿主：`storage::reveal` 会据此回 501，而不是静默失败。
    std::env::set_var(storage::reveal::HEADLESS_ENV, "1");
    let mut port = default_port();
    if let Some(i) = args.iter().position(|a| a == "--port") {
        if let Some(p) = args.get(i + 1).and_then(|p| p.parse::<u16>().ok()) {
            port = p;
        }
    }

    let app = api::router();
    let addr = format!("127.0.0.1:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("启动失败: 端口 {port} 被占用或不可用（{e}）。可用 --port 指定其他端口。");
            std::process::exit(1);
        }
    };

    println!("workbuddy-switch v{}", update::APP_VERSION);
    println!("webui: http://{addr}");
    println!("按 Ctrl+C 停止服务。");

    let no_open = args.iter().any(|a| a == "--no-open");
    if !no_open {
        open_browser(&addr);
    }

    spawn_background_loops();

    axum::serve(listener, app).await.unwrap();
}

fn open_browser(addr: &str) {
    let url = format!("http://{addr}");
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let mut c = std::process::Command::new("cmd");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：开浏览器不闪 cmd 窗
        }
        let _ = c.args(["/C", "start", &url]).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        accounts_json_payload, classify_command, flag_value, groups_arg, operations_json_payload,
        switch_dry_run_report, variant_arg, CliCommand, OPERATIONS,
    };
    use serde_json::{json, Value};
    use wb_switch_core::modules::variant::WbVariant;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn fixture_accounts() -> Vec<Value> {
        vec![
            json!({ "id": "acc-cn", "uid": "uid-cn", "nickname": "国内 A", "variant": "cn" }),
            json!({ "id": "acc-ai", "uid": "uid-ai", "nickname": "国际 B", "variant": "ai" }),
        ]
    }

    /// `accounts --json` 必须是可被 `serde_json` 直接解析的数组（可管道消费）。
    #[test]
    fn accounts_json_is_machine_readable() {
        let payload = accounts_json_payload(&fixture_accounts()).expect("序列化");
        let parsed: Value = serde_json::from_str(&payload).expect("必须能解析");
        let array = parsed.as_array().expect("必须是数组");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0].get("id"), Some(&json!("acc-cn")));
        // 空账号库也必须是合法空数组，而不是空字符串。
        let empty: Value = serde_json::from_str(&accounts_json_payload(&[]).expect("序列化"))
            .expect("必须能解析");
        assert_eq!(empty, json!([]));
    }

    /// `switch --dry-run` 只读账号元数据，报告里必须写清「未写入、未验证凭据」。
    #[test]
    fn switch_dry_run_never_writes_and_says_so() {
        let report = switch_dry_run_report("acc-cn", &fixture_accounts(), false, None)
            .expect("应命中账号");
        assert!(report.contains("未写入任何文件"), "{report}");
        assert!(report.contains("未验证凭据"), "{report}");
        assert!(report.contains("国内 A"), "{report}");
        assert!(report.contains("cn"), "{report}");

        // 找不到账号时必须报错（调用方据此 exit 1）。
        let error = switch_dry_run_report("nope", &fixture_accounts(), false, None)
            .expect_err("必须报错");
        assert!(error.contains("未找到该账号"), "{error}");
        // 空账号库不得 panic。
        assert!(switch_dry_run_report("nope", &[], false, None).is_err());
    }

    /// B12：客户端运行中只报告，不声称会关闭/重启进程。
    #[test]
    fn switch_dry_run_reports_running_client_without_closing_it() {
        let report = switch_dry_run_report("acc-ai", &fixture_accounts(), true, Some("uid-cn"))
            .expect("应命中账号");
        assert!(report.contains("本命令不会关闭或重启任何进程"), "{report}");
        assert!(report.contains("当前登录账号: uid-cn"), "{report}");
        // 已登录同一账号时给出「无需切换」而不是重复报告。
        let same = switch_dry_run_report("acc-cn", &fixture_accounts(), false, Some("uid-cn"))
            .expect("应命中账号");
        assert!(same.contains("无需切换"), "{same}");
    }

    /// `--json` 输出必须是可解析对象，且桌面专属项 `host == "desktop"`。
    #[test]
    fn operations_json_marks_desktop_host_only() {
        let payload = operations_json_payload().expect("序列化");
        let parsed: Value = serde_json::from_str(&payload).expect("必须能解析");
        let items = parsed
            .get("operations")
            .and_then(Value::as_array)
            .expect("operations 数组");
        assert!(!items.is_empty());
        for item in items {
            let host = item.get("host").and_then(Value::as_str).unwrap_or("");
            assert!(host == "cli" || host == "desktop", "非法 host：{host}");
        }
        let desktop: Vec<&str> = items
            .iter()
            .filter(|item| item.get("host").and_then(Value::as_str) == Some("desktop"))
            .filter_map(|item| item.get("name").and_then(Value::as_str))
            .collect();
        assert!(desktop.contains(&"restart-client"), "{desktop:?}");
        assert!(desktop.contains(&"reveal-in-file-manager"), "{desktop:?}");
        // 桌面专属项不得出现在「CLI 可发现」的子命令里。
        let cli_names: Vec<&str> = items
            .iter()
            .filter(|item| item.get("host").and_then(Value::as_str) == Some("cli"))
            .filter_map(|item| item.get("name").and_then(Value::as_str))
            .collect();
        assert!(!cli_names.contains(&"restart-client"));
        assert_eq!(OPERATIONS.len(), items.len());
    }

    /// LP1 + C5：无参与 `-` 开头的选项仍走 serve，未登记名字必须报未知。
    #[test]
    fn cli_classifies_subcommands_and_rejects_unknown() {
        assert_eq!(classify_command(None), CliCommand::Serve);
        assert_eq!(classify_command(Some("serve")), CliCommand::Serve);
        assert_eq!(classify_command(Some("--port")), CliCommand::Serve);
        assert_eq!(classify_command(Some("--no-open")), CliCommand::Serve);
        assert_eq!(classify_command(Some("accounts")), CliCommand::Accounts);
        assert_eq!(classify_command(Some("switch")), CliCommand::Switch);
        assert_eq!(classify_command(Some("token-stats")), CliCommand::TokenStats);
        assert_eq!(classify_command(Some("operations")), CliCommand::Operations);
        assert_eq!(classify_command(Some("archive")), CliCommand::Archive);
        assert_eq!(classify_command(Some("help")), CliCommand::Help);
        assert_eq!(classify_command(Some("-h")), CliCommand::Help);
        assert_eq!(classify_command(Some("version")), CliCommand::Version);
        assert_eq!(classify_command(Some("status")), CliCommand::Status);
        assert_eq!(
            classify_command(Some("accountz")),
            CliCommand::Unknown("accountz".to_string())
        );
    }

    /// `--groups` 支持逗号分隔、多次出现、空值忽略；不传 = 全部（空列表）。
    #[test]
    fn archive_groups_flag_merges_and_trims() {
        assert!(groups_arg(&args(&["archive", "export"])).is_empty());
        assert_eq!(
            groups_arg(&args(&["archive", "export", "--groups", "accounts, logs"])),
            vec!["accounts".to_string(), "logs".to_string()]
        );
        assert_eq!(
            groups_arg(&args(&[
                "archive", "export", "--groups=accounts", "--groups", "memory,"
            ])),
            vec!["accounts".to_string(), "memory".to_string()]
        );
        assert!(groups_arg(&args(&["archive", "export", "--groups="])).is_empty());
    }

    /// `--out` / `--in` 的两种写法都要认，空值当作没传（走缺省）。
    #[test]
    fn archive_flag_value_accepts_space_and_equals_forms() {
        assert_eq!(
            flag_value(&args(&["archive", "export", "--out", "a.zip"]), "--out"),
            Some("a.zip".to_string())
        );
        assert_eq!(
            flag_value(&args(&["archive", "export", "--out=b.zip"]), "--out"),
            Some("b.zip".to_string())
        );
        assert_eq!(flag_value(&args(&["archive", "export"]), "--out"), None);
        assert_eq!(
            flag_value(&args(&["archive", "export", "--out="]), "--out"),
            None
        );
        assert_eq!(
            flag_value(&args(&["archive", "export", "--out"]), "--out"),
            None,
            "缺少值的尾巴不能被当成路径"
        );
    }

    /// 不传档位时必须仍是国内版（改造前行为）。
    #[test]
    fn cli_variant_defaults_to_cn() {
        assert_eq!(variant_arg(&args(&["status"])), WbVariant::Cn);
        assert_eq!(variant_arg(&args(&["status", "--debug"])), WbVariant::Cn);
        assert_eq!(variant_arg(&args(&["status", "--variant"])), WbVariant::Cn);
        assert_eq!(variant_arg(&args(&["status", "cn"])), WbVariant::Cn);
        assert_eq!(
            variant_arg(&args(&["status", "--variant=cn"])),
            WbVariant::Cn
        );
    }

    #[test]
    fn cli_variant_reads_space_and_equals_forms() {
        assert_eq!(
            variant_arg(&args(&["status", "--variant", "ai"])),
            WbVariant::Ai
        );
        assert_eq!(
            variant_arg(&args(&["status", "--variant=ai"])),
            WbVariant::Ai
        );
        assert_eq!(
            variant_arg(&args(&["status", "--variant", "AI", "--no-open"])),
            WbVariant::Ai
        );
        assert_eq!(
            variant_arg(&args(&["--variant=ai", "status"])),
            WbVariant::Ai
        );
    }
}
