// 与 Rust 后端命令返回结构对齐的类型定义（对照 server.py 各 API 响应）

/**
 * WorkBuddy 客户端档位：国内版（cn）/ 国际版（ai）。
 * 后端以字符串返回，历史数据与旧响应可能缺省该字段，读取时统一按国内版处理。
 */
export type WbVariant = "cn" | "ai";

/**
 * 账号卡片显示名取哪个字段（每账号独立的本地偏好）。
 * 缺省/异常按 `nickname` 处理；选定字段为空时回退 `nickname → uid`。
 */
export type DisplayField = "nickname" | "phone" | "note";

export interface AccountMeta {
  id: string;
  uid: string | null;
  email: string | null;
  nickname: string | null;
  /** 官方手机号（实时读 profile_raw，仅国内版账号可能有）。 */
  phoneNumber?: string | null;
  /** 本地备注（用户自填，不来自官方数据）。 */
  note?: string | null;
  /** 本地显示字段偏好。 */
  displayField?: DisplayField | null;
  enterpriseName: string | null;
  expiresAt: number | null;
  refreshExpiresAt: number | null;
  refreshedAt: number | null;
  createdAt: number | null;
  needsRelogin: boolean;
  needsReloginReason: string | null;
  /** 账号所属档位；缺省（旧后端/历史账号）按国内版处理。 */
  variant?: WbVariant;
  /**
   * 企业版账号（有 enterpriseId，或 type 为 ultimate/exclusive）；
   * 缺省（旧后端/历史账号）按非企业处理。
   */
  isEnterprise?: boolean;
}

export interface AppStatus {
  running: boolean;
  authFile: string;
  current: {
    uid: string | null;
    nickname: string | null;
    email: string | null;
  } | null;
  appPath: string;
  version: string;
  /** 上述字段所属档位；缺省按国内版处理。 */
  variant?: WbVariant;
}

export interface OAuthStartResult {
  loginId: string;
  verificationUri: string;
  expiresIn: number;
}

export interface OAuthPollResult {
  done: boolean;
  result?: AccountMeta;
  error?: string;
}

/** 导出文件中的完整账号记录（含 token，仅导出命令返回；字段与账号库原始记录一致）。 */
export interface AccountRecord {
  id?: string;
  uid?: string | null;
  nickname?: string | null;
  email?: string | null;
  access_token?: string | null;
  refresh_token?: string | null;
  token_type?: string | null;
  domain?: string | null;
  expiresAt?: number | null;
  refreshExpiresAt?: number | null;
  auth_raw?: unknown;
  profile_raw?: unknown;
  createdAt?: number | null;
  [key: string]: unknown;
}

/** 导入文件账号的脱敏预览（不含 token）。 */
export interface ImportPreviewAccount {
  index: number;
  uid: string | null;
  nickname: string | null;
  email: string | null;
  hasToken: boolean;
  /** access_token 为 WorkBuddy 加密信封：可导入，但仅切换可用（签到/积分不可用）。 */
  encrypted: boolean;
}

/** 导入结果计数。 */
export interface ImportResult {
  ok: boolean;
  imported: number;
  skipped: number;
  overwritten: number;
}

export interface Session {
  id: string;
  title: string;
  cwd: string;
  updatedAt: number;
  hasHistory: boolean;
  /** WorkBuddy playground（侧栏「任务」）；缺省视为空间会话。 */
  isPlayground?: boolean;
}

/** 临时备份的清理状态：cleaned 已回收；pending 已保留待下次维护重试；legacyRetained 旧操作无生命周期记录。 */
export type SessionBackupCleanupState = "cleaned" | "pending" | "legacyRetained";

/**
 * 临时备份残留（待清理 / 待恢复）：复制、同步、恢复报告共用同一结构。
 * `cleanupPending` 表示已完成但本轮没清理成功（下次切号重试）；`needsRecovery`
 * 表示必须保留材料、需要恢复流程或人工确认。
 */
export interface TemporaryFileInfo {
  operationId: string;
  sessionId?: string;
  title?: string;
  state: "cleanupPending" | "needsRecovery";
  reason: string;
}

/**
 * 会话附属目录（`{cid}/subagents`、`{cid}/tool-results`）的结果摘要。
 *
 * 客户端渲染子 agent 面板依赖这些文件；缺失时面板是**静默空白**而不是报错，
 * 所以复制 / 同步 / 恢复三条路径的结果都必须能带到用户眼前。
 * 字段对应 core 侧 `SidecarOutcome`（`serde(rename_all = "camelCase")`），
 * 全部可选：旧后端不返回该字段。`Optional`：`SidecarOutcome` 自身字段非可选。
 */
export interface SidecarOutcome {
  /** 源侧存在附属目录；绝大多数会话没有子 agent。 */
  sourcePresent?: boolean;
  agentsCopied?: number;
  toolResultsCopied?: number;
  otherCopied?: number;
  /** 目标已存在而跳过（绝不覆盖）的文件数。 */
  skipped?: number;
  /** 其中目标已存在但内容与源不一致的个数——没有被覆盖，副本内容并不完整。 */
  mismatched?: number;
  /** 附属目录根下存在但没有复制的条目数（白名单外目录、散落文件、符号链接）。 */
  ignoredEntries?: number;
  notes?: string[];
  /** 单个文件失败的原因；非空即必须让用户看见。 */
  errors?: string[];
}

/** 本次新建的副本（目标 UUID 由后端预分配）。 */
export interface CopyResult {
  id: string;
  newId: string;
  groupId: string;
  /** 待清理位置（已清理为 null）；仅表示待清理，不是可撤销备份。 */
  backup: string | null;
  /** 成功后立即清理：cleaned 已回收 / pending 待下次维护重试；旧后端可能缺字段。 */
  cleanupState?: SessionBackupCleanupState;
  /** 清理失败原因（`cleanupState` 为 pending 时有值）。 */
  cleanupError?: string;
  /** 附属目录结果（有内容或失败时才出现）；旧后端缺该字段。 */
  sidecar?: SidecarOutcome;
}

/** 目标账号上已有真实有效的副本：复用而不是重复复制。 */
export interface LinkedCopyResult {
  id: string;
  sessionId: string;
  groupId: string;
}

/**
 * 置顶状态同步结果（复制 / 同步报告共用）。
 *
 * 置顶记录不在 SQLite，而在各账号自己的
 * `storage/user-<uid>-personal/global/conversations.json`；跨账号复制会换 sessionId，
 * 因此按关联映射把来源账号的置顶条目单向追加到目标账号。
 * `skippedReason` 只在没能同步时出现——置顶同步失败不影响复制/同步本身的成败。
 */
