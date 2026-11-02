# Xremail — 多端多账号邮件客户端 · 详细技术规划

> 版本 v1.2 · Rust + Tauri 2 + React/TS；Windows/Android 为目标平台。
> **本文件保留原始方案供参考，不再是当前执行基线。** 当前范围与门槛以 [ROADMAP](ROADMAP.md) 为准，技术语义以 [ARCHITECTURE](ARCHITECTURE.md)、安全以 [SECURITY](SECURITY.md)、验证以 [TESTING_STRATEGY](TESTING_STRATEGY.md) 为准。
> 用户最新决策：个人研究、AI 编码、国内邮箱优先、Gmail 暂缓。原文的 OAuth 关键路径、固定工期、全部平台支持与“不返工”承诺均已撤回。原文 SQL/伪代码不是可直接执行的迁移或实现规范。
> 实际前端版本以 package.json/lock 为准：TypeScript 6、Vite 8；旧文中的 TS 5.9/Vite 7 是早期选型参考。
> 实际前端**依赖清单**也与原文不同：TanStack Query / Router / Zustand、Tailwind + shadcn/ui、
> TanStack Virtual、TipTap **都没有引入**，SMTP 也不是原文的 `lettre` 而是自研协议层——
> 原因与代价见 [ADR-0001 的后续偏离](adr/0001-ui-stack-tauri-react.md)。
> 维护者：xyyi0719

---

## 0.1 原始决策记录（历史；D3 的执行已暂缓）

| # | 议题 | 决策 | 影响 |
|---|---|---|---|
| D1 | UI 技术栈 | **Tauri 2 + React 19 + TypeScript**（Vite / Tailwind 4 / shadcn-ui / TanStack Virtual+Query+Router / TipTap） | 单一 workspace 覆盖 Win/Android/iOS/macOS/Linux；Rust 承担全部协议与存储，前端纯渲染 |
| D2 | 跨端同步深度 | **先做 L1（IMAP 原生），L2 在 P0 即落地抽象接口与 schema 预留**（`xremail-api` trait + `local_secret`/`label`/`rule` 表），P6 再补自建服务端，**不返工** | MVP 无服务端依赖；多端状态同步开箱即用；增值能力路径已留好 |
| D3 | Gmail 接入 | **走正式 OAuth 2.0 验证流程**（Device Code + XOAUTH2），第 1 周即启动 Google Cloud 项目与验证；同时保留「应用专用密码」作为开发期兜底 | P1 关键路径最长的一项，必须最早启动；国内三家先行交付 |
| D4 | 前端形态 | **不用 Electron / Flutter / Slint** | — |
| D5 | 邮件渲染 | **sandbox iframe（无 allow-scripts）+ Rust 侧 ammonia 清洗 + 严格 CSP** | 必须在 P2 之前实现安全渲染组件，不可延期 |

**D2 的具体含义**（务必理解，避免做出错误承诺）：
- ✅ 跨端会同步：邮件本体、文件夹、已读/未读、星标、移动、删除（全部由 IMAP flag 承载，服务器权威）
- ❌ 跨端**暂不**同步：本地标签、过滤规则、签名、联系人、凭据 → 这些在 v1.0 是**每设备独立**
- ⏳ P6 启用自建 E2EE 同步服务后，上述「本地增值态」才跨端
- 📌 UI 文案需如实说明，不得暗示「所有设置都会同步」

---

## 0. 一句话架构

**Rust Core（协议/存储/同步/加密/搜索） + Tauri 2 外壳（Rust 进程） + React/TS 前端（纯渲染） + 可选自建 E2EE 同步服务（Rust/axum）**

所有网络 I/O、协议解析、数据库、加密都在 Rust 侧完成；前端只负责展示和交互，通过 Tauri Command（请求/响应）+ Tauri Event（增量推送）通信。Android 端前台服务、Windows 端托盘常驻都跑在 Rust 侧，不依赖 WebView 存活。

---

## 1. 范围定义

### 1.1 必须做（MVP）
- 多账户聚合：163 / 126 / QQ / 沃邮箱 / Gmail / Exchange(O365) / 通用 IMAP 自定义
- 每个账户独立凭据，支持「授权码」「密码」「OAuth2(XOAUTH2)」三种认证方式
- 统一收件箱（多账户混排、可按账户筛选）、文件夹树、邮件列表、阅读页
- 增量同步、本地缓存、离线可读
- 发信（多发件人 From/Sender 选择）、草稿、附件、签名
- 全文搜索（SQLite FTS5）
- 实时推送：IMAP IDLE + 桌面/移动通知
- 状态跨端同步（已读/星标/移动/删除）

### 1.2 应该做（v1.x）
- 本地标签 + 过滤/规则引擎
- 稍后处理（Snooze）、待办标记
- 联系人管理
- 繁体/英文界面

> ⚠️ 按 **D2**，以上增值项在 v1.0 为**每设备独立**，不跨端同步。跨端同步能力在 P6 通过自建 E2EE 服务启用，P0 预留接口与 schema。

### 1.3 暂不做
- 端到端加密邮件（PGP/S/MIME）——协议不支持，等 IMAP 阶段
- 日历 / 任务 / 笔记（后续可走 CalDAV/CardDAV 扩展）
- 群发 / 营销邮件
- 服务端代理（不做 IMAP→云端转发，隐私优先）

---

## 2. 现实核查：协议与服务商约束（**最重要的一节，先看这个**）

> 这一节决定了哪些设计是可行的。跳过这节会做出一个跑不起来的客户端。

### 2.1 各服务商接入参数速查

| 服务商 | IMAP | SMTP | 认证方式 | 关键坑 |
|---|---|---|---|---|
| **163 个人** | `imap.163.com:993` (IMAPS) | `smtp.163.com:465` (SSL) / `994` (STARTTLS) | 16 位**客户端授权码** | 需网页版「设置 → POP/SMTP/IMAP」开启，首次需手机短信验证；授权码页面只显示一次 |
| **126** | `imap.126.com:993` | `smtp.126.com:465` | 同上 | 与 163 同体系 |
| **网易企业邮** | `imap.qiye.163.com:993`（华东）/ 华北节点域名不同 | `smtp.qiye.163.com:465/994/587` | 授权码 | **华东/华北/华南节点服务器域名不同**，必须支持自定义域名 |
| **QQ** | `imap.qq.com:993` | `smtp.qq.com:465` | 16 位**授权码** | 设置 → 账号与安全 → 安全设置 → 开启 IMAP/SMTP；**改 QQ 密码会让已发授权码全部失效** |
| **沃邮箱** | `imap.wo.cn:993`（Coxremail 体系） | `smtp.wo.cn:465` | 授权码 | ⚠️ **需实测确认**；Coxremail 系（`tz.mail.wo.cn/coxremail`）近期已改为「客户端必须用授权码，不能用原密码」 |
| **Gmail** | `imap.gmail.com:993` | `smtp.gmail.com:465` | **OAuth2 XOAUTH2**（或应用专用密码） | 见下 |
| **Exchange/O365** | `outlook.office365.com:993` | `smtp.office365.com:587` | OAuth2 / 客户端 ID+密钥 | Basic Auth 已被微软禁用 |

