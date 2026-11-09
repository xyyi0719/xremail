//! SMTP 传输层：TCP 连接 + rustls TLS。
//!
//! ## 安全约定（见 [SECURITY §2.2](../../../docs/SECURITY.md)）
//!
//! * **强制 TLS**：要么隐式 TLS（连上就是 TLS，465），要么明文连接后用
//!   STARTTLS 升级（587）。STARTTLS 失败**不降级**——不会退回到明文上认证。
//! * 使用**系统根证书**校验。系统根不可用时**报错**，不静默改为不校验。
//! * 不提供「跳过证书校验」的开关。
//! * 连接与命令都有**超时**。
//! * 错误信息里**不含**用户名或口令。
//!
//! ## 为什么要自己实现按行读写
//!
//! STARTTLS 要求把**同一条 TCP 连接**从明文升级成 TLS（RFC 3207 §4）。
//! 如果用 `BufReader` 缓冲，就可能出现「已经读进用户态缓冲区、还没交给
//! TLS 层的明文字节」——这些字节在升级后会被当成密文解析，直接把会话搅坏。
//! 所以这里自己维护一个极小的行缓冲：升级前要求缓冲区里**没有**未消费的字节。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::error::SmtpError;

/// TLS 模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// 隐式 TLS：连上就是 TLS（465）。
    Direct,
    /// 明文 + STARTTLS（587）。
    Starttls,
}

impl TlsMode {
    /// 模式名（报告里用）。
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Direct => "direct（隐式 TLS）",
            TlsMode::Starttls => "starttls（明文 + STARTTLS）",
        }
    }
}

/// 连接配置。
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    /// 服务器主机名（同时用于 SNI 与证书主机名校验）。
    pub host: String,
    /// 服务器端口。
    pub port: u16,
    /// TLS 模式。
    pub tls_mode: TlsMode,
    /// 建连超时。
    pub connect_timeout: Duration,
    /// 单条命令的超时。
    pub command_timeout: Duration,
    /// TLS 协议版本偏好。
    pub tls_versions: TlsVersions,
}

impl SmtpConfig {
    /// 隐式 TLS 的配置（465）。
    pub fn direct(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls_mode: TlsMode::Direct,
            connect_timeout: Duration::from_secs(15),
            command_timeout: Duration::from_secs(30),
            tls_versions: TlsVersions::default(),
        }
    }

    /// 明文 + STARTTLS 的配置（587）。
    pub fn starttls(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls_mode: TlsMode::Starttls,
            connect_timeout: Duration::from_secs(15),
            command_timeout: Duration::from_secs(30),
            tls_versions: TlsVersions::default(),
        }
    }

    /// 只用 TLS 1.2。
    ///
    /// 给那些**不认 TLS 1.3** 的服务器用（如 `smtp.wo.cn`，
    /// 它对 TLS 1.3 的 ClientHello 直接回 fatal alert）。
    pub fn with_tls12_only(mut self) -> Self {
        self.tls_versions = TlsVersions::Tls12Only;
        self
    }
}

/// TLS 协议版本偏好。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsVersions {
    /// 默认：TLS 1.3 + 1.2（rustls 的默认集合）。
    #[default]
    Default,
    /// 只用 TLS 1.2。
    ///
    /// ## 为什么要留这个开关
    ///
    /// 有些国内邮箱的 SMTP 服务器（如 `smtp.wo.cn`）**只认 TLS 1.2**：
    /// 见到 TLS 1.3 的 ClientHello，它直接回一个 fatal alert
    /// （`HandshakeFailure`），握手就断了。
    ///
    /// 这是**服务器能力问题**，不是**证书问题**：
    /// 换到 1.2 之后，证书照样按系统根校验、主机名照样核对，
    /// **一点都没有放松**（见 [SECURITY §2.2](../../../docs/SECURITY.md)）。
    ///
    /// 降级这件事必须**让用户看见**——探测报告会写明
    /// 「服务器拒绝 TLS 1.3，已回退到 TLS 1.2」。
    Tls12Only,
}

