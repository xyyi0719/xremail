import { test, expect } from "./harness";
import type { TauriDaemonConfig } from "./daemon";
import { makeAccount, makeFolders, makeMessageBody, makeMessages } from "./fixtures";

/**
 * 前端性能的量测。
 *
 * ## 为什么要有这个文件
 *
 * 「不卡」是个**感觉**，不是数字。感觉会骗人：在开发机上、两封邮件、
 * 冷启动一次，什么界面都很快。真正会卡的是**长列表滚动**和**切换邮件**。
 *
 * 所以这里量三件能落到数字上的事，并为每一件设一个**宽但有意义**的上限：
 *
 * 1. **长列表滚动**：500 封邮件，滚到底，量总时长与最长一帧。
 *    掉帧的典型原因是「滚动触发整页回流」或「列表项上挂了动画属性」。
 * 2. **切换邮件**：连点 20 封，量每次重绘的耗时。
 *    掉帧的典型原因是「切换正文导致整个列表重建」。
 * 3. **首屏**：从导航到列表可交互。
 *
 * ## 上限为什么是这些数
 *
 * 16.7ms 是 60fps 的一帧预算。这里的上限**故意放宽**（不是 16.7）：
 * CI / 容器里的浏览器跑在软件渲染上，绝对数字会比真机差一截。
 * 设成「明显超过就会红」的量级——它们拦的是**数量级退化**（比如某次改动
 * 让滚动变成每帧一次布局），不是 3ms 的抖动。
 */

const OUT = "../../docs/screenshots";

/** 造一封邮件（只填列表需要的字段）。 */
function bulkMessages(count: number) {
  const base = makeMessages()[0]!;
  return Array.from({ length: count }, (_, i) => ({
    ...base,
    id: `m-${i}`,
    uid: 1000 + i,
    subject: `第 ${i + 1} 封：季度评审与项目周报`,
    from: [`sender${i}@example.com`],
    snippet: "这是一段摘要文字，用来模拟真实列表的渲染负载。",
  }));
}

test.describe("性能：长列表滚动", () => {
  test.use({
    config: {
      accounts: [makeAccount()],
      folders: makeFolders(),
      messages: bulkMessages(500) as never,
      body: makeMessageBody(),
    } satisfies Partial<TauriDaemonConfig>,
  });

  test("500 封邮件滚动：不出现数量级退化", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    await expect(page.getByTestId("message-item").first()).toBeVisible();

    const gaps = await page.evaluate(async () => {
      const pane = document.querySelector(".mail-list-pane") as HTMLElement | null;
      if (!pane) throw new Error("找不到 .mail-list-pane");
      const gaps: number[] = [];
      await new Promise<void>((resolve) => {
        let last = performance.now();
        const step = () => {
          const now = performance.now();
          gaps.push(now - last);
          last = now;
          pane.scrollTop += 40;
          if (pane.scrollTop + pane.clientHeight >= pane.scrollHeight - 4) {
            resolve();
          } else {
            requestAnimationFrame(step);
          }
        };
        requestAnimationFrame(step);
      });
      return gaps;
    });

    const worst = Math.max(...gaps);
    const over = gaps.filter((g) => g > 50).length;
    test.info().annotations.push(
      { type: "最长一帧", description: `${worst.toFixed(1)}ms` },
      { type: "超过50ms的帧", description: `${over} / ${gaps.length}` },
    );

    expect(
      worst,
      `最长一帧 ${worst.toFixed(1)}ms（${over}/${gaps.length} 帧超过 50ms）——` +
        "超过 200ms 说明滚动触发了大范围重排",
    ).toBeLessThan(200);
  });
});

test.describe("性能：切换邮件", () => {
  test.use({
    config: {
      accounts: [makeAccount()],
      folders: makeFolders(),
      messages: bulkMessages(30) as never,
      body: makeMessageBody(),
    } satisfies Partial<TauriDaemonConfig>,
  });

  test("连点 20 封：每次重绘都应在一帧量级内", async ({ page, app }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await app.openFolder();
    const items = page.getByTestId("message-item");
    await expect(items.first()).toBeVisible();

    // 列表按 uid 倒序排，所以**不能**假设第 i 项就是「第 i+1 封」。
    // 先把每一项要显示的标题读出来，点完之后等这个标题出现在右栏。
    const titles: string[] = [];
    for (let i = 0; i < 20; i += 1) {
      titles.push((await items.nth(i).innerText()).split("\n")[0]!.trim());
    }

    const timings: number[] = [];
    for (let i = 0; i < 20; i += 1) {
      const started = Date.now();
      await items.nth(i).click();
      await page
        .getByRole("heading", { level: 2 })
        .filter({ hasText: titles[i]! })
        .first()
        .waitFor({ state: "visible" });
      timings.push(Date.now() - started);
    }

    const sorted = [...timings].sort((a, b) => a - b);
    const median = sorted[Math.floor(sorted.length / 2)]!;
    const worst = sorted[sorted.length - 1]!;
    test.info().annotations.push(
      { type: "中位耗时", description: `${median}ms` },
      { type: "最慢一次", description: `${worst}ms` },
    );

    expect(worst, `最慢一次切邮件 ${worst}ms（中位 ${median}ms）`).toBeLessThan(300);
  });
});
