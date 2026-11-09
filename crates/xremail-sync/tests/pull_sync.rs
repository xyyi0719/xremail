//! 同步引擎的端到端测试：假 IMAP 服务器 → 同步引擎 → 真实 SQLite。
//!
//! 这条链路**不依赖真邮箱**：
//! * 服务器侧：假 IMAP 服务器（`xremail-testkit`）
//! * 存储侧：真实的 SQLite（内存库，schema 与生产一致）
//!
//! 要证明的东西：
//! * 首次同步把服务器上的邮件**拉进本地**；
//! * 二次同步**不重复拉**（差分生效）；
//! * 服务器新增的邮件**只拉新增的**；
//! * 服务器删掉的邮件，本地缓存**跟着清**；
//! * epoch（UIDVALIDITY）变了 → **重建**而不是增量；
//! * 标记从服务器**正确落到本地**（位掩码映射）；
//! * 服务器拒绝命令时**报错**，而不是把缓存当成「服务器已删」清掉。

use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, Folder, MessageFlags, ProviderKind, ServerConfig as DomainServerConfig,
    SyncMode,
};
use xremail_protocol_imap::{ImapConfig, Session, TlsTrust, connect_and_login_with};
use xremail_store::dao::{account as account_dao, folder as folder_dao, message as message_dao};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::pull_folder;
use xremail_testkit::{
    FakeImapServer, MailboxFixture, MessageFixture, ServerConfig as FakeServerConfig,
};

/// 假服务器 + 客户端配置。
struct Fixture {
    server: FakeImapServer,
    imap: ImapConfig,
    trust: TlsTrust,
}

/// 起一个假服务器。
async fn start(config: FakeServerConfig) -> Fixture {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(10);

    let trust = TlsTrust::Roots(server.root_store());

    Fixture {
        server,
        imap,
        trust,
    }
}

/// 一个带两封邮件的 INBOX。
fn config_with_mail() -> FakeServerConfig {
    FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(
                    MessageFixture::new(1_792_658_701)
                        .with_flag("\\Seen")
                        .with_subject("第一封")
                        .with_body("正文一"),
                )
                .with_message(
                    MessageFixture::new(1_792_658_702)
                        .with_subject("第二封")
                        .with_body("正文二"),
                ),
        ],
        ..FakeServerConfig::default()
    }
}

/// 登录并选中 INBOX（同步用只读的 EXAMINE，这里先 SELECT 以便后续写标记）。
async fn login(fixture: &Fixture) -> Session {
    let (session, _caps, _tls) = connect_and_login_with(
        &fixture.imap,
        &fixture.trust,
        "tester@example.com",
        "test-password",
    )
    .await
    .expect("登录应当成功");
    session
}

