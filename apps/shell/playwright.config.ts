import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright E2E 配置（T9：真实浏览器里的端到端用例）。
 *
 * ## 为什么不跑 Tauri
 *
 * 这是 Tauri 应用，但 E2E **不拉起 Tauri 窗口**：那需要 WebView2 / WebKitGTK /
 * WKWebView 加原生依赖，CI 上装不起来也跑不稳。
 *
 * 前端的全部后端调用都经由 `@tauri-apps/api/core` 的 `invoke`，而它只是一行转发：
 *
 *   async function invoke(cmd, args, options) {
 *     return window.__TAURI_INTERNALS__.invoke(cmd, args, options);
 *   }
 *
 * 所以在 `page.addInitScript` 里往页面塞一个假的 `window.__TAURI_INTERNALS__`，
 * 整条 IPC 就被换掉了；跑的是**真的 Vite 开发服务器 + 真的 Chromium + 真的 React**，
 * 和 Tauri 里唯一的差别是「后端是假的」——这正是 E2E 想要的隔离。
 *
 * ## 服务器
 *
 * 用 `vite` 开发服务器（不是 `vite build` + `preview`）：不用先构建、
 * 失败时页面源码就是 TSX 原文，好排查。端口固定 4173（与 `vite.config.ts`
 * 里 Tauri 用的 1420 分开），`strictPort` 保证「端口被占」直接报错而不是
 * 悄悄换个端口让人测到别的进程上。
 */
export default defineConfig({
  testDir: "./e2e",
  // E2E 用例共享一个 Vite 服务器，但每个用例各自起一个浏览器上下文；
  // 默认并行即可（后端桩是每个页面自己的，互不影响）。
  fullyParallel: true,
  // CI 上不留 `test.only`（漏删的 only 会让整个套件只剩一条）。
  forbidOnly: !!process.env.CI,
  // CI 上网络抖动导致的重试：本地不重试，失败就要看见。
  retries: process.env.CI ? 2 : 0,
  // CI 上两个 worker 够用：一个 Vite 服务器 + 无头 chromium，再多只抢 CPU。
  workers: process.env.CI ? 2 : undefined,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  // 产物目录放 `e2e-results/`，并在 `.gitignore` 里忽略。
  outputDir: "./e2e-results",
  use: {
    baseURL: "http://127.0.0.1:4173",
    // 失败时留证：trace 可回放，截图看一眼就知道卡在哪。
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  webServer: {
    command: "npm run dev -- --host 127.0.0.1 --port 4173 --strictPort",
    url: "http://127.0.0.1:4173",
    // 本地已经开着一个 dev server 时直接复用（省掉十几秒启动）。
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    // 注意**不要**改成 `stdout: "pipe"`：
    //  1. 本地复用已有服务器（`reuseExistingServer`）时，pipe 会去读一个
    //     不属于自己的进程的输出，`webServer` 永远「没就绪」——本地跑会挂；
    //  2. vite 的开发服务器监听整个项目根目录，trace / 截图产物一旦
    //     落在被监听的位置，会被当成源码改动触发整页重载。
    // 产物目录 `e2e-results/` 已加进 `.gitignore`，且用完即删。
    stdout: "ignore",
    stderr: "pipe",
  },
});
