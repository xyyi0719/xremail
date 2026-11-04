//! 假 IMAP 服务器的 `IDLE`（RFC 2177）支持。
//!
//! 这些用例测的是**假服务器自己**的行为，所以用**裸连接**（只走 TLS + 文本行），
//! 不经过协议层——协议层的 IDLE 客户端还没写，那是另一条链路的事。
//!
//! 每条用例的注释都写明了「它在什么**错误实现**上会失败」：
//! 用例的价值不在于跑绿，而在于能钉住那些看起来也能跑通的错实现。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use xremail_testkit::{FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig};

/// 等响应的上限。给小一点：服务器真挂住时不要在 CI 里干等。
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// 一条裸 IMAP 连接。
struct Raw {
    reader: BufReader<tokio::io::ReadHalf<tokio_rustls::client::TlsStream<TcpStream>>>,
    writer: tokio::io::WriteHalf<tokio_rustls::client::TlsStream<TcpStream>>,
}

impl Raw {
    /// 连上去，并读掉问候行。
    async fn connect(server: &FakeImapServer) -> Self {
        let connector = TlsConnector::from(Arc::new(server.client_config()));
        let tcp = TcpStream::connect(server.addr())
            .await
            .expect("连假服务器失败");
        let name =
            rustls::pki_types::ServerName::try_from(server.host().to_string()).expect("主机名非法");
        let stream = connector.connect(name, tcp).await.expect("TLS 握手失败");
        let (read_half, write_half) = tokio::io::split(stream);
        let mut raw = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        };
        let greeting = raw.read_line().await;
        assert!(
            greeting.starts_with("* OK"),
            "问候行不是 `* OK`：{greeting:?}"
        );
        raw
    }

    /// 发一行（自动加 CRLF）。
    async fn send(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("写命令失败");
        self.writer.flush().await.expect("刷新失败");
    }

    /// 读一行；超时、断线或读出错（服务器没发 `close_notify` 就断开）时
    /// 返回 `None`。
    ///
    /// 「被掐断」在故障注入里是**预期行为**，所以不把 I/O 错误当 panic。
    async fn read_line_or_none(&mut self, timeout: Duration) -> Option<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(timeout, self.reader.read_line(&mut line))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        Some(line.trim_end_matches(['\r', '\n']).to_string())
    }

    /// 读一行（超时或断线即 panic）。
    async fn read_line(&mut self) -> String {
        match self.read_line_or_none(READ_TIMEOUT).await {
            Some(line) => line,
            None => panic!("等服务器响应超时，或者服务器把连接关掉了"),
        }
    }

    /// 读一行，并断言它包含某段文本。
    async fn expect_contains(&mut self, needle: &str) -> String {
        let line = self.read_line().await;
        assert!(
            line.contains(needle),
            "期望包含 {needle:?}，实际是 {line:?}"
        );
        line
    }

    /// 读到第一条以 `tag ` 开头的响应（打完 tag 的收尾响应）。
    async fn expect_tagged(&mut self, tag: &str) -> String {
        loop {
            let line = self.read_line().await;
            if line.starts_with(&format!("{tag} ")) {
                return line;
            }
        }
    }

    /// 登录。
    async fn login(&mut self) {
        // 服务器的 `LOGIN` 只认 quoted-string（`command.quoted_values()`）。
        self.send("l1 LOGIN \"tester@example.com\" \"test-password\"")
            .await;
        let line = self.expect_tagged("l1").await;
        assert!(line.starts_with("l1 OK"), "登录失败：{line:?}");
    }

    /// 选中 `INBOX`。
    async fn select_inbox(&mut self) {
        // 文件夹名同样要从 quoted-string 里取。
        self.send("s1 SELECT \"INBOX\"").await;
        let line = self.expect_tagged("s1").await;
        assert!(line.contains("OK"), "SELECT 失败：{line:?}");
    }

    /// 登录 + 选中 `INBOX`：`IDLE` 的正常前置条件。
    async fn login_and_select(&mut self) {
        self.login().await;
        self.select_inbox().await;
    }
}

/// 带 `IDLE` 能力、`INBOX` 里有信、其余与默认一致。
fn config() -> ServerConfig {
    let mut config = ServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(1_792_658_701).with_flag("\\Seen"))
                .with_message(MessageFixture::new(1_792_658_702)),
            MailboxFixture::new("已发送").with_attribute("\\Sent"),
        ],
        ..ServerConfig::default()
    };
    // 「IDLE 能不能宣告」只取决于能力集。`ServerConfig` 不为它加开关。
    config.capabilities.push("IDLE".to_string());
    config
}

