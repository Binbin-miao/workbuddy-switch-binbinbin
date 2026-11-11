import { describe, expect, it } from "vitest";

import type { AccountMeta, MirrorPreview, MemoryMergeReport } from "@/lib/types";
import {
  assetSyncSuppressedByMirror,
  canSubmitMirror,
  canSubmitMemory,
  DEFAULT_MEMORY_MODE,
  initialMirrorSelection,
  initialMemorySelection,
  memoryAssetLabel,
  memorySummaryWarning,
  memoryToasts,
  mirrorSubmitBlockReason,
  resolveMemorySources,
  showsSourcePicker,
  toMirrorSwitchArgs,
  toMemorySwitchArgs,
  type MemoryMode,
} from "./switch-memory-mode";

// ---------------------------------------------------------------------------
// 构造测试数据：只填决策真正用到的字段，其余补齐 AccountMeta / MemoryMergeReport 必填项。
// ---------------------------------------------------------------------------

function account(p: Partial<AccountMeta> & Pick<AccountMeta, "id">): AccountMeta {
  return {
    uid: null,
    email: null,
    nickname: null,
    enterpriseName: null,
    expiresAt: null,
    refreshExpiresAt: null,
    refreshedAt: null,
    createdAt: null,
    needsRelogin: false,
    needsReloginReason: null,
    ...p,
  };
}

function report(p: Partial<MemoryMergeReport> = {}): MemoryMergeReport {
  return {
    accounts: 2,
    sources: [
      { label: "A", chars: 100 },
      { label: "B", chars: 200 },
    ],
    mergedChars: 300,
    written: [{ label: "A", backup: "MEMORY.md.bak-1", hadContent: true }],
    unchanged: [],
    persona: { written: [], unchanged: [], errors: [] },
    errors: [],
    mode: "merge",
    ...p,
  };
}

describe("默认与打开重置 — 破坏性项永不默认选中", () => {
  it("DEFAULT_MEMORY_MODE 恒为 off", () => {
    expect(DEFAULT_MEMORY_MODE).toBe("off");
    expect(showsSourcePicker(DEFAULT_MEMORY_MODE)).toBe(false);
    expect(memoryAssetLabel(DEFAULT_MEMORY_MODE)).toBeNull();
    expect(memorySummaryWarning(DEFAULT_MEMORY_MODE)).toBeNull();
    expect(toMemorySwitchArgs(DEFAULT_MEMORY_MODE, "u1")).toEqual({});
  });

  it("initialMemorySelection 每次调用都复位到「不同步 + 跟随默认母本」", () => {
    const first = initialMemorySelection();
    const second = initialMemorySelection();
    expect(first).toEqual({ mode: "off", sourceUid: null });
    expect(second).toEqual(first);
    // 不共享引用：改一次不影响下次打开（防状态残留）
    first.mode = "overwrite";
    expect(initialMemorySelection().mode).toBe("off");
  });
});

describe("toMemorySwitchArgs — mode → 提交参数（契约：不传 = 不同步）", () => {
  it("不同步：两个字段都不下发，等于旧版「不勾选」", () => {
    expect(toMemorySwitchArgs("off", "u1")).toEqual({});
  });

  it("合并：只下发 memoryMode，不带母本", () => {
    expect(toMemorySwitchArgs("merge", "")).toEqual({ memoryMode: "merge" });
  });

  it("合并：即便残留母本 uid 也不下发（防脏参数）", () => {
    expect(toMemorySwitchArgs("merge", "u1")).toEqual({ memoryMode: "merge" });
  });

  it("覆盖：下发 mode 与母本 uid", () => {
    expect(toMemorySwitchArgs("overwrite", "u1")).toEqual({
      memoryMode: "overwrite",
      memorySourceUid: "u1",
    });
  });

  it("删除旧 mergeMemory：返回值永不出现该键", () => {
    for (const mode of ["off", "merge", "overwrite"] as MemoryMode[]) {
      expect(toMemorySwitchArgs(mode, "u1")).not.toHaveProperty("mergeMemory");
    }
  });

  it.each(["", "   "])("覆盖但母本为空 %j：不发破坏性请求，退化为不同步", (uid) => {
    expect(toMemorySwitchArgs("overwrite", uid)).toEqual({});
  });
});

describe("canSubmitMemory — 覆盖无母本时拦截提交", () => {
  it("不同步 / 合并无需母本，始终可提交", () => {
    expect(canSubmitMemory("off", "")).toBe(true);
    expect(canSubmitMemory("merge", "")).toBe(true);
  });

  it("覆盖 + 有效母本可提交", () => {
    expect(canSubmitMemory("overwrite", "u1")).toBe(true);
  });

  it.each(["", "   ", "\t"])("覆盖 + 空母本 %j 拒绝提交", (uid) => {
    expect(canSubmitMemory("overwrite", uid)).toBe(false);
  });
});

