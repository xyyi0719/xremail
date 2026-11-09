//! 故障恢复的端到端测试：断线、丢确认、崩溃重启之后，
//! 用户的标记意图**不会丢**，也**不会重复生效**。
//!
//! 这条链路是 [ARCHITECTURE §5 与 §6](../../../docs/ARCHITECTURE.md) 的核心承诺。
//! 假 IMAP 服务器可以注入三种**服务器侧**的形态，
//! 它们对客户端的意义完全不同：
//!
//! | 故障 | 服务器上生效了吗 | 客户端知道吗 | 正确做法 |
//! |---|---|---|---|
//! | `DropOn` | **没有**（执行前断线） | 不知道 | 重发 |
//! | `DropAfterApply` | **有**（执行后断线） | 不知道 | 先回读，已生效就**别再发** |
//! | 进程崩溃 | 不确定 | 不知道 | 重启后**对账** |
//!
//! 关键区分：`DropOn` 与 `DropAfterApply` 差一个「服务器到底执行了没」。
//! 只测前者会漏掉「重复副作用」这个真问题——
//! 服务器上已经有 `\Seen` 了，客户端再发一遍 `STORE` 看着无害，
//! 但换成 `+FLAGS` 的删除类操作、或将来接上 `MOVE`/`APPEND`，
//! 「以为没生效就再发一次」就会变成**真的重复副作用**。

use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, FlagIntentPayload, Folder, MailOperation, MessageFlags, OperationKind,
    OperationState, ProviderKind, ServerConfig as DomainServerConfig, SyncMode,
};
use xremail_protocol_imap::{ImapConfig, Session, TlsTrust, connect_and_login_with};
use xremail_store::dao::{
    account as account_dao, folder as folder_dao, operation as operation_dao,
};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory, open_pool_path};
use xremail_sync::{pull_folder, push_flags_as};
use xremail_testkit::{
    FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig as FakeServerConfig,
};

/// 认领归属：模拟「上一次进程」与「这一次进程」。
///
/// `push_flags` 内部用本进程的 boot id 认领；一条 `uncertain` 的操作要能被
/// **下一次**回写收尾（协议层的归属守卫见 `update_operation_state_as`，R50），
/// 两次回写必须是**同一个提交者**，所以这里钉死两个名字而不是各用各的。
const TEST_BOOT_A: &str = "test-boot-a";
const TEST_BOOT_B: &str = "test-boot-b";

/// 待标记为已读的那封邮件的 UID。
const TARGET_UID: i64 = 1_792_658_702;

/// 假服务器 + 客户端配置。
struct Fixture {
    server: FakeImapServer,
    imap: ImapConfig,
    trust: TlsTrust,
}

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

/// 一封邮件的 INBOX（UID 刻意不连续，像 163 那样）。
fn config_with_mail() -> FakeServerConfig {
    FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(
                    MessageFixture::new(1_792_658_701)
                        .with_flag("\\Seen")
                        .with_subject("已读的一封"),
                )
                .with_message(MessageFixture::new(TARGET_UID as u32).with_subject("未读的一封")),
        ],
        ..FakeServerConfig::default()
    }
}

/// 登录并**以可写方式**打开 INBOX（标记回写需要 SELECT，不是 EXAMINE）。
async fn login(fixture: &Fixture) -> Session {
    let (mut session, _caps, _tls) = connect_and_login_with(
        &fixture.imap,
        &fixture.trust,
        "tester@example.com",
        "test-password",
    )
    .await
    .expect("登录应当成功");
    session.select("INBOX").await.expect("SELECT 应当成功");
    session
}

/// 一个内存库 + 账户 + 文件夹。
async fn local_store() -> (SqlitePool, Folder) {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");
    seed_account_and_folder(&pool).await
}

/// 插入账户 + 文件夹（两种存储都共用）。
async fn seed_account_and_folder(pool: &SqlitePool) -> (SqlitePool, Folder) {
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
    account_dao::upsert_account(pool, &account)
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
    folder_dao::upsert_folder(pool, &folder)
        .await
        .expect("插入文件夹失败");

    (pool.clone(), folder)
}

