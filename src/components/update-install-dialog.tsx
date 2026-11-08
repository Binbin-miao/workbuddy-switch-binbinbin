import { useEffect, useState } from "react";
import { Download, ExternalLink, Loader2, RefreshCw } from "lucide-react";

import { DemoAction } from "@/components/demo-action";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { GITHUB_RELEASE_URL, openReleaseUrl } from "@/lib/update";
import { useUpdateState } from "@/lib/use-update-state";

interface UpdateInstallDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * 统一更新弹窗：只展示 Rust 更新状态机（与托盘菜单同源）的阶段与进度。
 *
 * 阶段与进度来自 `update-state` 订阅，不再自己调用 `@tauri-apps/plugin-updater`；
 * 下载 / 重启都只是把动作交给后端（`updateDownload` / `updateRestart`），状态由后端
 * 持有——关掉弹窗再打开，看到的仍是同一阶段与进度。
 */
export function UpdateInstallDialog({ open, onOpenChange }: UpdateInstallDialogProps) {
  const snapshot = useUpdateState();
  const [restarting, setRestarting] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  const phase = snapshot.phase;
  const targetVersion = snapshot.latest ?? "新版本";
  const percent = snapshot.percent;
  const error = actionError ?? snapshot.message ?? "未知更新错误";

  useEffect(() => {
    if (!open) return;
    // 每次打开重置本地动作态：阶段与错误文案都由快照承载。
    setRestarting(false);
    setCancelling(false);
    setActionError(null);
  }, [open]);

  // 阶段离开 downloading（取消成功 / 换源 / 完成 / 失败）就松开取消按钮：
  // 留着 disabled 会让用户以为卡死，而实际上已经可以再点下载了。
  useEffect(() => {
    if (phase !== "downloading") setCancelling(false);
  }, [phase]);

  async function startDownload() {
    setActionError(null);
    try {
      await api.updateDownload();
    } catch (e) {
      setActionError(`下载更新失败：${api.asError(e)}`);
    }
  }

  /**
   * 取消后台下载：后端幂等，取消成功后阶段回到「可升级」并保留最新版本号，
   * 用户可以随时再点一次下载。`cancelling` 只防连点，阶段一变就复位。
   */
  async function cancelDownload() {
    setCancelling(true);
    setActionError(null);
    try {
      await api.updateCancelDownload();
    } catch (e) {
      setCancelling(false);
      setActionError(`取消下载失败：${api.asError(e)}`);
    }
  }

  /** 与托盘一致：已知目标版本则重试下载，否则重试检查。 */
  async function retry() {
    if (snapshot.latest) {
      await startDownload();
      return;
    }
    setActionError(null);
    try {
      const result = await api.checkUpdate(undefined, true);
      if (!result.ok) {
        setActionError(result.message || result.error || "检查更新失败");
      }
    } catch (e) {
      setActionError(`检查更新失败：${api.asError(e)}`);
    }
  }

  /** 跳过当前版本：只写配置，不动已经下好的包与更新源。 */
  async function skipVersion() {
    if (!snapshot.latest) return;
    setActionError(null);
    try {
      await api.updateSkipVersion(snapshot.latest);
      onOpenChange(false);
    } catch (e) {
      setActionError(`跳过版本失败：${api.asError(e)}`);
    }
  }

  async function restartApp() {
    setRestarting(true);
    setActionError(null);
    try {
      await api.updateRestart();
    } catch (e) {
      setRestarting(false);
      setActionError(`重启失败：${api.asError(e)}`);
    }
  }

  async function openRelease() {
    try {
      await openReleaseUrl(GITHUB_RELEASE_URL);
    } catch (e) {
      setActionError(api.asError(e));
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        // 只有「正在安装并重启」不可关；下载在后台跑，关窗不该拦——
        // 否则文案说"可随时从托盘查看进度"，按钮却把人锁在弹窗里。
        if (!next && restarting) return;
        onOpenChange(next);
      }}
    >
      <DialogContent showCloseButton={!restarting}>
        <DialogHeader>
          <DialogTitle>
            {phase === "checking" && "正在检查更新"}
            {phase === "downloading" && `正在升级到 v${targetVersion}`}
            {(phase === "idle" || phase === "upToDate") && "当前已是最新版本"}
            {phase === "available" && "发现新版本"}
            {phase === "readyToRestart" && "更新已就绪"}
            {phase === "error" && "拉取更新失败"}
          </DialogTitle>
          <DialogDescription>
            {phase === "checking" && "正在检查 GitHub Release 中的签名更新包，请稍候。"}
            {phase === "downloading" &&
              "下载在后台进行，关掉这个窗口不会中断；进度见托盘提示，完成后由你决定何时重启安装。"}
            {(phase === "idle" || phase === "upToDate") &&
              "没有发现高于当前版本的签名更新包。"}
            {phase === "available" &&
              `发现新版本 v${targetVersion}，下载后由你决定何时重启安装。`}
            {phase === "readyToRestart" && "更新包已下载，重启应用后完成安装并生效。"}
            {phase === "error" && "自动更新未完成，你仍然可以从 GitHub Release 页面手动下载。"}
          </DialogDescription>
        </DialogHeader>

        {phase === "checking" && (
          <div className="flex items-center gap-2 rounded-md bg-muted/50 p-3 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" />
            正在连接公开 Release 更新源…
          </div>
        )}

        {phase === "available" && (
          <div className="space-y-1 rounded-md border p-3 text-sm">
            <p>v{targetVersion} 已确认可升级。</p>
            <p className="text-xs text-muted-foreground">
              开启「自动后台下载」时，发现新版本会自动开始下载；这里只是手动入口。下载在后台进行，关掉窗口不会中断。
            </p>
          </div>
        )}

        {phase === "downloading" && (
          <div className="space-y-2 rounded-md border p-3">
            <div className="flex items-center justify-between text-sm">
              <span className="flex items-center gap-2">
                <Loader2 className="size-4 animate-spin text-primary" />
                下载更新包
              </span>
              <span className="font-mono text-xs text-muted-foreground">
                {percent === null ? "下载中…" : `${percent}%`}
              </span>
            </div>
            <div className="h-2 overflow-hidden rounded-full bg-muted">
              <div
                className="h-full rounded-full bg-primary transition-[width] duration-200"
                style={{ width: `${percent ?? 30}%` }}
              />
            </div>
            <div className="flex items-center justify-between gap-2">
              <p className="text-xs text-muted-foreground">
                涓流通道会自动换源，进度偶尔回落属正常。
              </p>
              <Button
                variant="outline"
                size="sm"
                onClick={() => void cancelDownload()}
                disabled={cancelling}
              >
                {cancelling && <Loader2 className="animate-spin" />}
                {cancelling ? "正在取消…" : "取消下载"}
              </Button>
            </div>
          </div>
        )}

        {phase === "readyToRestart" && (
          <div className="rounded-md border border-emerald-500/30 bg-emerald-500/10 p-3 text-sm text-emerald-800">
            v{targetVersion} 已准备完成，重启应用后生效。
          </div>
        )}

        {phase === "error" && (
          <div className="space-y-2 rounded-md border border-destructive/30 bg-destructive/5 p-3 text-sm">
            <p className="text-destructive">{error}</p>
            <p className="break-all text-xs text-muted-foreground">{GITHUB_RELEASE_URL}</p>
          </div>
        )}

        <DialogFooter>
          {phase === "available" && (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                稍后提醒
              </Button>
              <DemoAction>
                <Button
                  variant="ghost"
                  onClick={() => void skipVersion()}
                  disabled={!snapshot.latest}
                >
                  跳过 v{targetVersion}
                </Button>
              </DemoAction>
              <DemoAction>
                <Button onClick={() => void startDownload()}>
                  <Download />
                  立即下载
                </Button>
              </DemoAction>
            </>
          )}
          {phase === "readyToRestart" && (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)} disabled={restarting}>
                立即关闭
              </Button>
              <DemoAction>
                <Button onClick={() => void restartApp()} disabled={restarting}>
                  {restarting ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                  {restarting ? "正在重启…" : "重启以完成升级"}
                </Button>
              </DemoAction>
            </>
          )}
          {phase === "error" && (
            <>
              <Button variant="outline" onClick={() => void openRelease()}>
                <ExternalLink />
                打开 GitHub Release
              </Button>
              <DemoAction>
                <Button onClick={() => void retry()}>
                  <RefreshCw />
                  重试
                </Button>
              </DemoAction>
            </>
          )}
          {(phase === "idle" || phase === "upToDate") && (
            <Button variant="default" onClick={() => onOpenChange(false)}>
              关闭
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
