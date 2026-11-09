//! 发信队列的 DAO。
//!
//! 对应 `send_queue` 表（`0001_init.sql` 建表，`0003_send_queue.sql` 扩展）。
//! 它是**用户写好的信**与**服务器收没收**之间的缓冲（见
//! [ARCHITECTURE §5](../../../../docs/ARCHITECTURE.md)）。
//!
//! ## 状态机
//!
//! ```text
//! draft ──构建邮件字节──> queued ──发送器取走──> running ──250──> succeeded
//!   ▲                       │                      │
//!   │                       │                      ├──4xx / 连接失败──> retryable
//!   │                       │                      ├──5xx───────────> failed
//!   │                       │                      └──应答丢失──────> uncertain
//!   └───用户改内容──────────┴──────────────────────────────┘
//! ```
//!
//! `uncertain` 尤其重要：**不能自动重试**。
//! 邮件发出去之后，服务器上不会留下一条「这封信已被接受」的记录供回读——
//! `DATA` 之后应答丢了就是「不知道」。盲目重发就是重复投递。
//!
//! ## 状态迁移是 CAS
//!
//! 每个 `mark_*` / `claim_*` 都是 `UPDATE ... WHERE id = ? AND state = ...`，
//! 返回「有没有真的改到行」。**返回的行数就是并发闸门**：
//! 两个发送器同时取同一条邮件，只有一个能把 `queued` 改成 `running`，
//! 另一个拿到 `false` 就知道「别人在处理」。
//!
//! ## 重试次数是**真的**有上限（R31）
//!
//! 成功的路径认领时 `attempts` 加一（见 [`claim_for_sending`]），
//! 所以 `attempts` 之后就是「这封信被真的提交了几次」。
//! 认领条件里有 `AND attempts < ?`，调用方给的就是上限。
//!
//! 到顶之后这条**不会再被认领**，调用方把它收成 `failed / retry_exhausted`
//! 并要求人工改内容（见 [`fail_if_exhausted`]）。
//!
//! > 改动前的 `attempts` 是个只增不查的死计数器：认领条件里没有上界，
//! > 全仓也没有别处检查——「重试 N 次」是一句空头承诺。
//! > 另有一个**真的**不用这条路径的出口：`uncertain` 经用户确认后走
//! > [`retry_uncertain`]，那是「人替服务器做判断」，不是自动重试，
//! > 所以它**不**重置 `attempts`，也不受上限约束（一事一议）。
//!
//! ## envelope 与 headers 分离
//!
//! `send_queue.envelope` 存的是**实际投递地址**（from + 全部收件人 + bcc），
//! `send_queue.headers` 存的是**显示用头部**（To / Cc / Subject，**没有 Bcc**）。
//! 两者分开是 Bcc 不泄漏、也不破坏 To/Cc 的前提（ARCHITECTURE §5）。

use serde::Deserialize;
use serde::Serialize;
use sqlx::FromRow;
use sqlx::SqlitePool;
use xremail_core::AppError;
use xremail_core::AppResult;
use xremail_core::model::{SendAttachment, SendQueueEntry, SendState};

use crate::codec::{enum_from_text, enum_to_text};
use crate::error::map_sqlx;

/// 一条发信最多真的提交几次。
///
/// **放在存储层**：因为这个数字是 `claim_for_sending` 的**条件**——
/// 写进文档、却不在认领时检查，就是一句空头承诺（R31）。
/// 上层只需要照着它传，不需要自己再记一个「应该取几」。
///
/// 取 5，与同步引擎的 `MAX_OPERATION_ATTEMPTS`
/// （`crates/xremail-sync/src/reconcile.rs`）同一个理由：足以扛过网络抖动、
/// 服务器临时 4xx 与维护窗口，又不会让一个永远发不出去的信无限重试。
///
/// 到顶之后不自动再发：`failed / retry_exhausted` **要求人工介入**——
/// 见 [`fail_if_exhausted`] 里对出路的说明。
pub const MAX_SEND_ATTEMPTS: i64 = 5;

