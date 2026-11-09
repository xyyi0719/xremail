//! 标记意图（已读 / 星标）的服务层测试。
//!
//! 覆盖三件最容易出错的事：
//!
//! 1. 意图**落库**后，`list_messages` 立刻反映出星标（显示层叠加）；
//! 2. 文件夹**没同步过**（没有 epoch）时**明确报错**；
//! 3. 多个意图按 `seq` 定序：后来的覆盖先来的。
//!
//! 不依赖真实邮箱、凭据、网络或本机绝对路径
//! （见 [TESTING_STRATEGY §5.2](../../../../docs/TESTING_STRATEGY.md)）。

use sqlx::SqlitePool;
use xremail_api::NewAccountRequest;
use xremail_core::model::{
    Folder, Message, MessageFlags, OperationState, ProviderKind, SmtpAuth, SyncMode, TlsMode,
};
use xremail_shell_lib::service::{AccountService, MessageFlag, MessageService};
use xremail_store::dao::folder as folder_dao;
use xremail_store::dao::message as message_dao;
use xremail_store::dao::operation as operation_dao;

/// 文件夹的 epoch。
const EPOCH: i64 = 42;
/// 目标邮件的 UID。
const UID: i64 = 7;

/// 一份合法的账户请求。
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

/// 一个文件夹（`uidvalidity` = `None` 表示还没同步过）。
fn folder(account_id: &str, uidvalidity: Option<i64>) -> Folder {
    Folder {
        id: "folder-1".to_string(),
        account_id: account_id.to_string(),
        remote_name: "INBOX".to_string(),
        remote_id: None,
        local_name: Some("收件箱".to_string()),
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

/// 一封邮件（服务器快照：没有任何标记）。
fn message(account_id: &str, uid: i64) -> Message {
    Message {
        id: "message-1".to_string(),
        account_id: account_id.to_string(),
        folder_id: "folder-1".to_string(),
        uid,
        uidvalidity: EPOCH,
        message_id: Some("<abc@163.com>".to_string()),
        subject: Some("你好".to_string()),
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

/// 造一个已应用迁移的内存库，并写入账户、文件夹与一封邮件。
///
/// `uidvalidity` 为 `None` 时模拟「文件夹还没同步过」。
async fn setup(uidvalidity: Option<i64>) -> (SqlitePool, String, MessageService) {
    let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
        .await
        .expect("打开内存库失败");
    xremail_store::migrate(&pool).await.expect("应用迁移失败");

    let accounts = AccountService::new(pool.clone());
    let account = accounts
        .create(request("user@163.com"))
        .await
        .expect("创建账户失败");

    folder_dao::upsert_folder(&pool, &folder(&account.id, uidvalidity))
        .await
        .expect("写入文件夹失败");
    message_dao::upsert_message(&pool, &message(&account.id, UID))
        .await
        .expect("写入邮件失败");

    (pool.clone(), account.id, MessageService::new(pool))
}

#[tokio::test]
async fn a_starred_intent_shows_up_in_list_messages_right_away() {
    let (_pool, account_id, service) = setup(Some(EPOCH)).await;

    // 落库之前：服务器快照里没有星标。
    let before = service
        .list_messages(&account_id, "folder-1")
        .await
        .unwrap();
    assert!(!before[0].flags.is_starred(), "落库前不该凭空多出星标");

    service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Flagged, true)
        .await
        .expect("标记星标应当成功");

    // 落库之后：本地视图立刻反映出来（服务器还没确认，但界面要先变）。
    let after = service
        .list_messages(&account_id, "folder-1")
        .await
        .unwrap();
    assert!(
        after[0].flags.is_starred(),
        "意图落库后本地视图应当立刻出现星标"
    );
    // 只动被点的那一个位。
    assert!(
        !after[0].flags.is_read(),
        "只点了星标，不该顺带把邮件标成已读"
    );
}

#[tokio::test]
async fn the_intent_only_carries_the_bit_that_was_asked_for() {
    let (_pool, account_id, service) = setup(Some(EPOCH)).await;

    let operation = service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Seen, true)
        .await
        .expect("标记已读应当成功");

    assert_eq!(operation.state, OperationState::Queued);
    assert_eq!(operation.uidvalidity, EPOCH);
    assert_eq!(operation.uid, UID);
    assert_eq!(
        operation.intent.add,
        MessageFlags::empty().with(MessageFlags::SEEN),
        "置位应当只带 SEEN 那一项"
    );
    assert!(operation.intent.remove.is_empty(), "置位时不该同时带清除位");

    let cleared = service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Seen, false)
        .await
        .expect("取消已读应当成功");
    assert!(cleared.intent.add.is_empty(), "清位时不该带置位");
    assert_eq!(
        cleared.intent.remove,
        MessageFlags::empty().with(MessageFlags::SEEN),
        "清位应当只带 SEEN 那一项"
    );
}

#[tokio::test]
async fn a_folder_that_was_never_synced_reports_a_clear_error() {
    let (_pool, account_id, service) = setup(None).await;

    let error = service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Seen, true)
        .await
        .expect_err("没同步过的文件夹不该能标记");

    let message = error.to_string();
    assert!(
        message.contains("还没同步过"),
        "错误信息应当说清是文件夹没同步过，实际是：{message}"
    );
}