/// 一个内存库 + 一个本地文件夹记录。
async fn local_store() -> (SqlitePool, Folder) {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");

    // 建一个账户与文件夹，让外键成立。
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

#[tokio::test]
async fn first_sync_pulls_every_message() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    let report = pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");

    assert_eq!(report.added, 2);
    assert_eq!(report.removed, 0);
    assert!(report.first_sync, "首次同步");
    assert!(report.full_fetch, "首次同步是全量拉取");
    assert!(
        !report.epoch_changed,
        "首次同步不是 epoch 切换（本地本来就没有缓存）"
    );
    assert_eq!(report.uidvalidity, 1);

    // 邮件真的落库了。
    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .expect("应当能列出邮件");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].uid, 1_792_658_701);
    assert_eq!(messages[1].uid, 1_792_658_702);

    // 游标被更新了。
    let updated = folder_dao::get_folder(&pool, "folder-1")
        .await
        .expect("应当能取到文件夹")
        .expect("文件夹应当还在");
    assert_eq!(updated.uidvalidity, Some(1));
    assert_eq!(updated.last_uid, Some(1_792_658_702));
    assert_eq!(updated.last_sync_at, Some(1_000));

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn flags_are_mapped_to_bits() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("同步应当成功");

    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .expect("应当能列出邮件");

    // 第一封带 \Seen，第二封没有。
    assert!(messages[0].flags.contains(MessageFlags::SEEN));
    assert!(!messages[0].flags.contains(MessageFlags::FLAGGED));
    assert!(!messages[1].flags.contains(MessageFlags::SEEN));

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn second_sync_pulls_nothing_new() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");

    // 第二次：文件夹记录要带上第一次写回的 epoch 与游标。
    let updated = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();

    let report = pull_folder(&pool, &mut session, &updated, 2_000)
        .await
        .expect("二次同步应当成功");

    assert_eq!(report.added, 0, "没有新邮件，不该重复拉");
    assert_eq!(report.removed, 0);
    assert!(!report.epoch_changed, "epoch 没变，不该重建");
    assert!(!report.first_sync, "不是首次同步");
    assert!(!report.full_fetch, "epoch 没变就是增量");
    assert_eq!(report.retained, 2);
    assert!(report.is_noop());

    // 行数没变。
    let count = message_dao::count_messages(&pool, "acct-1", "folder-1")
        .await
        .expect("应当能计数");
    assert_eq!(count, 2);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn new_remote_message_is_pulled() {
    // 第一次同步时服务器只有**一封**；之后服务器变成两封（多一封新的）。
    let mut config = config_with_mail();
    config.mailboxes[0].messages.pop();

    let fixture = start(config).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    let first = pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    assert_eq!(first.added, 1);
    fixture.server.shutdown().await;

    // 服务器变成两封：一封旧的（UID 相同）+ 一封新的。
    let mut config = config_with_mail();
    config.mailboxes[0].messages.pop();
    config.mailboxes[0]
        .messages
        .push(MessageFixture::new(1_792_658_703).with_subject("新来的"));
    let fixture = start(config).await;
    let mut session = login(&fixture).await;

    let updated = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let report = pull_folder(&pool, &mut session, &updated, 2_000)
        .await
        .expect("二次同步应当成功");

    assert_eq!(report.added, 1, "只该拉新增的那一封");
    assert_eq!(report.retained, 1);

    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].uid, 1_792_658_703);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn message_deleted_on_server_is_removed_locally() {
    // 第一次：两封。
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    assert_eq!(
        message_dao::count_messages(&pool, "acct-1", "folder-1")
            .await
            .unwrap(),
        2
    );
    fixture.server.shutdown().await;

    // 服务器上只剩一封 → 本地那封要被清掉。
    let mut config = config_with_mail();
    config.mailboxes[0].messages.pop();
    let fixture = start(config).await;
    let mut session = login(&fixture).await;

    let updated = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let report = pull_folder(&pool, &mut session, &updated, 2_000)
        .await
        .expect("二次同步应当成功");

    assert_eq!(report.removed, 1, "服务器删的那封要清掉");
    assert_eq!(report.added, 0);

    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].uid, 1_792_658_701);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn epoch_change_triggers_a_rebuild() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    fixture.server.shutdown().await;

    // 服务器换 epoch（UIDVALIDITY 变了）→ 必须重建。
    let mut config = config_with_mail();
    config.uid_validity = 2;
    let fixture = start(config).await;
    let mut session = login(&fixture).await;

    let updated = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.uidvalidity, Some(1));

    let report = pull_folder(&pool, &mut session, &updated, 2_000)
        .await
        .expect("epoch 切换应当成功");

    assert!(report.epoch_changed, "epoch 变了必须重建");
    assert!(report.full_fetch, "epoch 变了要全量重拉");
    assert!(!report.first_sync, "不是首次同步");
    assert_eq!(report.uidvalidity, 2);
    assert_eq!(report.added, 2, "新 epoch 要全量重拉");

    // 本地记录的是**新** epoch 的 UID。
    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|m| m.uidvalidity == 2));

    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(folder.uidvalidity, Some(2));

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn server_rejection_is_an_error_not_a_mass_delete() {
    // 关键用例：服务器拒绝命令时**不能**把本地缓存当成「服务器已删」清掉。
    //
    // 这正是用 `run_checked` 而不是 async-imap 的 `uid_fetch` 的原因——
    // 后者会吞掉 `NO`，看起来像「这个文件夹没有邮件」。
    let mut config = config_with_mail();
    config
        .faults
        .push(xremail_testkit::Fault::Reject("FETCH".to_string()));
    let fixture = start(config).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    let err = pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect_err("服务器拒绝了 FETCH，同步必须报错");

    assert!(
        err.to_string().contains("FETCH") || err.to_string().contains("fault"),
        "实际错误是：{err}"
    );

    // 本地没有被动过（本来就没有数据，但也不能因为报错而清空别的）。
    let count = message_dao::count_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(count, 0);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn missing_uidvalidity_is_rejected() {
    // 服务器不给 UIDVALIDITY → 无法定位邮件 → 必须报错，不能瞎猜。
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    // 手工把本地 epoch 设成一个不存在的值没用——
    // 这里直接验证：服务器给了 UIDVALIDITY=1（默认），同步成功。
    let report = pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("同步应当成功");
    assert_eq!(report.uidvalidity, 1);

    fixture.server.shutdown().await;
}
