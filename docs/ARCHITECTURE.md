# 架构设计

> 当前设计基线；不代表所有模块已实现。执行范围见 [ROADMAP](ROADMAP.md)，验收见 [TESTING_STRATEGY](TESTING_STRATEGY.md)。
> 保留：Rust 核心 + Tauri 2 + React/TS、先 L1、P0 预留 L2。修订：同步语义、操作恢复、检索、安全边界。
> 维护者：xyyi0719

## 1. 原则与边界

1. Rust 是持久化写入入口；UI 发送命令并读取 DTO，不直接操作数据库或协议。
2. SQLite 是**本机读模型**，不是服务器邮件状态的全局权威；服务器是 L1 邮件/flag 的权威，SQLite 还保存本地意图与待发送操作。
3. 离线只能读取已缓存内容；未下载正文/附件不可用。离线操作排队，不承诺已送达。
4. `xremail-core` 不依赖存储/网络/UI；称为“依赖方向受控”，不使用“无依赖倒置”这一含混说法。
5. `xremail-sync` 初期明确是 **IMAP-aware** 的编排层。UIDVALIDITY、UID、MODSEQ 属于 IMAP 语义，不伪装成通用传输能力；JMAP 将另行设计适配。
6. 状态提交后才发事件；事件是刷新提示，SQLite 查询是可恢复事实。UI 重连/漏事件后重新查询。
7. 凭据、HTML 邮件及附件默认不可信；安全边界见 SECURITY，不通过失败降级削弱。
8. 先保证数据正确与失败可恢复，再做性能和跨平台扩展；所有容量与连接上限均需测量。

## 2. 模块与依赖

当前已有 `xremail-core`、`xremail-store`、`xremail-crypto`、`xremail-api`、`xremail-sync`、`xremail-protocol-imap`、`xremail-protocol-smtp`、`xremail-message`、`xremail-testkit`、`xremail-diagnostics` 与 `apps/shell/src-tauri`；以下为目标职责，按需创建，不要求一次拆齐。

| 模块 | 职责 | 边界 |
|---|---|---|
| xremail-core | 领域模型、错误、事件、身份/操作类型 | 不依赖 sqlx/tokio/tauri |
| xremail-api | 应用服务 DTO、L2 最小契约 | 不实现网络；不强迫其他模块反向依赖 UI |
| xremail-store | SQLite、迁移、事务、检索投影、操作队列 | 不发网络请求 |
| xremail-protocol-imap | IMAP 会话、能力与命令结果 | 不写库，不决定重试；**不依赖 xremail-message** |
| xremail-protocol-smtp | SMTP 提交与分阶段结果 | 不保证 exactly-once。**只读探测**（EHLO / STARTTLS / AUTH / 证书体检）与**提交路径**（`submit_message`：MAIL FROM / RCPT TO / DATA / QUIT）均已实现；提交路径只做 ASCII 信封地址 + RFC 2047 编码的标题（不做 SMTPUTF8） |
| xremail-sync | 账户调度、IMAP 同步、操作恢复 | 依赖 store/protocol/message/core，不认识 Tauri |
| xremail-message | MIME 结构、charset 解码、RFC 5322 头 | 不落库、不加载远程资源；**不做 HTML 清洗**（清洗在 `xremail-core::html`，见 §9.1） |
| xremail-provider | 可覆盖预设、接入诊断 | 企业邮箱需管理员配置，不能按品牌猜服务器。**尚未创建**（服务器地址目前由用户手填） |
| xremail-crypto | 成熟密码库封装、版本化密文格式 | 不自行实现密码算法；不承担所有平台钥匙串职责 |
| 平台 SecretStore 适配器 | Windows/Android 等安全存储 | 与可选远端 VaultStore 分离 |
| xremail-testkit | 假服务器、合成样本、故障注入 | 测试专用；生产代码不得依赖 |
| xremail-diagnostics | 需要同时用协议层与内容层的诊断工具 | 生产代码不得依赖 |
| shell | 组合根、Command/Event、窗口/平台生命周期 | Command 薄封装，业务在服务层 |

依赖基线：`shell → 应用服务/编排 → store、protocol、message、provider → core`。
trait 放在使用方或低层契约模块；不因“只有 trait”就允许产生环。密钥存储与 vault 同步不是同一接口。

### 2.1 协议层硬规则：命令必须自证成败

**同步引擎一律用 `xremail_protocol_imap::run_checked`，不得直接用 async-imap 的
`uid_fetch` / `uid_store` / `uid_search`。**

原因：async-imap 0.12 在服务器回 `NO` / `BAD` 时**返回 `Ok`**（只是结果为空）。
它的响应解析用 `take_while(filter(...))`，而 `filter` 遇到**自己那个 tag** 的
`Done` 响应时返回 `false`——那个携带 `NO` 的响应于是**从未被解析成错误**就被丢弃。

后果：同步引擎会把「服务器拒绝了」当成「写成功了」，本地状态与远端悄悄分叉。
`run_checked` 自己跑命令、自己读响应、自己判定 `OK` / `NO` / `BAD`，
并在错误里带上 response code（`[READ-ONLY]` / `[MODIFIED]` / …）。

这条规则同时是 §6「读快照 → 重放意图 → **回读确认**」的前提：
连「写有没有成功」都读不出来，就谈不上确认。

### 2.2 写操作的三种失败必须分清

提交本地操作时，失败分三类，**处置完全不同**：

| 情况 | 状态 | 理由 |
|---|---|---|
| 服务器明确回了 `NO` | `retryable` | 服务器告诉我们**没执行** |
| 服务器回了 `NO [MODIFIED]` | `retryable` | 条件不满足，重新读快照重算（**有界**，见 §6） |
| 连接断了 / 超时 | `uncertain` | **不知道**服务器执行了没 |

