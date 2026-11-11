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

use serde_json::json;

use wb_switch_core::modules::{
    account, archive, auth_file, checkin, config, process, rotate, travel, update,
    variant::WbVariant,
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

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "status" => print_status(variant_arg(&args)),
        "archive" => {
            if let Err(error) = archive_command(&args) {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        "version" | "--version" | "-V" => {
            println!("workbuddy-switch {}", env!("CARGO_PKG_VERSION"));
        }
        _ => serve(&args).await,
    }
}

async fn serve(args: &[String]) {
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
    use super::{flag_value, groups_arg, variant_arg};
    use wb_switch_core::modules::variant::WbVariant;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
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
