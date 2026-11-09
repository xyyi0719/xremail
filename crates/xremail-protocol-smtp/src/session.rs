//! SMTP 会话：EHLO / STARTTLS / AUTH / 投递动词 / QUIT。
//!
//! ## 探测路径严格只读
//!
//! 探测（[`crate::probe`]）只用这四个命令：
//!
//! * `EHLO` —— 读能力集
//! * `STARTTLS` —— 升级到 TLS
//! * `AUTH` —— 认证
//! * `QUIT` —— 礼貌收尾
//!
//! 探测**永远不会**发 `MAIL FROM` / `RCPT TO` / `DATA`——
//! `MAIL FROM` 会**开启一次投递事务**，`DATA` 之后的内容就是真邮件。
//! 探测工具一旦顺手带上投递命令，「只是跑一下探测」和「误发一封信」
//! 之间就没有技术屏障了。邮箱协议是**共享资源**：一次误投可能真的进别人的收件箱。
//!
//! 投递动词（`MAIL FROM` / `RCPT TO` / `DATA`）只为**提交链路**
//! （[`crate::submit`]）而存在，放在这里是因为它们和 `EHLO` / `AUTH`
//! 共用同一条会话与同一套 TLS 升级逻辑。
//!
//! ## 能力协商（EHLO）
//!
//! `EHLO` 的应答里是服务器宣告的 ESMTP 扩展清单。本模块把它解析成
//! [`SmtpCapabilities`]——**提交链路的每一个决策都以它为准**：
//! 带不带 `BODY=8BITMIME`（RFC 6152）、带不带 `SMTPUTF8`（RFC 6531）、
//! 用哪个 `AUTH` 机制，都**不是客户端猜的**。
//!
//! 铁律：**服务器没宣告的扩展，客户端不许用**。
//! 用了服务器拒收的是**整个事务**（不是只拒这一条命令）——
//! 已经发出去的 `AUTH`、开了头的事务全都白费。
//!
//! 为什么这里放这套模型（而不是留在探测模块）：能力是**会话**的属性，
//! 探测只是它的一个只读用途。会话层持有它，提交链路才能在发任何东西之前
//! 先问一句「服务器到底支持什么」。
//!
//! ## 为什么在这里实现 dot-stuffing
//!
//! RFC 5321 §4.5.2 要求：正文里以 `.` 开头的行，线上要写成 `..`——
//! 否则服务器会把单独一行的 `.` 当成「正文结束」，信就截断了。
//! 这是**线上格式**问题，属于会话层，不属于提交层。
//!
//! 顺带也不实现 `RSET`：它只影响当前会话的投递状态，
//! 而本模块的调用方要么是探测（不开启投递状态），
//! 要么是一次性提交（会话结束即弃）——实现它只会平白扩大「会改动服务端状态」的表面。

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use zeroize::Zeroizing;

use crate::error::SmtpError;
use crate::transport::{SmtpConfig, TlsInfo, TlsTrust, TlsVersions, Transport};

/// 认证机制。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMechanism {
    /// `AUTH PLAIN`：一次性 base64（`\0user\0password`）。
    Plain,
    /// `AUTH LOGIN`：base64 分两段交互（先用户名、后口令）。
    Login,
    /// `AUTH XOAUTH2`：**只识别，不实现**（需要 OAuth 令牌，不是账号口令）。
    XOAuth2,
}

impl AuthMechanism {
    /// 机制名（与服务器宣告的写法一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMechanism::Plain => "PLAIN",
            AuthMechanism::Login => "LOGIN",
            AuthMechanism::XOAuth2 => "XOAUTH2",
        }
    }

    /// 按名字解析（服务器宣告的机制名 → 枚举）。
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "PLAIN" => Some(AuthMechanism::Plain),
            "LOGIN" => Some(AuthMechanism::Login),
            "XOAUTH2" => Some(AuthMechanism::XOAuth2),
            _ => None,
        }
    }
}

/// 一条 EHLO 扩展。
#[derive(Debug, Clone)]
pub struct Extension {
    /// 关键字（大写），如 `8BITMIME`。
    pub keyword: String,
    /// 参数（如 `SIZE` 的 `52428800`、`AUTH` 的 `LOGIN PLAIN`）。
    pub params: Vec<String>,
    /// 服务器返回的原文（不含 `250-` 前缀）。
    pub raw: String,
}