describe("showsSourcePicker / memoryAssetLabel / memorySummaryWarning — 单一 mode 驱动 UI 决策", () => {
  it("只有覆盖显示母本下拉", () => {
    expect(showsSourcePicker("off")).toBe(false);
    expect(showsSourcePicker("merge")).toBe(false);
    expect(showsSourcePicker("overwrite")).toBe(true);
  });

  it("摘要资产项：不同步不计入徽标，合并 / 覆盖都计入", () => {
    expect(memoryAssetLabel("off")).toBeNull();
    expect(memoryAssetLabel("merge")).toBe("工作记忆");
    expect(memoryAssetLabel("overwrite")).toBe("工作记忆（覆盖）");
  });

  it("覆盖模式给出醒目摘要警示，其余不打扰", () => {
    expect(memorySummaryWarning("overwrite")).toMatch(/替换/);
    expect(memorySummaryWarning("overwrite")).toMatch(/备份/);
    expect(memorySummaryWarning("off")).toBeNull();
    expect(memorySummaryWarning("merge")).toBeNull();
  });
});

describe("resolveMemorySources — 账号库 → 母本选项 + 默认选中 + 是否需确认", () => {
  const library = [
    account({ id: "id-1", uid: "u1", nickname: "一号", email: "one@x.com" }),
    account({ id: "id-2", uid: "u2", nickname: "二号", email: "two@x.com" }),
    account({ id: "id-3", uid: "u3", nickname: "三号", email: "three@x.com" }),
  ];

  it("无账号：无选项、无默认、需确认（界面禁用提交）", () => {
    expect(resolveMemorySources([], ["u1"])).toEqual({
      options: [],
      defaultUid: null,
      needsConfirm: true,
    });
  });

  it("账号全部缺 uid：core 无法定位目录，一律不作为母本", () => {
    const noUid = [account({ id: "id-x", nickname: "没 uid" })];
    expect(resolveMemorySources(noUid, ["id-x"])).toEqual({
      options: [],
      defaultUid: null,
      needsConfirm: true,
    });
  });

  it("单账号 + 当前 uid 命中：默认选中且标记「当前登录」", () => {
    const single = [library[0]];
    const r = resolveMemorySources(single, ["u1"]);
    expect(r.defaultUid).toBe("u1");
    expect(r.needsConfirm).toBe(false);
    expect(r.options).toEqual([{ uid: "u1", label: "一号", isCurrent: true }]);
  });

  it("多账号：当前 uid 命中第三个就默认第三个（不猜第一个）", () => {
    const r = resolveMemorySources(library, ["u3"]);
    expect(r.defaultUid).toBe("u3");
    expect(r.needsConfirm).toBe(false);
    expect(r.options.find((o) => o.uid === "u3")?.isCurrent).toBe(true);
    expect(r.options.find((o) => o.uid === "u1")?.isCurrent).toBe(false);
  });

  it("当前账号用 id 或 email 也能匹配上（对齐账号页 isWorkbuddyCurrent 口径）", () => {
    expect(resolveMemorySources(library, ["id-2"]).defaultUid).toBe("u2");
    expect(resolveMemorySources(library, ["two@x.com"]).defaultUid).toBe("u2");
  });

  it("多个候选标识按优先级：命中即停，不落到后面的 sessionUid", () => {
    const r = resolveMemorySources(library, ["u1", "u2"]);
    expect(r.defaultUid).toBe("u1");
    expect(r.needsConfirm).toBe(false);
  });

  it("首个候选为空/null 时继续用下一个（会话接口异步回来兜底）", () => {
    const r = resolveMemorySources(library, [null, "", "u2"]);
    expect(r.defaultUid).toBe("u2");
    expect(r.needsConfirm).toBe(false);
  });

  it("无法确定当前账号：默认列表第一项 + 需确认，且不得标任何项为「当前登录」", () => {
    const r = resolveMemorySources(library, [null, "查无此号"]);
    expect(r.defaultUid).toBe("u1");
    expect(r.needsConfirm).toBe(true);
    expect(r.options.every((o) => !o.isCurrent)).toBe(true);
  });

  it("匹配上的账号若无 uid，跳过它继续找，不误选", () => {
    const mixed = [
      account({ id: "cur", uid: null, nickname: "当前但缺 uid" }),
      library[1],
    ];
    const r = resolveMemorySources(mixed, [null, "cur"]);
    // 「cur」命中了无 uid 账号，不能作为母本；无其它线索 → 退到第一项有效 uid 并需确认
    expect(r.defaultUid).toBe("u2");
    expect(r.needsConfirm).toBe(true);
  });

  it("label 走 account-display 口径：displayField=note 时取备注", () => {
    const withNote = account({
      id: "id-n",
      uid: "un",
      nickname: "昵称",
      note: "我的备注",
      displayField: "note",
    });
    expect(resolveMemorySources([withNote], ["un"]).options[0].label).toBe("我的备注");
  });

  it("label 回退：选定字段为空时回退 nickname → uid", () => {
    const fallback = account({
      id: "id-f",
      uid: "uf",
      nickname: null,
      displayField: "phone",
    });
    expect(resolveMemorySources([fallback], ["uf"]).options[0].label).toBe("uf");
  });

  it("选项顺序与账号库一致，且过滤无 uid 项", () => {
    const mixed = [library[2], account({ id: "no", nickname: "无 uid" }), library[0]];
    expect(resolveMemorySources(mixed, [null]).options.map((o) => o.uid)).toEqual(["u3", "u1"]);
  });
});

