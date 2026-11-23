import { test, expect } from "./harness";

/**
 * WebDAV 账户同步面板的 UI 冒烟。
 *
 * 真的填表、真的点按钮、真的断言结果文案。不测真实网络（E2E 后端是假 daemon）。
 */
test.describe("账户同步面板", () => {
  test("面板存在，且四个必填项为空时按钮禁用", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await page.getByTestId("settings-toggle").click();

    const panel = page.getByTestId("vault-sync-panel");
    await expect(panel).toBeVisible();
    // 主密码没填 → 上传按钮应禁用（防止空口令加密）。
    const upload = panel.getByRole("button", { name: /上传到坚果云/ });
    await expect(upload).toBeDisabled();
  });

  test("填完必填项后可上传，并显示上传摘要", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await page.getByTestId("settings-toggle").click();
    const panel = page.getByTestId("vault-sync-panel");

    await panel.getByLabel("坚果云账号（邮箱）").fill("me@example.com");
    await panel.getByLabel("应用密码").fill("app-password");
    await panel.getByLabel("主密码").fill("a-strong-master-password");

    const upload = panel.getByRole("button", { name: /上传到坚果云/ });
    await expect(upload).toBeEnabled();
    await upload.click();

    await expect(page.getByTestId("vault-message")).toContainText("已上传");
  });

  test("下载会报告导入数量", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await page.getByTestId("settings-toggle").click();
    const panel = page.getByTestId("vault-sync-panel");

    await panel.getByLabel("坚果云账号（邮箱）").fill("me@example.com");
    await panel.getByLabel("应用密码").fill("app-password");
    await panel.getByLabel("主密码").fill("a-strong-master-password");
    await panel.getByRole("button", { name: /从这里下载/ }).click();

    await expect(page.getByTestId("vault-message")).toContainText("已导入 1 个账户");
  });
});
