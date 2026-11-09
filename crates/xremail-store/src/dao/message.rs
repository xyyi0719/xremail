//! 邮件 DAO。
//!
//! 对应 `message` 表。**只放邮件的元数据**——正文在 `message_body` 表，
//! 列文件夹时不会碰到大文本列（见 [ARCHITECTURE §4.2](../../../../docs/ARCHITECTURE.md)）。
//!
//! ## 去重键是四元组，不是 Message-ID
//!
//! `(account_id, folder_id, uidvalidity, uid)` 才是远端邮件的身份。
//! RFC 5322 的 `Message-ID` **仅作线索**：可能缺失、不唯一、跨文件夹也不唯一
//! （见 [ARCHITECTURE §4.1](../../../../docs/ARCHITECTURE.md)）。
//! schema 上有对应的 `UNIQUE` 约束，这里按它做 upsert。

use sqlx::FromRow;
use sqlx::SqlitePool;
use xremail_core::AppResult;
use xremail_core::html::html_to_text;
use xremail_core::model::{Message, MessageBody};

use crate::codec::{addresses_from_json, addresses_to_json};
use crate::error::map_sqlx;

/// `message` 表的一行。
#[derive(Debug, Clone, FromRow)]
#[sqlx(rename_all = "snake_case")]
struct MessageRow {
    id: String,
    account_id: String,
    folder_id: String,
    uid: i64,
    uidvalidity: i64,
    message_id: Option<String>,
    subject: Option<String>,
    snippet: Option<String>,
    from_addr: String,
    to_addr: String,
    cc_addr: String,
    bcc_addr: String,
    date_sent: Option<i64>,
    internal_date: Option<i64>,
    flags: i64,
    size: Option<i64>,
    has_attachment: bool,
}

/// 插入或更新一封邮件。
///
/// 冲突键是 `(account_id, folder_id, uidvalidity, uid)`。
/// `id` 是**本地** ID：冲突时**保留**原来的 `id`，
/// 否则 `message_body` 等以 `message_id` 为外键的表会被孤立。
pub async fn upsert_message(pool: &SqlitePool, message: &Message) -> AppResult<()> {
    let from_addr = addresses_to_json(&message.from, "message.from")?;
    let to_addr = addresses_to_json(&message.to, "message.to")?;
    let cc_addr = addresses_to_json(&message.cc, "message.cc")?;
    let bcc_addr = addresses_to_json(&message.bcc, "message.bcc")?;

    sqlx::query!(
        r#"
        INSERT INTO message (
            id, account_id, folder_id, uid, uidvalidity,
            message_id, subject, snippet,
            from_addr, to_addr, cc_addr, bcc_addr,
            date_sent, internal_date, flags, size, has_attachment
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (account_id, folder_id, uidvalidity, uid) DO UPDATE SET
            message_id     = excluded.message_id,
            subject        = excluded.subject,
            snippet        = excluded.snippet,
            from_addr      = excluded.from_addr,
            to_addr        = excluded.to_addr,
            cc_addr        = excluded.cc_addr,
            bcc_addr       = excluded.bcc_addr,
            date_sent      = excluded.date_sent,
            internal_date  = excluded.internal_date,
            flags          = excluded.flags,
            size           = excluded.size,
            has_attachment = excluded.has_attachment
        "#,
        message.id,
        message.account_id,
        message.folder_id,
        message.uid,
        message.uidvalidity,
        message.message_id,
        message.subject,
        message.snippet,
        from_addr,
        to_addr,
        cc_addr,
        bcc_addr,
        message.date_sent,
        message.internal_date,
        i64::from(message.flags.bits()),
        message.size,
        message.has_attachment,
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "upsert_message"))?;

    Ok(())
}

