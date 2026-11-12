import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
// @ts-expect-error type error without @types/node package
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => ({
  plugins: [react(), tailwindcss()],

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