把 `uncertain` 当成 `retryable` 是危险的：盲目重试可能把同一次改动执行两遍。
反过来把 `retryable` 当成 `uncertain` 也不行——那会让本该重试的操作永远卡住。

**服务器回 `OK` 不等于改动生效了。** 必须**回读确认**：再读一次标记，
只核对**自己动过的那几个位**（服务器上别的位，比如 `Recent`，不归我们管）。

## 3. 运行时与连接所有权

- 每账户一个 AccountActor 接收有界 mailbox，拥有账户 IMAP 会话及同步状态。
- 首版默认每账户一条 IMAP 连接；仅为保守起点，不声称服务商固定允许 3–5 条。连接数与全局预算配置化，后续按实测调优。
- 同一流只允许一个拥有者读写。IDLE 是会话状态，不是另一个 task 同时读 socket。
- 用户操作到达：请求退出 IDLE → 发送 DONE → 等 tagged completion → 执行命令 → 返回 IDLE。
- IDLE 仅监听被选中的 mailbox；其他文件夹需轮询或调度切换，不能声称监听 INBOX 覆盖全账户。
- 无 IDLE 时轮询；认证失败进入待重新授权，不用无限重连掩盖问题。
  （2026-11-03 落地：`crates/xremail-sync/src/actor.rs` 里 `AppError::Auth` 是**终态**——
  发 `AccountEvent::NeedsReauth` 后直接退出 actor，不再退避重试；
  应用层映射成 `SyncState::ReauthRequired`。所以错授权码不会把日志刷满、
  也不会把「要人管」伪装成「正在重连」。用户重新保存授权码后再 `start` 即可）
  （**2026-11-03 补正**：上面这条在此之前只对「登录路径」成立——命令层的 `NO`
  曾被误判成认证失败（删掉一个文件夹就能把整个账户判死并停在待重新授权），
  而外壳层的 `connect` 又把真·登录被拒拍平成协议错（错授权码被永久重试）。
  两处均已修：命令错误一律走 `connection::classify_imap_error`，只有登录路径
  才能产出 `ProtocolError::Auth`；外壳层改走 `xremail_sync::map_protocol_error`。见 R25）
- SMTP 可独立连接；操作 worker 按账户/文件夹身份顺序执行，不能与 Actor 争用 IMAP 流。
- SQLite WAL、有界连接池、busy timeout；所有业务写入由 Rust 协调，不等于只有一个连接或一个 task。
- 退出、账户删除、网络切换均传播取消；后台任务应被监督并回收。
  （桌面端的「退出」只有一条路：托盘菜单 → `app.exit(0)` → `RunEvent::Exit` →
  `Scheduler::shutdown()`；**关闭窗口不算退出**，只是把窗口藏起来、同步继续跑。
  取舍与不能用的写法见 §9 的「窗口生命周期与退出」。）

## 4. 身份、缓存与同步事务

### 4.1 身份不变量

远端邮件定位使用 `(account_id, folder_id, uidvalidity, uid)`。本地 message ID 独立生成。
RFC Message-ID 只能作线索，不能作为唯一键或自动跨文件夹去重依据。

### 4.2 同步主路径

1. 建立并校验 TLS；认证前后按需要获取 CAPABILITY，使用认证后能力。
2. LIST/只读选择文件夹，取得 UIDVALIDITY、可用的 MODSEQ 等信息。
3. 对比已提交游标；epoch 变化时先走 4.3。
4. 根据能力拉 UID 集合/增量 FLAGS；分批 upsert 摘要，事务提交游标后发事件。
5. 按阅读需求或缓存策略用 BODY.PEEK 拉正文/附件；正文到达与检索投影同事务更新。
6. 比对服务器已删除 UID；删缓存必须尊重本地草稿/未发送内容的独立归属。
7. 进入 IDLE 或轮询；所有请求有超时、资源上限和可取消边界。

**实现落点（2026-10-22，`crates/xremail-protocol-imap`）**：第 1 步与第 2 步的实测结论已固化——


* **服务器宣告 `ID` 能力时，登录后必须立即发一次 `ID`（RFC 2971）**。163 在不发 `ID` 时把
  `EXAMINE` / `SELECT` 一律回成 `NO … Unsafe Login`，邮箱根本进不去。内容任意（`ID NIL` 除外）。
* **能力集只能在认证后拿**：163 认证前拒绝一切 IMAP 命令，连 `LOGOUT` 都不行。
* **邮箱名在线路上是 modified UTF-7**（RFC 3501 §5.1.3），不解码就认不出「已发送」；
  实现在 `xremail-protocol-imap::utf7`。
* **游标不能一律基于 UIDNEXT**：163 的 `STATUS` 接受 `UIDNEXT` 字段但**不给值**，
  须用 `UID SEARCH` / `UID FETCH 1:*` 取最大 UID。
* **不要无条件请求 `HIGHESTMODSEQ`**：不支持 CONDSTORE 的服务器（如 163）会回
  `BAD Parse command error`。

初次同步优先摘要与最近邮件，不默认下载全部附件。性能指标必须区分摘要、正文与附件。

**实现落点（2026-10-23，`crates/xremail-sync/src/pull.rs`）**：第 3–6 步已实现并有端到端测试
（假 IMAP 服务器 × 真客户端 × 真实 SQLite）：

- 第 2 步用**协议层的安全 `examine_checked`**（服务器拒绝时一定报错）；
  拿不到 UIDVALIDITY 就**报错**而不是猜一个（没有 epoch 就无法定位邮件）。
