/**
 * 写信侧的附件上限。
 *
 * ## 唯一事实来源在后端
 *
 * 三个数字定义在 `crates/xremail-core/src/attachment.rs`
 * （`MAX_ATTACHMENT_BYTES` / `MAX_ATTACHMENTS_PER_MESSAGE` /
 * `MAX_TOTAL_ATTACHMENT_BYTES`），由 Tauri 命令 `attachment_limits` 原样转发。
 * **这个文件里不写任何数字**：它只是把命令的结果缓存下来，
 * 供写信面板同步读取（校验发生在 onChange 里，不能等异步）。
 *
 * 曾经的事故：前端写死 20 MiB、后端是 18 MiB，
 * 用户在前端能过、点发送才被拒——只在两边各写一份数字，早晚会分叉。
 *
 * 这条链路用两处用例闭合（改任一端都会红）：
 * `apps/shell/src-tauri/tests/attachment_limits.rs`（命令 == core 常量）与
 * `apps/shell/src/__tests__/attachment-limits.test.ts`（本文件 == 命令返回值，
 * 且喂一组**与真实值不同的**数字时阈值会跟着变）。
 * 「后端别把 18 改成 20」这件事本文件管不了，由
 * `crates/xremail-core/tests/limits_match_the_backend.rs` 钉住数值。
 *
 * ## 拿不到后端数字时怎么办
 *
 * 预热失败（比如在浏览器里跑、IPC 没起来）时 [`limits`] 返回 `null`，
 * 调用方**不要**自己兜一个默认值，而是把校验交给后端
 * （后端本来就会再校验一遍，前端这一道只是提前给个友好的错误）。
 */

/** 与 `xremail_shell_lib::cmd::AttachmentLimits` 一致（serde camelCase）。 */
export interface AttachmentLimits {
  /** 单个附件的字节上限。 */
  maxAttachmentBytes: number;
  /** 一封信的附件个数上限。 */
  maxAttachmentsPerMessage: number;
  /** 一封信的附件总字节上限。 */
  maxTotalAttachmentBytes: number;
}

let cached: AttachmentLimits | null = null;
let pending: Promise<AttachmentLimits | null> | null = null;

/** 只在这一处信任后端给的东西：字段必须是正整数，否则当作「没拿到」。 */
function isUsable(value: unknown): value is AttachmentLimits {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const one = value as Partial<AttachmentLimits>;
  return (
    Number.isInteger(one.maxAttachmentBytes) &&
    Number.isInteger(one.maxAttachmentsPerMessage) &&
    Number.isInteger(one.maxTotalAttachmentBytes) &&
    (one.maxAttachmentBytes ?? 0) > 0 &&
    (one.maxAttachmentsPerMessage ?? 0) > 0 &&
    (one.maxTotalAttachmentBytes ?? 0) > 0
  );
}

/**
 * 从后端读一次上限并缓存（应用启动时调用；重复调用共用一个 Promise）。
 *
 * 失败**不让应用崩**：返回 `null`，等下次再试。
 */
export function loadAttachmentLimits(
  invoke: (cmd: string) => Promise<unknown>,
): Promise<AttachmentLimits | null> {
  if (cached) {
    return Promise.resolve(cached);
  }
  if (pending) {
    return pending;
  }
  pending = invoke("attachment_limits")
    .then((value) => {
      if (isUsable(value)) {
        cached = value;
        return value;
      }
      return null;
    })
    .catch(() => null)
    .finally(() => {
      pending = null;
    });
  return pending;
}

/**
 * 已经拿到的上限；没拿到就是 `null`（调用方据此决定是否跳过前置校验）。
 */
export function limits(): AttachmentLimits | null {
  return cached;
}

/** 测试用：清掉缓存，让下一次 [`loadAttachmentLimits`] 真的去问后端。 */
export function resetAttachmentLimitsForTests(): void {
  cached = null;
  pending = null;
}

/** 字节数 → 人读的大小，按 1024 进制、保留一位小数。 */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * 上限的人类可读说法（错误信息与提示文字用）。
 *
 * **从后端数字推导**，不再手写「10 MiB」「18 MiB」这些字面量：
 * 后端改了上限，界面上的说法跟着变。
 */
export function limitLabels(value: AttachmentLimits): {
  single: string;
  count: string;
  total: string;
} {
  return {
    single: formatBytes(value.maxAttachmentBytes),
    count: `${value.maxAttachmentsPerMessage} 个`,
    total: formatBytes(value.maxTotalAttachmentBytes),
  };
}
