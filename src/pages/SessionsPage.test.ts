import { describe, expect, it } from "vitest";

import { blockedReason, emptyNotice } from "./SessionsPage";

/** 改动前的空态文案：WorkBuddy 零回归与旧后端回退都必须与它逐字一致。 */
const OLD_TITLE = "还没有关联会话组";
const OLD_TAIL = "从账号切换时复制会话后，关联组会显示在这里。";

describe("emptyNotice", () => {
  it("数据根就绪但没建过关联组时沿用旧文案", () => {
    const notice = emptyNotice({
      client: "codebuddyIde",
      dataRootState: "ready",
      storeStatus: "missing",
      scope: "cn",
    });

    expect(notice).toEqual({ title: OLD_TITLE, description: OLD_TAIL });
  });

  it("数据根缺失时改说没有找到数据目录，并说明下一步该做什么", () => {
    const notice = emptyNotice({
      client: "codebuddyIde",
      dataRootState: "missing",
      storeStatus: "missing",
      scope: "cn",
    });

    expect(notice.title).toBe("没有找到 CodeBuddy IDE 的数据目录");
    expect(notice.description).toContain("装好并登录一次后回到本页刷新即可");
  });

  it("WorkBuddy 的 notApplicable 与改动前文案逐字一致", () => {
    const notice = emptyNotice({
      client: "workbuddy",
      dataRootState: "notApplicable",
      storeStatus: "missing",
      scope: undefined,
    });

    expect(notice).toEqual({ title: OLD_TITLE, description: OLD_TAIL });
  });

  it("旧后端不返回该字段时回退旧文案，不出现未安装也不出现 undefined", () => {
    const notice = emptyNotice({
      client: "vscodeExt",
      dataRootState: undefined,
      storeStatus: "missing",
      scope: undefined,
    });

    expect(notice).toEqual({ title: OLD_TITLE, description: OLD_TAIL });
  });

  it("数据根就绪、存储可读但当前档位零组时追加换档提示", () => {
    const notice = emptyNotice({
      client: "codebuddyIde",
      dataRootState: "ready",
      storeStatus: "ready",
      scope: "cn",
    });

    expect(notice.description).toBe(`${OLD_TAIL}这个档位还没有关联会话组，换个档位看看。`);
  });

  it("WorkBuddy 没有档位概念，即使存储可读也不追加换档提示", () => {
    const notice = emptyNotice({
      client: "workbuddy",
      dataRootState: "notApplicable",
      storeStatus: "ready",
      scope: undefined,
    });

    expect(notice.description).toBe(OLD_TAIL);
  });
});

describe("blockedReason", () => {
  it("插件数据根缺失时提前禁用并说明先做什么", () => {
    expect(blockedReason("vscodeExt", "missing")).toBe(
      "没有找到 CodeBuddy 插件的数据目录：请先在 VS Code 里安装插件并登录使用一次",
    );
  });

  it("能力不足的客户端报能力原因，与未安装无关", () => {
    expect(blockedReason("codebuddyIde", "missing")).toBe("当前客户端暂不支持新增关联会话");
    expect(blockedReason("codebuddyIde", undefined)).toBe("当前客户端暂不支持新增关联会话");
  });

  it("数据根就绪、旧后端缺字段、或 WorkBuddy 都不得禁用入口", () => {
    expect(blockedReason("vscodeExt", "ready")).toBeNull();
    expect(blockedReason("vscodeExt", undefined)).toBeNull();
    expect(blockedReason("workbuddy", "notApplicable")).toBeNull();
  });
});
