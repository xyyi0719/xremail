import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import "./ComposePanel.css";
import RichTextEditor, {
  htmlToPlainText,
  textToHtml,
} from "./RichTextEditor";
import type {
  DraftAttachment,
  DraftDetail,
  DraftDto,
  DraftInput,
  SendResult,
} from "../types";
import * as ATTACHMENT_LIMITS from "../attachmentLimits";

interface Props {
  /** 用哪个账户发信（写进草稿的 accountId）。 */
  accountId: string;
  /** 账户邮箱，只用来显示给用户看。 */
  accountEmail: string;
  /** 请求关闭面板（父组件负责卸载）。 */
  onClose: () => void;
}

/** 面板里维护的表单内容。 */
interface ComposeForm {
  /** 收件人（To）。 */
  to: string;
  /** 抄送（Cc）。 */
  cc: string;
  /** 密送（Bcc）。 */
  bcc: string;
  subject: string;
  /** 纯文本正文（纯文本模式下是用户直接编辑的）。 */
  bodyText: string;
  /** 富文本正文（富文本模式下是用户直接编辑的；纯文本模式下从 bodyText 生成）。 */
  bodyHtml: string;
  attachments: DraftAttachment[];
}

/** 正文编辑模式。 */
type EditorMode = "plain" | "rich";

/** 发送结果的界面说法。 */
interface SendOutcome {
  kind: "sent" | "uncertain" | "failed";
  /** 主文案。 */
  text: string;
  /** 后端给的补充说明。 */
  detail: string | null;
  /** 服务器给的 Message-ID。 */
  messageId: string | null;
  /** 发信条目 id——`uncertain` 时要靠它「撤回来改」。 */
  entryId: string | null;
}

/** 待用户二次确认的动作。 */
interface PendingConfirm {
  kind: "resend" | "withdraw";
  title: string;
  text: string;
  /** 确认按钮上的字（要能一眼看出这一步到底会做什么）。 */
  confirmLabel: string;
  /** 撤回哪一封（`withdraw` 时用）。 */
  entryId?: string;
}

/** 自动存草稿的防抖间隔（毫秒）。 */
const AUTOSAVE_DELAY_MS = 800;

// 附件上限**不在这里定义**：后端是唯一事实来源，
// 由 `attachment_limits` 命令在应用启动时下发（见 `../attachmentLimits`）。
// 这个文件里不写任何上限数字——曾经前端写 20 MiB、后端 18 MiB，
// 用户在前端能过、点发送才被拒。

/**
 * 可执行 / 脚本类扩展名黑名单（**不含点**，小写）。
 *
 * 为什么看扩展名而不是 MIME 类型：MIME 类型是**发送方自己填的**，
 * 一个 `.exe` 完全可以声明成 `text/plain`，拿 MIME 做拦截等于没拦。
 *
 * 这不是安全边界（改名就能绕过），只是「手滑把可执行文件发出去」的护栏；
 * 真正的防线在对方的服务端与客户端扫描。
 */
const BLOCKED_EXTENSIONS = new Set([
  "exe",
  "bat",
  "cmd",
  "com",
  "scr",
  "msi",
  "js",
  "vbs",
  "jar",
  "ps1",
  "sh",
  "app",
  "dmg",
  "pkg",
  "pif",
  "cpl",
  "hta",
]);

/** 空表单。 */
const EMPTY_FORM: ComposeForm = {
  to: "",
  cc: "",
  bcc: "",
  subject: "",
  bodyText: "",
  bodyHtml: "",
  attachments: [],
};

/**
 * 收件人分隔符。
 *
 * 逗号（中英文）、分号（中英文）、空白都能分隔多个地址——
 * 邮箱地址本身不含这些字符。
 */
const ADDRESS_SPLIT = /[,;，；\s]+/;

/** 把一行收件人文本拆成地址数组（去掉空项）。 */
function parseAddresses(text: string): string[] {
  return text
    .split(ADDRESS_SPLIT)
    .map((one) => one.trim())
    .filter((one) => one !== "");
}

/**
 * 收件人地址的**轻量**校验。
 *
 * 只提示、不拦人：真正的校验在后端（那边会返回逐条问题）。
 * 前端做这层是为了让用户**当场**发现自己少写了 `@`，
 * 而不是等发送失败之后才知道。
 */
function addressWarnings(text: string): string[] {
  const trimmed = text.trim();
  if (trimmed === "") {
    return [];
  }
  const warnings: string[] = [];
  if (/\s/.test(trimmed)) {
    // 空白也是分隔符——多半是少写了逗号。
    warnings.push("地址之间有空白：空白也会被当成分隔符");
  }
  if (trimmed.split(/[,;，；]/).some((piece) => piece.trim() === "")) {
    warnings.push("有空的地址项，可能多打了逗号");
  }
  for (const token of parseAddresses(trimmed)) {
    if (!token.includes("@")) {
      warnings.push(`「${token}」缺少 @，可能不是有效的邮箱地址`);
    }
  }
  return warnings;
}

