# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.1/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

维护者：xyyi0719

## [Unreleased]

### Security
- **补上 `deny.toml`，让发布检查单里的「`cargo deny` 无告警」真的可执行**（2026-11-10，R71）
  - `SECURITY.md` 的检查单要求它，但仓库**没有 `deny.toml`**；没有配置时 `cargo deny`
    回退到空白的允许列表，于是每条依赖都被判 `rejected`——**失败原因跟许可证无关，纯粹是没配**
  - 清点依赖实际用到的许可证后，明确写下允许哪些：MIT / Apache-2.0 / BSD-2·3-Clause /
    ISC / Zlib / 0BSD / Unlicense / CC0 / Unicode-3.0，以及 **`MPL-2.0`**
    （弱 copyleft，单独说明为什么接受：用到它的 5 个 crate 是构建期的 `cssparser` /
    `selectors` 等与 `option-ext`；MPL 只要求「修改了那些文件才需以 MPL 公开」，
    本项目没有改）。**没有**放行 GPL / AGPL / LGPL / SSPL / BUSL
  - **变异验证**：把 `MPL-2.0` 从允许列表删掉 → 立刻 `error[rejected]`，
    证明这份配置真的有约束力，不是摆设
  - `cargo deny check licenses bans` → **`bans ok, licenses ok`**
  - 同步修正 `SECURITY.md` 的命令：日常用 `scripts/security_audit.py`（漏洞）+
    `cargo deny check licenses bans`（许可证/禁用项，不需网络）；`wildcards` 设成 `warn`，
    因为本项目靠 `workspace = true` / `path = …` 组织，`deny` 会拦下几十条自己的内部依赖
- **修掉两个生产路径上的依赖漏洞，并把依赖审计变成常规动作**（2026-11-10，R70）
  - 仓库此前**没有任何依赖审计手段**。第一次跑 `cargo-audit` 就报 **3 个漏洞 + 8 个未维护警告**，
    其中两个在**生产路径**上：
    - `ctutils`：在 `xremail-crypto` 的 argon2 / chacha20poly1305 依赖链里，
      掩码生成与 `u128` 比较**不是常数时间** → 升级 0.4.3 → **0.4.8**
    - `tokio`：`Interval` 在 1.54.0 有竞态，影响所有异步代码与调度器 → 升级 1.54.0 → **1.54.1**
  - **3 个漏洞降到 1 个**；stable 下 fmt / clippy / 1144 条测试全绿
  - 新增 `scripts/security_audit.py`：本机 `git` 到 GitHub 不稳，脚本在拉库失败时改用
    `gh api` 下 tarball 并整理成 `cargo audit --db` 能接受的形式（要 git 仓库且要有 `origin` remote）；
    输出会打出每个受影响的 crate **处在依赖树的什么位置**，而不只是列清单
  - **残余 1 个**：`libsqlite3-sys 0.37.0` 受 CVE-2026-119375 影响，**没有可用补丁**
    （`sqlx 0.9.0` 已是最新）。可达性低（数据库路径固定、攻击是本地且需用户操作），但**不是零**，已记录
- **找出「零调用方的 `pub` 函数」的工具**（2026-11-10，`scripts/find_unused_pub_fns.py`）
  - `pub` 是给外部的承诺，所以编译器**不会**为零调用的 `pub fn` 报 `dead_code`——
    `cargo clippy -D warnings` 一声不吭，这类东西会一直藏着
  - 脚本按文本找「除了定义行之外没有任何 `name(` 出现」的 `pub fn`，
    并明确声明自己是**启发式**、输出是「值得看一眼的清单」而不是「可以删的清单」
  - 2026-11-10 查了一遍，23 个命中里人工确认 3 个真·零调用，三条注释都写错了：
    `delete_messages_in_folder` 说「用于 epoch 切换」（实际 epoch 切换走的是
    事务内按世代删除，这个是**全删**、语义不同且更危险）、
    `read_addresses` 说「测试辅助」（测试里也没用）、`imap_config_for`（无注释）
  - **处理方式是标注而不是删除**：`pub` 是承诺，删掉可能打掉别人依赖的入口；
    但必须让下一个人知道它们**不是活代码**
- **属性测试的轮数做成可配置，默认提到 2 万轮**（2026-11-10，T10 的折中）
  - 覆盖率引导 fuzz 仍然没做（要 nightly + `libfuzzer-sys`，不进 PR CI）。
    但这些固定种子的属性测试**成本几乎只在启动上**——原先固定 1000–2000 轮、
    整组只要 0.07 秒，没有理由跑这么少
  - `XREMAIL_PROPERTY_ROUNDS` 可覆盖；三个文件都支持（HTML 消毒 / MIME / 邮箱名）
  - **已经用它扫过**：HTML 消毒器 200 万轮（约 1200 万次输入，14.4s）、
    MIME 解析器 200 万轮（约 1400 万次输入，9.4s），**都全过**——
    这是「这两个解析器在这规模下是稳的」的证据
  - 踩到一个坑并修掉：不是所有用例都该跟同一个轮数走。
    `attachments_never_exceed_the_cap` 每轮要生成最多 3000 个原子，
    统一放大后它一条就要 34 秒，后来单独定了 `rounds()/50`
- **CI 偶发失败频率的量化工具**（2026-11-10，`scripts/ci_failure_rate.py`）
  - 风险库里有一类问题只能靠频率回答（「偶发失败」「改了之后应该好了」），
    而频率散在几百次 CI 运行里，人肉翻 `gh run list` 翻到第十几次就开始糊弄
  - 脚本打印成功/失败次数、失败那几次的运行 id、以及**样本量提醒**：
    「N 次全成功」在「其实没修好」前提下的发生概率是 `(1-p)^N`
  - 立刻用它量了 R49：**20 次样本 / 失败 1 次 = 5%**；串行化之后只跑了 6 次，
    **漏判概率 74%**；要让漏判概率降到 5% 以下需要**约 59 次连续成功**。
    R49 的登记已按这个数改写——**在那之前不能说「已修复」**
- **真实邮箱验收的离线预检**（2026-11-10，`scripts/precheck_real_mail.py`）
  - 真验收会**真的发一封邮件**，所以有两道开关挡在普通 CI 之外——这是对的，
    但副作用是：**没有任何东西能在"真的跑之前"发现环境已经坏了**
  - 新增的预检**不需要真凭据**，检查的全是可离线判断的东西：凭据文件在不在、
    权限是否过松（口令是明文，不该 0644）、有没有 `163` 与 `qq` 两段、
    `user`/`password` 是否非空、QQ 的数字用户名会不会被正确补成 `@qq.com`、
    两个账户是不是配成了同一家
  - **不打印口令、也不打印完整地址**（只留首字母 + 域名）
  - 四条失败路径都实测过：权限过松 / 缺段 / 文件不存在 / 两段相同
  - 顺带暴露一个已知限制：`real_mail_roundtrip.rs` **写死**从 `163` 与 `qq` 取账户，
    想换组合要先改代码——预检会把这件事明确报出来
  - 接到 CI 的完整方案写进了 TESTING_STRATEGY §6.1（`workflow_dispatch`
    手动触发 + Encrypted secrets，**不进普通 PR CI**，政策不变）
- **交接文档 `docs/HANDOVER.md`**（2026-11-10）
  - 13 轮下来，文档体系已经很密（ARCHITECTURE 设计、ROADMAP 进度、ISSUES 风险、
    TESTING 策略），但**缺一份"现在到底什么状态、哪些话能信"的总览**
  - 内容包括：六条最容易踩的坑（文档会骗你 / 行号会失效 / `git checkout --` 和
    `sqlx prepare` 是破坏性的 / 先看 CI 的 job 列表 / sqlx 的 INTEGER-REAL /
    迁移注释里不能有分号）、当前能力矩阵（能用 / 只做一半 / 完全没做）、
    66 条风险里还开着的 11 条按"要不要动手"分类、改各模块前要知道的坑、
    本机能验与不能验的清单、五条方法论、以及建议的推进顺序
  - 每条事实都对着代码核实过（迁移 11 条 / crate 11 个 / 通知 8 条单测 /
    托盘菜单 4 项 / R65 的 8916182 这个数）
- **新邮件通知**（2026-11-09，P5-L）
  - 之前 `tauri_plugin_notification` 只注册、**全仓没有一处调用**，应用从不发通知
  - **只通知 `INBOX`**：别的文件夹有新信（已发送的自副本、归档、垃圾箱里新来的
    垃圾邮件）不该打断用户
  - **前台不通知**：判据用 `is_focused()` 而不是 `is_visible()`——窗口被托盘藏起来时
    `visible == false`，那正是**需要**通知的时候
  - **首次同步与 epoch 切换不通知**：那是全量重拉历史，一封都不是新的
  - **聚合显示**：一轮 50 封只弹**一条**，标题写账号与封数，超过 3 封正文改成
    「还有 N 封」。逐封弹等于骚扰
  - 判据都是纯函数、有 8 条单测；查账户与发通知在 `spawn` 出去的短任务里，
    不阻塞同步链路；发不出去只记 debug 日志（没有通知守护进程是正常情况）
  - **未验证**：在真实桌面会话里真的弹出（本机 `notify-send` 也被沙箱的
    AppArmor 只读限制挡住）；点击通知聚焦窗口也没验
- **T10 的最后一块：IMAP 响应解析（服务器 → 客户端）的属性测试 + 变异测试**（2026-11-09）
  - 新增两个测试文件，与前三个属性测试同一套做法（固定种子 SplitMix64、每条不变量一个 `#[test]`、
    断言里打印输入与输出）：
    - `crates/xremail-protocol-imap/tests/imap_parsing_properties.rs` **12 条**（纯函数、跑得快）：
      `FETCH` 属性一一对应、`BODY[…]` 落到**哪个**字段、`LIST` 属性→角色（属性优先、不认识的属性不认角色）、
      IDLE 推送只有「有变化 / BYE 报错」两种结论、`COPYUID` 端点与展开上限、`APPENDUID` 闭区间、
      以及假服务器几个解析器的自洽性（`Command` / `SequenceSet` / `parse_fetch` / `parse_store` / `search`）
    - `crates/xremail-protocol-imap/tests/response_properties.rs` **12 条**（真 TLS 端到端）：
      `EXAMINE` 缺 `[UIDVALIDITY]` 时是 `None` 而不是 0、`[HIGHESTMODSEQ]`/`[UNSEEN]`/`[UIDNEXT]` 照收、
      `COPYUID` 按下标对应 / 长短不一不硬套 / 展开有上限、没有 `COPYUID` 时源集合退回入参（排序去重）、
      `NO` / `BAD` / 未知状态一律明确报错（`[MODIFIED]` 单独成类）、**没有结局就挂住而不是报成功**
  - 为了让「服务器说了一句形状不对的话」能在线路上造出来，`xremail-testkit` 新增两个**测试专用**故障：
    `Fault::RespondWith`（换掉完成响应）与 `Fault::InjectUntagged`（在数据与结局之间插一行原文）。
    既有故障语义与 `handle_command` 一行未动
  - 为去掉「每条样本起一台 TLS 服务器」的开销，`command` / `transfer` / `append` / `idle` 里
    六个私有解析函数改为 `pub`（只加可见性 + 文档，函数体与调用方一字未动）
  - **变异测试 18 个全部被捕获**（`end-1`、展开少一个、上限放大、去掉长度检查、按值查映射、
    不认 BYE、不认识的属性认成 trash、见到 untagged 就收工、`BODY[TEXT]` 落错字段、序列集开区间、
    假服务器放宽 `STORE` / 取件项括号 / 把不认识的搜索键当 `ALL` 等），细节见
    [ISSUES_AND_RISKS §6](docs/ISSUES_AND_RISKS.md) 与 [TESTING_STRATEGY §5.1.1](docs/TESTING_STRATEGY.md)
  - **钉住了一处已知残余**（不是新缺陷）：`COPYUID` 的源、目标两侧各自被截到同一个上限时长度**恰好相等**，
    `has_uid_mapping()` 会把这份被截断的映射当成可用的一对一映射
  - 顺带坐实：async-imap 的 `APPEND` 在**它自己的响应解析**里读 `[APPENDUID …]`，完成响应形状认不出时
    `append_message` 会**挂住**——调用方的超时不是可选项
  - 用例数 **1062 → 1086**（`cargo test --workspace -- --list`，含 1 个 `#[ignore]`）
- **WebKitGTK 上的 HTML 隔离实测：本地手工验收工具 `scripts/webkit_sandbox_probe.py`**（2026-11-09，R06 / P5-L）
  - 在 `xvfb-run` 起的**真实 WebKitGTK 2.52.6**（`WebKit2.WebView`）里，把 `tauri.conf.json` 的 CSP **原文**
    放进页面，把**真消毒器**（Rust `sanitize_html`，经 `rustc` 编成的小 CLI 调用）的输出与若干
    **绕过消毒器**的样本塞进 DOM，逐条断言运行时行为并打印 PASS/FAIL + 实际观测值
  - 覆盖：远程图片（http / https / 协议相对 / `srcset` / CSS 背景 / 表格背景）**零请求**、`data:` 图片**能**显示、
    内联脚本 / 事件属性 / `javascript:`（含点击）不执行、sandbox iframe 碰不到父窗口、
    无 sandbox 的 `srcdoc` **读写得到父窗口**、`frame-src` 拦远程 iframe、`file:` / `data:` 强导航被拦
  - **结果**：WebKitGTK 49/49 PASS；同一组用例在 Playwright Chromium 上逐条结论一致
    （`--engine=both` 98/98 PASS，差异 0 项）；CSP 改成响应头下发（更接近 Tauri）再跑同样 49/49 PASS
  - **顺带量到的**：强导航到远程 http **会**把主文档换掉（两个引擎都一样）——隔离靠
    `MessageView.tsx` 的 `preventDefault` + 协议白名单，消毒器只保证 `href` 是 http(s)/mailto
  - **没验的**：Tauri 真实源与 IPC/ACL、macOS WKWebView、真 GPU 环境、链接交给系统浏览器的端到端行为
  - **不接 CI**：CI 上没有 WebKitGTK 的图形环境。跑法见脚本头注释与 [TESTING_STRATEGY §2.4.1](docs/TESTING_STRATEGY.md)
- **Linux 打包链路打通：deb 与 AppImage**（2026-11-09，P5-L）
  - `npm run tauri build -- --bundles deb` → **7.7 MiB**；`--bundles appimage` →
    **87.2 MiB**（自带依赖）。二进制本身 17.4 MiB
  - 新增 `scripts/verify_linux_runtime.py`：**在 Xvfb 虚拟显示里真跑打包产物**，
    断言动态库齐全、启动不 panic、建出数据库、**10 条迁移全部成功且条数与
    `migrations/*.sql` 一致**、20 张表齐全。deb 与 AppImage 两种格式都验过
  - 脚本做了变异测试：把期望的迁移条数改错，它会立刻报「对不上」
  - 一个容易误判的点（实测澄清）：deb 声明 `libgtk-3-0`，而 Ubuntu 24.04+ 装的是
    `libgtk-3-0t64`。看着像依赖缺陷，但 t64 包 `Provides: libgtk-3-0`，
    `apt-get -s install` 能正常解析——**不是**问题
  - **未做**：rpm（本机无 `rpmbuild`）。**未验证**：真实桌面会话里双击启动、
    Fedora 等非 Debian 系发行版
