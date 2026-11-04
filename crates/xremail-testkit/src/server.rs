//! 假 IMAP 服务器。
//!
//! 用途：让同步引擎、协议层的测试**不依赖真实邮箱**。
//! 真实邮箱只能复现一种情况；假服务器能复现**各种怪癖**——
//! 比如 163 拒绝 `EXAMINE`、要求先发 `ID`、认证前拒绝一切命令。
//!
//! ## 安全性
//!
//! * 用**自签证书**：真实客户端不可能信任它。测试要显式把那一张 CA
//!   交给客户端（`xremail_protocol_imap::TlsTrust::Roots`）。
//! * 监听地址是 `127.0.0.1`，只对本机可见。
//!
//! ## 状态
//!
//! 邮箱内容放在**共享的可变状态**里（多个连接看到同一份），
//! 这样 `STORE` 的效果能被后续的 `FETCH` / `SEARCH` 看到——
//! 真实服务器就是这样。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::certificate::{CertError, ServerCertificate};
use crate::config::{Fault, MailboxFixture, MessageFixture, ServerConfig};
use crate::fetch::{FetchItem, StoreOperation, build_fetch_response, parse_fetch, parse_store};
use crate::parser::{Command, quote};
use crate::search;
use crate::sequence::SequenceSet;

/// 回一行响应（自动加 CRLF）。
///
/// 宏而不是函数：IMAP 响应几乎都要带 tag 与参数，
/// 写成宏能直接用 `{}` 占位，不必在每个调用点包一层 `format!`。
macro_rules! reply {
    ($writer:expr, $($arg:tt)*) => {{
        // 先同步格式化好再 await：`std::fmt::Arguments` 不是 Send，
        // 跨 await 持有它会让整个 future 变成非 Send，tokio::spawn 就用不了了。
        let mut buffer = String::new();
        {
            use std::fmt::Write as _;
            let _ = buffer.write_fmt(format_args!($($arg)*));
        }
        write_line(&mut $writer, &buffer).await?
    }};
}

/// 假服务器错误。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// 监听失败。
    #[error("监听失败: {0}")]
    Listen(String),
    /// TLS 接受器构造失败。
    #[error("TLS 接受器构造失败: {0}")]
    Tls(String),
    /// 证书生成失败。
    #[error("证书生成失败: {0}")]
    Certificate(#[from] CertError),
}

/// 假 IMAP 服务器。
pub struct FakeImapServer {
    addr: SocketAddr,
    certificate: Arc<ServerCertificate>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
    /// 已收到的命令（按到达顺序）。
    log: Arc<Mutex<Vec<String>>>,
    /// 邮箱的**活的状态**（`APPEND` 之类的写操作会改它）。
    mailboxes: Arc<Mutex<Vec<MailboxFixture>>>,
    /// 这个服务器的 UIDVALIDITY。
    uid_validity: u32,
}

impl FakeImapServer {
    /// 启动一个假服务器。
    pub async fn start(config: ServerConfig) -> Result<Self, ServerError> {
        // 证书里的名字要和客户端连的主机名一致，否则主机名校验会失败
        // ——那正是我们要测的东西。
        let certificate = Arc::new(ServerCertificate::self_signed("localhost")?);

        let acceptor = TlsAcceptor::from(Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    certificate.chain.clone(),
                    certificate.private_key.clone_key(),
                )
                .map_err(|e| ServerError::Tls(e.to_string()))?,
        ));

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| ServerError::Listen(e.to_string()))?;
        let addr = listener
            .local_addr()
            .map_err(|e| ServerError::Listen(e.to_string()))?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let uid_validity = config.uid_validity;
        let config = Arc::new(config);
        // 邮箱的**活的状态**：配置里的内容是初始值，复制一份出来给连接用。
        let mailboxes = Arc::new(Mutex::new(config.mailboxes.clone()));
        // 命令日志与一次性故障的状态：**整个服务器共享**（不是每个连接一份）。
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let faults: Arc<Mutex<FaultState>> = Arc::new(Mutex::new(FaultState::default()));

        let handle = tokio::spawn({
            let log = log.clone();
            let mailboxes = mailboxes.clone();
            async move {
                tokio::select! {
                    _ = shutdown_rx => {}
                    _ = accept_loop(listener, acceptor, config, mailboxes, log, faults) => {}
                }
            }
        });

        Ok(Self {
            addr,
            certificate,
            shutdown: shutdown_tx,
            handle,
            log,
            mailboxes,
            uid_validity,
        })
    }

    /// 监听地址。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 端口。
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// 这个服务器的 UIDVALIDITY。
    pub fn uid_validity(&self) -> u32 {
        self.uid_validity
    }

    /// 邮箱当前的内容（快照）。
    ///
    /// 用来断言「服务器上真的多了一封信」——
    /// 比如 `APPEND` 之后已发送文件夹里该有几封。
    pub fn mailboxes(&self) -> Vec<MailboxFixture> {
        let guard = lock_mailboxes(&self.mailboxes);
        guard.clone()
    }

    /// 客户端要连的主机名（与证书里的 SAN 一致）。
    pub fn host(&self) -> &'static str {
        "localhost"
    }

    /// 证书。
    pub fn certificate(&self) -> &ServerCertificate {
        &self.certificate
    }

    /// 只信这张假证书的根集合。
    ///
    /// 客户端配置由**协议层**自己拼（它才知道该用哪个 provider），
    /// 这里只给根集合。
    pub fn root_store(&self) -> rustls::RootCertStore {
        self.certificate.root_store()
    }

    /// 只信这张假证书的客户端 TLS 配置。
    ///
    /// 给需要**绕过协议层**直接连的测试用（如验证服务器自身行为）。
    pub fn client_config(&self) -> rustls::ClientConfig {
        rustls::ClientConfig::builder()
            .with_root_certificates(self.certificate.root_store())
            .with_no_client_auth()
    }

    /// 停掉服务器。
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }

    /// 已收到的命令（按到达顺序，**不含** tag）。
    ///
    /// 用途：断言「客户端到底让服务器做了什么」——
    /// 比如「重连后没有重复请求已经拉过的邮件」「标记只被写了一次」。
    pub fn command_log(&self) -> Vec<String> {
        let guard = self
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    }
}

