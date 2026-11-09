//! 跨通道的「认领了但没人收」缺口（R50）。
//!
//! ## 为什么单独一个文件
//!
//! `operation_gate.rs` 钉的是**同一通道内**的互斥（R34）：
//! 「同一封邮件的移动 / 删除同时只能有一条在途」。而这里钉的是
//! **两条通道撞在一条 `UPDATE` 上**时才出现的那道缺口——
//! 它不属于任何一条通道的语义，是两个 `WHERE` 条件各自都「看着对」、
//! 叠在一起却漏掉一个时序的产物。
//!
//! ## 缺口长什么样
//!
//! * [`operation::claim_for_push`]：`move` / `delete` 的认领要「本邮件没有**别的**
//!   `running` 搬移」，flag 不受限（`kind = 'flags' OR …`）；
//! * [`operation::mark_operation_retryable`] / [`operation::update_operation_state`]：
//!   **不带任何 `owner_boot` / `id` 之外的守卫**，只要是 `running` 就改——包括
//!   改掉**别人**正在提交的那一行。
//!
//! 于是「搬移认领 → 同一封邮件的 flag 认领」（flag 不受搬移闸门约束）
//! 这个时序里，搬移的失败处置会把一条**在途的** flag 行改写成
//! `retryable` / `failed`。两个后果叠在一起：
//!
//! 1. 认领 flag 的那个 `push_flags` 拿着 `claimed` 继续发 `STORE`，收尾时
//!    `WHERE state = 'running'` 改不到行——**结果被静默丢弃**，服务器上标记
//!    改了，库里那条却是「可重试」，下一轮再发一遍；
//! 2. 真正失败/结果未知的**搬移**那一行没人写终态，留在 `running`，
//!    要等 15 分钟租约才被收成 `uncertain`——这次失败的**原因**就此丢失。
//!
//! ## 真实触发面
//!
//! 用户点星标走 `apps/shell` 的 `set_flag_intent → push_flags`，actor 那一轮走
//! `flush_pending → push_transfers`，两条是**同一个进程、同一个 `boot_id`** 的
//! 独立会话，交替认领同一封邮件上的 flag 与搬移行是正常时序，
//! 不需要崩溃也不需要时钟错乱。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, FlagIntentPayload, Folder, MailOperation, MessageFlags, OperationKind,
    OperationState, ProviderKind, ServerConfig, SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::{account, folder, operation};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};

/// 测试用的 epoch。
const EPOCH: i64 = 42;

/// 本进程（两条通道共用的 `boot_id`）。
const BOOT: &str = "boot-self";

/// 建一个已应用迁移的内存库，并种好账户 + 两个文件夹。
async fn seeded_pool() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("应用迁移失败");

    let now = 1_700_000_000_000;
    let account = Account {
        id: "a1".to_string(),
        email: "a1@example.com".to_string(),
        provider: ProviderKind::Netease163,
        display_name: None,
        color: None,
        auth_kind: AuthKind::Password,
        server: ServerConfig {
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            imap_tls: TlsMode::Implicit,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 465,
            smtp_tls: TlsMode::Implicit,
            smtp_auth: SmtpAuth::Login,
        },
        username: "a1@example.com".to_string(),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: now,
        updated_at: now,
    };
    account::upsert_account(&pool, &account)
        .await
        .expect("账户应当写入");

    for (id, name) in [("inbox", "INBOX"), ("archive", "Archive")] {
        let folder = Folder {
            id: id.to_string(),
            account_id: "a1".to_string(),
            remote_name: name.to_string(),
            remote_id: None,
            local_name: None,
            delimiter: Some("/".to_string()),
            subscribed: true,
            selectable: true,
            last_sync_at: None,
            last_uid: None,
            highest_modseq: None,
            uidvalidity: Some(EPOCH),
            role: None,
        };
        folder::upsert_folder(&pool, &folder)
            .await
            .expect("文件夹应当写入");
    }

    pool
}

/// 造一条待入队的 flag 操作（在 uid 7 上标已读）。
fn flag_operation(id: &str) -> MailOperation {
    MailOperation {
        id: id.to_string(),
        account_id: "a1".to_string(),
        folder_id: "inbox".to_string(),
        uid: 7,
        uidvalidity: EPOCH,
        kind: OperationKind::Flags,
        intent: FlagIntentPayload::new(MessageFlags(MessageFlags::SEEN), MessageFlags(0)),
        destination_folder_id: None,
        state: OperationState::Queued,
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 1,
        updated_at: 1,
    }
}

/// 造一条待入队的移动操作（把 uid 7 搬到 archive）。
fn move_operation(id: &str) -> MailOperation {
    MailOperation {
        id: id.to_string(),
        account_id: "a1".to_string(),
        folder_id: "inbox".to_string(),
        uid: 7,
        uidvalidity: EPOCH,
        kind: OperationKind::Move,
        intent: FlagIntentPayload::default(),
        destination_folder_id: Some("archive".to_string()),
        state: OperationState::Queued,
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 2,
        updated_at: 2,
    }
}