describe("memoryToasts — 报告 → toast 按请求 mode 分文案", () => {
  it("不同步：不弹任何记忆 toast", () => {
    expect(memoryToasts("off", report())).toEqual([]);
    expect(memoryToasts("off", undefined)).toEqual([]);
  });

  it("合并 + 无报告：沿用现有「工作记忆未合并」警告", () => {
    const t = memoryToasts("merge", undefined);
    expect(t).toHaveLength(1);
    expect(t[0].level).toBe("warning");
    expect(t[0].title).toBe("工作记忆未合并");
    expect(t[0].description).toMatch(/没有收到合并结果/);
  });

  it("覆盖 + 无报告：改为「工作记忆未覆盖」警告", () => {
    const t = memoryToasts("overwrite", undefined);
    expect(t).toHaveLength(1);
    expect(t[0].level).toBe("warning");
    expect(t[0].title).toBe("工作记忆未覆盖");
    expect(t[0].description).toMatch(/没有收到覆盖结果/);
  });

  it("合并成功：沿用「N 份记忆合成 X 字」文案", () => {
    const t = memoryToasts("merge", report({ written: [{ label: "A", backup: "b", hadContent: true }, { label: "B", backup: "b", hadContent: true }] }));
    expect(t).toHaveLength(1);
    expect(t[0].level).toBe("success");
    expect(t[0].title).toBe("工作记忆已合并并写入 2 个账号");
    expect(t[0].description).toBe("2 份记忆合成 300 字；被覆盖的原文已备份为 MEMORY.md.bak-时间戳");
  });

  it("覆盖成功：用报告 sources 的母本名 + 被覆盖账号数 + 备份提示", () => {
    const r = report({
      mode: "overwrite",
      sourceUid: "u9",
      sources: [{ label: "小明的账号", chars: 200 }],
      mergedChars: 200,
      written: [
        { label: "A", backup: "b", hadContent: true },
        { label: "B", backup: "b", hadContent: true },
        { label: "C", backup: "b", hadContent: true },
      ],
    });
    const t = memoryToasts("overwrite", r);
    expect(t).toHaveLength(1);
    expect(t[0].level).toBe("success");
    expect(t[0].title).toBe("已把「小明的账号」的记忆复制到 3 个账号");
    expect(t[0].description).toMatch(/MEMORY\.md\.bak-时间戳/);
    expect(t[0].description).toMatch(/还原/);
  });

  it("覆盖成功但母本名缺失：降级为「母本账号」不显示 undefined", () => {
    const r = report({ mode: "overwrite", sources: [], written: [{ label: "A", backup: "b", hadContent: true }] });
    expect(memoryToasts("overwrite", r)[0].title).toBe("已把「母本账号」的记忆复制到 1 个账号");
  });

  it("报告带 errors：合并 / 覆盖分别改动标题，且只报 error 不伪装成功", () => {
    const r = report({
      written: [],
      errors: [{ label: "A", error: "读不到" }],
      persona: { written: [], unchanged: [], errors: [{ label: "B", file: "SOUL.md", error: "写失败" }] },
    });
    const merge = memoryToasts("merge", r);
    const overwrite = memoryToasts("overwrite", r);
    expect(merge).toHaveLength(1);
    expect(merge[0].level).toBe("error");
    expect(merge[0].title).toBe("工作记忆没有全部合并");
    expect(overwrite[0].title).toBe("工作记忆没有全部覆盖");
    expect(merge[0].description).toContain("A：读不到");
    expect(merge[0].description).toContain("B / SOUL.md：写失败");
  });

  it("errors 与 written 同时存在：error 与 success 都弹，分级不丢", () => {
    const r = report({ errors: [{ label: "A", error: "写失败" }] });
    expect(memoryToasts("merge", r).map((t) => t.level)).toEqual(["error", "success"]);
  });

  it("无写入 + note：info 提示按 mode 分标题", () => {
    const r = report({ written: [], note: "没有任何记忆文件" });
    expect(memoryToasts("merge", r)[0]).toMatchObject({ level: "info", title: "工作记忆未合并", description: "没有任何记忆文件" });
    expect(memoryToasts("overwrite", r)[0].title).toBe("工作记忆未覆盖");
  });

  it("无写入 + unchanged：info「工作记忆无需更新」", () => {
    const r = report({ written: [], unchanged: [{ label: "A" }] });
    expect(memoryToasts("overwrite", r)[0]).toMatchObject({ level: "info", title: "工作记忆无需更新" });
  });

  it("报告里的 mode 字段不覆盖请求 mode（老宿主/异常回填时仍按用户所选出文案）", () => {
    const r = report({ mode: "overwrite", sources: [{ label: "母本", chars: 1 }] });
    expect(memoryToasts("merge", r)[0].title).toMatch(/已合并/);
  });
});

