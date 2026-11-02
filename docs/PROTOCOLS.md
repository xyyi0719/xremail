# 各服务商协议参数与实测记录

> **这份文档的价值全靠真实数据积累。**
> 任何人在真实邮箱上验证出的参数、行为、坑，请直接补充到对应小节，并更新"验证状态"列。
> 标注 `⚠️ 待实测` 的内容**不要**当作事实写进代码。

真实参数与实测记录；**2026-10-22 至 2026-10-29 的实测结论已写入各节**。
尚待实测的部分在各处标注 `⚠️ 待实测`（主要是 126、企业邮、Gmail、Exchange 与并发连接上限）。
> 维护者：xyyi0719

> 目标服务商为 163、QQ 与沃邮箱（`wo.cn`）。**不按品牌推定服务器地址**——
> 163 / QQ / 沃 的 IMAP 与 SMTP 参数均已由本项目客户端实测确认，126 / 企业邮 / Gmail / Exchange
> 仍是候选值、**必须实测确认**。Gmail 章节仅保留历史研究，当前暂缓，授权流程恢复前重新核实。

---

## 维护规则

1. **来源分级**：
   - `官方文档` — 来自服务商官方帮助页 / 开发者文档（附链接）
   - `社区资料` — 附具体来源，不等于本项目验证；本人实测另记录环境与原始脱敏证据
   - `待实测` — 传闻、约定俗成，必须自己验证
2. **每次实测**请记录：日期、客户端版本、区域/节点、观察到的行为。
3. **发现差异**时以实测为准，并在本文件标注"与社区资料不一致"。
4. 记录**失败现象**（错误码、提示文案）比记录成功更有价值。

---

## 三家国内服务商的实测对比（2026-10-23）

| | 163 | QQ | 沃 |
|---|---|---|---|
| 服务器体系 | Coremail | QQMail XMIMAP4Server | **Coremail**（与 163 同源） |
| IDLE | ❌ | ✅ | ❌ |
| MOVE | ❌ | ✅ | ❌ |
| CONDSTORE | ❌ | ❌ | ❌ |
| UIDPLUS | ✅ | ✅ | ✅ |
| NAMESPACE | ❌ | ✅ | ❌ |
| 需先发 `ID` | ✅ **必须** | ❌ 不需要 | ❌ 不需要 |
| LIST 给角色属性 | ✅ | ❌ **不给** | ✅ |
| 文件夹名 | 中文 | 英文 | 英文 |
| UIDVALIDITY | 1 | **1792993698**（大数） | 1 |
| UIDNEXT | ❌ 不给 | ✅ 给 | ❌ 不给 |

**三条结论**：

1. **CONDSTORE 三家都没有** → 条件写只能走 ARCHITECTURE §6 的
   「读快照 → 重放意图 → **回读确认**」。这也验证了我们不用 `UNCHANGEDSINCE` 是对的。
2. **`ID` 是 163 独有的要求**（沃也是 Coremail 却不需要）→
   连接层按能力集「有能力就发」是安全的，但不是必需的。
3. **不能只靠 RFC 6154 属性认文件夹角色**——QQ 不给。必须准备名字兼底。

## 写标记（`UID STORE`）的实测（2026-10-26）

工具：`xremail-flag-smoke`（`crates/xremail-diagnostics`）。
三家真实账号各跑一遍：连上 → `SELECT INBOX` → 挑一封**未读**邮件 →
`UID STORE <uid> +FLAGS (\Seen)` → **回读确认** → 再发一次同样的命令 → **恢复原状**。

| | 163 | QQ | 沃 |
|---|---|---|---|
| 收件箱邮件数 | 4 | 70 | 6 |
| TLS | TLSv1.3 / TLS13_AES_256_GCM_SHA384 | TLSv1.2 / TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 | TLSv1.3 / TLS13_AES_256_GCM_SHA384 |
| `UID STORE +FLAGS (\Seen)` | OK | OK | OK |
| 回读确认 `\Seen` 已生效 | ✅ | ✅ | ✅ |
| **重复发同一个标记** | **OK** | **OK** | **OK** |
| `-FLAGS (\Seen)` 恢复 | OK | OK | OK |

**两条结论**：

1. **三家都对「重复设置同一个标记」回 `OK`，不回 `NO [MODIFIED]`。**
   所以「基线过期」这条分支在这三家上**不会被触发**——它是为别的服务商（以及假服务器）
   准备的防御分支，不是这三家的行为。**不要**把「服务器回 OK」当成「标记生效了」：
   真正兜底的是**回读确认**（`UID FETCH` 读回 `FLAGS` 逐位比对）。
   假服务器里那条「回 OK 却没真改」的用例正是为了守住这一点。
2. **`STORE` 必须以可写方式打开邮箱。** 拉取那一轮用的是 `EXAMINE`（只读），
   只读邮箱拒绝 `STORE`；同一个 session 上必须重新 `SELECT` 才能写。
   这条对三家都一样。

**副作用**：测试会真的改一封邮件的状态，跑完会恢复；恢复后回读确认与原状一致。

## 通用基线

| 项 | 约定 |
|---|---|
| IMAP 端口 | 993（隐式 TLS）为常用候选；143 + STARTTLS 是否可用须逐服务商确认，不宣称唯一端口 |
| SMTP 端口 | 465（隐式 TLS）为主；587 + STARTTLS、994 为辅 |
| TLS | 生产仅隐式 TLS 或强制 STARTTLS，失败不降级；明文只限隔离的本机假服务器 |
| 认证 | 163/QQ/沃邮箱：密码字段填**授权码**；Gmail：XOAUTH2；Exchange：OAuth2 或客户端 ID+密钥 |
| 编码 | 全部按 UTF-8 存储；入站需处理 GBK/GB18030/Big5 |
| 时间 | 统一 epoch 毫秒（UTC） |

### 三个容易踩的通用陷阱（2026-10-26 真实邮箱实测）

| 陷阱 | 症状 | 正确做法 |
|---|---|---|
| **EXAMINE 只认线路原名** | `LIST` 返回的名字是 modified UTF-7（`&XfJT0ZAB-`）；解码成「草稿箱」再去 `EXAMINE`，服务器回 `NO EXAMINE Folder not exist` | 库里存**线路原样**（`folder.remote_name`），显示时再解码 |
| **UID 是邮箱作用域的** | 会话停在另一个文件夹时，`UID FETCH <uid>` 返回**空结果**（不是报错）；「拉到了 0 封」看起来像「服务器上没有这封邮件」 | 取数据前先 `EXAMINE` / `SELECT` 对应文件夹 |
| **LIST / UID FETCH / UID STORE / UID SEARCH 会吐掉带 tag 的 NO** | 服务器拒绝了，客户端却拿到 `Ok(空结果)` | 一律走安全命令层 `run_checked`（自己读响应、自己判 OK/NO/BAD） |

前两条不是某个服务商的怪癖，是 IMAP 本身的规定；它们在 163 / QQ / 沃 上都成立。

---

## 163 邮箱（@163.com）

