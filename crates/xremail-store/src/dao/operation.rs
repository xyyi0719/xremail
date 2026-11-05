//! 本地待提交操作的 DAO。
//!
//! 对应 `mail_operation` 表。它是**用户意图与服务器现实之间的缓冲**：
//! 用户在本机的每一次标记变更先落在这里，同步层再把它们提交给服务器
//! 并确认（见 [ARCHITECTURE §5 与 §6](../../../../docs/ARCHITECTURE.md)）。
//!
//! ## 状态机
//!
//! ```text
//! queued ──提交成功──> succeeded
//!   │
//!   ├──可重试的失败──> retryable ──再试──> ...
//!   │
//!   ├──不该重试────> failed
//!   │
//!   ├──认领提交────> running ──> succeeded / retryable / failed / uncertain
//!   │
//!   └──不知道执行了没> uncertain
//! ```
//!
//! `running` 是「有人认领了，正发给服务器」的**短暂**状态。它只说明认领，
//! 不说明认领者还活着（`owner_boot`，见迁移 `0006_operation_owner.sql`）。
//! 进程死在 `running` 上，重启后由 `reconcile` 收成 `uncertain`——
//! **不直接重试**，因为命令可能已经到达服务器。
//!
//! `uncertain` 尤其重要：命令发出去了但没收到确认，
//! **不能盲目重试**——可能已经执行过了。
//!
//! ## 闸门是「每封邮件」，不是「每条操作」
//!
//! 移动 / 删除类（`OperationKind::Move` / `OperationKind::Delete`）有两条
//! **每封邮件级**的约束，两层合起来才顶用（R34）：
//!
//! 1. **入队去重**（[`enqueue_operation`]）：同一封邮件已有未完结的同通道操作时不再
//!    插入第二条——挡住「同一动作被点了两次」。
//! 2. **认领互斥**（[`claim_for_push`]）：同一时刻只放行一条 `running`——挡住两条
//!    flush 并发时各认领一条、各发一次 `MOVE`（目标文件夹里出两份）。
//!
//! 只有第 1 层不够：两条 flush 可能同时认领**不同**的行（历史遗留的重复行就摆在库里）。
//! 只有第 2 层也不够：重复行会一直留在队列里，一条做完另一条再去碰一封已经不存在的邮件。
//!
//! flag 通道（`OperationKind::Flags`）**不参与**这两条约束：它有自己的合并逻辑
//! （按 `seq` 定序、以服务器快照为准，见 [ARCHITECTURE §6](../../../../docs/ARCHITECTURE.md)），
//! 多行 flag 意图叠在一起是合法的。

use sqlx::FromRow;
use sqlx::SqlitePool;
use xremail_core::{AppError, AppResult};
use xremail_core::model::{FlagIntentPayload, MailOperation, OperationKind, OperationState};

use crate::codec::{addresses_from_json, addresses_to_json, enum_from_text, enum_to_text};
use crate::error::map_sqlx;

/// `enqueue_operation` 撞上 `idx_mail_operation_seq` 唯一索引时的重试次数。
///
/// 并发取 `MAX(seq) + 1` 有两处会撞：另一个入队者先提交了同一个 `seq`
/// （唯一索引兜住），或者自己升级写锁时拿到 `SQLITE_BUSY_SNAPSHOT`。
/// 两种都不是**数据**错误，是「再来一次就好」，所以重试；
/// 用尽仍失败时按 `Storage`（可重试）报错，不 panic、不静默丢意图。
const ENQUEUE_SEQ_RETRIES: u32 = 8;

