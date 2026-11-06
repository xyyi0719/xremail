//! 按需拉取正文。
//!
//! ## 为什么正文是**按需**拉的
//!
//! 同步时只拉元数据与**头**（主题、发件人、日期）。
//! 正文可能很大（还有附件），一个邮箱里几千封邮件全拉一遍
//! 既慢又费流量——用户可能只读其中几封。
//!
//! 所以正文在**用户点开时**才拉（见 [`fetch_body`]）。
//!
//! ## 为什么用 `BODY.PEEK[]` 而不是 `BODY[]`
//!
//! `BODY[]` 会**置上 `\Seen`**。而“看正文”不一定等于“标记已读”
//! （用户可能只是预览，或者他用的是「读后 N 秒才标记已读」的设置）。
//! `BODY.PEEK[]` 只取内容，不改标记——标记的变化走独立的回写链路
//! （见 [`crate::push`]）。
//!
//! ## 正文与头是分开的
//!
//! 正文存在 `message_body` 表（见 [ARCHITECTURE §4.2](../../../docs/ARCHITECTURE.md)）：
//! 列文件夹时不会碰到大文本列。

use sqlx::SqlitePool;
use xremail_core::AppResult;
use xremail_core::model::MessageBody;
use xremail_protocol_imap::{CommandOutcome, Session};
use xremail_store::dao::attachment as attachment_dao;
use xremail_store::dao::message as message_dao;

use crate::net::{SyncOptions, run_checked_with_timeout_for};
use crate::pull::map_protocol_error;

/// 拉取一封邮件的正文并落库。
///
/// 返回 `true` 表示拉到了（并已落库），`false` 表示服务器上没有这封邮件。
///
/// **幂等**：重复调用会覆盖已有的正文。
///
/// ## 为什么要先 EXAMINE
///
/// **UID 只在它自己的邮箱里有意义**。会话当前选中的邮箱是另一个文件夹时，
/// `UID FETCH <uid>` 会返回**空结果**（不是报错）——于是「拉到了 0 封」看起来像
/// 「服务器上没有这封邮件」。
///
/// 所以拉正文前**必须**把会话切到 `remote_name` 那个文件夹。
pub async fn fetch_body(
    pool: &SqlitePool,
    session: &mut Session,
    account_id: &str,
    folder_id: &str,
    remote_name: &str,
    uid: i64,
    uidvalidity: i64,
) -> AppResult<bool> {
    fetch_body_with_options(
        pool,
        session,
        account_id,
        folder_id,
        remote_name,
        uid,
        uidvalidity,
        SyncOptions::default(),
    )
    .await
}

/// 同 [`fetch_body`]，但用**调用方给的**选项（主要是命令超时）。
///
/// 参数比 [`fetch_body`] 多一个：`fetch_body` 的签名不能改（现有 ~10 个调用点），
/// 选项只能作为额外参数跟着，所以这里放行 `too_many_arguments`。
#[allow(clippy::too_many_arguments)]
pub async fn fetch_body_with_options(
    pool: &SqlitePool,
    session: &mut Session,
    account_id: &str,
    folder_id: &str,
    remote_name: &str,
    uid: i64,
    uidvalidity: i64,
    options: SyncOptions,
) -> AppResult<bool> {
    // 1. 先在本地找到这封邮件（要它的本地 ID）。
    let messages = message_dao::list_messages(pool, account_id, folder_id).await?;
    let Some(message) = messages
        .into_iter()
        .find(|message| message.uid == uid && message.uidvalidity == uidvalidity)
    else {
        return Ok(false);
    };

    // 2. 打开对应的文件夹（只读，不置 \Seen）。
    //
    // 少了这一步，服务器会拿**当前选中的邮箱**去查这个 UID → 返回空结果。
    //
    // 必须带超时：async-imap 的 `examine` 不会自己超时。
    xremail_protocol_imap::examine_checked_with_timeout(
        session,
        remote_name,
        options.command_timeout,
    )
    .await
    .map_err(map_protocol_error)?;

    // 3. 拉整封（PEEK：不置 \Seen）。
    let command = format!("UID FETCH {uid} (UID BODY.PEEK[])");
    let outcome = run_checked_with_timeout_for(session, &command, options.command_timeout)
        .await
        .map_err(map_protocol_error)?;

    let CommandOutcome::Fetches(fetched) = outcome else {
        return Ok(false);
    };
    let Some(fetched) = fetched.first() else {
        return Ok(false);
    };
    let Some(raw) = &fetched.body else {
        return Ok(false);
    };

    // 4. 解析（charset 猜测与 MIME 递归都在里面）。
    let parsed = xremail_message::parse_message(raw)
        .map_err(|e| xremail_core::AppError::Protocol(format!("解析邮件失败: {e}")))?;

    // 4. 落库。
    let body = MessageBody {
        message_id: message.id.clone(),
        text_plain: parsed.text_plain.clone(),
        html_raw: parsed.html_raw.clone(),
        charset: parsed.charset.clone(),
    };
    message_dao::upsert_message_body(pool, &body).await?;

    // 5. 有附件的标记跟着更新。
    if parsed.has_attachment {
        message_dao::update_has_attachment_by_uid(
            pool,
            account_id,
            folder_id,
            uidvalidity,
            uid,
            true,
        )
        .await?;
    }

    // 6. 附件元数据（**只存描述，不存字节**；字节在下载时才写文件）。
    //
    // `replace_attachments` 是「整体替换」：重拉同一封邮件不会清掉已经下载的
    // 那几个部件的本地路径与 `fetched` 标记（见 `dao::attachment`）。
    attachment_dao::replace_attachments(
        pool,
        &message.id,
        &attachment_records(&message.id, &parsed.attachments),
        current_time_ms(),
    )
    .await?;

    Ok(true)
}