/// `send_queue` 表的一行（不含附件字节）。
#[derive(Debug, Clone, FromRow)]
#[sqlx(rename_all = "snake_case")]
struct SendQueueRow {
    id: String,
    account_id: String,
    envelope: String,
    headers: String,
    body_text: Option<String>,
    body_html: Option<String>,
    message_bytes: Option<Vec<u8>>,
    message_id: Option<String>,
    idempotency_key: Option<String>,
    state: String,
    attempts: i64,
    error_kind: Option<String>,
    error_message: Option<String>,
    created_at: i64,
    updated_at: i64,
}

/// `send_queue.envelope` 列的 JSON 形状。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvelopeJson {
    /// 发件人（SMTP 的 MAIL FROM）。
    from: String,
    /// 全部收件人（To ∪ Cc ∪ Bcc 去重）：SMTP 的 RCPT TO 列表。
    recipients: Vec<String>,
    /// 密送：**只进信封，绝不写进邮件头**。
    bcc: Vec<String>,
}

/// `send_queue.headers` 列的 JSON 形状：显示用头部，**不含 Bcc**。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadersJson {
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
    in_reply_to: Option<String>,
    references: Option<String>,
}

// ===========================================================================
// 草稿
// ===========================================================================

/// 保存草稿。
///
/// 返回 `false` 表示**拒绝保存**：这一行已经进入不可编辑的状态
/// （`queued` / `running` / `succeeded` / `uncertain`）。
///
/// ## 为什么拒绝编辑「已排队」的信
///
/// 排队那一刻邮件字节就**冻结**了（`message_bytes`）。
/// 此时再改内容，用户看到的和实际会发出去的就不是同一封信——
/// 比「改不了」严重得多。改内容只能改草稿状态的那一行。
///
/// ## 为什么允许改 `retryable` / `failed` 的信
///
/// 那两种状态的含义是「服务器**明确没有**收下这封信」（4xx/5xx），
/// 所以重发是安全的。改内容会**清掉旧的 `message_bytes`**：
/// 旧字节对应的是旧内容，留着就会把另一封信发出去。
///
/// `uncertain` 不在允许之列：服务器可能已经收下了。
/// 要改就另起一封新的草稿——那是新的一封信，不该复用同一行的身份。
pub async fn save_draft(pool: &SqlitePool, entry: &SendQueueEntry) -> AppResult<bool> {
    if entry.state != SendState::Draft {
        return Err(AppError::Invalid(format!(
            "save_draft 只接受草稿状态，收到 {:?}",
            entry.state
        )));
    }
    if entry.to.is_empty() && entry.cc.is_empty() && entry.bcc.is_empty() {
        return Err(AppError::Invalid("草稿至少要有一个收件人".to_string()));
    }

    let envelope = envelope_to_json(entry)?;
    let headers = headers_to_json(entry)?;

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| map_sqlx(e, "save_draft: begin"))?;

    let result = sqlx::query!(
        r#"
        INSERT INTO send_queue (
            id, account_id, envelope, headers,
            body_text, body_html, message_bytes, message_id, idempotency_key,
            state, attempts, error_kind, error_message, created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?, 'draft', 0, NULL, NULL, ?, ?)
        ON CONFLICT (id) DO UPDATE SET
            envelope = excluded.envelope,
            headers = excluded.headers,
            body_text = excluded.body_text,
            body_html = excluded.body_html,
            message_bytes = NULL,
            message_id = NULL,
            state = 'draft',
            attempts = 0,
            error_kind = NULL,
            error_message = NULL,
            updated_at = excluded.updated_at
        WHERE send_queue.state IN ('draft', 'retryable', 'failed')
        "#,
        entry.id,
        entry.account_id,
        envelope,
        headers,
        entry.body_text,
        entry.body_html,
        entry.idempotency_key,
        entry.created_at,
        entry.updated_at,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| map_sqlx(e, "save_draft"))?;

    if result.rows_affected() == 0 {
        // 状态不允许改：回滚，什么都没动。
        tx.rollback()
            .await
            .map_err(|e| map_sqlx(e, "save_draft: rollback"))?;
        return Ok(false);
    }

    replace_attachments(&mut tx, &entry.id, &entry.attachments).await?;

    tx.commit()
        .await
        .map_err(|e| map_sqlx(e, "save_draft: commit"))?;

    Ok(true)
}

