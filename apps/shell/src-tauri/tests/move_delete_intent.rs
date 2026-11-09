//! 移动 / 删除意图的服务层测试。
//!
//! 这一层只做**校验 + 入队**，不认识 IMAP（提交在 `xremail-sync` 里，
//! 那条链路由 `crates/xremail-sync/tests/push_transfers.rs` 端到端覆盖）。
//! 所以要在这里钉住的是**校验**：哪些请求必须被挡在队列之外。
//!
//! 覆盖四件事：
//!
//! 1. 移动意图落库时带上了**目标文件夹**与绑定 epoch；
//! 2. 目标文件夹就是源文件夹 → 明确报错（否则服务器上会拷出一份重复的）；
//! 3. 目标文件夹属于别的账户 → 明确报错；
//! 4. 文件夹没同步过（没有 epoch）→ 明确报错，**绝不**拿 `uidvalidity = 0` 去猜。
//!
//! 不依赖真实邮箱、凭据、网络或本机绝对路径
//! （见 [TESTING_STRATEGY §5.2](../../../../docs/TESTING_STRATEGY.md)）。

use sqlx::SqlitePool;
use xremail_api::NewAccountRequest;
use xremail_core::model::{
    Folder, Message, MessageFlags, OperationKind, OperationState, ProviderKind, SmtpAuth, SyncMode,
    TlsMode,
};
use xremail_shell_lib::service::{AccountService, MessageService};
use xremail_store::dao::folder as folder_dao;
use xremail_store::dao::message as message_dao;
use xremail_store::dao::operation as operation_dao;

/// 文件夹的 epoch。
const EPOCH: i64 = 42;
/// 目标邮件的 UID。
const UID: i64 = 7;

fn request(email: &str) -> NewAccountRequest {
    NewAccountRequest {
        email: email.to_string(),
        provider: ProviderKind::Netease163,
        display_name: Some("我的邮箱".to_string()),
        imap_host: "imap.163.com".to_string(),
        imap_port: 993,
        imap_tls: TlsMode::Implicit,
        smtp_host: "smtp.163.com".to_string(),
        smtp_port: 465,
        smtp_tls: TlsMode::Implicit,
        smtp_auth: SmtpAuth::Login,
        username: email.to_string(),
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
    }
}

