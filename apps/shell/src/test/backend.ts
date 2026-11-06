import { open } from "@tauri-apps/plugin-dialog";
import { vi } from "vitest";
import { FLAG_FLAGGED, FLAG_SEEN } from "../types";
import type {
  Account,
  DkimConfig,
  DraftDetail,
  DraftDto,
  DraftInput,
  Folder,
  Message,
  MessageBody,
  SendResult,
} from "../types";
import type { SearchHit } from "../components/SearchPanel";
import type { InvokeArgs, InvokeImpl } from "./tauriState";

/**
 * 一个假的 Tauri 后端。
 *
 * 前端的所有写操作都走 `invoke`；这里按命令名分发，行为尽量贴近
 * Rust 侧真正的语义（尤其是 `set_message_flag` 会改本地账本，
 * 之后 `list_messages` 会把改过的位返回来）。
 */

export interface InvokeCall {
  cmd: string;
  args: InvokeArgs;
}

export interface BackendOptions {
  accounts?: Account[];
  folders?: Folder[];
  messages?: Message[];
  /** `set_message_flag` 抛这个错（测「回写失败」）。 */
  flagError?: Error | null;
  /** `forget_credential` 抛这个错。 */
  forgetError?: Error | null;
  /** `save_credential` 抛这个错。 */
  saveError?: Error | null;
  /** `sync_account` 抛这个错。 */
  syncError?: Error | null;
  /** `send_message` 返回的状态（默认 `sent`）。 */
  sendState?: SendResult["state"];
  /** `send_message` 返回的说明。 */
  sendDetail?: string | null;
  /** `send_message` 抛这个错。 */
  sendError?: Error | null;
  /**
   * `resend_uncertain` 返回的状态。
   *
   * 默认跟着 `sendState` 走（再提交一次的结果通常和刚才一样）；
   * 想在「再发一次」之后换成别的结果，单独设它。
   */
  resendState?: SendResult["state"];
  /** `resend_uncertain` 返回的说明。 */
  resendDetail?: string | null;
  /** `resend_uncertain` 抛这个错。 */
  resendError?: Error | null;
  /** `save_draft` 抛这个错。 */
  saveDraftError?: Error | null;
  /**
   * 假后端「读文件」得到的文件大小（按绝对路径查）。
   *
   * 前端选文件时只拿得到路径，所以 `size` 传 0；真实大小由后端读文件后回填。
   * 没登记的路径用 `DEFAULT_STUB_ATTACHMENT_SIZE` 顶替。
   */
  fileSizes?: Record<string, number>;
  /**
   * `search_messages` 返回的命中。
   *
   * 不做真的全文检索（那是后端 FTS5 的事），
   * 这里只按 query 做子串过滤，够前端测试用。
   */
  searchHits?: SearchHit[];
  /**
   * `open_message` 返回的正文。
   *
   * 默认是「纯文本正文、没有 HTML」；
   * 测 HTML 渲染路径时传一个带 `htmlSafe` 的。
   */
  messageBody?: MessageBody;
  /** get_dkim_config 抛这个错。 */
  dkimError?: Error | null;
  /** set_dkim_config 抛这个错。 */
  dkimSaveError?: Error | null;
}

export interface Backend {
  accounts: Account[];
  folders: Folder[];
  messages: Message[];
  flagError: Error | null;
  forgetError: Error | null;
  saveError: Error | null;
  syncError: Error | null;
  /** `send_message` 返回的状态。 */
  sendState: SendResult["state"];
  /** `send_message` 返回的说明。 */
  sendDetail: string | null;
  /** `send_message` 抛这个错。 */
  sendError: Error | null;
  /** `resend_uncertain` 返回的状态。 */
  resendState: SendResult["state"];
  /** `resend_uncertain` 返回的说明。 */
  resendDetail: string | null;
  /** `resend_uncertain` 抛这个错。 */
  resendError: Error | null;
  /** `save_draft` 抛这个错。 */
  saveDraftError: Error | null;
  /** 已经存过的草稿（按 `list_drafts` 的口径存着）。 */
  drafts: DraftDto[];
  /** 草稿全文（`get_draft` 的返回），按 id 存。 */
  draftDetails: Map<string, DraftDetail>;
  /** 每次 `send_message` 收到的草稿。 */
  sentDrafts: DraftInput[];
  /** 发出去的条目（`withdraw_draft` 按 entryId 找它们）。 */
  sentEntries: SentEntry[];
  /** 假后端「读文件」得到的文件大小。 */
  fileSizes: Record<string, number>;
  /** `search_messages` 备好的命中。 */
  searchHits: SearchHit[];
  /** `open_message` 返回的正文。 */
  messageBody: MessageBody;
  /** `get_dkim_config` 抛这个错。 */
  dkimError: Error | null;
  /** `set_dkim_config` 抛这个错。 */
  dkimSaveError: Error | null;
  /** 每个账户的 DKIM 配置（**不含私钥**，像真的 `account_dkim` 表）。 */
  dkimConfigs: Map<string, DkimConfig>;
  /** 所有 invoke 调用（含参数），按顺序。 */
  calls: InvokeCall[];
  /** 装给 `invoke` 的实现。 */
  impl: InvokeImpl;
  /** 某个命令的全部调用。 */
  callsTo(cmd: string): InvokeCall[];
  /** 某个命令的最后一次调用。 */
  lastCallTo(cmd: string): InvokeCall | undefined;
}

