# 路线图

> 执行范围入口。**新接手请先读 [HANDOVER](HANDOVER.md)**——它讲的是「现在处于什么状态、还差什么」。
> 个人研究项目，AI 编码，允许一年以上探索；不使用未经测量的 AI 耗时系数或固定交付日期。
> 当前已交付 P0–P3（见 §2 的打勾项）；P4 的 Windows 侧已交付、**Android 侧只做到「能编出 APK」**（真机上的运行时行为一条未验，见 P4 小节）；
> **P5-L（Linux 支持）** 的主要工作已落地（打包、钥匙串、托盘、通知、开机自启、WebKitGTK 沙箱验证），剩下的只有「真实桌面会话 / 非 Debian 发行版」这类需要真机的项；
> P5（本地增值功能：标签/规则/联系人）/ P6（L2 远端同步）**未开始**。
> 已确认：国内邮箱优先、Gmail 暂缓、先 L1 且 P0 预留 L2。
> 维护者：xyyi0719

## 1. MECE 工作分区

按主要交付物划分；安全/测试是各工作区的验收条件，不再额外重复计入工期。

| 区域 | 唯一主责 | 不负责 | 对应阶段 |
|---|---|---|---|
| A 外部接入 | 配置、认证、协议能力证据 | 缓存/业务 UI | P1 |
| B 数据与同步 | 身份、存储、增量、操作恢复 | HTML 展示/SMTP 提交 | P0/P2 |
| C 邮件交互 | 阅读、检索、编写、发送 | 系统生命周期 | P2/P3 |
| D 平台宿主 | Windows/Android 生命周期、通知、安全存储适配 | 邮箱业务规则 | P4 |
| E 可选扩展 | 标签/规则等本地能力、L2 服务、额外服务商 | 核心收发门槛 | P5/P6 |
| F 工程交付 | 构建、测试工具、诊断、个人安装包 | 业务实现 | P0/P7 |

**测试用例扩写** 横跨 A–C，按 [TESTING_STRATEGY §5](TESTING_STRATEGY.md) 的 T1–T10 顺序在各阶段推进，不单列为一条串行工作线。

阶段编号用于保持原文档关联，不表示严格串行：Android PoC、威胁验证与协议探测可提前。

## 2. 当前仓库事实

- [x] Git 仓库、Tauri/React 脚手架、Cargo workspace、xremail-core。
- [x] 文档与 CI workflow 文件；测试策略/AI 工作规范已定义。
- [x] `xremail-core` 错误分类单测 4 个（2026-10-20）。
- [x] `xremail-store`：迁移 + 账户/文件夹 DAO + 身份约束（2026-10-22）；T1 用例 30 个。
- [x] `xremail-crypto`：argon2id + ChaCha20-Poly1305 版本化密文封装（2026-10-22）；T3 用例 40 个。
- [x] `xremail-api`：SecretStore / VaultStore / SyncBackend 契约 + 内存 mock（2026-10-22）；T4 用例 25 个。
- [x] `xremail-sync`：UID 差分 / epoch 切换 / bit 意图合并三个纯函数模块（2026-10-22）；T2 用例 57 个。
  - 2026-10-23 升级为**带 IO 的同步引擎**：拉取链路（差分 → epoch 切换 → 落库）
    与回写链路（意图 → `UID STORE` → **回读确认**）
- [x] `xremail-protocol-imap`：IMAP 协议层 + 只读 probe + **安全命令层** `run_checked`（2026-10-22/23）。
  - **三家实测完成**：163 / QQ / 沃（各自的协议事实见 [PROTOCOLS.md](PROTOCOLS.md)）
  - `run_checked` 自己判定 `OK`/`NO`/`BAD`——async-imap 会**吞掉**服务器的拒绝（R15）
- [x] `xremail-message`：MIME 结构、charset 解码、RFC 5322 头（2026-10-23）；T5 用例 40 个。
  - **GB2312 / GBK / GB18030 三种都要支持**；30 封真实邮件解码零错误
- [x] `xremail-testkit`：假 IMAP 服务器 + 故障注入（2026-10-22）；T7 前置已就位。
  - 能复现 163 的「必须先发 `ID`」「认证前拒绝一切命令」等怪癖
- [x] `xremail-diagnostics`：诊断工具（字符集抽样，只输出编码名与计数）
- [x] `xremail-api`：新增应用服务 DTO `NewAccountRequest`（校验 + 转领域对象，14 个用例）。
- [x] shell：服务层 + 真实持久化链路 + React 账户列表（2026-10-22）；服务测试 10 + IPC 测试 8。
  - **实测账本**：Rust 用例 4 → **544**（新增 540 个）
- [x] **同步引擎接进应用**（2026-10-26）：UI 里能看到**真实邮箱**里的邮件
  - 服务层 `MessageService` / `SyncService`；Command 8 个；前端：账户 → 同步 → 文件夹 → 邮件 → 正文
  - 验收：三家真实账号各跑通一次（163 / QQ / 沃，6 个文件夹全部同步，正文解码零错误）
- [x] 凭据录入：授权码**能在应用里填**了，也**能落盘**（Windows 凭据管理器）；
  关掉应用不用重填（R19：Windows 已处理，其它平台仍需重填）。落盘的平台适配见 P4。
- [x] **平台钥匙串**（2026-10-26）：授权码存进 **Windows 凭据管理器**，数据库里只留不透明引用
  - `secrets/windows_store.rs`（`CredWriteW`/`CredReadW`/`CredDeleteW`），条目名 `xremail:account:<account_id>`
  - 非 Windows 是**桩**：不落盘、不静默降级；平台不支持时**如实报错**
  - 用例 6 个，**真跑** Windows 凭据管理器（存 → 读 → 改 → 删），不靠 mock
- [x] **写操作真正回写服务器**（2026-10-26）：标记已读 / 星标从「只改本地」变成 `UID STORE` 回写
  - 意图先落库（`queued`）→ 提交 → **回读确认** → 六态回报前端（`applied` / `state`）
  - 三家真实邮箱实测：`UID STORE` 全 OK、**回读确认生效**、重复发同一标记也回 OK（见 `docs/PROTOCOLS.md`）
  - epoch 变了 → 意图作废（`failed` / `epoch_changed`），不会把旧 UID 套到另一封信上
- [x] 调度器（AccountActor）：什么时候同步、同步哪些文件夹、重连退避
      （2026-11-03，`crates/xremail-sync/src/actor.rs`；**IDLE、应用层接线均已补**，见 P1/P2 小节）。
- [x] 收发链路（P3）：SMTP **提交路径**已写完并跑通真发信（2026-10-29，163 ↔ QQ 双向，见 P3 小节）。
- [x] **撰写面板**（2026-10-30）：能写一封信并发出去——收件人 / 抄送 / 主题 /
  **富文本**正文（自写 `contenteditable` 编辑器，**不引入新依赖**）、
  草稿保存 / 撤回 / 发送；地址与附件校验**在后端**（前端绕过也拦得住）。
  前端用例 6 个文件、后端用例覆盖草稿与发信结局（`sent` / `uncertain` / `failed` 三态分清）。
- [x] **本地全文搜索**（2026-10-30）：FTS5 + trigram + 触发器维护索引；
  远端 IMAP 搜索不可靠（163 只认 ALL），搜索落在本地缓存上。
  短词（< 3 字符）退化成 `LIKE` 子串扫描；`rebuild_index` / `check_index_consistency` 兜底。
  存储层 21 个用例、前端 1 个文件。
- [x] **HTML 正文能看也能搜**（2026-10-30）：纯函数消毒器（`xremail-core::html`），
  **在出后端前**完成——原始 HTML 不出后端（`MessageBodyView` 只带 `htmlSafe`）；
  HTML-only 的真实邮件（实测 163 大多如此）**从 HTML 派生纯文本**，写入时派生、
  启动时回填老数据，所以这类邮件**也搜得到**。
- [ ] 真实邮箱 **CI 验收**：现在的真实邮箱证据全部来自本机手工运行。
  - [x] **前置：不需要真凭据的预检**（2026-11-10，`scripts/precheck_real_mail.py`）
    - 以前这条路完全没有可离线检查的环节——凭据文件路径变了、段名写错、
      权限过松、两个账户配成同一家，都要等到**真发信那一步**才知道
    - 预检查的全是可离线判断的：文件在不在、权限是否 0600、有没有 `163` 与 `qq` 两段、
      `user`/`password` 是否非空、QQ 号码会不会被补成 `@qq.com`、两个账户是不是同一家
    - **不打印口令、不打印完整地址**（只留首字母 + 域名）
    - 四条失败路径都实测过（权限过松 / 缺段 / 文件不存在 / 两段相同）
    - 顺带暴露一个已知限制：用例**写死**从 `163` 与 `qq` 取账户，换组合要先改代码
  - [ ] **接到 CI**（还没做）：需要两个专用测试账户的授权码放进 GitHub Encrypted secrets，
        用 `workflow_dispatch` 手动触发（**不**进普通 PR CI）。完整方案见
        [TESTING_STRATEGY §6.1](TESTING_STRATEGY.md)。**卡在「凭据托管方式」这个决定上**