/// 扩展支持度。
///
/// 每一项都对应发送链路里的一个具体决策：能不能发 8 位正文、
/// 能不能用 UTF-8 标题、能不能一次发很大的附件。
#[derive(Debug, Clone, Default)]
pub struct ExtensionSupport {
    /// `8BITMIME` —— 允许 `BODY=8BITMIME`。
    pub eight_bit_mime: bool,
    /// `SMTPUTF8` —— 允许 UTF-8 邮箱地址与非 ASCII 标题。
    pub smtputf8: bool,
    /// `SIZE` —— 单封邮件大小上限（字节）。
    pub size: Option<u64>,
    /// `STARTTLS` —— 支持明文连接后升级到 TLS。
    pub starttls: bool,
    /// `PIPELINING` —— 支持命令流水线。
    pub pipelining: bool,
    /// `DSN` —— 支持投递状态通知。
    pub dsn: bool,
    /// `CHUNKING` —— 支持 `BDAT` 分块传输。
    pub chunking: bool,
    /// `BINARYMIME` —— 支持二进制 MIME 传输。
    pub binary_mime: bool,
    /// `ENHANCEDSTATUSCODES` —— 增强状态码。
    pub enhanced_status_codes: bool,
    /// `REQUIRETLS` —— 服务器**要求**必须走 TLS。
    pub requiretls: bool,
    /// `AUTH` —— 宣告了认证机制。
    pub auth: bool,
}

/// 服务器在 `EHLO` 应答里宣告的能力。
///
/// 这是**提交链路唯一的权威**：客户端不猜服务器支持什么，
/// 只按服务器**自己宣告的**来用。
///
/// 之所以把「扩展逐条原文」和「推导出来的支持度」放在一起：
/// 原文用于诊断报告（要照着服务器的真实写法抄配置），
/// 支持度用于决策（少一次字符串匹配，也少一处写错关键字的机会）。
#[derive(Debug, Clone, Default)]
pub struct SmtpCapabilities {
    /// 宣告的扩展逐条（保留服务器原文）。
    pub extensions: Vec<Extension>,
    /// 由 [`extensions`] 推导出的支持度。
    pub support: ExtensionSupport,
    /// 宣告的 `AUTH` 机制（大写、去重、排序）。
    pub auth_mechanisms: Vec<String>,
}

impl SmtpCapabilities {
    /// 从 `EHLO` 应答解析能力。
    pub fn parse(response: &SmtpResponse) -> Self {
        Self::from_extensions(parse_extensions(response))
    }

    /// 从已解析的扩展清单构造。
    pub fn from_extensions(extensions: Vec<Extension>) -> Self {
        let support = extension_support(&extensions);
        let auth_mechanisms = auth_mechanisms(&extensions);
        Self {
            extensions,
            support,
            auth_mechanisms,
        }
    }

    /// 是否宣告了 `8BITMIME`（RFC 6152）：可以带 `BODY=8BITMIME`。
    pub fn supports_8bitmime(&self) -> bool {
        self.support.eight_bit_mime
    }

    /// 是否宣告了 `SMTPUTF8`（RFC 6531）：可以带 `SMTPUTF8` 参数。
    pub fn supports_smtputf8(&self) -> bool {
        self.support.smtputf8
    }

    /// 是否宣告了 `AUTH`，且包含指定机制。
    pub fn supports_auth_mechanism(&self, mechanism: AuthMechanism) -> bool {
        self.auth_mechanisms
            .iter()
            .any(|name| name.eq_ignore_ascii_case(mechanism.as_str()))
    }

    /// 是否宣告了某个关键字（不区分大小写）。
    pub fn has_keyword(&self, keyword: &str) -> bool {
        self.extensions
            .iter()
            .any(|ext| ext.keyword.eq_ignore_ascii_case(keyword))
    }
}