/** 一条发出去的信（`withdraw_draft` 能把它撤回来）。 */
export interface SentEntry {
  entryId: string;
  draft: DraftInput;
  state: SendResult["state"];
}

/** 单个附件上限（10 MiB）。 */
const MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024;
/** 一封信的附件个数上限。 */
const MAX_ATTACHMENTS = 5;
/** 一封信的附件总大小上限（20 MiB）。 */
const MAX_TOTAL_ATTACHMENT_BYTES = 20 * 1024 * 1024;
/** 没登记大小的附件，假后端统一报这个大小。 */
const DEFAULT_STUB_ATTACHMENT_SIZE = 2048;

/** 扩展名 → MIME（只覆盖常见几种，够测试用）。 */
const MIME_BY_EXTENSION: Record<string, string> = {
  txt: "text/plain",
  md: "text/markdown",
  html: "text/html",
  json: "application/json",
  png: "image/png",
  jpg: "image/jpeg",
  pdf: "application/pdf",
};

/** 按扩展名猜 MIME（真实后端也会这么兜底）。 */
function guessMime(path: string): string {
  const base = path.split(/[\\/]/).pop() ?? path;
  const dot = base.lastIndexOf(".");
  if (dot <= 0 || dot === base.length - 1) {
    return "";
  }
  return MIME_BY_EXTENSION[base.slice(dot + 1).toLowerCase()] ?? "";
}

/**
 * 让「选择附件」的文件选择框返回这些路径。
 *
 * Tauri 的文件选择框是原生插件，单元测试里拉不起来——
 * 所以在 `test/setup.ts` 里把它 mock 掉了，这里只是往里塞返回值。
 */
export function fakeFilePicker(paths: string[]): void {
  vi.mocked(open).mockImplementation((async () => paths) as never);
}

/** `send_message` 各状态对应的默认说明。 */
const DEFAULT_SEND_DETAIL: Record<SendResult["state"], string> = {
  sent: "服务器已确认接受",
  uncertain: "连接在等待服务器回应时中断",
  failed: "服务器明确拒绝了这封信",
};

