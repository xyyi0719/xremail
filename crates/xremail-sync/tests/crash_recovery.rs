//! 崩溃恢复 / 启动对账的端到端测试。
//!
//! 这条链路是 [ARCHITECTURE §4.4](../../../docs/ARCHITECTURE.md) 的恢复路径：
//!
//! ```text
//! 进程死在提交途中 → 库里有行停在 running → 下次启动对账 → 收成 uncertain
//!                  → 回读确认（幂等）→ 对齐
//! ```
//!
//! 要证明的东西，按「错了会怎样」排序：
//!
//! * **`uncertain` 不会被自动重发**。自动重发 `MOVE` 会在服务器上多一份邮件；
//!   自动重发 `SMTP DATA` 会让人收到两封一模一样的信。
//! * **本进程自己的在途行不被回收**。误收会把活着的操作翻成「结果未知」，
//!   用户白确认一次、还烧掉一次尝试配额。
//! * **次数到顶的残骸也会被收掉**。这依赖「先回收、后判过期」的顺序：
//!   反过来做，一条属于上次进程、次数也已到顶的 `running` 会永远卡在 `running`。
//! * **对账是幂等的**。重复执行必须什么都不做（未来若在调度器的 `Startup`
//!   轮再补一个触发点也是安全的）。
//!   重复执行必须什么都不做。
//! * **孤儿与旧 epoch 会被判死**，而不是留在队列里被反复重试到次数用尽。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, FlagIntentPayload, Folder, MailOperation, OperationKind, OperationState,
    ProviderKind, SendQueueEntry, SendState, ServerConfig as DomainServerConfig, SyncMode,
};
use xremail_store::dao::{
    account as account_dao, folder as folder_dao, operation as operation_dao,
    send_queue as send_queue_dao,
};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::{MAX_OPERATION_ATTEMPTS, reconcile_account_as};

/// `Open` 消息的 flag 意图：最小可用的 payload。
fn no_intent() -> FlagIntentPayload {
    FlagIntentPayload::default()
}

async fn store() -> SqlitePool {
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

    pool
}

fn folder(id: &str, remote_name: &str, uidvalidity: Option<i64>) -> Folder {
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
        uidvalidity,
    }
}

/// 入队一条操作，并把它推进到指定状态。
///
/// `owner_boot` 通过真实的认领写入，而不是直接改列：
/// 这样测试验证的是**产品代码真的会记归属**，而不是断言一个我手动塞的值。
async fn enqueue(
    pool: &SqlitePool,
    id: &str,
    kind: OperationKind,
    state: OperationState,
    uidvalidity: i64,
    destination: Option<&str>,
    owner_boot: Option<&str>,
) {
    let operation = MailOperation {
        id: id.to_string(),
        account_id: "acct-1".to_string(),
        folder_id: "folder-1".to_string(),
        uid: 42,
        uidvalidity,
        kind,
        intent: no_intent(),
        destination_folder_id: destination.map(str::to_string),
        state: OperationState::Queued,
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 0,
        updated_at: 0,
    };
    operation_dao::enqueue_operation(pool, &operation)
        .await
        .expect("入队失败");

    match state {
        OperationState::Queued => {}
        OperationState::Running => {
            let boot = owner_boot.expect("running 必须有归属");
            assert!(
                operation_dao::claim_for_push(pool, id, boot, MAX_OPERATION_ATTEMPTS, 1)
                    .await
                    .expect("认领失败"),
                "认领应当成功"
            );
        }
        OperationState::Uncertain => {
            operation_dao::update_operation_state(
                pool,
                id,
                OperationState::Uncertain,
                Some("interrupted"),
                Some("上次进程中断"),
                1,
            )
            .await
            .expect("标 uncertain 失败");
        }
        other => panic!("测试不需要把操作推进到 {other:?}"),
    }
}

async fn state_of(pool: &SqlitePool, id: &str) -> (OperationState, Option<String>, i64) {
    let op = operation_dao::get_operation(pool, id)
        .await
        .expect("读操作失败")
        .expect("操作应当还在");
    (op.state, op.error_kind, op.attempts)
}