/// 起服务器（用例结束时关掉）。
async fn start(config: ServerConfig) -> FakeImapServer {
    FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败")
}

/// 没进 SELECTED 就 `IDLE`，必须回 `NO`，**不能**回 `+ idling`。
///
/// 会失败的错误实现：无条件回 `+ idling`（不检查是否已选中文件夹），
/// 于是客户端会「进入」一个根本没有文件夹上下文的 IDLE——
/// 之后推送该按哪个文件夹匹配都无从谈起。
#[tokio::test]
async fn idle_before_select_is_rejected_with_no() {
    let server = start(config()).await;
    let mut raw = Raw::connect(&server).await;
    raw.login().await;

    raw.send("i1 IDLE").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("i1 NO"),
        "未选中文件夹就 IDLE 应当回 `NO`，实际是 {line:?}"
    );
    assert!(
        line.contains("not in selected state"),
        "`NO` 的理由不对：{line:?}"
    );
    assert!(
        !line.starts_with('+'),
        "未选中文件夹时**不能**回续行：{line:?}"
    );

    server.shutdown().await;
}

/// 回 `+ idling`；发 `DONE` 之后收到的是用**原始 tag** 的
/// `OK IDLE terminated`。
///
/// 会失败的错误实现：把 `DONE` 当作 tag 回 `DONE OK`，
/// 或者回 untagged 的 `* OK` —— 客户端会一直等那条带 tag 的收尾响应。
#[tokio::test]
async fn idle_returns_continuation_then_answers_done_with_the_original_tag() {
    let server = start(config()).await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;

    raw.send("i1 IDLE").await;
    let continuation = raw.read_line().await;
    assert_eq!(
        continuation, "+ idling",
        "IDLE 应当回裸的续行 `+ idling`，实际是 {continuation:?}"
    );

    raw.send("DONE").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("i1 "),
        "`DONE` 的收尾响应用**进入 IDLE 时那条命令**的 tag，实际是 {line:?}"
    );
    assert!(
        line.starts_with("i1 OK"),
        "`DONE` 之后应当回 `OK IDLE terminated`，实际是 {line:?}"
    );
    assert!(
        line.contains("IDLE terminated"),
        "收尾响应文本不对：{line:?}"
    );

    server.shutdown().await;
}

/// `push_exists` 要送到**正卡在 IDLE 里**的那个连接，而且推完**仍在 IDLE**。
///
/// 会失败的错误实现：
/// * 推完就把连接踢出 IDLE（下面那句 `DONE` 的 `OK` 就收不到了）；
/// * 把推送攒进「命令队列」，等 IDLE 结束了才发（下面读不到 `* 7 EXISTS`）。
#[tokio::test]
async fn push_exists_reaches_the_idling_connection() {
    let server = start(config()).await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;

    raw.send("i1 IDLE").await;
    assert_eq!(raw.read_line().await, "+ idling");

    // 客户端此刻正阻塞在 IDLE 里（连 `DONE` 都还没发），推一条通知过去。
    server.push_exists("INBOX", 7);
    raw.expect_contains("* 7 EXISTS").await;

    // 还在 IDLE 里：`DONE` 仍然有效。
    raw.send("DONE").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("i1 OK IDLE terminated"),
        "推送之后应当仍留在 IDLE 里，实际收到 {line:?}"
    );

    server.shutdown().await;
}

/// `push_expunge` 要送出一条 `* n EXPUNGE`。
///
/// 会失败的错误实现：只认 `EXISTS` 一种推送（`EXPUNGE` 被丢掉或当成
/// `EXISTS` 发出去），客户端就永远删不掉自己本地的序号。
#[tokio::test]
async fn push_expunge_reaches_the_idling_connection() {
    let server = start(config()).await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;

    raw.send("i1 IDLE").await;
    assert_eq!(raw.read_line().await, "+ idling");

    // 序号（不是 UID）：假服务器只是把给定数字原样发出去。
    server.push_expunge("INBOX", 1);
    raw.expect_contains("* 1 EXPUNGE").await;

    raw.send("DONE").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("i1 OK IDLE terminated"),
        "推送 EXPUNGE 之后应当仍留在 IDLE 里，实际收到 {line:?}"
    );

    server.shutdown().await;
}

