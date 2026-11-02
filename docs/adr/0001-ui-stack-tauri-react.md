# ADR-0001：UI 技术栈选用 Tauri 2 + React/TypeScript

- **状态**：已接受
- **日期**：2026-10-18
- **决策者**：xyyi0719

---

## 背景

需求是一个**多端**（Windows 10+、Android 8+，后续可能 iOS/macOS/Linux）的邮件客户端，
底层要求 Rust，界面技术栈需要选型。

邮件客户端的 UI 形态非常特殊：左中右三栏布局、十万级邮件的虚拟滚动、富文本编辑器、
附件拖拽、键盘快捷键、触屏手势、HTML 邮件安全渲染。**UI 层的工程量可能超过协议层**，
所以选型的核心问题是："哪个技术栈能最快做出一个能用的邮件客户端？"

约束：

1. 核心业务逻辑（IMAP/SMTP/存储/加密/搜索）必须是 Rust。
2. 至少覆盖 Windows + Android，且要能低成本扩展到 iOS/macOS。
3. 需要真机后台同步能力（Android 尤其）。
4. 个人/小团队可维护。

---

## 决策

**采用 Tauri 2 + React 19 + TypeScript。**

- 桌面与移动共用一个 `src-tauri`（Rust）工程与一套 React 前端。
- 全部协议、存储、加密、同步逻辑在 Rust 侧；前端只负责渲染与交互。
- 前端栈锁定：Vite / TanStack Query / TanStack Router / Zustand / Tailwind CSS 4 /
  shadcn-ui (Radix) / TanStack Virtual / TipTap。
- HTML 邮件在 sandbox iframe 中渲染（不给 `allow-scripts`）。

---

## 理由

### 1. 唯一"移动端已 stable 且生产可用"的非 Flutter 跨端方案

- Tauri 2.0 于 2024-10 发布 stable，当前版本 2.12（2026-09），Android / iOS /
  Windows / macOS / Linux 全平台支持，且 2.12 起支持 Android 端的推送/本地通知插件。
- 对比：Slint 的移动端仍标注"仅 Rust 后端"；egui 的移动端支持需手工适配；
  Dioxus 移动端成熟度不足；GTK/libadwaita 几乎只面向 Linux。

### 2. 邮件类 UI 恰好是 Web 生态的主场

- **虚拟列表**：TanStack Virtual 成熟度远高于任何 Rust 方案的现成实现。
- **富文本编辑器**：TipTap（ProseMirror）提供开箱即用的邮件撰写体验；
  Rust 侧几乎没有等价物，从零写一个至少 2~3 个月。
- **拖拽 / 剪贴板 / 快捷键 / 无障碍 / 手势**：全部现成。
- **HTML 邮件渲染**：iframe 天然提供进程/源隔离，配合 CSP 与 Rust 侧清洗，
  比在 Rust GUI 里手动做 DOM 隔离更可靠。

### 3. 满足"底层用 Rust"

Tauri 本身就是 Rust 应用：`src-tauri` 是一个标准 Cargo crate，插件机制也是 Rust。
所有敏感能力（凭据、协议、数据库、加密）都在 Rust 侧，前端拿不到明文凭据。
这比"Flutter + Rust 桥接"少一层 FFI，也少一套语言。

### 4. 一套代码双形态

Windows 宽屏三栏 ↔ Android 单栏堆叠，用响应式断点 + 路由布局切换，
而不是维护两套 UI 源码。移动端再单独用一套 Tauri 移动工程（`src-tauri/gen/android`）。

### 5. 交付物友好

安装包小（无 Electron runtime）、启动快、可选 `tray-icon` / `single-instance` /
`notification` / `autostart` 等官方插件。

---

## 后果

### 正面

- 移动端 + 桌面端同时可用，且不引入 Dart / TypeScript 之外的第三种语言。
- UI 迭代速度快，能把人力集中在协议与同步这类真正难的地方。
- 前端生态让"多端适配"几乎不额外花成本。

### 负面 / 代价

- **需同时掌握 TS 与 Rust 两种语言。**
- **Windows 依赖 WebView2 运行时**（Win11 自带，Win10 多数已装；需在安装器中处理缺失情况）。
- **Android WebView 版本碎片化**：不同厂商 ROM 的 WebView 版本差异大。
  缓解：`minSdk 24`、避免实验性 CSS/JS API、用 `@supports` 降级、UI 库选兼容性好的版本。
- **HTML 邮件仍需 WebView**：即使换原生 UI 框架，邮件正文也大概率要嵌 WebView，
  所以"用 Web 技术"在这个场景并非主要成本。
