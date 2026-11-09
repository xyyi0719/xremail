# 交接文档：接手前要知道的事

> 写给下一个人（可能是几个月后的你，也可能是别人）。
> 这份文档**不重复** [ARCHITECTURE](ARCHITECTURE.md) 的设计、也不重复
> [ROADMAP](ROADMAP.md) 的进度——它只回答一件事：
> **这个仓库现在处于什么状态，哪些话能信、哪些不能，还差什么、差在哪。**
>
> 最后更新：2026-11-10

---

## 0. 一句话

**核心邮件引擎是可靠的**（收、发、同步、搜索、附件都通，有 1147 条用例、
CI 四个 job 全绿），**但它只在两个平台上被真正验证过一半**：
Windows 上的真实收发验过，Linux 只能跑到"能启动、能建库"，
**Android 一行运行时都没跑过**。

---

## 1. 先读这一节：这个仓库最容易踩的坑

### 1.1 文档会骗你，代码不会

这不是修辞。**同一条风险（R39）在文档里翻转过三次**：

| 时间 | 文档说 | 实际 |
|---|---|---|
| 2026-11-06 | 「已修复」 | **没实现**（`TransferOutcome` 未绑定即丢弃） |
| 2026-11-09 早 | 「文档超前，实际未实现」 | 对 |
| 2026-11-09 晚 | 「已实现」 | **对**（第 7 轮真的补上了） |

三次里有两次错，**两次都是"改状态时没读代码"**。

**所以：看到「已修复」先去代码里确认一遍。** 反过来也一样——
文档说「未实现」也不一定是真的没做。

### 1.2 行号一定会失效，文本片段不会

这个仓库的行号引用在几轮之内全部错位过。改文档时**用标识符原文
`grep -F` 定位**，不要信 `文件:行号`。
（`ARCHITECTURE.md` §4.4 里已经写了这条规则，照着做。）

### 1.3 工作树里有多人改动时，这两个命令是破坏性的

- `git checkout -- <file>`：会**清掉别人未提交的改动**（真发生过一次，
  一个子代理把自己的改动清空了，靠逐段重建才救回来）
- `cargo sqlx prepare`：会**先删光整个 `.sqlx/` 再重写**。如果那一刻
  工作树编译不过，你会得到一个空目录（也真发生过两次）

要撤销自己的编辑就用 `edit` 工具改回去。

### 1.4 看到 CI 红，先确认 job 列表是否完整

2026-11-10 有一次运行**只创建了 2 个 job**（应该有 4 个），整体结论是
`failure`——但那跟代码无关，是 GitHub Actions 的抖动，重跑就好了。
**直接去查代码会白找一轮。**

### 1.5 `sqlx::query!` 会把 INTEGER 放宽成 REAL

写过 `started_at < now - lease` 这种条件，边界那一毫秒判不出来。
**必须在 SQL 里化简成 `started_at <= ?`**（R32 踩过，R48 差点再踩一次）。

### 1.6 迁移 SQL 的注释里不能有分号

sqlx 按 `;` 切语句，注释里多一个分号会让后面的语句整体错位——
最坏的结果是**迁移记成成功、列却没加上**。

---

## 2. 现在到底处于什么状态

### 2.1 能用的

| 能力 | 状态 | 证据 |
|---|---|---|
| 收信、同步、断线重连 | ✅ | 163/QQ/沃三家真实账号实测；1000+ 条用例 |
| 发信、草稿、附件、DKIM | ✅ | 163 ↔ QQ **真实双向收发**验过 |
| 全文搜索（中文短词） | ✅ | FTS5 + trigram，短词退化成 LIKE |
| HTML 正文（消毒后渲染） | ✅ | 后端消毒，原始 HTML 不出后端；WebKitGTK 与 Chromium 98/98 一致 |
| 多账户、多文件夹、移动/删除 | ✅ | 删除优先进垃圾箱（按服务器宣告的 `\Trash`） |
| Linux 打包（deb / AppImage） | ✅ | 7.7 MiB / 87.2 MiB，Xvfb 里真跑过 |

### 2.2 只做了一半的

| 能力 | 做到哪 | 差什么 |
|---|---|---|
| **Linux 运行** | Xvfb 里能启动、建库、跑完 11 条迁移、托盘图标渲染出来 | 真实桌面会话（输入、面板、通知弹窗）；非 Debian 系发行版 |
| **Linux 钥匙串** | Secret Service 适配已落地，无守护进程时如实报错 | 真钥匙串上的写→读→删往返（本机没有 `gnome-keyring-daemon`） |
| **通知** | 代码已落地、判据有 8 条单测 | 真实桌面里是否真的弹出；点击通知（**没做**） |
| **托盘** | 图标能建出来、菜单四项（含开机自启开关） | 左键事件在 Linux 上**根本不发**（平台能力，不是 bug）；点击行为没验 |
| **Android** | 能编出 APK（9 个 crate 全过 `aarch64-linux-android`） | **运行时一行没跑过**：数据目录、SQLite、阅读、发信、前后台、通知、Keystore |

