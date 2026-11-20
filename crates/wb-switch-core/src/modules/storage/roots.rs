//! 存储根解析：四分类 × 两档位的根清单。
//!
//! 关键判据：**不存在也要返回**（`exists = false`），把「空目录」与「路径不存在」
//! 交给上层用 `exists` + `partial` 区分，不允许过滤掉条目——过滤掉等于把零值
//! 静默当成「没有这个分类」，前端就只能显示 `--`。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::modules::session;
use crate::modules::variant::WbVariant;
use crate::modules::config;

/// 存储分类。`id()` 即前端 key，改动即破坏前端缓存与文案映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageCategory {
    /// 工具自身数据（`config::store_dir()`）。
    Tool,
    /// 客户端数据（`WbVariant::data_root()`），逐档位各一条。
    Client,
    /// 会话资产（`{data_root}/projects`），能力由 `session::session_copy_supported_at` 探测。
    Sessions,
    /// 回收站（`{store_dir}/trash`）。
    Trash,
}

impl StorageCategory {
    /// 全部分类（遍历入口）。
    pub const ALL: [StorageCategory; 4] = [
        StorageCategory::Tool,
        StorageCategory::Client,
        StorageCategory::Sessions,
        StorageCategory::Trash,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Client => "client",
            Self::Sessions => "sessions",
            Self::Trash => "trash",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Tool => "工具数据",
            Self::Client => "客户端数据",
            Self::Sessions => "会话资产",
            Self::Trash => "回收站",
        }
    }

    /// 回收站不走 `clean`（它只有 `purge` 一个出口），其余三分类可生成清理计划。
    pub fn is_cleanable(self) -> bool {
        !matches!(self, Self::Trash)
    }

    /// 解析前端传来的分类 id；未知值返回 `None`（不静默回落成任何分类）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tool" => Some(Self::Tool),
            "client" => Some(Self::Client),
            "sessions" => Some(Self::Sessions),
            "trash" => Some(Self::Trash),
            _ => None,
        }
    }
}

/// 一条根记录。`variant` 对工具数据/回收站无意义，固定填 `Cn` 占位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageRoot {
    pub category: StorageCategory,
    pub variant: WbVariant,
    pub path: PathBuf,
    pub exists: bool,
}

/// 取路径最后一段。用于把既有的绝对根「重挂」到注入的家目录上，
/// 这样既能复用 `config::store_dir()` / `variant::data_root()` 作为单一事实来源，
/// 又不需要在测试里碰真实家目录。
fn leaf_of(path: &Path) -> PathBuf {
    path.file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| path.to_path_buf())
}

/// 注入家目录的工具存储根（等价于 `config::store_dir()`）。
pub fn store_root_at(home: &Path) -> PathBuf {
    home.join(leaf_of(&config::store_dir()))
}

/// 注入家目录的档位数据根（等价于 `WbVariant::data_root()`）。
pub fn data_root_at(variant: WbVariant, home: &Path) -> PathBuf {
    home.join(leaf_of(&variant.data_root()))
}

/// 全档位、全分类的根清单（真实家目录）。
pub fn resolve_roots() -> Vec<StorageRoot> {
    resolve_roots_at(&config::home_dir())
}

/// [`resolve_roots`] 的可测实现：注入家目录，禁止触碰真实 home。
///
/// 返回顺序固定为：工具数据 → 回收站 → 每个档位的客户端数据 + 会话资产，
/// 前端按这个顺序渲染卡片，不依赖 `HashMap` 遍历顺序。
pub fn resolve_roots_at(home: &Path) -> Vec<StorageRoot> {
    let store = store_root_at(home);
    let trash = store.join("trash");
    let mut roots = vec![
        StorageRoot {
            category: StorageCategory::Tool,
            variant: WbVariant::Cn,
            exists: store.is_dir(),
            path: store,
        },
        StorageRoot {
            category: StorageCategory::Trash,
            variant: WbVariant::Cn,
            exists: trash.is_dir(),
            path: trash,
        },
    ];
    for variant in WbVariant::ALL {
        let data = data_root_at(variant, home);
        let sessions = data.join("projects");
        roots.push(StorageRoot {
            category: StorageCategory::Client,
            variant,
            exists: data.is_dir(),
            path: data.clone(),
        });
        roots.push(StorageRoot {
            category: StorageCategory::Sessions,
            variant,
            // 不得按档位写死：国际版数据根与国内版不同构（实测无 projects/）。
            exists: session::session_copy_supported_at(&data),
            path: sessions,
        });
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-storage-roots-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时家目录");
        dir
    }

    #[test]
    fn covers_four_categories_and_both_variants() {
        let home = temp_home("cover");
        let roots = resolve_roots_at(&home);
        let categories: Vec<&'static str> = roots.iter().map(|r| r.category.id()).collect();
        assert_eq!(categories, vec!["tool", "trash", "client", "sessions", "client", "sessions"]);
        let variants: Vec<&'static str> = roots
            .iter()
            .filter(|r| r.category == StorageCategory::Client)
            .map(|r| r.variant.as_str())
            .collect();
        assert_eq!(variants, vec!["cn", "ai"]);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_roots_are_reported_with_exists_false() {
        let home = temp_home("missing");
        let roots = resolve_roots_at(&home);
        assert_eq!(roots.len(), 6);
        // 零值必须显式：不存在返回 exists=false 而不是被过滤掉。
        assert!(roots.iter().all(|r| !r.exists));
        // 路径本身仍然给出，前端才能显示「空」而不是「--」。
        assert!(roots
            .iter()
            .all(|r| !r.path.as_os_str().is_empty() && r.path.starts_with(&home)));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn existing_store_dir_is_reported() {
        let home = temp_home("exists");
        std::fs::create_dir_all(store_root_at(&home).join("trash")).expect("建 trash");
        let roots = resolve_roots_at(&home);
        let tool = roots
            .iter()
            .find(|r| r.category == StorageCategory::Tool)
            .expect("tool 根");
        let trash = roots
            .iter()
            .find(|r| r.category == StorageCategory::Trash)
            .expect("trash 根");
        assert!(tool.exists);
        assert!(trash.exists);
        assert!(trash.path.ends_with("trash"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn category_parse_rejects_unknown_instead_of_defaulting() {
        assert_eq!(StorageCategory::parse("sessions"), Some(StorageCategory::Sessions));
        assert_eq!(StorageCategory::parse(" TRASH "), Some(StorageCategory::Trash));
        assert_eq!(StorageCategory::parse("everything"), None);
        assert_eq!(StorageCategory::parse(""), None);
    }

    #[test]
    fn only_trash_is_not_cleanable() {
        assert!(!StorageCategory::Trash.is_cleanable());
        for category in [
            StorageCategory::Tool,
            StorageCategory::Client,
            StorageCategory::Sessions,
        ] {
            assert!(category.is_cleanable());
        }
    }
}