/// 读出一条发信条目（含附件字节）。
///
/// 不存在时返回 `Ok(None)`——「没有这条」不是错误。
pub async fn get_entry(pool: &SqlitePool, id: &str) -> AppResult<Option<SendQueueEntry>> {
    let rows = sqlx::query_as!(
        SendQueueRow,
        r#"
        SELECT id, account_id, envelope, headers,
               body_text, body_html, message_bytes, message_id, idempotency_key,
               state, attempts, error_kind, error_message, created_at, updated_at
        FROM send_queue
        WHERE id = ?
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "get_entry"))?;

    let Some(row) = rows else { return Ok(None) };

    let mut entry = row_to_entry(&row)?;
    entry.attachments = list_attachments(pool, id).await?;
    Ok(Some(entry))
}

/// 列出某个账户的全部发信条目（**不含**附件字节）。
///
/// 顺序：先按状态（未完结的排前面），再按创建时间。
/// 界面上的「发件箱」要能一眼看出哪些还没发出去。
pub async fn list_entries(pool: &SqlitePool, account_id: &str) -> AppResult<Vec<SendQueueEntry>> {
    let rows = sqlx::query_as!(
        SendQueueRow,
        r#"
        SELECT id, account_id, envelope, headers,
               body_text, body_html, message_bytes, message_id, idempotency_key,
               state, attempts, error_kind, error_message, created_at, updated_at
        FROM send_queue
        WHERE account_id = ?
        ORDER BY
            CASE state
                WHEN 'draft' THEN 0
                WHEN 'queued' THEN 1
                WHEN 'running' THEN 2
                WHEN 'retryable' THEN 3
                WHEN 'uncertain' THEN 4
                WHEN 'failed' THEN 5
                ELSE 6
            END,
            created_at ASC
        "#,
        account_id
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_entries"))?;

    rows.iter().map(row_to_entry).collect()
}

/// 列出某个账户里处于指定状态的条目（**不含**附件字节）。
///
/// 发送器用它捞「该发的」：`queued` 与 `retryable`。
pub async fn list_entries_in_states(
    pool: &SqlitePool,
    account_id: &str,
    states: &[SendState],
) -> AppResult<Vec<SendQueueEntry>> {
    if states.is_empty() {
        return Ok(Vec::new());
    }

    let mut texts = Vec::with_capacity(states.len());
    for state in states {
        texts.push(enum_to_text(state, "send_queue.state")?);
    }

    // 状态值是绑定参数（不拼进 SQL 字符串），但「有几个状态」是运行时才知道的，
    // 所以用 QueryBuilder 拼占位符——sqlx 0.9 不再允许裸的动态 SQL 字符串。
    let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        r#"
        SELECT id, account_id, envelope, headers,
               body_text, body_html, message_bytes, message_id, idempotency_key,
               state, attempts, error_kind, error_message, created_at, updated_at
        FROM send_queue
        WHERE account_id =
        "#,
    );
    builder.push_bind(account_id);
    builder.push(" AND state IN (");
    {
        let mut separated = builder.separated(", ");
        for text in &texts {
            separated.push_bind(text);
        }
    }
    builder.push(") ORDER BY created_at ASC");

    let rows = builder
        .build_query_as::<SendQueueRow>()
        .fetch_all(pool)
        .await
        .map_err(|e| map_sqlx(e, "list_entries_in_states"))?;

    rows.iter().map(row_to_entry).collect()
}

