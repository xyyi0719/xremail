/**
 * 与 Rust 侧对应的类型定义。
 *
 * 命名规则：camelCase（`xremail-core` 的 `Account` 标了
 * `#[serde(rename_all = "camelCase")]`）。
 *
 * 枚举值保持 Rust 侧的显式 serde 名（`netease_163` 等）——
 * 那些字符串同时也是数据库列值，改动需要写迁移。
 */

/** 服务商标记。 */
export type ProviderKind =
  | "netease_163"
  | "netease_126"
  | "netease_enterprise"
  | "tencent"
  | "wo"
  | "google"
  | "exchange"
  | "generic";

/** 认证方式。 */
export type AuthKind = "password" | "oauth2";

/** 传输加密方式。 */
export type TlsMode = "none" | "starttls" | "implicit";

/** SMTP 认证方式。 */
export type SmtpAuth = "none" | "plain" | "login" | "xoauth2";

/** 同步方式。 */
export type SyncMode = "idle" | "poll";

/** 服务商显示名。 */
export const PROVIDER_LABELS: Record<ProviderKind, string> = {
  netease_163: "163 邮箱",
  netease_126: "126 邮箱",
  netease_enterprise: "企业邮",
  tencent: "QQ 邮箱",
  wo: "沃邮箱",
  google: "Gmail",
  exchange: "Exchange",
  generic: "其它",
};

/** 加密方式显示名。 */
export const TLS_LABELS: Record<TlsMode, string> = {
  none: "明文",
  starttls: "STARTTLS",
  implicit: "SSL/TLS",
};

/** SMTP 认证方式显示名。 */
export const SMTP_AUTH_LABELS: Record<SmtpAuth, string> = {
  none: "不认证",
  plain: "PLAIN",
  login: "LOGIN",
  xoauth2: "XOAUTH2",
};

/** 同步方式显示名。 */
export const SYNC_MODE_LABELS: Record<SyncMode, string> = {
  idle: "实时推送",
  poll: "轮询",
};

/** 服务器配置（对应 `xremail_core::model::ServerConfig`）。 */
export interface ServerConfig {
  imapHost: string;
  imapPort: number;
  imapTls: TlsMode;
  smtpHost: string;
  smtpPort: number;
  smtpTls: TlsMode;
  smtpAuth: SmtpAuth;
}

/** 一个账户（对应 `xremail_core::model::Account`）。 */
export interface Account {
  id: string;
  email: string;
  provider: ProviderKind;
  displayName: string | null;
  color: string | null;
  authKind: AuthKind;
  server: ServerConfig;
  username: string;
  /** 凭据的引用（**不是凭据本身**）；`null` = 凭据没落盘。 */
  secretRef: string | null;
  oauthClientId: string | null;
  syncMode: SyncMode;
  pollIntervalSecs: number;
  signatureHtml: string | null;
  createdAt: number;
  updatedAt: number;
}

/** 新增账户的请求（对应 `xremail_api::NewAccountRequest`）。 */
export interface NewAccountRequest {
  email: string;
  provider: ProviderKind;
  displayName?: string | null;
  imapHost: string;
  imapPort: number;
  imapTls: TlsMode;
  smtpHost: string;
  smtpPort: number;
  smtpTls: TlsMode;
  smtpAuth: SmtpAuth;
  username: string;
  syncMode: SyncMode;
  pollIntervalSecs: number;
}

/**
 * 一个文件夹（对应 `service::message::FolderDto`）。
 *
 * `name` 是**解码后**的显示名（如「草稿箱」）；
 * `rawName` 是线路上的原样（modified UTF-7），发命令时用。
 */
export interface Folder {
  id: string;
  name: string;
  rawName: string;
  delimiter: string | null;
  uidvalidity: number | null;
  lastUid: number | null;
  lastSyncAt: number | null;
}

/** 一封邮件（对应 `xremail_core::model::Message`）。 */
export interface Message {
  id: string;
  accountId: string;
  folderId: string;
  uid: number;
  uidvalidity: number;
  /** RFC Message-ID——**只是线索**（可能缺失、不唯一）。 */
  messageId: string | null;
  subject: string | null;
  snippet: string | null;
  from: string[];
  to: string[];
  cc: string[];
  bcc: string[];
  dateSent: number | null;
  internalDate: number | null;
  flags: number;
  size: number | null;
  hasAttachment: boolean;
  createdAt: number;
  updatedAt: number;
}

/** 一封邮件的正文（对应 `xremail_shell_lib::service::message::MessageBodyView`）。 */
export interface MessageBody {
  messageId: string;
  textPlain: string | null;
  /**
   * **后端消毒过的** HTML——标签与属性都在白名单里，
   * 脚本、事件处理器、远程图片、样式都没了。
   *
   * 原始 HTML 不出后端（ARCHITECTURE §7），所以这里可以**直接渲染**。
   */
  htmlSafe: string | null;
  charset: string | null;
}

