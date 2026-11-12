import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import AttachmentPanel from "./AttachmentPanel";
import { Archive, Trash2, ChevronDown } from "lucide-react";
import type { Message, MessageBody } from "../types";
import { isUnread } from "../types";

interface Props {
  accountId: string;
  folderId: string;
  message: Message;
  /** 切换已读 / 未读。 */
  onToggleSeen: (message: Message) => void | Promise<void>;
  onError: (message: string) => void;
  /**
   * 附件列表拉不出来时的回调，和 `onError` **分开**。
   *
   * `onError`（App 的 `handleError`）除了记错误还会清掉当前选中的邮件；
   * 附件拉不出来不该让整封信消失，所以这里给一条独立的通道。
   */
  onAttachmentError: (message: string) => void;
}

/**
 * 邮件正文。
 *
 * ## 为什么可以渲染 HTML
 *
 * 渲染的是**后端消毒过的** HTML（`htmlSafe`），不是原始邮件正文：
 * 消毒在后端完成（`xremail-core::html`），脚本、事件处理器、
 * 远程图片都已经没了，**原始 HTML 根本不出后端**（ARCHITECTURE §9.1）。
 * 前端再渲染一次，等于让邮件内容拿到一个 XSS 口子。
 *
 * 真实情况：国内服务商的邮件**大多是 HTML-only**（没有 text/plain），
 * 实测 163 / QQ / 沃三家的抽样邮件纯文本都是 0 字。
 * 只显示源码的话，这类邮件等于**读不了**。
 *
 * ## 为什么链接不让应用自己跳
 *
 * 邮件里的 `<a href="https://…">` 一旦让 webview 自己跳，
 * 整个应用窗口就被那个网页顶掉了——用户的邮件客户端**没了**。
 * 所以正文里的链接一律交给**系统浏览器**打开（`opener` 插件）。
 *
 * ## 为什么打开邮件不自动标记已读
 *
 * 用户可能只是瞄一眼、扫一眼列表，结果服务器上的已读位被改了——
 * 那是**改变了他的邮箱状态**，而他并没有要求过。所以这里只给一个
 * 显式的「标记为已读 / 标记为未读」按钮，改不改由用户决定。
 */
function MessageView({
  accountId,
  folderId,
  message,
  onToggleSeen,
  onError,
  onAttachmentError,
}: Props) {
  const [body, setBody] = useState<MessageBody | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let disposed = false;
    setBody(null);
    setLoading(true);

    void invoke<MessageBody | null>("open_message", {
      accountId,
      folderId,
      uid: message.uid,
    })
      .then((result) => {
        if (!disposed) {
          setBody(result);
        }
      })
      .catch((e) => {
        if (!disposed) {
          onError(String(e));
        }
      })
      .finally(() => {
        if (!disposed) {
          setLoading(false);
        }
      });

    return () => {
      disposed = true;
    };
  }, [accountId, folderId, message.uid, onError]);

  const plain = body?.textPlain ?? null;
  const html = body?.htmlSafe ?? null;

  /**
   * 正文里的链接一律交给系统浏览器。
   *
   * 邮件内容不可信：让 webview 自己跳会把整个应用窗口换成那个网页。
   * 这里 `preventDefault` 掉，然后交给 `opener` 插件开系统浏览器。
   */
  const handleBodyClick = (event: React.MouseEvent<HTMLDivElement>) => {
    const anchor = (event.target as HTMLElement).closest("a");
    const href = anchor?.getAttribute("href");
    if (!href) {
      return;
    }
    // 不管什么链接都不让应用自己跳。
    event.preventDefault();
    if (!/^(https?:|mailto:)/i.test(href)) {
      return;
    }
    void openUrl(href).catch(() => {
      // 打开失败（比如系统没有浏览器）不该把整个应用搞崩。
    });
  };

  return (
    <section className="panel message-view">
      {/* 顶部工具条：**sticky**，读长邮件时"归档/删除/标记"始终在眼前。
          以前这些操作在标题下方，滚到正文中段就找不到了。 */}
      <div className="reader-bar">
        <button type="button" className="icon-btn" title="归档" aria-label="归档">
          <Archive size={16} strokeWidth={1.9} />
        </button>
        <button type="button" className="icon-btn" title="删除" aria-label="删除">
          <Trash2 size={16} strokeWidth={1.9} />
        </button>
        <span className="grow" />
        <span className="reader-status">
          {isUnread(message) ? "未读" : "已读"}
        </span>
        <button type="button" className="icon-btn" title="更多" aria-label="更多">
          <ChevronDown size={16} strokeWidth={1.9} />
        </button>
      </div>

      <div className="reader-inner">
        <h2 className="subject">{message.subject || "（无主题）"}</h2>

        <div className="sender">
          <span className="av" aria-hidden="true">
            {(message.from[0] ?? "?").trim().charAt(0).toUpperCase() || "?"}
          </span>
          <div className="who">
            <b>{message.from.join(", ") || "（未知发件人）"}</b>
            <span>
              {message.to.length > 0 ? `发给 ${message.to.join(", ")}` : "（无收件人）"}
            </span>
          </div>
          <div className="acts">
            {/* 标记已读/未读仍在这里保留一个文字按钮——它是测试与键盘都依赖的入口 */}
            <button
              type="button"
              className="chip message-flag-button"
              onClick={() => void onToggleSeen(message)}
            >
              {isUnread(message) ? "标记为已读" : "标记为未读"}
            </button>
          </div>
        </div>

        {loading && <p className="panel-empty">正在取正文…</p>}

        {!loading && html && (
          // 消毒过的 HTML：标签与属性都在白名单里，
          // 脚本、事件处理器、远程图片都没了（ARCHITECTURE §9.1）。
          <div
            className="message-body message-body-html"
            onClick={handleBodyClick}
            dangerouslySetInnerHTML={{ __html: html }}
          />
        )}

        {!loading && !html && plain && <pre className="message-body">{plain}</pre>}

        {!loading && !html && !plain && (
          <p className="panel-empty">这封邮件没有正文。</p>
        )}
      </div>

      {/*
        附件在正文**下面**：先读信再看附件，和所有邮件客户端一样。
        正文还在取的时候先不问附件（省一次往返）。
      */}
      <div className="reader-inner reader-attachments">
        <AttachmentPanel
          accountId={accountId}
          folderId={folderId}
          message={message}
          ready={!loading}
          onError={onAttachmentError}
        />
      </div>
    </section>
  );
}

export default MessageView;