/// 移动 / 删除通道的 `kind` 取值：**每封邮件同时只能有一条在途**的判定依据。
///
/// 两个 `kind` 都是「把这封信从源文件夹搬走」的意图——不管目标是另一个文件夹
/// 还是垃圾箱。同一封邮件上排两条，并发 flush 时就会各发一次 `MOVE`
/// （没有 `MOVE` 的服务器上是 `COPY` + `STORE \Deleted`），目标里多出两份。
///
/// flag 通道**刻意不在**这里：它的载荷可以安全地叠在一起（按 `seq` 定序、
/// 以服务器快照重算净变化），强行合并反而会丢掉用户的意图。
const TRANSFER_KIND_VALUES: &str = "'move', 'delete'";

/// `mail_operation` 表的一行。
#[derive(Debug, Clone, FromRow)]
#[sqlx(rename_all = "snake_case")]
struct OperationRow {
    id: String,
    account_id: String,
    folder_id: String,
    uid: i64,
    uidvalidity: i64,
    kind: String,
    intent: String,
    destination_folder_id: Option<String>,
    state: String,
    seq: i64,
    base_version: Option<i64>,
    attempts: i64,
    error_kind: Option<String>,
    error_message: Option<String>,
    created_at: i64,
    updated_at: i64,
}

/// 入队一条操作。
///
/// `seq` 由存储层分配：同一账户 + 文件夹内**严格递增**。
/// 调用方不要自己填——填重了会撞上唯一索引。
///
/// 意图自相矛盾（同一 bit 既置位又清除）时**直接拒绝**：
/// 这种意图说明调用方有 bug，静默挑一个语义会掩盖它。
pub async fn enqueue_operation(pool: &SqlitePool, operation: &MailOperation) -> AppResult<()> {
    if operation.intent.is_contradictory() {
        return Err(xremail_core::AppError::Invalid(format!(
            "操作 {} 的意图自相矛盾：同一 bit 既要置位又要清除",
            operation.id
        )));
    }

    let kind = enum_to_text(&operation.kind, "operation.kind")?;
    let intent = addresses_to_json(&operation.intent, "operation.intent")?;
    let state = enum_to_text(&operation.state, "operation.state")?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| map_sqlx(e, "enqueue_operation: begin"))?;

    // 分配序号：同一账户 + 文件夹内的最大值 + 1。
    let next_seq: i64 = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(MAX(seq), 0) + 1 FROM mail_operation
        WHERE account_id = ? AND folder_id = ?
        "#,
        operation.account_id,
        operation.folder_id
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| map_sqlx(e, "enqueue_operation: next seq"))?;

    sqlx::query!(
        r#"
        INSERT INTO mail_operation (
            id, account_id, folder_id, uid, uidvalidity,
            kind, intent, destination_folder_id, state, seq, base_version,
            attempts, error_kind, error_message, created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        operation.id,
        operation.account_id,
        operation.folder_id,
        operation.uid,
        operation.uidvalidity,
        kind,
        intent,
        operation.destination_folder_id,
        state,
        next_seq,
        operation.base_version,
        operation.attempts,
        operation.error_kind,
        operation.error_message,
        operation.created_at,
        operation.updated_at,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| map_sqlx(e, "enqueue_operation"))?;

    tx.commit()
        .await
        .map_err(|e| map_sqlx(e, "enqueue_operation: commit"))?;

    Ok(())
}