> **仓库已推送**（2026-10-26，代码批次 `975ffda`、文档批次 `f6f7406`），CI **四个 job 全绿**
> （含 Windows 凭据管理器的真实往返用例）。R17 已处理。

## 3. 分阶段门槛

### P0：可测试的最小地基

- [x] xremail-store：迁移、账户/文件夹 DAO、身份约束（`crates/xremail-store`，2026-10-22）
  - 迁移 `0001_init.sql`（10 张表）；身份不变量 `UNIQUE (account_id, folder_id, uidvalidity, uid)` 落到 schema
  - DAO：账户 upsert/list/get/delete，文件夹 upsert/list/get/delete
- [x] xremail-store：跨表事务封装 → **已完成**（`crates/xremail-store/src/dao/sync.rs` 的 `commit_folder_sync`）
- [x] xremail-api：SecretStore、VaultStore、SyncBackend 最小契约与可运行 mock；CAS 冲突测试（`crates/xremail-api`，2026-10-22）
  - `SyncBackend` 只提供原子的 `compare_and_swap`，刻意不提供「先查后写」组合（TOCTOU）
  - 双设备 / 三方 CAS 竞争：恰好一个赢家；另有用例证明 API 形状本身防住 TOCTOU
- [x] L2 schema 预留：`local_secret` 只存版本化密文、`label` / `rule` 预留稳定 ID 与 schema version（迁移已就位）
- [x] 最小密码封装测试（`xremail-crypto`：argon2id + ChaCha20-Poly1305，40 个用例）
  - 口令错误与密文篡改返回**同一个**错误（分开就成了 oracle）
  - 头与 nonce 作为 AAD：元数据被改也会被发现
  - `rekey()` 提升 KDF 成本，旧封装仍能解开；`to_bytes`/`parse` 自包含字节格式供 L2 搬运
- [x] Command → 服务 → SQLite → React 真实列表，不以硬编码空列表验收（2026-10-22）
  - `list_accounts` 改为真实查询；新增 `create_account` / `delete_account`
  - 启动时解析数据目录（桌面 `%APPDATA%/com.xremail.app`，Android app 内部存储）、开库、**应用迁移**；
    迁移失败则启动失败，不降级为空库
  - 服务层 `apps/shell/src-tauri/src/service/`：校验 → 落库；**不认识 Tauri**，
    发事件是 Command 的事（状态提交后才发）
  - 前端：账户列表 + 新增表单 + 删除；不引新依赖（不用 TanStack/Tailwind）
  - 事件作为**刷新提示**使用：收到 `account://added` 等事件后重新查询，不解析载荷
  - 服务商只作标记，**服务器地址由用户手填**——163/QQ/沃 的参数尚未实测（R01/R02），
    不能按品牌推定
  - IPC 层测试 8 个：锁死 camelCase 线格式、参数校验、增删的完整链路
- [x] 新库 / 迁移幂等重跑 / 事务整体回滚测试（**升级路径**已补：第二个迁移 `0002_operation_seq.sql` 已就位）
- [x] 跨表事务封装（`crates/xremail-store/src/dao/sync.rs`：`commit_folder_sync` 一个事务内完成
  清旧世代 → 写新邮件 → 删服务器已删的 → 更新游标）
- [x] 实际执行 fmt/clippy/test/typecheck/build（2026-10-26 本地全绿；
  **CI 四个 job 全绿**（Rust ubuntu / Rust windows / Frontend / Docs sanity），
  仓库已推送，见 ISSUES_AND_RISKS R17）
- [x] **测试用例扩写 T1–T5**：T1（迁移与事务）、T2（同步纯函数）、T3（密文封装）、
  T4（L2 契约）、T5（MIME / charset）均已完成；Rust 用例 4 → **687**。
  见 [TESTING_STRATEGY §5](TESTING_STRATEGY.md)。

**出口**：真实持久化链路与 mock 契约均有测试，明确哪些模块尚未创建。
*当前状态：T1–T5 全部完成，真实账户列表已打通（Rust 用例 4 → 687）。**P0 已出关。*
剩余：无（P0 清单已全部勾选；后续工作归 P1/P2）。*

### P1：国内邮箱只读接入

- [x] **163：只读 probe 实测**（2026-10-22，`crates/xremail-protocol-imap`）
  - 预认证：TLSv1.3、证书由公开 CA（DigiCert）签发、**系统根即可校验通过**
  - 认证前**拒绝一切 IMAP 命令**（连 `LOGOUT` 都回 `BAD command not support`）
  - **必须先发 `ID`（RFC 2971）**，否则 `EXAMINE` / `SELECT` 被回 `NO … Unsafe Login`
  - **无 IDLE / 无 CONDSTORE / 无 MOVE / 无 UIDNEXT** ⇒ 只能轮询 + 「快照+意图重放」
  - 邮箱名是 **modified UTF-7**，需解码（已实现 `xremail-protocol-imap::utf7`）
  - 详细记录见 [PROTOCOLS.md 163 章](PROTOCOLS.md)
- [x] **沃邮箱：只读 probe 实测**（2026-10-23）
  - `imap.wo.cn:993`，TLSv1.3，**授权码登录成功**
  - 服务器自称 `Coremail System IMap Server Ready(wo.cn[...])` ⇒ **与 163 同源的 Coremail 体系**
    （先前文档推测的 Coxremail 已被推翻）
  - **无 IDLE / 无 MOVE / 无 NAMESPACE / 无 CONDSTORE**；**有 UIDPLUS**；
    LIST **带** RFC 6154 角色属性；文件夹名是英文（`Sent Items` / `Drafts` / `Trash` / `Junk E-mail`）
  - **不需要先发 `ID`**（与 163 相反）⇒ `ID` 是 163 独有的要求
- [x] **QQ：只读 probe 实测**（2026-10-23）
  - `imap.qq.com:993`，TLSv1.3，授权码登录成功
  - **三家里唯一支持 IDLE**（服务器接受 `+ idling`），也**有 MOVE**、有 NAMESPACE
  - **无 CONDSTORE / 无 QRESYNC** ⇒ 同样只能走「快照+意图重放」
  - **LIST 不给 RFC 6154 角色属性**（只给 `\NoSelect` / `\HasChildren`）⇒
    文件夹角色**必须**按名字兼底（已实现并有单测）
  - **UIDVALIDITY 是大数**（1792993698）而 UIDNEXT 很小（105）⇒ epoch 切换是常态路径
- [x] 凭据适配（`%USERPROFILE%\.xremail\accounts.txt`，**仓库之外**；支持 `domain =`；QQ 只写 QQ 号时自动补 `@qq.com`）
- [x] MIME / 中文字符集：实测三家 charset 并实现 `crates/xremail-message`
  - **GB2312 / GBK / GB18030 三种都要支持**（QQ 用 gb2312 *和* gb18030，沃用 gbk）
  - **只看顶层 `Content-Type` 会看错**——三家顶层几乎都是 multipart，正文 charset 在子部分里
  - 30 封真实邮件解析，**解码零错误**
- [x] 按需正文拉取（`BODY.PEEK[]` → `message_body` 表；正文不随同步全量拉）
- [x] **测试用例扩写 T5**（MIME / charset 解析，含 GBK/GB18030 样本）——`xremail-message` 40 个用例
- [x] 假 IMAP 服务器（T7 前置）：先有假服务器，再上真服务器
  - [x] **已建** `crates/xremail-testkit`（2026-10-22）：可配置的假 IMAP 服务器
    （能力集、要求先发 ID、拒绝 EXAMINE、认证前拒绝一切命令、故障注入）+ 自签证书
  - [x] 协议层端到端测试 10 个（假服务器 × 真客户端）
  - [x] 补齐同步链路命令：UID FETCH / UID SEARCH / UID STORE（2026-10-23）
  - [x] 补齐：`APPEND`（发信存副本，2026-10-30 已做）
  - [x] 补齐：`COPY` / `MOVE` / `UID EXPUNGE` / `EXPUNGE`（2026-11-03）
    - 协议层 `crates/xremail-protocol-imap/src/transfer.rs`：自己发命令、自己读 tagged 响应、
      自己判定成败（async-imap 的 `uid_copy` / `uid_mv` 在 `NO` / `BAD` 时不报错，同 [`command`] 的坑）
    - 解析 `COPYUID`（RFC 4315）：源/目标 UID **按下标对齐**；目标文件夹的 UIDVALIDITY
      服务器**不给**，所以 `destination_uid_validity` 恒为 `None`（宁可空，不造假）
    - 端到端用例 9 个（`tests/transfer.rs`）：UID 映射、多封乱序对齐、目标不存在报错、
      空集合不报错、`MOVE` 不碰别人标删的信、`UID EXPUNGE` 只删指定、裸 `EXPUNGE` 删全部
    - 顺手修了 `append.rs` 的一个真 bug：`APPENDUID 123 5:7` 被错报成 6
      （`UidRange` 是**闭区间**，原代码多减了 1），发信副本的 UID 会全记错一位

**出口**：三种真实账户各有实测记录；没有账号或配置时保留阻塞，不宣称完成。
*当前状态（2026-10-26）：**三家均已只读实测**（各自的协议事实记在 [PROTOCOLS.md](PROTOCOLS.md)），
只读链路已在 crate 层打通并**接进应用**（UI 里能看到真实邮件）。
**P1 出关**——凭据落盘也已补上（2026-10-26，Windows 凭据管理器），
**P1 的出口条件已达成**（三种真实账户各有实测记录 + 只读链路接进应用 + 凭据可从应用录入并落盘）。*

