//! 账号切换：备份 → 关进程 → 恢复/复制会话（可选）→ 写认证 → 启动。
//!
//! 对照 server.py `switch_account`。切换过程中通过进度回调向前端推送实时进度，
//! 避免界面长时间无反馈被误认为卡死。core 不依赖 Tauri，进度回调由宿主适配
//! （桌面端转发为 `switch-progress` 事件，HTTP 端写入轮询/SSE）。
//!
//! 顺序要点（design §1）：互斥 → 关进程 → 恢复/复制/同步会话 → 写认证 → 启动。
//! 复制与同步顺序调用、共用同一把档位操作锁，且在「关闭 App + 恢复未完成写入」之后
//! 才执行；会话写入一律在 WorkBuddy 停止写入之后。普通会话操作失败不阻止认证切换，
//! 只有「无法安全恢复的中间产物」才暂停切换并明确报告恢复需求。
//!
//! 暂停启动的判定有两次，口径相同（design §5 / R5）：
//!   1. 写入之前——切换开头恢复未完成写入后仍有不可安全恢复的中间产物；
//!   2. 写入之后——本次复制/同步新留下的未完成操作（正文/数据库/组表任一阶段失败都会
//!      保留操作记录），不能带着不一致的会话内容写认证并启动 App。

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::auth_file;
use crate::modules::experts;
use crate::modules::memory;
use crate::modules::mirror;
use crate::modules::process::{close_workbuddy, launch_workbuddy};
use crate::modules::session::{self, SessionPaths, SyncSelection};
use crate::modules::session_link::{
    self, Operation, RecoveryReport, LOCK_BUSY_MESSAGE_PREFIX, LOCK_UNAVAILABLE_MESSAGE_PREFIX,
};
use crate::modules::variant::WbVariant;

/// 切换进度回调（宿主注入，如 Tauri `app.emit` 或 HTTP 进度缓存）。
pub type ProgressFn = Box<dyn Fn(&str) + Send + Sync>;

/// `restart=false` 携带会话写入意图时的拒绝文案（复制/同步各自一条）。
pub const COPY_WITHOUT_RESTART_MESSAGE: &str =
    "本次切换未重启 WorkBuddy（restart=false），已拒绝会话复制请求；如需复制请勾选重启切换";
pub const SYNC_WITHOUT_RESTART_MESSAGE: &str =
    "本次切换未重启 WorkBuddy（restart=false），已拒绝会话同步请求；如需同步请勾选重启切换";

/// 恢复报告里是否存在阻碍启动的问题：未恢复一致的中间产物必须先处理。
///
/// 拿不到档位锁、或中间产物被改动/丢失都属此类（design §4 / §5）：此时继续写认证并
/// 启动 App 会让 Official App 在最坏状态下打开会话，因此暂停切换与启动。
/// `retryable=true` 沿用复制侧「可延后重试且不阻断启动」的约定；同步恢复失败必须
/// 返回 false，即使故障只是暂时的，也不能带着半完成的正文/数据库/基线启动。
pub fn recovery_blocks_startup(report: &RecoveryReport) -> bool {
    report.needs_recovery.iter().any(|issue| !issue.retryable)
}

/// 阻碍启动的原因汇总（用于错误文案）。
///
/// 必须带操作标识：阻断分支只返回 Err 字符串，前端 catch 不能只看到原因。
pub fn recovery_blocking_detail(report: &RecoveryReport) -> String {
    report
        .needs_recovery
        .iter()
        .filter(|issue| !issue.retryable)
        .map(|issue| format!("{}：{}", issue.operation_id, issue.reason))
        .collect::<Vec<String>>()
        .join("；")
}

/// 恢复报告的前端投影（与复制/同步报告同形，便于展示恢复信息）。
pub fn recovery_report_json(report: &RecoveryReport) -> Value {
    json!({
        "recovered": report.recovered.len(),
        "abandoned": report.abandoned.len(),
        "needsRecovery": report
            .needs_recovery
            .iter()
            .map(|issue| json!({
                "operationId": issue.operation_id,
                "reason": issue.reason,
                "retryable": issue.retryable,
            }))
            .collect::<Vec<Value>>(),
        // 附属目录未能完整补齐：非阻断提示，与 needsRecovery 分开，避免被当成阻断项。
        "sidecarWarnings": report
            .sidecar_warnings
            .iter()
            .map(|warning| json!({
                "operationId": warning.operation_id,
                "sessionId": warning.session_id,
                "reason": warning.reason,
            }))
            .collect::<Vec<Value>>(),
        // 待清理/待恢复的临时备份残留：与复制/同步报告同一结构。
        "temporaryFiles": report.temporary_files,
    })
}

/// 某档位当前未完成操作的 id 集合。
///
/// 用于区分「本次新产生的未完成写入」与「切换开头已尝试恢复的历史残留」：
/// 后者若是可重试类（例如映射库暂不可用），按既有口径不阻断切换。
fn pending_operation_ids(paths: &SessionPaths, variant: WbVariant) -> BTreeSet<String> {
    session_link::pending_operations(paths, variant)
        .into_iter()
        .map(|operation| operation.operation_id)
        .collect()
}

