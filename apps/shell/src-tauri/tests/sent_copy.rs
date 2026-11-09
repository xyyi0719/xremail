//! 「发完确保已发送副本」的集成测试。
//!
//! 核心是**自适应**：服务器自己存了（163）就别补，服务器没存（QQ）就补一份。
//! 用例直接测 [`SentCopyService`]，因为 `Message-ID` 得由用例自己指定
//! ——走 `ComposeService::send` 的话它是构建时随机生成的，事先造不出来。
//!
//! 服务器是 `xremail-testkit` 里的假 IMAP 服务器；
//! 不依赖真实邮箱、凭据、网络或本机绝对路径
//! （见 [TESTING_STRATEGY §5.2](../../../../docs/TESTING_STRATEGY.md)）。

use std::time::{Duration, Instant};

use xremail_api::{NewAccountRequest, SecretValue};
use xremail_core::model::{Account, ProviderKind, SmtpAuth, SyncMode, TlsMode};
use xremail_protocol_imap::TlsTrust as ImapTlsTrust;
use xremail_shell_lib::service::account::AccountService;
use xremail_shell_lib::service::sent_copy::{SentCopyOutcome, SentCopyService};
use xremail_testkit::{FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig};

const USER: &str = "user@163.com";
const PASSWORD: &str = "imap-auth-code";

/// 在内存库里建一个指向假服务器的账户（两处套件共用）。
async fn account(imap: &FakeImapServer) -> Account {
    let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
        .await
        .expect("打开内存库失败");
    xremail_store::migrate(&pool).await.expect("应用迁移失败");

    AccountService::new(pool)
        .create(NewAccountRequest {
            email: USER.to_string(),
            provider: ProviderKind::Netease163,
            display_name: None,
            imap_host: imap.host().to_string(),
            imap_port: imap.port(),
            imap_tls: TlsMode::Implicit,
            smtp_host: "smtp.163.com".to_string(),
            smtp_port: 465,
            smtp_tls: TlsMode::Implicit,
            smtp_auth: SmtpAuth::Login,
            username: USER.to_string(),
            sync_mode: SyncMode::Poll,
            poll_interval_secs: 300,
        })
        .await
        .expect("建账户失败")
}

/// 一套「假 IMAP 服务器 + 账户 + 凭据」。
struct Fixture {
    imap: FakeImapServer,
    account: Account,
    credential: SecretValue,
}

impl Fixture {
    /// 造一套套件。
    ///
    /// `sent_messages` 是**事先放在「已发送」里的信**——
    /// 模拟「服务器自己存了一份」的情形。
    async fn new(sent_messages: Vec<MessageFixture>, with_sent_folder: bool) -> Self {
        let mut mailboxes = vec![MailboxFixture::new("INBOX")];
        if with_sent_folder {
            let mut sent = MailboxFixture::new("已发送").with_attribute("\\Sent");
            for message in sent_messages {
                sent = sent.with_message(message);
            }
            mailboxes.push(sent);
        }

        let imap = FakeImapServer::start(ServerConfig {
            user: USER.to_string(),
            password: PASSWORD.to_string(),
            mailboxes,
            ..ServerConfig::default()
        })
        .await
        .expect("假 IMAP 服务器启动失败");

        let account = account(&imap).await;

        Self {
            imap,
            account,
            credential: SecretValue::from_str_lossless(PASSWORD),
        }
    }

    /// 造一个信任假服务器自签 CA 的服务。
    /// 造一套**带故障注入**的套件（R46 的用例要服务器「收下命令但不回话」）。
    async fn with_faults(faults: Vec<Fault>) -> Self {
        let mut sent = MailboxFixture::new("已发送").with_attribute("\\Sent");
        sent = sent.with_message(message_with_id(1, "<already@example.com>"));

        let imap = FakeImapServer::start(ServerConfig {
            user: USER.to_string(),
            password: PASSWORD.to_string(),
            mailboxes: vec![MailboxFixture::new("INBOX"), sent],
            faults,
            ..ServerConfig::default()
        })
        .await
        .expect("假 IMAP 服务器启动失败");

        let account = account(&imap).await;
        let credential = SecretValue::from_str_lossless(PASSWORD);
        Self {
            imap,
            account,
            credential,
        }
    }

    fn service(&self) -> SentCopyService {
        // 等待时间设 0：这些用例要的是逻辑，不是等待。
        SentCopyService::new(ImapTlsTrust::Roots(self.imap.root_store())).with_settle(0)
    }

    /// 「已发送」文件夹里的信。
    fn sent_messages(&self) -> Vec<MessageFixture> {
        self.imap
            .mailboxes()
            .into_iter()
            .find(|mailbox| mailbox.name == "已发送")
            .map(|mailbox| mailbox.messages)
            .unwrap_or_default()
    }
}

/// 一封带指定 `Message-ID` 的信。
fn message_with_id(uid: u32, message_id: &str) -> MessageFixture {
    MessageFixture::new(uid).with_content(&format!("Message-ID: {message_id}\r\n\r\n正文\r\n"))
}

