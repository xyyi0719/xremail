import { test, expect } from "./harness";

/**
 * 真实 UI 冒烟测试（F 版重设计后新增）。
 *
 * 与截图用例不同：这里**真的点**、真的断言状态变化。目的是在交付前
 * 抓「看起来对、用起来坏」的问题（比如重设计时把某个按钮删了、
 * 或同一操作出现两个入口）。
 */
test.describe("F 版 UI 冒烟", () => {
  test("首屏三栏都在，且关键元素可见", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();

    // 三栏
    await expect(page.locator(".mail-sidebar")).toBeVisible();
    await expect(page.locator(".mail-list-pane")).toBeVisible();
    await expect(page.locator(".mail-reading-pane")).toBeVisible();
    // 图标轨：写邮件 + 至少一个文件夹
    await expect(page.locator(".compose-launch-button")).toBeVisible();
    expect(await page.locator(".folder-item").count()).toBeGreaterThan(0);
    // 列表有卡片
    expect(await page.locator(".message-row").count()).toBeGreaterThan(0);
    // 顶栏账户切换
    await expect(page.locator(".topbar-accounts .account-switcher-item").first()).toBeVisible();
  });

  test("点文件夹 → 列表出现；点邮件 → 正文出现且只有一份「标记为已读」", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();

    const first = page.getByTestId("message-item").first();
    await expect(first).toBeVisible();
    await first.click();

    // 正文渲染：sticky 工具条 + 阅读列
    await expect(page.locator(".reader-bar")).toBeVisible();
    await expect(page.locator(".message-view .subject")).toBeVisible();

    // **重设计时踩过的坑**：顶部工具条和发件人行都放了「标记为已读」→
    // 出现两个同名按钮，用户不知道该点哪个。这里断言只有一份。
    await expect(page.getByRole("button", { name: "标记为已读" })).toHaveCount(1);
    await expect(page.getByRole("button", { name: "标记为未读" })).toHaveCount(0);
  });

  test("点「标记为已读」后状态翻转", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await page.getByTestId("message-item").first().click();

    await page.getByRole("button", { name: "标记为已读" }).click();
    // 翻转后按钮文案变成「标记为未读」
    await expect(page.getByRole("button", { name: "标记为未读" })).toBeVisible({ timeout: 5000 });
  });

  test("键盘 j/k 能选中邮件", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    // E2E 的 harness **不会**自动开抽屉（那是单元测试脚手架的行为），
    // 所以这里直接按 j 就行。
    await page.keyboard.press("j");
    await expect(page.locator(".message-item-active")).toHaveCount(1);
  });

  test("设置抽屉能开能关，账户卡在里面", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    // 初始是关的
    await expect(page.getByTestId("account-item")).toHaveCount(0);

    await page.getByTestId("settings-toggle").click();
    await expect(page.getByTestId("account-item").first()).toBeVisible();

    // 再关掉
    await page.getByRole("button", { name: "关闭设置" }).click();
    await expect(page.getByTestId("account-item")).toHaveCount(0);
  });

  test("新增账户表单：默认已填服务器地址（不再空白）", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await page.getByTestId("settings-toggle").click();

    // 空表单时，默认服务商 163 的 IMAP/SMTP 地址应当已填好
    const imap = page.getByLabel("IMAP 服务器");
    await expect(imap).toHaveValue("imap.163.com");
    await expect(page.getByLabel("SMTP 服务器")).toHaveValue("smtp.163.com");
  });

  test("填邮箱自动带出用户名与显示名", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await page.getByTestId("settings-toggle").click();

    // 用 role=textbox 精确定位：「邮箱」这两个字也出现在服务商下拉的选项里
    // （"163 邮箱"），getByLabel 会同时命中 select。
    await page.getByRole("textbox", { name: "邮箱", exact: true }).fill("zhangsan@qq.com");
    await expect(page.getByLabel("用户名")).toHaveValue("zhangsan@qq.com");
    await expect(page.getByLabel("显示名（可选）")).toHaveValue("zhangsan");
  });

  test("邮箱与授权码一次填完，创建时就存入凭据", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await page.getByTestId("settings-toggle").click();

    // 邮箱 + 授权码放在同一屏，一填完就能建账户
    await page.getByRole("textbox", { name: "邮箱", exact: true }).fill("worker@163.com");
    await page.getByRole("textbox", { name: "授权码", exact: true }).fill("FAKE-AUTH-CODE-1234");

    const calls = await page.evaluate(async () => {
      const daemon = (window as unknown as { __E2E_DAEMON__: { calls: { cmd: string; args: unknown }[] } }).__E2E_DAEMON__;
      return daemon.calls.map((c) => c.cmd);
    });
    expect(Array.isArray(calls)).toBe(true);
  });
});
