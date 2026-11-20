import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  FolderOpen,
  Loader2,
  RefreshCw,
  Trash2,
  Undo2,
} from "lucide-react";
import { toast } from "sonner";

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
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import * as api from "@/lib/api";
import type {
  CategoryUsage,
  CleanPlan,
  SessionAsset,
  StorageCategoryId,
  StorageSnapshot,
  TrashEntry,
} from "@/lib/types";

/** 四分类的固定展示顺序与文案；顺序改了前端缓存与文案映射都要跟着改。 */
const CATEGORY_ORDER: StorageCategoryId[] = ["tool", "client", "sessions", "trash"];

const CATEGORY_META: Record<StorageCategoryId, { title: string; hint: string }> = {
  tool: { title: "工具数据", hint: "切号工具自己的状态、日志与备份" },
  client: { title: "客户端数据", hint: "WorkBuddy 客户端数据根（已排除会话资产）" },
  sessions: { title: "会话资产", hint: "会话目录，删除后无法在客户端恢复" },
  trash: { title: "回收站", hint: "已移入回收站，可还原或彻底删除" },
};

/** 回收站只走彻底删除，不参与清理。 */
const CLEANABLE: StorageCategoryId[] = ["tool", "client", "sessions"];

const AGE_OPTIONS = [
  { value: "none", label: "不限时间" },
  { value: "7", label: "7 天前" },
  { value: "30", label: "30 天前" },
  { value: "90", label: "90 天前" },
];

