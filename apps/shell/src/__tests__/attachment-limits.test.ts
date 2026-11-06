import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  formatBytes,
  limitLabels,
  limits,
  loadAttachmentLimits,
  resetAttachmentLimitsForTests,
} from "../attachmentLimits";

/**
 * 附件上限**只能来自后端**。
 *
 * 这里的核心手法：给 `loadAttachmentLimits` 喂一组**明显不是默认值**的数字
 * （7 MiB / 3 个 / 11 MiB），然后断言模块暴露出来的就是这组。
 * 如果哪天有人把上限又写回前端（比如 `const MAX = 18 * MIB`），
 * 这些用例立刻会红——这正是它存在的意义。
 *
 * 真后端那侧有一组对称的用例（`apps/shell/src-tauri/tests/attachment_limits.rs`），
 * 断言 `attachment_limits` 命令返回的就是 `xremail_core::attachment` 的三个常量。
 */

/** 一组刻意与真后端默认值不同的上限。 */
const ODD_LIMITS = {
  maxAttachmentBytes: 7 * 1024 * 1024,
  maxAttachmentsPerMessage: 3,
  maxTotalAttachmentBytes: 11 * 1024 * 1024,
};

beforeEach(() => {
  resetAttachmentLimitsForTests();
});

describe("附件上限来自后端", () => {
  it("拿到之前是 null（不给自己编一个默认值）", () => {
    expect(limits()).toBeNull();
  });

  it("读到的就是命令返回的那组数字", async () => {
    const invoke = vi.fn(async () => ODD_LIMITS);
    const loaded = await loadAttachmentLimits(invoke);

    expect(invoke).toHaveBeenCalledWith("attachment_limits");
    expect(loaded).toEqual(ODD_LIMITS);
    expect(limits()).toEqual(ODD_LIMITS);
    // 关键断言：界面上的数字跟着后端走，不是写死的那一套。
    expect(limits()?.maxAttachmentBytes).toBe(7 * 1024 * 1024);
    expect(limits()?.maxAttachmentsPerMessage).toBe(3);
    expect(limits()?.maxTotalAttachmentBytes).toBe(11 * 1024 * 1024);
  });

  it("后端换了数字，前端阈值也跟着换", async () => {
    await loadAttachmentLimits(async () => ODD_LIMITS);
    expect(limitLabels(limits()!).single).toBe("7.0 MB");

    resetAttachmentLimitsForTests();
    await loadAttachmentLimits(async () => ({
      maxAttachmentBytes: 2 * 1024 * 1024,
      maxAttachmentsPerMessage: 1,
      maxTotalAttachmentBytes: 3 * 1024 * 1024,
    }));
    expect(limitLabels(limits()!).single).toBe("2.0 MB");
    expect(limitLabels(limits()!).count).toBe("1 个");
    expect(limitLabels(limits()!).total).toBe("3.0 MB");
  });

  it("只问后端一次（之后走缓存）", async () => {
    const invoke = vi.fn(async () => ODD_LIMITS);
    await loadAttachmentLimits(invoke);
    await loadAttachmentLimits(invoke);
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("并发调用共用一次请求", async () => {
    const invoke = vi.fn(async () => ODD_LIMITS);
    const [first, second] = await Promise.all([
      loadAttachmentLimits(invoke),
      loadAttachmentLimits(invoke),
    ]);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(first).toEqual(second);
  });

  it("命令失败时返回 null，不把假数字塞进来", async () => {
    const loaded = await loadAttachmentLimits(async () => {
      throw new Error("IPC 没起来");
    });
    expect(loaded).toBeNull();
    expect(limits()).toBeNull();
  });

  it("返回形状不对时当作没拿到（不信任脏数据）", async () => {
    const loaded = await loadAttachmentLimits(async () => ({
      maxAttachmentBytes: 0,
      maxAttachmentsPerMessage: -1,
      maxTotalAttachmentBytes: 0,
    }));
    expect(loaded).toBeNull();
    expect(limits()).toBeNull();
  });
});

describe("上限的文字说法", () => {
  it("按 1024 进制、保留一位小数", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(2048)).toBe("2.0 KB");
    expect(formatBytes(7 * 1024 * 1024)).toBe("7.0 MB");
    expect(formatBytes(18 * 1024 * 1024)).toBe("18.0 MB");
  });

  it("三个标签都由同一组数字推导", () => {
    expect(limitLabels(ODD_LIMITS)).toEqual({
      single: "7.0 MB",
      count: "3 个",
      total: "11.0 MB",
    });
  });
});