- 第 3 步：`plan_epoch_transition` 判定；epoch 变了走 4.3。
- 第 4 步：远端 UID 集合用 `UID FETCH 1:* (UID)`——**FETCH 是必备命令，SEARCH 是可选扩展**；
  然后 `diff_uids` 作差；拉取时带上 `BODY.PEEK[HEADER]`（头），
  解析出主题 / 发件人 / 日期 / Message-ID 一并落库。
- 第 5 步：正文**按需**拉（`BODY.PEEK[]`，见 `crates/xremail-sync/src/body.rs`）——
  正文可能很大且带附件，一个邮箱几千封全拉一遍既慢又费流量。
- 第 6 步：服务器已删的 UID 清缓存；**本地草稿的保留策略**由 `UidDiff::retain_local` 落实。
- 第 7 步（进入 IDLE 或轮询）**已实现（2026-11-03）**：
  `crates/xremail-sync/src/actor.rs`——一个账户一个 actor，交给调度后立刻同步一轮，
  之后按 `poll_interval_secs` 轮询，`Scheduler::wake` 可插队；
  失败按 `min_backoff`→`max_backoff` 翻倍退避；每一轮都包在
  `catch_unwind` 里（R13），panic 只停在那一轮。
  **服务器支持 IDLE 时走长连接**（`crates/xremail-protocol-imap/src/idle.rs` 的
  `idle_once`，由 `crates/xremail-sync/src/actor.rs` 的 `wait_for_change` 调用）：
  等 `+ idling` → 读推送 →
  发 `DONE` → 等 tagged `OK`；30 分钟或一到推送就发 `DONE` 重进，
  出口无条件 `is_authenticated()` 校验——**没走完 `DONE` 三次握手的会话一律丢弃重连**，
  绝不把半 IDLE 流的 `Session` 还回连接池。IDLE 失败 / 服务器不支持 → 自动降级轮询。
  同时：这个 actor **已经接进应用层**——`apps/shell/src-tauri/src/lib.rs` 启动时
  从凭据可用性决定拉起哪些账户、`RunEvent::Exit` 时停；
  `cmd::sync_account` 在「第一次拿到可用凭据」时也补一次 `start`（幂等，已在跑就不叫醒，
  避免和本次手动同步并发跑两轮）。

**三条实测得来的实现约束（2026-10-23）**：

* **远端 UID 集合不要用 `UID SEARCH ALL`**：163 与沃虽实现了 SEARCH，但它在 RFC 3501 里是
  **可选**扩展；`UID FETCH` 才是必备命令。
* **`1:*` 在大 UID 空间下不能展开**：163 的 UID 是 17 亿量级，
  把 sequence set 展开成列表会吃掉几个 GB——要**逐个问 `contains`**。
* **文件夹角色不能只靠 RFC 6154 属性**：**QQ 不给**（只给 `\NoSelect` / `\HasChildren`），
  必须准备**按名字**的兼底（`Sent Messages` / `Drafts` / `Deleted Messages` / `Junk`）。

### 4.3 UIDVALIDITY 变化

在一个数据库事务内切换 epoch、失效旧 UID 映射与游标、隔离绑定旧 epoch 的 pending 操作。
禁止把旧操作直接套到新 UID；重新全量建立该文件夹快照。保留操作记录与冲突原因供恢复/提示，附件引用按回收策略处理，不能误删共享 blob。
PLAN 已有 `sync_state.uidvalidity`，无需重复添加同义字段。

**实现落点（2026-10-22，`crates/xremail-sync/src/epoch.rs`）**：`plan_epoch_transition` 把三种情形分开——
epoch 未变（增量）、首次同步（全量拉取）、epoch 变了（**重建快照**）。
重建时列出绑定旧 epoch 的待提交操作 ID（`quarantined`），**不**把它们套到新 UID 上。
UIDVALIDITY 不得为 0（RFC 3501），收到 0 视为协议错误。

**实现落点（2026-10-22，P0 迁移 `0001_init.sql`）**：epoch 与游标统一放在 `folder` 表
（`uidvalidity` / `last_uid` / `highest_modseq` / `last_sync_at`），与 `xremail-core` 的
`Folder` 结构一一对应，**不另建 `sync_state` 表**——那样会出现两处同义字段。
`mail_operation` / `send_queue` 对邮件只做**逻辑引用**（不设外键）：epoch 切换后旧 UID 失效，
这些操作应被隔离并保留冲突原因，而不是随邮件一起被级联删除。

### 4.4 启动对账（崩溃恢复）

进程可能在**任何**时刻死掉（断电、崩溃、被系统杀掉），而死的位置决定了服务器上
到底发生了什么：死在**发命令之前**可以安全重放，死在**发出之后、收到确认之前**
重放就是重复执行。本地唯一能观测到的是「这一行是不是还停在 `running`」。

所以对账的目标**不是猜服务器做了什么**，而是把没人负责的行标成没人负责，
交给下一轮的回读确认去核对（`push_flags` / `push_transfers` 天然幂等）。

**实现落点（2026-11-03，`crates/xremail-sync/src/reconcile.rs`）**：

* **按 boot 归属回收**。`running` 只说明有人认领了，不说明认领者还活着。
  认领时写 `owner_boot`（迁移 `0006_operation_owner.sql`，`claim_for_push` /
  `claim_for_sending`），对账只收 `owner_boot IS NULL OR owner_boot <> 本次 boot` 的行；
  `IS NULL` 单独判是因为升级上来的老数据没有归属，按「不是我的」处理。
  没有这层守卫，同一台机器上另一个**还活着**的进程的存途行会被翻成「结果未知」。