/** 文件名（路径的最后一段）。 */
function baseName(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

/**
 * 文件的扩展名（小写，不含点）。
 *
 * 没有扩展名返回空串（`.bashrc` 这种「点开头」的文件也算没有扩展名）。
 */
function extensionOf(name: string): string {
  const base = baseName(name);
  const dot = base.lastIndexOf(".");
  if (dot <= 0 || dot === base.length - 1) {
    return "";
  }
  return base.slice(dot + 1).toLowerCase();
}

/** 字节数格式成人能读的形式。 */
function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * 附件上限检查（前端提前拦一道；后端也会强制）。
 *
 * 上限**从后端拿**（[`ATTACHMENT_LIMITS.limits`]，应用启动时预热）。
 * 还没拿到就跳过这一道：后端会在保存草稿 / 发信时用同一组常量再校验，
 * 前端这里只是让用户早点看到错误。
 *
 * 注意：**新选的文件大小前端拿不到**（`open()` 只给路径），
 * 所以这里只能拦住「个数」与「已知大小」的附件；真实大小由后端读文件时校验。
 */
function checkAttachmentLimits(attachments: DraftAttachment[]): string | null {
  const limits = ATTACHMENT_LIMITS.limits();
  if (!limits) {
    return null;
  }
  const labels = ATTACHMENT_LIMITS.limitLabels(limits);
  if (attachments.length > limits.maxAttachmentsPerMessage) {
    return `一封信最多 ${labels.count}附件，现在有 ${attachments.length} 个`;
  }
  const tooBig = attachments.find(
    (one) => one.size > limits.maxAttachmentBytes,
  );
  if (tooBig) {
    return `附件「${tooBig.name}」超过单个附件上限（${labels.single}）`;
  }
  const total = attachments.reduce((sum, one) => sum + one.size, 0);
  if (total > limits.maxTotalAttachmentBytes) {
    return `附件总大小超过上限（${labels.total}）`;
  }
  return null;
}

/**
 * 附件区下面那行提示文字。
 *
 * 上限从后端来（[`ATTACHMENT_LIMITS.limits`]）；还没预热好就不要编数字，
 * 只说「以后端为准」。
 */
function attachmentHintText(): string {
  const limits = ATTACHMENT_LIMITS.limits();
  if (!limits) {
    return "附件的大小与个数以保存时后端的校验为准；";
  }
  const labels = ATTACHMENT_LIMITS.limitLabels(limits);
  return `单个附件不超过 ${labels.single}，一封信最多 ${labels.count}，合计不超过 ${labels.total}；`;
}

/** 表单是不是完全空的（空草稿不值得往库里写）。 */function isEmptyForm(form: ComposeForm): boolean {
  return (
    form.to.trim() === "" &&
    form.cc.trim() === "" &&
    form.bcc.trim() === "" &&
    form.subject.trim() === "" &&
    form.bodyText.trim() === "" &&
    form.bodyHtml.trim() === "" &&
    form.attachments.length === 0
  );
}

/**
 * 按契约拼出发给后端的草稿结构。
 *
 * ## 为什么 `bodyText` 永远都有
 *
 * 有些客户端（以及大多数网关）只读 `text/plain`；没有它正文就是空的。
 * 富文本模式下这里从 HTML 抽一份纯文本出来。
 */
function buildDraft(
  accountId: string,
  form: ComposeForm,
  mode: EditorMode,
  id: string | null,
): DraftInput {
  const rich = mode === "rich";
  return {
    accountId,
    // 没有 id 就是「新建一份」——**不要**给 `null`：后端按缺省处理。
    ...(id === null ? {} : { id }),
    to: parseAddresses(form.to),
    cc: parseAddresses(form.cc),
    bcc: parseAddresses(form.bcc),
    subject: form.subject,
    bodyText: rich ? htmlToPlainText(form.bodyHtml) : form.bodyText,
    bodyHtml: rich ? form.bodyHtml : null,
    attachments: form.attachments,
  };
}

/**
 * 把发送结果翻译成界面上的说法。
 *
 * 三种状态必须分开说：`uncertain`（服务器没确认）**不是**「失败」——
 * 重发一封可能已经发出去的邮件，就是给用户发了两封信。
 */
function describeSendResult(result: SendResult): SendOutcome {
  switch (result.state) {
    case "sent":
      return {
        kind: "sent",
        text: "已发送",
        detail: result.detail || null,
        messageId: result.messageId,
        entryId: result.entryId ?? null,
      };
    case "uncertain":
      return {
        kind: "uncertain",
        text: "发送结果不确定：服务器没有确认，这封信可能已经发出去了，不要重复发送",
        detail: result.detail || null,
        messageId: result.messageId,
        entryId: result.entryId ?? null,
      };
    case "failed":
      return {
        kind: "failed",
        text: "发送失败",
        detail: result.detail || null,
        messageId: result.messageId,
        entryId: result.entryId ?? null,
      };
    default:
      // 后端加了新状态而前端没跟上：别静默吞掉。
      return {
        kind: "failed",
        text: "发送失败：后端返回了不认识的状态",
        detail: null,
        messageId: null,
        entryId: null,
      };
  }
}

/** 时间戳格式化成 HH:MM:SS。 */
function formatTime(ts: number): string {
  const date = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/** 时间戳格式化成「YYYY-MM-DD HH:MM:SS」（草稿列表用）。 */
function formatDateTime(ts: number): string {
  const date = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/**
 * 「再发一次」的二次确认。
 *
 * 这里说的是**原来那封**再提交一次（Message-ID 不变），
 * 不是另写一封新的——见 `confirmResend`。
 */
function resendConfirm(entryId: string): PendingConfirm {
  return {
    kind: "resend",
    entryId,
    title: "确认再发一次？",
    text: "服务器没有确认这封信有没有送到。再发一次会把原来这封原样再提交一遍：如果对方其实已经收到了第一封，他会收到两封。",
    confirmLabel: "确认再发一次",
  };
}

/**
 * 「撤回来改」的二次确认。
 *
 * 撤回哪一封由 `entryId` 决定：「再发一次」之后可能**同时有好几封**
 * 结果未确认，每封都得能单独撤回。
 */
function withdrawConfirm(entryId: string): PendingConfirm {
  return {
    kind: "withdraw",
    entryId,
    title: "确认撤回来改？",
    text: "把这封信撤回来，变成可以继续编辑的草稿。如果对方其实已经收到了它，你改完再发就会变成两封。",
    confirmLabel: "确认撤回来",
  };
}

/**
 * 写信面板。
 *
 * ## 自动存草稿
 *
 * 改动后等 800ms 再存一次（防抖）：每敲一个字就写一次库既吵又慢，
 * 而完全不等又会丢掉用户刚写的半句话。
 *
 * ## 草稿箱
 *
 * 面板打开时拉一次 `list_drafts`：用户可以在列表里点开**上次那封**接着改
 * （`get_draft` → 回填表单），而不是每次都新建一份草稿。
 *
 * ## 三种发送结果
 *
 * `sent` / `uncertain` / `failed` 必须在界面上分开说，
 * 理由见 `types.ts` 里 `SendResult` 的说明——尤其是 `uncertain`：
 * 那是「不知道有没有发出去」，自动重发会让用户收到两封信，
 * 所以它只能由用户**明确选择**「再发一次」或「撤回来改」，且都要二次确认。
 */
function ComposePanel({ accountId, accountEmail, onClose }: Props) {
  const [form, setForm] = useState<ComposeForm>(EMPTY_FORM);
  /** 正文编辑模式（纯文本 / 富文本）。 */
  const [mode, setMode] = useState<EditorMode>("plain");
  /** 密送栏默认收起来（它平时不该被看见）。 */
  const [showBcc, setShowBcc] = useState(false);
  /** 已经发出去了就不再自动存草稿。 */
  const [sent, setSent] = useState(false);
  const [savedAt, setSavedAt] = useState<string | null>(null);
  const [savingDraft, setSavingDraft] = useState(false);
  const [sending, setSending] = useState(false);
  const [outcome, setOutcome] = useState<SendOutcome | null>(null);
  /**
   * 结果未确认、还挂在那里的发信条目 id。
   *
   * 「再发一次」会发出**新的一封**，所以未确认的条目可能有多个；
   * 每封都要能单独撤回（只能撤回自己那一封）。
   */
  const [pendingUncertain, setPendingUncertain] = useState<string[]>([]);
  const [formError, setFormError] = useState<string | null>(null);

  // ---- 草稿箱 ----
  const [drafts, setDrafts] = useState<DraftDto[]>([]);
  const [draftsLoading, setDraftsLoading] = useState(true);
  const [draftsError, setDraftsError] = useState<string | null>(null);
  /** 正在编辑的那份草稿（列表里高亮它）。 */
  const [activeDraftId, setActiveDraftId] = useState<string | null>(null);
  /** 待用户二次确认的动作。 */
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  /** 附件相关的提示（超限、黑名单之类的）。 */
  const [attachmentNotice, setAttachmentNotice] = useState<string | null>(null);

  /**
   * 已经存过的草稿 id。
   *
   * 用 ref 而不是 state：它只是「下次存草稿要带上 id」，
   * 界面上不显示它，没必要为它重渲染一次。
   */
  const draftIdRef = useRef<string | null>(null);
  /**
   * 回填草稿后要跳过的自动保存次数。
   *
   * 回填不是「改动」——不然刚打开一封草稿就立刻又存一遍。
   */
  const skipAutosaveRef = useRef(0);
  /**
   * 附件集合的「指纹」。
   *
   * 附件的真实大小只有后端知道（前端选文件时只拿到路径），
   * 所以存完草稿要再问后端一次——但只在附件集合**变了**时才问。
   */
  const sizeProbeRef = useRef("");
  /**
   * 上一次被编辑的是哪个模式。
   *
   * 切换模式时据此决定另一边的内容要不要重新生成：
   * 纯文本 → 富文本时用文本生成 HTML，富文本 → 纯文本时抽纯文本。
   */
  const lastEditedRef = useRef<EditorMode>("plain");

  /** 拉一次草稿列表。 */
  const refreshDrafts = useCallback(async () => {
    setDraftsLoading(true);
    try {
      const list = await invoke<DraftDto[]>("list_drafts", { accountId });
      setDrafts(list);
      setDraftsError(null);
    } catch (e) {
      setDraftsError(`拉取草稿列表失败：${String(e)}`);
    } finally {
      setDraftsLoading(false);
    }
  }, [accountId]);

  useEffect(() => {
    void refreshDrafts();
  }, [refreshDrafts]);

  /**
   * 问后端要附件的真实大小。
   *
   * 前端选文件时只拿到路径（`open()` 不给大小），所以大小先填 0，
   * 由后端读文件后回填——这里在存完草稿后再取一次草稿全文。
   */
  const refreshAttachmentSizes = useCallback(
    async (id: string, attachments: DraftAttachment[]) => {
      if (!attachments.some((one) => one.size === 0)) {
        return;
      }
      const signature = attachments
        .map((one) => `${one.path}|${one.name}`)
        .join(",");
      if (signature === sizeProbeRef.current) {
        return;
      }
      sizeProbeRef.current = signature;
      try {
        const detail = await invoke<DraftDetail>("get_draft", { id });
        setForm((prev) => ({
          ...prev,
          attachments: prev.attachments.map((attachment) => {
            const found = detail.attachments.find(
              (one) =>
                one.name === attachment.name &&
                (one.path === attachment.path || one.path === ""),
            );
            if (!found || found.size <= 0) {
              return attachment;
            }
            return {
              ...attachment,
              size: found.size,
              mime: found.mime || attachment.mime,
            };
          }),
        }));
      } catch {
        // 拿不到就算了：后端保存草稿时会用真实大小。
      }
    },
    [],
  );

  /**
   * 存一次草稿。
   *
   * 成功后把后端给的 id 记下来，后面的存草稿就变成「更新同一份草稿」
   * 而不是每次新建一份。
   */
  const persistDraft = useCallback(
    async (snapshot: ComposeForm) => {
      setSavingDraft(true);
      try {
        const dto = await invoke<DraftDto>("save_draft", {
          draft: buildDraft(accountId, snapshot, mode, draftIdRef.current),
        });
        const isNew = dto.id !== draftIdRef.current;
        draftIdRef.current = dto.id;
        setActiveDraftId(dto.id);
        setSavedAt(formatTime(Date.now()));
        setFormError(null);
        if (isNew) {
          // 新建了一份草稿：列表里多了一条，刷新一下。
          void refreshDrafts();
        }
        void refreshAttachmentSizes(dto.id, snapshot.attachments);
      } catch (e) {
        setFormError(`自动保存草稿失败：${String(e)}`);
      } finally {
        setSavingDraft(false);
      }
    },
    [accountId, mode, refreshDrafts, refreshAttachmentSizes],
  );

  /**
   * 自动存草稿：改动后等 800ms。
   *
   * 空的表单不存（没什么可存的），已经发出去的也不存，
   * **结果未确认的也不存**——那封信的状态已经不是草稿了，
   * 往回写只会得到「不能改」的报错。
   *
   * 依赖 `form` 的**引用**：每改一个字段就是新对象，
   * 于是计时器被重置——这正是防抖要的。
   */
  useEffect(() => {
    if (sent || outcome?.kind === "uncertain") {
      return;
    }
    if (isEmptyForm(form)) {
      return;
    }
    if (skipAutosaveRef.current > 0) {
      skipAutosaveRef.current -= 1;
      return;
    }
    const timer = window.setTimeout(() => {
      void persistDraft(form);
    }, AUTOSAVE_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [form, sent, outcome, persistDraft]);

  const update = useCallback(
    <K extends keyof ComposeForm>(key: K, value: ComposeForm[K]) => {
      setForm((prev) => ({ ...prev, [key]: value }));
    },
    [],
  );

  /** 纯文本正文变了。 */
  const handlePlainChange = useCallback((text: string) => {
    lastEditedRef.current = "plain";
    setForm((prev) => ({ ...prev, bodyText: text }));
  }, []);

  /** 富文本正文变了（传进来的已经是消毒过的 HTML）。 */
  const handleRichChange = useCallback((html: string) => {
    lastEditedRef.current = "rich";
    setForm((prev) => ({ ...prev, bodyHtml: html }));
  }, []);

  /**
   * 切换正文模式。
   *
   * 两个方向都要**保住内容**：
   * 纯文本 → 富文本用文本生成 HTML；富文本 → 纯文本从 HTML 抽纯文本。
   * 如果另一边刚被编辑过（`lastEditedRef`），就用编辑过的那份，
   * 这样来回切不会把用户刚写的东西覆盖掉。
   */
  const switchMode = useCallback(
    (next: EditorMode) => {
      if (next === mode) {
        return;
      }
      setForm((prev) => {
        if (next === "rich") {
          const html =
            lastEditedRef.current === "rich" && prev.bodyHtml !== ""
              ? prev.bodyHtml
              : textToHtml(prev.bodyText);
          return { ...prev, bodyHtml: html };
        }
        const text =
          lastEditedRef.current === "plain" && prev.bodyText !== ""
            ? prev.bodyText
            : htmlToPlainText(prev.bodyHtml);
        return { ...prev, bodyText: text };
      });
      setMode(next);
    },
    [mode],
  );

  /**
   * 打开一份草稿接着改。
   *
   * 关键：把 id 记下来，后面保存 / 发送带回去的就是**同一份**草稿。
   */
  const openDraft = useCallback(async (id: string) => {
    try {
      const detail = await invoke<DraftDetail>("get_draft", { id });
      draftIdRef.current = detail.id;
      setActiveDraftId(detail.id);
      setSent(false);
      setOutcome(null);
      setFormError(null);
      setAttachmentNotice(null);
      const hasHtml = detail.bodyHtml !== null && detail.bodyHtml !== "";
      setMode(hasHtml ? "rich" : "plain");
      lastEditedRef.current = hasHtml ? "rich" : "plain";
      setForm({
        to: detail.to.join(", "),
        cc: detail.cc.join(", "),
        bcc: detail.bcc.join(", "),
        subject: detail.subject,
        bodyText: detail.bodyText,
        bodyHtml: detail.bodyHtml ?? "",
        attachments: detail.attachments.map((one) => ({ ...one })),
      });
      // 回填不是「改动」：跳过一次自动保存。
      skipAutosaveRef.current = 1;
      // 大小后端已经给全了，不用再去问一次。
      sizeProbeRef.current = detail.attachments
        .map((one) => `${one.path}|${one.name}`)
        .join(",");
    } catch (e) {
      setFormError(`打开草稿失败：${String(e)}`);
    }
  }, []);

  /** 新建一封（不接着任何旧草稿）。 */
  const startNewDraft = useCallback(() => {
    draftIdRef.current = null;
    setActiveDraftId(null);
    setForm(EMPTY_FORM);
    setMode("plain");
    lastEditedRef.current = "plain";
    setSent(false);
    setOutcome(null);
    setFormError(null);
    setAttachmentNotice(null);
    sizeProbeRef.current = "";
    skipAutosaveRef.current = 1;
  }, []);

  /**
   * 选附件。
   *
   * 用 Tauri 的文件选择框拿**路径**，不读文件内容：
   * 几十 MB 的文件读成字节数组会把 IPC 参数撑爆（后端自己读文件）。
   *
   * `open()` 只给路径，所以 `name` 从路径末段取，
   * `mime` 与 `size` 先留空（0）——真实大小由后端读文件后回填。
   */
  const handlePickFiles = useCallback(async () => {
    setAttachmentNotice(null);
    let picked: string[] | string | null;
    try {
      picked = await openFileDialog({
        multiple: true,
        title: "选择附件",
      } as const);
    } catch (e) {
      setAttachmentNotice(`打不开文件选择框：${String(e)}`);
      return;
    }
    const paths = picked === null ? [] : Array.isArray(picked) ? picked : [picked];
    if (paths.length === 0) {
      return;
    }
    const blocked = paths
      .map((path) => baseName(path))
      .filter((name) => BLOCKED_EXTENSIONS.has(extensionOf(name)));
    if (blocked.length > 0) {
      setAttachmentNotice(
        `不能添加可执行文件或脚本：${blocked.join("、")}`,
      );
      return;
    }
    const added: DraftAttachment[] = paths.map((path) => ({
      name: baseName(path),
      mime: "",
      size: 0,
      path,
    }));
    const problem = checkAttachmentLimits([...form.attachments, ...added]);
    if (problem !== null) {
      setAttachmentNotice(problem);
      return;
    }
    setForm((prev) => ({ ...prev, attachments: [...prev.attachments, ...added] }));
  }, [form.attachments]);

  /** 移除单个附件。 */
  const removeAttachment = useCallback((index: number) => {
    setForm((prev) => ({
      ...prev,
      attachments: prev.attachments.filter((_, i) => i !== index),
    }));
  }, []);

  /**
   * 发送。
   *
   * 没写收件人就在前端拦下来：发出去也是白跑一趟，
   * 还会在服务器上留下一封没人收的信。
   *
   * `fresh` = 真的再发一封（不带 id）：用户已经在二次确认里知道
   * 对方可能收到两封了。
   */
  const handleSend = useCallback(
    async (options: { fresh?: boolean } = {}) => {
      const fresh = options.fresh === true;
      const draft = buildDraft(
        accountId,
        form,
        mode,
        fresh ? null : draftIdRef.current,
      );
      if (draft.to.length + draft.cc.length + draft.bcc.length === 0) {
        setFormError("至少要一个收件人");
        setOutcome(null);
        return;
      }

      setSending(true);
      setFormError(null);
      setOutcome(null);
      if (fresh) {
        // 「再发一次」发的是**新的一封**（不带 id）。
        // 把草稿 id 清掉：刚才那封的状态还没定，
        // 拿它的 id 去存草稿会被后端拒（“这条发信记录已经不是草稿了”）。
        draftIdRef.current = null;
        setActiveDraftId(null);
      }
      try {
        const result = await invoke<SendResult>("send_message", { draft });
        setOutcome(describeSendResult(result));
        if (result.state === "sent") {
          // 发出去了就不再自动存草稿，也不用再提醒「没发送」。
          setSent(true);
          draftIdRef.current = null;
          setActiveDraftId(null);
          void refreshDrafts();
        } else if (result.state === "uncertain") {
          // 记住条目 id：用户要「撤回来改」时要用它。
          draftIdRef.current = result.entryId;
          setActiveDraftId(result.entryId);
          // 这一封也挂进「未确认」名单——「再发一次」之后可能有好几封。
          setPendingUncertain((prev) =>
            prev.includes(result.entryId) ? prev : [...prev, result.entryId],
          );
        }
        // `uncertain` 什么都不做：**不自动重发**。
        // 服务器没确认的时候重发，很可能让用户收到两封一样的信。
      } catch (e) {
        // 命令直接失败 = 明确没发出去，允许改完重试。
        setOutcome({
          kind: "failed",
          text: "发送失败",
          detail: String(e),
          messageId: null,
          entryId: null,
        });
      } finally {
        setSending(false);
      }
    },
    [accountId, form, mode, refreshDrafts],
  );

  /**
   * 「撤回来改」：把这封信撤回来，变成可编辑的草稿。
   *
   * 后端会把状态收回到 `draft`，于是这封信又能改了——
   * 改完再发就是**两封**（所以才要二次确认）。
   */
  const confirmWithdraw = useCallback(
    async (entryId: string) => {
      setPendingConfirm(null);
      try {
        const ok = await invoke<boolean>("withdraw_draft", { entryId });
        if (!ok) {
          setFormError("撤不回来：这封信的状态已经变了");
          return;
        }
        draftIdRef.current = entryId;
        setActiveDraftId(entryId);
        // 从「未确认」名单里拿掉。
        setPendingUncertain((prev) => prev.filter((one) => one !== entryId));
        // 撤回来的就是当前这封时，把结果提示收起来（它已经变成草稿了）。
        setOutcome((prev) => (prev?.entryId === entryId ? null : prev));
        setSent(false);
        setFormError(null);
        void refreshDrafts();
      } catch (e) {
        setFormError(`撤回来失败：${String(e)}`);
      }
    },
    [refreshDrafts],
  );

  /**
   * 「再发一次」：把**原来那封**未确认的信原样再提交一次。
   *
   * 注意这**不是** `handleSend({ fresh: true })`：那条路会新写一封草稿、
   * 生成新的 Message-ID，于是对方可能收到两封**内容一样但来路不同**的信，
   * 而且旧那封还挂在「未确认」名单里没人管。
   * 走 `resend_uncertain` 则复用当初冻结的那份字节，Message-ID 不变。
   */
  const confirmResend = useCallback(
    async (entryId: string) => {
      setPendingConfirm(null);
      setSending(true);
      setFormError(null);
      try {
        const result = await invoke<SendResult>("resend_uncertain", {
          entryId,
        });
        const next = describeSendResult(result);
        setOutcome(next);
        if (next.kind === "sent") {
          // 这回服务器确认了：这封从「未确认」名单里拿掉。
          setPendingUncertain((prev) => prev.filter((one) => one !== entryId));
          setSent(true);
          draftIdRef.current = null;
          setActiveDraftId(null);
          void refreshDrafts();
        } else if (next.kind === "uncertain") {
          // 还是没确认：名字照旧挂着，等用户再决定（再发 / 撤回）。
          setPendingUncertain((prev) =>
            prev.includes(entryId) ? prev : [...prev, entryId],
          );
        } else {
          // 这次服务器明确拒绝了：这封**没出去**，留在名单里没意义，
          // 让用户改完重发；同时把「撤回来改」留给它（失败的信也能撤回）。
          setPendingUncertain((prev) => prev.filter((one) => one !== entryId));
        }
      } catch (e) {
        // 命令直接失败 = 这次没有投递成功；原条目状态没准没动，
        // 所以留在名单里，用户还能「撤回来改」。
        setOutcome({
          kind: "failed",
          text: "再发一次失败",
          detail: String(e),
          messageId: null,
          entryId,
        });
      } finally {
        setSending(false);
      }
    },
    [refreshDrafts],
  );

  /** 二次确认框里的「确认」。 */
  const handleConfirmYes = useCallback(async () => {
    const pending = pendingConfirm;
    if (pending?.kind === "resend") {
      const entryId = pending.entryId ?? outcome?.entryId ?? null;
      if (entryId === null) {
        setPendingConfirm(null);
        setFormError("拿不到这封信的编号，没法再发一次");
        return;
      }
      await confirmResend(entryId);
      return;
    }
    if (pending?.kind === "withdraw") {
      const entryId = pending.entryId ?? outcome?.entryId ?? null;
      if (entryId === null) {
        setPendingConfirm(null);
        setFormError("拿不到这封信的编号，没法撤回来");
        return;
      }
      await confirmWithdraw(entryId);
    }
  }, [pendingConfirm, outcome, confirmResend, confirmWithdraw]);

  /** 还有没发出去的改动吗？ */
  const dirty = !isEmptyForm(form) && !sent;

  /** 关闭面板：没发出去的改动要先问一句。 */
  const handleClose = () => {
    if (dirty) {
      const ok = window.confirm("这封信还没有发送，确定要关闭吗？");
      if (!ok) {
        return;
      }
      // 临走前把当前内容存一次，别让用户白填。
      void persistDraft(form);
    }
    onClose();
  };

  /**
   * 发送按钮什么时候不让点。
   *
   * - 正在发：不许并发发第二封；
   * - 已经发出去了：不许再发一封一样的；
   * - 服务器没确认（`uncertain`）：**停用**。
   *   这封信可能已经发出去了，这时候点一下就是给用户发两封——
   *   要再发得走「再发一次」那条路（有二次确认）。
   */
  const sendBlocked = sending || sent || outcome?.kind === "uncertain";

  const toWarnings = addressWarnings(form.to);
  const ccWarnings = addressWarnings(form.cc);
  const bccWarnings = addressWarnings(form.bcc);

  /**
   * 还挂着、但**当前结果区没在展示**的未确认条目。
   *
   * 结果区正在展示的那封，它的「撤回来改」按钮在上面（跟「再发一次」并排）；
   * 其余每一封都得能单独撤回（不然用户只能看着它悬着）。
   * 「再发一次」之后结果区可能换成别的结果（比如「再发一次失败」），
   * 那原来那封就落到下面这个名单里，所以这里比的是**名单里有没有**，
   * 而不是跟 `outcome.entryId` 比。
   */
  const otherUncertain = pendingUncertain.filter(
    (entryId) => entryId !== outcome?.entryId || outcome.kind !== "uncertain",
  );

  return (
    <section className="panel compose-panel" aria-label="写信面板">
      <div className="compose-head">
        <h2 className="panel-title">写邮件</h2>
        <button type="button" className="compose-close" onClick={handleClose}>
          关闭
        </button>
      </div>

      <p className="panel-hint">
        发信账户：{accountEmail}。改动会自动存草稿，正文可以切纯文本 / 富文本。
      </p>

      {/* ---- 草稿箱 ---- */}
      <section className="compose-drafts" aria-label="草稿箱">
        <div className="compose-drafts-head">
          <h3 className="compose-drafts-title">草稿箱</h3>
          <button
            type="button"
            className="compose-drafts-new"
            aria-label="新建一封"
            onClick={startNewDraft}
          >
            新建一封
          </button>
        </div>

        {draftsLoading && <p className="compose-hint">正在加载草稿…</p>}

        {draftsError && (
          <p className="compose-error" role="alert" data-testid="compose-drafts-error">
            {draftsError}
          </p>
        )}

        {!draftsLoading && !draftsError && drafts.length === 0 && (
          <p className="compose-hint" data-testid="compose-drafts-empty">
            还没有草稿。写点什么，它会自动存下来。
          </p>
        )}

        {drafts.length > 0 && (
          <ul className="compose-draft-list">
            {drafts.map((draft) => (
              <li key={draft.id} className="compose-draft-item">
                <button
                  type="button"
                  className="compose-draft-open"
                  aria-label={`打开草稿 ${draft.subject || "无主题"}`}
                  aria-current={draft.id === activeDraftId}
                  onClick={() => void openDraft(draft.id)}
                >
                  <span className="compose-draft-subject">
                    {draft.subject.trim() === "" ? "（无主题）" : draft.subject}
                  </span>
                  <span className="compose-draft-to">
                    {draft.to.length === 0 ? "（没有收件人）" : draft.to.join("、")}
                  </span>
                  <span className="compose-draft-time">
                    {formatDateTime(draft.updatedAt)}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      <div className="compose-fields">
        <label className="field">
          <span>收件人</span>
          <input
            type="text"
            value={form.to}
            onChange={(e) => update("to", e.target.value)}
            placeholder="多个地址用逗号分开"
          />
        </label>
        {toWarnings.length > 0 && (
          <p
            className="compose-field-warning"
            role="status"
            data-testid="compose-address-warning"
          >
            {toWarnings.join("；")}
          </p>
        )}

        <label className="field">
          <span>抄送</span>
          <input
            type="text"
            value={form.cc}
            onChange={(e) => update("cc", e.target.value)}
            placeholder="多个地址用逗号分开"
          />
        </label>
        {ccWarnings.length > 0 && (
          <p
            className="compose-field-warning"
            role="status"
            data-testid="compose-address-warning"
          >
            {ccWarnings.join("；")}
          </p>
        )}

        <button
          type="button"
          className="compose-bcc-toggle"
          aria-expanded={showBcc}
          onClick={() => setShowBcc((prev) => !prev)}
        >
          {showBcc ? "隐藏密送" : "显示密送"}
        </button>

        {showBcc && (
          <>
            <label className="field">
              <span>密送</span>
              <input
                type="text"
                value={form.bcc}
                onChange={(e) => update("bcc", e.target.value)}
                placeholder="多个地址用逗号分开"
              />
            </label>
            {bccWarnings.length > 0 && (
              <p
                className="compose-field-warning"
                role="status"
                data-testid="compose-address-warning"
              >
                {bccWarnings.join("；")}
              </p>
            )}
          </>
        )}
      </div>

      <p className="compose-hint">
        密送（Bcc）收件人不会出现在收件人列表里——只有邮件头里能看到。
      </p>

      <div className="compose-fields">
        <label className="field">
          <span>主题</span>
          <input
            type="text"
            value={form.subject}
            onChange={(e) => update("subject", e.target.value)}
          />
        </label>

        <div className="compose-mode" role="group" aria-label="正文编辑模式">
          <button
            type="button"
            className="compose-mode-button"
            aria-label="纯文本模式"
            aria-pressed={mode === "plain"}
            onClick={() => switchMode("plain")}
          >
            纯文本
          </button>
          <button
            type="button"
            className="compose-mode-button"
            aria-label="富文本模式"
            aria-pressed={mode === "rich"}
            onClick={() => switchMode("rich")}
          >
            富文本
          </button>
        </div>

        {mode === "plain" ? (
          <label className="field">
            <span>正文</span>
            <textarea
              className="compose-body"
              rows={8}
              value={form.bodyText}
              onChange={(e) => handlePlainChange(e.target.value)}
            />
          </label>
        ) : (
          <div className="field">
            <span>正文</span>
            <RichTextEditor value={form.bodyHtml} onChange={handleRichChange} />
          </div>
        )}

        <div className="field">
          <span>附件</span>
          <button
            type="button"
            className="compose-attach-button"
            aria-label="添加附件"
            onClick={() => void handlePickFiles()}
          >
            添加附件
          </button>
        </div>
      </div>

      <p className="compose-hint">{attachmentHintText()}
        可执行文件与脚本不能作为附件。实际大小以保存时后端的校验为准。
      </p>

      {attachmentNotice && (
        <p
          className="compose-error"
          role="alert"
          data-testid="compose-attachment-notice"
        >
          {attachmentNotice}
        </p>
      )}

      {form.attachments.length > 0 && (
        <ul className="compose-attachments">
          {form.attachments.map((attachment, index) => (
            <li
              key={`${attachment.path}-${attachment.name}-${index}`}
              className="compose-attachment"
            >
              <span className="compose-attachment-name">{attachment.name}</span>
              <span className="compose-attachment-size" data-testid="compose-attachment-size">
                {attachment.size > 0
                  ? formatBytes(attachment.size)
                  : "大小待后端校验"}
              </span>
              <button
                type="button"
                className="compose-attachment-remove"
                aria-label={`移除附件 ${attachment.name}`}
                onClick={() => removeAttachment(index)}
              >
                移除
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="compose-actions">
        <span className="compose-saved" data-testid="compose-saved">
          {savedAt !== null
            ? `草稿已保存 ${savedAt}`
            : savingDraft
              ? "正在保存草稿…"
              : "改动后会自动存草稿"}
        </span>
        <button
          type="button"
          className="compose-send"
          disabled={sendBlocked}
          onClick={() => void handleSend()}
        >
          {sending ? "发送中…" : "发送"}
        </button>
      </div>

      {formError && (
        <p className="compose-error" role="alert" data-testid="compose-error">
          {formError}
        </p>
      )}

      {outcome && (
        <div
          className={`compose-result compose-result-${outcome.kind}`}
          role="status"
          data-testid="compose-send-result"
        >
          <strong>{outcome.text}</strong>
          {outcome.detail && (
            <span className="compose-result-detail">{outcome.detail}</span>
          )}
          {outcome.kind === "sent" && outcome.messageId && (
            <span className="compose-result-detail">
              Message-ID：{outcome.messageId}
            </span>
          )}
          {outcome.kind === "uncertain" && (
            <>
              <span className="compose-result-detail">
                发送按钮已停用：要再发得走下面的「再发一次」或「撤回来改」，
                两者都会先请你确认。
              </span>
              <div className="compose-uncertain-actions">
                <button
                  type="button"
                  className="compose-uncertain-button"
                  aria-label="再发一次"
                  onClick={() => {
                    const entryId = outcome.entryId;
                    if (!entryId) {
                      setFormError("拿不到这封信的编号，没法再发一次");
                      return;
                    }
                    setPendingConfirm(resendConfirm(entryId));
                  }}
                >
                  再发一次
                </button>
                <button
                  type="button"
                  className="compose-uncertain-button"
                  aria-label="撤回来改"
                  onClick={() => {
                    const entryId = outcome.entryId;
                    if (!entryId) {
                      setFormError("拿不到这封信的编号，没法撤回来");
                      return;
                    }
                    setPendingConfirm(withdrawConfirm(entryId));
                  }}
                >
                  撤回来改
                </button>
              </div>
            </>
          )}
          {outcome.kind === "failed" && (
            <span className="compose-result-detail">
              改完可以直接再点「发送」重试。
            </span>
          )}
        </div>
      )}

      {otherUncertain.length > 0 && (
        <div
          className="compose-uncertain-pending"
          data-testid="compose-uncertain-pending"
        >
          <strong>另外还有 {otherUncertain.length} 封结果未确认的信</strong>
          <span className="compose-result-detail">
            服务器没确认这些信有没有送到。每一封都可以单独撤回来改——
            撤回来不会重发，但对方如果已经收到了，改完再发就会变成两封。
          </span>
          <ul className="compose-uncertain-list">
            {otherUncertain.map((entryId) => (
              <li key={entryId} className="compose-uncertain-item">
                <span>…{entryId.slice(-6)}</span>
                <button
                  type="button"
                  className="compose-uncertain-button"
                  aria-label={`撤回来改 ${entryId}`}
                  onClick={() => setPendingConfirm(withdrawConfirm(entryId))}
                >
                  撤回来改
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      {pendingConfirm && (
        <div
          className="compose-confirm"
          role="alertdialog"
          aria-modal="true"
          aria-labelledby="compose-confirm-title"
          data-testid="compose-confirm"
        >
          <h3 id="compose-confirm-title">{pendingConfirm.title}</h3>
          <p className="compose-confirm-text">{pendingConfirm.text}</p>
          <div className="compose-confirm-actions">
            <button
              type="button"
              className="compose-confirm-yes"
              aria-label={pendingConfirm.confirmLabel}
              onClick={() => void handleConfirmYes()}
            >
              {pendingConfirm.confirmLabel}
            </button>
            <button
              type="button"
              className="compose-confirm-no"
              aria-label="取消"
              onClick={() => setPendingConfirm(null)}
            >
              取消
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

export default ComposePanel;
