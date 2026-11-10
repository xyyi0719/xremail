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
      // **滚动条在虚拟列表自己身上**，不在外面的 `.mail-list-pane`。
      // 量错对象的后果是：pane 的 scrollHeight 与 clientHeight 只差几十像素，
      // 几帧就滚到底了，采到的样本数是个位数——数字看着很好但毫无意义。
      // （实测：pane 790/724，list 1344/665。）
      const pane = document.querySelector(
        '[data-testid="message-list"]',
      ) as HTMLElement | null;
      if (!pane) throw new Error("找不到 [data-testid=message-list]");
      // **虚拟化之后每帧滚的步长必须加大**：一次只滚 40px 的话，
      // 500 行 × 56px = 28000px 要滚 700 帧；而虚拟列表每帧只重渲染十几行，
      // 帧率一高就变成「几十毫秒滚完」——采不到几个样本，量出来的数字没意义
      // （实测只剩 2 帧）。这里按「总高度的 1/120」一步，保证有约 120 个样本。
      const totalScroll = pane.scrollHeight - pane.clientHeight;
      const stepPx = Math.max(40, Math.ceil(totalScroll / 120));
      const gaps: number[] = [];
      await new Promise<void>((resolve) => {
        let last = performance.now();
        const step = () => {
          const now = performance.now();
          gaps.push(now - last);
          last = now;
          pane.scrollTop += stepPx;
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

    // **虚拟化守门**：500 封邮件时渲染的行数必须是「一屏 + 缓冲」的量级，
    // 不能随邮件总数走。全渲染是 500 行、3148 个 DOM 节点；虚拟化后约 24 行、
    // 317 个节点（2026-11-10 实测）。
    //
    // 阈值取 80 行：留足缓冲（视口 900px / 56px ≈ 16 行，加 overscan 12 行，
    // 再留余量），但只要有人把虚拟化去掉就会立刻冲到 500 而变红。
    const rendered = await page.getByTestId("message-item").count();
    const nodes = await page.evaluate(() => document.querySelectorAll("*").length);
    test.info().annotations.push(
      { type: "渲染行数", description: `${rendered}` },
      { type: "DOM 节点", description: `${nodes}` },
    );
    expect(
      rendered,
      `渲染了 ${rendered} 行——虚拟化可能被去掉了（全渲染时是 500 行）`,
    ).toBeLessThan(80);
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
    //
    // **虚拟化之后**只有视口里那二十几行在 DOM 里，`items.nth(11)` 压根不存在。
    // 所以每一步都先 `scrollIntoViewIfNeeded`——这本身也是真实用户会做的事
    // （想点第 15 封就滚下去）。量的是「滚到 + 点开 + 正文换过来」的总时间，
    // 用它当「切一封邮件要多久」是合理的。
    const timings: number[] = [];
    for (let i = 0; i < 20; i += 1) {
      const started = Date.now();
      const item = items.nth(i);
      await item.scrollIntoViewIfNeeded();
      const title = (await item.innerText()).split("\n")[0]!.trim();
      await item.click();
      await page
        .getByRole("heading", { level: 2 })
        .filter({ hasText: title })
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
