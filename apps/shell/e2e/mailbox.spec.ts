import { expect, test } from "./harness";

/**
 * 主流程 ①②：打开邮箱 → 看到邮件列表 → 点开一封 → 看到正文。
 *
 * 这两条是「用户最先做的事」：装好应用、连上账户之后，打开就能看到信，
 * 点开就能读。任何一处挂了，用户会认为「这个客户端不能用」。
 */

test("打开文件夹后邮件列表按时间倒序显示标题和发件人", async ({ app, page }) => {
  await app.openFolder();

  // 列表标题带条数（两封）。
  await expect(page.getByRole("heading", { name: "邮件（2）" })).toBeVisible();

  const subjects = page.locator(".message-subject");
  await expect(subjects).toHaveCount(2);
  // uid 大的（新的一封）在上面。
  await expect(subjects.nth(0)).toHaveText("季度预算评审安排");
  await expect(subjects.nth(1)).toHaveText("上周的项目周报");

  // 每行还带发件人，用户不点开也能判断这封要不要看。
  await expect(page.locator(".message-detail").nth(0)).toHaveText(
    "boss@example.com",
  );
  await expect(page.locator(".message-detail").nth(1)).toHaveText(
    "reports@example.com",
  );

  // 打开文件夹确实向后端要了列表，参数是当前账户 + 收件箱。
  const daemon = await app.daemon();
  const listCall = daemon.calls.find((call) => call.cmd === "list_messages");
  expect(listCall?.args).toMatchObject({ accountId: "acc-1", folderId: "f-inbox" });
});

test("点开一封邮件能看到正文", async ({ app, page }) => {
  await app.openFolder();

  await page.locator(".message-item").first().click();

  // 正文面板的标题就是这封邮件的主题。
  await expect(
    page.getByRole("heading", { name: "季度预算评审安排" }),
  ).toBeVisible();
  await expect(page.locator("pre.message-body")).toHaveText(
    "评审安排在周三下午两点，地点是 3 楼会议室。",
  );

  // 打开时向后端要的正是点中的那一封（uid=102，不是列表里第一行的 id）。
  const daemon = await app.daemon();
  const openCall = daemon.calls.find((call) => call.cmd === "open_message");
  expect(openCall?.args).toMatchObject({
    accountId: "acc-1",
    folderId: "f-inbox",
    uid: 102,
  });

  // 「打开不自动标已读」：只给按钮，不动服务器上的已读位。
  await expect(
    page.getByRole("button", { name: "标记为已读" }),
  ).toBeVisible();
  expect(daemon.calls.some((call) => call.cmd === "set_message_flag")).toBe(
    false,
  );
});

