//! 在文件管理器中定位目录（能力探测 + 平台命令）。
//!
//! 能力边界是显式的：`serve` / webui 场景由 `wb-switch-server` 在启动时设置
//! [`HEADLESS_ENV`]，此时 [`reveal_supported`] 返回 `false`，HTTP 路由回 `501`
//! 并在前端把按钮置灰——**不允许静默成功**（点了没反应比报错更糟）。

use std::path::{Path, PathBuf};

/// 启动方声明「无桌面宿主」的环境变量（由 `wb-switch serve` 设置）。
pub const HEADLESS_ENV: &str = "WB_SWITCH_HEADLESS";

/// 目标平台（显式取值便于单测覆盖三平台，不依赖编译期 `cfg`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealHost {
    Windows,
    Macos,
    Linux,
}

impl RevealHost {
    /// 当前平台。
    pub fn current() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::Linux
        }
    }
}

/// 打开动作：程序 + 参数（纯数据，便于断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevealCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// 本进程是否具备「打开本机文件管理器」的能力。
pub fn reveal_supported() -> bool {
    if std::env::var_os(HEADLESS_ENV).is_some() {
        return false;
    }
    cfg!(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux"
    ))
}

/// 构造打开命令（纯函数）。
///
/// - Windows：文件用 `explorer /select,<path>` 定位，目录直接打开。
/// - macOS：文件用 `open -R`（在 Finder 中显示），目录用 `open`。
/// - Linux：`xdg-open`，目录打开自身、文件打开父目录（没有通用的「定位文件」）。
pub fn reveal_command(path: &Path, host: RevealHost) -> RevealCommand {
    let is_dir = path.is_dir();
    match host {
        RevealHost::Windows => {
            let argument = if is_dir {
                path.to_string_lossy().to_string()
            } else {
                format!("/select,{}", path.to_string_lossy())
            };
            RevealCommand {
                program: "explorer".to_string(),
                args: vec![argument],
            }
        }
        RevealHost::Macos => RevealCommand {
            program: "open".to_string(),
            args: if is_dir {
                vec![path.to_string_lossy().to_string()]
            } else {
                vec!["-R".to_string(), path.to_string_lossy().to_string()]
            },
        },
        RevealHost::Linux => {
            let target: PathBuf = if is_dir {
                path.to_path_buf()
            } else {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.to_path_buf())
            };
            RevealCommand {
                program: "xdg-open".to_string(),
                args: vec![target.to_string_lossy().to_string()],
            }
        }
    }
}

/// 在文件管理器中定位；不支持的环境返回可读 `Err`，不静默成功。
pub fn reveal(path: &Path) -> Result<(), String> {
    if !reveal_supported() {
        return Err("在文件管理器中定位仅在桌面端支持（当前为浏览器 / 服务端模式）".to_string());
    }
    if path.as_os_str().is_empty() {
        return Err("路径为空，无法定位".to_string());
    }
    if !path.exists() {
        return Err(format!("路径不存在：{}", path.display()));
    }
    let command = reveal_command(path, RevealHost::current());
    std::process::Command::new(&command.program)
        .args(&command.args)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("调用 {} 失败：{error}", command.program))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-storage-reveal-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn windows_selects_files_and_opens_directories() {
        let dir = temp_dir("win");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"a").expect("写文件");

        let on_file = reveal_command(&file, RevealHost::Windows);
        assert_eq!(on_file.program, "explorer");
        assert!(on_file.args[0].starts_with("/select,"));
        assert!(on_file.args[0].ends_with("a.txt"));

        let on_dir = reveal_command(&dir, RevealHost::Windows);
        assert_eq!(on_dir.args, vec![dir.to_string_lossy().to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn macos_uses_open_dash_r_for_files() {
        let dir = temp_dir("mac");
        let file = dir.join("b.txt");
        std::fs::write(&file, b"b").expect("写文件");
        let command = reveal_command(&file, RevealHost::Macos);
        assert_eq!(command.program, "open");
        assert_eq!(command.args[0], "-R");
        let on_dir = reveal_command(&dir, RevealHost::Macos);
        assert_eq!(on_dir.args.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn linux_opens_parent_for_files() {
        let dir = temp_dir("linux");
        let file = dir.join("c.txt");
        std::fs::write(&file, b"c").expect("写文件");
        let command = reveal_command(&file, RevealHost::Linux);
        assert_eq!(command.program, "xdg-open");
        assert_eq!(command.args, vec![dir.to_string_lossy().to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_path_is_reported_instead_of_silently_ignored() {
        let dir = temp_dir("missing");
        let absent = dir.join("nope");
        // 无论宿主是否支持 reveal，目标不存在都必须报错（不能静默成功）。
        let result = reveal(&absent);
        assert!(result.is_err(), "不存在的路径竟然返回成功");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_path_is_rejected() {
        assert!(reveal(Path::new("")).is_err());
    }
}