/// 本次会话写入新留下的未完成操作（按创建顺序，便于稳定输出）。
fn newly_unfinished_writes(before: &BTreeSet<String>, after: Vec<Operation>) -> Vec<Operation> {
    let mut created: Vec<Operation> = after
        .into_iter()
        .filter(|operation| !before.contains(&operation.operation_id))
        .collect();
    created.sort_by_key(|operation| operation.created_at);
    created
}

/// 未完成写入的说明文案：会话标识 + 操作标识 + 原因。
fn unfinished_writes_detail(writes: &[Operation]) -> String {
    writes
        .iter()
        .map(|operation| {
            let reason = operation
                .last_error
                .clone()
                .unwrap_or_else(|| "尚未完成".to_string());
            format!(
                "{}（操作 {}）：{reason}",
                operation.target.session_id, operation.operation_id
            )
        })
        .collect::<Vec<String>>()
        .join("；")
}

/// restart=false 时携带写入意图：显式拒绝对应的会话操作（不静默丢弃，design §4.1）。
fn reject_session_writes_without_restart(
    has_copy: bool,
    has_sync: bool,
) -> (Option<Value>, Option<Value>) {
    let copy = has_copy.then(|| json!({ "error": COPY_WITHOUT_RESTART_MESSAGE }));
    let sync = has_sync.then(|| {
        json!({
            "synced": [],
            "skipped": [],
            "errors": [{ "error": SYNC_WITHOUT_RESTART_MESSAGE }],
        })
    });
    (copy, sync)
}

/// `restart=false` 携带镜像意图时的拒绝文案。
pub const MIRROR_WITHOUT_RESTART_MESSAGE: &str =
    "镜像切换必须在重启 WorkBuddy 的前提下执行；本次未重启（restart=false），已拒绝镜像请求";

/// `restart=false` 且请求了镜像切换 → 返回可读错误（在备份/进程/写入之前失败）。
fn reject_mirror_without_restart(restart: bool, has_mirror: bool) -> Option<String> {
    (!restart && has_mirror).then(|| MIRROR_WITHOUT_RESTART_MESSAGE.to_string())
}

/// 镜像结果 → 报告 JSON：失败**不阻断切号**，统一投影为 MirrorReport 形状（错误进 errors）。
fn mirror_report_json(result: Result<mirror::MirrorReport, String>) -> Value {
    let report = result.unwrap_or_else(|error| mirror::MirrorReport {
        deleted_sessions: 0,
        copied_sessions: 0,
        restored_copies: 0,
        backup: None,
        needs_recovery: false,
        experts_updated: false,
        experts_restored: false,
        warnings: Vec::new(),
        skipped: Vec::new(),
        errors: vec![mirror::MirrorError {
            session_id: String::new(),
            error,
        }],
    });
    serde_json::to_value(&report).unwrap_or(Value::Null)
}

/// 还原镜像备份（写端点）：自己持跨进程 auth 锁后再还原，两端宿主共用同一入口。
pub fn restore_mirror_backup(backup: &Path) -> Result<mirror::MirrorReport, String> {
    let variant = mirror_backup_variant(backup);
    let _guard = crate::modules::auth_transaction::try_auth().map_err(|error| error.to_string())?;
    let paths = SessionPaths::for_variant(variant);
    // 限根：只允许还原落在镜像备份根目录内的路径（硬要求在 mirror 侧围栏，不改回滚算法）。
    // 专家团清单挂在数据根下（与档位无关），生产恒取真实数据根。
    mirror::mirror_restore_within_root(
        &paths,
        &crate::modules::config::wb_data_root(),
        variant,
        backup,
    )
}

/// 从备份目录推断档位：目录约定为 `…/session-transactions/{variant}/{uuid}`。
fn mirror_backup_variant(backup: &Path) -> WbVariant {
    backup
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .map(|raw| WbVariant::parse(Some(raw)))
        .unwrap_or(WbVariant::Cn)
}