export interface SessionPinnedSyncInfo {
  /** 本次追加到目标账号的置顶条目数（已同步过则为 0）。 */
  added: number;
  /** 未能同步的原因；成功或无事可做时不出现。 */
  skippedReason?: string;
}

/** 切换时的会话复制报告；复制失败时后端只回 `error`（切换本身仍继续）。 */
export interface SessionCopyReport {
  sourceUid?: string;
  targetUid?: string;
  /** 跨档复制时带两侧档位；同档复制不出现这两个字段。 */
  sourceVariant?: WbVariant;
  targetVariant?: WbVariant;
  copied?: CopyResult[];
  alreadyLinked?: LinkedCopyResult[];
  errors?: { id: string; error: string }[];
  /** 仍有未完成的会话写入时为 true（失败项可重试，不会产生第二个副本）。 */
  needsRecovery?: boolean;
  /** 置顶状态同步结果（附属信息，失败只体现在 `skippedReason`）。 */
  pinned?: SessionPinnedSyncInfo;
  /** 临时备份残留（待清理/待恢复）；无异常时为空数组。 */
  temporaryFiles?: TemporaryFileInfo[];
  error?: string;
}

/** 切换前对未完成会话写入的恢复结果。 */
export interface SessionRecoveryReport {
  recovered: number;
  abandoned: number;
  needsRecovery: { operationId: string; reason: string; retryable: boolean }[];
  /** 临时备份残留（待清理/待恢复）；无异常时为空数组。 */
  temporaryFiles?: TemporaryFileInfo[];
  /** 恢复时附属目录补写的失败项；不阻断恢复，但要能看见。 */
  sidecarWarnings?: SidecarWarning[];
}

/** 恢复过程中附属目录补写失败的一条记录（后端 `SidecarWarning`）。 */
export interface SidecarWarning {
  operationId: string;
  /** 目标会话 id，供前端定位到具体会话。 */
  sessionId: string;
  reason: string;
}

// ---------------------------------------------------------------------------
// 会话同步（关联组）：预览与执行契约，与 core / Tauri / HTTP 三端同形
// ---------------------------------------------------------------------------

/**
 * 同步判定结果（design §3.2 优先级表）：
 * `identical` 两边一致、`fastForward` 有新增可同步、`ahead` 仅目标账号有更新、
 * `diverge` 两边都改过需显式覆盖、`unknown` 无法确认。
 */
export type SessionSyncVerdict = "identical" | "fastForward" | "ahead" | "diverge" | "unknown";

/** 同步写入模式：只有后端 `availableModes` 里给出的模式才允许提交。 */
export type SessionSyncMode = "fastForward" | "overwrite" | "unifyOverwrite" | "statusOnly";

/** 关联组成员（不含正文）：`state` 为 active 时才算该账号的有效成员。 */
export interface SessionLinkMember {
  memberId: string;
  uid: string;
  accountId: string | null;
  sessionId: string;
  state: "active" | "stale" | "superseded";
}

/** 关联组的预览项；`defaultChecked` 与 `availableModes` 是勾选权限的唯一来源。 */
export interface SessionLinkPreviewGroup {
  groupId: string;
  title: string;
  cwd: string;
  /**
   * 当前账号这份副本已被软删除（标题仍从库里读出来，避免列表变成一堆「(无标题)」）。
   * 这类组没有任何可同步内容：列表里标明「本侧已删除」并折叠置底，不参与全选。
   */
  sourceDeleted?: boolean;
  verdict: SessionSyncVerdict;
  /** 来源独有记录数（多重集差集，仅用于向用户解释）。 */
  extraA: number;
  /** 目标独有记录数（多重集差集，仅用于向用户解释）。 */
  extraB: number;
  common: number;
  defaultChecked: boolean;
  /**
   * 正文写入模式；为空且没有 `archiveAction` 时才不可勾选
   * （identical / ahead / unknown / 预览凭据不可用，但仅同步状态——归档或取消归档——仍可勾选）。
   */
  availableModes: SessionSyncMode[];
  reason: string;
  /**
   * 仅同步生命周期状态（正文零写入），方向由后端决定，不随用户勾选变化：
   * `statusOnly` = 源已归档 → 目标归档；`unarchive` = 源已恢复 → 目标取消归档。
   */
  archiveAction?: "statusOnly" | "unarchive";
  /** 记录数（不是消息数）：不可验证时 source/target 为 0、baseline 为 null。 */
  recordCount: { source: number; target: number; baseline: number | null };
  source: SessionLinkMember | null;
  target: SessionLinkMember | null;
  /** 勾选时必须原样回传的预览凭据；缺失即不可勾选。 */
  previewToken?: string;
}

/** 关联会话预览：`supported` 为 false（或 storeStatus 为 unsupported）时不展示同步区块。 */
export interface SessionLinksPreview {
  supported: boolean;
  storeStatus: "ready" | "missing" | "unavailable" | "unsupported";
  storeError?: string;
  sourceUid: string;
  targetUid: string;
  /** 跨档预览时带两侧档位；同档预览不出现这两个字段。 */
  sourceVariant?: WbVariant;
  targetVariant?: WbVariant;
  groups: SessionLinkPreviewGroup[];
}

/** 一条同步选择：与预览凭据绑定，执行时后端会重新校验。 */
export interface SessionSyncSelection {
  groupId: string;
  previewToken: string;
  mode: SessionSyncMode;
}

// ---------------------------------------------------------------------------
// 镜像切换（显式独立通道，绝不复用 syncSelections / buildSelections / MemorySyncMode）
// ---------------------------------------------------------------------------

/**
 * 镜像预览里逐条列出的「目标账号将被软删」会话；清单不折叠。
 * 身份以 `LinkMember.uid` 精确匹配，绝不按标题全局去重（11-06 误删教训）。
 */
export interface MirrorTarget {
  sessionId: string;
  title: string;
  /** 目标会话归属的 user_id；清单只从目标 uid 的行生成。 */
  userId: string;
  /** 所属关联组 id；未建立 LinkGroup 的私有会话为空串。 */
  groupId: string;
  /** 预览时目标会话行的 updated_at，落盘前现场复核。 */
  updatedAt: number;
  /** 预览时目标会话的规范化摘要，落盘前逐条重算，不符零写入。 */
  fullDigest: string;
  /** 会话级软删；沿用旧 kind:"session" 语义，镜像不引入记录级删除。 */
  kind: "session";
}