/// TLS 信任来源。
///
/// 生产只用 [`TlsTrust::SystemRoots`]：服务端证书由公开 CA 签发，
/// 校验失败就是失败，**不提供**「跳过校验」的开关
/// （见 [SECURITY §2.2](../../../docs/SECURITY.md)）。
///
/// [`TlsTrust::Roots`] 只给**测试**用：假 SMTP 服务器用自签证书，
/// 测试需要显式把那张 CA 交给客户端。它不会出现在生产路径上。
#[derive(Debug, Clone)]
pub enum TlsTrust {
    /// 系统根证书。
    SystemRoots,
    /// 指定的根证书集合（仅测试）。
    Roots(rustls::RootCertStore),
    /// 系统根 + **额外**信任的 CA 证书。
    ///
    /// **不是**「跳过校验」：服务端证书仍然被强制校验，只是信任锚多了一个。
    /// 用途是自建/测试服务器（自签 CA 不在系统根里）。
    SystemRootsWithExtra(Vec<rustls::pki_types::CertificateDer<'static>>),
}

impl TlsTrust {
    /// 系统根 + 从 PEM 文件读进来的额外 CA。
    ///
    /// 用于自建/测试服务器。**不**关闭校验——证书照样必须通过验证。
    pub fn system_roots_with_pem(path: &std::path::Path) -> Result<Self, SmtpError> {
        let raw = std::fs::read(path)
            .map_err(|e| SmtpError::Tls(format!("读取 CA 文件 {} 失败: {e}", path.display())))?;
        let extra = parse_pem_certificates(&raw)?;
        if extra.is_empty() {
            return Err(SmtpError::Tls(format!(
                "CA 文件 {} 里没有找到证书",
                path.display()
            )));
        }
        Ok(TlsTrust::SystemRootsWithExtra(extra))
    }
}

/// 从 PEM 文本里抽出 DER 证书。
fn parse_pem_certificates(
    raw: &[u8],
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, SmtpError> {
    use x509_parser::pem::Pem;

    let mut certs = Vec::new();
    for pem in Pem::iter_from_buffer(raw) {
        let pem = pem.map_err(|e| SmtpError::Tls(format!("PEM 解析失败: {e}")))?;
        certs.push(rustls::pki_types::CertificateDer::from(pem.contents));
    }
    Ok(certs)
}

/// TLS 会话信息。
#[derive(Debug, Clone)]
pub struct TlsInfo {
    /// 协议版本，如 `TLSv1.3`。
    pub protocol: String,
    /// 协商出的加密套件。
    pub cipher: String,
    /// 证书链与主机名是否校验通过。
    ///
    /// rustls 默认强制校验；能走到这里说明已通过。
    pub verified: bool,
    /// 服务器发来的证书链长度（含叶子证书）。
    pub chain_len: usize,
    /// 叶子证书体检结果（解析失败时为 `None`）。
    pub certificate: Option<CertificateInfo>,
}

/// 证书体检结果。
///
/// 探测要回答的是「这个服务器给的是什么证」——CN、签发者、有效期，
/// 缺一不可（IMAP 层当时没做，是因为那边不需要）。
#[derive(Debug, Clone)]
pub struct CertificateInfo {
    /// 主题 CN。
    pub subject_cn: Option<String>,
    /// 签发者 CN。
    pub issuer_cn: Option<String>,
    /// 主题（完整 DN）。
    pub subject: String,
    /// 签发者（完整 DN）。
    pub issuer: String,
    /// 生效日（UTC，`YYYY-MM-DD`）。
    pub not_before: String,
    /// 过期日（UTC，`YYYY-MM-DD`）。
    pub not_after: String,
    /// 是否已经过期。
    pub expired: bool,
    /// 是否**尚未生效**（当前时间早于 `not_before`）。
    ///
    /// 单独一个字段而不是把它并进 `expired`：两者含义相反，混在一起会让人
    /// 把「证书还没开始生效」读成「证书过期了」。一个尚未生效的证书
    /// 同样是**不可用**的，所以 CLI 会把它和过期一样标红——但文案要分开。
    ///
    /// 之前**没有**这一项：`not_before` 只被格式化成字符串打印，从不比较，
    /// 于是一个「明天才生效」的证书会显示成「未过期」，看着像没问题。
    pub not_yet_valid: bool,
    /// 距离过期还有多少天（负数表示已经过期）。
    pub days_remaining: i64,
    /// 是否自签（主题 CN 与签发者 CN 相同）。
    pub self_signed: bool,
}

/// 传输层连接。
pub(crate) struct Transport {
    conn: Option<Conn>,
    buf: Vec<u8>,
    pos: usize,
}

enum Conn {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Transport {
    /// 包裹一条明文 TCP 连接。
    pub(crate) fn plain(tcp: TcpStream) -> Self {
        Self {
            conn: Some(Conn::Plain(tcp)),
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// 包裹一条已经握好手的 TLS 连接。
    pub(crate) fn tls(tls: TlsStream<TcpStream>) -> Self {
        Self {
            conn: Some(Conn::Tls(Box::new(tls))),
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// 读一行（去掉行尾的 CRLF）。
    pub(crate) async fn read_line(&mut self, timeout: Duration) -> Result<String, SmtpError> {
        loop {
            if let Some(offset) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                let end = self.pos + offset;
                let line = String::from_utf8_lossy(&self.buf[self.pos..end]).to_string();
                self.pos = end + 1;
                // 丢掉已消费的部分，缓冲不能无限长。
                if self.pos >= 4096 {
                    self.buf.drain(..self.pos);
                    self.pos = 0;
                }
                return Ok(line);
            }

            let n = self.read_chunk(timeout).await?;
            if n == 0 {
                return Err(SmtpError::Io("连接被对端关闭".into()));
            }
        }
    }

    /// 写一行（自动补 CRLF）。
    pub(crate) async fn write_line(
        &mut self,
        timeout: Duration,
        line: &str,
    ) -> Result<(), SmtpError> {
        let mut payload = String::with_capacity(line.len() + 2);
        payload.push_str(line);
        payload.push_str("\r\n");
        let bytes = payload.as_bytes();

        let written = {
            let Some(conn) = self.conn.as_mut() else {
                return Err(SmtpError::Io("连接已关闭".into()));
            };
            match conn {
                Conn::Plain(tcp) => tokio::time::timeout(timeout, tcp.write_all(bytes)).await,
                Conn::Tls(tls) => tokio::time::timeout(timeout, tls.write_all(bytes)).await,
            }
        };

        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SmtpError::Io(format!("写入失败: {e}"))),
            Err(_) => Err(SmtpError::Timeout("写入超时".into())),
        }
    }

    /// 写一段**原始字节**（不带行终止符）。
    ///
    /// `DATA` 的正文不是「一行」——它是一大段自己拼好的字节
    /// （dot-stuffing + CRLF + 结束符），所以需要一个不经行处理的写法。
    /// 超时语义与 [`Transport::write_line`] 一致。
    pub(crate) async fn write_bytes(
        &mut self,
        timeout: Duration,
        bytes: &[u8],
    ) -> Result<(), SmtpError> {
        let written = {
            let Some(conn) = self.conn.as_mut() else {
                return Err(SmtpError::Io("连接已关闭".into()));
            };
            match conn {
                Conn::Plain(tcp) => tokio::time::timeout(timeout, tcp.write_all(bytes)).await,
                Conn::Tls(tls) => tokio::time::timeout(timeout, tls.write_all(bytes)).await,
            }
        };

        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SmtpError::Io(format!("写入失败: {e}"))),
            Err(_) => Err(SmtpError::Timeout("写入超时".into())),
        }
    }

    /// 把**当前这条连接**从明文升级成 TLS（STARTTLS 之后调用）。
    pub(crate) async fn upgrade_to_tls(
        &mut self,
        host: &str,
        timeout: Duration,
        trust: &TlsTrust,
        versions: TlsVersions,
    ) -> Result<(), SmtpError> {
        // 缓冲区里必须没有「读过但没消费」的明文字节：
        // 它们在升级后会被当成密文，整个会话就废了。
        if self.pos < self.buf.len() {
            return Err(SmtpError::Tls(
                "STARTTLS 前还有未读完的明文字节，无法安全升级".into(),
            ));
        }

        let conn = self
            .conn
            .take()
            .ok_or_else(|| SmtpError::Io("连接已关闭".into()))?;
        let Conn::Plain(tcp) = conn else {
            return Err(SmtpError::Tls("连接已经是 TLS，不能再升级".into()));
        };

        let tls = handshake(tcp, host, timeout, trust, versions).await?;
        self.conn = Some(Conn::Tls(Box::new(tls)));
        self.buf.clear();
        self.pos = 0;
        Ok(())
    }

    /// TLS 会话信息（还没升级到 TLS 时是 `None`）。
    pub(crate) fn tls_info(&self) -> Option<TlsInfo> {
        let Some(Conn::Tls(stream)) = self.conn.as_ref() else {
            return None;
        };
        let (_io, session) = stream.get_ref();
        let chain = session.peer_certificates().unwrap_or(&[]);

        Some(TlsInfo {
            protocol: session
                .protocol_version()
                .map(protocol_name)
                .unwrap_or_else(|| "未知".into()),
            cipher: session
                .negotiated_cipher_suite()
                .map(|c| format!("{c:?}"))
                .unwrap_or_else(|| "未知".into()),
            // rustls 默认强制校验证书链与主机名；握手成功即已通过。
            verified: true,
            chain_len: chain.len(),
            certificate: chain
                .first()
                .and_then(|der| parse_certificate(der.as_ref())),
        })
    }

    /// 读一批字节进缓冲。
    async fn read_chunk(&mut self, timeout: Duration) -> Result<usize, SmtpError> {
        let mut chunk = [0u8; 1024];

        let read = {
            let Some(conn) = self.conn.as_mut() else {
                return Err(SmtpError::Io("连接已关闭".into()));
            };
            match conn {
                Conn::Plain(tcp) => tokio::time::timeout(timeout, tcp.read(&mut chunk)).await,
                Conn::Tls(tls) => tokio::time::timeout(timeout, tls.read(&mut chunk)).await,
            }
        };

        match read {
            Ok(Ok(n)) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(n)
            }
            Ok(Err(e)) => Err(SmtpError::Io(format!("读取失败: {e}"))),
            Err(_) => Err(SmtpError::Timeout("读取超时".into())),
        }
    }
}

/// 建立明文 TCP 连接。
async fn connect_tcp(config: &SmtpConfig) -> Result<TcpStream, SmtpError> {
    let tcp = tokio::time::timeout(
        config.connect_timeout,
        TcpStream::connect((config.host.as_str(), config.port)),
    )
    .await
    .map_err(|_| SmtpError::Timeout(format!("连接 {}:{} 超时", config.host, config.port)))?
    .map_err(|e| SmtpError::Connect(format!("连接 {}:{} 失败: {e}", config.host, config.port)))?;

    // Nagle 会给「发一条命令、等一条响应」的交互带来延迟。
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

/// 隐式 TLS：连上就握手。
pub(crate) async fn connect_tls(
    config: &SmtpConfig,
    trust: &TlsTrust,
) -> Result<Transport, SmtpError> {
    let tcp = connect_tcp(config).await?;
    let tls = handshake(
        tcp,
        &config.host,
        config.connect_timeout,
        trust,
        config.tls_versions,
    )
    .await?;
    Ok(Transport::tls(tls))
}

/// 明文连接（**仅供 STARTTLS 升级路径使用**）。
pub(crate) async fn connect_plain(config: &SmtpConfig) -> Result<Transport, SmtpError> {
    let tcp = connect_tcp(config).await?;
    Ok(Transport::plain(tcp))
}

/// TLS 握手。
async fn handshake(
    tcp: TcpStream,
    host: &str,
    timeout: Duration,
    trust: &TlsTrust,
    versions: TlsVersions,
) -> Result<TlsStream<TcpStream>, SmtpError> {
    let tls_connector = build_tls_connector(trust, versions)?;

    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| SmtpError::Tls(format!("主机名不合法: {e}")))?;

    let stream = tokio::time::timeout(timeout, tls_connector.connect(server_name, tcp))
        .await
        .map_err(|_| SmtpError::Timeout("TLS 握手超时".into()))?
        .map_err(|e| classify_tls_error(host, e))?;

    Ok(stream)
}

/// 把 TLS 握手错误分成两类——因为**处理方式完全不同**。
///
/// * 服务器**拒绝**我们的 ClientHello（`HandshakeFailure` / `ProtocolVersion`）
///   → 可以**换协议版本重试**：它不认 TLS 1.3。
///   而证书校验一点不放松（只是不认这个协议版本）。
///   极少数服务器拒绝的原因是**不认我们的密码套件**而非版本
///   （见 [`SmtpError::TlsVersionRejected`]），换版本救不了——
///   这时错误里会带上针对该服务器的已知原因说明。
/// * 其它（证书过期、主机名不符、链不全）→ 是**信任问题**：
///   换版本也救不了，只能报错。
///   **绝不放宽校验**（见 [SECURITY §2.2](../../../docs/SECURITY.md)）。
///
/// rustls 的错误被 tokio-rustls 包进了 `io::Error` 里
/// （`io::ErrorKind::InvalidData` + 原始错误作为 inner），
/// 所以这里先把它**取出来**再分类。
fn classify_tls_error(host: &str, error: std::io::Error) -> SmtpError {
    if let Some(rustls_error) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        match rustls_error {
            rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure)
            | rustls::Error::AlertReceived(rustls::AlertDescription::ProtocolVersion) => {
                return SmtpError::TlsVersionRejected(format!(
                    "{rustls_error}{}",
                    known_tls_hint(host)
                ));
            }
            _ => {}
        }
    }
    SmtpError::Tls(format!("TLS 握手失败: {error}"))
}

