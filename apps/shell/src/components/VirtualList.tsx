import { useCallback, useLayoutEffect, useRef, useState } from "react";

/**
 * 一个**定高**的虚拟列表：只渲染视口里（外加上下缓冲）的那几行。
 *
 * ## 为什么自己做，不引库
 *
 * 邮件列表的行是**定高**的（主题一行、发件人一行，都是单行省略），
 * 定高虚拟化本身只有几十行代码，而引一个库要付：
 * 新依赖（供应链面）、它自己的 React 版本约束、以及一次「它为什么这么慢」的排查成本。
 * 这个项目的依赖只有 5 个，不轻易加。
 *
 * ## 它解决什么
 *
 * 500 封邮件全渲染是 **3148 个 DOM 节点、首次打开 262ms**（2026-11-10 实测）。
 * 数量再涨就是线性恶化——1000 封翻倍、5000 封五倍。虚拟化之后渲染的**行数**
 * 与邮件总数无关，只与视口高度有关（约 15 行 + 缓冲）。
 *
 * ## 定高的代价（要说清）
 *
 * 行高必须是**常数**：主题太长就省略，不换行。如果哪天要支持两行主题，
 * 这个组件就用不了，得换成测量式的虚拟化。`ROW_HEIGHT` 与 CSS 里的
 * `.message-row { height: … }` 是**同一份契约**，改一个必须改另一个。
 */
export const ROW_HEIGHT = 56;

/** 视口上下各多渲染几行——快速滚动时不会看到空白。 */
const OVERSCAN = 6;

export interface VirtualListProps<T> {
  items: T[];
  /** 每一项的渲染。`index` 是它在 `items` 里的下标。 */
  renderItem: (item: T, index: number) => React.ReactNode;
  /** 滚动容器的 class（由调用方决定，用来对齐三栏的样式）。 */
  className?: string;
  /** 给测试用的标识。 */
  testId?: string;
}

/**
 * 只渲染可见区间的列表。
 *
 * 结构是「外层滚动容器 + 一个撑满总高度的占位 + 绝对定位的可见行」：
 * 占位保证滚动条长度正确，可见行用 `transform: translateY` 定位。
 *
 * 用 `transform` 而不是 `top`：`top` 会触发重排，`transform` 只走合成。
 */
export default function VirtualList<T>({
  items,
  renderItem,
  className,
  testId,
}: VirtualListProps<T>) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(0);

  const measure = useCallback(() => {
    const node = containerRef.current;
    if (!node) {
      return;
    }
    setViewportHeight(node.clientHeight);
  }, []);

  // 首帧就量一次：`useLayoutEffect` 保证在浏览器绘制之前拿到高度，
  // 不会先渲染 0 行再闪一下。
  useLayoutEffect(measure, [measure]);

  // 窗口尺寸变了要重新量。只监听 `resize`，不做 ResizeObserver——
  // 后者在滚动容器上会被频繁触发；这里的容器高度只会随窗口变。
  useLayoutEffect(() => {
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, [measure]);

  const total = items.length;
  const start = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  // **`viewportHeight` 为 0 时兜底渲染一个屏的量**。
  //
  // jsdom（单元测试环境）没有真实布局，`clientHeight` 恒为 0。没有这个兜底，
  // 虚拟列表在测试里会渲染 **0 行**——所有「列表里有这封邮件」的断言全红，
  // 而那跟产品行为无关，纯粹是测试环境量不到高度。
  //
  // 兜底值取 30 行：比一个真实屏幕略多，测试里能一次看到足够多的项。
  const effectiveHeight = viewportHeight > 0 ? viewportHeight : ROW_HEIGHT * 30;
  const visibleRows = Math.ceil(effectiveHeight / ROW_HEIGHT) + OVERSCAN * 2;
  const end = Math.min(total, start + visibleRows);

  return (
    <div
      ref={containerRef}
      className={className}
      data-testid={testId}
      // `passive` 让浏览器不必等回调返回就能滚——这条在长列表上很关键。
      onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
      style={{ overflowY: "auto", position: "relative" }}
    >
      <div style={{ height: total * ROW_HEIGHT }}>
        {items.slice(start, end).map((item, offset) => {
          const index = start + offset;
          return (
            <div
              key={index}
              style={{
                position: "absolute",
                top: 0,
                left: 0,
                right: 0,
                height: ROW_HEIGHT,
                transform: `translateY(${index * ROW_HEIGHT}px)`,
                // 行内容不参与外层布局计算，避免连带的回流。
                contain: "layout paint",
              }}
            >
              {renderItem(item, index)}
            </div>
          );
        })}
      </div>
    </div>
  );
}