#[tokio::test]
async fn an_operation_left_running_by_a_dead_process_becomes_uncertain() {
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();

    // 上一次进程（boot-a）认领了，然后死了。
    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Running,
        100,
        None,
        Some("boot-a"),
    )
    .await;

    // 这一次启动（boot-b）对账。
    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");

    assert_eq!(report.reclaimed_operations, 1, "应当回收一条残骸");

    let (state, kind, _) = state_of(&pool, "op-1").await;
    assert_eq!(
        state,
        OperationState::Uncertain,
        "中断的操作是「不知道」，不能当成「没做」重放"
    );
    assert_eq!(kind.as_deref(), Some("interrupted"));
}

#[tokio::test]
async fn rows_claimed_by_the_live_process_are_left_alone() {
    // 这条钉的是 `owner_boot` 守卫的全部价值。
    // 若启动时无条件扫所有 `running`，同一台机器上另一个**还活着**的进程
    // 正在提交的行会被翻成「结果未知」——用户白确认一次，还烧掉一次配额。
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();

    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Running,
        100,
        None,
        Some("boot-b"),
    )
    .await;

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");

    assert_eq!(report.reclaimed_operations, 0, "自己的行不是残骸");

    let (state, _, _) = state_of(&pool, "op-1").await;
    assert_eq!(state, OperationState::Running, "活着的操作必须留在 running");
}

#[tokio::test]
async fn a_legacy_row_without_an_owner_is_reclaimed() {
    // 升级上来的库里，`owner_boot` 是 NULL。若不单独判这一支，
    // 那批行永远扫不到——正是这次改动要修的漏洞。
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();

    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Running,
        100,
        None,
        Some("boot-a"),
    )
    .await;

    // 手动抹掉归属，模拟「迁移之前就在 running 的老数据」。
    sqlx::query("UPDATE mail_operation SET owner_boot = NULL WHERE id = 'op-1'")
        .execute(&pool)
        .await
        .unwrap();

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");

    assert_eq!(
        report.reclaimed_operations, 1,
        "没有归属的行按「不是我的」处理"
    );
}

#[tokio::test]
async fn an_exhausted_row_stuck_in_running_is_reclaimed_then_failed() {
    // 这条钉的是**顺序**：先回收、后判过期。
    //
    // 反过来做的话，`expire_exhausted` 的状态过滤（`queued/retryable/uncertain`）
    // 会漏掉这一行，它就永远停在 `running`——既没人推它，也没人报错。
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();

    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Running,
        100,
        None,
        Some("boot-a"),
    )
    .await;

    // 把尝试次数顶到上限（模拟「失败了很多次之后进程又死了」）。
    sqlx::query("UPDATE mail_operation SET attempts = ? WHERE id = 'op-1'")
        .bind(MAX_OPERATION_ATTEMPTS)
        .execute(&pool)
        .await
        .unwrap();

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");

    assert_eq!(report.reclaimed_operations, 1, "先收成 uncertain");
    assert_eq!(report.expired, 1, "再判次数到顶");

    let (state, kind, _) = state_of(&pool, "op-1").await;
    assert_eq!(state, OperationState::Failed);
    assert_eq!(
        kind.as_deref(),
        Some("retry_exhausted"),
        "到顶之后要报明确原因，不能只说 failed"
    );
}

#[tokio::test]
async fn an_uncertain_operation_is_never_reverted_by_reconcile() {
    // 对账只能判「做不成」，不能替服务器判断「上次没执行」然后把意图重发。
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();

    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Uncertain,
        100,
        None,
        None,
    )
    .await;

    reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");

    let (state, _, _) = state_of(&pool, "op-1").await;
    assert_eq!(
        state,
        OperationState::Uncertain,
        "没有人的显式决定，uncertain 不该变"
    );
}

#[tokio::test]
async fn reconcile_is_idempotent() {
    // 为什么必须幂等：它是启动时跑的，而启动可能因为库已被另一个窗口
    // 初始化过等各种原因多跑一次。
    // 重复执行必须什么都不做，否则第二遍会把第一遍的成果再改一次。
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();

    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Running,
        100,
        None,
        Some("boot-a"),
    )
    .await;

    let first = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("第一次对账应当成功");
    assert!(!first.is_noop(), "第一遍应当真的动了行");

    let second = reconcile_account_as(&pool, "acct-1", "boot-b", 20)
        .await
        .expect("第二次对账应当成功");
    assert!(second.is_noop(), "第二遍应当什么都不做，实际 {second:?}");
}

