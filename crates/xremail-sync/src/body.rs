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

    Ok(true)
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
