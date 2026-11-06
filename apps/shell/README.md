# Xremail · 应用外壳

Tauri 2 + React 19 + TypeScript 的前端与 Rust 外壳。**桌面与移动共用这一份工程**。

```
src/                React 前端（仅渲染，无业务逻辑）
src-tauri/          Rust 外壳：Command / Event / 平台集成
public/favicon.svg  应用图标
```

## 命令

```bash
npm install            # 首次
npm run dev            # 仅前端（Vite，端口 1420）
npm run tauri dev      # 完整应用（桌面）
npm run tauri build    # 打包桌面版
npm run typecheck      # TypeScript 类型检查
npm run test           # Vitest 单元测试（12 个文件 / 69 个用例）
npm run test:e2e       # Playwright 端到端（2 个 spec / 5 条；首次先 npx playwright install chromium）
npm run android:init   # 生成 Android 工程（需 Android SDK/NDK，只做一次）
npm run android:dev    # 在 Android 设备/模拟器上运行
```

## 约束

- 前端**不直接**访问数据库、不处理协议、不碰凭据。
- 所有数据经 Tauri Command 获取，实时更新靠 `AppEvent` 事件订阅。
- **不引入第三方前端依赖**（唯一例外：Tauri 官方插件 `@tauri-apps/plugin-dialog`）：
  状态用 React 内置 state，样式用原生 CSS，富文本用 `contentEditable` 自写。
- HTML 邮件由**后端消毒**后渲染（`xremail-core::html`），原始 HTML 不出后端；
  当前**未**使用 sandbox iframe（偏离与代价见 [SECURITY §3.0](../../docs/SECURITY.md)）。
- `tauri.conf.json` 的 `app.security.csp` 不得设为 `null`（当前 `img-src` 不含 http/https）。

详见 [架构文档](../../docs/ARCHITECTURE.md) 与 [贡献指南](../../docs/CONTRIBUTING.md)。