> ⚠️ 上表除 Gmail 外均为**公开文档之外的约定俗成值**，各家会变更、部分地区节点不同。产品上必须提供「高级设置 → 手动填写服务器地址/端口/加密方式/认证方式」+「连接诊断」按钮兜底。

### 2.2 Gmail（历史研究，当前暂缓；以下政策与流程待重新核实）

> 本节不能用于实现授权流程。旧文的 Device Code 适用性、scope 名称、审核周期、Testing 限制与 Production/verification 关系未经完整核实；公开客户端的 client_id 也不等于可保密的 client secret。恢复 Gmail 时按平台与实际 scope 重做核查。

已核实（Google 官方）：

- **2025-03-14 起**，Google Workspace 全面禁止第三方应用使用密码基础认证（Basic Auth）。
- **2025-05-01 起**，Workspace 账号不再支持 "less secure apps"，只能走 OAuth。
- Gmail 的 IMAP/POP/SMTP **仍支持**（协议本身没下线），但认证必须用 **SASL XOAUTH2**。
- 个人版 Gmail 账号在开启两步验证后仍可用「应用专用密码」走基础认证（可作为临时/备用通道）。
- **OAuth 客户端注册要求**：
  - 每个平台（Windows 安装版 / Android / iOS / Web）都要**单独注册一个 OAuth Client**。
  - 桌面应用必须走 **Loopback 或 Device Code 流程**（不能有固定的 redirect_uri 猜测）。
  - 应用处于 **Testing（未验证）状态**时，refresh token **会过期（官方文档为 7 天，且仅允许 100 个测试用户）** → 意味着「未验证应用」无法长期稳定使用。
    > ⚠️ 此条请在实施时以 Google 控制台「OAuth app state overview」页面的最新表格为准复核。
  - 敏感 scope（`mail.imap`、`mail.send` 等）需要提交**验证审核**：要求有域名网站、隐私政策页、OAuth 同意屏品牌化。

**结论与对策：**

| 阶段 | 做法 |
|---|---|
| **P1 第 1 周** | 注册 Google Cloud 项目 → 创建 OAuth 客户端（Windows 安装版 / Android / Device Code 三类）→ 提交验证（需域名 + 隐私政策页 + 同意屏品牌化）→ `client_id` 放 `oauth/clients.json`，**不进 Git**，走环境变量/CI Secret |
| 开发期兜底 | 2FA 个人账号用「应用专用密码」走基础认证，不阻塞开发 |
| 长期发布 | 验证通过 → `publishingStatus = Production`，refresh token 不再 7 天过期 |
| 兜底路径 | 允许用户在「高级设置」填自己的 OAuth Client ID（自带凭据模式） |
| 授权码失效 | OAuth token 过期时前端弹「重新授权」，走 Device Code 流程（可在手机上完成） |

> 按 **D3**，验证流程是 P1 的长路径项。审核期间（P1 第 1~3 周）集中攻坚 163 / QQ / 沃邮箱。

> **这是整个项目最大的外部风险**。建议 **第 1 周就先把 Google Cloud 项目和 OAuth 客户端建好**，走完验证流程要 1–3 周，期间并行开发国内三家。

### 2.3 跨端同步的真相：三类数据，三种同步机制

用户需求「同步所有账号信息」必须拆开看，否则会做出错误承诺：

| 层 | 数据 | 同步机制 | 是否需要自建服务器 | 跨端延迟 |
|---|---|---|---|---|
| **L1 服务器权威态** | 邮件本体、文件夹、已读/未读、星标、移动、删除、标签(服务器标签) | **IMAP 原生**（flag 是服务器状态） | ❌ 不需要 | IDLE 秒级 / 轮询分钟级 |
| **L2 本地增值态** | 本地标签、稍后处理、过滤规则、签名、模板、联系人、阅读进度 | 自建 E2EE 同步服务 **或** 每设备独立 | ✅ 需要（或退化为单机功能） | 秒级（自建）/ 不同步（单机） |
| **L3 凭据** | 授权码/密码/OAuth refresh token | **绝不建议明文过服务器**。建议：主密码派生的密钥加密成 vault blob，服务端只存密文 | ⚠️ 需自建服务（存密文） | 秒级 |

**L1 是 IMAP 白送的**——这意味着「手机标记已读，PC 上也是已读」这件事，只要把 flag 写回服务器就能做到，不需要任何自建服务器。这是本项目「多端同步」的基石。

**L2/L3 需要一个自建同步服务**，我设计为**可选组件**：

```
xremail-sync-server (Rust + axum + SQLite/Postgres)
├── 端点: PUT/GET /vault/:device_group  (加密 blob, 带 revision + HLC)
├── 认证: 用户自建口令 → Argon2id → Bearer token
├── 加密: 客户端端到端（服务端零知识）
│        主密码 → Argon2id(盐, m=64MiB) → XChaCha20-Poly1305 key
│        客户端加密 vault，服务器只存 {ciphertext, revision, updated_at}
├── 同步的数据: 签名、规则、标签、模板、联系人、凭据密文、偏好设置
└── 可选部署: 自己的 VPS / NAS（飞牛/群晖）/ Cloudflare Worker
```

**冲突策略**：字段级 LWW + Hybrid Logical Clock（解决"手机改了标签、PC 同时改了签名"的时钟偏移问题）。凭据合并用"整条 account 记录 last-write-wins + 保留最近 3 个版本"。

**若不想维护服务器**：把 L2 降级为单机功能，跨端只同步 L1，并在 UI 里明说"规则/签名不同步"。这是 P0 阶段的默认形态。

### 2.4 其他现实约束

- **同一账户多端同时 IMAP 登录会互踢**：163/QQ 对单账号同时在线连接数有上限（常见 3~5）。两个设备都开 IDLE 会挤掉第三个。
  → 对策：**「省电模式」（轮询）与「实时模式」（IDLE）二选一**，或做「设备互斥 + 抢占用」协议（用服务器端唯一标识 + 客户端协商）。第一版先做：IDLE 失败自动降级为轮询，并在设置里提示"其他设备可能已断开"。
- **Android 后台保活被国产 ROM 杀**：需要 `Foreground Service` + 常驻通知 + 引导用户加自启动白名单（小米/华为/OPPO/Vivo）。这是必做项，不做则"实时推送"在国产手机上等于不存在。
- **移动网络下 IMAP 长连接会被运营商/NAT 切断**：必须有心跳（`NOOP`）+ 指数退避重连 + 切网检测（Android `ConnectivityManager` 回调）。
- **HTML 邮件是攻击面**：远程图片=追踪像素；`<script>`、外部表单、恶意链接。必须在 sandbox iframe + 严格 CSP 中渲染（见 §6）。

