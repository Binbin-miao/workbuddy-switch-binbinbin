//! 整份镜像切换（Wave 2 批次①）：只读预览 + 契约冻结，**零会话/记忆数据写入**。
//!
//! 语义：以「当前登录账号」为母本，切号时把目标账号的会话集合整份对齐成 100% 一致。
//! 本批次只交付只读预览端点与冻结契约；软删、复制、MEMORY.md 覆盖、还原端点属于批次②③，
//! 此文件绝不写 `sessions` 表、不置 `deleted_at`、不碰正文与 MEMORY.md。
//!
//! 两条安全不变量：
//! - 母本 uid 一律取自该档位登录态文件（`current_user_uid_at`），**绝不接受前端传入**；
//!   前端 `motherUid` 参数仅用于界面回显，宿主层必须忽略。
//! - 目标清单只从 `user_id = 目标 uid` 且 `deleted_at IS NULL` 的行生成（含未建组的私有
//!   会话，逐条不折叠）；身份按 `LinkMember.uid` 精确匹配，绝不按标题全局去重（11-06 误删教训）。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::modules::auth_file;
use crate::modules::config::{atomic_write, now_ms, wb_data_root};
use crate::modules::experts;
use crate::modules::memory;
use crate::modules::session::{
    self, current_user_uid_at, open_db, table_exists, SessionPaths, SyncSelection,
    REASON_PREVIEW_STALE,
};
use crate::modules::session_backup;
use crate::modules::session_link::{
    self, full_digest_of, LinkGroup, MemberState, PreviewBinding, PreviewMemberBinding, PreviewToken,
    SyncMode, SyncVerdict,
};
use crate::modules::variant::WbVariant;

/// 镜像预览凭据的 group_id 哨兵：与任何真实 LinkGroup.id 都不同，仅供 `verify_preview` 承载绑定。
pub const MIRROR_GROUP_ID: &str = "__wb_mirror__";

// ---------------------------------------------------------------------------
// 只读契约类型（serde camelCase 与 src/lib/types.ts 逐字对齐）
// ---------------------------------------------------------------------------

/// 逐条列出的「目标账号将被软删」会话；清单不折叠。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorTarget {
    pub session_id: String,
    pub title: String,
    /// 目标会话归属的 user_id；清单只从目标 uid 的行生成。
    pub user_id: String,
    /// 所属关联组 id；未建立 LinkGroup 的私有会话为空串。
    pub group_id: String,
    /// 预览时目标会话行的 updated_at，落盘前现场复核。
    pub updated_at: i64,
    /// 预览时目标会话的规范化摘要，落盘前逐条重算，不符零写入。
    pub full_digest: String,
    /// 会话级软删；镜像不引入记录级删除。
    pub kind: String,
}

/// 独立只读预览结果：凭证绑定 motherUid（当前登录账号），变化一律 REASON_PREVIEW_STALE。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPreview {
    pub mother_uid: String,
    pub mother_label: String,
    pub preview_token: String,
    /// 待删会话集合规范化摘要（sessionId 有序 + 各自 fullDigest + updatedAt + deletedAt 均为 NULL）。
    pub expected_digest: String,
    /// 逐条待软删清单，界面不得折叠。
    pub targets: Vec<MirrorTarget>,
    pub session_count: usize,
    /// 将从母本复制的会话数（母本有、目标没有，按 LinkGroup 判重）。
    pub copied_count: usize,
    /// 是否将把目标账号的专家团清单整份覆盖为母本的（母本无清单 / 集合已一致时为 false）。
    pub experts_pending: bool,
    /// 母本当前的专家团名字，供界面逐条复核；母本无清单时为空。
    #[serde(default)]
    pub experts_names: Vec<String>,
    /// 跳过专家团覆盖的原因（如「母本未添加专家团」或清单不可信）；无需跳过时为 null。
    #[serde(default)]
    pub experts_note: Option<String>,
    /// 执行前预登记的备份目录（预览阶段恒 null）。
    pub backup_dir: Option<String>,
    /// 组层面的合并缺口，供界面折叠展示；只读派生，不参与 `expectedDigest` 与执行分支。
    /// 旧后端不返回该键时按空清单处理。
    #[serde(default)]
    pub unmerged: Vec<MirrorUnmerged>,
}

/// 冻结的执行计划：作为 switchAccount 的独立参数并入 switch_account 主干（批次②③接线）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorPlan {
    /// 前端已不再下发母本（批次②a）；`#[serde(default)]` 让字段缺省合法，仅兼容旧载荷。
    /// 执行时**恒以登录态重取母本**（`current_user_uid_at`），本字段被忽略，绝不作作用域来源。
    #[serde(default)]
    pub mother_uid: String,
    pub targets: Vec<MirrorTarget>,
    pub expected_digest: String,
    pub preview_token: String,
}

/// 镜像切换执行报告（批次②③产出）：备份路径明文、中间态可还原、绝不伪装成成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorSkipped {
    pub session_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorError {
    pub session_id: String,
    pub error: String,
}

/// 自动同步成功的一个关联组：「母本领先、目标落后」的副本已补齐。
///
/// `mode` 只可能是 `fastForward`（目标为母本严格有序前缀，零内容损失）或
/// `overwrite`（分叉，按 P1 用母本整份覆盖，目标独有内容丢失）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorSyncedGroup {
    pub group_id: String,
    /// 组展示名（取母本会话标题）；读不到时为 `None`。
    pub title: Option<String>,
    pub mode: String,
    /// 是否以母本覆盖了目标（P1 的透明度代价：界面据此逐条列出被覆盖组）。
    pub overwritten: bool,
    /// 覆盖前目标正文的记录数。
    pub target_records_before: usize,
    /// 本次追加（快进）或覆盖后相比覆盖前新增的记录数。
    pub added_records: usize,
}

/// 自动同步失败的一个关联组：不阻断切号，只逐条上报。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorSyncError {
    /// 失败的组；整体失败（预览/加锁不可用）时为空串。
    pub group_id: String,
    pub title: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorReport {
    pub deleted_sessions: usize,
    pub copied_sessions: usize,
    /// 还原时：把本次「复制新增」的会话按软删撤销的条数（rollback 时填充）。
    #[serde(default)]
    pub restored_copies: usize,
    pub backup: Option<String>,
    pub needs_recovery: bool,
    /// 本次是否把目标账号的专家团清单整份覆盖为母本的（与 `MEMORY.md` 同形的独立步骤）。
    #[serde(default)]
    pub experts_updated: bool,
    /// 还原时是否把专家团清单写回镜像前原文；老备份无该步骤时为 false。
    #[serde(default)]
    pub experts_restored: bool,
    pub warnings: Vec<String>,
    pub skipped: Vec<MirrorSkipped>,
    pub errors: Vec<MirrorError>,
    /// 本次执行后仍缺的组层面合并缺口（只读观测，不参与任何执行分支）。
    /// 还原与删除同步没有该语义，恒为空清单；`#[serde(default)]` 兼容老载荷与老前端。
    #[serde(default)]
    pub unmerged: Vec<MirrorUnmerged>,
    /// 本次自动同步成与母本一致的组数（B1/B2 交给复制，不计入这里）。
    #[serde(default)]
    pub synced_sessions: usize,
    /// 其中按 P1 用母本整份覆盖目标（diverge）的组数。
    #[serde(default)]
    pub overwritten_sessions: usize,
    /// 逐组同步明细，供界面列出被覆盖的组名。
    #[serde(default)]
    pub sync_details: Vec<MirrorSyncedGroup>,
    /// 逐组同步失败；失败不阻断切号，与复制侧 `errors` 互不污染。
    #[serde(default)]
    pub sync_errors: Vec<MirrorSyncError>,
}

/// 组层面的合并缺口（只读观测）：母本在该组有存活 `Active` 成员，而目标账号在该组
/// 没有任何 `Active` 成员。
///
/// 语义只有一条定义，预览采样时就是「待合并清单」，执行后再采样就是「仍缺清单」。
/// 它**不参与** `expectedDigest`、token 绑定与任何执行分支，纯展示用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorUnmerged {
    pub group_id: String,
    /// 母本会话行的展示标题；母本行已不可读时为 `None`。
    pub title: Option<String>,
    pub mother_session_id: String,
    /// 该组里缺 `Active` 成员的账号（当前恒为单个目标 uid）。
    pub missing_uids: Vec<String>,
}

// ---------------------------------------------------------------------------
// 现场绑定与 STALE 六条（纯函数，零 IO，供批次②③执行前逐条复核）
// ---------------------------------------------------------------------------

/// 镜像预览现场快照。执行前用同一构造函数重算 `live`，两者逐字段比对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorBinding {
    /// 母本 uid（当前登录账号）。
    pub mother_uid: String,
    /// 目标 uid。
    pub target_uid: String,
    /// 母本侧指纹（`mother_mirror_fingerprint`）：组内母本成员集合 + 母本会话存活状态。
    pub mother_active_fingerprint: String,
    /// 待删会话集合规范化摘要。
    pub expected_digest: String,
    /// 逐条待删会话现场（sessionId 有序）。
    pub targets: Vec<MirrorTarget>,
    /// 预览涉及的关联组 id（排序去重；私有会话不占）。
    pub group_ids: Vec<String>,
    /// 现场仍能查到的关联组 id。
    pub found_group_ids: Vec<String>,
    /// 现场仍含母本 uid active 成员的组 id。
    pub groups_with_mother: Vec<String>,
    /// 现场待删会话是否仍全部 deleted_at IS NULL。
    pub all_targets_active: bool,
}

/// 一条失配原因：代码恒为 `REASON_PREVIEW_STALE`，detail 给出可读解释。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorStale {
    pub reason_code: String,
    pub detail: String,
}

fn stale(detail: &str) -> MirrorStale {
    MirrorStale {
        reason_code: REASON_PREVIEW_STALE.to_string(),
        detail: detail.to_string(),
    }
}

/// 母本 active 成员集合指纹：任一组成员替换/新增/移除都会改变摘要。
pub fn mother_active_fingerprint(groups: &[LinkGroup], mother_uid: &str) -> String {
    let mut lines: Vec<String> = groups
        .iter()
        .flat_map(|group| {
            group
                .members
                .iter()
                .filter(|member| member.uid == mother_uid && member.state == MemberState::Active)
                .map(move |member| {
                    format!(
                        "{}|{}|{}|{}",
                        group.id,
                        member.uid,
                        member.session_id,
                        member.state.as_str()
                    )
                })
        })
        .collect();
    lines.sort();
    lines.dedup();
    full_digest_of(lines.join("\n").as_bytes())
}

/// 母本侧镜像指纹：既有维度「组内母本 `Active` 成员集合」与下面的「母本会话存活状态」
/// 指纹合并，作为预览凭据的 `source.normalized_digest`。
///
/// 合并值恒不等于 0.1.81 的单一 digest → 升级前签发的凭据在升级后首次预览即**自然失配**
/// （Spec §2 要求：不做兼容放行）。
pub fn mother_mirror_fingerprint(
    groups: &[LinkGroup],
    mother_uid: &str,
    mother_states: &HashMap<String, Option<i64>>,
) -> String {
    format!(
        "{}|{}",
        mother_active_fingerprint(groups, mother_uid),
        mother_session_state_fingerprint(groups, mother_uid, mother_states)
    )
}

/// 母本会话存活状态指纹（N2）：覆盖 `mother_active_fingerprint` 看不见的两类母本侧变化——
/// 1. 母本**无组**存活会话的新增／删除／事后建组（I4，Bug B 的判据来源）；
/// 2. 母本在组内成员由 `Alive` 转为 `Gone`（Bug A 的触发点：组还在、行已被软删）。
///
/// 只覆盖「存在性 + 存活状态 + 成员集合」，不含标题/`updated_at` 等内容字段：用户只是继续
/// 对话不该让预览失效，而母本会话增删必须失效。
pub fn mother_session_state_fingerprint(
    groups: &[LinkGroup],
    mother_uid: &str,
    mother_states: &HashMap<String, Option<i64>>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut mother_active: HashSet<&str> = HashSet::new();
    for group in groups {
        for member in group
            .members
            .iter()
            .filter(|member| member.uid == mother_uid)
        {
            if member.state == MemberState::Active {
                mother_active.insert(member.session_id.as_str());
            }
            let liveness = if is_alive(mother_states, &member.session_id) {
                "alive"
            } else {
                "gone"
            };
            lines.push(format!(
                "member|{}|{}|{}|{}",
                group.id,
                member.session_id,
                member.state.as_str(),
                liveness
            ));
        }
    }
    for (session_id, deleted_at) in mother_states {
        if deleted_at.is_some() || mother_active.contains(session_id.as_str()) {
            continue;
        }
        lines.push(format!("unpaired|{session_id}"));
    }
    lines.sort();
    lines.dedup();
    full_digest_of(lines.join("\n").as_bytes())
}

/// 待删会话集合规范化摘要：sessionId 有序 + 各自 fullDigest + updatedAt + deletedAt=NULL。
/// 顺序无关——同一集合任意排列得到同一摘要。
pub fn expected_digest_of(targets: &[MirrorTarget]) -> String {
    let mut lines: Vec<String> = targets
        .iter()
        .map(|target| {
            format!(
                "{}|{}|{}|NULL",
                target.session_id, target.full_digest, target.updated_at
            )
        })
        .collect();
    lines.sort();
    full_digest_of(lines.join("\n").as_bytes())
}

/// 从预览现场构造绑定。`found_group_ids` / `groups_with_mother` / `all_targets_active`
/// 由调用方按现场实况填入（预览时前两者恒等于 group_ids、后者恒 true）。
#[allow(clippy::too_many_arguments)]
pub fn build_mirror_binding(
    mother_uid: &str,
    target_uid: &str,
    groups: &[LinkGroup],
    mother_states: &HashMap<String, Option<i64>>,
    targets: &[MirrorTarget],
    found_group_ids: Vec<String>,
    groups_with_mother: Vec<String>,
    all_targets_active: bool,
) -> MirrorBinding {
    let mut group_ids: Vec<String> = targets
        .iter()
        .map(|target| target.group_id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    group_ids.sort();
    group_ids.dedup();
    MirrorBinding {
        mother_uid: mother_uid.to_string(),
        target_uid: target_uid.to_string(),
        mother_active_fingerprint: mother_mirror_fingerprint(groups, mother_uid, mother_states),
        expected_digest: expected_digest_of(targets),
        targets: targets.to_vec(),
        group_ids,
        found_group_ids,
        groups_with_mother,
        all_targets_active,
    }
}

fn same_targets(expected: &[MirrorTarget], live: &[MirrorTarget]) -> bool {
    let key = |targets: &[MirrorTarget]| -> Vec<(String, String, i64)> {
        let mut rows: Vec<(String, String, i64)> = targets
            .iter()
            .map(|t| (t.session_id.clone(), t.full_digest.clone(), t.updated_at))
            .collect();
        rows.sort();
        rows
    };
    key(expected) == key(live)
}

/// 核对预览绑定与现场绑定，返回六类失配（空表示一致，可继续）。
///
/// 六条与设计一一对应：
/// 1. motherUid 变化；
/// 2. 母本在任一组 active 成员集合变化；
/// 3. 待删会话 fullDigest / updated_at 变化；
/// 4. 待删会话软删状态变化；
/// 5. group_id 查不到；
/// 6. 母本 uid 不在该组成员中。
pub fn verify_mirror_binding(expected: &MirrorBinding, live: &MirrorBinding) -> Vec<MirrorStale> {
    let mut result = Vec::new();
    if expected.mother_uid != live.mother_uid {
        result.push(stale("母本账号已变化，请重新预览"));
    }
    if expected.mother_active_fingerprint != live.mother_active_fingerprint {
        result.push(stale("母本账号的关联会话集合已变化，请重新预览"));
    }
    if !same_targets(&expected.targets, &live.targets) {
        result.push(stale("待删会话的内容或更新时间已变化，请重新预览"));
    }
    if !live.all_targets_active {
        result.push(stale("待删会话已被软删或移除，请重新预览"));
    }
    if expected
        .group_ids
        .iter()
        .any(|id| !live.found_group_ids.contains(id))
    {
        result.push(stale("会话关联组已不存在，请重新预览"));
    }
    if expected
        .group_ids
        .iter()
        .any(|id| !live.groups_with_mother.contains(id))
    {
        result.push(stale("母本账号已不在该组成员中，请重新预览"));
    }
    result
}

// ---------------------------------------------------------------------------
// 只读清单生成（纯函数，红测直接覆盖隔离 / 判重 / 字段齐备）
// ---------------------------------------------------------------------------

/// 单条会话行（只读查询结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorRow {
    pub session_id: String,
    pub user_id: String,
    pub title: Option<String>,
    pub custom_title: Option<String>,
    pub updated_at: i64,
    /// 最近活跃时刻：`COALESCE(last_activity_at, updated_at)`，缺列时退回 `updated_at`。
    /// 只给「落后副本预筛」用，**绝不进** [`session_row_digest`]（否则凭据会因时间戳变化失配）。
    pub activity_at: i64,
    /// Some = 已软删，绝不进清单；None = deleted_at IS NULL。
    pub deleted_at: Option<i64>,
}

fn nonblank(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn display_title(title: Option<String>, custom_title: Option<String>) -> String {
    nonblank(custom_title)
        .or_else(|| nonblank(title))
        .unwrap_or_else(|| "(无标题)".to_string())
}

/// 会话行规范化摘要：预览与执行前重算用同一口径，不符零写入。
pub fn session_row_digest(row: &MirrorRow) -> String {
    let deleted = row
        .deleted_at
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".to_string());
    let raw = format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        row.session_id,
        row.user_id,
        row.title.clone().unwrap_or_default(),
        row.custom_title.clone().unwrap_or_default(),
        row.updated_at,
        deleted
    );
    full_digest_of(raw.as_bytes())
}

/// 会话所属关联组：`LinkMember(uid, session_id, state=Active)` 精确匹配；私有会话空串。
pub fn group_id_for(groups: &[LinkGroup], uid: &str, session_id: &str) -> String {
    let mut ids: Vec<String> = groups
        .iter()
        .filter(|group| {
            group.members.iter().any(|member| {
                member.uid == uid
                    && member.session_id == session_id
                    && member.state == MemberState::Active
            })
        })
        .map(|group| group.id.clone())
        .collect();
    ids.sort();
    ids.into_iter().next().unwrap_or_default()
}

/// 母本是否仍拥有该组的对话（`KEEP` 第三类判据，I1/I2）。
///
/// `M(g) = MoAct(g) ∪ MoSup(g)`：`Active` 与 `Superseded` 一视同仁，`Stale` 不计入
/// （它描述「已判定失效」，不是母本仍持有的证据）。`M(g)` 为空说明母本在该组没有任何
/// 成员 → 无从证明归属，保留（fail-closed）；`M(g)` 内存在任一 `Alive` 成员即视为仍拥有。
fn mother_still_owns_group(
    groups: &[LinkGroup],
    group_id: &str,
    mother_uid: &str,
    mother_states: &HashMap<String, Option<i64>>,
) -> bool {
    let Some(group) = groups.iter().find(|group| group.id == group_id) else {
        return true;
    };
    let mother_members: Vec<&session_link::LinkMember> = group
        .members
        .iter()
        .filter(|member| member.uid == mother_uid && member.state != MemberState::Stale)
        .collect();
    if mother_members.is_empty() {
        return true;
    }
    mother_members
        .iter()
        .any(|member| is_alive(mother_states, &member.session_id))
}

/// 从只读行生成逐条待软删清单：`user_id` 隔离（母本 uid 不进）+ 仅未软删行 + 字段齐备。
///
/// 判据见 Spec §1.4（只跟着母本删）：
/// - `KEEP`：目标行无组（I5）／组内无母本任何成员（目标独有）／组内存在 `Alive` 母本成员
///   （I1/I2 正向：母本仍持有这段对话，不论该成员是 `Active` 还是 `Superseded`）。
/// - `DELETE`：组内母本成员集 `M(g)` 非空且**全部** `Gone`（I1：全部 gone 才算 gone）。
///
/// 纯函数零 IO：母本状态由调用方读一次后传入，函数内绝不读盘/读库。
pub fn mirror_targets_from_rows(
    rows: &[MirrorRow],
    target_uid: &str,
    mother_uid: &str,
    groups: &[LinkGroup],
    mother_states: &HashMap<String, Option<i64>>,
) -> Vec<MirrorTarget> {
    let mut targets: Vec<MirrorTarget> = rows
        .iter()
        .filter(|row| row.user_id == target_uid && row.deleted_at.is_none())
        .filter(|row| !row.session_id.is_empty())
        .filter_map(|row| {
            let group_id = group_id_for(groups, target_uid, &row.session_id);
            // I5：目标行无组 → 无从证明其归属母本，一律保留。
            if group_id.is_empty() {
                return None;
            }
            // I1/I2：组内仍有存活母本成员（或组内根本没有母本成员）→ 保留。
            if mother_still_owns_group(groups, &group_id, mother_uid, mother_states) {
                return None;
            }
            Some(MirrorTarget {
                session_id: row.session_id.clone(),
                title: display_title(row.title.clone(), row.custom_title.clone()),
                user_id: row.user_id.clone(),
                group_id,
                updated_at: row.updated_at,
                full_digest: session_row_digest(row),
                kind: "session".to_string(),
            })
        })
        .collect();
    targets.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    targets
}

/// 母本有、目标没有的会话数：`missing_mirror_session_ids` 的薄封装。
///
/// 预览的「将复制 N 条」与执行侧实际复制清单必须**同源同口径**——两处各算一遍必然漂移，
/// 执行前的 `record_count` 复核会因此恒定 STALE（反例 E）。
pub fn count_mirror_copies(
    groups: &[LinkGroup],
    mother_uid: &str,
    target_uid: &str,
    mother_states: &HashMap<String, Option<i64>>,
    target_alive_ids: &HashSet<String>,
) -> usize {
    missing_mirror_session_ids(groups, mother_uid, target_uid, mother_states, target_alive_ids).len()
}

/// 目标账号缺失的母本会话 id 清单（Spec §1.4 `MISSING` 三段式），去重排序。
///
/// 1. 组内缺失：`MoAct(g) ≠ ∅` 且目标在该组无 `Active` 成员 → 取该组内仍 `Alive` 的母本成员；
/// 2. 母本**无组**存活会话（I4，Bug B）：母本存活行里不出现在任何组 `MoAct` 的会话——
///    只遍历组永远发现不了它们，用户正在用的那条就这样丢了；
/// 3. 减去目标已存活存在的 id（P6：防重复复制）。
///
/// 身份一律以 `uid + session_id` 精确匹配，绝不按标题去重（反例 B）。纯函数零 IO。
pub fn missing_mirror_session_ids(
    groups: &[LinkGroup],
    mother_uid: &str,
    target_uid: &str,
    mother_states: &HashMap<String, Option<i64>>,
    target_alive_ids: &HashSet<String>,
) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut linked_by_mother: HashSet<&str> = HashSet::new();
    for group in groups {
        let mother_active: Vec<&session_link::LinkMember> = group
            .members
            .iter()
            .filter(|member| member.uid == mother_uid && member.state == MemberState::Active)
            .collect();
        for member in &mother_active {
            linked_by_mother.insert(member.session_id.as_str());
        }
        if mother_active.is_empty() {
            continue;
        }
        // 语义是「目标是否**已登记**为 Active 成员」，不是「目标的副本是否还在」：
        // 副本被用户删掉也照样跳过，尊重删除（与 `session.rs` 的 `member_is_valid` 不同口径）。
        let target_registered = group
            .members
            .iter()
            .any(|member| member.uid == target_uid && member.state == MemberState::Active);
        if target_registered {
            continue;
        }
        ids.extend(
            mother_active
                .iter()
                .filter(|member| is_alive(mother_states, &member.session_id))
                .map(|member| member.session_id.clone()),
        );
    }
    // I4：母本存活、且没有出现在任何组 `MoAct` 里的会话，只能按 id 直接复制。
    for (session_id, deleted_at) in mother_states {
        if deleted_at.is_some() || linked_by_mother.contains(session_id.as_str()) {
            continue;
        }
        ids.push(session_id.clone());
    }
    ids.retain(|session_id| !session_id.is_empty() && !target_alive_ids.contains(session_id));
    ids.sort();
    ids.dedup();
    ids
}