* **先回收、后判过期——顺序本身是语义**。反过来的话，一条属于上次进程、
  次数也已到顶的 `running` 会被 `expire_exhausted` 的状态过滤（`queued` /
  `retryable` / `uncertain`）漏掉，永远卡在 `running`：既没人推它，也没人报错。
* **`uncertain` 绝不改回 `queued` / `retryable`**。那等于替服务器判断「上次没执行」，
  而那正是我们不知道的事；唯一能改它的是**人的显式决定**
  （与 `send_queue::retry_uncertain` 同一套哲学，见 §5）。
* **只碰操作队列的状态与错误字段，不碰 `message`**。邮件本身的标记由回读对齐，
  对账不能凭空造第二套真相。
* **同时收掉的残骸**：孤儿操作（文件夹已删 → `folder_missing`；移动目标已删 →
  `destination_missing`）、绑定旧 epoch 的操作（→ `epoch_changed`，与 §4.3 一致）。
* **幂等**。触发点有两个：应用启动（`apps/shell/src-tauri/src/lib.rs:235`）与
  **每轮同步成功之后**（2026-11-06 随 R33 新增，`crates/xremail-sync/src/actor.rs:437 reconcile_after_tick`，
  `Synced` 事件前调用）。
  启动时要做的理由：调度器只对「已交给它调度」的账户跑，
  而崩溃恢复必须对**每个**账户都发生。幂等是为了容忍重复调用。
  **仍未做**：actor 的 `Startup` 轮**不单独**先对一次账——它靠第一轮 `tick`
  成功后的 `reconcile_after_tick` 顺带覆盖。
* 纯函数 `plan_reconcile` 把「观测 → 决定」从 IO 里拆出来，可单测；
  `reconcile_account_as` 是执行的壳。
* 用例：`crates/xremail-sync/tests/crash_recovery.rs` 11 个（含「活着的进程的行不动」
  「次数到顶的 `running` 先回收再判死」「幂等」三条关键的）。

### 4.5 重发前的幂等探测与次数闸门

§4.4 只解决「`running` 的残骸」；还有一类更隐蔽的重复：操作**自己**被重发。

* **`uncertain` 必须计次**。`uncertain` 表示「命令可能已经到服务器了」，它和
  `retryable` 一样会被 `claim_for_push` 重新认领（`state IN ('queued','retryable','uncertain')`），
  而那道闸门靠 `AND attempts < ?` 卡住。原先 `uncertain` 出口只用 `update_operation_state`
  改状态、**不自增 `attempts`**，于是 `attempts < ?` 永远成立——一条发不出去的
  `copy+delete` 会**每轮重发一次** `COPY`。现在两条出口都自增：
  `operation::mark_operation_retryable`（→ `retryable`）与
  `operation::mark_operation_uncertain`（→ `uncertain`）。自增点仍然**只有这两处**，
  认领（`claim_for_push`）不自增，否则一次失败会烧掉两个配额。
* **非幂等路由重发前先探测**。`MOVE` 幂等、`COPY` 不幂等；163 没有 `MOVE`，
  只能 `COPY` + 删除。所以重发前必须确认目标文件夹里是不是已经有这封：
  `message_dao::find_message_id_by_uid` 取本地 `Message-ID` → `EXAMINE <目标>` +
  `UID SEARCH HEADER Message-ID "<id>"`。探到就**跳过 `COPY`**、只补删除那一半
  （`transfer_push.rs` 的 `submit(..., copy_already_done)`）。
* **取不到 `Message-ID` 就不重发**。宁可停在 `uncertain` 等对账 / 用户确认，也不赌。
  这是本条与「反正重试」的分界线：**能证明已生效才重发，不能证明就停下来**。
* 探测只在 `attempts > 0` 时做——首次尝试不可能产生副本，而探测要切文件夹、多打两条命令。
* 用例：`crates/xremail-sync/tests/push_transfers.rs` 3 条（重发不多出副本 /
  无 `Message-ID` 拒绝重发 / 次数到顶一条命令都不发）。

将队列分为 **mail_operation**（flag/move/delete/append）与 **send_queue**（SMTP）。名称可在迁移时定稿，避免一个模糊 outbox 混淆语义。

- 一次本地命令在同事务内写操作意图与本地投影；提交后返回/发事件。
- UI 可立即显示带 pending 标识的本地结果，不能把它显示为服务器确认。
- 状态至少区分 queued、running、succeeded、retryable、failed、uncertain；具体枚举在实现时通过测试固定。
- 记录操作 ID、目标身份/epoch、基线版本、attempts、错误类别。进程中断的 running 必须按操作类型核对，而非一律重放（启动对账怎么收残骸见 §4.4）。

| 操作 | 重试原则 |
|---|---|
| bit 级 +FLAGS/-FLAGS | 可核对状态后重放；不要整组替换 FLAGS |
| MOVE/COPY/APPEND | 若响应丢失，先依据 UIDPLUS/MOVE 结果或可用线索核对；无法可靠确认则 uncertain |
| 删除 | 首版优先移入垃圾箱。无 UIDPLUS 时不普通 EXPUNGE，防止清除别的客户端已标删除的邮件 |
| SMTP | 连接/提交阶段分别分类；最终 DATA 可能已接受时标 uncertain，禁止盲重发 |

Message-ID、稳定操作 ID 有助于排查，但不能保证 SMTP 去重。明确不承诺网络故障下 exactly-once。