### P2：可靠同步与安全阅读

- [x] UIDVALIDITY 事务切换、删除差分。
  - epoch 切换在**一个事务**内完成（清旧世代 → 写新邮件 → 删服务器已删的 → 更新游标）
  - 绑定旧 epoch 的待提交操作**隔离**（标记 `failed`），绝不套用到新 UID 上
  - 用例：`epoch_change_triggers_a_rebuild`、`operations_bound_to_the_old_epoch_are_quarantined`
- [x] 断线恢复（崩溃后重启对账）（2026-11-03）：`crates/xremail-sync/src/reconcile.rs`。
  - 目标不是「猜服务器做了什么」，而是**把没人负责的行标成没人负责**，
    交给下一轮回读确认去核对（`push_flags` / `push_transfers` 天然幂等）
  - **按 boot 归属回收**：`running` 只说明有人认领了，不说明认领者还活着。
    认领时写 `owner_boot`（迁移 `0006_operation_owner.sql`），对账只收
    `owner_boot IS NULL OR owner_boot <> 本次 boot` 的行
  - **先回收、后判过期**（顺序本身是语义）：反过来，一条属于上次进程、
    次数也已到顶的 `running` 会被 `expire_exhausted` 漏掉，永远卡在 `running`
  - **本进程认领的行也有出口**（2026-11-06，R32）：`crates/xremail-store/src/dao/operation.rs:570`
    的 `RUNNING_LEASE_MS`（15 分钟）+ `reclaim_stale_running`，超过租约没写终态的行
    收成 `uncertain / lease_expired`；它在 `expire_exhausted` **之前**调用
  - `uncertain` **绝不**改回 `queued` / `retryable`：那等于替服务器判断
    「上次没执行」。只有人的显式决定能改它——外加一条**时间出口**（2026-11-06，R29）：
    超过 `UNCERTAIN_STALE_MS`（72 小时，`crates/xremail-sync/src/reconcile.rs:57`）转
    `failed / stale_unresolved`，文案让用户去服务器上核对后再决定
  - 同时收掉：孤儿操作（文件夹已删 → `folder_missing`；移动目标已删 →
    `destination_missing`）、旧 epoch 操作（→ `epoch_changed`）
  - 幂等：重复执行都不动手（触发点两处：**应用启动**
    `apps/shell/src-tauri/src/lib.rs:235`，以及 **每轮同步成功后**
    `crates/xremail-sync/src/actor.rs:865 reconcile_after_tick`，2026-11-06 随 R33 落地；
    actor 的 `Startup` 轮本身不单独对账）
  - 用例：`tests/crash_recovery.rs` 11 个
- [x] **账户调度器**（2026-11-03）：`crates/xremail-sync/src/actor.rs`。
  - 一个账户一个 actor，彼此隔离（一个账户的服务器卡住不拖住另一个）
  - 调度后立即先同步一轮 → 按账户自己的 `poll_interval_secs` 轮询；
    `Scheduler::wake` 插队（用于 UI 的「刷新」按钮）
  - 失败退避：`min_backoff` 起翻倍、封顶 `max_backoff`，成功即重置
  - **R13 的 panic 边界就在这一层**：每轮同步包 `catch_unwind`，
    panic 只停在那一轮；连续 3 次才放弃账户
  - 用例：13 个（5 单测 + 8 端到端，含「一个账户 panic 不影响另一个」）
  - **应用层接线已完成（2026-11-03）**：`apps/shell/src-tauri/src/lib.rs` 启动时
    列出账户，**只给凭据可用的**起 actor（没存授权码的账户起来只能一直失败）；
    `RunEvent::Exit` 时 `Scheduler::shutdown`。授权码是**后来才保存**的账户
    由 `cmd::sync_account` 补一次 `start`（`cmd::ensure_scheduled` 先问
    `Scheduler::is_running`，已在跑不叫醒）；
    `cmd::delete_account` 删完先 `stop` 再忘掉凭据。
  - 前端订阅 `sync://state-changed`，只在 `state === "online"` 时重查——
    每轮会报 `connecting` 和 `online` 两次，全刷等于白拉一倍数据。
- [x] **IDLE 状态切换**（2026-11-03）；其他文件夹调度仍未做。
  - 落点：`crates/xremail-protocol-imap/src/idle.rs`（`idle_once` / `IdleConfig` / `IdleEvent`）
    + `crates/xremail-sync/src/actor.rs`（`wait_for_change`）；actor 每轮先问
    `Accounts` 能力（`idle_supported` 缓存在 actor 里），支持就走长连接。
  - 协议细节：收到 `+ idling` 才开始等推送；**一到推送立刻发 `DONE`**（不只等 30 分钟
    定时器），否则服务器推的下一条要干等一轮超时；等 tagged `OK` 时**必须匹配
    进 IDLE 那条命令的原 tag**（`DONE` 本身不带 tag）。
  - **安全出口**：无论正常返回、超时、断线还是 panic，都先 `is_authenticated()`
    校验——`DONE` 三次握手没走完的会话**弃用重连**，不还回连接池。
  - 降级：IDLE 命令被回 `NO`/`BAD` 或能力探测指不支持 → 当轮改走轮询，
    并把 `idle_supported` 记为 `false`，下轮不再重试。
  - 用例：`crates/xremail-sync/tests/scheduler.rs`（`push_wakes_the_actor_before_the_poll_interval`、
    `server_without_idle_still_polls_and_does_not_pay_an_extra_connection`）
    + `crates/xremail-protocol-imap/tests/idle_client.rs`（`DONE` tag 匹配、
    同一会话重发、半行拼接、超时夹到 29 分钟以内）+ testkit 侧 7 个。
  - **假服务器的三个局限（写客户端时要知道）**：① `push_*` 只发信号、**不改 fixture**，
    断言邮件数量时不能把推送当成真的增删信；② `EXPUNGE` 的 `seq` **不会**像真服务器
    那样在前一封被删后前移；③ 离开 IDLE（含断线）后登记被摘掉、**不补发**——
    所以「推送丢 → 轮询兜底」永远不能省。
  - 已具备条件：三家能力已实测（QQ 有 IDLE，163/沃 无），
    能力判定逻辑在 `xremail-protocol-imap::extension_support`
- [x] flag bit 级意图、**回读确认**与 `uncertain` 状态。
  - 「读快照 → 重放意图 → **回读确认**」（ARCHITECTURE §6）
  - 三种失败分清：服务器回 `NO` → `retryable`；`NO [MODIFIED]` → `retryable`（有界重算）；
    连接断了 → **`uncertain`**（不知道服务器执行了没，**不能盲目重试**）
  - 只发**变化的位**，不整组替换 `FLAGS`
  - 用例：11 个（含「服务器回 OK 却不真改 → 回读发现」）
- [x] 移动 / 删除的保守策略（2026-11-03）
  - 落点：`xremail-sync::transfer_push`（`pub use transfer_push::{TransferPushReport, push_transfers}`）；
    路由拆成纯函数 `route_for`，不连服务器就能测；
    服务层 `MessageService::move_intent` / `delete_intent`；命令 `move_messages` / `delete_messages`
  - **删除 = 进垃圾箱**：有垃圾箱就挪进去（可撤销），没有才就地标删。
    垃圾箱**按角色认，不按名字猜**（2026-11-09）：`LIST` 认出的 RFC 6154 角色
    落进 `folder.role`（迁移 `0010_folder_role.sql`），删除时按
    「库里的 `trash` 角色 → role 为 `NULL` 时按文件夹名兜底 → 都不行就地标删」
    降级。`NULL` 的含义是「**还不知道**」，所以老库升级后还没同步过的账户
    仍然靠名字兜底，删除不会突然变成不可撤销的；
    服务器宣告 `\Trash` 但文件夹叫 `Rubbish` 之类时也能进对的垃圾箱
  - 源文件夹里**已经没有这封邮件** → 算成功（`MOVE` 不幂等，重发会多出一份）
  - 队列在同步轮次里**自动排空**（`flush_pending`），不需要界面开着
  - 用例：33 个（`crates/xremail-sync/tests/push_transfers.rs` 14 个端到端——
    其中 6 个是 2026-11-09 补的垃圾箱判定：`\Trash` 但名字叫 `Rubbish`、
    无属性且名字不像、role 为 `NULL` 时名字兜底、宣告为 `\Archive` 的 `Trash` 不被误用、
    同步后角色真的落库；`apps/shell/src-tauri/tests/move_delete_intent.rs` 8 个服务层；
    `transfer_push.rs` 里的 `route_for` 单测 8 个 + `trash_candidates` 5 个）
  - 前置已就位（2026-11-03）：协议层 `COPY` / `MOVE` 与假服务器都已支持
  - 保守路径：有 `MOVE` 就一步 `UID MOVE`；没有则 `UID COPY` → `UID STORE +FLAGS (\Deleted)`
    → **有 `UIDPLUS` 才** `UID EXPUNGE`（163 有 `UIDPLUS` 但没有 `MOVE`，走的是这一行）
  - 裸 `EXPUNGE` **不能**用：它会把这个文件夹里所有带 `\Deleted` 的信都删掉，
    包括别的客户端标了还没提交的（那也是用户的数据）
  - **重发不会多出副本（2026-11-03 修）**：`COPY` 不幂等，而 `uncertain` 原先
    **不自增 `attempts`**，`claim_for_push` 的 `AND attempts < ?` 对它永远成立
    → 一条发不出去的 `copy+delete` 每轮重发一次。现已：`uncertain` 也计次
    （`mark_operation_uncertain`）；重发前先用 `Message-ID` 探目标文件夹，探到就
    跳过 `COPY`；探不了就**不重发**。见 [ARCHITECTURE §4.5](ARCHITECTURE.md)