---

## 3. UI 技术栈分析

### 3.1 候选方案对比

| 维度 | **A. Tauri 2 + React/TS** | B. Flutter + Rust Core | C. Slint（纯 Rust） | D. egui/eframe |
|---|---|---|---|---|
| 桌面成熟度 | ✅ 稳定（v2.12） | ✅ 稳定 | ✅ 稳定 | ✅ 稳定 |
| Android 成熟度 | ✅ v2 已 stable | ✅ 一等公民 | ⚠️ 可用（仅 Rust 后端） | ⚠️ 需手工适配 |
| iOS/macOS | ✅ | ✅ | ✅ (iOS 仅 Rust) | ⚠️ 弱 |
| 邮件类 UI 生态 | ⭐⭐⭐⭐⭐ 虚拟列表/富文本/拖拽/无障碍全齐 | ⭐⭐⭐ 需自建或嵌 WebView | ⭐⭐ 需全部自建 | ⭐ 需全部自建 |
| 内存 / 启动速度 | 中（有 WebView） | 中 | 低 | 低 |
| 产物形态 | 需 WebView2 运行时 (Win) / 系统 WebView (Android) | 单 APK/IPA | 单二进制 | 单二进制 |
| 系统集成（通知/钥匙串/托盘/前台服务） | 官方+社区 plugin，需自写 Android Service | 需写 platform channel | 需大量自写 FFI | 需大量自写 FFI |
| 团队技能要求 | TS + Rust | Dart + Rust | Rust(+QML 语法) | Rust |
| HTML 邮件渲染 | iframe 天然隔离 | webview | 需嵌 wry | 需嵌 wry |
| 招聘/交接友好度 | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐ | ⭐⭐ |

### 3.2 推荐：**方案 A — Tauri 2 + React 19 + TypeScript**

理由（四条硬理由）：

1. **唯一"移动端已 stable 且生产可用"的非 Flutter 跨端方案**。Tauri 2.0 于 2024-10 stable，现为 2.12（2026-09）。Android / iOS / Windows / macOS / Linux 一套前端代码。
2. **邮件客户端的 UI 恰好是 Web 生态的主场**：三栏布局 + 十万级邮件虚拟滚动 + 富文本编辑器（写邮件）+ 拖拽（附件）+ 键盘快捷键 + 触屏手势。这些在 React 生态里是"轮子现成"，用 Slint/egui 则要从零写 3~6 个月。
3. **满足"底层用 Rust"**：Tauri 本身就是 Rust 应用（`src-tauri` 是 Rust crate），IMAP/SMTP/SQLite/加密/全文检索 100% Rust；前端只做展示，不碰协议与凭据。
4. **一套代码双形态**：Windows 宽屏三栏 ↔ Android 单栏堆叠，用响应式断点 + 路由布局切换，而非维护两套 UI。

### 3.3 何时该选别的方案

| 选 B (Flutter) 如果 | 选 C (Slint) 如果 | 选 D (egui) 如果 |
|---|---|---|
| 极度看重 Android 原生交互手感与系统级融合，团队愿意引入 Dart | 硬性要求"零 WebView、纯 Rust 单文件二进制" | 只做技术验证/原型，3 天内要看到东西 |

### 3.4 推荐前端技术栈（锁定）

```
运行时/构建   React 19 + TypeScript 5.9 + Vite 7
状态管理      TanStack Query 5 (服务端/同步状态) + Zustand 5 (纯 UI 状态)
路由          TanStack Router (类型安全, 支持 file-based)
UI 基座       Tailwind CSS 4 + shadcn/ui (Radix UI, 可自托管, 无运行时依赖)
虚拟列表      @tanstack/react-virtual (邮件列表, 支撑 10w+ 条)
富文本        TipTap 2 (ProseMirror) — 写邮件
HTML 邮件     自研 <SafeEmailFrame> 组件 (sandbox iframe + ammonia 清洗)
图表/统计     无（不需要）
国际化        react-i18next (zh-CN / en-US)
状态持久      TanStack Query + IndexedDB 镜像 (离线可读 UI 缓存)
测试          Vitest + Testing Library + Playwright (E2E) + cargo test / insta
```

**为什么不用 Ant Design / Arco**：邮件 UI 定制程度高（多栏、虚拟滚动、密集排版），Tailwind + Radix 更好控；shadcn/ui 源码进仓库、零依赖锁定风险。

### 3.5 UI 布局设计（两端）

**Windows / 桌面**
```
┌──────────────────────────────────────────────────────────────┐
│ 标题栏(自定义)  [搜索]              [同步状态] [⚙] [─][□][×]  │
├──────────┬──────────────────────────┬────────────────────────┤
│ 侧边栏    │  邮件列表 (虚拟滚动)        │  阅读/编辑区            │
│ ⬤ 统一收件箱│ ┌──────────────────────┐│  ┌────────────────┐  │
│ ⬤ 账户 A  │ │ ● 发件人  主题摘要  时间 ││  │ 邮件头(可回复/转发)│ │
│ ⬤ 账户 B  │ │ ○ 发件人  主题摘要  时间 ││  ├────────────────┤  │
│ ─────    │ │ …                       ││  │                │  │
│ 📁 文件夹  │ └──────────────────────┘│  │  安全 HTML 正文  │  │
│ 🏷 本地标签│                          │  │  (sandbox iframe)│ │
│ ⚙ 规则    │                          │  └────────────────┘  │
├──────────┴──────────────────────────┴────────────────────────┤
│ 状态栏: ● 已同步 12:03  ·  163(在线) QQ(重连中) Gmail(需授权)  │
└──────────────────────────────────────────────────────────────┘
```
- 三栏可拖拽调宽（持久化到设置）
- 阅读区支持"垂直分栏"/"标签页"两种模式
- 快捷键：`Ctrl+K` 快速搜索、`j/k` 上下、`e` 归档、`r` 回复、`Esc` 返回列表

**Android**
```
┌──────────────────┐      ┌──────────────────┐
│ 列表页            │ ←→  │ 阅读页            │
│ [🔍搜索][⚙][✉️]  │      │ ←  发件人 / 主题   │
│ ⬤ 统一  📁 标签    │      │ [回复][转发][更多] │
│ ┌──────────────┐ │      ├──────────────────┤
│ │ ● 发件人       │ │      │                  │
│ │   主题摘要     │ │      │  正文(HTML)      │
│ │   12:03       │ │      │                  │
│ └──────────────┘ │      │ [回复/全部回复]   │
│  ⋮ 下滑加载更多    │      └──────────────────┘
└──────────────────┘
```
- 统一收件箱 / 账户 / 标签 用底部 Tab 或抽屉
- 滑动操作：左滑归档、右滑删除、下拉刷新
- 收到新邮件：Android 系统通知（本地通知 + 点击深链到对应邮件）
- 写邮件：全屏页 + 工具栏（加粗/列表/附件/签名）