**实现落点（2026-10-23，后经 2026-11-03 核实修正）**：状态枚举已定在 `xremail-core::model::OperationState`
（六态，取值与 `mail_operation.state` 列一致）；`xremail-store::dao::operation` 提供入队、
列未完结操作、更新状态。**已实现的类别**：flag（2026-10-26）、append（2026-10-30，发信存副本）、
move / delete（2026-11-03）、SMTP（2026-10-28，`send_queue` 独立队列）。
`OperationKind` 三值：`flags` / `move` / `delete`（`crates/xremail-core/src/model.rs:463`），
移动的目标文件夹记在 `MailOperation::destination_folder_id`（迁移 `0005_operation_destination.sql`）；
协议层 `crates/xremail-protocol-imap/src/transfer.rs` 提供 `copy_messages` / `move_messages` /
`mark_deleted` / `uid_expunge` 与假服务器支持。

**实现落点（2026-10-26）**：flag 类操作**已真正回写服务器**。
链路：`MessageService::set_flag_intent` 先落一条 `mail_operation`（`queued`）→
`cmd::set_message_flag` 用凭据连服务器 → `xremail_sync::push_flags` 以**可写**方式
（`SELECT`，不是 `EXAMINE`）提交 `UID STORE` → **回读 `FLAGS` 逐位确认** →
按 §2.2 把结果分成 `succeeded` / `retryable` / `uncertain` → `cmd` **按操作 id 读回这一条**
的状态给前端（`FlagIntentResult`：`queued` / `applied` / `state` / `message`）。

「UI 可立即显示带 pending 标识的本地结果」这条**已落地**：
`MessageService::list_messages` 会把该文件夹里**未完结**的意图叠加到本地邮件上
（`overlay_pending_intents`，只叠加 `uidvalidity` 与当前 epoch 一致的那些），
所以点「已读」后列表立刻变，而服务器确认与否是**另一件事**——
前端拿到的是 `applied`（服务器回读确认过）而不是「UI 显示了」。

epoch 绑定：意图落库时记下当时的 `uidvalidity`。提交前发现 epoch 变了 → 这条意图
**作废**（`failed` / `epoch_changed`），因为旧 UID 在新 epoch 下指向另一封信。

**实现落点（2026-10-28）——发信路径**：`send_queue` 表与
`xremail-store::dao::send_queue` 已落地，状态枚举在
`xremail-core::model::SendState`（与 `mail_operation` 同名六态）。
`apps/shell/src-tauri/src/service/compose.rs` 的流程是：

1. 落草稿（`draft`）→ 2. 构建并**冻结字节**（`queued`，字节写进库里不再变）→
3. **抢占**（CAS：两个发送器只有一个能拿到 `running`）→ 4. 提交给服务器 →
5. 记结局 → 6. **回读**这一行确认。

两条硬规则，都是从「重复发信」这个代价反推出来的：

- **字节一旦冻结就不再变**。排队之后改内容，必须另存成新草稿——
  否则用户看到的和实际发出去的就不是同一封了。
- **不确定就不自动重发**。`DATA` 发出后没拿到最终应答 → `uncertain`，
  这封信**可能**已经在服务器手里，盲目重发会让用户收到两封。
  只有明确没发出去的失败（4xx 拒收、连不上、TLS 失败）才记 `retryable`。

**前置失败也要记账**：连不上 / TLS 握手失败发生在**任何命令发出之前**，
所以这封信**肯定没发出去**，重发是安全的。但它**不能**当成「什么都没发生」——
不记账，这一行就会永远停在 `running`，下次启动的回收逻辑会把它当成
「发到一半」变成 `uncertain`，把一个「肯定没发」的信说成「可能发了」。
所以前置失败写回 `retryable`，并在给用户看的说明里讲清楚。

**Bcc 只进信封**：进 `RCPT TO`，不进邮件头（没有 `Bcc:` 行）。
信封收件人 = To ∪ Cc ∪ Bcc（去重），与显示头分开。

**「已发送」副本不一律自己存**（2026-10-30 实测修正）：
**两家服务器都会自己存**——163 提交后几乎立刻可见，
QQ 要 **30–90 秒**才落盘（第一次实测查得太早，误记成「QQ 不存」）。
所以程序不能一律存（163 会变成两份），也不能一律不存（Gmail 这类根本不自存的就看不到自己发出去的信）。
**自适应做法**（`apps/shell/src-tauri/src/service/sent_copy.rs`）：
1. 发完等一个**短的沉降窗口**（2.5 秒）让服务器自己存；
2. 在「已发送」文件夹里按 `Message-ID` 找这封信；
3. 找到了 → 服务器自己存了，**不碰**；
4. 没找到 → 自己 `APPEND` 一份（协议层 `crates/xremail-protocol-imap/src/append.rs`）；
5. `APPEND` 被拒且理由是「已经存过了」 → 照样当「已有一份」（**第二道保险**）。

**匹配要比尖括号段**：`Message-ID` 在 RFC 5322 里一定是 `<…>` 形态，
而 QQ 会**改写** `Message-ID`、把原值挪到 `X-OQ-MSGID`。
所以匹配规则是「把每个头值里的尖括号段拿出来比」，
不是「拿整个头值比」（`Received:` 那种头什么都有，会误撞）。

**残余风险**：沉降窗口短 + 服务器慢存 ⇒ 可能补成两份（ISSUES_AND_RISKS R24）。

**TLS 回退**：服务器拒绝 TLS 1.3 时，**用同一条链路重试一次 TLS 1.2**。
证书校验**一点不放松**——回退只发生在协议版本上，且结果里写明回退过。