/// 批量插入或更新（在**一个事务**里）。
///
/// 一次同步拉到的邮件必须**整体成功或整体失败**：
/// 半截数据会让下一次的 UID 差分把没写进去的邮件当成"服务器已删"。
pub async fn upsert_messages(pool: &SqlitePool, messages: &[Message]) -> AppResult<()> {
    if messages.is_empty() {
        return Ok(());
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| map_sqlx(e, "upsert_messages: begin"))?;

    for message in messages {
        let from_addr = addresses_to_json(&message.from, "message.from")?;
        let to_addr = addresses_to_json(&message.to, "message.to")?;
        let cc_addr = addresses_to_json(&message.cc, "message.cc")?;
        let bcc_addr = addresses_to_json(&message.bcc, "message.bcc")?;

        sqlx::query!(
            r#"
            INSERT INTO message (
                id, account_id, folder_id, uid, uidvalidity,
                message_id, subject, snippet,
                from_addr, to_addr, cc_addr, bcc_addr,
                date_sent, internal_date, flags, size, has_attachment
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (account_id, folder_id, uidvalidity, uid) DO UPDATE SET
                message_id     = excluded.message_id,
                subject        = excluded.subject,
                snippet        = excluded.snippet,
                from_addr      = excluded.from_addr,
                to_addr        = excluded.to_addr,
                cc_addr        = excluded.cc_addr,
                bcc_addr       = excluded.bcc_addr,
                date_sent      = excluded.date_sent,
                internal_date  = excluded.internal_date,
                flags          = excluded.flags,
                size           = excluded.size,
                has_attachment = excluded.has_attachment
            "#,
            message.id,
            message.account_id,
            message.folder_id,
            message.uid,
            message.uidvalidity,
            message.message_id,
            message.subject,
            message.snippet,
            from_addr,
            to_addr,
            cc_addr,
            bcc_addr,
            message.date_sent,
            message.internal_date,
            i64::from(message.flags.bits()),
            message.size,
            message.has_attachment,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map_sqlx(e, "upsert_messages"))?;
    }

    tx.commit()
        .await
        .map_err(|e| map_sqlx(e, "upsert_messages: commit"))?;

    Ok(())
}

/// 列出某个文件夹里已缓存的 UID。
///
/// **只取 UID**，不取正文——差分只需要知道"有哪些"。
pub async fn list_message_uids(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
    uidvalidity: i64,
) -> AppResult<Vec<i64>> {
    let uids = sqlx::query_scalar!(
        r#"
        SELECT uid FROM message
        WHERE account_id = ? AND folder_id = ? AND uidvalidity = ?
        ORDER BY uid ASC
        "#,
        account_id,
        folder_id,
        uidvalidity
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_message_uids"))?;

    Ok(uids)
}

/// 列出某个文件夹里的邮件（按 UID 升序）。
pub async fn list_messages(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
) -> AppResult<Vec<Message>> {
    let rows = sqlx::query_as!(
        MessageRow,
        r#"
        SELECT id, account_id, folder_id, uid, uidvalidity,
               message_id, subject, snippet,
               from_addr, to_addr, cc_addr, bcc_addr,
               date_sent, internal_date, flags, size, has_attachment
        FROM message
        WHERE account_id = ? AND folder_id = ?
        ORDER BY uid ASC
        "#,
        account_id,
        folder_id
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_messages"))?;

    rows.iter().map(row_to_message).collect()
}

/// 按本地 ID 取一封邮件。
pub async fn get_message(pool: &SqlitePool, id: &str) -> AppResult<Option<Message>> {
    let row = sqlx::query_as!(
        MessageRow,
        r#"
        SELECT id, account_id, folder_id, uid, uidvalidity,
               message_id, subject, snippet,
               from_addr, to_addr, cc_addr, bcc_addr,
               date_sent, internal_date, flags, size, has_attachment
        FROM message
        WHERE id = ?
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "get_message"))?;

    row.as_ref().map(row_to_message).transpose()
}

/// 删掉一批 UID 对应的邮件，返回删掉几封。
///
/// 用于「服务器上已经没有的邮件」——它们是缓存，没有独立价值。
pub async fn delete_messages_by_uids(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
    uidvalidity: i64,
    uids: &[i64],
) -> AppResult<usize> {
    if uids.is_empty() {
        return Ok(0);
    }

    // 按 UID 逐个删：SQLite 不支持 `IN` 的参数数组，
    // 而一次同步里被删的邮件通常只有个位数。
    let mut deleted: u64 = 0;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| map_sqlx(e, "delete_messages: begin"))?;

    for uid in uids {
        let result = sqlx::query!(
            r#"
            DELETE FROM message
            WHERE account_id = ? AND folder_id = ? AND uidvalidity = ? AND uid = ?
            "#,
            account_id,
            folder_id,
            uidvalidity,
            uid
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map_sqlx(e, "delete_messages"))?;
        deleted += result.rows_affected();
    }

    tx.commit()
        .await
        .map_err(|e| map_sqlx(e, "delete_messages: commit"))?;

    Ok(deleted as usize)
}

/// 取某封信的 `Message-ID`，供**幂等探测**用。
///
/// **为什么需要它**：`COPY` 不幂等。一条 `copy+delete` 操作若在「命令已经发出、
/// 确认还没读到」之间断网或崩溃，重发就会在目标文件夹里多出一份副本（163 就没有
/// `MOVE`，只能走这条路）。重发之前先用 `Message-ID` 去目标文件夹问一句「这封在
/// 不在」，在就直接收工。取不到 `Message-ID` 就**不许**重发。
///
/// 回 `None` 有两种含义：邮件不在本地（已被别的路径删了），或者服务器没给
/// `Message-ID`。调用方对这两种情况一律按「不能确认」处理。
pub async fn find_message_id_by_uid(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
    uidvalidity: i64,
    uid: i64,
) -> AppResult<Option<String>> {
    let message_id = sqlx::query_scalar!(
        r#"
        SELECT message_id
        FROM message
        WHERE account_id = ? AND folder_id = ? AND uidvalidity = ? AND uid = ?
        "#,
        account_id,
        folder_id,
        uidvalidity,
        uid
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "find_message_id_by_uid"))?;

    // 外层的 `None` = 没有这行；内层的 `None` = 这行没 `Message-ID`。
    Ok(message_id.flatten())
}

