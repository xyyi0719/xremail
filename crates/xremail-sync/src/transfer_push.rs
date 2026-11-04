//! 移动 / 删除的回写：把队列里的 `Move` / `Delete` 意图提交给服务器并**确认**。
//!
//! ## 为什么单独一个模块
//!
//! 标记回写（[`crate::push_flags`]）的意图是「这封信应该带哪些 flag」，
//! 净变化可以按 bit 算；移动/删除的意图是「这封信不该在这个文件夹里了」，
//! 没有「净变化」这回事——只能做一次，然后**核对它真的走了**。
//!
//! ## 路由由服务器能力决定（不猜）
//!
//! | 场景 | 发什么 |
//! |---|---|
//! | 有 `MOVE` | 一条 `UID MOVE` |
//! | 没有 `MOVE` | `UID COPY` → `UID STORE +FLAGS (\Deleted)` → **有 `UIDPLUS` 才** `UID EXPUNGE`（163 是这一类） |
//! | 没有 `MOVE` 也没有 `UIDPLUS` | 停在「已标删」（三家实测都支持 `UIDPLUS`，这档只为兜底） |
//!
//! **裸 `EXPUNGE` 一律不用**：它会把这个文件夹里所有带 `\Deleted` 的信都删掉，
//! 包括别的客户端（手机、网页版）标了还没提交的。那也是用户的数据。
//!
//! ## 删除 = 进垃圾箱（保守优先）
//!
//! 用户点「删除」不等于「永久删除」。有垃圾箱就**挪进去**（等于 `Move`），
//! 没有才就地标删。垃圾箱靠文件夹名推断（[`role_by_name`]）——
//! 本地库目前不存 `\Trash` 属性，这一点已在文档里写明。
//!
//! ## 失败分类
//!
//! 与标记回写完全一致（见 [`crate::push_flags`] 的表格），复用同一个
//! [`SubmitError`]：明确 `NO` 可重试；断线/超时是 `uncertain`；
//! 认证失败不重试。
//!
//! 额外一条**移动专属**的保守处理：服务器上源文件夹已经没有这封邮件了，
//! 就认为**已经移动成功**。可能是我们上一次尝试生效了（应答丢了），
//! 也可能是别人挪走了——两种情况下「再挪一次」都是错的
//! （RFC 6851 的 `MOVE` 不是幂等的）。

use xremail_core::model::{Folder, MailOperation, MessageFlags, OperationKind, OperationState};
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::folders::role_by_name;
use xremail_protocol_imap::{
    CommandOutcome, Session, copy_messages, mark_deleted, move_messages, uid_expunge,
};
use xremail_store::dao::{folder as folder_dao, message as message_dao, operation};

use crate::net::run_checked_with_timeout;
use crate::push::{SubmitError, classify_submit_error};

/// 一轮移动/删除回写的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferPushReport {
    /// 服务器确认搬走了几封。
    pub moved: usize,
    /// 服务器确认删掉了几封（含挪进垃圾箱）。
    pub deleted: usize,
    /// 失败但可重试几封。
    pub retryable: usize,
    /// 失败且不该重试几封。
    pub failed: usize,
    /// 不知道服务器执行了没几封。
    pub uncertain: usize,
    /// 因 epoch 变化被隔离几封。
    pub quarantined: usize,
    /// 一条命令都没发就成功的（服务器上已经是目标状态）。
    pub noop: usize,
}

impl TransferPushReport {
    /// 是否没有需要服务器处理的东西。
    pub fn is_noop(&self) -> bool {
        self.moved == 0 && self.deleted == 0 && self.retryable == 0 && self.failed == 0
    }
}

/// 这条操作该怎么发。
///
/// 拆成纯函数是为了能**不连服务器**就测路由：
/// 「有 MOVE / 没有 MOVE」「有 UIDPLUS / 没有」「有没有垃圾箱」
/// 一共就这几种组合，而它们决定的是「会不会误删别人的数据」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// 一条 `UID MOVE`。
    Move { destination: String },
    /// `UID COPY` → 标删 →（可选）`UID EXPUNGE`。
    CopyDelete { destination: String, expunge: bool },
    /// 就地删除（没有垃圾箱可去）。
    Expunge { expunge: bool },
}