/// 故障状态：记录「一次性」故障触发过没有。
///
/// 一次性故障（`DropOnOnce` / `TruncateFetch { once: true }` / `IgnoreStoreOnce`）
/// 必须在**整个服务器**的范围内只触发一次——不是每个连接一次：
/// 「抖了一下」的意思是网络恢复之后就不再抖了。
#[derive(Debug, Default)]
struct FaultState {
    /// 已触发过的故障（键是故障的标识）。
    fired: std::collections::HashSet<String>,
}

impl FaultState {
    /// 认领一次性故障：第一次返回 `true`，之后返回 `false`。
    fn claim_once(&mut self, key: &str) -> bool {
        self.fired.insert(key.to_string())
    }
}

/// 接受连接循环。
async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    config: Arc<ServerConfig>,
    mailboxes: Arc<Mutex<Vec<MailboxFixture>>>,
    log: Arc<Mutex<Vec<String>>>,
    faults: Arc<Mutex<FaultState>>,
) {
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            continue;
        };
        let config = config.clone();
        let acceptor = acceptor.clone();
        let mailboxes = mailboxes.clone();
        let log = log.clone();
        let faults = faults.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, acceptor, config, mailboxes, log, faults).await;
        });
    }
}

/// 处理一个连接。
async fn handle_connection(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    config: Arc<ServerConfig>,
    mailboxes: Arc<Mutex<Vec<MailboxFixture>>>,
    log: Arc<Mutex<Vec<String>>>,
    faults: Arc<Mutex<FaultState>>,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let tls_stream = match acceptor.accept(stream).await {
        Ok(stream) => stream,
        // 握手失败（客户端不信任我们的自签证书）→ 直接断开。
        Err(_) => return Ok(()),
    };

    let (read_half, mut write_half) = tokio::io::split(tls_stream);
    let mut reader = BufReader::new(read_half);

    // 问候行。
    reply!(
        write_half,
        "* OK [CAPABILITY {}] Fake IMAP ready",
        config.capabilities.join(" ")
    );

    let mut state = SessionState::default();

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            break;
        }

        let Some(command) = Command::parse(&line) else {
            continue;
        };

        // 命令日志（**不含** tag）：用来断言「客户端到底让服务器做了什么」。
        {
            let mut guard = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.push(command_line(&command));
        }

        // 故障注入。
        let action = {
            let mut guard = faults
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            command_fault(&config, &mut guard, &command, state.authenticated)
        };
        match action {
            FaultAction::Hang => {
                // 不响应：客户端应当超时。
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                break;
            }
            FaultAction::Reject => {
                reply!(write_half, "{} NO [SERVERBUG] injected fault", command.tag);
                continue;
            }
            FaultAction::Drop => {
                // 直接断开：客户端**不知道**服务器执行了没。
                break;
            }
            FaultAction::DropAfterApply => {
                // 执行命令（效果落到服务器上），但**把响应丢掉**再断线。
                // 客户端收到的是「发出去就没了」，只能靠回读收敛。
                let mut discard = tokio::io::sink();
                let mut discard_reader = tokio::io::empty();
                let ctx = ServerCtx {
                    config: &config,
                    mailboxes: &mailboxes,
                    faults: &faults,
                };
                let _ = handle_command(
                    &mut discard,
                    &mut discard_reader,
                    &ctx,
                    &mut state,
                    &command,
                )
                .await;
                break;
            }
            FaultAction::Delay(ms) => {
                // 慢：命令收到后过一会儿才回。
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            FaultAction::None => {}
        }

        let ctx = ServerCtx {
            config: &config,
            mailboxes: &mailboxes,
            faults: &faults,
        };
        match handle_command(&mut write_half, &mut reader, &ctx, &mut state, &command).await {
            Ok(Continue::Continue) => {}
            Ok(Continue::Close) => break,
            Err(_) => break,
        }
    }

    Ok(())
}

/// 会话状态。
#[derive(Debug, Default)]
struct SessionState {
    /// 是否已认证。
    authenticated: bool,
    /// 是否已发过 `ID`。
    identified: bool,
    /// 当前选中的文件夹（在共享状态里的下标）。
    selected: Option<usize>,
    /// 选中的是不是只读（`EXAMINE` 打开的）。
    read_only: bool,
}

/// 命令处理需要的上下文。
///
/// 单独打包是为了不把每个处理函数的参数拉成一长串。
struct ServerCtx<'a> {
    /// 配置。
    config: &'a ServerConfig,
    /// 共享的邮箱状态。
    mailboxes: &'a Mutex<Vec<MailboxFixture>>,
    /// 共享的故障状态（一次性故障记账）。
    faults: &'a Mutex<FaultState>,
}

impl ServerCtx<'_> {
    /// 当前生效的「整封正文截断」故障（截到多少字节）。
    ///
    /// 一次性故障的记账必须发生在这里——**只在真的要截断整封正文时**才算数。
    /// 否则一次普通 FETCH（拉头，`BODY[HEADER]`）就会把这次故障「用掉」，
    /// 真正拉正文时反而不截断了。
    fn truncate_body_bytes(&self) -> Option<usize> {
        let fault = self
            .config
            .find_fault(|f| matches!(f, Fault::TruncateFetch { .. }))?;
        let Fault::TruncateFetch { bytes, once } = fault else {
            return None;
        };
        let (bytes, once) = (*bytes, *once);

        if !once {
            return Some(bytes);
        }
        let mut guard = self
            .faults
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.claim_once("truncate-fetch").then_some(bytes)
    }

    /// `STORE` 是不是「回 OK 但不真的改」。
    fn ignore_store(&self) -> bool {
        if self.config.has_fault(&Fault::IgnoreStore) {
            return true;
        }
        let has_once = self
            .config
            .find_fault(|f| matches!(f, Fault::IgnoreStoreOnce))
            .is_some();
        if !has_once {
            return false;
        }
        let mut guard = self
            .faults
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.claim_once("ignore-store")
    }
}