// ---------------------------------------------------------------------------
// 镜像切换：与工作记忆三选一完全独立的显式通道
// ---------------------------------------------------------------------------

function mirrorPreviewFixture(motherUid: string): MirrorPreview {
  return {
    motherUid,
    motherLabel: "母本账号",
    previewToken: "tok-delete-1",
    expectedDigest: "digest-1",
    targets: [{ sessionId: "s-1", title: "将被软删的会话", userId: "u-target", groupId: "g-1", updatedAt: 1000, fullDigest: "d1", kind: "session" }],
    sessionCount: 1,
    copiedCount: 0,
    expertsPending: false,
    expertsNames: [],
    expertsNote: null,
    backupDir: null,
  };
}

describe("镜像切换 — 母本 uid 由登录态确定，defaultUid 禁止代入", () => {
  it("initialMirrorSelection 恒为「未绑定母本 + 未启用 + 未确认」，与记忆母本默认值无关", () => {
    const selection = initialMirrorSelection();
    expect(selection).toEqual({
      motherUid: null,
      acknowledged: false,
      confirmed: false,
      enabled: false,
    });
    // 重新打开必须拿到同样的干净初值（不共享引用）。
    selection.motherUid = "u1";
    expect(initialMirrorSelection().motherUid).toBeNull();
  });

  it("记忆母本的 defaultUid 存在时，镜像切换仍不可提交（绝不隐式代入）", () => {
    const sources = resolveMemorySources(
      [account({ id: "id-1", uid: "u1", nickname: "一号" })],
      ["u1"],
    );
    expect(sources.defaultUid).toBe("u1");
    const preview = mirrorPreviewFixture("u1");
    expect(canSubmitMirror(initialMirrorSelection(), preview)).toBe(false);
    expect(toMirrorSwitchArgs(initialMirrorSelection(), preview)).toEqual({});
  });

  it("未启用镜像时，母本、备份确认与二次确认齐全也不可提交", () => {
    const preview = mirrorPreviewFixture("u2");
    const base = initialMirrorSelection();
    expect(canSubmitMirror({ ...base, motherUid: "u2" }, preview)).toBe(false);
    expect(canSubmitMirror({ ...base, motherUid: "u2", acknowledged: true }, preview)).toBe(false);
    expect(
      canSubmitMirror({ ...base, motherUid: "u2", acknowledged: true, confirmed: true }, preview),
    ).toBe(false);
    // 显式启用后，同一组条件才放行。
    expect(
      canSubmitMirror(
        { ...base, motherUid: "u2", acknowledged: true, confirmed: true, enabled: true },
        preview,
      ),
    ).toBe(true);
  });

  it("预览母本与所选母本 uid 不一致（uid 变化）一律拒绝，绝不复用旧凭证", () => {
    const selection = { motherUid: "u2", acknowledged: true, confirmed: true, enabled: true };
    expect(canSubmitMirror(selection, mirrorPreviewFixture("u1"))).toBe(false);
    expect(toMirrorSwitchArgs(selection, mirrorPreviewFixture("u1"))).toEqual({});
  });

  it("没有预览 / 空清单 / 无凭证时不提交", () => {
    const selection = { motherUid: "u2", acknowledged: true, confirmed: true, enabled: true };
    expect(canSubmitMirror(selection, null)).toBe(false);
    const empty = { ...mirrorPreviewFixture("u2"), targets: [], previewToken: "" };
    expect(canSubmitMirror(selection, empty)).toBe(false);
  });

  it("toMirrorSwitchArgs 只输出冻结的三字段 plan（不含 motherUid），不夹带别的通道", () => {
    const preview = mirrorPreviewFixture("u2");
    const args = toMirrorSwitchArgs(
      { motherUid: "u2", acknowledged: true, confirmed: true, enabled: true },
      preview,
    );
    // 母本由后端取自登录态：前端只输出 targets/expectedDigest/previewToken。
    expect(args.mirrorPlan).toEqual({
      targets: preview.targets,
      expectedDigest: "digest-1",
      previewToken: "tok-delete-1",
    });
    expect(args.mirrorPlan).not.toHaveProperty("motherUid");
    expect(args).not.toHaveProperty("syncSelections");
    expect(args).not.toHaveProperty("memoryMode");
    expect(args).not.toHaveProperty("memorySourceUid");
  });

  it("镜像启用后附加同步三项与一键全选全部置灰；未启用时全部放行", () => {
    const base = initialMirrorSelection();
    expect(assetSyncSuppressedByMirror(base)).toEqual({
      memory: false,
      persona: false,
      experts: false,
      sessionSelectAll: false,
    });
    expect(assetSyncSuppressedByMirror({ ...base, enabled: true })).toEqual({
      memory: true,
      persona: true,
      experts: true,
      sessionSelectAll: true,
    });
  });
});