- [x] 按需正文（`BODY.PEEK[]` → `message_body` 表）；头随拉取一起落库。
- [x] 附件下载（2026-11-06，B2）：按需下载 + 64 KiB 流式落盘、sha256 命名、重复下载走缓存、内联图 `cid:` → `data:`。**离线全量缓存状态仍未做**（见 R09）。
- [x] **HTML 默认禁网且不可访问宿主**（R06，2026-10-30）。
  - **清洗在后端完成**（`xremail-core::html::sanitize_html`）：标签 + 属性白名单，
    删 `script` / `style` / `iframe` / `on*` / `javascript:`，
    删**远程图片**（跟踪像素与信息泄露的通道）；
    未知标签**只删标签、留着内容**（真实邮件正文都被 `html` / `body` 包着）
  - **原始 HTML 不出后端**：`MessageBodyView` 只带 `htmlSafe`
  - 前端只渲染 `htmlSafe`；正文里的链接**不让应用自己跳**
    （否则整个应用窗口会被那个网页顶掉），一律交给系统浏览器
  - CSP 也挡了一层：`img-src` 里没有 http/https
  - 用例：core 单测 + 存储层（派生正文）+ shell 层（`MessageBodyView` 序列化里没有 `htmlRaw`）
- [x] FTS 投影、重建与中文短词策略（2026-10-30）。
  - 投影 `message_search_doc` + FTS5 外部内容表（`tokenize='trigram'`）；
    索引由**触发器**维护（epoch 切换、级联删除这些路径也绕不过触发器）
  - 短词（< 3 字符）退化成 `LIKE` **子串**扫描；查询词按字面匹配（`OR` 不是语法）
  - `rebuild_index`（可重复）+ `check_index_consistency`（查漏索引 / 孤儿行 / 正文过期）
- [x] **测试用例扩写 T6–T8 的剩余部分**（T6 与 T7 已完成，见 [TESTING_STRATEGY](TESTING_STRATEGY.md)）。
  原先卡的两项里，**乱序响应已于 2026-11-09 补上**（见下）；剩下的**完备的 OWASP 样本集**
  归 T6，单独写在 T6 那一行里，不再挡住这一条。
  - T7 的**假服务器已就位**（含故障注入）：IMAP 侧覆盖拉取与回写，
    **SMTP 侧覆盖提交与能力协商**（`xremail-testkit`）
  - **T6（HTML 攻击样本）已补上**（2026-10-30）：core 的消毒器单测
    （`javascript:` 协议、`data:` URL、事件属性、`style` 里的 `url(javascript:)`、
    整封邮件的 `html` / `body` 壳）与 shell 层的「原始 HTML 不出后端」断言
    （**完备的 OWASP 样本集仍缺**）
  - T8（故障注入矩阵）：断线 / 超时 / 丢应答已覆盖（→ `uncertain`）；
    **乱序响应已覆盖**（2026-11-09）：假服务器新增 `Fault::InterleaveUntagged`
    （把一条别的 untagged 响应插在命令自己的**数据**与 **tagged 完成响应**之间，
    可限定命令、可只插一次，插入的响应合法且**不改变服务器状态**），
    `crates/xremail-protocol-imap/tests/interleaved.rs` 11 条端到端用例证明
    `run_checked` / transfer 两条读循环都只认自己的 tag。**未覆盖**：真服务商
    按随机时机插队的样本、`IDLE` 期间的穿插（那本来就是 `IDLE` 的正常语义）

**出口**：两客户端并发、崩溃恢复、恶意邮件与检索一致性测试通过；不承诺任意网络下固定收敛秒数。
*当前状态（2026-11-03）：同步引擎的**拉取**与**回写**（标记 + 移动/删除）两条链路已在 crate 层完成并有端到端测试；
**HTML 安全门与 FTS 已落地**（2026-10-30），**调度器与后台同步已落地并接进应用**（2026-11-03，IDLE + 轮询 + 退避 + 启动/退出接线）；
仍缺断线恢复。*

### P3：发送与草稿

- [x] **实测三家的 SMTP 参数**（R18）：服务器地址、`STARTTLS` 语义、`8BITMIME`、
  授权码认证、发信后的 `APPEND` 行为。
  - **163 / QQ 已实测完毕**（2026-10-29，**真发了信**）：参数见上，
    并探明「**两家都会自己存副本**，但 163 立刻可见、QQ 要 30–90 秒才落盘」——
    所以副本策略要**自适应**（见 P3 小节）
  - **沃仍发不了**（服务器只收静态 RSA 套件，rustls 连不上）→ 需换 TLS 后端
  - **126 本机无账号**（未测）
  - 预认证阶段已测过 163 的 SMTP 问候与 `EHLO` 能力（无 SIZE / 无 PIPELINUX，有 8BITMIME 与 STARTTLS），
    但**没有发过一封信**
  - [x] **只读探测阶段已实测**（2026-10-28，工具 \`xremail-smtp-probe\`，用本项目自己的 Rust 客户端）：
    - **163** \`smtp.163.com:465\`：TLS 1.3，证书校验通过，\`AUTH LOGIN\` 认证成功（\`235\`）；
      有 \`8BITMIME\`、**无** \`SMTPUTF8\`。
    - **QQ** \`smtp.qq.com:465\`：TLS 1.2，证书校验通过，\`AUTH LOGIN\` 认证成功（\`235\`）；
      有 \`8BITMIME\` / \`SMTPUTF8\` / \`PIPELINING\`，\`SIZE 73400320\`。
    - **沃** \`smtp.wo.cn:465\`：❌ **连不上**——服务器只接受**静态 RSA** 密钥交换套件
      （\`openssl s_client\` 协商出 \`TLS_RSA_WITH_AES_128_GCM_SHA256\` 可证），
      而 rustls 只支持 ECDHE，**换 TLS 版本无效**。⇒ 沃的发信需要另一个 TLS 后端，
      P3 首发只做 163 / QQ。
    - **126**：本机凭据文件里没有 126 账号，**待用户补授权码**后再测。
    - 原始报告见 [PROTOCOLS.md「SMTP 实测」](PROTOCOLS.md#smtp-实测2026-10-28-起)。
  - 副本的自适应逻辑**已补上**（2026-10-30）：协议层 `APPEND` + 「先查、没有才补」
    + 拒绝即「已有一份」的兼保险。
- [x] SMTP 提交、草稿、附件与签名；服务器已发送副本策略先实测，避免重复 APPEND。
  - [x] **提交路径 + 草稿 + 附件**（2026-10-28）：协议层 `submit_message`
        （MAIL FROM / RCPT TO / DATA / QUIT + 分阶段结果）、存储层 `send_queue` 表与 DAO、
        服务层 `ComposeService`（落草稿 → **冻结字节** → 抢占 → 提交 → 记结局 → 回读）、
        前端写信面板（800ms 防抖自动存草稿）
  - [x] **DKIM 签名**（2026-11-06，D1，见 `crates/xremail-dkim/`）；域签名 / ARC 与 Ed25519 未做
  - [x] **已发送副本行为已实测**（2026-10-29 真发信，2026-10-30 修正）：
        **两家服务器都会自己存**——163 提交后几乎立刻可见，QQ 要 **30–90 秒**才落盘
        （第一次实测查得太早，误记成「QQ 不存」）；QQ 还会**改写 `Message-ID`**
        （原值挪到 `X-OQ-MSGID`）
  - [x] **协议层 `APPEND` + 自适应存副本**（2026-10-30）：
        `crates/xremail-protocol-imap/src/append.rs`（命令编码 + 逐行解析字面量应答）、
        服务层 `apps/shell/src-tauri/src/service/sent_copy.rs`（先查 2.5 秒 →
        找不到才补 → 被拒且说「已存过」就当已有一份）
        - 匹配按**头值里的尖括号段**比（QQ 改写了 `Message-ID`）
        - 用例：协议层 4 条、服务层 4 条 + 单元 6 条、真邮箱往返断言**正好一份**
- [x] envelope recipients 与显示头分离，Bcc 不泄漏且不破坏 To/Cc。
  - Bcc **只进信封**（`RCPT TO`），不进邮件头；信封收件人 = To ∪ Cc ∪ Bcc 去重
  - 用例：`bcc_recipient_is_only_in_the_envelope`
- [x] DATA 最终应答丢失测试与用户可见 uncertain 状态；不盲重发。
  - 状态机已接上发信路径：`SendState::Uncertain` **不可自动重试**（服务器可能已收下它）
  - 前端 `SendResult.state` 三态分开显示，`uncertain` 时**禁用发送按钮**
  - 用例：`missing_server_reply_is_uncertain_not_retryable`
- [x] **测试用例扩写 T8 续**（SMTP 提交阶段分类、重复投递防护）。
  - 提交阶段分类：`NotSent`（前置失败，可重试）/ `Rejected`（服务器拒收）/ `Uncertain`（DATA 后丢应答）
  - 重复投递防护：不确定的信不自动重发 + 冻结字节后不可改内容
  - 用例：服务层 8 条、存储层 18 条、IPC 2 条、协议层 12 条（**不依赖真实邮箱**）

**出口**：专用测试账户发信成功与失败恢复都有证据，收件人/附件/Bcc 正确。

### P4：Windows 与 Android 平台验收

- [x] **托盘图标**与**退出/恢复语义**（2026-11-09，`apps/shell/src-tauri/src/tray.rs`）
  - **语义**：关闭主窗口 = **退到托盘**（窗口隐藏，进程与后台同步继续跑）；
        托盘菜单「退出」= **真退出**（`app.exit(0)` → `RunEvent::Exit` →
        `Scheduler::shutdown()`）；托盘左键单击 = 显示并聚焦主窗口
  - 托盘菜单：**显示主窗口** / **立即同步** / **退出**。前者与单实例插件的
        「第二次启动」走同一个入口；「立即同步」对所有**已调度**账户逐个
        `Scheduler::wake`（新增 `service::scheduler::wake_all`，不复用 `start`，
        因为它的语义是「确保在调度」）；「退出」用 `app.exit` 而不是
        `std::process::exit`——后者不会发 `RunEvent::Exit`，那段收尾就成了死代码
  - 只开 tauri 自己的 `tray-icon` feature（**没有**新增第三方依赖，`Cargo.lock` 未变）；
        托盘只在桌面端创建（`#[cfg(desktop)]`），Android 上没有；图标取
        `app.default_window_icon()`，即 `bundle.icon` 里那张 32x32
  - **开机自启时不特殊处理**（仍然弹窗口）：能做，但 `setup` 里窗口默认已显示，
        「按参数再关掉」一旦参数解析出错就是「点了没反应」；自启的诉求
        （登录后后台同步已在跑）当前实现已满足
  - **实测到什么程度**：Linux 上（无显示器）编译通过、`cargo clippy -p xremail-shell
        --all-targets -D warnings` 零警告、`cargo fmt` 干净；菜单 id → 动作的映射
        有 4 个单元用例。**Windows 上的实际行为（图标出不出、左键是否聚焦、
        退出时 actor 是否真的被停掉）没有真机验证**——本机跑不了 GUI，
        Linux 上 `TrayIconEvent` 又根本不发（libayatana-appindicator 只给菜单），
        所以「左键显示窗口」在 Linux 上不生效，要用菜单里的「显示主窗口」
  - 通知（`tauri_plugin_notification`）、单实例（`tauri_plugin_single_instance`，
        第二次启动聚焦已有窗口）、开机自启（`tauri_plugin_autostart`）**早已接线**
        （2026-11-03 核实），这次没有改动它们
