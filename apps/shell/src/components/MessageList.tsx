import { useMemo, useState } from "react";
import VirtualList from "./VirtualList";
import type { Message } from "../types";
import { isStarred, isUnread } from "../types";

interface Props {
  messages: Message[];
  selectedUid: number | null;
  onSelect: (message: Message) => void;
  /**
   * 切换星标。
   *
   * 返回 Promise 是为了让列表知道请求还在路上——连点两次会把
   * 「加星 / 取消星」互相抵消，所以请求没回来之前按钮是禁用的。
   */
  onToggleStar: (message: Message) => void | Promise<void>;
}

/** 邮件列表。 */
function MessageList({
  messages,
  selectedUid,
  onSelect,
  onToggleStar,
}: Props) {
  // 正在提交星标意图的那封邮件（uid）；同一时刻只允许一个。
  const [pendingUid, setPendingUid] = useState<number | null>(null);

  // 新的在上。
  //
  // **必须 memo**：`[...messages].sort()` 每次渲染都重新排一遍。500 封邮件时
  // 每次列表重渲染（比如点星标、切选中的行）都要做一遍 O(n log n) 的比较，
  // 而且会产生一个新数组——子组件的 props 每次都变，跟着一起重渲染。
  // `messages` 的引用只在真的拉过一轮邮件之后才变，所以这个 memo 命中率很高。
  //
  // **位置**：必须在下面那个 `length === 0` 的提前返回**之前**——
  // 放在后面会让「空列表 → 有邮件」的那一次渲染多一个 hook，
  // React 会报 “Rendered more hooks than during the previous render”（实测 23 条红）。
  const ordered = useMemo(
    () => [...messages].sort((a, b) => b.uid - a.uid),
    [messages],
  );

  if (messages.length === 0) {
    return (
      <section className="panel">
        <h2 className="panel-title">邮件</h2>
        <p className="panel-empty">这个文件夹里没有邮件。</p>
      </section>
    );
  }

  const toggleStar = async (message: Message) => {
    setPendingUid(message.uid);
    try {
      await onToggleStar(message);
    } finally {
      setPendingUid(null);
    }
  };

  const renderRow = (message: Message) => {
    const starred = isStarred(message);
    return (
      <div className="message-row">
        <button
          type="button"
          className={
            message.uid === selectedUid
              ? "message-item message-item-active"
              : "message-item"
          }
          onClick={() => onSelect(message)}
          data-testid="message-item"
        >
          <span
            className={
              isUnread(message)
                ? "message-subject message-unread"
                : "message-subject"
            }
          >
            {message.subject || "（无主题）"}
          </span>
          <span className="message-detail">
            {message.from[0] || "（未知发件人）"}
            {message.hasAttachment && " · 附件"}
          </span>
        </button>
        {/* 星标按钮放在行尾而不是行首：行首是「选哪封」的地方，
            星标是「对这一封做什么」，分开更不容易误点。 */}
        <button
          type="button"
          className={
            starred
              ? "message-star-toggle message-star-toggle-on"
              : "message-star-toggle"
          }
          onClick={() => void toggleStar(message)}
          disabled={pendingUid === message.uid}
          aria-pressed={starred}
          aria-label={starred ? "取消星标" : "加星标"}
          title={starred ? "取消星标" : "加星标"}
        >
          {starred ? (
            <span className="message-star">★</span>
          ) : (
            <span aria-hidden="true">☆</span>
          )}
        </button>
      </div>
    );
  };

  return (
    <section className="panel message-panel">
      <h2 className="panel-title">邮件（{ordered.length}）</h2>
      {/* **虚拟列表**：只渲染视口里那十几行。
          全渲染时 500 封是 3148 个 DOM 节点、首次打开 262ms（2026-11-10 实测），
          数量再涨就线性恶化；虚拟化之后渲染行数与邮件总数无关。
          代价见 `VirtualList` 的注释：行高必须是常数。 */}
      <VirtualList
        items={ordered}
        renderItem={renderRow}
        className="message-list"
        testId="message-list"
      />
    </section>
  );
}

export default MessageList;