/// 删除一条草稿。
///
/// 只有 `draft` 状态能删：已经排队或已经发出去的信，
/// 删掉它只会让「发没发出去」这件事变得无从查证。
pub async fn delete_draft(pool: &SqlitePool, id: &str) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        DELETE FROM send_queue
        WHERE id = ? AND state = 'draft'
        "#,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "delete_draft"))?;

    Ok(result.rows_affected() > 0)
}

// ===========================================================================
// 附件
// ===========================================================================

/// 读出某条目的附件（按 `position` 排序）。
pub async fn list_attachments(pool: &SqlitePool, send_id: &str) -> AppResult<Vec<SendAttachment>> {
    let rows = sqlx::query!(
        r#"
        SELECT filename, content_type, content
        FROM send_attachment
        WHERE send_id = ?
        ORDER BY position ASC
        "#,
        send_id
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_attachments"))?;

    Ok(rows
        .into_iter()
        .map(|row| SendAttachment {
            filename: row.filename,
            content_type: row.content_type,
            bytes: row.content,
        })
        .collect())
}

/// 替换某条目的附件。
///
/// 整份替换而不是增量改：草稿的附件列表本来就是用户手上的一份，
/// 增删改都走「保存整份」这一条路径，最不容易出错。
async fn replace_attachments(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    send_id: &str,
    attachments: &[SendAttachment],
) -> AppResult<()> {
    sqlx::query!(
        r#"
        DELETE FROM send_attachment
        WHERE send_id = ?
        "#,
        send_id
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| map_sqlx(e, "replace_attachments: delete"))?;

    for (position, attachment) in attachments.iter().enumerate() {
        sqlx::query!(
            r#"
            INSERT INTO send_attachment (
                id, send_id, filename, content_type, content, size, position
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
            format!("{send_id}:{position}"),
            send_id,
            attachment.filename,
            attachment.content_type,
            attachment.bytes,
            attachment.bytes.len() as i64,
            position as i64,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| map_sqlx(e, "replace_attachments: insert"))?;
    }

    Ok(())
}

// ===========================================================================
// 状态迁移（全部是 CAS）
// ===========================================================================

/// 把草稿变成「已排队」：邮件字节在这里**冻结**。
///
/// 只接受 `draft` / `retryable` / `failed`：
/// `queued` / `running` 说明有人在处理，`succeeded` 说明已经发出去了，
/// `uncertain` 要人裁决。
///
/// 返回 `false` = 状态不允许（别人抢走了，或状态变了）。
pub async fn queue_for_sending(
    pool: &SqlitePool,
    id: &str,
    message_bytes: &[u8],
    message_id: &str,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'queued',
            message_bytes = ?,
            message_id = ?,
            error_kind = NULL,
            error_message = NULL,
            updated_at = ?
        WHERE id = ?
          AND state IN ('draft', 'retryable', 'failed')
        "#,
        message_bytes,
        message_id,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "queue_for_sending"))?;

    Ok(result.rows_affected() > 0)
}