/// 一条命令在日志里的样子（**不含** tag）。
fn command_line(command: &Command) -> String {
    if command.args.is_empty() {
        command.name.clone()
    } else {
        format!("{} {}", command.name, command.args)
    }
}

/// 命令命中的故障动作。
enum FaultAction {
    /// 正常处理。
    None,
    /// 不响应（客户端应当超时）。
    Hang,
    /// 回 `NO`。
    Reject,
    /// 直接断开连接（客户端**不知道**服务器执行了没）。
    Drop,
    /// **执行**命令但不回响应（效果生效了，确认丢了）。
    ///
    /// 与 [`FaultAction::Drop`] 的区别：`Drop` 是执行**前**断线，
    /// 命令根本没生效；这个是执行**后**断线。
    /// 只有后者能验证「重放不产生重复副作用」——客户端必须先回读，
    /// 确认意图已生效，才不再发第二条 `STORE`。
    DropAfterApply,
    /// 延迟这么久再处理。
    Delay(u64),
}

/// 判定一条命令要触发哪个故障。
///
/// 匹配规则：
/// * **按命令名**：`UID FETCH` 要写子命令名 `FETCH`
///   （否则 `Fault::Reject("FETCH")` 拦不住 `UID FETCH`）；
/// * **按命令原文**：[`Fault::DropOnContains`] 才能区分两条都叫 `FETCH`
///   的命令（取 UID 集合 vs 批量拉元数据）。
///
/// `once` 类故障用 [`FaultState`] 记账：**整个服务器**只触发一次。
fn command_fault(
    config: &ServerConfig,
    faults: &mut FaultState,
    command: &Command,
    authenticated: bool,
) -> FaultAction {
    // 按原文匹配的故障最具体，先判。
    if let Some(Fault::DropOnContains { text, once }) =
        config.find_fault(|f| matches!(f, Fault::DropOnContains { .. }))
    {
        let hit = command_line(command)
            .to_ascii_uppercase()
            .contains(&text.to_ascii_uppercase());
        if hit && (!*once || faults.claim_once("drop-on-contains")) {
            return FaultAction::Drop;
        }
    }

    // 按命令名匹配：命令名本身 + `UID` 的子命令名。
    let mut names = vec![command.name.clone()];
    if let Some((sub, _)) = command.uid_subcommand() {
        names.push(sub);
    }

    for fault in &config.faults {
        let name_match = match fault {
            Fault::HangOn(command)
            | Fault::Reject(command)
            | Fault::DropOn(command)
            | Fault::DropOnOnce { command }
            | Fault::SlowResponse { command, .. }
            | Fault::DropAfterLoginOn { command }
            | Fault::DropAfterApply { command, .. } => {
                names.iter().any(|name| name.eq_ignore_ascii_case(command))
            }
            _ => false,
        };
        if !name_match {
            continue;
        }

        match fault {
            Fault::HangOn(_) => return FaultAction::Hang,
            Fault::Reject(_) => return FaultAction::Reject,
            Fault::DropOn(_) => return FaultAction::Drop,
            Fault::DropOnOnce { command } => {
                if faults.claim_once(&format!("drop-once:{command}")) {
                    return FaultAction::Drop;
                }
            }
            Fault::SlowResponse { delay_ms, .. } => return FaultAction::Delay(*delay_ms),
            // 只在**认证之后**才掐：登录之前它没有意义。
            Fault::DropAfterLoginOn { .. } if authenticated => return FaultAction::Drop,
            // 「执行了，确认丢了」：服务器这边效果生效，确认没送回来。
            Fault::DropAfterApply { command, once }
                if !*once || faults.claim_once(&format!("drop-after-apply:{command}")) =>
            {
                return FaultAction::DropAfterApply;
            }
            _ => {}
        }
    }

    FaultAction::None
}

/// 命令处理结果。
enum Continue {
    /// 继续。
    Continue,
    /// 关闭连接。
    Close,
}