#[tokio::test]
async fn a_copy_is_appended_when_the_folder_does_not_have_the_message() {
    // QQ 那种服务器：自己不留副本 → 程序得补一份。
    let fixture = Fixture::new(Vec::new(), true).await;

    let outcome = fixture
        .service()
        .ensure_sent_copy(
            &fixture.account,
            &fixture.credential,
            "<new@example.com>",
            b"Message-ID: <new@example.com>\r\n\r\nbody\r\n",
        )
        .await
        .expect("存副本失败");

    assert_eq!(
        outcome,
        SentCopyOutcome::Appended { uid: Some(1) },
        "文件夹里没有这封信，就该补一份"
    );

    let messages = fixture.sent_messages();
    assert_eq!(messages.len(), 1, "「已发送」里应当正好一份");
    assert_eq!(
        messages[0].raw.as_deref(),
        Some(&b"Message-ID: <new@example.com>\r\n\r\nbody\r\n"[..]),
        "补的副本必须和发出去的那份一字不差"
    );
    assert!(
        messages[0].flags.iter().any(|flag| flag == "\\Seen"),
        "存进去的是已经发出去的信，不该算未读"
    );
}

#[tokio::test]
async fn no_copy_is_appended_when_the_server_already_kept_one() {
    // 163 那种服务器：自己存了一份 → 再补就变成两封。
    let fixture = Fixture::new(vec![message_with_id(7, "<kept@example.com>")], true).await;

    let outcome = fixture
        .service()
        .ensure_sent_copy(
            &fixture.account,
            &fixture.credential,
            "<KEPT@example.com>",
            b"Message-ID: <kept@example.com>\r\n\r\nbody\r\n",
        )
        .await
        .expect("检查已发送失败");

    assert_eq!(
        outcome,
        SentCopyOutcome::AlreadyPresent,
        "已经有副本了，不该再补"
    );
    assert_eq!(
        fixture.sent_messages().len(),
        1,
        "不能补成两份——已发送里多一封同样的信很难解释"
    );
}

#[tokio::test]
async fn a_missing_sent_folder_is_reported_instead_of_guessing_one() {
    // 没有「已发送」文件夹 → 明确告诉调用方，**不**自己挑一个文件夹乱存。
    let fixture = Fixture::new(Vec::new(), false).await;

    let outcome = fixture
        .service()
        .ensure_sent_copy(
            &fixture.account,
            &fixture.credential,
            "<new@example.com>",
            b"Message-ID: <new@example.com>\r\n\r\nbody\r\n",
        )
        .await
        .expect("检查已发送失败");

    assert_eq!(outcome, SentCopyOutcome::NoSentFolder);
}

#[tokio::test]
async fn a_missing_message_id_never_triggers_an_append() {
    // 没有 `Message-ID` 就没法判断文件夹里是不是已经有了这份，
    // 这时候**宁可不动**——补出第二份比缺一份更难解释。
    let fixture = Fixture::new(Vec::new(), true).await;

    let error = fixture
        .service()
        .ensure_sent_copy(&fixture.account, &fixture.credential, "", b"body\r\n")
        .await
        .expect_err("没有 Message-ID 就该明确报错，不能悄悄补一份");

    assert!(
        error.to_string().contains("Message-ID"),
        "报错要说清为什么不动：{error}"
    );
    assert!(fixture.sent_messages().is_empty(), "不该补出没头没尾的一份");
}

/// **R46**：服务器收下 `UID FETCH` 却不回话时，必须在超时内返回错误，
/// **不能永久挂住**。
///
/// 这条以前会挂死：`sent_copy` 里那两处 `run_checked` 是裸调用，
/// 而 `run_checked` 内部的读循环没有超时、async-imap 也没有命令超时。
/// 用户在写信面板上看到的就是永远的「正在发送」。
///
/// 不止断言「返回了错误」，还断言**它回得够快**——只断言错误的话，
/// 一个「等 30 秒再失败」的实现也会过，而那对用户和挂死差别不大。
#[tokio::test]
async fn a_server_that_never_answers_does_not_hang_the_sent_copy_check() {
    // 服务器收到 `EXAMINE`（「查已发送」的第一步）就不回话——半死连接的样子。
    //
    // 为什么不用 `HangOn("FETCH")`：假服务器**总是**在 `EXAMINE` 的
    // `[UIDNEXT]` 里给值，于是 `max_uid` 走的是捷径、根本不会发 `UID FETCH`，
    // 那条分支在这套夹具下**不可达**（我试过，用例照样绿，说明它没打到点上）。
    // 换 `EXAMINE` 才真正踩到「发出去没人应」。
    let fixture = Fixture::with_faults(vec![Fault::HangOn("EXAMINE".to_string())]).await;
    // 超时调短：用例要的是「有界」，不是等满 30 秒。
    let service = SentCopyService::new(ImapTlsTrust::Roots(fixture.imap.root_store()))
        .with_settle(0)
        .with_command_timeout(Duration::from_millis(300));

    let started = Instant::now();
    let result = service
        .ensure_sent_copy(
            &fixture.account,
            &fixture.credential,
            "<new@example.com>",
            b"Subject: hi\r\n\r\nbody\r\n",
        )
        .await;
    let elapsed = started.elapsed();

    let error = result.expect_err("服务器不回话时必须报错，不能当成成功");
    let text = error.to_string();
    assert!(
        text.contains("超时") || text.contains("timeout"),
        "错误里应当说清是超时：{text}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "必须在超时后立刻返回，实际用了 {elapsed:?}"
    );
}
