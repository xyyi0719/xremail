# 测试策略

> 状态：规范已定义，测试设施**已部分落地**（Rust 1147 个用例 / 前端 95 个用例 / Playwright E2E 5 条，
> 假 IMAP 与假 SMTP 服务器可用；fuzz 未开始，见 §5）。适用于个人研究项目与 AI 编码工作流。
> 目标：以可重复证据证明数据正确、安全隔离和失败恢复，不以覆盖率或编译成功代替验收。
> 维护者：xyyi0719

## 1. 测试分层与边界

| 层级 | 验证对象 | 隔离方式 | 触发 |
|---|---|---|---|
| 单元 | 状态转换、UID 集合差分、字符集、操作意图合并 | 纯函数、固定时钟/随机源 | 每次修改 |
| 存储集成 | 迁移、事务、外键、FTS、队列恢复 | 每例独立临时文件 SQLite，启用生产配置 | 存储/同步改动 |
| 协议契约 | IMAP/SMTP 解析、能力降级、断线与超时 | 可脚本化假服务器，按真实协议字节交互 | 协议改动 |
| 应用集成 | Command → 服务 → 存储 → 事件 | 真实服务与数据库，假网络/SecretStore | 业务链路改动 |
| UI/宿主 | 用户操作、HTML 沙箱、权限、通知 | 浏览器测试 + 真实 Tauri/WebView | UI/安全/平台改动 |
| 实机接入 | 服务商差异、TLS、授权、移动生命周期 | 专用测试邮箱/设备，人工批准 | 接入与里程碑验收 |

Mock 通过不能证明服务商兼容。Playwright 浏览器通过不能证明 WebView2/Android WebView 或 Tauri 权限正确。
不假定 Windows 支持现成的 Tauri WebDriver 流程；宿主自动化方案需 PoC，未覆盖项保留人工验收。

## 2. 核心场景矩阵

### 2.1 身份与同步

- 身份键是 `(account_id, folder_id, uidvalidity, uid)`；同 UID 在不同 epoch/folder 不得串信。
- UIDVALIDITY 从 A 变 B：缓存、游标与旧操作一起处理；崩溃发生在事务前后均可恢复。
- 旧 epoch 的 pending 操作不得自动套用新 UID；保留冲突原因及用户可见状态。
- 邮件删除、UID 存在空洞、空邮箱、文件夹重命名/移除、重复消息 ID、无消息 ID。
- 初次同步中断后继续；已提交批次不重复建行；收到 EXPUNGE 后不能误用序号当 UID。
- CAPABILITY 在认证前后变化；无 IDLE/CONDSTORE/UIDPLUS/MOVE 时走明确降级。
- IDLE → DONE → tagged completion → 普通命令；不得一边 IDLE 一边复用同一流。

### 2.2 操作队列与不确定结果

- 本地状态与操作记录同事务提交，事件仅在提交后发出。
- `+Seen` 与 `-Seen` 是相反意图；对不同 bit 的操作不应整组覆盖 FLAGS。
- CONDSTORE：UNCHANGEDSINCE 成功、MODIFIED 冲突、重新读取后重试有界。
- 无 CONDSTORE：新快照 + bit 级意图重放；不宣称消除了竞态。
- 服务端已执行但应答丢失：flag 可核对重放；MOVE/COPY/APPEND/SMTP 不得一概盲重试。
- SMTP 最终 DATA 已提交但未收到确认：进入 `uncertain`，提示可能已发送，手动重发前确认。
- **发信路径的三个具体断言**（2026-10-28 已全部有用例）：
  - 前置失败（连不上 / TLS 握手失败）→ 这封信**肯定没发出去** → 记 `retryable`，
    **不能**停在 `running`（否则会被回收逻辑误判成「发到一半」）
  - 服务器 4xx 拒收 → `retryable`；5xx 拒收 → `failed`（重发多少次都一样）
  - DATA 后丢应答 → `uncertain`，**不自动重发**
- 排队后冻结的字节不可改：用户改内容必须另存成新草稿
- Bcc 只进信封（`RCPT TO`），不进邮件头
- 无 UIDPLUS 时禁止普通 EXPUNGE 清除其他客户端标记的邮件；需保守策略或提示不支持。
- 队列上限、取消、重试预算、永久失败与退出重启后的状态恢复。

### 2.3 存储与检索

- 新库迁移、已有版本升级、重复启动、失败回滚、锁竞争、磁盘满/只读。
- FTS：元数据先到、正文晚到、正文修改、删除、级联删除、事务回滚、重建后一致。
- 中文 1/2/3 字、英文、混合字符、标点与非法 MATCH 表达式；参数化查询。
- trigram 对不足 3 字的查询不得默默返回“无结果”；显式采用受限回退或提示限制。
- 分页排序含稳定 tie-breaker；同时间戳与同步中插入不会丢行/重复。

### 2.4 安全与解析