/// 账号库到 `[(uid, label)]` 的投影：记忆合并与专家团同步共用同一份形状。
///
/// 缺 uid 的账号照样保留（uid 为空串），由各自的模块决定怎么报告——这里不做静默丢弃。
fn account_pairs() -> Vec<(String, String)> {
    account::load_accounts()
        .iter()
        .map(|acc| {
            let uid = acc
                .get("uid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (uid, account::account_display_name(acc))
        })
        .collect()
}

/// 切换账号。
///
/// `copy_session_ids` 非空时按路径 B 复制勾选会话（新 id，云端可同步）；
/// `sync_selections` 非空时把来源账号的新增内容同步到目标账号的关联会话（保留目标
/// sessionId、标题与自定义标题）。两者共用同一把档位操作锁，顺序执行、不重复写入。
pub fn switch_account(
    progress_fn: Option<&ProgressFn>,
    account_id: &str,
    restart: bool,
    share_sessions: bool,
    copy_session_ids: &[String],
    sync_selections: &[SyncSelection],
    memory_sync: Option<memory::MemorySyncMode>,
    memory_source_uid: Option<&str>,
    merge_experts: bool,
    sync_persona: bool,
    persona_mode: memory::PersonaMode,
    mirror_plan: Option<mirror::MirrorPlan>,
    deleted_sync_plan: Option<mirror::DeletedSyncPlan>,
) -> Result<Value, String> {
    // 快照与导出必须和令牌刷新处在同一个事务里：在任何备份、进程管理之前取锁，
    // 这样一次失败的切换不会留下副作用。
    let auth_guard = crate::modules::auth_transaction::try_auth().map_err(|e| e.to_string())?;
    // 镜像切换是破坏性 DB 写入，必须在重启（关闭 App）前提下执行；未重启即拒绝，
    // 且在备份/进程/写入之前失败，绝不静默丢弃用户已确认的镜像意图。
    if let Some(error) = reject_mirror_without_restart(
        restart,
        mirror_plan.is_some() || deleted_sync_plan.is_some(),
    ) {
        return Err(error);
    }
    // 两者都写 sessions 表、targets 可能重叠：同时跑会让后者的现场摘要必然失配。
    if mirror_plan.is_some() && deleted_sync_plan.is_some() {
        return Err("镜像切换与「同步已删除会话」互斥：一次切号只能选其中一种".to_string());
    }
    // 镜像与「复制 / 同步会话」语义互斥：镜像会把目标账号的会话整份对齐，而逐项同步会
    // 先改目标库，现场摘要随即与预览不符 → 镜像必然失败，用户却只看到「切号成功」。
    // 这里在任何写入之前直接拒绝，不做静默降级。
    if mirror_plan.is_some() && (!copy_session_ids.is_empty() || !sync_selections.is_empty()) {
        return Err("镜像切换与复制/同步会话互斥：一次切号只能选其中一种".to_string());
    }
    let progress = |message: &str| {
        eprintln!("[switch] progress: {message}");
        if let Some(p) = progress_fn {
            p(message);
        }
    };

    progress("开始切换账号…");
    let acc =
        account::find_account(account_id).ok_or_else(|| format!("账号不存在: {account_id}"))?;
    // 档位以账号自身为准：签名里的参数无法表达「用 A 档位操作 B 档位账号」。
    let variant = account::variant_of(&acc);
    // 备份失败必须在这里中止：`?` 之后才允许任何会话/记忆写入与认证覆盖，
    // 保证「写认证前一定有可回滚的磁盘副本」。Ok(None) 表示原文件本就不存在，
    // 允许继续，但如实记录：此时无磁盘备份，写后失败靠写前快照自动回滚。
    let backup = auth_file::backup_auth_file(variant)?;
    if backup.is_none() {
        eprintln!("[switch] 切换前认证文件不存在，本次不生成备份；写认证失败时按写前快照自动回滚");
    }

    let mut copy_report: Option<Value> = None;
    let mut session_report: Option<Value> = None;
    let mut sync_report: Option<Value> = None;
    let mut recovery_report: Option<Value> = None;
    let mut memory_report: Option<Value> = None;
    let mut experts_report: Option<Value> = None;
    let mut persona_report: Option<Value> = None;
    let mut mirror_report: Option<Value> = None;
    let mut deleted_sync_report: Option<Value> = None;
    if restart {
        progress("正在关闭 WorkBuddy…");
        close_workbuddy(variant, 20)?;
        // 关进程后先恢复未完成的会话写入：恢复成功或复制侧可安全延后重试的失败
        // 不阻断切换；同步仍未完成、拿不到锁或中间产物异常则暂停启动（design §4 / §5）。
        let recovery = match session::recover_pending_session_operations(variant) {
            Ok(report) => report,
            Err(error) => {
                return Err(format!(
                    "无法恢复未完成的会话写入（{error}），已暂停切换与启动 WorkBuddy；请稍后重试"
                ));
            }
        };
        let blocking = recovery_blocks_startup(&recovery);
        if !recovery.is_empty() {
            recovery_report = Some(recovery_report_json(&recovery));
        }
        if blocking {
            let detail = recovery_blocking_detail(&recovery);
            return Err(format!(
                "检测到无法安全恢复的会话写入（{detail}），已暂停切换与启动 WorkBuddy；请先处理该会话后再试"
            ));
        }
        // 本次是否请求了会话写入；同时记下写入前已存在的未完成记录，供写入后比对。
        let ops_paths = SessionPaths::for_variant(variant);
        let attempts_session_writes = !copy_session_ids.is_empty() || !sync_selections.is_empty();
        let pending_before = if attempts_session_writes {
            pending_operation_ids(&ops_paths, variant)
        } else {
            BTreeSet::new()
        };
        if !copy_session_ids.is_empty() {
            progress("正在复制会话到目标账号…");
            // 复制失败本身只记进报告、不阻断切换；但若本次留下了未完成的写入，
            // 会在复制与同步都跑完后统一暂停切换与启动（见下方 pending 差集检查）。
            copy_report = Some(
                match session::copy_sessions_for_switch(&acc, copy_session_ids) {
                    Ok(report) => report,
                    // 档位锁被占用或无法建立互斥：此时可能另有会话写入正在进行，
                    // 不能当成普通复制失败后继续写认证并启动 App。锁失败文案前缀
                    // 由 session_link 提供，不在这里嗅探整句错误文案。
                    Err(error)
                        if error.starts_with(LOCK_BUSY_MESSAGE_PREFIX)
                            || error.starts_with(LOCK_UNAVAILABLE_MESSAGE_PREFIX) =>
                    {
                        return Err(format!(
                            "无法独占会话操作（{error}），已暂停切换与启动 WorkBuddy；请稍后重试"
                        ));
                    }
                    Err(error) => json!({"error": error}),
                },
            );
        }
        if !sync_selections.is_empty() {
            // 同步与复制顺序执行（同一把档位锁），不重复写入。
            progress("正在同步会话到目标账号…");
            sync_report = Some(
                match session::sync_sessions_for_switch(&acc, sync_selections) {
                    Ok(report) => report,
                    Err(error)
                        if error.starts_with(LOCK_BUSY_MESSAGE_PREFIX)
                            || error.starts_with(LOCK_UNAVAILABLE_MESSAGE_PREFIX) =>
                    {
                        return Err(format!(
                            "无法独占会话操作（{error}），已暂停切换与启动 WorkBuddy；请稍后重试"
                        ));
                    }
                    // 同步失败不阻断切换：契约与成功路径同形，错误挂在 errors 里。
                    Err(error) => json!({
                        "synced": [],
                        "skipped": [],
                        "errors": [{ "error": error }],
                    }),
                },
            );
        }
        if share_sessions {
            // 旧的「全体转移」兼容路径（默认关闭），Rust 版暂未实现
            session_report = Some(json!({"error": "share_sessions 兼容路径暂未在 Rust 版实现"}));
        }
        // 本次复制/同步新留下的未完成操作：不带着不一致的会话内容写认证并启动 App
        // （design §5 / R5）。未完成写入也不在这里被报告成成功——错误已在各自报告里，
        // 这里只决定「暂停」。未开始写入的失败（预览过期、源会话被删等）不产生操作记录，
        // 因此仍然只是跳过该项、继续切号。
        if attempts_session_writes {
            let created = newly_unfinished_writes(
                &pending_before,
                session_link::pending_operations(&ops_paths, variant),
            );
            if !created.is_empty() {
                return Err(format!(
                    "本次会话写入未完成（{}），已暂停切换与启动 WorkBuddy；请重试，下次切号会先完成恢复",
                    unfinished_writes_detail(&created)
                ));
            }
        }
        if let Some(plan) = mirror_plan.as_ref() {
            // 镜像软删：失败不阻断切号（与 copy/sync 同形记进报告，字段名 mirror）。
            progress("正在执行整份镜像…");
            let target_uid = acc.get("uid").and_then(Value::as_str).unwrap_or_default();
            let paths = SessionPaths::for_variant(variant);
            mirror_report = Some(mirror_report_json(mirror::mirror_apply_at(
                &paths,
                variant,
                target_uid,
                &acc,
                // 人格目录只挂在数据根下（与档位无关），生产恒取真实数据根。
                &crate::modules::config::wb_data_root(),
                plan,
            )));
        }
        if let Some(plan) = deleted_sync_plan.as_ref() {
            // 删除同步：把「别的账号已删、这里还留着」的会话也软删。
            // 失败不阻断切号，与镜像同形记进报告（字段名 deletedSync）。
            progress("正在同步其他账号已删除的会话…");
            let paths = SessionPaths::for_variant(variant);
            deleted_sync_report = Some(mirror_report_json(mirror::deleted_sync_apply_at(
                &paths, variant, plan,
            )));
        }
    } else {
        // restart=false 表示本次不做任何会话写入；携带写入意图时显式拒绝该会话操作，
        // 不能静默丢弃（design §4.1）。
        let (copy, sync) = reject_session_writes_without_restart(
            !copy_session_ids.is_empty(),
            !sync_selections.is_empty(),
        );
        copy_report = copy;
        sync_report = sync;
    }
    if let Some(mode) = memory_sync {
        // 记忆是纯本地 Markdown 文件，与会话写入无关：不受 restart=false 限制，也不参与
        // 「未完成会话写入」的暂停判定。必须在写认证之前执行——那时账号库与各账号的
        // 人格目录都还是切换前的状态。两种模式互斥，分派表保持单一出口。
        memory_report = Some(match mode {
            memory::MemorySyncMode::Merge => {
                progress("正在合并并分发工作记忆…");
                memory::merge_and_distribute(&account_pairs())
            }
            memory::MemorySyncMode::Overwrite => {
                progress("正在复制并覆盖工作记忆…");
                memory::copy_and_overwrite(memory_source_uid, &account_pairs())
            }
        });
    }
    if sync_persona {
        // 人设与记忆解耦：没勾「合并记忆」也能单独同步人设。
        // 同样必须在写认证之前——那时各账号的人格目录还是切换前的状态。
        progress(if persona_mode == memory::PersonaMode::Overwrite {
            "正在同步人设（以最完整一份为准）…"
        } else {
            "正在同步人设（只替换默认模板）…"
        });
        persona_report = Some(memory::sync_persona(&account_pairs(), persona_mode));
    }
    if merge_experts {
        // 专家团清单也是纯本地 JSON 文件（`{home}/.workbuddy/experts/custom/<uid>/experts.json`），
        // 与会话写入无关，同样不受 restart=false 限制。位置必须在写认证之前、并且在
        // restart=true 的 close_workbuddy 之后：留给 App 的写回窗口越小，清单越不会被覆盖。
        progress("正在同步专家团…");
        experts_report = Some(experts::merge_and_distribute(&account_pairs()));
    }
    progress("正在写入认证文件…");
    auth_file::write_account_to_auth_file(&acc, variant)?;
    // 启动客户端可能要几十秒，且新客户端可能立刻发起刷新，锁必须在这里放掉。
    drop(auth_guard);
    if restart {
        progress("正在启动 WorkBuddy…");
        launch_workbuddy(variant, Some(&progress))?;
    }
    progress("切换完成");

    let mut result = json!({
        "ok": true,
        "account": account::account_display_name(&acc),
        "variant": variant.as_str(),
        "backup": backup.map(|p| p.to_string_lossy().to_string()),
    });
    if let Some(c) = copy_report {
        result["sessionCopy"] = c;
    }
    if let Some(s) = session_report {
        result["sessionShare"] = s;
    }
    if let Some(s) = sync_report {
        result["sessionSync"] = s;
    }
    if let Some(r) = recovery_report {
        result["sessionRecovery"] = r;
    }
    if let Some(m) = memory_report {
        result["memoryMerge"] = m;
    }
    if let Some(e) = experts_report {
        result["expertsMerge"] = e;
    }
    if let Some(p) = persona_report {
        result["personaSync"] = p;
    }
    if let Some(m) = deleted_sync_report {
        result["deletedSync"] = m;
    }
    if let Some(m) = mirror_report {
        result["mirror"] = m;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    //! 编排层可注入纯函数的单测：切换全流程（关进程/写认证/启动）需真机验收，
    //! 这里覆盖「不可安全恢复必须暂停启动」「本次新留下的未完成写入必须暂停启动」
    //! 与「restart=false 拒绝写入意图」三条判定。

    use super::*;
    use crate::modules::session_link::{
        save_operation, OpPhase, OperationMember, RecoveryIssue, SidecarWarning,
        TemporaryFileIssue, OPERATION_VERSION,
    };

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb_switch_orchestrate_{}_{name}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn paths(&self) -> SessionPaths {
            SessionPaths {
                store_root: self.0.join("store"),
                data_root: self.0.join("data"),
                link_namespace: crate::modules::session::LinkNamespace::WorkBuddy,
                auth_file: self.0.join("auth.info"),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 测试用操作记录：只关心 id、阶段与失败原因。
    fn operation(id: &str, phase: OpPhase, error: Option<&str>, created_at: i64) -> Operation {
        Operation {
            version: OPERATION_VERSION,
            operation_id: id.to_string(),
            kind: "sync".to_string(),
            variant: WbVariant::Cn,
            source_variant: None,
            group_id: "g-1".to_string(),
            source: OperationMember {
                account_id: None,
                uid: "uid-a".to_string(),
                session_id: "sess-1".to_string(),
            },
            target: OperationMember {
                account_id: None,
                uid: "uid-b".to_string(),
                session_id: "sess-b".to_string(),
            },
            expected_content_digest: "digest".to_string(),
            expected_record_count: 1,
            phase,
            backup: None,
            lifecycle_version: None,
            cleanup_state: None,
            last_error: error.map(str::to_string),
            created_at,
            updated_at: created_at,
        }
    }

    fn report_with(retryable: bool) -> RecoveryReport {
        RecoveryReport {
            recovered: Vec::new(),
            abandoned: Vec::new(),
            temporary_files: Vec::new(),
            sidecar_warnings: Vec::new(),
            needs_recovery: vec![RecoveryIssue {
                operation_id: "op-1".to_string(),
                reason: "目标内容与操作记录不一致，已停止恢复".to_string(),
                retryable,
            }],
        }
    }

    /// 中间产物被改动/丢失（不可重试）→ 暂停启动；可重试的失败不阻断。
    #[test]
    fn recovery_blocks_startup_only_for_unrecoverable_issues() {
        assert!(recovery_blocks_startup(&report_with(false)));
        assert!(!recovery_blocks_startup(&report_with(true)));
        assert!(!recovery_blocks_startup(&RecoveryReport::default()));
        let blocking = recovery_blocking_detail(&report_with(false));
        assert!(blocking.contains("op-1"), "{blocking}");
        assert!(blocking.contains("已停止恢复"), "{blocking}");
        assert!(recovery_blocking_detail(&report_with(true)).is_empty());
    }

    /// 恢复报告投影：三类结果与不可重试标记都要能被前端看到。
    #[test]
    fn recovery_report_projection_carries_all_categories() {
        let mut report = report_with(false);
        report.recovered.push("op-2".to_string());
        report.abandoned.push("op-3".to_string());
        let value = recovery_report_json(&report);
        assert_eq!(value["recovered"], 1);
        assert_eq!(value["abandoned"], 1);
        assert_eq!(value["needsRecovery"][0]["operationId"], "op-1");
        assert_eq!(value["needsRecovery"][0]["retryable"], false);
        assert_eq!(value["temporaryFiles"], json!([]));
    }

    /// 只有临时备份残留时也不能当成「恢复什么都没做」：宿主必须把 sessionRecovery 带给前端。
    #[test]
    fn recovery_is_not_empty_when_only_temporary_files_remain() {
        let mut report = RecoveryReport::default();
        report
            .temporary_files
            .push(TemporaryFileIssue::cleanup_pending(
                "op-cleanup".to_string(),
                Some("sess-1".to_string()),
                Some("标题".to_string()),
                "临时目录删除失败：权限不足".to_string(),
            ));
        assert!(!report.is_empty());
        assert!(report.is_clean(), "待清理不得变成启动阻断");
        let value = recovery_report_json(&report);
        assert_eq!(value["temporaryFiles"][0]["operationId"], "op-cleanup");
        assert_eq!(value["temporaryFiles"][0]["state"], "cleanupPending");
        assert_eq!(value["temporaryFiles"][0]["sessionId"], "sess-1");
        assert_eq!(value["temporaryFiles"][0]["title"], "标题");
    }

    /// 附属目录失败属于非阻断提示：不得暂停启动，但必须出现在恢复报告里。
    ///
    /// 事实是字段已产出、却没有任何消费方——用户侧不可达，等价于没记。因此这里
    /// 钉住三件事：不阻断启动、仍算「有话说」、能穿过 IPC 到前端。
    #[test]
    fn sidecar_warnings_are_reported_without_blocking_startup() {
        let mut report = RecoveryReport::default();
        report.sidecar_warnings.push(SidecarWarning {
            operation_id: "op-sidecar".to_string(),
            session_id: "sess-2".to_string(),
            reason: "subagents/agent-x.jsonl：拒绝访问".to_string(),
        });
        assert!(
            !recovery_blocks_startup(&report),
            "附属目录失败绝不允许阻断启动"
        );
        assert!(report.is_clean(), "附属目录失败不得升级为需要人工处理");
        assert!(!report.is_empty(), "只有附属目录提示时也必须回传报告");
        let value = recovery_report_json(&report);
        assert_eq!(value["sidecarWarnings"][0]["operationId"], "op-sidecar");
        assert_eq!(value["sidecarWarnings"][0]["sessionId"], "sess-2");
        assert_eq!(
            value["sidecarWarnings"][0]["reason"],
            "subagents/agent-x.jsonl：拒绝访问"
        );
        // 不得混进阻断项：前端据此决定是 warning 还是 error。
        assert_eq!(value["needsRecovery"], json!([]));
    }

    /// 本次新留下的未完成写入必须被识别出来：历史残留不算在本次头上，已完成的也不算。
    #[test]
    fn newly_unfinished_writes_only_counts_this_switch() {
        let dir = TempDir::new("pending-writes");
        let paths = dir.paths();
        // 历史残留（切换开头已尝试恢复，可重试类不阻断）。
        save_operation(
            &paths,
            &operation("op-old", OpPhase::BodyWritten, Some("映射库暂不可用"), 1),
        )
        .unwrap();
        let before = pending_operation_ids(&paths, WbVariant::Cn);
        assert_eq!(before.len(), 1);

        // 本次复制/同步新产生一条未完成操作。
        save_operation(
            &paths,
            &operation(
                "op-new",
                OpPhase::DbWritten,
                Some("目标会话记录更新失败"),
                2,
            ),
        )
        .unwrap();
        let created = newly_unfinished_writes(
            &before,
            session_link::pending_operations(&paths, WbVariant::Cn),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].operation_id, "op-new");
        let detail = unfinished_writes_detail(&created);
        assert!(detail.contains("目标会话记录更新失败"), "{detail}");

        // 已完成的写入不算未完成；没有失败原因时退化为操作 id。
        save_operation(&paths, &operation("op-done", OpPhase::Completed, None, 3)).unwrap();
        let created = newly_unfinished_writes(
            &BTreeSet::new(),
            session_link::pending_operations(&paths, WbVariant::Cn),
        );
        assert_eq!(
            created
                .iter()
                .map(|operation| operation.operation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["op-old", "op-new"],
            "按创建时间排序，且不含已完成操作"
        );
        assert!(
            unfinished_writes_detail(&[operation("op-bare", OpPhase::Prepared, None, 4)])
                .contains("op-bare")
        );

        // 别的档位的未完成写入不算在本档位头上。
        let mut other = operation("op-ai", OpPhase::Prepared, Some("待恢复"), 5);
        other.variant = WbVariant::Ai;
        save_operation(&paths, &other).unwrap();
        assert!(!pending_operation_ids(&paths, WbVariant::Cn).contains("op-ai"));
    }

    /// restart=false 携带同步意图 → 与复制同样显式拒绝，且契约与成功路径同形。
    #[test]
    fn session_writes_without_restart_are_rejected_explicitly() {
        let (copy, sync) = reject_session_writes_without_restart(false, true);
        assert!(copy.is_none());
        let sync = sync.expect("携带同步意图必须给出拒绝报告");
        assert_eq!(sync["errors"][0]["error"], SYNC_WITHOUT_RESTART_MESSAGE);
        assert!(sync["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("restart=false"));
        assert!(sync["synced"].as_array().unwrap().is_empty());
        assert!(sync["skipped"].as_array().unwrap().is_empty());

        // 两种意图同时存在：各自拒绝，互不吞掉。
        let (copy, sync) = reject_session_writes_without_restart(true, true);
        assert_eq!(copy.unwrap()["error"], COPY_WITHOUT_RESTART_MESSAGE);
        assert_eq!(
            sync.unwrap()["errors"][0]["error"],
            SYNC_WITHOUT_RESTART_MESSAGE
        );

        // 都没有写入意图：不产生任何报告（保持原有语义）。
        let (copy, sync) = reject_session_writes_without_restart(false, false);
        assert!(copy.is_none() && sync.is_none());
    }

    // --- 批次②c：镜像接线（restart 拒绝 / 报告不阻断 / 还原持锁） ---------

    #[test]
    fn mirror_requires_restart() {
        assert_eq!(
            reject_mirror_without_restart(false, true).as_deref(),
            Some(MIRROR_WITHOUT_RESTART_MESSAGE)
        );
        assert!(reject_mirror_without_restart(true, true).is_none(), "重启时可执行");
        assert!(reject_mirror_without_restart(false, false).is_none(), "无镜像不拦");
        assert!(reject_mirror_without_restart(true, false).is_none());
    }

    // --- 批次②d：互斥矩阵入口断言（N3 / N4，必须在任何写入之前失败） -------

    /// 把 `WB_SWITCH_HOME` 指向临时目录并在 drop 时还原。
    ///
    /// `switch_account` 的第一件事是取认证锁（锁文件路径来自 HOME），单测绝不该碰真实
    /// `~/.wb-switch`，也不该因为本机残留锁而误判。
    struct HomeGuard(Option<std::ffi::OsString>);

    impl HomeGuard {
        fn new(root: &Path) -> Self {
            let previous = std::env::var_os(crate::modules::config::HOME_ENV_VAR);
            std::env::set_var(crate::modules::config::HOME_ENV_VAR, root);
            Self(previous)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(crate::modules::config::HOME_ENV_VAR, value),
                None => std::env::remove_var(crate::modules::config::HOME_ENV_VAR),
            }
        }
    }

    fn entry_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    /// 互斥判定只读 `is_some()` / `is_empty()`，计划内容不作数，给个最小非空形状即可。
    fn minimal_mirror_plan() -> mirror::MirrorPlan {
        mirror::MirrorPlan {
            mother_uid: String::new(),
            targets: Vec::new(),
            expected_digest: "digest".to_string(),
            preview_token: "token".to_string(),
        }
    }

    /// 环境变量是进程全局的：本文件里有三个用例会临时改写 `WB_SWITCH_HOME`（本组两条 +
    /// `restore_mirror_backup_reports_lock_conflict_when_held`），用一把测试专用互斥把本组
    /// 两条串行化，避免彼此把对方占用的锁读成「认证锁冲突」。
    static HOME_ENV_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_home_env() -> std::sync::MutexGuard<'static, ()> {
        HOME_ENV_SERIAL
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// 在临时 HOME 下跑一次 `switch_account`，返回 (错误文案, 会话备份根, 认证备份目录, 跑前条目数)。
    ///
    /// 三个路径全部由 `root` 直接拼出（= `HOME=root` 时 `~/.wb-switch` 的等价路径），
    /// **不读环境变量**：并行用例可能把 HOME 瞬时改走甚至移除，一旦用 `config::backup_dir()`
    /// 取值，断言目标会漂到真实用户目录上。
    fn switch_under_temp_home(
        root: &Path,
        copy_session_ids: &[String],
        mirror_plan: Option<mirror::MirrorPlan>,
        deleted_sync_plan: Option<mirror::DeletedSyncPlan>,
    ) -> (String, std::path::PathBuf, std::path::PathBuf, usize) {
        let stores = SessionPaths {
            store_root: root.join(".wb-switch"),
            data_root: root.join("data"),
            auth_file: root.join("auth.info"),
            link_namespace: crate::modules::session::LinkNamespace::WorkBuddy,
        };
        let transactions = crate::modules::session_backup::transactions_root(&stores, WbVariant::Cn);
        let auth_backup_dir = stores.backup_root();
        let before = entry_count(&transactions);

        // 调用方持有 `HOME_ENV_SERIAL`：`WB_SWITCH_HOME` 在本次调用期间不会被别的用例改写，
        // 认证锁因此必定空闲（锁路径取自 HOME）。
        let _home = HomeGuard::new(root);
        // 互斥判定在账号解析之前，故意用不存在的 id：证明失败先于任何读账号/备份/写入。
        let error = switch_account(
            None,
            "no-such-account",
            true,
            false,
            copy_session_ids,
            &[],
            None,
            None,
            false,
            false,
            memory::PersonaMode::Safe,
            mirror_plan,
            deleted_sync_plan,
        )
        .unwrap_err();
        (error, transactions, auth_backup_dir, before)
    }

    #[test]
    fn switch_account_rejects_mirror_with_deleted_sync_plan() {
        let _serial = lock_home_env();
        let dir = TempDir::new("mutex-deleted-sync");
        let (error, transactions, auth_backup_dir, before) = switch_under_temp_home(
            &dir.0,
            &[],
            Some(minimal_mirror_plan()),
            Some(mirror::DeletedSyncPlan {
                targets: Vec::new(),
                expected_digest: "digest".to_string(),
            }),
        );

        assert!(
            error.contains("互斥"),
            "镜像与「同步已删除会话」必须互斥：{error}"
        );
        assert_eq!(
            entry_count(&transactions),
            before,
            "互斥必须在会话备份目录生成之前失败"
        );
        assert!(!transactions.exists(), "会话备份根不得被创建");
        assert!(!auth_backup_dir.exists(), "认证备份目录不得被创建");
    }

    #[test]
    fn switch_account_rejects_mirror_with_copy_session_ids() {
        let _serial = lock_home_env();
        let dir = TempDir::new("mutex-copy");
        let (error, transactions, auth_backup_dir, before) = switch_under_temp_home(
            &dir.0,
            &["s-1".to_string()],
            Some(minimal_mirror_plan()),
            None,
        );

        assert!(error.contains("互斥"), "镜像与复制会话必须互斥：{error}");
        assert_eq!(
            entry_count(&transactions),
            before,
            "互斥必须在会话备份目录生成之前失败"
        );
        assert!(!transactions.exists(), "会话备份根不得被创建");
        assert!(!auth_backup_dir.exists(), "认证备份目录不得被创建");
    }

    #[test]
    fn mirror_failure_is_reported_not_blocking() {
        // 失败：投影为 MirrorReport 且不进 Err（切号继续），错误落 errors。
        let failed = mirror_report_json(Err("boom".to_string()));
        assert_eq!(failed["deletedSessions"], 0);
        assert_eq!(failed["errors"][0]["error"], "boom");
        // 成功：字段原样透传。
        let ok = mirror_report_json(Ok(mirror::MirrorReport {
            deleted_sessions: 3,
            copied_sessions: 0,
            restored_copies: 0,
            backup: Some("/x".to_string()),
            needs_recovery: false,
            experts_updated: false,
            experts_restored: false,
            warnings: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
        }));
        assert_eq!(ok["deletedSessions"], 3);
        assert_eq!(ok["backup"], "/x");
        assert!(ok["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn restore_mirror_backup_reports_lock_conflict_when_held() {
        // 写端点必须自己持 try_auth：锁被占用时返回冲突错误，不触碰任何数据。
        // 本用例两次 `try_auth` 必须看到同一个 HOME，否则「持锁→冲突」的因果断链；
        // 与同样改写 `WB_SWITCH_HOME` 的互斥用例串行化（只加锁，不改任何断言）。
        let _serial = lock_home_env();
        let prev = std::env::var_os(crate::modules::config::HOME_ENV_VAR);
        let dir = std::env::temp_dir().join(format!(
            "wb-mirror-restore-lock-{}-{}",
            std::process::id(),
            crate::modules::config::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(crate::modules::config::HOME_ENV_VAR, &dir);

        let guard = crate::modules::auth_transaction::try_auth().unwrap();
        let backup = dir
            .join("session-transactions")
            .join("cn")
            .join("no-such-uuid");
        let result = restore_mirror_backup(&backup);
        drop(guard);

        match prev {
            Some(value) => std::env::set_var(crate::modules::config::HOME_ENV_VAR, value),
            None => std::env::remove_var(crate::modules::config::HOME_ENV_VAR),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            matches!(result, Err(ref error) if error.starts_with(crate::modules::auth_transaction::BUSY_PREFIX)),
            "持锁时应返回锁冲突：{result:?}"
        );
    }
}
