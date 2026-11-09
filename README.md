# Xremail

**多端、多账号、隐私优先的邮件客户端。** 核心用 Rust 写，界面用 Web 技术，Windows / Android 优先，iOS / macOS / Linux 随后。
> 维护者：xyyi0719

计划先接入 163、QQ 与沃邮箱，保留通用 IMAP/SMTP 手动配置；Gmail 暂缓，其他服务商后续评估。**已能实际收发邮件**（163 / QQ 的真实收发往返已验收）；授权码存平台钥匙串（Windows / Linux），后台调度（`Scheduler` + IDLE）与桌面托盘 / 开机自启 / 新邮件通知已落地。
**L1 邮件状态由 IMAP 服务器同步**，时效受网络、服务商和平台后台限制；本地设置等 L2 数据暂不跨端。

---

## 当前状态

> **v0.3.0 · P0–P3 已出关；P4 的 Windows 侧已交付、Android 侧只验到「能编出 APK」；P5-L（Linux）主要工作已落地；P5 增值功能 / P6 未开始**（2026-11-09）
>
> 架构持续复核。当前设计见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。
> 2026-10-30 验证（当时的数字）：`cargo fmt` / `cargo clippy -D warnings` / `cargo test`（**687 个 Rust 用例**）/
> `npm typecheck` / `npm test`（**67 个前端用例**）/ `npm build` / `cargo sqlx prepare --check` 均通过，
> CI 四个 job 全绿。
> 2026-11-06 复跑：`cargo test --workspace` **987 passed / 0 failed / 1 ignored**（`--list` 988）、
> `npm test` **15 个文件 / 95 条**、`npm run test:e2e`（Playwright）**2 个文件 / 5 条**全绿。
> 2026-11-09 复跑（托盘 + T8 乱序响应 + folder 角色 + DKIM 跨平台用例合并后）：
> `cargo fmt --all --check` / `cargo clippy --workspace --all-targets -D warnings` /
> `cargo test --workspace` **1061 passed / 0 failed / 1 ignored**（`--list` 1062）/
> `npm typecheck` / `npm test` 95 条 / Playwright 5 条 / `cargo sqlx prepare --check` /
> `check_doc_links.py` 全绿。
> 2026-11-09 再复跑（T10 补上 IMAP 响应解析的 24 条属性测试后）：
> `cargo test --workspace` **1112 passed / 0 failed / 1 ignored**（`--list` 1113）、
> `cargo clippy --workspace --all-targets -D warnings` 零警告、`cargo fmt --all -- --check` 干净。
> 2026-11-09 本轮复核（只改文档）：`cargo test --workspace -- --list` 点数 **1113**（逐包相加同为 1113），
> 其中 1 条 `#[ignore]`（真实邮箱往返 `real_round_trip_between_two_mailboxes`）→ 实际执行 **1112**；
> `npx vitest run` 实测 **15 个文件 / 95 条**；Playwright **2 个 spec / 5 条**。
> **CI 口径要说清**：`.github/workflows/ci.yml` 有 **4 个 job**（`Rust (ubuntu-latest)` / `Rust (windows-latest)` /
> `Frontend` / `Docs sanity`），它们上一次的绿是 **2026-11-09 的 `e0af98b`**（当时 CI 计数 1056）；
> 其后的 `R46` / `R47` / 新邮件通知等提交**本地全绿，但 CI 是否已经跑绿没有核**——本机连不上 GitHub Actions。
>
> **已落地**：存储层（`xremail-store`，含 FTS5 全文索引）、密文封装（`xremail-crypto`）、
> L2 契约（`xremail-api`）、同步引擎（`xremail-sync`，含 `Scheduler` actor、IDLE 与崩溃对账）、
> IMAP / SMTP 协议层（`xremail-protocol-imap` / `xremail-protocol-smtp`）、内容解析（`xremail-message`）、
> **DKIM 签名（`xremail-dkim`）**、HTML 消毒（`xremail-core::html`）、假服务器（`xremail-testkit`）、
> 诊断工具（`xremail-diagnostics`）；外壳侧另有托盘 / 开机自启（`src/tray.rs`）与新邮件通知（`src/notify.rs`）、
> 平台钥匙串（Windows 凭据管理器 / Linux Secret Service）。
>
> **能读、能搜、能写、能发**（2026-10-30）：UI 里能看真实邮箱的邮件（正文经**后端消毒**后渲染），
> 本地**全文搜索**可用（命中带片段），撰写面板可发信（草稿 / 撤回 / 发送，校验以后端为准）。
> 163 / QQ 两家的**真实收发往返**已实测通过；30 封真实邮件解码零错误。
> 协议事实见 [docs/PROTOCOLS.md](docs/PROTOCOLS.md)。
>
> **还缺**：**Android 的运行时一条没验**（本机无设备与模拟器；钥匙串在 Android 上仍是桩、
> `lib.rs` 没有 `Resumed`/`Suspended` 分支、Android 13+ 的通知运行时权限能否拿到未验）、
> macOS / Android 的钥匙串适配、邮件列表未虚拟化、跨端推送未做；
> 通知与「点通知跳到那一封」在真实桌面会话里是否成立**未验**。
> 想看进度：[docs/ROADMAP.md](docs/ROADMAP.md)