/// `MAIL FROM` 的 ESMTP 参数。
///
/// 参数**只能**在服务器宣告了对应扩展时带（RFC 6152 §3 / RFC 6531 §3.4）：
/// 服务器没宣告还带上，它拒收的是**整个事务**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MailFromParams {
    /// 带 `BODY=8BITMIME`：正文含 8 位内容，且服务器宣告了 `8BITMIME`。
    pub body_8bitmime: bool,
    /// 带 `SMTPUTF8`：信封地址含非 ASCII，且服务器宣告了 `SMTPUTF8`。
    pub smtputf8: bool,
}

/// 地址里含非 ASCII 字符吗（含了就需要 `SMTPUTF8`）。
///
/// RFC 6531 §3.4：信封里出现非 ASCII（UTF-8）地址时，客户端**必须**在
/// `MAIL FROM` 上带 `SMTPUTF8` 参数，否则服务器有权拒收整个事务。
///
/// 「非 ASCII」指**任何**字节 > 0x7F——中文邮箱名（`张三@qq.com`）
/// 与国际域名（`user@例子.中国`）都算。
pub fn address_requires_smtputf8(address: &str) -> bool {
    !address.is_ascii()
}

/// 把 EHLO 响应解析成扩展清单。
///
/// 响应第一行是服务器问候（`250-smtp.163.com Hello`），
/// 后面每行一条扩展（RFC 5321 §4.1.4）。
pub fn parse_extensions(response: &SmtpResponse) -> Vec<Extension> {
    response
        .texts()
        .into_iter()
        // 跳过第一行（问候）。
        .skip(1)
        .map(|raw| {
            let mut parts = raw.split_whitespace();
            let mut keyword = parts.next().unwrap_or_default().to_ascii_uppercase();
            let mut params: Vec<String> = Vec::new();

            // 老式写法 `AUTH=LOGIN PLAIN`：等号后面是参数。
            if let Some(idx) = keyword.find('=') {
                let rest = keyword[idx + 1..].to_string();
                keyword = keyword[..idx].to_string();
                if !rest.is_empty() {
                    params.push(rest);
                }
            }
            params.extend(parts.map(|s| s.to_string()));

            Extension {
                keyword,
                params,
                raw,
            }
        })
        .collect()
}

/// 从扩展清单里解析 AUTH 机制。
pub fn auth_mechanisms(extensions: &[Extension]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for ext in extensions {
        if ext.keyword != "AUTH" {
            continue;
        }
        out.extend(ext.params.iter().filter(|p| !p.is_empty()).cloned());
    }

    out.sort();
    out.dedup();
    out
}

/// 从扩展清单里提取支持度。
pub fn extension_support(extensions: &[Extension]) -> ExtensionSupport {
    let mut support = ExtensionSupport::default();

    for ext in extensions {
        match ext.keyword.as_str() {
            "8BITMIME" => support.eight_bit_mime = true,
            "SMTPUTF8" => support.smtputf8 = true,
            "STARTTLS" => support.starttls = true,
            "PIPELINING" => support.pipelining = true,
            "DSN" => support.dsn = true,
            "CHUNKING" => support.chunking = true,
            "BINARYMIME" => support.binary_mime = true,
            "ENHANCEDSTATUSCODES" => support.enhanced_status_codes = true,
            "REQUIRETLS" => support.requiretls = true,
            "AUTH" => support.auth = true,
            "SIZE" => support.size = ext.params.first().and_then(|p| p.parse().ok()),
            _ => {}
        }
    }

    support
}

/// 一条 SMTP 响应。
#[derive(Debug, Clone)]
pub struct SmtpResponse {
    /// 响应码（220 / 250 / 334 / 535 …）。
    pub code: u16,
    /// 原始行（逐条，含 `250-` / `250 ` 前缀）。
    pub lines: Vec<String>,
    /// 末行正文（去掉响应码与分隔符）。
    pub text: String,
}

impl SmtpResponse {
    /// 是否是成功码（2xx）。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.code)
    }

    /// 每一行的正文（去掉 `250-` / `250 ` 前缀）。
    pub fn texts(&self) -> Vec<String> {
        self.lines.iter().map(|line| response_text(line)).collect()
    }

    /// 末行原文（**含**响应码，如 `535 5.7.8 Error: authentication failed`）。
    pub fn raw_text(&self) -> String {
        self.lines
            .last()
            .map(|line| line.trim().to_string())
            .unwrap_or_default()
    }
}

