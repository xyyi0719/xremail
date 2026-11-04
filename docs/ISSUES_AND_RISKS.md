# 问题核查与风险登记册

> v3 · 本轮审查修订。本文替代上一版未经充分核实的示例与风险评级。
> 范围：个人研究项目、AI 编写代码、国内邮箱优先；Gmail 暂缓。完成时间不作承诺。
> 维护者：xyyi0719

## 1. 事实、推断、待验证

- **仓库事实**（2026-10-30）：workspace 有 10 个 crate——`xremail-core` / `xremail-store` /
  `xremail-crypto` / `xremail-api` / `xremail-sync` / `xremail-protocol-imap` /
  `xremail-protocol-smtp` / `xremail-message` / `xremail-testkit` / `xremail-diagnostics` + Tauri shell。
  SQLite 迁移与 DAO、同步引擎的**拉取与回写**链路、IMAP 与 SMTP 协议层、假 IMAP / SMTP 服务器、
  FTS5 全文索引、HTML 消毒均已落地；**收发均已实现**（163 ↔ QQ 真实收发往返已验收）。
- **实测事实**：163 / QQ / 沃三家真实邮箱均已只读实测（TLS、能力集、IDLE/MOVE/CONDSTORE、
  UID 空间、字符集、文件夹角色），记录见 [PROTOCOLS.md](PROTOCOLS.md)。
  30 封真实邮件用 `xremail-message` 解析，**解码零错误**。
- **规划**：平台适配（P4）、标签/规则（P5）、L2 远端同步（P6）**都不是已交付功能**。
- **外部未知**：并发连接限制、Android 后台行为——
  必须用注明环境的实测建立证据。
- `cargo test` 现在有 **778 个用例**（2026-11-03），六关本地全绿；
  前端另有 **67 个用例**（12 个文件）。
  CI **四个 job 全绿**（Rust ubuntu / Rust windows / Frontend / Docs sanity），R17 已处理。
- 2026-10-20 复核：Rust 1.99.1 / Node 22.21.0 / 本机仅 JDK 8 且无 Android SDK → **Android 构建在本机暂不可行**（P4 需先装 JDK 17 + SDK/NDK）。

## 2. 对上一版审查的纠错

| 旧结论/示例 | 修正 |
|---|---|
| 已有 SQLite schema 缺少 UIDVALIDITY，需要加 `last_uidvalidity` | PLAN 已规划 `sync_state.uidvalidity`，尚无实际迁移。缺的是原子重建与待发送操作失效规则，不是另一个同义字段 |
| 不支持 CONDSTORE 就用本地时间戳/HLC 仲裁 IMAP flag | IMAP 不提供每个 flag 的跨设备修改时间；本地 HLC 无法排序其他客户端操作。采用服务器快照 + 本地操作意图重放，明确存在并发覆盖窗口 |
| MODSEQ 大者胜 | MODSEQ 是 mailbox 内的同步版本，不是通用冲突时间。使用 UNCHANGEDSINCE 条件写入及 MODIFIED 后重新读取 |
| FTS 改为 `content='message'` 并加插入/删除触发器即可 | 示例的列不匹配；正文在独立表、可能晚到；外部内容 FTS 删除也不能照搬普通表 DELETE。必须选定投影模型并测试更新、删除、重建 |
| 无 IDLE 就整个架构失效 | 只影响延迟与功耗，轮询仍能正确同步；无 CONDSTORE 只影响增量效率/并发检测 |
| 自动降级到加密文件 | 不应静默降低存储保障。钥匙串失败时先提供重试/仅会话保存，文件 vault 需要独立设计与显式选择 |
| AI 固定带来 1.8–2 倍耗时、6 个月必能交付 | 无项目样本支持这些系数。使用验收门槛而不是预测承诺 |
| Linux 支持由 Tauri 免费获得 | 构建、系统依赖、钥匙串、通知、分发均需单独验证 |
| 文档示例可直接用于开发 | 旧 IMAP、SQL、XSS、keyring 示例未编译/验证，已撤下，不能作为实现基线 |
| `AppError` 的 `ctx()` 只是附加文本 | 旧实现把错误统一折叠成 `Other`，**丢失了** `is_retryable()` / `requires_user_action()` 判定，与 §3 的重试契约矛盾。已改为按变体保留类别并补单测（见 §2 附记） |
| `panic = "abort"` 无副作用 | 体积更小，但后台同步 task 的 panic 会终止整个进程；tokio 任务级 `catch_unwind` 在 abort 下无效（见 R13） |
| CI 通过 = 项目健康 | CI 无任何业务测试；`cargo test` 只跑 `xremail-core` 的 4 个错误分类用例 |