fn folder(id: &str, account_id: &str, remote_name: &str, uidvalidity: Option<i64>) -> Folder {
    Folder {
        id: id.to_string(),
        account_id: account_id.to_string(),
        remote_name: remote_name.to_string(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".to_string()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        uidvalidity,
        role: None,
    }
}

fn message(account_id: &str, folder_id: &str, uid: i64) -> Message {
    Message {
        id: format!("message-{uid}"),
        account_id: account_id.to_string(),
        folder_id: folder_id.to_string(),
        uid,
        uidvalidity: EPOCH,
        message_id: Some("<abc@163.com>".to_string()),
        subject: Some("要被搬走的一封".to_string()),
        snippet: None,
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        date_sent: None,
        internal_date: None,
        flags: MessageFlags::empty(),
        size: None,
        has_attachment: false,
    }
}

/// 内存库 + 账户 + INBOX（+ 可选的另一个文件夹）。
async fn setup(
    uidvalidity: Option<i64>,
    with_archive: bool,
) -> (SqlitePool, String, MessageService, String) {
    let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
        .await
        .expect("打开内存库失败");
    xremail_store::migrate(&pool).await.expect("应用迁移失败");

    let accounts = AccountService::new(pool.clone());
    let account = accounts
        .create(request("user@163.com"))
        .await
        .expect("创建账户失败");

    let inbox = folder("folder-1", &account.id, "INBOX", uidvalidity);
    folder_dao::upsert_folder(&pool, &inbox)
        .await
        .expect("写入文件夹失败");

    if uidvalidity.is_some() {
        message_dao::upsert_message(&pool, &message(&account.id, "folder-1", UID))
            .await
            .expect("写入邮件失败");
    }

    if with_archive {
        folder_dao::upsert_folder(
            &pool,
            &folder("folder-archive", &account.id, "Archive", Some(EPOCH)),
        )
        .await
        .expect("写入 Archive 失败");
    }

    (
        pool.clone(),
        account.id,
        MessageService::new(pool),
        inbox.id,
    )
}

#[tokio::test]
async fn a_move_intent_records_the_destination_and_epoch() {
    let (_pool, account_id, service, folder_id) = setup(Some(EPOCH), true).await;

    let operations = service
        .move_intent(&account_id, &folder_id, &[UID], "folder-archive")
        .await
        .expect("移动意图应当入队");

    assert_eq!(operations.len(), 1);
    let operation = &operations[0];
    assert_eq!(operation.kind, OperationKind::Move);
    assert_eq!(operation.uid, UID);
    assert_eq!(operation.uidvalidity, EPOCH, "必须绑定 epoch");
    assert_eq!(
        operation.destination_folder_id.as_deref(),
        Some("folder-archive")
    );
    assert_eq!(operation.state, OperationState::Queued);
}

#[tokio::test]
async fn moving_into_the_same_folder_is_rejected() {
    // 目标就是源 → 服务器上会变成「拷一份到自己」，不是用户想要的。
    let (_pool, account_id, service, folder_id) = setup(Some(EPOCH), true).await;

    let error = service
        .move_intent(&account_id, &folder_id, &[UID], &folder_id)
        .await
        .expect_err("应当被拒绝");

    assert!(
        error.to_string().contains("源文件夹"),
        "错误信息该说清楚是目标等于源，实际：{error}"
    );
}

#[tokio::test]
async fn moving_into_another_accounts_folder_is_rejected() {
    // 跨账户移动必须在服务层就被挡住：文件夹查询按账户隔离，
    // 另一个账户的 `folder_id` 在本账户里查不到 → 直接拒绝。
    let (pool, account_id, service, folder_id) = setup(Some(EPOCH), true).await;

    let accounts = AccountService::new(pool.clone());
    let other = accounts
        .create(request("other@163.com"))
        .await
        .expect("创建第二个账户失败");
    folder_dao::upsert_folder(
        &pool,
        &folder("folder-other", &other.id, "Archive", Some(EPOCH)),
    )
    .await
    .expect("写入别人的文件夹失败");

    let error = service
        .move_intent(&account_id, &folder_id, &[UID], "folder-other")
        .await
        .expect_err("跨账户应当被拒绝");

    assert!(
        error.to_string().contains("在这个账户里不存在"),
        "错误信息该说清目标不在本账户，实际：{error}"
    );
}

#[tokio::test]
async fn delete_intent_records_no_destination() {
    // 删除的去处由同步层决定（有垃圾箱就挪进去），服务层不该写死。
    let (_pool, account_id, service, folder_id) = setup(Some(EPOCH), false).await;

    let operations = service
        .delete_intent(&account_id, &folder_id, &[UID])
        .await
        .expect("删除意图应当入队");

    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].kind, OperationKind::Delete);
    assert_eq!(operations[0].destination_folder_id, None);
    assert_eq!(operations[0].uidvalidity, EPOCH);
}

#[tokio::test]
async fn a_folder_that_was_never_synced_refuses_to_move() {
    let (_pool, account_id, service, folder_id) = setup(None, true).await;

    let error = service
        .move_intent(&account_id, &folder_id, &[UID], "folder-archive")
        .await
        .expect_err("没有 epoch 时应当拒绝");

    assert!(
        error.to_string().contains("epoch"),
        "错误信息该提到 epoch，实际：{error}"
    );
}

#[tokio::test]
async fn moving_a_message_that_is_not_local_is_rejected() {
    let (_pool, account_id, service, folder_id) = setup(Some(EPOCH), true).await;

    let error = service
        .move_intent(&account_id, &folder_id, &[999], "folder-archive")
        .await
        .expect_err("本地没有这封信时应当拒绝");

    assert!(
        error.to_string().contains("999"),
        "错误信息该带上 UID，实际：{error}"
    );
}

#[tokio::test]
async fn moving_a_missing_destination_reports_a_clear_error() {
    let (_pool, account_id, service, folder_id) = setup(Some(EPOCH), true).await;

    let error = service
        .move_intent(&account_id, &folder_id, &[UID], "folder-nope")
        .await
        .expect_err("目标不存在时应当报错");

    assert!(
        error.to_string().contains("folder-nope"),
        "错误信息该带上目标 id，实际：{error}"
    );
}

#[tokio::test]
async fn an_empty_selection_queues_nothing() {
    // 界面上「选了 0 封」不该产生任何操作记录。
    let (pool, account_id, service, folder_id) = setup(Some(EPOCH), true).await;

    let operations = service
        .move_intent(&account_id, &folder_id, &[], "folder-archive")
        .await
        .expect("空选择不该报错");

    assert!(operations.is_empty());

    let pending = operation_dao::list_pending_operations(&pool, &account_id, &folder_id)
        .await
        .expect("读队列失败");
    assert!(pending.is_empty(), "队列里不该有任何东西");
}