/// 读回一条操作的状态。
async fn load(pool: &SqlitePool, id: &str) -> MailOperation {
    operation::get_operation(pool, id)
        .await
        .expect("读操作应当成功")
        .expect("操作应当还在")
}

#[tokio::test]
async fn a_retryable_move_mark_does_not_overwrite_an_in_flight_flag() {
    // 时序：`push_transfers` 认领了 uid 7 的**移动** → 用户在同一封邮件上点星标，
    // `set_flag_intent → push_flags` 认领同 UID 的 **flag**（flag 通道不受搬移闸门
    // 约束，见 `claim_for_push` 的 `kind = 'flags' OR …`）→ 搬移那边在
    // `select_for_write` / `COPY` 上失败，按 `Retryable` 收尾。
    //
    // 缺陷现场：`mark_operation_retryable` 只认 `state = 'running'`，不认「这一行
    // 是不是我认领的」。于是它改到的是**那条 flag 行**——`push_flags` 手上那条
    // 被翻成 `retryable`，收尾时 `WHERE state = 'running'` 改不到行；而真正的移动行
    // 还停在 `running`，这次失败**一点都没记下来**。
    let pool = seeded_pool().await;
    operation::enqueue_operation(&pool, &move_operation("op-move"))
        .await
        .expect("移动入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-move", BOOT, 5, 1_000)
            .await
            .expect("搬移认领应当成功")
    );

    operation::enqueue_operation(&pool, &flag_operation("op-flag"))
        .await
        .expect("flag 入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-flag", BOOT, 5, 1_100)
            .await
            .expect("flag 认领应当不报错"),
        "flag 通道不受搬移闸门约束（这一条是设计，R34 的注释写明了）"
    );

    assert!(
        operation::mark_operation_retryable(
            &pool,
            "op-move",
            BOOT,
            "select_failed",
            "选不中",
            1_200,
        )
        .await
        .expect("标记应当不报错"),
        "认领过的那条应当能被自己收成 retryable"
    );

    let flag = load(&pool, "op-flag").await;
    assert_eq!(
        flag.state,
        OperationState::Running,
        "在途的 flag 被翻成了 {:?}：认领它的 push_flags 收尾时 WHERE state='running' \
         将改不到行，服务器上的 STORE 结果于是被静默丢弃，下一轮还会再发一遍",
        flag.state
    );
    assert_eq!(flag.attempts, 0, "在途的 flag 不该被记一次失败");
    assert_eq!(flag.error_kind, None, "别人认领的行不该被写上失败原因");

    let moved = load(&pool, "op-move").await;
    assert_eq!(
        moved.state,
        OperationState::Retryable,
        "真正失败的那条没被收成 retryable：它停在 {:?}——这次失败根本没记下来",
        moved.state
    );
    assert_eq!(moved.attempts, 1, "失败要记在那条失败的行上");
}

#[tokio::test]
async fn a_terminal_move_mark_does_not_overwrite_an_in_flight_flag() {
    // 同一条缺口的另一支：`update_operation_state`（写终态）也一样没有归属守卫。
    //
    // 认领了移动 → 同一封邮件的 flag 被另一条通道认领 → 移动这一轮算出
    // 「这条发不成」（`transfer_push` 的 `destination_missing` / `permanent` 两支）
    // → 终态被写到了**别人的** running 行上：在途的 flag 直接变成 `failed`，
    // 而服务器上那条 `STORE` 其实正在/已经执行，结果被丢弃。
    let pool = seeded_pool().await;
    operation::enqueue_operation(&pool, &move_operation("op-move"))
        .await
        .expect("移动入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-move", BOOT, 5, 1_000)
            .await
            .expect("搬移认领应当成功")
    );

    operation::enqueue_operation(&pool, &flag_operation("op-flag"))
        .await
        .expect("flag 入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-flag", BOOT, 5, 1_100)
            .await
            .expect("flag 认领应当不报错")
    );

    assert!(
        operation::update_operation_state_as(
            &pool,
            "op-move",
            Some(BOOT),
            OperationState::Failed,
            Some("permanent"),
            Some("这条搬不过去"),
            1_200,
        )
        .await
        .expect("写终态应当不报错")
    );

    let flag = load(&pool, "op-flag").await;
    assert_eq!(
        flag.state,
        OperationState::Running,
        "在途的 flag 被写成了 {:?}——它的收尾将改不到行",
        flag.state
    );

    let moved = load(&pool, "op-move").await;
    assert_eq!(
        moved.state,
        OperationState::Failed,
        "真正失败的那条没被收成 failed，停在 {:?}",
        moved.state
    );
}