---

## 4. 系统架构

### 4.1 分层

```
┌─────────────────────────────────────────────────────────────┐
│  apps/shell/src (React/TS)                                  │
│  列表页 / 阅读页 / 写邮件 / 设置 / 账户管理                    │
│  仅通过 Tauri Command（请求-响应）与 Tauri Event（增量推送）通信 │
└───────────────▲─────────────────────────────────────────────┘
                │ commands / events
┌───────────────┴─────────────────────────────────────────────┐
│  apps/shell/src-tauri (Rust)                                │
│  ├─ cmd/            命令层：account, mail, folder, search…   │
│  ├─ events/         事件总线：MailUpsert, FlagChanged…       │
│  └─ platform/       Windows 托盘 & Android 前台服务 & 通知    │
└───────────────▲─────────────────────────────────────────────┘
                │
┌───────────────┴─────────────────────────────────────────────┐
│  crates/                                                   │
│  ├─ xremail-core        领域模型、错误、Provider trait、事件定义 │
│  ├─ xremail-sync        同步引擎：AccountActor / FolderActor   │
│  │                     IDLE / CONDSTORE / 重试 / 调度         │
│  ├─ xremail-protocol-imap   IMAP 客户端封装 (async-imap)      │
│  ├─ xremail-protocol-smtp   SMTP 发送 (lettre)                │
│  ├─ xremail-protocol-oauth  OAuth2 / XOAUTH2 / Device Code    │
│  ├─ xremail-store       SQLite (sqlx) schema + DAO + FTS5     │
│  ├─ xremail-crypto      密钥派生、字段加密、vault 封装          │
│  ├─ xremail-provider    各家预设、服务器自动探测、连接诊断      │
│  ├─ xremail-message     MIME 解析/构建、附件、字符集、净化      │
│  └─ xremail-api         领域服务接口（供 UI 与未来 HTTP 复用）  │
└───────────────▲─────────────────────────────────────────────┘
                │ (可选)
┌───────────────┴─────────────────────────────────────────────┐
│  apps/sync-server (Rust/axum)  可选·自建·E2EE                │
└─────────────────────────────────────────────────────────────┘
```

### 4.2 关键设计原则

1. **前端无状态权威**。UI 永远从 SQLite 读；Rust 侧是唯一写入者，通过事件推增量。前端不做本地乐观写（或乐观写后由 Rust 事件校正）。
2. **协议无关**。`xremail-sync` 只认 `MailStore` trait，不认 IMAP。以后接 JMAP/Exchange 时 sync 引擎不动。
3. **每账户一个 Actor**。一个 tokio task 独占该账户的 IMAP 连接与状态机，串行处理该账户的所有命令；账户之间并行。避免多连接被服务器踢。
4. **单写者 + WAL**。SQLite 开启 WAL 模式；所有 flag 写操作先落 `outbox` 表再由 Actor 推送到服务器，失败重试。可断电续传。
5. **凭据永不入库明文**。授权码存 OS 钥匙串（`keyring` crate → Windows 凭据管理器 / Android Keystore）；OAuth refresh token 走 `keyring`；若要做跨端，则用主密码加密后存 sync-server。
6. **可观测**。全链路 `tracing` 日志，Windows 侧有"诊断"页可导出日志包（不含凭据/正文）。

### 4.3 进程与生命周期

| 平台 | 常驻机制 | 说明 |
|---|---|---|
| Windows | 托盘图标 + `tauri-plugin-autostart`（可选开机自启） | 关闭窗口=隐藏到托盘；退出需确认"仍在后台同步" |
| Windows | `tauri-plugin-notification` | 新邮件系统通知（支持免打扰时段） |
| Android | **Foreground Service**（`startForeground` + 常驻通知） | 关键！保证 Rust 侧 tokio 任务与 IMAP IDLE 存活 |
| Android | `tauri-plugin-autostart` 对应「自启动白名单引导」页 | 小米/华为/OPPO 需引导用户加白名单，否则被杀 |
| Android | 通知点击 → deep link → 打开对应邮件 | |
| 通用 | `tauri-plugin-single-instance` | 防止多开导致连接数超限 |

> Android 前台服务实现：写一个 Tauri Android plugin（Kotlin），`Service` 启动后通过 `BroadcastReceiver`/AIDL 与 Rust 通信；Rust 侧常驻 tokio runtime 负责 IMAP。也可更简单：让 Tauri 的 `src-tauri` Activity 在 `onStart` 时拉起 Service，Service 里跑一个独立的 Rust 线程。

---

## 5. 数据模型（SQLite / sqlx）