/// IDLE 期间发别的命令，回 `BAD IDLE in progress`（用那条命令自己的 tag）。
///
/// 会失败的错误实现：把 IDLE 期间的命令当普通命令处理
/// （`NOOP` 直接被回 `OK NOOP completed`）——同一 socket 就出现了
/// 两个「拥有者」，客户端会以为自己可以边 IDLE 边发命令。
#[tokio::test]
async fn a_command_during_idle_is_rejected_with_bad() {
    let server = start(config()).await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;

    raw.send("i1 IDLE").await;
    assert_eq!(raw.read_line().await, "+ idling");

    raw.send("n1 NOOP").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("n1 "),
        "`BAD` 应当带**那条命令**的 tag，实际是 {line:?}"
    );
    assert!(
        line.contains("BAD IDLE in progress"),
        "IDLE 期间的其他命令应当回 `BAD IDLE in progress`，实际是 {line:?}"
    );

    // 拒掉之后仍留在 IDLE：`DONE` 才是唯一的出口。
    raw.send("DONE").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("i1 OK IDLE terminated"),
        "`BAD` 之后应当仍留在 IDLE 里，实际收到 {line:?}"
    );

    server.shutdown().await;
}

/// 离开 IDLE 之后，推送**不能**再落到这个连接上。
///
/// 会失败的错误实现：离开 IDLE 时忘记注销登记 —— 推送会被写进这个
/// 已经回到普通状态的连接，客户端会在 `NOOP` 附近读到一条莫名其妙的
/// `* 9 EXISTS`；登记表也会无界增长。
#[tokio::test]
async fn push_does_not_leak_to_a_connection_that_left_idle() {
    let server = start(config()).await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;

    raw.send("i1 IDLE").await;
    assert_eq!(raw.read_line().await, "+ idling");

    // 先正常收场。读到 `OK IDLE terminated` 说明服务器那边已经处理完
    // `DONE`（实现里是**先摘登记、再回 OK**，所以这里不会有竞态）。
    raw.send("DONE").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("i1 OK IDLE terminated"),
        "收尾响应不对：{line:?}"
    );

    // 现在再推：这个连接已经不在 IDLE 了，什么也不该收到。
    server.push_exists("INBOX", 9);

    raw.send("n1 NOOP").await;
    loop {
        let line = raw.read_line().await;
        assert!(
            !line.contains("EXISTS"),
            "推送泄漏到了已经离开 IDLE 的连接上：{line:?}"
        );
        if line.starts_with("n1 ") {
            assert!(
                line.contains("OK NOOP completed"),
                "NOOP 响应不对：{line:?}"
            );
            break;
        }
    }

    server.shutdown().await;
}

/// 三条与 `IDLE` 相关的故障注入确实对 `IDLE` 生效。
///
/// `IDLE` 没有子命令，而 `command_fault` 对 `UID FETCH` 这类是取**子命令名**
/// 匹配的——所以这里实测一遍「名字匹配规则走得通」。
///
/// 会失败的错误实现：故障匹配只认「命令 + 子命令」两级，
/// `IDLE` 这种没有子命令的命令永远匹配不上。
#[tokio::test]
async fn faults_still_hit_the_idle_command() {
    // 1) 宣告了 IDLE，但服务器拒绝（回 `NO`，不给续行）。
    let server = start(ServerConfig {
        faults: vec![Fault::Reject("IDLE".to_string())],
        ..config()
    })
    .await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;
    raw.send("i1 IDLE").await;
    let line = raw.read_line().await;
    assert!(
        line.starts_with("i1 NO"),
        "`Fault::Reject(\"IDLE\")` 应当回 `NO`，实际是 {line:?}"
    );
    assert!(!line.contains("+ idling"), "被拒绝时不能出现续行：{line:?}");
    server.shutdown().await;

    // 2) 挂住：一条响应都不发（客户端只能超时）。
    let server = start(ServerConfig {
        faults: vec![Fault::HangOn("IDLE".to_string())],
        ..config()
    })
    .await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;
    raw.send("i1 IDLE").await;
    let line = raw.read_line_or_none(Duration::from_millis(300)).await;
    assert!(
        line.is_none(),
        "`Fault::HangOn(\"IDLE\")` 不该有任何响应，实际收到 {line:?}"
    );
    server.shutdown().await;

    // 3) 直接掐断：连接被关掉，一个字节都不回。
    let server = start(ServerConfig {
        faults: vec![Fault::DropOn("IDLE".to_string())],
        ..config()
    })
    .await;
    let mut raw = Raw::connect(&server).await;
    raw.login_and_select().await;
    raw.send("i1 IDLE").await;
    let line = raw.read_line_or_none(READ_TIMEOUT).await;
    assert!(
        line.is_none(),
        "`Fault::DropOn(\"IDLE\")` 应当直接断开，实际收到 {line:?}"
    );
    server.shutdown().await;
}
