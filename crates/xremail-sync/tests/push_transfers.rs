//! 移动 / 删除回写链路的端到端测试：本地意图 → 队列 → 服务器 → **回读确认**。
//!
//! 这条链路是 [ARCHITECTURE §6](../../../docs/ARCHITECTURE.md) 的写路径：
//!
//! ```text
//! 用户移动/删除 → 入队（带 epoch 与目标）→ 选路由 → 发命令 → 回读确认 → 本地视图跟上
//! ```
//!
//! 要证明的东西：
//! * 邮件**真的**从源文件夹消失、**真的**出现在目标文件夹（不是「回了 OK」就算）；
//! * 有 `MOVE` 走 `MOVE`，没有就 `COPY` + 标删；没有 `UIDPLUS` 时**绝不** `EXPUNGE`；
//! * 删除优先**挪进垃圾箱**（可撤销），没有垃圾箱才就地删；
//! * 源文件夹里已经没有这封邮件 → 算成功（`MOVE` 不幂等，重发会多一份）；
//! * 断线 → `uncertain`（不知道服务器执行了没，**不能盲目重试**）；
//! * epoch 变了 → 旧操作被**隔离**，不套用到新 UID 上；
//! * 同步轮次会**自动排空**队列，不需要界面开着。

use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, FlagIntentPayload, Folder, MailOperation, OperationKind, OperationState,
    ProviderKind, ServerConfig as DomainServerConfig, SyncMode,
};
use xremail_protocol_imap::{
    CommandOutcome, ImapConfig, Session, TlsTrust, connect_and_login_with,
};
use xremail_store::dao::{
    account as account_dao, folder as folder_dao, message as message_dao,
    operation as operation_dao,
};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::{
    MAX_OPERATION_ATTEMPTS, TransferPushReport, pull_folder, push_transfers, sync_account,
};
use xremail_testkit::{
    FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig as FakeServerConfig,
};

const INBOX_UID: i64 = 1_792_658_702;
const OTHER_UID: i64 = 1_792_658_701;
/// 目标文件夹里那封「上一轮已经复制进去的副本」的 UID。
///
/// 与 `INBOX_UID` 不同：不同文件夹的 UID 空间是**各自独立**的。
const ARCHIVE_COPY_UID: i64 = 1_792_658_800;

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

/// INBOX 两封 + 一个 `Archive` 文件夹。
fn config_with_mail() -> FakeServerConfig {
    FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(
                    MessageFixture::new(OTHER_UID as u32)
                        .with_flag("\\Seen")
                        .with_subject("留着的一封"),
                )
                .with_message(MessageFixture::new(INBOX_UID as u32).with_subject("要被搬走的一封")),
            MailboxFixture::new("Archive"),
        ],
        ..FakeServerConfig::default()
    }
}

/// 同上，但 `Archive` 里**预先已经有一份副本**：`Message-ID` 与 INBOX 那封相同。
///
/// 这正是「上一轮 `COPY` 已经生效、但确认没读到」之后服务器上的样子。
/// 重发前必须先认出它，否则一份变两份。
fn config_with_copy_already_in_archive() -> FakeServerConfig {
    let mut config = config_with_mail();
    let archive = config
        .mailboxes
        .iter_mut()
        .find(|mailbox| mailbox.name == "Archive")
        .expect("config_with_mail 里应当有 Archive");

    // `Message-ID` 的拼法与 testkit 的默认邮件头一致（`<fixture-{uid}@example.com>`），
    // 但这里**故意让 UID 与 INBOX 那封不同**、`Message-ID` 与它相同——
    // 「同一个 `Message-ID` 出现在两个文件夹里」就是副本的样子。
    archive.messages.push(
        MessageFixture::new(ARCHIVE_COPY_UID as u32).with_content(&format!(
            "From: sender@example.com\r\n\
             To: tester@example.com\r\n\
             Subject: 上一轮已经复制进来的副本\r\n\
             Message-ID: <fixture-{INBOX_UID}@example.com>\r\n\
             \r\n\
             "
        )),
    );
    config
}

