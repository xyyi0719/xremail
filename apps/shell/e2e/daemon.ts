/**
 * 页面里那个「假 Tauri 后端」的实现。
 *
 * **这些函数会被整个序列化成源码注入浏览器**（见 `e2e/harness.ts` 的
 * `page.addInitScript`），所以它们必须自包含：只能用参数、局部变量和
 * 浏览器全局对象，**不能**引用模块作用域里的任何东西。
 * 这也是为什么类型定义放在函数体里——`interface` 只参与编译期，序列化后不留痕。
 *
 * 为什么不在页面里 `import`：页面加载的是 `src/` 里的真实应用代码，它只认
 * `window.__TAURI_INTERNALS__`；把假后端作为一段纯脚本注入，就不用给应用
 * 加任何 `import.meta.env` 之类的测试开关（业务代码零改动）。
 */

/** 假后端里的数据。与 `src/types.ts` 的形状一致，这里只关心 E2E 用到的字段。 */
export interface TauriDaemonConfig {
  accounts: Array<Record<string, unknown>>;
  folders: Array<Record<string, unknown>>;
  messages: Array<{ uid: number; flags: number } & Record<string, unknown>>;
  body: Record<string, unknown>;
  /** `send_message` 返回的状态；默认 `sent`。 */
  sendState?: "sent" | "uncertain" | "failed";
}

/** 注入后挂到 `window.__E2E_DAEMON__` 上的观测口（用例从这里读调用记录）。 */
export interface TauriDaemon {
  /** 按顺序记下的每次 invoke：命令名 + 参数。 */
  calls: Array<{ cmd: string; args: Record<string, unknown> }>;
  /** 当前在页面里的草稿（`save_draft` 写进来）。 */
  drafts: Array<Record<string, unknown>>;
  /** 每次 `send_message` 收到的草稿全文。 */
  sent: Array<Record<string, unknown>>;
  /** 被 `plugin:opener|open_url` 「打开」过的地址（不会真拉浏览器）。 */
  opened: string[];
}

/**
 * 装好 `window.__TAURI_INTERNALS__` 与两个插件的假 IPC。
 *
 * IPC 形状照着 Tauri v2 运行时：`@tauri-apps/api/core` 的 `invoke` 就是
 * `window.__TAURI_INTERNALS__.invoke(cmd, args, options)`；插件的命令名是
 * `plugin:<插件名>|<命令>`。按命令名分发，行为尽量贴近 Rust 侧真正的语义
 * （`save_draft` 分配 id、`set_message_flag` 改本地账本后再由 `list_messages` 读回来）。
 *
 * 必须是普通函数（`toString()` 序列化后要能独立执行）。
 */
