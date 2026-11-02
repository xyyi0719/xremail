# 安全模型

> 威胁模型 → 缓解措施 → 验证方式。**任何新增的邮件内容渲染、凭据存储、跨端同步代码，必须先在此文档登记。**
> 维护者：xyyi0719

---

## 1. 威胁模型

| 编号 | 威胁 | 等级 | 主要缓解 |
|---|---|---|---|
| T1 | 恶意邮件通过 HTML 执行脚本（XSS） | **严重** | sandbox iframe（无 `allow-scripts`）+ Rust 侧清洗 + CSP |
| T2 | 远程图片追踪像素泄露阅读行为 | 高 | 默认阻断远程图片 + 追踪器面板 |
| T3 | 凭据（授权码/密码/token）泄漏 | **严重** | OS 钥匙串 + 永不明文落库 + 日志脱敏 |
| T4 | 本地数据库被他人读取后泄漏邮件内容 | 中 | 文件权限、建议启用系统磁盘加密；当前未实现数据库/附件全量加密，vault 加密不保护邮件缓存 |
| T5 | 中间人攻击（伪造 IMAP/SMTP 服务器） | 高 | 强制 TLS + 系统根证书校验 + 拒绝降级 |
| T6 | 恶意附件被执行 | 高 | 不自动执行；危险类型仅纯文本预览；存盘内容寻址 |
| T7 | 跨端同步服务泄漏邮件内容 | 高 | 端到端加密，服务端零知识 |
| T8 | 同步服务被篡改 | 中 | 密文带认证标签（Aead）；服务端只存密文 |
| T9 | 恶意 IMAP 服务器返回超大响应耗尽内存/磁盘 | 中 | 响应体大小上限、分批拉取、单封邮件上限 |
| T10 | 日志/诊断包泄漏敏感信息 | 中 | 邮箱地址打码、body 不入日志、导出前二次脱敏 |
| T11 | 应用被恶意网页注入（WebView 场景） | 中 | Tauri 能力白名单（capabilities）+ 禁用任意命令 |
| T12 | 供应链投毒 | 中 | `cargo-audit` / `cargo-deny` / 锁文件提交 / 最小依赖 |

---

## 2. 凭据处理

### 2.1 存储

| 凭据类型 | 存储位置 | 说明 |
|---|---|---|
| 163/QQ/沃邮箱 授权码 | **平台 SecretStore** | Windows 凭据管理器；Android 需单独实现 Keystore 密钥保护的加密存储，不能假设 keyring crate 自动支持 |
| Gmail OAuth access/refresh token | **OS 钥匙串** | 同上；过期需重新授权 |
| 自建同步服务口令 | **OS 钥匙串** | |
| 主密码 | **不存储** | 每次解锁时用户输入；用于派生 vault 密钥 |

数据库里只有 `account.secret_ref`（一个不透明指针），**永远不是凭据本身**。

**实现状态（2026-10-26）**：Windows 侧已落地——`apps/shell/src-tauri/src/secrets/windows_store.rs`
走 Windows 凭据管理器（`CredWriteW` / `CredReadW` / `CredDeleteW`），条目名
`xremail:account:<account_id>`，通用凭据类型，持久化范围 `CRED_PERSIST_LOCAL_MACHINE`。
要点：

- **不静默降级**：平台没有钥匙串适配（Android / Linux / macOS 现在是
  `secrets/unsupported.rs` 的桩）时，保存**如实报错**，绝不落到明文文件或「只存内存」。
  桩的 `get` 恒为 `None`。
- **数据库里只有引用**：`account.secret_ref` 存的是上面那个条目名，不是授权码本身。
- **只按引用取用**：`CredentialService::resolve_credential` 只看 `account.secret_ref`，
  没有引用就**不去打扰钥匙串**；删账户/删凭据会同时清掉钥匙串条目与引用。