/** 独立只读预览结果：凭证绑定 motherUid（当前登录账号），变化一律 REASON_PREVIEW_STALE。 */
export interface MirrorPreview {
  motherUid: string;
  motherLabel: string;
  previewToken: string;
  /** 待删会话集合规范化摘要（sessionId 有序 + 各自 fullDigest + updatedAt + deletedAt 均为 NULL）。 */
  expectedDigest: string;
  /** 逐条待软删清单，界面不得折叠。 */
  targets: MirrorTarget[];
  /** 将软删的会话数（与 targets.length 逐条一致）。 */
  sessionCount: number;
  /** 将从母本复制的会话数（母本有、目标没有，按 LinkGroup 判重）。 */
  copiedCount: number;
  /** 执行前预登记的备份目录（预览阶段为 null）。 */
  backupDir: string | null;
}

/**
 * 冻结的执行计划：作为 switchAccount 的独立参数并入 switch_account 主干，继承 try_auth() 锁。
 *
 * 刻意不含 `motherUid`：母本恒由后端取自登录态（镜像执行端点忽略前端母本），
 * 前端不下发、也不暗示可伪造。预览返回的 `MirrorPreview.motherUid` 仅用于界面回显与
 * 「登录态在预览期间变化」的只读校验。
 */
export interface MirrorPlan {
  targets: MirrorTarget[];
  expectedDigest: string;
  previewToken: string;
}

/** 镜像切换执行报告：备份路径明文、中间态可还原、绝不伪装成成功。 */
export interface MirrorReport {
  /** 本次软删的会话数。 */
  deletedSessions: number;
  /** 本次从母本复制的会话数。 */
  copiedSessions: number;
  /** 备份绝对路径（明文展示，供一键还原）。 */
  backup: string | null;
  /** 备份成功但落盘失败：备份保留并登记为可还原项。 */
  needsRecovery: boolean;
  /** 已落盘但持久化证明缺失等警告，绝不据此反向写回。 */
  warnings: string[];
  skipped: { sessionId: string; reason: string }[];
  errors: { sessionId: string; error: string }[];
}

/** 已同步的关联组（保留目标 sessionId 与标题）。 */
export interface SessionSyncResultItem {
  groupId: string;
  status: "synced";
  verdict: SessionSyncVerdict;
  mode: SessionSyncMode;
  sourceSessionId: string;
  targetSessionId: string;
  recordCount: { source: number; targetBefore: number; target: number };
  updatedAt: number;
  /** 待清理位置（已清理为 null）；旧操作可能仍返回目录路径。 */
  backup: string | null;
  backupManifest: string | null;
  /** 成功后立即清理：cleaned 表示临时备份已回收；pending 表示待下次维护重试。 */
  cleanupState?: SessionBackupCleanupState;
  cleanupError?: string;
  /** 附属目录同步结果（只补目标缺失的，绝不覆盖）；旧后端缺该字段。 */
  sidecar?: SidecarOutcome;
  /** 附属目录整体失败的原因（连目录都没读到时）；与逐文件 `errors` 不重叠。 */
  sidecarError?: string;
  message: string;
}

/** 被跳过的关联组：`reasonCode` 为 previewStale 时说明预览已过期，不得显示为成功。 */
export interface SessionSyncSkippedItem {
  groupId: string;
  status: "skipped";
  reasonCode: string;
  message: string;
  verdict: SessionSyncVerdict | null;
}

/** 同步执行报告；`errors` 里可能是整批被拒（无 groupId）。 */
export interface SessionSyncReport {
  synced: SessionSyncResultItem[];
  skipped: SessionSyncSkippedItem[];
  errors: { groupId?: string; error: string }[];
  /** 仍有未完成/无法安全恢复的会话写入时为 true。 */
  needsRecovery?: boolean;
  /** 置顶状态同步结果（附属信息，失败只体现在 `skippedReason`）。 */
  pinned?: SessionPinnedSyncInfo;
  /** 临时备份残留（待清理/待恢复）；无异常时为空数组。 */
  temporaryFiles?: TemporaryFileInfo[];
  /** 附属目录同步结果（只补目标缺失的）；失败不阻断正文，但要能看见。 */
  sidecar?: SidecarOutcome;
}

// ---------------------------------------------------------------------------
// Client-scoped session group directory
// ---------------------------------------------------------------------------

export type SessionGroupClient = "workbuddy" | "codebuddyIde" | "vscodeExt";
export type SessionGroupStatus = "latest" | "behind" | "diverge" | "missing" | "unknown";
export type SessionMemberVersionStatus = SessionGroupStatus | "stale" | "superseded";

export interface SessionGroupSummary {
  key: string;
  client: SessionGroupClient;
  variantScope: WbVariant | null;
  groupId: string;
  groupVariant: WbVariant;
  title: string;
  projectLabel: string;
  latestActivityAt: number;
  memberCount: number;
  activeMemberCount: number;
  accountNames: string[];
  summaryStatus: SessionGroupStatus;
  summaryText: string;
  safeSourceMemberId: string | null;
  hasSafeSource: boolean;
}

export interface SessionGroupMemberDetail {
  memberId: string;
  accountId: string | null;
  uid: string;
  sessionId: string;
  accountName: string;
  variant: WbVariant;
  linkState: "active" | "stale" | "superseded";
  versionStatus: SessionMemberVersionStatus;
  title: string;
  projectLabel: string;
  updatedAt: number;
  recordCount: number | null;
  contentPreview?: { speaker: string; text: string }[];
  contentState: "ready" | "missing" | "unavailable";
  reason: string;
  canBeSource: boolean;
}

export interface SessionGroupList {
  client: SessionGroupClient;
  variantScope: WbVariant | null;
  storeStatus: "missing" | "ready" | "unavailable";
  storeError?: string;
  groups: SessionGroupSummary[];
}

export interface SessionGroupDetail extends SessionGroupSummary {
  members: SessionGroupMemberDetail[];
  addTargets: AccountMeta[];
  divergence?: {
    commonMemberIds: string[];
    branches: string[][];
  };
}

export interface SessionGroupPairPreview {
  client: SessionGroupClient;
  variantScope: WbVariant | null;
  groupId: string;
  sourceMemberId: string;
  targetMemberId: string;
  verdict: SessionSyncVerdict;
  availableModes: SessionSyncMode[];
  previewToken: string | null;
  reason: string;
  recordCount: { source: number; target: number; baseline: number | null } | null;
  extraTargetCount: number;
}

/** One read-only plan for making every active copy match a chosen group member. */
export interface SessionGroupUnifyPlan {
  client: SessionGroupClient;
  groupId: string;
  sourceMemberId: string;
  sourceName: string;
  targets: {
    memberId: string;
    accountName: string;
    preview: SessionGroupPairPreview | null;
    error: string | null;
  }[];
}