/// 同上，但被搬的那封**没有 `Message-ID`**。
///
/// RFC 5322 里 `Message-ID` 不是必填字段，现实里的信确实有缺的；
/// 缺了它就没法在目标文件夹里认出「这封是不是已经复制过去了」。
fn config_with_mail_without_message_id() -> FakeServerConfig {
    let mut config = config_with_mail();
    for mailbox in &mut config.mailboxes {
        for message in &mut mailbox.messages {
            if message.uid == INBOX_UID as u32 {
                // 换掉整封内容：默认头里那个 `Message-ID` 必须消失。
                message.content = "From: sender@example.com\r\n\
                     To: tester@example.com\r\n\
                     Subject: 没有 Message-ID 的一封\r\n\
                     \r\n\
                     正文"
                    .to_string();
            }
        }
    }
    config
}

/// 同上，但多一个垃圾箱（名字靠 `\Trash` 属性，路由靠名字推断）。
fn config_with_trash() -> FakeServerConfig {
    let mut config = config_with_mail();
    config
        .mailboxes
        .push(MailboxFixture::new("已删除").with_attribute("\\Trash"));
    config
}

/// 登录（不 SELECT，让调用方自己挑文件夹）。
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

/// 登录并**以可写方式**打开 INBOX。
async fn login_inbox(fixture: &Fixture) -> Session {
    let mut session = login(fixture).await;
    session.select("INBOX").await.expect("SELECT 应当成功");
    session
}

/// 内存库 + 账户 + INBOX（+ 可选的 `Archive` 本地记录）。
async fn local_store(with_archive: bool) -> (SqlitePool, Folder) {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");

    let now = 0;
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
        created_at: now,
        updated_at: now,
    };
    account_dao::upsert_account(&pool, &account)
        .await
        .expect("插入账户失败");

    let folder = make_folder("folder-1", "INBOX");
    folder_dao::upsert_folder(&pool, &folder)
        .await
        .expect("插入文件夹失败");

    if with_archive {
        folder_dao::upsert_folder(&pool, &make_folder("folder-archive", "Archive"))
            .await
            .expect("插入 Archive 失败");
    }

    (pool, folder)
}