/// 删掉某个文件夹里的**全部**邮件，返回删掉几封。
///
/// ## 这一条当前**没有调用方**，而且语义危险
///
/// 注释曾经写「用于 epoch 切换」——**那是错的**（2026-11-10 核实）。
/// epoch 切换走的是 [`super::sync::commit_folder_sync`] 里那段**事务内**的
/// `DELETE … WHERE uidvalidity != ?`：它只删**旧世代**，和新拉下来的那批
/// 在同一个事务里一起提交，中途失败就整体回滚。
///
/// 而这个函数是「**全删**」：连当前世代一起删。如果在 epoch 切换时误用它，
/// 会先把本地清空、再等同步重新拉——中间任何一步失败，用户看到的就是
/// 「邮箱突然空了」。
///
/// 之所以保留而不是删掉：它是个干净的单表操作，将来若有「清空某个文件夹的
/// 本地缓存」（用户主动触发、带确认的那种）可以用上。**但在那之前：
/// 想清旧世代就用 `commit_folder_sync`，不要用这个。**
pub async fn delete_messages_in_folder(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
) -> AppResult<usize> {
    let result = sqlx::query!(
        "DELETE FROM message WHERE account_id = ? AND folder_id = ?",
        account_id,
        folder_id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "delete_messages_in_folder"))?;

    Ok(result.rows_affected() as usize)
}

/// 改一封邮件的标记。
pub async fn update_message_flags(
    pool: &SqlitePool,
    id: &str,
    flags: xremail_core::model::MessageFlags,
) -> AppResult<bool> {
    let result = sqlx::query!(
        "UPDATE message SET flags = ? WHERE id = ?",
        i64::from(flags.bits()),
        id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "update_message_flags"))?;

    Ok(result.rows_affected() > 0)
}

/// `message_body` 表的一行。
#[derive(Debug, Clone, FromRow)]
#[sqlx(rename_all = "snake_case")]
struct MessageBodyRow {
    message_id: String,
    text_plain: Option<String>,
    html_raw: Option<String>,
    charset: Option<String>,
}

/// 写入（或覆盖）一封邮件的正文。
///
/// 正文**单独存**——列文件夹时不会碰到大文本列
/// （见 [ARCHITECTURE §4.2](../../../../docs/ARCHITECTURE.md)）。
///
/// ## 纯文本缺失时从 HTML 派生
///
/// 真实邮箱发出来的邮件**大多是 HTML-only**（实测 163：纯文本部分为空）。
/// 不派生的话，这封邮件的正文**一个字都搜不到**——搜索等于只搜标题。
///
/// 派生出来的文本会跟着 FTS 触发器进索引（`message_search_body_ai` /
/// `message_search_body_au`），所以这里写进去就够了，索引那边**不用管**。
pub async fn upsert_message_body(pool: &SqlitePool, body: &MessageBody) -> AppResult<()> {
    let text_plain = match body.text_plain.as_deref() {
        Some(text) if !text.trim().is_empty() => body.text_plain.clone(),
        _ => body
            .html_raw
            .as_deref()
            .map(html_to_text)
            .filter(|text| !text.trim().is_empty()),
    };

    sqlx::query!(
        r#"
        INSERT INTO message_body (message_id, text_plain, html_raw, charset)
        VALUES (?, ?, ?, ?)
        ON CONFLICT (message_id) DO UPDATE SET
            text_plain = excluded.text_plain,
            html_raw   = excluded.html_raw,
            charset    = excluded.charset
        "#,
        body.message_id,
        text_plain,
        body.html_raw,
        body.charset,
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "upsert_message_body"))?;

    Ok(())
}