- **Linux 上用系统钥匙串存授权码**（2026-11-09，P5-L）
  - 之前 Linux 上钥匙串是「如实报错」的桩：**每次冷启动都要重填授权码**。
    现在有了 Secret Service 适配（`apps/shell/src-tauri/src/secrets/linux_store.rs`）
  - 走 **D-Bus** 而不是链 `libsecret`：直接链会把可执行文件钉在某个发行版的
    SONAME 上，而 D-Bus 只要求「有个实现了 `org.freedesktop.secrets` 的守护进程」——
    GNOME Keyring、KWallet 都行。为此新增 `keyring` 3.x 与 `dbus-secret-service`
    两个 crate（依赖树很浅），这是对仓库「少拉 crate」约束的一次**明确权衡**
  - 契约与 Windows 实现逐条对齐：条目名原样用 `secret_ref`、机密存原始字节、
    「不存在」不是错误、错误信息不含机密
  - **没有守护进程时如实报「不可用」**，不落盘、不降级——这条用真实 D-Bus 会话
    （`dbus-run-session`）验过，并做了变异测试：把实现改成「失败也返回 None」
    后用例立刻变红
  - **尚未验证**：真钥匙串上的写→读→删往返（本机没有 `gnome-keyring-daemon`，
    也无法安装）。有桌面环境时用
    `XREMAIL_SECRET_SERVICE_PROBE=1 dbus-run-session -- <测试二进制>` 跑一遍即可
- **Android PoC 第一轮：应用第一次能以 Android APK 的形式被构建出来**（2026-11-09，P4 / R08）
  - `npx tauri android init` 生成 `apps/shell/src-tauri/gen/android/`（此前 `gen/` 下只有 `schemas/`）。
    **整个 `gen/` 目录本来就被根 `.gitignore` 忽略**，构建产物（`app/build`、`.gradle`、`jniLibs`）
    不会进 `git status`——所以没有提交任何生成物，也没有为此改 `.gitignore`
  - `npx tauri android build --debug --target aarch64` **真的产出了 APK**：
    `app/build/outputs/apk/universal/debug/app-universal-debug.apk`（266,388,896 字节，
    `com.xremail.app`、minSdk 24 / targetSdk 37、`lib/` 只有 `arm64-v8a`）。
    debug 带完整调试信息所以特别大；作为对照**也量了 release**：
    `npx tauri android build --target aarch64 --apk` → `app-universal-release-unsigned.apk`
    **14,869,199 字节 ≈ 14.2 MiB**（未签名，要装得先配签名）
  - **本仓库 9 个 crate 全部在 `aarch64-linux-android` 上编过**，连带 `sqlx` 的 `sqlite-bundled`
    （自带的 C 源码走 NDK clang）、`ring`、`async-imap`、`tokio-rustls`、`tauri`/`wry`/`tao`。
    **但只是编过，没有在任何设备上加载过**
  - 前端资源是**内嵌进 `.so`** 的（能在 `.so` 里找到 `dist/assets/index-*.css/js`），
    APK 的 `assets/` 下只有 `tauri.conf.json`
  - `tauri.conf.json`、`AndroidManifest.xml`、`build.gradle.kts` **一个字没改**——
    `cargo-ndk` 不需要往配置里塞东西，Tauri 2.12 的默认工程直接就能出包
- **两条「Android 上会怎样」的事实被坐实**（静态证据，不是运行结论）
  - **托盘确实不在 Android 构建里**：`tray` 模块是 `#[cfg(desktop)]`，打进 APK 的 `.so` 里
    `xremail_shell_lib::tray` 的符号**一个都没有**
  - **平台钥匙串在 Android 上是桩**：`platform_default_store()` 在非 Windows 上是
    `UnsupportedSecretStore`，产物 `.so` 里查得到它的文案「只有 Windows 凭据管理器已适配」、
    查不到 `CredWriteW`。后果是**授权码和 DKIM 私钥在 Android 上都写不进去**，每次冷启动要重填
    （R19 的 Android 侧）。Keystore 的适配**只写了路线**（ROADMAP P4），**没有实现**
- **CI 的 `Rust (ubuntu-latest)` 重新变绿：DKIM 用例不再依赖真实钥匙串**（2026-11-09）
  - 自 D1 DKIM 起，ubuntu job 的 `Test` 一步**一直是红的**：6 条用例（`dkim_service.rs`
    4 条 + `command_ipc.rs` 2 条）要把私钥写进平台钥匙串，而钥匙串只在 Windows 上有实现，
    其它平台是「如实报错」的桩。Windows job 是绿的，所以问题一直只出现在 ubuntu 上
  - 钥匙串改为**可注入**（`secrets/mod.rs` 的进程级覆盖槽 + `secrets/test_hooks.rs` 的
    串行闸与 `Drop` 自动还原）；未注入时行为与原来逐字一致。6 条用例改为装内存钥匙串，
    **断言没有放宽**，反而更严：钥匙串里存的必须是原样 PEM、`clear` 之后条目必须真的没了、
    被拒的密钥与非法 selector 一条都不许写进去
  - 顺带修掉一个此前没人注意的副作用：那 4 条用例在 Windows 上**真的往你的凭据管理器里
    写了 `xremail:dkim:<随机 uuid>` 条目，而且从不清理**
- **凭据服务的失败语义有了证据**（2026-11-09，R07 的「仍待定」）
  - 新增 `apps/shell/src-tauri/tests/credential_service.rs` 13 条：钥匙串里存什么、
    库里**只有不透明引用**（逐表逐列扫全库证明库里没有授权码明文）、没有引用时
    **不打扰**钥匙串、钥匙串故障与「没保存过」**分清**、保存失败时 `secret_ref`
    **不被写脏**、删除幂等、非 Windows 默认实现**不静默降级**
- **「删除 = 进垃圾箱」不再靠文件夹名猜**（2026-11-09，R04 残余）
  - 根因是**结论没地方放**：协议层早就按 RFC 6154 认出了 `\Trash`（服务器宣告了就用
    宣告的），但 `folder` 表没有 role 列，同步一结束那个结论就丢了，删除时只能重新
    拿文件夹名去比 `Deleted Messages` / `已删除` 之类。服务器把垃圾箱叫 `Rubbish`
    或「回收站」时猜不出来，删除就退化成**就地标删**（不可撤销）
  - 新迁移 `crates/xremail-store/migrations/0010_folder_role.sql`：`folder.role TEXT`
    可空，`CHECK` 限定为 `inbox`/`sent`/`drafts`/`trash`/`junk`/`archive`/`flagged`/`all`
    （与 `folders.rs` 实际产出的取值一致），外加按账户的部分索引。
    **不给默认值**：`NULL` 的语义是「**还不知道**」，不是「没有垃圾箱」——
    老库升级上来的行、以及服务器没宣告属性且名字也认不出的文件夹都是 `NULL`
  - 同步每轮把 `LIST` 的角色写进库（`xremail-sync/src/sync.rs`），
    删除时按「库里的 `trash` 角色 → `role` 为 `NULL` 时按文件夹名兜底 → 都不行就地标删」
    降级（`transfer_push::trash_candidates`）。库里角色**已知且不是** `trash` 时，
    名字再像垃圾箱也不算；当前文件夹自己永远排除在外
  - 老库升级后还没同步过一轮的账户**仍然靠名字兜底**，删除不会因为升级突然变成
    不可撤销的；同步一次之后才完全按角色走
  - `Folder` 新增 `role: Option<String>` 与 `is_trash()`（`xremail-core`）。
    构造 `Folder` 的代码需要补 `role: None`（仓库内已全部补齐）
  - 迁移的已知限制（写进了迁移注释）：SQLite 没有 `ALTER … ADD COLUMN IF NOT EXISTS`，
    `CASE` 也决定不了语句是否执行，所以「同一段迁移可重复应用」在 SQL 层做不到；
    正常升级路径由 `_sqlx_migrations` 记账保证只跑一次，手动重放会**明确报
    `duplicate column name` 且已有数据原样保留**（有用例钉住）
（2026-11-09，P4 / `apps/shell/src-tauri/src/tray.rs`）
  - 以前**关掉主窗口就等于退出进程**，后台同步（`Scheduler`）跟着停掉——不盯着
    界面的那段日子里一封新邮件都不会同步，开机自启也名存实亡；现在**关闭窗口是退到
    托盘**：窗口隐藏，进程与后台同步继续跑
  - 托盘菜单三项：**显示主窗口** / **立即同步** / **退出**；左键单击也显示并聚焦
    主窗口，和「第二次启动」的单实例行为走同一个入口（藏起来的窗口会先被 `show`）
  - **「立即同步」**对所有已调度账户逐个叫醒一轮（`service::scheduler::wake_all`），
    复用调度器自己的 `wake`，不自建一套同步
  - **「退出」是真退出**：`app.exit(0)` → `RunEvent::Exit` → `Scheduler::shutdown()`。
    刻意不用 `std::process::exit`——它不发 `RunEvent::Exit`，那段收尾会变成死代码，
    退出时 actor 还在 `IDLE` 或写库，未提交的写操作就停在 `running`
  - 只开 tauri 自己的 `tray-icon` feature，**没有新增第三方依赖**（`Cargo.lock` 未变）；
    托盘只在桌面端创建，Android 上不存在；图标直接取 `app.default_window_icon()`
  - **开机自启仍然弹窗口**（没有按启动参数「只驻留托盘」）：能做，但要在 `setup` 里
    把默认显示的窗口再关掉，参数一错就是「点了没反应」；自启要的效果（登录后立刻开始
    后台同步）现在已满足
  - 已知限制：Linux 上 `TrayIconEvent` 根本不发，左键「显示窗口」不生效（要用菜单）；
    macOS 左键会先弹菜单。**Windows 真机行为未验证**（本机无显示器）——Linux 上只有
    编译、clippy、fmt 零警告与 4 条菜单映射单元用例（`cargo test -p xremail-shell --lib tray::`）
- **假服务器支持「乱序响应」注入（T8 的最后一块）**（2026-11-09）
  - 163 / QQ 这类 Coremail 体系的服务器会在一条命令**处理过程中**把别的
    untagged 响应（`* 23 EXISTS`、`* 5 EXPUNGE`、`* 3 FETCH (FLAGS …)`）插到
    你的命令与它的 tagged 完成响应之间。`xremail-testkit` 新增
    `Fault::InterleaveUntagged { command, noise, once }`：对指定命令（`UID FETCH`
    写 `FETCH`），在命令自己的数据之后、tagged 响应之前插一条**别人的**
    untagged 响应；`noise` 可选当前邮件数的 `EXISTS`（默认）、`* 3 FETCH
    (FLAGS (\Seen))`、`* OK [ALERT]`、`* BYE`——全部合法、**不改变服务器状态**
  - 协议层新增 **11 条端到端用例**（`crates/xremail-protocol-imap/tests/interleaved.rs`）：
    `UID FETCH` 的结果与不注入时**逐字段一致**（`UID STORE` 判定 + 回读的标记、
    `COPY` 的映射、`UID EXPUNGE` 的真实删除都各自核对过），
    标了别的 tag 的 `NO` 不会被当成本命令的结局（ARCHITECTURE §2.1），
    会话收到 `* BYE` 也不会被误判成断开。**结论：客户端没有 bug**——
    `run_checked` 与 transfer 的读循环本来就按 tag 自证成败；用变异测试确认了
    「读到第一条 untagged 就收工」会让这些用例变红
- **附件下载与正文内联图片**（2026-11-06，B2）
  - 邮件里的附件现在可以按需下载：点了才去服务器取对应那一段，64 KiB
    一段流式落盘，超限立即中止并删掉临时文件（不会留下一个被截断的半截
    文件），落盘后核对大小、以内容 sha256 命名，同一附件重复下载走缓存
  - 正文里的 `cid:` 内联图直接换成本地 `data:` URL 显示；**SVG 一律拒收**，
    单图 2 MiB、合计 8 MiB 封顶
  - 附件上限（单个 10 MB / 最多 5 个 / 合计 18 MiB）**只在后端定义一处**，
    前端通过 `attachment_limits()` 命令取；此前前端写死 20 MiB、比后端宽，
    用户能选上、点发送才被后端拒
- **DKIM 发信签名**（2026-11-06，D1）
  - `crates/xremail-dkim/`：relaxed / simple 规范化、签名与验签（`ring`），
    签完自检「签名覆盖的字节 == 实际要发出的字节」，不一致直接报错不发
  - 私钥存平台钥匙串（`xremail:dkim:<account_id>`），只在保存那一刻经过
    一次界面，保存后输入框立刻清空、配置回读永远不含私钥；删账户时一并清掉
  - 在「冻结字节」之前盖章，所以「再发一次」发的仍是当初签名的那份字节
  - 已知限制：签发只支持 2048/3072/4096 位密钥（`ring` 的下限），
    1024 位只支持验签；一封只签一个域；未接 Ed25519
- **断线重连：会话断开立刻重建，不再等满轮询间隔**（2026-11-06，B1/R43）
  - 以前一次网络抖动之后，程序还在等这一轮等待走完（`poll_interval` 默认
    300 秒），期间既连不上也不会重试；现在会话类失败（`NO`/`BYE`、读写时
    传输层报错、超时、IDLE 期间连接失效）按 1s→2s→4s…（封顶
    `max_backoff`）立刻重建会话，实测掉线后 <10 秒就重新同步（修复前
    14 秒以上，且抖动严重时要等满 300 秒）
  - **认证失败仍然是终态**：错授权码只会报「需要重新授权」，不会陷入重试
  - **「压根连不上」不按重连处理**：`connection refused` / DNS 解析失败是
    服务器侧问题，走普通退避，不会用 1 秒一次的握手去砸维护中的服务器
  - 用户看到的错误文案与以前一字不差（会话标记在事件出口就被剥掉了）
- **Playwright 端到端测试落地**（2026-11-06，T9）
  - `apps/shell/playwright.config.ts` + `apps/shell/e2e/`：**2 个 spec / 5 条用例**，
    `npm run test:e2e --prefix apps/shell` 实测 5 passed（本地约 12 秒）
  - 不拉 Tauri 窗口：`@tauri-apps/api` 的 `invoke` 只是一行
    `window.__TAURI_INTERNALS__.invoke(...)`，所以 `page.addInitScript` 注入一个
    按命令分发的假后端即可（`e2e/daemon.ts`，思路同 `src/test/backend.ts`，
    后者依赖 vitest 的 `vi` 不能直接复用），起的是 Vite 开发服务器 + 真实 chromium
  - 覆盖：邮件列表（倒序、标题、发件人、`list_messages` 入参）→ 点开看正文
    （打开不自动标已读）→ 写信面板（没写收件人时前端拦下、`800ms` 自动存草稿、
    填好后发送显示「已发送」与 `Message-ID` 且发送按钮停用）
  - CI 的 `web` job 新增三步：装 chromium → 跑 E2E → 失败时上传 trace/截图
  - 用例数（2026-11-06 全量实测）：`cargo test --workspace` **842 passed / 0 failed /
    1 ignored**（`--list` 843）、`npm test` **12 个文件 / 69 条**、E2E **2 个文件 / 5 条**
- **`uncertain` 的「再发一次」人工出口打通**（2026-11-06，R30）
  - 之前界面上那条「再发一次」走的是 `send_message({fresh: true})`：**新写一封
    草稿、生成新的 `Message-ID`**，等于给用户又发了一封新信，而原来那封仍挂在
    未确认名单里——服务端已有的 `resend_uncertain` 全仓没有调用者
  - 新增 Tauri 命令 `resend_uncertain(entryId)`（账户取自发信记录本身，不信前端
    传的 `accountId`），前端 `ComposePanel` 的确认分支改调它，按返回的
    `sent` / `uncertain` / `failed` 分别更新结果提示与未确认名单
  - 顺手修掉：「再发一次」失败后原来那封从名单里消失、无法撤回
  - 用例：前端 4 条（`apps/shell/src/__tests__/compose-uncertain.test.tsx`）+
    `apps/shell/src-tauri/tests/compose_service.rs` 2 条 + `tests/command_ipc.rs` 1 条