/// 处理一条命令。
async fn handle_command<W, R>(
    mut writer: &mut W,
    reader: &mut R,
    ctx: &ServerCtx<'_>,
    state: &mut SessionState,
    command: &Command,
) -> std::io::Result<Continue>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let config = ctx.config;
    let mailboxes = ctx.mailboxes;

    // 163 行为：**认证前拒绝一切命令**（连 CAPABILITY / NOOP / LOGOUT 都不行）。
    // 我们的客户端总是先 LOGIN，所以这不会影响它；这条规则是为了
    // 保证假服务器不会比真实服务商更宽松。
    if !state.authenticated && command.name != "LOGIN" {
        reply!(writer, "{} BAD command not support", command.tag);
        return Ok(Continue::Continue);
    }

    match command.name.as_str() {
        "CAPABILITY" => {
            if config.has_fault(&Fault::NoCapabilityResponse) {
                reply!(writer, "{} BAD command not support", command.tag);
                return Ok(Continue::Continue);
            }
            reply!(writer, "* CAPABILITY {}", config.capabilities.join(" "));
            reply!(writer, "{} OK CAPABILITY completed", command.tag);
        }

        "LOGIN" => {
            let values = command.quoted_values();
            let user = values.first().cloned().unwrap_or_default();
            let password = values.get(1).cloned().unwrap_or_default();

            let rejected = config.has_fault(&Fault::RejectLogin)
                || user != config.user
                || password != config.password;

            if rejected {
                reply!(
                    writer,
                    "{} NO [AUTHENTICATIONFAILED] Invalid credentials",
                    command.tag
                );
                return Ok(Continue::Continue);
            }

            state.authenticated = true;
            reply!(writer, "{} OK LOGIN completed", command.tag);

            if config.has_fault(&Fault::DropAfterLogin) {
                return Ok(Continue::Close);
            }
        }

        "ID" => {
            if !state.authenticated {
                reply!(writer, "{} NO Not authenticated", command.tag);
                return Ok(Continue::Continue);
            }
            state.identified = true;
            // 服务器也报一下自己的身份（RFC 2971）。
            reply!(
                writer,
                "* ID (\"name\" \"FakeIMAP\" \"vendor\" \"Xremail Testkit\")"
            );
            reply!(writer, "{} OK ID completed", command.tag);
        }

        "LIST" | "LSUB" => {
            if let Some(err) = check_mailbox_access(config, state) {
                reply!(writer, "{} NO {}", command.tag, err);
                return Ok(Continue::Continue);
            }
            let listing = {
                let guard = lock_mailboxes(mailboxes);
                guard
                    .iter()
                    .map(|mailbox| {
                        (
                            mailbox.attributes.join(" "),
                            quote(&encode_mailbox_name(&mailbox.name)),
                        )
                    })
                    .collect::<Vec<(String, String)>>()
            };
            for (attributes, name) in listing {
                reply!(writer, "* LIST ({attributes}) \"/\" {name}");
            }
            reply!(writer, "{} OK LIST completed", command.tag);
        }

        "SELECT" | "EXAMINE" => {
            if command.name == "EXAMINE" && config.reject_examine {
                reply!(
                    writer,
                    "{} NO EXAMINE Unsafe Login. Please contact kefu@188.com for help",
                    command.tag
                );
                return Ok(Continue::Continue);
            }
            if let Some(err) = check_mailbox_access(config, state) {
                reply!(writer, "{} NO {}", command.tag, err);
                return Ok(Continue::Continue);
            }

            let read_only = command.name == "EXAMINE";
            let name = command.first_quoted().unwrap_or_default();

            let summary = {
                let guard = lock_mailboxes(mailboxes);
                let index = find_mailbox_index(&guard, &name);
                index.map(|index| {
                    let mailbox = &guard[index];
                    (
                        index,
                        mailbox.messages.len(),
                        mailbox
                            .messages
                            .iter()
                            .filter(|m| m.has_flag("\\Recent"))
                            .count(),
                        mailbox.max_uid(),
                        mailbox.messages.iter().map(|m| m.modseq).max().unwrap_or(0),
                    )
                })
            };

            let Some((index, exists, recent, _max_uid, highest_modseq)) = summary else {
                reply!(writer, "{} NO Mailbox does not exist", command.tag);
                return Ok(Continue::Continue);
            };

            state.selected = Some(index);
            state.read_only = read_only;

            reply!(writer, "* {exists} EXISTS");
            reply!(writer, "* {recent} RECENT");
            reply!(
                writer,
                "* OK [UIDVALIDITY {}] UIDs valid",
                config.uid_validity
            );
            if config.has_capability("CONDSTORE") {
                reply!(
                    writer,
                    "* OK [HIGHESTMODSEQ {highest_modseq}] Highest mod-sequence"
                );
            }
            reply!(
                writer,
                "* FLAGS (\\Answered \\Seen \\Deleted \\Draft \\Flagged)"
            );
            reply!(
                writer,
                "* OK [PERMANENTFLAGS (\\Answered \\Seen \\Deleted \\Draft \\Flagged)] Limited"
            );

            let mode = if read_only { "READ-ONLY" } else { "READ-WRITE" };
            reply!(
                writer,
                "{} OK [{mode}] {} completed",
                command.tag,
                command.name
            );
        }

        "STATUS" => {
            if let Some(err) = check_mailbox_access(config, state) {
                reply!(writer, "{} NO {}", command.tag, err);
                return Ok(Continue::Continue);
            }
            let name = command.first_quoted().unwrap_or_default();
            let summary = {
                let guard = lock_mailboxes(mailboxes);
                find_mailbox_index(&guard, &name).map(|index| {
                    let mailbox = &guard[index];
                    let unseen = mailbox
                        .messages
                        .iter()
                        .filter(|m| !m.has_flag("\\Seen"))
                        .count();
                    (mailbox.messages.len(), unseen)
                })
            };

            let Some((exists, unseen)) = summary else {
                reply!(writer, "{} NO Mailbox does not exist", command.tag);
                return Ok(Continue::Continue);
            };

            reply!(
                writer,
                "* STATUS {} (MESSAGES {exists} UNSEEN {unseen} UIDVALIDITY {})",
                quote(&name),
                config.uid_validity
            );
            reply!(writer, "{} OK STATUS completed", command.tag);
        }

        "APPEND" => {
            // APPEND：把一封信存进文件夹末尾。
            //
            // 正文是**字面量**：命令头里写长度，服务器回 `+`，
            // 客户端才发那么多字节过来——所以这里要读 reader。
            let Some(size) = command.literal_size() else {
                reply!(writer, "{} BAD APPEND without literal", command.tag);
                return Ok(Continue::Continue);
            };

            let name = command.first_quoted().unwrap_or_default();
            let index = {
                let guard = lock_mailboxes(mailboxes);
                find_mailbox_index(&guard, &name)
            };
            let Some(index) = index else {
                reply!(writer, "{} NO Mailbox does not exist", command.tag);
                return Ok(Continue::Continue);
            };

            let flags = parse_append_flags(&command.args);

            // 让客户端发正文。
            reply!(writer, "+ Ready for literal data");

            let mut body = vec![0u8; size];
            reader.read_exact(&mut body).await?;
            // 字面量后面跟一个 CRLF，命令才算结束。
            let mut terminator = [0u8; 2];
            reader.read_exact(&mut terminator).await?;

            // 锁内只改数据，不 await（锁不能跨 await）。
            let appended = {
                let mut guard = lock_mailboxes(mailboxes);
                guard.get_mut(index).map(|mailbox| {
                    let uid = mailbox.max_uid() + 1;
                    let modseq = mailbox.messages.iter().map(|m| m.modseq).max().unwrap_or(0) + 1;
                    mailbox.messages.push(MessageFixture {
                        uid,
                        flags: flags.clone(),
                        content: String::from_utf8_lossy(&body).into_owned(),
                        raw: Some(body),
                        modseq,
                    });
                    uid
                })
            };
            let Some(uid) = appended else {
                reply!(writer, "{} NO Mailbox does not exist", command.tag);
                return Ok(Continue::Continue);
            };

            reply!(
                writer,
                "{} OK [APPENDUID {} {}] APPEND completed",
                command.tag,
                config.uid_validity,
                uid
            );
        }

        "FETCH" => {
            return handle_fetch(&mut writer, ctx, state, &command.tag, &command.args, false).await;
        }

        "SEARCH" => {
            handle_search(&mut writer, ctx, state, &command.tag, &command.args, false).await?;
        }

        "STORE" => {
            handle_store(
                &mut writer,
                ctx,
                state,
                &command.tag,
                &command.args,
                false,
                "STORE",
            )
            .await?;
        }

        "COPY" => {
            handle_copy_or_move(
                &mut writer,
                ctx,
                state,
                &command.tag,
                &command.args,
                false,
                false,
            )
            .await?;
        }

        "MOVE" => {
            handle_copy_or_move(
                &mut writer,
                ctx,
                state,
                &command.tag,
                &command.args,
                false,
                true,
            )
            .await?;
        }

        "EXPUNGE" => {
            handle_expunge(&mut writer, ctx, state, &command.tag, None, "EXPUNGE").await?;
        }

        "UID" => {
            // `UID FETCH` / `UID SEARCH` / `UID STORE`。
            let Some((sub, rest)) = command.uid_subcommand() else {
                reply!(writer, "{} BAD Invalid UID command", command.tag);
                return Ok(Continue::Continue);
            };
            match sub.as_str() {
                "FETCH" => {
                    return handle_fetch(&mut writer, ctx, state, &command.tag, rest, true).await;
                }
                "SEARCH" => {
                    handle_search(&mut writer, ctx, state, &command.tag, rest, true).await?;
                }
                "STORE" => {
                    handle_store(
                        &mut writer,
                        ctx,
                        state,
                        &command.tag,
                        rest,
                        true,
                        "UID STORE",
                    )
                    .await?;
                }
                "COPY" => {
                    handle_copy_or_move(&mut writer, ctx, state, &command.tag, rest, true, false)
                        .await?;
                }
                "MOVE" => {
                    handle_copy_or_move(&mut writer, ctx, state, &command.tag, rest, true, true)
                        .await?;
                }
                "EXPUNGE" => {
                    handle_expunge(
                        &mut writer,
                        ctx,
                        state,
                        &command.tag,
                        Some(rest),
                        "UID EXPUNGE",
                    )
                    .await?;
                }
                other => {
                    reply!(
                        writer,
                        "{} BAD Unsupported UID command: {}",
                        command.tag,
                        other
                    );
                }
            }
        }

        "NOOP" => {
            reply!(writer, "{} OK NOOP completed", command.tag);
        }

        "LOGOUT" => {
            reply!(writer, "* BYE Fake IMAP signing off");
            reply!(writer, "{} OK LOGOUT completed", command.tag);
            return Ok(Continue::Close);
        }

        _ => {
            reply!(writer, "{} BAD command not support", command.tag);
        }
    }

    Ok(Continue::Continue)
}