## 3. MECE 风险分类

按“风险的主要归属”互斥分类，跨类依赖用编号引用，不重复登记。
严重度与处理阶段分开：严重度不是里程碑编号。

| ID | 归属 | 风险/缺口 | 严重度 | 处理门槛与证据 | 状态 |
|---|---|---|---|---|---|
| R01 | 外部接入 | 沃邮箱的服务器地址与授权码流程未实测确认 | 高 | 只读 probe、TLS 校验、认证后 CAPABILITY、脱敏记录；不能把未测项改成不支持 | **已实测**（2026-10-23：`imap.wo.cn:993` TLSv1.3、授权码登录成功、**Coremail 体系**、无 IDLE/MOVE/CONDSTORE；授权码**申请入口**仍待确认） |
| R02 | 外部接入 | 163/QQ/沃邮箱的能力与认证行为未实测 | 高 | 同上；分别记录三家，不互相推断 | **已实测**（2026-10-23，三家分别记录于 [PROTOCOLS.md](PROTOCOLS.md)；关键差异：QQ 独有 IDLE+MOVE、163 必须先发 `ID`、沃也是 Coremail 但不要求 `ID`） |
| R03 | 数据正确性 | UIDVALIDITY 变更会使旧 UID 操作指向错误邮件 | 高 | 同一事务更换 epoch、失效旧缓存/游标；旧 epoch 操作隔离，测试崩溃恢复 | **已实现**（`plan_epoch_transition` + `commit_folder_sync` 单事务切换；旧 epoch 操作隔离为 `failed`。用例：`epoch_change_triggers_a_rebuild`、`operations_bound_to_the_old_epoch_are_quarantined`。**崩溃恢复已补**（2026-11-03）：`tests/crash_recovery.rs` 中 `an_operation_on_a_stale_epoch_is_quarantined` 验证对账也能隔离旧 epoch 操作、`an_exhausted_row_stuck_in_running_is_reclaimed_then_failed` 验证死在提交途中的行能被收干净） |
| R04 | 数据正确性 | flag 并发、移动、删除、发送的重试语义未定义 | 高 | 按 ARCHITECTURE 的操作状态与不确定结果策略测试 | **已实现**（2026-11-03）：flag 并发已定（「读快照 → 重放意图 → **回读确认**」，三种失败分类 `retryable`/`uncertain`/`failed`，见 ARCHITECTURE §2.2）；**移动 / 删除已实现并定义清楚**（2026-11-03）：移动/删除的意图是「这封信不该在这个文件夹里了」——没有净变化可算，所以与 flag 回写**分两条通道**（`push_flags` 只取 `kind = flags`）。路由由服务器能力决定（有 `MOVE` 一步；否则 `COPY` + 标删，**有 `UIDPLUS` 才** `UID EXPUNGE`；**裸 `EXPUNGE` 一律不用**）；**源文件夹已无此信算成功**（`MOVE` 不幂等，重发会多一份）；**删除 = 进垃圾箱**（可撤销）。失败分类复用 `SubmitError`。用例：`tests/push_transfers.rs` 8 个 + `move_delete_intent.rs` 8 个 + `route_for` 单测 8 个。**残余**：垃圾箱靠**文件夹名**推断（库里还不存 `\Trash` 属性）、总配额与取消未做；**发送已定义并实现**（2026-10-28：`send_queue` 六态、`uncertain` 不自动重发、冻结字节后不可改内容，见 ARCHITECTURE §5） |
| R05 | 数据正确性 | FTS 正文晚到、索引不一致、中文短查询 | 中 | 投影事务、重建校验、1/2 字查询策略测试 | **已实现**（2026-10-30）：投影 `message_search_doc` + FTS5 外部内容表（trigram），索引由**触发器**维护（epoch 切换、级联删除这些不经过应用代码的路径也绕不过），`rebuild_index` + `check_index_consistency` 兼底，1~2 字符的查询退化成 `LIKE` 子串扫描；**残余**：子串扫描是全表扫描（个人邮箱量级可接受），trigram 仍无中文分词 |
| R06 | 安全边界 | HTML/CSS/URL 可绕过图片阻断或调用宿主能力 | 高 | 清洗 + 默认禁网 CSP + WebView 网络/IPC 测试；安全门槛先于真实 HTML 展示 | **已实现**（2026-10-30）：清洗在后端完成（`xremail-core::html::sanitize_html`，标签/属性白名单、删 `script`/`style`/`iframe`/`on*`/`javascript:`、删远程图片），**原始 HTML 不出后端**（`MessageBodyView` 只带 `htmlSafe`），前端只渲染 `htmlSafe`、链接交给系统浏览器，CSP 的 `img-src` 不含 http/https；**残余**：未知标签只删标签、留着内容（真实邮件正文被 `html`/`body` 包着，连内容删会整封信消失），**真实 WebView 里的渲染与网络行为尚未在宿主上验证** |
| R07 | 安全边界 | 钥匙串后端与失败语义、日志脱敏未落地 | 高 | 平台 SecretStore 契约测试；失败不明文落盘 | **部分实现**：契约与 mock 已有（`xremail-api::SecretStore`）；诊断工具**不打印凭据/地址**（脱敏已验证）；**Windows 后端已落地**（2026-10-26，`secrets/windows_store.rs`，真实往返用例）；**失败语义仍待定**（其它平台是桩，保存时如实报错、不落盘） |
| R08 | 运行平台 | Android 生命周期、后台预算与通知不确定 | 中 | 早期最小真机 PoC；前台同步可用不代表可后台常驻 | 待验证（P4） |
| R09 | 运行平台 | 首次同步与大附件耗尽内存/磁盘 | 中 | 有界批量/队列、取消、配额、超限测试，指标注明硬件与网络 | **部分实现**：有界批量已有（每批 200 个 UID；曾因 `1:*` 展开成 1.79 亿个元素把假服务器撑爆 8 GiB，已修）；**单封附件上限已实现**（2026-10-30：`xremail-core::attachment::MAX_ATTACHMENT_BYTES` 10 MB / 每封最多 5 个，由 `service/compose.rs` 在发信路径上强制）；**总配额与取消仍未做**（P2/P3） |
| R10 | 工程验证 | AI 编造 API、测试只复述实现、无独立预期 | 高 | TESTING_STRATEGY 与 AI_DEVELOPMENT_GUIDE 的证据门槛 | **已执行**（2026-10-23）：每批改动都跑**变异测试**——把实现改坏看用例会不会失败，累计 30/30 被捕获；另有假服务器与真实邮箱两条独立证据链。门槛见 [AI_DEVELOPMENT_GUIDE §4](AI_DEVELOPMENT_GUIDE.md) |
| R11 | 范围治理 | 文档把计划当事实，旧策略与新范围冲突 | 中 | ROADMAP 为执行范围入口；旧报告标废止；变更附验收证据 | 本轮修订 |
| R12 | 可选扩展 | L2 接口过早冻结、远端冲突与密钥恢复未定义 | 中 | P0 最小契约 + mock；P6 前另做威胁模型，不承诺零返工 | **部分实现**（契约 + CAS mock + 双设备竞争用例已就位；远端密钥恢复仍未定义，P6 前另做威胁模型） |
| R13 | 运行平台 | release profile 用 `panic = "abort"`，后台同步任务 panic 会杀掉整个应用 | 中 | **已决策（2026-10-22）**：改为 `unwind`；配套要求后台 task 边界 `catch_unwind`，待 `xremail-sync` 落地时实现 | **已实现**（2026-11-03）：profile 已改 `unwind`（仓库根 `Cargo.toml:96`）；任务边界在 `crates/xremail-sync/src/actor.rs`——每轮同步包在 `AssertUnwindSafe(future).catch_unwind()` 里，panic 只停在那一轮，actor 按退避重试，连续 3 次才放弃账户；事件回调再包一层。**证据**：`tests/scheduler.rs` 的 `a_panicking_round_is_contained_and_the_actor_retries`（前两次连接 panic，第三次同步成功）与 `repeated_panics_abandon_the_account_instead_of_hot_looping`（恰好试 3 次就停）；`one_panicking_account_does_not_stop_the_other` 证明账户间隔离。**残余**：Tauri 命令处理线程与前端 Wry IPC 回调不在这个边界内 |
| R14 | 工程验证 | CI 无业务测试，静态检查通过不代表行为正确 | 高 | 按 [TESTING_STRATEGY §5](TESTING_STRATEGY.md) 的 T1–T10 顺序扩写用例；每阶段出口要求对应测试；用例数作为进度指标写入 CHANGELOG | **进行中**：T1–T8 已扩写（Rust **778** 个用例，2026-11-03；其中调度器 13 个、COPY/MOVE 端到端 9 个、移动/删除写路径 24 个、崩溃恢复 11 个）、T6 / T7 转完成、T9 部分（前端 **67** 个用例 / 12 个文件，**Playwright E2E 未开始**）、T10 fuzz 未开始；CI 四个 job 全绿（R17 已处理） |
| R17 | 工程交付 | 13 个提交**从未推送到 GitHub**：全部成果只在本机磁盘上 | 高 | 推送前做 `git ls-remote` / `gh auth status` 连通性探测（本机 github.com 的 DNS 常被劫持到 127.0.0.1）；推送后核对远端提交号 | **已处理**（2026-10-26）：推送前探测通过（`git ls-remote` + `gh auth status`），推送后核对远端提交号 = 本地 HEAD；CI 在 `975ffda` 上**四个 job 全绿** |
| R18 | 外部接入 | 三家的 **SMTP** 服务器参数与提交行为未实测 | 高 | 同 R01/R02；重点测授权码认证、`STARTTLS` 语义、`8BITMIME`、发信后的 `APPEND` 行为 | **已实测**（2026-10-29，**真发了信**）：163 ✅（TLS 1.3、`AUTH LOGIN` 成功，`250 Mail OK queued as …`）、QQ ✅（TLS 1.2、`AUTH LOGIN` 成功，`250 OK: queued as.`——**queue id 是空的**）、沃 ❌（**服务器只收静态 RSA 套件，rustls 连不上**）、126 ⏳（本机无 126 账号）。**双向真发信已跑通**（163 → QQ、QQ → 163）：对方都收到了，正文标记核对一致。**「发信后存副本」已探明并实现**（2026-10-30）：**两家服务器都自己存副本**——163 提交后几乎立刻可见，QQ 要 **30–90 秒**才落盘（第一次实测查得太早，误记成「QQ 不存」）；QQ 还会**改写 `Message-ID`**（原值挪到 `X-OQ-MSGID`）。协议层 `APPEND` **已实现**（`crates/xremail-protocol-imap/src/append.rs`），服务层按「先查、没有才 `APPEND`」自适应执行，QQ 拒绝重复 `APPEND` 时按「已有一份」处理。验收用例 `apps/shell/src-tauri/tests/real_mail_roundtrip.rs` 断言**两个方向的「已发送」都正好一份**（轮询等满 120 秒） |
| R21 | 外部接入 | 沃邮箱的 **SMTP 侧在 rustls 上不可用**：服务器只接受静态 RSA 密钥交换套件（`TLS_RSA_WITH_*`），rustls 只支持 ECDHE/PSK | 中 | 换 TLS 栈：可选的系统原生 TLS 后端（SChannel / OpenSSL）或 `native-tls` 单独一个 SMTP 后端；**不得**用「跳过证书校验」来绕过 | **待决策**（2026-10-28 实测确认，交叉验证：openssl 协商出 `TLS_RSA_WITH_AES_128_GCM_SHA256`）。P3 首发只做 163 / QQ；沃的发信单列。**「换 TLS 版本」这条路已排除**：发信路径里的 TLS 1.3→1.2 回退对沃无效——问题在**密钥交换套件**，不在版本 |
| R19 | 产品可用性 | 凭据**只驻内存**，关掉应用就得重填授权码 | 中 | 平台钥匙串（P4）或 `local_secret` 里的版本化密文（`xremail-crypto` 已就位）+ 平台 `SecretStore` 适配器 | **Windows 已处理**（2026-10-26）：授权码进 Windows 凭据管理器，库里只留不透明 `secret_ref`，关掉应用不必重填；**其它平台仍是桩**（不落盘），在那里用仍需重填 |
| R20 | 协议层 | 两个**只有真实邮箱才暴露**的 IMAP 陷阱：① `EXAMINE` 只认**线路原名**（modified UTF-7），拿解码后的名字去访问会得到 `NO EXAMINE Folder not exist`；② **UID 是邮箱作用域**的，会话停在另一个文件夹时 `UID FETCH <uid>` 返回**空结果**而不是报错 | 高 | 库里存线路原名、显示时再解码（`FolderDto`）；拉正文前先 `EXAMINE` 对应文件夹；两条都有回归用例 | 已缓解（2026-10-26，均由真实 163/QQ/沃 同步暴露） |
| R16 | 协议层 | imap-proto 0.17 **不认识** `MODIFIED` 这个 response code（它的 `ResponseCode` 枚举里没有），且对未知 code 用 `opt()` 解析——整个 `[MODIFIED] …` 会落进 information 文本。所以条件写失败只能**按文本判别**，拿不到结构化的 code | 中 | 协议层新增 `ProtocolError::ConditionFailed`，按文本判别并写进注释；将来升级 imap-proto 时复核 | 已缓解 |
| R15 | 协议层 | async-imap 0.12 **吞掉**服务器回的 `NO` / `BAD`：`uid_fetch` / `uid_store` / `uid_search` 在服务器拒绝时返回 `Ok`（只是结果为空），同步引擎会把「服务器拒绝了」当成「写成功了」，本地状态与远端悄悄分叉 | **高** | 新增安全命令层 `xremail_protocol_imap::run_checked`（自己读响应、自己判定 OK/NO/BAD，错误带 response code）；**同步引擎一律用它，不用 async-imap 的同名方法**；已用假服务器把该行为钉成用例 | 已缓解（同步引擎已全面改用 `run_checked`；用例：`server_rejection_is_an_error_not_a_mass_delete`） |
| R23 | 协议层 | **163 的 IMAP 只支持 `UID SEARCH ALL`**：`SUBJECT` / `HEADER` / `TEXT` 等条件**静默返回空结果**，连 `NO` 都不回——看起来像「没搜到」，实际是「不支持这种搜法」（实测 2026-10-29，QQ 上同样的命令正常） | 中 | 依赖服务端搜索的功能**不能假定三家一致**：要么在客户端做（拉下来本地搜），要么先探测服务器支持哪些搜索键、不支持就**明确报错**而不是假装没结果 | **已缓解**（2026-10-30）：同步引擎**不受影响**（走 `UID FETCH <UID 区间>`，从不用 `SEARCH`）；**搜索改为落在本地**——`message_search_doc` + FTS5（trigram）全文索引，1~2 字符退化成 `LIKE` 子串扫描，因此**不依赖服务端搜索**（163 上也能搜）。**残余**：将来若用服务端搜索做补集或按服务器能力优化，必须先探测它支持哪些搜索键、不支持就**明确报错**，不能静默返回空 |
| R24 | 协议层 | **「存副本」可能补成两份**（残余风险，2026-10-30）：沉降窗口只有 2.5 秒，而 QQ 的副本要 30–90 秒才落盘；当前靠**服务器拒绝重复 `APPEND`**（QQ 的措辞 `[CANNOT] Mail already saved to Sent by SMTP…`）兜底。若某个服务器**既慢存副本、又接受 `APPEND`**，就会存成两份 | 低（163 / QQ 不触发） | 把沉降窗口做成**按账户自适应**（记住上次服务器落盘耗时），或改成「`APPEND` 后回读计数，多于一份就删掉多出来的」 | **已知残余**（实测两家都不会触发：163 立刻可查，QQ 拒重复 `APPEND`） |
| R22 | 产品可用性 | **写信面板的已知缺口**（2026-10-28 登记，**2026-11-03 核实已全部补齐**）：① 草稿箱列表与回填；② 富文本编辑器；③ `uncertain` 的二次确认重发 / 撤回；④ 附件大小与数量上限；⑤ 收件人地址校验 | 中 | 补草稿箱列表与回填；附件上限（后端也要有对应上限）；地址校验；`uncertain` 的二次确认流 | **已实现**（2026-10-30 随撰写面板交付，本条登记未同步更新，2026-11-03 核实纠正）：① `cmd.rs:576 list_drafts` + `service/compose.rs:495` + `ComposePanel.tsx:427` 打开面板即回填上次草稿；② 自写 `contenteditable` 富文本，`ComposePanel.tsx:255-256` 双形态提交（`bodyHtml` 不再恒为 `null`）；③ `ComposePanel.tsx:381 pendingUncertain` + `ComposePanel.tsx:360` 注释：只能由用户明确选「再发一次」或「撤回来改」，两者都要二次确认；④ `xremail-core::attachment` 单封 **10 MB / 最多 5 个**，`service/compose.rs:35` 调 `check_attachment_limits_all`，前端只传路径、字节由后端读；⑤ `service/compose.rs:34,1084` 调 `xremail-core::address::validate_addresses`。**残余**：附件类型黑名单仅前端提示，后端无 MIME 白名单 |