- **A 组审计缺陷集中修复**（2026-11-06，R26–R28 / R31 / R34–R42，详见
  [ISSUES_AND_RISKS §4](docs/ISSUES_AND_RISKS.md)），其中用户能感觉到的几处：
  - 一则错授权码不再被永久重试（早先已修），**标旗/搬移意图不会被每轮重发**
    （R26、R34），**同一封邮件不会因为两条 flush 并发而搬出两份**（R34）
  - 服务器不回应时**不再永久挂住**：写路径的 `select` / `examine` 与
    `COPY` / `MOVE` / `STORE \Deleted` / `UID EXPUNGE` 全部补上命令超时
    （R27、R38）
  - 无 `UIDPLUS` 的服务器上**不再假装删成功**（邮件不会下一轮又自己回来，
    R35）。~~移动成功后按 `COPYUID` 的目标 UID 直接写回本地（R39）~~——
    **这条当时并未实现**（2026-11-09 复核代码：`TransferOutcome` 未绑定即丢弃），
    R39 已改回待办，见下
  - `IDLE` 的三个边界（能力大小写、25 分钟上限、`EXAMINE` 只有 `NO` 才回退，
    R41/R42）；探测失败的账户**当轮就不再尝试 IDLE**，不再每轮白付一次 TLS
    （R37）；等待期的 panic 也会计数并在达上限后放弃账户（R28）
  - 假服务器按宣告能力 gate `MOVE` / `UID EXPUNGE`，并在 `MOVE` 后补发
    untagged `EXPUNGE`——「能力探测漏判」与「untagged 穿插」这两类 bug
    以后在 CI 里会红（R40）
- **对账从「只在启动」变成「每轮同步之后」**（2026-11-06，R29 / R32 / R33）
  - 常驻运行期间 `attempts` 用完的操作不会再「既不移动也不提示」地等到下次
    重启：每轮同步成功后都会对一次账（`AccountEvent::Synced` 带着对账报告，
    界面收到就重查队列）
  - 本进程自己认领后卡在 `running` 的行有了**租约**（15 分钟）：超时收成
    `uncertain / lease_expired`，由下一轮回读去核对，不再永久卡死
  - `uncertain` 有了**时间出口**：72 小时仍无结论就转成 `failed /
    stale_unresolved`，并明确提示「请到服务器上确认它是否已经完成」——不再
    无限期挂着，也**不会**替用户猜成成功或失败
- **认证失败成为终态，不再无限重试**（2026-11-03）
  - `SyncState::ReauthRequired` 从 2026-10-26 就声明好了（`crates/xremail-core/src/model.rs:566`，
    注释写「a human has to act」），但**全仓没有任何地方发它**——实际行为是
    `actor.rs` 里所有错误一律进 `Failed` 分支退避重试，于是一个错授权码会被
    **永久重试**，UI 一直显示「正在重连」，把「要人管」伪装成「会自愈」
  - `TickFailure::Failed` 从 `String` 改回保留 `AppError`：压成字符串后就只能
    比对错误文本，那是脆的；新增 `AccountEvent::NeedsReauth`，`AppError::Auth`
    发事件后**直接退出** actor；`TauriSink` 映射成 `ReauthRequired` + `SyncError`
  - 修掉一个会造成**静默停止同步**的 bug：actor 退出后句柄仍留在 `Scheduler`
    表里，而 `tx.send` 到死任务不报错，于是用户改完授权码再 `start` 只会叫醒
    一具尸体。新增 `reap_finished`（按 `join.is_finished()` 清）
  - 用例 790 → **792**：`auth_rejection_stops_the_actor_instead_of_retrying_forever`、
    `a_reauth_required_account_can_be_started_again`；后者用变异测试验证
    （把 `reap_finished` 改成空操作后该用例必失败）
- **假 IMAP 服务器的 `IDLE` 与主动推送**（2026-11-03）
  - `crates/xremail-testkit/src/server.rs`：`IDLE` 命令（回 `+ idling` → 等
    `DONE` → 用**原 tag** 回 `OK`）、服务器级 `PushRegistry`（登记
    「哪些连接正在 IDLE、选中的是哪个文件夹」）
  - 对外提供推送入口：`* n EXISTS` / `* n EXPUNGE` 只送给**正在那个文件夹
    IDLE** 的连接，离开 `IDLE` 后不再收到；这就是测试「推送不漏、不串文件夹」的基础
  - 错误路径也覆盖：未 `SELECT` 就 `IDLE` → `NO`；`IDLE` 期间发别的命令 → `BAD`；
    故障注入（`faults`）对 `IDLE` 同样生效
  - 用例 7 个（`crates/xremail-testkit/tests/idle.rs`）
- **`IDLE` 长连接推送与调度器接线**（2026-11-03）
  - 协议层 `crates/xremail-protocol-imap/src/idle.rs`：`idle_once`——发 `IDLE` →
    等 `+ idling` → 读推送 → 发 `DONE`（**不带 tag**）→ 等 tagged `OK`，
    而 tagged 判定**必须匹配进 IDLE 那条命令的原 tag**；半行响应按行拼接，
    不能把一次 TCP 读就当成一条完整行
  - 调度层 `crates/xremail-sync/src/actor.rs` 的 `wait_for_change`：拿到推送就立即发 `DONE`
    （不只等 30 分钟定时器，否则服务器推的下一条要干等一轮超时），然后叫醒同步
  - **安全出口**：无论正常返回、超时、断线还是 panic，都先 `is_authenticated()`
    校验；`DONE` 三次握手没走完的会话**弃用重连**，绝不还回连接池
  - **降级**：服务器不支持 `/` 回 `NO`·`BAD` / 出错 → 记 `idle_supported = false`，
    当轮改走轮询，下轮不再重试（三家实测：QQ 有 IDLE，163/沃 无）
  - **调度器接进应用**（`apps/shell/src-tauri/src/`）：启动时列出账户、**只给
    凭据可用的**起 actor（没存授权码的起来只能一直失败），`RunEvent::Exit` 时
    `Scheduler::shutdown`；`delete_account` 先 `stop` 再忘凭据；`sync_account` 在
    「第一次拿到可用凭据」时补 `start`
  - 新增 `cmd::ensure_scheduled`（`apps/shell/src-tauri/src/cmd.rs`）：先问
    `Scheduler::is_running`，已在跑就**不**叫醒——`Scheduler::wake` 会立刻让
    actor 跑一轮，而手动同步命令**正在自己跑一轮**，不先判就会同一账户并发跑两次
  - 前端订阅 `sync://state-changed`，只在 `state === "online"` 时重查
    （每轮报 `connecting` + `online` 两次，全刷等于白拉一倍数据）
  - 用例：`crates/xremail-protocol-imap/tests/idle_client.rs` +
    `crates/xremail-sync/tests/scheduler.rs`（总数 778 → 790）
- **移动 / 删除的写路径与本地排队**（2026-11-03）
  - 新增 `crates/xremail-sync/src/transfer_push.rs`：`push_transfers`
    + 纯函数 `route_for`（能在不连服务器的情况下测路由）
  - **路由由服务器能力决定**：有 `MOVE` 就一条 `UID MOVE`；没有则
    `UID COPY` → 标删 → **有 `UIDPLUS` 才** `UID EXPUNGE`（163 没有 `MOVE`
    但有 `UIDPLUS`，所以走的是完整三步，**不是**「止步于标删」）
  - **删除 = 进垃圾箱**：有垃圾箱就挪进去（可撤销），没有才就地标删；
    垃圾箱靠文件夹名推断，推不出来就不乱挪
  - **源文件夹里已经没有这封邮件 → 算成功**：`MOVE` 不是幂等的，
    重发会让目标文件夹多出一份（上次尝试可能已经生效，只是应答丢了）
  - **队列在同步轮次自动排空**（`sync::flush_pending`）：离线时点的移动
    不会丢，也不需要界面开着
  - 存储：迁移 `0005` 给 `mail_operation` 加 `destination_folder_id`，
    `OperationKind` 扩为 `flags` / `move` / `delete`
  - 命令 `move_messages` / `delete_messages`（只入队，能连上就立即提交）
  - 用例 16 个（`crates/xremail-sync/tests/push_transfers.rs` 8 个端到端 +
    `crates/xremail-sync/src/transfer_push.rs` 里 `route_for` 等 8 个单测）
- **`COPY` / `MOVE` / `EXPUNGE` 协议层与假服务器支持**（2026-11-03）
  - 新增 `crates/xremail-protocol-imap/src/transfer.rs`：`copy_messages` /
    `move_messages` / `mark_deleted` / `uid_expunge`，加 4 个命令构造函数
  - **为什么不直接用 async-imap**：它的 `uid_copy` / `uid_mv` 在服务器回带 tag 的
    `NO` / `BAD` 时**不报错**（与 `command.rs` 里记的是同一个响应解析坑），
    于是「没拷过去」会被当成「拷好了」——这是丢数据的那一类错
  - **解析 `COPYUID`**（RFC 4315）：源/目标 UID **按下标对齐**，
    并暴露 `destination_uid_of(source_uid)` 供上层核对；
    **不造假**：RFC 4315 只带源文件夹的 UIDVALIDITY，所以
    `destination_uid_validity` 恒为 `None`而不是猜一个
  - **删除为什么不能一律 `EXPUNGE`**：裸 `EXPUNGE` 会把这个文件夹里**所有**
    带 `\Deleted` 的信都删掉，包括别的客户端（手机/网页版）标了还没提交的；
    所以只在有 `UIDPLUS` 时才发 `UID EXPUNGE <set>`
  - 假服务器（`crates/xremail-testkit/src/server.rs`）补齐 `COPY` / `MOVE` /
    `UID COPY` / `UID MOVE` / `EXPUNGE` / `UID EXPUNGE`，并按 RFC 6851
    让 `MOVE` **只删被移动的那些**（不碰别人标删的）
  - 用例 9 个（`tests/transfer.rs`）

- **启动对账：进程崩溃后的恢复**（2026-11-03）
  - 新增 `crates/xremail-sync/src/reconcile.rs`：`reconcile_account_as` + 纯函数
    `plan_reconcile`（把「观测 → 决定」从 IO 拆出来，可单测）
  - **目标不是猜服务器做了什么**，而是把没人负责的行标成没人负责，
    交给下一轮回读确认去核对（`push_flags` / `push_transfers` 天然幂等）
  - **按 boot 归属回收**：`running` 只说明有人认领了，不说明认领者还活着。
    认领时写 `owner_boot`（迁移 `0006`），对账只收
    `owner_boot IS NULL OR owner_boot <> 本次 boot` 的行。
    没有这层守卫，同一台机器上另一个**还活着**的进程的存途行会被翻成「结果未知」
  - **先回收、后判过期——顺序本身是语义**：反过来的话，一条属于上次进程、
    次数也已到顶的 `running` 会被 `expire_exhausted` 的状态过滤漏掉，
    永远卡在 `running`：既没人推它，也没人报错
  - **`uncertain` 绝不改回 `queued` / `retryable`**：那等于替服务器判断
    「上次没执行」。只有人的显式决定能改它
  - 同时收掉孤儿操作（文件夹已删 → `folder_missing`；移动目标已删 →
    `destination_missing`）与旧 epoch 操作（→ `epoch_changed`）
  - **幂等**：重复执行都不动手（当前触发点只有**应用启动一处**，
    `apps/shell/src-tauri/src/lib.rs:196`；actor 的 `Startup` 轮尚未调它）
  - 用例 11 个（`crates/xremail-sync/tests/crash_recovery.rs`）

- **账户调度器（AccountActor）与后台任务 panic 边界**（2026-11-03）
  - **调度器**（`crates/xremail-sync/src/actor.rs`）：一个账户一个 actor，
    彼此隔离。账户 A 的服务器卡住（命令超时 30s × N 个文件夹）
    **不会**让账户 B 的同步排队等它
  - **节奏**：交给调度后**立刻**同步一轮，之后按账户自己的
    `poll_interval_secs` 轮询；`Scheduler::wake` 可以把一轮同步插到队首
    （用户点「刷新」不用等到轮询到点）
  - **失败退避**：连接失败从 `min_backoff` 起翻倍，封顶 `max_backoff`；
    成功一次就重置回 `min_backoff`
  - **R13 的 `catch_unwind` 任务边界**：每一轮同步（含 `connect` 与
    `sync_account` 内部的全部 await 点）都包在
    `AssertUnwindSafe(future).catch_unwind()` 里。panic **只停在那一轮**，
    actor 活着并按退避重试；连续 panic 到 `max_consecutive_panics`（默认 3）
    才**放弃**这个账户，避免「panic → 立刻重试 → 立刻 panic」热循环
  - 事件回调（`EventSink`）自己也包一层 `catch_unwind`：UI 层的回调出错
    不该让同步停摆
  - **不引入运行时依赖**：`futures-util` 只是显式声明（async-imap 已经依赖它），
    用它是因为 `std` 没有「对 future 做 `catch_unwind`」的能力，
    而 `tokio::spawn` + `JoinHandle` 会丢掉取消语义（子任务会脱离父任务继续跑）
  - 用例 13 个：5 个纯函数单测（退避翻倍与封顶、非正数间隔兜底、panic 载荷解析）
    + 8 个端到端（启动即同步 / 手动叫醒不等轮询 / 一轮 panic 被拦住后仍能成功 /
    连续 panic 恰好 3 次就放弃 / 连接失败退避 20→40→80ms /
    **一个账户 panic 不影响另一个** / stop 后不再连接 / 叫醒未调度的账户报错）

- **撰写面板（能真正写一封信）**（2026-10-30）
  - **前端**（`apps/shell/src/components/ComposePanel.tsx`、
    `RichTextEditor.tsx`、`ComposePanel.css`）：
    收件人 / 抄送 / 主题 / 正文（**富文本**，粗体、斜体、下划线、
    有序无序列表、链接、清除格式），**草稿保存**、**撤回草稿**、**发送**
  - **富文本编辑器是自写的**（`contenteditable` + `document.execCommand`），
    **不引入新依赖**（本项目的硬约束：前端不加依赖）
  - **后端**：`save_draft` / `list_drafts` / `get_draft` / `withdraw_draft`
    四个命令 + `service/compose.rs` 的草稿读写
  - **发送前校验**：收件人地址（`xremail-core::address::validate_addresses`）
    与附件限制（`xremail-core::attachment::check_attachment_limits_all`），
    **后端为准**——前端校验只是提前拦一下，绕过前端也拦得住
  - **后端会改写寄件人**：`from` 永远取账户地址，附件里的
    `Message-ID`/`Date` 不可信，**以实际提交的那封为准**
  - 前端用例 6 个文件（撰写、地址校验、附件限制、草稿、富文本、`uncertain` 结局）
