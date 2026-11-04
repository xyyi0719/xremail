//! 拉取链路：把服务器上的邮件同步进本地 SQLite。
//!
//! 一轮同步的固定流程（见 [ARCHITECTURE §4.2](../../../docs/ARCHITECTURE.md)）：
//!
//! 1. **打开文件夹**（`EXAMINE`）——拿到 epoch（UIDVALIDITY）与当前邮件数；
//! 2. **epoch 判定**——变了就不是增量更新，而是重建；
//! 3. **取远端 UID 集合**；
//! 4. **差分**——服务器有本地没有的（要拉）、本地有服务器没有的（服务器删了）；
//! 5. **拉取新增**并落库；
//! 6. **清理已被服务器删除的**缓存；
//! 7. **更新游标**。
//!
//! ## 为什么必须用 `run_checked`
//!
//! async-imap 0.12 会**吞掉**服务器回的 `NO` / `BAD`：
//! `uid_fetch` 在服务器拒绝时返回 `Ok`（只是结果为空）。
//! 那样一来「服务器拒绝了」看起来像「这个文件夹没有邮件」，
//! 同步引擎就会把整个文件夹的缓存当成「服务器已删」清掉。
//! 详见 [`xremail_protocol_imap::command`] 与 ARCHITECTURE §2.1。
//!
//! ## 为什么 `EXAMINE` 而不是 `SELECT`
//!
//! `EXAMINE` 打开的是**只读**邮箱：同步只读不写，用只读更安全
//! （万一有别的客户端在改，也不会互相踩）。
//! 标记回写另走 `SELECT`（见 ARCHITECTURE §6）。

use std::collections::BTreeSet;
use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::model::{Folder, Message, MessageFlags};
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::{CommandOutcome, ProtocolError, Session, examine_checked_with_timeout};

use crate::diff::{UidSet, diff_uids};
use crate::epoch::{EpochTransition, StoredEpoch, plan_epoch_transition};
use crate::net::{SyncOptions, run_checked_with_timeout_for};
use xremail_store::dao::message as message_dao;
use xremail_store::dao::sync::{FolderSyncWrite, commit_folder_sync};

/// 一轮文件夹同步的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSyncReport {
    /// 文件夹 ID。
    pub folder_id: String,
    /// 文件夹名在**线路上的原样**（modified UTF-7）。
    ///
    /// 后续要对这个文件夹发命令时用它（服务器只认自己给出的那个形式）。
    pub remote_name: String,
    /// 服务器当前的 epoch。
    pub uidvalidity: i64,
    /// 是否**全量拉取**（首次同步或 epoch 切换）。
    pub full_fetch: bool,
    /// 是否发生了 **epoch 切换**（旧世代作废、必须重建）。
    ///
    /// 与「首次同步」不同：首次同步时本地本来就没有缓存，
    /// 不存在「旧世代被作废」这回事。
    pub epoch_changed: bool,
    /// 是否是**首次**同步（本地此前没有任何缓存）。
    pub first_sync: bool,
    /// 新拉到几封。
    pub added: usize,
    /// 服务器已删、本地清掉几封。
    pub removed: usize,
    /// 两边都有、保持不变的封数。
    pub retained: usize,
    /// 同步后已知的最大 UID。
    pub last_uid: Option<i64>,
}

impl FolderSyncReport {
    /// 这一轮有没有实际改动。
    pub fn is_noop(&self) -> bool {
        self.added == 0 && self.removed == 0 && !self.epoch_changed
    }
}

/// 把协议层的错误映射成应用错误。
///
/// 保留「可重试 / 需用户处理」的分类：
/// 认证失败要用户处理，网络抖动可以重试。
pub(crate) fn map_protocol_error(error: ProtocolError) -> AppError {
    match error {
        ProtocolError::Auth(detail) => AppError::Auth(detail),
        ProtocolError::Io(detail) => AppError::Network(detail),
        ProtocolError::Timeout(detail) => AppError::Network(detail),
        ProtocolError::Connect(detail) => AppError::Network(detail),
        ProtocolError::Unsupported(detail) => AppError::Unsupported(detail),
        other => AppError::Protocol(other.to_string()),
    }
}

/// 同步一个文件夹：拉取并落库。
///
/// `folder` 是本地的文件夹记录（含 `remote_name` 与当前 epoch）。
/// 同步完成后本地文件夹的游标会被更新（`uidvalidity` / `last_uid` / `last_sync_at`）。
///
/// ## epoch 变化的处置
///
/// epoch 变了时，旧的 UID 映射**全部作废**：
/// 本地缓存的邮件被清空、重新全量拉取。
/// 绑定旧 epoch 的待提交操作**隔离**（不套用到新 UID 上）——
/// 这由 [`plan_epoch_transition`] 判定，本函数只负责落库。
pub async fn pull_folder(
    pool: &SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
) -> AppResult<FolderSyncReport> {
    pull_folder_with_options(pool, session, folder, now_ms, SyncOptions::default()).await
}