/// 列出某个文件夹里**还没完结**的操作。
///
/// 按 `seq` 升序——提交时按这个顺序重放意图。
///
/// ## 为什么 `uncertain` 也在里面
///
/// `uncertain` = 「命令发出去了，但没收到确认」。它**还没完结**：
/// 服务器上到底有没有生效，至今不知道。
///
/// 如果把它排除在外，这条意图就**永远没人管了**——
/// 用户点的星标既没提交成功，也不会被再提交一次，
/// 界面还会把本地视图弹回服务器的样子（像没点过）。
///
/// 捞出来不等于「盲目重试」：提交层会**先读服务器快照再重算净变化**
/// （见 `xremail_sync::push_flags`）。服务器上已经有了 → 一条命令都不发，
/// 直接算成功；没有 → 这才发。
/// 所以重放是**幂等**的，安全性由「以服务器为准」保证，
/// 而不是靠「不敢重试」把意图晾在那里。
pub async fn list_pending_operations(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
) -> AppResult<Vec<MailOperation>> {
    let rows = sqlx::query_as!(
        OperationRow,
        r#"
        SELECT id, account_id, folder_id, uid, uidvalidity,
               kind, intent, destination_folder_id, state, seq, base_version,
               attempts, error_kind, error_message, created_at, updated_at
        FROM mail_operation
        WHERE account_id = ? AND folder_id = ?
          AND state IN ('queued', 'running', 'retryable', 'uncertain')
        ORDER BY seq ASC
        "#,
        account_id,
        folder_id
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_pending_operations"))?;

    rows.iter().map(row_to_operation).collect()
}

/// 按 id 取一条操作。
///
/// 不存在时返回 `Ok(None)`——「没有这条操作」不是错误。
///
/// 用途：回写一条操作后，调用方要能读回**这一条**的状态
/// （`push_flags` 只给汇总报告，见 `xremail_sync::push_flags`）。
pub async fn get_operation(pool: &SqlitePool, id: &str) -> AppResult<Option<MailOperation>> {
    let rows = sqlx::query_as!(
        OperationRow,
        r#"
        SELECT id, account_id, folder_id, uid, uidvalidity,
               kind, intent, destination_folder_id, state, seq, base_version,
               attempts, error_kind, error_message, created_at, updated_at
        FROM mail_operation
        WHERE id = ?
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "get_operation"))?;

    match rows {
        Some(row) => Ok(Some(row_to_operation(&row)?)),
        None => Ok(None),
    }
}

/// 列出某个账户里**全部**未完结的操作。
///
/// epoch 切换时用它找出绑定旧 epoch 的那些（要隔离的）。
pub async fn list_all_pending_operations(
    pool: &SqlitePool,
    account_id: &str,
) -> AppResult<Vec<MailOperation>> {
    let rows = sqlx::query_as!(
        OperationRow,
        r#"
        SELECT id, account_id, folder_id, uid, uidvalidity,
               kind, intent, destination_folder_id, state, seq, base_version,
               attempts, error_kind, error_message, created_at, updated_at
        FROM mail_operation
        WHERE account_id = ?
          AND state IN ('queued', 'running', 'retryable', 'uncertain')
        ORDER BY seq ASC
        "#,
        account_id
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_all_pending_operations"))?;

    rows.iter().map(row_to_operation).collect()
}

/// 认领一条操作：`queued` / `retryable` / `uncertain` → `running`，并记下归属。
///
/// 这是**唯一的提交权闸门**：两个提交者同时认领同一行，只有一个能改到行，
/// 另一个拿到 `false`（什么都不做，也**不算失败**——不是它认领的）。
///
/// ## 为什么认领时不自增 `attempts`
///
/// 尝试次数要表达的是「失败了几次」。若认领也自增，那么一次失败会烧掉两个配额，
/// `attempts` 就不再是失败次数而是「失败次数 + 认领次数」，
/// 正常抖动就会被提前判成 `retry_exhausted`。
/// 所以自增点只有 [`mark_operation_retryable`] 和 [`mark_operation_uncertain`]
/// 两处，都是「确实尝试过一次且结果不明/失败」的收尾。
///
/// 配额由 `AND attempts < ?` 卡住：到顶的行认领失败，由对账统一收成
/// `failed / retry_exhausted`（见 `xremail_sync::reconcile`）。
pub async fn claim_for_push(
    pool: &SqlitePool,
    id: &str,
    boot_id: &str,
    max_attempts: i64,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE mail_operation
        SET state = 'running', started_at = ?, owner_boot = ?, updated_at = ?
        WHERE id = ?
          AND state IN ('queued', 'retryable', 'uncertain')
          AND attempts < ?
        "#,
        now_ms,
        boot_id,
        now_ms,
        id,
        max_attempts
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "claim_for_push"))?;

    Ok(result.rows_affected() > 0)
}