/// 已知「服务器只收静态 RSA 套件」的服务器名单。
///
/// 2026-10-28 实测：`smtp.wo.cn`（中国联通沃邮箱）要求 ClientHello 里
/// 带至少一个**静态 RSA** 密钥交换套件（`TLS_RSA_WITH_*`），
/// 否则回 `HandshakeFailure` alert 直接断掉握手。
///
/// 交叉验证：`openssl s_client -connect smtp.wo.cn:465 -tls1_2` 能连上，
/// 协商出的是 `TLS_RSA_WITH_AES_128_GCM_SHA256`——**静态 RSA**。
///
/// rustls **只支持 ECDHE/PSK**，没有静态 RSA 密钥交换，
/// 所以**换 TLS 版本没用**（实测强制 TLS 1.2 一样断）。
/// Windows SChannel 与 OpenSSL 都带静态 RSA 套件，它们能连上。
const KNOWN_STATIC_RSA_ONLY_HOSTS: &[&str] = &["smtp.wo.cn"];

/// 已知 TLS 兼容性问题服务器的说明文案。
///
/// 命中 [``KNOWN_STATIC_RSA_ONLY_HOSTS``] 时返回一段提示，
/// 让用户知道「这不是证书问题，也不是版本问题，换个 TLS 栈才有用」。
fn known_tls_hint(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if KNOWN_STATIC_RSA_ONLY_HOSTS.contains(&host.as_str()) {
        return format!(
            "（已知：{host} 要求 ClientHello 里带静态 RSA 密钥交换套件\
             （TLS_RSA_WITH_*），而 rustls 只支持 ECDHE；\
             换 TLS 版本无效，需要系统原生 TLS 才能连上）"
        );
    }
    String::new()
}