/// 同 [`pull_folder`]，但用**调用方给的**选项（主要是命令超时）。
pub async fn pull_folder_with_options(
    pool: &SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
    options: SyncOptions,
) -> AppResult<FolderSyncReport> {
    // 1. 打开文件夹（只读）。
    //
    // 必须带超时：async-imap 的 `examine` 不会自己超时，
    // 服务器不回话时整个同步就停在这一行。
    let mailbox =
        examine_checked_with_timeout(session, &folder.remote_name, options.command_timeout)
            .await
            .map_err(map_protocol_error)?;

    // 2. epoch：服务器必须给出 UIDVALIDITY，否则无法定位邮件。
    let observed = mailbox
        .uid_validity
        .map(i64::from)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AppError::Protocol(format!(
                "服务器没有给出 UIDVALIDITY（文件夹 {}），无法定位邮件",
                folder.remote_name
            ))
        })?;

    // 3. epoch 判定。
    let stored = StoredEpoch {
        uidvalidity: folder.uidvalidity,
        last_uid: folder.last_uid,
        highest_modseq: folder.highest_modseq,
    };
    // 绑定旧 epoch 的待提交操作：本批还没有操作队列，先传空。
    let transition = plan_epoch_transition(&stored, observed, &[])?;
    let epoch_changed = transition.requires_reset();

    // 4. 取远端 UID 集合。
    //
    // 用 `UID FETCH 1:* (UID)` 而不是 `UID SEARCH ALL`：
    // FETCH 是 IMAP 的**必备**命令，SEARCH 是可选扩展。
    let remote = fetch_remote_uids(session, options.command_timeout).await?;

    // 5. 差分。
    let local =
        message_dao::list_message_uids(pool, &folder.account_id, &folder.id, observed).await?;
    let local_set = UidSet::from_uids(local.iter().copied());
    let mut diff = diff_uids(&local_set, &remote);

    // 保留策略：本地草稿等有独立归属的内容，
    // 不能因为服务器上没有就当成「服务器已删」清掉。
    diff.retain_local(&protected_uids());

    // 6. epoch 变了 → 旧缓存全部作废，重新全量拉。
    let (added_uids, removed_uids) = if transition.requires_full_fetch() {
        // 全量：本地视为空。
        let all: Vec<i64> = remote.iter().collect();
        (all, Vec::new())
    } else {
        (diff.added.clone(), diff.removed.clone())
    };

    // 7. 拉取新增。
    let fetched = if added_uids.is_empty() {
        Vec::new()
    } else {
        fetch_metadata(session, &added_uids, options.command_timeout).await?
    };

    let mut to_write = Vec::with_capacity(fetched.len());
    for item in &fetched {
        to_write.push(to_domain_message(folder, observed, item));
    }

    // 8. 落库：清旧世代 + 写新邮件 + 删服务器已删的 + 更新游标，**一个事务**。
    //
    // 顺序很重要：先清旧世代再写新的。反过来的话，
    // 「清掉其他 epoch」会把刚写进来的新世代邮件也删掉。
    let mut updated = folder.clone();
    updated.uidvalidity = Some(observed);
    updated.last_uid = remote.max();
    updated.last_sync_at = Some(now_ms);

    let write = FolderSyncWrite {
        upserts: to_write.clone(),
        delete_uids: removed_uids,
        drop_other_epochs: epoch_changed,
    };
    let deleted = commit_folder_sync(pool, &updated, &write).await?;
    let added = write.upserts.len();

    Ok(FolderSyncReport {
        folder_id: folder.id.clone(),
        remote_name: folder.remote_name.clone(),
        uidvalidity: observed,
        full_fetch: transition.requires_full_fetch(),
        epoch_changed,
        first_sync: matches!(transition, EpochTransition::Initial { .. }),
        added,
        removed: deleted,
        retained: diff.retained.len(),
        last_uid: updated.last_uid,
    })
}

/// 取远端的全部 UID。
async fn fetch_remote_uids(session: &mut Session, timeout: Duration) -> AppResult<UidSet> {
    let outcome = run_checked_with_timeout_for(session, "UID FETCH 1:* (UID)", timeout)
        .await
        .map_err(map_protocol_error)?;

    let uids = match outcome {
        CommandOutcome::Fetches(messages) => messages
            .iter()
            .filter_map(|message| message.uid)
            .map(i64::from)
            .collect::<Vec<i64>>(),
        _ => Vec::new(),
    };

    Ok(UidSet::from_uids(uids))
}

/// 拉取一批 UID 的元数据（标记、大小与**头**）。
///
/// 带上头是因为列表视图要显示主题、发件人、日期——
/// 它们都在头里，而头很小（几 KB）。
/// **正文不在这里拉**（按需拉，见 [`crate::body::fetch_body`]）。
async fn fetch_metadata(
    session: &mut Session,
    uids: &[i64],
    timeout: Duration,
) -> AppResult<Vec<xremail_protocol_imap::FetchedMessage>> {
    // 分批：一次 `UID FETCH` 太多 UID 会让服务器超载。
    const BATCH: usize = 200;

    let mut out = Vec::new();
    for batch in uids.chunks(BATCH) {
        let set = build_uid_set(batch);
        let command = format!("UID FETCH {set} (UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])");
        let outcome = run_checked_with_timeout_for(session, &command, timeout)
            .await
            .map_err(map_protocol_error)?;
        if let CommandOutcome::Fetches(messages) = outcome {
            out.extend(messages);
        }
    }

    Ok(out)
}

