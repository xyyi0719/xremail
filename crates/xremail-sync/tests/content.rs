//! 头与正文接进同步引擎的端到端测试。
//!
//! 验证：
//! * 拉取阶段落**头**（主题 / 发件人 / 日期 / Message-ID）；
//! * **正文按需拉**（`BODY.PEEK[]`），落进 `message_body` 表；
//! * `BODY.PEEK[]` **不置** `\Seen`；
//! * 非 UTF-8（GBK）正文能正确解码；
//! * 附件标记能落库。

use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, Folder, MessageFlags, ProviderKind, ServerConfig as DomainServerConfig,
    SyncMode,
};
use xremail_protocol_imap::{ImapConfig, Session, TlsTrust, connect_and_login_with};
use xremail_store::dao::{account as account_dao, folder as folder_dao, message as message_dao};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::{fetch_body, pull_folder, read_body};
use xremail_testkit::{
    FakeImapServer, MailboxFixture, MessageFixture, ServerConfig as FakeServerConfig,
};

/// “中文” 的 GBK 字节。
const GBK_ZHONG_WEN: [u8; 4] = [0xD6, 0xD0, 0xCE, 0xC4];

async fn start(config: FakeServerConfig) -> (FakeImapServer, ImapConfig, TlsTrust) {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(10);

    let trust = TlsTrust::Roots(server.root_store());

    (server, imap, trust)
}

/// 一封带真实内容的 INBOX。
fn config_with_mail() -> FakeServerConfig {
    FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(
                    MessageFixture::new(1_792_658_701)
                        .with_flag("\\Seen")
                        .with_subject("已读的一封")
                        .with_body("正文一"),
                )
                .with_message(
                    MessageFixture::new(1_792_658_702)
                        .with_subject("未读的一封")
                        .with_body("正文二"),
                ),
        ],
        ..FakeServerConfig::default()
    }
}

async fn login(fixture: &ImapConfig, trust: &TlsTrust) -> Session {
    let (session, _caps, _tls) =
        connect_and_login_with(fixture, trust, "tester@example.com", "test-password")
            .await
            .expect("登录应当成功");
    session
}

async fn local_store() -> (SqlitePool, Folder) {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");

    let account = Account {
        id: "acct-1".to_string(),
        email: "tester@example.com".to_string(),
        provider: ProviderKind::Generic,
        display_name: None,
        color: None,
        auth_kind: AuthKind::Password,
        server: DomainServerConfig::new("imap.example.com", 993, "smtp.example.com", 465),
        username: "tester@example.com".to_string(),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 0,
        updated_at: 0,
    };
    account_dao::upsert_account(&pool, &account)
        .await
        .expect("插入账户失败");

    let folder = Folder {
        id: "folder-1".to_string(),
        account_id: "acct-1".to_string(),
        remote_name: "INBOX".to_string(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".to_string()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        uidvalidity: None,
        role: None,
    };
    folder_dao::upsert_folder(&pool, &folder)
        .await
        .expect("插入文件夹失败");

    (pool, folder)
}

// ------------------------------------------------------------------ 用例

/// 读回**服务器**上的标记。
///
/// 必须查服务器而不是本地：本地标记只会被同步/回写链路更新，
/// 正文拉取本身不改它——查本地就看不出 `BODY[]` 与 `BODY.PEEK[]` 的区别。
async fn server_flags(imap: &ImapConfig, trust: &TlsTrust, uid: u32) -> MessageFlags {
    let mut session = login(imap, trust).await;
    // IMAP 状态机：FETCH 前必须选中邮箱。
    session.select("INBOX").await.expect("SELECT 应当成功");
    let command = format!("UID FETCH {uid} (UID FLAGS)");
    let outcome = xremail_protocol_imap::run_checked(&mut session, &command)
        .await
        .expect("FETCH 应当成功");

    match outcome {
        xremail_protocol_imap::CommandOutcome::Fetches(messages) => {
            MessageFlags::from_imap_names(&messages[0].flags)
        }
        _ => panic!("应当返回 FETCH 结果"),
    }
}

#[tokio::test]
async fn headers_are_stored_after_pull() {
    let (server, imap, trust) = start(config_with_mail()).await;
    let mut session = login(&imap, &trust).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");

    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);

    let first = messages
        .iter()
        .find(|m| m.uid == 1_792_658_701)
        .expect("应当有第一封");

    // 头里的东西都落库了。
    assert_eq!(first.subject.as_deref(), Some("已读的一封"));
    assert_eq!(first.from.len(), 1);
    assert_eq!(first.from[0].email, "sender@example.com");
    assert_eq!(first.to[0].email, "tester@example.com");
    assert!(first.date_sent.is_some(), "日期应当落库");
    assert_eq!(
        first.message_id.as_deref(),
        Some("fixture-1792658701@example.com")
    );

    server.shutdown().await;
}