```sql
-- 账户
CREATE TABLE account (
  id            TEXT PRIMARY KEY,          -- uuid
  email         TEXT NOT NULL,
  provider      TEXT NOT NULL,            -- '163'|'qq'|'wo'|'gmail'|'custom'|'exchange'
  display_name  TEXT,
  color         TEXT,
  auth_kind     TEXT NOT NULL,            -- 'password'|'oauth2'
  -- 服务器配置（允许用户覆盖）
  imap_host     TEXT NOT NULL, imap_port INTEGER NOT NULL, imap_tls TEXT NOT NULL,
  smtp_host     TEXT NOT NULL, smtp_port INTEGER NOT NULL, smtp_tls TEXT NOT NULL,
  smtp_auth     TEXT NOT NULL,            -- 'none'|'plain'|'login'
  username      TEXT NOT NULL,
  -- 凭据只存 keyring 的引用，不存明文
  secret_ref    TEXT,
  oauth_client_id TEXT,
  sync_mode     TEXT NOT NULL DEFAULT 'idle',   -- 'idle' 实时 | 'poll' 省电
  poll_interval INTEGER NOT NULL DEFAULT 300,
  signature_html TEXT,                    -- (可被 sync-server 同步)
  created_at    INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

CREATE TABLE folder (
  id            TEXT PRIMARY KEY,          -- uuid
  account_id    TEXT NOT NULL REFERENCES account(id) ON DELETE CASCADE,
  remote_name   TEXT NOT NULL,            -- 服务器原始名（含层级分隔符）
  remote_id     TEXT,                     -- LIST 返回的 extended name
  local_name    TEXT,                     -- 用户自定义名
  delimiter     TEXT,
  attrs         INTEGER,                  -- \HasChildren 等
  subscribed    INTEGER NOT NULL DEFAULT 1,
  selectable    INTEGER NOT NULL DEFAULT 1,
  last_sync_at  INTEGER, last_uid INTEGER, highest_modseq INTEGER,
  UNIQUE(account_id, remote_name)
);

CREATE TABLE message (
  id            TEXT PRIMARY KEY,
  account_id    TEXT NOT NULL,
  folder_id     TEXT NOT NULL,
  uid           INTEGER NOT NULL,         -- IMAP UID
  uidvalidity   INTEGER NOT NULL,         -- UIDVALIDITY 变更需重建该文件夹
  message_id    TEXT,                     -- RFC Message-ID，用于去重
  in_reply_to   TEXT, references TEXT,
  thread_id     TEXT,                     -- 归一化后的会话键
  subject       TEXT, snippet TEXT,
  from_json     TEXT, to_json TEXT, cc_json TEXT, bcc_json TEXT, reply_to_json TEXT,
  date_sent     INTEGER,                  -- 发件时间 (epoch ms)
  internal_date INTEGER,                  -- 服务器接收时间
  flags         INTEGER NOT NULL DEFAULT 0,
  size          INTEGER,
  has_attach    INTEGER NOT NULL DEFAULT 0,
  is_read       INTEGER NOT NULL DEFAULT 0,
  is_starred    INTEGER NOT NULL DEFAULT 0,
  is_deleted    INTEGER NOT NULL DEFAULT 0,
  local_labels  TEXT,                     -- JSON array of label ids
  draft_id      TEXT,
  UNIQUE(account_id, folder_id, uid)
);
CREATE INDEX idx_msg_list  ON message(account_id, folder_id, date_sent DESC);
CREATE INDEX idx_msg_unread ON message(account_id, is_read, date_sent DESC);
CREATE INDEX idx_msg_msgid ON message(account_id, message_id);

-- 正文单独存，避免列表查询扫大字段
CREATE TABLE message_body (
  message_id    TEXT PRIMARY KEY REFERENCES message(id) ON DELETE CASCADE,
  text_plain    TEXT,
  html_raw      TEXT,                     -- 原始 HTML（未清洗）
  html_size     INTEGER,
  charset       TEXT
);

CREATE TABLE attachment (
  id            TEXT PRIMARY KEY,
  message_id    TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
  filename      TEXT, mime TEXT, size INTEGER, cid TEXT,
  content_id    TEXT,
  inline        INTEGER DEFAULT 0,
  local_path    TEXT,                     -- 落盘位置（内容寻址 sha256）
  sha256        TEXT, fetched INTEGER DEFAULT 0
);

-- 本地标签 / 规则
CREATE TABLE label (
  id TEXT PRIMARY KEY, account_id TEXT, name TEXT, color TEXT,
  parent_id TEXT, order_index INTEGER
);
CREATE TABLE rule (
  id TEXT PRIMARY KEY, account_id TEXT, name TEXT, priority INTEGER,
  conditions_json TEXT, actions_json TEXT, enabled INTEGER, hit_count INTEGER
);

-- 发件箱（本地优先，异步推送）
CREATE TABLE outbox (
  id TEXT PRIMARY KEY, account_id TEXT NOT NULL,
  raw_message BLOB,                       -- RFC822
  draft_id TEXT, state TEXT,              -- 'queued'|'sending'|'sent'|'failed'
  attempts INTEGER DEFAULT 0, last_error TEXT,
  created_at INTEGER, sent_at INTEGER
);

-- 同步状态
CREATE TABLE sync_state (
  account_id TEXT NOT NULL, folder_id TEXT NOT NULL,
  last_uid INTEGER, highest_modseq INTEGER, uidvalidity INTEGER,
  last_checked_at INTEGER, last_error TEXT, consecutive_failures INTEGER DEFAULT 0,
  PRIMARY KEY(account_id, folder_id)
);

-- 全文搜索
CREATE VIRTUAL TABLE message_fts USING fts5(
  subject, from_text, to_text, body_text,
  content='', tokenize='trigram'
);
-- 历史草案，已弃用：content='' 是 contentless，不等于外部内容表。
-- 当前检索投影方案见 ARCHITECTURE §7；此 SQL 不作为迁移依据。

-- 设置 / KV
CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE local_secret (id TEXT PRIMARY KEY, ciphertext BLOB, nonce BLOB, updated_at INTEGER);
```

---

## 6. 同步引擎设计（核心）

### 6.1 状态机（每账户）

```
      ┌──────────┐
      │ Disconnected ──backoff──► Connecting
      │      ▲                            │
      │      │ error                      │ ok
      │      │                            ▼
      │      └───── Authenticating ◄────┘
      │                 │ ok
      │                 ▼
      │            ┌──────────┐   quota/closed   ┌───────────┐
      │            │  Online  │───────────────► │ Backoff   │
      │            │ (IDLE /  │                  │ (退避 1m→30m)│
      │            │  Polling)│◄─────────────── └───────────┘
      │            └────┬─────┘
      │                 │ token expired
      │                 ▼
      │            ┌──────────┐
      └────────────│ Reauth   │  等待用户重新授权
                   └──────────┘
```

### 6.2 首轮全量同步流程（per account）

```
1. TCP 连接 (rustls, ALPN 无, 直接 TLS 1.2+)
2. CAPABILITY → 记录 IDLE / CONDSTORE / QRESYNC / UIDPLUS / LITERAL+ / ENABLE / UTF8=ACCEPT
3. AUTHENTICATE
   - password → LOGIN/PLAIN
   - oauth2   → XOAUTH2 (access token 过期时用 refresh token 换新)
4. LIST "" "*" → 同步 folder 表
5. SELECT INBOX (或已订阅文件夹)
6. 拉取该文件夹的 UID 列表与元信息：
   - 有 CONDSTORE: UID FETCH 1:* (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE MODSEQ)
   - 无 CONDSTORE:  UID SEARCH ALL → 分批 UID FETCH (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)
7. 与本地对比：
   - 新 UID        → 拉 header (BODY.PEEK[HEADER]) + 纯文本 (BODY.PEEK[TEXT])
   - 已知 UID     → 只更新 flags/modseq（增量）
   - 本地有/服务器无 → 标记已删
8. HTML 正文按需拉：
   - 规则：列表页不拉；打开邮件时拉 (BODY.PEEK[])，或首轮若 < 200KB 顺带拉
   - 附件：只取结构 (BODYSTRUCTURE)，内容点击时再拉
9. 写库 → 发出 MailUpsert 事件 → UI 增量刷新
```

### 6.3 增量 / 实时同步

- **IDLE 模式**（默认）：
  - 客户端发 `IDLE`，服务端推送 `* n EXISTS` / `* n EXPUNGE` / `* n FETCH (FLAGS)` / `* n RECENT`
  - 收到 `EXISTS` → 增量拉新 UID
  - 收到 `FETCH FLAGS` → 更新本地 flag
  - 收到 `EXPUNGE` → 标记本地已删
  - 30s 无事件 → 发 `NOOP` 保活
  - 断线 → 指数退避重连（1s, 2s, 5s, 10s, 30s, 1m, 2m, 5m, 15m, 30m，加 ±20% 抖动）
  - **IDLE 不可用/被拒** → 自动降级为轮询模式（`poll_interval`，默认 5 分钟）