- [x] **安全存储**（2026-10-26）：Windows 凭据管理器适配已落地（`secrets/windows_store.rs`），
      不静默降级。
- [ ] **早期可并行** Android PoC：构建、Rust 调用、Keystore 适配路线、前台/后台/息屏/进程恢复。
  - **已完成的是「构建」这一格**（2026-11-09，见下面的「Android PoC 第一轮」）：
        工程能生成、APK 能产出、全部 Rust crate 在 `aarch64-linux-android` 上编过、
        托盘确被排除、**平台钥匙串在 Android 上是桩**这条也在二进制里坐实
  - **仍未完成的**：前台/后台/息屏/进程恢复、通知、Keystore 适配本身——
        **一条都没有在 Android 设备或模拟器上跑过**，本机没有真机也没有 AVD
        （R08 仍是「待验证」）
- [ ] 根据 PoC 决定移动端前台同步与后台策略；必要时请求用户选择时效/功耗取舍。
  - **本轮不决策，也不该决策**：判断「前台同步够不够、要不要后台常驻」所需的证据
        （息屏后进程活多久、Doze 何时冻结、通知是否送达）恰好全是本节唯一验不了的那一类。
        现在能定的只有下文的**路线**，不是结论
- [ ] Android 阅读/发信/本地缓存验收，背景任务限制如实展示。
  - 同上：这条要求的是**运行时**验收。本轮的证据最多落到「数据目录与 SQLite 在 Android
        目标上能链接成可加载的 `.so`」，**打不开库、读不到一封信、发不出一次 SMTP**——
        没有设备，这些都不是可以「推断」出来的

#### Android PoC 第一轮（2026-11-09）：只做到「能编出 APK」

> 本节只写**实测**。凡没跑过的一律写「未验证」，不写「应该可以」。

**环境**：Linux x86_64（Ubuntu 26.4）、rustc 1.99.1、Tauri CLI 2.12.3 / `tauri` 2.12.4、
JDK 17.0.20.1、NDK `27.2.12479018`、platform `android-36` / `android-37.0`、
Gradle 9.6.1（wrapper 自带）、cargo-ndk。**没有任何 Android 设备或模拟器**
（`adb devices` 无设备，本机未安装 emulator / system-image，见 R08）。

**怎么跑通的**（命令与结果，可复现）：

1. `npx tauri android init` → `Project generated successfully`，产物在
   `apps/shell/src-tauri/gen/android/`（Gradle 工程 + Kotlin 入口 + 图标资源 +
   `tauri.settings.gradle`，后者把 `tauri-2.12.4/mobile/android` 与
   `tauri-plugin-{dialog,notification,opener}` 的 android 工程 `include` 进来）。
   生成时用的是默认参数：ABI `arm64-v8a/armeabi-v7a/x86/x86_64`，
   `minSdk 24`、`compileSdk 37`、`targetSdk 37`。
2. `npx tauri android build --debug --target aarch64` → **产出 APK**：
   `apps/shell/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`
   （266,388,896 字节 ≈ 254 MiB；同一次构建还顺带产出 `.aab` 72,387,850 字节）。
   另外单独跑了 `npx tauri android build --target aarch64 --apk`（release）作为对照，
   得到 `…/apk/universal/release/app-universal-release-unsigned.apk`（**14,869,199 字节 ≈ 14.2 MiB**）。
   - **体积**：debug 那份大得吓人，但**这不是缺陷**——
     `lib/arm64-v8a/libxremail_shell_lib.so` 一个就占 258,301,456 字节，因为 debug 构建带
     完整调试信息，且模板的 `debug` 构建类型显式 `keepDebugSymbols` 四个 ABI
     （`app/build.gradle.kts:35-38`）。同一份代码 release 出来只有 ≈ 14.2 MiB，
     两份的差值基本就是调试符号——所以「Android 包大」在本仓库不成立，**别拿 debug 的体积下结论**。
     注意 release 那份是**未签名**的，`tauri android build` 不给它签名；要装得先配签名，
     配置怎么放见生成工程根目录的 `settings.gradle` / `app/build.gradle.kts` 与同级
     `key.properties`（后者已被生成工程的 `.gitignore` 忽略，**本仓库没有、也不该有**签名文件）。
   - 产物里 `lib/` 只有 `arm64-v8a` 一个 ABI（`--target aarch64` 的结果），
     `.so` 由 `file` 证实是 `ELF 64-bit LSB ... ARM aarch64, for Android 24, built by NDK r27c`。
   - **前端确实在里面**：`dist` 是内嵌资源，打进了 `.so` 而不是 APK 的 `assets/`——
     在 `.so` 里能按名字找到本次 `dist` 产出的那两个文件名：`index-BA6F1NNh.css` 与 `index-LTbQfC79.js`
     （文件名带内容哈希，**下次前端改动后会变**，别拿这两个名字当常量去 `grep`）。
     （`assets/` 目录下只有一份 `tauri.conf.json`，别据此以为「前端没打进去」。）
   - **签名**：debug 构建用 AGP 自动生成的 debug keystore；本仓库**没有**提交任何签名文件，
     产出的 APK 只能装到个人设备上调试，不能分发。
3. **Rust 侧编过了哪些东西**（`aarch64-linux-android` 目标）：
   - 本仓库 **9 个 crate 全部编过**：`xremail-core` / `xremail-store` / `xremail-api` /
     `xremail-sync` / `xremail-protocol-imap` / `xremail-protocol-smtp` /
     `xremail-message` / `xremail-dkim` / `xremail-shell`
     （`xremail-crypto` 与 `xremail-testkit` 不在 shell 的依赖图里，本轮没有单独编译它们）
   - 关键的第三方：**`sqlx` / `sqlx-sqlite` / `libsqlite3-sys` 编过了**
     （`sqlite-bundled` 自带的 C 源码走 NDK 的 clang，是本轮最容易炸的一环，它没炸）；
     `ring`（DKIM 签名）、`async-imap`、`tokio` / `tokio-rustls`、
     `tauri` / `wry` / `tao` 与 `tauri-plugin-{dialog,notification,opener,single-instance,autostart}`
     也都编过了
   - **但「编过」≠「能跑」**：`libxremail_shell_lib.so` 能被链接出来、能放进 APK，
     没人加载过它