- **本地全文搜索**（2026-10-30）
  - **为什么在本地搜**：远端 IMAP 的搜索不可靠——163 的 UID SEARCH 只认 ALL，
    其它关键字**静默失效**（已知风险 R23），所以搜索必须落在本地缓存上
  - **存储层**（`crates/xremail-store/src/dao/search.rs` +
    迁移 `0004_message_search.sql`）：
    - `message_search_doc` 文档表（一封邮件一行：标题 / 收发件人 / 纯文本正文）
      + `message_search_fts` FTS5 **外部内容表**（倒排索引单独存放，正文不存两遍）
    - **分词器用 trigram**：中文正文没有空格，默认的 unicode61 会把整句中文
      当成**一个词**，「项目进度」搜不到「进度」；trigram 按 3 字符滑窗切分
    - **索引由触发器维护**：`message` / `message_body` 的写入与删除
      **自动**同步到索引，因此 epoch 切换、级联删除这些不经过应用代码的路径
      也不会留下脱节的索引
    - **短词回退**：trigram 服务不了 1~2 字符的词（「项目」），
      DAO 层退化成 `LIKE` 子串扫描（个人邮箱量级可接受）
    - **查询词按字面匹配**：每个词包成 FTS5 字符串字面量，
      用户输入的 `OR` / `*` / `-` 不会被当成 FTS5 语法
    - `rebuild_index`（**可重复**）与 `check_index_consistency`（查脱节）——
      兜底用，正常路径靠触发器
  - **前端**：`apps/shell/src/components/SearchPanel.tsx`
    （搜索框 + 命中片段，命中词用 `**` 标出）
  - 用例：存储层 21 个、前端 1 个文件
- **HTML 正文能看也能搜**（2026-10-30）
  - **纯函数消毒器**（`crates/xremail-core/src/html.rs`）：
    白名单标签 + 属性白名单、删 `script` / `style` / `iframe` / `on*` 事件
    与 `javascript:` 协议、删远程图片（防跟踪像素）；
    **未知标签只删标签、留着内容**——真实邮件的正文全都被
    `html` / `body` 包着，连内容一起删整封信就没了
  - **消毒在出后端前完成**：`MessageBodyView` 只带 `htmlSafe`，
    **原始 HTML 不出后端**（后端序列化结果里查不到 `htmlRaw`）
  - **HTML-only 邮件也有纯文本**：真实邮箱（实测 163）的邮件**大多只有
    `text/html`**，`text/plain` 是空的——只认纯文本的话，
    这类邮件**一个字都搜不到**、正文也读不出来。
    现在写入正文时缺纯文本会**从 HTML 派生**一份，
    启动时还会**回填**升级前入库的老邮件
  - **派生不覆盖原文**：已经有纯文本的邮件**原样保留**
  - **前端按 HTML 显示正文**（`apps/shell/src/components/MessageView.tsx`）：
    之前是把源码**打印出来**（当时没有消毒器，渲染等于给邮件一个 XSS 口子）；
    现在渲染的是消毒过的 `htmlSafe`，标签是真的元素不是转义文本
  - **正文里的链接交给系统浏览器**（`@tauri-apps/plugin-opener`）：
    不让应用自己跳——邮件内容不可信，让 webview 跳会把整个应用窗口
    换成那个网页；协议也只放行 `http(s)` / `mailto`
- **IMAP `APPEND`（发信存副本）**（2026-10-30）
  - **协议层**：`crates/xremail-protocol-imap/src/append.rs`
    （命令编码 + 逐行解析字面量应答）、`AppendOptions{flags,internal_date}`、
    `AppendOutcome{uid}`
  - **服务层**：`apps/shell/src-tauri/src/service/sent_copy.rs`
    ——「**先查、没有才补**」的自适应逻辑：
    1. 发完等 **2.5 秒**沉降窗口，让服务器自己存；
    2. 在「已发送」里按 `Message-ID` 找这封信；
    3. 找到了 → 服务器自己存了，**不碰**；
    4. 没找到 → 自己 `APPEND` 一份；
    5. 被拒且理由是「已经存过了」 → 当「已有一份」（**第二道保险**）
  - 匹配按**头值里的尖括号段**比——QQ 会**改写** `Message-ID`，
    原值挪到 `X-OQ-MSGID` 头里
  - 用例：协议层 4 条、服务层 4 条 + 单元 6 条；
    真邮箱往返用例断言**两个方向的「已发送」都正好一份**
- **真实发信往返验收**（`apps/shell/src-tauri/tests/real_mail_roundtrip.rs`）：
  用**真实的 163 / QQ 账户双向真发信**（163 → QQ、QQ → 163），
  逐项核对：提交结果、对方收件箱里的投递、**正文里的标记**、以及
  「服务器会不会自己存副本到已发送」。
  **默认跳过**，需 `XREMAIL_REAL_ACCOUNTS=1` 才允许动真邮箱；
  凭据从 `~/.xremail/accounts.txt` 读，**不入库**（文件在仓库外）。
  验收脚本**不用 `SEARCH`**（见下），而是像同步引擎那样用
  `UID FETCH <UID 区间>` 拉回全部头自己找标记。
- **发信链路（P3）**：从「能读」到「能发」
  - **MIME 构建**（`crates/xremail-message::compose`）：按 RFC 5322 组装邮件；
    主题 / 发件人按 RFC 2047 编码，附件 base64，正文 quoted-printable
  - **发送队列**（`crates/xremail-store` 新增 `send_queue` 表与 DAO）：
    草稿 → 排队 → 抢占 → 提交 → 记结局，六态与 `mail_operation` 对齐
  - **服务层**（`apps/shell/src-tauri/src/service/compose.rs`）：
    落草稿 → **冻结字节** → 抢占 → 提交 → 记结局 → 回读确认
  - **Tauri 命令**：`save_draft` / `send_message`（参数与前端契约已对齐）
  - **三种结局必须分清**：`sent`（服务器回了 250，信确实出去了）/
    `uncertain`（命令已发出、没拿到应答，**不知道**出去没有，**不能自动重发**）/
    `failed`（明确没发出去）
  - **前置失败也要记账**：连不上、TLS 握手失败都发生在**任何命令发出之前**，
    所以这封信**肯定没发出去**——但仍要写回库（`retryable`）。
    否则那一行会永远停在 `running`，被下次启动的回收逻辑误判成「发到一半」，
    把一个「肯定没发」的信说成「可能发了」，用户就不敢重发了
  - **Bcc 只进信封**：不进邮件头，收件人之间互相看不见
  - **TLS 1.3 → 1.2 回退**：服务器拒绝 TLS 1.3 时自动重试整条链路，
    证书校验**一点不放松**（回退只发生在协议版本上），结果里写明回退
  - **前端写信面板**：收件人 / 抄送 / 密送 / 主题 / 正文 / 附件 /
    800ms 防抖自动存草稿；三种结局分开显示，`uncertain` 时**禁用发送按钮**
  - 协议层：`crates/xremail-protocol-smtp` 新增 `submit_message`
    （`MAIL FROM` / `RCPT TO` / `DATA` / `QUIT` 全链路 + 分阶段结果）
  - 用例：服务层 8 条、存储层 18 条、IPC 2 条、协议层 12 条——
    **全部不依赖真实邮箱**（服务器是 `xremail-testkit` 里的假 SMTP 服务器）
- **SMTP 只读探测**（2026-10-28）：新 crate `crates/xremail-protocol-smtp`
  - 诊断工具 `xremail-smtp-probe`：**只实现** `EHLO` / `STARTTLS` / `AUTH` / `QUIT` 四个动词，
    **代码里根本没有** `MAIL FROM` / `RCPT TO` / `DATA` —— 不是开关关掉，是不存在，探测阶段不可能误发邮件
  - 输出一份 markdown 报告：TLS 版本与套件、证书体检、EHLO 扩展逐条、
    认证机制与结果，以及可直接抄进配置的 `smtp_host` / `smtp_port` / `smtp_tls` / `smtp_auth`
  - 口令只从 `%USERPROFILE%/.xremail/accounts.txt` 读，**不出现在命令行**；报告里账号脱敏成 `u***@example.com`
  - TLS 1.3 被服务器拒绝时**自动改用 TLS 1.2 重试整条链路**，并在报告里写明回退——
    证书校验**一点不放松**，降级只发生在协议版本上
  - **实测结论**（用本项目自己的 Rust 客户端，原始报告在 [PROTOCOLS.md](docs/PROTOCOLS.md)）：
    - 163 `smtp.163.com:465`：TLS 1.3，`AUTH LOGIN` 认证通过（`235`），支持 `8BITMIME`、无 `SMTPUTF8`
    - QQ `smtp.qq.com:465`：TLS 1.2，`AUTH LOGIN` 认证通过（`235`），
      支持 `8BITMIME` / `SMTPUTF8` / `PIPELINING`，`SIZE 73400320`
    - 沃 `smtp.wo.cn:465`：❌ **连不上**——服务器只收**静态 RSA** 密钥交换套件，rustls 只支持 ECDHE，
      换 TLS 版本无效（已用 `openssl s_client` 交叉验证）⇒ 沃的发信需要另一个 TLS 后端
    - 126：待用户补授权码后再测
- **前端第一套自动化测试**（2026-10-28）：`vitest` + `@testing-library/react` + `jsdom`
  - 把上一批只做过**一次性手工验证**的交互断言**固化进仓库**（账号切换、文件夹切换、
    邮件选中、正文渲染、已读/星标按钮的空态与错误态等）
  - CI 的 Frontend job 现在跑 `vitest run` + `tsc --noEmit` + `vite build`
- **故障注入与端到端同步用例**（2026-10-28）
  - `crates/xremail-testkit`：假 IMAP/SMTP 服务器可注入故障
    （断连、超时、慢响应、提交后丢弃应答），端到端用例覆盖
    「提交后应答丢失 → 回读确认 → 不重复发」「瞬时故障重试成功」
    「崩溃重启后操作队列对账恢复」
  - **修掉一个真 bug**：`list_pending_operations` 的 SQL **漏了 `uncertain` 状态**——
    提交结果不明的那批操作会被**静默丢弃**，永远重试不了
- **写操作真正回写服务器**（2026-10-26）
  - 标记已读 / 未读、星标 / 取消星标不再只改本地：意图先落库（`queued`）→
    `UID STORE` 提交 → **回读 `FLAGS` 逐位确认** → 六态回报前端
  - `cmd::set_message_flag` 返回 `FlagIntentResult`：`queued` / `applied`（服务器**回读确认过**
    才为真）/ `state` / `message`——**服务器回 OK 不等于改动生效**
  - epoch 绑定：意图记下当时的 `uidvalidity`；epoch 变了就作废（`failed` / `epoch_changed`），
    旧 UID 不会套到另一封信上
  - `MessageService::list_messages` 叠加未完结意图（`overlay_pending_intents`），
    UI 立刻变；但那是**本地投影**，不等于服务器确认
  - 新增 `operation_dao::get_operation`（按 id 读回**这一条**的状态）
- **平台钥匙串（Windows 凭据管理器）**
  - `apps/shell/src-tauri/src/secrets/windows_store.rs`：
    `CredWriteW` / `CredReadW` / `CredDeleteW`，条目名 `xremail:account:<account_id>`，
    通用凭据类型，持久化 `CRED_PERSIST_LOCAL_MACHINE`
  - **不静默降级**：其它平台是 `secrets/unsupported.rs` 的桩（`get` 恒 `None`），
    保存时**如实报错**，绝不落到明文文件或「只存内存」
  - 数据库里只有不透明的 `account.secret_ref`，授权码本身不落库
  - 用例**不 mock**：真往这台机器的凭据管理器写条目再读回、覆盖写、删除，
    并用 `CredEnumerateW` 复查**零残留**
- **真实邮箱的写操作实测**（`xremail-flag-smoke`，脱敏记录在 `docs/PROTOCOLS.md`）
  - 163 / QQ / 沃：`UID STORE +FLAGS (\Seen)` 全 **OK**、回读确认**已生效**、
    **重复发同一个标记也回 OK**（不回 `NO [MODIFIED]`）
  - 因此「基线过期」那条分支在这三家不会被触发；真正兜底的是**回读确认**
  - `STORE` 必须以**可写**方式打开邮箱（`SELECT`，不是 `EXAMINE`）——三家都一样
- **同步引擎接进应用（UI 里能看到真实邮箱的邮件）**
  - 服务层：`MessageService`（文件夹 / 邮件列表 / 正文）与 `SyncService`（一次同步）
  - Command：`sync_account` / `list_folders` / `list_messages` / `open_message`
  - 前端：账户 → 点「同步」→ 文件夹 → 邮件列表 → 正文
  - 一次同步是**有界**的：连接 → 登录 → 逐个文件夹拉取 → 断开
    （还没有调度器 / AccountActor，不做常驻连接）
  - 文件夹名在库里存**线路原样**（modified UTF-7），显示时再解码——
    见下面那条 Fixed
- **真实邮箱端到端验收**（2026-10-26，脱敏证据）：
  - 163：6 个文件夹 / 4 封 / 正文 charset utf-8
  - QQ：6 个文件夹 / 71 封 / 正文 charset gb2312
  - 沃：6 个文件夹 / 6 封 / 正文 charset gbk
  - 三家的正文解码**零替换字符**（U+FFFD），即 charset 策略在真实邮件上正确
- **诊断工具 `xremail-sync-smoke`**：用真实账号跑一遍「连接 → 同步 → 落库 → 读正文」，
  只输出统计（文件夹数 / 邮件数 / 字符集），**不打印**凭据、地址、主题与正文
- **头与正文接进同步引擎**
  - 拉取阶段拉 `BODY.PEEK[HEADER]`（头）→ 落主题 / 发件人 / 收件人 / 日期 / Message-ID
  - **正文按需拉**（`BODY.PEEK[]`）→ 落 `message_body` 表
    （正文可能很大且带附件，一个邮箱几千封全拉一遍既慢又费流量）
  - 用 `BODY.PEEK[]` 而不是 `BODY[]`：后者会**置上 `\Seen`**，
    而「看正文」不一定等于「标记已读」
  - `xremail-message` 拆出 `ParsedHeaders`：拉头时没有正文
  - 存储层新增 `message_body` DAO 与 `update_has_attachment_by_uid`
- **`xremail-testkit` 的邮件夹具支持原始字节**（`with_raw`）：
  用于测非 UTF-8 编码（GBK 正文）

### Fixed
- **SMTP 证书体检补上「尚未生效」这一档**（2026-11-10，R68）
  - `parse_certificate` 把 `notBefore` 只格式化成字符串、从不比较，`expired` 只看 `notAfter`——
    一个「明天才生效」的证书会被报成「未过期」，看着像没问题
  - `CertificateInfo` 新增 `not_yet_valid`，CLI 的打印从「二选一」改成**三种状态**；
    文案刻意不把「尚未生效」写成「已过期」，那会把人引向错误的排查方向
  - 反向保护用例确认「刚签发的证书不该被判成尚未生效」，变异测试验证有效
  - 严重度低：真正的 TLS 校验由 rustls 完成，这只是诊断展示
- **SMTP 认证载荷用完清零**（2026-11-10，R67）
  - `auth_plain` 的 `payload`（明文 `\0user\0pass`）与 `encoded`（它的 base64）、
    `auth_login` 的 `pass_b64` 都是普通 `String`，离开作用域只是把内存标记为可复用、
    **不清零**——与 `xremail-api` 的 `SecretValue`（用 `Zeroizing`）不一致
  - **base64 不是加密**：还原一下就是口令，不该让它留在堆里等被换出到磁盘
  - 三处都改成 `Zeroizing<String>`；`zeroize` 本来就是 workspace 依赖（DKIM 私钥在用），
    **没有新增第三方包**
  - `auth_failure_is_reported_without_the_password` 钉住脱敏承诺没被破坏