/** A client-reported current login, matched by stable UID or saved account ID. */
export interface SessionGroupCurrentAccount {
  variant?: WbVariant;
  uid?: string | null;
  accountId?: string | null;
  running?: boolean;
}

export interface SessionGroupActionReport {
  client: SessionGroupClient;
  groupId: string;
  sourceMemberId?: string;
  targetMemberId?: string;
  synced: SessionSyncResultItem[];
  skipped: SessionSyncSkippedItem[];
  errors: { groupId?: string; error: string }[];
  needsRecovery?: boolean;
  temporaryFiles?: TemporaryFileInfo[];
  restartedVariants?: WbVariant[];
  /** 插件侧：本次由 wb-switch 关闭并成功重开了 VS Code。 */
  restartedEditor?: boolean;
  /** 插件侧：写入已完成但 VS Code 未能自动重新打开（无 `errors` 数组的入口用它兜底）。 */
  editorError?: string;
}

/**
 * 应用内通知存档条目：toast 只存活几秒，这里保存最近 100 条供事后回看
 * （支持排障与验收核对，例如切号成功后到底提示了什么）。
 */
export interface AppNotification {
  level: "success" | "error" | "warning" | "info";
  title: string;
  description?: string;
  /** 毫秒时间戳。 */
  at: number;
}

/**
 * 错误日志来源（`~/.wb-switch/error.log` 的 `kind` 字段）：
 * 渲染崩溃 / 未捕获异常或 Promise 拒绝 / 后端错误。
 */
export type ErrorLogKind = "frontend_crash" | "frontend_unhandled" | "backend";

/** 工作记忆同步方式：合并 / 复制覆盖。不传该参数 = 不同步（旧版 `mergeMemory` 已移除）。 */
export type MemorySyncMode = "merge" | "overwrite";

export interface SwitchResult {
  ok: boolean;
  account: string;
  /** 目标账号自身档位；缺省按国内版处理。 */
  variant?: WbVariant;
  backup: string | null;
  sessionCopy?: SessionCopyReport;
  /** 本次的会话同步报告（未勾选同步时不返回）；含跳过与失败原因，不只是成功数。 */
  sessionSync?: SessionSyncReport;
  sessionRecovery?: SessionRecoveryReport;
  /** 本次的工作记忆报告（选「不同步」时不返回）；合并与覆盖共用同一形状。 */
  memoryMerge?: MemoryMergeReport;
  /** 本次的专家团清单同步报告（未勾选同步时不返回）。 */
  expertsMerge?: ExpertsMergeReport;
  /** 本次的人设同步报告（未勾选同步时不返回）。 */
  personaSync?: PersonaSyncReport;
  /** 本次的镜像切换报告（未提交镜像计划时不返回）。 */
  mirror?: MirrorReport;
  /** 本次的「同步已删除会话」报告（未提交删除同步计划时不返回）。 */
  deletedSync?: MirrorReport;
}

/**
 * 「同步已删除会话」的只读预览：某个账号已删、其他账号仍保留副本的会话。
 *
 * 只对已建立关联组的会话生效——没有关联就没有「同一段对话」的可靠证据。
 */
export interface DeletedSyncPreview {
  targets: MirrorTarget[];
  sessionCount: number;
  accountCount: number;
  expectedDigest: string;
}

/** 冻结的执行计划；服务端会整份重算，篡改无效。 */
export interface DeletedSyncPlan {
  targets: MirrorTarget[];
  expectedDigest: string;
}

/**
 * 人设同步结果：`SOUL.md` / `IDENTITY.md` / `USER.md` 各取最完整的一份写回各账号。
 *
 * `mode` 决定「已有设定」怎么处理：`safe` 只替换仍是默认模板的目标，
 * `overwrite` 内容不同就覆盖（覆盖前一定先备份）。
 */
export interface PersonaSyncReport {
  mode: "safe" | "overwrite";
  /** 本次写入的目标；`backup` 是被覆盖前的备份路径（原本无文件时为 null）。 */
  written: { label: string; file: string; backup: string | null; source: string }[];
  /** 保留原样、未写入的账号（safe 模式下多为手改过的人设）。 */
  unchanged: { label: string; file: string }[];
  errors: { label: string; file?: string; error: string }[];
}

/** 跨账号专家团清单同步结果：各账号的名单取并集后写回每个账号。 */
export interface ExpertsMergeReport {
  /** 参与本次处理的账号数（含没有 experts.json 的账号）。 */
  accounts: number;
  /** 合并后的专家团名字（已去重、已排序）。 */
  mergedExperts: string[];
  /** 合并后的专家团数量。 */
  mergedCount: number;
  /** 本次写入的账号；`backup` 是被覆盖前的原文备份路径（原本无文件时为 null）。 */
  written: { label: string; backup: string | null; hadContent: boolean }[];
  /** 合并结果与现有清单一致（集合相同）、未写入的账号。 */
  unchanged: { label: string }[];
  errors: { label: string; error: string }[];
  note?: string;
}

/**
 * 跨账号工作记忆结果：`MEMORY.md` 合并分发或复制覆盖，人格文件只补全缺失项。
 * 合并与覆盖共用同一形状，既有字段一个不删；新增 `mode` / `sourceUid` 描述本次方式。
 */
export interface MemoryMergeReport {
  /** 参与本次处理的账号数（含没有记忆文件的账号）。 */
  accounts: number;
  /** 实际贡献了记忆内容的账号及其字符数。覆盖模式只列母本一份。 */
  sources: { label: string; chars: number }[];
  /** 合并后的记忆字符数（覆盖模式 = 母本字符数）。 */
  mergedChars: number;
  /** 本次写入的账号；`backup` 是被覆盖前的原文备份路径（原本无文件时为 null）。 */
  written: { label: string; backup: string | null; hadContent: boolean }[];
  /** 结果与现有内容一致、未写入的账号。 */
  unchanged: { label: string }[];
  persona: {
    written: { label: string; file: string; backup: string | null; source: string }[];
    unchanged: { label: string; file: string }[];
    errors: { label: string; file: string; error: string }[];
  };
  errors: { label: string; error: string }[];
  /** 本次方式：合并 / 覆盖。前端分级文案以本次请求为准，不依赖报告回填。 */
  mode: MemorySyncMode;
  /** 覆盖模式的母本账号 uid；合并模式不返回。 */
  sourceUid?: string;
  note?: string;
}

