import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import App from "../App";
import type { Backend } from "./backend";
import { setInvokeImpl } from "./tauriState";

/**
 * 集成测试的脚手架。
 *
 * 这里的每个函数都只做一件事：**把异步的界面更新等到落定**。
 * 后端的每一次 `invoke` 都是异步的，不主动排空，React 就会在
 * `act(...)` 之外更新状态，然后报「更新没有包在 act 里」。
 */

/** 测试里用的账户邮箱（也是列表里那个按钮的可访问名）。 */
export const ACCOUNT_EMAIL = "me@example.com";

/**
 * 让出一个宏任务，把 pending 的 promise 链跑完。
 *
 * 平时用不上（`settle` 已经够了）；只有确实需要 `setTimeout` 跑过之后
 * 再收尾时，才**先在 act 外面**调它，再进 `settle`——顺序不能反。
 *
 * 用一个 `setTimeout(0)` 而不是几个 `Promise.resolve()`：
 * 微任务队列里可能还排着好几层 `.then()`，宏任务能让它们全部跑完。
 */
export async function nextMacrotask(): Promise<void> {
  await new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

/**
 * 把 pending 的 promise 链跑完，并让 React 把结果刷到界面上。
 *
 * 只用一个空的 **异步** `act`：`await act(async () => {})` 会自动把当前
 * 排着的微任务（也就是 `invoke` 返回的 promise 链）跑完，再把结果刷出去。
 *
 * 这里**不要**再套 `setTimeout`：宏任务会在 `act` 退出之后才跑，
 * 那就变成「在 act 之外更新状态」——React 19 的 act 环境一旦被
 * 这样破坏，后面所有更新都会被悄悄丢掉（组件看着像「点了没反应」）。
 */
export async function settle(): Promise<void> {
  await act(async () => {});
}

export interface AppHarness {
  backend: Backend;
  user: ReturnType<typeof userEvent.setup>;
  /** 选中账户 → 选中文件夹；返回时邮件列表已经加载完。 */
  openFolder(name?: string): Promise<void>;
  /**
   * 点一下，并把随之而来的异步更新等到落定。
   *
   * `user.click` 自己会把指针那几拍跑完，但它**不会**等 `onClick` 之后
   * 排进队列的 `.then()`（比如下载完的 `setState`）。少这一下的话，
   * 界面看起来「点了没反应」，其实只是还没刷出来。
   */
  click(target: Element): Promise<void>;
}

/**
 * 挂上 `<App />`，并把 `invoke` 接到假后端上。
 */
export async function renderApp(backend: Backend): Promise<AppHarness> {
  setInvokeImpl(backend.impl);
  const user = userEvent.setup();
  render(<App />);
  await settle();

  const openFolder = async (name = "收件箱"): Promise<void> => {
    await user.click(screen.getByRole("button", { name: ACCOUNT_EMAIL }));
    await settle();
    await user.click(screen.getByRole("button", { name: new RegExp(name) }));
    await settle();
  };

  const click = async (target: Element): Promise<void> => {
    // 点一下，再把 user-event / React 排下的那几拍推完。
    await user.click(target);
    await settle();
  };

  return { backend, user, openFolder, click };
}