- **发信队列的 `running` 补上租约：本进程卡住的那封信不再永远发不出去**（2026-11-09，R48）
  - 这是第 9 轮审计交回、第 10 轮核实的一条真实缺口。R32 给 `mail_operation` 补了
    「本进程认领 + 超时租约 → 收成 `uncertain`」，但**发信侧漏了同样的半边**：
    `send_queue` 只有按 `owner_boot` 的 `reclaim_running`（只收别个进程留下的），
    而 `claim_for_sending` 又拒绝 `running`
  - 后果：本进程在认领与写终态之间 `?` 早退或 panic，那封信**在本次进程里再也发不出去**，
    界面只显示「正在发送」，用户只能重启
  - 修法与 R32 对称；收成 `uncertain` 而不是重排——DATA 可能已经送达，重排等于让人收到两封
  - 变异测试：撤销修复 → 用例立刻变红
- **构建环境的两个只读 `$HOME` 障碍**（不是产品缺陷，记下来免得下一个人再踩）
  - Gradle wrapper 连自己的锁文件都建不了：`java.io.FileNotFoundException:
    /home/xl/.gradle/wrapper/dists/gradle-9.6.1-bin/…/gradle-9.6.1-bin.zip.lck (只读文件系统)`
    → `GRADLE_USER_HOME` 指到工作区
  - AGP 建不了 debug keystore：`java.io.IOException: Unable to create debug keystore in /home/xl/.android
    because it is not writable.` → `ANDROID_USER_HOME` / `ANDROID_SDK_HOME` 指到工作区
- **整封转发（作为附件）的邮件里，多个附件在库里互相顶掉**（2026-11-09，R45）
  - 这是 T10 的随机化属性测试撞出来的，不是人工审出来的：一封顶层是
    `message/rfc822` 的转发件，里面有两个附件时，两个都被记成部件号 `TEXT`
  - 根因在 `xremail-message` 的 `attachment_meta`：它拿「顶层 `Content-Type`
    是不是 `multipart/*`」决定要不要保留路径，而顶层 `message/rfc822` 也是容器、
    却不匹配这个前缀，于是**整条路径被清空**（`collect_leaves` 明明已经为内嵌
    邮件压了 `0` 占位）
  - 后果不只是编号难看：附件的 id 是 `{message_id}/{part_number}`，落库靠
    `ON CONFLICT (message_id, part_key)` 去重——两条同号就是一个 key，
    两个附件在库里互相顶掉，用户只看到一个
  - 判据改为「整封是不是只有一个部件」；整封单部件仍记 `TEXT`（`BODY.PEEK[0]`
    只取结构，不能用 `0`）
  - 证据：`crates/xremail-message/tests/parser_properties.rs` 的三条用例（最小复现 /
    随机化唯一性 / 单部件反向保护）
- **附件部件号与服务器对不上，下载会取错段**（2026-11-06，R44，提交 `6fddd07`）
  - 顶层 `multipart` 自己占 1 号，子部件应是 `1.1`、`1.2`……，此前记成 `1`、`2`，
    `BODY.PEEK[2]` 取回来的不是那个附件
  - `message/rfc822`（转发的邮件）按服务器的 `0` 占位，内嵌邮件里的附件号
    现在与 `BODYSTRUCTURE` 一致
  - 顶层只有一个部件时记作 `TEXT`，不用 `0`（`BODY.PEEK[0]` 只返回结构）
  - 顺带修掉：切分 MIME 时不再把正文先转成 UTF-8（GBK 正文会被换成乱码），
    也不再在段尾多带一个换行
- **文档表述与代码不符的六处，以及四处失效的行号**（2026-11-04）
  - **163 的能力事实被写反**。`CHANGELOG.md` 与 `docs/ARCHITECTURE.md` 的能力路由表把
    「`MOVE` 与 `UIDPLUS` 都没有」这一行标成「（163）」，于是读起来像「163 只能止步于标删」。
    实际 163 **有** `UIDPLUS`（`docs/PROTOCOLS.md` 的实测矩阵三处一致），缺的只是 `MOVE`，
    所以它走的是完整三步 `COPY` → 标删 → `UID EXPUNGE`。错的只在文档，
    代码按运行时能力路由（`transfer_push.rs` 的 `route_for`）不受影响。
    同一矛盾还残在 `docs/PROTOCOLS.md:347`「`UID MOVE` 支持情况需实测」，
    而同一节上文早已记为实测不支持。
  - **R22 的「残余」说反了**。原文写「附件类型黑名单仅前端提示，后端无 MIME 白名单」，
    实际后端有**扩展名黑名单**（`crates/xremail-core/src/attachment.rs:71 BLOCKED_ATTACHMENT_EXTENSIONS`，
    含双扩展名逐段比对），并由 `service/compose.rs:35` 的 `check_attachment_limits_all` **强制**；
    真正缺的只是 MIME 白名单。这条错的危害是会让后来的人重做一遍已存在的功能。
    顺带补上漏写的**单封合计 18 MiB** 上限（`attachment.rs:57`）。
  - **用例数三处不一致**。实测 `cargo test --workspace -- --list` = **803**，
    其中 1 个是 `#[ignore]` 的真实邮箱往返用例，实际执行 **802**。
    `docs/ISSUES_AND_RISKS.md` 的总览停在 778（与它自己的台账矛盾），
    `docs/TESTING_STRATEGY.md` 写 802 但同一文件给的计数命令会得 803。现已统一为
    「803（含 1 个 `#[ignore]`，实际执行 802）」并说明二者关系。
    另外 `TESTING_STRATEGY.md` 原写「计数：802 + 544（差 6 条）」，802−544=258 而非 6，
    已改成「Linux CI 计数自 2026-10-28 起未更新（当时 544），不是同一时点，不要相减」。
  - **四处行号全部失效**（文件对、行号错）：
    `lib.rs:140` → **`lib.rs:196`**（`reconcile_account` 调用，三处文档各错一次）；
    `actor.rs:460` → **`actor.rs:675`**（`pub struct Scheduler`）；
    `cmd.rs:548`/`:569` → **`cmd.rs:559`/`:580`**（`move_messages`/`delete_messages`）；
    `cmd.rs:576` + `compose.rs:495` → **`cmd.rs:750`** / **`compose.rs:501`**（`list_drafts`）。
  - **两处复选框与正文矛盾**：`docs/ROADMAP.md:87` 正文写「**已完成**」却仍为 `[ ]`
    （同一件事在 `:108` 已勾选）；`docs/ROADMAP.md:347` 把「通知 / 单实例」与托盘一起标为未做，
    而通知与单实例插件早已接线（`lib.rs:40`、`:47`），真正缺的只有**托盘图标**与**退出/恢复语义**。
- **认证失败的分类在两个方向上都错，其中一处让「认证终态」从未生效**（R25，2026-11-03）
  - `crates/xremail-protocol-imap/src/command.rs` 的 `classify_imap_error` 把
    **任意命令**的 `NO` 判成 `ProtocolError::Auth`。但 `EXAMINE` / `SELECT` 回 `NO`
    的原因绝大多数与凭据无关：文件夹在 `LIST` 与 `EXAMINE` 之间被别的客户端删掉/改名、
    变成 `\Noselect`、没有权限、`NO [INUSE] mailbox locked`、单连接邮箱数超限。
    经 `pull::map_protocol_error` 翻成 `AppError::Auth` 后，`actor` **直接退出**
    并把账户停在 `ReauthRequired`——**删掉一个文件夹就等于把整个账户判死**，
    而重新授权多少次都不会好
  - 反方向也错：`apps/shell/src-tauri/src/service/sync.rs:61` 的 `connect` 把
    **所有**连接/登录错误拍平成 `AppError::Protocol`，`connection.rs:261` 已经
    正确判出的 `ProtocolError::Auth` 在这里被抹掉，于是**错授权码被永久重试**、
    UI 一直显示「正在重连」——「认证失败终态」在产品路径上其实从未生效。
    `service/sent_copy.rs:147` 的「存副本」连接路径有同一问题
  - 修法：命令层的 `NO` 一律走 `connection::classify_imap_error`（`No` / `Bad` →
    `ProtocolError::Imap`，`Io` / `ConnectionLost` → `Io`），**只有登录路径才能产出
    `Auth`**；`pull::map_protocol_error` 改 `pub` 并由外壳层复用，保住
    「要人管」与「可重试」的区分
  - 用例 797 → **802**：`command_no_is_not_an_auth_failure`、
    `connection_lost_is_a_retryable_io_failure`、`map_protocol_error_keeps_auth`、
    `map_protocol_error_keeps_command_no_as_protocol`、
    `map_protocol_error_maps_transport_to_network`
- **`COPY` 重发会在目标文件夹多出副本**（2026-11-03，审计发现）
  - `crates/xremail-sync/src/transfer_push.rs` 的 `SubmitError::Uncertain` 分支用
    `update_operation_state` 收尾，**不自增 `attempts`**；而 `claim_for_push` 靠
    `AND attempts < ?` 卡重发、且 `state IN ('queued','retryable','uncertain')`
    ——于是 `uncertain` 的行可以被**无限重认领**，每轮重发一次 `COPY`。
    163 没有 `MOVE`，只能走 `copy+delete`，所以这条在主力账户上是可达路径
  - 新增 `operation::mark_operation_uncertain`（与 `mark_operation_retryable` 同样自增，
    只是落在 `uncertain`），`uncertain` 出口改走它
  - `copy+delete` **重发前先探测**：`message_dao::find_message_id_by_uid` 取本地
    `Message-ID` → `EXAMINE <目标>` + `UID SEARCH HEADER Message-ID "<id>"`，
    探到就跳过 `COPY`、只补删除那一半（`submit(..., copy_already_done)`）
  - **取不到 `Message-ID` 就不重发**，停在 `uncertain` 等对账 / 用户确认
- **`LIST` / `EXAMINE` / `SELECT` 没有命令超时**（2026-11-03，审计发现）
  - `crates/xremail-sync/src/net.rs` 自己写着「async-imap 没有命令超时，必须有」，
    但只有部分命令走了 `run_checked_with_timeout`；连接能建、服务器不回话时
    该账户的同步会**永久卡住**（`catch_unwind` 救不了 hang）
  - 现补上 `list_folders_with_timeout` / `examine_checked_with_timeout`，
    `SELECT` 也包上超时；单一数值来源改为 `SyncPolicy::command_timeout`
- **`wait_for_change` 不在 panic 边界内**（2026-11-03，审计发现）
  - 边界原先只包了 `tick`；`wait_for_change`（含 `connect` + `idle_once`）是
    `select!` 的另一条分支、**裸的**——它 panic 会撞到 tokio 任务边界，任务
    静悄悄死掉，`reap_finished` 清掉句柄后该账户再不同步，直到重启应用。
    R13 说「所有 panic 停在这里」，那就得真的盖住这条分支
  - 现改为「包边界的壳 + `wait_for_change_inner` 本体」：panic 记 `error` 日志、
    等满 `wait` 后按到点处理（不提前返回，否则 `connect` 里反复 panic 会成为忙循环）
- **文档里 5 处与代码不符的表述**（2026-11-03）：
  `Scheduler::is_scheduled` 这个函数**不存在**（真正做这件事的是
  `cmd::ensure_scheduled`）；「163 没有 `UIDPLUS`」与 `docs/PROTOCOLS.md` 的实测
  矛盾（163 **有** `UIDPLUS`，缺的是 `MOVE`）；用例数停在旧快照；
  `docs/CONTRIBUTING.md` 还写着「仅装有 JDK 8 且无 Android SDK」（ 2026-10-21 已装好）
- 用例 792 → **797**（`crates/xremail-sync/tests/push_transfers.rs` 新增 3 条：
  重发不多出副本 / 无 `Message-ID` 拒绝重发 / 次数到顶一条命令不发；
  另 2 条为命令超时用例）
### Known limitations
- **Android 侧只验证到「能编出 APK」**：阅读 / 发信 / 本地缓存、前台 / 后台 / 息屏 / 进程恢复、
  通知、Keystore **一条都没有在设备上跑过**。本机没有 Android 设备也没有模拟器
  （`adb devices` 为空，无 `emulator/` 与 `system-images/`，无 AVD），这些不是能「推断」出来的，
  所以 R08 **维持「待验证」**，对外仍只能称 Windows alpha
- `lib.rs` 目前**没有** Android 生命周期接线：只处理 `RunEvent::Exit`，没有
  `Resumed`/`Suspended` 分支；通知插件的权限已在 manifest 里，但应用从不调用通知 API、
  也不申请 Android 13+ 的运行时权限

### Changed
- **UI 按 UI_DESIGN.md 三期改造完成**（2026-11-10）
  - 一期：Tailwind v4 地基（CSS-first，无 config），`@theme inline` 映射到
    tokens.css 既有变量——不动任何像素，可随时回滚
  - 二期：shadcn 形态的 Sheet（Radix 源码进仓库）；「账户与设置」收进**非模态**
    右侧抽屉，顶栏只留 ⚙；骨架屏加载态（占位与内容同形状，不跳变）；
    通知条改左下角浮层 Toast（成功类 6s 自动消失，错误常驻）
  - 三期：键盘导航 j/k/↓/↑（选邮件）、s（星标）、c（写信）、
    Esc（先关浮层再清选择）、Ctrl/⌘+K（开抽屉聚焦搜索）
  - **硬规则有专项用例**：焦点在输入框里时单键导航必须全部失效
  - 性能全程在预算内：CSS gzip 7.5 KB（≤12）、JS gzip ~106 KB（≤110）、
    滚动 0/684 帧超 50ms、切换 <300ms（每期同闸门，零真实退化）
  - 测试：单测 95 → **99**（+4 键盘）、E2E 12；jsdom 无 scrollIntoView 的
    环境差异用 typeof 守卫解决
- **界面改成三栏布局（参考 Gmail），并为「不卡」建立可量化的基线**（2026-11-10，R73）
  - 改造前所有区块纵向堆在一页里：登录后要先滚过大半屏的设置区才看到邮件，
    而且每次滚动都在布局整页
  - 现在：**文件夹 | 邮件列表 | 正文**三栏；整页 `height: 100vh`，只有中栏与右栏各自滚动；
    设置区（添加账户 / 授权码 / DKIM / 搜索）默认**收起**
  - 性能上做了四件事：三栏都加 `contain: layout paint`（点开邮件只重绘右栏）、
    各栏独立 `overflow: auto`（滚动不触发整页回流）、列表项不放 transition / box-shadow
    （避免每帧重新合成）、窄屏（< 900px）退回单栏堆叠
  - 顺带修掉一个注释与实现不符：`DkimPanel` 写着「默认折叠」，代码里却从来没有 `<details>`
    ——单页布局时不明显，改成三栏后它把左栏整个撑满
  - **实测**（`apps/shell/e2e/performance.spec.ts`，带数字上限防退化）：
    500 封邮件滚动**最长一帧 17.5ms、0/877 帧超过 50ms**；
    连点 20 封切换**中位 50ms / 最慢 58ms**
- **清理未使用的依赖与清单声明，并用「未来的 stable」验证**（2026-11-10，R69）
  - 用 `cargo +nightly clippy`（比 stable 新 2 个版本）**模拟未来的 stable**，
    一次暴露 13 个警告——这正是「某天 CI 突然变红、且不可复现」的具体形态
  - 修掉其中可行动的：
    - `xremail-crypto` 未使用 `xremail-core`、`xremail-testkit` 未使用 `xremail-core` 与 `tracing`
      （已核实代码与测试都没用到，也没有 re-export）
    - 根 `Cargo.toml` 的 `anyhow` 声明**没有任何 crate 使用**（代码里零引用）
    - `encoding_rs` 在 `xremail-message` 里用版本号直接声明、没走 workspace，已统一
    - `[workspace.package]` 的 `repository` **没有任何 crate 继承**，已给 11 个 crate 补上
    - 一处 `.map_or_else(|| (), |v| panic!(…))` 被 `unnecessary_lazy_evaluations` 点名，改成 `if let`
  - **结果：nightly 下从 13 个警告降到 1 个**，剩下那个是 `xremail-crypto` 被标「未使用」——
    那是预期的（它服务 P6/L2，当前没有调用方），已在 `Cargo.toml` 里写明理由
  - **没改** `rust-toolchain.toml` 的 `channel = "stable"`：钉死会让安全更新变慢，
    不钉死就每年要处理一两次工具链升级——这是个需要人来做的决定