/// SMTP 会话。
pub struct SmtpSession {
    transport: Transport,
    timeout: Duration,
    host: String,
    tls_versions: TlsVersions,
    /// 服务器在**最近一次** `EHLO` 里宣告的能力。
    ///
    /// 每次 `EHLO` 都会覆盖它——所以 STARTTLS 之后重新 `EHLO`（RFC 3207 §4.2）
    /// 拿到的就是 TLS 阶段那份权威能力集，不会残留明文阶段的那份。
    capabilities: SmtpCapabilities,
}

impl SmtpSession {
    /// 隐式 TLS 连接（465）。
    pub async fn connect_tls(config: &SmtpConfig, trust: &TlsTrust) -> Result<Self, SmtpError> {
        let transport = crate::transport::connect_tls(config, trust).await?;
        Ok(Self {
            transport,
            timeout: config.command_timeout,
            host: config.host.clone(),
            tls_versions: config.tls_versions,
            capabilities: SmtpCapabilities::default(),
        })
    }

    /// 明文连接。
    ///
    /// **只用于 STARTTLS 升级路径**：连上后必须调 [`SmtpSession::starttls`]，
    /// 否则就等于在明文上继续——那样等于把口令送出去。
    pub async fn connect_plaintext(config: &SmtpConfig) -> Result<Self, SmtpError> {
        let transport = crate::transport::connect_plain(config).await?;
        Ok(Self {
            transport,
            timeout: config.command_timeout,
            host: config.host.clone(),
            tls_versions: config.tls_versions,
            capabilities: SmtpCapabilities::default(),
        })
    }

    /// 读服务器问候（`220`）。
    pub async fn greeting(&mut self) -> Result<SmtpResponse, SmtpError> {
        self.read_response().await
    }

    /// `EHLO <domain>`：读能力集。
    ///
    /// 应答里的扩展会被解析进会话（[`SmtpSession::capabilities`]），
    /// 供后续决策使用——**提交链路必须按这里宣告的能力来**，
    /// 不能按客户端的假设来。
    pub async fn ehlo(&mut self, domain: &str) -> Result<SmtpResponse, SmtpError> {
        let response = self.command(&format!("EHLO {domain}")).await?;
        if response.is_success() {
            self.capabilities = SmtpCapabilities::parse(&response);
        }
        Ok(response)
    }

    /// 服务器在**最近一次** `EHLO` 里宣告的能力。
    ///
    /// 还没发过 `EHLO`（或 `EHLO` 被拒）时是空能力集——
    /// 即「什么都不支持」，这样上层宁可报错也不会误用某个扩展。
    pub fn capabilities(&self) -> &SmtpCapabilities {
        &self.capabilities
    }

    /// `STARTTLS`：升级到 TLS。
    ///
    /// 收到 `220` 后握手。**调用方必须重新发一次 `EHLO`**
    /// （RFC 3207 §4.2：握手后服务器可能改变能力集，比如才宣告 `AUTH`）。
    pub async fn starttls(&mut self, trust: &TlsTrust) -> Result<SmtpResponse, SmtpError> {
        let response = self.command("STARTTLS").await?;
        if response.code != 220 {
            // 不降级：没有 220 就不握手，更不会在明文上继续认证。
            return Err(SmtpError::Unsupported(format!(
                "服务器不接受 STARTTLS: {}",
                response.text
            )));
        }
        self.transport
            .upgrade_to_tls(&self.host, self.timeout, trust, self.tls_versions)
            .await?;
        Ok(response)
    }