/// 发送器取走一条：`queued` / `retryable` → `running`，并记下提交归属。
///
/// 这是**唯一的发送权闸门**：两个发送器同时取同一条，
/// 只有一个能改到行（另一个拿到 `false`）。
///
/// `owner_boot` 的作用见迁移 `0006_operation_owner.sql`：
/// 进程死在 `running` 上之后，重启时要能区分「这条是残骸」与
/// 「这条是另一个还活着的进程正在发的」，否则会把活件翻成「结果未知」。
///
/// ## `max_attempts` 是**真的**上界（R31）
///
/// 条件里的 `AND attempts < ?` 意味着：这条已经提交过 `max_attempts` 次时，
/// 认领**失败**，返回 `false`——和「别人正在发」返回同一个值。
/// 调用方**必须先看清楚这条到底是哪种情况再给用户看文案**：
/// 把「重试次数用完」说成「正在被另一个发送器处理」，用户就会一直重试、
/// 一直收到同一句「稍后再试」，而这封**永远不会**发出去。
///
/// 区分办法见 [`fail_if_exhausted`]，`apps/shell` 的两个调用点都是先查它。
///
/// 返回 `false` 时**状态不变**（还是 `queued` / `retryable`）：
/// 本函数不替调用方做处置，因为「超时用完就转 failed」这句话属于
/// 用户可见的语义，应该由它自己说清（带一句人话的原因）。
pub async fn claim_for_sending(
    pool: &SqlitePool,
    id: &str,
    boot_id: &str,
    now_ms: i64,
    max_attempts: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'running',
            attempts = attempts + 1,
            started_at = ?,
            owner_boot = ?,
            updated_at = ?
        WHERE id = ?
          AND state IN ('queued', 'retryable')
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
    .map_err(|e| map_sqlx(e, "claim_for_sending"))?;

    Ok(result.rows_affected() > 0)
}

/// 已经没得再试了，就把这条收成 `failed / retry_exhausted`。
///
/// **用法**：只在 `claim_for_sending` 返回 `false` 之后调一次。
/// 返回 `true` 表示「这条确实是用完次数了，我把它收掉了」——
/// 调用方据此给用户一句**能行动**的话；返回 `false` 则是真正的「别人正在发」。
///
/// ## 为什么要有这个条目，而不是让行停在 `retryable`
///
/// 停在 `retryable` 的话，用户点「发送」只会得到 `claim_for_sending` 的
/// `false`；调用方若把它当成「正在被另一个发送器处理」，用户就会一直重试、
/// 一直被拒绝，而这封信**永远发不出去**，界面上却没有任何一处说清为什么。
///
/// 所以：到顶 = `failed`。`failed` 在界面上是可编辑的状态
/// （用户改内容就回到草稿，见 [`save_draft`]），
/// 也允许直接重新发送（[`queue_for_sending`] 接受 `failed` 并清空错误）。
///
/// 这**不是**判决，而是把选择权交回人：调用方读到 `should_fail`/`failed` 时，
/// 必须给出一句能行动的话，而不是「稍后再试」——后者会让人以为是暂时的事。
///
/// ## 为什么 `error_kind` 是 `retry_exhausted`
///
/// 它与 `interrupted`（进程死在发信中）、`rejected_5xx`、`rejected_4xx`
/// 并列，回答的是同一个问题：「这封信最后为什么没发出去」。
/// 也正因为值不同，界面上才有机会说清「不是服务器拒绝，是我们自己停下来了」。
///
/// ## 为什么一条 SQL 能做完
///
/// `WHERE` 里三段全都要：`state` 卡在 `queued` / `retryable`（
/// 有人正在发的不动、已经发出去的不动、`uncertain` 更不动——
/// 那是等人工裁决的）；`attempts >= ?` 卡在到顶。
/// 认领与收尾是两条语句，中间可能有人抢先认领——但那时 `state` 已是
/// `running`，这条 `UPDATE` 就改不到行，返回 `false`，正是我们要的。
pub async fn fail_if_exhausted(
    pool: &SqlitePool,
    id: &str,
    max_attempts: i64,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'failed',
            error_kind = 'retry_exhausted',
            error_message = ?,
            updated_at = ?
        WHERE id = ?
          AND state IN ('queued', 'retryable')
          AND attempts >= ?
        "#,
        format!(
            "这封信已经提交过 {max_attempts} 次都没成功，已停止自动重试。\n\
             信没有发出去。改一下内容再点「发送」会重新开始计数；\
             也可以先确认账号设置（服务器地址、密码）再试。"
        ),
        now_ms,
        id,
        max_attempts
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "fail_if_exhausted"))?;

    Ok(result.rows_affected() > 0)
}