/// 构造 TLS 连接器。
fn build_tls_connector(trust: &TlsTrust, versions: TlsVersions) -> Result<TlsConnector, SmtpError> {
    let mut roots = rustls::RootCertStore::empty();

    match trust {
        TlsTrust::Roots(store) => {
            roots = store.clone();
        }
        TlsTrust::SystemRoots | TlsTrust::SystemRootsWithExtra(_) => {
            // `load_native_certs` 返回的是一个**结构体**（certs + errors），
            // 不是 Result：部分证书加载失败时仍会返回已加载的那些。
            // 这里只要有任何一个错误就**整体失败**——宁可报错，也不静默少信任一些根。
            let loaded = rustls_native_certs::load_native_certs();
            if !loaded.errors.is_empty() {
                return Err(SmtpError::Tls(format!(
                    "读取系统根证书时出错（{} 项）",
                    loaded.errors.len()
                )));
            }
            if loaded.certs.is_empty() {
                // 系统根为空 ⇒ 无法校验证书。此时**必须失败**，不能退化成「不校验」。
                return Err(SmtpError::Tls("系统根证书为空，无法校验服务端证书".into()));
            }
            for cert in loaded.certs {
                roots
                    .add(cert)
                    .map_err(|e| SmtpError::Tls(format!("系统根证书无效: {e}")))?;
            }
        }
    }

    // 额外的信任锚：系统根 + 自签 CA（仍然强制校验）。
    if let TlsTrust::SystemRootsWithExtra(extra) = trust {
        for cert in extra {
            roots
                .add(cert.clone())
                .map_err(|e| SmtpError::Tls(format!("额外根证书无效: {e}")))?;
        }
    }

    let builder = match versions {
        TlsVersions::Default => rustls::ClientConfig::builder(),
        TlsVersions::Tls12Only => {
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        }
    };

    let config = builder.with_root_certificates(roots).with_no_client_auth();

    Ok(TlsConnector::from(Arc::new(config)))
}