/// 把一批 UID 拼成 sequence set。
///
/// 连续区间会被合并成 `a:b`，避免 `1,2,3,4,…` 这种长串。
fn build_uid_set(uids: &[i64]) -> String {
    if uids.is_empty() {
        return String::new();
    }

    let mut sorted: Vec<i64> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts: Vec<String> = Vec::new();
    let mut start = sorted[0];
    let mut previous = sorted[0];

    for &uid in &sorted[1..] {
        if uid == previous + 1 {
            previous = uid;
            continue;
        }
        parts.push(format_range(start, previous));
        start = uid;
        previous = uid;
    }
    parts.push(format_range(start, previous));

    parts.join(",")
}

/// 一个区间的写法：单个数字直接写，两个以上才写 `a:b`。
fn format_range(start: i64, end: i64) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

/// 把协议层的 FETCH 结果转成领域对象。
///
/// 本地 ID **独立生成**：远端的 `Message-ID` 只是线索，不能当主键。
fn to_domain_message(
    folder: &Folder,
    uidvalidity: i64,
    item: &xremail_protocol_imap::FetchedMessage,
) -> Message {
    // 头里才有主题 / 发件人 / 日期。
    let headers = item
        .header
        .as_ref()
        .map(|raw| xremail_message::parse_headers(raw))
        .unwrap_or_default();

    Message {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: i64::from(item.uid.unwrap_or(0)),
        uidvalidity,
        message_id: headers.message_id,
        subject: headers.subject,
        // 摘要先留空：它要靠正文，切开才知道。
        snippet: None,
        from: headers.from,
        to: headers.to,
        cc: headers.cc,
        bcc: headers.bcc,
        date_sent: headers.date,
        internal_date: None,
        flags: flags_from_imap(&item.flags),
        size: item.size.map(i64::from),
        has_attachment: false,
    }
}

/// IMAP 标记名 → 位掩码。
///
/// 布局与 IMAP 的系统标记一一对应（见 `xremail-core` 的 `MessageFlags`）。
pub fn flags_from_imap(names: &[String]) -> MessageFlags {
    let mut flags = MessageFlags::empty();
    for name in names {
        let bit = if name.eq_ignore_ascii_case("\\Seen") {
            MessageFlags::SEEN
        } else if name.eq_ignore_ascii_case("\\Answered") {
            MessageFlags::ANSWERED
        } else if name.eq_ignore_ascii_case("\\Flagged") {
            MessageFlags::FLAGGED
        } else if name.eq_ignore_ascii_case("\\Deleted") {
            MessageFlags::DELETED
        } else if name.eq_ignore_ascii_case("\\Draft") {
            MessageFlags::DRAFT
        } else if name.eq_ignore_ascii_case("\\Recent") {
            MessageFlags::RECENT
        } else {
            // 自定义关键字没有对应位，忽略。
            continue;
        };
        flags = flags.with(bit);
    }
    flags
}

/// 受保护的本地 UID 集合（本地草稿等有独立归属的内容）。
///
/// 下一批接上操作队列后，这里要返回「不能因为服务器上没有就清掉」的 UID。
pub fn protected_uids() -> BTreeSet<i64> {
    BTreeSet::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_set_merges_consecutive_ranges() {
        assert_eq!(build_uid_set(&[1, 2, 3]), "1:3");
        assert_eq!(build_uid_set(&[1, 2, 4]), "1:2,4");
        assert_eq!(build_uid_set(&[5]), "5");
        assert_eq!(build_uid_set(&[]), "");
    }

    #[test]
    fn uid_set_sorts_and_deduplicates() {
        assert_eq!(build_uid_set(&[3, 1, 2, 2]), "1:3");
    }

    #[test]
    fn range_formatting() {
        assert_eq!(format_range(7, 7), "7");
        assert_eq!(format_range(7, 9), "7:9");
    }

    #[test]
    fn imap_flags_map_to_bits() {
        let flags = flags_from_imap(&["\\Seen".to_string(), "\\Flagged".to_string()]);

        assert!(flags.contains(MessageFlags::SEEN));
        assert!(flags.contains(MessageFlags::FLAGGED));
        assert!(!flags.contains(MessageFlags::DELETED));
    }

    #[test]
    fn imap_flags_are_case_insensitive() {
        let flags = flags_from_imap(&["\\seen".to_string()]);

        assert!(flags.contains(MessageFlags::SEEN));
    }

    #[test]
    fn custom_keywords_are_ignored() {
        let flags = flags_from_imap(&["$Label1".to_string(), "\\Seen".to_string()]);

        assert_eq!(flags, MessageFlags::empty().with(MessageFlags::SEEN));
    }
}