- **文档与代码对齐**（2026-11-02）：一次全量审计，把文档里**与实际代码不符**的表述改掉。
  审计的标准是「文档说的能不能在代码里找到对应的东西」，不是「读起来顺不顺」
  - **README**：技术栈表改成**实际用的**——原生 CSS、React 内置 `state`、自写 `contentEditable`、
    自研 SMTP 协议层；**没有** TanStack Query / Zustand / Tailwind / shadcn / TipTap / lettre
    （TanStack 兄弟只是「与 SQLite 的取舍」里的参照物，**没进依赖**）
    - 摘要与状态表从「P0 / P1」更新到「P0–P3 已出关」
    - 用例数 397 → **687**（Rust）+ **67**（前端，12 个文件）
    - 架构图重排（原来那幅连边框都对不齐），并标出 `xremail-provider` 与 `apps/sync-server`
      **尚未创建**
  - **CONTRIBUTING**：前端约定里「用 TanStack Query / Zustand / Tailwind / 虚拟列表」改成实际做法
    （**不引入第三方前端依赖**，唯一例外是 Tauri 官方插件 `@tauri-apps/plugin-dialog`）
  - **SECURITY §3**：补 **§3.0 实现现状**，把与规范的**偏离**写明——
    **未用 sandbox iframe**（消毒后直接渲染进 DOM，代价是消毒器漏网就直接生效）、
    未实现「显示图片」与追踪器面板、未实现重定向警告页、`ammonia` 未引入
  - **PROTOCOLS**：删掉「尚无本项目真实账号的完整验证记录」这句自相矛盾的话；
    「还没用真实测试账户真的发过一封」等**已被 2026-10-29 实测推翻**的表述改成实测结论
  - **ARCHITECTURE**：`xremail-provider` 标注尚未创建
  - **ADR-0001**：补「后续偏离」小节（决策本身不变），记下前端选型实际被偏离的部分
  - **PLAN.md**：顶部横幅补一句「原文的前端依赖清单也已过时」
  - **ENVIRONMENT**：`rust-version` 1.88 → **1.94**；补一条本机 **`python3` 是 Microsoft Store 空壳**
    （直接退出 49、什么都不输出）——本地跑 `scripts/check_doc_links.py` 得用 `python`
  - **ISSUES_AND_RISKS / ROADMAP / TESTING_STRATEGY**：用例数与「收发未实现」的表述更新为当前事实；
    R14 的用例数更新为 687 + 67；**R23 转为已缓解**——搜索落在本地 FTS，**不依赖服务端 `SEARCH`**
    （163 上也能搜）；ROADMAP 的 T6–T8 补上 SMTP 侧假服务器与 T8 的覆盖边界
  - **顺手修掉两个 markdown 表格的渲染缺陷**（都是**老问题**，不是这次改出来的）：
    README 的阶段表表头只有一列（数据行却是两列）；`docs/ENVIRONMENT.md` 的「坑」表里
    `cmd` 那行的 `|` **没转义**——GFM 会把这一行拆成四列

### Fixed
- **`push_flags` 会默默吃掉移动/删除意图**（2026-11-03）：
  它把所有待提交操作按 UID 分组后算「净变化」，而移动/删除的 flag 载荷是空的，
  于是「净变化为零」被当成「已经在服务器上生效」直接标成 `succeeded`。
  结果：**邮件既没搬走也没删掉，队列却空了**——一次静默数据丢失
  （用户以为搬了，实际什么都没发生）。
  修法：`push_flags` 只取 `kind = flags` 的操作（旧 epoch 的隔离也同理，
  否则 `quarantined` 计数会翻倍）。
  回归用例：`push_flags::move_intents_are_left_to_the_transfer_channel`
  + `push_transfers::sync_account_drains_queued_moves_without_the_ui_open`。
- **`APPENDUID` 的 UID 区间被多减了 1**（2026-11-03）：
  `crates/xremail-protocol-imap/src/append.rs` 把
  `UidSetMember::UidRange` 当成**半开区间**处理（`end().checked_sub(1)`），
  而 imap-proto 是用 `fst..=snd` 构造的**闭区间**。
  于是 `APPENDUID 123 5:7` 会被报成 **6**——发信副本记下的 UID 全错一位，
  后续按 UID 回查副本会查错信。现实中很少碰到是因为主流服务器
  一次只 append 一封（回单个 UID 而不是区间），属于「极少触发但错得很难查」的那类。
  修法：直接取 `range.end()`，并把「闭区间」写进注释。
- **163 的 IMAP `SEARCH` 只支持 `ALL`**（实测 2026-10-29）：
  `SUBJECT` / `HEADER` / `TEXT` 等条件**静默返回空结果**，连 `NO` 都不回——
  看起来像「没搜到」，实际是「这个服务器不支持这种搜法」（QQ 上正常）。
  同步引擎**不受影响**（它走 `UID FETCH`，从不用 `SEARCH`），
  但这意味着**服务器端搜索功能在 163 上会静默失效**（新增 R23）。
- **文档里的相对链接断了 3 处**（`scripts/check_doc_links.py` 逮到）：
  `cmd.rs` 与 `service/compose.rs` 里的 `docs/ARCHITECTURE.md` 层级写错，
  `xremail-protocol-smtp/tests/submit_against_fake.rs` 指向了不存在的
  `../probe_against_fake.rs`。
  修法：按文件所在目录的实际层级重算 `../` 数量；
  顺带把**全仓库**同类链接一起校正（脚本每文件只报第一处断链，漏的会更多）
- **EXAMINE 必须用线路上的原名（modified UTF-7）**——两个真 bug，靠真实邮箱才暴露：
  - 症状：163 的 6 个文件夹只同步出 1 个（INBOX），其余 5 个回
    `NO EXAMINE Folder not exist`
  - 根因：`LIST` 返回的名字是 UTF-7 形式（`&XfJT0ZAB-`），解码成「草稿箱」后
    拿去 `EXAMINE`，服务器只认自己给出的那个形式
  - 修法：库里存**线路原样**（`folder.remote_name`），显示时再解码
    （新增 `FolderDto`，`name` 解码、`rawName` 保留原样）
  - 这条是 IMAP 的通病，不是 163 独有
- **UID 是邮箱作用域的**：会话当前选中的邮箱是另一个文件夹时，
  `UID FETCH <uid>` 返回**空结果**（不是报错）——于是「拉到了 0 封」
  看起来像「服务器上没有这封邮件」
  - 症状：正文一直拉不到（`fetch_body` 静默返回 `false`）
  - 修法：`fetch_body` 拉之前先 `EXAMINE` 对应文件夹
  - 回归用例：`body_is_fetched_even_when_the_session_is_on_another_folder`
    （变异测试确认：去掉那句 EXAMINE，该用例会失败）
- **假服务器把 `BODY[]` 当成只有正文**：
  RFC 3501 §6.4.8 里 `BODY[]` 是**整封邮件**（含头），`BODY[TEXT]` 才是纯正文。
  弄错的话客户端会把正文当成整封邮件去解析
- **P0 出关：Command → 服务 → SQLite → React 真实列表**（不再返回硬编码空列表）
  - 启动时解析数据目录（桌面 `%APPDATA%/com.xremail.app`，Android app 内部存储）、
    打开连接池并**应用迁移**；迁移失败则启动失败——**不降级**为空库或旧库
  - 服务层 `apps/shell/src-tauri/src/service/`：Command 只做「参数校验 → 调服务 → 返回 DTO」，
    业务在服务层；服务层**不认识 Tauri**，发事件是 Command 的事（**状态提交后才发**）
  - Command：`list_accounts`（真实查询）、`create_account`、`delete_account`
  - `xremail-api` 新增应用服务 DTO `NewAccountRequest`：入参校验 + 转领域对象（14 个用例）
  - 前端：账户列表 + 新增表单 + 删除；**不引新依赖**（未用 TanStack / Tailwind）
  - 事件当**刷新提示**用：收到 `account://added` 等事件后重新查询，不解析载荷
  - 服务商只作标记，**服务器地址由用户手填**：163/QQ/沃 的参数尚未实测（R01/R02），
    不能按品牌推定
- **IPC 层测试 8 个**（`tauri::test` 的 `mock_builder` + `get_ipc_response`）：
  走真实 IPC 链路，**锁死 camelCase 线格式**（`displayName` / `imapPort` / `server.imapHost`）
  - 这层测试当场抓到一个真 bug：`ServerConfig` 漏了 `rename_all`，
    嵌套对象仍是 snake_case——已修 core
- **服务层测试 10 个**：空库返回空数组、创建后可查回、校验失败不落半条记录、
  更新不换 id、**关掉连接重新打开同一个文件数据仍在**（证明真的落盘）
- **core 的过线结构体统一 camelCase**：`ServerConfig` / `Folder` / `Message` /
  `Address` / `MessageBody` / `Attachment`（`Account` 已有）。
  落库不这套命名——`xremail-store` 有自己的列映射，两边互不影响
- **`xremail-sync` crate（P0 / T2，同步纯函数）**：UID 差分、epoch 切换计算、bit 意图合并
  - `UidSet` / `diff_uids`：算出「要拉取的」与「服务器已删的」，升序稳定输出
    - `UidDiff::retain_local`：本地草稿与未发送内容有**独立归属**，不能因为服务器上没有就被清掉
    - `UidDiff::chunk_added`：摘要分批拉取，避免一次拉太多
  - `plan_epoch_transition`：epoch 未变 → 增量；首次同步 → 全量；epoch 变了 → **重建快照**，
    并列出绑定旧 epoch 的待提交操作（**不得**套用到新 UID 上）
    - `EpochTransition::requires_full_fetch()` 区分「全量拉取」与「重建快照」两个概念
  - `FlagIntent` / `apply_intents` / `plan_flag_update`：服务器快照与本地待提交意图**分开**保存
    - 同一 bit 的多个意图按 `seq` 排序：后者覆盖前者（`+Seen` 与 `-Seen` **确实会冲突**）
    - 只发**净变化**的位，净变化为空时**不发命令**（不整组替换 FLAGS）
    - 绑定旧 epoch 的意图进 `FlagPlan::stale`，不参与计算
    - `MAX_REPLAN_ATTEMPTS = 3`：MODIFIED 后重新读取并重算的次数上限（重试有界）
  - 本阶段**只含纯函数**：无网络、无 IO，可直接在 CI 跑；账户调度与 IMAP 会话属 P2
- **标记回写链路**（`xremail-sync::push`）：本地意图 → 队列 → 服务器 → **回读确认**
  - 流程：读快照 → 重放意图算净变化 → 发 `UID STORE` → **回读确认** → 更新状态
  - **三种失败分得很清**：
    - 服务器明确回 `NO` → `retryable`（它告诉我们**没执行**）
    - 服务器回 `NO [MODIFIED]` → `retryable`（基线过期，重新读快照重算，**有界**）
    - 连接断了 / 超时 → `uncertain`（**不知道**服务器执行了没，盲目重试可能重复执行）
  - 只发**变化的位**，不整组替换 `FLAGS`；增删同时存在时发两条命令
  - epoch 变了 → 绑定旧 epoch 的操作被**隔离**（`failed`），绝不套用到新 UID 上
- **操作队列**（`mail_operation` 表 + `xremail-store::dao::operation`）
  - 状态机：`queued` / `running` / `succeeded` / `retryable` / `failed` / `uncertain`
    （**这些取值就是 DB 列的取值**，改枚举名要配数据迁移）
  - `seq`（本地单调序号）决定同一封信上多个意图的定序——不能用 `created_at`（同毫秒会撞号）
  - 自相矛盾的意图（同一 bit 既置位又清除）**入队时就拒绝**
  - 新增迁移 `0002_operation_seq.sql`（顺带验证了升级路径）
- **同步引擎的拉取链路**（`xremail-sync::pull`）：服务器 → 差分 → SQLite
  - 固定流程：`EXAMINE`（拿 epoch）→ epoch 判定 → 取远端 UID 集合 → 差分 →
    拉取新增 → 清理服务器已删 → 更新游标
  - 远端 UID 集合用 `UID FETCH 1:* (UID)` 而不是 `UID SEARCH ALL`：
    **FETCH 是 IMAP 的必备命令，SEARCH 是可选扩展**
  - 拉取**只读**（`EXAMINE`），标记回写另走 `SELECT`
  - 报告区分三种情况：`first_sync`（首次）/ `epoch_changed`（旧世代作废）/ `full_fetch`
  - 用 `run_checked` 而不是 async-imap 的 `uid_fetch`——
    后者会**吞掉**服务器的 `NO`，那样会把整个文件夹的缓存当成「服务器已删」清掉
- **`xremail-store` 的邮件 DAO**（`dao/message.rs`）
  - 按 `(account_id, folder_id, uidvalidity, uid)` upsert（四元组才是身份，
    `Message-ID` 仅作线索）
  - `list_message_uids` 只取 UID，**不取正文**——差分只需要知道「有哪些」
- **`xremail-store` 的跨表同步提交**（`dao/sync.rs`）
  - 一轮文件夹同步要同时动三张表，必须**原子**：
    清旧世代 → 写新邮件 → 删服务器已删的 → 更新游标
  - 顺序很重要：先清旧世代再写新的（反过来会把刚写进来的新世代邮件也删掉）
- **`xremail-protocol-imap` 的安全 `examine_checked`**：打开文件夹并返回
  epoch / 邮件数等概况，服务器拒绝时**一定**返回错误
- **假 IMAP 服务器补齐同步链路命令**：`UID FETCH` / `UID SEARCH` / `UID STORE`
  - 假服务器有了**共享可变状态**：`STORE` 改完，后续 `FETCH` / `SEARCH` 能看到
  - `sequence` 模块：sequence set 解析（`1:10,15,20:*`）——**在服务端**，
    因为客户端（async-imap）是把 sequence set 当**原始字符串**发的
  - `search` 模块：搜索键解析与求值（标记类 / `UID` / 头字段 / 正文 / `NOT` / `OR` / 日期）
  - `fetch` 模块：取件项解析（`UID` / `FLAGS` / `RFC822.SIZE` / `BODY[HEADER.FIELDS (...)]` / `ENVELOPE`）
    与 `STORE` 参数解析（`+FLAGS` / `-FLAGS` / `.SILENT` / `UNCHANGEDSINCE`）
  - 完整实现 IMAP 状态机：未选中邮箱时 `FETCH` / `SEARCH` / `STORE` 都会被拒；
    `EXAMINE` 打开的只读邮箱拒绝 `STORE`
  - `BODY[...]`（不带 PEEK）会**置上 `\Seen`**，`BODY.PEEK[...]` 不会
  - 条件写 `UNCHANGEDSINCE (<modseq>)`：**全有或全无**——
    只要有一封的 modseq 对不上，整条命令都不改任何东西
- **`xremail-protocol-imap` 的安全命令层** `run_checked`（**重要**）
  - 自己跑命令、自己读响应、自己判定 `OK` / `NO` / `BAD`
  - 服务器回 `NO` / `BAD` 时**一定**返回错误
  - 错误信息里带上 response code（`[READ-ONLY]` / `[MODIFIED]` / …）
