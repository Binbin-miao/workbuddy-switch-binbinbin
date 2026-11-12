import { useEffect, useLayoutEffect, useState } from "react";
import { CircleAlert, ExternalLink, Loader2 } from "lucide-react";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { DialogAutoHeight } from "@/components/ui/dialog-auto-height";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { SessionSyncSection, type SessionLinksMeta } from "@/components/session-sync-section";
import { SessionTreeList } from "@/components/session-tree";
import * as api from "@/lib/api";
import { displayName } from "@/lib/account-display";
import {
  createSwitchProgressPoller,
  shouldBlockDismiss,
} from "@/lib/switch-dialog-lifecycle";
import { accountVariant, variantAppName } from "@/lib/variant";
import { useAccountsStore } from "@/stores/accounts";
import {
  assetSyncSuppressedByMirror,
  canSubmitMirror,
  mirrorSubmitBlockReason,
  canSubmitMemory,
  initialMirrorSelection,
  initialSwitchExtras,
  memoryAssetLabel,
  memorySummaryWarning,
  memoryToasts,
  resolveMemorySources,
  showsSourcePicker,
  toMirrorSwitchArgs,
  toMemorySwitchArgs,
  type MirrorSelection,
  type MemoryMode,
  type MemoryToastLevel,
} from "@/lib/switch-memory-mode";
import {
  hasMirrorRiskAcknowledged,
  rememberMirrorRiskAcknowledged,
} from "@/lib/mirror-risk-ack";
import {
  forgetMirrorBackup,
  lastMirrorBackup,
  rememberMirrorBackup,
} from "@/lib/mirror-backup";
import type {
  AccountMeta,
  DeletedSyncPreview,
  MirrorPreview,
  Session,
  SessionLinkPreviewGroup,
  SessionSyncSelection,
  SidecarOutcome,
  TemporaryFileInfo,
} from "@/lib/types";

/** 打开 / 重开共用的单一初值源：破坏性项一律不预选。 */
const INITIAL_EXTRAS = initialSwitchExtras();

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 目标账号 */
  account: AccountMeta | null;
  /** 切换完成后刷新列表 */
  onDone?: () => void;
}

/** 设计稿的 tab 样式：下划线指示 + 可选计数徽标。 */
const TAB_TRIGGER_CLASS =
  "-mb-px h-9 flex-none rounded-none border-b-2 border-transparent px-0.5 pb-2 text-sm font-medium text-muted-foreground hover:text-foreground data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:text-foreground data-[state=active]:shadow-none";

/** tab 计数徽标：0 时不显示，避免出现空的「0」。 */
function tabCount(count: number) {
  return count > 0 ? (
    <span className="ml-1 text-xs text-muted-foreground tabular-nums">{count}</span>
  ) : null;
}

/** 记忆结果 toast 的分级出口：表驱动，组件里只做一次遍历调用。 */
const MEMORY_TOAST_EMIT: Record<
  MemoryToastLevel,
  (title: string, options: { description: string }) => void
> = {
  success: (title, options) => toast.success(title, options),
  error: (title, options) => toast.error(title, options),
  warning: (title, options) => toast.warning(title, options),
  info: (title, options) => toast.info(title, options),
};

/**
 * 会话列表区最小高度：加载态、空态与列表共用同一下沿。
 *
 * 弹窗垂直居中，内容高度一变弹窗就上下撑开；
 * 打开时先渲染加载态、会话数据到达后换成列表，两端高度差越大跳得越明显。
 * 与「关联会话」tab 的下沿取同一数值，两个 tab 打开时的高度表现保持一致。
 */
const LIST_MIN_H = "min-h-[min(7.5rem,26vh)]";

/** 临时备份残留的可读描述：优先标题，其次会话 id，最后操作 id。 */
function describeTemporaryFile(item: TemporaryFileInfo): string {
  const label = item.title || item.sessionId || item.operationId;
  return `${label}：${item.reason}`;
}

/** 同一操作只保留一条：复制 / 同步 / 恢复三个报告可能重复上报同一残留。 */
function dedupeTemporaryFiles(items: TemporaryFileInfo[]): TemporaryFileInfo[] {
  const seen = new Map<string, TemporaryFileInfo>();
  for (const item of items) {
    const key = `${item.operationId}:${item.state}`;
    const existing = seen.get(key);
    if (!existing || (existing.state === "cleanupPending" && item.state === "needsRecovery")) {
      seen.set(key, item);
    }
  }
  return [...seen.values()];
}