## 4. 处理顺序

1. 确认 R01 的真实邮箱产品；实测 R02。无账号时可以写 mock 和只读工具，但不能宣称接入成功。
2. 固化 R03/R04 的身份、操作队列与恢复语义，再写存储和同步。
3. R06/R07 与第一条真实读信链路同时验收；不得等完整 UI 做好再补安全。
4. R08 做有限范围可行性验证，结果决定 Android 交付策略；不自动把 Android 移出目标。
5. 数据正确性通过后再测 R05/R09 的检索与性能；L2 服务、Gmail 与其他平台均不阻塞。

## 5. 关闭风险的规则

每条风险关闭须记录：关联提交、测试名称、运行环境、结果与残余限制。
“有方案”“编译通过”“文档已写”不能关闭运行时风险。真实邮箱日志须脱敏；账号与授权码不得进入文档、提交或对话。

## 6. 复核记录

| 日期 | 范围 | 结论 |
|---|---|---|
| 2026-10-20 | 全部 17 个 Markdown、Rust 与前端代码、CI 配置 | 修正 `ctx()` 丢失错误分类、代码注释中的断链与过期章节号、workspace resolver、作者署名；新增 `xremail-core` 4 个单测 |
| 2026-10-23 | 全部 13 个本地提交、10 个 crate、387 个用例 | R01/R02/R03 转为已实测/已实现；新增 R17（提交未推送）与 R18（SMTP 未实测）；修正 §1 里「workspace 只有 core 与 shell」等已过时的描述；为每条状态变更附上用例或提交名作为证据 |
| 2026-10-26 | 同步引擎接进应用后的代码与文档、397 个用例 | 新增 R19（凭据只驻内存，每次同步要重填）与 R20（真实邮箱才暴露的两个协议陷阱：EXAMINE 必须用 UTF-7 原名、UID 是邮箱作用域的）；R06 确认仍为安全状态（HTML 只当源码显示） |
| 2026-10-30 | P3 发信链路、本地全文搜索、HTML 正文消毒与派生 | R05 转为已实现（FTS 投影 + 触发器维护 + 重建/一致性检查 + 短词 `LIKE` 回退）；R06 转为已实现（后端消毒、原始 HTML 不出后端、链接交给系统浏览器、CSP 禁远程图片）；**残余**：真实 WebView 里的渲染与网络行为尚未在宿主上验证 |
| 2026-11-02 | 全量文档与代码对齐审计（未改代码） | R14 的用例数更新为 687 + 67；R23 转为已缓解（搜索落在本地，不依赖服务端 `SEARCH`）；**残余**：Playwright E2E 与 fuzz 仍未开始 |
| 2026-11-03 | 文档审计（推送前） | **发现调度器只是「crate 层已实现」**：`Scheduler` 在 `crates/xremail-sync/src/actor.rs:460` 存在且有 13 个用例，但 `apps/shell/src-tauri/src/` 里**没有任何地方 use 它**——产品路径仍是点刷新 → `cmd::sync_account` 跑一轮。文档原写「启动即同步一轮 → 按 poll_interval_secs 轮询」，读起来像已经在后台跑，已按实际改掉（ROADMAP / ARCHITECTURE §4 第 7 步）。**接线单独立项** |
| 2026-11-03 | 移动/删除写路径完工 | **R04 从「移动/删除仍未定义」改为已实现**：意图按「这封信不该在这个文件夹里了」建模，与 flag 回写分两条通道；路由由服务器能力决定，裸 `EXPUNGE` 明令禁用；删稿进垃圾箱可撤销。**顺带修了一个静默数据丢失**：`push_flags` 曾会把移动/删除意图当成「净变化为零」直接标成 `succeeded`（邮件没搬、队列却空了），现已隔离并有回归用例 |
| 2026-11-03 | 待办清单核对（文档优先） | **纠正三处「文档说没做、代码其实已做」**：R22 五条缺口全部已实现（改为已实现并附 `file:line`）；R09 的「大附件未做」收敛为「总配额与取消未做」（单封上限已落地）；ROADMAP P4 的「通知 / 单实例」实际已在 `lib.rs:37-53` 接线（只剩托盘与退出恢复） |
| 2026-11-03 | 调度器与 panic 边界落地 | 新增 `crates/xremail-sync/src/actor.rs`（AccountActor + Scheduler）与 `tests/scheduler.rs`；R13 转**已实现**；用例数 687 → **700**（`cargo test --workspace -j 1`：700 passed / 0 failed / 1 ignored） |
| 2026-11-03 | `COPY` / `MOVE` 协议层落地 | 新增 `crates/xremail-protocol-imap/src/transfer.rs`（`copy_messages` / `move_messages` / `mark_deleted` / `uid_expunge`）与假服务器支持（`COPY` / `MOVE` / `UID COPY` / `UID MOVE` / `EXPUNGE` / `UID EXPUNGE`，含 `COPYUID` 回应）；用例数 700 → **723**（`tests/transfer.rs` 9 个）。**顺手修了一个真 bug**：`append.rs` 把 `APPENDUID 123 5:7` 报成 6（`UidRange` 是闭区间，多减了 1）。**未做**：移动/删除的**写路径**（意图 → 队列 → 提交）仍未接 |
| 2026-11-03 | 移动 / 删除**写路径**落地 | 新增 `crates/xremail-sync/src/transfer_push.rs`（意图 → 队列 → 提交 → 回读确认），`OperationKind` 从只有 `flags` 扩到 `flags`/`move`/`delete`，迁移 `0005_operation_destination.sql` 加目标文件夹列；接通到命令层（`apps/shell/src-tauri/src/cmd.rs:548 move_messages`、`:569 delete_messages`，经 `service/message.rs`）与前端按钮；用例数 723 → **747**（`tests/push_transfers.rs` 8 个 + `transfer_push.rs` 单测 8 个 等） |
| 2026-11-03 | 崩溃恢复 / 启动对账落地 | 新增 `crates/xremail-sync/src/reconcile.rs`（`reconcile_account_as` + 纯函数 `plan_reconcile`），迁移 `0006_operation_owner.sql` 给 `mail_operation` / `send_queue` 加 `owner_boot`，认领时写归属；ROADMAP P2 「断线恢复」勾选，R03 的「崩溃恢复测试仍缺」转已完成；ARCHITECTURE 新增 §4.4；用例数 747 → **778**（`tests/crash_recovery.rs` 11 个） |
| 2026-11-03 | `IDLE` 客户端侧与调度器接线 | 新增 `crates/xremail-protocol-imap/src/idle.rs`（`idle_once` / `IdleConfig` / `IdleEvent`），由 `crates/xremail-sync/src/actor.rs` 的 `wait_for_change` 调用：等 `+ idling` → 读推送 → 发 `DONE` → 匹配**进 IDLE 那条命令的原 tag** 的 `OK`；**安全出口**无论正常/超时/断线/panic 都先 `is_authenticated()` 校验，`DONE` 未走完的会话弃用重连、不还连接池；不支持或失败 → 自动降级轮询。**应用层接线**：`apps/shell/src-tauri/src/lib.rs` 启动时只给「凭据可用」的账户起 actor、`RunEvent::Exit` 停全部；`cmd::delete_account` 先 `stop`；`cmd::sync_account` 补 `start`（新增 `Scheduler::is_scheduled`，**已启动就不叫醒**，避开与本次手动同步并发两轮）。前端订阅 `sync://state-changed` 且只在 `state === "online"` 时重查。用例数 778 → **790**（`crates/xremail-protocol-imap/tests/idle_client.rs` + `crates/xremail-sync/tests/scheduler.rs` 的 `push_wakes_the_actor_before_the_poll_interval` / `server_without_idle_still_polls_and_does_not_pay_an_extra_connection` 等；`cargo test --workspace -j 1`：790 passed / 0 failed / 1 ignored） |

相关：[架构](ARCHITECTURE.md) · [路线图](ROADMAP.md) · [测试策略](TESTING_STRATEGY.md)