/// 组层面的合并缺口清单（只读派生，零 IO）。
///
/// 入选条件：母本在该组有 `Active` **且存活**的成员，而目标账号在该组**没有任何**
/// `Active` 成员。两条排除规则按已拍板语义写死：
/// - 目标已有 `Active` 成员（哪怕它的会话已被删）→ 不入选：对话已属于目标账号，
///   「不补，尊重删除」；
/// - 母本成员全部 superseded → 不入选：没有可作副本源的存活母本。
///
/// 同一输入重复调用逐字相同（按 `group_id` → `mother_session_id` 排序后去重）。
pub fn unmerged_groups(
    groups: &[LinkGroup],
    mother_uid: &str,
    target_uid: &str,
    mother_states: &HashMap<String, Option<i64>>,
    mother_titles: &HashMap<String, String>,
) -> Vec<MirrorUnmerged> {
    let mut unmerged: Vec<MirrorUnmerged> = Vec::new();
    for group in groups {
        let target_registered = group
            .members
            .iter()
            .any(|member| member.uid == target_uid && member.state == MemberState::Active);
        if target_registered {
            continue;
        }
        for member in &group.members {
            if member.uid != mother_uid || member.state != MemberState::Active {
                continue;
            }
            if !is_alive(mother_states, &member.session_id) {
                continue;
            }
            unmerged.push(MirrorUnmerged {
                group_id: group.id.clone(),
                title: mother_titles.get(&member.session_id).cloned(),
                mother_session_id: member.session_id.clone(),
                missing_uids: vec![target_uid.to_string()],
            });
        }
    }
    unmerged.sort_by(|left, right| {
        left.group_id
            .cmp(&right.group_id)
            .then_with(|| left.mother_session_id.cmp(&right.mother_session_id))
    });
    unmerged.dedup();
    unmerged
}

// ---------------------------------------------------------------------------
// 只读预览端点（唯一 IO 编排；read_only 打开数据库，零会话数据写入）
// ---------------------------------------------------------------------------

fn account_uid_of(acc: &Value) -> String {
    acc.get("uid")
        .and_then(|value| value.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 登录态展示名；取不到时回退 uid。
pub fn account_label(auth: &Value, uid_fallback: &str) -> String {
    let account = auth.get("account").unwrap_or(auth);
    for key in ["label", "nickname", "email", "name"] {
        if let Some(value) = account.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return value.trim().to_string();
            }
        }
    }
    uid_fallback.to_string()
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(iter) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    // 先收进 Vec 释放 `stmt` 借用，避免尾部临时值析构晚于 stmt（E0597）。
    let names: Vec<String> = iter.flatten().collect();
    names.iter().any(|name| name == column)
}

/// 只读查询目标账号全部 `deleted_at IS NULL` 会话（含私有会话，逐条不折叠）。
fn read_target_rows(conn: &Connection, target_uid: &str) -> Result<Vec<MirrorRow>, String> {
    if !table_exists(conn, "sessions") {
        return Ok(Vec::new());
    }
    let custom_expr = if column_exists(conn, "sessions", "custom_title") {
        "custom_title"
    } else {
        "NULL"
    };
    let deleted_filter = if column_exists(conn, "sessions", "deleted_at") {
        " AND deleted_at IS NULL"
    } else {
        ""
    };
    let sql = format!(
        "SELECT id, user_id, title, {custom_expr}, updated_at, {} FROM sessions \
         WHERE user_id = ?1{deleted_filter} ORDER BY id",
        activity_expr(conn)
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| format!("镜像预览：会话清单查询失败：{error}"))?;
    let mapped = stmt
        .query_map([target_uid], |row| {
            Ok(MirrorRow {
                session_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                user_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                title: row.get::<_, Option<String>>(2)?,
                custom_title: row.get::<_, Option<String>>(3)?,
                updated_at: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                activity_at: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                deleted_at: None, // SQL 已过滤为 IS NULL
            })
        })
        .map_err(|error| format!("镜像预览：会话清单读取失败：{error}"))?;
    let mut rows = Vec::new();
    for row in mapped {
        rows.push(row.map_err(|error| format!("镜像预览：会话行读取失败：{error}"))?);
    }
    Ok(rows)
}

/// 最近活跃时刻的 SQL 表达式：`last_activity_at` 列存在时取 `COALESCE(它, updated_at)`。
///
/// 老库没有该列（`session.rs` 用 `ALTER TABLE` 追加），缺列时退回 `updated_at`，与列表
/// 排序键 `COALESCE(last_activity_at, created_at)` 同源同口径。
fn activity_expr(conn: &Connection) -> &'static str {
    if column_exists(conn, "sessions", "last_activity_at") {
        "COALESCE(last_activity_at, updated_at)"
    } else {
        "updated_at"
    }
}

/// 只读采样某账号**全部**会话行的存活状态，**不过滤** `deleted_at`。
///
/// 镜像判据要区分三种事实：行存在且未删（`Alive`）／行存在但已删（`Gone`）／行物理不存在
/// （`Gone`）——按 Spec §1.2 后两者等价，因此快照只登记「存在」的行，查不到 id 即视为物理
/// 不存在。`deleted_at` 列不存在（旧库）时全部映射为 `None`（按 `Alive` 处理，fail-closed：
/// 宁可少删，Z4）。空 `session_id` 不进快照（Z2：不得产生空串复制或空串软删）。
fn read_session_states(
    conn: &Connection,
    uid: &str,
) -> Result<HashMap<String, Option<i64>>, String> {
    if !table_exists(conn, "sessions") {
        return Ok(HashMap::new());
    }
    let deleted_expr = if column_exists(conn, "sessions", "deleted_at") {
        "deleted_at"
    } else {
        "NULL"
    };
    let sql = format!("SELECT id, {deleted_expr} FROM sessions WHERE user_id = ?1 ORDER BY id");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| format!("镜像预览：母本会话状态查询失败：{error}"))?;
    let mapped = stmt
        .query_map([uid], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<i64>>(1)?,
            ))
        })
        .map_err(|error| format!("镜像预览：母本会话状态读取失败：{error}"))?;
    let mut states: HashMap<String, Option<i64>> = HashMap::new();
    for row in mapped {
        let (session_id, deleted_at) =
            row.map_err(|error| format!("镜像预览：母本会话状态行读取失败：{error}"))?;
        if session_id.is_empty() {
            continue;
        }
        states.insert(session_id, deleted_at);
    }
    Ok(states)
}

/// `Alive(m)`（Spec §1.2）：母本 `sessions` 行存在且 `deleted_at IS NULL`。
/// 不在快照里 = 物理不存在 = `Gone`。
fn is_alive(mother_states: &HashMap<String, Option<i64>>, session_id: &str) -> bool {
    mother_states
        .get(session_id)
        .map(Option::is_none)
        .unwrap_or(false)
}

/// 存活行 id 集：`MISSING` 第三段「减去目标已存活存在」的去重依据。
fn alive_ids_of(rows: &[MirrorRow]) -> HashSet<String> {
    rows.iter()
        .map(|row| row.session_id.clone())
        .filter(|session_id| !session_id.is_empty())
        .collect()
}

fn load_groups(paths: &SessionPaths) -> Vec<LinkGroup> {
    match session_link::load_store(paths) {
        session_link::StoreState::Ready(store) => store.groups,
        _ => Vec::new(),
    }
}

/// 构造镜像预览凭据绑定：复用既有 `PreviewBinding`（`mother_uid` 绑定母本，
/// `group_fingerprint` 绑定 expectedDigest），执行时 `load_preview_token` + `verify_preview` 复核。
///
/// `source.normalized_digest` 用 `mother_mirror_fingerprint`（含无组存活会话与 `Gone` 状态），
/// `source.record_count` 用与执行侧同源的 `copied_count`——母本侧变化或缺失集合变化都会失配。
#[allow(clippy::too_many_arguments)]
fn build_mirror_token_binding(
    variant: WbVariant,
    mother_uid: &str,
    target_uid: &str,
    expected_digest: &str,
    groups: &[LinkGroup],
    mother_states: &HashMap<String, Option<i64>>,
    targets_len: usize,
    copied_count: usize,
) -> PreviewBinding {
    let member = |uid: &str, role: &str, digest: String, count: usize| PreviewMemberBinding {
        member_id: format!("mirror-{role}"),
        account_id: None,
        uid: uid.to_string(),
        session_id: String::new(),
        raw_digest: String::new(),
        normalized_digest: digest,
        record_count: count,
    };
    PreviewBinding {
        variant,
        group_id: MIRROR_GROUP_ID.to_string(),
        group_fingerprint: expected_digest.to_string(),
        source: member(
            mother_uid,
            "mother",
            mother_mirror_fingerprint(groups, mother_uid, mother_states),
            copied_count,
        ),
        target: member(
            target_uid,
            "target",
            expected_digest.to_string(),
            targets_len,
        ),
        baseline_ref: None,
        baseline_total_digest: None,
        baseline_record_count: None,
        verdict: SyncVerdict::Identical,
        source_status: None,
        target_status: None,
        archive_action: None,
        mother_uid: Some(mother_uid.to_string()),
    }
}

/// 生产入口：按档位路径只读预览「当前登录账号 → 目标账号」的整份镜像清单。
pub fn mirror_preview(variant: WbVariant, target_acc: &Value) -> Result<Value, String> {
    let preview = mirror_preview_at(
        &SessionPaths::for_variant(variant),
        // 专家团清单与人格目录同挂在数据根下（与档位无关），生产恒取真实数据根。
        &wb_data_root(),
        variant,
        target_acc,
    )?;
    serde_json::to_value(preview).map_err(|error| format!("镜像预览序列化失败：{error}"))
}

/// 路径注入的可测实现：只读数据库（`read_only = true`），唯一落盘是预览凭据 token
/// （既有只读预览机制，非会话/记忆数据），绝不写 sessions / MEMORY.md / 正文。
///
/// `memory_root` 是人设与专家团清单的根目录（生产为 `wb_data_root()`）：显式传入保证测试
/// 不触碰真实用户目录，也与执行侧 `mirror_apply_at` 用同一个根，预览与执行结论同源。
pub fn mirror_preview_at(
    paths: &SessionPaths,
    memory_root: &Path,
    variant: WbVariant,
    target_acc: &Value,
) -> Result<MirrorPreview, String> {
    // 母本恒取登录态，杜绝前端伪造 motherUid 扩大作用域。
    let mother_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法确定母本账号".to_string())?;
    let target_uid = account_uid_of(target_acc);
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法预览镜像".to_string());
    }
    if mother_uid == target_uid {
        return Err("当前账号与目标账号相同，无需镜像".to_string());
    }

    let conn = open_db(&paths.workbuddy_db(), true)
        .ok_or_else(|| "会话库不可读，无法预览镜像".to_string())?;
    let rows = read_target_rows(&conn, &target_uid)?;
    let groups = load_groups(paths);
    // 一次请求内**只采样一次**母本状态：targets 与 copiedCount 共用同一份快照，禁止两处各读一次。
    let mother_states = read_session_states(&conn, &mother_uid)?;
    let target_alive_ids = alive_ids_of(&rows);
    let targets = mirror_targets_from_rows(&rows, &target_uid, &mother_uid, &groups, &mother_states);
    let expected_digest = expected_digest_of(&targets);
    let copied_count = count_mirror_copies(
        &groups,
        &mother_uid,
        &target_uid,
        &mother_states,
        &target_alive_ids,
    );
    // 只读派生：复用同一次连接与同一份 `mother_states` 快照，不再第二次采样。
    let mother_titles: HashMap<String, String> = read_target_rows(&conn, &mother_uid)?
        .into_iter()
        .map(|row| (row.session_id, display_title(row.title, row.custom_title)))
        .collect();
    let unmerged = unmerged_groups(
        &groups,
        &mother_uid,
        &target_uid,
        &mother_states,
        &mother_titles,
    );

    let binding = build_mirror_token_binding(
        variant,
        &mother_uid,
        &target_uid,
        &expected_digest,
        &groups,
        &mother_states,
        targets.len(),
        copied_count,
    );
    let preview_token = session_link::save_preview_token(paths, binding)?;
    let auth = auth_file::read_auth_file_at(&paths.auth_file).unwrap_or(Value::Null);
    let mother_label = account_label(&auth, &mother_uid);
    // 与执行侧同一个纯函数判定（`experts::plan_overwrite_at`），两处结论不会漂移。
    // 内容不可信不该阻断预览：只在面板上说明跳过原因，才由执行侧折进 errors。
    let (experts_pending, experts_names, experts_note) =
        match experts::plan_overwrite_at(memory_root, &mother_uid, &target_uid) {
            Ok(plan) => (plan.requires_write, plan.mother_names, plan.note),
            Err(block) => (false, Vec::new(), Some(block.message().to_string())),
        };

    Ok(MirrorPreview {
        mother_uid,
        mother_label,
        preview_token,
        expected_digest,
        session_count: targets.len(),
        copied_count,
        experts_pending,
        experts_names,
        experts_note,
        targets,
        backup_dir: None,
        unmerged,
    })
}

// ---------------------------------------------------------------------------
// 执行链路（批次②）：软删 + 精确白名单回滚（本批不接线到 switch.rs）
// ---------------------------------------------------------------------------

/// 镜像备份清单版本；与操作目录约定一一对应，读到其它版本一律拒绝。
///
/// **不要升这个数**：`load_mirror_manifest` 是严格等值校验，升版会让磁盘上所有历史备份在
/// 还原时被拒，等于既有可还原项全部失效。新增字段一律 `#[serde(default)]` 做同版本扩展
/// （老 manifest 读成 `None`/空 → 该步骤跳过），代价是同版本内字段语义一旦落盘不可再改。
const MIRROR_BACKUP_VERSION: u32 = 1;
/// 镜像备份清单文件名（固定放在操作专属目录内）。
const MIRROR_MANIFEST_FILE: &str = "mirror-manifest.json";
/// 专家团清单原文的备份文件名：与清单同级，随操作目录一起持久化。
const EXPERTS_BACKUP_FILE: &str = "experts-backup.json";
/// 清单状态：备份已落、软删事务尚未可靠完成（或崩溃窗口），库可能已改但未标记完成。
const MIRROR_STATE_PENDING: &str = "pending";
/// 清单状态：软删事务已 commit 且读回校验通过，可放心一键还原。
const MIRROR_STATE_DONE: &str = "done";
/// 记忆步骤在报告与清单里的标识：它不是会话，不该冒用 `session_id` 语义。
const MEMORY_STEP_ID: &str = "MEMORY.md";

/// 单条待软删会话的备份登记：恢复只按 `(sessionId, userId, deletedAtWritten)` 三元组精确置空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MirrorBackupRow {
    session_id: String,
    user_id: String,
    group_id: String,
    group_fingerprint: String,
    title: String,
    updated_at_before: i64,
    /// 只登记未软删行，恒为空；保留字段以便未来扩展。
    deleted_at_before: Option<i64>,
    /// 预览时的规范化摘要，供人工核对。
    full_digest: String,
    /// 本次写入的 `deleted_at` 值，恢复时严格按其匹配。
    deleted_at_written: i64,
}

/// 镜像切换备份清单：先落备份再置位；清单读不回来就不算备份成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MirrorBackupManifest {
    version: u32,
    operation_id: String,
    variant: WbVariant,
    mother_uid: String,
    target_uid: String,
    preview_token: String,
    expected_digest: String,
    created_at: i64,
    /// `pending`（备份已落、软删/复制未确认完成/崩溃窗口）或 `done`（软删+复制已 commit 且读回校验通过）。
    /// 供 `scan_mirror_backups` 发现「库已改但 UI/启动恢复无感知」的窄窗口。
    #[serde(default = "default_mirror_state")]
    state: String,
    rows: Vec<MirrorBackupRow>,
    /// 本次「复制新增」的会话：回滚时按 ids 软删，禁止静默留下孤儿。
    #[serde(default)]
    copied: Vec<MirrorCopiedEntry>,
    /// 本次 `MEMORY.md` 整份覆盖的备份绝对路径（若有）。
    /// 记忆覆盖与 sessions 软删是两个独立可还原步骤，互不替代。
    #[serde(default)]
    memory_backup: Option<String>,
    /// 本次专家团清单整份覆盖的原文备份绝对路径（若有）；`None` 时还原跳过该步骤。
    #[serde(default)]
    experts_backup: Option<String>,
    /// 本次写入目标账号的专家团名字，供界面明文复核（不参与还原判定）。
    #[serde(default)]
    experts_names: Vec<String>,
    restore_steps: Vec<String>,
}

/// 一条「复制新增」登记：源会话 id、目标新 id、归属与创建时刻。回滚据此精确软删新副本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorCopiedEntry {
    pub source_session_id: String,
    pub new_session_id: String,
    pub user_id: String,
    pub group_id: String,
    pub created_at: i64,
}

/// 旧清单缺 `state` 时按 pending 处理（保守：宁可提示可还原，也不当作已完成）。
fn default_mirror_state() -> String {
    MIRROR_STATE_PENDING.to_string()
}

/// 构造「预览时冻结」的绑定：母本、指纹与待删清单都来自预览凭据与计划，绝不信任前端母本。
fn expected_mirror_binding(
    token: &PreviewToken,
    plan: &MirrorPlan,
    target_uid: &str,
) -> MirrorBinding {
    let mother_uid = token.binding.mother_uid.clone().unwrap_or_default();
    let mut group_ids: Vec<String> = plan
        .targets
        .iter()
        .map(|target| target.group_id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    group_ids.sort();
    group_ids.dedup();
    MirrorBinding {
        mother_uid,
        target_uid: target_uid.to_string(),
        mother_active_fingerprint: token.binding.source.normalized_digest.clone(),
        expected_digest: plan.expected_digest.clone(),
        targets: plan.targets.clone(),
        group_ids: group_ids.clone(),
        found_group_ids: group_ids.clone(),
        groups_with_mother: group_ids,
        all_targets_active: true,
    }
}

/// 构造「现场重算」的绑定：所有字段都从当前库与关联表重取，供 `verify_mirror_binding` 逐条比对。
fn live_mirror_binding(
    mother_uid: &str,
    target_uid: &str,
    groups: &[LinkGroup],
    mother_states: &HashMap<String, Option<i64>>,
    live_targets: &[MirrorTarget],
    expected: &MirrorBinding,
) -> MirrorBinding {
    let found_group_ids: Vec<String> = expected
        .group_ids
        .iter()
        .filter(|id| groups.iter().any(|group| &group.id == *id))
        .cloned()
        .collect();
    let groups_with_mother: Vec<String> = expected
        .group_ids
        .iter()
        .filter(|id| {
            groups.iter().any(|group| {
                &group.id == *id
                    && group.members.iter().any(|member| {
                        member.uid == mother_uid && member.state == MemberState::Active
                    })
            })
        })
        .cloned()
        .collect();
    let all_targets_active = expected
        .targets
        .iter()
        .all(|target| live_targets.iter().any(|live| live.session_id == target.session_id));
    build_mirror_binding(
        mother_uid,
        target_uid,
        groups,
        mother_states,
        live_targets,
        found_group_ids,
        groups_with_mother,
        all_targets_active,
    )
}

/// 先落备份清单再置位：在没有备份正文的前提下，复用操作专属目录约定 + 持久化写。
fn write_mirror_manifest(
    paths: &SessionPaths,
    variant: WbVariant,
    operation_id: &str,
    mother_uid: &str,
    target_uid: &str,
    plan: &MirrorPlan,
    groups: &[LinkGroup],
    live_targets: &[MirrorTarget],
    deleted_at_written: i64,
    experts_names: &[String],
) -> Result<PathBuf, String> {
    let dir = session_backup::transaction_dir(paths, variant, operation_id)?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("镜像备份目录创建失败：{error}"))?;
    let rows: Vec<MirrorBackupRow> = live_targets
        .iter()
        .map(|target| MirrorBackupRow {
            session_id: target.session_id.clone(),
            user_id: target.user_id.clone(),
            group_id: target.group_id.clone(),
            group_fingerprint: groups
                .iter()
                .find(|group| group.id == target.group_id)
                .map(session_link::group_fingerprint)
                .unwrap_or_default(),
            title: target.title.clone(),
            updated_at_before: target.updated_at,
            deleted_at_before: None,
            full_digest: target.full_digest.clone(),
            deleted_at_written,
        })
        .collect();
    let manifest = MirrorBackupManifest {
        version: MIRROR_BACKUP_VERSION,
        operation_id: operation_id.to_string(),
        variant,
        mother_uid: mother_uid.to_string(),
        target_uid: target_uid.to_string(),
        preview_token: plan.preview_token.clone(),
        expected_digest: plan.expected_digest.clone(),
        created_at: now_ms(),
        state: MIRROR_STATE_PENDING.to_string(),
        rows,
        copied: Vec::new(),
        memory_backup: None,
        // 专家团备份要等覆盖时才产生（且只有目标原本有清单才有原文），故此处先留空，
        // 由 `finalize_mirror_manifest` 回填。
        experts_backup: None,
        experts_names: experts_names.to_vec(),
        restore_steps: vec![
            "按清单逐条把 sessions.deleted_at 置空：WHERE id=? AND user_id=? AND deleted_at=<本清单 deletedAtWritten>".to_string(),
            "只还原本次写入值；严禁按 user_id 批量置空（会误恢复用户此前自删的会话）".to_string(),
            "本备份不涉及会话正文：镜像只软删，不写 jsonl，恢复亦不碰正文".to_string(),
            "专家团清单的原文备份在 experts-backup.json：还原时整份写回，不做三方合并".to_string(),
        ],
    };
    let content = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    let file = dir.join(MIRROR_MANIFEST_FILE);
    session_backup::durable_write_str(&file, &content)
        .map_err(|error| format!("镜像备份清单保存失败：{error}"))?;
    let read_back = std::fs::read_to_string(&file)
        .map_err(|error| format!("镜像备份清单回读失败：{error}"))?;
    let parsed: MirrorBackupManifest = serde_json::from_str(&read_back)
        .map_err(|error| format!("镜像备份清单回读解析失败：{error}"))?;
    if parsed != manifest {
        return Err("镜像备份清单保存后核验不一致，未按备份成功处理".to_string());
    }
    session_backup::sync_tree(&dir).map_err(|error| format!("镜像备份目录持久化失败：{error}"))?;
    Ok(dir)
}