- RFC 2047、multipart、嵌套上限、GB18030/GBK、错误 charset、损坏 MIME、超大 literal。
- HTML 攻击集覆盖脚本、事件属性、SVG、表单、base、meta refresh、相对 URL、srcset、CSS url/@import、样式隐藏与追踪资源。
- 默认阅读恶意邮件：网络请求计数为零、脚本不执行、不能访问父页面/IPC、不能自行导航。
- 放行图片与打开链接必须来自用户动作，走宿主校验；测试拒绝 file/javascript/data 等危险 URL。
- SecretStore 读取/写入/删除失败、锁定、用户取消；不得明文降级，日志与诊断包不得包含测试 canary secret。
- 账户归属校验与附件路径穿越；扩展名、Content-Type 不可信。

#### 2.4.1 在 WebKitGTK 上的实测结果（2026-11-09）

上面「默认阅读恶意邮件」那几条原先只在 Chromium（Playwright）上量过。Linux 的 WebView 是 **WebKitGTK**，
结论不能直接沿用，所以新增了一个**本地手工验收工具** `scripts/webkit_sandbox_probe.py`
（跑法与局限写在脚本头注释里；**不接 CI**——CI 上没有 WebKitGTK 的图形环境，接进去只会让 CI 变红）：

```bash
# 一次性准备：把真消毒器包成 stdin → stdout 的小 CLI（不加 Rust 目标、不加依赖），细节见脚本头注释
export PATH="$HOME/.cargo/bin:$PATH" CARGO_HOME="$PWD/.devcache/cargo-home" \
       CARGO_TARGET_DIR="$PWD/.devcache/cargo-target" SQLX_OFFLINE=true
cargo build -p xremail-core
mkdir -p "$CARGO_TARGET_DIR/htmlcli"
python3 scripts/webkit_sandbox_probe.py --write-cli-rs > "$CARGO_TARGET_DIR/htmlcli/main.rs"
rustc --edition 2021 -L "$CARGO_TARGET_DIR/debug/deps" \
      --extern xremail_core="$CARGO_TARGET_DIR/debug/libxremail_core.rlib" \
      "$CARGO_TARGET_DIR/htmlcli/main.rs" -o "$CARGO_TARGET_DIR/htmlcli/sanitize"

# 跑
xvfb-run -a python3 scripts/webkit_sandbox_probe.py                    # WebKitGTK，CSP 走 <meta>
xvfb-run -a python3 scripts/webkit_sandbox_probe.py --csp=header       # CSP 走响应头（更接近 Tauri）
PLAYWRIGHT_BROWSERS_PATH="$PWD/.devcache/pw-browsers" \
  xvfb-run -a python3 scripts/webkit_sandbox_probe.py --engine=both    # 与 Chromium 逐条对照
```

**环境**：WebKitGTK **2.52.6**（`webkit2gtk-4.1`，`WebKit2 4.1`），GTK3 `WebKit2.WebView` 放在 `Gtk.OffscreenWindow` 里，
整个进程跑在 `xvfb-run` 的 Xvfb 上（无 GPU，WebGL 关掉）。页面源：WebKitGTK 侧 `about:blank`
（`--csp=header` 时是 `http://127.0.0.1:<临时端口>`），Chromium 侧 `http://127.0.0.1:1`（文档由 `page.route` mock）。
CSP 取 `apps/shell/src-tauri/tauri.conf.json` 的 `app.security.csp` **原文**。

| 用例 | 断言 | WebKitGTK | Chromium | 一致？ |
|---|---|---|---|---|
| A1 | 真消毒器的输出喂进 DOM：script / iframe / svg / form / base / meta / style / `on*` / `style` 属性 / `javascript:` / `data:` 链接**一个都没有**（实测链接属性 `[null, 'https://example.com/a', null]`） | 10/10 PASS | 10/10 PASS | 是 |
| B0 | 阳性对照：同一页面去掉 CSP，图片请求真的打到监听端（TCP ≥ 1） | 2/2 PASS | 2/2 PASS | 是 |
| B1 | CSP `img-src` 不含 http/https：`http://` / `https://` / 协议相对 / `srcset` / CSS 背景 / 表格背景**监听端 0 个 TCP 连接**；同页 `data:` 图片 `complete/naturalWidth = true/1`（**能**显示） | 8/8 PASS | 8/8 PASS | 是 |
| C1 | 绕过消毒器直接塞进 DOM：内联 `<script>`（解析期与 `innerHTML` 注入两种）、事件属性、`javascript:`、`srcdoc` 里的内联脚本都**不执行**（心跳数组为空、`window.__pwned` 未定义） | 6/6 PASS | 6/6 PASS | 是 |
| J1 / J0 | 合成点击 `javascript:` 链接：CSP 下不执行、不导航；**无 CSP 的对照页点击后会执行**（证明是 CSP 起作用，不是点击没生效） | 3/3 + 3/3 PASS | 同 | 是 |
| S1 | `<iframe sandbox>` 里脚本**不执行**；`sandbox=allow-scripts` 时脚本执行但访问父窗口抛 `SecurityError`；**不带 sandbox 的 `srcdoc` iframe 能读并改掉父窗口的全局变量**（父页读回 `FORKED`）；`srcdoc` 子文档继承父页面的 CSP，无 nonce 的载荷脚本被拦 | 9/9 PASS | 9/9 PASS | 是 |
| F1 | CSP `frame-src` 不含 http：远程 iframe 0 请求；`srcdoc` iframe 里的内联脚本不执行，内容照样渲染 | 3/3 PASS | 3/3 PASS | 是 |
| N-file / N-data | `location.href` 强导航到 `file:///etc/passwd` / `data:`：被拦，主文档仍是原来的 DOM（引擎给出「The URL can't be shown」错误页） | 各 2/2 PASS | 同 | 是 |
| N-http | `location.href` 强导航到远程 http 页面：**主文档被换掉了**（WebKitGTK 的 `get_uri()` 变成目标地址、监听端收到 `/nav-target.html`；Chromium 执行上下文被销毁） | 2/2 PASS（如实记录为「会导航」） | 同 | 是 |