**协议层的范围边界**（2026-10-28，有意为之，不是遗漏）：
`submit_message` 一次一封（连一次 → 认证一次 → 发一封 → `QUIT` 一次）；
**不做 `RSET`**（中途失败直接断开，不把服务器状态留给下一次调用）；
**不做流式 `DATA`**（整封进内存，几十 MB 的附件会有压力）；
**不做 `SMTPUTF8`**（QQ 宣告了、163 没有，地址与标题的编码策略定下来之前发它会被整个事务拒收）；
`XOAUTH2` 明确 `Unsupported`（它要的是 OAuth 令牌，不是账号口令）。
服务器**没宣告 `AUTH`** 时会告警并跳过认证直接提交（对应本地中继这类场景），
但**不会**在明文上送口令——`STARTTLS` 缺失是硬失败。

## 6. flag 并发语义

服务器快照与本地待提交意图分开保存。`+Seen` 和 `-Seen` 可以冲突，并非“单 bit 都幂等所以没有冲突”。

- 有 CONDSTORE：基于已知 MODSEQ 用 UNCHANGEDSINCE 条件写；MODIFIED 后重新读取并按意图重算，重试有界。
- 无 CONDSTORE：读取快照、重放本地 bit 级意图、发送后回读确认；接受服务器执行顺序带来的竞态窗口。
- 本地时间戳/HLC 不能排序普通 IMAP 客户端的远端操作；MODSEQ 也不是跨 mailbox 的全局时间。
- 移动后原 UID 无效；删除/移动冲突应重新核对存在性，无法定位时失败或交给用户，不按时间戳猜测目标。
- L2 自建同步未来可用显式版本/CAS；不能把它的 HLC/LWW 规则套进 IMAP。

**实现落点（2026-10-22，`crates/xremail-sync/src/intent.rs`）**：`FlagIntent` 记录一个 bit 级意图
（uid、**epoch**、add/remove、`seq` 顺序号、基线 MODSEQ）。`apply_intents` 按 `seq` 排序作用于快照，
后者覆盖前者——`+Seen` 与 `-Seen` 的冲突就是这样定序的。
`plan_flag_update` 只产出**净变化**的位：净变化为空时返回 `None`（不发命令），
其余只含真正变化的位，**不整组替换 FLAGS**。绑定旧 epoch 的意图进 `FlagPlan::stale`，不参与计算。
服务器回 `MODIFIED` 后，客户端用新快照重调 `plan_flag_update` 即可重算，
次数上限见 `MAX_REPLAN_ATTEMPTS`。

**实现落点（2026-10-23，`crates/xremail-sync/src/push.rs` + `crates/xremail-store/src/dao/operation.rs`）**：

* **意图单独落库**（`mail_operation` 表），状态机六态：
  `queued` / `running` / `succeeded` / `retryable` / `failed` / `uncertain`。
  这些取值**就是 DB 列的取值**，改枚举名要配数据迁移。
* 同一封信上多个意图的定序靠 `seq`（本地单调序号）——**不能用 `created_at`**：
  同一毫秒里的两个意图会撞号。
* 提交流程：读快照 → 重放意图算净变化 → 发 `UID STORE` → **回读确认**。
  回读只核对**自己动过的那几个位**（服务器上别的位，比如 `\Recent`，不归我们管）。
* **三种失败分得很清**（见 §2.2）：服务器回 `NO` → `retryable`；
  回 `NO [MODIFIED]` → `retryable`（重新读快照重算，**有界**）；
  连接断了 / 超时 → **`uncertain`**。
* 绑定旧 epoch 的操作**隔离**为 `failed`，绝不套用到新 UID 上。

**实现落点（2026-11-03，`crates/xremail-sync/src/transfer_push.rs`）——移动 / 删除：**

* 与标记回写**分两条通道**：标记的意图是「这封信该带哪些 flag」（可按 bit 算净变化），
  移动/删除的意图是「这封信不该在这个文件夹里了」——没有净变化可算，
  只能做一次然后**核对它真的走了**。
* 路由由**服务器能力**决定，抽成纯函数 `route_for` 以便不连服务器就能测：

  | 场景 | 发什么 |
  |---|---|
  | 有 `MOVE` | 一条 `UID MOVE` |
  | 没有 `MOVE`（163、沃） | `UID COPY` → `UID STORE +FLAGS (\Deleted)` → **有 `UIDPLUS` 才** `UID EXPUNGE`（两家实测都有 `UIDPLUS`，会走到最后一步） |
  | `MOVE` 与 `UIDPLUS` 都没有（三家实测都不属于此类） | 停在「已标删」 |

* **裸 `EXPUNGE` 一律不用**：它会把这个文件夹里所有带 `\Deleted` 的信都删掉，
  包括别的客户端标了还没提交的。那也是用户的数据。
* **删除 = 进垃圾箱**（保守优先）：有垃圾箱就挪进去（可撤销），没有才就地标删。
  垃圾箱靠**文件夹名**推断（本地库还不存 `\Trash` 属性）；推不出来就不乱挪。
* **移动专属的保守处理**：源文件夹里已经没有这封邮件 → 算成功。
  可能是上次尝试生效了（应答丢了），也可能别人挪走了——两种情况下
  「再挪一次」都是错的（RFC 6851 的 `MOVE` 不是幂等的，`COPY` 重发会多一份）。
* 失败分类与标记回写**完全一致**（复用 `SubmitError`）：`NO` → `retryable`；
  断线/超时 → `uncertain`；认证失败不重试。
* **队列由同步轮次自动排空**（`sync::flush_pending`，每轮拉取之后）：
  用户离线时点的移动不会丢，也不需要界面开着。