| 阶段 | 状态 |
|---|---|
| P0 骨架与地基 | ✅ 已出关 |
| P1 账户接入与收信 | ✅ 已出关（真实邮箱接入应用，UI 里能看） |
| P2 同步引擎与阅读 | 🟡 引擎、后台调度器与 HTML 安全门已完成，只缺列表虚拟化 |
| P3 发信与草稿 | ✅ 已出关（真实邮箱收发往返已验收） |
| P4 平台化与实时推送 | 🟡 Windows 侧已交付；**Android 侧只做到「能编出 APK」**，运行时未验 |
| P5 增值功能（标签 / 规则 / 联系人） | ⬜ 未开始 |
| P5-L Linux 支持（可选） | 🟡 打包、钥匙串、托盘、通知、开机自启、WebKitGTK 沙箱验证已落地；真实桌面会话与非 Debian 发行版未验 |
| P6 跨端账号信息同步（可选） | ⬜ 未开始 |

> 路线图以验收门槛描述，不承诺固定日期（个人研究项目）。

---

## 架构

```
┌──────────────────────────────────────────────────────────┐
│  React 19 + TypeScript             仅渲染，无业务逻辑    │
└────────────────────────▲─────────────────────────────────┘
                         │ Tauri Command（请求/响应）
                         │ Tauri Event（增量推送）
┌────────────────────────┴─────────────────────────────────┐
│  src-tauri (Rust)           托盘 / 通知 / 钥匙串适配器   │
├──────────────────────────────────────────────────────────┤
│  crates/                     全部业务逻辑在 Rust 侧      │
│  xremail-core    领域模型 + HTML 消毒                    │
│  xremail-store  SQLite + FTS5  xremail-crypto 加密封装   │
│  xremail-sync   同步引擎        xremail-message MIME 解析│
│  xremail-protocol-imap / -smtp   IMAP / SMTP 协议层      │
│  xremail-api    领域服务接口    xremail-diagnostics 诊断 │
│  xremail-dkim   DKIM 签名       xremail-testkit 假服务器 │
└──────────────────────────────────────────────────────────┘
```

**目标架构：UI 经 Rust 服务查询 SQLite，持久化写入只走 Rust。** 事件用于刷新提示，漏事件后可重新查询；上图中**除 `xremail-provider`（厂商预设）外均已创建**——它还没开工。
平台 SecretStore 适配器**已经开了工、且两个平台都落地了**：Windows 走凭据管理器（`src-tauri/src/secrets/windows_store.rs`），
Linux 走 D-Bus 的 Secret Service（`src-tauri/src/secrets/linux_store.rs`，`keyring` 3.x）；**macOS 与 Android 仍是明确报错的桩**。
托盘（`src-tauri/src/tray.rs`）与新邮件通知（`src-tauri/src/notify.rs`）属于外壳层，不在 crate 列表里。

---

## 技术栈

| 层 | 选型 | 版本 |
|---|---|---|
| 外壳 | Tauri（Rust） | 2.12 |
| 前端 | React + TypeScript + Vite | 19 / 6.x / 8.x |
| 前端测试 | Vitest + Testing Library；E2E 用 Playwright（**不拉 Tauri 窗口**，注入 `window.__TAURI_INTERNALS__` 桩） | 5.x / 1.65 |
| 前端样式 | **原生 CSS**（无 Tailwind / shadcn） | — |
| 前端状态 | **React 内置 state**（无 TanStack / Zustand） | — |
| 富文本 | **自写**（contentEditable + `document.execCommand`） | — |
| 运行时 | tokio | 1.54 |
| IMAP | async-imap + imap-proto | 0.12 / 0.17 |
| SMTP | **自研协议层**（rustls + x509-parser，**未用 lettre**） | 0.23 / 0.16 |
| 存储 | SQLite (sqlx, WAL) + FTS5 | 0.9 |
| 全文索引 | FTS5 **trigram**（外部内容表 + 触发器维护） | — |
| 密钥库 | 平台 SecretStore：**Windows 凭据管理器**（2026-10-26，`CredWriteW` 系列）、**Linux Secret Service**（2026-11-09，`keyring` + `sync-secret-service`，走 D-Bus、**不链 `libsecret`**）；macOS / Android 是「如实报错」的桩 | keyring 3.6（**只在 Linux 上编译**：`[target.'cfg(target_os = "linux")'.dependencies]`） |
| 密码学 | argon2 + chacha20poly1305；DKIM 用 `ring`（RSA）+ `zeroize` | 0.6 / 0.11 / 0.17 / 1 |