/// `COPY` / `MOVE` 的实现（RFC 3501 §6.4.7、RFC 6851）。
///
/// `args` 形如 `<set> <mailbox>`。
async fn handle_copy_or_move<W>(
    mut writer: &mut W,
    ctx: &ServerCtx<'_>,
    state: &mut SessionState,
    tag: &str,
    args: &str,
    by_uid: bool,
    is_move: bool,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let config = ctx.config;
    let mailboxes = ctx.mailboxes;
    let label = if is_move { "MOVE" } else { "COPY" };

    let Some(index) = state.selected else {
        reply!(writer, "{tag} NO {label} State error");
        return Ok(());
    };

    // `MOVE` 要从源文件夹里删东西，只读文件夹不行；`COPY` 不动源文件夹。
    if is_move && state.read_only {
        reply!(writer, "{tag} NO [READ-ONLY] Mailbox is read-only");
        return Ok(());
    }

    let Some((set_text, rest)) = split_first_token(args) else {
        reply!(writer, "{tag} BAD Missing sequence set");
        return Ok(());
    };
    let set = match SequenceSet::parse(set_text) {
        Ok(set) => set,
        Err(err) => {
            reply!(writer, "{tag} BAD {err}");
            return Ok(());
        }
    };

    // 文件夹名可能是带引号的（`"INBOX"`），也可能是 modified UTF-7 形态。
    let target_name = crate::parser::quoted_values(rest)
        .first()
        .cloned()
        .unwrap_or_else(|| rest.trim().to_string());
    let target_index = {
        let guard = lock_mailboxes(mailboxes);
        find_mailbox_index(&guard, &target_name)
    };
    let Some(target_index) = target_index else {
        // `TRYCREATE` 是真实服务器的常见提示：目标文件夹不存在，先建再试。
        reply!(writer, "{tag} NO [TRYCREATE] Mailbox does not exist");
        return Ok(());
    };

    // 锁内只改数据，不 await（锁不能跨 await）。
    let outcome: Option<(Vec<u32>, Vec<u32>)> = 'calc: {
        let mut guard = lock_mailboxes(mailboxes);

        // 先把源消息**快照**出来：源和目标可能是同一个文件夹，
        // 那样没法同时持有不可变借用和可变借用。
        let snapshot: Vec<(u32, MessageFixture)> = {
            let Some(source) = guard.get(index) else {
                break 'calc None;
            };
            let targets = resolve_targets(&set, &source.messages, by_uid);
            let mut pairs: Vec<(u32, MessageFixture)> = targets
                .iter()
                .map(|&i| (source.messages[i].uid, source.messages[i].clone()))
                .collect();
            // 真实服务器的 `COPYUID` 是升序的。
            pairs.sort_by_key(|(uid, _)| *uid);
            pairs
        };

        if guard.get(target_index).is_none() {
            break 'calc None;
        }

        let mut source_uids = Vec::with_capacity(snapshot.len());
        let mut destination_uids = Vec::with_capacity(snapshot.len());
        for (uid, mut message) in snapshot {
            let destination = &mut guard[target_index];
            let new_uid = destination.max_uid() + 1;
            message.uid = new_uid;
            message.modseq = destination
                .messages
                .iter()
                .map(|m| m.modseq)
                .max()
                .unwrap_or(0)
                + 1;
            destination.messages.push(message);
            source_uids.push(uid);
            destination_uids.push(new_uid);
        }

        // `MOVE`：拷完再把源里那几封真的删掉。
        // RFC 6851 要求只删**被移动的那些**，不是「所有带 `\Deleted` 的」
        // ——后者是 `EXPUNGE` 的语义，会把用户手动标删的信也一并干掉。
        if is_move && !source_uids.is_empty() {
            let source = &mut guard[index];
            source
                .messages
                .retain(|message| !source_uids.contains(&message.uid));
        }

        Some((source_uids, destination_uids))
    };

    let Some((source_uids, destination_uids)) = outcome else {
        reply!(writer, "{tag} NO Mailbox does not exist");
        return Ok(());
    };

    if source_uids.is_empty() {
        // 集合没命中任何信：空操作也是成功（RFC 3501 不要求报错）。
        reply!(writer, "{tag} OK {label} completed");
        return Ok(());
    }

    reply!(
        writer,
        "{tag} OK [COPYUID {} {} {}] {label} completed",
        config.uid_validity,
        render_uid_set(&source_uids),
        render_uid_set(&destination_uids)
    );
    Ok(())
}