/// 算出一条操作的路由。
///
/// `destination`：`Move` 操作显式给的目标邮箱名。
/// `trash`：账户里垃圾箱的邮箱名（`Delete` 用它）。
pub(crate) fn route_for(
    kind: OperationKind,
    has_move: bool,
    has_uidplus: bool,
    destination: Option<&str>,
    trash: Option<&str>,
) -> Result<Route, &'static str> {
    match kind {
        // 删除优先挪进垃圾箱：能撤销的动作比不能撤销的安全。
        OperationKind::Delete => match trash {
            Some(trash) => Ok(move_route(has_move, has_uidplus, trash)),
            None => Ok(Route::Expunge {
                expunge: has_uidplus,
            }),
        },
        OperationKind::Move => match destination {
            Some(destination) => Ok(move_route(has_move, has_uidplus, destination)),
            None => Err("移动操作没有记录目标文件夹"),
        },
        // 标记操作不归这里管。
        OperationKind::Flags => Err("标记操作不走移动/删除通道"),
    }
}

/// 「搬到 `destination`」在不同能力下的两种走法。
fn move_route(has_move: bool, has_uidplus: bool, destination: &str) -> Route {
    if has_move {
        Route::Move {
            destination: destination.to_string(),
        }
    } else {
        Route::CopyDelete {
            destination: destination.to_string(),
            // 没有 `UIDPLUS` 时**不能** expunge：裸 `EXPUNGE` 会连带删掉
            // 别人标了 `\Deleted` 还没提交的邮件。停在这一步更安全。
            expunge: has_uidplus,
        }
    }
}

/// 把某个文件夹里待提交的移动/删除意图回写到服务器。
///
/// 流程与 [`crate::push_flags`] 同构：取未完结操作 → 隔离旧 epoch →
/// **认领** → 按能力选路由 → 发命令 → **回读确认** → 更新状态。
///
/// 认领这一步的意义见 [`crate::push_flags`] 的同名说明：
/// 它是唯一的提交权闸门，没认领到的方什么都不做且**不算失败**。
/// 把某个文件夹里待提交的移动/删除意图回写到服务器。
///
/// 认领时用本进程的启动标识（[`crate::boot::boot_id`]）。
pub async fn push_transfers(
    pool: &sqlx::SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
) -> AppResult<TransferPushReport> {
    push_transfers_as(pool, session, folder, now_ms, crate::boot::boot_id()).await
}