export function createBackend(options: BackendOptions = {}): Backend {
  const backend: Backend = {
    accounts: options.accounts ?? [],
    folders: options.folders ?? [],
    messages: options.messages ?? [],
    flagError: options.flagError ?? null,
    forgetError: options.forgetError ?? null,
    saveError: options.saveError ?? null,
    syncError: options.syncError ?? null,
    sendState: options.sendState ?? "sent",
    sendDetail: options.sendDetail ?? null,
    sendError: options.sendError ?? null,
    resendState: options.resendState ?? options.sendState ?? "sent",
    resendDetail: options.resendDetail ?? null,
    resendError: options.resendError ?? null,
    saveDraftError: options.saveDraftError ?? null,
    drafts: [],
    draftDetails: new Map<string, DraftDetail>(),
    sentDrafts: [],
    sentEntries: [],
    fileSizes: { ...(options.fileSizes ?? {}) },
    searchHits: options.searchHits ?? [],
    dkimError: options.dkimError ?? null,
    dkimSaveError: options.dkimSaveError ?? null,
    dkimConfigs: new Map<string, DkimConfig>(),
    messageBody: options.messageBody ?? {
      messageId: "<m1@example.com>",
      textPlain: "正文内容",
      htmlSafe: null,
      charset: null,
    },
    calls: [],
    impl: async () => undefined,
    callsTo(cmd: string): InvokeCall[] {
      return backend.calls.filter((call) => call.cmd === cmd);
    },
    lastCallTo(cmd: string): InvokeCall | undefined {
      const all = backend.callsTo(cmd);
      return all[all.length - 1];
    },
  };

  // 草稿 id 的自增计数器（模拟后端分配的主键）。
  let draftCounter = 0;

  const handlers: Record<string, (args: InvokeArgs) => unknown> = {
    list_accounts: () => backend.accounts.map((account) => ({ ...account })),
    app_info: () => ({
      name: "Xremail",
      version: "0.1.0",
      platform: "test",
      dataDir: null,
    }),
    list_folders: () => backend.folders.map((folder) => ({ ...folder })),
    // 每次都返回**新的对象**：后端返回新快照，前端才真的会重渲染。
    list_messages: () => backend.messages.map((message) => ({ ...message })),
    search_messages: (args) => {
      const query = String(args.query ?? "").trim().toLowerCase();
      if (query === "") {
        return [];
      }
      const limit = Number(args.limit ?? 50) || 50;
      return backend.searchHits
        .filter((hit) =>
          [hit.subject, hit.from, hit.snippet]
            .join(" ")
            .toLowerCase()
            .includes(query),
        )
        .slice(0, limit)
        .map((hit) => ({ ...hit }));
    },
    set_message_flag: (args) => {
      if (backend.flagError) {
        throw backend.flagError;
      }
      const uid = Number(args.uid);
      const message = backend.messages.find((m) => m.uid === uid);
      if (!message) {
        throw new Error(`没有 uid=${uid} 的邮件`);
      }
      const bit = args.flag === "flagged" ? FLAG_FLAGGED : FLAG_SEEN;
      message.flags =
        args.value === true ? message.flags | bit : message.flags & ~bit;
      return {
        queued: false,
        applied: true,
        state: "succeeded",
        message: null,
      };
    },
    forget_credential: () => {
      if (backend.forgetError) {
        throw backend.forgetError;
      }
      return true;
    },
    save_credential: () => {
      if (backend.saveError) {
        throw backend.saveError;
      }
      return undefined;
    },
    sync_account: () => {
      if (backend.syncError) {
        throw backend.syncError;
      }
      return { folders: backend.folders.length, added: 0, removed: 0 };
    },
    // ---------------- DKIM ----------------

    // 假后端也**不回传私钥**：这里只有选择器、域名和公钥，
    // 跟真的 `get_dkim_config` 一个口径。
    // 表里存的就这份形状（`backend.dkimConfigs`），像真的 `account_dkim` 表——
    // 私钥不在里面，它只存在于「用户刚刚提交的那次请求」里。
    get_dkim_config: (args) => {
      if (backend.dkimError) {
        throw backend.dkimError;
      }
      const accountId = String(args.accountId ?? "");
      const config = backend.dkimConfigs.get(accountId);
      return config ? { ...config } : null;
    },
    set_dkim_config: (args) => {
      if (backend.dkimSaveError) {
        throw backend.dkimSaveError;
      }
      const accountId = String(args.accountId ?? "");
      const selector = String(args.selector ?? "").trim();
      const domain = String(args.domain ?? "").trim().toLowerCase();
      const privateKeyPem = String(args.privateKeyPem ?? "");
      const previous = backend.dkimConfigs.get(accountId);
      if (privateKeyPem.trim() === "" && previous === undefined) {
        throw "请填写 DKIM 私钥（PEM）";
      }
      const saved: DkimConfig = {
        accountId,
        selector,
        domain,
        headerAlg: "rsa-sha256",
        enabled: true,
        publicKey: "MIIBIjANBgkq-stub-public-key",
        dnsName: `${selector}._domainkey.${domain}`,
        dnsRecord: `v=DKIM1; k=rsa; p=MIIBIjANBgkq-stub-public-key`,
      };
      // 只记「有没有私钥」这件事本身，私钥字符串不留在假后端里。
      backend.dkimConfigs.set(accountId, saved);
      return { ...saved };
    },
    clear_dkim_config: (args) => {
      const accountId = String(args.accountId ?? "");
      const removed = backend.dkimConfigs.delete(accountId);
      return removed;
    },
    open_message: () => backend.messageBody,
    create_account: () => ({}),

    // ---------------- 写信（草稿 / 发送） ----------------

    save_draft: (args) => {
      if (backend.saveDraftError) {
        throw backend.saveDraftError;
      }
      const draft = args.draft as DraftInput;
      // 新建时后端分配 id；带着 id 回来的是更新同一份草稿。
      const id = draft.id ?? `draft-${++draftCounter}`;
      // 后端自己读文件：前端给的 size 是 0，这里换成真实大小。
      const attachments = draft.attachments.map((one) => ({
        ...one,
        size:
          one.size > 0
            ? one.size
            : (backend.fileSizes[one.path] ?? DEFAULT_STUB_ATTACHMENT_SIZE),
        mime: one.mime !== "" ? one.mime : guessMime(one.path),
      }));
      // 上限以后端为准（前端只是提前拦一道）。
      if (attachments.length > MAX_ATTACHMENTS) {
        throw new Error(`附件数量超过上限（最多 ${MAX_ATTACHMENTS} 个）`);
      }
      if (attachments.some((one) => one.size > MAX_ATTACHMENT_BYTES)) {
        throw new Error("单个附件超过上限（10 MiB）");
      }
      const total = attachments.reduce((sum, one) => sum + one.size, 0);
      if (total > MAX_TOTAL_ATTACHMENT_BYTES) {
        throw new Error("附件总大小超过上限（20 MiB）");
      }
      const dto: DraftDto = {
        id,
        accountId: draft.accountId,
        subject: draft.subject,
        to: draft.to,
        state: "draft",
        updatedAt: Date.now(),
      };
      backend.drafts = [
        ...backend.drafts.filter((one) => one.id !== id),
        dto,
      ];
      backend.draftDetails.set(id, {
        ...dto,
        cc: draft.cc,
        bcc: draft.bcc,
        bodyText: draft.bodyText,
        bodyHtml: draft.bodyHtml ?? null,
        attachments,
      });
      return dto;
    },
    list_drafts: (args) => {
      const accountId = String(args.accountId);
      return backend.drafts
        .filter((one) => one.accountId === accountId)
        .map((one) => ({ ...one }));
    },
    // 取草稿全文：面板用它把上次那封的内容填回编辑区。
    get_draft: (args) => {
      const id = String(args.id);
      const detail = backend.draftDetails.get(id);
      if (!detail) {
        throw new Error(`没有 id=${id} 的草稿`);
      }
      return {
        ...detail,
        attachments: detail.attachments.map((one) => ({ ...one })),
      };
    },
    send_message: (args) => {
      if (backend.sendError) {
        throw backend.sendError;
      }
      const draft = args.draft as DraftInput;
      backend.sentDrafts.push(draft);
      const entryId = `entry-${backend.sentDrafts.length}`;
      backend.sentEntries.push({ entryId, draft, state: backend.sendState });
      return {
        messageId: `<sent-${backend.sentDrafts.length}@example.com>`,
        entryId,
        state: backend.sendState,
        detail:
          backend.sendDetail ?? DEFAULT_SEND_DETAIL[backend.sendState],
      };
    },
    // 「再发一次」：把原来那封**原样再提交一次**（不是新写一封）。
    // 所以条目数不变、Message-ID 不变，只更新那一条的状态。
    resend_uncertain: (args) => {
      if (backend.resendError) {
        throw backend.resendError;
      }
      const entryId = String(args.entryId);
      const entry = backend.sentEntries.find((one) => one.entryId === entryId);
      if (!entry) {
        // 真实后端：`发信记录 {id} 不存在`。
        throw new Error(`发信记录 ${entryId} 不存在`);
      }
      entry.state = backend.resendState;
      // 条目 id 里带着它当初是第几封，用来自洽地造出同一个 Message-ID。
      return {
        messageId: `<sent-${entryId.replace(/^entry-/, "")}@example.com>`,
        entryId,
        state: backend.resendState,
        detail:
          backend.resendDetail ?? DEFAULT_SEND_DETAIL[backend.resendState],
      };
    },
    // 撤回一封发出去的信：它变回可编辑的草稿。
    withdraw_draft: (args) => {
      const entryId = String(args.entryId);
      const index = backend.sentEntries.findIndex(
        (one) => one.entryId === entryId,
      );
      if (index < 0) {
        return false;
      }
      const entry = backend.sentEntries[index];
      // 撤回来之后**还是同一条记录**（只是状态从「结果未确认」变回「草稿」），
      // 所以草稿 id 就是条目 id——真实后端也是这么做的。
      const id = entryId;
      const dto: DraftDto = {
        id,
        accountId: entry.draft.accountId,
        subject: entry.draft.subject,
        to: entry.draft.to,
        state: "draft",
        updatedAt: Date.now(),
      };
      backend.drafts = [
        ...backend.drafts.filter((one) => one.id !== id),
        dto,
      ];
      backend.draftDetails.set(id, {
        ...dto,
        cc: entry.draft.cc,
        bcc: entry.draft.bcc,
        bodyText: entry.draft.bodyText,
        bodyHtml: entry.draft.bodyHtml ?? null,
        attachments: entry.draft.attachments.map((one) => ({ ...one })),
      });
      backend.sentEntries.splice(index, 1);
      return true;
    },
  };

  backend.impl = async (cmd: string, args?: InvokeArgs): Promise<unknown> => {
    const call: InvokeCall = { cmd, args: args ?? {} };
    backend.calls.push(call);
    const handler = handlers[cmd];
    if (!handler) {
      throw new Error(`测试里没有为命令「${cmd}」准备 mock`);
    }
    return handler(call.args);
  };

  return backend;
}