#[tokio::test]
async fn an_operation_left_running_in_the_send_queue_becomes_uncertain() {
    // 发信队列与操作队列是同一个坑：`claim_for_sending` 会置 `running`。
    // 它收成 `uncertain` 而不自动重发，是因为 SMTP DATA 可能已经送达——
    // 重发就是让人收到两封一样的信。
    let pool = store().await;

    let entry = SendQueueEntry {
        id: "s1".to_string(),
        account_id: "acct-1".to_string(),
        from: "tester@example.com".to_string(),
        to: vec!["friend@example.com".to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "你好".to_string(),
        body_text: Some("正文".to_string()),
        body_html: None,
        attachments: Vec::new(),
        in_reply_to: None,
        references: None,
        state: SendState::Draft,
        idempotency_key: "key-s1".to_string(),
        message_bytes: None,
        message_id: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 0,
        updated_at: 0,
    };
    send_queue_dao::save_draft(&pool, &entry).await.unwrap();
    send_queue_dao::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();
    send_queue_dao::claim_for_sending(&pool, "s1", "boot-a", 2, send_queue_dao::MAX_SEND_ATTEMPTS)
        .await
        .unwrap();

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");
    assert_eq!(report.reclaimed_sends, 1);

    let back = send_queue_dao::get_entry(&pool, "s1")
        .await
        .unwrap()
        .expect("条目应当还在");
    assert_eq!(
        back.state,
        SendState::Uncertain,
        "中断的发送是「不知道」，不是「没发成功」"
    );
}

#[tokio::test]
async fn an_operation_on_a_deleted_folder_is_failed() {
    let pool = store().await;
    // 故意不建 folder-1。
    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Queued,
        100,
        None,
        None,
    )
    .await;

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");
    assert_eq!(report.failed_orphans, 1);

    let (state, kind, _) = state_of(&pool, "op-1").await;
    assert_eq!(state, OperationState::Failed);
    assert_eq!(kind.as_deref(), Some("folder_missing"));
}

#[tokio::test]
async fn an_operation_on_a_stale_epoch_is_quarantined() {
    let pool = store().await;
    // 文件夹当前是 epoch 200，操作绑定的是 100。
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(200)))
        .await
        .unwrap();

    enqueue(
        &pool,
        "op-1",
        OperationKind::Flags,
        OperationState::Queued,
        100,
        None,
        None,
    )
    .await;

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");
    assert_eq!(report.quarantined, 1);

    let (state, kind, _) = state_of(&pool, "op-1").await;
    assert_eq!(state, OperationState::Failed);
    assert_eq!(
        kind.as_deref(),
        Some("epoch_changed"),
        "旧 UID 会指向另一封信，必须隔离而不是套用"
    );
}

#[tokio::test]
async fn a_move_whose_destination_is_gone_is_failed() {
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();

    // 目标文件夹不存在。
    enqueue(
        &pool,
        "op-1",
        OperationKind::Move,
        OperationState::Queued,
        100,
        Some("folder-gone"),
        None,
    )
    .await;

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");
    assert_eq!(report.failed_orphans, 1);

    let (state, kind, _) = state_of(&pool, "op-1").await;
    assert_eq!(state, OperationState::Failed);
    assert_eq!(kind.as_deref(), Some("destination_missing"));
}

#[tokio::test]
async fn a_healthy_queued_operation_survives_reconcile_untouched() {
    // 对账不能误伤正常排队的意图——它是「整理残骸」，不是「清理队列」。
    let pool = store().await;
    folder_dao::upsert_folder(&pool, &folder("folder-1", "INBOX", Some(100)))
        .await
        .unwrap();
    folder_dao::upsert_folder(&pool, &folder("folder-archive", "Archive", Some(100)))
        .await
        .unwrap();

    enqueue(
        &pool,
        "op-1",
        OperationKind::Move,
        OperationState::Queued,
        100,
        Some("folder-archive"),
        None,
    )
    .await;

    let report = reconcile_account_as(&pool, "acct-1", "boot-b", 10)
        .await
        .expect("对账应当成功");
    assert!(report.is_noop(), "健康队列不该被对账动过：{report:?}");

    let (state, kind, _) = state_of(&pool, "op-1").await;
    assert_eq!(state, OperationState::Queued);
    assert_eq!(kind, None);
}