export interface CheckinConfig {
  enabled: boolean;
  /** 关闭自动签到的账号 id；状态展示和刷新附带签到也跳过，主动手动签到不受影响。 */
  excluded_account_ids?: string[];
  /** 签到时间段（"HH:MM"，本地时区）；空串 = 不限制。两端都合法且 start < end 才生效。 */
  checkin_start: string;
  checkin_end: string;
  /** Legacy persisted fields; accepted by the backend but ignored by scheduling. */
  start_hour?: number;
  end_hour?: number;
  keepalive_days: number;
  lazy_refresh_hours: number;
  /**
   * 签到后给每个账号发一条随机对话（点亮额度活跃地图）。
   * 老配置缺这个键时按开启处理，与后端默认一致。
   */
  chat_on_checkin?: boolean;
}

export interface CheckinLog {
  ts: number;
  accountId: string | null;
  email: string;
  result: string;
  error?: string;
  /** 该行所属档位；历史日志缺省按国内版处理。 */
  variant?: WbVariant;
}

export interface CheckinResult {
  result: string;
  error?: string;
  /** 国际版签到活动未开放时的业务判定；不写成功日志、不计入失败重试。 */
  inactive?: boolean;
}

export interface TravelConfig {
  enabled: boolean;
}

export type TravelStatusLabel = "untraveled" | "no-buddy" | "traveling" | "finished";

export interface TravelStatus {
  label: TravelStatusLabel;
  rewardCredit: number | null;
  locationName?: string | null;
  arriveAt?: number | null;
}

/** 单个受限模型；`model` 为 null 表示日志里归因不到模型（显示「未知模型」，不猜测）。 */
export interface RateLimitEntry {
  model: string | null;
  /** 官方日志原文给出的恢复时刻（毫秒）。 */
  resetAt: number;
  /** 该事件首次出现的时刻（毫秒）。 */
  firstSeenAt: number;
  /** 去重前的原始命中行数（调试/排查用）。 */
  hitCount: number;
}

/** 一个账号当前受限的全部模型（按 `resetAt` 升序）。 */
export interface AccountRateLimits {
  accountId: string;
  limited: RateLimitEntry[];
}

/** 模型限额台账：一次返回全部账号的当前受限状态（数据来自本机日志）。 */
export interface RateLimitsPayload {
  scannedAt: number;
  /** 固定 2 天，回显便于调试。 */
  windowDays: number;
  /** 只包含至少有一个受限模型的账号。 */
  accounts: AccountRateLimits[];
}

/** 一处客户端 hook 配置的安装状态。 */
export interface RateLimitHookTarget {
  /** 备份标签（codebuddy / workbuddy / workbuddy-ai）。 */
  label: string;
  /** `settings.json` 路径。 */
  path: string;
  /** 该客户端数据根目录是否存在（唯一的存在性判据；不存在则不参与安装）。 */
  exists: boolean;
  /** 该配置里是否已注册本工具的 Stop / FinalStop。 */
  installed: boolean;
}

/**
 * 限额 hook 安装状态：脚本 + 三处客户端配置逐项结果。
 *
 * `installed` = 脚本存在且至少一处配置注册成功；`lastEventAt` 是最近一次由后端
 * 入账的 hook 限额事件时刻（null = 从未收到）。
 */
export interface RateLimitHookStatus {
  scriptPath: string;
  scriptExists: boolean;
  eventsPath: string;
  installed: boolean;
  lastEventAt?: number | null;
  targets: RateLimitHookTarget[];
}

/** 限额监听开关（`~/.wb-switch/rate_limit_config.json`）。 */
export interface RateLimitConfig {
  enabled: boolean;
  /** 用户点过「卸载 hook」→ 启动时不再自动接入；重新点「接入 hook」清除。 */
  hookOptOut: boolean;
  /**
   * 是否扫描两个 CodeBuddy IDE 的日志（默认 true）。
   * IDE 的 429 不触发任何 hook 事件，日志是它唯一的数据源；关闭只影响 IDE 两源，
   * CLI / WorkBuddy 的 hook 实时上报与未接 hook 时的日志兜底不变。
   */
  scanIdeLogs: boolean;
}

export interface AutoRotateConfig {
  enabled: boolean;
  check_interval_minutes: number;
  cooldown_minutes: number;
  min_gap_hours: number;
  min_urgency_hours: number;
  /** 配置键兼容保留：轮换已改用「会话存活门控」，该值不再参与决策，设置页也不再展示。 */
  active_guard_minutes: number;
  min_remaining_credits: number;
}

export interface RotateLog {
  ts: number;
  action: string;
  reason?: string | null;
  from?: { id: string; name?: string | null } | null;
  to?: { id: string; name?: string | null } | null;
}

export interface RotateStatus {
  config: AutoRotateConfig;
  cliConfigured: boolean;
  activeAccountId: string | null;
  activeAccountName: string | null;
  lastCheckAt: number | null;
  lastSwitchAt: number | null;
}

export interface CreditResource {
  packageCode: string | null;
  packageName: string | null;
  total: number;
  remaining: number;
  used: number;
  status: number | null;
  expireAt: number | null;
  expired: boolean;
  expiringSoon: boolean;
}

export interface CreditExpiry {
  ok: boolean;
  accountId?: string | null;
  accountName?: string;
  updatedAt?: number;
  /** 企业版不限量（limitNum=-1）时为 null，展示按 `unlimited` 分支。 */
  totalCapacity?: number | null;
  /** 企业版不限量（limitNum=-1）时为 null，展示按 `unlimited` 分支。 */
  totalRemaining?: number | null;
  expiringSoonRemaining?: number;
  expiredRemaining?: number;
  soonestExpireAt?: number | null;
  expiringSoon?: boolean;
  expired?: boolean;
  resources?: CreditResource[];
  /** 企业配额不限量：数字字段为 null、资源列表为空，不当作 0 展示。 */
  unlimited?: boolean;
  error?: string;
}

export interface CreditStatsSummary {
  currentRemaining: number;
  currentCapacity: number;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  /** 获取口径：本地观察快照的余额总量上涨（含签到/活动/套餐额度） */
  creditToday: number;
  creditThisMonth: number;
  todayCheckedInAccounts: number;
  todaySuccess: number;
  todayAlready: number;
  todayFailed: number;
}

export interface CreditStatsDailyPoint {
  date: string;
  usage: number;
  /** 官方用量按模型聚合（全量，不受请求明细条数限制）；本地观察口径下为空 */
  models?: { model: string; requestCount: number; credit: number }[];
}