* 一个必须钉住的细节：`push_flags` **只处理 `kind = flags` 的操作**。
  若让它把移动意图也收进来算净变化，空载荷会被当成「净变化为零」直接标成
  `succeeded`——邮件既没搬走也没删掉，队列却空了（静默丢数据）。
  回归用例：`push_flags::move_intents_are_left_to_the_transfer_channel`。

## 7. 检索模型

推荐先做显式 `search_document` 投影，聚合摘要与已解码正文；使用普通存内容 FTS5 表并在 DAO 事务内维护索引。
这是待迁移验证的设计，不沿用 PLAN 原来的 `content=''` 示例。若空间测量后改外部内容表，必须另行定义列对应、删除协议、触发器与重建。

元数据变化、正文晚到/替换、删除、UID epoch 重建都要更新投影。提供可重复 rebuild 与一致性检查。
trigram 对不足三个字符的检索有限制，中文短词采用有界 LIKE 回退或明确 UI 限制，不能假装具备完整中文分词。

**已实现（2026-10-30，迁移 `0004_message_search.sql` + `crates/xremail-store/src/dao/search.rs`）**：
投影是 `message_search_doc`（一封邮件一行：标题 / 收发件人 / 纯文本正文），
倒排索引用 FTS5 **外部内容表** `message_search_fts`（`tokenize='trigram'`），
正文只在文档表存一份。

与上面的设计有一处**故意的偏离**：索引**不由 DAO 事务维护**，而是交给
**数据库触发器**（`message` / `message_body` 的增删改 → 文档表 → FTS 表）。
理由是 epoch 切换、级联删除、导库这些路径**不经过应用代码**，
DAO 事务维护的索引会在那些路径上脱节；触发器是**任何写入路径都绕不过**的。
`rebuild_index`（可重复）与 `check_index_consistency`（查漏索引 / 孤儿行 / 正文过期）
作为兜底，正常路径用不上。

**只索引 `text_plain`，不索引 `html_raw`**：原始 HTML 里的标签、样式、内联脚本是噪音，
命中会被标签切碎，片段也是一堆尖括号。
**短词回退**：查询词里有 1~2 字符的词时，trigram 切不出词元（实测 `MATCH '"项目"'` 命中 0），
DAO 层退化成 `LIKE` **子串**扫描——这与用户输「项目」的直觉一致，代价是全表扫描。

## 8. L2 预留的最小范围

P0 按既定决策保留接口与 schema，但不建设远端服务：

- `SecretStore`：平台凭据读写删，与普通 SQLite 存储分离；失败语义明确。
- `VaultStore`：本地版本化密文封装与元数据持久化，不等于 OS 钥匙串。
- `SyncBackend`：读取远端密文/revision；以 expected revision 条件提交，原子返回新 revision 或冲突。
- 内存 mock 测试首次写、往返、双设备 CAS 竞争、错误返回；禁止独立 check_conflict 后无条件 push 的 TOCTOU 设计。
- label/rule 等表预留稳定 ID、schema version 与必要生命周期字段；不提前冻结复杂 CRDT/同步协议。

**实现落点（2026-10-22，`crates/xremail-api`）**：三个契约已建，内存 mock 可运行。
`SyncBackend` **只提供原子的 `compare_and_swap`**，刻意不提供「先查冲突再推送」的组合方法——
检查与推送之间状态可能已变，那会静默丢失更新（TOCTOU）。冲突错误 `SyncError::Conflict { current }`
携带远端当前修订号，调用方据此重新读取并合并。`xremail-crypto` 提供版本化密文封装，
其中 `to_bytes`/`parse` 是**自包含**字节格式，可作为不透明载荷在设备间搬运。

P6 前重新审查身份认证、密钥恢复、轮换、回滚/重放防护与元数据泄露。接口预留降低耦合，不保证零返工。

## 9. 平台与错误边界

Windows 先打通完整链路，Android 保留目标并尽早做小型生命周期 PoC。普通轮询不保证 Doze 下准时执行；前台服务也不是永久保活承诺。
Linux/macOS/iOS 不属于当前验收平台，不因 Tauri 支持就视为产品可用。

**平台钥匙串（2026-10-26 已实现 Windows 侧）**：`xremail_api::SecretStore` 的 Windows 适配
在 `apps/shell/src-tauri/src/secrets/windows_store.rs`，条目名 `xremail:account:<account_id>`
（通用凭据，`CRED_PERSIST_LOCAL_MACHINE`）。非 Windows 平台是 `secrets/unsupported.rs` 的**桩**：
它**不**把凭据落到任何地方（`get` 恒为 `None`），平台不支持时**如实报错**，
**不**降级成「存内存」或「存文件」——那样用户会以为已经保存了。
`CredentialService`（`service/credential.rs`）只认 `account.secret_ref`：
没有引用就**不去打扰钥匙串**；`forget_credential` 会同时删钥匙串条目并把 `secret_ref` 清成 `None`。

**panic 边界要盖住全部 await 点，不是只盖「主流程」**：`catch_unwind` 是手写的，
漏一处就等于没盖。`crates/xremail-sync/src/actor.rs` 里有**两条**会 `connect` 的路径——
`tick`（跑一轮同步）与 `wait_for_change`（等待期用 `IDLE` 维持连接）。两条都要包；
`wait_for_change` 曾漏掉，后果是任务**静悄悄死掉**而不是崩，比崩更难发现。
判断标准：凡是跑在 tokio 任务里、会 `await` 且可能 panic 的函数，要么自己就是边界，
要么**只**被已经包了边界的函数调用。**残余**：Tauri 命令处理线程与前端 Wry IPC
回调不在这套边界内（见 [ISSUES R13](ISSUES_AND_RISKS.md)）。