- **CONDSTORE 模式**（若服务器支持）：
  - 记录 `HIGHESTMODSEQ`，后续用 `UID FETCH 1:* (FLAGS MODSEQ)` 配合 `MODSEQ` 差异只取变化项 → 省流量
  - 更优：CONDSTORE 允许服务端在 STORE 时回传新 MODSEQ → 多端 flag 冲突可基于 MODSEQ 排序
- **多端 flag 冲突**：
  - 服务器是唯一权威；本地 pending flag 变更走 outbox 队列，优先推送
  - 收到服务器 flag 变更时，若本地有 pending 冲突：比较 `MODSEQ`（有 CONDSTORE）或时间戳 → 新者胜；标记「已在其他设备修改」提示
  - 星标（`\Flagged`）和已读（`\Seen`）使用**单 bit 语义**，天然幂等，冲突成本低
  - 移动/删除用 `UID MOVE`（IMAP MOVE，163/QQ 支持度较好），退化路径 `UID COPY` + `STORE \Deleted` + `EXPUNGE`

### 6.4 发信流程

```
草稿 ──(用户点发送)──► outbox(state='queued')
  └─► SMTP 提交 (lettre, tokio1-rustls)
        ├─ 587 + STARTTLS 优先；465 直接 TLS 兜底
        ├─ 认证：LOGIN / PLAIN / XOAUTH2
        ├─ 大附件（>25MB）→ 暂不支持内联，改提示或后续做 IMAP APPEND
        ├─ 成功 → outbox(state='sent') + 写入本地"已发送"文件夹
        └─ 失败 → 指数退避重试 (最多 5 次) → state='failed' + UI 提示手动重试
```
- 草稿本地优先保存（离线可写），后台发送队列
- 收件人/抄送/密送：注意 163/QQ 对 `Bcc` 在 SMTP 层面要求 `RCPT TO` 而不发 `To` 头
- 防雷同（多设备同草稿重复发送）：outbox 用 `client_id` 去重

### 6.5 调度与并发

| 项目 | 策略 |
|---|---|
| 账户并发 | 每账户 1 条 IMAP 连接，账户间并行（上限 5，同时避免服务器连接数限制） |
| 文件夹并发 | 单账户内串行（同一连接） |
| 批量拉取 | 每批 ≤ 500 个 UID，避免单次响应过大 |
| 抓正文并发 | 同一连接不支持并发，串行；必要时开第二条连接仅用于正文/附件下载（注意连接数限制） |
| 大附件 | 流式落盘（`content-addressed` 目录 `~/.local/share/xremail/blobs/<sha256前2>/<sha256>`） |
| 内存上限 | 单封邮件解析上限 50MB；HTML 渲染只保留前 N KB |

---

## 7. 安全设计

| 面 | 措施 |
|---|---|
| 凭据存储 | `keyring` crate → Windows Credential Manager / Android Keystore / macOS Keychain。**不落 SQLite** |
| 传输 | 全部 TLS（rustls 0.23 + `rustls-native-certs` 读系统根证书，避免中国本地 CA 缺失问题；用户可加自签 CA） |
| 本地敏感数据 | SQLite 文件本身可被读；若用户开启"主密码模式"，用 SQLCipher 或应用层 AES-256-GCM 加密 vault 表 |
| 跨端同步加密 | 客户端端到端：Argon2id 派生 → XChaCha20-Poly1305；服务端零知识，只存密文 |
| HTML 邮件 | ① Rust 侧 `ammonia` 清洗（剥离 script/iframe/style 外部引用）② 前端放入 `<iframe sandbox="allow-popups allow-popups-to-escape-sandbox">`（**不给 allow-scripts**）③ 严格 CSP `default-src 'none'; img-src data: cid: https:; style-src 'unsafe-inline'` ④ 远程图片默认阻断，加载器探测像素并提示 ⑤ 所有外链走 `https://` 且经"重定向警告页" |
| 追踪像素 | 解析 HTML 时检测 1×1 / 隐藏 img，列在"追踪器"面板里供用户一键放行 |
| 链接 | `href` 清洗掉 `javascript:`/`data:`；展示真实 URL 提示 |
| 附件 | 存盘按 mime 判定危险类型（exe/scr/html/svg/hta）→ 默认不打开，提示"以纯文本预览" |
| 日志 | `tracing` 脱敏：绝不记录授权码、token、正文、完整地址（邮箱地址打码） |
| 权限 | Android 只申请 `INTERNET` / `POST_NOTIFICATIONS` / `FOREGROUND_SERVICE` / `FOREGROUND_SERVICE_DATA_SYNC`；不申请通讯录/存储 |
| 供应链 | `cargo-deny`（license/advisory）、`cargo-audit`、CI 跑 `cargo clippy -D warnings` |

---

## 8. 依赖选型（版本已于 crates.io 实时核实）

| 用途 | crate | 版本 | 备注 |
|---|---|---|---|
| 异步运行时 | `tokio` | 1.54 | 全局 multi-thread runtime |
| IMAP | `async-imap` | 0.12.0 | feature `runtime-tokio`；基于 `imap-proto` |
| IMAP 协议类型 | `imap-proto` | 0.17.0 | 直接复用其 `Fetch`/`Flag`/`Command` 类型 |
| SMTP | `lettre` | 0.11.23 | feature `tokio1-rustls` + `builder` |
| SMTP 备选 | `async-smtp` | 0.10.2 | 需要更底层控制时 |
| TLS | `rustls` | 0.23（stable） | + `rustls-native-certs`、`webpki-roots` |
| 数据库 | `sqlx` | 0.9.0 | feature `runtime-tokio`,`sqlite`,`sqlite-bundled`；编译期校验 SQL |
| DB 备选 | `rusqlite` | 0.40.2 | 需要更细粒度控制时 |
| 钥匙串 | `keyring` | 4.2.0 | Windows/Android/macOS 后端 |
| OAuth2 | `oauth2` | 5.0.0 | + `openidconnect` 4.0.1；XOAUTH2 需自写扩展 |
| MIME | `mail-parser` / `mail-builder` | 0.9 / 0.9 | 解析与构建 RFC822 |
| HTML 净化 | `ammonia` | 4.2.4 | + `html5ever` 0.40.1 |
| 加密 | `argon2`、`chacha20poly1305`、`aes-gcm` | 最新稳定 | 派生密钥与 vault |
| 序列化 | `serde` + `serde_json` | 1.x | |
| 错误 | `thiserror` + `anyhow` | 2.x / 1.x | 库用 thiserror，binary 用 anyhow |
| 日志 | `tracing` + `tracing-subscriber` | 0.1 | |
| 调度 | `tokio` timers / `cron` | | 规则/定时发送 |
| 前端壳 | `tauri` | 2.12 | 插件：`tauri-plugin-notification`、`-autostart`、`-single-instance`、`-dialog`、`-opener`、`-fs` |