**总计**：WebKitGTK 49/49、Chromium 49/49；`--engine=both` 合计 **98/98 PASS，逐条结论差异 0 项**。
这个 49 是**可以按脚本复算**的：每个用例的断言条数就是脚本里 `assert_table([...])` 的项数——
A1 10、B0 1、B1 8、C1 6、J1 3、J0 3、S1 9、F1 3、N-file 2、N-data 2、N-http 2（数值导航在 `navigated`
分支只出 1 条，在「没被换掉」分支出 2 条，所以按 2 计是**上界**）= 49。**2026-11-09 复核：表格里的
「10/10、2/2、8/8、6/6、3/3 + 3/3、9/9、3/3、2/2」与脚本逐项对得上**（B0 实为 1 条，文档写 2/2 是按
「无 CSP 对照的整组」记的，偏宽一格；不影响「全 PASS」这个结论）。
注意：这 98 条是**探针脚本的断言项**，不是 `cargo test` 用例，**不计入** §5.3 的 1113。
把 WebKitGTK 侧的 CSP 改成**响应头**下发（`--csp=header`）再跑：**49/49 PASS**；引擎控制台原话是
`Refused to load http://127.0.0.1:PORT/img-http.gif because it does not appear in the img-src directive of the Content Security Policy.`

**结论**：

1. §2.4「默认阅读恶意邮件」四条里，**「网络请求为零、脚本不执行、不能访问父页面」在 WebKitGTK 上同样成立**；
   **「不能自行导航」只对 `file:` / `data:` 成立**——CSP 不管导航，强导航到远程 http 会把主文档换掉。
   产品里靠 `MessageView.tsx` 的 `preventDefault` + `/^(https?:|mailto:)/i` 白名单兜；
   消毒器只保证 `href` 是 http(s)/mailto，**不能**保证「点了不会进 webview」。
2. 文档原先说「WebKitGTK 与 Chromium 行为不同」，**在这组用例上没有看到证据**（差异 0 项）。
   差异可能存在，但需要能区分两者的用例（例如 `allow-popups` 系、`blob:` 在不透明源里的可用性），
   本工具没覆盖，**不写结论**。

**这套工具量不到的东西（如实列出）**：

- **IPC / Capability**：探针页面里没有 `__TAURI_INTERNALS__`、没有 `tauri://` 自定义协议，
  「不能访问 IPC」只能间接看「iframe 碰不到父窗口」，Tauri 的 ACL 层没验。
- **真实源**：`tauri://localhost`（Linux 上实际是 `http://tauri.localhost`）没被模拟，依赖 `'self'` 的结论要打折扣。
- **macOS / WKWebView**：本机没有。
- **真 GPU / 合成器**下的 WebKitGTK：本机只有 Xvfb + 软件渲染（启动时 `libEGL warning: DRI3 error`）。
- **链接交给系统浏览器**：`opener` 插件的端到端行为没验（探针里没有系统浏览器、没有插件）。
- **原生对话框 / 系统协议处理器**：`mailto:`、`confirm()` 这类需要真人点击并交给系统处理的路径，探针没有替身，没有出结论。

## 3. 测试数据与可重复性

- 优先合成 `.eml` 与脚本响应，真实样本先脱敏再入库；去掉邮箱、地址、token、追踪 URL 与个人附件。
- 夹具记录来源类型、编码、预期与对应问题编号；不把真实授权码写进源码或测试快照。
- 假服务器可控制断线点、延迟、乱序的非请求响应、能力集及错误返回。
- 定时测试使用虚拟时间；记录随机种子；不依赖任意 sleep 或公网可用性。
- 破坏性真实测试使用专用文件夹和合成邮件，必须人工明确批准。

## 4. 质量门槛