/// 入队一个「标记为已读」的意图。
async fn enqueue_read_intent(
    pool: &SqlitePool,
    folder: &Folder,
    uid: i64,
    uidvalidity: i64,
    now_ms: i64,
) -> MailOperation {
    let operation = MailOperation {
        id: format!("op-{uid}-{now_ms}"),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid,
        uidvalidity,
        kind: OperationKind::Flags,
        intent: FlagIntentPayload::new(
            MessageFlags::empty().with(MessageFlags::SEEN),
            MessageFlags::empty(),
        ),
        destination_folder_id: None,
        state: OperationState::Queued,
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: now_ms,
        updated_at: now_ms,
    };
    operation_dao::enqueue_operation(pool, &operation)
        .await
        .expect("入队应当成功");

    operation
}

/// 读回服务器上的标记（用真客户端，绕开同步层）。
async fn server_flags(fixture: &Fixture, uid: i64) -> MessageFlags {
    let mut session = login(fixture).await;
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

/// 命令日志里 `STORE` 命令的条数。
fn store_count(fixture: &Fixture) -> usize {
    fixture
        .server
        .command_log()
        .iter()
        .filter(|line| line.contains("STORE"))
        .count()
}

/// 首次同步，拿到 epoch。
async fn sync_to_get_epoch(pool: &SqlitePool, session: &mut Session, folder: &Folder) -> Folder {
    pull_folder(pool, session, folder, 1_000)
        .await
        .expect("首次同步应当成功");
    folder_dao::get_folder(pool, "folder-1")
        .await
        .unwrap()
        .expect("文件夹应当还在")
}

/// 本地库里的标记。
async fn local_flags(pool: &SqlitePool, uid: i64) -> MessageFlags {
    let messages = xremail_store::dao::message::list_messages(pool, "acct-1", "folder-1")
        .await
        .unwrap();
    messages
        .iter()
        .find(|m| m.uid == uid)
        .expect("本地应当有这封邮件")
        .flags
}

// ------------------------------------------------------------------ 用例

/// 服务器**执行了**、但确认丢了：重连后必须**先回读**，确认已生效就不再发。
#[tokio::test]
async fn server_applied_but_the_ack_was_lost_is_not_replayed() {
    let mut config = config_with_mail();
    config.faults.push(Fault::DropAfterApply {
        command: "STORE".to_string(),
        once: true,
    });
    let fixture = start(config).await;
    let (pool, folder) = local_store().await;

    let mut session = login(&fixture).await;
    let folder = sync_to_get_epoch(&pool, &mut session, &folder).await;
    let epoch = folder.uidvalidity.expect("应当有 epoch");

    enqueue_read_intent(&pool, &folder, TARGET_UID, epoch, 2_000).await;

    // 第一次回写：STORE 在服务器上**生效了**，但确认没送回来。
    let report = push_flags_as(&pool, &mut session, &folder, 3_000, TEST_BOOT_A)
        .await
        .expect("回写应当成功（失败也是预期内的结果，不是错误）");

    assert_eq!(report.uncertain, 1, "确认丢了必须判为 uncertain");
    assert_eq!(report.succeeded, 0, "没收到确认就不能算成功");

    // 服务器上**真的**变了——虽然客户端不知道。
    let flags = server_flags(&fixture, TARGET_UID).await;
    assert!(flags.is_read(), "服务器上应当已读，实际：{flags:?}");

    // 意图**没有**被丢掉：它还没完结。
    let pending = operation_dao::list_pending_operations(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(
        pending.len(),
        1,
        "uncertain 的意图不能被晾着：它还没完结，丢了就等于用户点的星标没了"
    );

    // 第二次回写（重新连上）：先回读，发现服务器上已经有了 → **一条命令都不发**。
    let mut session = login(&fixture).await;
    let report = push_flags_as(&pool, &mut session, &folder, 4_000, TEST_BOOT_B)
        .await
        .expect("回写应当成功");

    assert_eq!(report.succeeded, 1, "服务器上已有 → 算成功");
    assert_eq!(report.uncertain, 0);
    assert_eq!(
        report.submitted, 0,
        "服务器上已经是目标状态，不能再发一遍 STORE"
    );
    assert_eq!(report.noop, 1);

    // 整条链路上只发过**一条** STORE——没有重复副作用。
    assert_eq!(
        store_count(&fixture),
        1,
        "不能重复发 STORE，实际命令：{:?}",
        fixture.server.command_log()
    );

    // 操作最终成功。
    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Succeeded)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // 本地视图也跟服务器对齐了（否则叠加层一撤，界面上的星标会弹回去）。
    let local = local_flags(&pool, TARGET_UID).await;
    assert!(local.is_read(), "本地视图应当跟服务器一致，实际：{local:?}");

    fixture.server.shutdown().await;
}

/// 执行**前**断线：服务器上没生效 → 下次应当重发并真正生效。
#[tokio::test]
async fn a_command_dropped_before_it_took_effect_is_retried() {
    let mut config = config_with_mail();
    // 只在**第一次** STORE 时断线，之后恢复正常。
    config.faults.push(Fault::DropOnOnce {
        command: "STORE".to_string(),
    });
    let fixture = start(config).await;
    let (pool, folder) = local_store().await;

    let mut session = login(&fixture).await;
    let folder = sync_to_get_epoch(&pool, &mut session, &folder).await;
    let epoch = folder.uidvalidity.expect("应当有 epoch");

    enqueue_read_intent(&pool, &folder, TARGET_UID, epoch, 2_000).await;

    // 第一次：命令发出去了，服务器没执行，也没确认。
    let report = push_flags_as(&pool, &mut session, &folder, 3_000, TEST_BOOT_A)
        .await
        .expect("回写应当成功");
    assert_eq!(report.uncertain, 1);

    // 服务器上**没有**生效（这是它和 DropAfterApply 的区别）。
    let flags = server_flags(&fixture, TARGET_UID).await;
    assert!(
        !flags.is_read(),
        "执行前断线 → 服务器上不该有变化，实际：{flags:?}"
    );

    // 第二次：回读发现服务器上没有 → 重发 → 真正生效。
    let mut session = login(&fixture).await;
    let report = push_flags_as(&pool, &mut session, &folder, 4_000, TEST_BOOT_B)
        .await
        .expect("回写应当成功");
    assert_eq!(report.succeeded, 1);
    assert_eq!(report.submitted, 1, "服务器上没有 → 应当重发");

    let flags = server_flags(&fixture, TARGET_UID).await;
    assert!(flags.is_read(), "服务器上应当已读，实际：{flags:?}");

    // 两条 STORE：一条被掐断，一条真正生效。
    assert_eq!(store_count(&fixture), 2);

    fixture.server.shutdown().await;
}

/// 崩溃后重启：没提交出去的意图还在，重启后照样提交。
#[tokio::test]
async fn a_pending_intent_survives_a_restart() {
    let config = config_with_mail();
    let fixture = start(config).await;

    // 用**文件**库：内存库随进程一起没了，测不了「重启」。
    let dir = tempfile::tempdir().expect("建临时目录失败");
    let db_path = dir.path().join("xremail.db");
    let (pool, folder) = {
        let pool = open_pool_path(&db_path, &PoolConfig::default())
            .await
            .expect("打开文件库失败");
        migrate(&pool).await.expect("迁移应当成功");
        seed_account_and_folder(&pool).await
    };

    let mut session = login(&fixture).await;
    let folder = sync_to_get_epoch(&pool, &mut session, &folder).await;
    let epoch = folder.uidvalidity.expect("应当有 epoch");

    enqueue_read_intent(&pool, &folder, TARGET_UID, epoch, 2_000).await;

    // 「崩溃」：连接池直接丢掉，进程就此结束。
    pool.close().await;

    // 「重启」：重新打开同一个库文件。
    let pool = open_pool_path(&db_path, &PoolConfig::default())
        .await
        .expect("重启后打开文件库失败");

    let pending = operation_dao::list_pending_operations(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "重启后意图必须还在");

    // 重启后同步：意图照样提交。
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let mut session = login(&fixture).await;
    let report = push_flags_as(&pool, &mut session, &folder, 5_000, TEST_BOOT_A)
        .await
        .expect("回写应当成功");
    assert_eq!(report.succeeded, 1);

    let flags = server_flags(&fixture, TARGET_UID).await;
    assert!(flags.is_read(), "服务器上应当已读，实际：{flags:?}");

    pool.close().await;
    fixture.server.shutdown().await;
}