### 2.3 完全没做的

- **Android Keystore 适配**（现在是桩，授权码存不进去，每次冷启动要重填）
- **macOS / iOS**（未验证，钥匙串也是桩）
- **P5**（标签、规则、联系人、稍后处理）**完全没开始**
- **P6**（L2 端到端加密同步）**完全没开始**
- **rpm 打包**（本机没有 `rpmbuild`）

---

## 3. 66 条风险里，哪些还开着

风险登记册在 [ISSUES_AND_RISKS](ISSUES_AND_RISKS.md)。**大部分已闭环**，
还开着的只有下面这些——按"要不要动手"分类：

### 3.1 环境阻塞，本机做不了

| 编号 | 内容 | 需要什么 |
|---|---|---|
| **R08** | Android 生命周期、后台预算、通知全未验 | 一台 Android 设备或 AVD |
| **R21** | 沃邮箱 SMTP 用不了（服务器只收静态 RSA 套件） | 换 TLS 栈（SChannel / OpenSSL）——**这是个设计决策**，不是改代码 |
| **R09** | 同步/下载没有总配额与取消入口 | 设计决策 + 实现 |
| **R12** | L2 远端密钥恢复未定义 | P6 前的威胁建模 |

### 3.2 已知残余，已证明"不能简单修"

| 编号 | 内容 | 为什么停在这 |
|---|---|---|
| **R65** | GBK 字节恰好是合法 UTF-8 时静默解错 | **实测**：两个汉字的 GBK 4 字节，100%（8916182/8916182）都能被 UTF-8 解成一对非 ASCII 字符。任何"合法 UTF-8 优先"的判定都必然判错，真正的判别只能靠字形打分，不新增依赖做不了 |
| **R24** | 某些服务器可能把"存副本"补成两份 | 163/QQ 都不触发（163 立刻可查、QQ 拒重复 `APPEND`） |

### 3.3 偶发问题，需要更多数据

| 编号 | 内容 | 下一步 |
|---|---|---|
| **R49** | Windows 凭据管理器用例偶尔 `set` 成功但 `get` 读不到 | 已改为串行执行（缓解），但没有根因证据。**要修先统计失败率**，别急着加重试 |

### 3.4 进行中 / 部分完成

| 编号 | 内容 | 剩什么 |
|---|---|---|
| **R14** | 测试扩写 T1–T10 | T1–T8 已完成；**T10 仍缺覆盖率引导的 fuzz** |
| **R11** | 文档治理 | 常态化工作，没有终点 |

---

## 4. 如果你要碰某个模块，先知道这些

### 4.1 改 `xremail-store`（存储层）

- **状态机是核心风险区**。R48（发信队列没租约）、R50（收尾函数不认归属）
  都是从这儿挖出来的
- 改 `query!` 之后**必须重新生成 `.sqlx/`**：
  ```bash
  export SQLX_OFFLINE=false
  DB=.devcache/prep.db; rm -f $DB; touch $DB
  export DATABASE_URL=sqlite://$DB
  sqlx migrate run --source crates/xremail-store/migrations
  cargo sqlx prepare --workspace -- --all-targets
  ```
  **前提是工作树能编译**，否则 `.sqlx` 会被删光

### 4.2 改 `xremail-sync`（同步层）

- **`attempts` 自增**这件事极其容易漏：漏一处就是"每轮重发"
  （R26 踩过，R57 又差点踩）
- `uncertain` **绝不能**改回 `queued`/`retryable`——那等于替服务器判断
  "上次没执行"
- panic 边界要包住**两条**路径：`tick` **和** `wait_for_change`
  （R28 修过一处，历史上漏过一次）

### 4.3 改 `xremail-protocol-imap`

- **不要直接用 async-imap 的 `uid_fetch`/`uid_store`**——它们在服务器回
  `NO`/`BAD` 时返回 `Ok`（R15）。一律用 `run_checked`
- **`BYE` 不能一律判死**：RFC 3501 §7.1.5 允许它跟在 `OK` 之后，
  一律判死会让"登录成功后的 BYE"被报成认证失败（R58，与 R25 同一个坑）
- 超时之后会话就不可信了，但**接口上没有任何强制**（R62）

### 4.4 改 `xremail-message`（内容解析）

- **部件号口径**必须与 IMAP 一致：顶层 multipart 占 `1`，
  `message/rfc822` 压 `0`（R44/R45，各踩过一次）