    /// `AUTH PLAIN`。
    ///
    /// 口令只出现在 base64 载荷里，**不进日志、不进错误信息**。
    pub async fn auth_plain(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<SmtpResponse, SmtpError> {
        // 两份都装着凭据：`payload` 是明文（`\0user\0pass`），`encoded` 是它的 base64——
        // **base64 不是加密**，还原一下就是口令。两者都用 `Zeroizing` 包住，
        // 离开作用域时清零，别让它们残留在堆里等被换出到磁盘。
        let payload = Zeroizing::new(format!("\0{user}\0{password}"));
        let encoded = Zeroizing::new(STANDARD.encode(payload.as_bytes()));

        let response = self
            .command(&format!("AUTH PLAIN {}", encoded.as_str()))
            .await?;
        if response.code == 334 {
            // 服务器不接受初始响应：把同一份凭据（不带机制名）再发一次。
            return self.command(&encoded).await;
        }
        Ok(response)
    }

    /// `AUTH LOGIN`（base64 两段交互）。
    ///
    /// 口令只出现在 base64 载荷里，**不进日志、不进错误信息**。
    pub async fn auth_login(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<SmtpResponse, SmtpError> {
        let response = self.command("AUTH LOGIN").await?;
        if response.code != 334 {
            // 服务器直接给了结果（比如 `535` 或 `504`）。
            return Ok(response);
        }

        let user_b64 = STANDARD.encode(user.as_bytes());
        let response = self.command(&user_b64).await?;
        if response.code != 334 {
            return Ok(response);
        }

        // 口令的 base64 同样是凭据（base64 不是加密），用完清零。
        let pass_b64 = Zeroizing::new(STANDARD.encode(password.as_bytes()));
        self.command(&pass_b64).await
    }

    /// `MAIL FROM:<sender>`：开启一次投递事务。
    ///
    /// `body_8bitmime` 为真时带上 `BODY=8BITMIME`（RFC 6152）。
    ///
    /// **只有服务器宣告了 `8BITMIME` 才能带这个参数**——服务器没宣告时
    /// 带上它会被拒收（`501` / `555`）。带不带由调用方决定，
    /// 但「服务器不支持就不带」这条规则由提交层把关。
    ///
    /// 需要同时带 `SMTPUTF8`（RFC 6531）时用
    /// [`SmtpSession::mail_from_with_params`]。
    pub async fn mail_from(
        &mut self,
        sender: &str,
        body_8bitmime: bool,
    ) -> Result<SmtpResponse, SmtpError> {
        self.mail_from_with_params(
            sender,
            MailFromParams {
                body_8bitmime,
                smtputf8: false,
            },
        )
        .await
    }

    /// `MAIL FROM:<sender> [参数]`：按能力协商结果带 ESMTP 参数。
    ///
    /// 参数顺序固定为 `BODY=8BITMIME` → `SMTPUTF8`（RFC 6531 §3.4 的
    /// `esmtp-parameters` 语法允许任意顺序，这里按可读性固定）。
    pub async fn mail_from_with_params(
        &mut self,
        sender: &str,
        params: MailFromParams,
    ) -> Result<SmtpResponse, SmtpError> {
        let mut line = format!("MAIL FROM:<{sender}>");
        if params.body_8bitmime {
            line.push_str(" BODY=8BITMIME");
        }
        if params.smtputf8 {
            line.push_str(" SMTPUTF8");
        }
        self.command(&line).await
    }

    /// `RCPT TO:<recipient>`：添加一个信封收件人。
    ///
    /// 一次事务可以有多个 `RCPT TO`——每个信封收件人一条。
    pub async fn rcpt_to(&mut self, recipient: &str) -> Result<SmtpResponse, SmtpError> {
        self.command(&format!("RCPT TO:<{recipient}>")).await
    }

    /// `DATA`：进入数据态、写入正文、读**最终**应答。
    ///
    /// 返回的永远是**最终应答**：
    ///
    /// * 服务器没接受 `DATA`（如 `503`）→ 直接返回那个应答，**没有邮件发出去**。
    /// * 服务器回了 `354` → 写入正文，再读最终应答（`250` / `4xx` / `5xx`）。
    ///
    /// 正文写入后**拿不到最终应答**时返回 `Err`——
    /// 调用方（提交层）据此判定「不确定」：邮件可能已经被收下了。
    pub async fn data(&mut self, message: &[u8]) -> Result<SmtpResponse, SmtpError> {
        let response = self.command("DATA").await?;
        if response.code != 354 {
            // 服务器没进入数据态（缺 `MAIL FROM` / `RCPT TO` 等）。
            return Ok(response);
        }

        let payload = encode_data_payload(message);
        self.transport.write_bytes(self.timeout, &payload).await?;
        self.read_response().await
    }

    /// `QUIT`：礼貌收尾。
    pub async fn quit(&mut self) -> Result<SmtpResponse, SmtpError> {
        self.command("QUIT").await
    }

    /// TLS 会话信息（还没升级到 TLS 时是 `None`）。
    pub fn tls_info(&self) -> Option<TlsInfo> {
        self.transport.tls_info()
    }

    /// 发一条命令并读回响应。
    ///
    /// **命令内容一律不进日志**：`AUTH LOGIN` 的续行里是 base64 后的
    /// 用户名与口令，记下来等于把凭据写进日志文件。
    async fn command(&mut self, line: &str) -> Result<SmtpResponse, SmtpError> {
        self.transport.write_line(self.timeout, line).await?;
        self.read_response().await
    }

    /// 读一条响应（可能是多行）。
    async fn read_response(&mut self) -> Result<SmtpResponse, SmtpError> {
        let mut lines: Vec<String> = Vec::new();
        let mut code: u16;

        loop {
            let raw = self.transport.read_line(self.timeout).await?;
            let line = raw.trim_end().to_string();
            let bytes = line.as_bytes();

            if bytes.len() < 3
                || !bytes[0].is_ascii_digit()
                || !bytes[1].is_ascii_digit()
                || !bytes[2].is_ascii_digit()
            {
                return Err(SmtpError::Smtp(format!("响应行不合法: {line}")));
            }

            let parsed: u16 = line[..3]
                .parse()
                .map_err(|_| SmtpError::Smtp(format!("响应码不合法: {line}")))?;
            code = parsed;

            // 第 4 个字符是 `-` 表示后面还有同响应码的行（RFC 5321 §4.2.1）。
            let more = bytes.get(3) == Some(&b'-');
            lines.push(line);

            if !more {
                break;
            }
            if lines.len() > 64 {
                return Err(SmtpError::Smtp("响应行过多".into()));
            }
        }

        let text = lines.last().map(|l| response_text(l)).unwrap_or_default();
        Ok(SmtpResponse { code, lines, text })
    }
}

/// 把邮件正文编码成 `DATA` 的线上字节。
///
/// 两件事：
///
/// * **行终止符统一成 CRLF**（RFC 5321 §2.3.1）——线上就是 CRLF。
///   输入里若是 LF（很多 MIME 生成器这样），这里统一转成 CRLF。
/// * **dot-stuffing**（RFC 5321 §4.5.2）——正文里以 `.` 开头的行，
///   线上写成 `..`。不转的话，单独一行的 `.` 会被服务器当成「正文结束」，
///   邮件就截断了（而且客户端还以为发送成功）。
///
/// 末尾补上**单独一行的 `.`** 作为正文结束符。
fn encode_data_payload(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 64);

    let push_line = |out: &mut Vec<u8>, line: &[u8]| {
        // 以 `.` 开头的行要加点（dot-stuffing）。
        if line.first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    };

    let mut line_start = 0;
    for index in 0..message.len() {
        if message[index] != b'\n' {
            continue;
        }
        let mut end = index;
        // 行尾的 CR 不算内容（否则会变成 CRCRLF）。
        if end > line_start && message[end - 1] == b'\r' {
            end -= 1;
        }
        push_line(&mut out, &message[line_start..end]);
        line_start = index + 1;
    }

    // 最后一行：末尾可能没有换行符。
    if line_start < message.len() {
        let mut end = message.len();
        if end > line_start && message[end - 1] == b'\r' {
            end -= 1;
        }
        push_line(&mut out, &message[line_start..end]);
    }

    // 正文结束符：单独一行的 `.`（RFC 5321 §4.1.1.4）。
    out.extend_from_slice(b".\r\n");
    out
}

/// 去掉 `250-` / `250 ` 前缀，取正文。
fn response_text(line: &str) -> String {
    let bytes = line.as_bytes();
    let start = if bytes.len() >= 4 && (bytes[3] == b' ' || bytes[3] == b'-') {
        4
    } else {
        3
    };
    line.get(start..).unwrap_or("").trim().to_string()
}
