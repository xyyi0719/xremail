//! 附件 DAO。
//!
//! 对应 `attachment` 表（0001 建表、0008 补了 `part_key` / `downloaded_at` /
//! `created_at`）。领域的形状是 [`xremail_core::model::Attachment`]。
//!
//! ## 只存元数据与「本地文件在哪」，不存字节
//!
//! 附件字节落在**文件系统**（`app_data_dir/attachments/…`），表里只记路径与 sha256。
//! 字节进 SQLite 会让库里出现几 MB 的大行，列文件夹、备份、VACUUM 都要跟着变慢。
//!
//! ## 重拉正文 = 替换结构、不丢下载状态
//!
//! 同一封邮件可能被打开多次（本地没正文时按需去服务器拉）。每次拉到的 MIME 结构
//! 按 `(message_id, part_key)` 对齐：结构变了就更新元数据，
//! **但** `local_path` / `sha256` / `fetched` / `downloaded_at` 在部件还是同一个时**保留**——
//! 否则用户下过的附件在第二次打开时又变成「未下载」。
//!
//! 结构里**消失**的行（真实邮件极少变，但服务器可能换了内容）直接删掉，
//! 免得界面上出现一个点开就报错的残留项。
//!
//! `created_at` 只在行**第一次**插入时写（`ON CONFLICT … DO UPDATE` 不碰它），
//! 所以它记的是「这个部件第一次被看见」的时间，重拉不会把它刷新成现在。

use std::collections::HashSet;

use sqlx::FromRow;
use sqlx::SqlitePool;
use xremail_core::model::Attachment;

use crate::error::map_sqlx;

/// `attachment` 表的一行。
#[derive(Debug, Clone, FromRow)]
#[sqlx(rename_all = "snake_case")]
struct AttachmentRow {
    id: String,
    message_id: String,
    filename: Option<String>,
    mime: Option<String>,
    size: Option<i64>,
    content_id: Option<String>,
    inline: bool,
    local_path: Option<String>,
    sha256: Option<String>,
    fetched: bool,
    part_key: Option<String>,
    downloaded_at: Option<i64>,
    transfer_encoding: Option<String>,
}

/// `attachment` 表里那些**不属于领域模型**的列，只有测试与诊断用得上。
///
/// 领域模型（[`Attachment`]）里没有 `created_at`：界面不显示它，业务也不依赖它；
/// 但它是「重拉不覆盖」这条语义的一部分，需要有个办法断言。
#[derive(Debug, Clone, FromRow)]
pub struct AttachmentTimestamps {
    /// 这一行第一次被写入的时间。
    pub created_at: Option<i64>,
    /// 落盘完成的时间（没下载过就是 `None`）。
    pub downloaded_at: Option<i64>,
}

/// 取某个附件的时间戳（`created_at` / `downloaded_at`），不存在时返回 `None`。
pub async fn attachment_timestamps(
    pool: &SqlitePool,
    message_id: &str,
    attachment_id: &str,
) -> crate::StoreResult<Option<AttachmentTimestamps>> {
    let row = sqlx::query_as!(
        AttachmentTimestamps,
        r#"
        SELECT created_at, downloaded_at
        FROM attachment
        WHERE message_id = ? AND id = ?
        "#,
        message_id,
        attachment_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "attachment_timestamps"))?;

    Ok(row)
}