/// 短事务软删：条件 UPDATE 幂等且防跨账号（`user_id` + `deleted_at IS NULL` 双重前置）。
fn soft_delete_targets(
    conn: &mut Connection,
    target_uid: &str,
    targets: &[MirrorTarget],
    deleted_at: i64,
) -> Result<(usize, Vec<MirrorSkipped>), String> {
    session_backup::ensure_full_synchronous(conn)?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("会话数据事务开启失败：{error}"))?;
    let mut deleted = 0usize;
    let mut skipped = Vec::new();
    for target in targets {
        let affected = tx
            .execute(
                "UPDATE sessions SET deleted_at = ?1 \
                 WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
                params![deleted_at, target.session_id, target_uid],
            )
            .map_err(|error| format!("软删会话失败（{}）：{error}", target.session_id))?;
        if affected == 1 {
            deleted += 1;
        } else {
            skipped.push(MirrorSkipped {
                session_id: target.session_id.clone(),
                reason: "会话不存在、不属于目标账号或已被软删，已跳过".to_string(),
            });
        }
    }
    tx.commit()
        .map_err(|error| format!("会话数据提交失败：{error}"))?;
    Ok((deleted, skipped))
}

/// 落盘后读回校验：`deleted_at` 必须等于本次写入值，否则报「持久化证明缺失」（禁止反向写回）。
fn verify_soft_deleted(
    conn: &Connection,
    target_uid: &str,
    targets: &[MirrorTarget],
    expected_deleted_at: i64,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare("SELECT deleted_at FROM sessions WHERE id = ?1 AND user_id = ?2")
        .map_err(|error| format!("软删读回校验失败：{error}"))?;
    for target in targets {
        let value = stmt
            .query_row(params![target.session_id, target_uid], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .optional()
            .map_err(|error| format!("软删读回校验失败（{}）：{error}", target.session_id))?;
        match value {
            Some(Some(v)) if v == expected_deleted_at => {}
            other => {
                return Err(format!(
                    "会话 {} 的 deleted_at 读回为 {other:?}，与本次写入值不一致",
                    target.session_id
                ))
            }
        }
    }
    Ok(())
}

/// 执行整份镜像：现场双 verify → 摘要重算 → 先备份再置位 → 软删 → 读回校验 → 复制母本独有会话
/// → `MEMORY.md` 整份覆盖。任一 verify 非空即零写入（连复制与记忆覆盖也不做）。
///
/// `memory_root` 是人格目录根（生产为 `wb_data_root()`）：显式传入而非模块内取全局，
/// 既保证测试不触碰真实用户目录，也让两个档位共用同一份人格目录。
pub fn mirror_apply_at(
    paths: &SessionPaths,
    variant: WbVariant,
    target_uid: &str,
    acc: &Value,
    mother_acc: &Value,
    memory_root: &Path,
    plan: &MirrorPlan,
) -> Result<MirrorReport, String> {
    let target_uid = target_uid.trim();
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法执行镜像".to_string());
    }
    // 母本恒取登录态，忽略 plan.mother_uid（前端已不下发；后端同源 current_user_uid_at）。
    let mother_uid = current_user_uid_at(&paths.auth_file)
        .ok_or_else(|| "未读取到本机登录态，无法执行镜像".to_string())?;
    if mother_uid == target_uid {
        return Err("当前账号与目标账号相同，无需镜像".to_string());
    }
    // 预览凭据是执行授权的唯一依据：缺失即零写入。
    let token = session_link::load_preview_token(paths, &plan.preview_token)
        .ok_or_else(|| "镜像预览凭据已失效，请重新预览".to_string())?;

    let mut conn = open_db(&paths.workbuddy_db(), false)
        .ok_or_else(|| "会话库不可读，无法执行镜像".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("会话库缺少 sessions 表，无法执行镜像".to_string());
    }
    let rows = read_target_rows(&conn, target_uid)?;
    let groups = load_groups(paths);
    // 与 `mirror_preview_at` 同源：整条执行链路只采样一次母本状态，targets / copiedCount /
    // missing 三处共用同一份快照，禁止任何一处再读一次库。
    let mother_states = read_session_states(&conn, &mother_uid)?;
    let target_alive_ids = alive_ids_of(&rows);
    let live_targets = mirror_targets_from_rows(&rows, target_uid, &mother_uid, &groups, &mother_states);
    let live_expected_digest = expected_digest_of(&live_targets);
    // 母本行只读一次：标题（「仍缺清单」用）与活跃时刻（落后副本预筛用）同源，零新增查询。
    let mother_rows = read_target_rows(&conn, &mother_uid)?;
    let mother_titles: HashMap<String, String> = mother_rows
        .iter()
        .map(|row| {
            (
                row.session_id.clone(),
                display_title(row.title.clone(), row.custom_title.clone()),
            )
        })
        .collect();
    let mother_activity = activity_by_session(&mother_rows);
    let target_activity = activity_by_session(&rows);

    // 硬门禁 2：expectedDigest 逐条重算，与计划不符 → 零写入。
    if live_expected_digest != plan.expected_digest {
        return Err("待删会话集合与预览不一致（内容已变化），已停止镜像；请重新预览".to_string());
    }

    // 硬门禁 3：造镜像专用 live PreviewBinding（哨兵 group_id + mother_uid=Some(重取登录态)、
    // 逐字段重算），同跑 verify_preview；绝不复用逐组同步的 live_preview_binding。
    let live_copied_count = count_mirror_copies(
        &groups,
        &mother_uid,
        target_uid,
        &mother_states,
        &target_alive_ids,
    );
    let live_preview_binding = build_mirror_token_binding(
        variant,
        &mother_uid,
        target_uid,
        &live_expected_digest,
        &groups,
        &mother_states,
        live_targets.len(),
        live_copied_count,
    );
    let preview_stale = session_link::verify_preview(&token, &live_preview_binding);

    // 硬门禁 5：母本唯一权威 = 登录态重取。expected 取凭据绑定母本、live 取当前登录态，
    // 两个 verify 的 live 侧都由同一 mother_uid 构造，口径同源不漂移。
    let expected_binding = expected_mirror_binding(&token, plan, target_uid);
    let live_binding = live_mirror_binding(
        &mother_uid,
        target_uid,
        &groups,
        &mother_states,
        &live_targets,
        &expected_binding,
    );
    let mirror_stale = verify_mirror_binding(&expected_binding, &live_binding);

    if !preview_stale.is_empty() || !mirror_stale.is_empty() {
        let mut details = preview_stale;
        details.extend(mirror_stale.into_iter().map(|item| item.detail));
        return Err(format!(
            "镜像预览已过期，已停止且零写入：{}",
            details.join("；")
        ));
    }

    // 目标账号缺失的母本会话（已关联的不算缺失）：100% 镜像的另一半，与预览 `copiedCount` 同源。
    let missing = missing_mirror_session_ids(
        &groups,
        &mother_uid,
        target_uid,
        &mother_states,
        &target_alive_ids,
    );

    // 预筛（纯函数、零 IO）：母本领先而目标落后的组才是自动同步候选。全库全组算内容摘要
    // 不可接受（几百组 × 两份正文），这一步把内容预览的范围压到真正活跃且落后的组。
    let (behind, target_ahead) = classify_activity_shift(
        &groups,
        &mother_activity,
        &target_activity,
        &mother_uid,
        target_uid,
        MIRROR_SYNC_MARGIN_MS,
    );

    // 记忆是否已一致：必须并入「零操作」判定——否则「仅 MEMORY.md 不同」的切号会被整体跳过，
    // 镜像就不算 100%。
    let memory_pending = !memory::memory_already_identical(memory_root, &mother_uid, target_uid);
    // 专家团清单同理：与预览同一个纯函数判定（I5 单次采样口径），也必须并入零操作判定，
    // 否则「仅专家团不同」的切号会连备份目录都不建、什么都不做。
    let experts_plan = experts::plan_overwrite_at(memory_root, &mother_uid, target_uid);
    let experts_pending = matches!(&experts_plan, Ok(plan) if plan.requires_write);
    let experts_names: Vec<String> = match &experts_plan {
        Ok(plan) => plan.mother_names.clone(),
        Err(_) => Vec::new(),
    };
    // 清单不可信是必须上报的错误：即便本次无其它待办，也不能悄悄当成「无操作」吞掉。
    let experts_plan_error = match &experts_plan {
        Ok(_) => None,
        Err(block) => Some(expert_step_error(block.message())),
    };

    // 既无待删、也无待复制、也没有落后的副本、记忆与专家团都已一致：无操作、不建备份目录
    // （避免空还原项）。
    if live_targets.is_empty()
        && missing.is_empty()
        && behind.is_empty()
        && !memory_pending
        && !experts_pending
    {
        return Ok(MirrorReport {
            deleted_sessions: 0,
            copied_sessions: 0,
            restored_copies: 0,
            backup: None,
            needs_recovery: false,
            experts_updated: false,
            experts_restored: false,
            warnings: Vec::new(),
            skipped: Vec::new(),
            errors: experts_plan_error.into_iter().collect(),
            // 零操作：什么都没改，当前缺口就是执行后的缺口。
            unmerged: unmerged_groups(&groups, &mother_uid, target_uid, &mother_states, &mother_titles),
            synced_sessions: 0,
            overwritten_sessions: 0,
            sync_details: Vec::new(),
            sync_errors: Vec::new(),
        });
    }

    // 先落备份记录再置位（清单同时锚定软删与复制的回滚依据）。
    let deleted_at_written = now_ms();
    let operation_id = uuid::Uuid::new_v4().to_string();
    let backup_dir = write_mirror_manifest(
        paths,
        variant,
        &operation_id,
        &mother_uid,
        target_uid,
        plan,
        &groups,
        &live_targets,
        deleted_at_written,
        &experts_names,
    )?;

    // 软删（可能 0 行）：单事务 commit，随后读回校验。
    let (deleted, skipped) =
        soft_delete_targets(&mut conn, target_uid, &live_targets, deleted_at_written)?;
    verify_soft_deleted(&conn, target_uid, &live_targets, deleted_at_written).map_err(|detail| {
        format!(
            "已落盘但持久化证明缺失：{detail}；备份保留在 {}，请勿反向写回",
            backup_dir.display()
        )
    })?;

    // 分组归属修补：历史副本可能带着母本的 `space_id`，而 `spaces` 行按 `user_id` 归属
    // 账号——目标账号没有那一行，客户端按分组渲染时会把会话一起丢掉。复制前先归位，
    // 新副本才能与老副本落在同一个本地分组里。
    let space_repair = session::repair_hanging_space_links_at(paths, target_uid);

    // 复制母本独有会话：失败不阻断软删，错误进 errors；成功的新副本登记进清单供回滚。
    let (copied_entries, copy_errors, copy_needs_recovery) = if missing.is_empty() {
        (Vec::new(), Vec::new(), false)
    } else {
        copy_missing_sessions(paths, variant, acc, target_uid, &missing)
    };

    // 自动同步「母本领先、目标落后」的副本：必须在复制之后——复制会给缺失组新增目标
    // `Active` 成员、改变内容绑定，先取预览凭据必然 STALE。整段软失败（错误进
    // `sync_errors`），绝不阻断切号；认证尚未写入，登录态仍是母本。
    let sync_outcome = if behind.is_empty() {
        MirrorSyncOutcome::default()
    } else if account_uid_of(mother_acc) != mother_uid {
        // 母本凭据不可用就无从确定同步作用域：整段跳过并如实上报，不改走 Err（§8.4）。
        MirrorSyncOutcome::whole_failure(
            &behind,
            "母本账号凭据不可用，本次未自动同步落后副本".to_string(),
        )
    } else {
        sync_behind_copies(
            &behind,
            || {
                session::session_links_preview_cross_for_groups_at(
                    paths, variant, mother_acc, acc, &behind,
                )
            },
            |selections| {
                session::sync_sessions_cross_same_variant_at(
                    paths,
                    variant,
                    mother_acc,
                    acc,
                    selections,
                    |_| false,
                )
            },
        )
    };

    // MEMORY.md 整份覆盖（只写目标账号）：失败不阻断已完成的软删/复制；
    // 记忆备份路径登记进同一清单，作为与 sessions 解耦的独立可还原步骤。
    let (memory_backup, memory_errors) = if memory_pending {
        overwrite_target_memory(memory_root, &mother_uid, target_uid)
    } else {
        (None, Vec::new())
    };
    // 专家团清单整份覆盖（只写目标账号）：与记忆步骤同形——失败不阻断已完成的软删/复制，
    // 备份路径登记进同一清单，作为第三个独立可还原步骤。
    let (experts_backup, experts_updated, experts_errors) = if experts_pending {
        overwrite_target_experts(memory_root, &mother_uid, target_uid, &backup_dir)
    } else {
        (None, false, Vec::new())
    };
    let mut errors = copy_errors;
    errors.extend(experts_plan_error);
    errors.extend(memory_errors);
    errors.extend(experts_errors);

    // 软删+复制+记忆覆盖+专家团覆盖完成后把清单原子改写为 done；失败仅降级为 warning。
    let mut warnings = Vec::new();
    // B6：目标更新的组一律不回退（母本是唯一内容权威），但必须让人知道有这回事。
    if !target_ahead.is_empty() {
        warnings.push(format!(
            "{} 个组的母本内容比目标旧，本次未回退（镜像只补齐母本领先的副本）",
            target_ahead.len()
        ));
    }
    // B9-bis：预筛命中却没有任何一组真的写入（内容实为一致）——备份已建、却无条目。
    if !behind.is_empty() && sync_outcome.details.is_empty() && sync_outcome.errors.is_empty() {
        warnings.push(format!("预筛 {} 个组，核对后实际无需同步", behind.len()));
    }
    match space_repair {
        Ok(0) => {}
        Ok(fixed) => warnings.push(format!(
            "已把 {fixed} 条历史会话的分组归到本账号（它们此前指向别的账号的分组，客户端会把它们从列表里丢掉）"
        )),
        Err(error) => errors.push(MirrorError {
            session_id: String::new(),
            error: format!("修正历史会话分组失败：{error}"),
        }),
    }
    if let Err(error) = finalize_mirror_manifest(
        &backup_dir,
        &copied_entries,
        memory_backup.as_deref(),
        experts_backup.as_deref(),
    ) {
        warnings.push(format!(
            "镜像已完成但清单状态标记失败（{error}）；备份仍在 {}，扫描会显示为 pending",
            backup_dir.display()
        ));
    }

    // `needs_recovery` 只回填复制侧的真实值（`session.rs` 在「有未完成写入 / 恢复不干净」
    // 时才置 true）——它的语义是「下次切号会先恢复」，不能拿来表达「这次没全成」。
    // 复制在写入前就失败（如源正文不存在）、或多账号 MEMORY.md 覆盖失败，都不产生
    // pending operation，标 true 等于承诺一件不会发生的事；那类情况由前端用
    // `errors` 单独表达为「部分未完成，备份已保留」。
    let needs_recovery = copy_needs_recovery;

    // 执行后再读一次组存储（只读、单个 JSON 文件）才叫「仍缺」：复制成功的组已把目标
    // 登记为 `Active` 成员，会自动从清单里消失。母本状态沿用本次唯一一次采样。
    let groups_after = load_groups(paths);
    let unmerged = unmerged_groups(&groups_after, &mother_uid, target_uid, &mother_states, &mother_titles);

    Ok(MirrorReport {
        deleted_sessions: deleted,
        copied_sessions: copied_entries.len(),
        restored_copies: 0,
        backup: Some(backup_dir.to_string_lossy().to_string()),
        needs_recovery,
        experts_updated,
        experts_restored: false,
        warnings,
        skipped,
        errors,
        unmerged,
        synced_sessions: sync_outcome.synced,
        overwritten_sessions: sync_outcome.overwritten,
        sync_details: sync_outcome.details,
        sync_errors: sync_outcome.errors,
    })
}

