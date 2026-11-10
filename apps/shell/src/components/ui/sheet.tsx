import * as React from "react";
import * as SheetPrimitive from "@radix-ui/react-dialog";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "../../lib/utils";

/**
 * 侧滑抽屉（shadcn 的 Sheet）。
 *
 * 用途：承载「账户与设置」——添加账户、授权码、DKIM、搜索。
 * 设计文档：docs/UI_DESIGN.md §2.2 / §4.1。
 *
 * 为什么用 Radix：焦点陷阱、`Esc` 关闭、`aria-*`、滚动锁定这些自己写最容易漏，
 * 而漏了就是「感觉不专业」。源码复制进仓库（shadcn 的分发方式），
 * 样式用的全是 tokens.css 里的变量（经 tailwind.css 的 @theme inline 映射）。
 *
 * 关闭即卸载（Radix 的默认行为）：不许 `display:none` 藏着——
 * 那会让每个打开过的抽屉都留在 DOM 里（UI_DESIGN §5.2 红线三）。
 */
const Sheet = SheetPrimitive.Root;
const SheetTrigger = SheetPrimitive.Trigger;
const SheetClose = SheetPrimitive.Close;
const SheetPortal = SheetPrimitive.Portal;

const SheetOverlay = React.forwardRef<
  React.ElementRef<typeof SheetPrimitive.Overlay>,
  React.ComponentPropsWithoutRef<typeof SheetPrimitive.Overlay>
>(({ className, ...props }, ref) => (
  <SheetPrimitive.Overlay
    className={cn(
      "fixed inset-0 z-30 bg-black/40",
      "data-[state=open]:animate-in data-[state=closed]:animate-out",
      className,
    )}
    {...props}
    ref={ref}
  />
));
SheetOverlay.displayName = SheetPrimitive.Overlay.displayName;

const sheetVariants = cva(
  "fixed z-30 flex flex-col gap-4 bg-background shadow-lg transition ease-in-out data-[state=open]:animate-in data-[state=closed]:animate-out",
  {
    variants: {
      side: {
        // 抽屉从右滑入，占屏幕右侧固定宽度；小屏时占满。
        right:
          "inset-y-0 right-0 h-full w-3/4 sm:max-w-[30rem] border-l border-border data-[state=open]:slide-in-from-right data-[state=closed]:slide-out-to-right",
      },
    },
    defaultVariants: { side: "right" },
  },
);

interface SheetContentProps
  extends React.ComponentPropsWithoutRef<typeof SheetPrimitive.Content>,
    VariantProps<typeof sheetVariants> {}

const SheetContent = React.forwardRef<
  React.ElementRef<typeof SheetPrimitive.Content>,
  SheetContentProps
>(({ side = "right", className, children, ...props }, ref) => (
  <SheetPortal>
    {/* **非模态**：不渲染遮罩、不锁滚动、不 trap 焦点。
        这是设置抽屉刻意的取舍——打开时用户可能还要看一眼列表/正文，
        模态（遮罩 + aria-hidden 隔离外部）会把整个应用挡住。
        集成测试也依赖这一点：抽屉开着时文件夹按钮仍可被查询。 */}
    <SheetPrimitive.Content
      ref={ref}
      className={cn(sheetVariants({ side }), className)}
      // **不要**因为点了抽屉外面就自动关闭：这里装的是表单（授权码、DKIM），
      // 用户正输到一半点错一格，整个抽屉消失是不可接受的。
      // 关闭途径是右上角的 ✕ 和 `Esc`——都是明确动作。
      onInteractOutside={(event) => event.preventDefault()}
      onFocusOutside={(event) => event.preventDefault()}
      {...props}
    >
      {children}
      <SheetPrimitive.Close
        className="absolute right-4 top-4 rounded-sm opacity-70 transition-opacity hover:opacity-100 focus-visible:outline-2 focus-visible:outline-ring"
        aria-label="关闭设置"
      >
        ✕
      </SheetPrimitive.Close>
    </SheetPrimitive.Content>
  </SheetPortal>
));
SheetContent.displayName = SheetPrimitive.Content.displayName;

const SheetHeader = ({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) => (
  <div className={cn("flex flex-col gap-1 border-b border-border p-4", className)} {...props} />
);

const SheetBody = ({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) => (
  <div className={cn("flex-1 overflow-y-auto p-4", className)} {...props} />
);

const SheetTitle = SheetPrimitive.Title;
const SheetDescription = SheetPrimitive.Description;

export {
  Sheet,
  SheetTrigger,
  SheetClose,
  SheetContent,
  SheetHeader,
  SheetTitle,
  SheetDescription,
  SheetBody,
  type SheetContentProps,
};