> **与早期规划不同**：原计划里的 TanStack Query / Router / Zustand、Tailwind + shadcn/ui、
> TanStack Virtual、TipTap、lettre **都没有引入**。原因有二：一条是「前端不加第三方依赖」
> 的约束，一条是单人维护的实际节奏——富文本用 `contentEditable` 自写、SMTP 协议层自己实现、
> 样式用原生 CSS。代价是**邮件列表未虚拟化**、编辑器功能有限。
> 实际版本以 Cargo.lock / package-lock.json 为准。

---

## 目录结构

```
xremail/
├─ crates/                 Rust 库：
│  └─ core / store / crypto / api / sync / message / dkim
│     protocol-imap / protocol-smtp / testkit / diagnostics
├─ apps/
│  └─ shell/               Tauri 应用（桌面 + 移动）
│     ├─ src/              React 前端
│     └─ src-tauri/        Rust 外壳 + Command + Event
├─ docs/                   本文档集
└─ .github/workflows/      CI（Rust / Frontend / Docs sanity）
```

> P6 的自建 E2EE 同步服务（`apps/sync-server`）**尚未创建**；`crates/xremail-provider`
> （厂商预设）同样还没开工。

---

## 开发环境

| 依赖 | 版本 | 说明 |
|---|---|---|
| Rust | ≥ 1.94 | `rustup update stable`（MSRV 是 1.94：sqlx 0.9.0 的下限） |
| Node.js | ≥ 20 | 建议用 fnm / nvm |
| Android 端 | Android Studio + NDK + JDK 17 | 仅编译 Android 需要 |
| 平台工具 | Tauri 2 的 [前置依赖](https://v2.tauri.app/start/prerequisites/) | Windows 需 WebView2 + MSVC Build Tools |

```bash
# 首次
npm install --prefix apps/shell

# 开发（桌面）
npm run dev --prefix apps/shell        # 仅前端
npm run tauri dev --prefix apps/shell  # 完整应用

# Android
npm run tauri android init --prefix apps/shell   # 只需执行一次
npm run tauri android dev --prefix apps/shell

# Rust 侧
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/PLAN.md](docs/PLAN.md) | 原始详细规划（历史参考，当前执行范围以 ROADMAP 为准） |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 模块职责、线程模型、事件流、数据流时序、扩展指南 |
| [docs/PROTOCOLS.md](docs/PROTOCOLS.md) | 各服务商 IMAP/SMTP 实测参数与坑（**持续更新，实测即记录**） |
| [docs/SECURITY.md](docs/SECURITY.md) | 威胁模型、凭据处理、HTML 邮件沙箱规范、密钥派生 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 里程碑看板、每阶段完成定义、本周任务 |
| [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) | 开发约定、提交规范 |
| [docs/TESTING_STRATEGY.md](docs/TESTING_STRATEGY.md) | 测试分层、故障注入、协议/安全/平台验收 |
| [docs/AI_DEVELOPMENT_GUIDE.md](docs/AI_DEVELOPMENT_GUIDE.md) | AI 编码流程、任务模板、证据要求 |
| [docs/ISSUES_AND_RISKS.md](docs/ISSUES_AND_RISKS.md) | 风险登记册、旧审查纠错、关闭条件 |
| [docs/ENVIRONMENT.md](docs/ENVIRONMENT.md) | 本机 shell / 代理 / DNS / 工具链限制，各平台 WebView 与 API 差异 |
| [docs/adr/](docs/adr/) | 架构决策记录（ADR）：为什么选 Tauri、为什么先做 L1、Gmail 怎么处理 |
| [CHANGELOG.md](CHANGELOG.md) | 变更日志 |

---

## 三个必须先知道的现实约束

1. **国内邮箱优先，Gmail 暂缓**。个人研究项目按验收门槛推进，不承诺固定工期。目标服务商为 163、QQ 与沃邮箱（`wo.cn`）；恢复 Gmail 时重新核实 OAuth。当前范围见 [ROADMAP](docs/ROADMAP.md)。
2. **"同步所有账号信息"分三层**。邮件与已读/星标/移动/删除 → IMAP 原生跨端同步，零服务器；本地标签/规则/签名/联系人 → v1.0 每设备独立；凭据 → 存 OS 钥匙串，绝不明文同步。详见 [ADR-0002](docs/adr/0002-sync-depth-l1-first.md)。
3. **协议能力与后台时效需要实测**。服务商连接限制、IDLE/CONDSTORE 支持度不能按品牌推定；不支持时保守降级。Android 前台服务不保证永久后台同步，须真机验证。

---

## 贡献

见 [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md)。特别是：**只要你实测了某个服务商的协议参数，请把结果写进 [docs/PROTOCOLS.md](docs/PROTOCOLS.md)** —— 这份文档的价值全靠真实数据积累。

## 许可

MIT OR Apache-2.0