| 门槛 | 必需证据 |
|---|---|
| 合并普通修改 | fmt、clippy、受影响测试、前端类型检查/构建（适用时） |
| 数据/安全修改 | 成功、失败、边界、崩溃/并发反例；测试应能在错误实现上失败 |
| 接入某服务商 | 日期、客户端版本、区域/网络、认证方式、认证后能力、收发结果；仅实测范围有效 |
| 里程碑完成 | ROADMAP 验收链路 + 残余限制；未运行项标为未验证 |

不设统一 70% 覆盖率作为安全证明。优先覆盖状态转换、破坏性操作、恢复路径；后续有基线再设增量覆盖目标。

## 5. 测试用例扩写计划

> 目的：把「该测什么」（§2）变成**可排期、可验收**的用例集。
> 基线：2026-10-20 为 **4 个用例**（全部在 `xremail-core` 错误分类）。下列目标量级是**计划值**，不是承诺值；未写的用例不计入。

### 5.1 扩写顺序（按边际价值从高到低）

| 优先级 | 目标 | 为什么优先 | 依赖 | 阶段 | 状态（2026-10-23） |
|---|---|---|---|---|---|
| **T1** | `xremail-store` 迁移与事务 | 迁移错了=数据损坏，且是后续一切的地基 | 无 | P0 | ✅ 完成（两个迁移；新库/幂等/回滚/升级路径） |
| **T2** | 同步引擎纯函数（UID 差分、epoch 切换、bit 意图合并） | 同步是核心价值，这些函数无网络依赖，可直接在 CI 跑 | 无 | P0/P2 | ✅ 完成 |
| **T3** | `xremail-crypto` 密文封装（KDF 参数、AEAD 往返、篡改检测） | 错了=凭据丢失或静默失效 | 无 | P0 | ✅ 完成 |
| **T4** | `xremail-api` L2 契约（mock CAS 竞争） | 契约错了=P6 返工 | 无 | P0 | ✅ 完成 |
| **T5** | MIME / charset 解析（含 GBK/GB18030 样本） | 乱码是国内邮箱最直接的用户投诉 | 合成 `.eml` | P1 | ✅ 完成（`xremail-message`；**30 封真实邮件解码零错误**） |
| **T6** | HTML 清洗攻击样本集（OWASP XSS + 追踪像素） | 安全门槛，且纯函数、CI 友好 | 无 | P2 | ✅ 完成（2026-10-30，`xremail-core::html`：`javascript:` / `data:` / `on*` / `style` 里的 `url(javascript:)` / 整封邮件的 `html`+`body` 壳；**未开始**：真正完备的 OWASP 样本集） |
| **T7** | 假 IMAP/SMTP 服务器跑完整同步链路 | 覆盖状态机集成，但要先有 T2 | T2 | P2 | ✅ 完成（IMAP：假服务器 + 拉取 + 回写（标记与移动/删除两条通道）端到端；SMTP：假服务器 + 能力协商 + 提交往返，`xremail-testkit`） |
| **T8** | 故障注入（断线、超时、乱序响应、丢应答） | 验证「不确定结果不盲重发」等语义 | T7 | P2/P3 | 🟡 **部分**（2026-11-09 核实：与代码一致，维持 🟡）：断线/超时/丢应答已覆盖（→ `uncertain`）；**乱序响应已覆盖**（2026-11-09）：`Fault::InterleaveUntagged` 把一条**别的** untagged 响应插在命令自己的数据与 tagged 完成响应**之间**，11 条端到端用例证明命令层只认自己的 tag（`crates/xremail-protocol-imap/tests/interleaved.rs`）；仍缺的是「真服务商按随机时机插队」的样本 |
| **T9** | 前端组件与 E2E | 界面层，先保证后端语义正确再投入 | T5 | P3+ | 🟡 **部分**：前端组件用例 **15 个文件 / 95 个**（Vitest + Testing Library）；**Playwright E2E 已落地**（2026-11-06）：`apps/shell/playwright.config.ts` + `apps/shell/e2e/` 共 **2 个 spec 文件 / 5 条用例**，实测 **5 passed / 0 failed**（本地 12.2s；`CI=true` 模拟 10.8s）。起的是 Vite 开发服务器 + 真实 chromium，**不拉 Tauri 窗口**（CI 上没有 WebView），`window.__TAURI_INTERNALS__` 由 `page.addInitScript` 注入假后端。覆盖：① 文件夹邮件列表按 UID 倒序显示标题与发件人（含 `list_messages` 入参）；② 点开一封看到正文、打开不自动标已读；③ 写信面板没写收件人时前端拦下、不调 `send_message`（含「地址之间有空白」提醒）；④ 改动后 800ms 防抖自动存草稿、草稿箱里出现这一封；⑤ 收件人 + 主题填好后发送显示「已发送」与 `Message-ID`，发完发送按钮停用。已接进 CI 的 `web` job。**未覆盖**：附件与富文本编辑器（组件层在用），以及 `uncertain` 二次确认流 |
| **T10** | fuzz（MIME / HTML / IMAP 响应解析） | 防御性加固，非功能正确性 | T5/T6 | P2+ | 🟡 **部分**（2026-11-09 核实：四个文件、行数逐个数过，见下；仍缺覆盖率引导 fuzz，故维持 🟡）：**没有引入 `cargo-fuzz`**——它要 nightly + `libfuzzer-sys`，且不进 PR CI；改为**固定种子的随机化属性测试**（stable 即可跑、零新依赖、失败可逐字复现），**四个解析器全都有了**：`crates/xremail-core/tests/sanitizer_properties.rs` **6 条**（HTML 消毒器）、`crates/xremail-message/tests/parser_properties.rs` **7 条**（MIME）、`crates/xremail-protocol-imap/tests/utf7_properties.rs` **6 条**（邮箱名编解码）、**IMAP 响应解析**：`crates/xremail-protocol-imap/tests/imap_parsing_properties.rs` **12 条**（纯函数：`FETCH` 属性一一对应、`BODY[…]` 落到哪个字段、`LIST` 属性→角色、IDLE 推送归类、`COPYUID`/`APPENDUID` 端点与上限、假服务器解析器自洽）+ `crates/xremail-protocol-imap/tests/response_properties.rs` **13 条**（2026-11-09 复核：R47 修复时由 12 涨到 13；真 TLS 端到端：`EXAMINE` 缺 `[UIDVALIDITY]` 是 `None`、`[HIGHESTMODSEQ]`/`[UNSEEN]`/`[UIDNEXT]` 照收、`COPYUID` 按下标 / 长短不一不硬套 / 展开有上限 / **任一侧截断就整份作废**（R47）、没有 `COPYUID` 时源集合退回入参、`NO`/`BAD`/未知状态一律报错、`[MODIFIED]` 单独成类、**没有结局就挂住而不是报成功**）。为此 `xremail-testkit` 新增两个测试专用故障 `Fault::RespondWith` / `Fault::InjectUntagged`。**变异测试**：本轮 **18 个变异 18 个被捕获**（见 §5.1.1）。**它撞出过的真缺陷**：R45（整封转发的多个附件都记成 `TEXT`）；这处**已知残余**原先是钉住的（`truncated_copyuid_currently_looks_like_a_valid_mapping`），**2026-11-09 已修（R47）**，该用例改写成 `truncated_copyuid_is_refused_instead_of_used_as_a_mapping`。**仍缺**：覆盖率引导的探索式 fuzz（`cargo-fuzz` / `libFuzzer` / AFL 仍然没做）；**不在本层可控范围**：`imap-proto` 与 async-imap 自身的解析（第三方 crate）——已实测坐实 async-imap 的 `APPEND` 在完成响应形状认不出时**挂住**，这一点只能靠调用方超时兜住，并在 `response_properties.rs` 里钉住 |

