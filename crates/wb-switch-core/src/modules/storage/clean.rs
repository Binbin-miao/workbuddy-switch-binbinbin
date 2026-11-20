//! 清理计划与执行。
//!
//! 两段式：`plan_*` 纯读（G2，零副作用）→ `execute_*` 真正移入回收站。
//! `dry_run = true` 时一个字节都不动，只回计划摘要；这是接口的默认值，
//! 用户必须显式点「确认清理」才会真的移动文件。
//!
//! **可清理清单是显式白名单，不是「排除保护根后剩下的全部」。**
//! 原因：客户端数据根下还躺着 `skills/`、`experts/`、`memory/`、`IDENTITY.md` 等
//! 用户资产，按「非保护即清理」枚举会把它们当成垃圾列进清理计划（风险 R1）。

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::catalog::{measure_usage, modified_ms};
use super::protected::{ensure_variant_idle, guard_delete_at, EntryKind};
use super::roots::{data_root_at, store_root_at, StorageCategory};
use super::snapshot::invalidate_snapshot;
use super::trash::{move_to_trash_detailed_at, trash_root};
use crate::modules::config;
use crate::modules::variant::WbVariant;

/// 工具侧的缓存 / 日志文件（可安全重建）。
const TOOL_CACHE_FILES: [&str; 8] = [
    "hook-events.jsonl",
    "notifications.json",
    "travel_cache.json",
    "credit_usage_snapshots.json",
    "official_usage_cache.json",
    "auto_checkin_logs.json",
    "daily_chat_logs.json",
    "error.log",
];

/// 客户端侧的缓存目录（删掉后客户端会自行重建；不含用户资产）。
const CLIENT_CACHE_DIRS: [&str; 8] = [
    "cache",
    "logs",
    "traces",
    "diagnostics",
    "knowledge-upload-tmp",
    "shell-snapshots",
    "clipboard-images",
    "pending-telemetry",
];

/// 一条待清理项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanEntry {
    pub path: PathBuf,
    pub bytes: u64,
    pub kind: EntryKind,
    pub reason: &'static str,
    pub variant: WbVariant,
}

/// 被跳过的目标及原因（受保护 / 权限不足 / 时间未到）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanSkip {
    pub path: PathBuf,
    pub why: String,
}

/// 清理计划（只读产物）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanPlan {
    pub category: StorageCategory,
    pub entries: Vec<CleanEntry>,
    pub skipped: Vec<CleanSkip>,
    pub bytes: u64,
}

/// 执行结果。`dry_run = true` 时 `moved` 表示「将会移动多少项」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanOutcome {
    pub dry_run: bool,
    pub trash_id: Option<String>,
    pub moved: usize,
    pub bytes: u64,
    pub skipped: Vec<CleanSkip>,
}

/// 生成清理计划（真实家目录）。
pub fn plan_clean(
    category: StorageCategory,
    older_than_days: Option<u32>,
) -> Result<CleanPlan, String> {
    plan_clean_in(category, older_than_days, &config::home_dir())
}

/// 生成清理计划（可测实现：注入家目录）。
pub fn plan_clean_in(
    category: StorageCategory,
    older_than_days: Option<u32>,
    home: &Path,
) -> Result<CleanPlan, String> {
    let candidates = match category {
        StorageCategory::Tool => tool_candidates(home),
        StorageCategory::Client => client_candidates(home),
        StorageCategory::Sessions => session_candidates(home),
        // 回收站只有 purge 一个出口，不参与 clean。
        StorageCategory::Trash => {
            return Err("回收站只能彻底删除，不参与清理".to_string());
        }
    };
    let cutoff = older_than_days.map(|days| config::now_ms() - i64::from(days) * 86_400_000);
    let mut entries: Vec<CleanEntry> = Vec::new();
    let mut skipped: Vec<CleanSkip> = Vec::new();
    for candidate in candidates {
        if !candidate.path.exists() {
            continue;
        }
        if let Some(cutoff) = cutoff {
            if modified_ms(&candidate.path) > cutoff {
                continue;
            }
        }
        let Some(kind) = EntryKind::of_path(&candidate.path) else {
            skipped.push(CleanSkip {
                path: candidate.path,
                why: "既不是普通文件也不是目录".to_string(),
            });
            continue;
        };
        if let Err(why) = guard_delete_at(home, &candidate.path, kind) {
            skipped.push(CleanSkip {
                path: candidate.path,
                why,
            });
            continue;
        }
        entries.push(CleanEntry {
            bytes: bytes_of(&candidate.path, kind),
            path: candidate.path,
            kind,
            reason: candidate.reason,
            variant: candidate.variant,
        });
    }
    entries.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    let bytes = entries.iter().map(|entry| entry.bytes).sum();
    Ok(CleanPlan {
        category,
        entries,
        skipped,
        bytes,
    })
}