fn make_folder(id: &str, remote_name: &str) -> Folder {
    Folder {
        id: id.to_string(),
        account_id: "acct-1".to_string(),
        remote_name: remote_name.to_string(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".to_string()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        uidvalidity: None,
    }
}

/// 入队一条移动/删除意图。
#[allow(clippy::too_many_arguments)]
async fn enqueue_transfer(
    pool: &SqlitePool,
    folder: &Folder,
    uid: i64,
    kind: OperationKind,
    destination_folder_id: Option<&str>,
    now_ms: i64,
) -> MailOperation {
    let operation = MailOperation {
        id: format!("op-{uid}-{now_ms}"),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid,
        uidvalidity: folder.uidvalidity.expect("测试里应当已经同步过"),
        kind,
        intent: FlagIntentPayload::default(),
        destination_folder_id: destination_folder_id.map(str::to_string),
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

/// 入队一条**已经试过 `attempts` 次**的移动意图（目标是 `folder-archive`）。
///
/// 重发路径靠 `attempts` 判断两件事：`> 0` 才去探测目标文件夹里有没有副本；
/// `< MAX_OPERATION_ATTEMPTS` 才能被 `claim_for_push` 认领。
/// 所以测试要能直接把这两个字段摆成想要的值（这是"上一轮已经发过命令"的入口）。
async fn enqueue_retried_move(
    pool: &SqlitePool,
    folder: &Folder,
    uid: i64,
    state: OperationState,
    attempts: i64,
    now_ms: i64,
) -> MailOperation {
    let operation = MailOperation {
        id: format!("op-{uid}-{now_ms}"),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid,
        uidvalidity: folder.uidvalidity.expect("测试里应当已经同步过"),
        kind: OperationKind::Move,
        intent: FlagIntentPayload::default(),
        destination_folder_id: Some("folder-archive".to_string()),
        state,
        seq: 0,
        base_version: None,
        attempts,
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

/// 读回一条操作的当前状态（`push_transfers` 只返回汇总报告）。
async fn reload_operation(pool: &SqlitePool, id: &str) -> MailOperation {
    operation_dao::get_operation(pool, id)
        .await
        .expect("读回操作失败")
        .expect("操作应当还在")
}

/// 服务器收到的 `UID COPY` 条目。
///
/// **直接证据**：这里是空的，就不可能有"多出一份"；
/// 日志里多一条，目标文件夹里就真会多一封（`COPY` 不幂等）。
fn copy_commands(fixture: &Fixture) -> Vec<String> {
    fixture
        .server
        .command_log()
        .into_iter()
        .filter(|line| line.starts_with("UID COPY"))
        .collect()
}

/// 服务器上某个文件夹里有没有这个 UID（用真客户端，绕开同步层）。
async fn server_has_uid(fixture: &Fixture, mailbox: &str, uid: u32) -> bool {
    let mut session = login(fixture).await;
    session.examine(mailbox).await.expect("EXAMINE 应当成功");

    let outcome =
        xremail_protocol_imap::run_checked(&mut session, &format!("UID FETCH {uid} (UID)"))
            .await
            .expect("FETCH 应当成功");

    match outcome {
        CommandOutcome::Fetches(messages) => !messages.is_empty(),
        _ => false,
    }
}

/// 服务器上某个文件夹里有多少封。
async fn server_count(fixture: &Fixture, mailbox: &str) -> usize {
    let mut session = login(fixture).await;
    session.examine(mailbox).await.expect("EXAMINE 应当成功");

    let outcome = xremail_protocol_imap::run_checked(&mut session, "UID FETCH 1:* (UID)")
        .await
        .expect("FETCH 应当成功");

    match outcome {
        CommandOutcome::Fetches(messages) => messages.len(),
        _ => 0,
    }
}

/// 服务器上这个 UID 的标记。
async fn server_flags(
    fixture: &Fixture,
    mailbox: &str,
    uid: u32,
) -> xremail_core::model::MessageFlags {
    let mut session = login(fixture).await;
    session.examine(mailbox).await.expect("EXAMINE 应当成功");

    let outcome =
        xremail_protocol_imap::run_checked(&mut session, &format!("UID FETCH {uid} (UID FLAGS)"))
            .await
            .expect("FETCH 应当成功");

    match outcome {
        CommandOutcome::Fetches(messages) => {
            xremail_core::model::MessageFlags::from_imap_names(&messages[0].flags)
        }
        _ => panic!("应当返回 FETCH 结果"),
    }
}

/// 同步一次拿到 epoch，并返回从库里读回的最新文件夹。
async fn pull_and_reload(
    pool: &SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
) -> Folder {
    pull_folder(pool, session, folder, now_ms)
        .await
        .expect("首次同步应当成功");

    folder_dao::get_folder(pool, &folder.id)
        .await
        .unwrap()
        .expect("文件夹应当还在")
}

// ---------------------------------------------------------------- 移动用例

#[tokio::test]
async fn move_without_move_capability_uses_copy_then_delete() {
    // 默认能力集有 UIDPLUS、**没有** MOVE —— 正是 163/QQ 那一档。
    let fixture = start(config_with_mail()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some("folder-archive"),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.moved, 1, "一封应当被搬走");
    assert_eq!(report.failed + report.retryable + report.uncertain, 0);

    // 服务器上真的搬了：INBOX 少一封，Archive 多一封。
    assert!(
        !server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await,
        "源文件夹里不该还有它"
    );
    assert_eq!(
        server_count(&fixture, "Archive").await,
        1,
        "目标文件夹里应当多出一封"
    );

    // 本地视图也跟上了。
    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert!(
        messages.iter().all(|m| m.uid != INBOX_UID),
        "本地源文件夹里也不该还有它"
    );

    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Succeeded)
            .await
            .unwrap();
    assert_eq!(count, 1, "操作应当标成成功");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn move_uses_the_move_command_when_the_server_supports_it() {
    let mut config = config_with_mail();
    config.capabilities.push("MOVE".to_string());
    let fixture = start(config).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some("folder-archive"),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.moved, 1);
    assert!(!server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await);
    assert_eq!(server_count(&fixture, "Archive").await, 1);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn missing_source_is_treated_as_already_moved() {
    // 邮件已经不在源文件夹里了（上一次尝试生效了，应答丢了）。
    // 再发一次 `MOVE` / `COPY` 就会多出一份 —— 所以必须当成功处理。
    let fixture = start(config_with_mail()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    // 一个服务器上根本不存在的 UID。
    enqueue_transfer(
        &pool,
        &folder,
        999,
        OperationKind::Move,
        Some("folder-archive"),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.moved, 1, "应当按已搬走处理");
    assert_eq!(report.noop, 1, "而且是一条命令都没发的那种");
    assert_eq!(
        server_count(&fixture, "Archive").await,
        0,
        "绝不能因此复制出一份"
    );

    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------- 删除用例

#[tokio::test]
async fn delete_goes_into_trash_when_there_is_one() {
    // 删除 = 进垃圾箱：可撤销的动作优先。
    let fixture = start(config_with_trash()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(false).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    // 垃圾箱要作为本地文件夹存在，路由才找得到它。
    let mut trash = make_folder("folder-trash", "已删除");
    trash.uidvalidity = Some(1);
    folder_dao::upsert_folder(&pool, &trash)
        .await
        .expect("插入垃圾箱失败");

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Delete,
        None,
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.deleted, 1);
    assert!(
        !server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await,
        "源文件夹里不该还有它"
    );
    assert_eq!(
        server_count(&fixture, "已删除").await,
        1,
        "应当进了垃圾箱，而不是被真删"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn delete_without_trash_or_uidplus_only_marks_deleted() {
    // 兜底那一档：没有垃圾箱，也没有 UIDPLUS（三家实测都有 UIDPLUS，这里人为关掉）。
    // 裸 `EXPUNGE` 会连带删掉别的客户端标了 `\Deleted` 的邮件，所以停在这一步。
    let mut config = config_with_mail();
    config.capabilities.retain(|cap| cap != "UIDPLUS");
    let fixture = start(config).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(false).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Delete,
        None,
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.deleted, 1);
    assert!(
        server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await,
        "没有 UIDPLUS 时不该真删掉"
    );
    let flags = server_flags(&fixture, "INBOX", INBOX_UID as u32).await;
    assert!(
        flags.contains(xremail_core::model::MessageFlags::DELETED),
        "应当带上 \\Deleted"
    );

    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------- 故障用例

#[tokio::test]
async fn connection_loss_during_move_is_uncertain() {
    // 命令发出去了，但没收到服务器确认 → **不知道**执行了没。
    //
    // 这正是 `uncertain` 存在的理由：盲目重试可能多搬一份。
    let mut config = config_with_mail();
    config.faults.push(Fault::DropOn("COPY".to_string()));
    let fixture = start(config).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some("folder-archive"),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.uncertain, 1, "连接故障 → 不知道服务器执行了没");
    assert_eq!(report.moved, 0);
    assert_eq!(report.retryable, 0);

    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Uncertain)
            .await
            .unwrap();
    assert_eq!(count, 1);

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn operations_bound_to_the_old_epoch_are_quarantined() {
    // 旧 epoch 的 UID 在新 epoch 下指向另一封信 —— 绝不能套用。
    let fixture = start(config_with_mail()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some("folder-archive"),
        2_000,
    )
    .await;

    // 模拟重新同步后 epoch 变了。
    let mut moved_on = folder.clone();
    moved_on.uidvalidity = Some(folder.uidvalidity.unwrap() + 1);

    let report = push_transfers(&pool, &mut session, &moved_on, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.quarantined, 1, "旧 epoch 的操作应当被隔离");
    assert_eq!(report.moved, 0);
    assert_eq!(
        server_count(&fixture, "Archive").await,
        0,
        "绝不能把旧 UID 套到新 epoch 上"
    );

    let count = operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Failed)
        .await
        .unwrap();
    assert_eq!(count, 1);

    fixture.server.shutdown().await;
}

// ------------------------------------------------ 重发幂等（163 没有 `MOVE`）
//
// 没有 `MOVE` 时移动 = `COPY` + 标删，而 `COPY` **不幂等**：
// 命令已经发出、确认没读到，下一轮重发就会在目标文件夹里多出一份。
// 下面三个用例锁的就是「重发前先认一下副本」和「次数到顶就不再认领」。

#[tokio::test]
async fn resending_copy_does_not_duplicate_when_target_already_has_the_message() {
    // 上一轮 `COPY` 已经生效，只是确认丢在路上。
    // 目标文件夹里预先放一封同 `Message-ID` 的信，就是那一刻的服务器状态。
    let fixture = start(config_with_copy_already_in_archive()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    let archive_before = server_count(&fixture, "Archive").await;

    let operation = enqueue_retried_move(
        &pool,
        &folder,
        INBOX_UID,
        OperationState::Retryable,
        1,
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.moved, 1, "探测到副本 → 补完删除那一半就算搬走");

    // 先证明探针真的跑了：日志里应当有那条按 `Message-ID` 的 SEARCH。
    // 这同时证明日志是按「命令名 + 参数」记的，所以下面「没有 COPY」看的确实是 COPY 本身。
    let log = fixture.server.command_log();
    assert!(
        log.iter()
            .any(|line| line.contains("SEARCH") && line.contains("Message-ID")),
        "重发前应当先按 Message-ID 探测目标文件夹，实际命令：{log:?}"
    );
    // **直接证据**：日志里一条 `UID COPY` 都没有，所以目标文件夹不可能多出副本。
    assert!(
        copy_commands(&fixture).is_empty(),
        "目标文件夹里已经有这封了，不该再发 COPY，实际命令：{log:?}"
    );
    // 代理指标：邮件条数没增加。它是用户能看见的东西，但它只是代理——
    // 真正的因果在上面那条（`COPY` 出现几次，副本就有几份）。
    assert_eq!(
        server_count(&fixture, "Archive").await,
        archive_before,
        "重发不该让目标文件夹里的信变多"
    );

    let operation = reload_operation(&pool, &operation.id).await;
    assert_eq!(
        operation.state,
        OperationState::Succeeded,
        "副本已经在，删除那一半补完就该收成成功"
    );

    let messages = message_dao::list_messages(&pool, "acct-1", "folder-1")
        .await
        .unwrap();
    assert!(
        messages.iter().all(|message| message.uid != INBOX_UID),
        "源文件夹里那封信也得从本地视图里消失"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn resending_is_refused_when_message_id_is_missing() {
    // 没有 `Message-ID` 就无法确认目标文件夹里有没有副本。
    // 赌一把直接重发 = 可能多出一份（`COPY` 不幂等）；
    // 所以宁可停住等对账 / 用户确认，也不拿用户的数据去赌。
    let fixture = start(config_with_mail_without_message_id()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    let archive_before = server_count(&fixture, "Archive").await;

    let operation = enqueue_retried_move(
        &pool,
        &folder,
        INBOX_UID,
        OperationState::Retryable,
        1,
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.uncertain, 1, "认不出副本 → 结果不明，等对账");
    assert_eq!(report.moved, 0, "没搬就算不上成功");

    // 同上：先证明日志在录，否则「没有 COPY」证明不了任何东西。
    let log = fixture.server.command_log();
    assert!(!log.is_empty(), "命令日志不该是空的");
    // **直接证据**：拒绝重发就是一条 `COPY` 都不能发。
    assert!(
        copy_commands(&fixture).is_empty(),
        "没有 Message-ID 时不能赌着重发 COPY，实际命令：{log:?}"
    );
    // 代理指标：目标文件夹条数不变。
    assert_eq!(
        server_count(&fixture, "Archive").await,
        archive_before,
        "不该有任何东西被复制进目标文件夹"
    );

    let operation = reload_operation(&pool, &operation.id).await;
    assert_eq!(
        operation.state,
        OperationState::Uncertain,
        "结果不明就得落成 uncertain，不能当成功也不能当失败"
    );
    // 这一条是"停止循环"的全部依据：`claim_for_push` 靠 `attempts < ?` 卡重发，
    // 收尾时不自增 `attempts`，这一轮拒绝重发就等于白拒绝——下一轮照样再发一次 `COPY`。
    assert_eq!(
        operation.attempts, 2,
        "uncertain 必须计次（原本 1 次，这一轮 +1）"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn exhausted_operation_is_not_reclaimed_again() {
    // 次数到顶 → `claim_for_push` 的 `AND attempts < ?` 不再放行。
    // 这一轮一条会改服务器的命令都不该发；收成 `failed / retry_exhausted`
    // 是对账（`reconcile`）的事，不归这里管，所以状态也**不动**。
    //
    // 防的是什么：把闸门放宽（或让 `uncertain` 不再计次）之后，
    // 一条老发不出去的 `copy+delete` 会每轮再 `COPY` 一次，永远停不下来。
    let fixture = start(config_with_copy_already_in_archive()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    let archive_before = server_count(&fixture, "Archive").await;

    let operation = enqueue_retried_move(
        &pool,
        &folder,
        INBOX_UID,
        OperationState::Uncertain,
        MAX_OPERATION_ATTEMPTS,
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(
        report,
        TransferPushReport::default(),
        "没认领到就什么也不算：既不算 moved，也不算 failed"
    );

    // 同上：先证明日志在录（拉取阶段发过命令），否则「没发命令」是空话。
    let log = fixture.server.command_log();
    assert!(!log.is_empty(), "命令日志不该是空的");
    // **直接证据**：日志里没有任何一条会改服务器的命令。
    let mutating: Vec<String> = log
        .into_iter()
        .filter(|line| {
            ["UID COPY", "UID MOVE", "UID STORE", "UID EXPUNGE"]
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
        .collect();
    assert!(
        mutating.is_empty(),
        "次数到顶就不该再向服务器发命令，实际命令：{mutating:?}"
    );
    // 代理指标：目标文件夹条数不变。
    assert_eq!(
        server_count(&fixture, "Archive").await,
        archive_before,
        "不该有任何东西被复制进目标文件夹"
    );

    let operation = reload_operation(&pool, &operation.id).await;
    assert_eq!(
        operation.state,
        OperationState::Uncertain,
        "没被认领 → 状态不该被改成 succeeded / failed"
    );
    assert_eq!(
        operation.attempts, MAX_OPERATION_ATTEMPTS,
        "一条命令都没发，就不该再记一次尝试"
    );

    fixture.server.shutdown().await;
}

// ------------------------------------------------------ 同步轮次自动排空队列

#[tokio::test]
async fn sync_account_drains_queued_moves_without_the_ui_open() {
    // 用户在离线时点了移动（命令连不上，只入队）；下次同步自动提交。
    let fixture = start(config_with_mail()).await;
    let mut session = login(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some("folder-archive"),
        2_000,
    )
    .await;

    // 一次完整的账户同步（拉取 + 排空队列）。
    let account = account_dao::get_account(&pool, "acct-1")
        .await
        .unwrap()
        .expect("账户应当还在");
    sync_account(&pool, &mut session, &account, 3_000)
        .await
        .expect("同步应当成功");

    assert!(
        !server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await,
        "同步轮次应当把排队的移动提交掉"
    );
    assert_eq!(server_count(&fixture, "Archive").await, 1);

    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Succeeded)
            .await
            .unwrap();
    assert_eq!(count, 1, "操作应当被标成成功");

    fixture.server.shutdown().await;
}
