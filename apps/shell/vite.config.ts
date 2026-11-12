import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
// @ts-expect-error type error without @types/node package
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
/**
 * 把 `@layer xxx { ... }` 摊平到顶层。
 *
 * ## 为什么必须这么做
 *
 * Tailwind v4 把工具类包在 `@layer theme / base / utilities` 里。而 **CSS
 * Cascade Layers（`@layer`）是 Chrome 99+ 才有的**——本项目要在 Android 上
 * 跑，而安卓系统 WebView 可能是很旧的版本（实测模拟器 android-30 自带
 * **WebView 83**，2020 年）。
 *
 * WebView 83 **不认 `@layer`**：整块规则被当作未知 at-rule 忽略。表现是
 * 「类名挂在 DOM 上、样式全不生效」——抽屉 `position:fixed` 算出来是 `static`，
 * 浮层跑到页面中间；列表卡片没有圆角间距。**桌面端（新 Chromium）完全正常**，
 * 所以只有真机/模拟器才暴露。
 *
 * 摊平之后就是普通 CSS 规则，任何浏览器都认。代价是丢掉 cascade layer 的
 * 优先级管理——本项目的样式量不大（< 60KB），可接受。
 */
function flattenCascadeLayers() {
  return {
    name: "flatten-cascade-layers",
    enforce: "post" as const,
    generateBundle(_options: unknown, bundle: Record<string, { type: string; source: string }>) {
      for (const file of Object.values(bundle)) {
        if (file.type !== "asset" || !String(file.fileName).endsWith(".css")) continue;
        file.source = flattenLayers(String(file.source));
      }
    },
  };
}

/** 用括号配对把 `@layer name {...}` 展开到顶层（正则处理不了嵌套花括号）。 */
function flattenLayers(input: string): string {
  let out = "";
  let i = 0;
  while (i < input.length) {
    const at = input.indexOf("@layer", i);
    // 跳过不是块形式的 `@layer a, b;`
    if (at === -1) {
      out += input.slice(i);
      break;
    }
    out += input.slice(i, at);
    // 读图层名，直到 `{` 或 `;`
    let j = at + "@layer".length;
    while (j < input.length && input[j] !== "{" && input[j] !== ";") j += 1;
    if (j >= input.length) {
      out += input.slice(at);
      break;
    }
    if (input[j] === ";") {
      // `@layer a, b;` 这种声明式，保留（它不是块，不影响规则生效）
      out += input.slice(at, j + 1);
      i = j + 1;
      continue;
    }
    // 块形式：配对花括号，把里面的内容原样摊出来（递归展开嵌套 layer）
    let depth = 0;
    let k = j;
    for (; k < input.length; k += 1) {
      if (input[k] === "{") depth += 1;
      else if (input[k] === "}") {
        depth -= 1;
        if (depth === 0) break;
      }
    }
    const inner = input.slice(j + 1, k);
    out += flattenLayers(inner);
    i = k + 1;
  }
  return out;
}

export default defineConfig(() => ({
  plugins: [react(), tailwindcss(), flattenCascadeLayers()],

  // **JS 目标降到 ES2020**（而不是 Vite 默认的 esnext/es2022）。
  //
  // 原因：Android 系统 WebView 可能是很旧的版本。实测模拟器 android-30 镜像
  // 自带 **WebView 83.0.4103.106（Chrome 83，2020 年）**，而 Vite 默认产物用了
  // `||=` / `&&=` / `??=`（ES2021 逻辑赋值，Chrome 85+ 才有）。WebView 83 解析
  // 直接 `SyntaxError: Unexpected token '='`，整个脚本不执行 → 白屏。
  //
  // ES2020 覆盖到 Chrome 80，足以跑旧 WebView。代价是产物略大一点点。
  build: {
    target: "es2020",
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