struct Candidate {
    path: PathBuf,
    reason: &'static str,
    variant: WbVariant,
}

fn tool_candidates(home: &Path) -> Vec<Candidate> {
    let store = store_root_at(home);
    TOOL_CACHE_FILES
        .iter()
        .map(|name| Candidate {
            path: store.join(name),
            reason: "工具缓存与日志，可安全重建",
            variant: WbVariant::Cn,
        })
        .collect()
}

fn client_candidates(home: &Path) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for variant in WbVariant::ALL {
        let data = data_root_at(variant, home);
        for name in CLIENT_CACHE_DIRS {
            candidates.push(Candidate {
                path: data.join(name),
                reason: "客户端缓存目录，可安全重建",
                variant,
            });
        }
    }
    candidates
}

fn session_candidates(home: &Path) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for variant in WbVariant::ALL {
        let projects = data_root_at(variant, home).join("projects");
        let Ok(project_entries) = std::fs::read_dir(&projects) else {
            continue;
        };
        for project_entry in project_entries.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }
            let Ok(session_entries) = std::fs::read_dir(&project_path) else {
                continue;
            };
            for session_entry in session_entries.flatten() {
                let session_path = session_entry.path();
                if session_path.is_dir() {
                    candidates.push(Candidate {
                        path: session_path,
                        reason: "会话资产目录，删除后无法在客户端恢复",
                        variant,
                    });
                }
            }
        }
    }
    candidates
}

fn bytes_of(path: &Path, kind: EntryKind) -> u64 {
    match kind {
        EntryKind::Dir => measure_usage(path).bytes,
        EntryKind::File => std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
    }
}

/// 执行清理（真实家目录）。
pub fn execute_clean(plan: &CleanPlan, dry_run: bool) -> Result<CleanOutcome, String> {
    execute_clean_in(plan, dry_run, &trash_root())
}