/// 调 `copy_sessions_for_switch_at` 复制，解析 `copied`/`errors`（复制失败不阻断软删）。
fn copy_missing_sessions(
    paths: &SessionPaths,
    variant: WbVariant,
    acc: &Value,
    target_uid: &str,
    missing: &[String],
) -> (Vec<MirrorCopiedEntry>, Vec<MirrorError>, bool) {
    // App 已在 switch 主干 close_workbuddy 之后关闭，探针恒 false。
    let report = match session::copy_sessions_for_switch_at(paths, variant, acc, missing, |_| false) {
        Ok(report) => report,
        Err(error) => {
            return (
                Vec::new(),
                vec![MirrorError {
                    session_id: String::new(),
                    error,
                }],
                false,
            )
        }
    };
    let copied_entries: Vec<MirrorCopiedEntry> = report
        .get("copied")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let source = item.get("id").and_then(Value::as_str)?;
                    let new_id = item.get("newId").and_then(Value::as_str)?;
                    Some(MirrorCopiedEntry {
                        source_session_id: source.to_string(),
                        new_session_id: new_id.to_string(),
                        user_id: target_uid.to_string(),
                        group_id: item
                            .get("groupId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        created_at: now_ms(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let copy_errors: Vec<MirrorError> = report
        .get("errors")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| MirrorError {
                    session_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    error: item
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    // 复制侧自己的恢复标记：会话持久化不完整时它会是 true，必须原样上报，
    // 不能被镜像层压成「整份镜像完成」。
    let copy_needs_recovery = report
        .get("needsRecovery")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (copied_entries, copy_errors, copy_needs_recovery)
}

// ---------------------------------------------------------------------------
// 自动同步落后副本：预筛（纯函数）+ 模式映射（纯查表）+ 逐组同步
// ---------------------------------------------------------------------------

/// 预筛阈值：母本活跃时刻超出目标这么多，才算「母本领先、目标落后」。
///
/// 一轮真实对话必然把 `last_activity_at` 推到秒级新值；60s 只为吃掉「同一次写入被两条
/// 记录分摊 / 两个账号行落在同一事务里」造成的时间戳抖动。取 0 会让刚复制出来的新副本
/// 反复进候选，取太大（如 1 天）又会漏掉当天仍在活跃的组。
pub const MIRROR_SYNC_MARGIN_MS: i64 = 60_000;

/// 镜像自动选择的写入模式（纯查表）。
///
/// 只放行两种真的能写、且 `available_modes()` 也放行的判定：`fastForward` 与 `diverge`。
/// `identical` 无需写入；`ahead`（母本落后于目标）虽然在 `allows()` 里对
/// `UnifyOverwrite` 放行，但该模式不在 `available_modes()` —— 它是组级「整组保留来源副本」
/// 的专用动作，镜像层**禁止**自造它去绕过该限制；`unknown` 不可验证，覆盖不能绕过未知。
pub fn auto_sync_mode(verdict: SyncVerdict) -> Option<SyncMode> {
    match verdict {
        SyncVerdict::FastForward => Some(SyncMode::FastForward),
        SyncVerdict::Diverge => Some(SyncMode::Overwrite),
        SyncVerdict::Identical | SyncVerdict::Ahead | SyncVerdict::Unknown => None,
    }
}

/// 母本领先的候选组（纯函数，零 IO，返回按 `group_id` 升序去重）。
///
/// 判据（逐组，顶部扁平卫语句）：
/// 1. 组内没有母本 `Active` 成员 → 无从比较，跳过；
/// 2. 组内没有目标 `Active` 成员 → 目标无副本/副本已失效，交给复制路径（B1/B2）；
/// 3. `mother_max <= target_max + margin_ms` → 内容一致或目标更新（B3/B6），不碰。
pub fn groups_behind_candidates(
    groups: &[LinkGroup],
    mother_activity: &HashMap<String, i64>,
    target_activity: &HashMap<String, i64>,
    mother_uid: &str,
    target_uid: &str,
    margin_ms: i64,
) -> Vec<String> {
    classify_activity_shift(
        groups,
        mother_activity,
        target_activity,
        mother_uid,
        target_uid,
        margin_ms,
    )
    .0
}

/// 一次时间戳扫描的两份产出：`(母本领先的组, 目标更新且不回退的组)`。
///
/// 两者共用同一份 `sessions` 行快照与同一组判据，分开各扫一遍只是重复读取。
fn classify_activity_shift(
    groups: &[LinkGroup],
    mother_activity: &HashMap<String, i64>,
    target_activity: &HashMap<String, i64>,
    mother_uid: &str,
    target_uid: &str,
    margin_ms: i64,
) -> (Vec<String>, Vec<String>) {
    let mut behind: BTreeSet<String> = BTreeSet::new();
    let mut target_ahead: Vec<String> = Vec::new();
    for group in groups {
        let Some(mother_max) = active_activity_of(group, mother_uid, mother_activity) else {
            continue;
        };
        let Some(target_max) = active_activity_of(group, target_uid, target_activity) else {
            continue;
        };
        if mother_max > target_max + margin_ms {
            behind.insert(group.id.clone());
        } else if target_max > mother_max + margin_ms {
            target_ahead.push(group.id.clone());
        }
    }
    (behind.into_iter().collect(), target_ahead)
}

/// 组内某账号 `Active` 成员的最大活跃时刻；没有 `Active` 成员时返回 `None`。
///
/// 缺值（历史脏数据没有活动时间）按 0 处理：宁可多算一次内容预览，也不会写错。
fn active_activity_of(
    group: &LinkGroup,
    uid: &str,
    activity: &HashMap<String, i64>,
) -> Option<i64> {
    group
        .members
        .iter()
        .filter(|member| member.uid == uid && member.state == MemberState::Active)
        .map(|member| activity.get(&member.session_id).copied().unwrap_or(0))
        .max()
}

/// 只读行 → `session_id -> activity_at`（预筛输入；零新增查询）。
fn activity_by_session(rows: &[MirrorRow]) -> HashMap<String, i64> {
    rows.iter()
        .filter(|row| !row.session_id.is_empty())
        .map(|row| (row.session_id.clone(), row.activity_at))
        .collect()
}

/// 自动同步的产出：三个计数 + 明细，整份并进 [`MirrorReport`]。
#[derive(Debug, Default)]
struct MirrorSyncOutcome {
    synced: usize,
    overwritten: usize,
    details: Vec<MirrorSyncedGroup>,
    errors: Vec<MirrorSyncError>,
}

impl MirrorSyncOutcome {
    /// 整体失败（母本凭据不可用 / 预览读不到）：一组一条，其余组不牵连。
    fn whole_failure(group_ids: &[String], error: String) -> Self {
        Self {
            errors: group_ids
                .iter()
                .map(|group_id| MirrorSyncError {
                    group_id: group_id.clone(),
                    title: None,
                    error: error.clone(),
                })
                .collect(),
            ..Self::default()
        }
    }
}

/// 预览 JSON 的 `verdict` 取值 → 枚举；未知取值一律按 `Unknown`（覆盖不能绕过未知）。
fn verdict_of(raw: &str) -> SyncVerdict {
    match raw {
        "identical" => SyncVerdict::Identical,
        "fastForward" => SyncVerdict::FastForward,
        "ahead" => SyncVerdict::Ahead,
        "diverge" => SyncVerdict::Diverge,
        _ => SyncVerdict::Unknown,
    }
}

/// 把「母本领先的候选组」同步成与母本一致；失败逐组进 `sync_errors`，绝不阻断切号。
///
/// 预览与同步都以闭包注入：预览走 `session_links_preview_cross_for_groups_at`、写入走
/// `sync_sessions_cross`，两者内部各自持档位操作锁并复核凭据，镜像层不重复加锁、
/// 也不跳过 `verdict.allows(mode)` 的复核。
fn sync_behind_copies(
    candidate_group_ids: &[String],
    preview: impl FnOnce() -> Result<Value, String>,
    sync: impl FnOnce(&[SyncSelection]) -> Result<Value, String>,
) -> MirrorSyncOutcome {
    if candidate_group_ids.is_empty() {
        return MirrorSyncOutcome::default();
    }
    let preview = match preview() {
        Ok(value) => value,
        // B8：预览整体读不到 → 一组一条，绝不构造 SyncSelection。
        Err(error) => {
            return MirrorSyncOutcome::whole_failure(
                candidate_group_ids,
                format!("预览落后副本失败：{error}"),
            )
        }
    };
    let items = preview
        .get("groups")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut outcome = MirrorSyncOutcome::default();
    let mut titles: HashMap<String, Option<String>> = HashMap::new();
    let mut selections: Vec<SyncSelection> = Vec::new();
    for item in &items {
        let Some(group_id) = item.get("groupId").and_then(Value::as_str) else {
            continue;
        };
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string);
        titles.insert(group_id.to_string(), title.clone());
        let verdict = verdict_of(item.get("verdict").and_then(Value::as_str).unwrap_or_default());
        let Some(mode) = auto_sync_mode(verdict) else {
            // B7：不可验证的组必须让人看见；identical / ahead 无需写入，静默跳过。
            if verdict == SyncVerdict::Unknown {
                outcome.errors.push(MirrorSyncError {
                    group_id: group_id.to_string(),
                    title,
                    error: "目标副本已失效，需手动处理".to_string(),
                });
            }
            continue;
        };
        let Some(preview_token) = item.get("previewToken").and_then(Value::as_str) else {
            // B8：没有凭据绝不构造 selection（缺 token 会让整次同步 Err）。
            outcome.errors.push(MirrorSyncError {
                group_id: group_id.to_string(),
                title,
                error: "预览凭据已失效，本次未同步该组".to_string(),
            });
            continue;
        };
        let offered = item
            .get("availableModes")
            .and_then(Value::as_array)
            .is_some_and(|modes| modes.iter().any(|offered| offered.as_str() == Some(mode.as_str())));
        if !offered {
            outcome.errors.push(MirrorSyncError {
                group_id: group_id.to_string(),
                title,
                error: "预览未授权写入模式，本次未同步该组".to_string(),
            });
            continue;
        }
        selections.push(SyncSelection {
            group_id: group_id.to_string(),
            preview_token: preview_token.to_string(),
            mode,
        });
    }
    if selections.is_empty() {
        return outcome;
    }
    let group_ids: Vec<String> = selections
        .iter()
        .map(|selection| selection.group_id.clone())
        .collect();
    let report = match sync(&selections) {
        Ok(report) => report,
        // B9：写入整体失败（加锁/档位不支持/App 仍在运行）→ 一组一条，不重试。
        Err(error) => return MirrorSyncOutcome::whole_failure(&group_ids, error),
    };
    for item in array_of(&report, "synced") {
        let Some(group_id) = item.get("groupId").and_then(Value::as_str) else {
            continue;
        };
        let mode = item
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let overwritten = mode == SyncMode::Overwrite.as_str();
        let target_before = record_count_field(&item, "targetBefore");
        let target_after = record_count_field(&item, "target");
        outcome.details.push(MirrorSyncedGroup {
            group_id: group_id.to_string(),
            title: titles.get(group_id).cloned().flatten(),
            mode,
            overwritten,
            target_records_before: target_before,
            added_records: target_after.saturating_sub(target_before),
        });
    }
    // 请求写入却被判定跳过（内容其实已一致等）：不计数，但要如实上报（B9-bis 的噪音）。
    for key in ["skipped", "errors"] {
        for item in array_of(&report, key) {
            let group_id = item
                .get("groupId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let error = item
                .get("error")
                .or_else(|| item.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("同步未完成")
                .to_string();
            outcome.errors.push(MirrorSyncError {
                title: titles.get(&group_id).cloned().flatten(),
                group_id,
                error,
            });
        }
    }
    outcome.synced = outcome.details.len();
    outcome.overwritten = outcome.details.iter().filter(|item| item.overwritten).count();
    outcome
}

/// 同步报告里某个数组字段（`synced` / `skipped` / `errors`）；缺字段按空清单处理。
fn array_of(report: &Value, key: &str) -> Vec<Value> {
    report
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn record_count_field(item: &Value, key: &str) -> usize {
    item.get("recordCount")
        .and_then(|counts| counts.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

/// 整份覆盖目标账号 `MEMORY.md`：成功回填备份路径，失败折算成 `errors`（不阻断软删/复制）。
///
/// 错误项以 `MEMORY.md` 作为标识（它不是会话，不该冒用 session_id 语义）。
fn overwrite_target_memory(
    memory_root: &Path,
    mother_uid: &str,
    target_uid: &str,
) -> (Option<String>, Vec<MirrorError>) {
    let report = memory::copy_and_overwrite_to_at(memory_root, mother_uid, target_uid);
    let backup = report
        .get("written")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("backup"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let errors: Vec<MirrorError> = report
        .get("errors")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| MirrorError {
                    session_id: MEMORY_STEP_ID.to_string(),
                    error: item
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    (backup, errors)
}

/// 专家团步骤的错误项：它不是会话，因此用清单文件名作标识，不冒用 `session_id` 语义。
fn expert_step_error(error: impl Into<String>) -> MirrorError {
    MirrorError {
        session_id: experts::EXPERTS_FILE.to_string(),
        error: error.into(),
    }
}

/// 整份覆盖目标账号的专家团清单：成功回填备份路径，失败折成 `errors`（不阻断软删/复制）。
fn overwrite_target_experts(
    memory_root: &Path,
    mother_uid: &str,
    target_uid: &str,
    backup_dir: &Path,
) -> (Option<String>, bool, Vec<MirrorError>) {
    let backup_file = backup_dir.join(EXPERTS_BACKUP_FILE);
    match experts::overwrite_from_mother_at(memory_root, mother_uid, target_uid, &backup_file) {
        Ok(outcome) => (outcome.backup, outcome.written, Vec::new()),
        Err(block) => (None, false, vec![expert_step_error(block.message())]),
    }
}

/// 还原专家团清单：把备份原文整份写回（边界⑦：不做三方合并）。
///
/// 老清单没有该字段、或备份文件已不在 → 跳过且不报错（会话还原不受影响）；写回前把目标
/// 当前原文另存 `experts.json.bak-<ms>`，保留用户再反悔的可能。返回 `experts_restored`。
///
/// 备份内容必须先校验能解析为名字数组：崩溃窗口可能留下 0 字节/半写备份，直接写回会把
/// 用户清单清空还报成功。
fn restore_target_experts(
    memory_root: &Path,
    manifest: &MirrorBackupManifest,
    warnings: &mut Vec<String>,
    errors: &mut Vec<MirrorError>,
) -> bool {
    let Some(backup) = manifest.experts_backup.as_deref() else {
        return false;
    };
    let content = match std::fs::read_to_string(backup) {
        Ok(content) => content,
        Err(error) => {
            // 备份目录被清理或备份文件被删：跳过该步骤，不当作还原失败。
            warnings.push(format!("专家团备份已不可读（{error}），本次跳过专家团还原：{backup}"));
            return false;
        }
    };
    if let Err(error) = experts::parse_experts_text(&content) {
        warnings.push(format!(
            "专家团备份内容不可用（{error}），本次跳过专家团还原：{backup}"
        ));
        return false;
    }
    let target = experts::custom_dir_at(memory_root, &manifest.target_uid)
        .join(experts::EXPERTS_FILE);
    let Some(parent) = target.parent() else {
        errors.push(expert_step_error("专家团目录路径非法，无法还原"));
        return false;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        errors.push(expert_step_error(format!("创建专家目录失败：{error}")));
        return false;
    }
    if target.is_file() {
        let prior = target.with_file_name(format!("{}.bak-{}", experts::EXPERTS_FILE, now_ms()));
        if let Err(error) = std::fs::copy(&target, &prior) {
            errors.push(expert_step_error(format!(
                "备份当前专家团清单失败，已跳过还原：{error}"
            )));
            return false;
        }
    }
    match atomic_write(&target, &content) {
        Ok(()) => true,
        Err(error) => {
            errors.push(expert_step_error(format!("写回专家团清单失败：{error}")));
            false
        }
    }
}

/// 软删+复制可靠完成后，把清单原子改写为 done 并登记 copied（durable_write 不产出半写文件）。
fn finalize_mirror_manifest(
    backup_dir: &Path,
    copied: &[MirrorCopiedEntry],
    memory_backup: Option<&str>,
    experts_backup: Option<&str>,
) -> Result<(), String> {
    let file = backup_dir.join(MIRROR_MANIFEST_FILE);
    let text =
        std::fs::read_to_string(&file).map_err(|error| format!("镜像清单读取失败：{error}"))?;
    let mut manifest: MirrorBackupManifest =
        serde_json::from_str(&text).map_err(|error| format!("镜像清单解析失败：{error}"))?;
    manifest.copied = copied.to_vec();
    manifest.memory_backup = memory_backup.map(str::to_string);
    manifest.experts_backup = experts_backup.map(str::to_string);
    manifest.state = MIRROR_STATE_DONE.to_string();
    let content = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    session_backup::durable_write_str(&file, &content)
        .map_err(|error| format!("镜像清单状态更新失败：{error}"))?;
    Ok(())
}

fn load_mirror_manifest(backup_path: &Path) -> Result<MirrorBackupManifest, String> {
    let file = backup_path.join(MIRROR_MANIFEST_FILE);
    let text = std::fs::read_to_string(&file)
        .map_err(|error| format!("镜像备份清单不可读：{error}"))?;
    let manifest: MirrorBackupManifest =
        serde_json::from_str(&text).map_err(|error| format!("镜像备份清单已损坏：{error}"))?;
    if manifest.version != MIRROR_BACKUP_VERSION {
        return Err(format!("镜像备份清单版本不受支持（{}）", manifest.version));
    }
    Ok(manifest)
}

/// 精确白名单回滚：只按清单登记的 `(sessionId, userId, deletedAtWritten)` 三元组置空。
///
/// **严禁** `WHERE user_id=?` 批量置空（会误恢复用户此前自己删的会话）；**不碰正文文件**。
/// 返回的 `deleted_sessions` 表示本次还原行数，`skipped` 为未命中清单。
///
/// 专家团清单是文件步骤，无法与 DB 事务共提交：`tx.commit()` 之后单独执行，失败只折进
/// `errors`（备份目录保留可重试），`experts_restored=false` 由界面明文展示。
pub fn mirror_restore_at(
    paths: &SessionPaths,
    memory_root: &Path,
    variant: WbVariant,
    backup_path: &Path,
) -> Result<MirrorReport, String> {
    let manifest = load_mirror_manifest(backup_path)?;
    if manifest.variant != variant {
        return Err("镜像备份所属应用与当前不一致，拒绝还原".to_string());
    }
    let mut conn = open_db(&paths.workbuddy_db(), false)
        .ok_or_else(|| "会话库不可写，无法还原镜像".to_string())?;
    if !table_exists(&conn, "sessions") {
        return Err("会话库缺少 sessions 表，无法还原镜像".to_string());
    }
    session_backup::ensure_full_synchronous(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("会话数据事务开启失败：{error}"))?;
    let mut restored = 0usize;
    let mut skipped = Vec::new();
    for row in &manifest.rows {
        let affected = tx
            .execute(
                "UPDATE sessions SET deleted_at = NULL \
                 WHERE id = ?1 AND user_id = ?2 AND deleted_at = ?3",
                params![row.session_id, row.user_id, row.deleted_at_written],
            )
            .map_err(|error| format!("还原会话失败（{}）：{error}", row.session_id))?;
        if affected == 1 {
            restored += 1;
        } else {
            skipped.push(MirrorSkipped {
                session_id: row.session_id.clone(),
                reason: "会话不存在、归属不符或 deleted_at 已被改动，未还原".to_string(),
            });
        }
    }
    // 回滚本次「复制新增」的会话：同样按软删撤销（可再次还原），绝不静默留下孤儿。
    // 已被改动/不存在（如用户继续对话后 deleted_at 已非 NULL）则列入未撤销清单。
    let copy_deleted_at = now_ms();
    let mut restored_copies = 0usize;
    for entry in &manifest.copied {
        let affected = tx
            .execute(
                "UPDATE sessions SET deleted_at = ?1 \
                 WHERE id = ?2 AND user_id = ?3 AND deleted_at IS NULL",
                params![copy_deleted_at, entry.new_session_id, entry.user_id],
            )
            .map_err(|error| format!("撤销复制会话失败（{}）：{error}", entry.new_session_id))?;
        if affected == 1 {
            restored_copies += 1;
        } else {
            skipped.push(MirrorSkipped {
                session_id: entry.new_session_id.clone(),
                reason: "复制新增未撤销：会话不存在或已被改动".to_string(),
            });
        }
    }
    tx.commit()
        .map_err(|error| format!("会话数据提交失败：{error}"))?;
    // 文件侧的第三个可还原步骤：专家团清单回滚到镜像前原文（老备份缺字段则跳过）。
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let experts_restored = restore_target_experts(memory_root, &manifest, &mut warnings, &mut errors);
    Ok(MirrorReport {
        deleted_sessions: restored,
        copied_sessions: 0,
        restored_copies,
        backup: Some(backup_path.to_string_lossy().to_string()),
        needs_recovery: false,
        experts_updated: false,
        experts_restored,
        warnings,
        skipped,
        errors,
        // 还原不是镜像合并，没有「仍缺清单」语义。
        unmerged: Vec::new(),
        // 还原不跑落后副本同步，四个同步字段恒为「没有」。
        synced_sessions: 0,
        overwritten_sessions: 0,
        sync_details: Vec::new(),
        sync_errors: Vec::new(),
    })
}

/// 限根还原：canonicalize 后校验备份路径落在 `transactions_root` 之下，越界直接拒绝。
///
/// 只在 `mirror_restore_at` 之前加一道路径围栏，**不改其软删/回滚算法**。
pub fn mirror_restore_within_root(
    paths: &SessionPaths,
    memory_root: &Path,
    variant: WbVariant,
    backup_path: &Path,
) -> Result<MirrorReport, String> {
    let canonical_backup = std::fs::canonicalize(backup_path)
        .map_err(|error| format!("镜像备份路径不存在或不可访问：{error}"))?;
    // 校验落在「任一合法档位根」之下：两个档位的 transactions_root 都 canonicalize 后比对；
    // 某档位根不存在（尚无该档备份）则跳过该根。越界即拒绝，绝不触碰数据库。
    let in_range = [WbVariant::Cn, WbVariant::Ai].iter().any(|candidate| {
        let root = session_backup::transactions_root(paths, *candidate);
        std::fs::canonicalize(&root)
            .map(|canonical_root| canonical_backup.starts_with(&canonical_root))
            .unwrap_or(false)
    });
    if !in_range {
        return Err("备份路径不在允许的镜像备份目录内，已拒绝还原".to_string());
    }
    // manifest 内部会再校验 `manifest.variant == variant`，故落到此处的路径与档位一致。
    mirror_restore_at(paths, memory_root, variant, &canonical_backup)
}

/// 一处镜像备份的可观测摘要（扫描用）：状态、绝对路径、目标账号与软删行数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorBackupEntry {
    pub operation_id: String,
    pub variant: WbVariant,
    /// `pending`（崩溃窗口/未确认完成）或 `done`（软删已 commit 且读回校验通过）。
    pub state: String,
    /// 备份目录绝对路径（一键还原用）。
    pub backup: String,
    pub created_at: i64,
    pub target_uid: String,
    /// 本次写入的 `deleted_at`（各行一致；空清单为 None）。
    pub deleted_at: Option<i64>,
    /// 清单登记的行数（本次软删量）。
    pub deleted_sessions: usize,
}

/// 扫描所有镜像备份清单：暴露 `state`，让 UI 与启动恢复能发现「库已改但无感知」的窄窗口。
/// 按创建时间倒序；损坏/非镜像清单跳过（不 panic、不误报）。
pub fn scan_mirror_backups(paths: &SessionPaths) -> Vec<MirrorBackupEntry> {
    let root = paths
        .backup_root()
        .join(session_backup::TRANSACTIONS_DIR_NAME);
    let Ok(variant_dirs) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for variant_dir in variant_dirs.flatten() {
        let Ok(op_dirs) = std::fs::read_dir(variant_dir.path()) else {
            continue;
        };
        for op_dir in op_dirs.flatten() {
            let dir = op_dir.path();
            let Ok(text) = std::fs::read_to_string(dir.join(MIRROR_MANIFEST_FILE)) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<MirrorBackupManifest>(&text) else {
                continue;
            };
            entries.push(MirrorBackupEntry {
                operation_id: manifest.operation_id,
                variant: manifest.variant,
                state: manifest.state,
                backup: dir.to_string_lossy().to_string(),
                created_at: manifest.created_at,
                target_uid: manifest.target_uid,
                deleted_at: manifest.rows.first().map(|row| row.deleted_at_written),
                deleted_sessions: manifest.rows.len(),
            });
        }
    }
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    entries
}

// ---------------------------------------------------------------------------
// 红测（批次①：零 IO 副作用、契约字段、STALE 六条、隔离与判重）
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// 删除同步：某账号删掉的会话，在其他仍保留副本的账号上也软删
// ---------------------------------------------------------------------------

/// 删除同步的只读预览：列出「别处已删、这里还在」的会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedSyncPreview {
    /// 逐条待软删清单，界面不得折叠。
    pub targets: Vec<MirrorTarget>,
    /// 待软删会话数（与 `targets.len()` 一致）。
    pub session_count: usize,
    /// 涉及多少个账号。
    pub account_count: usize,
    /// 待删集合的规范化摘要；执行前现场重算，不符零写入。
    pub expected_digest: String,
}

/// 执行计划（前端回传）。服务端会整份重算，篡改无效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedSyncPlan {
    pub targets: Vec<MirrorTarget>,
    pub expected_digest: String,
}

/// 读**全部账号**的会话行，**包含已软删的**。
///
/// 与 `read_target_rows` 的关键差别：不能过滤 `deleted_at IS NULL`。
/// 「别的账号删了」这个事实只能从已软删的行（或行已不存在）里读出来。
fn read_all_rows(conn: &Connection) -> Result<Vec<MirrorRow>, String> {
    if !table_exists(conn, "sessions") {
        return Ok(Vec::new());
    }
    let custom_expr = if column_exists(conn, "sessions", "custom_title") {
        "custom_title"
    } else {
        "NULL"
    };
    let deleted_expr = if column_exists(conn, "sessions", "deleted_at") {
        "deleted_at"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT id, user_id, title, {custom_expr}, updated_at, {deleted_expr}, {} \
         FROM sessions ORDER BY id",
        activity_expr(conn)
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| format!("删除同步：会话清单查询失败：{error}"))?;
    let mapped = stmt
        .query_map([], |row| {
            Ok(MirrorRow {
                session_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                user_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                title: row.get::<_, Option<String>>(2)?,
                custom_title: row.get::<_, Option<String>>(3)?,
                updated_at: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                deleted_at: row.get::<_, Option<i64>>(5)?,
                activity_at: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            })
        })
        .map_err(|error| format!("删除同步：会话清单读取失败：{error}"))?;
    let mut rows = Vec::new();
    for row in mapped {
        rows.push(row.map_err(|error| format!("删除同步：会话行读取失败：{error}"))?);
    }
    Ok(rows)
}

/// 计算待软删集合：某个关联组里**既有已删/已不存在的成员，也有仍活跃的成员**时，
/// 把该组剩下的活跃成员全部纳入。
///
/// 只对已建立关联组的会话生效——没有关联就没有「同一段对话」的可靠证据，
/// 按标题猜会误删。`members < 2` 的组直接跳过（单条无从对比）。
pub fn deleted_sync_targets(rows: &[MirrorRow], groups: &[LinkGroup]) -> Vec<MirrorTarget> {
    let mut alive: HashSet<(&str, &str)> = HashSet::new();
    let mut by_key: HashMap<(&str, &str), &MirrorRow> = HashMap::new();
    for row in rows {
        by_key.insert((row.user_id.as_str(), row.session_id.as_str()), row);
        if row.deleted_at.is_none() {
            alive.insert((row.user_id.as_str(), row.session_id.as_str()));
        }
    }

    let mut targets: Vec<MirrorTarget> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for group in groups {
        let members: Vec<_> = group
            .members
            .iter()
            .filter(|member| member.state == MemberState::Active)
            .collect();
        if members.len() < 2 {
            continue;
        }
        let mut any_alive = false;
        let mut any_gone = false;
        for member in &members {
            if alive.contains(&(member.uid.as_str(), member.session_id.as_str())) {
                any_alive = true;
            } else {
                any_gone = true;
            }
        }
        if !(any_alive && any_gone) {
            continue;
        }
        for member in members {
            let key = (member.uid.clone(), member.session_id.clone());
            if !alive.contains(&(key.0.as_str(), key.1.as_str())) {
                continue;
            }
            if !seen.insert(key.clone()) {
                continue;
            }
            let Some(row) = by_key.get(&(key.0.as_str(), key.1.as_str())) else {
                continue;
            };
            targets.push(MirrorTarget {
                session_id: row.session_id.clone(),
                title: display_title(row.title.clone(), row.custom_title.clone()),
                user_id: row.user_id.clone(),
                group_id: group.id.clone(),
                updated_at: row.updated_at,
                full_digest: session_row_digest(row),
                kind: "session".to_string(),
            });
        }
    }
    targets.sort_by(|a, b| a.user_id.cmp(&b.user_id).then(a.session_id.cmp(&b.session_id)));
    targets
}

/// 只读扫描：全库 + 关联组，列出「别处已删、这里还在」的会话。
pub fn deleted_sync_preview_at(paths: &SessionPaths) -> Result<DeletedSyncPreview, String> {
    let conn = open_db(&paths.workbuddy_db(), true)
        .ok_or_else(|| "会话库不可读，无法扫描删除同步".to_string())?;
    let rows = read_all_rows(&conn)?;
    let groups = load_groups(paths);
    let targets = deleted_sync_targets(&rows, &groups);
    let account_count = targets
        .iter()
        .map(|target| target.user_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    Ok(DeletedSyncPreview {
        expected_digest: expected_digest_of(&targets),
        session_count: targets.len(),
        account_count,
        targets,
    })
}

/// 执行删除同步：现场重算 → 摘要比对 → 先备份 → 逐账号软删 → 读回校验。
///
/// 备份复用镜像的清单格式（`mother_uid` 留空、不复制、不动记忆），
/// 因此现有的「还原」入口可以直接撤销本次删除。
pub fn deleted_sync_apply_at(
    paths: &SessionPaths,
    variant: WbVariant,
    plan: &DeletedSyncPlan,
) -> Result<MirrorReport, String> {
    let mut conn = open_db(&paths.workbuddy_db(), false)
        .ok_or_else(|| "会话库不可读，无法执行删除同步".to_string())?;
    let rows = read_all_rows(&conn)?;
    let groups = load_groups(paths);
    let live_targets = deleted_sync_targets(&rows, &groups);
    if expected_digest_of(&live_targets) != plan.expected_digest {
        return Err(
            "待删集合与预览不一致（期间有会话被删或改动），已停止且零写入；请重新扫描".to_string(),
        );
    }
    if live_targets.is_empty() {
        return Ok(MirrorReport {
            deleted_sessions: 0,
            copied_sessions: 0,
            restored_copies: 0,
            backup: None,
            needs_recovery: false,
            experts_updated: false,
            experts_restored: false,
            warnings: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
            // 删除同步不涉及母本合并，没有「仍缺清单」语义。
            unmerged: Vec::new(),
            // 删除同步也不跑落后副本同步，四个同步字段恒为「没有」。
            synced_sessions: 0,
            overwritten_sessions: 0,
            sync_details: Vec::new(),
            sync_errors: Vec::new(),
        });
    }

    let deleted_at_written = now_ms();
    let operation_id = uuid::Uuid::new_v4().to_string();
    let backup_plan = MirrorPlan {
        mother_uid: String::new(),
        targets: plan.targets.clone(),
        expected_digest: plan.expected_digest.clone(),
        preview_token: String::new(),
    };
    let backup_dir = write_mirror_manifest(
        paths,
        variant,
        &operation_id,
        "",
        "",
        &backup_plan,
        &groups,
        &live_targets,
        deleted_at_written,
        // 删除同步不碰专家团：清单里不登记任何名字，还原时自然跳过该步骤。
        &[],
    )?;

    let mut by_uid: BTreeMap<String, Vec<MirrorTarget>> = BTreeMap::new();
    for target in &live_targets {
        by_uid
            .entry(target.user_id.clone())
            .or_default()
            .push(target.clone());
    }

    let mut deleted = 0usize;
    let mut skipped = Vec::new();
    for (uid, items) in &by_uid {
        let (count, mut skip) = soft_delete_targets(&mut conn, uid, items, deleted_at_written)?;
        deleted += count;
        skipped.append(&mut skip);
    }
    for (uid, items) in &by_uid {
        verify_soft_deleted(&conn, uid, items, deleted_at_written).map_err(|detail| {
            format!(
                "已落盘但持久化证明缺失：{detail}；备份保留在 {}，请勿反向写回",
                backup_dir.display()
            )
        })?;
    }

    let mut warnings = Vec::new();
    if let Err(error) = finalize_mirror_manifest(&backup_dir, &[], None, None) {
        warnings.push(format!(
            "删除同步已完成但清单状态标记失败（{error}）；备份仍在 {}，扫描会显示为 pending",
            backup_dir.display()
        ));
    }

    Ok(MirrorReport {
        deleted_sessions: deleted,
        copied_sessions: 0,
        restored_copies: 0,
        backup: Some(backup_dir.to_string_lossy().to_string()),
        needs_recovery: false,
        experts_updated: false,
        experts_restored: false,
        warnings,
        skipped,
        errors: Vec::new(),
        // 删除同步不涉及母本合并，没有「仍缺清单」语义。
        unmerged: Vec::new(),
        // 删除同步也不跑落后副本同步，四个同步字段恒为「没有」。
        synced_sessions: 0,
        overwritten_sessions: 0,
        sync_details: Vec::new(),
        sync_errors: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::config::now_ms;
    use crate::modules::session::LinkNamespace;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wb-mirror-{tag}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn paths_for(dir: &TempDir) -> SessionPaths {
        SessionPaths {
            store_root: dir.path().join("store"),
            data_root: dir.path().join("data"),
            auth_file: dir.path().join("auth.json"),
            link_namespace: LinkNamespace::WorkBuddy,
        }
    }

    fn group(id: &str, members: Vec<(&str, &str, MemberState)>) -> LinkGroup {
        LinkGroup {
            id: id.to_string(),
            variant: WbVariant::Cn,
            created_at: 0,
            members: members
                .into_iter()
                .map(|(uid, session_id, state)| session_link::LinkMember {
                    member_id: format!("{id}-{uid}-{session_id}"),
                    account_id: None,
                    uid: uid.to_string(),
                    session_id: session_id.to_string(),
                    variant: Some(WbVariant::Cn),
                    state,
                    linked_at: 0,
                    last_synced_at: None,
                })
                .collect(),
            pair_bases: Vec::new(),
        }
    }

    fn target(session_id: &str, group_id: &str, digest: &str, updated_at: i64) -> MirrorTarget {
        MirrorTarget {
            session_id: session_id.to_string(),
            title: format!("标题-{session_id}"),
            user_id: "u-target".to_string(),
            group_id: group_id.to_string(),
            updated_at,
            full_digest: digest.to_string(),
            kind: "session".to_string(),
        }
    }

    fn baseline_binding() -> (Vec<LinkGroup>, Vec<MirrorTarget>, MirrorBinding) {
        let groups = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-mother", MemberState::Active),
                ("u-target", "s-1", MemberState::Active),
            ],
        )];
        let targets = vec![target("s-1", "g-1", "digest-1", 1000)];
        let binding = build_mirror_binding(
            "u-mother",
            "u-target",
            &groups,
            &HashMap::new(),
            &targets,
            vec!["g-1".to_string()],
            vec!["g-1".to_string()],
            true,
        );
        (groups, targets, binding)
    }

    // --- 红测 1：STALE 六条逐条触发 + 一致时为空 ---------------------------

    #[test]
    fn verify_mirror_binding_consistent_binding_has_no_stale() {
        let (_, _, binding) = baseline_binding();
        assert!(
            verify_mirror_binding(&binding, &binding.clone()).is_empty(),
            "现场一致时不得报 STALE"
        );
    }

    #[test]
    fn verify_mirror_binding_reports_six_stale_rules() {
        let (_, _, expected) = baseline_binding();
        let cases: Vec<(&str, MirrorBinding, &str)> = vec![
            (
                "motherUid 变化",
                MirrorBinding {
                    mother_uid: "u-other".to_string(),
                    ..expected.clone()
                },
                "母本账号已变化",
            ),
            (
                "母本 active 成员集合变化",
                MirrorBinding {
                    mother_active_fingerprint: "changed".to_string(),
                    ..expected.clone()
                },
                "关联会话集合已变化",
            ),
            (
                "待删会话 fullDigest/updated_at 变化",
                MirrorBinding {
                    targets: vec![target("s-1", "g-1", "digest-changed", 1000)],
                    ..expected.clone()
                },
                "内容或更新时间已变化",
            ),
            (
                "待删会话软删状态变化",
                MirrorBinding {
                    all_targets_active: false,
                    ..expected.clone()
                },
                "已被软删或移除",
            ),
            (
                "group_id 查不到",
                MirrorBinding {
                    found_group_ids: Vec::new(),
                    ..expected.clone()
                },
                "关联组已不存在",
            ),
            (
                "母本 uid 不在该组成员中",
                MirrorBinding {
                    groups_with_mother: Vec::new(),
                    ..expected.clone()
                },
                "已不在该组成员中",
            ),
        ];
        assert_eq!(cases.len(), 6, "施工单要求恰好六条 STALE");
        for (name, live, keyword) in cases {
            let result = verify_mirror_binding(&expected, &live);
            assert_eq!(result.len(), 1, "{name} 应只触发一条：{result:?}");
            assert_eq!(result[0].reason_code, REASON_PREVIEW_STALE, "{name}");
            assert!(
                result[0].detail.contains(keyword),
                "{name} 文案缺失：{:?}",
                result[0].detail
            );
        }
    }

    // --- 红测 2：清单 user_id 隔离 + 母本 uid 不进清单 + 字段齐备 ----------

    #[test]
    fn mirror_targets_isolates_user_id_and_lists_every_field() {
        let groups = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-m-gone", MemberState::Active),
                ("u-target", "s-1", MemberState::Active),
            ],
        )];
        let rows = vec![
            MirrorRow {
                session_id: "s-1".to_string(),
                user_id: "u-target".to_string(),
                title: Some("原名".to_string()),
                custom_title: Some("改名".to_string()),
                updated_at: 1000,
                activity_at: 1000,
                deleted_at: None,
            },
            MirrorRow {
                session_id: "s-mother".to_string(),
                user_id: "u-mother".to_string(),
                title: Some("母本会话".to_string()),
                custom_title: None,
                updated_at: 1000,
                activity_at: 1000,
                deleted_at: None,
            },
            MirrorRow {
                session_id: "s-third".to_string(),
                user_id: "u-third".to_string(),
                title: Some("第三方".to_string()),
                custom_title: None,
                updated_at: 1000,
                activity_at: 1000,
                deleted_at: None,
            },
            MirrorRow {
                session_id: "s-deleted".to_string(),
                user_id: "u-target".to_string(),
                title: Some("已软删".to_string()),
                custom_title: None,
                updated_at: 1000,
                activity_at: 1000,
                deleted_at: Some(999),
            },
        ];
        let targets = mirror_targets_from_rows(&rows, "u-target", "u-mother", &groups, &HashMap::new());
        assert_eq!(targets.len(), 1, "只列目标 uid 且未软删的行");
        let only = &targets[0];
        assert_eq!(only.session_id, "s-1");
        assert_eq!(only.user_id, "u-target");
        assert_eq!(only.title, "改名", "custom_title 优先展示");
        assert_eq!(only.group_id, "g-1");
        assert_eq!(only.updated_at, 1000);
        assert_eq!(only.kind, "session");
        assert!(!only.full_digest.is_empty(), "fullDigest 必须齐备");
        assert!(
            targets.iter().all(|t| t.session_id != "s-mother"),
            "母本 uid 绝不进清单"
        );
    }

    #[test]
    fn ungrouped_target_rows_are_kept_with_empty_group_id() {
        // I5：目标行不在任何组里 → 无从证明其归属母本，一律保留（D1：不做「母本没有就删」）。
        // `groupId` 仍按 `group_id_for` 口径为空串（该字段供备份清单与回显使用）。
        let rows = vec![MirrorRow {
            session_id: "s-private".to_string(),
            user_id: "u-target".to_string(),
            title: None,
            custom_title: None,
            updated_at: 1,
            activity_at: 1,
            deleted_at: None,
        }];
        let targets = mirror_targets_from_rows(&rows, "u-target", "u-mother", &[], &HashMap::new());
        assert!(targets.is_empty(), "无组目标行必须保留：{targets:?}");
        assert_eq!(
            group_id_for(&[], "u-target", "s-private"),
            "",
            "未建组会话 groupId 为空"
        );
        assert_eq!(display_title(None, None), "(无标题)");
    }

    // --- 红测 2b：只跟着母本删（I1/I2/I5 + Bug A 判据） --------------------

    /// 母本会话状态快照：`alive` 为存活 id，`gone` 为已软删（`deleted_at` 时刻）的 id；
    /// 两者都没列出的 id 即「物理不存在」，与已软删等价（Spec §1.2）。
    fn mother_states(alive: &[&str], gone: &[(&str, i64)]) -> HashMap<String, Option<i64>> {
        let mut states: HashMap<String, Option<i64>> =
            alive.iter().map(|id| (id.to_string(), None)).collect();
        for (id, deleted_at) in gone {
            states.insert(id.to_string(), Some(*deleted_at));
        }
        states
    }

    /// 测试用母本账号对象：uid 与各用例 seed 的登录态一致（`seed_auth(.., "u-mother")`）。
    fn mother_acc() -> Value {
        json!({"uid": "u-mother"})
    }

    fn target_row(session_id: &str) -> MirrorRow {
        MirrorRow {
            session_id: session_id.to_string(),
            user_id: "u-target".to_string(),
            title: Some(format!("标题-{session_id}")),
            custom_title: None,
            updated_at: 1000,
            activity_at: 1000,
            deleted_at: None,
        }
    }

    #[test]
    fn mirror_targets_deletes_when_every_mother_member_is_gone() {
        // P1（母本行 deleted_at 非空）与 P2（母本行物理不存在）等价，都必须进待删清单。
        for (case, states) in [
            ("已软删", mother_states(&[], &[("s-m", 999)])),
            ("物理不存在", mother_states(&[], &[])),
        ] {
            let groups = vec![group(
                "g-1",
                vec![
                    ("u-mother", "s-m", MemberState::Active),
                    ("u-target", "s-1", MemberState::Active),
                ],
            )];
            let targets = mirror_targets_from_rows(
                &[target_row("s-1")],
                "u-target",
                "u-mother",
                &groups,
                &states,
            );
            assert_eq!(targets.len(), 1, "{case}：母本成员全 Gone 时副本应进清单（Bug A）");
            assert_eq!(targets[0].session_id, "s-1", "{case}");
            assert_eq!(targets[0].group_id, "g-1", "{case}");
        }
    }

    #[test]
    fn mirror_targets_keeps_group_when_any_mother_member_still_alive() {
        // P3：一条 gone、一条 alive → 全部 gone 才算 gone（I1）。
        let groups = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-m-gone", MemberState::Active),
                ("u-mother", "s-m-alive", MemberState::Superseded),
                ("u-target", "s-1", MemberState::Active),
            ],
        )];
        let states = mother_states(&["s-m-alive"], &[("s-m-gone", 111)]);
        let targets = mirror_targets_from_rows(
            &[target_row("s-1")],
            "u-target",
            "u-mother",
            &groups,
            &states,
        );
        assert!(targets.is_empty(), "存在任一 Alive 母本成员即保留：{targets:?}");
    }

    #[test]
    fn mirror_targets_keeps_superseded_member_that_is_still_alive() {
        // P4：Superseded 描述配对生命周期，不是保留理由；它仍 Alive 才是保留理由（I2）。
        let groups = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-m", MemberState::Superseded),
                ("u-target", "s-1", MemberState::Active),
            ],
        )];
        let states = mother_states(&["s-m"], &[]);
        let targets = mirror_targets_from_rows(
            &[target_row("s-1")],
            "u-target",
            "u-mother",
            &groups,
            &states,
        );
        assert!(targets.is_empty(), "母本仍持有该对话，不得删目标副本：{targets:?}");
    }

    #[test]
    fn mirror_targets_deletes_when_active_and_superseded_all_gone() {
        // P4b：Active 与 Superseded 全部 Gone → 进清单（Superseded 本身不是保留理由）。
        let groups = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-m-act", MemberState::Active),
                ("u-mother", "s-m-sup", MemberState::Superseded),
                ("u-target", "s-1", MemberState::Active),
            ],
        )];
        let states = mother_states(&[], &[("s-m-act", 1), ("s-m-sup", 2)]);
        let targets = mirror_targets_from_rows(
            &[target_row("s-1")],
            "u-target",
            "u-mother",
            &groups,
            &states,
        );
        assert_eq!(targets.len(), 1, "全部 Gone 且非空 → 删：{targets:?}");
        assert_eq!(targets[0].session_id, "s-1");
    }

    #[test]
    fn mirror_targets_keeps_ungrouped_and_motherless_groups() {
        // I5：目标无组 → 无从证明归属，保留；组内没有母本任何成员 → 目标独有，保留。
        let grouped = vec![group(
            "g-third",
            vec![
                ("u-third", "s-third", MemberState::Active),
                ("u-target", "s-2", MemberState::Active),
            ],
        )];
        let targets = mirror_targets_from_rows(
            &[target_row("s-private"), target_row("s-2")],
            "u-target",
            "u-mother",
            &grouped,
            &mother_states(&[], &[("s-m", 9)]),
        );
        assert!(targets.is_empty(), "无组与 M(g)=∅ 一律保留：{targets:?}");
    }

    #[test]
    fn mirror_preview_soft_deletes_copy_when_mother_member_gone() {
        // Bug A 端到端：母本行已软删、组与目标副本都还在 → 预览必须给出待删条目。
        let dir = TempDir::new("bug-a");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(
            &paths,
            &[
                ("s-1", "u-target", 1000, None),
                ("s-m", "u-mother", 2000, Some(999)),
            ],
        );
        write_link_store(
            &paths,
            vec![group(
                "g-1",
                vec![
                    ("u-mother", "s-m", MemberState::Active),
                    ("u-target", "s-1", MemberState::Active),
                ],
            )],
        );
        let preview =
            mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &json!({ "uid": "u-target" })).unwrap();
        assert_eq!(preview.session_count, 1, "母本已删 → 目标副本进待删清单");
        assert_eq!(preview.targets[0].session_id, "s-1");
        assert_eq!(preview.copied_count, 0, "母本没有存活会话要复制");
    }

    #[test]
    fn mirror_preview_without_deleted_at_column_treats_all_rows_alive() {
        // Z4：旧库缺 `deleted_at` 列 → 母本所有存在行按 Alive（fail-closed：宁可少删）。
        let dir = TempDir::new("no-deleted-at");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        std::fs::create_dir_all(&paths.data_root).unwrap();
        {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, custom_title TEXT, \
                 updated_at INTEGER);\
                 INSERT INTO sessions VALUES ('s-1','u-target','目标副本',NULL,1000);\
                 INSERT INTO sessions VALUES ('s-m','u-mother','母本',NULL,2000);",
            )
            .unwrap();
        }
        write_link_store(
            &paths,
            vec![group(
                "g-1",
                vec![
                    ("u-mother", "s-m", MemberState::Active),
                    ("u-target", "s-1", MemberState::Active),
                ],
            )],
        );
        let preview =
            mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &json!({ "uid": "u-target" })).unwrap();
        assert!(
            preview.targets.is_empty(),
            "缺列时母本行恒按 Alive → 不删：{:?}",
            preview.targets
        );
    }

    // --- t5：unmerged 只读观测（组层面合并缺口） ---------------------------

    fn titles_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, title)| (id.to_string(), title.to_string()))
            .collect()
    }

    #[test]
    fn unmerged_groups_lists_group_where_only_mother_is_active() {
        let groups = vec![group(
            "g-gap",
            vec![
                ("u-mother", "s-m", MemberState::Active),
                ("u-third", "s-third", MemberState::Active),
            ],
        )];
        let unmerged = unmerged_groups(
            &groups,
            "u-mother",
            "u-target",
            &mother_states(&["s-m"], &[]),
            &titles_of(&[("s-m", "母本标题")]),
        );

        assert_eq!(unmerged.len(), 1);
        assert_eq!(unmerged[0].group_id, "g-gap");
        assert_eq!(unmerged[0].mother_session_id, "s-m");
        assert_eq!(unmerged[0].title.as_deref(), Some("母本标题"));
        assert_eq!(unmerged[0].missing_uids, vec!["u-target".to_string()]);
    }

    /// B13：目标在该组已有 `Active` 记录（哪怕副本已删）→ 不进清单，尊重删除。
    #[test]
    fn unmerged_groups_skip_group_already_registered_to_target() {
        let groups = vec![group(
            "g-linked",
            vec![
                ("u-mother", "s-m", MemberState::Active),
                ("u-target", "s-t", MemberState::Active),
            ],
        )];
        let unmerged = unmerged_groups(
            &groups,
            "u-mother",
            "u-target",
            &mother_states(&["s-m"], &[("s-t", 5_000)]),
            &HashMap::new(),
        );

        assert!(unmerged.is_empty(), "目标已登记的组不得列为未合并");
    }

    /// B12：母本在该组只有 superseded 成员 → 不进清单（没有可作源的存活母本）。
    #[test]
    fn unmerged_groups_skip_group_where_mother_is_only_superseded() {
        let groups = vec![group(
            "g-sup",
            vec![("u-mother", "s-m", MemberState::Superseded)],
        )];
        let unmerged = unmerged_groups(
            &groups,
            "u-mother",
            "u-target",
            &mother_states(&["s-m"], &[]),
            &HashMap::new(),
        );

        assert!(unmerged.is_empty(), "superseded 成员不得凑数");
    }

    /// 母本成员仍 Active 但会话已软删 → 不进清单（没有存活会话可复制）。
    #[test]
    fn unmerged_groups_skip_group_where_mother_session_is_deleted() {
        let groups = vec![group(
            "g-deleted",
            vec![("u-mother", "s-m", MemberState::Active)],
        )];
        let unmerged = unmerged_groups(
            &groups,
            "u-mother",
            "u-target",
            &mother_states(&[], &[("s-m", 7_000)]),
            &HashMap::new(),
        );

        assert!(unmerged.is_empty());
    }

    /// B11 + 幂等：空集返回空清单；同一输入重复调用结果逐字相同。
    #[test]
    fn unmerged_groups_is_empty_on_empty_input_and_stable_across_calls() {
        assert!(unmerged_groups(&[], "u-mother", "u-target", &HashMap::new(), &HashMap::new()).is_empty());

        let groups = vec![
            group("g-b", vec![("u-mother", "s-b", MemberState::Active)]),
            group("g-a", vec![("u-mother", "s-a", MemberState::Active)]),
        ];
        let states = mother_states(&["s-a", "s-b"], &[]);
        let first = unmerged_groups(&groups, "u-mother", "u-target", &states, &HashMap::new());
        let second = unmerged_groups(&groups, "u-mother", "u-target", &states, &HashMap::new());

        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|item| item.group_id.as_str()).collect::<Vec<_>>(),
            vec!["g-a", "g-b"],
            "按 group_id 排序，结果可复现"
        );
    }

    /// A10/A11/B11/B14：预览带出 `unmerged`，但它不进 `expectedDigest`，重复预览也不写盘。
    #[test]
    fn mirror_preview_reports_unmerged_without_touching_expected_digest() {
        let dir = TempDir::new("unmerged-preview");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(
            &paths,
            &[
                ("s-m", "u-mother", 2_000, None),
                ("s-gap", "u-mother", 2_100, None),
                ("s-t", "u-target", 1_500, None),
            ],
        );
        write_link_store(
            &paths,
            vec![
                group(
                    "g-linked",
                    vec![
                        ("u-mother", "s-m", MemberState::Active),
                        ("u-target", "s-t", MemberState::Active),
                    ],
                ),
                group("g-gap", vec![("u-mother", "s-gap", MemberState::Active)]),
            ],
        );
        let target_acc = json!({ "uid": "u-target" });

        let first =
            mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &target_acc).unwrap();
        let second =
            mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &target_acc).unwrap();

        assert_eq!(first.unmerged.len(), 1, "只有 g-gap 是缺口：{:?}", first.unmerged);
        assert_eq!(first.unmerged[0].group_id, "g-gap");
        assert_eq!(first.unmerged[0].title.as_deref(), Some("标题-s-gap"));
        // 摘要只由 targets 决定：unmerged 若被折进去，这里会不等。
        assert_eq!(first.expected_digest, expected_digest_of(&first.targets));
        assert_eq!(first.expected_digest, second.expected_digest, "重复预览摘要不得漂移");
        assert_eq!(first.unmerged, second.unmerged, "重复预览清单不得漂移");
    }

    // --- 红测 3：跨账号关联键判重（uid 精确匹配，绝不按标题） --------------

    #[test]
    fn count_mirror_copies_uses_link_member_uid_not_title() {
        let groups = vec![
            group(
                "g-both",
                vec![
                    ("u-mother", "same-title", MemberState::Active),
                    ("u-target", "same-title", MemberState::Active),
                ],
            ),
            group(
                "g-mother-only",
                vec![("u-mother", "same-title", MemberState::Active)],
            ),
            group(
                "g-other-uid",
                vec![("u-third", "same-title", MemberState::Active)],
            ),
        ];
        assert_eq!(
            count_mirror_copies(
                &groups,
                "u-mother",
                "u-target",
                &HashMap::from([("same-title".to_string(), None)]),
                &HashSet::new(),
            ),
            1,
            "仅母本有、目标没有的组需复制；标题相同不合并；第三方 uid 不算母本"
        );
    }

    #[test]
    fn group_id_for_matches_uid_and_session_id_exactly() {
        let groups = vec![group(
            "g-1",
            vec![("u-third", "s-1", MemberState::Active)],
        )];
        assert_eq!(
            group_id_for(&groups, "u-target", "s-1"),
            "",
            "别的 uid 的同名 session_id 不得误判归属"
        );
    }

    #[test]
    fn mother_active_fingerprint_changes_when_members_change() {
        let base = vec![group(
            "g-1",
            vec![("u-mother", "s-1", MemberState::Active)],
        )];
        let changed = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-1", MemberState::Active),
                ("u-mother", "s-2", MemberState::Active),
            ],
        )];
        assert_ne!(
            mother_active_fingerprint(&base, "u-mother"),
            mother_active_fingerprint(&changed, "u-mother")
        );
    }

    // --- 红测 4：expectedDigest 顺序无关、内容变化即变 ---------------------

    #[test]
    fn expected_digest_is_order_independent_and_content_sensitive() {
        let a = vec![
            target("s-1", "g-1", "d1", 10),
            target("s-2", "g-1", "d2", 20),
        ];
        let b = vec![
            target("s-2", "g-1", "d2", 20),
            target("s-1", "g-1", "d1", 10),
        ];
        assert_eq!(expected_digest_of(&a), expected_digest_of(&b));
        let c = vec![
            target("s-1", "g-1", "d1-changed", 10),
            target("s-2", "g-1", "d2", 20),
        ];
        assert_ne!(expected_digest_of(&a), expected_digest_of(&c));
    }

    // --- 红测 5：previewToken 绑定 motherUid（复用 save/load_preview_token）+ 旧 token 兼容 ---

    #[test]
    fn mirror_token_binds_mother_uid_and_verify_preview_detects_change() {
        let dir = TempDir::new("token");
        let paths = paths_for(&dir);
        let groups = vec![group(
            "g-1",
            vec![("u-mother", "s-m", MemberState::Active)],
        )];
        let binding = build_mirror_token_binding(
            WbVariant::Cn,
            "u-mother",
            "u-target",
            "digest-x",
            &groups,
            &HashMap::new(),
            1,
            1,
        );
        assert_eq!(binding.mother_uid.as_deref(), Some("u-mother"));
        let token_id = session_link::save_preview_token(&paths, binding.clone()).unwrap();
        let token = session_link::load_preview_token(&paths, &token_id).unwrap();
        assert_eq!(token.binding.mother_uid.as_deref(), Some("u-mother"));
        assert_eq!(token.binding.group_fingerprint, "digest-x");

        let mut drifted = token.binding.clone();
        drifted.mother_uid = Some("u-other".to_string());
        let reasons = session_link::verify_preview(&token, &drifted);
        assert!(
            reasons.iter().any(|r| r.contains("母本账号已变化")),
            "verify_preview 必须纳入 motherUid 比较：{reasons:?}"
        );
        assert!(
            session_link::verify_preview(&token, &token.binding.clone()).is_empty(),
            "现场一致零 STALE"
        );
    }

    #[test]
    fn legacy_token_without_mother_uid_deserializes_to_none() {
        // 旧逐组同步凭据没有 mother_uid 字段：serde(default) 必须得 None，零回归。
        let legacy = json!({
            "version": session_link::PREVIEW_TOKEN_VERSION,
            "previewId": "00000000-0000-0000-0000-000000000000",
            "createdAt": 1,
            "binding": {
                "variant": "cn",
                "groupId": "g-legacy",
                "groupFingerprint": "fp",
                "source": { "memberId": "m", "uid": "u-a", "sessionId": "s", "rawDigest": "", "normalizedDigest": "", "recordCount": 0 },
                "target": { "memberId": "n", "uid": "u-b", "sessionId": "t", "rawDigest": "", "normalizedDigest": "", "recordCount": 0 },
                "verdict": "identical"
            }
        });
        let token: session_link::PreviewToken = serde_json::from_value(legacy).unwrap();
        assert_eq!(token.binding.mother_uid, None);
    }

    // --- 红测 6：只读预览、零会话数据写入、幂等 ----------------------------

    #[test]
    fn mirror_preview_at_is_read_only_idempotent_and_binds_mother() {
        let dir = TempDir::new("readonly");
        let paths = paths_for(&dir);
        std::fs::create_dir_all(&paths.data_root).unwrap();
        std::fs::write(
            &paths.auth_file,
            r#"{"account":{"uid":"u-mother","label":"母本账号"}}"#,
        )
        .unwrap();
        {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, custom_title TEXT, updated_at INTEGER, deleted_at INTEGER);\
                 INSERT INTO sessions VALUES ('s-1','u-target','会话一',NULL,1000,NULL);\
                 INSERT INTO sessions VALUES ('s-2','u-target','会话二','别名',2000,NULL);\
                 INSERT INTO sessions VALUES ('s-3','u-mother','母本会话',NULL,1000,NULL);\
                 INSERT INTO sessions VALUES ('s-4','u-target','已软删',NULL,1000,999);",
            )
            .unwrap();
        }
        let target_acc = json!({ "uid": "u-target" });
        // 前端伪造 motherUid 也不能改变母本：母本恒取登录态 u-mother。
        let forged_acc = json!({ "uid": "u-target", "motherUid": "u-attacker" });
        // s-1 / s-2 的母本成员物理不存在（Gone）→ 两条都进待删清单（「只跟着母本删」）。
        seed_gone_mother_groups(&paths, &["s-1", "s-2"]);

        let snapshot = |conn: &Connection| -> Vec<(String, Option<i64>)> {
            let mut stmt = conn
                .prepare("SELECT id, deleted_at FROM sessions ORDER BY id")
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })
            .unwrap()
            .flatten()
            .collect()
        };
        let before = {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            snapshot(&conn)
        };

        let first = mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &target_acc).unwrap();
        let second = mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &forged_acc).unwrap();

        assert_eq!(first.mother_uid, "u-mother", "母本恒取登录态");
        assert_eq!(second.mother_uid, "u-mother", "前端伪造 motherUid 无效");
        assert_eq!(first.mother_label, "母本账号");
        assert_eq!(first.session_count, 2, "仅目标 uid 且未软删的两条");
        assert_eq!(first.targets.len(), 2, "清单逐条不折叠");
        assert!(first.backup_dir.is_none(), "预览阶段无备份目录");
        assert!(!first.preview_token.is_empty(), "必须签发预览凭据");
        assert_eq!(
            first.expected_digest, second.expected_digest,
            "同一现场两次预览摘要一致（幂等）"
        );
        assert_eq!(first.targets, second.targets, "同一现场清单一致（幂等）");
        assert_eq!(first.session_count, second.session_count);

        let after = {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            snapshot(&conn)
        };
        assert_eq!(before, after, "预览零会话数据写入：id 与 deleted_at 逐行不变");
        assert!(
            !paths.data_root.join("MEMORY.md").exists(),
            "预览绝不写 MEMORY.md"
        );

        let token = session_link::load_preview_token(&paths, &second.preview_token).unwrap();
        assert_eq!(token.binding.mother_uid.as_deref(), Some("u-mother"));
    }

    #[test]
    fn mirror_preview_at_rejects_same_mother_and_target() {
        let dir = TempDir::new("same");
        let paths = paths_for(&dir);
        std::fs::write(&paths.auth_file, r#"{"account":{"uid":"u-same"}}"#).unwrap();
        let error = mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &json!({ "uid": "u-same" }))
            .unwrap_err();
        assert!(error.contains("相同"), "{error}");
    }

    // --- 批次②：软删执行 + 精确白名单回滚 ---------------------------------

    fn seed_auth(paths: &SessionPaths, uid: &str) {
        std::fs::write(
            &paths.auth_file,
            format!(r#"{{"account":{{"uid":"{uid}","label":"母本账号"}}}}"#),
        )
        .unwrap();
    }

    /// 建 `sessions` 表并按 `(id, user_id, updated_at, deleted_at)` 播种；只读列齐备。
    fn seed_session_db(paths: &SessionPaths, rows: &[(&str, &str, i64, Option<i64>)]) {
        std::fs::create_dir_all(&paths.data_root).unwrap();
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (\
             id TEXT, user_id TEXT, title TEXT, custom_title TEXT, \
             updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        for (id, uid, updated, deleted) in rows {
            conn.execute(
                "INSERT INTO sessions (id, user_id, title, custom_title, updated_at, deleted_at) \
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
                rusqlite::params![id, uid, format!("标题-{id}"), updated, deleted],
            )
            .unwrap();
        }
    }

    fn deleted_at_of(paths: &SessionPaths, session_id: &str) -> Option<i64> {
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.query_row(
            "SELECT deleted_at FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .unwrap()
    }

    /// 走真实只读预览产出计划与凭据，保证执行侧拿到的是「真凭据」而非手搓。
    fn preview_plan(paths: &SessionPaths, target_uid: &str) -> MirrorPlan {
        let preview =
            mirror_preview_at(paths, &paths.data_root, WbVariant::Cn, &json!({ "uid": target_uid }))
                .unwrap();
        MirrorPlan {
            mother_uid: preview.mother_uid.clone(),
            targets: preview.targets,
            expected_digest: preview.expected_digest,
            preview_token: preview.preview_token,
        }
    }

    #[test]
    fn mirror_apply_soft_deletes_only_target_active_rows() {
        let dir = TempDir::new("apply-scope");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(
            &paths,
            &[
                ("s-1", "u-target", 1000, None),
                ("s-2", "u-target", 2000, None),
                ("s-other", "u-other", 1000, None),
                ("s-mother", "u-mother", 1000, None),
                ("s-already", "u-target", 1000, Some(999)),
            ],
        );
        seed_gone_mother_groups(&paths, &["s-1", "s-2"]);
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 2, "只列 u-target 未软删两行");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();
        assert_eq!(report.deleted_sessions, 2);
        assert!(report.backup.is_some(), "软删必须留备份");
        assert!(deleted_at_of(&paths, "s-1").is_some());
        assert!(deleted_at_of(&paths, "s-2").is_some());
        assert_eq!(deleted_at_of(&paths, "s-other"), None, "跨账号零影响");
        assert_eq!(deleted_at_of(&paths, "s-mother"), None, "母本会话零影响");
        assert_eq!(deleted_at_of(&paths, "s-already"), Some(999), "已软删行不被改写");
    }

    /// 造一条「计划自报属于 u-target」的待删条目：`user_id` 只来自计划，DB 侧另行校验。
    fn planned_target(session_id: &str) -> MirrorTarget {
        MirrorTarget {
            session_id: session_id.to_string(),
            title: format!("标题-{session_id}"),
            user_id: "u-target".to_string(),
            group_id: "g-probe".to_string(),
            updated_at: 1000,
            full_digest: "digest-probe".to_string(),
            kind: "session".to_string(),
        }
    }

    #[test]
    fn soft_delete_skips_rows_that_belong_to_another_account() {
        // N7 跨账号写入隔离（Spec §4.3）：`UPDATE` 恒带 `user_id` + `deleted_at IS NULL`
        // 双前置，计划若指向别人的会话，实际影响 0 行 → 进 `skipped`，绝不跨账号写入。
        //
        // 该分支在 E2E 不可达：`read_target_rows` 已按目标 uid 过滤，实时重算只产出目标账号
        // 自己的行（见 `mirror_apply_soft_deletes_only_target_active_rows` 里 `s-other` 永不被写）。
        // 只有并发变化或计划被篡改才会命中，所以在这一层直接构造目标入参。
        let dir = TempDir::new("n7-cross-account");
        let paths = paths_for(&dir);
        seed_session_db(
            &paths,
            &[
                ("s-own", "u-target", 1000, None),
                ("s-other", "u-other", 1000, None),
            ],
        );
        let mut conn = Connection::open(paths.workbuddy_db()).unwrap();
        let targets = vec![
            // 正向对照：真属于目标账号的行必须被写，证明下面的「0 行」不是断言失效。
            planned_target("s-own"),
            // 计划谎称属于目标账号、实际属于 u-other：必须影响 0 行。
            planned_target("s-other"),
        ];

        let (deleted, skipped) = soft_delete_targets(&mut conn, "u-target", &targets, 4242).unwrap();

        assert_eq!(deleted, 1, "只有真属于目标账号的行被写入");
        assert!(deleted_at_of(&paths, "s-own").is_some());
        assert_eq!(skipped.len(), 1, "影响 0 行必须进 skipped，不得静默忽略");
        assert_eq!(skipped[0].session_id, "s-other");
        assert!(skipped[0].reason.contains("已跳过"), "{}", skipped[0].reason);
        assert_eq!(
            deleted_at_of(&paths, "s-other"),
            None,
            "跨账号写入隔离：别人账号的行原封不动"
        );
    }

    #[test]
    fn mirror_apply_is_idempotent_after_repreview() {
        let dir = TempDir::new("apply-idem");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        seed_gone_mother_groups(&paths, &["s-1"]);
        let plan = preview_plan(&paths, "u-target");
        let first = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();
        assert_eq!(first.deleted_sessions, 1);
        let written = deleted_at_of(&paths, "s-1").unwrap();

        // 同一份计划重放必须 fail-closed（集合已变），且不重复写入、不改写已置位行。
        assert!(
            mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).is_err(),
            "同一计划重放应被拒绝而非再次软删"
        );
        assert_eq!(deleted_at_of(&paths, "s-1"), Some(written), "重放不动已置位行");

        // 重新预览：目标已空 → 第二次 deleted=0，且不建空备份。
        let empty_plan = preview_plan(&paths, "u-target");
        assert!(empty_plan.targets.is_empty());
        let second = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &empty_plan).unwrap();
        assert_eq!(second.deleted_sessions, 0);
        assert!(second.backup.is_none(), "空清单不建备份目录");
    }

    #[test]
    fn mirror_apply_rejects_digest_mismatch_with_zero_write() {
        let dir = TempDir::new("apply-digest");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let mut plan = preview_plan(&paths, "u-target");
        plan.expected_digest = "tampered-digest".to_string();

        let error = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap_err();
        assert!(error.contains("不一致"), "{error}");
        assert_eq!(deleted_at_of(&paths, "s-1"), None, "摘要不符零写入");
    }

    #[test]
    fn mirror_apply_rejects_when_mother_changed_with_zero_write() {
        let dir = TempDir::new("apply-mother");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        // 预览后登录态被切到别的母本：现场母本与凭据不符，双 verify 必报 STALE。
        seed_auth(&paths, "u-attacker");

        let error = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap_err();
        assert!(error.contains("过期"), "{error}");
        assert_eq!(deleted_at_of(&paths, "s-1"), None, "母本变化零写入");
    }

    #[test]
    fn mirror_restore_only_reverts_this_write_not_prior_deletes() {
        let dir = TempDir::new("restore-scope");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(
            &paths,
            &[
                ("s-1", "u-target", 1000, None),
                // 用户此前自己删的会话：deleted_at=999，绝不因本次镜像回滚被误恢复。
                ("s-prior", "u-target", 1000, Some(999)),
            ],
        );
        seed_gone_mother_groups(&paths, &["s-1"]);
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 1, "只列未软删的 s-1");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();
        assert_eq!(report.deleted_sessions, 1);
        let backup = PathBuf::from(report.backup.clone().unwrap());
        assert!(deleted_at_of(&paths, "s-1").is_some());

        let restored = mirror_restore_at(&paths, &paths.data_root, WbVariant::Cn, &backup).unwrap();
        assert_eq!(restored.deleted_sessions, 1, "只还原本次写入一行");
        assert_eq!(deleted_at_of(&paths, "s-1"), None, "本次软删被还原");
        assert_eq!(
            deleted_at_of(&paths, "s-prior"),
            Some(999),
            "禁止批量置空：用户此前自删必须保留"
        );
    }

    #[test]
    fn mirror_apply_does_not_touch_session_body_files() {
        let dir = TempDir::new("apply-body");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let project = paths.projects_dir().join("proj-a");
        std::fs::create_dir_all(&project).unwrap();
        let body = project.join("sessions.jsonl");
        std::fs::write(&body, b"{\"id\":\"s-1\",\"content\":\"keep me\"}\n").unwrap();
        let before = std::fs::read(&body).unwrap();

        let plan = preview_plan(&paths, "u-target");
        mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();

        let after = std::fs::read(&body).unwrap();
        assert_eq!(before, after, "镜像只软删数据库行，绝不改会话正文");
    }

    #[test]
    fn mirror_restore_within_root_accepts_in_root_backup() {
        let dir = TempDir::new("restore-inroot");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        seed_gone_mother_groups(&paths, &["s-1"]);
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();
        let backup = PathBuf::from(report.backup.clone().unwrap());

        let restored =
            mirror_restore_within_root(&paths, &paths.data_root, WbVariant::Cn, &backup).unwrap();
        assert_eq!(restored.deleted_sessions, 1, "根内路径可还原");
        assert_eq!(deleted_at_of(&paths, "s-1"), None);
    }

    #[test]
    fn mirror_restore_rejects_path_outside_root() {
        let dir = TempDir::new("restore-outside");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        let plan = preview_plan(&paths, "u-target");
        mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap(); // 确保根目录存在

        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let error =
            mirror_restore_within_root(&paths, &paths.data_root, WbVariant::Cn, &outside)
                .unwrap_err();
        assert!(error.contains("不在允许"), "{error}");
    }

    #[test]
    fn mirror_scan_lists_backups_with_done_state() {
        let dir = TempDir::new("scan");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        seed_gone_mother_groups(&paths, &["s-1"]);
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();

        let entries = scan_mirror_backups(&paths);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.state, "done", "软删成功后清单应为 done");
        assert_eq!(entry.deleted_sessions, 1);
        assert_eq!(entry.target_uid, "u-target");
        assert_eq!(Some(entry.backup.clone()), report.backup);
        assert_eq!(
            entry.deleted_at,
            deleted_at_of(&paths, "s-1"),
            "扫描暴露本次写入的 deletedAt"
        );
    }

    #[test]
    fn mirror_manifest_state_pending_then_done() {
        // 缺省（无 state 字段）的清单按 pending 处理（保守：提示可还原，不当作已完成）。
        let legacy = json!({
            "version": 1, "operationId": "op", "variant": "cn", "motherUid": "m",
            "targetUid": "t", "previewToken": "p", "expectedDigest": "d",
            "createdAt": 1, "rows": [], "restoreSteps": []
        });
        let manifest: MirrorBackupManifest = serde_json::from_value(legacy).unwrap();
        assert_eq!(manifest.state, "pending", "缺省清单按 pending");

        // apply 成功后落盘清单必须为 done。
        let dir = TempDir::new("state");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        seed_gone_mother_groups(&paths, &["s-1"]);
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();
        let file = PathBuf::from(report.backup.unwrap()).join(MIRROR_MANIFEST_FILE);
        let on_disk: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(on_disk.state, "done", "软删成功后清单应改写为 done");
    }

    #[test]
    fn scan_ignores_non_mirror_transactions() {
        let dir = TempDir::new("scan-nonmirror");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        seed_gone_mother_groups(&paths, &["s-1"]);
        let plan = preview_plan(&paths, "u-target");
        mirror_apply_at(&paths, WbVariant::Cn, "u-target", &json!({"uid": "u-target"}), &mother_acc(), &paths.data_root, &plan).unwrap();

        // 另造一个非镜像交易目录（只有不属于 mirror 的清单），扫描必须忽略。
        let other = session_backup::transactions_root(&paths, WbVariant::Cn).join("other-op");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("manifest.json"), "{}").unwrap();

        let entries = scan_mirror_backups(&paths);
        assert_eq!(entries.len(), 1, "只返回 mirror manifest，不含其它交易目录");
    }

    // --- 批次③a：复制母本独有会话 + 复制可回滚 -----------------------------

    fn write_link_store(paths: &SessionPaths, groups: Vec<LinkGroup>) {
        let store = session_link::LinkStore {
            version: session_link::LINK_STORE_VERSION,
            revision: 1,
            groups,
        };
        let file = paths.session_links_file();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(file, serde_json::to_string(&store).unwrap()).unwrap();
    }

    /// 给每个目标行配一个「母本侧已删」的关联组（`m-<id>` 在 `sessions` 里物理不存在 = `Gone`）。
    ///
    /// 0.1.82 的镜像判据是「只跟着母本删」（Spec D1）：目标行若不在任何组、或所属组里仍有存活
    /// 母本成员，一律保留。因此凡是要断言「会被软删」的用例，都必须先造出母本侧已删的证据。
    fn seed_gone_mother_groups(paths: &SessionPaths, target_session_ids: &[&str]) {
        let mother_ids: Vec<String> = target_session_ids
            .iter()
            .map(|session_id| format!("m-{session_id}"))
            .collect();
        let groups: Vec<LinkGroup> = target_session_ids
            .iter()
            .zip(mother_ids.iter())
            .map(|(target_id, mother_id)| {
                group(
                    &format!("g-{target_id}"),
                    vec![
                        ("u-mother", mother_id.as_str(), MemberState::Active),
                        ("u-target", target_id, MemberState::Active),
                    ],
                )
            })
            .collect();
        write_link_store(paths, groups);
    }

    /// 可复制环境：源账号 u-mother 有带正文的 s-mother，目标 u-target 空；关联组 g-1 只有母本。
    fn copy_ready_env(dir: &TempDir) -> SessionPaths {
        let paths = paths_for(dir);
        seed_auth(&paths, "u-mother");
        std::fs::create_dir_all(&paths.data_root).unwrap();
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, custom_title TEXT, \
             updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('s-mother','u-mother','母本会话',NULL,2000,NULL)",
            [],
        )
        .unwrap();
        drop(conn);
        let econn = Connection::open(paths.edge_sync_db(WbVariant::Cn)).unwrap();
        econn
            .execute_batch(
                "CREATE TABLE edge_sync_mapping (session_id TEXT, conversation_id TEXT, \
                 msg_channel TEXT, created_at INTEGER);",
            )
            .unwrap();
        drop(econn);
        let proj = paths.projects_dir().join("ws-a");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("s-mother.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type":"user","sessionId":"s-mother","text":"你好"}),
                json!({"type":"assistant","sessionId":"s-mother","text":"hi"})
            ),
        )
        .unwrap();
        write_link_store(
            &paths,
            vec![group("g-1", vec![("u-mother", "s-mother", MemberState::Active)])],
        );
        paths
    }

    fn target_active_count(paths: &SessionPaths, uid: &str) -> i64 {
        Connection::open(paths.workbuddy_db())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE user_id=?1 AND deleted_at IS NULL",
                [uid],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn missing_mirror_session_ids_excludes_linked() {
        let groups = vec![
            group("g-mother-only", vec![("u-mother", "s-m", MemberState::Active)]),
            group(
                "g-both",
                vec![
                    ("u-mother", "s-m2", MemberState::Active),
                    ("u-target", "s-t2", MemberState::Active),
                ],
            ),
        ];
        assert_eq!(
            missing_mirror_session_ids(
                &groups,
                "u-mother",
                "u-target",
                &HashMap::from([("s-m".to_string(), None), ("s-m2".to_string(), None)]),
                &HashSet::new(),
            ),
            vec!["s-m".to_string()],
            "只算母本独有组；已关联（g-both）不算缺失"
        );
    }

    // --- 批次③b：无关联组母本存活会话按 id 复制（I4 / Bug B） --------------

    /// 无组可复制环境：源账号 u-mother 有 3 条**没有任何关联组**的存活会话（Bug B 现场形态），
    /// 每条都带正文；目标 u-target 空；关联组列表为空（Z1）。
    fn copy_ready_ungrouped_env(dir: &TempDir) -> SessionPaths {
        let paths = paths_for(dir);
        seed_auth(&paths, "u-mother");
        std::fs::create_dir_all(&paths.data_root).unwrap();
        let ids = ["s-loose-a", "s-loose-b", "s-loose-c"];
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, custom_title TEXT, \
             updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        for id in ids {
            conn.execute(
                "INSERT INTO sessions VALUES (?1,'u-mother',?2,NULL,2000,NULL)",
                rusqlite::params![id, format!("标题-{id}")],
            )
            .unwrap();
        }
        drop(conn);
        let econn = Connection::open(paths.edge_sync_db(WbVariant::Cn)).unwrap();
        econn
            .execute_batch(
                "CREATE TABLE edge_sync_mapping (session_id TEXT, conversation_id TEXT, \
                 msg_channel TEXT, created_at INTEGER);",
            )
            .unwrap();
        drop(econn);
        let proj = paths.projects_dir().join("ws-a");
        std::fs::create_dir_all(&proj).unwrap();
        for id in ids {
            std::fs::write(
                proj.join(format!("{id}.jsonl")),
                format!(
                    "{}\n{}\n",
                    json!({"type":"user","sessionId":id,"text":"你好"}),
                    json!({"type":"assistant","sessionId":id,"text":"hi"})
                ),
            )
            .unwrap();
        }
        write_link_store(&paths, Vec::new());
        paths
    }

    fn mother_rows(paths: &SessionPaths) -> Vec<(String, Option<i64>, i64)> {
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, deleted_at, updated_at FROM sessions \
                 WHERE user_id = 'u-mother' ORDER BY id",
            )
            .unwrap();
        let rows: Vec<(String, Option<i64>, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .flatten()
            .collect();
        rows
    }

    #[test]
    fn missing_mirror_session_ids_includes_ungrouped_mother_sessions() {
        // P5/I4：母本 3 条无组存活会话必须全部进复制清单；已软删的无组会话不复制。
        let states = mother_states(
            &["loose-a", "loose-b", "loose-c"],
            &[("loose-dead", 999)],
        );
        assert_eq!(
            missing_mirror_session_ids(&[], "u-mother", "u-target", &states, &HashSet::new()),
            vec![
                "loose-a".to_string(),
                "loose-b".to_string(),
                "loose-c".to_string()
            ],
            "只遍历组的旧写法会漏掉这 3 条"
        );
        assert_eq!(
            count_mirror_copies(&[], "u-mother", "u-target", &states, &HashSet::new()),
            3,
            "copiedCount 与 missing 同源"
        );
    }

    #[test]
    fn missing_mirror_session_ids_skips_ids_target_already_has() {
        // P6：目标已存活存在的 id 不重复复制。
        let states = mother_states(&["loose-a", "loose-b"], &[]);
        let target_alive: HashSet<String> = HashSet::from(["loose-a".to_string()]);
        assert_eq!(
            missing_mirror_session_ids(&[], "u-mother", "u-target", &states, &target_alive),
            vec!["loose-b".to_string()]
        );
    }

    #[test]
    fn mother_mirror_fingerprint_tracks_ungrouped_and_gone_states() {
        let groups = vec![group(
            "g-1",
            vec![
                ("u-mother", "s-m", MemberState::Active),
                ("u-target", "s-1", MemberState::Active),
            ],
        )];
        let base = mother_states(&["s-m", "loose"], &[]);
        let fingerprint =
            |states: &HashMap<String, Option<i64>>| mother_mirror_fingerprint(&groups, "u-mother", states);

        assert_eq!(fingerprint(&base.clone()), fingerprint(&base), "同现场恒等");
        assert_ne!(
            fingerprint(&mother_states(&["s-m", "loose", "loose-2"], &[])),
            fingerprint(&base),
            "母本无组存活会话新增必须改变指纹（I4）"
        );
        assert_ne!(
            fingerprint(&mother_states(&["loose"], &[("s-m", 999)])),
            fingerprint(&base),
            "母本成员转 Gone 必须改变指纹（Bug A 触发点）"
        );
        assert_ne!(
            fingerprint(&base),
            mother_active_fingerprint(&groups, "u-mother"),
            "合并指纹恒不等于旧指纹 → 升级后旧 token 自然失配"
        );
    }

    #[test]
    fn mirror_preview_and_apply_copy_ungrouped_mother_sessions() {
        // Z1 + P5 端到端：`groups == []` 时目标行全部保留，母本全部存活会话都要复制。
        let dir = TempDir::new("copy-loose");
        let paths = copy_ready_ungrouped_env(&dir);
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});

        let preview =
            mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &json!({"uid": "u-target"})).unwrap();
        assert!(preview.targets.is_empty(), "没有母本已删证据 → 一条都不删");
        assert_eq!(preview.copied_count, 3, "3 条无组存活会话全部计入（Bug B）");

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &target_acc, &mother_acc(),
            &paths.data_root,
            &plan,
        )
        .unwrap();
        assert_eq!(report.copied_sessions, 3, "预览「将复制」与实际复制必须一致");
        assert_eq!(target_active_count(&paths, "u-target"), 3, "目标出现 3 条副本");

        let again = mirror_preview_at(&paths, &paths.data_root, WbVariant::Cn, &json!({"uid": "u-target"})).unwrap();
        assert_eq!(again.copied_count, 0, "复制后建组 → 不再算缺失，不重复复制");
    }

    #[test]
    fn mirror_copy_leaves_mother_side_untouched() {
        // I8/P9：复制只读源——母本 sessions 行、正文 jsonl、MEMORY.md 全部零写入。
        let dir = TempDir::new("copy-readonly-src");
        let paths = copy_ready_ungrouped_env(&dir);
        let memory_root = dir.path().join("memory-root");
        let mother_dir = memory::personal_dir_at(&memory_root, "u-mother");
        std::fs::create_dir_all(&mother_dir).unwrap();
        std::fs::write(mother_dir.join("MEMORY.md"), "## 母本记忆\n").unwrap();

        let body = paths
            .projects_dir()
            .join("ws-a")
            .join("s-loose-a.jsonl");
        let body_before = std::fs::read(&body).unwrap();
        let rows_before = mother_rows(&paths);

        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});
        let plan = preview_plan(&paths, "u-target");
        mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &target_acc, &mother_acc(),
            &memory_root,
            &plan,
        )
        .unwrap();

        assert_eq!(std::fs::read(&body).unwrap(), body_before, "母本正文零写入");
        assert_eq!(mother_rows(&paths), rows_before, "母本 sessions 行零写入");
        assert_eq!(
            std::fs::read_to_string(mother_dir.join("MEMORY.md")).unwrap(),
            "## 母本记忆\n",
            "母本 MEMORY.md 零写入"
        );
    }

    #[test]
    fn mirror_apply_rejects_when_mother_loose_sessions_changed() {
        // N2：预览后母本无组会话集合变化（**数量不变**，故只能由新指纹维度抓到）→ 零写入。
        let dir = TempDir::new("loose-changed");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(
            &paths,
            &[
                ("loose-a", "u-mother", 1000, None),
                ("loose-b", "u-mother", 1000, None),
            ],
        );
        write_link_store(&paths, Vec::new());
        let plan = preview_plan(&paths, "u-target");
        assert!(plan.targets.is_empty());

        {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            conn.execute("UPDATE sessions SET deleted_at = 999 WHERE id = 'loose-a'", [])
                .unwrap();
            conn.execute(
                "INSERT INTO sessions VALUES ('loose-c','u-mother','新会话',NULL,3000,NULL)",
                [],
            )
            .unwrap();
        }

        let error = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}), &mother_acc(),
            &paths.data_root,
            &plan,
        )
        .unwrap_err();
        assert!(error.contains("过期"), "母本无组会话变化必须拒绝：{error}");
        assert_eq!(
            target_active_count(&paths, "u-target"),
            0,
            "零写入：一条副本都不该产生"
        );
    }

    #[test]
    fn mirror_copy_adds_mother_only_sessions() {
        let dir = TempDir::new("copy-add");
        let paths = copy_ready_env(&dir);
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 0, "目标无待删行");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &mother_acc(), &paths.data_root, &plan).unwrap();
        assert_eq!(report.copied_sessions, 1, "应复制 1 个母本独有会话");
        assert_eq!(report.deleted_sessions, 0);
        assert_eq!(target_active_count(&paths, "u-target"), 1, "目标库出现副本");

        let backup = PathBuf::from(report.backup.clone().unwrap());
        let manifest: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(backup.join(MIRROR_MANIFEST_FILE)).unwrap())
                .unwrap();
        assert_eq!(manifest.copied.len(), 1, "复制登记进清单");
        assert_eq!(manifest.state, "done");
    }

    #[test]
    fn mirror_copy_is_idempotent_after_repreview() {
        let dir = TempDir::new("copy-idem");
        let paths = copy_ready_env(&dir);
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});

        let plan1 = preview_plan(&paths, "u-target");
        let r1 = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &mother_acc(), &paths.data_root, &plan1).unwrap();
        assert_eq!(r1.copied_sessions, 1);

        // 重预览：副本在含母本的组里，不算多余、不再复制、不被误软删。
        let plan2 = preview_plan(&paths, "u-target");
        assert_eq!(plan2.targets.len(), 0, "母本对端副本不算待删");
        let r2 = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &mother_acc(), &paths.data_root, &plan2).unwrap();
        assert_eq!(r2.copied_sessions, 0, "已关联不重复复制");
        assert_eq!(r2.deleted_sessions, 0, "副本不被误软删");
        assert_eq!(target_active_count(&paths, "u-target"), 1, "副本仍在");
    }

    #[test]
    fn mirror_copy_failure_does_not_block_soft_delete() {
        let dir = TempDir::new("copy-fail");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        // 目标有一个多余行（待软删）；母本组里的 s-mother 无正文 → 复制必失败。
        seed_session_db(&paths, &[("s-extra", "u-target", 1000, None)]);
        {
            let conn = Connection::open(paths.workbuddy_db()).unwrap();
            conn.execute(
                "INSERT INTO sessions VALUES ('s-mother','u-mother','母本',NULL,2000,NULL)",
                [],
            )
            .unwrap();
        }
        write_link_store(
            &paths,
            vec![
                group("g-1", vec![("u-mother", "s-mother", MemberState::Active)]),
                // s-extra 的母本成员物理不存在（Gone）→ 按 I1/I3 进待删清单。
                group(
                    "g-extra",
                    vec![
                        ("u-mother", "m-s-extra", MemberState::Active),
                        ("u-target", "s-extra", MemberState::Active),
                    ],
                ),
            ],
        );
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});
        let plan = preview_plan(&paths, "u-target");
        assert_eq!(plan.targets.len(), 1, "s-extra 待软删");

        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &mother_acc(), &paths.data_root, &plan).unwrap();
        assert_eq!(report.deleted_sessions, 1, "软删成功");
        assert_eq!(report.copied_sessions, 0, "复制失败");
        assert!(!report.errors.is_empty(), "复制错误进 errors");
        assert!(deleted_at_of(&paths, "s-extra").is_some(), "软删未被复制失败阻断");
    }

    #[test]
    fn mirror_restore_soft_deletes_copied_sessions() {
        let dir = TempDir::new("copy-rollback");
        let paths = copy_ready_env(&dir);
        let target_acc = json!({"id":"acc-u-target","uid":"u-target","variant":"cn"});
        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(&paths, WbVariant::Cn, "u-target", &target_acc, &mother_acc(), &paths.data_root, &plan).unwrap();
        assert_eq!(report.copied_sessions, 1);
        assert_eq!(target_active_count(&paths, "u-target"), 1, "副本存在");

        let backup = PathBuf::from(report.backup.clone().unwrap());
        let restored =
            mirror_restore_within_root(&paths, &paths.data_root, WbVariant::Cn, &backup).unwrap();
        assert_eq!(restored.restored_copies, 1, "复制新增被撤销");
        assert_eq!(target_active_count(&paths, "u-target"), 0, "副本已软删");
    }

    #[test]
    fn mirror_memory_failure_does_not_block_soft_delete() {
        let dir = TempDir::new("memory-fail");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[("s-1", "u-target", 1000, None)]);
        seed_gone_mother_groups(&paths, &["s-1"]);

        // 母本有记忆；目标的人格目录位置被一个同名**文件**占住 → 创建目录必失败。
        let memory_root = dir.path().join("memory-root");
        let mother_dir = memory::personal_dir_at(&memory_root, "u-mother");
        std::fs::create_dir_all(&mother_dir).unwrap();
        std::fs::write(mother_dir.join("MEMORY.md"), "## 母本记忆\n").unwrap();
        std::fs::write(memory::personal_dir_at(&memory_root, "u-target"), "占位文件").unwrap();

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}), &mother_acc(),
            &memory_root,
            &plan,
        )
        .unwrap();

        assert_eq!(report.deleted_sessions, 1, "记忆覆盖失败不得回滚已完成的软删");
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.session_id == MEMORY_STEP_ID),
            "记忆失败必须进 errors：{:?}",
            report.errors
        );
        // 记忆步骤失败不该伪造备份路径。
        let manifest_file = PathBuf::from(report.backup.clone().unwrap()).join(MIRROR_MANIFEST_FILE);
        let manifest: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_file).unwrap()).unwrap();
        assert_eq!(manifest.memory_backup, None, "未写成功就不得登记记忆备份");
        assert_eq!(manifest.state, "done", "会话步骤已完成，清单仍应 done");
    }

    #[test]
    fn mirror_memory_overwrite_runs_even_when_sessions_match() {
        let dir = TempDir::new("memory-only");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        // 目标没有任何会话差异 → 会话步骤零操作，但 MEMORY.md 不同，仍必须覆盖。
        seed_session_db(&paths, &[]);
        let memory_root = dir.path().join("memory-root");
        let mother_dir = memory::personal_dir_at(&memory_root, "u-mother");
        std::fs::create_dir_all(&mother_dir).unwrap();
        std::fs::write(mother_dir.join("MEMORY.md"), "## 母本记忆\n").unwrap();
        let target_dir = memory::personal_dir_at(&memory_root, "u-target");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("MEMORY.md"), "## 目标旧记忆\n").unwrap();

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}), &mother_acc(),
            &memory_root,
            &plan,
        )
        .unwrap();

        assert_eq!(report.deleted_sessions, 0);
        assert!(
            std::fs::read_to_string(target_dir.join("MEMORY.md"))
                .unwrap()
                .contains("母本记忆"),
            "仅记忆不同也必须整份覆盖"
        );
        let manifest_file = PathBuf::from(report.backup.clone().unwrap()).join(MIRROR_MANIFEST_FILE);
        let manifest: MirrorBackupManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_file).unwrap()).unwrap();
        assert!(
            manifest.memory_backup.is_some(),
            "记忆备份路径必须登记进同一清单"
        );
    }

    // -----------------------------------------------------------------------
    // 专家团清单整份覆盖：与 MEMORY.md 同形的第三个独立可还原步骤
    // -----------------------------------------------------------------------

    fn seed_experts(root: &Path, uid: &str, text: &str) {
        let dir = experts::custom_dir_at(root, uid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(experts::EXPERTS_FILE), text).unwrap();
    }

    fn read_experts(root: &Path, uid: &str) -> Option<String> {
        std::fs::read_to_string(experts::custom_dir_at(root, uid).join(experts::EXPERTS_FILE)).ok()
    }

    fn manifest_of(report: &MirrorReport) -> MirrorBackupManifest {
        let dir = PathBuf::from(report.backup.clone().expect("报告应带备份目录"));
        serde_json::from_str(&std::fs::read_to_string(dir.join(MIRROR_MANIFEST_FILE)).unwrap())
            .unwrap()
    }

    /// 「会话齐、MEMORY.md 同、仅专家团不同」的现场跑完一次镜像，返回还原所需的现场。
    fn run_experts_only_mirror(dir: &TempDir) -> (SessionPaths, PathBuf, PathBuf) {
        let paths = paths_for(dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[]);
        let root = paths.data_root.clone();
        seed_experts(&root, "u-mother", "[\n  \"alpha-expert\"\n]");
        seed_experts(&root, "u-target", "[\"preimage-expert\"]");

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}), &mother_acc(),
            &root,
            &plan,
        )
        .unwrap();
        assert!(
            report.experts_updated,
            "前置：专家团必须已被覆盖：{:?}",
            report.errors
        );
        let backup = PathBuf::from(report.backup.clone().expect("前置：必须建备份目录"));
        (paths, root, backup)
    }

    #[test]
    fn mirror_apply_writes_experts_even_when_sessions_already_match() {
        // 零操作判定回归：会话集合与 MEMORY.md 都已一致，只有专家团不同 → 必须真的写专家团，
        // 且不得判成「无操作」（漏了这条，「仅专家团不同」的切号会什么都不做）。
        let dir = TempDir::new("experts-only");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[]);
        let root = paths.data_root.clone();
        seed_experts(&root, "u-mother", "[\n  \"alpha-expert\"\n]");
        seed_experts(&root, "u-target", "[\"stale-expert\"]");

        let preview =
            mirror_preview_at(&paths, &root, WbVariant::Cn, &json!({"uid": "u-target"})).unwrap();
        assert!(preview.experts_pending, "预览必须标出专家团待覆盖");
        assert_eq!(preview.experts_names, vec!["alpha-expert".to_string()]);
        assert_eq!(preview.experts_note, None);

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}), &mother_acc(),
            &root,
            &plan,
        )
        .unwrap();

        assert!(report.backup.is_some(), "仅专家团不同不得判为零操作");
        assert!(report.experts_updated);
        assert_eq!(
            read_experts(&root, "u-target").unwrap(),
            "[\n  \"alpha-expert\"\n]",
            "目标是母本原文（含顺序）"
        );
        assert_eq!(
            read_experts(&root, "u-mother").unwrap(),
            "[\n  \"alpha-expert\"\n]",
            "母本只读"
        );
        let manifest = manifest_of(&report);
        assert_eq!(manifest.experts_names, vec!["alpha-expert".to_string()]);
        let experts_backup = PathBuf::from(manifest.experts_backup.expect("必须登记专家团备份"));
        assert_eq!(
            std::fs::read_to_string(&experts_backup).unwrap(),
            "[\"stale-expert\"]",
            "备份就是覆盖前的目标原文"
        );
    }

    #[test]
    fn mirror_apply_is_noop_when_sessions_memory_experts_all_match() {
        let dir = TempDir::new("experts-match");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        seed_session_db(&paths, &[]);
        let root = paths.data_root.clone();
        // 集合相同、顺序与缩进不同 → 仍算一致：不写、不留备份、不发生成备份目录。
        seed_experts(&root, "u-mother", "[\"x\",\"y\"]");
        let original = "[ \"y\",\r\n  \"x\" ]";
        seed_experts(&root, "u-target", original);

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}), &mother_acc(),
            &root,
            &plan,
        )
        .unwrap();

        assert_eq!(report.backup, None, "三样全同才算零操作");
        assert!(!report.experts_updated);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(read_experts(&root, "u-target").unwrap(), original, "无变化不得改写");
    }

    #[test]
    fn mirror_apply_folds_experts_error_without_blocking_soft_delete() {
        // 目标清单不可信：不覆盖、报错，但已完成的复制照常保留（失败不阻断）。
        let dir = TempDir::new("experts-invalid-target");
        let paths = copy_ready_ungrouped_env(&dir);
        let root = paths.data_root.clone();
        seed_experts(&root, "u-mother", "[\"alpha-expert\"]");
        seed_experts(&root, "u-target", "not-json");

        let preview =
            mirror_preview_at(&paths, &root, WbVariant::Cn, &json!({"uid": "u-target"})).unwrap();
        assert!(!preview.experts_pending, "不可信清单不得被覆盖");
        assert!(
            preview.experts_note
                .as_deref()
                .is_some_and(|note| note.contains("不是合法 JSON")),
            "{:?}",
            preview.experts_note
        );

        let plan = preview_plan(&paths, "u-target");
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}), &mother_acc(),
            &root,
            &plan,
        )
        .unwrap();

        assert_eq!(report.copied_sessions, 3, "专家团失败不得阻断复制");
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.session_id == experts::EXPERTS_FILE),
            "{:?}",
            report.errors
        );
        assert!(!report.experts_updated);
        assert_eq!(
            read_experts(&root, "u-target").unwrap(),
            "not-json",
            "不可信清单一字不动"
        );
    }

    #[test]
    fn mirror_restore_writes_experts_back_to_preimage() {
        let dir = TempDir::new("experts-restore");
        let (paths, root, backup) = run_experts_only_mirror(&dir);
        // 镜像后用户又加了专家团：还原仍按备份原文整份写回（不做三方合并）。
        seed_experts(&root, "u-target", "[\"user-added-later\"]");

        let restored = mirror_restore_at(&paths, &root, WbVariant::Cn, &backup).unwrap();

        assert!(restored.experts_restored);
        assert!(restored.errors.is_empty(), "{:?}", restored.errors);
        assert_eq!(
            read_experts(&root, "u-target").unwrap(),
            "[\"preimage-expert\"]",
            "回到镜像前原文"
        );
        // 写回前把当时的目标原文另存，保留再反悔的余地。
        let dir_target = experts::custom_dir_at(&root, "u-target");
        let saved: Vec<PathBuf> = std::fs::read_dir(&dir_target)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().starts_with("experts.json.bak-"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(saved.len(), 1, "{saved:?}");
        assert_eq!(
            std::fs::read_to_string(&saved[0]).unwrap(),
            "[\"user-added-later\"]"
        );
    }

    #[test]
    fn mirror_restore_skips_experts_for_legacy_manifest() {
        // 老备份清单（无 expertsBackup / expertsNames 字段）：仍可反序列化、会话照常还原，
        // 专家团步骤跳过且不报错。
        let dir = TempDir::new("experts-legacy");
        let (paths, root, backup) = run_experts_only_mirror(&dir);
        let file = backup.join(MIRROR_MANIFEST_FILE);
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("expertsBackup");
        object.remove("expertsNames");
        std::fs::write(&file, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let restored = mirror_restore_at(&paths, &root, WbVariant::Cn, &backup).unwrap();

        assert!(!restored.experts_restored);
        assert!(restored.errors.is_empty(), "{:?}", restored.errors);
        assert_eq!(
            read_experts(&root, "u-target").unwrap(),
            "[\n  \"alpha-expert\"\n]",
            "老备份没有原文可回滚，保持镜像后内容"
        );
    }

    #[test]
    fn mirror_restore_skips_experts_when_backup_file_missing() {
        let dir = TempDir::new("experts-backup-gone");
        let (paths, root, backup) = run_experts_only_mirror(&dir);
        std::fs::remove_file(backup.join(EXPERTS_BACKUP_FILE)).unwrap();

        let restored = mirror_restore_at(&paths, &root, WbVariant::Cn, &backup).unwrap();

        assert!(!restored.experts_restored);
        assert!(restored.errors.is_empty(), "备份文件被删不是还原失败：{:?}", restored.errors);
        assert!(
            restored.warnings.iter().any(|item| item.contains("专家团")),
            "{:?}",
            restored.warnings
        );
        assert_eq!(
            read_experts(&root, "u-target").unwrap(),
            "[\n  \"alpha-expert\"\n]"
        );
    }

    #[test]
    fn mirror_restore_skips_experts_when_backup_content_corrupt() {
        // 崩溃窗口可能留下 0 字节或半写备份：必须校验内容可解析，绝不能让还原把目标清单
        // 写成空文件还报成功。
        let dir = TempDir::new("experts-backup-corrupt");
        let (paths, root, backup) = run_experts_only_mirror(&dir);
        std::fs::write(backup.join(EXPERTS_BACKUP_FILE), "").unwrap();

        let restored = mirror_restore_at(&paths, &root, WbVariant::Cn, &backup).unwrap();

        assert!(!restored.experts_restored);
        assert!(restored.errors.is_empty(), "备份不可用不是还原失败：{:?}", restored.errors);
        assert!(
            restored.warnings.iter().any(|item| item.contains("专家团")),
            "{:?}",
            restored.warnings
        );
        assert_eq!(
            read_experts(&root, "u-target").unwrap(),
            "[\n  \"alpha-expert\"\n]",
            "不可用备份不得覆盖目标清单"
        );
    }

    #[test]
    fn mirror_manifest_keeps_version_1_and_reads_back() {
        // 同版本 schema 扩展：老 JSON 缺新字段也能读成 None / 空，版本号恒定不变。
        let legacy = json!({
            "version": 1,
            "operationId": "op-legacy",
            "variant": "cn",
            "motherUid": "u-mother",
            "targetUid": "u-target",
            "previewToken": "token",
            "expectedDigest": "digest",
            "createdAt": 1,
            "state": "done",
            "rows": [],
            "copied": [],
            "memoryBackup": null,
            "restoreSteps": []
        });
        let manifest: MirrorBackupManifest = serde_json::from_value(legacy).unwrap();
        assert_eq!(manifest.version, MIRROR_BACKUP_VERSION, "版本必须保持 1");
        assert!(manifest.experts_backup.is_none());
        assert!(manifest.experts_names.is_empty());

        // 新写法落盘后版本仍是 1，且专家团备份字段可回读。
        let dir = TempDir::new("manifest-expert-fields");
        let (_, _, backup) = run_experts_only_mirror(&dir);
        let reread: MirrorBackupManifest = serde_json::from_str(
            &std::fs::read_to_string(backup.join(MIRROR_MANIFEST_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(reread.version, 1);
        assert!(reread.experts_backup.is_some());
        assert_eq!(reread.experts_names, vec!["alpha-expert".to_string()]);
    }

    /// 删除同步只动「混合组」：既有人已删、又有人还活着的关联组。
    /// 全活、全删、单成员、无组一律不动——任何一条判错都会误删或漏删。
    #[test]
    fn deleted_sync_flags_only_mixed_groups() {
        let row = |uid: &str, sid: &str, deleted: Option<i64>| MirrorRow {
            session_id: sid.to_string(),
            user_id: uid.to_string(),
            title: Some(format!("{sid}-title")),
            custom_title: None,
            updated_at: 100,
            activity_at: 100,
            deleted_at: deleted,
        };
        let rows = vec![
            row("u1", "s1", None),      // 混合组：仍活跃 → 应删
            row("u2", "s2", Some(1)),   // 混合组：已软删
            row("u1", "s3", None),      // 全活组：都不动
            row("u2", "s4", None),      // 全活组
            row("u1", "s5", Some(2)),   // 全删组：都不动
            row("u2", "s6", Some(3)),   // 全删组
            row("u1", "s7", None),      // 未建立关联：不动
            row("u3", "s8", None),      // 混合组但对方行已不存在 → 仍算 gone → 应删
        ];
        let groups = vec![
            group(
                "g-mixed",
                vec![
                    ("u1", "s1", MemberState::Active),
                    ("u2", "s2", MemberState::Active),
                ],
            ),
            group(
                "g-alive",
                vec![
                    ("u1", "s3", MemberState::Active),
                    ("u2", "s4", MemberState::Active),
                ],
            ),
            group(
                "g-dead",
                vec![
                    ("u1", "s5", MemberState::Active),
                    ("u2", "s6", MemberState::Active),
                ],
            ),
            group(
                "g-missing",
                vec![
                    ("u3", "s8", MemberState::Active),
                    ("u4", "s9", MemberState::Active), // s9 行不存在
                ],
            ),
        ];
        let targets = deleted_sync_targets(&rows, &groups);
        let mut ids: Vec<&str> = targets.iter().map(|t| t.session_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["s1", "s8"]);
        assert!(targets.iter().all(|t| t.kind == "session"));
        assert!(targets.iter().all(|t| t.user_id == "u1" || t.user_id == "u3"));
    }

    /// 单成员组与「成员全部 inactive」的组一律跳过：没有对比就没有"别处已删"。
    #[test]
    fn deleted_sync_skips_single_and_inactive_groups() {
        let row = |uid: &str, sid: &str| MirrorRow {
            session_id: sid.to_string(),
            user_id: uid.to_string(),
            title: None,
            custom_title: None,
            updated_at: 1,
            activity_at: 1,
            deleted_at: None,
        };
        let rows = vec![row("u1", "s1"), row("u1", "s2")];
        let groups = vec![
            group("g-single", vec![("u1", "s1", MemberState::Active)]),
            group(
                "g-inactive",
                vec![
                    ("u1", "s2", MemberState::Active),
                    ("u2", "s3", MemberState::Stale),
                ],
            ),
        ];
        assert!(deleted_sync_targets(&rows, &groups).is_empty());
    }

    // --- 自动同步落后副本（akasha-mirror-sync） ---------------------------

    fn activity(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(id, at)| (id.to_string(), *at)).collect()
    }

    /// 母本与目标各有一条 `Active` 成员的双人组。
    fn paired_group(id: &str, mother_sid: &str, target_sid: &str) -> LinkGroup {
        group(
            id,
            vec![
                ("u-mother", mother_sid, MemberState::Active),
                ("u-target", target_sid, MemberState::Active),
            ],
        )
    }

    /// 用例内的预筛阈值：60（毫秒口径与生产常量一致，数值缩小便于书写）。
    const TEST_MARGIN_MS: i64 = 60;

    fn behind_of(
        groups: &[LinkGroup],
        mother: &[(&str, i64)],
        target: &[(&str, i64)],
    ) -> Vec<String> {
        groups_behind_candidates(
            groups,
            &activity(mother),
            &activity(target),
            "u-mother",
            "u-target",
            TEST_MARGIN_MS,
        )
    }

    /// 生产阈值就是 60 秒：一轮真实对话必然把 `last_activity_at` 推到秒级新值，
    /// 60s 只为吃掉同一次写入被两条记录分摊的抖动。
    #[test]
    fn mirror_sync_margin_is_one_minute() {
        assert_eq!(MIRROR_SYNC_MARGIN_MS, 60_000);
    }

    /// T8：查表口径。`ahead` 必须 `None` —— 镜像不得自造 `UnifyOverwrite` 绕过
    /// `available_modes()` 的限制。
    #[test]
    fn auto_sync_mode_maps_only_actionable_verdicts() {
        let cases = [
            (SyncVerdict::FastForward, Some(SyncMode::FastForward)),
            (SyncVerdict::Diverge, Some(SyncMode::Overwrite)),
            (SyncVerdict::Identical, None),
            (SyncVerdict::Ahead, None),
            (SyncVerdict::Unknown, None),
        ];
        for (verdict, expected) in cases {
            assert_eq!(auto_sync_mode(verdict), expected, "{verdict:?}");
        }
    }

    /// T1：母本 1000 / 目标 900，差 100 > 60 → 候选。
    #[test]
    fn groups_behind_candidates_picks_mother_leading_group() {
        let groups = vec![paired_group("g-1", "s-m", "s-t")];
        assert_eq!(
            behind_of(&groups, &[("s-m", 1000)], &[("s-t", 900)]),
            vec!["g-1".to_string()]
        );
    }

    /// T2 + 阈值边界：差值等于阈值（60）视为抖动不进候选，超出 1ms 才进。
    #[test]
    fn groups_behind_candidates_swallows_timestamp_jitter() {
        let groups = vec![paired_group("g-1", "s-m", "s-t")];
        assert!(
            behind_of(&groups, &[("s-m", 1000)], &[("s-t", 940)]).is_empty(),
            "差值恰好等于阈值：当成同一次写入的抖动"
        );
        assert!(
            behind_of(&groups, &[("s-m", 1000)], &[("s-t", 941)]).is_empty(),
            "差值 59ms：低于阈值，不进候选"
        );
        assert_eq!(
            behind_of(&groups, &[("s-m", 1000)], &[("s-t", 939)]),
            vec!["g-1".to_string()],
            "差值 61ms：超过阈值才算母本领先"
        );
    }

    /// T3（B6）：母本落后于目标 → 不回退，零候选。
    #[test]
    fn groups_behind_candidates_never_reverts_target_leading_group() {
        let groups = vec![paired_group("g-1", "s-m", "s-t")];
        assert!(behind_of(&groups, &[("s-m", 900)], &[("s-t", 1000)]).is_empty());
    }

    /// T4：副本缺 `last_activity_at`（历史脏数据）按 0 处理 → 必进候选。
    #[test]
    fn groups_behind_candidates_treats_missing_activity_as_zero() {
        let groups = vec![paired_group("g-1", "s-m", "s-t")];
        assert_eq!(
            behind_of(&groups, &[("s-m", 1000)], &[]),
            vec!["g-1".to_string()]
        );
    }

    /// T5（B1/B2）：目标在该组没有 `Active` 成员 → 交给复制路径，不进候选。
    #[test]
    fn groups_behind_candidates_skips_group_without_target_active_copy() {
        let groups = vec![
            group("g-no-copy", vec![("u-mother", "s-m", MemberState::Active)]),
            group(
                "g-superseded",
                vec![
                    ("u-mother", "s-m", MemberState::Active),
                    ("u-target", "s-t", MemberState::Superseded),
                ],
            ),
        ];
        assert!(
            behind_of(&groups, &[("s-m", 1000)], &[("s-t", 100)]).is_empty(),
            "无副本/副本已失效一律交给复制，不进同步候选"
        );
    }

    /// T6：组内没有母本 `Active` 成员 → 无从比较。
    #[test]
    fn groups_behind_candidates_skips_group_without_mother_active_member() {
        let groups = vec![
            group("g-target-only", vec![("u-target", "s-t", MemberState::Active)]),
            group(
                "g-mother-stale",
                vec![
                    ("u-mother", "s-m", MemberState::Stale),
                    ("u-target", "s-t", MemberState::Active),
                ],
            ),
        ];
        assert!(behind_of(&groups, &[("s-m", 1000)], &[("s-t", 100)]).is_empty());
    }

    /// T7：`count == 0` 显式化 —— 空组集返回空 `Vec`，不 panic。
    #[test]
    fn groups_behind_candidates_returns_empty_for_no_groups() {
        assert!(groups_behind_candidates(
            &[],
            &HashMap::new(),
            &HashMap::new(),
            "u-mother",
            "u-target",
            MIRROR_SYNC_MARGIN_MS
        )
        .is_empty());
    }

    fn sample_report() -> MirrorReport {
        MirrorReport {
            deleted_sessions: 1,
            copied_sessions: 2,
            restored_copies: 0,
            backup: None,
            needs_recovery: false,
            experts_updated: false,
            experts_restored: false,
            warnings: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
            unmerged: Vec::new(),
            synced_sessions: 1,
            overwritten_sessions: 1,
            sync_details: vec![MirrorSyncedGroup {
                group_id: "g-1".to_string(),
                title: Some("组一".to_string()),
                mode: "overwrite".to_string(),
                overwritten: true,
                target_records_before: 4,
                added_records: 2,
            }],
            sync_errors: vec![MirrorSyncError {
                group_id: "g-2".to_string(),
                title: None,
                error: "目标副本已失效，需手动处理".to_string(),
            }],
        }
    }

    /// T9：老载荷（没有四个同步字段）照旧可读，同步字段落到「没有」。
    #[test]
    fn mirror_report_defaults_sync_fields_for_legacy_payload() {
        let legacy = json!({
            "deletedSessions": 3,
            "copiedSessions": 0,
            "backup": null,
            "needsRecovery": false,
            "warnings": [],
            "skipped": [],
            "errors": [],
        });
        let report: MirrorReport = serde_json::from_value(legacy).unwrap();
        assert_eq!(report.deleted_sessions, 3);
        assert_eq!(report.synced_sessions, 0);
        assert_eq!(report.overwritten_sessions, 0);
        assert!(report.sync_details.is_empty());
        assert!(report.sync_errors.is_empty());
    }

    /// T10：新报告按 camelCase 输出四个同步键，前端才读得到。
    #[test]
    fn mirror_report_serializes_sync_fields_in_camel_case() {
        let value = serde_json::to_value(sample_report()).unwrap();
        for key in [
            "syncedSessions",
            "overwrittenSessions",
            "syncDetails",
            "syncErrors",
        ] {
            assert!(value.get(key).is_some(), "缺少 {key}：{value}");
        }
        assert_eq!(value["syncedSessions"], 1);
        assert_eq!(value["syncDetails"][0]["targetRecordsBefore"], 4);
        assert_eq!(value["syncDetails"][0]["addedRecords"], 2);
        assert_eq!(value["syncDetails"][0]["overwritten"], true);
    }

    fn preview_item(group_id: &str, verdict: &str, modes: &[&str], token: Option<&str>) -> Value {
        let mut item = json!({
            "groupId": group_id,
            "title": format!("组-{group_id}"),
            "verdict": verdict,
            "availableModes": modes,
        });
        if let Some(token) = token {
            item["previewToken"] = json!(token);
        }
        item
    }

    fn preview_of(items: Vec<Value>) -> Value {
        json!({"storeStatus": "ready", "groups": items})
    }

    /// B4：快进组 → 追加母本新记录，目标原有内容不丢；计数只加同步数。
    #[test]
    fn sync_behind_copies_fast_forwards_mother_leading_group() {
        let outcome = sync_behind_copies(
            &["g-1".to_string()],
            || Ok(preview_of(vec![preview_item("g-1", "fastForward", &["fastForward"], Some("tok-1"))])),
            |selections| {
                assert_eq!(selections.len(), 1);
                assert_eq!(selections[0].group_id, "g-1");
                assert_eq!(selections[0].mode, SyncMode::FastForward);
                assert_eq!(selections[0].preview_token, "tok-1");
                Ok(json!({
                    "synced": [{
                        "groupId": "g-1",
                        "status": "synced",
                        "mode": "fastForward",
                        "recordCount": {"source": 5, "targetBefore": 2, "target": 5},
                    }],
                    "skipped": [],
                    "errors": [],
                }))
            },
        );
        assert_eq!(outcome.synced, 1);
        assert_eq!(outcome.overwritten, 0);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let only = &outcome.details[0];
        assert_eq!(only.title.as_deref(), Some("组-g-1"));
        assert_eq!(only.mode, "fastForward");
        assert!(!only.overwritten);
        assert_eq!(only.target_records_before, 2);
        assert_eq!(only.added_records, 3);
    }

    /// B5：分叉组 → 按 P1 用母本覆盖，`overwritten` 计数与 `targetRecordsBefore` 齐备。
    #[test]
    fn sync_behind_copies_overwrites_diverged_group() {
        let outcome = sync_behind_copies(
            &["g-1".to_string(), "g-2".to_string()],
            || {
                Ok(preview_of(vec![
                    preview_item("g-1", "diverge", &["overwrite"], Some("tok-1")),
                    preview_item("g-2", "diverge", &["overwrite"], Some("tok-2")),
                ]))
            },
            |selections| {
                assert_eq!(selections.len(), 2);
                assert!(selections.iter().all(|s| s.mode == SyncMode::Overwrite));
                Ok(json!({
                    "synced": [{
                        "groupId": "g-1",
                        "status": "synced",
                        "mode": "overwrite",
                        "recordCount": {"source": 9, "targetBefore": 4, "target": 9},
                    }],
                    "skipped": [],
                    "errors": [{"groupId": "g-2", "error": "目标内容不存在，未同步"}],
                }))
            },
        );
        assert_eq!(outcome.synced, 1);
        assert_eq!(outcome.overwritten, 1);
        assert_eq!(outcome.details[0].target_records_before, 4);
        assert_eq!(outcome.details[0].added_records, 5);
        assert_eq!(outcome.errors.len(), 1, "失败组不阻断其它组");
        assert_eq!(outcome.errors[0].group_id, "g-2");
        assert_eq!(outcome.errors[0].title.as_deref(), Some("组-g-2"));
    }

    /// B3/B6：identical 与 ahead 一律不写，也不产生明细或错误。
    #[test]
    fn sync_behind_copies_ignores_identical_and_ahead_groups() {
        let called = std::cell::Cell::new(false);
        let outcome = sync_behind_copies(
            &["g-1".to_string(), "g-2".to_string()],
            || {
                Ok(preview_of(vec![
                    preview_item("g-1", "identical", &[], None),
                    preview_item("g-2", "ahead", &[], Some("tok-2")),
                ]))
            },
            |_| {
                called.set(true);
                Ok(json!({}))
            },
        );
        assert!(!called.get(), "内容一致 / 目标更新一律零写入");
        assert_eq!(outcome.synced, 0);
        assert!(outcome.details.is_empty());
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    }

    /// B7：不可验证的组进 `sync_errors`，不重试、不影响其它组。
    #[test]
    fn sync_behind_copies_reports_unknown_group_without_writing() {
        let called = std::cell::Cell::new(false);
        let outcome = sync_behind_copies(
            &["g-1".to_string()],
            || Ok(preview_of(vec![preview_item("g-1", "unknown", &[], None)])),
            |_| {
                called.set(true);
                Ok(json!({}))
            },
        );
        assert!(!called.get());
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].error, "目标副本已失效，需手动处理");
    }

    /// B8：没有 `previewToken` 绝不构造 selection（缺 token 会让整次同步 Err）。
    #[test]
    fn sync_behind_copies_never_builds_selection_without_preview_token() {
        let called = std::cell::Cell::new(false);
        let outcome = sync_behind_copies(
            &["g-1".to_string()],
            || Ok(preview_of(vec![preview_item("g-1", "fastForward", &[], None)])),
            |_| {
                called.set(true);
                Ok(json!({}))
            },
        );
        assert!(!called.get(), "无凭据不写");
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].group_id, "g-1");
    }

    /// B9：同步整体失败（加锁失败等）→ 一组一条，`mirror_apply_at` 仍是 Ok。
    #[test]
    fn sync_behind_copies_downgrades_whole_failure_to_group_errors() {
        let outcome = sync_behind_copies(
            &["g-1".to_string(), "g-2".to_string()],
            || {
                Ok(preview_of(vec![
                    preview_item("g-1", "diverge", &["overwrite"], Some("tok-1")),
                    preview_item("g-2", "fastForward", &["fastForward"], Some("tok-2")),
                ]))
            },
            |_| Err("无法独占会话操作：会话操作进行中".to_string()),
        );
        assert_eq!(outcome.synced, 0);
        assert_eq!(outcome.errors.len(), 2);
        assert!(outcome
            .errors
            .iter()
            .all(|item| item.error.contains("无法独占会话操作")));
    }

    /// 接线（T13/T14 的真实链路部分）：预筛出「母本领先」的组后，`mirror_apply_at` 会在
    /// 复制之后进入同步步骤；目标正文不可读时该组进 `sync_errors` 而不是整份失败，
    /// 目标更新的组只记 warning 不回退。
    #[test]
    fn mirror_apply_routes_behind_groups_into_sync_without_blocking() {
        let dir = TempDir::new("sync-wiring");
        let paths = paths_for(&dir);
        seed_auth(&paths, "u-mother");
        std::fs::create_dir_all(&paths.data_root).unwrap();
        let conn = Connection::open(paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, custom_title TEXT, \
             updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        // g-1：母本更活跃 → 落后副本候选；g-2：目标更活跃 → 只记 warning，绝不回退。
        for (id, uid, title, updated_at) in [
            ("m-behind", "u-mother", "母本领先", 100_000),
            ("t-behind", "u-target", "目标落后", 1_000),
            ("m-ahead", "u-mother", "母本落后", 1_000),
            ("t-ahead", "u-target", "目标领先", 100_000),
        ] {
            conn.execute(
                "INSERT INTO sessions VALUES (?1, ?2, ?3, NULL, ?4, NULL)",
                rusqlite::params![id, uid, title, updated_at],
            )
            .unwrap();
        }
        drop(conn);
        let econn = Connection::open(paths.edge_sync_db(WbVariant::Cn)).unwrap();
        econn
            .execute_batch(
                "CREATE TABLE edge_sync_mapping (session_id TEXT, conversation_id TEXT, \
                 msg_channel TEXT, created_at INTEGER);",
            )
            .unwrap();
        drop(econn);
        write_link_store(
            &paths,
            vec![
                group(
                    "g-1",
                    vec![
                        ("u-mother", "m-behind", MemberState::Active),
                        ("u-target", "t-behind", MemberState::Active),
                    ],
                ),
                group(
                    "g-2",
                    vec![
                        ("u-mother", "m-ahead", MemberState::Active),
                        ("u-target", "t-ahead", MemberState::Active),
                    ],
                ),
            ],
        );

        let plan = preview_plan(&paths, "u-target");
        assert!(
            plan.targets.is_empty(),
            "母本仍持有这两段对话，不该软删：{:?}",
            plan.targets
        );
        let report = mirror_apply_at(
            &paths,
            WbVariant::Cn,
            "u-target",
            &json!({"uid": "u-target"}),
            &mother_acc(),
            &paths.data_root,
            &plan,
        )
        .unwrap();

        assert_eq!(report.synced_sessions, 0);
        assert_eq!(report.overwritten_sessions, 0);
        assert!(report.sync_details.is_empty());
        // 测试环境没有正文文件 → 判定不可验证，必须进 sync_errors 而不是 Err。
        assert_eq!(report.sync_errors.len(), 1, "{:?}", report.sync_errors);
        assert_eq!(report.sync_errors[0].group_id, "g-1");
        assert_eq!(report.sync_errors[0].error, "目标副本已失效，需手动处理");
        // 目标更新的那组零写入，但要在 warnings 里说清「没有回退」。
        assert!(
            report.warnings.iter().any(|line| line.contains("未回退")),
            "{:?}",
            report.warnings
        );
    }

    /// 候选为空（`count == 0`）时不读预览、不写任何东西。
    #[test]
    fn sync_behind_copies_does_nothing_without_candidates() {
        let called = std::cell::Cell::new(false);
        let outcome = sync_behind_copies(
            &[],
            || {
                called.set(true);
                Ok(preview_of(Vec::new()))
            },
            |_| {
                called.set(true);
                Ok(json!({}))
            },
        );
        assert!(!called.get(), "无候选不得触发任何 IO");
        assert_eq!(outcome.synced, 0);
        assert!(outcome.details.is_empty() && outcome.errors.is_empty());
    }
}