/**
 * 一封邮件的一个附件（对应 `cmd::AttachmentDto`）。
 *
 * **只有元数据，没有字节**：字节留在后端的磁盘上（`<数据目录>/attachments/…`），
 * 前端要的是「下载 / 打开 / 另存为」三个动作，从不经 IPC 搬运内容。
 */
export interface Attachment {
  id: string;
  /** 文件名（可能已经解码过 RFC 2231 / RFC 2047；后端保证不含路径分隔符）。 */
  filename: string;
  /** 媒体类型，例如 `application/pdf`。 */
  contentType: string;
  /** 解码后的字节数；`0` 表示未知。 */
  size: number;
  /** 内联图片的 Content-ID（去掉了尖括号），普通附件为 `null`。 */
  contentId: string | null;
  /** 是不是正文里引用的内联资源（`cid:` 图片）。 */
  isInline: boolean;
  /** 本地是否已经有这份文件。 */
  downloaded: boolean;
  /** 落盘时间（毫秒时间戳）。 */
  downloadedAtMs: number | null;
  /**
   * 本地文件的绝对路径——**只用于展示**（「已保存到 …」）。
   *
   * 前端从不读它：打开 / 另存为都走后端命令。
   */
  localPath: string | null;
}

/** 一次下载的结果（对应 `service::attachment::AttachmentDownload`）。 */
export interface AttachmentDownload {
  attachmentId: string;
  /** 本地文件的绝对路径（展示用，前端不读它）。 */
  path: string;
  /** 建议的文件名（另存为时作为默认名）。 */
  name: string;
  size: number;
  sha256: string;
  /** `true` 表示本地早就有了，没有再去服务器取。 */
  cached: boolean;
}

/** 同步结果（对应 `cmd::SyncResult`）。 */export interface SyncResult {
  folders: number;
  added: number;
  removed: number;
}

/** 应用基本信息（对应 Rust 侧的 `AppInfo`）。 */
export interface AppInfo {
  name: string;
  version: string;
  platform: string;
  dataDir: string | null;
}

/** IMAP 标记位（与 `xremail_core::model::MessageFlags` 一致）。 */
export const FLAG_SEEN = 1 << 0;
export const FLAG_ANSWERED = 1 << 1;
export const FLAG_FLAGGED = 1 << 2;
export const FLAG_DELETED = 1 << 3;
export const FLAG_DRAFT = 1 << 4;
export const FLAG_RECENT = 1 << 5;

/** 这封邮件是不是「未读」。 */
export function isUnread(message: Message): boolean {
  return (message.flags & FLAG_SEEN) === 0;
}

/** 有没有星标。 */
export function isStarred(message: Message): boolean {
  return (message.flags & FLAG_FLAGGED) !== 0;
}

/**
 * 标记位的目标（对应 `set_message_flag` 的 `flag` 参数）。
 *
 * 只暴露 IMAP 里真正有交互入口的两个位：`\Seen`（已读）与 `\Flagged`（星标）。
 */
export type FlagTarget = "seen" | "flagged";

/**
 * 标记意图的结果状态（对应 Rust 侧 `FlagIntentResult::state`）。
 *
 * 这五种状态**含义不同**，界面上必须分开说：
 *
 * - `succeeded`：服务器回读确认了，确实生效；
 * - `queued`：意图写进本地账本了，等下次同步提交（还没到服务器）；
 * - `retryable`：服务器**明确**没接受，可以重试；
 * - `uncertain`：连接断了，**不知道**服务器执行了没（已记账，不会重复执行）；
 * - `failed`：彻底失败。
 *
 * 把后三种揉成一句「操作失败」是错的：用户分不清「肯定没生效」
 * 和「不知道有没有生效」——前者重试没问题，后者重试可能重复执行。
 */
export type FlagIntentState =
  | "queued"
  | "succeeded"
  | "retryable"
  | "failed"
  | "uncertain";

/** 标记意图的结果（对应 Rust 侧 `FlagIntentResult`，camelCase 序列化）。 */
export interface FlagIntentResult {
  /** 意图已落库（本地排队中）。 */
  queued: boolean;
  /** 服务器已回读确认、确实生效了。 */
  applied: boolean;
  /** 意图现在到哪一步了。 */
  state: FlagIntentState;
  /** 给用户看的中文说明（可能是 null）。 */
  message: string | null;
}

/** 界面提示的严重级别（决定配色）。 */
export type NoticeKind = "ok" | "info" | "warn" | "unknown" | "error";

/**
 * 一条界面提示。
 *
 * 和 `App` 里的 `error` 状态分开：error 是「命令炸了」，
 * notice 是「命令成功了，但结果有区别」——成功的操作不该显示成错误。
 */
export interface Notice {
  kind: NoticeKind;
  text: string;
}

/**
 * 草稿里的一个附件（对应后端 `DraftAttachment`）。
 *
 * ## 为什么传路径而不是字节
 *
 * 原来前端把整个文件读成 `number[]` 塞进 IPC 参数：JSON 化之后体积是
 * 二进制的 3–4 倍，几 MB 的文件就能把参数撑爆，几十 MB 的更是直接没戏。
 * 改成只传**路径**，由后端自己读文件，前端内存与 IPC 参数体都小得多。
 */
