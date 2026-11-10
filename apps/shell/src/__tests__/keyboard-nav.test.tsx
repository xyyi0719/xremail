import { describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import { createBackend } from "../test/backend";
import { makeAccount, makeFolder, makeMessage } from "../test/fixtures";
import { ACCOUNT_EMAIL, renderApp, settle } from "../test/harness";

vi.mock("@tauri-apps/api/core", async () => {
  const { invokeMock } = await import("../test/tauriState");
  return { invoke: invokeMock };
});

vi.mock("@tauri-apps/api/event", async () => {
  const { listenMock } = await import("../test/tauriState");
  return { listen: listenMock };
});

/**
 * 键盘导航（UI_DESIGN §2.3）。
 *
 * 最重要的不是「按 j 会动」——而是**硬规则**：焦点在输入框里时，
 * 单键导航必须全部失效。用户在写收件人时按 `s` 是打字母，
 * 不是加星标。这条规则坏了就是数据事故，所以单独成组。
 */
function twoMessageBackend() {
  return createBackend({
    accounts: [makeAccount({ id: "acc-1", email: ACCOUNT_EMAIL })],
    folders: [makeFolder({ id: "f1", name: "收件箱" })],
    messages: [
      makeMessage({
        id: "m-new", accountId: "acc-1", folderId: "f1",
        uid: 102, subject: "第二封",
      }),
      makeMessage({
        id: "m-old", accountId: "acc-1", folderId: "f1",
        uid: 101, subject: "第一封",
      }),
    ],
  });
}

describe("键盘导航", () => {
  it("按 j 依次选中下一封，k 回到上一封", async () => {
    const { user, openFolder } = await renderApp(twoMessageBackend());
    await openFolder();
    // 脚手架默认打开设置抽屉；单键导航在抽屉开着时让位（抽屉里有表单）。
    await user.click(screen.getByRole("button", { name: "关闭设置" }));
    await settle();

    // 初始没有选中：按 j 选中第一封（最新的，uid 倒序）。
    await user.keyboard("j");
    await settle();
    const items = screen.getAllByTestId("message-item");
    expect(items[0]).toHaveClass("message-item-active");

    await user.keyboard("j");
    await settle();
    expect(items[1]).toHaveClass("message-item-active");

    await user.keyboard("k");
    await settle();
    expect(items[0]).toHaveClass("message-item-active");
  });

  it("焦点在输入框里时，单键导航必须失效", async () => {
    const { user, openFolder } = await renderApp(twoMessageBackend());
    await openFolder();
    // 脚手架默认打开设置抽屉；导航在抽屉开着时让位，先关掉。
    await user.click(screen.getByRole("button", { name: "关闭设置" }));
    await settle();
    // 选中第一封作为「基线」。
    await user.keyboard("j");
    await settle();
    expect(screen.getAllByTestId("message-item")[0]).toHaveClass("message-item-active");

    // 搜索框住在设置抽屉里。重新打开抽屉（点 ⚙），再点搜索框聚焦。
    // （Ctrl+K 的聚焦行为已在下一个用例单独覆盖；这里它会被 Radix 的
    // 焦点管理抢回去，用点击更稳。）
    await user.click(screen.getByTestId("settings-toggle"));
    await settle();
    const search = screen.getByRole("searchbox");
    await user.click(search);
    await user.keyboard("sj");
    await settle();

    // 字母真的进了输入框，而不是触发了「加星标」「下一封」。
    expect(search).toHaveValue("sj");

    // 关掉抽屉再看主界面：选中没有变化，还在第一封。
    await user.click(screen.getByRole("button", { name: "关闭设置" }));
    await settle();
    expect(screen.getAllByTestId("message-item")[0]).toHaveClass("message-item-active");
  });


  it("Esc 先关写信浮层，再清空正文选择", async () => {
    const { user, openFolder } = await renderApp(twoMessageBackend());
    await openFolder();
    // 脚手架默认打开设置抽屉；单键导航在抽屉开着时让位（抽屉里有表单）。
    await user.click(screen.getByRole("button", { name: "关闭设置" }));
    await settle();

    // 选中一封 + 打开写信。
    await user.keyboard("j");
    await settle();
    await user.keyboard("c");
    await settle();
    expect(screen.getByRole("button", { name: "关闭" })).toBeVisible();

    // 第一次 Esc：关写信浮层（正文选择保留）。
    await user.keyboard("{Escape}");
    await settle();
    expect(screen.queryByRole("button", { name: "关闭" })).toBeNull();
    expect(screen.getAllByTestId("message-item")[0]).toHaveClass("message-item-active");

    // 第二次 Esc：清空选择。
    await user.keyboard("{Escape}");
    await settle();
    expect(
      screen
        .queryAllByTestId("message-item")
        .some((el) => el.classList.contains("message-item-active")),
    ).toBe(false);
  });

  it("Ctrl+K 打开设置抽屉并聚焦搜索框", async () => {
    const { user, openFolder } = await renderApp(twoMessageBackend());
    await openFolder();
    // 脚手架默认打开设置抽屉；单键导航在抽屉开着时让位（抽屉里有表单）。
    await user.click(screen.getByRole("button", { name: "关闭设置" }));
    await settle();

    await user.keyboard("{Control>}k");
    await settle();
    expect(screen.getByRole("searchbox")).toHaveFocus();
  });
});