/// 发送成功（收到 250 终应答）。
pub async fn mark_succeeded(pool: &SqlitePool, id: &str, now_ms: i64) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'succeeded',
            error_kind = NULL,
            error_message = NULL,
            updated_at = ?
        WHERE id = ? AND state = 'running'
        "#,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "mark_succeeded"))?;

    Ok(result.rows_affected() > 0)
}

/// 失败但可重试（连接失败、4xx）。
///
/// 4xx 的意思是「服务器**没有**收下这封信」，所以重发是安全的。
pub async fn mark_retryable(
    pool: &SqlitePool,
    id: &str,
    error_kind: &str,
    error_message: &str,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'retryable',
            error_kind = ?,
            error_message = ?,
            updated_at = ?
        WHERE id = ? AND state = 'running'
        "#,
        error_kind,
        error_message,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "mark_retryable"))?;

    Ok(result.rows_affected() > 0)
}

/// 明确失败且不可自动重试（5xx、地址非法、本地构建失败）。
pub async fn mark_failed(
    pool: &SqlitePool,
    id: &str,
    error_kind: &str,
    error_message: &str,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'failed',
            error_kind = ?,
            error_message = ?,
            updated_at = ?
        WHERE id = ? AND state = 'running'
        "#,
        error_kind,
        error_message,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "mark_failed"))?;

    Ok(result.rows_affected() > 0)
}

/// 不确定：命令发出去了，但没收到最终应答。
///
/// **不能自动重试**——服务器可能已经收下了这封信。
pub async fn mark_uncertain(
    pool: &SqlitePool,
    id: &str,
    error_kind: &str,
    error_message: &str,
    now_ms: i64,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'uncertain',
            error_kind = ?,
            error_message = ?,
            updated_at = ?
        WHERE id = ? AND state = 'running'
        "#,
        error_kind,
        error_message,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "mark_uncertain"))?;

    Ok(result.rows_affected() > 0)
}

/// 进程重启后回收 `running` 的条目 → `uncertain`。
///
/// ## 为什么不是 `retryable`
///
/// `running` 说明上次进程**死在发信途中**。邮件有没有被服务器收下？
/// 不知道——所以是 `uncertain`，不是「没发成功」。
///
/// 把它标成 `retryable` 看起来更省事（直接重发），但那是在赌：
/// 万一服务器已经收下了，用户就会收到两封。
///
/// 宁可让人来看一眼，也不替服务器做这个判断。
///
/// ## `owner_boot` 守卫
///
/// 只收「不是本 boot 留下的」行：另一个还活着的进程正在发的信不是残骸。
/// `owner_boot IS NULL` 单独判——老数据没有归属，按「不是我的」处理，
/// 否则升级上来的库永远收不到那批行。
pub async fn reclaim_running(
    pool: &SqlitePool,
    account_id: &str,
    boot_id: &str,
    now_ms: i64,
) -> AppResult<usize> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'uncertain',
            error_kind = 'interrupted',
            error_message = '上次发送过程中进程中断，结果未知',
            owner_boot = NULL,
            started_at = NULL,
            updated_at = ?
        WHERE account_id = ?
          AND state = 'running'
          AND (owner_boot IS NULL OR owner_boot <> ?)
        "#,
        now_ms,
        account_id,
        boot_id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "reclaim_running"))?;

    Ok(result.rows_affected() as usize)
}

/// 发信队列的 `running` 租约长度（毫秒）。
///
/// 与 [`crate::dao::operation::RUNNING_LEASE_MS`] 同一个数（15 分钟）、同一个理由：
/// 这个值只是「怎么算太久了」的一个宽松上界，不是精度要求。
///
/// 比 `mail_operation` 那边更宽松也不影响正确性：租约只决定「什么时候认为
/// 这次认领已经没人管了」，判早了会把**还在发**的信误判成结果未知（那是更糟的
/// 错误——用户会看到「不确定」而实际上信可能正在发）。15 分钟远大于一次 SMTP
/// 提交的任何合理耗时（命令超时是 30 秒），所以只会收真正的残骸。
pub const RUNNING_LEASE_MS: i64 = 15 * 60 * 1000;

