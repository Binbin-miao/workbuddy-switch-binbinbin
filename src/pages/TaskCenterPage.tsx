import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, RefreshCw, Wand2 } from "lucide-react";
import { toast } from "sonner";

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
import { displayName } from "@/lib/account-display";
import type {
  AccountMeta,
  ClaimLedgerSummary,
  ClaimState,
  GrowthAutoItem,
  TaskCenterItem,
  TaskCenterList,
} from "@/lib/types";

/**
 * 领取台账三态文案。`unknown` 必须单独成句：查不到 ≠ 没得领（B10 / L4）。
 */
function claimStateLabel(summary: ClaimLedgerSummary | null): string {
  if (!summary) return "领取状态未知（官方暂不可查）";
  switch (summary.state) {
    case "done":
      return "今日已领完";
    case "partial":
      return `还剩 ${summary.remaining} 项`;
    case "nothing":
      return "今日无可领取";
    default:
      return "领取状态未知（官方暂不可查）";
  }
}

function claimStateVariant(state: ClaimState | undefined): "default" | "secondary" | "outline" {
  if (state === "partial") return "default";
  if (state === "unknown" || state === undefined) return "outline";
  return "secondary";
}

/**
 * 成长任务：列出 / 接受 / 领取，以及一键完成。
 *
 * 「一键完成」会替用户触发真实行为（上报客户端事件、发对话、领养 Buddy），
 * 属于代劳操作，故单独放在按钮区并明确标注耗时与副作用。
 */