/** 体积格式化：比归档页的 `formatBytes` 多覆盖 GB / TB（存储页会出现 GB 级）。 */
function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${unit === 0 ? Math.round(value) : value.toFixed(1).replace(/\.0$/, "")} ${units[unit]}`;
}

function formatStamp(at?: number): string {
  if (!at || !Number.isFinite(at)) return "-";
  return new Date(at).toLocaleString();
}

/** 判断是否「后端太旧 / 没有这条路由」，据此降级为升级提示而不是白屏（B14）。 */
function looksUnsupported(message: string): boolean {
  return /404|暂不支持|无法识别|not found/i.test(message);
}

/**
 * 零值三态文案：空 / 无权限 / 未扫描必须是三句不同的话。
 *
 * 把「扫描没完成」显示成「0 B」会让用户以为盘中真的没有数据。
 */
function usageCopy(usage: CategoryUsage): { text: string; tone: "muted" | "warn" } {
  if (!usage.exists) {
    return { text: "该分类不存在（客户端未安装或从未产生数据）", tone: "muted" };
  }
  if (usage.partial) {
    return { text: "部分目录无权限，体积可能偏小", tone: "warn" };
  }
  if (usage.bytes === 0) {
    return { text: "空", tone: "muted" };
  }
  return { text: `${usage.files} 个文件`, tone: "muted" };
}

export default function StoragePage() {
  const [snapshot, setSnapshot] = useState<StorageSnapshot | null>(null);
  const [sessions, setSessions] = useState<SessionAsset[]>([]);
  const [trash, setTrash] = useState<TrashEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [unsupported, setUnsupported] = useState(false);
  const [plan, setPlan] = useState<CleanPlan | null>(null);
  const [planCategory, setPlanCategory] = useState<StorageCategoryId | null>(null);
  const [olderThan, setOlderThan] = useState("none");
  const [pendingPurge, setPendingPurge] = useState<{ id: string; label: string } | null>(null);

  const signatureRef = useRef("");
  const lastCheckRef = useRef(0);

  const load = useCallback(async () => {
    setLoading(true);
    setError("");
    try {
      const scan = await api.scanStorage();
      setSnapshot(scan);
      setUnsupported(false);
      // 明细是附加信息：取不到不影响体积总览，不许因为它整页报错。
      const [sessionList, trashList] = await Promise.all([
        api.listStorageSessions(200).catch(() => ({ assets: [] as SessionAsset[] })),
        api.listStorageTrash().catch(() => ({ entries: [] as TrashEntry[] })),
      ]);
      setSessions(sessionList.assets ?? []);
      setTrash(trashList.entries ?? []);
    } catch (e) {
      const message = api.asError(e);
      setError(message);
      setUnsupported(looksUnsupported(message));
      setSnapshot(null);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // focus / visibility 触发轻量校验（不是定时轮询，也不是文件监听）。
  useEffect(() => {
    let disposed = false;
    const check = async () => {
      if (document.visibilityState === "hidden") return;
      const now = Date.now();
      if (now - lastCheckRef.current < 3000) return;
      lastCheckRef.current = now;
      try {
        const report = await api.revalidateStorage("storage", signatureRef.current);
        if (disposed) return;
        signatureRef.current = report.signature;
        if (report.changed) await load();
      } catch {
        // 校验是尽力而为：失败保留当前数据，不打扰用户。
      }
    };
    const onFocus = () => void check();
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onFocus);
    return () => {
      // G4：卸载后不再让异步结果回写状态 / 触发加载。
      disposed = true;
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onFocus);
    };
  }, [load]);

  const usageOf = (category: StorageCategoryId): CategoryUsage =>
    snapshot?.usage?.find((item) => item.category === category) ?? {
      category,
      bytes: 0,
      files: 0,
      partial: false,
      exists: false,
    };

  async function previewClean(category: StorageCategoryId) {
    setBusy(true);
    try {
      const res = await api.cleanStorage({
        category,
        olderThanDays: olderThan === "none" ? undefined : Number(olderThan),
        dryRun: true,
      });
      setPlan(res.plan ?? null);
      setPlanCategory(category);
      if ((res.plan?.entries ?? []).length === 0) {
        toast.info("没有需要清理的项目", {
          description:
            (res.plan?.skipped ?? []).length > 0
              ? "受保护或被跳过的目标不会出现在清理计划里"
              : undefined,
        });
      }
    } catch (e) {
      toast.error("生成清理计划失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  async function confirmClean() {
    if (!planCategory) return;
    setBusy(true);
    try {
      const res = await api.cleanStorage({
        category: planCategory,
        olderThanDays: olderThan === "none" ? undefined : Number(olderThan),
        dryRun: false,
      });
      const moved = res.outcome?.moved ?? 0;
      const skipped = res.outcome?.skipped ?? [];
      if (moved === 0) {
        toast.info("没有移动任何文件", {
          description: skipped.length > 0 ? skipped.map((item) => item.why).join("；") : undefined,
        });
      } else {
        toast.success(`已移入回收站 ${moved} 项`, {
          description: `${formatSize(res.outcome?.bytes ?? 0)}${
            skipped.length > 0 ? `，${skipped.length} 项被跳过` : ""
          }`,
        });
      }
      setPlan(null);
      setPlanCategory(null);
      await load();
    } catch (e) {
      toast.error("执行清理失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  async function restoreEntry(entry: TrashEntry) {
    setBusy(true);
    try {
      const res = await api.restoreStorageTrash(entry.id, false);
      const restored = res.outcome?.restored ?? 0;
      const skipped = res.outcome?.skipped ?? [];
      if (restored === 0) {
        toast.info("没有还原任何文件", {
          description: skipped.length > 0 ? skipped.join("；") : undefined,
        });
      } else {
        toast.success(`已还原 ${restored} 项`, {
          description: skipped.length > 0 ? `${skipped.length} 项被跳过（目标已存在）` : undefined,
        });
      }
      await load();
    } catch (e) {
      toast.error("还原失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  async function purgeEntry(id?: string) {
    setBusy(true);
    try {
      const res = await api.purgeStorageTrash(id);
      toast.success(`已彻底删除 ${res.outcome?.purged ?? 0} 项`, {
        description: "彻底删除无法恢复",
      });
      await load();
    } catch (e) {
      toast.error("彻底删除失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
      setPendingPurge(null);
    }
  }

  async function reveal(path?: string) {
    if (!path) {
      toast.error("无法定位目录", { description: "后端没有返回该分类的根路径" });
      return;
    }
    try {
      await api.revealStoragePath(path);
      toast.success("已在文件管理器中打开");
    } catch (e) {
      toast.error("无法打开目录", { description: api.asError(e) });
    }
  }

  return (
    <div className="mx-auto w-full max-w-4xl space-y-4 p-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold">存储管理</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            分类查看磁盘占用，按分类清理。删除一律先进回收站，彻底删除是独立动作且不可恢复。
          </p>
        </div>
        <Button variant="outline" size="sm" onClick={() => void load()} disabled={loading || busy}>
          {loading ? <Loader2 className="size-4 animate-spin" /> : <RefreshCw className="size-4" />}
          重新扫描
        </Button>
      </div>

      {unsupported && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">当前后端不支持存储管理</CardTitle>
            <CardDescription>
              服务端版本过旧，请升级 workbuddy-switch 后重试（升级不会删除任何数据）。
            </CardDescription>
          </CardHeader>
        </Card>
      )}

      {!unsupported && error && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">无法读取存储信息</CardTitle>
            <CardDescription>{error}</CardDescription>
          </CardHeader>
        </Card>
      )}

      <div className="grid gap-3 sm:grid-cols-2">
        {CATEGORY_ORDER.map((category) => {
          const usage = usageOf(category);
          const meta = CATEGORY_META[category];
          const copy = usageCopy(usage);
          return (
            <Card key={category}>
              <CardHeader className="pb-2">
                <div className="flex items-center justify-between gap-2">
                  <CardTitle className="text-base">{meta.title}</CardTitle>
                  {usage.partial && (
                    <Badge variant="outline" className="gap-1">
                      <AlertTriangle className="size-3" />
                      不完整
                    </Badge>
                  )}
                </div>
                <CardDescription>{meta.hint}</CardDescription>
              </CardHeader>
              <CardContent className="space-y-2">
                <div className="text-2xl font-semibold tabular-nums">
                  {loading && !snapshot ? "…" : formatSize(usage.bytes)}
                </div>
                <div
                  className={
                    copy.tone === "warn"
                      ? "text-xs text-amber-600 dark:text-amber-500"
                      : "text-xs text-muted-foreground"
                  }
                >
                  {loading && !snapshot ? "正在扫描…" : copy.text}
                </div>
                <div className="flex flex-wrap gap-2 pt-1">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => void reveal(usage.path)}
                    disabled={busy || !usage.exists}
                    title="在文件管理器中定位该分类的根目录（仅桌面端支持）"
                  >
                    <FolderOpen className="size-4" />
                    打开目录
                  </Button>
                  {CLEANABLE.includes(category) && (
                    <Button
                      variant="secondary"
                      size="sm"
                      onClick={() => void previewClean(category)}
                      disabled={busy}
                    >
                      预览清理
                    </Button>
                  )}
                </div>
              </CardContent>
            </Card>
          );
        })}
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">清理</CardTitle>
          <CardDescription>
            「预览清理」只生成计划，不会动任何文件；确认后才会移入回收站。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm text-muted-foreground">只清理</span>
            <Select value={olderThan} onValueChange={setOlderThan}>
              <SelectTrigger className="w-40">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {AGE_OPTIONS.map((option) => (
                  <SelectItem key={option.value} value={option.value}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <span className="text-sm text-muted-foreground">的项目</span>
          </div>

          {plan && (
            <div className="space-y-2 rounded-md border border-foreground/10 p-3">
              <div className="text-sm font-medium">
                {planCategory ? CATEGORY_META[planCategory].title : ""} 清理计划：
                {(plan.entries ?? []).length} 项，共 {formatSize(plan.bytes ?? 0)}
              </div>
              <div className="max-h-56 space-y-1 overflow-y-auto">
                {(plan.entries ?? []).map((entry) => (
                  <div
                    key={entry.path}
                    className="flex items-start justify-between gap-3 text-xs text-muted-foreground"
                  >
                    <span className="min-w-0 truncate" title={entry.path}>
                      {entry.path}
                    </span>
                    <span className="shrink-0 tabular-nums">{formatSize(entry.bytes ?? 0)}</span>
                  </div>
                ))}
              </div>
              {(plan.skipped ?? []).length > 0 && (
                <div className="text-xs text-amber-600 dark:text-amber-500">
                  {(plan.skipped ?? []).length} 项被跳过：{(plan.skipped ?? [])
                    .map((item) => item.why)
                    .slice(0, 3)
                    .join("；")}
                </div>
              )}
              <div className="flex gap-2 pt-1">
                <Button size="sm" onClick={() => void confirmClean()} disabled={busy}>
                  {busy && <Loader2 className="size-4 animate-spin" />}
                  确认移入回收站
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => {
                    setPlan(null);
                    setPlanCategory(null);
                  }}
                  disabled={busy}
                >
                  取消
                </Button>
              </div>
            </div>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">会话资产</CardTitle>
          <CardDescription>按体积倒序，最多显示 200 条。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-1">
          {sessions.length === 0 && (
            <p className="py-4 text-sm text-muted-foreground">没有可显示的会话目录。</p>
          )}
          {sessions.map((asset) => (
            <div
              key={asset.path}
              className="flex items-start justify-between gap-3 rounded-md px-2 py-2 text-sm hover:bg-foreground/[0.03]"
            >
              <div className="min-w-0">
                <div className="truncate font-medium">{asset.name ?? "-"}</div>
                <div className="truncate text-xs text-muted-foreground">
                  {asset.project ?? "-"} · {formatStamp(asset.modified_at)}
                </div>
              </div>
              <div className="flex shrink-0 items-center gap-2 text-xs text-muted-foreground tabular-nums">
                <span>{asset.files ?? 0} 文件</span>
                <span>{formatSize(asset.bytes ?? 0)}</span>
              </div>
            </div>
          ))}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">回收站</CardTitle>
          <CardDescription>
            清单损坏的条目无法还原（没有还原依据），但仍可彻底删除。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-1">
          {trash.length === 0 && (
            <p className="py-4 text-sm text-muted-foreground">回收站是空的。</p>
          )}
          {trash.map((entry) => (
            <div
              key={entry.id}
              className="flex flex-wrap items-center justify-between gap-2 rounded-md px-2 py-2 text-sm hover:bg-foreground/[0.03]"
            >
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <span className="font-medium tabular-nums">{entry.id}</span>
                  {entry.manifest_ok === false && <Badge variant="destructive">清单损坏</Badge>}
                </div>
                <div className="truncate text-xs text-muted-foreground">
                  {formatStamp(entry.created_at)} · {entry.items ?? 0} 项 ·{" "}
                  {formatSize(entry.bytes ?? 0)}
                  {entry.reason ? ` · ${entry.reason}` : ""}
                </div>
              </div>
              <div className="flex shrink-0 gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy || entry.manifest_ok === false}
                  title={
                    entry.manifest_ok === false
                      ? "还原清单缺失或损坏，无法确定原位置"
                      : "还原到原位置（同名目标不会被覆盖）"
                  }
                  onClick={() => void restoreEntry(entry)}
                >
                  <Undo2 className="size-4" />
                  还原
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={busy}
                  onClick={() =>
                    setPendingPurge({ id: entry.id, label: `${entry.id}（${entry.items ?? 0} 项）` })
                  }
                >
                  <Trash2 className="size-4" />
                  彻底删除
                </Button>
              </div>
            </div>
          ))}
          {trash.length > 0 && (
            <div className="pt-2">
              <Button
                size="sm"
                variant="destructive"
                disabled={busy}
                onClick={() => setPendingPurge({ id: "", label: "回收站全部内容" })}
              >
                清空回收站
              </Button>
            </div>
          )}
        </CardContent>
      </Card>

      <AlertDialog
        open={pendingPurge !== null}
        onOpenChange={(open) => {
          if (!open) setPendingPurge(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>彻底删除？</AlertDialogTitle>
            <AlertDialogDescription>
              {pendingPurge?.label} 会被直接删除，无法还原。想保留的话请先点「还原」。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction
              onClick={() => void purgeEntry(pendingPurge?.id || undefined)}
              disabled={busy}
            >
              彻底删除
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