/**
 * 软删 0 条不是错误状态：目标账号的会话若全在含母本的关联组内，后端算出的 targets
 * 就是 0，但此时仍可能要复制母本独有会话、覆盖 MEMORY.md。
 * 本机实测 2c41d2bd / 3b0de806 两账号正是这种状态——旧逻辑把按钮永久锁死。
 */
describe("镜像切换 — 软删 0 条必须可执行（曾因 targets>0 前置导致按钮永久禁用）", () => {
  const ready = {
    motherUid: "u2",
    acknowledged: true,
    confirmed: true,
    enabled: true,
  };

  function previewWith(targets: MirrorPreview["targets"], copiedCount: number): MirrorPreview {
    return {
      ...mirrorPreviewFixture("u2"),
      targets,
      sessionCount: targets.length,
      copiedCount,
    };
  }

  it("待软删 0 条但仍有会话要复制：可提交，且能产出 mirrorPlan", () => {
    const preview = previewWith([], 13);
    expect(mirrorSubmitBlockReason(ready, preview)).toBeNull();
    expect(canSubmitMirror(ready, preview)).toBe(true);
    expect(toMirrorSwitchArgs(ready, preview).mirrorPlan).toBeDefined();
  });

  it("软删与复制同时为 0 也不拦：前端看不到 MEMORY.md 是否已一致，交后端 no-op 判定", () => {
    const preview = previewWith([], 0);
    // 回归锁：曾把 0/0 当成「已与母本一致」直接禁用按钮，那是假陈述——
    // 目标账号的 MEMORY.md 可能与母本不同，后端仍会执行覆盖。
    expect(mirrorSubmitBlockReason(ready, preview)).toBeNull();
    expect(canSubmitMirror(ready, preview)).toBe(true);
  });

  it("阻塞原因逐条可读：未启用 / 无母本 / 未预览 / 空凭证 / 母本变化 / 未勾备份", () => {
    const preview = previewWith(mirrorPreviewFixture("u2").targets, 0);
    expect(mirrorSubmitBlockReason({ ...ready, enabled: false }, preview)).toContain(
      "启用镜像切换",
    );
    expect(mirrorSubmitBlockReason({ ...ready, motherUid: null }, preview)).toContain("登录账号");
    expect(mirrorSubmitBlockReason(ready, null)).toContain("预览");
    expect(mirrorSubmitBlockReason(ready, { ...preview, previewToken: "   " })).toContain(
      "预览已失效",
    );
    expect(mirrorSubmitBlockReason(ready, mirrorPreviewFixture("u1"))).toContain(
      "登录账号已变化",
    );
    expect(mirrorSubmitBlockReason({ ...ready, acknowledged: false }, preview)).toContain(
      "我已知会先备份",
    );
  });

  it("条件齐备时原因为 null，但未二次确认仍不可提交", () => {
    const preview = previewWith(mirrorPreviewFixture("u2").targets, 0);
    expect(mirrorSubmitBlockReason(ready, preview)).toBeNull();
    expect(canSubmitMirror({ ...ready, confirmed: false }, preview)).toBe(false);
  });
});