/// `EXPUNGE`（RFC 3501 §6.4.3）与 `UID EXPUNGE`（RFC 4315 §2.1）。
///
/// `args` 为 `None` 时是普通 `EXPUNGE`（删所有带 `\Deleted` 的）；
/// 为 `Some(set)` 时只删「集合里 **且** 带 `\Deleted`」的。
async fn handle_expunge<W>(
    mut writer: &mut W,
    ctx: &ServerCtx<'_>,
    state: &mut SessionState,
    tag: &str,
    args: Option<&str>,
    label: &str,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(index) = state.selected else {
        reply!(writer, "{tag} NO {label} State error");
        return Ok(());
    };
    if state.read_only {
        reply!(writer, "{tag} NO [READ-ONLY] Mailbox is read-only");
        return Ok(());
    }

    let set = match args {
        Some(text) => match SequenceSet::parse(text.trim()) {
            Ok(set) => Some(set),
            Err(err) => {
                reply!(writer, "{tag} BAD {err}");
                return Ok(());
            }
        },
        None => None,
    };

    // 锁内只改数据，不 await。
    let removed: Option<Vec<u32>> = {
        let mut guard = lock_mailboxes(ctx.mailboxes);
        match guard.get_mut(index) {
            None => None,
            Some(mailbox) => {
                let max_uid = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
                let doomed: Vec<usize> = mailbox
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| message.has_flag("\\Deleted"))
                    .filter(|(_, message)| match &set {
                        Some(set) => set.contains(message.uid, max_uid),
                        None => true,
                    })
                    .map(|(index, _)| index)
                    .collect();

                // 从后往前删，免得下标错位。
                for &index in doomed.iter().rev() {
                    mailbox.messages.remove(index);
                }

                Some(doomed.iter().map(|&index| index as u32 + 1).collect())
            }
        }
    };

    let Some(removed) = removed else {
        reply!(writer, "{tag} NO Mailbox does not exist");
        return Ok(());
    };

    // 从大到小报：这样每个序号在**报出时**都还是有效的。
    for sequence in removed.iter().rev() {
        reply!(writer, "* {} EXPUNGE", sequence);
    }
    reply!(writer, "{tag} OK {label} completed");
    Ok(())
}

/// 把升序的 UID 列表渲染成紧凑的 `uid-set`（连续段用 `a:b`）。
fn render_uid_set(uids: &[u32]) -> String {
    let mut out = String::new();
    let mut start = 0;
    while start < uids.len() {
        let mut end = start;
        while end + 1 < uids.len() && uids[end + 1] == uids[end] + 1 {
            end += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if end == start {
            out.push_str(&uids[start].to_string());
        } else {
            out.push_str(&format!("{}:{}", uids[start], uids[end]));
        }
        start = end + 1;
    }
    out
}

/// 解析 `APPEND` 参数里的标记列表（`(\Seen)`）。
fn parse_append_flags(args: &str) -> Vec<String> {
    let Some(open) = args.find('(') else {
        return Vec::new();
    };
    let Some(close) = args[open..].find(')') else {
        return Vec::new();
    };
    args[open + 1..open + close]
        .split_whitespace()
        .map(|flag| flag.to_string())
        .collect()
}

/// 处理 `FETCH` / `UID FETCH`。
async fn handle_fetch<W>(
    mut writer: &mut W,
    ctx: &ServerCtx<'_>,
    state: &mut SessionState,
    tag: &str,
    args: &str,
    by_uid: bool,
) -> std::io::Result<Continue>
where
    W: AsyncWrite + Unpin,
{
    let config = ctx.config;
    let mailboxes = ctx.mailboxes;
    // IMAP 状态机：必须先选中一个文件夹。
    let Some(index) = state.selected else {
        reply!(writer, "{tag} NO FETCH State error");
        return Ok(Continue::Continue);
    };

    let (set_text, items) = match parse_fetch(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            reply!(writer, "{tag} BAD {err}");
            return Ok(Continue::Continue);
        }
    };
    let set = match SequenceSet::parse(&set_text) {
        Ok(set) => set,
        Err(err) => {
            reply!(writer, "{tag} BAD {err}");
            return Ok(Continue::Continue);
        }
    };

    // 取整封正文（不带 PEEK）会**置上 `\Seen`**（RFC 3501 §6.4.8）。
    let sets_seen = items.iter().any(|item| match item {
        FetchItem::Body { peek, .. } => !peek,
        FetchItem::Rfc822 { part } => part.is_none(),
        _ => false,
    });

    // 锁内只算数据，锁外才写响应（锁不能跨 await）。
    let prepared = {
        let mut guard = lock_mailboxes(mailboxes);
        match guard.get_mut(index) {
            None => None,
            Some(mailbox) => {
                let targets = resolve_targets(&set, &mailbox.messages, by_uid);

                if sets_seen {
                    for &target in &targets {
                        let message = &mut mailbox.messages[target];
                        if !message.has_flag("\\Seen") {
                            message.flags.push("\\Seen".to_string());
                            message.modseq += 1;
                        }
                    }
                }

                let with_modseq = config.has_capability("CONDSTORE");
                let responses: Vec<crate::fetch::FetchResponse> = targets
                    .iter()
                    .map(|&target| {
                        let message = &mailbox.messages[target];
                        build_fetch_response(target as u32 + 1, message, &items, with_modseq)
                    })
                    .collect();
                Some(responses)
            }
        }
    };

    let Some(responses) = prepared else {
        reply!(writer, "{tag} NO Mailbox does not exist");
        return Ok(Continue::Continue);
    };

    for response in responses {
        // 只截断**整封正文**（`BODY[]`）；头不截——截了头会挡住
        // 「先同步元数据、再按需拉正文」这条正常链路。
        let cut = match &response.literal {
            Some((section, _)) if section == "BODY[]" => ctx.truncate_body_bytes(),
            _ => None,
        };

        write_fetch_response(&mut writer, &response, cut).await?;

        if cut.is_some() {
            // 收到一半就断了：连接直接关掉，客户端会发现字面量对不上。
            return Ok(Continue::Close);
        }
    }
    reply!(writer, "{tag} OK FETCH completed");
    Ok(Continue::Continue)
}