#[tokio::test]
async fn a_missing_message_reports_a_clear_error() {
    let (_pool, account_id, service) = setup(Some(EPOCH)).await;

    let error = service
        .set_flag_intent(&account_id, "folder-1", 999, MessageFlag::Seen, true)
        .await
        .expect_err("不存在的邮件不该能标记");

    let message = error.to_string();
    assert!(
        message.contains("没有"),
        "错误信息应当说清本地没有这封邮件，实际是：{message}"
    );
}

#[tokio::test]
async fn a_missing_folder_reports_a_clear_error() {
    let (_pool, account_id, service) = setup(Some(EPOCH)).await;

    let error = service
        .set_flag_intent(&account_id, "没有这个文件夹", UID, MessageFlag::Seen, true)
        .await
        .expect_err("不存在的文件夹不该能标记");

    assert!(error.to_string().contains("文件夹"));
}

#[tokio::test]
async fn a_later_intent_overrides_the_earlier_one() {
    let (_pool, account_id, service) = setup(Some(EPOCH)).await;

    service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Flagged, true)
        .await
        .expect("加星标应当成功");
    service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Flagged, false)
        .await
        .expect("取消星标应当成功");

    // 后来的意图（seq 更大）覆盖先前的。
    let messages = service
        .list_messages(&account_id, "folder-1")
        .await
        .unwrap();
    assert!(
        !messages[0].flags.is_starred(),
        "后发的取消星标应当覆盖先前的加星标"
    );
}

#[tokio::test]
async fn the_operation_is_persisted_with_a_sequence_number() {
    let (pool, account_id, service) = setup(Some(EPOCH)).await;

    let operation = service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Flagged, true)
        .await
        .expect("标记星标应当成功");

    // `seq` 由存储层分配（同一账户 + 文件夹内严格递增）。
    assert!(operation.seq >= 1, "seq 应当由存储层分配");

    let stored = operation_dao::get_operation(&pool, &operation.id)
        .await
        .expect("读操作应当成功")
        .expect("操作应当已经落库");
    assert_eq!(stored.id, operation.id);
    assert_eq!(stored.seq, operation.seq);
    assert_eq!(stored.state, OperationState::Queued);
}

#[tokio::test]
async fn getting_a_missing_operation_reports_none_instead_of_failing() {
    let (pool, _account_id, _service) = setup(Some(EPOCH)).await;

    let stored = operation_dao::get_operation(&pool, "没有这条操作")
        .await
        .expect("读不存在的操作不该报错");

    assert!(stored.is_none(), "不存在的操作应当是 None");
}

#[tokio::test]
async fn a_completed_operation_is_no_longer_overlaid() {
    let (pool, account_id, service) = setup(Some(EPOCH)).await;

    let operation = service
        .set_flag_intent(&account_id, "folder-1", UID, MessageFlag::Flagged, true)
        .await
        .expect("标记星标应当成功");

    // 服务器已确认 → 意图不再「未完结」，本地视图回到快照 + 服务器确认过的状态。
    operation_dao::update_operation_state(
        &pool,
        &operation.id,
        OperationState::Succeeded,
        None,
        None,
        1_700_000_000_000,
    )
    .await
    .expect("更新操作状态应当成功");

    let messages = service
        .list_messages(&account_id, "folder-1")
        .await
        .unwrap();
    assert!(
        !messages[0].flags.is_starred(),
        "完结的意图不再叠加到本地视图上"
    );
}