export interface DraftAttachment {
  /** 文件名（显示用）。 */
  name: string;
  /** MIME 类型；**可能是空串**（前端拿不到，后端会按扩展名兜底）。 */
  mime: string;
  /** 字节数。**前端选文件时拿不到真实大小**（先填 0），后端读完文件后会用真实大小覆盖。 */
  size: number;
  /** 本机绝对路径；**空串**表示「字节已经在库里了」（回填旧草稿时的情形）。 */
  path: string;
}

/**
 * 待保存 / 待发送的邮件草稿（对应后端 `DraftInput`）。
 *
 * 嵌套结构体在 JSON 里也是 camelCase（Rust 侧 `#[serde(rename_all = "camelCase")]`），
 * 所以这里全是 camelCase 的字段名。
 */
export interface DraftInput {
  accountId: string;
  /** 已有草稿的 id；新建时**不传**这个字段（后端会新分配一个）。 */
  id?: string;
  to: string[];
  cc: string[];
  bcc: string[];
  subject: string;
  /** 纯文本正文——**始终**要有：有些客户端只读纯文本。 */
  bodyText: string;
  /** 富文本模式下的 HTML；纯文本模式下是 null（后端会消毒）。 */
  bodyHtml?: string | null;
  attachments: DraftAttachment[];
}

/** 草稿摘要（对应后端 `DraftDto`，`list_drafts` 的返回）。 */
export interface DraftDto {
  id: string;
  accountId: string;
  subject: string;
  to: string[];
  /** `list_drafts` 只列草稿态，所以这里恒为 `"draft"`。 */
  state: "draft";
  /** 最后修改时间（epoch 毫秒）。 */
  updatedAt: number;
}

/**
 * 草稿全文（对应后端 `DraftDetail`，`get_draft` 的返回）。
 *
 * 比 `DraftDto` 多了正文与附件——「接着上次那封改」要把整份内容填回编辑区。
 */
export interface DraftDetail extends DraftDto {
  cc: string[];
  bcc: string[];
  bodyText: string;
  bodyHtml: string | null;
  attachments: DraftAttachment[];
}

/**
 * 发送结果（对应后端 `SendResult`）。
 *
 * 三种状态**含义不同**，界面上必须分开说：
 *
 * - `sent`：服务器确认接受，这封信确实出去了；
 * - `uncertain`：服务器没给确认——**不知道**出去没有。
 *   这种情况下自动重发可能让用户收到两封信，所以不能自动重试；
 * - `failed`：服务器明确拒绝了，可以改完重试。
 *
 * 把后两种揉成一句「发送失败」是错的：前者重试可能重复发信，
 * 后者重试却是安全的。
 */
export interface SendResult {
  /** 邮件的 Message-ID。 */
  messageId: string;
  /**
   * 发信条目 id。
   *
   * `uncertain` 时要用它才能把这封信撤回来（`withdraw_draft`）——
   * 没有这个 id 就没法「撤回来改」。
   */
  entryId: string;
  /** sent = 服务器确认接受；uncertain = 不确定，**不能自动重发**；failed = 明确失败。 */
  state: "sent" | "uncertain" | "failed";
  /** 给用户看的一句话说明。 */
  detail: string;
}

/**
 * 把一封「结果未确认」的信**原样再提交一次**（`resend_uncertain` 的参数）。
 *
 * 与 `send_message` 的区别：这里不带草稿内容，只带条目 id——
 * 后端会把当初冻结的那份字节重新投递，Message-ID 不变，
 * 所以对端认得出「这是原来那封」，而不是又一封新邮件。
 *
 * 返回的仍是 `SendResult`：再提交一次之后照样可能 `sent` / `uncertain` / `failed`。
 */
export interface ResendUncertainArgs {
  /** 发信条目 id（`SendResult.entryId`）。 */
  entryId: string;
}

/**
 * 把标记意图的结果翻译成一句给用户看的话。
 *
 * 为什么每种状态都单独说一句：见 `FlagIntentState` 的说明——
 * 「没生效」和「不知道有没有生效」必须让用户分得清。
 */
export function describeFlagIntent(result: FlagIntentResult): Notice {
  switch (result.state) {
    case "succeeded":
      return { kind: "ok", text: "已生效" };
    case "queued":
      return { kind: "info", text: "已排队，等下次同步提交" };
    case "retryable":
      return { kind: "warn", text: "服务器没接受，稍后重试" };
    case "uncertain":
      return {
        kind: "unknown",
        text: "连接中断，不知道服务器执行了没（已记账，不会重复执行）",
      };
    case "failed":
      return {
        kind: "error",
        text: result.message ? `失败了：${result.message}` : "失败了",
      };
    default:
      // 后端加了新状态而前端没跟上：别静默吞掉。
      return { kind: "error", text: "后端返回了不认识的状态" };
  }
}
