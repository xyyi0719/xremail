# 贡献指南

> 维护者：xyyi0719

## 环境

| 依赖 | 版本 | 安装 |
|---|---|---|
| Rust | ≥ 1.94 | `rustup update stable` |
| Node.js | ≥ 20 | `fnm install 20` / `nvm use 20` |
| pnpm / npm | 任意 | 仓库用 npm（`package-lock.json` 已提交） |
| sqlx-cli 0.9 | 仅改 SQL / 迁移时 | `cargo install sqlx-cli --version 0.9.0 --no-default-features --features sqlite,rustls` |
| Android SDK + NDK + JDK 17 | 仅 Android | Android Studio |
| Windows: WebView2 + MSVC Build Tools | 仅 Windows 桌面 | [Tauri 前置依赖](https://v2.tauri.app/start/prerequisites/) |

> 2026-10-21 本机已就绪：**JDK 17.0.20.1 + Android SDK + NDK 27.2** 均已装好（见
> [ENVIRONMENT §1.8](ENVIRONMENT.md)），Android 产物可构建；剩余卡点是符号链接
> 权限（需重新登录一次），不再是「没装工具链」。
>
> MSRV 由 1.88 上提至 1.94：sqlx 0.9.0 声明的 MSRV 是 1.94.0。

```bash
npm install --prefix apps/shell
cargo check --workspace
```

### SQL 变更流程（sqlx 离线数据）

`xremail-store` 的 SQL 用 `sqlx` 的 `query!` / `query_as!` 宏，**在编译期**对 schema 校验：
列名、列数、类型、可空性写错都会直接编译失败（不等到运行时）。

仓库里提交了 `.sqlx/` 离线查询数据，因此普通编译**不需要数据库**：

```bash
# 日常编译/测试：直接用仓库里的离线数据
SQLX_OFFLINE=true cargo check --workspace
```

改了迁移或 SQL 之后，必须重新生成离线数据：

```bash
# 1. 造一个带最新 schema 的库（SQLite 下 sqlx 不会自动建库文件，需先 touch）
touch target/xremail-dev.db
sqlx migrate run --source crates/xremail-store/migrations

# 2. 连库重新生成离线数据（需临时关掉离线模式）
SQLX_OFFLINE=false DATABASE_URL="sqlite://target/xremail-dev.db" \
  cargo sqlx prepare --workspace -- --all-targets
```

生成的 `.sqlx/*.json` **要一并提交**。CI 的「Verify sqlx offline data」步骤用
`cargo sqlx prepare --workspace --check` 校验它是否与迁移/查询同步，过期即失败。

---

## 目录与职责

| 路径 | 放什么 | 不放什么 |
|---|---|---|
| `crates/xremail-core` | 领域模型、错误、事件 | 任何 `sqlx` / `tokio` / `tauri` 引用 |
| `crates/xremail-store` | SQL、迁移、DAO | 网络调用 |
| `crates/xremail-sync` | 同步状态机、调度、outbox | UI 相关的一切 |
| `crates/xremail-protocol-*` | 协议细节 | 数据库、业务规则 |
| `crates/xremail-message` | MIME 结构、charset 解码、RFC 5322 头 | HTML 清洗（渲染层的事）、落库、远程资源 |
| `crates/xremail-testkit` | 假服务器、合成样本、故障注入 | 生产代码 |
| `crates/xremail-diagnostics` | 需要同时用协议层与内容层的诊断工具 | 生产代码 |
| `crates/xremail-api` | trait 与 DTO | 实现（实现放各领域 crate） |
| `apps/shell/src-tauri` | Command、Event、平台集成 | 业务规则（应在 crate 里） |
| `apps/shell/src` | React 组件、路由、样式 | 协议细节、SQL、凭据处理 |

**依赖方向铁律**：`xremail-core` ← 一切；`xremail-api` ← 一切；UI → `xremail-api`；`xremail-sync` 不认识 UI。

另外两条实测出来的：

* `xremail-sync` **可以**依赖 `store` / `protocol` / `message`（它要把拉回来的东西解析并落库），但**不得**依赖 `xremail-testkit`（那是测试替身）。
* `xremail-protocol-imap` **不得**依赖 `xremail-message`——协议层只管线路格式。
  需要同时用两者的工具放到 `xremail-diagnostics`。

---

## 代码风格

### Rust

- `cargo fmt`（默认 rustfmt，不改配置）
- `cargo clippy --workspace --all-targets -- -D warnings` **必须零告警**
- 公开 API 写 `///` 文档注释；crate 顶部写 `//!` 说明
- 错误类型用 `thiserror`；应用层错误用 `anyhow`；**库 crate 不要用 `anyhow`**
- 禁止 `unwrap()` / `expect()` 于可能失败的运行时路径（测试代码除外）
- 禁止 `panic!` 作为错误传播手段
- 公开结构体加 `#[derive(Debug, Clone, PartialEq, Eq)]`（便于断言）
- 不使用 `unsafe`（如确需，单独 PR 说明理由）

### TypeScript / React

- `strict: true`，禁止 `any`（实在不行用 `unknown` + 收窄）
- 组件用函数式 + Hooks；**不写 class 组件**
- **不引入第三方前端依赖**（当前唯一的例外是 Tauri 官方插件 `@tauri-apps/plugin-dialog`）：
  状态用 React 内置 `useState` / `useReducer`，样式用原生 CSS，
  富文本用 `contentEditable` 自写（见 `src/components/RichTextEditor.tsx`）
- **禁止在 React 里直接调 `invoke` 之外的方式取数据**；所有数据经 `xremail-api` 的 Tauri Command
- 邮件列表**目前未虚拟化**（原因见 README「与早期规划不同」）：数据量到万级前不值得为此加依赖
- 可访问性：交互元素必须有 `aria-label` 或可见文本

### 命名

| 类型 | 约定 | 例 |
|---|---|---|
| Rust 结构体/枚举 | `PascalCase` | `MessageFlags` |
| Rust 函数/变量 | `snake_case` | `list_messages` |
| TS 组件 | `PascalCase` | `MessageList.tsx` |
| TS 函数/变量 | `camelCase` | `useAccounts()` |
| DB 表 | `snake_case` 复数 | `message` / `outbox` |
| 事件名 | `PascalCase` 变体 | `AppEvent::MessageUpserted` |
| 迁移文件 | `NNNN_snake_case.sql` | `0001_init.sql` |

---

## 命名规范

| 场景 | 写法 | 例子 |
|---|---|---|
| 展示名（UI / 窗口 / 网页标题 / 文档标题） | `Xremail` | `# Xremail` |
| Rust crate | 全小写 | `xremail-core`、`xremail-store` |
| npm 包 | 全小写 | `xremail-shell` |
| Tauri identifier | 全小写反域名 | `com.xremail.app` |
| 目录 | 全小写 | `crates/xremail-core/` |
| GitHub 仓库 | 全小写 | `xyyi0719/xremail` |

> Rust crate 名与 npm 包名**必须全小写**（crates.io / npm 规范），因此只有展示层用 `Xremail`。
> Tauri identifier 决定应用在系统中的身份（注册表 / bundle id / 包名），**发布后不可更改**。

---

## 提交规范

[Conventional Commits](https://www.conventionalcommits.org/)：

```
<type>(<scope>): <subject>

<body>

<footer>
```

**type**

| type | 用途 |
|---|---|
| `feat` | 新功能 |
| `fix` | 修 bug |
| `docs` | 只改文档 |
| `refactor` | 不改行为的重构 |
| `perf` | 性能 |
| `test` | 测试 |
| `build` | 构建 / 依赖 |
| `ci` | CI 配置 |
| `chore` | 杂项 |

**scope**（尽量用 crate / 模块名）：`core` `store` `sync` `imap` `smtp` `crypto` `provider` `shell` `ui` `docs`

示例：

```
feat(sync): 支持 CONDSTORE 增量拉取
fix(imap): UIDVALIDITY 变更时清空本地缓存并重拉
docs(protocols): 补充 163 的 IDLE 实测结果
feat(ui): 邮件列表支持按账户筛选
```

---

## 推送前先探明连通性

本机 `github.com` 被 DNS 劫持，直连必失败。**访问 GitHub（推送、拉取、调 API）之前，先探明能不能连通，再执行操作**：

```bash
git ls-remote origin      # 推送走的真实通道（SSH over 443）
gh auth status            # API 通道
```

探不通就不推，先按 [ENVIRONMENT.md §1.5.1](ENVIRONMENT.md) 定位到具体通道再修。
代理地址是本机手动设置、随网络变化，**不要写进文档或脚本**，用前读注册表当前值。

---

## 测试策略

以 [TESTING_STRATEGY.md](TESTING_STRATEGY.md) 为当前验收规范。下表里 Rust 单测 / 集成测试 / 前端单测 / 端到端四层**已在跑**
（端到端于 2026-11-06 落地：`npm run test:e2e --prefix apps/shell`，2 个 spec / 5 条），性能基准仍未建；
真实 Tauri/WebView 与 Android 另需宿主验证。
AI 工作流与审查模板见 [AI_DEVELOPMENT_GUIDE.md](AI_DEVELOPMENT_GUIDE.md)。
本机 shell / 代理 / 工具链限制与各平台差异见 [ENVIRONMENT.md](ENVIRONMENT.md)；写脚本或 CI 前先查它。

| 层 | 工具 | 要求 |
|---|---|---|
| Rust 单元测试 | `cargo test` | 状态机转换、编解码、charset 嗅探、冲突仲裁必须有单测 |
| Rust 集成测试 | `cargo test` + `xremail-testkit` | Mock IMAP/SMTP 服务器跑通完整同步流程 |
| SQL 迁移 | `sqlx` | 迁移可前滚；破坏性变更需写 `docs/adr/` |
| 前端单测 | Vitest + Testing Library | 关键组件与 hook |
| 端到端 | Playwright | 账户添加 → 收信 → 阅读 → 已读回写 的主链路 |
| 回归样本 | `crates/xremail-testkit/src/samples/` | 各家真实邮件的**脱敏**样本（含 GBK 样本） |
| 性能 | 待建基准 | 见 [ARCHITECTURE §10](ARCHITECTURE.md)，先测基线再设阈值 |

**门槛**：新增协议解析 / 状态机 / 冲突处理代码，必须同时带测试。

用例扩写的优先级、阶段归属与「算完成」标准见 [TESTING_STRATEGY §5](TESTING_STRATEGY.md)。

---

## 文档规则

| 什么时候 | 改哪份 |
|---|---|
| 实测了某服务商的参数或行为 | [PROTOCOLS.md](PROTOCOLS.md) —— **最重要的贡献** |
| 改了架构或模块职责 | [ARCHITECTURE.md](ARCHITECTURE.md) |
| 做了不可逆的技术决策 | 新增 `docs/adr/NNNN-*.md`（模板见 ADR-0001） |
| 改了范围 / 里程碑 / 风险 | [PLAN.md](PLAN.md) + [ROADMAP.md](ROADMAP.md) |
| 改了安全策略 | [SECURITY.md](SECURITY.md) |
| 用户可见的行为变化 | [CHANGELOG.md](../CHANGELOG.md) |

**ADR 模板**

```markdown
# ADR-NNNN：<标题>

- 状态：提议 / 已接受 / 已废弃 / 被 ADR-XXXX 取代
- 日期：YYYY-MM-DD
- 决策者：<姓名>

## 背景
<为什么现在要做这个决定>

## 决策
<决定了什么>

## 理由
<为什么选它>

## 后果
### 正面
### 负面 / 代价
### 风险与缓解

## 备选方案
<考虑过但没选的，及为什么>

## 复审条件
<什么情况下重新评估这个决定>
```

---

## PR 流程

1. 从 `main` 切分支：`feat/xxx` / `fix/xxx` / `docs/xxx`
2. 本地过一遍：`cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` + `npm run build --prefix apps/shell`
   - 改了 `docs/` 或源码注释里的相对链接，再加一步 `python scripts/check_doc_links.py`
     （Windows 上用 `python`，CI 上用 `python3`；**本地与 CI 是同一份脚本**）
   - 链接层级写错一位不会有人立刻发现，所以靠这道闸门——2026-10-26 就靠它逮到过一次断链
3. PR 描述写清：**改了什么 / 为什么 / 怎么验证 / 截图（UI 变更）**
4. CI 必须绿
5. 至少一位 reviewer 批准（可自己 review 别人的）

**分支保护**：`main` 不允许直接 push。

---

## 敏感信息红线

以下**永远不提交**（已在 `.gitignore` 中，但也要自觉）：

- 邮箱授权码 / 密码 / OAuth token
- `oauth/clients.json`（OAuth Client ID/Secret）
- 真实邮箱地址（测试代码里用 `test@example.com`）
- 真实邮件正文样本（**必须脱敏**后再进 `xremail-testkit`）

发现泄漏：**立即重置该凭据**，然后 `git filter-repo` 清理历史并通知所有克隆者。
