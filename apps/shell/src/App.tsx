import AccountForm from "./components/AccountForm";
import AccountList from "./components/AccountList";
import FolderRail from "./components/FolderRail";
import AccountSwitcher from "./components/AccountSwitcher";
import { Settings } from "lucide-react";
import ComposePanel from "./components/ComposePanel";
import MessageList from "./components/MessageList";
import MessageView from "./components/MessageView";
import SearchPanel, { type SearchHit } from "./components/SearchPanel";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
import { loadAttachmentLimits } from "./attachmentLimits";
import "./tailwind.css";
import { Skeleton } from "./components/ui/skeleton";
import { Sheet, SheetTrigger, SheetContent, SheetHeader, SheetTitle, SheetBody } from "./components/ui/sheet";
import "./tokens.css";
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
  // 设置区（添加账户 / 搜索 / 账户与 DKIM）默认**收起**。
  //
  // 以前它们是和邮件列表一起纵向堆在同一页里的：登录之后要先滚过大半屏的
  // 设置区才看到邮件——日常收发时那半屏是纯粹的干扰，而且每次滚动都要
  // 重新布局整页。收起之后首屏就是「文件夹 + 列表 + 正文」三栏。
  const [settingsOpen, setSettingsOpen] = useState(false);
  // Ctrl+K 打开抽屉时，`onOpenAutoFocus` 里要把焦点交给**搜索框**而不是默认的第一格。
  const focusSearchOnOpen = useRef(false);
  const [accounts, setAccounts] = useState<Account[] | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  // 附件（读信侧）的错误单独一条通道，理由见 `handleAttachmentError`。
  const [attachmentError, setAttachmentError] = useState<string | null>(null);
  // 「命令成功了，但结果有区别」的反馈走这条通道，跟 `error` 分开：
  // 成功的操作不能显示成错误。
  const [notice, setNotice] = useState<Notice | null>(null);

  // 成功类提示 6 秒后自动消失；error / warn 留着（用户必须看到，且可手动关）。
  // 定时器随 notice 变化重建——连续两条成功提示时，重置计时而不是叠加。
  useEffect(() => {
    if (notice === null || (notice.kind !== "ok" && notice.kind !== "info")) {
      return;
    }
    const timer = window.setTimeout(() => setNotice(null), 6_000);
    return () => window.clearTimeout(timer);
  }, [notice]);

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
    // 附件上限在后端（`xremail-core::attachment`），启动时问一次并缓存：
    // 写信面板的校验是同步的，拿不到就交给后端拦。
    void loadAttachmentLimits((cmd) => invoke<unknown>(cmd));
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

  /**
   * 附件的错误单独记一条。
   *
   * 正文那边传下来的 `handleError` 除了记错误之外，还会清掉当前选中的邮件
   * （那是对的：正文拉不出来，留在这一封也没意义）。但附件列表拉不出来
   * 不该让整封信消失——用户还能读正文、还看得见其他附件。所以附件自己
   * 一条错误通道，显示在主错误下面，换一封邮件时清掉。
   */
  const handleAttachmentError = useCallback(
    (message: string) => setAttachmentError(message),
    [],
  );

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

  // =========================================================================
  // 键盘导航（UI_DESIGN §2.3）
  //
  // **硬规则：焦点在输入类元素里时必须全部失效**——用户在写收件人时按 `s`
  // 是打字母，不是加星标。判断的是 `event.target`，不是 document.activeElement：
  // 事件从哪发出就从哪拦。
  //
  // 列表顺序与 MessageList 的展示顺序一致（uid 倒序，新的在上）；
  // 这里用 useMemo 与它共享同一份排序结果。
  // =========================================================================
  const orderedForKeys = useMemo(
    () => [...messages].sort((a, b) => b.uid - a.uid),
    [messages],
  );

  // 把列表里第 index 行滚进视野。
  //
  // **必须做 `typeof` 守卫**：jsdom 没有实现 `Element.prototype.scrollIntoView`，
  // 直接调用在 CI 的单元测试里是 TypeError（本地有 polyfill 时发现不了——
  // 2026-11-10 CI 上 Frontend job 因此红过一次）。键盘导航的核心是
  // 「选中会变」，滚动只是锦上添花，环境不支持就安静跳过。
  const scrollToIndex = (index: number) => {
    const row = document.querySelectorAll('[data-testid="message-item"]')[index];
    if (row && typeof row.scrollIntoView === "function") {
      row.scrollIntoView({ block: "nearest" });
    }
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const editable =
        target !== null &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.tagName === "SELECT" ||
          target.isContentEditable);
      if (editable) {
        return;
      }

      // `Ctrl/⌘+K`：聚焦搜索框。搜索框住在设置抽屉里，先开抽屉再聚焦——
      // 抽屉是非模态的（UI_DESIGN §4.1），聚焦后主界面仍可用。
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        focusSearchOnOpen.current = true;
        setSettingsOpen(true);
        return;
      }

      if (event.ctrlKey || event.metaKey || event.altKey) {
        return;
      }

      // 设置抽屉开着时，单键导航让位（抽屉里有自己的表单快捷键语义）。
      if (settingsOpen) {
        return;
      }

      const currentIndex = message === null ? -1 : orderedForKeys.findIndex((m) => m.uid === message.uid);

      switch (event.key) {
        case "j":
        case "ArrowDown": {
          // 从「未选中」按 j：选中第一封（最新的）。
          const next = orderedForKeys[currentIndex + 1] ?? orderedForKeys[0];
          if (next) {
            event.preventDefault();
            setMessage(next);
            scrollToIndex(orderedForKeys.indexOf(next));
          }
          return;
        }
        case "k":
        case "ArrowUp": {
          const prev = currentIndex <= 0 ? null : orderedForKeys[currentIndex - 1];
          if (prev) {
            event.preventDefault();
            setMessage(prev);
            scrollToIndex(orderedForKeys.indexOf(prev));
          }
          return;
        }
        case "s": {
          if (message !== null) {
            event.preventDefault();
            void handleToggleStar(message);
          }
          return;
        }
        case "c": {
          // 写新邮件：需要先有账户。
          if (accountId !== null) {
            event.preventDefault();
            setComposeAccountId(accountId);
          }
          return;
        }
        case "Escape": {
          // 关最上层：先关写信浮层，再清空正文选择。
          if (composeAccountId !== null) {
            event.preventDefault();
            setComposeAccountId(null);
          } else if (message !== null) {
            event.preventDefault();
            setMessage(null);
          }
          return;
        }
        default:
          return;
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [
    orderedForKeys,
    message,
    accountId,
    composeAccountId,
    settingsOpen,
    handleToggleStar,
  ]);

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
        {/* 账户切换在顶栏：切换账户是高频动作，不该藏在设置抽屉里。
            多个账户时横向排开，当前账户高亮。 */}
        {accounts !== null && (
          <div className="topbar-accounts">
            <AccountSwitcher
              accounts={accounts}
              selectedId={accountId}
              onSelect={(account) => setAccountId(account.id)}
            />
          </div>
        )}
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
      {/* 「账户与设置」收进右侧抽屉：设置是低频动作，不该把邮件推走。
          打开时 Sheet 会挂 Portal 并锁列表滚动；关闭即卸载（UI_DESIGN §5.2）。 */}
      <Sheet modal={false} open={settingsOpen} onOpenChange={setSettingsOpen}>
        <SheetTrigger asChild>
          <button
            type="button"
            className="settings-toggle"
            data-testid="settings-toggle"
          >
            <Settings size={16} strokeWidth={1.9} />
            <span className="settings-label">账户与设置</span>
          </button>
        </SheetTrigger>
        <SheetContent
          onOpenAutoFocus={(event) => {
            if (!focusSearchOnOpen.current) {
              // ⚙ 点开：让 Radix 默认聚焦第一格表单。
              return;
            }
            // **Ctrl+K 进来：焦点必须落在搜索框上**。
            // 不能用 setTimeout/rAF 去抢——Radix 的默认聚焦会在挂载后
            // 再移一次焦点，比任何定时器都晚（实测输入落空）。
            // 正确姿势是拦下它自己的 onOpenAutoFocus，此刻内容已挂载。
            event.preventDefault();
            focusSearchOnOpen.current = false;
            document.querySelector<HTMLInputElement>('input[type="search"]')?.focus();
          }}
        >
          <SheetHeader>
            <SheetTitle className="text-lg font-semibold">账户与设置</SheetTitle>
          </SheetHeader>
          <SheetBody className="flex flex-col gap-6">
            {/* `accounts` 在首帧是 `null`（还没拉回来）。加载态由下面的
                「加载中…」负责，这里只在拿到了之后才渲染管理卡。 */}
            {accounts !== null && (
              <AccountList
                accounts={accounts}
                onSelect={(account) => setAccountId(account.id)}
                onDelete={handleDelete}
                onSynced={handleSynced}
                onError={handleError}
                onNotice={handleNotice}
                onChanged={refresh}
              />
            )}
            <AccountForm onCreated={refresh} />
            <SearchPanel onOpen={handleOpenHit} onError={handleError} />
          </SheetBody>
        </SheetContent>
      </Sheet>

      </header>

      {error && (
        <p className="app-error" data-testid="app-error">
          操作失败：{error}
        </p>
      )}

      {attachmentError && (
        <p className="app-error" data-testid="attachment-error" role="alert">
          附件：{attachmentError}
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

      {/* 骨架屏：占位与真实三栏**同形状**，数据到位后布局不跳。
          比「加载中…」三个字好——那会让首屏从一行字突变成整页。 */}
      {accounts === null ? (
        <div className="mail-layout" aria-label="加载中">
          <aside className="mail-sidebar">
            <Skeleton className="h-9 w-full" />
            <Skeleton className="h-8 w-3/4" />
            <Skeleton className="h-24 w-full" />
          </aside>
          <section className="mail-list-pane">
            <Skeleton className="mb-3 h-6 w-24" />
            <Skeleton className="mb-2 h-12 w-full" />
            <Skeleton className="mb-2 h-12 w-full" />
            <Skeleton className="h-12 w-full" />
          </section>
          <section className="mail-reading-pane">
            <Skeleton className="mb-3 h-7 w-1/2" />
            <Skeleton className="h-20 w-full" />
          </section>
        </div>
      ) : (
        <>
          {/* `data-mobile-view` 让 CSS 在窄屏做「列表 ↔ 详情」整页切换：
              选中邮件时只显示正文，否则只显示列表。参考 Gmail 移动端——
              手机上三栏并排是没法用的（每栏只剩一百多像素）。 */}
          <div
            className="mail-layout"
            data-testid="mail-layout"
            data-mobile-view={message !== null ? "detail" : "list"}
          >
            {/* 左栏：账户 + 文件夹 + 写信入口。窄、不怎么变，不随滚动重排。 */}
            <aside className="mail-sidebar">
              <FolderRail
                folders={folders}
                selectedId={folderId}
                onSelect={(folder) => setFolderId(folder.id)}
                onCompose={() => accountId && setComposeAccountId(accountId)}
                composeDisabled={!accountId}
              />
            </aside>

            {/* 中栏：邮件列表。只渲染摘要，点开正文不重建这一列。 */}
            <section className="mail-list-pane" aria-label="邮件列表">
              {accountId && folderId && (
                <MessageList
                  messages={messages}
                  selectedUid={message?.uid ?? null}
                  onSelect={setMessage}
                  onToggleStar={handleToggleStar}
                />
              )}
            </section>

            {/* 右栏：正文。 */}
            <section className="mail-reading-pane" aria-label="邮件正文">
              {/* 移动端专用：返回列表。桌面端由 CSS 隐藏（三栏并排时不需要）。 */}
              <button
                type="button"
                className="mobile-back"
                onClick={() => setMessage(null)}
                aria-label="返回列表"
              >
                ← 返回
              </button>
              {accountId && folderId && message && (
                <MessageView
                  accountId={accountId}
                  folderId={folderId}
                  message={message}
                  onToggleSeen={handleToggleSeen}
                  onError={handleError}
                  onAttachmentError={handleAttachmentError}
                />
              )}
            </section>
          </div>

          {composeAccountId && composeAccount && (
            <ComposePanel
              accountId={composeAccount.id}
              accountEmail={composeAccount.email}
              onClose={() => setComposeAccountId(null)}
            />
          )}
        </>
      )}
    </main>
  );
}

export default App;