**Rust 用例总数：4 → 1147**（2026-10-30 达 687；2026-11-03 随调度器 700、
移动/删除写路径 747、崩溃恢复 778、IDLE 客户端侧与调度器接线 790、
认证失败终态 792、审计修复（重发幂等 / 命令超时 / panic 边界补漏）797、
认证分类纠偏（R25）802、文档审计补正 803、A 组审计缺陷修复（R26–R42）835、对账时机与租约（R29/R32/R33）842；
2026-11-06 E 组全量复跑仍是 843，未新增用例；同日断线重连（B1/R43）870；
同日附件下载（B2）与 DKIM（D1）合并后 **988**；
2026-11-09 托盘与退出语义、DKIM 跨平台用例 + 凭据契约测试（R07）、
T8 乱序响应（11 条）、folder 角色落库（R04）合并后 **1050**；
2026-11-09 属性测试补上 UTF-7（6 条）后 **1056**，CI 实测同口径）；
2026-11-09 T10 补上 IMAP 响应解析（当时 24 条：`imap_parsing_properties.rs` 12 + `response_properties.rs` 12）后 **1086**；
2026-11-09 修 R46 / R47 后 **1088**（R47 把「截断的 `COPYUID` 被当成有效映射」那条钉住现状的用例改写成
2 条：`response_properties.rs` 由 12 涨到 13）；
同日新邮件通知（`notify.rs` 的 8 条纯函数用例）合并后 **本轮 1096**。
其中 **1 个是 `#[ignore]`**（`apps/shell/src-tauri/tests/real_mail_roundtrip.rs` 的
`real_round_trip_between_two_mailboxes`，真实邮箱往返、会真的发信；旧版引用的 `:68` 已失效，按用例名检索），
所以 `cargo test` 实际执行 **1144** 个
（2026-11-09 本轮实测：`cargo test --workspace -- --list 2>/dev/null | grep -c ': test$'` = **1096**，
逐包相加同为 1113：core 127 / store 113 / crypto 40 / api 38 / sync 216 / protocol-imap 161 /
protocol-smtp 37 / testkit 69 / message 105 / dkim 40 / diagnostics 3 / shell 147；
同口径全量跑累加 `test result:` = **1112 passed / 0 failed / 1 ignored**）；
上一条记录（同日较早，1088 那一批）实测为 `env -u DBUS_SESSION_BUS_ADDRESS cargo test --workspace -j 4 --no-fail-fast`，
累加所有 `test result:` 行 = **1085 passed / 0 failed / 1 ignored**（`--list` 1086）
（2026-11-06 实测：`cargo test --workspace -j 4 --no-fail-fast`，
累加所有 `test result:` 行 = **987 passed / 0 failed / 1 ignored**；
`--` `--list` 点数同口径 **988**）。
**注意口径**：803 是 2026-11-04 用 `--list` 点的数，835 / 842 / 843 / 870 / 988 是 2026-11-06 把
全部测试目标**全跑一遍**累加的（同一口径下修正 R34 回归前是 833 passed
+ 2 failed；842 那一轮是 842 passed / 0 failed / 1 ignored，E 组复跑也是）。843 与 842 两种数法在本轮**已经对齐**
（同日同仓库 `--list` 也是 843），但历史节点是按当年当时的口径记的，**不要跨口径互相减**，也不要拿它当「新增了多少条」。
**870 → 988 的 118 条**几乎全部来自附件下载（B2）与 DKIM（D1）两个并行分支合并，
所以这个数**只用于总量核对**，不要用来推断某一个特性补了多少条。
**前端用例 0 → 95**（15 个文件；`npx vitest run` 实测 15 个文件 / 95 条，R30 补了 4 条、B2 补了附件面板与
上限下发的 5 条、D1 补了 DKIM 面板的 5 条、R29/R32/R33 与 B1 各补若干）。
**注意**：`vitest.config.ts` 的 `testTimeout` 已从默认 5s 调到 15s（`TEST_TIMEOUT_MS` 可覆盖）——
并行跑时 `user-event` 的逐帧交互偶尔会超过 5s，表现是「单跑通过、全量跑偶发超时」。
**端到端 0 → 5**（2 个 spec 文件；`npx playwright test` 实测 5 passed，见 T9）。

