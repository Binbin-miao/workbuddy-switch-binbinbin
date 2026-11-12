import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  TokenStatistics,
  TokenStatsAvailability,
  TokenStatsSource,
  TokenStatsTotals,
} from "@/lib/types";

const { getTokenStatisticsMock } = vi.hoisted(() => ({
  getTokenStatisticsMock: vi.fn(),
}));

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return { ...actual, getTokenStatistics: getTokenStatisticsMock };
});

const BASE_TIME = Date.parse("2026-06-01T12:00:00Z");
const TTL_MS = 30 * 60 * 1000;

function tokenStatistics(generatedAt: number): TokenStatistics {
  return { generatedAt, rangeDays: null, sources: [] };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

function tokenTotals(records: number): TokenStatsTotals {
  return {
    total: 0,
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    uncachedInput: 0,
    records,
    cacheHitRate: null,
  };
}

function tokenSource(
  source: TokenStatsSource["source"],
  availability: TokenStatsAvailability,
  records = 0,
  filesScanned = 0,
): TokenStatsSource {
  return {
    source,
    availability,
    summary: tokenTotals(records),
    models: [],
    projects: [],
    sessions: [],
    daily: [],
    hours: [],
    filesScanned,
    parseErrors: 0,
  };
}

function statisticsWith(sources: TokenStatsSource[]): TokenStatistics {
  return { generatedAt: BASE_TIME, rangeDays: null, sources };
}

/** 每个用例拿一份全新的模块实例，隔离模块级缓存；切页重挂载复用的是同一实例。 */
async function freshModule() {
  vi.resetModules();
  return import("./TokenStatsPage");
}

beforeEach(() => {
  vi.spyOn(Date, "now").mockReturnValue(BASE_TIME);
  getTokenStatisticsMock.mockReset();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("Token 统计会话内缓存（30min TTL + inflight 去重）", () => {
  it("30 分钟内切页 5 次只触发 1 次全量扫描", async () => {
    const page = await freshModule();
    const stats = tokenStatistics(BASE_TIME);
    getTokenStatisticsMock.mockResolvedValue(stats);

    for (let mount = 0; mount < 5; mount += 1) {
      expect(await page.loadCachedTokenStatistics(false)).toEqual(stats);
    }
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(1);
  });

  it("并发请求（含强制刷新）共享同一次扫描，不重复 invoke", async () => {
    const page = await freshModule();
    const gate = deferred<TokenStatistics>();
    getTokenStatisticsMock.mockReturnValue(gate.promise);

    const first = page.loadCachedTokenStatistics(false);
    const second = page.loadCachedTokenStatistics(true);
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(1);

    const stats = tokenStatistics(BASE_TIME);
    gate.resolve(stats);
    expect(await first).toEqual(stats);
    expect(await second).toEqual(stats);
  });

  it("连续点「刷新统计」每次都强制重采，不读缓存", async () => {
    const page = await freshModule();
    getTokenStatisticsMock.mockResolvedValueOnce(tokenStatistics(BASE_TIME));
    await page.loadCachedTokenStatistics(false);
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(1);

    const second = tokenStatistics(BASE_TIME + 1_000);
    getTokenStatisticsMock.mockResolvedValueOnce(second);
    expect(await page.loadCachedTokenStatistics(true)).toEqual(second);

    const third = tokenStatistics(BASE_TIME + 2_000);
    getTokenStatisticsMock.mockResolvedValueOnce(third);
    expect(await page.loadCachedTokenStatistics(true)).toEqual(third);
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(3);
  });

  it("缓存超过 30 分钟后进入页面自动重新扫描，不会永久脏读", async () => {
    const page = await freshModule();
    getTokenStatisticsMock.mockResolvedValueOnce(tokenStatistics(BASE_TIME));
    await page.loadCachedTokenStatistics(false);
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(1);

    vi.mocked(Date.now).mockReturnValue(BASE_TIME + TTL_MS + 1);
    const refreshed = tokenStatistics(BASE_TIME + TTL_MS + 1);
    getTokenStatisticsMock.mockResolvedValueOnce(refreshed);
    expect(await page.loadCachedTokenStatistics(false)).toEqual(refreshed);
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(2);
  });

  it("扫描失败不写缓存，也不留下永久中毒的 inflight", async () => {
    const page = await freshModule();
    getTokenStatisticsMock.mockRejectedValueOnce(new Error("扫描失败"));
    await expect(page.loadCachedTokenStatistics(false)).rejects.toThrow("扫描失败");

    const recovered = tokenStatistics(BASE_TIME);
    getTokenStatisticsMock.mockResolvedValueOnce(recovered);
    expect(await page.loadCachedTokenStatistics(false)).toEqual(recovered);
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(2);
  });

  it("有扫描在飞时刷新也复用它，避免两次全量重扫把进度分母打乱", async () => {
    const page = await freshModule();
    const gate = deferred<TokenStatistics>();
    getTokenStatisticsMock.mockReturnValue(gate.promise);

    const first = page.loadCachedTokenStatistics(true);
    const second = page.loadCachedTokenStatistics(true);
    expect(getTokenStatisticsMock).toHaveBeenCalledTimes(1);

    gate.resolve(tokenStatistics(BASE_TIME));
    await Promise.all([first, second]);
  });
});

describe("数据源映射表与可用性三态", () => {
  it("TOKEN_SOURCES 按固定顺序覆盖 5 个数据源，第 5 个是 JetBrains 插件", async () => {
    const page = await freshModule();

    expect(page.TOKEN_SOURCES.map((item) => item.key)).toEqual([
      "workbuddy",
      "workbuddy-ai",
      "codebuddy-cli",
      "codebuddy-ide",
      "codebuddy-jetbrains",
    ]);
    expect(page.TOKEN_SOURCES.map((item) => item.label)).toEqual([
      "WorkBuddy",
      "WorkBuddy 国际版",
      "CodeBuddy CLI",
      "CodeBuddy IDE / VS Code",
      "CodeBuddy 插件（JetBrains）",
    ]);
  });

  it("not-installed 的源置灰，ready 与 no-data 都可点", async () => {
    const page = await freshModule();
    const stats = statisticsWith([
      tokenSource("workbuddy", "ready", 12, 3),
      tokenSource("codebuddy-cli", "no-data", 0, 8),
      tokenSource("codebuddy-jetbrains", "not-installed"),
    ]);

    expect(page.sourceAvailability(stats, "codebuddy-jetbrains")).toBe("not-installed");
    expect(page.sourceAvailability(stats, "codebuddy-cli")).toBe("no-data");
    expect(page.sourceAvailability(stats, "workbuddy")).toBe("ready");
    // 统计还没到手时不得置灰任何 Tab；统计已到但源缺席（旧后端）按没装处理。
    expect(page.sourceAvailability(null, "codebuddy-jetbrains")).toBeNull();
    expect(page.sourceAvailability(statisticsWith([]), "codebuddy-jetbrains")).toBe("not-installed");
  });

  it("三态空态文案各命中一次且互不相同", async () => {
    const page = await freshModule();

    const notInstalled = page.emptySourceMessage(tokenSource("codebuddy-jetbrains", "not-installed"));
    const noData = page.emptySourceMessage(tokenSource("codebuddy-cli", "no-data", 0, 7));
    const readyEmpty = page.emptySourceMessage(tokenSource("workbuddy", "ready"));

    expect(notInstalled).toBe("本机未检测到 CodeBuddy 数据目录（可忽略此栏）。");
    expect(noData).toBe("已扫描 7 个会话文件，但没有可用的 usage。");
    expect(readyEmpty).toBe("尚未发现该来源的本地会话日志。");
  });
});