export interface CreditStatsAccount {
  accountId: string;
  accountName: string;
  isCurrent: boolean;
  currentRemaining: number | null;
  totalCapacity: number | null;
  lastSnapshotAt: number | null;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  /** 获取口径（可选：兼容尚未下发该字段的旧后端，缺省按 0 处理） */
  creditToday?: number;
  creditThisMonth?: number;
  checkedInToday: boolean | null;
  checkinStatusToday: string | null;
  lastCheckinAt: number | null;
  lastCheckinResult: string | null;
  /** 按账号的逐日观察消耗（缺省兼容旧后端）；官方可用时趋势图优先使用官方 daily */
  daily?: CreditStatsDailyPoint[];
  /** 档位标记。后端当前不下发，前端容忍性读取；缺省时回退到按 accountId 的映射表 */
  variant?: WbVariant;
}

export interface CreditStatsUsageEvent {
  kind: "usage";
  ts: number;
  date: string;
  accountId: string;
  accountName: string;
  amount: number;
  /** 档位标记。后端当前不下发，前端容忍性读取；缺省时回退到按 accountId 的映射表 */
  variant?: WbVariant;
}

export interface CreditStatsCheckinEvent {
  kind: "checkin";
  ts: number;
  date: string;
  accountId: string | null;
  accountName: string;
  result: string;
  error?: string | null;
  /** 档位标记。后端当前不下发，前端容忍性读取；缺省时回退到按 accountId 的映射表 */
  variant?: WbVariant;
}

export type CreditStatsEvent = CreditStatsUsageEvent | CreditStatsCheckinEvent;

export type CreditOfficialUsageStatus = "complete" | "partial" | "unavailable";

export interface CreditOfficialUsageSummary {
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
}

export interface CreditOfficialUsageModel {
  model: string;
  requestCount: number;
  credit: number;
}

export interface CreditOfficialUsageAccount {
  accountId: string;
  accountName: string;
  ok: boolean;
  requestCount: number;
  detailTruncated: boolean;
  usageToday: number | null;
  usage7Days: number | null;
  usageThisMonth: number | null;
  error?: string | null;
  reportedTotal?: number | null;
  fetchedCount?: number;
  /** 缺省兼容旧后端响应。 */
  models?: CreditOfficialUsageModel[];
  /** 按账号的逐日官方消耗（全量聚合，不受 requests 明细上限影响；缺省兼容旧后端） */
  daily?: CreditStatsDailyPoint[];
}

export interface CreditOfficialUsageRequest {
  accountId: string;
  accountName: string;
  requestId: string;
  credit: number;
  model: string;
  client: string;
  requestTime: string;
}

export interface CreditOfficialUsageError {
  accountId: string;
  accountName: string;
  error: string;
}

export interface CreditOfficialUsage {
  status: CreditOfficialUsageStatus;
  rangeStart: string;
  rangeEnd: string;
  /** 官方用量最近一次采集时间；缓存命中时保持采集当时的时间。 */
  collectedAt?: number;
  summary: CreditOfficialUsageSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditOfficialUsageAccount[];
  requests: CreditOfficialUsageRequest[];
  /** 官方全部有效请求按模型汇总；不受 requests 明细上限影响。 */
  models?: CreditOfficialUsageModel[];
  detailLimitPerAccount: number;
  errors: CreditOfficialUsageError[];
}

export interface CreditStatistics {
  generatedAt: number;
  retentionDays: number;
  coverageStartAt: number | null;
  summary: CreditStatsSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditStatsAccount[];
  events: CreditStatsEvent[];
  /** 官方接口不可用时仍使用上述本地观察字段；缺省兼容旧后端。 */
  officialUsage?: CreditOfficialUsage;
}

export interface TokenStatsTotals { total: number; input: number; output: number; cacheRead: number; cacheWrite: number; uncachedInput: number; records: number; cacheHitRate: number | null; }
export interface TokenStatsGroup extends TokenStatsTotals { key: string; title?: string | null; project?: string; sessionId?: string; }
/** 一次模型调用的明细行；`total = input + output + cacheWrite`，`uncachedInput = max(0, input - cacheRead)`，`thinking` 是 `output` 中思考过程的 token 数（回复内容 = max(0, output - thinking)），均与聚合口径一致。 */
export interface TokenStatsRequestRow { timestamp: number; model: string; project: string; sessionId: string; title?: string | null; input: number; output: number; cacheRead: number; cacheWrite: number; uncachedInput: number; thinking: number; total: number; }
/** `workbuddy-ai` 为国际版本地数据源，与国内版分开统计，数据源缺失时为空集。 */
export interface TokenStatsSource { source: "workbuddy" | "workbuddy-ai" | "codebuddy-cli" | "codebuddy-ide"; summary: TokenStatsTotals; models: TokenStatsGroup[]; projects: TokenStatsGroup[]; sessions: TokenStatsGroup[]; daily: TokenStatsGroup[]; /** Optional model-specific daily series for trend filtering. */ dailyByModel?: Record<string, TokenStatsGroup[]>; /** 仅 CodeBuddy CLI 来源返回的最近请求明细；旧后端或缺失时按空数组处理。 */ requests?: TokenStatsRequestRow[]; hours: TokenStatsGroup[]; filesScanned: number; parseErrors: number; coverageStartAt?: number | null; coverageEndAt?: number | null; }
export interface TokenStatistics { generatedAt: number; rangeDays?: number | null; sources: TokenStatsSource[]; }

export interface CodeBuddyCliStatus {
  configured: boolean;
  authMode?: "settings-env" | "api-key-helper";
  environmentOverride?: boolean;
  settingsPresent: boolean;
  helperPresent: boolean;
  helperSupportsAccountIds: boolean;
  helperCurrent?: boolean;
  migrationRequired?: boolean;
  /**
   * 需要用户介入的脱节：settings.json 里的 Token 已长期匹配不上账号库。
   *
   * 与 `syncInProgress` 互斥。保活刷新造成的短暂不一致不算脱节。
   */
  syncPending?: boolean;
  /**
   * 保活刷新刚换完账号库的 Token、settings.json 尚未同步的正常中间态。
   *
   * 仅在账号库 mtime 落在同步宽限窗口内时为真；此时不该提示用户点「更新认证」。
   */
  syncInProgress?: boolean;
  activeIndex: number | null;
  activeAccountId: string | null;
  activeAccountName: string | null;
  /** 当前 CLI 账号所属档位；尚未接入时缺省。 */
  activeAccountVariant?: WbVariant | null;
  accountCount: number;
  statePath: string;
}

export interface CodeBuddyCliSwitchResult {
  ok: boolean;
  configured: boolean;
  synced: boolean;
  verified?: boolean;
  authMode?: "settings-env" | "api-key-helper";
  activeIndex?: number;
  activeAccountId?: string;
  source?: string;
  skipped?: boolean;
  regionChanged?: boolean;
  cliClosed?: boolean;
  closedProcessCount?: number;
  message?: string;
  error?: string;
}