- **Android 后台保活需要自写**：Tauri 官方没有"前台服务"插件，
  需要自己写一个 Kotlin plugin 拉起 `ForegroundService`，并与 Rust 侧通信。
  这是 P4 的主要工作量。
- **内存占用高于纯原生 UI**（WebView 进程）。

### 风险与缓解

| 风险 | 缓解 |
|---|---|
| Android 前台服务自写成本超预期 | 尽早（不是 P4 才发现）做一个最小 PoC 验证 Rust 任务能在 Service 中存活 |
| WebView 碎片化导致 UI 异常 | 固定目标 WebView 版本下限；建立真机矩阵测试（至少 3 个主流厂商 ROM） |
| Tauri 移动端生态尚在演进 | 锁定版本；关注官方 roadmap；必要时把"长连接 + 通知"下沉到 Rust 侧原生活动 |

---

## 备选方案

### Flutter + Rust Core

- **优点**：Android 端体验与系统融合最好；UI 一致性极佳；Dart 生态成熟。
- **否决理由**：引入第三种语言与 FFI 层；Flutter 侧的 WebView 依赖
  `flutter_inappwebview` 并不比 Tauri 更省心；跨 iOS/Android/桌面统一发布链更长。
  如果未来要做重度 Android 原生交互（如系统级分享、桌面小组件），可重新评估。

### Slint（纯 Rust UI）

- **优点**：零 WebView、单二进制、内存最低、纯 Rust 技术栈。
- **否决理由**：虚拟列表、富文本编辑器、拖拽、触屏手势全部要自建，
  估多花 3~6 个月；这与"尽快能用"的目标冲突。
  保留为**未来降级选项**：若内存/体积成为硬约束再切换。

### egui / eframe

- **优点**：上手最快，调试工具友好。
- **否决理由**：即时模式不适合大量列表与富文本；观感偏"开发者工具"；
  生产级打磨成本高。适合做原型，不适合做产品。

### Electron

- **优点**：生态最成熟。
- **否决理由**：内存占用是 Tauri 的数倍，与"邮件客户端要常驻后台"直接冲突。

### 原生 Android(Kotlin/Compose) + 复用 Rust core

- **优点**：Android 端体验最好，通知与保活最可靠。
- **否决理由**：要维护两套 UI；跨端复用率低。与"多端"目标相悖。

---

## 后续偏离（2026-10-30）

本 ADR 的**核心决策**（Tauri 2 + React 19 + TypeScript，业务逻辑全在 Rust 侧）**不变且成立**。
但**具体的前端选型在实际推进中被偏离**——记录在这里，避免后来人按上面的文字去找不存在的依赖：

| 原决策 | 实际 | 原因 |
|---|---|---|
| TanStack Query / Router / Zustand | **都没用**，状态用 React 内置 state | 「前端不加第三方依赖」+ 单人维护节奏 |
| Tailwind CSS 4 + shadcn/ui (Radix) | **都没用**，样式用原生 CSS | 同上 |
| TanStack Virtual | **没用**，邮件列表未虚拟化 | 同上；代价是万级列表会卡（待补） |
| TipTap (ProseMirror) | **没用**，自写 `contentEditable` + `document.execCommand` | 同上；代价是编辑器功能有限 |
| SMTP 用 `lettre` | **没用**，自研 SMTP 协议层（rustls + x509-parser） | 需要探证书链、探能力集、控制提交路径的每一步 |
| HTML 邮件在 sandbox iframe 中渲染 | **未实现**，后端消毒后直接渲染进 DOM | 原始 HTML 不出后端 + CSP `img-src` 不含远程来源；代价是消毒器一旦漏网就直接在应用上下文生效（见 [SECURITY §3.0](../SECURITY.md)） |

**这不代表 ADR 被推翻**：选 Tauri + React 的理由（§理由 1–5）依然成立。
变的是「具体用哪个 UI 库」，而那部分本来就写着「前端栈锁定」——
锁定的是**方向**（Web 生态），不是**清单**。
当前实际依赖见 [README 技术栈表](../../README.md)。

---

出现以下任一情况时重新评估：

1. Tauri 移动端生态在 6 个月内没有实质进展，或 Android 端存在无法绕过的架构限制。
2. 性能实测未达 [ARCHITECTURE §9](../ARCHITECTURE.md) 的内存/启动预算。
3. 需要做系统级能力（桌面小组件、系统级搜索、分享扩展）且 Tauri 插件生态无法满足。
4. 团队构成变为"几乎没有前端经验"，Slint/egui 的开发效率反超。