4. **Rust 调用链路的静态结论**（看代码 + 看打出来的二进制，**不是**运行结论）：
   - **数据目录**：`init_storage()` 用 `app.path().app_data_dir()`（`apps/shell/src-tauri/src/lib.rs:189`），
     Tauri 在 Android 上解析为 app 内部存储（`/data/user/0/com.xremail.app/...`）。
     `com.xremail.app` 这个包名在生成工程里坐实了（`aapt2 dump badging` 的 `package:` 行）。
     **运行时是否真的创建成功、SQLite 是否真的落在那里——未验证**。
   - **SQLite**：`sqlx` 的 `sqlite-bundled` 在 Android 上**编得过、链得过**；
     `llvm-nm` 在打进 APK 的 `.so` 里能找到 `sqlite3_open` 一类符号（3 处）。
     迁移与 FTS5 在那个目标上**是否行为一致未验证**。
   - **平台钥匙串在 Android 上是桩（这是硬事实，不是推断）**：
     `apps/shell/src-tauri/src/secrets/mod.rs` 的 `platform_default_store()` 在
     `#[cfg(not(windows))]` 分支上返回 `unsupported::UnsupportedSecretStore`
     ——`set` 恒返回 `Unavailable`、`get` 恒 `None`、`delete` 恒 `false`。
     这条在**打进 APK 的那个 `.so` 里**被核实：`UnsupportedSecretStore` 的错误文案
     「只有 Windows 凭据管理器已适配」在里面出现 1 次，而 Windows 实现的符号
     （`windows_store` / `CredWriteW`）**一个都没有**。
     直接后果（就是 R19 的 Android 侧）：
     **Android 上授权码存不进钥匙串 → 每次冷启动都要重新输入**；
     同期 DKIM 私钥也存不进去（它同样走 `xremail:dkim:<account_id>` 这条钥匙串路径）。
     这是**如实报错**，不是静默降级成「存到文件里」——那条路本仓库刻意不走（R07）。
   - **托盘被正确排除**：`tray` 模块在 `lib.rs:25-26` 是 `#[cfg(desktop)]`，
     打进 APK 的 `.so` 里 `xremail_shell_lib::tray` 的符号**数量为 0**。
     所以 Android 上不存在「关窗口退到托盘」——**关窗口是什么语义，未验证**。
   - **后台同步没有任何 Android 生命周期接线**：`lib.rs` 只处理了
     `RunEvent::Exit`（停调度器），**没有** `RunEvent::Resumed` / `Suspended`
     或 `WindowEvent::Resumed` / `Suspended` 的分支（全仓 `grep` 无命中）。
     Tauri 2.12 确实会把这些事件转发到 Rust（`tauri-2.12.4/src/app.rs` 的
     `RunEvent::Resumed` 与 `WindowEvent::Resumed/Suspended`，后者由
     `WryActivity.kt` 的 `onPause`/`onResume` → `Rust.onPause/onResume` 驱动），
     **我们只是还没用**。于是当前形态是：`Scheduler` 在进程活着时照常跑，
     进程被系统杀掉就什么都没了（下一轮要从「打开应用」开始）。
     **息屏/后台时进程能活多久、Doze 何时冻结 socket，一个字都没有测过**。
   - **通知**：`tauri-plugin-notification` 的 android 工程进了构建，
     其 manifest 带来 `POST_NOTIFICATIONS` / `WAKE_LOCK` / `RECEIVE_BOOT_COMPLETED`
     三个权限（`aapt2` 在 APK 里都看得到），并注册了
     `TimedNotificationPublisher` / `LocalNotificationRestoreReceiver` 等接收器。
     **这一段在 2026-11-09 之后已经不准了**：当时写的是「应用自己从未调用过通知 API」，
     但同一天 P5-L 的新邮件通知已经落地（`apps/shell/src-tauri/src/notify.rs`，
     触发点是 `TauriSink` 收到 `AccountEvent::Synced`）。所以现在的情况是：
     - 桌面端：通知**代码存在且判据有单测**，但**在真实桌面会话里是否真的弹出未验证**
       （本机连 `notify-send` 都被沙箱的 AppArmor 只读限制挡住）
     - Android：`notify_new_mail` 会调 `request_permission()`，但
       **Android 13+ 的运行时 `POST_NOTIFICATIONS` 权限在真机上能不能申请到、弹出来长什么样，仍然未验证**
       ——这一段**只有真机/AVD 才能回答**，不要因为「代码写了」就当成验过
5. **为跑通构建改了什么**：**Android 工程与 `tauri.conf.json` 一个字都没改**，
   `AndroidManifest.xml` 与 `build.gradle.kts` 保持 `init` 生成的原样。
   卡住的是**沙箱环境**（`$HOME` 只读），不是 Tauri 2.12 的配置：
   - Gradle wrapper 连自己的锁文件都建不了：
     `java.io.FileNotFoundException: /home/xl/.gradle/wrapper/dists/gradle-9.6.1-bin/…/gradle-9.6.1-bin.zip.lck (只读文件系统)`
     → 用 `GRADLE_USER_HOME` 指到工作区内（`<工作区>/.devcache/gradle-home`）
   - AGP 建 debug keystore 时同样撞只读：
     `java.io.IOException: Unable to create debug keystore in /home/xl/.android because it is not writable.`
     → 用 `ANDROID_USER_HOME` / `ANDROID_SDK_HOME` 指到工作区内
   - 另显式设了 `CC_aarch64_linux_android` / `AR_aarch64_linux_android`，
     让 C 依赖（`ring`、`libsqlite3-sys`）用 NDK 的 clang 编——这两项在
     `tauri android build` 的默认行为下是否必需，**没有单独做对照实验**，不宣称
   - **`gen/` 整个目录被根 `.gitignore:22-23` 忽略**，构建产物（`app/build`、
     `.gradle`、`jniLibs`）不会出现在 `git status` 里；`cargo-ndk` 因此**不需要**
     也不应该把 `APP_ABI` 塞进 `tauri.conf.json`

**Android Keystore 适配路线**（**只是路线，没有实现任何一行**）：
- 落点在哪：新增 `apps/shell/src-tauri/src/secrets/android_store.rs`，实现
  `xremail_api::SecretStore` 的 `get` / `set` / `delete` 三个方法；
  `secrets/mod.rs` 的 `platform_default_store()` 加一个 `#[cfg(target_os = "android")]`
  分支指向它。**不改契约**（`unsupported.rs` 的模块注释里已经写死了这条）。
- 用哪个设施：**Android Keystore 里的 AES-GCM 密钥**（`KeyGenParameterSpec`，
  `PURPOSE_ENCRYPT | PURPOSE_DECRYPT`，`BLOCK_MODE_GCM`）加密每条凭据，
  **密文放进本模块自己的 Android SharedPreferences**（`MODE_PRIVATE`），
  **绝不放进 SQLite**——数据库文件会被备份/拷走，这条理由在 `secrets/mod.rs` 顶部写着。
  Android Keystore **不直接存任意长度数据**（也不是一个 KeyValue 存容器），
  所以「密钥在 Keystore、密文在别处」是唯一形状；把它们混成「Keystore 里存授权码」是错的。
- 为什么值得做：Keystore 的密钥不可导出；`minSdk 24` 已经有 `KeyGenParameterSpec`，不需要降版本。
  （StrongBox 在 API 28+ 才有、且不是每台机器都有，落地时要按 `isStrongBoxBacked()` 探测并回退。）
- 跨平台形状：这与将来 Linux 的 Secret Service、macOS 的 Keychain 是**同一种适配器**，
  不是特例——所以先在 R19 里把「哪些平台已有实现」列清楚再动手。
- 验收门槛（**必须真机**，模拟器只算一半）：冷启动后不重填授权码、进程被杀后仍在；
  删除账户后 SharedPreferences 里不留残骸（Keystore 里那把密钥是否随之删除，也要一并断言）；
  用户改锁屏密码/恢复出厂后的失败路径要**如实报错**而不是静默丢凭据。
  **以上一条都没做，也没有任何一条被验证过。**
- **什么时候做**：与「后台策略」一起决策，因为两者都要先有一个能跑的设备环境。

**这一轮到底验证到了什么程度**（照 R08 的复核门槛逐条对）：

| 项 | 本轮状态 | 还差什么 |
|---|---|---|
| 工程生成 / APK 产出 | **已验证**（本机可复现） | 无 |
| Rust 在 Android 目标上编译 | **已验证**（9 个 crate + sqlx/ring/tauri 全过） | 无（但没跑过） |
| 数据目录 / SQLite 打开 / 迁移 | **未验证** | 真机或 AVD 上启动一次，看 `app_data_dir` 与 `xremail.db` 是否落地 |
| 阅读 / 发信 / 本地缓存 | **未验证** | 同上，且要能连真实邮箱（163/QQ） |
| 前台 / 后台 / 息屏 / 进程恢复 | **未验证** | 真机；`adb` 观察进程优先级与 `dumpsys` 的 Doze 状态 |
| 通知 | **未验证**（2026-11-09 起代码会调 `request_permission()` 并发通知——见 `src/notify.rs`；但**真机上能不能拿到 Android 13+ 的运行时权限、通知长什么样，没验过**） | 真机 + Android 13+ 的运行时权限 |
| Keystore | **未实现**（当前是桩，已在二进制里坐实） | 先写适配器，再按上面的门槛验 |