### 5.1.1 一条新规矩：用**变异测试**证明用例有效

§5.2 要求「用例在错误实现上必须会失败」。光靠阅读代码判断不够——
2026-10-23 起，**每批改动都跑一遍变异测试**：把实现改坏，看用例会不会失败。

已做的几轮：

| 批次 | 变异数 | 捕获 |
|---|---|---|
| 同步纯函数（T2） | 7 | 7/7 |
| 拉取链路 | 7 | 7/7 |
| 标记回写 | 8 | 8/8（其中 1 条**第一版没被抓住**——用例查的是本地标记，改成查服务器上的真实状态后才抓住） |
| 头与正文 | 7 | 7/7 |
| IMAP 响应解析（T10，2026-11-09） | 18 | 18/18：`APPENDUID` 改成 `end-1`、展开少一个、上限放大 20 倍、去掉 `has_uid_mapping` 的长度检查、按值而不是按下标查映射、源集合为空时不回退、IDLE 不认 `BYE`、不认识的属性认成 `trash`、见到 untagged 就收工、LIST 只认名字、`BODY[TEXT]` 落进 `body`、`SequenceSet` 开区间、假服务器放宽 `STORE` / 取件项括号 / 把不认识的搜索键当 `ALL`、读到断线当成成功。**其中一条第一版没被抓住**：`BODY[TEXT]` 落进 `body` 时字段数仍是 1，而第一版的断言是「恰好一个字段非空」——它照样绿；第二版改成**逐段钉死是哪一个字段**（头→header、正文→text、整封/部件→body）后才抓住。这个教训与「第一版消毒器按子串判」是同一类：**断言要窄到能区分「对」与「碰巧对」** |
| 认证失败终态（`reap_finished`） | 1 | 1/1（把 `reap_finished` 改成空操作后 `a_reauth_required_account_can_be_started_again` 确实失败：`tx.send` 到已死的 actor 不报错，重启只是叫醒尸体——这类「静默停止同步」只能靠变异测试挡住） |
| 截断的 `COPYUID`（R47，2026-11-09） | 2 | 2/2（撤销 `copy_uid_of` 的截断处置 → `truncated_copyuid_is_refused_instead_of_used_as_a_mapping` 与 `truncation_only_cuts_the_tail_and_says_so` 同时变红） |
| 命令超时（R46，2026-11-09） | 1 | 1/1（把超时改成 3600 秒 → `a_server_that_never_answers_does_not_hang_the_sent_copy_check` **挂死**，`timeout 60` 退出码 124，正是修复前的行为） |

变异脚本在 `target/mutate*.js`（不入库）；2026-11-09 那一轮用的是 `.devcache/mut/mutate.sh`
（同样不入库：逐条 `cp` 备份 → 改一处 → 只跑相关测试目标 → 还原，跑完 `git diff src/` 核对无残留）。