- **不进日志**：诊断工具与命令返回只输出条目名与状态，不输出凭据内容。
- 权威来源见 `windows` crate 的 `Win32_Security_Credentials`；Android 侧仍需单独实现
  Keystore 保护的存储，**不能**假设同一份代码可平移。

### 2.2 传输

- 生产环境 IMAP/SMTP 强制 TLS；STARTTLS 必须在认证前成功升级，失败不得降级明文。明文仅限隔离的本机假服务器测试，不提供生产“不安全模式”。
- TLS 配置采用 rustls；平台信任根加载必须验证，不能泛称某类国内 CA 一定缺失。系统根也不能自动信任任意自签证书。
- 用户可导入自签 CA（存本地，不上传）。
- **禁止** TLS 版本降级；**禁止**忽略证书错误（"跳过验证"开关不存在）。

### 2.3 内存

- 凭据使用可清理的秘密类型并尽量缩短存活时间；zeroize 是尽力防护，不保证消除库内部复制、交换文件或崩溃转储中的全部副本。
- 钥匙串不可用时提供重试或显式仅会话保存，不自动回退明文/加密文件；本地文件 vault 需要另行评审口令、解锁与恢复设计。
- 不把凭据放进 `String` 的 `Debug` 输出（自定义 `Debug` 实现打印 `***`）。

---

## 3. HTML 邮件渲染规范（**强制**）

这是邮件客户端最大的攻击面。渲染路径必须严格按下述四层执行：

### 3.0 实现现状（2026-10-30）与本规范的偏离

**已实现的部分**：

- 第 1 层的清洗在 `xremail-core::html::sanitize_html`——**不是 ammonia**，是手写的白名单消毒器：
  标签 / 属性白名单，删 `script` / `style` / `iframe` / `on*` 事件属性、
  `javascript:` / `data:` 协议，删**远程图片**（跟踪像素与信息泄露的通道）；
  **未知标签只删标签、留着内容**——真实邮件的正文全被 `html` / `body` 包着，
  连内容一起删整封信就没了。
- **原始 HTML 不出后端**：`MessageBodyView` 只带 `htmlSafe`，前端拿不到 `html_raw`。
- 链接：正文里的链接**交给系统浏览器**（`@tauri-apps/plugin-opener`），
  应用窗口不被那个网页顶掉；危险协议在渲染前就被删掉。
- 远程图片两道闸：清洗时删 `<img src="http...">`，且 `tauri.conf.json` 的 CSP
  `img-src` **不含 http/https**。

**与本规范的偏离（已知，未消）**：

1. **没有用 sandbox iframe**：消毒过的 HTML 直接渲染进 DOM
   （`dangerouslySetInnerHTML`，见 `src/components/MessageView.tsx`）。
   偏离的理由是「清洗在后端完成 + 原始 HTML 不出后端 + CSP 不含远程来源」；
   **代价**：一旦消毒器漏掉一个危险构造，它就直接在应用上下文里生效。
   ⇒ 真实 WebView 里的渲染与网络行为**尚未在宿主上验证**（见 R06 残余风险）。
2. **「显示图片」按钮与追踪器面板未实现**：远程图片一律阻断，没有逐域放行名单。
3. **重定向警告页未实现**：链接直接交给系统浏览器，没有本应用内的警告页。
4. `ammonia` 未引入；CSS 的处理是**整块丢弃** `<style>`，不保留任何内联样式。

### 第 1 层：Rust 侧清洗（`xremail-core::html`）

> 下面写的是**规范**；实际实现见 §3.0（手写消毒器，不用 ammonia）。

用 `ammonia` 清洗原始 HTML：