- 折行**不能按字节切**——切在汉字中间，单独看是合法 UTF-8，
  接收端拼回来就是乱码
- 头字段值要先 `trim`（R66）

### 4.5 改 `apps/shell`（外壳）

- Command 只做「校验 → 调服务 → 返回 DTO」，业务逻辑在服务层
- **宿主能力**（钥匙串 / 托盘 / 通知）都是平台分发的，加新平台要在
  `secrets/mod.rs` 的 `platform_default_store()` 里加 `cfg` 分支——
  注意别写成 `not(windows)`，那会和 Linux 分支同时成立
- 事件是**刷新提示**，不是数据来源

---

## 5. 怎么验证你改的东西

### 5.1 本机能跑的（每次改完都该跑）

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_HOME=<repo>/.devcache/cargo-home
export CARGO_TARGET_DIR=<repo>/.devcache/cargo-target
export SQLX_OFFLINE=true

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -j 4 --no-fail-fast
python3 scripts/check_doc_links.py
```

**模拟 CI 的钥匙串环境**（本机有 D-Bus，CI 没有）：

```bash
env -u DBUS_SESSION_BUS_ADDRESS cargo test --workspace -j 4 --no-fail-fast
```

### 5.2 需要额外环境的

| 验什么 | 怎么跑 | 需要 |
|---|---|---|
| Linux 打包产物能不能跑 | `python3 scripts/verify_linux_runtime.py [appimage]` | `xvfb-run`、打包产物（**要先重新打包**，见下） |
| HTML 消毒在真实 WebView 里 | `xvfb-run -a python3 scripts/webkit_sandbox_probe.py --engine=both` | WebKitGTK、Playwright |
| 真钥匙串往返 | `XREMAIL_SECRET_SERVICE_PROBE=1 dbus-run-session -- <测试二进制>` | `gnome-keyring-daemon` |
| 真实邮箱收发 | 先 `python3 scripts/precheck_real_mail.py` 预检，再 `cargo test --test real_mail_roundtrip -- --ignored` | `~/.xremail/accounts.txt`（**在仓库之外**；`.gitignore` 没覆盖这个名字，靠"不在仓库里"保证安全） |

> **一个真踩过的坑**：`verify_linux_runtime.py` 会比对「库里跑出来的迁移条数」
> 与「`migrations/*.sql` 的文件数」。加了新迁移之后**必须重新打包**，
> 否则那条断言会报「库里 10 条、migrations 里有 11 个」——**这是脚本在对，
> 不是脚本太严**。它抓的正是「打包产物比代码旧」这件事。
>
> ```bash
> cd apps/shell && npm run tauri build -- --bundles deb
> ```

### 5.3 本机**验不了**的

- Windows 真机上的托盘、通知、凭据管理器（只能在 CI 的 Windows job 上跑）
- Android 任何运行时行为
- macOS / iOS 任何东西

---

## 6. 几条方法论（比代码更值得留着）

1. **变异测试是最便宜的"这个用例真的有用吗"**。这个仓库的很多用例
   在变异后照样绿——说明它们保护的不是它们声称的东西
2. **"没抓住"要如实记**。比如 R64 把字符切改成字节切，用例没变红
   （切点碰巧都合法）——所以那条用例证明的是"行不超长"，
   **不证明**"切点合法"
3. **"看起来该修"和"值得修"是两件事**。R62 如果照原方案做，
   要改 61 处调用方，去解决一个已被现有分类覆盖、且从未复现的问题
4. **测试结果要连着"编译是否成功"一起看**。`grep` 过滤会把编译错误
   吃掉，然后你会在一个已经修好的分支上绕十几分钟
5. **平台相关的用例不能当作全局结论**。"六关本地全绿"曾经是在 Windows
   上得出的，而 CI 有 ubuntu job——那边一直在红

---

## 7. 如果要继续，建议的顺序

1. **拿到一台 Android 设备或 AVD** → 跑 P4 的运行时验收。这是最大的一块空白
2. **在真实桌面会话里跑一遍**（`scripts/verify_linux_runtime.py` 已经能跑
   大部分，缺的是真实输入与面板）
3. **装 `gnome-keyring-daemon`** → 验完 Linux 钥匙串的往返
4. **决定 R21**（沃邮箱的 TLS）要不要投入——这是唯一剩下的服务商接入问题
5. **P5 / P6** 是全新的功能线，开始前先做威胁建模与需求确认

---

相关：[架构](ARCHITECTURE.md) · [路线图](ROADMAP.md) · [风险](ISSUES_AND_RISKS.md) · [测试策略](TESTING_STRATEGY.md) · [环境](ENVIRONMENT.md)
