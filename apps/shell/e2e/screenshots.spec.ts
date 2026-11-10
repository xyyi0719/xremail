import { test } from "./harness";

/**
 * 给文档/README 用的截图，不是断言用例。
 *
 * 复用 `harness.ts` 的假后端（`e2e/daemon.ts` 注入 `window.__TAURI_INTERNALS__`），
 * 所以截到的是**真实的前端界面**，只是数据是假的、不连任何服务器。
 *
 * 产物落在 `docs/screenshots/`，文件名与 README 里的引用一一对应。
 *
 * 为什么做成 E2E 而不是手截：
 * 手截的图会随时间过期、而且没人知道是什么状态下截的。这里每次跑都能
 * 重出一份，数据固定（`fixtures.ts`），所以图里每一行文字都可复现。
 */
const OUT = "../../docs/screenshots";

test("截图：邮件列表与正文", async ({ page, app }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await app.openFolder();
  await page.getByTestId("message-item").first().click();
  await page.waitForTimeout(500);
  // 界面是**单页长滚动**（账户设置 → 文件夹 → 邮件 → 正文）。
  // 把目标区域滚到视口中间，让截图上既有列表又有正文。
  await page.getByTestId("message-item").first().evaluate((el) => {
    el.scrollIntoView({ block: "start" });
    window.scrollBy(0, -180);
  });
  await page.waitForTimeout(300);
  await page.screenshot({ path: `${OUT}/01-mailbox.png`, fullPage: false });
});

test("截图：写信面板", async ({ page, app }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await app.openFolder();
  await page.getByRole("button", { name: "写邮件" }).click();
  await page.waitForTimeout(400);
  await page.getByText("写邮件", { exact: true }).last().evaluate((el) => {
    el.scrollIntoView({ block: "start" });
    window.scrollBy(0, -120);
  });
  await page.waitForTimeout(300);
  await page.screenshot({ path: `${OUT}/02-compose.png`, fullPage: false });
});

test("截图：整页概览", async ({ page, app }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await app.openFolder();
  await page.waitForTimeout(400);
  // 整页图：一次看完账户设置 → 文件夹 → 邮件列表的结构。
  await page.screenshot({ path: `${OUT}/03-overview-full.png`, fullPage: true });
});
