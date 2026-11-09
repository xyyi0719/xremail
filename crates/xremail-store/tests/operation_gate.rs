//! `xremail-store` 的写路径操作队列（`mail_operation`）测试。
//!
//! 测试预期来自**「一个意图恰好执行一次」这条语义**，不来自实现本身
//! （见 [AI 开发工作规范 §1.3](../../../docs/AI_DEVELOPMENT_GUIDE.md)）。
//!
//! 覆盖的不变量（R34）：
//! * **同一封邮件的移动 / 删除只留一条在途**：重复入队不新增行，
//!   已有那条的 `seq` / 状态 / 次数一概不动。
//! * **同一封邮件同时只能有一条在途**：库里本来就有两条（历史遗留数据）时，
//!   第二个认领者必须失败——否则两条 flush 各认领一条、各发一次 `MOVE`，
//!   目标文件夹里会出两份。
//! * **互斥按邮件算，不按文件夹或 kind 算**：不同邮件互不影响，
//!   移动与删除互相挡；flag 通道**不参与**（有自己的合并逻辑）。
//! * **终态不挡新意图**：`failed` 之后要能再入队，否则用户重试永远被一条
//!   放弃的旧意图挡住。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, FlagIntentPayload, Folder, MailOperation, MessageFlags, OperationKind,
    OperationState, ProviderKind, ServerConfig, SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::{account, folder, operation};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};

/// 测试用的 epoch。换 epoch 就是「另一批 UID」。
const EPOCH: i64 = 42;

/// 建一个已应用迁移的内存库。
async fn fresh_pool() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("应用迁移失败");
    pool
}