**出口**：Windows 与 Android 分别记录能力矩阵；若 Android 未验收，只能称 Windows alpha，不能称双端 MVP。
当前未获授权取消 Android 或承诺始终后台实时同步。
> 2026-11-09 记：Android **仍未验收**，所以对外仍只能称 Windows alpha。
> 上表里「未验证」的六项，没有设备就推进不了——这是本轮最该被记住的一句话。

### P5：本地增值功能（按需求选择）

标签、规则、联系人、稍后处理逐项交付。每设备独立的事实必须在 UI 明示。
Gmail 将来有需求再开独立决策：授权流程、scope、个人使用与公开发布要求均重新核实，不预设 Device Code 可用。

### P5-L：Linux 支持（可选，不承诺）

**当前事实（2026-11-09 更新）**：Linux 编译早已被 CI 验证（ubuntu job 编译的是应用本体）；
**运行**这一轮也验了一部分——用 Xvfb 虚拟显示真跑了打包产物（`scripts/verify_linux_runtime.py`：
启动不 panic、建出数据库、10 条迁移全部成功、托盘图标真的渲染出来、deb 与 AppImage 都跑通）。
**但「真实桌面会话」仍未验**：Xvfb 里没有真实输入、没有面板承载托盘、
`notify-send` 也被沙箱的 AppArmor 只读限制挡住。详见 [ENVIRONMENT.md §2](ENVIRONMENT.md)。

若要支持 Linux，需额外完成：

- [x] **WebKitGTK 沙箱验证**（2026-11-09，`scripts/webkit_sandbox_probe.py`）
  - **验了什么**：在 `xvfb-run` 起的**真实 WebKitGTK 2.52.6**（`WebKit2.WebView`，GTK3）里，把
    `apps/shell/src-tauri/tauri.conf.json` 的 CSP **原文**放进页面、把**真消毒器**
    （`xremail-core::html::sanitize_html`，经 `rustc` 编成的小 CLI 调用）的输出与若干
    **绕过消毒器**的样本塞进 DOM，逐条断言运行时行为——远程图片零请求、内联脚本 / 事件属性 /
    `javascript:`（含点击）不执行、`data:` 图片能显示、sandbox iframe 碰不到父窗口、
    无 sandbox 的 `srcdoc` 读写得到父窗口、`frame-src` 拦远程 iframe、`file:` / `data:` 强导航被拦。
    明细与逐条观测值见 [TESTING_STRATEGY §2.4](TESTING_STRATEGY.md) 的「在 WebKitGTK 上的实测结果」
  - **结论**：本组用例 WebKitGTK **49/49 PASS**（CSP 走 `<meta>`；改成响应头下发再跑一遍同样 49/49 PASS）；
    同一组用例、同一份 CSP、同一份断言在 Playwright Chromium 上**逐条结论一致**
    （`--engine=both`：98/98 PASS，差异项 0）——**「行为与 Chromium 有差异」这句话在本组用例上不成立**，
    原句的写法是把推测当成了结论，已改
  - **顺带量到的一条（不是新漏洞，是既有设计的前提）**：CSP 管不到导航。`location.href` 强导航到远程
    http 页面会把主文档换掉（WebKitGTK 与 Chromium 都一样）；隔离完全靠 `MessageView.tsx` 对点击的
    `preventDefault` + `/^(https?:|mailto:)/i` 白名单。消毒器只保证 `href` 是 http(s)/mailto，
    **不保证用户点了一定不进 webview**——「点链接交给系统浏览器」这一环在宿主上的端到端行为**未验证**
  - **没验成的**：Tauri 的真实源（`tauri://localhost` / `http://tauri.localhost` + 自定义协议 + IPC/ACL）、
    macOS 的 WKWebView、有真实 GPU/合成器的 WebKitGTK（本机只有 Xvfb + 软件渲染）、
    原生 `confirm()`/`mailto:` 那类需要真人点击并交给系统处理器的路径（探针没有替身，没有断言）
  - **为什么不接 CI**：CI（ubuntu runner）上没有 WebKitGTK 的图形环境，接进去只会让 CI 变红；
    这是**本地手工验收工具**，动了消毒器 / CSP / 正文渲染之后在有桌面的 Linux 上手工跑一遍
- [x] **Secret Service 凭据后端**（2026-11-09，`apps/shell/src-tauri/src/secrets/linux_store.rs`）
  - 走 **D-Bus**（`keyring` 3.x + `sync-secret-service`），**不链 `libsecret`**：
    直接链会把可执行文件钉在某个发行版的 SONAME 上，而 D-Bus 只要求
    「有个实现了 `org.freedesktop.secrets` 的守护进程在跑」——GNOME Keyring、
    KWallet 都行。代价是新增两个 crate（`keyring` / `dbus-secret-service`，
    依赖树很浅），这条偏离了仓库「少拉 crate」的一般约束，**是权衡后接受的**
  - 契约与 Windows 实现逐条对齐：条目名原样用 `secret_ref`、机密存原始字节、
    「不存在」不是错误、错误信息不含机密
  - **实测到什么程度**：`tests/linux_secret_service.rs` 在**真实 D-Bus 会话**
    （`dbus-run-session`）里验了「没有守护进程时三个方法都明确报
    `Unavailable`」，并用**变异测试**确认「改成静默降级就立刻变红」；
    `credential_service.rs` 的跨平台用例改成三平台各自成立
  - **未验证**：**真钥匙串上的写→读→删往返**（这台机器没有
    `gnome-keyring-daemon`，也无法安装）。等桌面环境或装好守护进程后用
    `python3 scripts/verify_linux_keychain.py` 跑一遍即可（没有守护进程时它会**明说验不了**，
    不会假装跑过）
- [ ] 托盘（libayatana-appindicator）、通知（D-Bus）、开机自启（XDG autostart .desktop）
  - [x] **托盘在 Linux 上真的建出来了**（2026-11-09，Xvfb 实测）
    - 此前这条写的是「运行未验证」。实测后修正：**托盘本身是能工作的**，
      之所以一直看到「创建托盘图标失败」，是因为
      `libayatana-appindicator` 要把图标渲染成 PNG 写到
      **`$XDG_RUNTIME_DIR/tray-icon/`**，而这台机器的 `/run/user/1000`
      在沙箱下**只读**。给一个可写的 `XDG_RUNTIME_DIR` 之后：
      目录里出现 `tray-icon-main-0.png`（933 字节，`file` 确认是
      32×32 RGBA PNG），日志里不再有那条 warn
    - 已写进 `scripts/verify_linux_runtime.py` 作为断言（**变异测试**：
      把 `XDG_RUNTIME_DIR` 指回只读路径 → 脚本立刻报「托盘创建失败」）。
      deb 与 AppImage 两种产物都验过
    - **仍然成立的限制**：Linux 上 `TrayIconEvent` **不发**（libayatana-appindicator
      只给菜单，不给左键事件），所以「左键单击显示窗口」在 Linux **确实不生效**，
      要用菜单里的「显示主窗口」。这是平台能力，不是缺省实现
    - **未验证**：托盘的**外观与点击行为**（Xvfb 里没有真实输入，也没有
      面板/任务栏去承载它），以及「关窗口 → 退到托盘 → 从菜单叫回来」这条
      交互闭环
  - [x] **新邮件通知**（2026-11-09，`apps/shell/src-tauri/src/notify.rs`）
    - 之前 `tauri_plugin_notification` 只注册、**全仓没有一处调用**，应用从不发通知
    - 策略（产品取舍，写在模块文档里）：
      **只通知 `INBOX`**（别的文件夹有新信不该打断用户）；
      **前台不通知**（判据是 `is_focused()` 而不是 `is_visible()`——窗口被托盘藏起来时
      `visible == false`，那正是需要通知的时候）；
      **首次同步与 epoch 切换不通知**（那是全量重拉历史，一封都不是新的）；
      **聚合显示**（一轮 50 封只弹一条，标题写账号与封数，超过 3 封正文改成「还有 N 封」）
    - 触发点：`TauriSink` 收到 `AccountEvent::Synced` 且 `INBOX.added > 0`；
      查账户地址与发通知都在 `spawn` 出去的短任务里，**不阻塞** sink 链路
      （sink 卡住等于卡住那个账户的同步循环）
    - 判据都是**纯函数**（`inbox_added` / `compose_notice`），8 条单测覆盖：
      只认 INBOX、大小写不敏感、首次同步不通知、epoch 切换不通知、
      无新邮件不通知、聚合不逐封、多文件夹只数 INBOX
    - **未验证**：通知在真实桌面会话里**真的弹出来**
      （本机 `notify-send` 都发不出去：`org.freedesktop.DBus.Error.Failed:
      Failed to query AppArmor policy: Read-only file system`——沙箱限制，
      不是代码问题）。**点击通知聚焦窗口**也没验：那是插件事件，
      Xvfb 里点不了
    - **未做**：「点通知跳到那一封」——需要前端配合（切账户 → 文件夹 →
      选中邮件），是一条独立交互链路；现在点通知只保证**能回到应用**
  - [x] **开机自启有了开关**（2026-11-09）：托盘菜单里新增**勾选项**「开机自启」
    - 之前 `tauri_plugin_autostart` 只注册、**没有任何 UI 去开关它**——等于这个功能不存在
    - 做成**一个勾选项**而不是「打开/关闭」两项：后者会让菜单长得像状态指示器，
      用户点哪一个都得先自己判断当前是开还是关
    - 勾的**状态读系统本身**（`is_enabled()`），不在内存里记 bool：用户可以在
      启动器设置里、或 `systemctl --user disable` 把它关掉，自记的 bool 会和
      真实状态分叉，菜单上的勾就开始说谎
    - `is_enabled()` 读失败时**不切换**（猜错就会把用户已经打开的自启关掉）；
      只是让勾显示为未勾选（显示成「没开」最多让用户多点一下，比显示一个
      过期的「开了」诚实）
    - 菜单项句柄存进 `TrayMenuState<R>`（泛型，不写死 `Wry`——写死的话
      `MockRuntime` 下永远拿不到，这类「只在 mock 下静默失效」的分支最难发现）
    - **实测到什么程度**：`TrayAction::from_menu_id` 与 `wakes_sync` 的单测覆盖了
      新菜单项（切换自启**不该**顺带开一轮全网同步）；托盘在 Xvfb 下能建出来，
      启动时读 `is_enabled()` 不报错
    - **未验证**：点菜单项之后 `~/.config/autostart/*.desktop` **真的被写出来**
      （Xvfb 下点不了菜单；我试过用一个独立探针程序直接调插件，没编过，
      **没有把"应该能行"写成结论**）