/// 按 MIME 结构**替换**一封邮件的附件元数据。
///
/// `attachments` 里每一项的 `part_key` 必须有值（解析侧一定会给）。
/// 返回写进去的条数。
///
/// 语义见模块文档：形状按 `(message_id, part_key)` 对齐，下载状态保留，
/// 消失的行删除。整个过程在**一笔事务**里完成。
///
/// ## 为什么要在这里把下载状态**钉住**（R55）
///
/// 「重拉正文不丢下载状态」原先只靠「`ON CONFLICT DO UPDATE` 的 `SET` 列表里
/// 没有那四列」来保证——也就是说，它是一条**写给读者看的约定**，数据库里
/// 没有任何东西拦住「某天有人往 `SET` 里补一行」。一旦补上，一次重拉正文就会把
/// 用户已经下载过的附件清成 `fetched = 0`（文件还在盘上，界面却显示未下载），
/// 或者反过来只写 `local_path` 不写 `fetched`——那正是 0008 迁移里
/// 「`fetched` 与 `local_path` 必须同时成立」那句注释要防的半截状态。
///
/// 所以 INSERT 里这四列一律写 `NULL` / `0`（新行本来就没下载过），
/// 而 `ON CONFLICT` 的 `SET` 只在**行还没下载**时才采纳新解析结果：
/// `attachment.fetched = 1` 时 `CASE` 把四个字段全部写回旧值，
/// 不管 `SET` 里写了什么都改不动它。
pub async fn replace_attachments(
    pool: &SqlitePool,
    message_id: &str,
    attachments: &[Attachment],
    now_ms: i64,
) -> crate::StoreResult<usize> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| map_sqlx(e, "replace_attachments.begin"))?;

    let mut keep: Vec<String> = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let Some(part_key) = attachment.part_key.as_deref() else {
            // 没有部件路径就无从对齐；跳过而不是写一条永远对不上的行。
            continue;
        };
        keep.push(part_key.to_string());

        sqlx::query!(
            r#"
            -- 下载状态**只**能由 `mark_downloaded` 写（见函数文档 R55）：
            -- 解析侧传来的 local_path / sha256 / fetched / downloaded_at
            -- 即便有值也不是「这个文件已经落盘」的证据，新行一律写成「未下载」。
            INSERT INTO attachment (
                id, message_id, filename, mime, size, content_id, "inline",
                part_key, created_at, transfer_encoding
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (message_id, part_key) DO UPDATE SET
                filename   = excluded.filename,
                mime       = excluded.mime,
                -- size 是**解码后的字节数 / 落盘字节数**：已经下载过（fetched=1）
                -- 就以本地文件为准，只有没下载时才采用这次解析出来的长度。
                size       = CASE
                    WHEN attachment.fetched THEN attachment.size
                    ELSE excluded.size
                END,
                content_id = excluded.content_id,
                "inline"   = excluded."inline",
                transfer_encoding = excluded.transfer_encoding,
                -- 下载状态四列：已下载的行一律写回旧值（DB 层钉住，不靠 SET 列表漏写）。
                local_path    = CASE
                    WHEN attachment.fetched THEN attachment.local_path
                    ELSE excluded.local_path
                END,
                sha256        = CASE
                    WHEN attachment.fetched THEN attachment.sha256
                    ELSE excluded.sha256
                END,
                fetched       = CASE
                    WHEN attachment.fetched THEN attachment.fetched
                    ELSE excluded.fetched
                END,
                downloaded_at = CASE
                    WHEN attachment.fetched THEN attachment.downloaded_at
                    ELSE excluded.downloaded_at
                END
            "#,
            attachment.id,
            message_id,
            attachment.filename,
            attachment.mime,
            attachment.size,
            attachment.content_id,
            attachment.inline,
            part_key,
            now_ms,
            attachment.transfer_encoding,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map_sqlx(e, "replace_attachments.upsert"))?;
    }

    // 结构里没有了的部件 → 删掉。
    let existing = sqlx::query!(
        r#"SELECT part_key FROM attachment WHERE message_id = ? AND part_key IS NOT NULL"#,
        message_id
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| map_sqlx(e, "replace_attachments.list"))?;
    let keep: HashSet<&str> = keep.iter().map(String::as_str).collect();
    for row in existing {
        let Some(part_key) = row.part_key.as_deref() else {
            continue;
        };
        if keep.contains(part_key) {
            continue;
        }
        sqlx::query!(
            r#"DELETE FROM attachment WHERE message_id = ? AND part_key = ?"#,
            message_id,
            part_key
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map_sqlx(e, "replace_attachments.prune"))?;
    }

    tx.commit()
        .await
        .map_err(|e| map_sqlx(e, "replace_attachments.commit"))?;

    Ok(keep.len())
}