/// 把解析出来的附件元数据变成领域对象。
///
/// `id` 由 `message_id + part_number` 决定——同一封邮件同一个部件永远是同一个 ID，
/// 这样重拉正文时 `ON CONFLICT (message_id, part_key)` 能对上，
/// 前端手里的附件 ID 也不会变。
fn attachment_records(
    message_id: &str,
    metas: &[xremail_message::AttachmentMeta],
) -> Vec<xremail_core::model::Attachment> {
    metas
        .iter()
        .map(|meta| xremail_core::model::Attachment {
            id: format!("{message_id}/{}", meta.part_number),
            message_id: message_id.to_string(),
            filename: meta.filename.clone(),
            mime: Some(meta.content_type.clone()),
            size: Some(meta.size),
            content_id: meta.content_id.clone(),
            inline: meta.is_inline,
            local_path: None,
            sha256: None,
            fetched: false,
            part_key: Some(meta.part_number.clone()),
            downloaded_at_ms: None,
            transfer_encoding: meta.transfer_encoding.clone(),
        })
        .collect()
}

/// 当前时间（epoch 毫秒）。
fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 只拉**一个部件**的原始字节（附件下载用）。
///
/// 与 [`fetch_body`] 的区别：
/// * 命令是 `UID FETCH <uid> (UID BODY.PEEK[<part>])`——只要那一段，
///   不把整封邮件（含所有附件）搬下来；
/// * **不落库、不解析**，把（仍是传输编码形态的）字节交给调用方，
///   由调用方边解码边写文件。
///
/// 返回 `None` 表示服务器上没有这个 UID 或没有这个部件。
///
/// **幂等**：`PEEK` 不改变 `\Seen`，重复调用没有副作用。
pub async fn fetch_attachment_part(
    session: &mut Session,
    remote_name: &str,
    uid: i64,
    part: &str,
) -> AppResult<Option<Vec<u8>>> {
    fetch_attachment_part_with_options(session, remote_name, uid, part, SyncOptions::default())
        .await
}

/// 同 [`fetch_attachment_part`]，但用**调用方给的**选项（主要是命令超时）。
pub async fn fetch_attachment_part_with_options(
    session: &mut Session,
    remote_name: &str,
    uid: i64,
    part: &str,
    options: SyncOptions,
) -> AppResult<Option<Vec<u8>>> {
    let part = valid_part_number(part)?;

    // 同 `fetch_body`：UID 只在它自己的邮箱里有意义。
    xremail_protocol_imap::examine_checked_with_timeout(
        session,
        remote_name,
        options.command_timeout,
    )
    .await
    .map_err(map_protocol_error)?;

    let command = format!("UID FETCH {uid} (UID BODY.PEEK[{part}])");
    let outcome = run_checked_with_timeout_for(session, &command, options.command_timeout)
        .await
        .map_err(map_protocol_error)?;

    let CommandOutcome::Fetches(fetched) = outcome else {
        return Ok(None);
    };
    Ok(fetched.into_iter().find_map(|fetched| fetched.body))
}

/// 部件号只允许 `1`、`1.2`、`3.1.1` 这种。
///
/// **这不是洁癖**：部件号要拼进 IMAP 命令里，放行任意字符串就等于
/// 把用户可控的内容塞进协议行（`BODY.PEEK[1] (UID)` 之类）。
fn valid_part_number(part: &str) -> AppResult<String> {
    let ok = !part.is_empty()
        && part.len() <= 64
        && part
            .split('.')
            .all(|segment| !segment.is_empty() && segment.chars().all(|ch| ch.is_ascii_digit()));
    if ok {
        Ok(part.to_string())
    } else {
        Err(xremail_core::AppError::Invalid(format!(
            "部件号不合法: {part}"
        )))
    }
}

/// 读回一封邮件的正文（本地已有时）。
pub async fn read_body(
    pool: &SqlitePool,
    account_id: &str,
    folder_id: &str,
    uid: i64,
    uidvalidity: i64,
) -> AppResult<Option<MessageBody>> {
    let messages = message_dao::list_messages(pool, account_id, folder_id).await?;
    let Some(message) = messages
        .into_iter()
        .find(|message| message.uid == uid && message.uidvalidity == uidvalidity)
    else {
        return Ok(None);
    };

    message_dao::get_message_body(pool, &message.id).await
}