- **`xremail-testkit` crate（T7 前置，假 IMAP 服务器）**：协议层的测试**不再依赖真实邮箱**
  - 真邮箱只能复现**一种**情况；假服务器把各家服务商的怪癖做成**开关**，在 CI 里全部跑一遍
  - `ServerConfig`：能力集、`require_id`（163 式的“先发 ID”）、
    `reject_examine`、初始文件夹与邮件、UIDVALIDITY
  - `Fault`：登录后断连 / 对某命令不响应（客户端超时）/ 拒绝某命令 / 认证失败
  - 自签证书：真实客户端不可能信任它，测试要**显式**把那一张 CA 交给客户端
    （`TlsTrust::Roots`）；生产路径始终用系统根，**没有**跳过校验的开关
  - **认证前拒绝一切命令**（连 `LOGOUT` 都回 `BAD`）——复现 163
  - 文件夹名按 **modified UTF-7** 编码上线（`&XfJT0ZAB-` = 已发送）
- **本机测试账号库**（`%USERPROFILE%\.xremail\accounts.txt`，**仓库之外**）
  - 分段格式：`[163]` / `[qq]` / `[wo]` / …，每段 `user` + `password`
  - 配套模板 `accounts.template`（含各服务商的**开启路径与申请入口**）
  - 探测工具新增 `--list`（只列已配置的 provider，账号脱敏）与 `--provider`
  - 授权码用 `SecretValue` 包着：丢弃时清零，`Debug` 不打印内容
- **`xremail-protocol-imap` crate（P1，IMAP 协议层）**：rustls + async-imap，只读探测工具
  - `connect_and_login`：隐式 TLS（强制校验系统根，**无**明文/跳过校验开关）→ LOGIN → **ID（RFC 2971）**
  - `utf7`：IMAP 邮箱名的 **modified UTF-7** 编解码（RFC 3501 §5.1.3）——不解码就认不出「已发送」
  - `probe`：**严格只读**探测（只发 LOGIN / CAPABILITY / ID / LIST / EXAMINE / STATUS / IDLE+DONE；
    **绝不** STORE / MOVE / COPY / DELETE / EXPUNGE / APPEND，也不发 SMTP 邮件）
  - 探测输出**脱敏**：不打印授权码、完整邮箱地址、邮件内容；文件夹按特殊用途属性判角色
  - 凭据从**本机文件**读（仓库之外），不进命令行参数、不进日志
- **P1 实测：163 邮箱（2026-10-22）**——两个足以改变设计的发现
  - **必须先发 `ID`（RFC 2971）自报身份，否则进不去邮箱**：
    不发 `ID` 时 `EXAMINE` / `SELECT` 一律被回 `NO … Unsafe Login`。
    `ID NIL` 不算；内容任意（含空串）都算。已实现为「服务器宣告 `ID` 能力时登录后立即发送」
  - **没有 IDLE、没有 CONDSTORE** ⇒ 163 只能用**轮询**，且 flag 写入要走
    「读快照 + 重放本地意图 + 回读确认」路径（ARCHITECTURE §6）
  - 另：**无 UIDNEXT**（`STATUS` 接受该字段但不给值）⇒ 同步游标不能基于 UIDNEXT；
    **无 MOVE** ⇒ 移动走 `COPY` + `\Deleted` + `EXPUNGE`；
    `EXAMINE` **不报 UNSEEN**（要用 `STATUS`）；请求 `HIGHESTMODSEQ` 会得到
    `BAD Parse command error`（不要无条件请求）
  - 邮箱名在线路上是 modified UTF-7（`&XfJT0ZAB-` = 已发送）
  - UID 空间很大（实测 1792658701~1792658704，**不是** 1..N）
  - 认证后能力集、文件夹特殊用途属性、永久标记（不含 `\Recent`）等已记入
    [PROTOCOLS.md](docs/PROTOCOLS.md)；IDLE/CONDSTORE 矩阵的 163 行已从「待实测」改为实测结论
  - Rust 用例总数 194 → **212**（新增 18 个：utf7 编解码 11 + 凭据读取/脱敏 7）
- **T2 用例 57 个**（单元 47 + 场景 10）：首次同步 / 增量同步 / 服务器删除保留草稿 /
  epoch 变化重建并隔离旧意图 / 跨 epoch 作差是错的 / 本地视图合并 / 点了又取消不发命令 /
  MODIFIED 后按新快照重算 / 旧意图绝不发给服务器 / 只发变化的位
  - **已用 7 个变异验证用例有效性**：不排序 seq（1 个失败）、空 delta 也发命令（6）、
    不隔离旧 epoch 操作（2）、added/removed 反转（6）、忽略 remove 位（5）、
    忽略 epoch 校验（1）、保留策略失效（2）——全部被捕获
  - Rust 用例总数：99 → **163**（含 core 新增 7 个 flags 集合代数用例）
- **`xremail-core` 的 `MessageFlags` 补齐集合代数**：`union` / `intersection` / `difference` /
  `is_empty` / `bits`——bit 意图合并需要「快照 → 最终」的净变化计算
- **`xremail-store` crate（P0 存储地基）**：SQLite 持久化层，含迁移、连接池与账户/文件夹 DAO
  - 迁移 `crates/xremail-store/migrations/0001_init.sql`：10 张表——`account` / `folder` / `message` / `message_body` / `attachment` / `mail_operation` / `send_queue` / `local_secret` / `label` / `rule`
  - **身份不变量落到 schema 层**：`message` 表的 `UNIQUE (account_id, folder_id, uidvalidity, uid)`；RFC 5322 `Message-ID` **不建唯一约束**（仅作线索）
  - 复合外键 `message(account_id, folder_id) → folder(account_id, id)`：保证邮件的账户与文件夹一致
  - 删除策略逐表声明：缓存类（文件夹/邮件/正文/附件）随上级级联；`mail_operation` / `send_queue` 对邮件只做**逻辑引用**，不随邮件删除（epoch 切换后需隔离核对）
  - `CHECK` 约束覆盖 IMAP 语义：`uid > 0`（UID 0 非法）、`uidvalidity > 0`、端口范围、`poll_interval_secs > 0`
  - `mail_operation` 与 `send_queue` **分表**，不混为一个 outbox（两者的重试语义不同）
  - 连接池：WAL、`busy_timeout`、**显式 `PRAGMA foreign_keys = ON`**（SQLite 默认不启用外键）、有界连接池
  - 错误分类：约束冲突（唯一键/外键/CHECK）映射为 `AppError::Invalid`（**不可重试**），其余 sqlx 错误映射为 `AppError::Storage`
- **T1 测试用例 30 个**（`xremail-store` 单元 6 + 集成 24）：迁移建表、迁移幂等、外键确实生效、身份唯一约束、不同 epoch 相同 UID 共存、Message-ID 非唯一、复合外键、CHECK 约束、逐表删除策略、账户/文件夹往返、upsert 语义、未知枚举不静默回退、事务整体回滚、文件库重开可读
  - Rust 用例总数：4（core）→ **34**（P0 阶段中间态，现为 99）
- **sqlx 离线查询数据 `.sqlx/`**：SQL 查询在**编译期**对 schema 校验（列名、列数、类型、可空性），CI 无需起数据库
  - 已用反例验证：写错列名 / 列数不匹配 / 类型不匹配 / 可空性不匹配，四类均在编译期报错
- **CI 新增「Verify sqlx offline data」步骤**：`cargo sqlx prepare --workspace --check` 校验 `.sqlx/` 与迁移/查询同步，过期即失败（已验证：故意改查询后返回 exit 1）
- **`docs/ENVIRONMENT.md` §2.6「Ubuntu 26.04 开发机准备清单」**：发行版基线对照表（glibc 2.43 / GCC 15.2 / LLVM 21.1 / Python 3.14 / Rust 1.93.1 / OpenJDK 25）、已核实的系统依赖包名、Rust/Node 装法、Android 组件、与 CI 的联动（`ubuntu-latest` 将于 2026-11 切到 26.04）、glibc 兼容下限对 .deb 的影响、WSL2 预演方式
- **本机补装工具**：`cargo-audit` 0.22.2、`cargo-deny` 0.20.2、`git-filter-repo`（pip --user）
- **Android 工具链就绪**（2026-10-21）：JDK 17.0.20.1、Android SDK（cmdline-tools 13.0 / platform-tools 37.0.1 / platforms android-36 与 37.0 / build-tools 36.0.0 与 37.0.0）、NDK 27.2.12479018、cargo-ndk 4.1.2、4 个 Rust Android target；均装在用户目录（本机非管理员）
  - 用户级环境变量：`JAVA_HOME`、`ANDROID_HOME`、`ANDROID_SDK_ROOT`、`NDK_HOME`
  - `tauri android init` 成功，`cargo build --target aarch64-linux-android` 产物经 `file` 确认为 ARM aarch64 ELF
  - **未完成**：APK 打包卡在 Windows 符号链接权限（非管理员且未重登录，开发者模式已写入注册表待生效）——见 [ENVIRONMENT §1.8](docs/ENVIRONMENT.md)
- 环境特性与平台差异文档 `docs/ENVIRONMENT.md`：本机 shell / 代理 / DNS / 工具链限制，各平台 WebView 与 API 差异，写脚本与 CI 前的检查清单
- 测试用例扩写计划 `docs/TESTING_STRATEGY.md §5`：T1–T10 优先级、每项的「算完成」标准、用例数作为进度指标
- CI 新增「Report test case count」步骤，输出 Rust 用例数到 job summary
- 详细技术规划文档 `docs/PLAN.md`（v1.2，历史参考），含协议核查、UI 选型对比、数据模型、同步引擎设计
- 架构基线 `docs/ARCHITECTURE.md`：身份 epoch、IMAP-aware 编排、IDLE 单一所有者、bit 级操作意图、不确定结果、FTS 投影、L2 CAS 契约
- 各服务商协议参数速查 `docs/PROTOCOLS.md`（含实测记录模板）
- 安全模型与威胁分析 `docs/SECURITY.md`
- 验收门槛路线图 `docs/ROADMAP.md`
- 贡献指南 `docs/CONTRIBUTING.md`
- 测试策略 `docs/TESTING_STRATEGY.md`：测试分层、故障注入、协议/安全/平台验收场景
- AI 开发工作规范 `docs/AI_DEVELOPMENT_GUIDE.md`：不变规则、职责边界、可复用任务模板、常见错误
- 风险登记册 `docs/ISSUES_AND_RISKS.md`：MECE 分类、纠错记录、关闭条件
- 架构决策记录：ADR-0001（UI 技术栈）、ADR-0002（同步深度）、ADR-0003（Gmail OAuth，暂缓）
- Tauri 2 + React 19 + TypeScript 工程脚手架（`apps/shell`）
- `xremail-core` crate：领域模型（账户、文件夹、邮件、Flags、地址、事件）
- `xremail-core` 错误分类单测 4 个
- `xremail-core` 的 `Folder` 新增 `uidvalidity` 字段（该文件夹的 **epoch**）：它本来就是领域概念（ARCHITECTURE §4.3 要求按 epoch 切换游标），此前模型里没有，DAO 无法往返

### Changed
- **「QQ 不存副本」的结论是错的，已修正**（2026-10-30）：
  第一次实测在提交完**立刻**查「已发送」，QQ 是空的，当时记成「QQ 不存」。
  **QQ 其实存**，只是**异步落盘**，实测 **30–90 秒**后才看得到。
  ⇒ 查副本**必须轮询**（真邮箱用例现在等满 120 秒），
  查一次就下结论会把「还没到」误判成「没有」。
  受影响的文档：`docs/PROTOCOLS.md`、`docs/ISSUES_AND_RISKS.md` R18、
  `docs/ARCHITECTURE.md` §5、`docs/ROADMAP.md` P3。
- **文档对齐到发信链路的真实状态**（2026-10-28）：
  - `docs/ARCHITECTURE.md` §5：补上发信路径的**实现落点**（落草稿 → 冻结字节 → 抢占 →
    提交 → 记结局 → 回读）与三条硬规则（冻结后不可改、不确定不自动重发、前置失败也要记账）；
    补上协议层的**范围边界**（不做 `RSET` / 不做流式 `DATA` / 不做 `SMTPUTF8` /
    `XOAUTH2` 明确不支持——这些是有意的取舍）
  - `docs/ROADMAP.md`：P3 的提交路径、草稿、附件、envelope 分离、不确定态、
    T8 续写勾上；**DKIM 与「已发送副本 `APPEND` 行为」明确标为未做**
  - `docs/PROTOCOLS.md`：新增「提交路径的状态」——三家的探测结论**直接决定能不能发**，
    并写明「还没用真实测试账户真的发过一封信」
  - `docs/ISSUES_AND_RISKS.md`：R04（发送已定义并实现）、R18（提交路径已实现但未实测真发）、
    R21（「换 TLS 版本」这条路对沃无效——问题在密钥交换套件）
  - `docs/TESTING_STRATEGY.md`：用例数更新为 **544**（Rust）+ **27**（前端）；
    补记本机 16 核构建会被 OOM 杀掉 rustc 的坑与两种避开方式
- **文档与代码对齐（2026-10-23）**——此前文档比代码落后了好几轮：
  - `docs/ROADMAP.md`：P0 剩余项（跨表事务封装、升级路径测试）已完成；
    P1 的「沃邮箱待实测」「QQ 与沃未开始」「T5」「按需正文」均已完成；
    P2 拆成「已实现」与「仍缺」；「下一批任务」按依赖顺序重写
  - `docs/ISSUES_AND_RISKS.md`：§1 的「workspace 只有 core 与 shell」等描述已过时；
    R01/R02/R03 转为已实测/已实现；R04/R07/R09/R12/R13 转为部分实现；
    **新增 R17**（13 个提交从未推送）与 **R18**（SMTP 参数未实测）；
    复核记录补 2026-10-23 一行
  - `docs/ARCHITECTURE.md`：模块表补 `xremail-message` / `xremail-diagnostics`，
    修好 `xremail-sync` 那行被吃掉换行的表格；
    §4.2 / §5 / §6 补「实现落点」，把实测得来的约束写进正文
  - `docs/TESTING_STRATEGY.md`：T1–T10 加状态列；新增 §5.1.1
    「用**变异测试**证明用例有效」（四轮共 29 个变异）
  - `docs/PROTOCOLS.md`：163 章的「仍待实测」清单里字符集抽样已补测
  - `README.md`：当前状态段重写（阶段徽标从「P0 脚手架」改为实际状态）
- **R13 决策：release profile 的 panic 策略由 `abort` 改为 `unwind`**
  - 理由：P0 开始引入 tokio 异步任务，`abort` 下任一 panic 会终止整个进程，后台同步任务无法用 `catch_unwind` 兜底；现在改成本最低，体积代价很小
  - 保留的配套要求：后台 task 边界需 `catch_unwind`（待 `xremail-sync` 落地时实现）
  - 依据：[ARCHITECTURE §9](docs/ARCHITECTURE.md)、[ISSUES_AND_RISKS R13](docs/ISSUES_AND_RISKS.md)
- **MSRV 由 1.88 上提至 1.94**：sqlx 0.9.0 声明的 MSRV 是 1.94.0，继续声明 1.88 会成为假声明
- **领域枚举改为显式 `serde` 名称**（`ProviderKind` / `AuthKind` / `TlsMode` / `SmtpAuth` / `SyncMode`）
  - 原因：这些字符串会作为 SQLite 列值落库。若依赖 `rename_all` 自动推导，Rust 变体改名会让旧行读不出来（静默丢数据）
  - 附带修正：`ProviderKind::Netease163` 原先自动推导为 `netease163`（数字不产生分隔符），现为显式的 `netease_163`
