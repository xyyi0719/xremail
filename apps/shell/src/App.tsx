import AccountForm from "./components/AccountForm";
import AccountList from "./components/AccountList";
import ComposePanel from "./components/ComposePanel";
import FolderList from "./components/FolderList";
import MessageList from "./components/MessageList";
import MessageView from "./components/MessageView";
import SearchPanel, { type SearchHit } from "./components/SearchPanel";
import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  Account,
  AppInfo,
  FlagIntentResult,
  FlagTarget,
  Folder,
  Message,
  Notice,
  SyncResult,
} from "./types";
import { describeFlagIntent, isStarred, isUnread } from "./types";
import "./App.css";

/**
 * 账户相关的事件名。
 *
 * 事件是**刷新提示**，不是数据来源（见 ARCHITECTURE §1.6）：
 * 收到后重新查询，漏事件也能靠重查恢复。
 */
const ACCOUNT_EVENTS = [
  "account://added",
  "account://updated",
  "account://removed",
] as const;

/**
 * 后台同步的状态事件。
 *
 * 只对“同步完了”这个状态刷一次：调度器每轮会报两次
 * （`connecting` 开始、`online` 结束），两次都重查等于白拉一倍数据。
 */
const SYNC_EVENT = "sync://state-changed";

/** 后台一轮同步落地后报的状态（见 Rust 侧 `SyncState::Online`）。 */
const SYNC_DONE_STATE = "online";

/** `sync://state-changed` 的载荷形状（对应 Rust 侧 `AppEvent`）。 */
type SyncStateEvent = {
  payload?: { state?: string };
};

