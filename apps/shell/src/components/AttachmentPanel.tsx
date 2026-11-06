import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openPath } from "@tauri-apps/plugin-opener";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import type { Attachment, AttachmentDownload, Message } from "../types";

interface Props {
  accountId: string;
  folderId: string;
  message: Message;
  /** 正文加载完之后才去问附件，省一次往返。 */
  ready: boolean;
  onError: (message: string) => void;
}

/** 字节数格式成人能读的形式（与写信面板同一套写法）。 */
export function formatAttachmentSize(bytes: number): string {
  if (bytes <= 0) {
    return "大小未知";
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * 下载 / 打开过程中的状态。
 *
 * 只有「这个附件正在下载」「这个附件下载好了」这两种瞬态需要记；
 * 下载好之后的正经状态在 `attachment.downloaded` 里（后端说了算）。
 */
interface PendingState {
  attachmentId: string;
  kind: "downloading" | "opening" | "saving";
}

/**
 * 一封邮件的附件列表。
 *
 * ## 为什么字节不走 IPC
 *
 * 附件可能有十几 MB。把字节塞进 IPC 消息里（哪怕 base64 包一层），
 * 意味着整份内容要在 webview 和 Rust 之间复制一遍、并且在 JS 堆里再存一份。
 * 所以后端只回**元数据**，要「打开」就传路径给系统，要「另存为」就让后端复制文件
 * （`save_attachment_as`）。前端从头到尾没见过附件内容。
 *
 * ## 内联图片（`cid:`）不在这里
 *
 * 正文里 `cid:` 图片由**后端**替换成 `data:` URI（只对本封邮件里真正下载过的
 * `image/*` 部件，见 `xremail_core::inline_image`）。这里只列「当成文件」的附件，
 * 免得同一张图在正文里出现一次、在列表里又出现一次。
 */
export default function AttachmentPanel({
  accountId,
  folderId,
  message,
  ready,
  onError,
}: Props) {
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [pending, setPending] = useState<PendingState | null>(null);
  /**
   * 刚刚落盘的那份文件（用于「已保存到 …」的可点击提示）。
   *
   * 路径来自后端的返回，不是前端拼的——用户要的是「这次下载的东西在哪」。
   */
  const [lastSaved, setLastSaved] = useState<{
    attachmentId: string;
    path: string;
  } | null>(null);

  useEffect(() => {
    if (!ready) {
      return;
    }
    let disposed = false;
    setAttachments([]);
    setLastSaved(null);

    void invoke<Attachment[]>("list_attachments", { messageId: message.id })
      .then((result) => {
        if (!disposed) {
          setAttachments(result);
        }
      })
      .catch((e) => {
        if (!disposed) {
          onError(String(e));
        }
      });

    return () => {
      disposed = true;
    };
  }, [ready, message.id, onError]);

  /** 把列表里的某一条换成下载后的状态。 */
  const markDownloaded = useCallback(
    (attachmentId: string, download: AttachmentDownload) => {
      setAttachments((current) =>
        current.map((one) =>
          one.id === attachmentId
            ? {
                ...one,
                downloaded: true,
                downloadedAtMs: one.downloadedAtMs ?? Date.now(),
                localPath: download.path,
                size: download.size > 0 ? download.size : one.size,
              }
            : one,
        ),
      );
    },
    [],
  );

  const download = useCallback(
    async (attachment: Attachment): Promise<AttachmentDownload | null> => {
      setPending({ attachmentId: attachment.id, kind: "downloading" });
      try {
        const result = await invoke<AttachmentDownload>("download_attachment", {
          messageId: message.id,
          accountId,
          folderId,
          uid: message.uid,
          attachmentId: attachment.id,
        });
        markDownloaded(attachment.id, result);
        setLastSaved({ attachmentId: attachment.id, path: result.path });
        return result;
      } catch (e) {
        onError(String(e));
        return null;
      } finally {
        setPending(null);
      }
    },
    [accountId, folderId, message.uid, markDownloaded, onError],
  );

  /**
   * 打开：先确保本地有，再交给系统默认程序。
   *
   * 已经下载过的不会再去服务器取（后端 `existing()` 短路）。
   */
  const open = useCallback(
    async (attachment: Attachment) => {
      if (!attachment.downloaded) {
        // 没下载过就先下载，成功了再开。
        const result = await download(attachment);
        if (!result) {
          return;
        }
      }
      setPending({ attachmentId: attachment.id, kind: "opening" });
      try {
        // 打开的是**后端给过的本地路径**，不是前端拼出来的：
        // 后端只放行它自己记账过、且确实落在数据目录里的文件。
        await invoke("open_attachment", {
          messageId: message.id,
          attachmentId: attachment.id,
        });
      } catch (e) {
        onError(String(e));
      } finally {
        setPending(null);
      }
    },
    [download, message.id, onError],
  );

  /**
   * 另存为：先问用户存哪，再让后端把文件复制过去。
   *
   * 复制在 Rust 侧做（`std::fs::copy`），内容不过 JS。
   */
  const saveAs = useCallback(
    async (attachment: Attachment) => {
      const target = await saveFileDialog({
        defaultPath: attachment.filename,
      });
      if (!target) {
        return;
      }
      setPending({ attachmentId: attachment.id, kind: "saving" });
      try {
        const saved = await invoke<string>("save_attachment_as", {
          messageId: message.id,
          attachmentId: attachment.id,
          targetPath: target,
        });
        setLastSaved({ attachmentId: attachment.id, path: saved });
      } catch (e) {
        onError(String(e));
      } finally {
        setPending(null);
      }
    },
    [message.id, onError],
  );

  const revealSaved = useCallback((path: string) => {
    // 交给系统文件管理器；失败不该影响阅读。
    void openPath(path).catch(() => undefined);
  }, []);

  if (attachments.length === 0) {
    return null;
  }

  return (
    <div className="message-attachments" data-testid="message-attachments">
      <h3 className="message-attachments-title">
        附件（{attachments.length}）
      </h3>
      <ul className="message-attachment-list">
        {attachments.map((attachment) => {
          const busy = pending?.attachmentId === attachment.id;
          return (
            <li
              key={attachment.id}
              className="message-attachment"
              data-testid="message-attachment"
            >
              <span className="message-attachment-name" title={attachment.contentType}>
                {attachment.filename}
              </span>
              <span className="message-detail">
                {formatAttachmentSize(attachment.size)}
                {attachment.downloaded ? " · 已在本地" : ""}
              </span>
              <span className="message-attachment-actions">
                <button
                  type="button"
                  className="message-flag-button"
                  data-testid="attachment-download"
                  disabled={busy || attachment.downloaded}
                  onClick={() => void download(attachment)}
                >
                  {attachment.downloaded
                    ? "已下载"
                    : busy
                      ? "下载中…"
                      : "下载"}
                </button>
                <button
                  type="button"
                  className="message-flag-button"
                  data-testid="attachment-open"
                  disabled={busy}
                  onClick={() => void open(attachment)}
                >
                  打开
                </button>
                <button
                  type="button"
                  className="message-flag-button"
                  data-testid="attachment-save-as"
                  disabled={busy || !attachment.downloaded}
                  onClick={() => void saveAs(attachment)}
                >
                  另存为…
                </button>
              </span>
            </li>
          );
        })}
      </ul>

      {lastSaved && (
        <p className="message-attachment-saved" data-testid="attachment-saved">
          已保存到{" "}
          <button
            type="button"
            className="message-attachment-link"
            data-testid="attachment-reveal"
            onClick={() => revealSaved(lastSaved.path)}
          >
            {lastSaved.path}
          </button>
          <span className="message-detail">
            （点一下用文件管理器打开所在位置）
          </span>
        </p>
      )}
    </div>
  );
}