### 5.2 每个目标的「算完成」标准

- [ ] 至少一个**失败路径**用例（错误输入 → 预期错误类型，不只是“不报错”）
- [ ] 至少一个**边界**用例（空集合、最大长度、0 值、null）
- [ ] 至少一个**不变量**用例（事务后状态符合预期；重复执行幂等）
- [ ] 用例在**错误实现**上会失败（若改坏实现测试仍绿，说明用例无效）
- [ ] 不依赖真实邮箱、真实凭据、真实网络、本机绝对路径

### 5.3 用例数作为进度指标

```
cargo test --workspace -- --list | grep -c ': test$'
```

该数字随每次提交变化，写入 CHANGELOG。**用例数本身不是质量证明**——它只用于回答「比上次多了多少」。
CI 已在 `Test` 步骤后输出该计数（见 `.github/workflows/ci.yml`）。

计数：**1147**（2026-11-10 在 Linux 本机实测；含 3 个 `#[ignore]`，实际执行 **1144**，
全量跑累加 `test result:` 同为 1112 passed / 0 failed / 1 ignored）。
上一条记录是 1088（2026-11-09 修 R46 / R47 那一批；`--list` 口径，R47 把现状用例改写成 2 条）。
再上一条是 1086（2026-11-09 T10 补 IMAP 解析那一批；实际执行 1085）。
上一条记录是 988（2026-11-06 在 Windows 本机实测；实际执行 987）。**注意**：这一条在历史顺序上应当排在 1086 / 1088 之前（日期更早），旧版把它接在 1086 之后是排版错误，数字本身不动。
同一天用「全量跑一遍累加 `test result:`」的数法也是 987 passed + 1 ignored。
上一条记录是 870（2026-11-06 断线重连那一批，实际执行 869）；再上一条是 803（2026-11-04，`--list` 口径，实际执行 802）。

> Linux CI 的计数自 2026-10-28 起就没有再更新（当时是 **544**）。
> 它与上面的 Windows 数字**不是同一时点**，不要相减得到「差多少条」。

> 一律以 `cargo test --workspace` 在本机的实测输出为准。
近期新增用例的分布（2026-10-30 那轮共 103 条）：发信服务层 8、存储层 `send_queue` 18、
协议层 SMTP 提交 12、MIME 构建（`xremail-message::compose`）、假服务器故障注入与端到端同步、IPC 线格式。
之后的增量见 §5.1 的节点列表（调度器 13、移动/删除写路径 24、崩溃恢复 11、IDLE、对账与租约等）。

### 5.4 明确不做的

- 不追求覆盖率百分比达标；不为此写无断言的用例凑数
- 不把 mock 通过当作真实服务商兼容的证据（见 §1 分层）
- 不在普通 CI 中使用真实凭据
- 不写依赖 `sleep` 的时序测试（用虚拟时间，见 §3）

---

## 6. 命令与 CI 现状

