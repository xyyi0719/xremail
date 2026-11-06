//! 账户级同步编排的端到端测试：假 IMAP 服务器 → 同步 → 真实 SQLite。
//!
//! 要证明：
//! * 服务器上的文件夹被同步成本地文件夹记录；
//! * 邮件落库，且**同一个文件夹第二次同步不会重复**；
//! * 服务器已删的邮件本地跟着清；
//! * 服务器拒绝 `LIST` 时**报错**，而不是得到一个空文件夹列表
//!   （那会让用户以为邮箱是空的）。

use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, MessageFlags, ProviderKind, ServerConfig, SyncMode, TlsMode,
};
use xremail_protocol_imap::{ImapConfig, Session, TlsTrust, connect_and_login_with};
use xremail_store::dao::{account as account_dao, folder as folder_dao, message as message_dao};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::sync_account;
use xremail_testkit::{
    FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig as FakeServerConfig,
};

/// 一个已配置的账户（凭据不进库，只在测试里用）。
fn test_account() -> Account {
    Account {
        id: "acct-1".to_string(),
        email: "tester@example.com".to_string(),
        provider: ProviderKind::Generic,
        display_name: None,
        color: None,
        auth_kind: AuthKind::Password,
        server: ServerConfig {
            imap_host: "localhost".to_string(),
            imap_port: 993,
            imap_tls: TlsMode::Implicit,
            smtp_host: "localhost".to_string(),
            smtp_port: 465,
            smtp_tls: TlsMode::Implicit,
            smtp_auth: xremail_core::model::SmtpAuth::Login,
        },
        username: "tester@example.com".to_string(),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 0,
        updated_at: 0,
    }
}

/// 起假服务器 + 登录。
async fn start(config: FakeServerConfig) -> (FakeImapServer, Session, TlsTrust) {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(10);

    let trust = TlsTrust::Roots(server.root_store());

    let (session, _caps, _tls) =
        connect_and_login_with(&imap, &trust, "tester@example.com", "test-password")
            .await
            .expect("登录应当成功");

    (server, session, trust)
}

/// 内存库 + 一个账户。
async fn local_store() -> (SqlitePool, Account) {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");

    let account = test_account();
    account_dao::upsert_account(&pool, &account)
        .await
        .expect("插入账户失败");

    (pool, account)
}

#[tokio::test]
async fn folders_and_messages_are_synced() {
    let config = FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX").with_message(
                MessageFixture::new(1_792_658_701)
                    .with_flag("\\Seen")
                    .with_subject("第一封"),
            ),
            MailboxFixture::new("已发送").with_attribute("\\Sent"),
        ],
        ..FakeServerConfig::default()
    };

    let (server, mut session, _trust) = start(config).await;
    let (pool, account) = local_store().await;

    let summary = sync_account(&pool, &mut session, &account, 1_000)
        .await
        .expect("同步应当成功");

    assert_eq!(summary.folders, 2, "两个文件夹都该同步");
    assert_eq!(summary.added, 1);

    // 本地文件夹记录。
    let folders = folder_dao::list_folders(&pool, "acct-1").await.unwrap();
    assert_eq!(folders.len(), 2);
    let inbox = folders
        .iter()
        .find(|f| f.remote_name == "INBOX")
        .expect("应当有 INBOX");
    assert_eq!(inbox.uidvalidity, Some(1));

    // 邮件落库了。
    let messages = message_dao::list_messages(&pool, "acct-1", &inbox.id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].subject.as_deref(), Some("第一封"));
    assert!(messages[0].flags.contains(MessageFlags::SEEN));

    server.shutdown().await;
}

#[tokio::test]
async fn second_sync_does_not_duplicate() {
    let config = FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(1_792_658_701).with_subject("一封")),
        ],
        ..FakeServerConfig::default()
    };

    let (server, mut session, _trust) = start(config).await;
    let (pool, account) = local_store().await;

    sync_account(&pool, &mut session, &account, 1_000)
        .await
        .expect("首次同步应当成功");

    let second = sync_account(&pool, &mut session, &account, 2_000)
        .await
        .expect("二次同步应当成功");

    assert_eq!(second.added, 0, "不该重复拉");
    assert_eq!(second.folders, 1);

    let folders = folder_dao::list_folders(&pool, "acct-1").await.unwrap();
    let messages = message_dao::list_messages(&pool, "acct-1", &folders[0].id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1, "同一个文件夹不该出现两行");

    server.shutdown().await;
}

#[tokio::test]
async fn folder_id_is_stable_across_syncs() {
    // 文件夹的本地 id 被 `message` 的外键指着，换 id 会让已缓存的邮件失去归属。
    let config = FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX").with_message(MessageFixture::new(1_792_658_701)),
        ],
        ..FakeServerConfig::default()
    };

    let (server, mut session, _trust) = start(config).await;
    let (pool, account) = local_store().await;

    sync_account(&pool, &mut session, &account, 1_000)
        .await
        .expect("首次同步应当成功");
    let first_id = folder_dao::list_folders(&pool, "acct-1").await.unwrap()[0]
        .id
        .clone();

    sync_account(&pool, &mut session, &account, 2_000)
        .await
        .expect("二次同步应当成功");
    let second_id = folder_dao::list_folders(&pool, "acct-1").await.unwrap()[0]
        .id
        .clone();

    assert_eq!(first_id, second_id, "文件夹的本地 id 必须稳定");

    server.shutdown().await;
}

#[tokio::test]
async fn server_rejecting_list_is_an_error_not_an_empty_list() {
    // 服务器拒绝 `LIST` 时**必须报错**。
    // 若当成「空列表」，用户会以为邮箱是空的，而真实原因是服务器拒绝了。
    let mut config = FakeServerConfig {
        mailboxes: vec![MailboxFixture::new("INBOX")],
        ..FakeServerConfig::default()
    };
    config.faults.push(Fault::Reject("LIST".to_string()));

    let (server, mut session, _trust) = start(config).await;
    let (pool, account) = local_store().await;

    let err = sync_account(&pool, &mut session, &account, 1_000)
        .await
        .expect_err("服务器拒绝了 LIST，同步必须报错");

    // B1：`LIST` 被 `NO` 拒是**语义**失败——重连一万次也一样失败，
    // 所以既不能变成空列表，也**不能**被当成「会话可恢复」去触发短重连。
    assert!(
        !xremail_sync::is_session_recoverable(&err),
        "服务器拒绝 LIST 是语义错误，不该被当成掉线：{err}"
    );
    // 文案本身由 `pull::map_protocol_error` 透传服务器给的细节（B1 之前是
    // 「列文件夹失败: …」的前缀）；这里钉住的是「细节没被丢掉」。
    assert!(
        err.to_string().contains("injected fault"),
        "实际错误是：{err}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn non_implicit_tls_account_is_rejected() {
    // 只支持隐式 TLS：不静默降级成明文。
    let (server, mut session, _trust) = start(FakeServerConfig::default()).await;
    let (pool, mut account) = local_store().await;

    account.server.imap_tls = TlsMode::None;

    let err = sync_account(&pool, &mut session, &account, 1_000)
        .await
        .expect_err("明文连接必须被拒绝");

    assert!(err.to_string().contains("隐式 TLS"), "实际错误是：{err}");

    server.shutdown().await;
}