- [x] **打包链路：deb 与 AppImage**（2026-11-09）
  - `cd apps/shell && npm run tauri build -- --bundles deb` → **7.7 MiB**，
    `--bundles appimage` → **87.2 MiB**（自带依赖；二进制本身 17.4 MiB）
  - deb 的依赖由 Tauri 生成：`libayatana-appindicator3-1` / `libwebkit2gtk-4.1-0` /
    `libgtk-3-0`。**`libgtk-3-0` 在本机（Ubuntu 24.04+）是 t64 变体**
    （`libgtk-3-0t64`，它 `Provides: libgtk-3-0`），`apt-get -s install` 解析通过，
    **不是**依赖缺陷——这一条是实测得出的，不是推断
  - **不只是「能打包」，还验了「装上去能跑」**：新增
    `scripts/verify_linux_runtime.py`，在 **Xvfb 虚拟显示**里真跑打包产物，
    断言：`ldd` 无 `not found`、启动不 panic、建出 `xremail.db`、
    `_sqlx_migrations` 的条数与 `migrations/*.sql` 一致且全部 `success`、
    关键表齐全。deb 与 AppImage 两种格式都跑通（10 条迁移 / 20 张表）
  - 脚本本身做了变异测试：把期望迁移数改错，它会立刻报「对不上」
  - **未做**：**rpm**（本机没有 `rpmbuild`）；`tauri build` 不带 `--bundles`
    时 `targets: "all"` 在 Linux 上会同时试 deb 与 appimage，两者都快，可以照常跑
  - **未验证**：在**真正的桌面会话**里双击启动（Xvfb 下跑得起来，托盘也能建出来——
    见 P5-L 托盘那条；缺的是真实输入与面板承载）；以及 Fedora 等非 Debian 系发行版
- [ ] 至少 2 个主流发行版的真机运行验证（Ubuntu / Fedora 系）

> 优先级低于 P4 的 Windows/Android 验收。**在用户明确提出需求前不投入。**

### P6：可选 L2 加密同步

先做威胁模型、账户认证、CAS、密钥恢复、回滚保护，再实现服务与双设备测试。凭据同步默认关闭，不能因已有 trait 宣称无需返工。

### P7：个人安装与长期维护

可在 Windows alpha 后提前做个人安装包、备份/恢复、迁移和排错文档。不把公开上架、商业签名、社区增长当作当前前提。
额外平台与公开分发有需求时再确定验收范围。

## 4. 下一批任务（不附虚构工期）

按依赖顺序排列；**每完成一个门槛再选下一批**，不把建议当作用户授权。

1. ~~**把同步引擎接进应用**（P1 出口条件的最后一节）。~~ **已完成（2026-10-26）**。
   - 服务层：文件夹与邮件列表的查询；Command 接线；前端加一个邮件列表
   - 前提已全部具备：crate 层的拉取/回写链路已通，`xremail-store` 有 message / message_body DAO
   - 验收：UI 里能看到**真实邮箱**里的邮件（中文标题、正文）——**已达**（见下）
2. ~~**凭据录入**~~ **已完成（2026-10-26）**：授权码进应用，并**落平台钥匙串**
   （Windows 凭据管理器；数据库里只有不透明 `secret_ref`）。
   - 与 [SECURITY §2.1](SECURITY.md) 一致：数据库里只有不透明的 `secret_ref` ✓
   - 平台不支持时**如实报错**，不静默降级成内存 / 文件
   - 遗留：Android 侧钥匙串适配（Keystore）还没做
3. ~~**调度器**（AccountActor）~~ **已完成（2026-11-03）**：`crates/xremail-sync/src/actor.rs`。
   - 一个账户一个 actor；启动即同步 → 按 `poll_interval_secs` 轮询；`wake` 插队
   - 失败退避 `min_backoff` → `max_backoff`，成功即重置
   - **认证失败是终态（2026-11-03）**：`AppError::Auth` → 发 `AccountEvent::NeedsReauth`
     后直接退出 actor，不进退避循环（错授权码重试一万次也不会对）；
     应用层映射成 `SyncState::ReauthRequired`，用户重存授权码后再 `start` 即可。
     同时补上 `reap_finished`：actor 退出后要清句柄，否则重启只是叫醒一具尸体
   - R13 的 `catch_unwind` 边界已在这一层落地（每轮同步一个边界）
   - **`IDLE` 长连接推送已实现（2026-11-03）**：`crates/xremail-protocol-imap/src/idle.rs`
     （`idle_once`，由 `crates/xremail-sync/src/actor.rs` 的 `wait_for_change` 调用）；
     不支持或失败即降级轮询
   - **应用层接线已完成**：`apps/shell/src-tauri/src/lib.rs` 启动时只给
     「凭据可用」的账户起 actor，`RunEvent::Exit` 停全部
4. ~~**同步引擎接进应用的同时跑一次真邮箱验收**~~ **已完成（2026-10-26）**：
   三个测试账号各跑一次完整同步（连接 → 登录 → 列文件夹 → 拉取 → 落库 → 读正文），
   脱敏证据：163 = 6 文件夹 / 4 封 / utf-8；QQ = 6 文件夹 / 71 封 / gb2312；
   沃 = 6 文件夹 / 6 封 / gbk；正文解码**零替换字符**。
   工具：`xremail-sync-smoke`（`crates/xremail-diagnostics`）。
5. **SMTP**（P3）：先实测三家的 SMTP 参数（R18），再写提交与 `uncertain` 恢复。
6. 提前做有限 Android 生命周期验证，不等待完整 UI。

前 4 项完成时，P1 的出口条件（「三种真实账户各有实测记录」）才真正达成。

**已完成（2026-10-26）**：同步引擎已接进应用（UI 能看到真实邮件），三家真实账号
各跑通一次完整同步（见上）；**凭据落平台钥匙串**（Windows 凭据管理器）；
**写操作真正回写服务器**（标记已读 / 星标 → `UID STORE` → 回读确认 → 六态回报），
三家真实邮箱的 `UID STORE` 实测记录在 [PROTOCOLS.md](PROTOCOLS.md)。

**下一批建议（更新于 2026-11-06）**：调度器、R13 的 panic 边界、
`COPY` / `MOVE` 协议 + 假服务器支持、**移动/删除的写路径**、
**断线恢复（崩溃后对账）**、**`IDLE` 客户端侧与调度器接线**、
**断线重连**（2026-11-06 完工，B1/R43：会话断开立刻重建，不再等满轮询间隔）、
**附件下载与内联图片**（2026-11-06 完工，B2；部件号口径见 R44）、
**DKIM 发信签名**（2026-11-06 完工，D1：签发 2048/3072/4096 位密钥，私钥在平台钥匙串）
均已完工。
剩下最接近核心可靠度的是**真实邮箱的验收证据**（`docs/ROADMAP.md:75`：
发信/收信/附件往返至今没有在 CI 里跑过一次真实账户），
以及 **fuzz**（T10）——原列在这里的「**乱序响应**（T8 剩下的一半）」已于 2026-11-09 完工
（`Fault::InterleaveUntagged` + `tests/interleaved.rs`）。

**已完成（2026-10-23）**：三家测试账号已准备并完成只读实测（凭据仅在本机配置）；
只读 probe、假服务器、同步纯函数、同步引擎（拉取 + 回写 + 按需正文）、
MIME/charset 解析均已落地。

相关：[架构](ARCHITECTURE.md) · [风险](ISSUES_AND_RISKS.md) · [测试](TESTING_STRATEGY.md) · [AI 规范](AI_DEVELOPMENT_GUIDE.md)