/// 造一个测试账户。
fn sample_account(id: &str) -> Account {
    Account {
        id: id.to_string(),
        email: format!("{id}@example.com"),
        provider: ProviderKind::Netease163,
        display_name: Some(format!("账户 {id}")),
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
        username: format!("{id}@example.com"),
        secret_ref: Some(format!("keychain::{id}")),
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

/// 造一个测试文件夹。
fn sample_folder(id: &str, account_id: &str, remote_name: &str) -> Folder {
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
        uidvalidity: Some(EPOCH),
        role: None,
    }
}

/// 建好账户 + 收件箱 + 归档（常用的一组前置条件）。
async fn seeded_pool() -> SqlitePool {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .expect("账户应当写入");
    for (id, name) in [("inbox", "INBOX"), ("archive", "Archive")] {
        folder::upsert_folder(&pool, &sample_folder(id, "a1", name))
            .await
            .expect("文件夹应当写入");
    }
    pool
}

/// 造一条**还没入队**的操作。
fn transfer(
    id: &str,
    folder_id: &str,
    uid: i64,
    kind: OperationKind,
    destination: Option<&str>,
) -> MailOperation {
    MailOperation {
        id: id.to_string(),
        account_id: "a1".to_string(),
        folder_id: folder_id.to_string(),
        uid,
        uidvalidity: EPOCH,
        kind,
        intent: FlagIntentPayload::default(),
        destination_folder_id: destination.map(str::to_string),
        state: OperationState::Queued,
        // `seq` 由存储层分配，这里先占位。
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 1,
        updated_at: 1,
    }
}

/// 库里 `mail_operation` 的行数。
async fn count_rows(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM mail_operation"#)
        .fetch_one(pool)
        .await
        .expect("统计行数失败")
}

/// 取一行操作（按 id）。
async fn load(pool: &SqlitePool, id: &str) -> Option<MailOperation> {
    operation::get_operation(pool, id)
        .await
        .expect("读取操作失败")
}

// ===========================================================================
// 入队去重（R34）
// ===========================================================================

#[tokio::test]
async fn enqueuing_the_same_move_twice_keeps_only_one_operation() {
    let pool = seeded_pool().await;

    let first = transfer("op-1", "inbox", 7, OperationKind::Move, Some("archive"));
    operation::enqueue_operation(&pool, &first)
        .await
        .expect("首次入队应当成功");

    // 用户又点了一次「移动」：不同的 id、目标还更远一点。
    let second = transfer("op-2", "inbox", 7, OperationKind::Move, Some("archive"));
    operation::enqueue_operation(&pool, &second)
        .await
        .expect("重复入队是正常结果，不是错误");

    assert_eq!(
        count_rows(&pool).await,
        1,
        "同一封邮件的移动意图只该留一条：两条会被两个 flush 各发一次 MOVE"
    );

    let stored = load(&pool, "op-1").await.expect("第一条应当还在");
    assert_eq!(stored.seq, 1, "已有那条的 seq 不该被动过");

    // 去重是按**邮件**查得回来的：调用方据此把「同一条」还给界面。
    let pending = operation::find_pending_transfer(&pool, "a1", "inbox", 7, EPOCH)
        .await
        .expect("查询应当成功")
        .expect("应当有一条在途操作");
    assert_eq!(pending.id, "op-1", "回读到的应当是真正生效的那一条");
}

#[tokio::test]
async fn a_queued_move_also_absorbs_a_later_delete() {
    // 移动与删除是同一个通道：「把这封信搬走」。
    // 两条都在途时，一条把信搬去归档、另一条把源文件夹的信删掉——
    // 谁先谁后都难看，所以第二条**不该**入队。
    let pool = seeded_pool().await;

    operation::enqueue_operation(
        &pool,
        &transfer("op-move", "inbox", 7, OperationKind::Move, Some("archive")),
    )
    .await
    .expect("移动入队应当成功");

    operation::enqueue_operation(
        &pool,
        &transfer("op-del", "inbox", 7, OperationKind::Delete, None),
    )
    .await
    .expect("重复入队是正常结果");

    assert_eq!(count_rows(&pool).await, 1);
    assert!(load(&pool, "op-del").await.is_none(), "删除意图不该被插入");
}

#[tokio::test]
async fn after_a_finished_transfer_a_new_one_can_be_enqueued() {
    // 终态不该挡新意图：上一轮的移动做完了，用户还能再移一次。
    let pool = seeded_pool().await;

    operation::enqueue_operation(
        &pool,
        &transfer("op-1", "inbox", 7, OperationKind::Move, Some("archive")),
    )
    .await
    .expect("首次入队应当成功");
    operation::update_operation_state(&pool, "op-1", OperationState::Succeeded, None, None, 2)
        .await
        .expect("改状态应当成功");

    operation::enqueue_operation(
        &pool,
        &transfer("op-2", "inbox", 7, OperationKind::Move, Some("archive")),
    )
    .await
    .expect("上一轮已完结，这次应当真的插入");

    assert_eq!(count_rows(&pool).await, 2);
    let second = load(&pool, "op-2").await.expect("第二条应当在库里");
    assert_eq!(second.seq, 2, "seq 在文件夹内继续递增");
}

#[tokio::test]
async fn different_mails_and_different_kinds_are_not_deduplicated() {
    let pool = seeded_pool().await;

    // 三个维度各换一个：不同 UID、不同文件夹、不同 epoch。
    for (id, folder_id, uid, epoch) in [
        ("op-a", "inbox", 7, EPOCH),
        ("op-b", "inbox", 8, EPOCH),
        ("op-c", "archive", 7, EPOCH),
    ] {
        let mut op = transfer(id, folder_id, uid, OperationKind::Move, Some("archive"));
        op.uidvalidity = epoch;
        operation::enqueue_operation(&pool, &op)
            .await
            .expect("不同邮件应当各自入队");
    }

    // 同 UID、不同 epoch：UID 只在 epoch 内有效，所以这是「另一封邮件」。
    let mut other_epoch = transfer("op-d", "inbox", 7, OperationKind::Move, Some("archive"));
    other_epoch.uidvalidity = EPOCH + 1;
    operation::enqueue_operation(&pool, &other_epoch)
        .await
        .expect("换 epoch 的 UID 是另一封邮件");

    assert_eq!(
        count_rows(&pool).await,
        4,
        "四封（或四个 epoch 下）的邮件互不干扰"
    );
}

#[tokio::test]
async fn flag_operations_are_not_merged_into_the_transfer_channel() {
    // flag 通道有自己的合并逻辑（`apply_intents` 按 seq 定序、以服务器快照重算），
    // 在这里把它和移动/删除合并会丢掉用户的意图。所以两者互不挡。
    let pool = seeded_pool().await;

    operation::enqueue_operation(
        &pool,
        &transfer("op-move", "inbox", 7, OperationKind::Move, Some("archive")),
    )
    .await
    .expect("移动入队应当成功");

    let mut flag_op = transfer("op-flag", "inbox", 7, OperationKind::Flags, None);
    flag_op.intent = FlagIntentPayload::new(MessageFlags(MessageFlags::SEEN), MessageFlags(0));
    operation::enqueue_operation(&pool, &flag_op)
        .await
        .expect("flag 意图不该被移动意图挡住");

    assert_eq!(count_rows(&pool).await, 2, "flag 与移动各留一条");

    // 反过来：已经有在途的 flag，也不该挡住移动。
    let mut another_flag = transfer("op-flag-2", "inbox", 9, OperationKind::Flags, None);
    another_flag.intent = FlagIntentPayload::new(MessageFlags(0), MessageFlags(MessageFlags::SEEN));
    operation::enqueue_operation(&pool, &another_flag)
        .await
        .expect("flag 入队应当成功");
    operation::enqueue_operation(
        &pool,
        &transfer(
            "op-move-9",
            "inbox",
            9,
            OperationKind::Move,
            Some("archive"),
        ),
    )
    .await
    .expect("移动不该被 flag 意图挡住");
    assert_eq!(count_rows(&pool).await, 4);
}

#[tokio::test]
async fn contradictory_intent_is_still_rejected() {
    // 去重只挡「重复」，不该顺手放过**错的**意图。
    let pool = seeded_pool().await;

    let mut op = transfer("op-bad", "inbox", 7, OperationKind::Flags, None);
    op.intent = FlagIntentPayload::new(
        MessageFlags(MessageFlags::SEEN),
        MessageFlags(MessageFlags::SEEN),
    );

    let err = operation::enqueue_operation(&pool, &op)
        .await
        .expect_err("自相矛盾的意图应当被拒绝");
    assert!(!err.to_string().is_empty());
    assert_eq!(count_rows(&pool).await, 0, "被拒绝的意图不该落库");
}

// ===========================================================================
// 认领互斥（R34）
// ===========================================================================

/// 直接写两行「历史遗留」的同邮件操作（绕过 `enqueue_operation` 的去重）。
///
/// 真实场景：升级前的老库，或两条入队分别走在两个进程里。
async fn insert_legacy_rows(pool: &SqlitePool) {
    for (id, seq) in [("legacy-1", 1_i64), ("legacy-2", 2_i64)] {
        sqlx::query!(
            r#"
            INSERT INTO mail_operation (
                id, account_id, folder_id, uid, uidvalidity,
                kind, intent, destination_folder_id, state, seq, base_version,
                attempts, error_kind, error_message, created_at, updated_at
            ) VALUES (?, 'a1', 'inbox', 7, ?, 'move', '{"add":0,"remove":0}', 'archive', 'queued', ?, NULL, 0, NULL, NULL, 1, 1)
            "#,
            id,
            EPOCH,
            seq
        )
        .execute(pool)
        .await
        .expect("插入历史遗留行失败");
    }
}

#[tokio::test]
async fn only_one_of_two_legacy_rows_for_the_same_mail_can_be_claimed() {
    // 这条钉的正是 R34 的现场：`claim_for_push` 是按行认领的，
    // 若只看「这一行能不能认领」，两条都会被认领、各发一次 MOVE。
    let pool = seeded_pool().await;
    insert_legacy_rows(&pool).await;

    let first = operation::claim_for_push(&pool, "legacy-1", "boot-a", 5, 10)
        .await
        .expect("认领应当成功");
    let second = operation::claim_for_push(&pool, "legacy-2", "boot-b", 5, 10)
        .await
        .expect("认领应当成功（返回 false，不是报错）");

    assert!(first, "第一条应当被认领");
    assert!(
        !second,
        "同一封邮件的第二条不该被认领——那会让两次 flush 各发一次 MOVE，目标里出两份"
    );

    let blocked = load(&pool, "legacy-2").await.expect("第二条应当还在");
    assert_eq!(
        blocked.state,
        OperationState::Queued,
        "被挡下的那条要留在原地等下一轮，不能悄悄变成别的状态"
    );
    assert_eq!(blocked.attempts, 0, "没认领到就不该消耗尝试次数");
}

#[tokio::test]
async fn the_gate_is_released_once_the_running_one_finishes() {
    // 闸门不能是「永久锁」：前一条有了结局（这里是失败），后一条就得能上。
    let pool = seeded_pool().await;
    insert_legacy_rows(&pool).await;

    assert!(
        operation::claim_for_push(&pool, "legacy-1", "boot-a", 5, 10)
            .await
            .expect("认领应当成功")
    );
    operation::mark_operation_retryable(&pool, "legacy-1", "boot-a", "timeout", "超时", 11)
        .await
        .expect("标记可重试应当成功");

    assert!(
        operation::claim_for_push(&pool, "legacy-2", "boot-b", 5, 12)
            .await
            .expect("认领应当成功"),
        "排在后面的那条现在应当能上了"
    );

    // 再回头认领 legacy-1：legacy-2 在跑，所以它又被挡下。
    assert!(
        !operation::claim_for_push(&pool, "legacy-1", "boot-a", 5, 13)
            .await
            .expect("认领应当成功"),
        "同一封邮件同一时刻只能有一条在途"
    );
}

#[tokio::test]
async fn the_gate_is_per_mail_not_per_folder_or_kind() {
    let pool = seeded_pool().await;

    // 同一文件夹里的**不同**邮件：互不影响。
    for (id, uid) in [("op-7", 7), ("op-8", 8)] {
        operation::enqueue_operation(
            &pool,
            &transfer(id, "inbox", uid, OperationKind::Move, Some("archive")),
        )
        .await
        .expect("入队应当成功");
    }
    assert!(
        operation::claim_for_push(&pool, "op-7", "boot-a", 5, 10)
            .await
            .expect("认领应当成功")
    );
    assert!(
        operation::claim_for_push(&pool, "op-8", "boot-b", 5, 10)
            .await
            .expect("认领应当成功"),
        "不同邮件不该互相挡：那会把整个文件夹的写入串成一队"
    );

    // 同一封邮件的 flag 操作：不受移动/删除的闸门约束。
    let mut flag_op = transfer("op-flag", "inbox", 7, OperationKind::Flags, None);
    flag_op.intent = FlagIntentPayload::new(MessageFlags(MessageFlags::SEEN), MessageFlags(0));
    operation::enqueue_operation(&pool, &flag_op)
        .await
        .expect("flag 入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-flag", "boot-b", 5, 11)
            .await
            .expect("认领应当成功"),
        "flag 通道有自己的合并逻辑，不该被搬移闸门挡住"
    );
}