/// 同 [`push_transfers`]，但显式指定认领归属。
pub async fn push_transfers_as(
    pool: &sqlx::SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
    boot_id: &str,
) -> AppResult<TransferPushReport> {
    let mut report = TransferPushReport::default();

    let Some(epoch) = folder.uidvalidity else {
        // 没同步过就没有 epoch，也就不知道 UID 指的是哪封信。
        return Ok(report);
    };

    let pending = operation::list_pending_operations(pool, &folder.account_id, &folder.id).await?;

    // 1. 隔离绑定旧 epoch 的操作（旧 UID 在新 epoch 下指向另一封信）。
    let (stale, live): (Vec<MailOperation>, Vec<MailOperation>) =
        pending.into_iter().partition(|op| op.uidvalidity != epoch);

    for op in &stale {
        if !matches!(op.kind, OperationKind::Move | OperationKind::Delete) {
            // 标记操作留给 push_flags 去隔离。
            continue;
        }
        operation::update_operation_state(
            pool,
            &op.id,
            OperationState::Failed,
            Some("epoch_changed"),
            Some(&format!(
                "操作绑定的是 epoch {}，当前 epoch 是 {epoch}；旧 UID 指向另一封信，不能套用",
                op.uidvalidity
            )),
            now_ms,
        )
        .await?;
        report.quarantined += 1;
    }

    // 2. 只处理移动/删除，按 seq 已经排好序。
    let transfers: Vec<MailOperation> = live
        .into_iter()
        .filter(|op| matches!(op.kind, OperationKind::Move | OperationKind::Delete))
        .collect();

    if transfers.is_empty() {
        return Ok(report);
    }

    // 3. 以**可写**方式打开源文件夹（拉取那一轮用的是 EXAMINE）。
    session
        .select(&folder.remote_name)
        .await
        .map_err(|error| AppError::Protocol(format!("以可写方式打开文件夹失败: {error}")))?;

    // 4. 能力与垃圾箱：每轮取一次，不逐条问。
    let capabilities = session
        .capabilities()
        .await
        .map_err(|error| AppError::Protocol(format!("读服务器能力失败: {error}")))?;
    let has_move = capabilities.has_str("MOVE");
    let has_uidplus = capabilities.has_str("UIDPLUS");
    let trash = find_trash(pool, folder).await?;

    // 5. 逐条提交。
    for op in &transfers {
        // 认领：没抢到（或次数已到顶）就跳过。
        //
        // `MOVE` 不幂等（重发会多出一份），所以这道闸门在这里比在
        // `push_flags` 里更要紧：宁可这轮不发，也不能发两遍。
        if !operation::claim_for_push(
            pool,
            &op.id,
            boot_id,
            crate::reconcile::MAX_OPERATION_ATTEMPTS,
            now_ms,
        )
        .await?
        {
            continue;
        }

        let destination = match op.kind {
            OperationKind::Move => {
                match destination_of(pool, op).await? {
                    Some(name) => Some(name),
                    None => {
                        // 目标文件夹没了：这条意图再也做不成了。
                        operation::update_operation_state(
                            pool,
                            &op.id,
                            OperationState::Failed,
                            Some("destination_missing"),
                            Some("目标文件夹已不存在（可能被删除或改名），请重新选择"),
                            now_ms,
                        )
                        .await?;
                        report.failed += 1;
                        continue;
                    }
                }
            }
            // 删除的目标是垃圾箱（可能没有）。
            _ => None,
        };

        let route = match route_for(
            op.kind,
            has_move,
            has_uidplus,
            destination.as_deref(),
            trash.as_deref(),
        ) {
            Ok(route) => route,
            Err(message) => {
                operation::update_operation_state(
                    pool,
                    &op.id,
                    OperationState::Failed,
                    Some("permanent"),
                    Some(message),
                    now_ms,
                )
                .await?;
                report.failed += 1;
                continue;
            }
        };

        // 6. 重发前的**幂等探测**。
        //
        // `COPY` 不幂等：上一轮可能已经把信复制到目标文件夹了，只是确认没读到。
        // 无条件重发就会一份变两份、两份变三份——163 没有 `MOVE`，`copy+delete`
        // 是它唯一的路子，所以这不是理论问题。
        //
        // 只在 `attempts > 0` 时问：首次尝试不可能有副本，而探测要切文件夹、
        // 多打两条命令，不值得给每条新操作都付这个代价。
        let mut copy_already_done = false;
        if matches!(route, Route::CopyDelete { .. }) && op.attempts > 0 {
            let Route::CopyDelete { destination, .. } = &route else {
                unreachable!("上面刚判断过只有 CopyDelete 才进这段");
            };

            let message_id = message_dao::find_message_id_by_uid(
                pool,
                &folder.account_id,
                &folder.id,
                epoch,
                op.uid,
            )
            .await?;

            // 探测结果先收起来，**切回源文件夹之后再处理**。
            //
            // `destination_has_message_id` 会 `EXAMINE` 目标文件夹，无论成功还是失败
            // 会话都停在目标文件夹上。若带着这个状态 `continue`，下一条操作会对着
            // **错的文件夹**搜 UID，被误判成 `AlreadyGone` 后静默丢掉——那正是本条
            // 要修的那类「邮件没搬、队列却空了」。
            let probe = match &message_id {
                // 没有 `Message-ID` 就无法确认目标文件夹里有没有副本。
                // 宁可停住等对账 / 用户确认，也不能赌着重发一遍。
                None => Err(
                    "命令可能已经发出，但这封信没有 Message-ID，无法确认目标文件夹里是否已有副本，因此不重发"
                        .to_owned(),
                ),
                Some(id) => match destination_has_message_id(session, destination, id).await {
                    Ok(found) => Ok(found),
                    Err(error) => Err(format!(
                        "重发前探测目标文件夹失败，因此不重发：{}",
                        submit_error_message(&error)
                    )),
                },
            };

            // 探测换了选中的文件夹，提交之前必须切回**可写**的源文件夹。
            session
                .select(&folder.remote_name)
                .await
                .map_err(|error| AppError::Protocol(format!("切回源文件夹失败: {error}")))?;

            match probe {
                // 已经在目标文件夹里了：`COPY` 那一半不必再发。
                Ok(found) => copy_already_done = found,
                Err(message) => {
                    operation::mark_operation_uncertain(pool, &op.id, "unknown", &message, now_ms)
                        .await?;
                    report.uncertain += 1;
                    continue;
                }
            }
        }

        match submit(session, op.uid, &route, copy_already_done).await {
            Ok(result) => {
                // 本地视图跟着服务器走：源文件夹里这封邮件已经不该显示了。
                message_dao::delete_messages_by_uids(
                    pool,
                    &folder.account_id,
                    &folder.id,
                    epoch,
                    &[op.uid],
                )
                .await?;

                operation::update_operation_state(
                    pool,
                    &op.id,
                    OperationState::Succeeded,
                    None,
                    None,
                    now_ms,
                )
                .await?;

                match op.kind {
                    OperationKind::Delete => report.deleted += 1,
                    _ => report.moved += 1,
                }
                if result == SubmitResult::AlreadyGone {
                    report.noop += 1;
                }
            }
            Err(SubmitError::Retryable { message }) => {
                operation::mark_operation_retryable(
                    pool,
                    &op.id,
                    "server_rejected",
                    &message,
                    now_ms,
                )
                .await?;
                report.retryable += 1;
            }
            Err(SubmitError::Uncertain { message }) => {
                // 必须走 `mark_operation_uncertain`（而不是 `update_operation_state`）：
                // 只有它会自增 `attempts`，而 `claim_for_push` 靠 `attempts < ?` 卡住重发。
                // 不自增 = 这条永远可以被重认领 = 每轮重发一次 `COPY`。
                operation::mark_operation_uncertain(pool, &op.id, "unknown", &message, now_ms)
                    .await?;
                report.uncertain += 1;
            }
            Err(SubmitError::Failed { message }) => {
                operation::update_operation_state(
                    pool,
                    &op.id,
                    OperationState::Failed,
                    Some("permanent"),
                    Some(&message),
                    now_ms,
                )
                .await?;
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

/// 提交成功的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitResult {
    /// 命令生效了。
    Submitted,
    /// 服务器上源文件夹里已经没有这封邮件了，不必再发命令。
    AlreadyGone,
}

/// 按路由发命令，并**回读确认**。
///
/// `copy_already_done` 为真时跳过 `COPY` 那一半（调用方已经用 `Message-ID`
/// 在目标文件夹里探到副本了），只补完源文件夹的删除。
/// 取出 [`SubmitError`] 里的人话部分（三个分支都只有一个 `message`）。
fn submit_error_message(error: &SubmitError) -> &str {
    match error {
        SubmitError::Retryable { message }
        | SubmitError::Uncertain { message }
        | SubmitError::Failed { message } => message,
    }
}

async fn submit(
    session: &mut Session,
    uid: i64,
    route: &Route,
    copy_already_done: bool,
) -> Result<SubmitResult, SubmitError> {
    let uid32 = u32::try_from(uid).map_err(|_| SubmitError::Failed {
        message: format!("UID {uid} 超出 32 位范围，不能用 IMAP 命令表达"),
    })?;

    // 先看一眼它还在不在。
    //
    // 不在 → 上一次尝试多半已经生效了（应答丢了）。**不能重发**：
    // `MOVE` 不是幂等的，`COPY` 重发会在目标文件夹里多出一份。
    if !source_has_uid(session, uid32).await? {
        return Ok(SubmitResult::AlreadyGone);
    }

    match route {
        Route::Move { destination } => {
            move_messages(session, &[uid32], destination)
                .await
                .map_err(classify_submit_error)?;
            confirm(session, uid32, Expectation::Absent).await?;
        }
        Route::CopyDelete {
            destination,
            expunge,
        } => {
            // 探测已经确认目标文件夹里有这封，再发一次 `COPY` 就多一份。
            if !copy_already_done {
                copy_messages(session, &[uid32], destination)
                    .await
                    .map_err(classify_submit_error)?;
            }
            mark_deleted(session, &[uid32])
                .await
                .map_err(classify_submit_error)?;
            if *expunge {
                uid_expunge(session, &[uid32])
                    .await
                    .map_err(classify_submit_error)?;
                confirm(session, uid32, Expectation::Absent).await?;
            } else {
                confirm(session, uid32, Expectation::FlaggedDeleted).await?;
            }
        }
        Route::Expunge { expunge } => {
            mark_deleted(session, &[uid32])
                .await
                .map_err(classify_submit_error)?;
            if *expunge {
                uid_expunge(session, &[uid32])
                    .await
                    .map_err(classify_submit_error)?;
                confirm(session, uid32, Expectation::Absent).await?;
            } else {
                confirm(session, uid32, Expectation::FlaggedDeleted).await?;
            }
        }
    }

    Ok(SubmitResult::Submitted)
}

/// 目标文件夹里是否已经有这封（按 `Message-ID` 认）。
///
/// 会**切换选中的文件夹**（它是 `EXAMINE` 的），调用方必须自己切回去——
/// `submit` 需要的是可写的源文件夹。
async fn destination_has_message_id(
    session: &mut Session,
    destination: &str,
    message_id: &str,
) -> Result<bool, SubmitError> {
    session
        .examine(destination)
        .await
        .map_err(|error| SubmitError::Uncertain {
            message: format!("打开目标文件夹 {destination} 失败: {error}"),
        })?;

    // `Message-ID` 理论上不含引号，但服务器给的东西不能信：
    // 去掉引号、空白与控制字符，避免拼出一条坏命令。
    let sanitized: String = message_id
        .chars()
        .filter(|c| *c != '"' && !c.is_whitespace() && !c.is_control())
        .collect();

    let command = format!("UID SEARCH HEADER Message-ID \"{sanitized}\"");
    let outcome = run_checked_with_timeout(session, &command)
        .await
        .map_err(classify_submit_error)?;

    match outcome {
        CommandOutcome::Search(uids) => Ok(!uids.is_empty()),
        other => Err(SubmitError::Uncertain {
            message: format!("期望 UID SEARCH 返回命中列表，实际是 {other:?}"),
        }),
    }
}

/// 回读时要看到什么才算成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expectation {
    /// 源文件夹里应该**没有**这封邮件了。
    Absent,
    /// 还在，但应该带 `\Deleted`（没有 `UIDPLUS` 时的终点）。
    FlaggedDeleted,
}

/// 回读确认。
///
/// 服务器回 `OK` 不等于改动生效（见 [ARCHITECTURE §6](../../../docs/ARCHITECTURE.md)）。
async fn confirm(
    session: &mut Session,
    uid: u32,
    expectation: Expectation,
) -> Result<(), SubmitError> {
    match expectation {
        Expectation::Absent => {
            if source_has_uid(session, uid).await? {
                return Err(SubmitError::Retryable {
                    message: format!("回读确认失败：UID {uid} 仍在源文件夹里"),
                });
            }
            Ok(())
        }
        Expectation::FlaggedDeleted => {
            let flags = read_flags(session, uid).await?;
            let Some(flags) = flags else {
                // 已经不在了——比期望的更好（比如别的客户端把它 expunge 了）。
                return Ok(());
            };
            if !flags.contains(MessageFlags::DELETED) {
                return Err(SubmitError::Retryable {
                    message: format!("回读确认失败：UID {uid} 没有带上 \\Deleted"),
                });
            }
            Ok(())
        }
    }
}

/// 读一封邮件的标记；不在服务器上时返回 `None`。
async fn read_flags(session: &mut Session, uid: u32) -> Result<Option<MessageFlags>, SubmitError> {
    let command = format!("UID FETCH {uid} (UID FLAGS)");
    let outcome = run_checked_with_timeout(session, &command)
        .await
        .map_err(classify_submit_error)?;

    let messages = match outcome {
        CommandOutcome::Fetches(messages) => messages,
        _ => Vec::new(),
    };

    Ok(messages
        .first()
        .map(|message| MessageFlags::from_imap_names(&message.flags)))
}

/// 源文件夹里还有没有这个 UID。
async fn source_has_uid(session: &mut Session, uid: u32) -> Result<bool, SubmitError> {
    let command = format!("UID FETCH {uid} (UID)");
    let outcome = run_checked_with_timeout(session, &command)
        .await
        .map_err(classify_submit_error)?;

    Ok(match outcome {
        CommandOutcome::Fetches(messages) => !messages.is_empty(),
        _ => false,
    })
}

/// `Move` 操作的目标邮箱名（线路上的原名）。
async fn destination_of(pool: &sqlx::SqlitePool, op: &MailOperation) -> AppResult<Option<String>> {
    let Some(id) = op.destination_folder_id.as_deref() else {
        return Ok(None);
    };
    Ok(folder_dao::get_folder(pool, id)
        .await?
        .map(|folder| folder.remote_name))
}

/// 账户里的垃圾箱（线路上的原名）。
///
/// 本地库不存 `\Trash` 属性，只能按名字推断（[`role_by_name`]）。
/// 推断不出来就返回 `None`——那时候删除会就地标删，**不会**乱挪到某个
/// 名字碰巧像垃圾箱的文件夹里去。
async fn find_trash(pool: &sqlx::SqlitePool, folder: &Folder) -> AppResult<Option<String>> {
    let folders = folder_dao::list_folders(pool, &folder.account_id).await?;
    Ok(folders
        .into_iter()
        // 当前文件夹自己就是垃圾箱 → 在里面「删除」就是真删，没有更软的去处。
        .find(|candidate| {
            candidate.id != folder.id && role_by_name(&candidate.remote_name) == Some("trash")
        })
        .map(|folder| folder.remote_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DST: &str = "Trash";

    #[test]
    fn move_uses_move_command_when_server_supports_it() {
        let route = route_for(OperationKind::Move, true, true, Some(DST), None).unwrap();
        assert_eq!(
            route,
            Route::Move {
                destination: DST.to_string()
            }
        );
    }

    #[test]
    fn move_falls_back_to_copy_then_delete_without_move() {
        let route = route_for(OperationKind::Move, false, true, Some(DST), None).unwrap();
        assert_eq!(
            route,
            Route::CopyDelete {
                destination: DST.to_string(),
                expunge: true
            }
        );
    }

    #[test]
    fn without_uidplus_copy_stops_at_deleted_flag() {
        // 兜底那一档：没有 UIDPLUS，所以**绝不能** expunge。
        let route = route_for(OperationKind::Move, false, false, Some(DST), None).unwrap();
        assert_eq!(
            route,
            Route::CopyDelete {
                destination: DST.to_string(),
                expunge: false
            }
        );
    }

    #[test]
    fn delete_routes_into_trash_when_available() {
        let route = route_for(OperationKind::Delete, true, true, None, Some("Trash")).unwrap();
        assert_eq!(
            route,
            Route::Move {
                destination: "Trash".to_string()
            }
        );
    }

    #[test]
    fn delete_without_trash_expunges_in_place() {
        let route = route_for(OperationKind::Delete, true, true, None, None).unwrap();
        assert_eq!(route, Route::Expunge { expunge: true });
    }

    #[test]
    fn delete_without_trash_or_uidplus_only_marks_deleted() {
        let route = route_for(OperationKind::Delete, false, false, None, None).unwrap();
        assert_eq!(route, Route::Expunge { expunge: false });
    }

    #[test]
    fn move_without_destination_is_an_error() {
        assert!(route_for(OperationKind::Move, true, true, None, Some("Trash")).is_err());
    }

    #[test]
    fn flags_never_go_through_transfer_channel() {
        assert!(route_for(OperationKind::Flags, true, true, Some(DST), Some("Trash")).is_err());
    }
}