> ⚠️ 弃用提醒：同步版 `imap` crate 停留在 `3.0.0-alpha.15`（2025-02 未更新），**不要用**。用 `async-imap`。

---

## 9. 里程碑与任务分解

> 估时假设：**1 名全职开发者**（Rust+TS 都能上手）。兼职按 ×2~2.5 折算。

### P0 — 骨架与地基（1 周）
- [ ] Cargo workspace 骨架（§4.1 目录结构）
- [ ] `tauri init` + React/TS + Vite + Tailwind + shadcn 接入
- [ ] CI：GitHub Actions（Windows + Ubuntu）→ fmt / clippy / test / tauri build
- [ ] `xremail-core`：领域模型、错误类型、事件定义
- [ ] `xremail-store`：schema + sqlx 迁移 + 基本 DAO
- [ ] **（D2）落地 L2 预留**：`xremail-api` 抽象 trait（`SyncBackend` / `VaultStore` 接口）+ `local_secret`/`label`/`rule` 表 + `xremail-crypto` 的 KDF/密文封装（P6 直接接自建服务，无需改核心）
- [ ] **（D3）启动 Google Cloud 项目与 OAuth 验证提交**
- [ ] 打通链路：Tauri Command `list_folders` → SQLite → 前端空列表渲染

**出口标准**：`cargo tauri dev` 能跑，前端显示真实 SQLite 里的空账户列表；且 L2 抽象接口已就位。

### P1 — 账户接入与收信（2.5 周）⚠️ 关键路径
- [ ] `xremail-provider`：163/126/QQ/沃邮箱/Gmail/Exchange 预设 + 自动探测
- [ ] `xremail-protocol-imap`：连接、认证（LOGIN/PLAIN/XOAUTH2）、CAPABILITY、LIST、SELECT
- [ ] `xremail-protocol-oauth`：Device Code 流程 + XOAUTH2 字符串构造 + token 刷新
- [ ] `xremail-message`：MIME 解析（正文、附件、字符集转换 GBK/GB18030 ← 国内邮箱常见坑）
- [ ] 账户增删改 UI + 「连接诊断」页（逐项测试 IMAP/SMTP/认证，输出可读错误）
- [ ] 首轮全量同步：文件夹树 + 邮件列表（虚拟滚动）+ 邮件头
- [ ] **⚡ 并行启动：注册 Google Cloud 项目、申请 OAuth 客户端、走验证流程**
- [ ] **⚡ 并行启动：申请 163/QQ/沃邮箱测试账号，实际抓包确认服务器地址与 IDLE 支持度**

**出口标准**：4 家邮箱都能添加并看到收件箱邮件列表。

### P2 — 同步引擎与阅读（2 周）
- [ ] 增量同步 + CONDSTORE（可用时）+ UIDVALIDITY 变更处理
- [ ] IDLE 实时监听 + 心跳 + 重连退避 + 轮询兜底
- [ ] 阅读页：安全 HTML 渲染（sandbox iframe + ammonia）、纯文本回退、图片阻断
- [ ] 附件下载（流式落盘）+ 内联图片（cid）
- [ ] FTS5 全文搜索 + 搜索 UI
- [ ] 状态写回服务器：已读/星标/移动/删除 + outbox 队列
- [ ] 统一收件箱（多账户混排）

**出口标准**：双端同时登录，一端标记已读/移动，另一端 30 秒内可见。

### P3 — 发信与草稿（1.5 周）
- [ ] `xremail-protocol-smtp` 接入 + 多发件人 From 选择
- [ ] 写邮件 UI（TipTap 富文本 + 纯文本模式切换）
- [ ] 草稿自动保存（本地）+ 草稿同步
- [ ] 附件上传（多文件、拖拽、粘贴图片）
- [ ] 签名（HTML/纯文本）、默认每账户签名
- [ ] 发件失败重试 + 手动重发 UI

### P4 — 平台化与实时推送（1.5 周）
- [ ] Windows：托盘、开机自启、通知、关闭即隐藏、单一实例
- [ ] Android：**Foreground Service + 常驻通知**、自启动白名单引导页、通知深链
- [ ] i18n（zh-CN / en-US）
- [ ] 设置页（账户、同步、通知、安全、日志导出）
- [ ] 性能压测：单账户 5 万封邮件的列表滚动与搜索

### P5 — 增值功能（2 周）
- [ ] 本地标签 + 侧边栏 + 标签筛选
- [ ] 过滤规则引擎（条件匹配 + 动作）
- [ ] 稍后处理 / 提醒
- [ ] 联系人（本地 + CardDAV 可选）
- [ ] 移动端手势（滑动归档/删除）

### P6 — 跨端账号信息同步（2~3 周，可选但差异化强；P0 已预留接口）
- [ ] `apps/sync-server`（axum + SQLite + 静态托管）
- [ ] 客户端 E2EE vault：Argon2id → XChaCha20-Poly1305
- [ ] 同步：签名、规则、标签、模板、联系人、凭据（密文）
- [ ] 冲突解决：HLC + 字段级 LWW
- [ ] 一键部署脚本（Docker Compose）+ 客户端配置引导

### P7 — 分发与打磨（2 周）
- [ ] Windows：MSIX/NSIS 打包、代码签名（可选）
- [ ] Android：签名、AAB、F-Droid/Google Play
- [ ] iOS/macOS（若需要）
- [ ] 崩溃报告（可选 Sentry 自建）
- [ ] 文档：README、用户手册、协议排错 FAQ

### 总计
- **MVP（P0–P4）≈ 8.5 周**（约 2 个月全职）
- **v1.0（P0–P6）≈ 11 周**
- 加上 P7 与打磨：**约 3 个月**。兼职请按 ×2 估算。

---

## 10. 目录结构