- 删除：`<script>` `<iframe>` `<object>` `<embed>` `<form>` `<input>` `<style>`、base、meta refresh。首版不保留任意 CSS；未来保留样式需专门 CSS 清洗与网络泄漏测试。
- 删除所有 `on*` 事件属性
- 属性白名单仅保留已验证的展示属性；首版移除 style、srcset、后台资源属性，不信任 ammonia 自动清洗 CSS。
- 链接协议与资源协议分开。http/https/mailto 链接提取成惰性展示，由宿主确认后打开；cid 只解析对应邮件内资源，不能把任意相对路径盲改为 cid。
- **移除** `javascript:` / `data:` / `vbscript:` 协议的 URL
- `<a href>` 保留原 URL 供 UI 展示"真实链接"提示

### 第 2 层：sandbox iframe（前端组件 `<SafeEmailFrame>`）

> ⚠️ **未实现**（2026-10-30）：当前渲染路径是「后端消毒 + 直接渲染进 DOM」，
> 见 §3.0 的偏离说明。

- iframe 使用空 sandbox，不授予 allow-scripts、allow-same-origin 或弹窗逃逸权限；referrerpolicy=no-referrer。
- srcdoc 自身不阻止网络请求。宿主构造固定文档壳，把 CSP meta 放在所有不可信内容之前；不依赖支持不一致的 iframe csp 属性。
- 初始 CSP 设计为 default-src 'none'、script-src 'none'、connect-src 'none'、object-src 'none'、frame-src 'none'、form-action 'none'、base-uri 'none'；仅为经过宿主校验/重编码的内联图片设计最小资源来源。
- 不默认放行 https 图片、媒体或字体。cid/blob/data 的具体交付与 opaque origin 行为需在 WebView2/Android WebView 实测，不能仅靠字符串配置宣称安全。
- 移除活动链接，展示链接清单/宿主操作入口；应用侧 opener 再校验目标。不依赖无脚本 iframe 内点击事件能跨 origin 被父页面读取。
- 必须通过网络请求、脚本、导航与 IPC 隔离测试后才开放真实 HTML；未通过时只显示纯文本。

### 第 3 层：远程图片策略

默认**全部阻断**远程图片（用户点"显示图片"才放行）。

- 阻断时用占位图替换 `<img src="http...">`，并保留原始 URL。
- **追踪像素检测**：解析阶段标记 1×1 / 尺寸极小 / `display:none` 的 `<img>`，在"追踪器"面板列出，用户可按域名逐个放行。
- 放行名单持久化在本地设置（v1.0 每设备独立；P6 后可跨端）。

### 第 4 层：链接安全

- 展示前对 URL 做协议白名单校验。
- 点击外链 → 显示**重定向警告页**（目标域名 + 完整 URL），避免 `javascript:` 与钓鱼。
- 长 URL / IDN 域名 / 同形字（homoglyph）做高亮提示。

### 附件

| 类型 | 策略 |
|---|---|
| 图片（png/jpg/gif/webp） | 可预览 |
| 文本/PDF | 可预览（PDF 交给系统查看器，不内置渲染） |
| `html` / `svg` / `xml` | **不渲染**，仅下载；提示"该文件可能包含脚本" |
| `exe` `scr` `bat` `cmd` `ps1` `jar` `apk` `dll` | 不预览、不自动执行，下载需二次确认 |
| 未知类型 | 存盘，不预览 |

附件按内容寻址存储（`sha256`）：内容相同才去重，同名不同内容分别存储；删除按引用计数/可恢复回收规则处理。

---

## 4. 跨端同步的端到端加密

> 仅在 P6 启用（见 [ADR-0002](adr/0002-sync-depth-l1-first.md)）。设计先定，代码后补。

```
用户主密码
   │
   ├─ Argon2id(salt = 服务端分配的 per-group salt, m=64MiB, t=3, p=1)
   │     └─ 32 字节主密钥  ← 只存在于内存，退出即销毁
   │
   ├─ vault_key = HKDF(master_key, info="xremail-vault-v1")
   │
   └─ vault = XChaCha20-Poly1305(vault_key, nonce, plaintext)
         └─ plaintext = JSON{ accounts_meta, signatures, rules, labels,
                            templates, contacts, credential_sealed }
```

