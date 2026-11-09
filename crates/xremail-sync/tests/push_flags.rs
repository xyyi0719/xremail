//! 标记回写链路的端到端测试：本地意图 → 队列 → 服务器 → **回读确认**。
//!
//! 这条链路是 [ARCHITECTURE §6](../../../docs/ARCHITECTURE.md) 的核心：
//!
//! ```text
//! 用户改标记 → 入队（意图）→ 读快照 → 重放意图 → 发 STORE → 回读确认 → 更新状态
//! ```
//!
//! 要证明的东西：
//! * 意图真的提交到了服务器，服务器上的标记**真的**变了；
//! * **回读确认**：不是「服务器回了 OK」就算成功；
//! * 服务器拒绝 → `retryable`，**不是** `succeeded`；
//! * 连接断了 → `uncertain`（不知道服务器执行了没，**不能盲目重试**）；
//! * epoch 变了 → 旧操作被**隔离**，不套用到新 UID 上；
//! * 净变化为零 → **不发命令**。

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
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::{pull_folder, push_flags};
use xremail_testkit::{
    FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig as FakeServerConfig,
};

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
                .with_message(MessageFixture::new(1_792_658_702).with_subject("未读的一封")),
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

/// 入队一个标记意图。
async fn enqueue_flag_intent(
    pool: &SqlitePool,
    folder: &Folder,
    uid: i64,
    uidvalidity: i64,
    add: MessageFlags,
    remove: MessageFlags,
    now_ms: i64,
) -> MailOperation {
    let operation = MailOperation {
        id: format!("op-{uid}-{now_ms}"),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid,
        uidvalidity,
        kind: OperationKind::Flags,
        intent: FlagIntentPayload::new(add, remove),
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

// ------------------------------------------------------------------ 用例

#[tokio::test]
async fn intent_is_applied_on_the_server_and_confirmed() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    // 先同步一次，拿到 epoch。
    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.expect("应当有 epoch");

    // 用户把第二封标记为已读。
    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_702,
        epoch,
        MessageFlags::empty().with(MessageFlags::SEEN),
        MessageFlags::empty(),
        2_000,
    )
    .await;

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写应当成功");

    assert_eq!(report.succeeded, 1);
    assert_eq!(report.retryable, 0);
    assert_eq!(report.uncertain, 0);
    assert_eq!(report.submitted, 1);

    // 服务器上**真的**变了。
    let flags = server_flags(&fixture, 1_792_658_702).await;
    assert!(flags.is_read(), "服务器上应当已读，实际：{flags:?}");

    // 本地也更新了。
    let messages = xremail_store::dao::message::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    let second = messages.iter().find(|m| m.uid == 1_792_658_702).unwrap();
    assert!(second.flags.is_read(), "本地应当已读");

    // 操作状态是 succeeded。
    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Succeeded)
            .await
            .unwrap();
    assert_eq!(count, 1);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn only_the_changed_bit_is_sent() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    // 只加 \Flagged。第一封已经有 \Seen——不该被清掉，也不该被重发。
    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_701,
        epoch,
        MessageFlags::empty().with(MessageFlags::FLAGGED),
        MessageFlags::empty(),
        2_000,
    )
    .await;

    push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写应当成功");

    let flags = server_flags(&fixture, 1_792_658_701).await;
    assert!(flags.is_starred(), "应当加星");
    assert!(flags.is_read(), "\\Seen 不该被动到");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn net_zero_change_sends_no_command() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    // 服务器上第一封**已经**是 \Seen 了 → 净变化为零 → 不该发命令。
    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_701,
        epoch,
        MessageFlags::empty().with(MessageFlags::SEEN),
        MessageFlags::empty(),
        2_000,
    )
    .await;

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写应当成功");

    assert_eq!(report.submitted, 0, "净变化为零不该发命令");
    assert_eq!(report.noop, 1);
    assert_eq!(report.succeeded, 1, "意图已经生效，算成功");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn server_rejection_is_retryable_not_succeeded() {
    // 服务器拒绝 STORE → 必须是 `retryable`，**不能**是 `succeeded`。
    let mut config = config_with_mail();
    config.faults.push(Fault::Reject("STORE".to_string()));
    let fixture = start(config).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_702,
        epoch,
        MessageFlags::empty().with(MessageFlags::SEEN),
        MessageFlags::empty(),
        2_000,
    )
    .await;

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.retryable, 1, "服务器拒绝 → 可重试");
    assert_eq!(report.succeeded, 0, "拒绝绝不能算成功");
    assert_eq!(report.uncertain, 0);

    // 操作状态是 retryable，尝试次数 +1。
    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Retryable)
            .await
            .unwrap();
    assert_eq!(count, 1);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn connection_loss_is_uncertain() {
    // 服务器在 STORE 之前断连 → **不知道**服务器执行了没 → `uncertain`。
    //
    // 这正是 `uncertain` 存在的理由：盲目重试可能重复执行。
    let mut config = config_with_mail();
    config.faults.push(Fault::DropOn("STORE".to_string()));
    let fixture = start(config).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_702,
        epoch,
        MessageFlags::empty().with(MessageFlags::SEEN),
        MessageFlags::empty(),
        2_000,
    )
    .await;

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.uncertain, 1, "连接故障 → 不知道服务器执行了没");
    assert_eq!(report.succeeded, 0);
    assert_eq!(report.retryable, 0);

    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Uncertain)
            .await
            .unwrap();
    assert_eq!(count, 1);

    // R26：`uncertain` 出口必须累加 `attempts`。
    //
    // 不累加的话，认领条件里的 `attempts < ?` 永远成立：每轮同步都会重新认领、
    // 重新发一次 `STORE`（对 `MOVE` 就是每轮搬一次），「最多试 N 次」形同虚设。
    // 这里直接读库里的 `attempts`：DAO 没有暴露读取单行计数的接口，
    // 而这条用例要钉的恰恰是**库里的数**。
    let attempts: i64 = sqlx::query_scalar("SELECT attempts FROM mail_operation LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 1, "一次「结果未知」就该算一次尝试");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn operations_bound_to_the_old_epoch_are_quarantined() {
    // epoch 变了 → 旧操作必须隔离，**不能**套用到新 UID 上。
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    // 入队一个绑定 epoch=1 的操作。
    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_702,
        epoch,
        MessageFlags::empty().with(MessageFlags::SEEN),
        MessageFlags::empty(),
        2_000,
    )
    .await;
    fixture.server.shutdown().await;

    // 服务器换 epoch。
    let mut config = config_with_mail();
    config.uid_validity = 2;
    let fixture = start(config).await;
    let mut session = login(&fixture).await;

    // 重新同步一次：本地 epoch 变成 2，旧的 epoch-1 操作就过期了。
    pull_folder(&pool, &mut session, &folder, 3_000)
        .await
        .expect("重新同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(folder.uidvalidity, Some(2));

    let report = push_flags(&pool, &mut session, &folder, 4_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.quarantined, 1, "旧 epoch 的操作必须被隔离");
    assert_eq!(report.submitted, 0, "隔离的操作不许发命令");
    assert_eq!(report.succeeded, 0);

    // 服务器上那封**不该**被改成已读。
    let flags = server_flags(&fixture, 1_792_658_702).await;
    assert!(!flags.is_read(), "隔离的操作绝不能作用到服务器上");

    let count = operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Failed)
        .await
        .unwrap();
    assert_eq!(count, 1);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn remove_flag_is_applied() {
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    // 取消第一封的已读。
    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_701,
        epoch,
        MessageFlags::empty(),
        MessageFlags::empty().with(MessageFlags::SEEN),
        2_000,
    )
    .await;

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写应当成功");

    assert_eq!(report.succeeded, 1);
    let flags = server_flags(&fixture, 1_792_658_701).await;
    assert!(!flags.is_read(), "\\Seen 应当被去掉");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn server_that_says_ok_but_does_not_apply_is_caught_by_read_back() {
    // 服务器回 OK 但**不真的改**标记。
    //
    // 这是**回读确认**存在的全部理由：
    // 只看「服务器回了 OK」会把没生效的改动当成成功。
    let mut config = config_with_mail();
    config.faults.push(Fault::IgnoreStore);
    let fixture = start(config).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_702,
        epoch,
        MessageFlags::empty().with(MessageFlags::SEEN),
        MessageFlags::empty(),
        2_000,
    )
    .await;

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.succeeded, 0, "回读发现没生效，就不能算成功");
    assert_eq!(report.retryable, 1, "回读不一致 → 可重试");
    assert_eq!(report.uncertain, 0);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn add_and_remove_in_one_intent_are_applied_together() {
    // 一个意图里既要加 \Flagged 又要清 \Seen → 必须发**两条**命令
    // （IMAP 的 STORE 一次只能做一种操作），且两条都要生效。
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .expect("首次同步应当成功");
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    enqueue_flag_intent(
        &pool,
        &folder,
        1_792_658_701,
        epoch,
        MessageFlags::empty().with(MessageFlags::FLAGGED),
        MessageFlags::empty().with(MessageFlags::SEEN),
        2_000,
    )
    .await;

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写应当成功");

    assert_eq!(report.succeeded, 1);
    assert_eq!(report.submitted, 2, "增删各一条命令");

    let flags = server_flags(&fixture, 1_792_658_701).await;
    assert!(flags.is_starred(), "\\Flagged 应当加上");
    assert!(!flags.is_read(), "\\Seen 应当去掉");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn contradictory_intent_is_rejected_at_enqueue() {
    let (pool, folder) = local_store().await;

    let operation = MailOperation {
        id: "op-bad".to_string(),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: 1,
        uidvalidity: 1,
        kind: OperationKind::Flags,
        intent: FlagIntentPayload::new(
            MessageFlags::empty().with(MessageFlags::SEEN),
            MessageFlags::empty().with(MessageFlags::SEEN),
        ),
        destination_folder_id: None,
        state: OperationState::Queued,
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 0,
        updated_at: 0,
    };

    let err = operation_dao::enqueue_operation(&pool, &operation)
        .await
        .expect_err("自相矛盾的意图必须被拒绝");

    assert!(err.requires_user_action());
}

#[tokio::test]
async fn move_intents_are_left_to_the_transfer_channel() {
    // 回归：`push_flags` 曾经把**移动/删除**意图也收进来算「净变化」。
    //
    // 它们的 flag 载荷是空的，于是「净变化为零」——被当成「已经在服务器上生效」
    // 直接标成 `Succeeded`。邮件既没搬走也没删掉，队列却空了：
    // 一次静默的数据丢失（用户以为搬了，实际上什么都没发生）。
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store().await;

    pull_folder(&pool, &mut session, &folder, 1_000)
        .await
        .unwrap();
    let folder = folder_dao::get_folder(&pool, "folder-1")
        .await
        .unwrap()
        .unwrap();
    let epoch = folder.uidvalidity.unwrap();

    let operation = MailOperation {
        id: "op-move".to_string(),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: 1_792_658_702,
        uidvalidity: epoch,
        kind: OperationKind::Move,
        intent: FlagIntentPayload::default(),
        destination_folder_id: Some("folder-archive".to_string()),
        state: OperationState::Queued,
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 2_000,
        updated_at: 2_000,
    };
    operation_dao::enqueue_operation(&pool, &operation)
        .await
        .expect("入队应当成功");

    let report = push_flags(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.succeeded, 0, "移动意图不该被标记回写当成成功");
    assert_eq!(report.noop, 0);

    // 它还躺着队列里，等 `push_transfers` 去提交。
    let stored = operation_dao::get_operation(&pool, "op-move")
        .await
        .unwrap()
        .expect("操作应当还在");
    assert_eq!(stored.state, OperationState::Queued, "不该被动过");

    fixture.server.shutdown().await;
}
