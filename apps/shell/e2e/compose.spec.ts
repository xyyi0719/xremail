import { expect, test } from "./harness";

/**
 * 主流程 ③：写信面板——收件人校验、自动存草稿、发送。
 *
 * 这里只测「用户看得到的结果」：页面上的文案、草稿真的被存了、
 * 没写收件人时**没有**发出去。具体的附件规则、结果不确定的分支
 * 由 `src/__tests__/compose*.test.tsx` 在组件层覆盖，E2E 只守住主干。
 */

test("写信面板：没写收件人时拦在前端，不会调用发送", async ({ app, page }) => {
  await app.openFolder();
  await page.getByRole("button", { name: "写邮件" }).click();

  const panel = page.locator('section[aria-label="写信面板"]');
  await expect(panel).toBeVisible();
  // 打开时先拉一次草稿列表（空）。
  await expect(page.getByTestId("compose-drafts-empty")).toBeVisible();

  // 只写主题、不写收件人。顺手给一个地址提醒的场景：
  // 两个地址之间少写了逗号——空白会被当成分隔符，给出提醒。
  const to = page.getByPlaceholder("多个地址用逗号分开").first();
  await to.fill("a@example.com b@example.com");
  await expect(page.getByTestId("compose-address-warning")).toContainText(
    "地址之间有空白",
  );

  // 清空收件人（只剩主题）：这时点发送应该被前端拦下。
  await to.fill("");
  await page.getByRole("button", { name: "发送", exact: true }).click();

  await expect(page.getByTestId("compose-error")).toHaveText("至少要一个收件人");
  const daemon = await app.daemon();
  expect(daemon.calls.some((call) => call.cmd === "send_message")).toBe(false);
});

test("写信面板：改动后自动存草稿，草稿箱里出现这一封", async ({ app, page }) => {
  await app.openFolder();
  await page.getByRole("button", { name: "写邮件" }).click();

  await page.getByPlaceholder("多个地址用逗号分开").first().fill("boss@example.com");
  // 主题输入框没有占位符，按它的标签找。
  await page.getByLabel("主题", { exact: true }).fill("季度预算评审安排");
  await page.locator("textarea.compose-body").fill("评审安排在周三下午两点。");

  // 自动保存是 800ms 防抖：等状态栏说明「已保存」。
  await expect(page.getByTestId("compose-saved")).toContainText("草稿已保存", {
    timeout: 10_000,
  });

  // 后端真的收到了草稿，而且内容就是用户写的。
  const daemon = await app.daemon();
  expect(daemon.drafts).toHaveLength(1);
  expect(daemon.drafts[0]).toMatchObject({
    accountId: "acc-1",
    to: ["boss@example.com"],
    subject: "季度预算评审安排",
  });

  // 草稿箱列表里出现了它（按钮的可访问名带主题）。
  await expect(
    page.getByRole("button", { name: "打开草稿 季度预算评审安排" }),
  ).toBeVisible();
});

test("写信面板：收件人和主题填好后点发送，显示已发送", async ({ app, page }) => {
  await app.openFolder();
  await page.getByRole("button", { name: "写邮件" }).click();

  await page.getByPlaceholder("多个地址用逗号分开").first().fill("boss@example.com");
  await page.getByLabel("主题", { exact: true }).fill("请确认评审时间");
  await page.locator("textarea.compose-body").fill("没问题的话我按时到。");

  await page.getByRole("button", { name: "发送", exact: true }).click();

  const result = page.getByTestId("compose-send-result");
  await expect(result).toContainText("已发送");
  await expect(result).toContainText("Message-ID：<sent-1@example.com>");

  const daemon = await app.daemon();
  expect(daemon.sent).toHaveLength(1);
  expect(daemon.sent[0]).toMatchObject({
    to: ["boss@example.com"],
    subject: "请确认评审时间",
    bodyText: "没问题的话我按时到。",
    bodyHtml: null,
  });
  // 发出去之后发送按钮停用，防止连点发出两封。
  await expect(page.getByRole("button", { name: "发送", exact: true })).toBeDisabled();
});