- 服务端存储：`{ group_id, revision, ciphertext, nonce, updated_at, device_id }`
- 服务端看不到密文明文，但会看到 group/device 标识、请求时序、IP、密文大小等元数据；不能承诺不知道设备数量。
- 认证凭据与 vault 加密密钥必须分离；不把同一口令派生值直接同时当 bearer 和加密密钥。完整认证与恢复协议在 P6 前另行评审。
- 完整性：AEAD 检测密文篡改；版本、group、用途等上下文需认证绑定。AEAD 本身不防回滚/重放，需本地最高已知 revision、CAS 与明确的新设备信任策略。
- **不加密**的内容：revision、updated_at、group_id（否则无法做增量同步）。

---

## 5. Tauri 能力最小化

`apps/shell/src-tauri/capabilities/` 里显式声明允许的窗口与插件权限：

- 禁止 `shell` 的任意命令执行（仅 `opener` 打开 https 链接）。
- 禁止前端直接访问文件系统（附件读取走 Rust 命令，且按 message 归属校验）。
- 禁止 `dangerousDisableAssetCspModification`。
- Tauri 配置里 `app.security.csp` 必须非空（模板默认是 `null`，**必须改**）。

---

## 6. 日志与诊断

- 使用 `tracing`，`Subscriber` 统一做脱敏：
  - 邮箱地址 → `a***@163.com`
  - 授权码 / token / 口令 → 永不记录（用 `Secret<T>` 包装类型，Debug 输出 `***`）
  - 邮件正文 → 不记录
  - URL 中的 query 参数可能含 token → 记录前剥离 query
- "导出诊断包" = `tracing` 日志 + 版本信息 + 账户配置（**不含**凭据）+ 脱敏后的协议日志。
- 日志文件默认 7 天滚动，最多 20MB，存放在应用数据目录。

---

## 7. 资源耗尽防护

| 攻击 | 防护 |
|---|---|
| 超大邮件头 | 限制单行 1MB、总头 16MB |
| 超大正文 | 单封 50MB 上限，超出截断并标记 |
| 海量小邮件 | 批量拉取上限；同步速率限制 |
| 恶意压缩炸弹（附件） | 解压比例上限（若做解压） |
| 无限文件夹层级 | 目录树深度上限 32 |
| 超大附件 | 附件大小上限 + 磁盘配额提示 |

---

## 8. 依赖与供应链

```bash
cargo install cargo-audit cargo-deny
cargo audit
cargo deny check          # licenses / bans / advisories
```

- `Cargo.lock` **必须提交**（本项目发布二进制）。
- 依赖许可证仅允许 MIT / Apache-2.0 / BSD / ISC / MPL-2.0。
- CI 检查 `cargo clippy -- -D warnings` 与 `cargo fmt --check`。
- 前端：`npm audit`；锁定 `package-lock.json`。

---

## 9. 漏洞报告

请**不要**开公开 issue。发送邮件至 `security@xremail.example`（占位，正式发布前替换），或使用 GitHub 私密漏洞报告。
我们会在 72 小时内确认收到，14 天内给出修复或缓解方案。

---

## 10. 安全检查清单（每个里程碑收尾必须过）

- [ ] 没有任何路径把凭据写进 SQLite / 日志 / 崩溃报告
- [ ] HTML 邮件渲染经过四层处理，iframe 无 `allow-scripts`
- [ ] 远程图片默认阻断，追踪像素可识别
- [ ] 所有网络连接强制 TLS，无"忽略证书错误"开关
- [ ] `capabilities/` 未授予多余权限
- [ ] `app.security.csp` 非空
- [ ] 附件危险类型不预览
- [ ] `cargo audit` / `cargo deny` 无告警
- [ ] 日志导出经过脱敏演练

---

## 相关

- 架构：[ARCHITECTURE.md](ARCHITECTURE.md)
- 规划：[PLAN.md](PLAN.md)
- 协议参数：[PROTOCOLS.md](PROTOCOLS.md)