#[tokio::test]
async fn body_is_fetched_on_demand() {
    let (server, imap, trust) = start(config_with_mail()).await;
    let mut session = login(&imap, &trust).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");

    // 拉取阶段**不**存正文（正文按需拉）。
    let count = message_dao::count_message_bodies(&pool, "acct-1")
        .await
        .unwrap();
    assert_eq!(count, 0, "拉取阶段不该拉正文");

    // 按需拉。
    let ok = fetch_body(
        &pool,
        &mut session,
        "acct-1",
        "folder-1",
        "INBOX",
        1_792_658_702,
        1,
    )
    .await
    .expect("拉正文应当成功");
    assert!(ok);

    let body = read_body(&pool, "acct-1", "folder-1", 1_792_658_702, 1)
        .await
        .expect("读正文应当成功")
        .expect("正文应当存在");
    assert_eq!(body.text_plain.as_deref(), Some("正文二"));

    server.shutdown().await;
}

#[tokio::test]
async fn body_fetch_does_not_set_seen() {
    // `BODY.PEEK[]` 不得置 `\Seen`——标记的变化走独立的回写链路。
    let (server, imap, trust) = start(config_with_mail()).await;
    let mut session = login(&imap, &trust).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");

    fetch_body(
        &pool,
        &mut session,
        "acct-1",
        "folder-1",
        "INBOX",
        1_792_658_702,
        1,
    )
    .await
    .expect("拉正文应当成功");

    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    let second = messages.iter().find(|m| m.uid == 1_792_658_702).unwrap();
    assert!(
        !second.flags.contains(MessageFlags::SEEN),
        "本地标记不该被正文拉取改动"
    );

    // **服务器**上的标记也不能变——查本地是看不出
    // `BODY[]` 与 `BODY.PEEK[]` 的区别的。
    let flags = server_flags(&imap, &trust, 1_792_658_702).await;
    assert!(
        !flags.contains(MessageFlags::SEEN),
        "BODY.PEEK 不该在服务器上置 \\Seen，实际标记：{:?}",
        flags
    );

    server.shutdown().await;
}

#[tokio::test]
async fn gbk_body_is_decoded_correctly() {
    // 一封**原始字节**是 GBK 的邮件：头是 ASCII，正文是 GBK 字节。
    let mut raw: Vec<u8> = Vec::new();
    raw.extend_from_slice(
        b"Subject: =?gb2312?B?1tDOxA==?=\r\n\
          From: sender@example.com\r\n\
          To: tester@example.com\r\n\
          Date: Mon, 01 Jan 2024 00:00:00 +0800\r\n\
          Content-Type: text/plain; charset=gbk\r\n\
          \r\n",
    );
    raw.extend_from_slice(&GBK_ZHONG_WEN);

    let config = FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(1_792_658_701).with_raw(raw)),
        ],
        ..FakeServerConfig::default()
    };

    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");

    // 主题是 RFC 2047 + gb2312 → 应当解出中文。
    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(messages[0].subject.as_deref(), Some("中文"));

    // 正文是 GBK 字节 → 也要解出中文。
    fetch_body(
        &pool,
        &mut session,
        "acct-1",
        "folder-1",
        "INBOX",
        1_792_658_701,
        1,
    )
    .await
    .expect("拉正文应当成功");

    let body = read_body(&pool, "acct-1", "folder-1", 1_792_658_701, 1)
        .await
        .unwrap()
        .expect("正文应当存在");
    assert_eq!(body.text_plain.as_deref(), Some("中文"));
    assert_eq!(body.charset.as_deref(), Some("gbk"));

    server.shutdown().await;
}