| 项 | 值 | 来源 | 状态 |
|---|---|---|---|
| IMAP | `imap.163.com:993` (IMAPS) | 社区实测 | ✅ **已实测**（2026-10-22，预认证阶段） |
| SMTP | `smtp.163.com:465` (SSL) | 官方/社区 | ✅ **已实测**（2026-10-22） |
| SMTP 备选 | `smtp.163.com:994` (STARTTLS) | 社区 | ✅ **已实测**（2026-10-22，隐式 TLS 也能握手） |
| 认证 | 16 位**客户端授权码** | [官方帮助](https://help.mail.163.com/faqDetail.do?code=d7a5dc8471cd0c0e8b4b8f4f8e49998b374173cfe9171305fa1ce630d7f67ac2a5feb28b66796d3b) | ✅ |
| 开启方式 | 网页版 → 设置 → POP/SMTP/IMAP → 开启 → 短信验证 | 官方 | ✅ |

### 真实发信往返实测（2026-10-29）

用本项目的 Rust 客户端做**双向真发信**（163 → QQ、QQ → 163），逐项核对：

| 项 | 163 | QQ |
|---|---|---|
| SMTP 提交 | ✅ `250 Mail OK queued as gzga-smtp-…` | ✅ `250 OK: queued as.`（**queue id 是空的**） |
| 对方收到 | ✅ 落在收件方 INBOX | ✅ 落在收件方 INBOX |
| **服务器自动存副本到「已发送」** | ✅ **存了**，提交后**几乎立刻**可见 | ✅ **也存了**，但**落盘慢**——实测要 **30–90 秒**才在 IMAP 里看得到 |
| 改写 `Message-ID`？ | ❌ 不改，原值就是我们的 | ⚠️ **改**！原值挪到 `X-OQ-MSGID` 头里 |
| 重复 `APPEND` 会怎样 | 拒（会变成两份） | 拒：`[CANNOT] Mail already saved to Sent by SMTP, no need to APPEND again` |
| `UID SEARCH` | ⚠️ **只支持 `ALL`**（见下） | ✅ 正常（`SUBJECT` / `HEADER` / `TEXT` 都能用） |

**四个坑**：

1. **`SEARCH` 对非 `ALL` 的条件静默返回空**。`UID SEARCH SUBJECT "…"`、
   `UID SEARCH HEADER Subject "…"`、`UID SEARCH TEXT "…"`
   在 163 上**全部返回空结果，且不报 `NO`**——看起来像「没搜到」，
   实际上是「这个服务器不支持这种搜法」。
   同步引擎**不受影响**（它走 `UID FETCH <UID 区间>`，从不用 `SEARCH`）。
   但任何依赖服务端搜索的功能在 163 上都会**静默失效**（ISSUES_AND_RISKS R22）。

2. **文件夹名在线路上是 modified UTF-7**。163 的 `LIST` 给的是
   `&XfJT0ZAB-`（= 已发送）、`&g0l6P3ux-`（= 草稿箱）、
   `&V4NXPpCuTvY-`（= 垃圾邮件）……
   拿**解码后**的名字去 `EXAMINE` 会得到
   `NO EXAMINE Folder not exist`——**只有 `raw_name` 能打开**。
   （同步引擎存的就是 `raw_name` ✓，`RemoteFolder::raw_name` 的文档写明了这点。）

3. **「服务器存不存副本」不能只看一眼**。
   第一次实测时，提交完立刻去查「已发送」，QQ 是空的，当时记的是「QQ 不存」——
   **这个结论是错的**：QQ 存了，只是**异步落盘**，实测 **30–90 秒**后才看得到。
   ⇒ 查副本**必须轮询**（实测代码里等满 120 秒），
   查一次就下结论会把「还没到」误判成「没有」。

4. **QQ 改写 `Message-ID`**，所以「找我们发出去的那封」不能只认 `Message-ID` 头。
   QQ 把我们的 `Message-ID: <…@qq.com>` 换成了自己的 `<1793325491.1@qq.com>`，
   原值放在 `X-OQ-MSGID` 里。
   ⇒ 匹配规则：**把每个头值里的尖括号段**（`<…>`）拿出来比，
   别拿整个头值比（`Received:` 那种头什么都有，会误撞）。

**自适应存副本的做法**（两家都对，不硬编码品牌）：

1. 发完，等一个**短的沉降窗口**（当前 2.5 秒）让服务器自己存；
2. 在「已发送」文件夹里按 `Message-ID` 找这封信（**尖括号段**比，见坑 4）；
3. 找到了 → 服务器自己存了，**不碰**；
4. 没找到 → 自己 `APPEND` 一份；
5. `APPEND` 被拒且拒的理由是「已经存过了」（QQ 的措辞）→ 照样当「已有一份」——
   **第二道保险**，万一我们没认出服务器存的那份，也不会补出第二份。

### 预认证实测记录（2026-10-22）

> **环境**：Windows 11，本机；客户端为 **Node.js 22.21.0 的 `tls` 模块**（连通性预探测，
> **不是**本项目的 Rust 客户端，Rust 客户端的结论要重测）。
> **区域/节点**：本机出口 IP 所在区域未知，未记录（后续需补）。
> **凭据**：本轮**未使用任何凭据**，只做预认证阶段探测。

**DNS 与连通性**

| 目标 | 解析结果 | TCP |
|---|---|---|
| `imap.163.com` | 220.197.33.205 / 220.197.33.210 | 993 ✅ / 143 ✅ |
| `smtp.163.com` | 220.197.33.215 / 220.197.33.205 | 465 ✅ / 994 ✅ |

**TLS（IMAP 993 与 SMTP 465/994 三者一致）**

| 项 | 观察值 |
|---|---|
| 协议版本 | TLSv1.3 |
| 加密套件 | `TLS_AES_256_GCM_SHA384` |
| 证书 subject | `CN=*.163.com, O=NetEase (Hangzhou) Network Co., Ltd, C=CN` |
| 证书 issuer | `CN=GeoTrust G2 TLS CN RSA4096 SHA256 2022 CA1, O=DigiCert, Inc.` |
| 有效期 | 2026-09-25 → 2027-04-11 |
| 主机名校验 | ✅ **通过**（系统根证书链，无需导入自签 CA） |

> **与 SECURITY §2.2 相关**：本次实测确认 163 的服务端证书由**公开 CA（DigiCert）**签发，
> 系统根即可校验通过——**不是**「国内 CA 一定缺失」的情况。
> 但这只是 163 一家的结论，不能推广到其他服务商。

**IMAP 预认证阶段**

| 观察 | 响应 |
|---|---|
| 问候行 | `* OK Coremail System IMap Server Ready(163com[…])` |
| `CAPABILITY` | `CAPABILITY BAD command not support` |
| `NOOP` | `NOOP BAD command not support` |
| `LOGOUT` | `LOGOUT BAD command not support` |

> **重要结论**：163（Coremail 体系）在**认证前拒绝一切 IMAP 命令**，
> 连 `LOGOUT` 都回 `BAD command not support`。
> ⇒ **能力集只能在登录之后获取**，预认证探测拿不到。
> 这与 [ARCHITECTURE §4.2]「使用认证后能力」一致，且明确了「认证后」不是可选项而是强制项。
> 探针实现时**不要**假设登录前能拿到 CAPABILITY。

**SMTP 预认证阶段（EHLO）**

| 观察 | 响应 |
|---|---|
| 问候行 | `220 163.com Anti-spam GT for Coremail System (163com[20141201])` |
| 能力行 | `250-mail` / `250-AUTH LOGIN PLAIN XOAUTH2` / `250-AUTH=LOGIN PLAIN XOAUTH2` / `250-coremail <base64>` / `250-STARTTLS` / `250-ID` / `250 8BITMIME` |

| 结论 | 状态 |
|---|---|
| 认证机制含 `LOGIN`（授权码走这个）与 `PLAIN`；另有 `XOAUTH2` | ✅ 已实测 |
| 扩展 `8BITMIME` | ✅ 已实测 |
| 扩展 `STARTTLS` | ✅ 已实测 |
| **无 `SIZE` 扩展** | ✅ 已实测（发信不能用 SIZE 优化） |
| **无 `PIPELINING`** | ✅ 已实测 |
| 994 端口在**隐式 TLS** 下也能握手成功，且仍宣告 `STARTTLS` | ✅ 已实测（**含义待确认**：994 究竟是明文 SMTP 端口还是也接受隐式 TLS，**一次观察不足以断言**，需用明文连一次对照） |

### 认证后实测记录（2026-10-22）

> **环境**：Windows 11，本机；客户端为 **本项目的 `xremail-protocol-imap`**
> （rustls 0.23 + async-imap 0.12）。这是**项目自己的客户端**的实测结论，
> 与上面那段 Node 预探测相互印证。
>
> **凭据**：用户本机专用测试账号的客户端授权码，仅存在于
> `%USERPROFILE%/.xremail/probe-163.txt`（**仓库之外**），全程未进 Git、未进日志。

#### ⚠️ 最重要的发现：必须先发 `ID`（RFC 2971），否则进不去邮箱

163（Coremail 体系）**认证前拒绝一切 IMAP 命令**（见上）。认证后还有第二道门：

| 顺序 | 结果 |
|---|---|
| `LOGIN` → `EXAMINE INBOX` | ❌ `NO EXAMINE Unsafe Login. Please contact kefu@188.com for help` |
| `LOGIN` → `SELECT INBOX` | ❌ `NO SELECT Unsafe Login. …` |
| `LOGIN` → `NOOP` → `SELECT INBOX` | ❌ 仍然 Unsafe Login |
| `LOGIN` → `CAPABILITY` → `SELECT INBOX` | ❌ 仍然 Unsafe Login |
| `LOGIN` → `ID NIL` → `SELECT INBOX` | ❌ 仍然 Unsafe Login（`NIL` 不算自报身份） |
| **`LOGIN` → `ID (...)` → `EXAMINE INBOX`** | ✅ `OK [READ-ONLY] Examine completed` |
| **`LOGIN` → `ID (...)` → `SELECT INBOX`** | ✅ `OK [READ-WRITE] SELECT completed` |

`ID` 的**内容不重要**：`("name" "X")`、`("version" "1.0")`、`("name" "")`
都能通过；只要不是 `ID NIL` 就行。关键是**发过这个命令**。

> **实现要求**：服务器宣告 `ID` 能力时，登录后必须立即发一次 `ID`，
> 然后才能访问邮箱。本项目已在 `xremail-protocol-imap` 的
> `connect_and_login` 里实现（见 `CLIENT_IDENTIFICATION`）。
> 这条对**其他服务商也可能成立**，不能只当 163 的特例。

#### 认证后能力集（完整）

```
IMAP4rev1  ID  LITERAL+  SASL-IR  SPECIAL-USE  STARTTLS  UIDPLUS
AUTH=XOAUTH2  APPENDLIMIT=71680000
X-CM-EXT-1  XAPPLEPUSHSERVICE  XLIST
```

| 观察 | 说明 |
|---|---|
| 认证机制**只宣告 `AUTH=XOAUTH2`** | 但基础命令 `LOGIN` 可用——RFC 3501 的 `LOGIN` 本就不需要在 CAPABILITY 里宣告。163 的授权码走 `LOGIN`。 |
| `APPENDLIMIT=71680000` | 单封邮件上限 ~68 MiB。**没有 `SIZE` 扩展**（SMTP 侧同样没有），发信不能靠 SIZE 预检。 |
| `X-CM-EXT-1` / `XAPPLEPUSHSERVICE` / `XLIST` | 网易与苹果的私有扩展，不依赖。 |
| 服务器自报 `ID` | `("name" "Coremail Imap" "vendor" "Mailtech" "TransID" "…")` |

#### 扩展支持度（认证后实测）

| 扩展 | 支持 | 对同步引擎的影响 |
|---|---|---|
| **IDLE** | ❌ | **只能用轮询**。ARCHITECTURE 的「实时模式」对 163 不成立 |
| **CONDSTORE** | ❌ | 没有 `UNCHANGEDSINCE` 条件写 → 走 ARCH §6 的「读快照 + 重放本地意图 + 回读确认」路径 |
| QRESYNC | ❌ | 与 CONDSTORE 一致 |
| **UIDPLUS** | ✅ | `COPY` / `MOVE` 能拿到目标 UID 与 UIDVALIDITY |
| **MOVE** | ❌ | 移动只能 `COPY` + `\Deleted` + `EXPUNGE`（与 PROTOCOLS 原有推测**一致**） |
| LIST-EXTENDED | ❌ | 但 `SPECIAL-USE` 在，特殊用途属性**照样随 LIST 返回**（见下） |
| NAMESPACE | ❌ | 没有层级声明，但 LIST 响应里的分隔符是 `/` |
| UTF8=ACCEPT | ❌ | 不宣告；RFC 3501 默认就是 UTF-8 |
| ID | ✅ | 但它是**准入条件**，不只是可选信息 |
| QUOTA | ❌ | 查不到配额 |
| LITERAL+ | ✅ | |
| LITERAL- | ❌ | |

#### 文件夹

`LIST "" "*"` 返回 6 个，分隔符 `/`：

| 特殊用途属性 | 解码后的名字 |
|---|---|
| （无） | `INBOX` |
| `\Drafts` | 草稿箱 |
| `\Sent` | 已发送 |
| `\Trash` | 已删除 |
| `\Junk` | 垃圾邮件 |
| （无） | 病毒文件夹 |

> **IMAP 邮箱名在线路上是 modified UTF-7**（RFC 3501 §5.1.3），
> 例如「已发送」在线上是 `&XfJT0ZAB-`。**不解码就认不出角色**。
> 本项目已在 `xremail-protocol-imap::utf7` 实现编解码（含 11 个用例）。
>
> 注意：163 **不给** `\All` / `\Archive` 之类的属性给「病毒文件夹」，
> 也没有给「所有邮件」标 `\All`——角色识别不能只靠 `INBOX` 之外的猜测。

#### 邮箱状态

`EXAMINE INBOX`（只读）实测：

```
* 4 EXISTS
* 1 RECENT
* OK [UIDVALIDITY 1] UIDs valid
* FLAGS (\Answered \Seen \Deleted \Draft \Flagged)
* OK [PERMANENTFLAGS (\Answered \Seen \Deleted \Draft \Flagged)] Limited
```

| 观察 | 影响 |
|---|---|
| `UIDVALIDITY = 1` | epoch 很「年轻」，但**不能假定 UID 从 1 开始** |
| **UID 实际是 1792658701 ~ 1792658704** | UID 空间很大，**不是** 1..N。差分逻辑不能假设连续 |
| `PERMANENTFLAGS` **不含 `\Recent`** | 响应码是 `Limited`。客户端不能自己设 `\Recent`（本来也不该） |
| `EXAMINE` **不报 `UNSEEN`** | 未读数必须用 `STATUS` 拿（`STATUS` 给 `UNSEEN 1`） |
| **`UIDNEXT` 拿不到** | `STATUS INBOX (UIDNEXT)` 返回 `* STATUS "INBOX" ()`——字段被接受但**没有值**。⇒ 同步游标**不能**基于 UIDNEXT，要靠 `UID SEARCH` / `UID FETCH 1:*` 取最大 UID |
| **`HIGHESTMODSEQ` 会让命令失败** | `STATUS INBOX (HIGHESTMODSEQ)` → `BAD Parse command error`（与不支持 CONDSTORE 一致）。**不要**无条件请求它 |
| `STATUS` 可用的项 | `MESSAGES` `RECENT` `UNSEEN` `UIDVALIDITY` ✅；`UIDNEXT` 空；`HIGHESTMODSEQ` ❌ |

**仍待实测**：并发连接上限、IDLE 不可用时的服务端行为。
（SMTP 提交行为已于 2026-10-28 探测、2026-10-29 真实收发往返补测。）

**已补测（2026-10-23）**：

- **邮件头字符集抽样**：163 的正文 charset 全是 `utf-8`（4/4 封），
  主题也是 `utf-8`；顶层**全是 multipart**（4/4）⇒ 只看顶层看不到正文 charset。
  三家的完整对照见 [字符集矩阵](#字符集矩阵)。
- **`BODY.PEEK` 不置 `\Seen`**：已在假服务器上实现并用端到端用例钉住
  （`BODY.PEEK[]` 不改标记，`BODY[]` 会改）。三家真实邮箱的实测仍待做。

**已知坑**

- 授权码页面**只显示一次**，必须当场保存；之后可再次生成新的。
- 官方原文："一个授权码可以同时设置多个客户端" → 适合多端共用。
- 首次开启需**手机短信验证**（扫码或手动发短信）。
- 单账号同时在线客户端数受限（社区反馈 3~5），多端同时 IDLE 会互踢。
- 大量邮件使用 **GB2312/GBK** 编码，且部分 `Subject` 头未声明 charset。
- 服务端对 `UID MOVE` 的支持情况需实测；若不支持需走 `COPY` + `\Deleted` + `EXPUNGE` 路径。

---

## 126 邮箱（@126.com）

与 163 同体系，参数仅主机名不同。

| 项 | 值 | 状态 |
|---|---|---|
| IMAP | `imap.126.com:993` | ⚠️ 待实测（**本机凭据文件里没有 126 账号**，需用户补一个授权码） |
| SMTP | `smtp.126.com:465` | ⚠️ 待实测（同上：缺 126 账号） |
| 认证 | 授权码（同 163 流程） | ✅ |

**待办**：126 的读写两侧都还没实测——**不是忘了，是没有可用的 126 账号**。
补上授权码后，用 `xremail-smtp-probe --provider 126` 走一遍 SMTP，
再用 `xremail-sync-smoke` 走一遍 IMAP，然后把两份结果贴回本表。

---

## 网易企业邮（@qiye.163.com）

⚠️ **节点不同主机名不同**，必须支持用户手动填写服务器地址。

| 项 | 值 | 状态 |
|---|---|---|
| IMAP（华东） | `imap.qiye.163.com:993` | [官方手册](https://qiye.163.com/doc/%E5%A6%82%E4%BD%95%E8%BF%9B%E8%A1%8C%E5%AE%A2%E6%88%B7%E7%AB%AF%E8%AE%BE%E7%BD%AE.pdf) |
| IMAP（华北） | `imapbj.qiye.163.com` / `imaphz.qiye.163.com` | 官方手册（按账号归属） |
| SMTP | `smtp.qiye.163.com:465` / `994` / `587` | 官方手册 |
| 认证 | 企业邮箱授权码（[申请入口](https://mail.qiye.163.com/static/commonweb/auth/authcode.html)） | 官方 |

**已知坑**

- 同一企业内不同账号可能落在**不同区域节点**（华东/华北/华南），服务器地址不同。
- 官方文档提醒：同一浏览器同一时间只能登录一个企业账号。
- 建议在产品里提供"连接诊断"自动列出可用地址并测试。

---

## QQ 邮箱（@qq.com）

### 真实发信往返实测（2026-10-29）

见上文 163 一节的对照表。QQ 这边三个关键点：

- **`SEARCH` 正常**（`SUBJECT` / `HEADER` / `TEXT` 都有结果）——
  和 163 正好相反，所以**搜索功能不能假定三家一致**。
- **QQ 会改写 `Message-ID`**，原值挪到 `X-OQ-MSGID` 头里。
  ⇒ 「找我们发出去的那封」要比**头值里尖括号的那一段**，
  只认 `Message-ID` 头会认不出自己的信。
- **QQ 的副本落盘很慢**：SMTP 提交后 **30–90 秒**才在 `Sent Messages` 里看得到
  （163 几乎立刻就有）。
  ⇒ 查副本**必须轮询**，不能查一次就下结论（见上文第 3 条）。
  ⇒ QQ 还会拒绝重复 `APPEND`：`[CANNOT] Mail already saved to Sent by SMTP, no need to APPEND again`
  ——这是「服务器自己存了」的第二道保险。

| 项 | 值 | 来源 | 状态 |
|---|---|---|---|
| IMAP | `imap.qq.com:993` (IMAPS) | 实测（2026-10-23） | ✅ **已实测** |
| SMTP | `smtp.qq.com:465` (SSL) | 实测（2026-10-28） | ✅ **已实测**（TLS 1.2 + `AUTH LOGIN` 认证通过；详见「SMTP 实测」小节） |
| 认证 | 16 位**授权码** | [官方帮助](https://help.mail.qq.com/detail/106/985) | ✅ |
| 开启方式 | 设置 → 账号与安全 → 安全设置 → 开启 POP3/IMAP/SMTP → 生成授权码 | 官方 | ✅ |

### 认证后实测记录（2026-10-23）

只读探测（LOGIN → CAPABILITY → LIST → EXAMINE → IDLE），
服务器自称 `QQMail XMIMAP4Server`。

| 项 | 实测值 |
|---|---|
| TLS | TLSv1_3 / TLS13_AES_256_GCM_SHA384，rustls 系统根校验通过 |
| 认证后能力集 | `CHILDREN` `COMPRESS=DEFLATE` `ID` `IDLE` `IMAP4` `IMAP4rev1` `MOVE` `NAMESPACE` `UIDPLUS` `XAPPLEPUSHSERVICE` `XLIST` |
| 认证前能力集 | `IMAP4` `IMAP4rev1` `ID` `AUTH=PLAIN` `AUTH=LOGIN` `AUTH=XOAUTH2` `NAMESPACE` |
| IDLE | ✅ **服务器接受**（`+ idling` 续行正常）——三家里唯一 |
| MOVE | ✅ 有（163 / 沃都没有） |
| CONDSTORE / QRESYNC | ❌ 无 → 条件写只能走「读快照 + 重放意图 + 回读确认」 |
| UIDPLUS | ✅ 有 |
| NAMESPACE | ✅ `NAMESPACE (("" "/")) NIL NIL` |
| LIST-EXTENDED / UTF8 / QUOTA / LITERAL+ | ❌ 都没有（但宣告了 XLIST） |
| 文件夹 | 7 个，分隔符 `/` |
| INBOX | 70 封 / 15 未读 |
| UIDVALIDITY | **1792993698**（大数） |
| UIDNEXT | 105 |
| 永久标记 | EXAMINE 不回 PERMANENTFLAGS |

**已知坑**

- **修改 QQ 密码会让所有已生成的授权码立即失效** → 需要良好错误提示引导用户重新生成。
- 授权码生成需要扫码（微信/QQ）或密码验证。
- 客户端必须是 16 位授权码，**不能用 QQ 密码**（官方明确警告）。
- 有反垃圾策略：对高频/异常 IP 的 SMTP 提交会返回 `550 Spam is embedded` 等拒绝码。
- **LIST 不返回 RFC 6154 的特殊用途属性**：实测只给 `\NoSelect` / `\HasChildren`，
  尽管能力集里有 XLIST。所以「已发送 / 草稿箱 / 垃圾邮件」只能**按名字**认
  （实测名：`Sent Messages` / `Drafts` / `Deleted Messages` / `Junk`）。
  **不能只靠属性认角色**——这是 QQ 独有的坑。
- **UIDVALIDITY 是大数而 UIDNEXT 很小**（1792993698 vs 105）：
  说明服务器重建过邮箱。这正好说明 epoch 与 UID 空间**无关**，
  epoch 切换逻辑（ARCHITECTURE §4.3）在 QQ 上很可能是常态路径。
- **不需要先发 `ID`**：实测登录后直接 EXAMINE 就成功（与 163 相反）。

---

## 沃邮箱（@wo.cn）

| 项 | 值 | 来源 | 状态 |
|---|---|---|---|
| IMAP | `imap.wo.cn:993` | 实测（2026-10-23） | ✅ **已实测** |
| SMTP | `smtp.wo.cn:465` | 实测（2026-10-28） | ❌ **实测连不上**：服务器只接受**静态 RSA** 密钥交换套件，rustls 不支持（详见「SMTP 实测」小节） |
| POP | `pop.wo.cn` | [应用市场说明](https://mail.wo.cn/) | ⚠️ 待实测 |
| 认证 | **授权码**（实测：授权码可登录） | [荣耀社区回复](https://club.honor.com/cn/thread-29278971-1-1.html) | ✅ 已实测 |

### 认证后实测记录（2026-10-23）

只读探测。服务器自称 **`Coremail System IMap Server Ready(wo.cn[...])`**——
即**与 163 同源的 Coremail 体系**。

| 项 | 实测值 |
|---|---|
| TLS | TLSv1_3 / TLS13_AES_256_GCM_SHA384，rustls 系统根校验通过 |
| 认证后能力集 | `APPENDLIMIT=20971520` `ID` `IMAP4rev1` `LITERAL+` `SPECIAL-USE` `STARTTLS` `UIDPLUS` `XLIST` |
| IDLE | ❌ **没有**（只能轮询） |
| CONDSTORE / QRESYNC / MOVE / NAMESPACE | ❌ 都没有（发 `NAMESPACE` 回 `BAD command not support`） |
| UIDPLUS | ✅ 有 |
| LIST-EXTENDED | ❌ 不宣告，但 **LIST 实际带 RFC 6154 属性**（`\Sent` / `\Drafts` / `\Trash` / `\Junk`） |
| 文件夹 | 6 个，分隔符 `/`，名是英文：`Sent Items` / `Drafts` / `Trash` / `Junk E-mail` / `Virus Items` |
| INBOX | 6 封 / 2 未读 / 2 RECENT |
| UIDVALIDITY | 1 |
| UIDNEXT | **不给**（与 163 一样） |
| 永久标记 | `Answered` `Seen` `Deleted` `Draft` `Flagged` |

**已知坑**

- **沃邮箱是 Coremail 体系**（帮助中心与 163 同源），
  **不是**先前推测的 Coxremail 体系——实测问候行就是 `Coremail System IMap Server Ready(wo.cn[...])`。
- 官方口径已改为“客户端登录必须使用授权码，不能使用原密码”。
- 账号可以是**手机号**（如 `13012345678@wo.cn`）。
- **不需要先发 `ID`**：实测登录后直接 EXAMINE 就成功。
  即：`ID` 是 **163 独有**的要求，不是 Coremail 家族的通例。
- 无 UIDNEXT → 游标只能靠 UID 差分取最大 UID。
- 文件夹名是**英文**，但 LIST 给了角色属性 → 角色识别靠属性即可。

---

## Gmail

> **唯一必须走 OAuth2 的服务商。** 详见 [ADR-0003](adr/0003-gmail-oauth-verified.md)。
> ⚠️ **当前暂缓接入**：Gmail 不在 P1 范围，下表仅作背景参考。恢复接入时需重新核实授权流程与发布要求。

| 项 | 值 | 来源 | 状态 |
|---|---|---|---|
| IMAP | `imap.gmail.com:993` | [官方](https://developers.google.com/workspace/gmail/imap/imap-smtp) | ✅ |
| SMTP | `smtp.gmail.com:465` | 官方 | ✅ |
| 认证 | `AUTH XOAUTH2` | [官方](https://developers.google.com/workspace/gmail/imap/xoauth2-protocol) | ✅ |

**已核实的官方政策**

| 生效时间 | 政策 | 来源 |
|---|---|---|
| 2025-03-14 | Google Workspace 全面禁止第三方应用使用密码基础认证 | [Workspace 帮助](https://support.google.com/a/answer/14114704) |
| 2025-05-01 | Workspace 账号不再支持 "less secure apps" | [Google 帮助](https://support.google.com/mail/answer/12103) |
| 持续 | IMAP/POP/SMTP **协议本身未下线**，但 Workspace 必须用 OAuth | 官方 |

**OAuth 客户端注册要求**

- 每个平台**单独注册**一个 OAuth Client（Windows 安装版 / Android / iOS / Web）。
- 桌面应用必须走 **Loopback**（本地 HTTP 回调）或 **Device Code**（设备码，用户在手机浏览器完成）流程。
- 应用处于 **Testing** 状态时：refresh token **会过期**（官方文档为 7 天），且**限 100 个测试用户**。
  > 实施时以 Google 控制台 "OAuth app state overview" 页面最新表格为准。
- 敏感 scope（`https://mail.google.com/`）需提交**验证审核**：要求域名网站、隐私政策页、同意屏品牌化。
- 审核周期 1~3 周 → **必须在 P1 第 1 周就启动**。

> ⚠️ 上方为原始研究结论，未在 Google 控制台实际核验，且 Gmail 已暂缓。
> 恢复接入时以官方最新文档为准，不假定 Device Code 流程适用于本项目形态。

**开发期兜底**

- 开启两步验证的个人账号可用「应用专用密码」走基础认证，不阻塞开发。
- 产品内提供"使用自己的 OAuth Client ID"入口。

> ⚠️ 个人账号 + 应用专用密码是否仍可用，需在恢复接入时实测确认。

---

## Exchange / Outlook 365

| 项 | 值 | 状态 |
|---|---|---|
| IMAP | `outlook.office365.com:993` | 社区 |
| SMTP | `smtp.office365.com:587` (STARTTLS) | 社区 |
| 认证 | OAuth2 / 应用注册 | Basic Auth 已被微软禁用 |

> P1 不实现，列入 P5+。

---

## SMTP 实测（2026-10-28 起）

> P3 发信的前置。提交路径要求「命令必须自证成败」（见 [ARCHITECTURE §2.1](ARCHITECTURE.md)），
> 所以在写 `MAIL FROM` 之前，必须先弄清每家的握手语义。

### 服务器地址候选（**不按品牌推定，逐个实测**）

| 服务商 | 候选地址 | 来源 | 验证状态 |
|---|---|---|---|
| 163 | `smtp.163.com:465`（隐式 TLS） | 官方/社区 | ✅ **已实测**（2026-10-28，本项目 Rust 客户端：TLS 1.3 + `AUTH LOGIN` 认证通过） |
| 126 | `smtp.126.com:465` | 社区 | ⚠️ 待实测（**本机凭据文件里没有 126 账号**，需要用户补一个授权码） |
| QQ | `smtp.qq.com:465` | 社区 | ✅ **已实测**（2026-10-28，本项目 Rust 客户端：TLS 1.2 + `AUTH LOGIN` 认证通过） |
| 沃 | `smtp.wo.cn:465` | 社区 | ❌ **实测连不上**（2026-10-28：服务器只接受**静态 RSA** 密钥交换套件，rustls 不支持——见下面「沃」的记录） |
| 网易企业邮 | `smtp.qiye.163.com:465` / `:994` / `:587` | 官方手册 | ⚠️ 待实测（**节点不同主机名不同，必须支持用户手填**） |
| Exchange / 365 | `smtp.office365.com:587`（STARTTLS） | 社区 | P1 不实现，列入 P5+ |

### 怎么测

用 `xremail-smtp-probe`（诊断工具，只读，**不会投递任何邮件**）：

```bash
# 列出凭据文件里配好的 provider
cargo run -p xremail-diagnostics --bin xremail-smtp-probe -- --list

# 只探测不认证（不读口令、不发口令）
cargo run -p xremail-diagnostics --bin xremail-smtp-probe -- --provider 163 --no-auth

# 隐式 TLS（465）
cargo run -p xremail-diagnostics --bin xremail-smtp-probe -- --provider 163 --host smtp.163.com --port 465

# 明文 + STARTTLS（587）
cargo run -p xremail-diagnostics --bin xremail-smtp-probe -- --provider 126 --host smtp.126.com --port 587 --tls-mode starttls
```

口令只从 `%USERPROFILE%/.xremail/accounts.txt` 读，**不出现在命令行**；报告里账号会脱敏成 `u***@example.com`。

> 工具**只实现** `EHLO` / `STARTTLS` / `AUTH` / `QUIT` 四个动词，**没有任何 `MAIL FROM` / `RCPT TO` / `DATA` 代码路径**——
> 不是靠开关关掉，是压根不存在，探测阶段不可能误发邮件。

### 报告里要读什么

| 报告字段 | 决定什么 |
|---|---|
| `## EHLO 扩展` 逐条 | `8BITMIME`（正文能否带 8 位）、`SIZE`（单封上限）、`PIPELINING`、`SMTPUTF8`（标题是否要 RFC 2047 编码） |
| `## 认证` 的机制列表与结果 | 填 `smtp_auth`：`login` / `plain` / `xoauth2`；失败时带服务器原文（如 `535 …`） |
| `## TLS` 段 | 填 `smtp_tls`：`implicit` / `starttls`；以及证书 CN、签发者、过期日 |
| STARTTLS 前后两份 EHLO | 有些服务器**只在 TLS 之后**才宣告 `AUTH`——这决定认证该放在升级前还是升级后 |

### 实测记录：163 / SMTP / 2026-10-28

> 用的是**本项目自己的客户端**（`xremail-protocol-smtp`，rustls，强制校验系统根），
> 不是外部工具的预探测——所以这份记录**可以直接作为 P3 发信实现的依据**。

#### Xremail SMTP 探测报告

- 目标       : smtp.163.com:465
- provider   : 163
- 账号       : x***@163.com
- 凭据来源   : %USERPROFILE%\.xremail\accounts.txt
- TLS 模式   : direct（隐式 TLS）
- 信任来源   : 系统根（rustls-native-certs）
- 客户端     : xremail-protocol-smtp（rustls，强制校验系统根）
- TLS 版本   : 默认 1.3+1.2（服务器拒绝 ClientHello 时改用 1.2 重试，报告会写明）

## TLS

| 协议版本 | 加密套件 | 校验 | 证书链长度 |
|---|---|---|---|
| TLSv1.3 | TLS13_AES_256_GCM_SHA384 | ✅ 通过（rustls 强制校验） | 2 |

| 证书主题 | 签发者 | 生效日 | 过期日 |
|---|---|---|---|
| *.163.com | GeoTrust G2 TLS CN RSA4096 SHA256 2022 CA1 | 2026-09-25 | 2027-04-11 |

- 证书链体检：非自签；剩余 164 天；未过期
- 主题全名：`C=CN, ST=Zhejiang, L=Hangzhou, O=NetEase (Hangzhou) Network Co., Ltd, CN=*.163.com`
- 签发者全名：`C=US, O=DigiCert, Inc., CN=GeoTrust G2 TLS CN RSA4096 SHA256 2022 CA1`

## 服务器问候与能力

- 问候原文：`220 163.com Anti-spam GT for Coremail System (163com[20141201])`
- STARTTLS：未执行（隐式 TLS，连上就是 TLS）

## EHLO 扩展

| 扩展（服务器原文） | 支持 | 对本项目的意义 |
|---|---|---|
| `AUTH LOGIN PLAIN XOAUTH2` | ✅ | 可用的认证机制 |
| `AUTH=LOGIN PLAIN XOAUTH2` | ✅ | 可用的认证机制 |
| `coremail 1Uxr2xKj7kG0xkI17xGrU7I0s8FY2U3Uj8Cz28x1UUUUU7Ic2I0Y2UrZvW1QUCa0xDrUUUUj` | — | （本项目暂未归类的扩展） |
| `STARTTLS` | ✅ | 支持明文连接后升级到 TLS |
| `ID` | — | （本项目暂未归类的扩展） |
| `8BITMIME` | ✅ | 可发 8 位正文（`BODY=8BITMIME`） |

## 认证

- AUTH 机制（TLS 阶段）：LOGIN, PLAIN, XOAUTH2
- AUTH 机制（明文阶段）：（未宣告）
- 认证结果：✅ 成功（机制 LOGIN）—— `235 Authentication successful`

## 建议配置

可以直接抄进应用配置的 SMTP 参数块（取值与 xremail-core 领域模型的 serde 名一致）：

```toml
smtp_host = "smtp.163.com"
smtp_port = 465
smtp_tls = "implicit"
smtp_auth = "login"
```

- `smtp_tls`：`implicit` = 隐式 TLS（465）；`starttls` = 明文 + STARTTLS（587）。
- `smtp_auth`：`login` = `AUTH LOGIN`；`plain` = `AUTH PLAIN`；`xoauth2` = `AUTH XOAUTH2`（本工具未实现）。
- 支持 `8BITMIME`：发信时可以声明 `BODY=8BITMIME`。
- **不支持** `SMTPUTF8`：标题里的非 ASCII 字符要按 MIME 编码（RFC 2047）。

**结论**：`smtp_tls = "implicit"`、`smtp_auth = "login"`。支持 `8BITMIME`、**不支持** `SMTPUTF8`
（标题里的非 ASCII 字符要走 MIME / RFC 2047 编码）。

---

### 实测记录：QQ / SMTP / 2026-10-28

#### Xremail SMTP 探测报告

- 目标       : smtp.qq.com:465
- provider   : qq
- 账号       : 2***@qq.com
- 凭据来源   : %USERPROFILE%\.xremail\accounts.txt
- TLS 模式   : direct（隐式 TLS）
- 信任来源   : 系统根（rustls-native-certs）
- 客户端     : xremail-protocol-smtp（rustls，强制校验系统根）
- TLS 版本   : 默认 1.3+1.2（服务器拒绝 ClientHello 时改用 1.2 重试，报告会写明）

## TLS

| 协议版本 | 加密套件 | 校验 | 证书链长度 |
|---|---|---|---|
| TLSv1.2 | TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 | ✅ 通过（rustls 强制校验） | 3 |

| 证书主题 | 签发者 | 生效日 | 过期日 |
|---|---|---|---|
| *.mail.qq.com | DigiCert Secure Site OV G5 TLS CN RSA4096 SHA384 2022 CA1 | 2026-10-23 | 2027-05-09 |

- 证书链体检：非自签；剩余 192 天；未过期
- 主题全名：`C=CN, ST=Guangdong Province, L=Shenzhen, O=Tencent Technology (Shenzhen) Company Limited, CN=*.mail.qq.com`
- 签发者全名：`C=US, O=DigiCert, Inc., CN=DigiCert Secure Site OV G5 TLS CN RSA4096 SHA384 2022 CA1`

## 服务器问候与能力

- 问候原文：`220 newxmesmtplogicsvrsza63-0.qq.com XMail Esmtp QQ Mail Server.`
- STARTTLS：未执行（隐式 TLS，连上就是 TLS）

## EHLO 扩展

| 扩展（服务器原文） | 支持 | 对本项目的意义 |
|---|---|---|
| `PIPELINING` | ✅ | 支持命令流水线 |
| `SIZE 73400320` | ✅ | 单封邮件大小上限（字节） |
| `AUTH LOGIN PLAIN XOAUTH XOAUTH2` | ✅ | 可用的认证机制 |
| `AUTH=LOGIN` | ✅ | 可用的认证机制 |
| `MAILCOMPRESS` | — | （本项目暂未归类的扩展） |
| `SMTPUTF8` | ✅ | 可发 UTF-8 邮箱地址与非 ASCII 标题 |
| `8BITMIME` | ✅ | 可发 8 位正文（`BODY=8BITMIME`） |

## 认证

- AUTH 机制（TLS 阶段）：LOGIN, PLAIN, XOAUTH, XOAUTH2
- AUTH 机制（明文阶段）：（未宣告）
- 认证结果：✅ 成功（机制 LOGIN）—— `235 Authentication successful`

## 建议配置

可以直接抄进应用配置的 SMTP 参数块（取值与 xremail-core 领域模型的 serde 名一致）：

```toml
smtp_host = "smtp.qq.com"
smtp_port = 465
smtp_tls = "implicit"
smtp_auth = "login"
```

- `smtp_tls`：`implicit` = 隐式 TLS（465）；`starttls` = 明文 + STARTTLS（587）。
- `smtp_auth`：`login` = `AUTH LOGIN`；`plain` = `AUTH PLAIN`；`xoauth2` = `AUTH XOAUTH2`（本工具未实现）。
- 单封邮件大小上限：`SIZE 73400320` 字节。
- 支持 `8BITMIME`：发信时可以声明 `BODY=8BITMIME`。
- 支持 `SMTPUTF8`：可以用 UTF-8 邮箱地址与非 ASCII 标题。
- 支持 `PIPELINING`：可以流水线下发命令。

**结论**：`smtp_tls = "implicit"`、`smtp_auth = "login"`。
支持 `8BITMIME` / `SMTPUTF8` / `PIPELINING`，单封上限 `SIZE 73400320`（约 70MB）。
注意：**QQ 的 SMTP 证书主题是 `*.mail.qq.com`**（SAN 里另含 `smtp.qq.com`，主机名校验通过）。

---

### 实测记录：沃 / SMTP / 2026-10-28 —— ❌ 连不上

#### Xremail SMTP 探测报告

- 目标       : smtp.wo.cn:465
- provider   : wo
- 账号       : 1***@wo.cn
- 凭据来源   : %USERPROFILE%\.xremail\accounts.txt
- TLS 模式   : direct（隐式 TLS）
- 信任来源   : 系统根（rustls-native-certs）
- 客户端     : xremail-protocol-smtp（rustls，强制校验系统根）
- TLS 版本   : 默认 1.3+1.2（服务器拒绝 ClientHello 时改用 1.2 重试，报告会写明）

## 结果

- **失败阶段**: 服务器拒绝了 TLS 握手: received fatal alert: HandshakeFailure（已知：smtp.wo.cn 要求 ClientHello 里带静态 RSA 密钥交换套件（TLS_RSA_WITH_*），而 rustls 只支持 ECDHE；换 TLS 版本无效，需要系统原生 TLS 才能连上）
- 可重试     : true
- 需用户处理 : false

**根因（已用 `openssl s_client` 交叉验证）**：

| 客户端 | 结果 |
|---|---|
| `openssl s_client -connect smtp.wo.cn:465 -tls1_2` | ✅ 连上，协商出 `AES128-GCM-SHA256`，即 **`TLS_RSA_WITH_AES_128_GCM_SHA256`**（**静态 RSA** 密钥交换） |
| 本项目（rustls）默认 1.3+1.2 | ❌ `received fatal alert: HandshakeFailure` |
| 本项目（rustls）强制 `--tls12-only` | ❌ 同样 `HandshakeFailure` |

所以问题**不是 TLS 版本**，而是**密钥交换套件**：沃的 SMTP 只接受 `TLS_RSA_WITH_*`
这类**静态 RSA** 套件，而 **rustls 只支持 ECDHE/PSK，没有静态 RSA**。
OpenSSL 与 Windows SChannel 都带静态 RSA 套件，所以它们能连上。

**这对项目意味着什么**（必须诚实写清，别假装能连）：

- 沃邮箱的 **IMAP 侧完全正常**（`imap.wo.cn:993` 已实测可读，见本文档「沃邮箱」小节）。
- 沃邮箱的 **SMTP 侧在 rustls 上不可用**。要支持它，只有三条路：
  1. 给 SMTP 传输层加一个**可选的系统原生 TLS 后端**（Windows SChannel / OpenSSL），
     与 rustls 后端并存，按服务器探测结果选择；
  2. 用 `native-tls` 单独做一个 SMTP 后端（同样支持静态 RSA）；
  3. 放弃沃的发信能力，只保留收信。
- 这条路**不在 P3 的首发范围**里。P3 先做 163 / QQ（两者已实测可连），
  沃的发信单列为待办，**不要在计划里假装它能发**。

### 提交路径的状态（2026-10-28 发信链路落地后；2026-10-29 真实收发往返已补测）

上面三份探测记录**直接决定了发信能不能做**：

| 服务商 | 探测结论 | 提交路径 |
|---|---|---|
| 163 | TLS 1.3 可连、`AUTH LOGIN` 成功、有 `8BITMIME` | ✅ 可发（**2026-10-29 已用真实测试账户发出过**，双向往返通过） |
| QQ | TLS 1.2 可连、`AUTH LOGIN` 成功、有 `8BITMIME` / `SMTPUTF8` | ✅ 可发（同上，已真实发出） |
| 沃 | ❌ 静态 RSA 套件，rustls 连不上 | ❌ **发不了**——要换 TLS 后端（见上） |

发信路径与探测路径**用同一套 TLS 信任与同一套回退逻辑**：
TLS 1.3 被服务器拒绝时，用同一条链路重试 TLS 1.2，
**证书校验一点不放松**（回退只发生在协议版本上）。
对沃**无效**——问题在密钥交换套件，不在版本。

**已补测的一次实测（2026-10-29）**：发信之后服务器会不会 `APPEND` 一份到「已发送」。
**结论：两家都会自动存副本**（163 几乎立刻可见，QQ 需 30–90 秒落盘），
而且两家**都会拒绝重复 `APPEND`**（QQ 直接回 `Mail already saved to Sent by SMTP`）。
⇒ 程序**不自动**往「已发送」里塞副本：两边都存一份就是两份，用户会收到重复的邮件。

**还没做的**：`SMTPUTF8`（RFC 6531）——QQ 宣告了、163 没有。
地址与标题的编码策略定下来之前不发它，否则服务器会拒收整个事务。
当前只能提交全 ASCII 的信封地址 + RFC 2047 编码的标题。

---

## IDLE / CONDSTORE 支持度矩阵

> 实测后填 ✅/❌/⚠️。这是同步引擎能否用实时模式的关键依据。

| 服务商 | IMAP IDLE | CONDSTORE | QRESYNC | UIDPLUS | 备注 |
|---|---|---|---|---|---|
| 163 | ❌ **实测无** | ❌ **实测无** | ❌ **实测无** | ✅ 实测有 | **只能用轮询**；无 CONDSTORE → 走「快照+意图重放」；无 MOVE → COPY+\Deleted+EXPUNGE；**无 UIDNEXT** → 游标靠 UID SEARCH/FETCH 取最大 UID；**必须先发 `ID`** |
| 126 | ⚠️ 待实测 | ⚠️ 待实测 | ⚠️ 待实测 | ⚠️ 待实测 | |
| QQ | ✅ **实测有** | ❌ **实测无** | ❌ **实测无** | ✅ 实测有 | **三家里唯一支持 IDLE 的**；无 CONDSTORE → 走「快照+意图重放」；**有 MOVE** → 可一步移动；**不发 RFC 6154 角色属性** → 文件夹角色靠名字认；**不需要先发 `ID`** |
| 沃邮箱 | ❌ **实测无** | ❌ **实测无** | ❌ **实测无** | ✅ 实测有 | **Coremail 体系（与 163 同源，不是 Coxremail）**；无 MOVE → COPY + \Deleted + EXPUNGE；**不需要先发 `ID`**；无 NAMESPACE（发了回 `BAD`） |
| Gmail | ✅ 预期支持 | ⚠️ 待实测 | ⚠️ 待实测 | ⚠️ 待实测 | |
| Exchange | ✅ 预期支持 | ⚠️ 待实测 | ⚠️ 待实测 | ⚠️ 待实测 | |

**通用降级策略**（代码必须实现，与上表无关）：

1. `CAPABILITY` 探测，不支持 IDLE → 直接进轮询模式。
2. 支持 IDLE 但连接被服务端踢（收到 `BYE` 或登录冲突）→ 指数退避后重试，连续失败 N 次 → **自动降级为轮询**并通知用户。
3. 任何模式都要有兜底轮询（默认 5 分钟），保证 IDLE 不可靠时仍能收到新邮件。

---

## 字符集矩阵

> **2026-10-23 已实测**（三家各抽 20 封，只统计编码名，不打印内容）。

| 服务商 | 正文 charset（实测） | 主题 charset（实测） | 顶层 multipart | 传输编码 |
|---|---|---|---|---|
| 163 | `utf-8`×4 | `utf-8`×4 | 4/4 | — |
| QQ | **`gb18030`×1、[`gb2312`×5**]、`utf-8`×14 | `gb2312`×2、`utf-8`×12 | 15/20 | `quoted-printable`×5、`8bit`×1 |
| 沃邮箱 | **`gbk`×4**、`utf-8`×2 | `gbk`×1、`utf-8`×5 | 5/6 | `7bit`×1 |
| Gmail | UTF-8（预期） | UTF-8 | | |

**结论**

1. **GB2312 / GBK / GB18030 三种都要支持**——QQ 用 gb2312 *和* gb18030，沃用 gbk。
2. **只看顶层 `Content-Type` 会看错**：三家的邮件**顶层几乎都是 multipart**，
   正文的 charset 在**子部分**里。必须**递归**拆解。
   （顶层没有 charset ≠ 正文没有 charset——递归后三家都是 0 封缺失。）
3. **传输编码**：quoted-printable / 8bit / 7bit 都要处理。
4. 主题**全部**用 RFC 2047 编码字，charset 与正文可能不同。

**实测验证**：用 `xremail-message` 解析三家共 30 封真实邮件，
**解码出错 0 封**（没有出现 U+FFFD 替换字符）。

**解码顺序（`xremail-message` 必须严格遵守）**

1. 有 charset → 严格按 charset 解码；`gb2312`/`gbk` 一律升级为 **GB18030**（超集，覆盖生僻字）。
2. 无 charset → 尝试 **UTF-8 严格解码** → 失败则 **GB18030 严格解码** → 再失败用 latin1 兜底（不报错，但要标记）。
3. `Subject` 等头部字段必须先做 **RFC 2047** 解码（`=?gb2312?B?...?=`）。
4. 解码结果统一存 UTF-8。

---

## 并发连接限制

| 服务商 | 单账号同时在线数 | 来源 |
|---|---|---|
| 163 | 约 3~5 | 社区 |
| QQ | 约 3~5 | 社区 |
| 沃邮箱 | 未知 | ⚠️ 待实测 |
| Gmail | 未公开限制但有并发配额 | 官方（quota 文档） |

**产品对策**

- 每账户**只开 1 条** IMAP 连接（账户间并行，上限 5）。
- 提供「实时模式(IDLE)」/「省电模式(轮询)」切换，**并给出提示**："同一账号在多台设备上同时开启实时模式可能导致互相断开"。
- 检测到被踢时，在状态栏明确提示，而不是静默重试。

---

## 排错速查

| 现象 / 错误 | 常见原因 | 处置 |
|---|---|---|
| `AUTHENTICATIONFAILED` | 用了账号密码而非授权码 | 引导用户去网页版生成授权码 |
| 163/QQ 登录后被立刻踢下线 | 同账号多端同时登录 / 触发风控 | 退避重试 + 提示关闭其他客户端 |
| `550 Spam is embedded` / `550 Suspected spam` | QQ 反垃圾策略命中 | 降低频率；提示用户检查发信域名信誉 |
| `[AUTH] 535 Error: authentication failed` | 授权码失效（QQ 改密码后） | 明确提示"授权码已失效，请重新生成" |
| 连接超时 | 网络/防火墙；或服务器地址错 | 提供连接诊断，逐项测试 DNS/TCP/TLS/认证 |
| 邮件正文乱码 | charset 处理错误 | 按上面的解码顺序；提交样本到 issue |
| `SELECT` 报 `UIDVALIDITY changed` | 服务器重建了文件夹 | 清空该文件夹本地缓存并全量重拉（**必须实现**） |
| SMTP `535 Authentication failed` | 授权码失效 / 误用了登录密码 | 先看报告里服务器宣告了哪些 AUTH 机制，再区分「机制不支持」与「凭证错」 |
| SMTP 连接正常但没有 `AUTH` 宣告 | 服务器只在 TLS 之后才宣告认证能力 | 升级 TLS 后重新看 EHLO 扩展；据此决定认证放在升级前还是升级后 |
| Gmail `NO`/`BAD` + "unauthorized" | OAuth token 过期 | 走 refresh 或重新走 Device Code 授权 |

---

## 实测记录模板

复制下面这段，填好后贴到对应服务商小节下：

```markdown
#### 实测记录：<服务商> / <项目> / <日期>

- 客户端版本：<git sha 或版本号>
- 账号类型：<个人 / 企业 / Workspace>
- 节点/区域：<华东 / 华北 / ...>

| 项目 | 结果 | 观察到的原始响应/提示 |
|---|---|---|
| IMAP 连接 | ✅/❌ | |
| CAPABILITY | | |
| IDLE | | |
| CONDSTORE | | |
| AUTH 机制 | | |
| LIST 返回 | | |
| SELECT INBOX | | |
| UID FETCH 1:* 耗时 | | |
| 典型 charset | | |
| 附件大小上限 | | |
| 连续连接数 | | |
| 其他坑 | | |
```

---

## 相关

- 规划总纲：[PLAN.md](PLAN.md)
- 架构设计：[ARCHITECTURE.md](ARCHITECTURE.md)
- Gmail 决策：[adr/0003-gmail-oauth-verified.md](adr/0003-gmail-oauth-verified.md)