/// 回收**本进程**认领后卡住的 `send_queue` 行（R32 的发信侧）。
///
/// ## 为什么发信也需要这个
///
/// [`reclaim_running`] 只收「**不是本 boot** 留下的」行。本进程自己认领之后、
/// 写终态之前如果 `?` 早退（`require_account`、读附件、取草稿字节……都在
/// `claim_for_sending` 与终态写入之间）或 panic，那条行就停在 `running`
/// 且 `owner_boot` 等于本进程——`reclaim_running` 不管它（那是「我的」），
/// `claim_for_sending` 也拒绝它（`state IN ('queued','retryable')`），
/// 于是**在本次进程的整个生命周期里**这封信再也发不出去，界面只显示
/// 「正在发送」，用户只能重启应用。
///
/// ## 收成 `uncertain` 而不是重排
///
/// 命令可能已经到了服务器（`DATA` 已经发出去、只是终态没写下来）。
/// 重排会让它**再发一封**；判成 `uncertain` 则交给用户决定（与 DATA 后
/// 丢应答那一套语义一致，见 ARCHITECTURE §5）。宁可让用户手工确认一次，
/// 也不要替他发第二封。
///
/// ## 判据为什么写成 `started_at <= ?`
///
/// 与 `operation::reclaim_stale_running` 同一个坑：`sqlx::query!` 会把
/// `INTEGER` 列的类型放宽成 `REAL`，写成 `started_at < now - lease` 时
/// 边界那一毫秒判不出来。必须在 SQL 里化简成与一个已算好的值比较。
pub async fn reclaim_stale_running(
    pool: &SqlitePool,
    account_id: &str,
    boot_id: &str,
    lease_ms: i64,
    now_ms: i64,
) -> AppResult<usize> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'uncertain',
            error_kind = 'lease_expired',
            error_message = ?,
            owner_boot = NULL,
            started_at = NULL,
            updated_at = ?
        WHERE account_id = ?
          AND state = 'running'
          AND owner_boot = ?
          AND started_at IS NOT NULL
          AND started_at <= ?
        "#,
        "发送开始后长时间没有结果，按结果未知处理（可能进程中断在写终态之前）。请先到「已发送」或对方那里确认这封信到底发出去没有，再决定怎么办",
        now_ms,
        account_id,
        boot_id,
        now_ms - lease_ms
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "reclaim_stale_running"))?;

    Ok(result.rows_affected() as usize)
}

/// 用户**显式**决定再发一次：`uncertain` → `queued`。
///
/// 字节不变，所以 `Message-ID` 不变——对端能认出这是同一封，
/// 有机会自己把重复的那份合并掉。
///
/// 这是**人替服务器做的判断**，不是自动重试。
pub async fn retry_uncertain(pool: &SqlitePool, id: &str, now_ms: i64) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE send_queue
        SET state = 'queued',
            error_kind = NULL,
            error_message = NULL,
            updated_at = ?
        WHERE id = ?
          AND state = 'uncertain'
          AND message_bytes IS NOT NULL
        "#,
        now_ms,
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "retry_uncertain"))?;

    Ok(result.rows_affected() > 0)
}