/// 启动对账：把「**不是本 boot 留下的**」`running` 收成 `uncertain`。
///
/// ## 为什么不是无条件扫所有 `running`
///
/// `owner_boot` 守卫是这一条的全部价值：同一台机器上的另一个进程
/// （或同一进程里另一个提交者）正在提交的行**不是残骸**。
/// 无条件扫会把活着的操作翻成「结果未知」，让用户白确认一次。
///
/// `owner_boot IS NULL` 必须单独判——老数据没有归属，按「不是我的」处理，
/// 否则升级上来的库永远扫不到那批行（那正是本 TODO 要修的东西）。
///
/// 收成 `uncertain` 而不是 `queued`/`retryable`：命令可能已经到服务器了，
/// 让下一轮回读去核对（`push_flags` / `push_transfers` 的幂等性），
/// 而不是替服务器判断「没执行」然后重发一次。
pub async fn reclaim_running(
    pool: &SqlitePool,
    account_id: &str,
    boot_id: &str,
    now_ms: i64,
) -> AppResult<usize> {
    let result = sqlx::query!(
        r#"
        UPDATE mail_operation
        SET state = 'uncertain',
            error_kind = 'interrupted',
            error_message = ?,
            owner_boot = NULL,
            started_at = NULL,
            updated_at = ?
        WHERE account_id = ?
          AND state = 'running'
          AND (owner_boot IS NULL OR owner_boot <> ?)
        "#,
        "上次提交过程中进程中断，结果未知",
        now_ms,
        account_id,
        boot_id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "reclaim_running"))?;

    Ok(result.rows_affected() as usize)
}

/// 把尝试次数到顶的未完结操作收成 `failed / retry_exhausted`。
///
/// 单独扫而不是在提交路径里顺手判：中途某处顺手标 `failed` 会多出第二套口径，
/// 而「什么时候算彻底放弃」应该是**一个人能一眼看全**的规则。
///
/// 不收 `running`：那一批要么属于本 boot 的在途操作（不该动），
/// 要么已经被 [`reclaim_running`] 先收成了 `uncertain`（对账的顺序保证）。
pub async fn expire_exhausted(
    pool: &SqlitePool,
    account_id: &str,
    max_attempts: i64,
    now_ms: i64,
) -> AppResult<usize> {
    let result = sqlx::query!(
        r#"
        UPDATE mail_operation
        SET state = 'failed',
            error_kind = 'retry_exhausted',
            error_message = ?,
            updated_at = ?
        WHERE account_id = ?
          AND state IN ('queued', 'retryable', 'uncertain')
          AND attempts >= ?
        "#,
        "重试次数已用完，操作放弃（可能是服务器一直拒绝这条意图）",
        now_ms,
        account_id,
        max_attempts
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "expire_exhausted"))?;

    Ok(result.rows_affected() as usize)
}

/// 更新一条操作的状态。
///
/// 只允许在**未完结**的状态之间迁移：`succeeded` / `failed` 是终态，
/// 不许被改回去。没有这条守卫时，一条已经确认成功的操作可能被后到的
/// 失败分支覆盖成 `retryable`，然后重放一遍——那是靠时序碰运气的正确性。
pub async fn update_operation_state(
    pool: &SqlitePool,
    id: &str,
    state: OperationState,
    error_kind: Option<&str>,
    error_message: Option<&str>,
    now_ms: i64,
) -> AppResult<bool> {
    let state_text = enum_to_text(&state, "operation.state")?;

    let result = sqlx::query!(
        r#"
        UPDATE mail_operation
        SET state = ?, error_kind = ?, error_message = ?, updated_at = ?
        WHERE id = ?
          AND state IN ('queued', 'running', 'retryable', 'uncertain')
        "#,
        state_text,
        error_kind,
        error_message,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "update_operation_state"))?;

    Ok(result.rows_affected() > 0)
}