#[tokio::test]
async fn attachment_flag_is_stored() {
    // 一封带附件的邮件：拉正文后 `has_attachment` 应当为真。
    let raw = b"Subject: with attachment\r\n\
               From: sender@example.com\r\n\
               Content-Type: multipart/mixed; boundary=b\r\n\
               \r\n\
               --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
               see attachment\r\n\
               --b\r\nContent-Type: application/pdf\r\n\
               Content-Disposition: attachment; filename=a.pdf\r\n\r\n\
               %PDF-1.4\r\n\
               --b--\r\n";

    let config = FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(1_792_658_701).with_raw(raw.to_vec())),
        ],
        ..FakeServerConfig::default()
    };

    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    fetch_body(
        &pool,
        &mut session,
        "acct-1",
        "folder-1",
        "INBOX",
        1_792_658_701,
        1,
    )
    .await
    .expect("拉正文应当成功");

    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert!(messages[0].has_attachment, "有附件的邮件应当标记出来");

    server.shutdown().await;
}

#[tokio::test]
async fn fetching_body_of_unknown_message_is_false() {
    let (server, imap, trust) = start(config_with_mail()).await;
    let mut session = login(&imap, &trust).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");

    // 本地没有这封 → 不报错，只是「没有」。
    let ok = fetch_body(
        &pool,
        &mut session,
        "acct-1",
        "folder-1",
        "INBOX",
        999_999,
        1,
    )
    .await
    .expect("不应当报错");
    assert!(!ok);

    server.shutdown().await;
}

/// UID 是**邮箱作用域**的：拉正文前必须把会话切到那封邮件所在的文件夹。
///
/// 少了这一步，服务器会拿当前选中的邮箱去查那个 UID → 返回**空结果**，
/// 于是「拉到了 0 封」看起来像「服务器上没有这封邮件」（实测 2026-10-26，
/// 三家真实邮箱上只同步出 INBOX、正文一直拉不到）。
#[tokio::test]
async fn body_is_fetched_even_when_the_session_is_on_another_folder() {
    // 假服务器上放两个文件夹，INBOX 有一封带正文的邮件。
    let config = FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX").with_message(
                MessageFixture::new(1_792_658_701)
                    .with_subject("有正文的一封")
                    .with_body("正文一"),
            ),
            MailboxFixture::new("空文件夹"),
        ],
        ..FakeServerConfig::default()
    };
    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;

    let (pool, folder) = local_store().await;

    // 先同步 INBOX（会话停在 INBOX 上）。
    let report = pull_folder(&pool, &mut session, &folder, 0)
        .await
        .expect("同步应当成功");
    assert_eq!(report.added, 1);

    // 会话切到**另一个**文件夹。
    session
        .examine("空文件夹")
        .await
        .expect("应当能打开另一个文件夹");

    // 现在拉 INBOX 里那封的正文——必须成功。
    let ok = fetch_body(
        &pool,
        &mut session,
        "acct-1",
        "folder-1",
        "INBOX",
        1_792_658_701,
        1,
    )
    .await
    .expect("拉正文不应报错");
    assert!(ok, "应当拉到正文");

    let body = read_body(&pool, "acct-1", "folder-1", 1_792_658_701, 1)
        .await
        .expect("读正文不应报错")
        .expect("正文应当已落库");
    assert_eq!(body.text_plain.as_deref(), Some("正文一"));

    drop(server);
}
