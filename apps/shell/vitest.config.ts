import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

/**
 * React 的 `act(...)` **只存在于开发版构建**里。
 *
 * 如果跑测试的 shell 里带着 `NODE_ENV=production`（CI 镜像、某些
 * 打包脚本都会这么设），React 会解析到生产构建，
 * `React.act is not a function` 会让每个用例都炸。
 * 这里在**加载配置时**就把 NODE_ENV 拨回 test——配置早于 Vite
 * 解析 `react` 的包导出条件，所以来得及。
 */
if (process.env.NODE_ENV === "production") {
  process.env.NODE_ENV = "test";
}

// 测试配置单独一份：构建配置（vite.config.ts）里不放测试相关的东西，
// 产物构建也不会把测试代码带进去。
export default defineConfig({
  plugins: [react()],

  test: {
    // 组件测试要 DOM。
    environment: "jsdom",
    // describe / it / expect 等全局可用，测试文件不用逐个 import。
    globals: true,
    // jest-dom 匹配器 + act 环境 + 「console 必须干净」的检查。
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    // 默认 5s：并行跑时 CPU 被抢，user-event 的逐帧交互会超时，TEST_TIMEOUT_MS 可以按机器调。
    testTimeout: Number(process.env.TEST_TIMEOUT_MS ?? 15000),
  },
});
