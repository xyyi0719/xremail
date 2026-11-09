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
        role: None,
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

/// 库里唯一一条操作的状态与错误说明。
async fn operation_state(pool: &SqlitePool) -> (String, Option<String>) {
    use sqlx::Row;

    let row = sqlx::query("SELECT state, error_message FROM mail_operation LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("应当有一条操作");
    (row.get("state"), row.get("error_message"))
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

    assert_eq!(report.noop, 1, "一条命令都没发就该只算 noop");
    assert_eq!(report.moved, 0, "没搬走的信不能算作「已移动」（R36）");
    assert_eq!(report.deleted, 0);
    assert_eq!(report.total(), 1, "七个计数互斥，合计等于处理条数");
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
async fn delete_without_trash_or_uidplus_is_failed_instead_of_faked() {
    // R35：没有垃圾箱、也没有 UIDPLUS 时，能做的最多是「标删」——那封信
    // **还在原地**。裸 `EXPUNGE` 会连带删掉别的客户端标了 `\Deleted` 的邮件，
    // 所以不能用它收尾；把「标了个标记」报成「已删除」是假话。
    // 现在这条路由直接拒绝，操作记 `Failed`，由人决定。
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

    assert_eq!(report.deleted, 0, "信没走，不能算删掉了");
    assert_eq!(report.failed, 1, "应当明确报失败");
    assert!(
        server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await,
        "没有 UIDPLUS 时不该真删掉"
    );
    let flags = server_flags(&fixture, "INBOX", INBOX_UID as u32).await;
    assert!(
        !flags.contains(xremail_core::model::MessageFlags::DELETED),
        "既然不算成功，就一条命令都不该发（不该留下去不掉的 \\Deleted）"
    );

    let (state, message) = operation_state(&pool).await;
    assert_eq!(state, "failed");
    assert!(
        message.unwrap_or_default().contains("UIDPLUS"),
        "失败原因要说清是服务器缺能力，否则用户看不懂为什么删不掉"
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
    // R26 的教训是「拒绝重发却不计次 = 白拒绝」，所以「这一轮**没有**命令飞出去」
    // 与「这一轮**可能**飞出去了」必须分开看：
    //
    // * 这里查不到 `Message-ID`，`transfer_push` 于是**拒绝发任何命令**
    //   （连 `UID SEARCH` 都不发，那条拒绝有 `copy_commands` 的直接证据）。
    //   这一轮既没有 `COPY` 也没有任何别的写给服务器的命令，**不是一次尝试**，
    //   所以 `attempts` 不动——计次只该落在「真可能执行过」的出口上。
    //   以前这里 +1，代价是：一条「上一轮已经搬完、源端本地行被删掉」的操作
    //   每轮同步都烧一次配额，五次之后被判成 `retry_exhausted`（见
    //   `tests/transfer_recovery.rs`）。
    // * 真发过 `COPY`、只是探测答不上来的那条路**照旧计次**，由
    //   `tests/transfer_recovery.rs` 的
    //   `an_unreachable_probe_after_a_real_copy_still_counts_as_an_attempt`
    //   反证——那一条要是也免计次，R26 的病（每轮重发一份 `COPY`）就回来了。
    assert_eq!(
        operation.attempts, 1,
        "这一轮一条命令都没发（没有任何要补充的疑点），不该算作一次尝试"
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

// ---------------------------------------------------- 垃圾箱判定：role 优先，名字兜底

/// 同上，但垃圾箱**只在属性上叫 `\Trash`**，名字完全不像垃圾箱（`Rubbish`）。
///
/// 这正是「靠文件夹名猜」猜不出来的那一类：R04 的残余。
fn config_with_trash_named_rubbish() -> FakeServerConfig {
    let mut config = config_with_mail();
    config
        .mailboxes
        .push(MailboxFixture::new("Rubbish").with_attribute("\\Trash"));
    config
}

/// 同上，但那个「可能是垃圾箱」的文件夹**不宣告任何属性，名字也不像垃圾箱**。
///
/// 名字不在 `role_by_name` 的兜底表里（表里是 `Deleted Items` / `已删除` 之类），
/// 所以两条线索都不存在。
///
/// 用 ASCII 名字是为了让断言能直接比对线路上的原样：假服务器把非 ASCII 名字
/// 编码成 modified UTF-7 再发（`回收站` → `&Vt5lNnrZ-`），本地库的 `remote_name`
/// 存的也是这个编码形式。这本身是对的（发命令必须用线路原样），只是测试里
/// 拿 ASCII 名字读起来更清楚。
fn config_without_any_trash_hint() -> FakeServerConfig {
    let mut config = config_with_mail();
    config.mailboxes.push(MailboxFixture::new("Maybe Bin"));
    config
}

/// 同步一轮，让 `LIST` 的结论落进本地 `folder` 表。
async fn sync_once(fixture: &Fixture, pool: &SqlitePool) {
    let mut session = login(fixture).await;
    let account = account_dao::get_account(pool, "acct-1")
        .await
        .unwrap()
        .expect("账户应当还在");
    sync_account(pool, &mut session, &account, 1_000)
        .await
        .expect("同步应当成功");
}

/// 本地库里 `remote_name` 对应的文件夹（同步之后才存在）。
async fn local_folder(pool: &SqlitePool, remote_name: &str) -> Folder {
    folder_dao::list_folders(pool, "acct-1")
        .await
        .expect("列出文件夹失败")
        .into_iter()
        .find(|f| f.remote_name == remote_name)
        .unwrap_or_else(|| panic!("本地库里应当有 {remote_name}"))
}

/// 服务器收到的、把信搬往 `mailbox` 的命令有几条。
///
/// 假服务器默认**没有** `MOVE` 能力（163/QQ 那一档），所以「搬过去」实际是
/// `UID COPY <uid> "<mailbox>"` + 标删 + `UID EXPUNGE`（见模块文档的路由表）。
/// 有 `MOVE` 时则是 `UID MOVE`。两种都算「搬」。
fn moves_into(fixture: &Fixture, mailbox: &str) -> usize {
    fixture
        .server
        .command_log()
        .into_iter()
        .filter(|line| {
            (line.starts_with("UID MOVE") || line.starts_with("UID COPY")) && line.contains(mailbox)
        })
        .count()
}

#[tokio::test]
async fn sync_persists_the_trash_role_even_when_the_name_looks_nothing_like_trash() {
    // 承诺的第一半：`LIST` 宣告了 `\Trash`，本地库就要**记住**它——
    // 名字叫 `Rubbish` 也一样。这一步不做，下面那条删除用例就无从谈起。
    let fixture = start(config_with_trash_named_rubbish()).await;
    let (pool, _folder) = local_store(false).await;

    sync_once(&fixture, &pool).await;

    let rubbish = local_folder(&pool, "Rubbish").await;
    assert_eq!(
        rubbish.role.as_deref(),
        Some("trash"),
        "服务器宣告的 \\Trash 必须落进 folder.role（R04 残余的根因）"
    );
    assert!(rubbish.is_trash());

    let inbox = local_folder(&pool, "INBOX").await;
    assert_eq!(
        inbox.role.as_deref(),
        Some("inbox"),
        "INBOX 的角色也要落库（名字兜底那条路径）"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn delete_goes_into_a_trash_the_name_would_never_reveal() {
    // **这次改动的核心承诺**：服务器宣告 `\Trash`，但文件夹叫 `Rubbish`。
    // 按名字猜会猜不出来（于是就地标删、不可撤销）；按库里的 role 就必须挪进去。
    let fixture = start(config_with_trash_named_rubbish()).await;
    let (pool, _folder) = local_store(false).await;

    // 先同步一轮：INBOX 与 Rubbish 都进库，Rubbish 的 role 由 `LIST` 宣告。
    sync_once(&fixture, &pool).await;
    let inbox = local_folder(&pool, "INBOX").await;
    assert_eq!(inbox.role.as_deref(), Some("inbox"));

    let mut session = login_inbox(&fixture).await;
    let inbox = pull_and_reload(&pool, &mut session, &inbox, 1_500).await;

    enqueue_transfer(&pool, &inbox, INBOX_UID, OperationKind::Delete, None, 2_000).await;

    let report = push_transfers(&pool, &mut session, &inbox, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.deleted, 1);
    assert!(
        !server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await,
        "源文件夹里不该还有它"
    );
    assert_eq!(
        server_count(&fixture, "Rubbish").await,
        1,
        "服务器宣告了 \\Trash，删除就必须进这个文件夹——名字像不像垃圾箱不算数"
    );
    assert_eq!(
        moves_into(&fixture, "Rubbish"),
        1,
        "应当搬进 Rubbish（COPY 或 MOVE），而不是就地 EXPUNGE"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn delete_stays_in_place_when_no_folder_claims_the_trash_role() {
    // 承诺的第二半：既没有属性、名字也不像（`Maybe Bin` 不在兜底表里），
    // 就**不能**乱猜——退回「就地标删」，而不是挪进某个碰巧像垃圾箱的文件夹。
    let fixture = start(config_without_any_trash_hint()).await;
    let (pool, _folder) = local_store(false).await;

    sync_once(&fixture, &pool).await;
    let inbox = local_folder(&pool, "INBOX").await;
    let recycle = local_folder(&pool, "Maybe Bin").await;
    assert_eq!(
        recycle.role, None,
        "没宣告、名字也认不出 → role 是 NULL（「不知道」），不是「不是垃圾箱」"
    );

    let mut session = login_inbox(&fixture).await;
    let inbox = pull_and_reload(&pool, &mut session, &inbox, 1_500).await;

    enqueue_transfer(&pool, &inbox, INBOX_UID, OperationKind::Delete, None, 2_000).await;

    let report = push_transfers(&pool, &mut session, &inbox, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.deleted, 1);
    assert!(
        !server_has_uid(&fixture, "INBOX", INBOX_UID as u32).await,
        "就地标删：源文件夹里应当没有它了"
    );
    assert_eq!(
        server_count(&fixture, "Maybe Bin").await,
        0,
        "两条线索都没有时，绝不能把信挪进一个名字碰巧像垃圾箱的文件夹"
    );
    assert_eq!(
        moves_into(&fixture, "Maybe Bin"),
        0,
        "应当一条搬运命令都没有"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn an_unknown_role_falls_back_to_the_name_instead_of_losing_the_trash() {
    // **降级顺序**：库里的 role 为 NULL（升级上来的老行、或这一轮没认出来）时，
    // 才退回按名字猜。这条用例保证「升级之后、还没同步过」的账户不会突然
    // 从「删除 = 进垃圾箱」退化成「就地标删」——对用户来说那就是删没了。
    let fixture = start(config_with_trash()).await;
    let (pool, folder) = local_store(false).await;
    let folder = {
        let mut session = login_inbox(&fixture).await;
        pull_and_reload(&pool, &mut session, &folder, 1_000).await
    };

    // 手工塞一个 role 为 NULL 的垃圾箱：模拟 0010 之前就落库的老行，
    // 它的名字在兜底表里（`已删除`）。
    let mut trash = make_folder("folder-trash", "已删除");
    trash.uidvalidity = Some(1);
    assert_eq!(trash.role, None, "老行的 role 就是 NULL");
    folder_dao::upsert_folder(&pool, &trash)
        .await
        .expect("插入垃圾箱失败");

    let mut session = login_inbox(&fixture).await;
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
    assert_eq!(
        server_count(&fixture, "已删除").await,
        1,
        "role 未知时必须退回名字兜底，否则老账户的删除会变成不可撤销的"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn a_known_non_trash_role_beats_a_trash_looking_name() {
    // 两条线索**冲突**时以宣告为准：服务器说这个文件夹是 `\Archive`，
    // 哪怕它叫 `Trash`，也不该被当成垃圾箱——那会把信挪进一个用户没指望的地方。
    let mut config = config_with_mail();
    config
        .mailboxes
        .push(MailboxFixture::new("Trash").with_attribute("\\Archive"));
    let fixture = start(config).await;
    let (pool, folder) = local_store(false).await;
    let folder = {
        let mut session = login_inbox(&fixture).await;
        pull_and_reload(&pool, &mut session, &folder, 1_000).await
    };

    sync_once(&fixture, &pool).await;
    let archive = local_folder(&pool, "Trash").await;
    assert_eq!(archive.role.as_deref(), Some("archive"));

    let mut session = login_inbox(&fixture).await;
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
    assert_eq!(
        server_count(&fixture, "Trash").await,
        0,
        "服务器说它是 \\Archive，名字再像垃圾箱也不许去"
    );
    assert_eq!(moves_into(&fixture, "Trash"), 0);

    fixture.server.shutdown().await;
}

// -------------------------------------- 目标文件夹的新 UID 写回本地（R39）
//
// 移动成功后，目标文件夹的本地行要**当场**补上，而不是等下一轮全量同步。
// 不补的话，那封信在一段时间里哪也找不到（源端已删、目标端未知），
// 还可能被重复下载。
//
// 难点全在「凭什么知道目标侧的那个 UID 是真的」：
// * `COPYUID` 给目标 UID，但不给目标文件夹的 UIDVALIDITY（RFC 4315 §3）；
// * 本地 `message` 的身份是 `(account_id, folder_id, uidvalidity, uid)`，
//   没有 epoch 就没地方放它；
// * 目标文件夹可能刚被服务器重建过（epoch 变了），这时 `COPYUID` 说的
//   到底是哪个世代，两边都不确定。
//
// 所以下面的用例分两类：**拿得准的就必须写**（MOVE / COPY 两条路由），
// **拿不准的一律不写**（拿不到 epoch、epoch 对不上），而且不写时不能留下坏数据。

/// 被测的两个文件夹在本地叫什么。
///
/// 假服务器的 UIDVALIDITY 是**服务器全局**的（每个文件夹都一样），所以这里
/// 不能靠「两个 epoch 不同」来证明「写回用的是目标的 epoch」；但可以证明
/// 写回用的就是服务器**真的**给出的那个值，且与库里目标文件夹记着的一致。
const INBOX_LOCAL: &str = "folder-1";
const ARCHIVE_LOCAL: &str = "folder-archive";

/// 目标文件夹里「那封被挪过去的信」在本地的那一行。
///
/// 按**远端身份**（epoch + UID）找，不按 `id`：写回不该沿用源端的 `id`，
/// 所以测试也不能拿 `id` 去认它（那样会把「搬了 id」这种错实现当成对的）。
async fn local_row_in_archive(
    pool: &SqlitePool,
    archive_epoch: i64,
    archive_uid: i64,
) -> Option<xremail_core::model::Message> {
    message_dao::list_messages(pool, "acct-1", ARCHIVE_LOCAL)
        .await
        .expect("列出目标文件夹应当成功")
        .into_iter()
        .find(|message| message.uid == archive_uid && message.uidvalidity == archive_epoch)
}

/// 服务器上 `Archive` 文件夹此刻的内容（快照）。
fn server_archive(fixture: &Fixture) -> MailboxFixture {
    fixture
        .server
        .mailboxes()
        .into_iter()
        .find(|mailbox| mailbox.name == "Archive")
        .expect("服务器上应当有 Archive")
}

/// 让本地的 `Archive` 文件夹像**已经完整同步过一轮**：epoch 与库里一致。
///
/// 写回的门槛之一是「`EXAMINE` 拿到的目标 epoch == 库里记着的那个」，
/// 所以要先让库里那个值是真的。直接写库而不是跑一轮拉取：这一组用例验的是
/// 写回，不是拉取（拉取的效果由最后一组「再跑一轮不重复」的用例负责）。
async fn seed_archive_epoch(pool: &SqlitePool, epoch: i64) {
    let mut archive = make_folder(ARCHIVE_LOCAL, "Archive");
    archive.uidvalidity = Some(epoch);
    folder_dao::upsert_folder(pool, &archive)
        .await
        .expect("插入 Archive 失败");
}

#[tokio::test]
async fn move_writes_the_destination_row_back_with_the_servers_own_uid() {
    // 有 `MOVE` 的服务器：一步 `UID MOVE`，`COPYUID` 带目标侧的新 UID。
    let fixture = start(config_with_move()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;
    let archive_epoch = i64::from(fixture.server.uid_validity());
    seed_archive_epoch(&pool, archive_epoch).await;

    // 源端那封在本地的那一行：写回**不能**沿用它的 id。
    let source_row = message_dao::list_messages(&pool, "acct-1", INBOX_LOCAL)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.uid == INBOX_UID)
        .expect("源端那封应当在本地");

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some(ARCHIVE_LOCAL),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.moved, 1);
    assert_eq!(
        report.not_written_back, 0,
        "有 COPYUID + 拿得到 epoch，就该写回"
    );

    // 服务器「实际值」：直接看它的活状态，不看客户端说了什么。
    let archive = server_archive(&fixture);
    assert_eq!(archive.messages.len(), 1, "服务器上应当多出一封");
    let destination_uid = i64::from(archive.messages[0].uid);

    // 本地目标文件夹里**立刻**有它，且 UID / epoch 都是服务器给的值。
    let archive_rows = message_dao::list_messages(&pool, "acct-1", ARCHIVE_LOCAL)
        .await
        .unwrap();
    let Some(row) = archive_rows
        .iter()
        .find(|message| message.uid == destination_uid && message.uidvalidity == archive_epoch)
    else {
        panic!(
            "移动成功后目标文件夹本地应当已经有它（epoch {archive_epoch}，uid {destination_uid}），实际：{:?}",
            archive_rows
                .iter()
                .map(|m| (m.uid, m.uidvalidity))
                .collect::<Vec<_>>()
        );
    };
    assert_eq!(
        row.subject.as_deref(),
        Some("要被搬走的一封"),
        "内容要跟着搬"
    );
    assert_ne!(
        row.id, source_row.id,
        "目标端是新的一行，不是把源端那行的 id 搬过来"
    );

    // 源端本地行照旧删掉（原有行为不变）。
    let inbox_rows = message_dao::list_messages(&pool, "acct-1", INBOX_LOCAL)
        .await
        .unwrap();
    assert!(inbox_rows.iter().all(|message| message.uid != INBOX_UID));
    assert_eq!(inbox_rows.len(), 1, "INBOX 里还剩没被搬的那封");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn copy_route_also_writes_the_destination_row_back() {
    // 163 形态：没有 `MOVE`，`COPY` + 标删 + `UID EXPUNGE`。
    // `COPY` 同样回 `COPYUID`，目标侧的新 UID 一样要接住——这是最容易漏的一半：
    // 「只有 MOVE 那条路修了」在 163 用户那里等于没修。
    let fixture = start(config_with_mail()).await;
    let mut session = login_inbox(&fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;
    let archive_epoch = i64::from(fixture.server.uid_validity());
    seed_archive_epoch(&pool, archive_epoch).await;

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some(ARCHIVE_LOCAL),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.moved, 1);
    assert_eq!(report.not_written_back, 0);
    // 先证明走的确实是 `COPY` 那条路（否则这个用例证明不了「COPY 也接了」）。
    assert_eq!(copy_commands(&fixture).len(), 1);

    let archive = server_archive(&fixture);
    assert_eq!(archive.messages.len(), 1);
    let destination_uid = i64::from(archive.messages[0].uid);

    assert!(
        local_row_in_archive(&pool, archive_epoch, destination_uid)
            .await
            .is_some(),
        "COPY 路由也要把目标侧的新 UID 写回本地"
    );

    fixture.server.shutdown().await;
}

/// **库里记着的**目标 epoch 与服务器此刻的不一致（目标文件夹刚被重建过）。
async fn push_move_into_archive_with(
    fixture: &Fixture,
    local_epoch: Option<i64>,
) -> (SqlitePool, Folder, TransferPushReport) {
    let mut session = login_inbox(fixture).await;
    let (pool, folder) = local_store(true).await;
    let folder = pull_and_reload(&pool, &mut session, &folder, 1_000).await;

    // `None` 表示「库里还没有这个文件夹的 epoch」。
    if let Some(epoch) = local_epoch {
        seed_archive_epoch(&pool, epoch).await;
    }

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some(ARCHIVE_LOCAL),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    (pool, folder, report)
}

/// 本地已经有 INBOX 与 Archive**两个**文件夹、且 epoch 都是服务器此刻的值。
///
/// 给「`EXAMINE` 被拒」那类服务器用：它们连拉取都只能用 `SELECT`，所以
/// 这里不走 `pull_and_reload`，而是按**服务器真实宣告的** UIDVALIDITY
/// 把文件夹写进库——写回的门槛（库里 epoch == 服务器此刻的 epoch）才成立。
async fn seed_synced_folders(pool: &SqlitePool, fixture: &Fixture, now_ms: i64) -> Folder {
    let epoch = i64::from(fixture.server.uid_validity());
    let mut inbox = make_folder(INBOX_LOCAL, "INBOX");
    inbox.uidvalidity = Some(epoch);
    inbox.last_sync_at = Some(now_ms);
    folder_dao::upsert_folder(pool, &inbox)
        .await
        .expect("插入 INBOX 失败");
    seed_archive_epoch(pool, epoch).await;
    inbox
}

#[tokio::test]
async fn a_destination_epoch_that_disagrees_with_the_store_means_no_write_back() {
    // 库里记着的目标 epoch 是**旧的**：这中间目标文件夹被服务器重建过。
    // 此刻 `COPYUID` 里的 UID 属于哪个世代说不准 → 一行都不能写，
    // 交给下一轮同步自己 `EXAMINE` 后从服务器拉。
    //
    // （epoch 必须是个正经的正数：库里那一列有 `CHECK (uidvalidity > 0)`。）
    let fixture = start(config_with_mail()).await;
    let archive_epoch = i64::from(fixture.server.uid_validity());
    let (pool, _folder, report) =
        push_move_into_archive_with(&fixture, Some(archive_epoch + 100)).await;

    assert_eq!(report.moved, 1, "移动本身是成功的");
    assert_eq!(
        report.not_written_back, 1,
        "没写回要计一笔，否则日志里找不到"
    );
    assert_eq!(
        report.total(),
        1,
        "`not_written_back` 是附注，不是并列的结局，不能进合计（R36）"
    );

    let archive_rows = message_dao::list_messages(&pool, "acct-1", ARCHIVE_LOCAL)
        .await
        .unwrap();
    assert!(
        archive_rows.is_empty(),
        "epoch 对不上时一行都不许写，实际：{:?}",
        archive_rows
            .iter()
            .map(|m| (m.uid, m.uidvalidity))
            .collect::<Vec<_>>()
    );

    // 服务器那边该做的都做了（这不是失败，只是「晚一轮」）。
    assert_eq!(server_archive(&fixture).messages.len(), 1);
    let count =
        operation_dao::count_operations_in_state(&pool, "acct-1", OperationState::Succeeded)
            .await
            .unwrap();
    assert_eq!(count, 1, "写回不了不影响「移动成功」这个结论");

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn an_unknown_destination_epoch_means_no_write_back() {
    // 库里根本没有目标文件夹的 epoch（从没同步过它）→ 同样不写。
    let fixture = start(config_with_mail()).await;
    let (pool, _folder, report) = push_move_into_archive_with(&fixture, None).await;

    assert_eq!(report.moved, 1);
    assert_eq!(report.not_written_back, 1);

    let archive_rows = message_dao::list_messages(&pool, "acct-1", ARCHIVE_LOCAL)
        .await
        .unwrap();
    assert!(
        archive_rows.is_empty(),
        "不知道 epoch 就不写：写进去的会是一行对不上任何世代的数据"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn a_destination_that_cannot_be_examined_means_no_write_back() {
    // 拿不到目标 epoch 的另一种样子：`EXAMINE` 直接回 `NO`
    // （163 的真实行为之一，见 PROTOCOLS.md）。
    //
    // 关键是两点：不写，而且**会话还能用**——下一条操作照着「选中了源文件夹」
    // 继续发，不能被这一步带偏。
    //
    // `reject_examine` 在**第一轮同步**时就要打开（那是 163 的真实形态：
    // 连拉取都只能用 `SELECT`），所以本地文件夹不能靠 `pull_and_reload` 建。
    let mut config = config_with_mail();
    config.reject_examine = true;
    let fixture = start(config).await;

    // 这台服务器拒绝 `EXAMINE`，所以切文件夹也只能用可写方式（`SELECT`）。
    let (pool, _folder) = local_store(true).await;
    let folder = seed_synced_folders(&pool, &fixture, 1_000).await;
    let mut session = login(&fixture).await;
    session.select("INBOX").await.expect("SELECT 应当成功");

    enqueue_transfer(
        &pool,
        &folder,
        INBOX_UID,
        OperationKind::Move,
        Some(ARCHIVE_LOCAL),
        2_000,
    )
    .await;

    let report = push_transfers(&pool, &mut session, &folder, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.moved, 1, "移动本身成功了");
    assert_eq!(report.not_written_back, 1, "拿不到 epoch → 不写");
    assert_eq!(report.uncertain, 0, "会话没坏，不该牵连别的计数");

    let archive_rows = message_dao::list_messages(&pool, "acct-1", ARCHIVE_LOCAL)
        .await
        .unwrap();
    assert!(archive_rows.is_empty(), "拿不到 epoch 就一行都不写");
    // 用服务器自己的活状态验，不用 `server_has_uid`：那个辅助函数靠 `EXAMINE`，
    // 而这台服务器恰恰拒绝 `EXAMINE`。
    let inbox = fixture
        .server
        .mailboxes()
        .into_iter()
        .find(|mailbox| mailbox.name == "INBOX")
        .expect("服务器上应当有 INBOX");
    assert!(
        inbox
            .messages
            .iter()
            .all(|message| message.uid != INBOX_UID as u32),
        "服务器那边确实搬走了"
    );

    fixture.server.shutdown().await;
}

/// 有 `MOVE` 能力的配置（默认那一档是 163 形态：只有 `UIDPLUS`）。
fn config_with_move() -> FakeServerConfig {
    let mut config = config_with_mail();
    config.capabilities.push("MOVE".to_string());
    config
}

/// 让服务器像**只有 `MOVE`**：没有 `UIDPLUS`。用来隔开两条路由的副作用。
///
/// 有 `UIDPLUS` 时 `UID MOVE` 收尾会再 `EXAMINE` 源文件夹一次（客户端要
/// 切回去），本地库读写的缓存会被填上；没有它时那一次不会发生，
/// 「写回自己按对了 key」这件事才不会被别人顺手遮住。
fn config_with_move_without_uidplus() -> FakeServerConfig {
    let mut config = config_with_move();
    config.capabilities.retain(|cap| cap != "UIDPLUS");
    config
}

/// 再跑一轮 `sync_account`（用户视角的「下一轮同步」）。
async fn sync_again(fixture: &Fixture, pool: &SqlitePool, now_ms: i64) {
    let mut session = login(fixture).await;
    let account = account_dao::get_account(pool, "acct-1")
        .await
        .unwrap()
        .expect("账户应当还在");
    sync_account(pool, &mut session, &account, now_ms)
        .await
        .expect("同步应当成功");
}

/// 断言目标文件夹本地只有一行、且就是服务器上那一封。
///
/// 「写回的 key 与下一轮同步算出来的 key 一致」是这组用例的全部意义：
/// 不一致的话，写回的是一个 key、同步算出的是另一个，本地就会出现同一封信
/// 两行——用户看到重复，正文也可能挂在其中一行上。
async fn assert_one_row_matching_the_server(
    fixture: &Fixture,
    pool: &SqlitePool,
    archive_epoch: i64,
) {
    let archive = server_archive(fixture);
    assert_eq!(archive.messages.len(), 1, "服务器上只有一封");
    let server_uid = i64::from(archive.messages[0].uid);

    let rows = message_dao::list_messages(pool, "acct-1", ARCHIVE_LOCAL)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "下一轮同步不该再插一行（写回的 key 必须与同步算出来的一致），实际：{:?}",
        rows.iter()
            .map(|m| (m.uid, m.uidvalidity, m.id.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(rows[0].uid, server_uid, "本地这一行指向服务器上那一封");
    assert_eq!(rows[0].uidvalidity, archive_epoch, "epoch 要对得上");
}

#[tokio::test]
async fn a_second_sync_after_the_write_back_does_not_duplicate_the_row() {
    // 最容易忽略的一条：写回的那一行，下一轮同步必须**认出**它就是同一封。
    // 认不出来（key 差一点：epoch 错、uid 错、或者干脆按 id 另算了一套）→
    // 本地多一行，用户看到重复。
    //
    // 用「只有 MOVE」的配置：这类服务器没有 `UIDPLUS`，`UID MOVE` 收尾时
    // 客户端不会再 `EXAMINE` 源文件夹，于是写回那一次本地库读写的缓存是空的——
    // 若写回的 key 是靠某个缓存凑巧对上，这里就会露馅。
    let fixture = start(config_with_move_without_uidplus()).await;
    let archive_epoch = i64::from(fixture.server.uid_validity());
    let (pool, _folder, report) = push_move_into_archive_with(&fixture, Some(archive_epoch)).await;

    assert_eq!(report.moved, 1);
    assert_eq!(report.not_written_back, 0, "前提：这一轮确实写回了");

    sync_again(&fixture, &pool, 4_000).await;

    assert_one_row_matching_the_server(&fixture, &pool, archive_epoch).await;

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn a_second_sync_after_the_copy_route_write_back_does_not_duplicate_the_row() {
    // 同上，走 163 的 `COPY` + 标删那条路。
    let fixture = start(config_with_mail()).await;
    let archive_epoch = i64::from(fixture.server.uid_validity());
    let (pool, _folder, report) = push_move_into_archive_with(&fixture, Some(archive_epoch)).await;

    assert_eq!(report.moved, 1);
    assert_eq!(report.not_written_back, 0);

    sync_again(&fixture, &pool, 4_000).await;

    assert_one_row_matching_the_server(&fixture, &pool, archive_epoch).await;

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn a_delete_without_a_destination_writes_nothing_back() {
    // 就地删除（`Route::Expunge`）没有目标侧，不该产生任何写回尝试——
    // 它连 `EXAMINE` 都不该多打一条。
    let fixture = start(config_without_any_trash_hint()).await;
    let (pool, _folder) = local_store(false).await;
    sync_once(&fixture, &pool).await;
    let inbox = local_folder(&pool, "INBOX").await;

    let mut session = login_inbox(&fixture).await;
    let inbox = pull_and_reload(&pool, &mut session, &inbox, 1_500).await;
    enqueue_transfer(&pool, &inbox, INBOX_UID, OperationKind::Delete, None, 2_000).await;

    let before = fixture.server.command_log().len();
    let report = push_transfers(&pool, &mut session, &inbox, 3_000)
        .await
        .expect("回写本身不该报错");

    assert_eq!(report.deleted, 1);
    assert_eq!(
        report.not_written_back, 0,
        "就地删除没有目标侧，不是「挪过去了但本地没跟上」，不该计这一笔"
    );

    let after = fixture.server.command_log();
    let new_commands: Vec<&String> = after.iter().skip(before).collect();
    assert!(
        !new_commands
            .iter()
            .any(|line| **line == format!("EXAMINE \"{}\"", inbox.remote_name)),
        "就地删除不该为了写回去 `EXAMINE` 源文件夹自己，实际：{new_commands:?}"
    );

    fixture.server.shutdown().await;
}