```
xremail/
├─ Cargo.toml                      # workspace
├─ rust-toolchain.toml
├─ .env.example                    # OAUTH_CLIENT_ID 等
├─ crates/
│  ├─ xremail-core/
│  │  ├─ src/{model.rs, error.rs, event.rs, provider.rs, lib.rs}
│  ├─ xremail-protocol-imap/
│  │  ├─ src/{client.rs, session.rs, fetch.rs, idle.rs, commands.rs}
│  ├─ xremail-protocol-smtp/
│  ├─ xremail-protocol-oauth/
│  │  ├─ src/{google.rs, xoauth2.rs, device_code.rs}
│  ├─ xremail-message/
│  │  ├─ src/{parse.rs, build.rs, charset.rs, sanitize.rs}
│  ├─ xremail-store/
│  │  ├─ migrations/*.sql
│  │  ├─ src/{db.rs, account.rs, folder.rs, message.rs, outbox.rs, fts.rs}
│  ├─ xremail-sync/
│  │  ├─ src/{actor.rs, machine.rs, scheduler.rs, outbox_worker.rs, idler.rs}
│  ├─ xremail-provider/
│  │  ├─ src/{preset.rs, autodiscover.rs, diagnose.rs}
│  ├─ xremail-crypto/
│  │  ├─ src/{kdf.rs, secret.rs, vault.rs}
│  ├─ xremail-api/                 # 领域服务 trait（UI 与未来 HTTP 共用）
│  └─ xremail-testkit/             # mock IMAP/SMTP server（测试用）
├─ apps/
│  ├─ shell/
│  │  ├─ src-tauri/
│  │  │  ├─ src/{lib.rs, main.rs, cmd/, events/, platform/}
│  │  │  ├─ tauri.conf.json
│  │  │  ├─ capabilities/
│  │  │  └─ android/              # Kotlin 前台服务 plugin
│  │  ├─ src/
│  │  │  ├─ routes/  components/  hooks/  stores/  lib/  styles/
│  │  │  └─ features/{mailbox, accounts, composer, search, settings}
│  │  └─ package.json
│  └─ sync-server/                # 可选
├─ docs/
│  ├─ PLAN.md                      # 本文件
│  ├─ PROTOCOLS.md                 # 各服务商实测参数（持续更新）
│  └─ ARCHITECTURE.md
└─ Justfile / Makefile
```

---

## 11. 风险登记册

| # | 风险 | 概率 | 影响 | 缓解措施 |
|---|---|---|---|---|
| R1 | **Gmail OAuth 验证未通过 / 7 天 token 过期** | 高 | 高 | P1 第一周就启动验证；支持"自带 Client ID"与"应用专用密码"双兜底；国内三家先跑通 |
| R2 | 沃邮箱/163/QQ 服务器地址或行为与公开信息不符 | 中 | 中 | 内置"自定义服务器"配置 + 连接诊断页；建立 `PROTOCOLS.md` 持续记录实测值；准备 3~5 个测试账号 |
| R3 | **Android 后台被杀**（国产 ROM） | 高 | 高 | 前台服务 + 常驻通知 + 自启动白名单引导；提供"省电模式"降级为系统 JobScheduler 周期同步 |
| R4 | 同账号多端 IDLE 互踢 | 高 | 中 | IDLE↔轮询自动降级；检测到被踢（`BYE`/登录踢出）自动退避并提示；可设"仅一台设备实时" |
| R5 | HTML 邮件 XSS / 追踪 | 中 | 高 | iframe sandbox（无 allow-scripts）+ ammonia + CSP + 图片阻断 + 链接重定向页；安全测试用例集 |
| R6 | 十万级邮件的列表性能 | 中 | 中 | 虚拟列表 + 列表只查摘要字段 + 正文分表 + FTS 外部内容表；压测纳入 CI |
| R7 | 中国网络下 TLS 根证书问题 | 中 | 中 | 用 `rustls-native-certs`（系统根）+ 允许导入自签 CA + 清晰的诊断错误 |
| R8 | 范围蔓延（做成 Outlook） | 高 | 中 | 严守 §1.1/§1.2 边界；P5 之后才算增值功能；每个新需求问"是否影响 L1 同步" |
| R9 | 自建 sync-server 的运维负担 | 中 | 低 | 做成**可选**组件；提供 Docker 一键部署；不依赖它也能完整使用 L1 功能 |
| R10 | 邮件正文大 / 内存飙升 | 中 | 中 | 流式解析、单封上限、附件落盘不驻内存、列表不载正文 |
| R11 | 字符集（GBK/GB18030/Big5）乱码 | 高 | 中 | `charset` crate + 编码嗅探；国内邮箱实测样本库做回归测试 |
| R12 | 163/QQ 频繁要求短信验证 | 中 | 中 | 授权码一旦生成长期有效（163 官方说明"一个授权码可多客户端"）；失败时 UI 明确引导"重新生成授权码" |

---

## 12. 立即可执行的第一步（本周）

**Day 1**
- [ ] 建 Git 仓库、workspace 骨架、CI
- [ ] 注册 Google Cloud 项目 → 创建 OAuth 客户端（Windows 安装版 / Android / Device Code）→ 记录 Client ID/Secret 到 `.env.local`
- [ ] 申请 1 个 163 邮箱、1 个 QQ 邮箱、1 个沃邮箱、1 个 Gmail 账号，均开启 IMAP/SMTP 并生成授权码
- [ ] 写一个最小 Rust 程序（`cargo script` 或 examples/）：连 163 IMAP → LOGIN → CAPABILITY → 打印 IDLE/CONDSTORE 是否支持 → 拉 INBOX 10 封邮件头。**这一步能立刻验证所有假设**

**Day 2–3**
- [ ] 把验证结果写进 `docs/PROTOCOLS.md`（各家的真实服务器、IDLE 支持、字符集、连接数限制）
- [ ] 搭 `xremail-store` schema + sqlx 迁移
- [ ] Tauri 脚手架 + 前端路由骨架

**Day 4–5**
- [ ] `xremail-protocol-imap` 最小可用：连接 → 认证 → 选 INBOX → FETCH 列表
- [ ] 写 mock IMAP server（`xremail-testkit`）用于后续单测

**Day 6–7**
- [ ] 账户管理 UI + 连接诊断
- [ ] 邮件列表 UI（真实数据）

> 第一个可演示节点：**第 1 周末，看到 163/QQ 真实邮件出现在应用列表里**。

---

## 附录 A：常用命令速查（协议层）

```
CAPABILITY                        # 看服务器支持什么
LIST "" "*"                       # 列文件夹
SELECT INBOX
UID SEARCH ALL                    # 所有 UID
UID FETCH 1:* (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)
UID FETCH <uid> (BODY.PEEK[HEADER])
UID FETCH <uid> (BODY.PEEK[TEXT])
UID FETCH <uid> (BODY.PEEK[])     # 完整邮件
UID FETCH <uid> (BODYSTRUCTURE)   # 附件结构
UID STORE <uid> +FLAGS (\Seen)
UID STORE <uid> -FLAGS (\Seen)
UID COPY <uid> "Target"
UID MOVE <uid> "Target"
UID EXPUNGE <uid>
IDLE / DONE                       # 推送
NOOP                              # 保活
```

## 附录 B：国内邮箱字符集坑

163/QQ/沃邮箱的邮件大量使用 **GB2312 / GBK / GB18030**，部分无 charset 声明但内容是 GBK。处理顺序：
1. 有 charset → 严格按 charset 解码（GB18030 优先，覆盖 GBK/GB2312）
2. 无 charset → 尝试 UTF-8 严格解码 → 失败则 GB18030 严格解码 → 再失败则按 latin1 兜底
3. 标题（Subject）必须做 RFC 2047 解码（`=?gb2312?B?...?=`）
4. 处理后统一在数据库存 UTF-8

---

*文档结束。实现过程中请持续更新 `docs/PROTOCOLS.md`（实测协议参数）与本文件的状态标记。*