/** 切换账号弹窗：可勾选当前账号的会话复制到目标账号（路径 B）。 */
export function SwitchAccountDialog({ open, onOpenChange, account, onDone }: Props) {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [loadingSessions, setLoadingSessions] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  /** 展开的节点：任务 / 空间 / 文件夹。默认全部收起。 */
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [currentUid, setCurrentUid] = useState<string | null>(null);
  const [progress, setProgress] = useState("");
  /** 会话同步：勾选结果（默认值来自后端 defaultChecked）与预览组（结果反馈回显用）。 */
  const [syncSelections, setSyncSelections] = useState<SessionSyncSelection[]>([]);
  const [syncGroups, setSyncGroups] = useState<SessionLinkPreviewGroup[]>([]);
  /** 当前 tab：默认关联会话；该 tab 不可用时回落到复制会话。 */
  const [tab, setTab] = useState<"links" | "copy" | "sync" | "mirror">("links");
  /** 关联会话区块上报的状态：tab 徽标与常驻提示用。 */
  const [linksMeta, setLinksMeta] = useState<SessionLinksMeta | null>(null);
  /** 工作记忆三选一：不同步（默认）/ 合并 / 用母本复制覆盖。破坏性项永不默认选中。 */
  const [memoryMode, setMemoryMode] = useState<MemoryMode>(INITIAL_EXTRAS.memoryMode);
  /** 用户手动选过的母本 uid；null = 跟随渲染期算出的默认母本（当前登录账号）。 */
  const [memorySourceUid, setMemorySourceUid] = useState<string | null>(INITIAL_EXTRAS.memorySourceUid);
  /** 专家团：把本机各账号已添加的专家团汇总成一份写回每个账号（只增不减）。 */
  const [mergeExperts, setMergeExperts] = useState(INITIAL_EXTRAS.mergeExperts);
  /** 人设：把最完整的一份 SOUL / IDENTITY / USER 同步到所有账号（默认全关）。 */
  const [syncPersona, setSyncPersona] = useState(INITIAL_EXTRAS.syncPersona);
  /** 人设覆盖策略：默认只替换仍是客户端默认模板的目标，手改过的保留。 */
  const [personaMode, setPersonaMode] = useState<"safe" | "overwrite">(INITIAL_EXTRAS.personaMode);
  /** 镜像切换：显式母本 + 已知会先备份 + 二次确认，独立通道。 */
  const [mirror, setMirror] = useState<MirrorSelection>(initialMirrorSelection);
  const [mirrorPreview, setMirrorPreview] = useState<MirrorPreview | null>(null);
  const [mirrorPreviewBusy, setMirrorPreviewBusy] = useState(false);
  const [mirrorError, setMirrorError] = useState("");
  /** 镜像切换的二次确认弹层：复述母本名 + 影响数 + 勾选「我已知会先备份」。 */
  const [mirrorConfirmOpen, setMirrorConfirmOpen] = useState(false);
  /** 首次启用镜像的风险确认弹层：确认一次后本机记住，之后不再重复询问。 */
  const [mirrorRiskOpen, setMirrorRiskOpen] = useState(false);
  /** 成功后的一次性还原入口：明文备份绝对路径。 */
  const [mirrorBackup, setMirrorBackup] = useState<string | null>(null);
  /** 删除同步：只读扫描结果 + 用户是否已确认（确认后随本次切号执行）。 */
  const [deletedSync, setDeletedSync] = useState<DeletedSyncPreview | null>(null);
  const [deletedSyncBusy, setDeletedSyncBusy] = useState(false);
  const [deletedSyncError, setDeletedSyncError] = useState("");
  const [deletedSyncConfirmed, setDeletedSyncConfirmed] = useState(false);

  /** 镜像启用后必须关闭的通道：附加同步三项与一键全选（语义互斥，防双重写入）。 */
  const mirrorBlocks = assetSyncSuppressedByMirror(mirror);

  // 账号库与当前登录态：母本下拉的选项与默认值都由此派生，不额外发请求。
  const accounts = useAccountsStore((s) => s.accounts);
  const statusCurrent = useAccountsStore((s) => s.status?.current ?? null);

  /**
   * 登录态母本 uid：镜像切换的母本恒取此刻登录账号，只读，绝不由用户手选。
   * 与后端 `current_user_uid_at` 同源；`status.current.uid` 即账号页 isWorkbuddyCurrent 口径。
   */
  const loginUid = statusCurrent?.uid ?? null;

  const variant = accountVariant(account);
  const accountId = account?.id;

  /**
   * 母本默认值在渲染期派生，不用 useEffect 同步；用户一旦手动改过
   * （memorySourceUid !== null）就不再被默认值覆盖。
   *
   * 「当前登录账号」只认 status.current：它与账号页 isWorkbuddyCurrent 同口径。
   * 不用组件里的 currentUid —— 那个来自 listSessions(目标账号档位).current，
   * 跨档位切号时可能指向另一档位的账号，拿它当母本会静默猜错。
   * status 缺失时 resolveMemorySources 会退到第一项并置 needsConfirm，界面提示确认。
   */
  const memorySources = resolveMemorySources(accounts, [
    statusCurrent?.uid ?? null,
    statusCurrent?.email ?? null,
  ]);
  const effectiveSourceUid = memorySourceUid ?? memorySources.defaultUid ?? "";
  const sourceNeedsConfirm = memorySources.needsConfirm && memorySourceUid === null;
  const sourceLabel =
    memorySources.options.find((option) => option.uid === effectiveSourceUid)?.label ?? "母本账号";

  /** busy 期间任何路径都不允许关闭弹窗：后端切号不可取消，关掉只会造成状态脱节。 */
  function handleOpenChange(next: boolean) {
    if (!next && shouldBlockDismiss(busy)) return;
    onOpenChange(next);
  }

  // 桌面端：弹窗打开期间常驻订阅后端进度事件。不随 busy 重建订阅——
  // listen() 是异步注册，等 busy=true 才注册会漏掉切号刚开始推送的首帧。
  useEffect(() => {
    if (!open || api.isWebui()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<{ message: string }>("switch-progress", (e) => {
      setProgress(e.payload.message);
    }).then((fn) => {
      // StrictMode / 弹窗秒关时 Promise 可能晚于清理才 resolve：立即解绑，避免残留监听。
      if (disposed) {
        fn();
        return;
      }
      unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [open]);

  // webui：没有事件通道，仅在「弹窗打开且切号进行中」轮询进度快照。
  // 弹窗关闭 / 切号结束即停止，避免常驻组件在后台每 600ms 发一次无效请求。
  useEffect(() => {
    if (!open || !busy || !api.isWebui()) return;
    return createSwitchProgressPoller({
      fetchProgress: api.switchProgress,
      onMessage: setProgress,
      intervalMs: 600,
      setInterval: (cb, ms) => window.setInterval(cb, ms),
      clearInterval: (id) => window.clearInterval(id),
    });
  }, [open, busy]);

  // 母本恒等于登录态 uid：弹窗打开或登录态变化时同步，用户不可手选。
  // 跨母本必须作废旧预览与全部确认，凭证绝不跨母本续用（登录态在切号期间可能被外部刷新）。
  useEffect(() => {
    if (!open) return;
    if (mirror.motherUid === loginUid) return;
    setMirror({
      motherUid: loginUid,
      // 风险确认一旦在本机确认过就不再追问；其余破坏性选择一律回到未确认。
      acknowledged: hasMirrorRiskAcknowledged(),
      confirmed: false,
      enabled: false,
    });
    setMirrorPreview(null);
    setMirrorError("");
    setMirrorConfirmOpen(false);
  }, [open, loginUid, mirror.motherUid]);

  // 立即打开弹框；首次绘制前重置旧内容，两个 tab 各自加载数据。
  useLayoutEffect(() => {
    if (!open || !accountId) return;
    let cancelled = false;
    setSelected(new Set());
    setExpanded(new Set());
    setPermCheck(null);
    setError("");
    // 旧版本允许在切号中关掉弹窗：重开时必须回到可交互的初始态，
    // 否则遮罩会一直显示「正在处理中」且所有控件禁用。
    setBusy(false);
    setProgress("");
    setSessions([]);
    setCurrentUid(null);
    setSyncSelections([]);
    setSyncGroups([]);
    setLinksMeta(null);
    // 打开重置：与初始 useState 共用同一个纯函数，保证「初值 == 重开值」，
    // 默认全关（人设关闭）+ 镜像切换母本置 null，绝不把上次的破坏性选择静默延续。
    const extras = initialSwitchExtras();
    setMemoryMode(extras.memoryMode);
    setMemorySourceUid(extras.memorySourceUid);
    setMergeExperts(extras.mergeExperts);
    setSyncPersona(extras.syncPersona);
    setPersonaMode(extras.personaMode);
    setMirror(extras.mirror);
    setMirrorPreview(null);
    setMirrorPreviewBusy(false);
    setMirrorError("");
    setMirrorConfirmOpen(false);
    // 上次备份路径持久化在 localStorage：切号会重启应用、弹窗会关，
    // 只放 state 里等于还原入口永远不再出现（承诺可还原却找不到入口）。
    setMirrorBackup(lastMirrorBackup());
    setTab("links");
    setLoadingSessions(true);
    void api.listSessions(variant)
      .then((res) => {
        if (cancelled) return;
        setSessions(res.sessions);
        setCurrentUid(res.current);
      })
      .catch((e) => {
        if (!cancelled) setError(api.asError(e));
      })
      .finally(() => {
        if (!cancelled) setLoadingSessions(false);
      });
    // 关闭时只取消回写，保留内容到退出动画结束。
    return () => { cancelled = true; };
  }, [open, accountId, variant]);

  function toggleSession(id: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function toggleFolder(ids: string[]) {
    setSelected((prev) => {
      const next = new Set(prev);
      const allOn = ids.length > 0 && ids.every((id) => next.has(id));
      if (allOn) ids.forEach((id) => next.delete(id));
      else ids.forEach((id) => next.add(id));
      return next;
    });
  }

  function toggleExpanded(key: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  /** 母本（当前登录账号）标签：二次确认弹层据此复述，避免只显示一串 uid。 */
  const mirrorMotherLabel =
    memorySources.options.find((option) => option.uid === mirror.motherUid)?.label ??
    mirrorPreview?.motherLabel ??
    "母本账号";

  /**
   * 二次确认按钮的唯一阻塞原因：按钮 `disabled` 与下方文案都由此派生。
   * `null` = 可以点；非空 = 逐字告诉用户还缺什么，绝不静默禁用。
   */
  const mirrorSubmitBlocker = mirrorSubmitBlockReason(mirror, mirrorPreview);

  /** 独立只读预览：母本由后端取自登录态，返回的 motherUid 只做只读校验与回显。 */
  async function loadMirrorPreview() {
    if (!loginUid || !accountId) return;
    setMirrorPreviewBusy(true);
    setMirrorError("");
    setMirrorPreview(null);
    setMirror((prev) => ({ ...prev, confirmed: false }));
    try {
      const res = await api.mirrorPreview({ targetAccountId: accountId, variant });
      // 预览期间登录账号发生变化：不静默丢弃，明确提示重新打开切号窗口。
      if (res.motherUid !== loginUid) {
        setMirrorError("当前登录账号已变化，请重新打开切号窗口再试");
        return;
      }
      setMirrorPreview(res);
    } catch (e) {
      setMirrorError(api.asError(e));
    } finally {
      setMirrorPreviewBusy(false);
    }
  }

  /** 镜像启用时清空已勾的附加同步与逐项会话选择：语义互斥，避免与整份镜像双重写入。 */
  function disableAssetSync() {
    setMemoryMode("off");
    setSyncPersona(false);
    setMergeExperts(false);
    setSelected(new Set());
    setSyncSelections([]);
    setSyncGroups([]);
  }

  /**
   * 启用 / 停用镜像切换。
   *
   * 首次启用要求先确认风险（本机记住，之后免勾）；已记住则直接启用。
   * 停用时一并作废二次确认——破坏性选择绝不静默延续到下一次切号。
   */
  function toggleMirrorEnabled(next: boolean) {
    if (!next) {
      setMirror((prev) => ({ ...prev, enabled: false, confirmed: false }));
      return;
    }
    if (hasMirrorRiskAcknowledged()) {
      setMirror((prev) => ({ ...prev, enabled: true, acknowledged: true, confirmed: false }));
      disableAssetSync();
      return;
    }
    // 首次使用：先开风险确认弹层，确认后才真正启用。
    setMirrorRiskOpen(true);
  }

  /** 首次风险确认通过：记住「用户已知风险」，并真正启用镜像。 */
  function confirmMirrorRisk() {
    rememberMirrorRiskAcknowledged();
    setMirror((prev) => ({ ...prev, enabled: true, acknowledged: true, confirmed: false }));
    disableAssetSync();
  }

  /** 二次确认通过：复述母本名 + 影响数后才置 confirmed，doSwitch 据此产出独立 mirrorPlan。 */
  function confirmMirror() {
    setMirror((prev) => ({ ...prev, confirmed: true }));
    setMirrorConfirmOpen(false);
  }

  /** 扫描「别的账号已删、这里还留着」的会话（只读，不写任何数据）。 */
  async function scanDeletedSync() {
    setDeletedSyncBusy(true);
    setDeletedSyncError("");
    setDeletedSync(null);
    setDeletedSyncConfirmed(false);
    try {
      setDeletedSync(await api.deletedSyncPreview());
    } catch (e) {
      setDeletedSyncError(api.asError(e));
    } finally {
      setDeletedSyncBusy(false);
    }
  }

  /** 还原入口：toast 一次性 action 与 tab 内常驻入口共用同一实现。 */
  function restoreMirrorBackup(backup: string) {
    void api
      .restoreMirrorBackup({ backup })
      .then((r) => {
        if (r.ok) {
          toast.success("已还原镜像切换前的备份");
          forgetMirrorBackup();
          setMirrorBackup(null);
        } else {
          const message = r.error ?? "备份可能已不存在";
          toast.error("还原失败", { description: message });
          // 只有"路径已失效"这类终态才清掉入口。锁冲突（另一个进程正在切号，
          // 文案是「请稍后重试」）必须保留——清掉唯一入口等于丢回滚路径，
          // 留一条死入口只是碍眼，代价小得多。
          if (/不存在|不可读|读取失败|解析失败|不在允许|已损坏|不受支持/.test(message)) {
            forgetMirrorBackup();
            setMirrorBackup(null);
          }
        }
      })
      .catch((e) => toast.error("还原失败", { description: api.asError(e) }));
  }

  async function doSwitch() {
    if (!account) return;
    // 破坏性操作守卫：覆盖模式没选出母本时不发请求，也不静默降级成合并。
    if (!canSubmitMemory(memoryMode, effectiveSourceUid)) return;
    // 镜像守卫：用户启用了镜像就必须走完预览与二次确认才允许切号。
    // 否则请求里不会带 mirrorPlan——切号照常成功，用户却以为镜像已经跑过。
    if (mirror.enabled && !canSubmitMirror(mirror, mirrorPreview)) {
      setError("镜像切换还没走完：请先预览受影响清单并完成二次确认，或取消启用镜像切换");
      return;
    }
    // 镜像与复制/同步会话互斥（后端同样会拒绝，这里先拦住，别让用户白等一次切号）。
    if (mirror.enabled && (selected.size > 0 || syncSelections.length > 0)) {
      setError("镜像切换与复制/同步会话互斥：一次切号只能选其中一种");
      return;
    }
    setBusy(true);
    setProgress("正在切换账号…");
    setError("");
    const requestedCopy = selected.size > 0;
    const requestedSync = syncSelections.length > 0;
    try {
      const res = await api.switchAccount({
        accountId: account.id,
        copySessionIds: requestedCopy ? [...selected] : undefined,
        // 勾选绑定预览凭据；执行前后端会重新校验，版本变化则跳过该项。
        syncSelections: requestedSync ? syncSelections : undefined,
        ...toMemorySwitchArgs(memoryMode, effectiveSourceUid),
        mergeExperts: mergeExperts || undefined,
        syncPersona: syncPersona || undefined,
        personaMode: syncPersona ? personaMode : undefined,
        // 独立通道：只有显式母本 + 已知会先备份 + 二次确认 + 预览凭证绑定同一母本才产出。
        ...toMirrorSwitchArgs(mirror, mirrorPreview),
        // 删除同步：只有用户看过清单并确认过才带上；服务端会整份重算，篡改无效。
        ...(deletedSyncConfirmed && deletedSync
          ? {
              deletedSyncPlan: {
                targets: deletedSync.targets,
                expectedDigest: deletedSync.expectedDigest,
              },
            }
          : {}),
      });
      const nickname = displayName(account);
      const parts: string[] = [];
      const copyReport = res.sessionCopy;
      const copiedCount = copyReport?.copied?.length ?? 0;
      const linkedCount = copyReport?.alreadyLinked?.length ?? 0;
      const copyErrors = copyReport?.errors ?? [];
      if (copiedCount > 0) parts.push(`已复制 ${copiedCount} 个会话`);
      if (linkedCount > 0) parts.push(`已关联 ${linkedCount} 个之前复制过的会话`);
      if (copyErrors.length > 0) parts.push(`会话复制失败 ${copyErrors.length} 个`);
      const syncReport = res.sessionSync;
      const syncedItems = syncReport?.synced ?? [];
      const skippedItems = syncReport?.skipped ?? [];
      const syncErrors = syncReport?.errors ?? [];
      if (syncedItems.length > 0) parts.push(`已同步 ${syncedItems.length} 个会话`);
      if (skippedItems.length > 0) parts.push(`跳过 ${skippedItems.length} 个会话`);
      if (syncErrors.length > 0) parts.push(`会话同步失败 ${syncErrors.length} 个`);
      if (res.backup) parts.push(`备份: ${res.backup}`);
      // 专家团与工作记忆属于「切号时顺带同步的资产」：成功量并入同一条汇总，失败另开
      // error toast（分级不能丢）。只报数量，不列名单。
      const expertsReport = res.expertsMerge;
      if (expertsReport && expertsReport.written.length > 0) {
        const backedUp = expertsReport.written.some((item) => item.backup);
        parts.push(
          `专家团已同步到 ${expertsReport.written.length} 个账号（共 ${expertsReport.mergedCount} 个）${
            backedUp ? "，原有清单已备份" : ""
          }`,
        );
      }
      // 镜像切换：一次切号只报一条主结果，三种语义分开——
      // ① needsRecovery：有未完成写入，下次切号会先恢复；
      // ② errors 非空：部分步骤没做成（写入前就失败的居多，不产生待恢复记录）；
      // ③ 其余：整份完成。
      // 无论哪种，备份路径都先记下来：没有入口的"可还原"等于没有还原。
      const mirrorReport = res.mirror;
      if (mirrorReport) {
        const backup = mirrorReport.backup;
        const failed = mirrorReport.errors.length > 0;
        const unfinished = mirrorReport.needsRecovery;
        const title = unfinished
          ? `镜像未完成、需要恢复：已软删 ${mirrorReport.deletedSessions} 个、复制 ${mirrorReport.copiedSessions} 个`
          : failed
            ? `镜像部分完成：已软删 ${mirrorReport.deletedSessions} 个、复制 ${mirrorReport.copiedSessions} 个`
            : `整份镜像完成：软删 ${mirrorReport.deletedSessions} 个会话、复制 ${mirrorReport.copiedSessions} 个会话`;
        parts.push(title);
        if (backup) {
          rememberMirrorBackup(backup);
          setMirrorBackup(backup);
        }
        const restoreAction = backup
          ? { label: "还原", onClick: () => restoreMirrorBackup(backup) }
          : undefined;
        if (unfinished) {
          toast.error("镜像切换未全部完成，下次切号会先恢复", {
            description: `${backup ? `备份：${backup}；` : ""}已登记为可还原项，绝不按成功清理回收。`,
            action: restoreAction,
          });
        } else if (failed) {
          toast.warning("镜像部分未完成，备份已保留", {
            description: `${backup ? `备份：${backup}；` : ""}可以在「镜像切换」tab 里一键还原。`,
            action: restoreAction,
          });
        } else if (backup) {
          toast.success(title, {
            description: `覆盖前已先备份，明文路径：${backup}。如需撤销可点「还原」。`,
            action: restoreAction,
          });
        }
        for (const warning of mirrorReport.warnings) {
          toast.warning("镜像切换已落盘但持久化证明缺失", { description: warning });
        }
        if (failed) {
          toast.error("部分镜像切换未完成", {
            description: mirrorReport.errors.map((item) => `${item.sessionId}：${item.error}`).join("；"),
          });
        }
        if (mirrorReport.skipped.length > 0) {
          toast.warning("部分镜像切换已跳过", {
            description: mirrorReport.skipped.map((item) => `${item.sessionId}：${item.reason}`).join("；"),
          });
        }
      }
      // 删除同步：与镜像同形，先记备份路径再按结果分级提示。
      const deletedReport = res.deletedSync;
      if (deletedReport) {
        const backup = deletedReport.backup;
        if (backup) {
          rememberMirrorBackup(backup);
          setMirrorBackup(backup);
        }
        const restoreAction = backup
          ? { label: "还原", onClick: () => restoreMirrorBackup(backup) }
          : undefined;
        if (deletedReport.errors.length > 0 || deletedReport.needsRecovery) {
          toast.warning("同步删除部分未完成，备份已保留", {
            description: `${backup ? `备份：${backup}；` : ""}可在「关联会话」tab 一键还原。`,
            action: restoreAction,
          });
        } else {
          toast.success(`已同步删除 ${deletedReport.deletedSessions} 个会话`, {
            description: backup
              ? `覆盖前已先备份：${backup}。如需撤销可点「还原」。`
              : "没有需要删除的会话。",
            action: restoreAction,
          });
        }
      }
      // 成功清理：临时备份已回收，不再展示可还原路径；待清理项单独提示，不写进成功文案。
      toast.success(`已切换至「${nickname}」`, {
        description: parts.length ? parts.join("；") : `${variantAppName(accountVariant(account))} 已重启为目标账号。`,
      });
      // 复制失败或被后端跳过时必须显式提示，不能静默当成成功。
      if (copyReport?.error) {
        toast.error("会话复制未执行", { description: copyReport.error });
      } else if (copyErrors.length > 0) {
        // 失败提示必须带会话标识（能取标题时优先标题），不是只有原因。
        const sessionLabel = (id: string) =>
          sessions.find((item) => item.id === id)?.title || id;
        toast.error("部分会话未复制", {
          description: copyErrors
            .map((item) => `${sessionLabel(item.id)}：${item.error}`)
            .join("；"),
        });
      } else if (requestedCopy && !copyReport) {
        toast.warning("会话未复制", {
          description: "没有收到复制结果：当前版本可能不支持复制会话；账号已切换，但会话没有复制。",
        });
      }
      // 同步结果：成功、跳过、失败与恢复信息都要能看到，不能只显示成功数。
      const groupLabel = (groupId: string) =>
        syncGroups.find((group) => group.groupId === groupId)?.title || groupId;
      // 失败与跳过合并成一条提示：两者可能同时出现，不能只报其中一类。
      const syncIssues = [
        ...syncErrors.map(
          (item) => `${item.groupId ? groupLabel(item.groupId) : "全部会话"}：${item.error}`,
        ),
        // 预览过期/前置条件变化一律跳过并给出原因，绝不显示成已同步。
        ...skippedItems.map((item) => `${groupLabel(item.groupId)}：${item.message}`),
      ];
      if (syncIssues.length > 0) {
        if (syncErrors.length > 0) {
          toast.error("部分会话没有同步（失败或已跳过）", { description: syncIssues.join("；") });
        } else {
          toast.warning("有会话没有同步（已跳过）", { description: syncIssues.join("；") });
        }
      } else if (requestedSync && !syncReport) {
        toast.warning("会话同步未执行", {
          description: "没有收到同步结果：当前版本可能不支持同步会话，或本次切换没有重启应用。",
        });
      }
      if (syncReport?.needsRecovery) {
        toast.error("有会话同步没有完成", {
          description: "已保留操作记录与备份，下次切换会先恢复；恢复完成前不会再改动目标账号的内容。",
        });
      }
      // 未完成的会话写入：可重试项只提示，阻断项由后端直接返回错误。
      const recoveryIssues = res.sessionRecovery?.needsRecovery ?? [];
      if (recoveryIssues.length > 0) {
        toast.error("有会话操作没有完成", {
          description: recoveryIssues.map((issue) => issue.reason).join("；"),
        });
      }
      // 附属目录（子 agent transcript / 工具结果）缺失时，客户端把面板渲染成空白而不报错。
      // 结果挂在单个会话项上（复制 / 同步），恢复的走 sidecarWarnings 列表；三处合并成一条提示。
      const sidecarLabel = (id: string) => sessions.find((item) => item.id === id)?.title || id;
      const sidecarIssues: string[] = [];
      const collectSidecar = (id: string, outcome?: SidecarOutcome) => {
        if (!outcome) return;
        const failed = outcome.errors?.length ?? 0;
        if (failed > 0) sidecarIssues.push(`${sidecarLabel(id)}：${failed} 个附属文件未写入`);
        const ignored = outcome.ignoredEntries ?? 0;
        if (ignored > 0) sidecarIssues.push(`${sidecarLabel(id)}：${ignored} 个条目不在同步范围`);
      };
      for (const item of copyReport?.copied ?? []) collectSidecar(item.id, item.sidecar);
      for (const item of syncReport?.synced ?? []) {
        collectSidecar(item.targetSessionId, item.sidecar);
        if (item.sidecarError) {
          sidecarIssues.push(`${sidecarLabel(item.targetSessionId)}：${item.sidecarError}`);
        }
      }
      for (const warning of res.sessionRecovery?.sidecarWarnings ?? []) {
        sidecarIssues.push(`${sidecarLabel(warning.sessionId)}：${warning.reason}`);
      }
      if (sidecarIssues.length > 0) {
        toast.warning("子 agent 内容可能显示不全", {
          description: `${sidecarIssues.join("；")}。主会话不受影响，缺失的只是子 agent 面板内容。`,
        });
      }
      // 记忆结果：合并 / 覆盖分级提示全部由纯决策函数给出，按本次请求的 mode 出文案。
      for (const item of memoryToasts(memoryMode, res.memoryMerge)) {
        MEMORY_TOAST_EMIT[item.level](item.title, { description: item.description });
      }
      // 专家团同步：失败必须独立提示；成功量已并入上面的汇总，不重复弹第二条成功 toast。
      if (expertsReport) {
        if (expertsReport.errors.length > 0) {
          toast.error("专家团没有全部同步", {
            description: expertsReport.errors
              .map((item) => `${item.label}：${item.error}`)
              .join("；"),
          });
        } else if (expertsReport.written.length === 0 && expertsReport.note) {
          toast.info("专家团未同步", { description: expertsReport.note });
        } else if (expertsReport.written.length === 0 && expertsReport.unchanged.length > 0) {
          toast.info("专家团无需更新", { description: "所有账号的专家团清单已经一致。" });
        }
      } else if (mergeExperts) {
        toast.warning("专家团未同步", {
          description: "没有收到同步结果：当前版本可能不支持该功能，账号已切换，但专家团没有改动。",
        });
      }
      // 人设同步：与记忆合并是两条独立通道，必须单独提示——
      // 「记忆合并成功但人设被跳过」正是上一版最容易让人误判的地方。
      const personaReport = res.personaSync;
      if (personaReport) {
        if (personaReport.errors.length > 0) {
          toast.error("人设没有全部同步", {
            description: personaReport.errors
              .map((item) => `${item.label}${item.file ? ` / ${item.file}` : ""}：${item.error}`)
              .join("；"),
          });
        }
        if (personaReport.written.length > 0) {
          const modeLabel =
            personaReport.mode === "overwrite" ? "以最完整一份为准覆盖" : "只替换默认模板";
          toast.success(`人设已同步到 ${personaReport.written.length} 处`, {
            description: `${modeLabel}；被覆盖的原文已备份为 <文件名>.bak-时间戳`,
          });
        } else if (personaReport.errors.length === 0 && personaReport.unchanged.length > 0) {
          toast.info("人设无需更新", {
            description:
              "各账号已有自定义人设（安全模式不覆盖），或已是最完整的那一份。需要强制统一请选「以最完整一份为准」。",
          });
        }
      } else if (syncPersona) {
        toast.warning("人设未同步", {
          description: "没有收到同步结果：当前版本可能不支持该功能，账号已切换，但人设没有改动。",
        });
      }
      // 临时备份残留：待清理说明重试入口；待恢复说明必须保留材料并给出下一步。
      const temporaryFiles = dedupeTemporaryFiles([
        ...(copyReport?.temporaryFiles ?? []),
        ...(syncReport?.temporaryFiles ?? []),
        ...(res.sessionRecovery?.temporaryFiles ?? []),
      ]);
      const pendingFiles = temporaryFiles.filter((item) => item.state === "cleanupPending");
      const recoveryFiles = temporaryFiles.filter((item) => item.state === "needsRecovery");
      // 报告级 temporaryFiles 是权威来源；仅当旧后端没给该字段时，才回退到成功项上的 pending。
      const pendingCleanupReasons: string[] = [];
      if (temporaryFiles.length === 0) {
        for (const item of copyReport?.copied ?? []) {
          if (item.cleanupState === "pending") {
            pendingCleanupReasons.push(item.cleanupError ?? "临时文件待清理");
          }
        }
        for (const item of syncedItems) {
          if (item.cleanupState === "pending") {
            pendingCleanupReasons.push(item.cleanupError ?? "临时文件待清理");
          }
        }
      }
      if (pendingCleanupReasons.length > 0 || pendingFiles.length > 0) {
        const details = [...pendingCleanupReasons, ...pendingFiles.map(describeTemporaryFile)];
        toast.warning("会话已完成，部分临时文件待清理", {
          description: `${details.join("；")}。下次切号时会自动重试清理。`,
        });
      }
      if (recoveryFiles.length > 0) {
        toast.error("有临时备份需要确认", {
          description: `${recoveryFiles.map(describeTemporaryFile).join("；")}。已保留现场，请按提示处理后重试。`,
        });
      }
      onOpenChange(false);
      onDone?.();
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
      setProgress("");
    }
  }

  /** 打开系统设置授权面板（默认完全磁盘访问），供小白一键跳转。 */
  async function openPermissionSettings() {
    try {
      await api.openPermissionSettings("all_files");
    } catch (e) {
      // 打开失败时退化为提示
      setError(api.asError(e));
    }
  }

  /** 权限自检：确认完全磁盘访问是否生效（探针按目标账号档位）。 */
  const [permCheck, setPermCheck] = useState<string | null>(null);
  async function runPermissionCheck() {
    setPermCheck("检测中…");
    try {
      const res = await api.checkAuthPermission(accountVariant(account));
      setPermCheck(res.ok ? `✓ ${res.message}` : `✗ ${res.error}（${res.dir}）`);
    } catch (e) {
      setPermCheck(`✗ ${api.asError(e)}`);
    }
  }

  // 出现「无权限」错误时，自动每 2s 轮询一次授权状态；用户拖入 app 授权成功后自动恢复
  useEffect(() => {
    if (!open || !error.includes("无权限")) return;
    let cancelled = false;
    let timer: number | undefined;
    const check = async () => {
      try {
        const res = await api.checkAuthPermission(accountVariant(account));
        if (res.ok) {
          if (!cancelled) {
            setPermCheck("✓ 授权成功，可以重新切换了");
            setError("");
          }
          return;
        }
      } catch {
        /* 忽略中间态 */
      }
      if (!cancelled) timer = window.setTimeout(check, 2000);
    };
    check();
    return () => {
      cancelled = true;
      if (timer) window.clearTimeout(timer);
    };
  }, [error, open, variant]);

  const copyCount = selected.size;
  const syncCount = syncSelections.length;
  /** 覆盖项数量：底部摘要据此提示风险。 */
  const overwriteCount = syncSelections.filter((item) => item.mode === "overwrite").length;
  const linksAvailable = linksMeta?.available ?? true;
  /**
   * 切号时顺带同步的资产（记忆 / 人设 / 专家团）：摘要与勾选说明共用一处措辞，
   * 避免「只勾资产」时摘要还写「仅切换账号」。覆盖模式也计入徽标（memoryAssetLabel 非空）。
   */
  const assetParts = [
    memoryAssetLabel(memoryMode),
    syncPersona ? "人设" : null,
    mergeExperts ? "专家团" : null,
  ].filter((part): part is string => part !== null);
  const assetLabel = assetParts.length > 0 ? assetParts.join("、") : null;
  /** 「附加同步」tab 徽标：有多少项会真正执行。 */
  const syncAssetCount = assetParts.length;
  const sessionSummary =
    copyCount > 0 && syncCount > 0
      ? `将复制 ${copyCount} 个、同步 ${syncCount} 个关联会话`
      : copyCount > 0
        ? `将复制 ${copyCount} 个会话`
        : syncCount > 0
          ? `将同步 ${syncCount} 个关联会话`
          : "";
  const summaryMain = sessionSummary
    ? assetLabel
      ? `${sessionSummary}，并同步${assetLabel}`
      : sessionSummary
    : assetLabel
      ? `切换账号并同步${assetLabel}`
      : "本次仅切换账号";
  const summarySub =
    overwriteCount > 0
      ? `其中 ${overwriteCount} 个会替换目标账号的完整内容`
      : copyCount === 0 && syncCount === 0
        ? assetLabel
          ? `不复制、不同步会话，但会把${assetLabel}同步到所有账号`
          : "未选择复制或同步会话"
        : copyCount === 0
          ? "未选择复制会话"
          : syncCount === 0
            ? "未选择同步会话"
            : null;
  /** 覆盖模式在底部摘要也醒目提示（切到别的 tab 时仍能看到），不只在区块内。 */
  const memoryWarning = memorySummaryWarning(memoryMode);
  const memoryBlocked = !canSubmitMemory(memoryMode, effectiveSourceUid);
  const needsPermission = error.includes("无权限");
  const sessionsEmpty = !loadingSessions && sessions.length === 0;
  const copyHint = loadingSessions
    ? "正在加载会话…"
    : error && sessionsEmpty
      ? "无法加载会话列表，暂不能复制"
      : sessionsEmpty
        ? currentUid
          ? "当前账号暂无会话，无法复制"
          : "未检测到当前登录账号，无法列出会话"
        : "把当前账号勾选的会话复制给目标账号（复制后归属目标账号）；已经复制过的不会重复复制；已置顶的会话会同步置顶到目标账号";

  // 关联 tab 不可用（国际版能力判定不通过）时回落到复制会话，避免停在空 tab；
  // 同时清掉失效勾选，避免摘要 / 提交残留上一轮或不可用状态下的 syncSelections
  // （后端已兜住不会产生写入，这里保证前端摘要与提交口径一致）。
  useEffect(() => {
    if (linksAvailable) return;
    setSyncSelections([]);
    setSyncGroups([]);
    setTab((prev) => (prev === "links" ? "copy" : prev));
  }, [linksAvailable]);

  /** 第三个 tab：跨账号同步（记忆 / 人设 / 专家团）。原来挤在底部，收纳到这里。 */
  const syncTabContent = (
    <div className="space-y-2.5">
      {/* 镜像启用时这里是全灰的，原因必须写在当前 tab 内，不能只在镜像 tab 里。 */}
      {mirrorBlocks.memory && (
        <Alert variant="default" className="min-w-0">
          <CircleAlert />
          <AlertTitle>镜像切换已启用</AlertTitle>
          <AlertDescription className="min-w-0 break-words">
            附加同步（工作记忆 / 人设 / 专家团）与一键全选已暂时关闭，避免和整份镜像双重写入。
            要逐项同步，请先到「镜像切换」tab 取消启用。
          </AlertDescription>
        </Alert>
      )}
      <p className="text-xs text-muted-foreground">
        这些都是本地文件操作，与会话无关；选定后会在写入登录态之前完成，覆盖前都会先备份。
      </p>

      <div className="space-y-2 rounded-md border border-border px-3 py-2.5">
        <div className="text-sm font-medium">工作记忆</div>
        <RadioGroup
          value={memoryMode}
          disabled={busy || mirrorBlocks.memory}
          onValueChange={(value) => setMemoryMode(value as MemoryMode)}
          className="gap-2"
        >
          <label className="flex cursor-pointer items-start gap-2">
            <RadioGroupItem value="off" className="mt-0.5" />
            <div className="min-w-0 space-y-0.5">
              <div className="text-xs font-medium">不同步</div>
              <div className="text-xs text-muted-foreground">这次只切换账号，不动任何账号的工作记忆。</div>
            </div>
          </label>
          <label className="flex cursor-pointer items-start gap-2">
            <RadioGroupItem value="merge" className="mt-0.5" />
            <div className="min-w-0 space-y-0.5">
              <div className="text-xs font-medium">合并成一份（只增不减）</div>
              <div className="text-xs text-muted-foreground">
                把各账号的工作记忆合到一起，再写回每个账号；原有内容不会丢，写入前先备份。
              </div>
            </div>
          </label>
          <label className="flex cursor-pointer items-start gap-2">
            <RadioGroupItem value="overwrite" className="mt-0.5" />
            <div className="min-w-0 space-y-0.5">
              <div className="text-xs font-medium">用我这份覆盖所有账号</div>
              <div className="text-xs text-muted-foreground">
                把指定账号的工作记忆复制给其他所有账号；其他账号原来的记忆会被替换。
              </div>
            </div>
          </label>
        </RadioGroup>

        {showsSourcePicker(memoryMode) && (
          <div className="ml-6 space-y-2 border-l border-border pl-3">
            <Alert variant="destructive" className="min-w-0">
              <CircleAlert />
              <AlertTitle>这是覆盖操作</AlertTitle>
              <AlertDescription className="min-w-0 break-words">
                「{sourceLabel}」的工作记忆会替换其他账号原有的记忆。覆盖前每个账号都会自动备份为
                MEMORY.md.bak-时间戳，随时可以还原；只改工作记忆，不动人设、专家团和云端记忆。
              </AlertDescription>
            </Alert>

            <div className="space-y-1.5">
              <label className="text-xs font-medium" htmlFor="memory-source-uid">母本账号</label>
              <Select
                value={effectiveSourceUid || undefined}
                onValueChange={setMemorySourceUid}
                disabled={busy || memorySources.options.length === 0}
              >
                <SelectTrigger
                  id="memory-source-uid"
                  size="sm"
                  className="w-full min-w-0 bg-background text-xs"
                  aria-label="母本账号"
                >
                  <SelectValue
                    placeholder={
                      memorySources.options.length === 0
                        ? "账号库里没有可用作母本的账号"
                        : "选择母本账号"
                    }
                  />
                </SelectTrigger>
                <SelectContent>
                  {memorySources.options.map((option) => (
                    <SelectItem key={option.uid} value={option.uid} className="text-xs">
                      {option.label}
                      {option.isCurrent ? "（当前登录）" : ""}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <p className="text-xs text-muted-foreground">
                默认选当前登录账号，可以在这里改成别的账号。
              </p>
              {sourceNeedsConfirm && memorySources.options.length > 0 && (
                <p className="text-xs font-medium text-amber-700 dark:text-amber-400">
                  请确认母本：没能识别当前登录账号，已默认选列表第一项，请核对后再覆盖。
                </p>
              )}
              {memorySources.options.length === 0 && (
                <p className="text-xs font-medium text-amber-700 dark:text-amber-400">
                  覆盖需要至少一个记录了 uid 的账号作为母本，当前无法提交覆盖。
                </p>
              )}
            </div>
          </div>
        )}
      </div>

      <div className="space-y-2 rounded-md border border-border px-3 py-2.5">
        <label className="flex cursor-pointer items-start gap-2">
          <Checkbox
            checked={syncPersona}
            disabled={busy || mirrorBlocks.persona}
            onCheckedChange={(value) => setSyncPersona(value === true)}
            className="mt-0.5"
          />
          <div className="min-w-0 space-y-1">
            <div className="text-sm font-medium">同步人设</div>
            <div className="text-xs text-muted-foreground">
              把你设置的人设同步到其他账号（对应 SOUL / IDENTITY / USER 三个文件）。
            </div>
          </div>
        </label>
        {syncPersona && (
          <RadioGroup
            value={personaMode}
            disabled={busy || mirrorBlocks.persona}
            onValueChange={(value) => setPersonaMode(value as "safe" | "overwrite")}
            className="ml-6 gap-2 border-l border-border pl-3"
          >
            <label className="flex cursor-pointer items-start gap-2">
              <RadioGroupItem value="safe" className="mt-0.5" />
              <div className="min-w-0 space-y-0.5">
                <div className="text-xs font-medium">只补空白账号</div>
                <div className="text-xs text-muted-foreground">
                  哪个账号的人设还是系统自带的空白模板，就写入你这份；你自己改过的人设不动。
                </div>
              </div>
            </label>
            <label className="flex cursor-pointer items-start gap-2">
              <RadioGroupItem value="overwrite" className="mt-0.5" />
              <div className="min-w-0 space-y-0.5">
                <div className="text-xs font-medium">全部换成同一份</div>
                <div className="text-xs text-muted-foreground">
                  所有账号统一成内容最全的那份；其他账号原有内容会先备份再覆盖。
                </div>
              </div>
            </label>
          </RadioGroup>
        )}
      </div>

      <label className="flex cursor-pointer items-start gap-2 rounded-md border border-border px-3 py-2.5">
        <Checkbox
          checked={mergeExperts}
          disabled={busy || mirrorBlocks.experts}
          onCheckedChange={(value) => setMergeExperts(value === true)}
          className="mt-0.5"
        />
        <div className="min-w-0 space-y-1">
          <div className="text-sm font-medium">同步专家团</div>
          <div className="text-xs text-muted-foreground">
            把各账号添加过的专家团合并成一份，写到每个账号；只新增不删除，原有清单先备份。
          </div>
        </div>
      </label>

    </div>
  );

  // 镜像切换独立为第 4 个 tab（排在「附加同步」右侧，用户明确要求）。
  const mirrorUnmerged = mirrorPreview?.unmerged ?? [];
  const mirrorTabContent = (
    <div className="space-y-2">
      {/* 镜像切换：显式独立通道。默认全关，绝不并入一键全选 / 工作记忆 / 人设 / 归档。 */}
      <div className="space-y-2 rounded-md border border-destructive/40 px-3 py-2.5">
        <div className="flex items-center justify-between gap-2">
          <div className="text-sm font-medium">镜像切换（beta）</div>
          <span className="text-[11px] text-muted-foreground">默认关闭 · 独立通道</span>
        </div>
        <p className="text-xs text-muted-foreground">
          以当前登录账号为母本，把目标账号的会话集合整份对齐：目标多出的会话软删（可还原），母本多出的会话复制，MEMORY.md 与专家团清单整份覆盖。默认关闭，须先预览逐条清单、再二次确认；执行前逐条复核并先备份，成功后可一键还原。人设与归档状态不受影响；专家团清单会跟镜像一起整份覆盖，还原时一并回滚。
        </p>

        {/* 启用勾选：镜像与附加同步互斥——勾上后下面三个开关与一键全选一律置灰。 */}
        <label className="flex cursor-pointer items-start gap-2">
          <Checkbox
            checked={mirror.enabled}
            disabled={busy}
            onCheckedChange={(value) => toggleMirrorEnabled(value === true)}
            className="mt-0.5"
          />
          <div className="min-w-0 space-y-0.5">
            <div className="text-xs font-medium">启用镜像切换</div>
            <div className="text-xs text-muted-foreground">
              启用后，附加同步（工作记忆 / 人设 / 专家团）与一键全选会置灰，避免两套写入互相打架。首次启用要确认一次风险，之后本机不再重复询问。
            </div>
          </div>
        </label>

        {/* 母本恒为当前登录账号（后端同源 current_user_uid_at）：只读展示，用户不可手选。 */}
        <div className="space-y-1.5">
          <label className="text-xs font-medium" htmlFor="mirror-mother-uid">母本账号（当前登录账号）</label>
          <Select value={mirror.motherUid ?? ""} disabled>
            <SelectTrigger
              id="mirror-mother-uid"
              size="sm"
              className="w-full min-w-0 bg-background text-xs"
              aria-label="镜像切换母本账号（当前登录账号，只读）"
            >
              <SelectValue
                placeholder={loginUid ? "当前登录账号" : "未识别到当前登录账号"}
              />
            </SelectTrigger>
            <SelectContent>
              {memorySources.options.map((option) => (
                <SelectItem key={option.uid} value={option.uid} className="text-xs">
                  {option.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        <Button
          variant="outline"
          size="sm"
          disabled={busy || !mirror.enabled || !mirror.motherUid || mirrorPreviewBusy}
          onClick={() => void loadMirrorPreview()}
        >
          {mirrorPreviewBusy ? <Loader2 className="animate-spin" /> : <CircleAlert />}
          {mirrorPreviewBusy ? "正在核对整份镜像…" : "预览镜像将改变的会话"}
        </Button>
        {/* 预览按钮为什么不能点：逐条说清，不留"灰着不解释"的状态。 */}
        {!mirrorPreviewBusy && !mirror.enabled && (
          <p className="text-xs text-muted-foreground">先勾选上面的「启用镜像切换」，才能预览。</p>
        )}
        {!mirrorPreviewBusy && mirror.enabled && !mirror.motherUid && (
          <p className="text-xs text-muted-foreground">
            没认到当前登录账号，请先登录母本账号，再重新打开切号窗口。
          </p>
        )}

        {mirrorError && (
          <Alert variant="destructive" className="min-w-0">
            <AlertDescription className="min-w-0 break-all">{mirrorError}</AlertDescription>
          </Alert>
        )}

        {mirrorPreview && (
          <div className="space-y-2 rounded-md border bg-muted/30 px-2.5 py-2">
            <div className="text-xs font-medium">
              {mirrorPreview.sessionCount > 0
                ? `母本「${mirrorMotherLabel}」为准，将软删 ${mirrorPreview.sessionCount} 个会话、复制 ${mirrorPreview.copiedCount} 个会话`
                : `母本「${mirrorMotherLabel}」已包含目标账号现有的全部会话；本次将复制 ${mirrorPreview.copiedCount} 个会话，并按需覆盖 MEMORY.md`}
            </div>
            {/* 逐条待软删清单：不折叠，含私有会话；母本会话不进此清单。 */}
            {mirrorPreview.targets.length === 0 ? (
              <p className="text-xs text-muted-foreground">
                没有要软删的会话——目标账号的会话母本都有，不会删除任何东西。
              </p>
            ) : (
              <ul className="max-h-40 space-y-1 overflow-y-auto text-xs">
                {mirrorPreview.targets.map((target) => (
                  <li
                    key={`mirror:${target.sessionId}`}
                    className="flex items-start justify-between gap-2"
                  >
                    <span className="min-w-0 break-all">{target.title || target.sessionId}</span>
                    <span className="shrink-0 text-muted-foreground tabular-nums">
                      {target.groupId ? "关联会话 · 软删" : "私有会话 · 软删"}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            {/* 组层面的合并缺口：只读观测，有才显示；不参与任何提交判据。 */}
            {mirrorUnmerged.length > 0 && (
              <details className="rounded-md border border-dashed px-2.5 py-2">
                <summary className="cursor-pointer text-xs font-medium">
                  {`本次将合并的对话（${mirrorUnmerged.length}）`}
                </summary>
                <ul className="mt-1.5 max-h-40 space-y-1 overflow-y-auto text-xs">
                  {mirrorUnmerged.map((item) => (
                    <li
                      key={`mirror-unmerged:${item.groupId}:${item.motherSessionId}`}
                      className="flex items-start justify-between gap-2"
                    >
                      <span className="min-w-0 break-all">
                        {item.title || item.motherSessionId}
                      </span>
                      <span className="shrink-0 text-muted-foreground tabular-nums">
                        {item.groupId ? `组 ${item.groupId.slice(0, 8)}` : "未建组"}
                      </span>
                    </li>
                  ))}
                </ul>
              </details>
            )}
            {/* 专家团清单：与 MEMORY.md 同形的第三步。跳过不是错误，因此只显示原因、不禁用执行。 */}
            {mirrorPreview.expertsPending && (
              <p className="text-xs">
                {`将把专家团清单整份覆盖为母本「${mirrorMotherLabel}」的 ${mirrorPreview.expertsNames.length} 个：${
                  mirrorPreview.expertsNames.length > 0
                    ? mirrorPreview.expertsNames.join("、")
                    : "（空清单）"
                }；目标账号独有的专家团会被移除（成功后可在还原时一并回滚）。`}
              </p>
            )}
            {mirrorPreview.expertsNote !== null && (
              <p className="text-[11px] text-muted-foreground">
                {`专家团清单本次不覆盖：${mirrorPreview.expertsNote}`}
              </p>
            )}
            <label className="flex cursor-pointer items-start gap-2">
              <Checkbox
                checked={mirror.acknowledged}
                disabled={busy}
                onCheckedChange={(value) =>
                  setMirror((prev) => ({
                    ...prev,
                    acknowledged: value === true,
                    confirmed: false,
                  }))
                }
                className="mt-0.5"
              />
              <span className="text-xs">我已知会先备份，覆盖前保留原文，成功后可一键还原</span>
            </label>
            <Button
              variant="destructive"
              size="sm"
              disabled={busy || mirrorSubmitBlocker !== null}
              onClick={() => setMirrorConfirmOpen(true)}
            >
              二次确认镜像切换
            </Button>
            {mirror.confirmed && (
              <p className="text-xs font-medium text-destructive">
                {`已确认：本次切号将整份镜像，软删 ${mirrorPreview.sessionCount} 个会话、复制 ${mirrorPreview.copiedCount} 个会话。`}
              </p>
            )}
          </div>
        )}

        {/* 启用后始终说明还缺什么（未预览时也要说），不留"灰着不解释"的状态。 */}
        {mirror.enabled && mirrorSubmitBlocker !== null && (
          <p className="text-xs text-muted-foreground">{mirrorSubmitBlocker}</p>
        )}

        {/* 常驻还原入口：明文备份绝对路径，不建列表、不伪装成成功。 */}
        {mirrorBackup && (
          <div className="space-y-1 rounded-md border border-amber-500/50 px-2.5 py-2">
            <div className="text-xs font-medium">上次镜像切换的备份（可还原）</div>
            <p className="break-all text-[11px] text-muted-foreground">{mirrorBackup}</p>
            <p className="text-[11px] text-muted-foreground">
              会话、MEMORY.md 与专家团清单都会回滚到镜像前
            </p>
            <Button
              variant="outline"
              size="sm"
              disabled={busy}
              onClick={() => restoreMirrorBackup(mirrorBackup)}
            >
              还原这次镜像
            </Button>
          </div>
        )}
      </div>

      {/* 二次确认弹层：复述母本名 + 影响数，确认才置 confirmed（doSwitch 据此产出 mirrorPlan）。 */}
      <AlertDialog open={mirrorConfirmOpen} onOpenChange={setMirrorConfirmOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>确认镜像切换？</AlertDialogTitle>
            <AlertDialogDescription>
              {`母本「${mirrorMotherLabel}」为准：软删 ${mirrorPreview?.sessionCount ?? 0} 个会话、复制 ${mirrorPreview?.copiedCount ?? 0} 个会话，并把目标账号的 MEMORY.md 与专家团清单整份覆盖为母本的（覆盖前自动备份）。执行前每项仍会重新校验预览凭据与现场摘要，内容已变化会跳过而不是软删；成功后可一键还原，还原会把 MEMORY.md 与专家团清单一起回滚到镜像前。人设与归档状态不受影响。`}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={busy}>取消</AlertDialogCancel>
            <AlertDialogAction disabled={busy} onClick={confirmMirror}>
              确认镜像
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      {/* 首次启用镜像的风险确认：确认一次后本机记住，之后不再重复询问。 */}
      <AlertDialog open={mirrorRiskOpen} onOpenChange={setMirrorRiskOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>确认启用镜像切换？</AlertDialogTitle>
            <AlertDialogDescription>
              镜像切换以当前登录账号为母本，把目标账号整份对齐：目标多出的会话会被软删（可还原），母本多出的会话会被复制，工作记忆整份覆盖。启用后附加同步与一键全选会置灰。确认一次之后本机不再重复询问，但每次真正执行仍要预览清单并二次确认。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel disabled={busy}>取消</AlertDialogCancel>
            <AlertDialogAction disabled={busy} onClick={confirmMirrorRisk}>
              我已知风险，启用
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );


  // 「复制会话」tab 内容：勾选即意图，提交结果由底部摘要兜底确认。
  const copyTabContent = (
    <>
      {loadingSessions ? (
        // 两行高度对齐真实提示文案（有会话时折成两行）。
        <div className="px-1">
          <Skeleton className="h-4 w-full" />
          <Skeleton className="h-4 w-3/4" />
        </div>
      ) : (
        <p
          className={
            sessionsEmpty
              ? "px-1 text-xs text-amber-700 dark:text-amber-400"
              : "px-1 text-xs text-muted-foreground"
          }
        >
          {copyHint}
        </p>
      )}
      {loadingSessions ? (
        <div
          className={`flex items-center justify-center gap-2 text-sm text-muted-foreground ${LIST_MIN_H}`}
        >
          <Loader2 className="animate-spin" /> 加载会话…
        </div>
      ) : sessions.length === 0 ? (
        <p
          className={`flex items-center justify-center px-3 text-center text-sm text-muted-foreground ${LIST_MIN_H}`}
        >
          {currentUid ? "当前账号暂无会话" : "未检测到当前登录账号，无法列出会话"}
        </p>
      ) : (
        <SessionTreeList
          sessions={sessions}
          selected={selected}
          expanded={expanded}
          onToggleSession={toggleSession}
          onToggleGroup={toggleFolder}
          onToggleExpanded={toggleExpanded}
          className={`max-h-[min(22rem,45vh)] overflow-y-auto pr-1 ${LIST_MIN_H}`}
        />
      )}
    </>
  );

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        showCloseButton={!busy}
        onEscapeKeyDown={(event) => {
          if (shouldBlockDismiss(busy)) event.preventDefault();
        }}
        onInteractOutside={(event) => {
          if (shouldBlockDismiss(busy)) event.preventDefault();
        }}
        className="flex max-h-[min(90vh,calc(100vh-2rem))] min-w-0 flex-col gap-0 overflow-hidden p-0"
      >
        <DialogAutoHeight>
        <div className="flex max-h-[calc(min(90vh,100vh-2rem)-2px)] min-w-0 flex-col gap-4 p-6">
        <DialogHeader className="shrink-0">
          <DialogTitle>切换到「{account ? displayName(account) : "该账号"}」</DialogTitle>
          <DialogDescription>切换时将重启 {variantAppName(accountVariant(account))}。</DialogDescription>
        </DialogHeader>

        {busy && (
          <div className="absolute inset-0 z-50 flex flex-col items-center justify-center gap-3 rounded-lg bg-background/85 backdrop-blur-sm">
            <Loader2 className="size-8 animate-spin text-primary" />
            <p className="text-sm font-medium">{progress || "正在切换账号…"}</p>
            <p className="max-w-xs text-center text-xs text-muted-foreground">
              正在处理中，请勿关闭窗口
            </p>
          </div>
        )}

        {/* min-h-0：矮窗口下压缩本区而不是裁切页脚。列表按自身 max-h 滚动；矮窗口或权限大卡时 tab 内容区可滚，避免裁切。仅当常驻提示超过剩余空间时本层滚动。 */}
        <div className="flex min-h-0 flex-col gap-3 overflow-x-hidden overflow-y-auto">
          {/* 常驻提示区：切换错误 / 权限 / 关联检查失败不随 tab 切换隐藏。 */}
          {error && (
            <Alert variant={needsPermission ? "warning" : "destructive"} className="min-w-0 shrink-0 break-all">
              <AlertDescription className="min-w-0 break-all">
                <div className="min-w-0 break-all">{error}</div>
                {needsPermission && (
                  <div className="mt-2 space-y-2">
                    <div className="rounded-md border bg-muted/60 p-3 text-xs text-muted-foreground">
                      <p className="mb-1 font-medium text-foreground">如何授权（只需 3 步）：</p>
                      <ol className="list-decimal space-y-1 pl-4">
                        <li>点击下方「打开完全磁盘访问」</li>
                        <li>
                          把 <b>workbuddy-switch.app</b> 从 Finder 拖进面板列表（即使没提示框也直接拖），
                          打开它的开关
                        </li>
                        <li>授权后这里会自动检测到，无需其他操作</li>
                      </ol>
                    </div>
                    <div className="flex flex-wrap gap-2">
                      <Button variant="outline" size="sm" onClick={openPermissionSettings}>
                        <ExternalLink />
                        打开完全磁盘访问
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => void api.revealAppInFinder()}
                      >
                        在 Finder 中显示
                      </Button>
                      <Button variant="secondary" size="sm" onClick={runPermissionCheck}>
                        立即检测
                      </Button>
                    </div>
                  </div>
                )}
                {permCheck && <div className="mt-2 text-xs">{permCheck}</div>}
              </AlertDescription>
            </Alert>
          )}
          {linksMeta?.error && (
            <Alert variant="destructive" className="min-w-0 shrink-0">
              <CircleAlert />
              <AlertTitle>无法检查会话</AlertTitle>
              <AlertDescription className="min-w-0 break-all">{linksMeta.error}</AlertDescription>
            </Alert>
          )}
          {linksMeta?.storeStatus === "unavailable" && (
            <Alert variant="warning" className="min-w-0 shrink-0">
              <CircleAlert />
              <AlertTitle>同步记录不可用</AlertTitle>
              <AlertDescription className="min-w-0 break-all">
                {`${linksMeta.storeError || "原因未知"}；本次不会同步任何会话，请处理后重试。`}
              </AlertDescription>
            </Alert>
          )}

          <Tabs
            value={tab}
            onValueChange={(value) => setTab(value as "links" | "copy" | "sync" | "mirror")}
            className="flex min-h-0 flex-col gap-3 overflow-hidden"
          >
            <TabsList className="h-auto w-full shrink-0 justify-start gap-3 overflow-x-auto rounded-none border-b border-border bg-transparent p-0 sm:gap-5">
              {linksAvailable && (
                <TabsTrigger value="links" className={TAB_TRIGGER_CLASS}>
                  关联会话
                  {tabCount(linksMeta?.groupCount ?? 0)}
                </TabsTrigger>
              )}
              <TabsTrigger value="copy" className={TAB_TRIGGER_CLASS}>
                复制会话
                {tabCount(copyCount)}
              </TabsTrigger>
              <TabsTrigger value="sync" className={TAB_TRIGGER_CLASS}>
                附加同步
                {tabCount(syncAssetCount)}
              </TabsTrigger>
              {/* 镜像切换独立成 tab，位置固定在「附加同步」右侧（用户明确要求）。 */}
              <TabsTrigger value="mirror" className={TAB_TRIGGER_CLASS}>
                镜像切换
                <span className="text-[10px] text-muted-foreground">beta</span>
              </TabsTrigger>
            </TabsList>
            {/* forceMount：切换 tab 不得卸载另一侧，否则关联勾选会被预览重拉重置。 */}
            {linksAvailable && (
              <TabsContent
                value="links"
                forceMount
                className="min-h-0 overflow-y-auto data-[state=inactive]:hidden data-[state=active]:animate-in data-[state=active]:fade-in-0 data-[state=active]:duration-150"
              >
                <div className="mb-2 space-y-2 rounded-md border border-amber-500/40 px-3 py-2.5">
                  <div className="text-sm font-medium">同步其他账号已删除的会话</div>
                  <p className="text-xs text-muted-foreground">
                    某个账号删掉的会话，其他账号仍留着的副本也一起软删。只对已建立关联的会话生效；
                    先预览、先备份，执行后可一键还原。启用镜像切换时不可用（两者互斥）。
                  </p>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={busy || deletedSyncBusy || mirrorBlocks.sessionSelectAll}
                    onClick={() => void scanDeletedSync()}
                  >
                    {deletedSyncBusy ? <Loader2 className="animate-spin" /> : <CircleAlert />}
                    {deletedSyncBusy ? "正在扫描…" : "扫描并预览"}
                  </Button>
                  {deletedSyncError && (
                    <p className="break-all text-xs text-destructive">{deletedSyncError}</p>
                  )}
                  {deletedSync && deletedSync.sessionCount === 0 && (
                    <p className="text-xs text-muted-foreground">
                      没有需要同步删除的会话：已关联的会话里不存在「一边删了、一边还留着」的情况。
                    </p>
                  )}
                  {deletedSync && deletedSync.sessionCount > 0 && (
                    <div className="space-y-2 rounded-md border bg-muted/30 px-2.5 py-2">
                      <div className="text-xs font-medium">
                        {`将在 ${deletedSync.accountCount} 个账号上软删 ${deletedSync.sessionCount} 个会话`}
                      </div>
                      <ul className="max-h-40 space-y-1 overflow-y-auto text-xs">
                        {deletedSync.targets.map((target) => (
                          <li
                            key={`deleted:${target.userId}:${target.sessionId}`}
                            className="flex items-start justify-between gap-2"
                          >
                            <span className="min-w-0 break-all">
                              {target.title || target.sessionId}
                            </span>
                            <span className="shrink-0 text-muted-foreground">
                              {memorySources.options.find((o) => o.uid === target.userId)?.label ??
                                target.userId.slice(0, 8)}
                            </span>
                          </li>
                        ))}
                      </ul>
                      <label className="flex cursor-pointer items-start gap-2">
                        <Checkbox
                          checked={deletedSyncConfirmed}
                          disabled={busy}
                          onCheckedChange={(value) => setDeletedSyncConfirmed(value === true)}
                          className="mt-0.5"
                        />
                        <span className="text-xs">
                          我确认把这 {deletedSync.sessionCount} 个会话也删掉（执行前自动备份）
                        </span>
                      </label>
                      {deletedSyncConfirmed && (
                        <p className="text-xs font-medium text-amber-700 dark:text-amber-400">
                          已确认：将随本次切号一起软删，成功后可在本 tab 一键还原。
                        </p>
                      )}
                    </div>
                  )}
                </div>
                <SessionSyncSection
                  open={open}
                  account={account}
                  disabled={busy || mirrorBlocks.sessionSelectAll}
                  onChange={(state) => {
                    setSyncSelections(state.selections);
                    setSyncGroups(state.groups);
                  }}
                  onMetaChange={setLinksMeta}
                />
              </TabsContent>
            )}
            <TabsContent
              value="copy"
              forceMount
              className="min-h-0 space-y-2 overflow-y-auto data-[state=inactive]:hidden data-[state=active]:animate-in data-[state=active]:fade-in-0 data-[state=active]:duration-150"
            >
              {copyTabContent}
            </TabsContent>
            <TabsContent
              value="sync"
              forceMount
              className="min-h-0 overflow-y-auto data-[state=inactive]:hidden data-[state=active]:animate-in data-[state=active]:fade-in-0 data-[state=active]:duration-150"
            >
              {syncTabContent}
            </TabsContent>
            <TabsContent
              value="mirror"
              forceMount
              className="min-h-0 overflow-y-auto data-[state=inactive]:hidden data-[state=active]:animate-in data-[state=active]:fade-in-0 data-[state=active]:duration-150"
            >
              {mirrorTabContent}
            </TabsContent>
          </Tabs>
        </div>

        <DialogFooter className="shrink-0 sm:justify-between">
          <div className="min-w-0 space-y-0.5">
            <div className="text-sm font-medium">{summaryMain}</div>
            {summarySub && <div className="text-xs text-muted-foreground">{summarySub}</div>}
            {memoryWarning && (
              <div className="text-xs font-medium text-amber-700 dark:text-amber-400">
                {memoryWarning}
              </div>
            )}
            {memoryBlocked && (
              <div className="text-xs text-amber-700 dark:text-amber-400">
                请先选出母本账号，或改回「不同步 / 合并」再切换。
              </div>
            )}
          </div>
          <div className="flex shrink-0 gap-2">
            <Button variant="outline" onClick={() => handleOpenChange(false)} disabled={busy}>
              取消
            </Button>
            <Button onClick={doSwitch} disabled={busy || memoryBlocked}>
              {busy ? "切换中…" : "确认切换"}
            </Button>
          </div>
        </DialogFooter>
        </div>
        </DialogAutoHeight>
      </DialogContent>
    </Dialog>
  );
}