export interface CodeBuddyCliInstallResult {
  ok: boolean;
  configured: boolean;
  helperPresent: boolean;
  helperSupportsAccountIds: boolean;
  verified?: boolean;
  authMode?: "settings-env" | "api-key-helper";
  message?: string;
  error?: string;
}

export interface GithubConfig {
  owner?: string;
  repo?: string;
  proxy?: string;
  /** 显式代理开关：默认关闭。地址可以留着，只有打开才生效。 */
  proxy_enabled?: boolean;
  /** 发现新版本后是否自动后台下载。缺省 true（用户诉求就是"后台自动下"）。 */
  auto_download?: boolean;
  /** 用户明确跳过的版本号；空/缺省 = 没有跳过任何版本。 */
  skipped_version?: string;
}

export interface UpdateInfo {
  ok: boolean;
  current?: string;
  latest?: string;
  latestTag?: string;
  hasUpdate?: boolean;
  releaseName?: string;
  releaseUrl?: string;
  publishedAt?: string;
  error?: string;
  message?: string;
}

/** 统一更新服务的阶段（Rust 状态机，经 `update-state` 事件推送）。 */
export type UpdatePhase =
  | "idle"
  | "checking"
  | "upToDate"
  | "available"
  | "downloading"
  | "readyToRestart"
  | "error";

/**
 * 更新状态快照：托盘菜单与前端弹窗显示同一阶段（单一真相源在 Rust）。
 *
 * `latest` 无 `v` 前缀；`percent` 在下载总量未知时为 null；`message` 是错误 / 提示文案。
 */
export interface UpdateSnapshot {
  phase: UpdatePhase;
  latest: string | null;
  percent: number | null;
  message: string | null;
  checkedAt: number | null;
}

/** CodeBuddy CN IDE（桌面客户端）状态；与 CodeBuddy CLI 独立。 */
export interface CodeBuddyCnIdeStatus {
  installed: boolean;
  running: boolean;
  /**
   * 是否存在 IDE 登录态（`state.vscdb` 里有会话 secret 行）。
   *
   * 只读查询、不解密；查询失败或文件不存在时为 false。仅用于文案与入口判定，
   * 不参与切换判定。国内版与国际版 IDE 状态都返回该字段（旧后端可能缺省 `undefined`）。
   */
  loggedIn?: boolean;
  dataDir: string | null;
  dbPath: string | null;
  dbExists: boolean;
  appPath: string | null;
  activeAccountId: string | null;
  activeAccountName: string | null;
  detectedFrom?: string;
  statePath?: string;
}

export interface CodeBuddyCnIdeSwitchResult {
  ok: boolean;
  account: string;
  accountId: string;
  dbPath?: string;
  restarted?: boolean;
  message?: string;
  /** 切换时复制会话的结果（未勾选复制时不返回）。 */
  sessionCopy?: VscodeSessionCopyResult;
  /** 切换时同步关联会话的结果（未勾选同步时不返回）。 */
  sessionSync?: VscodeSessionSyncReport;
}

/** VS Code 内 CodeBuddy 扩展（tencent-cloud.coding-copilot）状态；与 CN IDE / CLI 独立。 */
export interface VscodeExtStatus {
  /** VS Code 用户数据目录是否存在。 */
  installed: boolean;
  /** CodeBuddy 扩展是否已安装（globalStorage/<extensionId> 存在）。 */
  extensionInstalled: boolean;
  running: boolean;
  /**
   * 是否存在扩展登录态（`state.vscdb` 里有会话 secret 行）。
   *
   * 只读查询、不解密；查询失败或文件不存在时为 false。仅用于提示文案，
   * 不参与切换判定（false 时切换按新会话写入，同样可用）。
   */
  loggedIn: boolean;
  dataDir: string | null;
  dbPath: string | null;
  dbExists: boolean;
  activeAccountId: string | null;
  activeAccountName: string | null;
  detectedFrom?: string;
  statePath?: string;
}

export interface VscodeExtSwitchResult {
  ok: boolean;
  account: string;
  accountId: string;
  dbPath?: string;
  /** 本次是否真的执行了「关闭并重新打开 VS Code」（切换前未运行时为 false）。 */
  restarted?: boolean;
  /** 本次切换是否由 wb-switch 关闭了 VS Code（为 false 时表示编辑器本来没运行）。 */
  closedByUs?: boolean;
  /** 切换前是否读到了既有会话；false = 扩展未登录，按新会话载荷写入。 */
  existingSession?: boolean;
  /**
   * 后端生成的生效说明：区分「已重新打开 VS Code」「自动重开失败（含原因）」
   * 「本来未运行」三种情形，不再出现「重载窗口生效」。
   */
  message?: string;
  /** 切换时复制会话的结果（未勾选复制时不返回）。 */
  sessionCopy?: VscodeSessionCopyResult;
  /** 切换时同步关联会话的结果（未勾选同步时不返回）。 */
  sessionSync?: VscodeSessionSyncReport;
}

/** JetBrains IDE 的一条配置目录状态（IDEA / PyCharm 各自独立）。 */
export interface JetbrainsTargetStatus {
  /** 配置目录名（如 "PyCharm2026.2"）。 */
  configDir: string;
  /** 是否安装了 CodeBuddy 插件（plugins/coding-copilot* 目录存在）。 */
  pluginInstalled: boolean;
  running: boolean;
  /** 是否存在插件登录态（secret-storage.xml 里有会话 secret）。 */
  loggedIn: boolean;
  secretPath: string;
}

export interface JetbrainsStatus {
  /** 是否存在任一受支持的 JetBrains 配置目录。 */
  installed: boolean;
  /** 是否至少一个配置目录安装了 CodeBuddy 插件。 */
  pluginInstalled: boolean;
  /** 是否有「装了插件」的 IDE 正在运行。 */
  running: boolean;
  /** 是否任一装了插件的目标存在登录态。 */
  loggedIn: boolean;
  configRoot: string | null;
  targets: JetbrainsTargetStatus[];
  activeAccountId: string | null;
  activeAccountName: string | null;
  detectedFrom?: string;
  statePath?: string;
}

export interface JetbrainsSwitchResult {
  ok: boolean;
  account: string;
  accountId: string;
  /** 本次写入的配置目录名列表（一次切换覆盖所有装了插件的 IDE）。 */
  written?: string[];
  /** 本次是否真的执行了「关闭并重新打开 IDE」。 */
  restarted?: boolean;
  /** 本次切换是否由 wb-switch 关闭了 IDE。 */
  closedByUs?: boolean;
  message?: string;
}