export default function TaskCenterPage() {
  const [accounts, setAccounts] = useState<AccountMeta[]>([]);
  const [accountId, setAccountId] = useState("");
  const [list, setList] = useState<TaskCenterList | null>(null);
  const [autoResults, setAutoResults] = useState<GrowthAutoItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [autoRunning, setAutoRunning] = useState(false);
  const [error, setError] = useState("");
  const [ledger, setLedger] = useState<ClaimLedgerSummary | null>(null);

  useEffect(() => {
    void api
      .getAccounts()
      .then((res) => {
        setAccounts(res.accounts);
        if (res.accounts.length > 0) setAccountId(res.accounts[0].id);
      })
      .catch((e) => setError(api.asError(e)));
  }, []);

  const load = useCallback(
    async (id: string) => {
      if (!id) return;
      setLoading(true);
      setError("");
      try {
        setList(await api.listTaskCenter(id));
      } catch (e) {
        setError(api.asError(e));
        setList(null);
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  useEffect(() => {
    if (accountId) void load(accountId);
  }, [accountId, load]);

  // 领取台账只读：后端在拉任务时写入，这里只负责展示三态文案。
  useEffect(() => {
    const uid = accounts.find((account) => account.id === accountId)?.uid;
    if (!uid) {
      setLedger(null);
      return;
    }
    let disposed = false;
    void api
      .getClaimLedgerSummary(uid)
      .then((summary) => {
        if (!disposed) setLedger(summary);
      })
      .catch(() => {
        // 台账是旁路信息：取不到就显示「状态未知」，不影响任务列表。
        if (!disposed) setLedger(null);
      });
    // G4：账号切换 / 卸载后丢弃过期响应。
    return () => {
      disposed = true;
    };
  }, [accountId, accounts]);

  // 打开面板即把已达标的奖励领掉：只领该领的、幂等，不产生任何上报数据。
  // 同一账号每次进入只自动领一次，避免反复请求；失败不打扰用户，仍可用按钮手动领。
  const autoClaimed = useRef("");
  useEffect(() => {
    if (!accountId || !list || (list.claimable ?? 0) === 0) return;
    if (autoClaimed.current === accountId) return;
    autoClaimed.current = accountId;
    void (async () => {
      try {
        const res = await api.claimTaskCenter(accountId);
        if (res.error || res.claimed.length === 0) return;
        const total = res.creditTotal ?? 0;
        toast.success(`自动领取 ${res.claimed.length} 项${total > 0 ? `，+${total} 积分` : ""}`);
        await load(accountId);
      } catch {
        // 自动领取失败不影响浏览，手动按钮仍在。
      }
    })();
  }, [accountId, list, load]);

  async function acceptAll() {
    if (!accountId) return;
    setBusy(true);
    try {
      const res = await api.acceptTaskCenter(accountId);
      const failed = res.failed ?? [];
      if (res.error) {
        toast.error("接受任务失败", { description: res.error });
      } else if (res.accepted.length === 0) {
        toast.info(res.note ?? "没有需要接受的任务");
      } else if (failed.length > 0) {
        // 外层成功但个别任务被判 not found，必须如实说明，不能报「全部成功」。
        toast.success(`已接受 ${res.accepted.length} 个，${failed.length} 个被上游拒绝`, {
          description: failed.map((f) => `${f.code}：${f.reason}`).join("；"),
        });
      } else {
        toast.success(`已接受 ${res.accepted.length} 个任务`);
      }
      await load(accountId);
    } catch (e) {
      toast.error("接受任务失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  async function claimAll() {
    if (!accountId) return;
    setBusy(true);
    try {
      const res = await api.claimTaskCenter(accountId);
      if (res.error) {
        toast.error("领取奖励失败", { description: res.error });
      } else {
        if (res.claimed.length > 0) {
          toast.success(`已领取 ${res.claimed.length} 个奖励`, {
            description: `共 ${res.creditTotal ?? 0} 积分`,
          });
        } else {
          toast.info("没有可领取的奖励", {
            description: res.skipped.length > 0 ? "未达标的任务需要先在客户端里完成任务" : undefined,
          });
        }
        if (res.errors.length > 0) {
          toast.error(`${res.errors.length} 个奖励领取失败`, {
            description: res.errors.map((item) => `${item.title ?? item.code}：${item.error}`).join("；"),
          });
        }
      }
      await load(accountId);
    } catch (e) {
      toast.error("领取奖励失败", { description: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  /**
   * 一键完成：逐个任务走「接受 → 触发行为 → 回读 → 领奖」闭环。
   *
   * 会发真实对话、按真人节奏间隔上报，整体耗时数分钟，故单独用 running 态
   * 区分于普通的接受/领取，避免按钮互相抢占。
   */
  async function runAuto() {
    if (!accountId) return;
    setAutoRunning(true);
    setAutoResults([]);
    try {
      const res = await api.runGrowthAuto(accountId);
      if (res.error) {
        toast.error("一键完成失败", { description: res.error });
        return;
      }
      const items = res.results ?? [];
      setAutoResults(items);
      const done = items.filter((item) => item.status === "done").length;
      const failed = items.filter((item) => item.status === "failed").length;
      const total = res.creditTotal ?? 0;
      if (failed > 0) {
        toast.warning(`完成 ${done} 项，${failed} 项失败`, {
          description: items
            .filter((item) => item.status === "failed")
            .map((item) => `${item.code}：${item.message}`)
            .join("；"),
        });
      } else {
        toast.success(`一键完成结束，成功 ${done} 项${total > 0 ? `，+${total} 积分` : ""}`);
      }
      if (res.note) toast.info(res.note);
      await load(accountId);
    } catch (e) {
      toast.error("一键完成失败", { description: api.asError(e) });
    } finally {
      setAutoRunning(false);
    }
  }

  const tasks = list?.tasks ?? [];
  const claimable = tasks.filter((task) => task.claimable).length;

  return (
    <div className="mx-auto w-full max-w-4xl space-y-4 p-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <div className="flex items-center gap-2">
            <h1 className="text-xl font-semibold">成长任务</h1>
            <Badge variant="secondary">beta</Badge>
            {accountId && (
              <Badge variant={claimStateVariant(ledger?.state)}>{claimStateLabel(ledger)}</Badge>
            )}
          </div>
          <p className="mt-1 text-sm text-muted-foreground">
            列出当前账号的全部成长任务，可一键接受与领取已达标的奖励。「一键完成」会替该账号触发真实行为
            （上报客户端事件、发对话、领养 Buddy）并领奖，耗时数分钟。
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Select value={accountId} onValueChange={setAccountId}>
            <SelectTrigger className="w-56">
              <SelectValue placeholder="选择账号" />
            </SelectTrigger>
            <SelectContent>
              {accounts.map((account) => (
                <SelectItem key={account.id} value={account.id}>
                  {displayName(account)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button variant="outline" size="sm" onClick={() => void load(accountId)} disabled={loading || busy}>
            {loading ? <Loader2 className="size-4 animate-spin" /> : <RefreshCw className="size-4" />}
            刷新
          </Button>
        </div>
      </div>

      {error && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">无法读取任务</CardTitle>
            <CardDescription>{error}</CardDescription>
          </CardHeader>
        </Card>
      )}

      {list?.error && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">无法读取任务</CardTitle>
            <CardDescription>{list.error}</CardDescription>
          </CardHeader>
        </Card>
      )}

      {list?.errors && list.errors.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">部分口径取不到</CardTitle>
            <CardDescription>{list.errors.join("；")}</CardDescription>
          </CardHeader>
        </Card>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button onClick={() => void acceptAll()} disabled={busy || autoRunning || !accountId}>
          一键接受任务
        </Button>
        <Button variant="secondary" onClick={() => void claimAll()} disabled={busy || autoRunning || !accountId}>
          一键领取奖励{claimable > 0 ? `（${claimable}）` : ""}
        </Button>
        <Button
          variant="outline"
          onClick={() => void runAuto()}
          disabled={busy || autoRunning || !accountId}
          title="替当前账号触发真实行为（上报事件、发对话、领养 Buddy）并领奖，耗时数分钟"
        >
          {autoRunning ? <Loader2 className="size-4 animate-spin" /> : <Wand2 className="size-4" />}
          一键完成成长任务
        </Button>
        {busy && <Loader2 className="size-4 animate-spin text-muted-foreground" />}
        {autoRunning && (
          <span className="text-xs text-muted-foreground">
            正在完成成长任务，含真实对话与节奏间隔，预计数分钟…
          </span>
        )}
      </div>

      {autoResults.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">一键完成结果</CardTitle>
            <CardDescription>
              逐条记录每个任务的执行情况。「已点亮」表示行为已触发但上游计分尚未落账，下次再跑会补上。
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-1">
            {autoResults.map((item) => (
              <AutoResultRow key={item.code} item={item} />
            ))}
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">任务列表</CardTitle>
          <CardDescription>
            共 {tasks.length} 项，可领取 {claimable} 项。打开本页会自动领掉已达标的奖励；
            未接受的任务上游不返回进度——先点「一键接受任务」，之后才会开始计数。
            <span className="mt-1 block">
              领取台账：{claimStateLabel(ledger)}
              {ledger?.state === "unknown"
                ? "（官方接口本次没有返回任务数据，保留上一次的记录）"
                : ""}
            </span>
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-1">
          {loading && tasks.length === 0 && (
            <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              正在读取…
            </div>
          )}
          {!loading && tasks.length === 0 && !list?.error && (
            <p className="py-6 text-sm text-muted-foreground">该账号暂无可显示的成长任务。</p>
          )}
          {tasks.map((task) => (
            <TaskRow key={task.code} task={task} />
          ))}
        </CardContent>
      </Card>
    </div>
  );
}

/** 一键完成里单条任务的状态标签。 */
const AUTO_STATE: Record<GrowthAutoItem["status"], { label: string; variant: "default" | "secondary" | "destructive" | "outline" }> = {
  done: { label: "已完成", variant: "default" },
  progressed: { label: "已点亮", variant: "outline" },
  skipped: { label: "已跳过", variant: "secondary" },
  failed: { label: "失败", variant: "destructive" },
};

function AutoResultRow({ item }: { item: GrowthAutoItem }) {
  const state = AUTO_STATE[item.status] ?? AUTO_STATE.skipped;
  return (
    <div className="flex items-start justify-between gap-3 rounded-md px-2 py-2 hover:bg-foreground/[0.03]">
      <div className="min-w-0">
        <div className="truncate text-sm font-medium">{item.title || item.code}</div>
        <div className="truncate text-xs text-muted-foreground">{item.message}</div>
      </div>
      <div className="flex shrink-0 items-center gap-2 text-xs text-muted-foreground">
        {item.credit > 0 && <span className="tabular-nums">+{item.credit} 积分</span>}
        <Badge variant={state.variant}>{state.label}</Badge>
      </div>
    </div>
  );
}

function TaskRow({ task }: { task: TaskCenterItem }) {
  const notAccepted = task.acceptStatus === "not_accepted" || task.acceptStatus === "";
  const progress = task.progressKnown ? `${task.current}/${task.target}` : "—";
  const state = task.claimed
    ? "已领取"
    : task.claimable
      ? "可领取"
      : notAccepted
        ? "未接受"
        : task.progressKnown
          ? "进行中"
          : "暂无进度";

  return (
    <div className="flex items-start justify-between gap-3 rounded-md px-2 py-2 hover:bg-foreground/[0.03]">
      <div className="min-w-0">
        <div className="truncate text-sm font-medium">{task.title}</div>
        {task.hint && <div className="truncate text-xs text-muted-foreground">{task.hint}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-3 text-xs text-muted-foreground">
        <span className="tabular-nums">{progress}</span>
        {task.credit > 0 && <span className="tabular-nums">+{task.credit} 积分</span>}
        <Badge variant={task.claimable ? "default" : "secondary"}>{state}</Badge>
      </div>
    </div>
  );
}