/// 写一条 FETCH 响应。
///
/// 有字面量时格式是 `* 1 FETCH (UID 1 BODY[HEADER] {23}\r\n<23 字节>)`。
///
/// `truncate` 非空时模拟「收到一半就断了」：**长度照报，实际只发前 N 字节**，
/// 剩下的不补——客户端读不满声明的长度就会报错。
async fn write_fetch_response<W>(
    writer: &mut W,
    response: &crate::fetch::FetchResponse,
    truncate: Option<usize>,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match &response.literal {
        Some((section, bytes)) => {
            // 字面量的长度写在行尾，**真正的字节在下一行之后**，
            // 所以这一行不能带 `)`——`)` 要等字节发完再补。
            let prefix = if response.simple.is_empty() {
                format!("* {} FETCH ({} {{{}}}", response.seq, section, bytes.len())
            } else {
                format!(
                    "* {} FETCH ({} {} {{{}}}",
                    response.seq,
                    response.simple,
                    section,
                    bytes.len()
                )
            };
            write_line(writer, &prefix).await?;
            match truncate {
                // 截断：**不写**收尾的 `)`——连接马上就会断，
                // 客户端必然发现字面量不完整。
                Some(limit) => {
                    let cut = limit.min(bytes.len());
                    writer.write_all(&bytes[..cut]).await?;
                    writer.flush().await
                }
                None => {
                    writer.write_all(bytes).await?;
                    writer.write_all(b")\r\n").await?;
                    writer.flush().await
                }
            }
        }
        None => {
            write_line(
                writer,
                &format!("* {} FETCH ({})", response.seq, response.simple),
            )
            .await
        }
    }
}

/// 处理 `SEARCH` / `UID SEARCH`。
async fn handle_search<W>(
    mut writer: &mut W,
    ctx: &ServerCtx<'_>,
    state: &mut SessionState,
    tag: &str,
    args: &str,
    by_uid: bool,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mailboxes = ctx.mailboxes;
    let Some(index) = state.selected else {
        reply!(writer, "{tag} NO SEARCH State error");
        return Ok(());
    };

    let key = match search::parse(args) {
        Ok(key) => key,
        Err(err) => {
            reply!(writer, "{tag} BAD {err}");
            return Ok(());
        }
    };

    let results = {
        let guard = lock_mailboxes(mailboxes);
        match guard.get(index) {
            None => None,
            Some(mailbox) => {
                let max_uid = mailbox.max_uid();
                let hits: Vec<usize> = mailbox
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| search::matches(&key, message, max_uid))
                    .map(|(index, _)| index)
                    .collect();

                Some(if by_uid {
                    hits.iter()
                        .map(|&index| mailbox.messages[index].uid)
                        .collect::<Vec<u32>>()
                } else {
                    hits.iter()
                        .map(|&index| index as u32 + 1)
                        .collect::<Vec<u32>>()
                })
            }
        }
    };

    let Some(results) = results else {
        reply!(writer, "{tag} NO Mailbox does not exist");
        return Ok(());
    };

    let joined = results
        .iter()
        .map(u32::to_string)
        .collect::<Vec<String>>()
        .join(" ");
    reply!(writer, "* SEARCH {joined}");
    reply!(writer, "{tag} OK SEARCH completed");
    Ok(())
}

/// `STORE` 的结果。
enum StoreOutcome {
    /// 出错（`NO`）。
    Failed(String),
    /// 成功，被改动的邮件 `(序号, UID, 标记)`。
    Changed(Vec<(u32, u32, Vec<String>)>),
}

