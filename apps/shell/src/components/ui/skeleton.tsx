import { cn } from "../../lib/utils";

/**
 * 骨架屏（shadcn 形态）。
 *
 * 用途：数据还没回来时的占位。比「加载中…」三个字好——
 * 布局在加载前后**不跳**（占位与真实内容同形状），这才是「不卡」的体感来源之一。
 *
 * `animate-pulse` 是 Tailwind 内置的关键帧（透明度呼吸），只动 opacity、
 * 不触发重排，符合 UI_DESIGN §5.2 的红线（不做每帧 JS、不做布局动画）。
 */
function Skeleton({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("animate-pulse rounded-md bg-muted", className)} {...props} />;
}

export { Skeleton };