/// 协议版本的可读写法（rustls 的 `Debug` 是 `TLSv1_3`，不好看）。
fn protocol_name(version: rustls::ProtocolVersion) -> String {
    match version {
        rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2".to_string(),
        rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3".to_string(),
        other => format!("{other:?}"),
    }
}

/// 解析叶子证书，做一次「体检」。
fn parse_certificate(der: &[u8]) -> Option<CertificateInfo> {
    use x509_parser::prelude::{FromDer, X509Certificate};

    let (_rest, cert) = X509Certificate::from_der(der).ok()?;

    let subject_cn = common_name(cert.subject());
    let issuer_cn = common_name(cert.issuer());

    let validity = cert.validity();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let not_after_ts = validity.not_after.timestamp();
    let not_before_ts = validity.not_before.timestamp();

    Some(CertificateInfo {
        subject_cn: subject_cn.clone(),
        issuer_cn: issuer_cn.clone(),
        subject: cert.subject().to_string(),
        issuer: cert.issuer().to_string(),
        not_before: format_date(&validity.not_before),
        not_after: format_date(&validity.not_after),
        expired: now > not_after_ts,
        not_yet_valid: now < not_before_ts,
        days_remaining: (not_after_ts - now) / 86_400,
        self_signed: subject_cn.is_some() && subject_cn == issuer_cn,
    })
}

/// 取 CN（通用名）。
fn common_name(name: &x509_parser::prelude::X509Name<'_>) -> Option<String> {
    name.iter_common_name()
        .find_map(|attr| attr.as_str().ok().map(|s| s.to_string()))
}

/// 把 ASN.1 时间格式化成 `YYYY-MM-DD`（UTC）。
fn format_date(time: &x509_parser::time::ASN1Time) -> String {
    let dt = time.to_datetime();
    format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day())
}