/// 处理 `STORE` / `UID STORE`。
async fn handle_store<W>(
    mut writer: &mut W,
    ctx: &ServerCtx<'_>,
    state: &mut SessionState,
    tag: &str,
    args: &str,
    by_uid: bool,
    label: &str,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let config = ctx.config;
    let mailboxes = ctx.mailboxes;
    let Some(index) = state.selected else {
        reply!(writer, "{tag} NO STORE State error");
        return Ok(());
    };

    // `EXAMINE` 打开的文件夹是只读的，不能改标记。
    if state.read_only {
        reply!(writer, "{tag} NO [READ-ONLY] Mailbox is read-only");
        return Ok(());
    }

    let (set_text, rest) = match split_first_token(args) {
        Some(parts) => parts,
        None => {
            reply!(writer, "{tag} BAD Missing sequence set");
            return Ok(());
        }
    };
    let set = match SequenceSet::parse(set_text) {
        Ok(set) => set,
        Err(err) => {
            reply!(writer, "{tag} BAD {err}");
            return Ok(());
        }
    };
    let request = match parse_store(rest) {
        Ok(request) => request,
        Err(err) => {
            reply!(writer, "{tag} BAD {err}");
            return Ok(());
        }
    };

    // 「听话的假服务器」：回 OK 但**不真的改**。
    // 客户端只有**回读确认**才能发现（见 ARCHITECTURE §6）。
    // 先算好，免得在邮箱锁里再拿一次故障锁。
    let ignore_store = ctx.ignore_store();

    // 条件写（RFC 3501 §6.4.6）：**全有或全无**——
    // 只要有一封的 modseq 对不上，整条命令都不改任何东西。
    let outcome = {
        let mut guard = lock_mailboxes(mailboxes);
        match guard.get_mut(index) {
            None => None,
            Some(mailbox) => {
                let targets = resolve_targets(&set, &mailbox.messages, by_uid);

                // 条件写只在服务器宣告 CONDSTORE 时才有意义。
                // 163 这类服务器没有 CONDSTORE，发 `UNCHANGEDSINCE` 会被当成语法错误。
                let supports_condstore = config.has_capability("CONDSTORE");
                let conflict = supports_condstore
                    && request.unchangedsince.is_some_and(|expected| {
                        targets
                            .iter()
                            .any(|&target| mailbox.messages[target].modseq != expected)
                    });

                if conflict {
                    Some(StoreOutcome::Failed(format!(
                        "[MODIFIED] {label} failed: mod-sequence changed"
                    )))
                } else {
                    let mut changes = Vec::new();
                    for &target in &targets {
                        let message = &mut mailbox.messages[target];
                        let before = message.flags.clone();

                        match request.operation {
                            StoreOperation::Add => {
                                for flag in &request.flags {
                                    if !message.has_flag(flag) {
                                        message.flags.push(flag.clone());
                                    }
                                }
                            }
                            StoreOperation::Remove => {
                                message.flags.retain(|flag| {
                                    !request
                                        .flags
                                        .iter()
                                        .any(|remove| remove.eq_ignore_ascii_case(flag))
                                });
                            }
                            StoreOperation::Set => {
                                message.flags = request.flags.clone();
                            }
                        }

                        let ignore = ignore_store;
                        if ignore {
                            // 「听话的假服务器」：回 OK 但**不真的改**。
                            // 客户端只有**回读确认**才能发现（见 ARCHITECTURE §6）。
                            message.flags = before;
                        } else if message.flags != before {
                            message.modseq += 1;
                            changes.push((target as u32 + 1, message.uid, message.flags.clone()));
                        }
                    }
                    Some(StoreOutcome::Changed(changes))
                }
            }
        }
    };

    match outcome {
        None => {
            reply!(writer, "{tag} NO Mailbox does not exist");
        }
        Some(StoreOutcome::Failed(message)) => {
            reply!(writer, "{tag} NO {message}");
        }
        Some(StoreOutcome::Changed(changes)) => {
            // `.SILENT` 变体不回 untagged FETCH 响应。
            if !request.silent {
                for (seq, uid, flags) in &changes {
                    reply!(
                        writer,
                        "* {seq} FETCH (UID {uid} FLAGS ({}))",
                        flags.join(" ")
                    );
                }
            }
            reply!(writer, "{tag} OK {label} completed");
        }
    }

    Ok(())
}

/// 算出 sequence set 命中哪些邮件（返回下标）。
///
/// `by_uid` 为真时 sequence set 是 **UID**；否则是**序号**（第几封）。
///
/// **不展开 set**——而是拿每封邮件去问 set。
/// 因为 `1:*` 在大 UID 空间下可能覆盖上亿个 UID（163 就是），
/// 展开成 `Vec` 会直接吃掉几个 GB。
fn resolve_targets(
    set: &SequenceSet,
    messages: &[crate::config::MessageFixture],
    by_uid: bool,
) -> Vec<usize> {
    if by_uid {
        let max_uid = messages.iter().map(|m| m.uid).max().unwrap_or(0);
        messages
            .iter()
            .enumerate()
            .filter(|(_, message)| set.contains(message.uid, max_uid))
            .map(|(index, _)| index)
            .collect()
    } else {
        let count = messages.len() as u32;
        messages
            .iter()
            .enumerate()
            .filter(|(index, _)| set.contains((*index as u32) + 1, count))
            .map(|(index, _)| index)
            .collect()
    }
}

/// 切出第一个词（其余原样返回）。
fn split_first_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    match input.find(char::is_whitespace) {
        Some(index) => Some((&input[..index], input[index..].trim_start())),
        None => {
            if input.is_empty() {
                None
            } else {
                Some((input, ""))
            }
        }
    }
}

/// 检查邮箱访问权限。
///
/// 复现 163 的两道人墙：未认证不能访问；要求 `ID` 时没发 `ID` 也不能访问。
fn check_mailbox_access(config: &ServerConfig, state: &SessionState) -> Option<&'static str> {
    if !state.authenticated {
        return Some("Not authenticated");
    }
    if config.require_id && !state.identified {
        return Some("Unsafe Login. Please contact kefu@188.com for help");
    }
    None
}

/// 锁住邮箱状态。
///
/// 用 `std::sync::Mutex`：注意**绝不能**把 guard 跨 `.await` 持有
/// （那会让 future 变成非 `Send`，`tokio::spawn` 就用不了了）。
fn lock_mailboxes(mailboxes: &Mutex<Vec<MailboxFixture>>) -> MutexGuard<'_, Vec<MailboxFixture>> {
    mailboxes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 按名字找文件夹下标（接受明文与 UTF-7 两种形态）。
fn find_mailbox_index(mailboxes: &[MailboxFixture], name: &str) -> Option<usize> {
    mailboxes
        .iter()
        .position(|mailbox| mailbox.name == name || encode_mailbox_name(&mailbox.name) == name)
}

/// 把文件夹名编码成 IMAP 线路形态（modified UTF-7）。
///
/// 中文名在线上是 `&XfJT0ZAB-` 这种形态。
fn encode_mailbox_name(name: &str) -> String {
    if name.is_ascii() {
        return name.to_string();
    }
    let mut out = String::new();
    out.push('&');
    let mut buffer: Vec<u8> = Vec::new();
    for unit in name.encode_utf16() {
        buffer.extend_from_slice(&unit.to_be_bytes());
    }
    out.push_str(&modified_b64_encode(&buffer));
    out.push('-');
    out
}

/// modified BASE64 编码（`/` 写作 `,`，无填充）。
fn modified_b64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in input {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(ALPHABET[((buffer >> bits) & 0x3f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (6 - bits)) & 0x3f) as usize] as char);
    }
    out
}

/// 写一行（自动加 CRLF）。
async fn write_line<W>(writer: &mut W, line: &str) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await
}