- **项目改名：ReMail → Xremail**
  - 展示名 `Xremail`；crate / npm 包名 `xremail-core` / `xremail-shell`；Tauri identifier `com.xremail.app`
  - 原因：`remail` 在 crates.io（Rust 邮件服务器 v0.0.1）与 npm（React 邮件组件框架 v2.1.0）均已被占用；`xremail` 三个注册表均可用，GitHub 无同名仓库
  - 目录 `crates/remail-core/` → `crates/xremail-core/`；根 Cargo.toml 的 workspace 依赖键同步改名
  - 命名规范见 [CONTRIBUTING.md](docs/CONTRIBUTING.md)
- `ProviderKind` 的沃邮箱变体重命名为 `Wo`（显示名「沃邮箱」），移除服务商名称中的厂商字样；序列化值同步变更
- 文档与提交信息中不再出现厂商名称，仅保留产品名与域名
- 架构语义重写：本地 SQLite 明确为读模型；服务器快照与本地操作意图分离；不承诺 exactly-once
- 冲突仲裁不再用本地时间戳猜测远端操作顺序；有 CONDSTORE 用 UNCHANGEDSINCE，否则快照 + 意图重放
- 删除策略：无 UIDPLUS 时不使用普通 EXPUNGE，避免清除其他客户端的删除标记
- 安全：撤回凭据静默降级与沙箱弹窗逃逸建议；默认禁网；明确 AEAD 不能单独防回滚重放
- 路线图改为按验收门槛推进，撤回未经测量的耗时系数与固定交付日期
- 文档作者/负责人署名为 xyyi0719

### Added
- **`xremail-message` crate**：MIME 结构、字符集解码、RFC 5322 头
  - **字符集**：UTF-8 / GB2312 / GBK / GB18030 / Big5 / ISO-8859-*（encoding_rs）
  - 无 charset 时的**猜测**（先严格 UTF-8 → GB18030 → windows-1252）
  - **内容编码**：base64 / quoted-printable / 7bit / 8bit
  - **编码字**：RFC 2047 编码字（含「相邻编码字之间空白要丢掉」）
  - **结构解析**：Content-Type 参数、multipart **递归**拆解、头字段折行
  - **解析结果**：整封邮件 → 主题 / 地址 / 日期 / 纯文本 / 原始 HTML / 附件标记
  - **HTML 清洗不做**——那是渲染层的事（ARCHITECTURE §7）
- **`xremail-diagnostics` crate**：诊断工具（同时用协议层与 MIME 层，
  所以单独成 crate——这两层之间不应有依赖）
  - **采样**：只读抽样，只输出**编码名与计数**，
    绝不打印主题 / 正文 / 地址
- **QQ 与沃邮箱的实测记录**（2026-10-23）：三家国内服务商的协议事实已全部落到 `docs/PROTOCOLS.md`
  - **QQ 是三家里唯一支持 IDLE 的**（✅ 服务器接受 `+ idling`），也**有 MOVE**
  - **沃是 Coremail 体系**（与 163 同源），**不是**先前推测的 Coxremail
  - **`ID` 是 163 独有的要求**：QQ 与沃登录后不发 ID 也能访问邮箱
  - **CONDSTORE 三家都没有** → 条件写只能走「读快照 + 重放意图 + 回读确认」
  - **QQ 不发 RFC 6154 角色属性** → 文件夹角色必须**按名字**兼底
  - **QQ 的 UIDVALIDITY 是大数**（1792993698）而 UIDNEXT 很小（105）→ epoch 切换是常态路径
  - 凭据文件支持 `domain =` 键；QQ 只写 QQ 号时自动补 `@qq.com`

### Fixed
- **Tauri IPC 测试在 Linux 上全挂**（2026-10-26 CI 首次推送时暴露）：
  `tauri::test` 构造的 `InvokeRequest` 里 `url` 必须是**平台正确**的本地 origin
  （Windows/Android 是 `http://tauri.localhost`，其它平台是 `tauri://localhost`）。
  写错会被当成远程来源 → 走 ACL 检查 → mock 应用的 ACL 是空的 →
  命令被一律拒绝（`not allowed. Plugin not found`）。
  **这个坑在 Windows 上看不见**，只有 Linux CI 才暴露。
  修法：`tests/command_ipc.rs` 的 `local_origin()`；
  机制用「在 Windows 上换成 tauri://localhost 立刻复现」反向验证过。
- **multipart 边界分隔符前面的 CRLF 没被去掉**（RFC 2046 §5.1.1）：
  导致每个部件的正文末尾多一个换行
- **base64 解码遇到非字母表字符就整段失败**：
  RFC 2045 §6.8 规定「不在 base64 字母表里的字符一律忽略」
- **`1:*` 被展开成上亿个元素**（假服务器内存爆掉，8 GiB 分配失败）：
  sequence set 的 `resolve` 是**展开**语义，而 `1:*` 在大 UID 空间下
  （163 的 UID 就是 17 亿量级）会覆盖上亿个 UID。
  现在假服务器**不展开 set**，而是拿每封邮件去问 `contains`；
  `resolve` 保留但注明「不要在大 UID 空间下用」
- **探测工具的 IDLE 用法漏了 `init()`**：async-imap 的 `done()` 里有断言，
  没 `init()` 过就直接 `done()` 会 panic。163 不支持 IDLE 所以碰不到，
  **QQ 支持 → 第一次跑 QQ 就炸了**。现已按正确流程
  （`idle()` → `init()` → `wait_with_timeout()` → `done()`）实现
- **async-imap 0.12 会吞掉服务器回的 `NO` / `BAD`**（**严重**）：
  `uid_fetch` / `uid_store` / `uid_search` 在服务器拒绝时**返回 `Ok`**，
  只是结果为空。后果是同步引擎会把「服务器拒绝了」当成「写成功了」，
  本地状态与远端悄悄分叉。
  根因：库的响应解析用 `take_while(filter(...))` 读响应，而 `filter` 遇到
  **自己那个 tag** 的 `Done` 响应时返回 `false`——那个携带 `NO` 的响应
  于是**从未被解析成错误**就被丢弃。
  本项目的对策：新增 `run_checked` 安全命令层，并在测试里把这个行为钉住
- **上一批的“用例数统计脚本”有 bug**：用 `awk -F'[ ;]'` 取 `$6`，
  而 `passed; 1 failed` 之间的 `"; "` 会切出一个**空字段**，
  于是失败数**恒为 0**。因此上一批报的“六关全绿”里其实有 1 个用例是失败的
  （`xremail-testkit` 的 `first_atom_skips_quoted`）。
  对策：改用 `target/gates.sh`，它**看退出码**而不是看打印出来的数字
- **`xremail-testkit` 的 `first_atom` 是死 API**：没有任何调用点，
  且它的测试断言与文档描述相反（文档说“第一个未加引号的词”，
  测试却期望带引号时也返回它）——该测试从一开始就是失败的。直接删除
- **`MessageFixture` 的头字段替换会把头与正文之间的空行弄丢**，
  导致 `header()` 只能看到第一个头字段
- **登录失败被一律当成“密码错了”**：网络抖动导致的连接断开也是 `Auth` 错误，
  会让用户反复检查一个根本没问题的密码，也让重试策略失效。
  现在按 async-imap 的错误变体分类：`NO` → 认证失败（需用户处理）、
  `IO` / `ConnectionLost` → 传输故障（**可重试**）、`BAD` / 解析失败 → 协议层问题
- **`modified UTF-7` 编码器把 `&` 编成了 `&ACY-`**：RFC 3501 规定 `&` 必须写成 `&-`
  （移位段为空），而不是当作普通字符进移位段。用例当场捕获，已修
- **Windows 上 `tauri::test` 的测试二进制无法启动**（`STATUS_ENTRYPOINT_NOT_FOUND`）
  - 原因：测试二进制静态导入 `comctl32.dll!TaskDialogIndirect`（只有 v6 SxS 才有），
    而 `tauri-build` 只给主程序嵌入 v6 manifest
  - 解法：`apps/shell/src-tauri/build.rs` 把 comctl32 改为延迟加载
    （`/DELAYLOAD:comctl32.dll` + 链接 `delayimp.lib`）
- **Windows 上强杀 Tauri 应用会留下 `msedgewebview2.exe` 残留**，
  导致下次启动报 `WebView2 0x800700AA 请求的资源在使用中`。
  该错误发生在 Tauri 建窗口阶段（**早于用户的 setup 钩子**），
  看起来像「setup 没跑」，容易误判——详见 [ENVIRONMENT §1.9](docs/ENVIRONMENT.md)
- **桌面端专属插件在 Android 目标上编译失败**：`tauri-plugin-single-instance` 的 `init` 与 `tauri-plugin-autostart` 的 `init` / `MacosLauncher` 只在桌面端存在
  - `apps/shell/src-tauri/src/lib.rs`：两个插件用 `#[cfg(desktop)]` 隔离，`use tauri::Manager` 同步加门（Android 侧未用到，否则 clippy `-D warnings` 会报错）
  - 影响面：仅 Android 目标的编译；桌面端行为不变
- **`docs/ENVIRONMENT.md` 更新过时的 Android 结论**
  - §1.1 工具链表：OpenJDK 从「仅 1.8.0_502」改为 17.0.20.1，并补 Android SDK / NDK / cargo-ndk / cargo-audit / cargo-deny / git-filter-repo 行
  - 新增 §1.8「Android 工具链」：安装位置、用户级环境变量、5 条踩坑（Adoptium 跳转 GitHub 被劫持、包名实为 `platforms;android-37.0`、`sdkmanager` 需 JDK 17、Git Bash 下 `reg add` 的 `/v` 被 MSYS 改写、桌面端插件在 Android 无 API），以及当前唯一卡点（符号链接权限需重登录）
  - §2.1 平台支持表：Android 从「⛔ 本机不可构建」改为「🟡 Rust 侧可交叉编译，APK 打包待权限」
- **`docs/ENVIRONMENT.md` §1.5 不再写死本机手动设置的代理地址**
  - 删除文档里的具体代理 `host:port`（该值会随网络环境变化，且属于内网信息）；改为运行时从注册表读 `ProxyServer` / `ProxyEnable` / `ProxyOverride`
  - 实测发现文档里的旧值已与注册表当前值**不一致**，证实写死必然失效
  - 明确仓库级 `http.proxy` / `https.proxy` 属本机手动配置：落在 `.git/config`，`.git/` 不被跟踪、不随仓库分发，换机器需重设
  - `--resolve` 用的 GitHub 公网 IP 不再写死，改为经代理走 DoH 实时取（GitHub 公网 IP 会轮换）
  - 补充 Git Bash 下 `reg query ... /v` 会被 MSYS 当路径改写的坑（用 `//v` 或 `MSYS_NO_PATHCONV=1`）
- **新增“连 GitHub 前先探明”规则**（`docs/ENVIRONMENT.md` §1.5.1、`docs/AI_DEVELOPMENT_GUIDE.md` §3/§5.0、`docs/CONTRIBUTING.md`）
  - 每次访问 GitHub（推送 / 拉取 / 调 API / 开 PR）**先探明连通性，再执行操作**
  - 探针：`git ls-remote origin`（推送通道，SSH over 443）、`gh auth status` / `gh api rate_limit`（API 通道）、`curl -x <当前代理>`（HTTPS 通道）
  - 附探针结果 → 结论 → 下一步的判读表；**探不通不推**，避免反复推送产生半截状态或让 CI 空跑
  - 实测本机到 GitHub 的连接**间歇性失败**（`git ls-remote` 5 次中 3 次成功、`gh api` 5/5 成功、`curl` 过代理 5 次中 1 次成功）；**单次探针失败先重试**再判定断网
  - 补记三种通道各自的失败特征：SSH over 443 为 `Connection reset by <ip> port 443`（TCP 能连、SSH 会话被 reset）、`gh api` 为 `net/http: TLS handshake timeout`、`curl` 为 `schannel: failed to receive handshake`
  - 实测**换 GitHub IP 无效**：9 个候选里只有本地 DNS 解析到的那个接受 `ssh.github.com` 的 SSH 服务（其余 `Connection closed by <ip> port 443`），且 TCP 443 对所有候选都通 → 问题在会话层，只能重试
  - 探针脚本重试次数定为 `TRIES=6`（刻意收敛的上限：探针单次成功率约 25%，整轮全失败概率约 18%；6 次仍不通就停下报给维护者，不继续刷），并把 **push 本身也纳入同一重试循环**（探针过了 push 仍可能被 reset）
  - 补记两个判成功相关的坑：给 `git push` 接管道（如 `| tail`）会让退出码变成管道末端命令的，把失败读成成功；推送后用 `ls-remote` 核验本身也会间歇性失败，核验失败不等于没推上去，应以 `git push` 退出码为准并比对 SHA
- **改名遗漏**：`ReMail → Xremail` 后仍残留旧 crate 名
  - `apps/shell/src/App.tsx`：占位页文案显示的 `remail-store` 改为 `xremail-store`（用户可见）
  - `apps/shell/src-tauri/src/lib.rs`：注释中的 `remail-sync` 改为 `xremail-sync`
  - `apps/shell/src-tauri/src/cmd.rs`、`apps/shell/src-tauri/src/state.rs`：注释中的 `remail-store` 改为 `xremail-store`
  - `ProviderKind` 枚举**变体名与序列化值未改**（`netease163` / `netease126` / `netease_enterprise` / `tencent` / `wo` / `google` / `exchange` / `generic` 保持不变）
- **`ProviderKind` 文档注释移除厂商名**，改用产品名与域名，与 `Wo` 变体重命名时确立的规范一致
  - `NetEase enterprise mail` → `网易企业邮 mailboxes (@qiye.163.com)`
  - `QQ Mail (Tencent)` → `QQ mailboxes (@qq.com)`
  - `Wo Mail` → `沃邮箱 mailboxes (@wo.cn)`
  - `Gmail / Google Workspace` → `Gmail / Workspace mailboxes`
  - `Microsoft Exchange / Outlook 365` → `Exchange / Outlook 365 mailboxes`
  - `AuthKind::OAuth2` 的 `Google/Microsoft` → `Gmail / Exchange`
  - 同步修正 163 / 126 的注释为产品名 + 域名写法
- `AppError` 的 `ctx()` 不再把错误统一折叠成 `Other`，保留 `is_retryable()` / `requires_user_action()` 判定
- 代码注释中的文档相对路径断链（`crates/xremail-core/src/*.rs`、`apps/shell/src-tauri/src/*.rs`）
- 代码注释中过期的 `ARCHITECTURE §N` 章节号引用
- workspace `resolver` 由 `2` 改为 `3`（edition 2024 隐含 MSRV 感知解析），Cargo.lock 无变化

### Notes
- 关键技术选型已锁定：Tauri 2 + React/TS、先做 L1（IMAP 原生）同步并预留 L2 抽象
- 后续持续扩写测试用例，按 TESTING_STRATEGY §5 的 T1–T10 顺序在 P0–P3 分批推进
- Linux 编译已由 CI 验证，运行未验证；列为可选 P5-L，需求明确后再投入
- Gmail 暂缓，不再是 P1 关键路径；恢复接入前需重新核实授权与发布要求
- 弃用同步版 `imap` crate
- 尚未实现：SQLite 持久化、协议适配、收发链路、自动化业务测试