**窗口生命周期与退出（2026-11-09，`apps/shell/src-tauri/src/tray.rs`）**：桌面端有托盘，
语义只有两条——

* **关闭主窗口 = 退到托盘**：`Builder::on_window_event` 里对 `label == "main"` 的
  `CloseRequested` 直接 `hide()`，进程、窗口与后台同步都留着。不能靠「窗口关掉、
  让托盘图标把进程留住」：Tauri 2 的 `WindowEvent::CloseRequested` **不暴露** `api`，
  应用拦不住销毁；窗口一旦销毁，托盘左键只会去聚焦一个不存在的窗口，
  于是一个**关不掉也回不去**的常驻进程。
* **真退出只有一条路**：托盘菜单「退出」→ `app.exit(0)` → `RunEvent::Exit` →
  `Scheduler::shutdown()`。**不能**换成 `std::process::exit`：它不发 `RunEvent::Exit`，
  上面那条收尾就成了永不执行的代码，退出时 actor 还在 `IDLE` 或写库，
  未提交的写操作会停在 `running`（下次启动靠对账收拾）。系统发起的退出（注销、关机）
  同样走 `Exit`，收尾一致。

托盘菜单只有「显示主窗口 / 立即同步 / 退出」三项。左键单击 = 显示并聚焦，
与 `tauri_plugin_single_instance` 的「第二次启动」走同一个函数（藏起来的窗口必须先
`show`）。「立即同步」对**已调度**账户逐个 `Scheduler::wake`（`service::scheduler::wake_all`），
**不**用 `start`——后者的语义是「确保在调度」，读起来像新增了一份调度。
开机自启时**仍然弹窗口**：按启动参数「只驻留托盘」能做，但那要在 `setup` 里
把默认显示的窗口再关掉，参数一错就是「点了没反应」，而自启的诉求
（登录后后台同步已在跑）当前实现已满足。

托盘是 tauri 的核心 API（`tauri::tray`），只需要开 tauri 自己的 `tray-icon` feature，
不引入新依赖；托盘只在桌面端创建（`#[cfg(desktop)]`）。
**验证程度**：Linux 上编译与 clippy、fmt 通过，菜单 id → 动作有单元用例；
**Windows 真机行为（图标、左键聚焦、退出收尾）未验证**——本机无显示器。

错误按可恢复原因分类：传输临时失败退避；认证需用户动作；磁盘满/迁移失败不能当普通 Storage 错误无限重试；协议错误不原样输出可能含凭据/正文的响应。
事件枚举以 `xremail-core/src/event.rs` 为准，flag 更新使用 MessageUpserted，当前没有 FlagsChanged。
**约束冲突不是可重试错误**：`xremail-store` 把唯一键/外键/CHECK 违反映射为 `AppError::Invalid`
（不可重试），其余 sqlx 错误映射为 `AppError::Storage`（可重试）。重试同一份输入不可能成功，
因此不能一律归入 Storage。

错误附加上下文时必须保留错误变体，否则 `is_retryable()` / `requires_user_action()` 判定失效。
`AppError::with_context` 按变体逐个包裹，`Context::ctx` 基于它实现，已有单测锁定该行为。
`panic = "abort"` 会让后台任务无法用 `catch_unwind` 兜底，单个同步 task 的 panic 就会终止整个应用。
因此 release profile 选用 `unwind`（仓库根 `Cargo.toml:96`），并在同步任务边界加 `catch_unwind`
（`crates/xremail-sync/src/actor.rs`，每轮同步一个边界）；取舍与残余见 [ISSUES_AND_RISKS R13](ISSUES_AND_RISKS.md)。

### 9.1 HTML 正文的安全边界（R06）

邮件正文一律**不可信**。**清洗在后端完成**（`xremail_core::html::sanitize_html`），
出后端的只有清洗过的 `htmlSafe`——**原始 HTML 不出后端**（`MessageBodyView` 只带 `htmlSafe`）。
前端用 `dangerouslySetInnerHTML` 渲染它，所以**安全边界就是后端那道消毒**；
消毒器的白名单因此在 core 层维护，用例锁住（`crates/xremail-core/src/html.rs`）。

消毒规则：标签白名单 + 属性白名单；`script` / `style` / `iframe` / `object` / `embed` /
`form` 等**连内容一起删**（它们的内容本身就是载荷）；删 `on*` 事件属性与
`javascript:` / `data:` 协议；删**远程图片**（`src` 指向 http/https）——
远程图片是**跟踪像素**与**信息泄露**的通道。
其余**未知标签只删标签、留着内容**：真实邮件的正文全都被 `html` / `body` 包着，
连内容一起删的话整封信就没了。

正文只有 `text/html` 时，**从 HTML 派生纯文本**（`html_to_text`），
写入时派生、启动时回填老数据：实测 163 的邮件大多只有 `text/html`，
只认纯文本的话这类邮件**搜不到**也读不出来（见 §7）。

## 10. 性能预算与复审

旧版启动、内存、5000 封 3 分钟、30 秒双端一致等数字都是未测目标，不是门槛保证。
先测基线，记录设备/网络/数据规模与缓存状态，再设回归阈值；安全和数据正确性不因性能而让步。

架构变更需写明不变量、迁移与失败路径；改变 UI 栈、同步层次或 Android 范围须用户确认。测试覆盖见 TESTING_STRATEGY。

相关：[风险](ISSUES_AND_RISKS.md) · [安全](SECURITY.md) · [测试](TESTING_STRATEGY.md)