现有命令：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
npm run typecheck --prefix apps/shell
npm test --prefix apps/shell        # vitest run（15 个文件 / 95 条）
npm run test:e2e --prefix apps/shell # playwright test（2 个 spec / 5 条；CI 里先跑 playwright install chromium）
npm run build --prefix apps/shell
```

### 6.1 本地改一行就跑全量是浪费：用 `scripts/test_changed.py`

全量是 **1113 个用例**，改一段注释也跑一遍，等于把最贵的一步做成最高频的一步。
本地日常改成：

```bash
python scripts/test_changed.py            # 只跑受本次改动影响的包
python scripts/test_changed.py --dry-run  # 只看计划
python scripts/test_changed.py --all      # 发版 / 推 PR 前跑全量
```

判据是**反向依赖**，不是目录名：改了 `xremail-store`，所有 `use xremail_store`
的包都跟着重测（它们吃它的 API）；只改叶子包就没人陪跑。细节：

- 改动落在根 `Cargo.toml` / `Cargo.lock` → **自动退化成全量**（依赖换版本，影响面是整个 workspace）
- 只改 `docs/**` / `*.md` → **一行测试也不跑**，只跑文档链接检查
- 改 `apps/shell/src/**` 或 Tauri 侧 → `tsc` + `vitest` + `build`；
  Tauri 侧一定连前端一起跑，因为 IPC 的线格式两边共享，字段名漂移只会静默拿到 `undefined`
- 未跟踪的新文件也算改动（否则新加的用例文件会被漏掉）

**它不能替代什么**：增量只看「改了哪些文件」，看不出「这次改动破坏了
别的 crate 的语义假设」。所以**发版与推 PR 必须跑 `--all`**。
另外 `cargo test -p` 仍然要编译上游依赖，「省」的是**用例执行**与无关包的链接，
不是编译。

2026-10-30 实测（**当时的数字**，不是现状；现在为 **1113** 个 Rust 用例（实际执行 1112）、95 个前端用例、5 条 E2E，
见 §5.1 与 §5.3）：
以上六条在本机全部通过；`cargo test --workspace` 实际执行 **544** 个 Rust 用例
（0 失败），CI 上 Linux 是 **538**（少 6 条 Windows 专属用例），`npm test` 跑 **27** 个前端用例（5 个文件，0 失败）。
用例数为进度指标，不等于业务已验证。

**本机构建的坑**（2026-10-28 踩过）：16 核默认并行 16 个 rustc，
带调试信息时内存峰值超过 31 GB，**rustc 会被 OOM 杀掉**，
留下的半截产物让后续构建报 `found invalid metadata files for crate ...`——
看着像 cargo 的锅，实际是内存不够。两种避开方式：
`-j 4` 限并发，或 `CARGO_PROFILE_DEV_DEBUG=0` 关掉调试信息（省内存也省磁盘）。
CI 的 runner 内存更小，必要时同样要限并发。

现有 CI 的具体执行项以 `.github/workflows/ci.yml` 为准；有 workflow 不等于远端已经跑绿。
Vitest 与假协议服务器已经在跑；**Playwright E2E 于 2026-11-06 落地**并接进 `web` job
（装 chromium → `npm run test:e2e` → 失败时上传 trace 与截图）。**尚未建设**：fuzz（T10），
不在本文虚构已通过结果。
新增测试工具时提交安装方式、命令与最小验证用例。真实凭据测试不得进入普通 PR CI。

### 6.1 真实邮箱验收：现在能做什么、还差什么

`apps/shell/src-tauri/tests/real_mail_roundtrip.rs` 会**真的发一封邮件**出去，
用两道开关把它挡在普通 CI 之外（`#[ignore]` + `XREMAIL_REAL_ACCOUNTS=1`）。
这是对的——但它带来一个副作用：**没有任何东西能在"真的跑之前"发现环境已经坏了**。

**已经做了的**（2026-11-10）：`scripts/precheck_real_mail.py`——**不需要真凭据也能跑**的预检：

```bash
python3 scripts/precheck_real_mail.py
```

它检查的全是**可以离线判断**的东西：凭据文件在不在、权限是否过松（口令是明文，
不该 0644）、有没有 `163` 与 `qq` 两段、每段 `user`/`password` 是否非空、
QQ 的数字用户名会不会被正确补成 `@qq.com`、两个账户是不是配成了同一家。
它**不打印口令、也不打印完整地址**（只留首字母 + 域名）。

输出同时暴露一个已知限制：**用例目前写死从 `163` 与 `qq` 两段取账户**
（`require_mailbox(&accounts, "163")` / `"qq"`）——想用别的组合要先改那个文件，
预检会把这件事明确报出来，而不是等人跑起来才发现。

**还没做的（这就是 ROADMAP 上那条未完成项）**：把它变成 **CI 里的一次真实验收**。
需要的前置条件与方案：

| 需要什么 | 说明 |
|---|---|
| 两个专用测试账户 | 不碰任何真实个人邮箱；授权码只放 GitHub 的 **Encrypted secrets**，永不进仓库 |
| 触发方式 | **`workflow_dispatch` 手动触发**（不是 `push` / `pull_request`）——「不在普通 PR CI 中使用真实凭据」这条政策不变 |
| 注入方式 | 在 job 里把 secrets 写成 `$HOME/.xremail/accounts.txt`（权限 `600`），跑完在 `always()` 里删掉 |
| 频率 | 手动，或一条独立的 `schedule`（例如每周一次）。**不要**放进主 CI 矩阵 |
| 失败怎么处理 | 发信是**有副作用**的动作：一次网络抖动不该把整个仓库标红。建议**只在自己的 workflow 里红**，不进主分支保护 |
| 验收门槛 | 断言「对方真的收到」与「发件方「已发送」里正好一份」，与现有用例一致 |

**为什么没有直接做**：这一项需要**真凭据**（两个测试邮箱的授权码）。
把凭据接进 CI 是一次关于密钥托管方式的决定——用谁的账号、放哪个仓库的 secrets、
谁能触发——不属于可以单方面决定的事。预检脚本与上面的方案已经就位，
拿到账号之后接上去即可。


## 7. 性能与 Android 验收

- 性能报告注明设备、OS、网络、账户数量、邮件数、正文/附件大小分布、冷/热缓存；分别计量摘要同步与正文下载。
- 同时记录首屏时间、峰值内存、磁盘增长、重连次数、取消延迟，不只报平均耗时。
- Android 分开测试：前台、退后台、息屏、Doze、网络切换、进程被杀、重启；记录 Android/ROM 版本与权限。
- 前台服务不保证无限后台网络，也不保证 OEM 不杀进程。无法达到后台目标时明确展示同步时效，不用“30 秒必同步”兜底承诺。

相关：[风险](ISSUES_AND_RISKS.md) · [架构](ARCHITECTURE.md) · [AI 开发规范](AI_DEVELOPMENT_GUIDE.md)