/// 列出一封邮件的附件（按部件路径排序，稳定）。
pub async fn list_attachments(
    pool: &SqlitePool,
    message_id: &str,
) -> crate::StoreResult<Vec<Attachment>> {
    let rows = sqlx::query_as!(
        AttachmentRow,
        r#"
        SELECT id, message_id, filename, mime, size, content_id, "inline",
               local_path, sha256, fetched, part_key, downloaded_at, transfer_encoding
        FROM attachment
        WHERE message_id = ?
        ORDER BY part_key ASC, id ASC
        "#,
        message_id
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_attachments"))?;

    Ok(rows.iter().map(row_to_attachment).collect())
}

/// 取一封邮件里某个附件。
pub async fn get_attachment(
    pool: &SqlitePool,
    message_id: &str,
    attachment_id: &str,
) -> crate::StoreResult<Option<Attachment>> {
    let row = sqlx::query_as!(
        AttachmentRow,
        r#"
        SELECT id, message_id, filename, mime, size, content_id, "inline",
               local_path, sha256, fetched, part_key, downloaded_at, transfer_encoding
        FROM attachment
        WHERE message_id = ? AND id = ?
        "#,
        message_id,
        attachment_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "get_attachment"))?;

    Ok(row.as_ref().map(row_to_attachment))
}

/// 记下「这个附件已经落到本地哪个文件」。
///
/// `sha256` 是**解码后字节**的摘要，`size` 是落盘的字节数——
/// 两者一起写，`fetched` 与 `local_path` 同进同退，不留半截状态。
///
/// 这是**唯一**允许写 `fetched` / `local_path` / `sha256` / `downloaded_at` 的地方，
/// 也是它们永远同时成立的唯一理由（[`replace_attachments`] 在 DB 层把已下载的行
/// 钉死，见那里的 R55）。调用方必须先确认**文件已经落盘**再调：记完账就是一个
/// 不可撤销的承诺——`fetched = 1` 之后重拉正文也不会再刷新它。
pub async fn mark_downloaded(
    pool: &SqlitePool,
    message_id: &str,
    attachment_id: &str,
    local_path: &str,
    sha256: &str,
    size: i64,
    now_ms: i64,
) -> crate::StoreResult<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE attachment
        SET local_path = ?, sha256 = ?, size = ?, fetched = 1, downloaded_at = ?
        WHERE message_id = ? AND id = ?
        "#,
        local_path,
        sha256,
        size,
        now_ms,
        message_id,
        attachment_id
    )
    .execute(pool)
    .await
    .map_err(|e| map_sqlx(e, "mark_downloaded"))?;

    Ok(result.rows_affected() > 0)
}

/// 列出一封邮件里的**内联**部件（渲染 `cid:` 图片用）。
pub async fn list_inline_attachments(
    pool: &SqlitePool,
    message_id: &str,
) -> crate::StoreResult<Vec<Attachment>> {
    let rows = sqlx::query_as!(
        AttachmentRow,
        r#"
        SELECT id, message_id, filename, mime, size, content_id, "inline",
               local_path, sha256, fetched, part_key, downloaded_at, transfer_encoding
        FROM attachment
        WHERE message_id = ? AND "inline" = 1
        ORDER BY part_key ASC, id ASC
        "#,
        message_id
    )
    .fetch_all(pool)
    .await
    .map_err(|e| map_sqlx(e, "list_inline_attachments"))?;

    Ok(rows.iter().map(row_to_attachment).collect())
}

/// 数一封邮件里**还没**下载过的附件数（诊断 / 离线缓存状态用）。
pub async fn count_pending_downloads(
    pool: &SqlitePool,
    message_id: &str,
) -> crate::StoreResult<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) FROM attachment
        WHERE message_id = ? AND fetched = 0
        "#,
        message_id
    )
    .fetch_one(pool)
    .await
    .map_err(|e| map_sqlx(e, "count_pending_downloads"))?;

    Ok(count)
}

/// 行 → 领域对象。
fn row_to_attachment(row: &AttachmentRow) -> Attachment {
    Attachment {
        id: row.id.clone(),
        message_id: row.message_id.clone(),
        filename: row.filename.clone(),
        mime: row.mime.clone(),
        size: row.size,
        content_id: row.content_id.clone(),
        inline: row.inline,
        local_path: row.local_path.clone(),
        sha256: row.sha256.clone(),
        fetched: row.fetched,
        part_key: row.part_key.clone(),
        downloaded_at_ms: row.downloaded_at,
        transfer_encoding: row.transfer_encoding.clone(),
    }
}