#[tokio::test]
async fn a_running_flag_operation_does_not_block_a_move() {
    // 反向：flag 在途不挡移动。
    let pool = seeded_pool().await;

    let mut flag_op = transfer("op-flag", "inbox", 7, OperationKind::Flags, None);
    flag_op.intent = FlagIntentPayload::new(MessageFlags(MessageFlags::SEEN), MessageFlags(0));
    operation::enqueue_operation(&pool, &flag_op)
        .await
        .expect("flag 入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-flag", "boot-a", 5, 10)
            .await
            .expect("认领应当成功")
    );

    operation::enqueue_operation(
        &pool,
        &transfer("op-move", "inbox", 7, OperationKind::Move, Some("archive")),
    )
    .await
    .expect("入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-move", "boot-b", 5, 11)
            .await
            .expect("认领应当成功"),
        "在途的 flag 不该挡住搬移"
    );
}

// ===========================================================================
// 租约：本进程自己卡住的 running（R32）
// ===========================================================================

#[tokio::test]
async fn a_running_operation_of_this_process_is_reclaimed_after_its_lease() {
    // 现场：认领成功之后，写终态之前某个 `?` 早退或 panic 了。
    // 那条行停在 `running` 且 `owner_boot` 是**自己**——`reclaim_running`
    // 只收别人的，`expire_exhausted` 排除 `running`，于是它在本进程里永久卡死。
    let pool = seeded_pool().await;
    operation::enqueue_operation(
        &pool,
        &transfer("op-1", "inbox", 7, OperationKind::Move, Some("archive")),
    )
    .await
    .expect("入队应当成功");

    let claimed_at = 1_000_000;
    assert!(
        operation::claim_for_push(&pool, "op-1", "boot-self", 5, claimed_at)
            .await
            .expect("认领应当成功")
    );

    // 租约内：别动它。它可能只是还在跑。
    let early = operation::reclaim_stale_running(
        &pool,
        "a1",
        "boot-self",
        operation::RUNNING_LEASE_MS,
        claimed_at + 5,
    )
    .await
    .expect("回收应当成功");
    assert_eq!(early, 0, "租约没过期就不该动它");
    assert_eq!(
        load(&pool, "op-1").await.expect("应当还在").state,
        OperationState::Running
    );

    // 过了租约：收成 `uncertain`，让下一轮回读去核对。
    let reclaimed = operation::reclaim_stale_running(
        &pool,
        "a1",
        "boot-self",
        operation::RUNNING_LEASE_MS,
        // 恰好到期就收：边界取「不早于」，与「租约内不动」合起来正好无缝。
        claimed_at + operation::RUNNING_LEASE_MS,
    )
    .await
    .expect("回收应当成功");
    assert_eq!(reclaimed, 1);

    let row = load(&pool, "op-1").await.expect("应当还在");
    assert_eq!(
        row.state,
        OperationState::Uncertain,
        "结果未知才是诚实的处置：命令可能已经到了服务器"
    );
    assert_eq!(row.error_kind.as_deref(), Some("lease_expired"));
    assert_eq!(row.attempts, 0, "回收不该烧掉尝试次数");
}

#[tokio::test]
async fn the_lease_only_covers_rows_this_process_claimed() {
    // 别人的行归 `reclaim_running` 管（它会先收走），这里不能越权——
    // 尤其是「别的进程还活着、正在提交」的时候。
    let pool = seeded_pool().await;
    operation::enqueue_operation(
        &pool,
        &transfer("op-1", "inbox", 7, OperationKind::Move, Some("archive")),
    )
    .await
    .expect("入队应当成功");
    assert!(
        operation::claim_for_push(&pool, "op-1", "boot-other", 5, 10)
            .await
            .expect("认领应当成功")
    );

    let reclaimed = operation::reclaim_stale_running(&pool, "a1", "boot-self", 0, 10)
        .await
        .expect("回收应当成功");
    assert_eq!(reclaimed, 0, "不是本进程认领的行，租约不该碰");
    assert_eq!(
        load(&pool, "op-1").await.expect("应当还在").state,
        OperationState::Running
    );
}