/// 用户**显式**把「结果未确认」的信撤回来继续改：`uncertain` → `draft`。
///
/// ## 和 [`retry_uncertain`] 的区别
///
/// `retry_uncertain` 是「再发一次」（同一份字节，`Message-ID` 不变）；
/// 这个是「**不发了，我要改内容**」——所以冻结的字节必须**清掉**：
/// 留着的话，下次发出去的还是旧内容，而用户以为自己改了。
///
/// 清掉字节之后，这封信和普通草稿没区别了。
///
/// ## 代价
///
/// 万一服务器其实已经收下了这封信，用户改完再发就变成两封——
/// 所以**必须由用户确认**（前端弹二次确认，把这个风险说清楚）。
pub async fn withdraw_to_draft(pool: &SqlitePool, id: &str, now_ms: i64) -> AppResult<bool> {
    // 这条用**运行时查询**而不是 `query!` 宏：宏需要一条离线缓存条目，
    // 而离线缓存是**整仓一起**生成的——开发过程中单独重生成会覆盖别人的。
    // 这条 SQL 很直白（没有列名映射的风险），运行时校验够用。
    let result = sqlx::query(
        r#"
        UPDATE send_queue
        SET state = 'draft',
            message_bytes = NULL,
            message_id = NULL,
            attempts = 0,
            error_kind = NULL,
            error_message = NULL,
            updated_at = ?
        WHERE id = ?
          AND state = 'uncertain'
        "#,
    )
    .bind(now_ms)
    .bind(id)
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "withdraw_to_draft"))?;

    Ok(result.rows_affected() > 0)
}

// ===========================================================================
// 编解码
// ===========================================================================

/// 把条目编码成 `envelope` 列的 JSON。
///
/// 收件人集合由 [`SendQueueEntry::envelope_recipients`] 计算，
/// 调用方**不能**自己填——收件人只有一个来源，免得两处各写一份、越走越偏。
fn envelope_to_json(entry: &SendQueueEntry) -> AppResult<String> {
    let envelope = EnvelopeJson {
        from: entry.from.clone(),
        recipients: entry.envelope_recipients(),
        bcc: entry.bcc.clone(),
    };

    serde_json::to_string(&envelope)
        .map_err(|e| AppError::Invalid(format!("send_queue.envelope 编码失败: {e}")))
}

/// 把条目编码成 `headers` 列的 JSON。
///
/// 这里**没有 Bcc**：显示用头部不该出现密送地址。
fn headers_to_json(entry: &SendQueueEntry) -> AppResult<String> {
    let headers = HeadersJson {
        to: entry.to.clone(),
        cc: entry.cc.clone(),
        subject: entry.subject.clone(),
        in_reply_to: entry.in_reply_to.clone(),
        references: entry.references.clone(),
    };

    serde_json::to_string(&headers)
        .map_err(|e| AppError::Invalid(format!("send_queue.headers 编码失败: {e}")))
}

/// 把数据库行还原成领域类型。
fn row_to_entry(row: &SendQueueRow) -> AppResult<SendQueueEntry> {
    let envelope: EnvelopeJson = serde_json::from_str(&row.envelope)
        .map_err(|e| AppError::Invalid(format!("send_queue.envelope 解析失败: {e}")))?;
    let headers: HeadersJson = serde_json::from_str(&row.headers)
        .map_err(|e| AppError::Invalid(format!("send_queue.headers 解析失败: {e}")))?;
    let state = enum_from_text::<SendState>(&row.state, "send_queue.state")?;

    Ok(SendQueueEntry {
        id: row.id.clone(),
        account_id: row.account_id.clone(),
        from: envelope.from,
        to: headers.to,
        cc: headers.cc,
        bcc: envelope.bcc,
        subject: headers.subject,
        body_text: row.body_text.clone(),
        body_html: row.body_html.clone(),
        // 附件单独读：列附件字节会把列表查询变成几十兆的读。
        attachments: Vec::new(),
        in_reply_to: headers.in_reply_to,
        references: headers.references,
        state,
        idempotency_key: row.idempotency_key.clone().unwrap_or_default(),
        message_bytes: row.message_bytes.clone(),
        message_id: row.message_id.clone(),
        attempts: row.attempts,
        error_kind: row.error_kind.clone(),
        error_message: row.error_message.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}