export function installDaemon(config: TauriDaemonConfig): void {
  type Args = Record<string, unknown>;
  type Message = { uid: number; flags: number };
  type DraftInput = {
    id?: string;
    accountId: string;
    to: string[];
    cc: string[];
    bcc: string[];
    subject: string;
    bodyText: string;
    bodyHtml?: string | null;
    attachments: unknown[];
  };

  const internals = window as unknown as Record<string, unknown>;
  const sentState = config.sendState ?? "sent";
  const messages = config.messages;
  // 与 `src/types.ts` 的 FLAG_FLAGGED / FLAG_SEEN 一致。
  const FLAG_SEEN = 1 << 0;
  const FLAG_FLAGGED = 1 << 2;

  const daemon: TauriDaemon = {
    calls: [],
    drafts: [],
    sent: [],
    opened: [],
  };
  internals.__E2E_DAEMON__ = daemon;

  let draftCounter = 0;

  const commandHandlers: Record<string, (args: Args) => unknown> = {
    list_accounts: () => config.accounts,
    app_info: () => ({
      name: "Xremail",
      version: "0.1.0",
      platform: "e2e",
      dataDir: null,
    }),
    list_folders: () => config.folders,
    // 每次都返回新的对象：后端返回新快照，前端才会真的重渲染。
    list_messages: () => config.messages.map((one) => ({ ...one })),
    search_messages: () => [],
    open_message: () => config.body,
    set_message_flag: (args) => {
      const uid = Number(args.uid);
      const message = messages.find((one) => one.uid === uid) as
        | Message
        | undefined;
      if (!message) {
        throw new Error(`没有 uid=${uid} 的邮件`);
      }
      const bit = args.flag === "flagged" ? FLAG_FLAGGED : FLAG_SEEN;
      message.flags =
        args.value === true ? message.flags | bit : message.flags & ~bit;
      return { queued: false, applied: true, state: "succeeded", message: null };
    },
    sync_account: () => ({
      folders: config.folders.length,
      added: 0,
      removed: 0,
    }),
    forget_credential: () => true,
    save_credential: () => undefined,
    create_account: () => ({}),

    // ---------------- 写信 ----------------
    save_draft: (args) => {
      const draft = args.draft as DraftInput;
      const id = draft.id ?? `draft-${++draftCounter}`;
      const dto = {
        id,
        accountId: draft.accountId,
        subject: draft.subject,
        to: draft.to,
        state: "draft",
        updatedAt: Date.now(),
      };
      daemon.drafts = [...daemon.drafts.filter((one) => one.id !== id), dto];
      return dto;
    },
    list_drafts: (args) =>
      daemon.drafts.filter((one) => one.accountId === args.accountId),
    get_draft: (args) => {
      const found = daemon.drafts.find((one) => one.id === args.id);
      if (!found) {
        throw new Error(`没有 id=${args.id} 的草稿`);
      }
      return found;
    },
    send_message: (args) => {
      const draft = args.draft as DraftInput;
      daemon.sent.push({ ...draft, bodyHtml: draft.bodyHtml ?? null });
      return {
        messageId: `<sent-${daemon.sent.length}@example.com>`,
        entryId: `entry-${daemon.sent.length}`,
        state: sentState,
        detail: null,
      };
    },
    resend_uncertain: () => {
      throw new Error("E2E 里没有准备 resend_uncertain");
    },
    withdraw_draft: () => false,
  };

  // 页面里的 `fetch`：不拦的话 dev server 的 HMR 长连接会一直挂着，
  // 用例结束关页面时偶尔会报「连接被重置」。返回一个永远挂起的 promise。
  // （这个函数在浏览器里跑，不会被 Node 的参数类型检查拦下，所以先转成 unknown。）
  (internals as { fetch: unknown }).fetch = () => new Promise(() => undefined);

  // 插件命令（`plugin:<名字>|<命令>`）。真插件会拉起原生窗口/系统浏览器，
  // headless chromium 里没有这些，于是换成可观测的假实现。
  const pluginHandlers: Record<string, (args: Args) => unknown> = {
    // 文件选择框：默认「什么都没选」。用例需要选文件时，
    // 先 `page.evaluate` 往 `__E2E_PICKED_FILES__` 里塞路径。
    "plugin:dialog|open": () => {
      const picked = internals.__E2E_PICKED_FILES__ as string[] | undefined;
      internals.__E2E_PICKED_FILES__ = undefined;
      return picked ?? null;
    },
    // 真插件会拉起系统浏览器；这里只记账——用例据此断言
    // 「点邮件里的链接不会让应用窗口跳走」。
    "plugin:opener|open_url": (args) => {
      daemon.opened.push(String(args.url));
      return undefined;
    },
  };

  internals.__TAURI_INTERNALS__ = {
    invoke: async (cmd: string, args: Args = {}) => {
      daemon.calls.push({ cmd, args });
      const handler = commandHandlers[cmd] ?? pluginHandlers[cmd];
      if (!handler) {
        throw new Error(`E2E 里没有为命令「${cmd}」准备桩`);
      }
      return handler(args);
    },
    // 事件订阅：应用只调用，不需要真的推送（E2E 用例不测后台事件）。
    transformCallback: (callback: unknown) => callback,
    unregisterCallback: () => undefined,
    convertFileSrc: (path: string) => path,
    metadata: {
      currentWindow: { label: "main" },
      currentWebview: { label: "main" },
    },
  };
}