function App() {
  const [accounts, setAccounts] = useState<Account[] | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  // 「命令成功了，但结果有区别」的反馈走这条通道，跟 `error` 分开：
  // 成功的操作不能显示成错误。
  const [notice, setNotice] = useState<Notice | null>(null);

  // 当前浏览的账户 / 文件夹 / 邮件。
  const [accountId, setAccountId] = useState<string | null>(null);
  const [folders, setFolders] = useState<Folder[]>([]);
  const [folderId, setFolderId] = useState<string | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [message, setMessage] = useState<Message | null>(null);

  // 写信面板：存的是**打开面板时**那个账户的 id——
  // 用户中途换账户的话，面板里的收件人还是按开面板时的身份准备的。
  const [composeAccountId, setComposeAccountId] = useState<string | null>(null);

  // 待打开的搜索命中。
  //
  // 搜索结果点开后要“先切账户 → 再切文件夹 → 再等邮件列表加载完 → 才能打开邮件”，
  // 中间隔着两次异步加载，所以先把目标记在这儿，等列表到了再兑现。
  const [pendingOpen, setPendingOpen] = useState<SearchHit | null>(null);

  const refresh = useCallback(async () => {
    try {
      setAccounts(await invoke<Account[]>("list_accounts"));
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    void invoke<AppInfo>("app_info")
      .then(setInfo)
      .catch((e) => setError(String(e)));
  }, [refresh]);

  useEffect(() => {
    let disposed = false;
    const unlisteners: UnlistenFn[] = [];

    for (const name of ACCOUNT_EVENTS) {
      void listen(name, () => {
        void refresh();
      })
        .then((unlisten) => {
          if (disposed) {
            unlisten();
          } else {
            unlisteners.push(unlisten);
          }
        })
        .catch((e) => setError(String(e)));
    }

    return () => {
      disposed = true;
      for (const unlisten of unlisteners) {
        unlisten();
      }
    };
  }, [refresh]);

  // 后台同步（调度器的 actor）跑完一轮 → 重查。
  //
  // 这是“邮箱自己跟上服务器”对用户可见的唯一途径：没有这条订阅，
  // 后台拉到的新邮件要等到下一次手动操作才会出现在列表里。
  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | null = null;

    void listen<SyncStateEvent>(SYNC_EVENT, (event) => {
      const state = event.payload?.payload?.state;
      // 未知形状也刷新：宁可多查一次，不要让后台同步看不见。
      if (state && state !== SYNC_DONE_STATE) {
        return;
      }
      void refresh();
    })
      .then((disposeListener) => {
        if (disposed) {
          disposeListener();
        } else {
          unlisten = disposeListener;
        }
      })
      .catch((e) => setError(String(e)));

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refresh]);

  // 切换账户 → 重新拉文件夹。
  useEffect(() => {
    if (!accountId) {
      setFolders([]);
      setFolderId(null);
      setMessages([]);
      setMessage(null);
      return;
    }
    void invoke<Folder[]>("list_folders", { accountId })
      .then(setFolders)
      .catch((e) => setError(String(e)));
  }, [accountId]);

  // 切换文件夹 → 重新拉邮件。
  useEffect(() => {
    if (!accountId || !folderId) {
      setMessages([]);
      setMessage(null);
      return;
    }
    void invoke<Message[]>("list_messages", { accountId, folderId })
      .then(setMessages)
      .catch((e) => setError(String(e)));
  }, [accountId, folderId]);

  // 点开一条搜索命中：切到对应的账户与文件夹，等列表到了再打开那封邮件。
  const handleOpenHit = useCallback((hit: SearchHit) => {
    setPendingOpen(hit);
    setAccountId(hit.accountId);
    setFolderId(hit.folderId);
  }, []);

  // 邮件列表加载完 → 兑现“待打开的命中”。
  useEffect(() => {
    if (!pendingOpen || folderId !== pendingOpen.folderId || messages.length === 0) {
      return;
    }
    const found = messages.find((item) => item.uid === pendingOpen.uid);
    if (found) {
      setMessage(found);
      setPendingOpen(null);
    }
  }, [pendingOpen, folderId, messages]);

  const handleDelete = useCallback(
    async (id: string) => {
      try {
        await invoke<boolean>("delete_account", { id });
        // 事件会触发刷新；这里也直接刷一次，事件丢了也不影响结果。
        await refresh();
      } catch (e) {
        setError(String(e));
      }
    },
    [refresh],
  );

  const handleSynced = useCallback(
    async (_result: SyncResult) => {
      // 同步完成后刷新文件夹与邮件。
      if (accountId) {
        const next = await invoke<Folder[]>("list_folders", { accountId });
        setFolders(next);
        if (folderId) {
          setMessages(await invoke<Message[]>("list_messages", { accountId, folderId }));
        }
      }
    },
    [accountId, folderId],
  );

  const handleError = useCallback((message: string) => setError(message), []);

  const handleNotice = useCallback((n: Notice) => setNotice(n), []);

  /**
   * 提交一个标记意图（已读 / 星标），然后重新拉一次列表。
   *
   * 为什么不直接在前端把位改了：后端维护着一份「未提交意图」的账本，
   * `list_messages` 会把它叠在服务器快照上返回。重新拉一次拿到的是
   * 后端账本的真实状态，前端不用自己猜，也不用维护乐观更新。
   */
  const handleSetFlag = useCallback(
    async (target: Message, flag: FlagTarget, value: boolean) => {
      try {
        const result = await invoke<FlagIntentResult>("set_message_flag", {
          accountId: target.accountId,
          folderId: target.folderId,
          uid: target.uid,
          flag,
          value,
        });
        // 五种状态有五种说法，成功 / 排队 / 不确定各说各的。
        setNotice(describeFlagIntent(result));
      } catch (e) {
        setError(String(e));
        return;
      }

      try {
        const next = await invoke<Message[]>("list_messages", {
          accountId: target.accountId,
          folderId: target.folderId,
        });
        // 提交期间用户可能已经换文件夹了——那这份列表属于旧文件夹，
        // 写进去会把新文件夹的邮件冲掉。
        if (accountId !== target.accountId || folderId !== target.folderId) {
          return;
        }
        setMessages(next);
        // 顺手把打开着的那封也换成最新的：否则正文面板里的
        // 「标记为已读 / 未读」按钮还按旧位显示。
        setMessage((prev) => {
          if (!prev || prev.uid !== target.uid) {
            return prev;
          }
          return next.find((m) => m.uid === target.uid) ?? prev;
        });
      } catch (e) {
        setError(String(e));
      }
    },
    [accountId, folderId],
  );

  const handleToggleStar = useCallback(
    (target: Message) => handleSetFlag(target, "flagged", !isStarred(target)),
    [handleSetFlag],
  );

  const handleToggleSeen = useCallback(
    // `value = true` 的含义是「加上 IMAP 的 \Seen 位」＝标记已读，
    // 所以当前**未读**时传 true、已读时传 false。
    // （传反了会变成永远不生效的空操作：未读时去清一个本来就没置的位。）
    (target: Message) => handleSetFlag(target, "seen", isUnread(target)),
    [handleSetFlag],
  );

  // 写信面板挂在哪个账户上（账户列表里找它的邮箱，用来显示给用户看）。
  const composeAccount = useMemo(
    () => accounts?.find((account) => account.id === composeAccountId) ?? null,
    [accounts, composeAccountId],
  );

  return (
    <main className="app-shell">
      <header className="app-header">
        <div>
          <h1 className="app-title">Xremail</h1>
          <p className="app-subtitle">本地邮件客户端</p>
        </div>
        {info && (
          <dl className="app-info">
            <dt>版本</dt>
            <dd>
              {info.name} {info.version}（{info.platform}）
            </dd>
            <dt>数据目录</dt>
            <dd className="app-info-path">{info.dataDir ?? "—"}</dd>
          </dl>
        )}
      </header>

      {error && (
        <p className="app-error" data-testid="app-error">
          操作失败：{error}
        </p>
      )}

      {notice && (
        <div
          className={`app-notice app-notice-${notice.kind}`}
          role="status"
          data-testid="app-notice"
        >
          <span>{notice.text}</span>
          <button
            type="button"
            className="app-notice-close"
            aria-label="关闭提示"
            onClick={() => setNotice(null)}
          >
            ×
          </button>
        </div>
      )}

      <AccountForm />

      <SearchPanel onOpen={handleOpenHit} onError={handleError} />

      {accounts === null ? (
        <p className="app-loading">加载中…</p>
      ) : (
        <>
          <AccountList
            accounts={accounts}
            onSelect={(account) => setAccountId(account.id)}
            onDelete={handleDelete}
            onSynced={handleSynced}
            onError={handleError}
            onNotice={handleNotice}
            onChanged={refresh}
          />

          {accountId && (
            <>
              <div className="compose-launch">
                <button
                  type="button"
                  className="compose-launch-button"
                  onClick={() => setComposeAccountId(accountId)}
                >
                  写邮件
                </button>
                <span className="compose-launch-hint">
                  用选中的账户发信，改动会自动存草稿
                </span>
              </div>

              {composeAccountId && composeAccount && (
                <ComposePanel
                  accountId={composeAccount.id}
                  accountEmail={composeAccount.email}
                  onClose={() => setComposeAccountId(null)}
                />
              )}

              <FolderList
                folders={folders}
                selectedId={folderId}
                onSelect={(folder) => setFolderId(folder.id)}
              />

              {folderId && (
                <>
                  <MessageList
                    messages={messages}
                    selectedUid={message?.uid ?? null}
                    onSelect={setMessage}
                    onToggleStar={handleToggleStar}
                  />

                  {message && (
                    <MessageView
                      accountId={accountId}
                      folderId={folderId}
                      message={message}
                      onToggleSeen={handleToggleSeen}
                      onError={handleError}
                    />
                  )}
                </>
              )}
            </>
          )}
        </>
      )}
    </main>
  );
}

export default App;