/// 执行清理（可测实现：注入回收站根）。
///
/// 顺序不可颠倒：先 P3（客户端运行中拒绝）→ 再移入回收站 → 最后失效快照。
pub fn execute_clean_in(
    plan: &CleanPlan,
    dry_run: bool,
    trash_root_path: &Path,
) -> Result<CleanOutcome, String> {
    if dry_run {
        return Ok(CleanOutcome {
            dry_run: true,
            trash_id: None,
            moved: plan.entries.len(),
            bytes: plan.bytes,
            skipped: Vec::new(),
        });
    }
    if matches!(
        plan.category,
        StorageCategory::Client | StorageCategory::Sessions
    ) {
        let mut variants: Vec<WbVariant> = Vec::new();
        for entry in &plan.entries {
            if !variants.contains(&entry.variant) {
                variants.push(entry.variant);
            }
        }
        for variant in variants {
            ensure_variant_idle(variant)?;
        }
    }
    if plan.entries.is_empty() {
        // 零值不当成功，但也不是错误：返回 moved=0 让前端显示「没有需要清理的项目」。
        return Ok(CleanOutcome {
            dry_run: false,
            trash_id: None,
            moved: 0,
            bytes: 0,
            skipped: Vec::new(),
        });
    }
    let paths: Vec<PathBuf> = plan.entries.iter().map(|entry| entry.path.clone()).collect();
    let outcome = move_to_trash_detailed_at(trash_root_path, &paths, "storage_clean")?;
    invalidate_snapshot();
    Ok(CleanOutcome {
        dry_run: false,
        trash_id: outcome.entry.map(|entry| entry.id),
        moved: outcome.moved,
        bytes: outcome.bytes,
        skipped: outcome
            .skipped
            .into_iter()
            .map(|skip| CleanSkip {
                path: skip.path,
                why: skip.why,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::storage::trash::{list_trash_at, trash_root_at};

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-switch-storage-clean-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时家目录");
        dir
    }

    #[test]
    fn tool_plan_lists_only_known_cache_files() {
        let home = temp_home("tool");
        let store = store_root_at(&home);
        std::fs::create_dir_all(&store).expect("建工具目录");
        std::fs::write(store.join("notifications.json"), vec![b'n'; 5]).expect("写缓存");
        // 账号库是受保护的，必须既不在 entries 也不在 skipped 里被当成垃圾。
        std::fs::write(store.join("accounts.json"), b"[]").expect("写账号库");
        std::fs::write(store.join("unknown.bin"), b"?").expect("写未知文件");

        let plan = plan_clean_in(StorageCategory::Tool, None, &home).expect("计划");
        let names: Vec<String> = plan
            .entries
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(names, vec!["notifications.json"]);
        assert_eq!(plan.bytes, 5);
        // 白名单之外的未知文件永远不会进入计划。
        assert!(!names.contains(&"unknown.bin".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn trash_category_cannot_be_planned() {
        let home = temp_home("trashcat");
        let error = plan_clean_in(StorageCategory::Trash, None, &home).expect_err("必须拒绝");
        assert!(error.contains("彻底删除"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dry_run_moves_nothing() {
        let home = temp_home("dryrun");
        let data = data_root_at(WbVariant::Cn, &home);
        std::fs::create_dir_all(data.join("cache")).expect("建缓存");
        std::fs::write(data.join("cache/x.bin"), vec![b'x'; 9]).expect("写文件");
        let plan = plan_clean_in(StorageCategory::Client, None, &home).expect("计划");
        assert_eq!(plan.entries.len(), 1);

        let outcome = execute_clean_in(&plan, true, &trash_root_at(&home)).expect("dry run");
        assert!(outcome.dry_run);
        assert_eq!(outcome.moved, 1);
        assert!(outcome.trash_id.is_none());
        assert!(data.join("cache/x.bin").exists(), "dry run 不得动任何字节");
        assert!(list_trash_at(&trash_root_at(&home)).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn real_run_moves_into_trash_and_can_be_restored() {
        let home = temp_home("real");
        let store = store_root_at(&home);
        std::fs::create_dir_all(&store).expect("建工具目录");
        let file = store.join("notifications.json");
        std::fs::write(&file, vec![b'y'; 11]).expect("写缓存");
        let plan = plan_clean_in(StorageCategory::Tool, None, &home).expect("计划");

        let outcome = execute_clean_in(&plan, false, &trash_root_at(&home)).expect("真实清理");
        assert!(!outcome.dry_run);
        let id = outcome.trash_id.expect("应有回收站条目");
        assert_eq!(outcome.moved, 1);
        assert_eq!(outcome.bytes, 11);
        assert!(!file.exists(), "原文件必须已移走");
        assert_eq!(list_trash_at(&trash_root_at(&home)).len(), 1);

        let restored = crate::modules::storage::trash::restore_trash_at(
            &trash_root_at(&home),
            &id,
            false,
        )
        .expect("还原");
        assert_eq!(restored.restored, 1);
        assert!(file.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn older_than_days_filters_by_mtime() {
        let home = temp_home("aging");
        let store = store_root_at(&home);
        std::fs::create_dir_all(&store).expect("建工具目录");
        std::fs::write(store.join("notifications.json"), b"n").expect("写缓存");
        // 90 天前的门槛：刚写的文件必须被排除。
        let plan = plan_clean_in(StorageCategory::Tool, Some(90), &home).expect("计划");
        assert!(plan.entries.is_empty());
        assert_eq!(plan.bytes, 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn session_plan_finds_assets_but_never_the_projects_root() {
        let home = temp_home("sessions");
        let data = data_root_at(WbVariant::Cn, &home);
        let session = data.join("projects/proj-a/session-1");
        std::fs::create_dir_all(&session).expect("建会话目录");
        std::fs::write(session.join("log.json"), vec![b'l'; 3]).expect("写会话文件");

        let plan = plan_clean_in(StorageCategory::Sessions, None, &home).expect("计划");
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].bytes, 3);
        assert!(plan.entries[0].path.ends_with("session-1"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_plan_returns_zero_moved_not_fake_success() {
        let home = temp_home("emptyplan");
        let plan = plan_clean_in(StorageCategory::Tool, None, &home).expect("计划");
        assert!(plan.entries.is_empty());
        let outcome = execute_clean_in(&plan, false, &trash_root_at(&home)).expect("执行");
        assert_eq!(outcome.moved, 0);
        assert_eq!(outcome.bytes, 0);
        std::fs::remove_dir_all(&home).ok();
    }
}