/// 增加一条操作的尝试次数，并置为可重试。
///
/// 自增点只有本函数和 [`mark_operation_uncertain`]。若认领（[`claim_for_push`]）
/// 也自增，一次失败会烧掉两个配额，正常抖动就会被提前判成 `retry_exhausted`。
///
/// 同样只允许从未完结状态迁过来：终态不能被一次后到的失败覆盖成 `retryable`。
pub async fn mark_operation_retryable(
    pool: &SqlitePool,
    id: &str,
    error_kind: &str,
    error_message: &str,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE mail_operation
        SET state = 'retryable', attempts = attempts + 1,
            error_kind = ?, error_message = ?, updated_at = ?
        WHERE id = ?
          AND state IN ('queued', 'running', 'retryable', 'uncertain')
        "#,
        error_kind,
        error_message,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "mark_operation_retryable"))?;

    Ok(result.rows_affected() > 0)
}

/// 把一条操作收成「结果不明」，**并增加尝试次数**。
///
/// ## 为什么要自增
///
/// `uncertain` 与 `retryable` 一样，都表示「这一轮真的发了命令，但没拿到可信
/// 的结果」，下一轮会被 `claim_for_push` 重新认领（它的 `state IN (...)` 含
/// `uncertain`）。原先这条路径用 [`update_operation_state`] 收尾，虽然把状态写成
/// 了 `uncertain`，但 **`attempts` 不动**——于是 `claim_for_push` 的
/// `AND attempts < ?` 对它永远成立，一条老发不出去的 `copy+delete` 会**每轮
/// 重发一次**，而 `COPY` 不幂等（163 没有 `MOVE`）：目标文件夹里就会不断多出
/// 副本。次数不涨就没有任何东西能让这个循环停下来。
///
/// 与 [`mark_operation_retryable`] 的唯一区别是落到 `uncertain` 而不是
/// `retryable`：前者是「确认失败」，后者是「不知道成没成」，下游对二者的处理
/// 不同（`uncertain` 要先回读核对）。
pub async fn mark_operation_uncertain(
    pool: &SqlitePool,
    id: &str,
    error_kind: &str,
    error_message: &str,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE mail_operation
        SET state = 'uncertain', attempts = attempts + 1,
            error_kind = ?, error_message = ?, updated_at = ?
        WHERE id = ?
          AND state IN ('queued', 'running', 'retryable', 'uncertain')
        "#,
        error_kind,
        error_message,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "mark_operation_uncertain"))?;

    Ok(result.rows_affected() > 0)
}

/// 统计某个账户里处于某状态的操作数。
pub async fn count_operations_in_state(
    pool: &SqlitePool,
    account_id: &str,
    state: OperationState,
) -> AppResult<i64> {
    let state_text = enum_to_text(&state, "operation.state")?;

    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) FROM mail_operation
        WHERE account_id = ? AND state = ?
        "#,
        account_id,
        state_text
    )
    .fetch_one(pool)
    .await
    .map_err(|e| map_sqlx(e, "count_operations_in_state"))?;

    Ok(count)
}

/// 行 → 领域对象。
fn row_to_operation(row: &OperationRow) -> AppResult<MailOperation> {
    let kind: OperationKind = enum_from_text(&row.kind, "operation.kind")?;
    let intent: FlagIntentPayload = addresses_from_json(&row.intent, "operation.intent")?;
    let state: OperationState = enum_from_text(&row.state, "operation.state")?;

    Ok(MailOperation {
        id: row.id.clone(),
        account_id: row.account_id.clone(),
        folder_id: row.folder_id.clone(),
        uid: row.uid,
        uidvalidity: row.uidvalidity,
        kind,
        intent,
        destination_folder_id: row.destination_folder_id.clone(),
        state,
        seq: row.seq,
        base_version: row.base_version,
        attempts: row.attempts,
        error_kind: row.error_kind.clone(),
        error_message: row.error_message.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}
