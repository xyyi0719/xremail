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
//!   └──不知道执行了没> uncertain
//! ```
//!
//! `uncertain` 尤其重要：命令发出去了但没收到确认，
//! **不能盲目重试**——可能已经执行过了。

use sqlx::FromRow;
use sqlx::SqlitePool;
use xremail_core::AppResult;
use xremail_core::model::{FlagIntentPayload, MailOperation, OperationKind, OperationState};

use crate::codec::{addresses_from_json, addresses_to_json, enum_from_text, enum_to_text};
use crate::error::map_sqlx;

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

/// 更新一条操作的状态。
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
