import { test as base, expect } from "@playwright/test";
import { installDaemon, type TauriDaemon, type TauriDaemonConfig } from "./daemon";
import {
  ACCOUNT_EMAIL,
  INBOX_NAME,
  makeAccount,
  makeFolders,
  makeMessageBody,
  makeMessages,
} from "./fixtures";

/**
 * E2E 脚手架：每个用例拿到一个「后端是假的、其余全是真的」应用。
 *
 * - 页面：Vite 开发服务器上的真实 `index.html` + `src/main.tsx`；
 * - 后端：`e2e/daemon.ts` 注入的 `window.__TAURI_INTERNALS__`；
 * - 初始化脚本必须在**任何应用代码之前**跑（`addInitScript` 的语义），
 *   所以应用挂载时的第一次 `invoke("list_accounts")` 就已经走假后端了。
 */

export interface E2EApp {
  /** 页面里的假后端（调用记录、草稿、已发送）。 */
  daemon(): Promise<TauriDaemon>;
  /** 选中账户 → 选中文件夹；返回时邮件列表已经加载完。 */
  openFolder(name?: string): Promise<void>;
}

export const test = base.extend<{
  config: Partial<TauriDaemonConfig>;
  app: E2EApp;
}>({
  // 用例可以在 `test.use({ config: { ... } })` 里覆盖这些数据。
  config: [{}, { option: true }],

  // 覆盖内置的 `page`：每个用例的页面在导航前就装好假后端。
  page: async ({ page, config }, use) => {
    const daemonConfig: TauriDaemonConfig = {
      accounts: [makeAccount()],
      folders: makeFolders(),
      messages: makeMessages(),
      body: makeMessageBody(),
      ...config,
    };
    // `installDaemon` 被序列化成源码后注入：它自包含（见 e2e/daemon.ts 的注释）。
    await page.addInitScript(installDaemon, daemonConfig);
    await use(page);
  },

  app: async ({ page }, use) => {
    const app: E2EApp = {
      daemon: () =>
        page.evaluate(
          () =>
            (window as unknown as { __E2E_DAEMON__: TauriDaemon })
              .__E2E_DAEMON__,
        ),
      openFolder: async (name = INBOX_NAME) => {
        await page.getByRole("button", { name: ACCOUNT_EMAIL }).click();
        await page.getByRole("button", { name: new RegExp(name) }).click();
        // 邮件列表标题是「邮件（n）」——等它出现再往下走，
        // 后面的断言就不用各自再等一次。
        await expect(page.locator(".message-list")).toBeVisible();
      },
    };
    await page.goto("/");
    await use(app);
  },
});

export { expect };