/** VS Code 扩展的一条可复制会话。 */
export interface VscodeSession {
  /** 会话 id（32 位小写 hex）。 */
  id: string;
  /** 工作区目录名 md5(工作区)（32 位小写 hex，不可反解为路径）。 */
  workspaceHash: string;
  /** 会话标题；无标题时为 "(无标题)"。 */
  title: string;
  /** 最近消息时间（epoch 毫秒，0 表示未知）。 */
  updatedAt: number;
  /** 会话类型（如 "craft"）。 */
  type: string;
  /** 是否包含正文消息。 */
  hasHistory: boolean;
}

/** 复制某项会话的引用（工作区 hash + 会话 id）。 */
export interface VscodeSessionRef {
  workspaceHash: string;
  conversationId: string;
}

/** VS Code 扩展会话复制结果。 */
export interface VscodeSessionCopyResult {
  sourceUid: string | null;
  targetUid: string;
  copied: { workspaceHash: string; oldId: string; newId: string; messages: number }[];
  errors?: { workspaceHash: string; conversationId: string; error: string }[];
  /** 索引备份根目录（便于用户找回）。 */
  backup?: string;
  /**
   * 已复制但未能建立关联的条目（复制成功、登记失败；登记失败不回滚复制）。
   * 前端据此提示「已复制但未建立关联」，不得静默当成成功。
   */
  linkErrors?: { workspaceHash?: string; conversationId?: string; error: string }[];
}

/** VS Code 插件侧「关联会话」的同步结果项（与 core 报告同形）。 */
export interface VscodeSessionSyncResultItem {
  groupId: string;
  status: "synced";
  verdict: SessionSyncVerdict;
  mode: SessionSyncMode;
  /** 记录数（条）：source 为来源当前条数，targetBefore/target 为副本写入前后条数。 */
  recordCount: { source: number; targetBefore: number; target: number };
  /** 附件合并结果（快进模式补入的附件）。 */
  assets: { copied: number; overwritten: number };
  /** 本次备份目录（快进为索引备份，覆盖为整目录备份）。 */
  backup: string;
  message: string;
}

/** 被跳过的同步项（`reasonCode` 为 previewStale 时说明预览已过期，不得显示为成功）。 */
export interface VscodeSessionSyncSkippedItem {
  groupId: string;
  status: "skipped";
  reasonCode: string;
  message: string;
  verdict: SessionSyncVerdict | null;
}

/** VS Code 插件侧「关联会话」同步报告；`errors` 里可能是整批被拒（无 groupId）。 */
export interface VscodeSessionSyncReport {
  synced: VscodeSessionSyncResultItem[];
  skipped: VscodeSessionSyncSkippedItem[];
  errors: { groupId?: string; error: string }[];
}

/** 可复制会话列表。 */
export interface VscodeSessionList {
  sourceUid: string | null;
  sessions: VscodeSession[];
  /** 无法解析（损坏）的工作区索引数量。 */
  skipped?: number;
  /**
   * 扩展数据根目录；`null` 表示未找到（与「有目录但没有会话」区分）。
   * 可选：旧后端不返回该字段时为 `undefined`，前端按旧文案处理。
   */
  dataRoot?: string | null;
}

// ---------------------------------------------------------------------------
// 成长中心任务（beta）
// ---------------------------------------------------------------------------

/** 一条成长任务。`progressKnown` 为 false 时上游没有下发进度，进度值不可信。 */
export interface TaskCenterItem {
  code: string;
  title: string;
  /** 达成条件或操作指引，可能为空串。 */
  hint: string;
  current: number;
  target: number;
  progressKnown: boolean;
  credit: number;
  energy: number;
  acceptStatus: string;
  claimed: boolean;
  claimable: boolean;
}

export interface TaskCenterList {
  tasks: TaskCenterItem[];
  /** 非致命错误（例如小程序口径拉取失败），主列表仍然可用。 */
  errors?: string[];
  claimable?: number;
  acceptable?: number;
  /** 整份列表取不到时的致命错误。 */
  error?: string;
}

export interface TaskAcceptResult {
  accepted: string[];
  /** 上游逐个任务返回的失败项（例如 task not found），外层成功也要看这里。 */
  failed?: { code: string; reason: string }[];
  note?: string;
  error?: string;
}

export interface TaskClaimResult {
  claimed: { code: string; title?: string; credit: number }[];
  /** 未领取的原因：已领取 / 上游未下发进度 / 需真实行为达标。 */
  skipped: { code: string; title?: string; reason: string }[];
  errors: { code: string; title?: string; error: string }[];
  creditTotal?: number;
  error?: string;
}

/** 一键完成里单个任务的执行结果。 */
export interface GrowthAutoItem {
  code: string;
  title: string;
  /** done=已领到奖励；progressed=行为已触发但进度未落账；skipped=无需动作；failed=失败。 */
  status: "done" | "progressed" | "skipped" | "failed";
  message: string;
  credit: number;
}

export interface GrowthAutoResult {
  results: GrowthAutoItem[];
  /** 本次到账积分合计。 */
  creditTotal?: number;
  /** 非致命提示（例如补领阶段的失败项）。 */
  note?: string;
  error?: string;
}

// ---------------------------------------------------------------------------
// 数据迁移（整包 zip 备份 / 还原）
// ---------------------------------------------------------------------------

/** 一个可打包的数据组（账号 / 设置 / 日志 / 统计 / 记忆 / 关联）。 */
export interface ArchiveGroupInfo {
  /** accounts | settings | logs | stats | memory | experts | links */
  id: string;
  title: string;
  description: string;
  /** 含登录凭据：zip 内为明文，导出后不可外传。 */
  sensitive: boolean;
  files: number;
  bytes: number;
  /** 组内最大单文件字节数；超过 core 的 16 MiB 上限时这一组无法导出。 */
  largestFileBytes: number;
}

/** 打开一个 zip 后的清单（导入前展示给用户确认）。 */
export interface ArchiveInspect {
  formatVersion: number;
  appVersion: string;
  exportedAt: number;
  groups: ArchiveGroupInfo[];
}

/** 导出结果：桌面端写盘返回 `path`；webui 返回 base64 `data` 由浏览器下载。 */
export interface ArchiveExportResult {
  path?: string;
  fileName: string;
  bytes: number;
  groups: string[];
  data?: string;
}

/** 导入结果计数；`backupDir` 是导入前自动备份所在目录。 */
export interface ArchiveImportResult {
  groups: string[];
  backupDir: string;
  accountsAdded: number;
  accountsUpdated: number;
}