/// 给**已有**的 HTML-only 邮件补派生文本。
///
/// 写路径已经会派生（见 [`upsert_message_body`]），但那规则是后加的——
/// **升级之前**就入库的邮件 `text_plain` 仍然是空的，搜不到。
///
/// 启动时补一次：只碰「HTML 有、纯文本空」的行，
/// 正常邮件一个都不动。
///
/// 更新 `message_body` 会触发 `message_search_body_au`，
/// 索引里的正文**跟着一起补上**。
///
/// 返回补了多少封。
pub async fn backfill_derived_text(pool: &SqlitePool) -> AppResult<usize> {
    let rows = sqlx::query!(
        r#"
        SELECT message_id, html_raw
        FROM message_body
        WHERE html_raw IS NOT NULL
          AND (text_plain IS NULL OR TRIM(text_plain) = '')
        "#
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "backfill_derived_text.select"))?;

    let mut patched = 0usize;
    for row in rows {
        let Some(html) = row.html_raw.as_deref() else {
            continue;
        };
        let text = html_to_text(html);
        if text.trim().is_empty() {
            continue;
        }
        sqlx::query!(
            r#"
            UPDATE message_body
            SET text_plain = ?
            WHERE message_id = ?
            "#,
            text,
            row.message_id,
        )
        .execute(pool)
        .await
        .map_err(|e| map_sqlx(e, "backfill_derived_text.update"))?;
        patched += 1;
    }

    Ok(patched)
}

/// 取一封邮件的正文。
pub async fn get_message_body(
    pool: &SqlitePool,
    message_id: &str,
) -> AppResult<Option<MessageBody>> {
    let row = sqlx::query_as!(
        MessageBodyRow,
        r#"
        SELECT message_id, text_plain, html_raw, charset
        FROM message_body
        WHERE message_id = ?
        "#,
        message_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "get_message_body"))?;

    Ok(row.map(|row| MessageBody {
        message_id: row.message_id,
        text_plain: row.text_plain,
        html_raw: row.html_raw,
        charset: row.charset,
    }))
}

/// 统计某账户里已经存了多少封正文。
pub async fn count_message_bodies(pool: &SqlitePool, account_id: &str) -> AppResult<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) FROM message_body
        WHERE message_id IN (SELECT id FROM message WHERE account_id = ?)
        "#,
        account_id
    )
    .fetch_one(pool)
    .await
    .map_err(|e| map_sqlx(e, "count_message_bodies"))?;

    Ok(count)
}

/// 按远端身份（账户 + 文件夹 + epoch + UID）改一封邮件的标记。
///
/// 同步层回写标记后用它把本地视图对齐到**服务器确认过的状态**。
pub async fn update_message_flags_by_uid(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
    uidvalidity: i64,
    uid: i64,
    flags: xremail_core::model::MessageFlags,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE message SET flags = ?
        WHERE account_id = ? AND folder_id = ? AND uidvalidity = ? AND uid = ?
        "#,
        i64::from(flags.bits()),
        account_id,
        folder_id,
        uidvalidity,
        uid
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "update_message_flags_by_uid"))?;

    Ok(result.rows_affected() > 0)
}

/// 按远端身份更新「有附件」标记。
pub async fn update_has_attachment_by_uid(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
    uidvalidity: i64,
    uid: i64,
    has_attachment: bool,
) -> AppResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE message SET has_attachment = ?
        WHERE account_id = ? AND folder_id = ? AND uidvalidity = ? AND uid = ?
        "#,
        has_attachment,
        account_id,
        folder_id,
        uidvalidity,
        uid
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "update_has_attachment_by_uid"))?;

    Ok(result.rows_affected() > 0)
}

/// 统计某个文件夹里缓存了几封邮件。
pub async fn count_messages(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
) -> AppResult<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) FROM message
        WHERE account_id = ? AND folder_id = ?
        "#,
        account_id,
        folder_id
    )
    .fetch_one(pool)
    .await
    .map_err(|e| map_sqlx(e, "count_messages"))?;

    Ok(count)
}

/// 行 → 领域对象。
fn row_to_message(row: &MessageRow) -> AppResult<Message> {
    Ok(Message {
        id: row.id.clone(),
        account_id: row.account_id.clone(),
        folder_id: row.folder_id.clone(),
        uid: row.uid,
        uidvalidity: row.uidvalidity,
        message_id: row.message_id.clone(),
        subject: row.subject.clone(),
        snippet: row.snippet.clone(),
        from: addresses_from_json(&row.from_addr, "message.from")?,
        to: addresses_from_json(&row.to_addr, "message.to")?,
        cc: addresses_from_json(&row.cc_addr, "message.cc")?,
        bcc: addresses_from_json(&row.bcc_addr, "message.bcc")?,
        date_sent: row.date_sent,
        internal_date: row.internal_date,
        flags: xremail_core::model::MessageFlags(row.flags as u32),
        size: row.size,
        has_attachment: row.has_attachment,
    })
}
