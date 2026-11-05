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
//! | 没有 `MOVE`、有 `UIDPLUS` | `UID COPY` → `UID STORE +FLAGS (\Deleted)` → `UID EXPUNGE`（163 是这一类） |
//! | 没有 `MOVE` 也没有 `UIDPLUS` | **不发**，操作直接记 `Failed` 并报给用户（见 [`NO_UIDPLUS`]） |
//!
//! **裸 `EXPUNGE` 一律不用**：它会把这个文件夹里所有带 `\Deleted` 的信都删掉，
//! 包括别的客户端（手机、网页版）标了还没提交的。那也是用户的数据。
//!
//! 第二、三档的差别是**能不能真的把信弄走**：没有 `UIDPLUS` 时 `COPY` + 标删
//! 只会让源文件夹多一封 `\Deleted`、目标文件夹多一份副本，那封信**还在原地**。
//! 把它记成「移动成功」是假话（R35），所以宁可失败等人决定。
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

use std::time::Duration;

use xremail_core::model::{Folder, MailOperation, OperationKind, OperationState};
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::folders::role_by_name;
use xremail_protocol_imap::{
    CommandOutcome, Session, copy_messages_with_timeout, has_capability, mark_deleted_with_timeout,
    move_messages_with_timeout, uid_expunge_with_timeout,
};
use xremail_store::dao::{folder as folder_dao, message as message_dao, operation};

use crate::net::{SyncOptions, run_checked_with_timeout_for};
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
    ///
    /// 七个计数**全为 0** 才算空转（R36）：漏掉 `uncertain` / `quarantined` /
    /// `noop` 的话，一条「结果未知」的操作也会被报成「没事可做」，
    /// 上层据此判断的就成了假话。
    pub fn is_noop(&self) -> bool {
        self.total() == 0
    }

    /// 本轮一共处理了几条操作（应当是七项之和）。
    pub fn total(&self) -> usize {
        self.moved
            + self.deleted
            + self.retryable
            + self.failed
            + self.uncertain
            + self.quarantined
            + self.noop
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
    /// `UID COPY` → 标删 → `UID EXPUNGE`（走这条一定得有 `UIDPLUS`）。
    CopyDelete { destination: String },
    /// 就地删除（没有垃圾箱可去）。
    Expunge,
}

/// 没有 `UIDPLUS` 时给出的拒绝理由（R35）。
///
/// 没有 `UIDPLUS` 时能做的最多是「标 `\Deleted`」，而**裸 `EXPUNGE` 一律不用**
/// （会连带删掉别的客户端标了还没提交的信）。于是源文件夹里那封信**还在**：
/// 既没有搬进目标文件夹，也没有真的消失。把它记成成功，等于对用户说
/// 「已经移走了」——下次拉取它又回来了，用户会以为是丢信了。
/// 所以这里**拒绝**，让操作落到 `Failed` 并被界面报出来，由人决定怎么办。
const NO_UIDPLUS: &str = "服务器不支持 UIDPLUS：既不能安全地 expunge，也做不到真正的移走/删除。这个结果不算成功，需要你决定怎么处理";

/// 算出一条操作的路由。
///
/// `destination`：`Move` 操作显式给的目标邮箱名。
/// `trash`：账户里垃圾箱的邮箱名（`Delete` 用它）。
///
/// 没有 `UIDPLUS` 时一律 `Err`（理由见 [`NO_UIDPLUS`]）。
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
            Some(trash) => move_route(has_move, has_uidplus, trash),
            None if has_uidplus => Ok(Route::Expunge),
            None => Err(NO_UIDPLUS),
        },
        OperationKind::Move => match destination {
            Some(destination) => move_route(has_move, has_uidplus, destination),
            None => Err("移动操作没有记录目标文件夹"),
        },
        // 标记操作不归这里管。
        OperationKind::Flags => Err("标记操作不走移动/删除通道"),
    }
}

/// 「搬到 `destination`」在不同能力下的两种走法。
fn move_route(has_move: bool, has_uidplus: bool, destination: &str) -> Result<Route, &'static str> {
    if has_move {
        Ok(Route::Move {
            destination: destination.to_string(),
        })
    } else if has_uidplus {
        Ok(Route::CopyDelete {
            destination: destination.to_string(),
        })
    } else {
        // 没有 `UIDPLUS` 的 `COPY`+标删只能把信**复制**过去再把源里的标成
        // `\Deleted`：目标文件夹多一份、源文件夹还在，两边都对不上。
        Err(NO_UIDPLUS)
    }
}

/// 把某个文件夹里待提交的移动/删除意图回写到服务器。
///
/// 流程与 [`crate::push_flags`] 同构：取未完结操作 → 隔离旧 epoch →
/// **认领** → 按能力选路由 → 发命令 → **回读确认** → 更新状态。
///
/// 认领这一步的意义见 [`crate::push_flags`] 的同名说明：
/// 它是唯一的提交权闸门，没认领到的方什么都不做且**不算失败**。
///
/// 认领时用本进程的启动标识（[`crate::boot::boot_id`]），
/// 命令超时用 [`crate::net::COMMAND_TIMEOUT`]。
pub async fn push_transfers(
    pool: &sqlx::SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
) -> AppResult<TransferPushReport> {
    push_transfers_as(pool, session, folder, now_ms, crate::boot::boot_id()).await
}

/// 同 [`push_transfers`]，但显式指定认领归属（命令超时用默认值）。
pub async fn push_transfers_as(
    pool: &sqlx::SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
    boot_id: &str,
) -> AppResult<TransferPushReport> {
    push_transfers_as_with_options(
        pool,
        session,
        folder,
        now_ms,
        boot_id,
        SyncOptions::default(),
    )
    .await
}

/// 同 [`push_transfers_as`]，并显式给出命令超时。
///
/// **每一条命令都必须有上限**（R27）：服务器收了命令却不回话时，
/// 没有超时的那一条会把整个账户的同步循环永久钉住——连「重连」都轮不到，
/// 因为 actor 还卡在上一轮里。
pub async fn push_transfers_as_with_options(
    pool: &sqlx::SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
    boot_id: &str,
    options: SyncOptions,
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

    let timeout = options.command_timeout;

    // 3. 以**可写**方式打开源文件夹（拉取那一轮用的是 EXAMINE）。
    //
    // 打不开就**一条都不提交**，并且每一条都记下来：这一轮确实什么都没发，
    // 但「没发」的状态要写进库里，否则界面上看不出这批操作卡住了
    // （R33：认领失败也曾被静默 `continue` 吃掉）。
    //
    // 记成 `retryable` 而不是 `uncertain`：`SELECT` 是我们自己的命令，服务器
    // 明着回 `NO`/断开就说明这条命令**没执行**，发送状态本身是确定的。
    if let Err(error) = select_for_write(session, &folder.remote_name, timeout).await {
        let message = submit_error_message(&error);
        for op in &transfers {
            operation::mark_operation_retryable(pool, &op.id, "select_failed", message, now_ms)
                .await?;
            report.retryable += 1;
        }
        return Ok(report);
    }

    // 4. 能力与垃圾箱：每轮取一次，不逐条问。
    let capabilities = tokio::time::timeout(timeout, session.capabilities())
        .await
        .map_err(|_| AppError::Network(format!("读服务器能力超时（{timeout:?}）")))?
        .map_err(|error| AppError::Protocol(format!("读服务器能力失败: {error}")))?;
    let has_move = has_capability(&capabilities, "MOVE");
    let has_uidplus = has_capability(&capabilities, "UIDPLUS");
    let trash = find_trash(pool, folder).await?;

    // 5. 逐条提交。
    for (index, op) in transfers.iter().enumerate() {
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
            let Route::CopyDelete { destination } = &route else {
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
                Some(id) => {
                    match destination_has_message_id(session, destination, id, timeout).await {
                        Ok(found) => Ok(found),
                        Err(error) => Err(format!(
                            "重发前探测目标文件夹失败，因此不重发：{}",
                            submit_error_message(&error)
                        )),
                    }
                }
            };

            // 探测换了选中的文件夹，提交之前必须切回**可写**的源文件夹。
            //
            // 切不回去就别再动这个会话了：后面的每一条操作都建立在「选中了源文件夹」
            // 这个前提上，在错的状态上继续发命令会是另一类「邮件没搬、队列却空了」。
            // 剩下的操作（含这一条，除非探测本身已有结论）统一记成 `uncertain`，
            // 等下一轮重连再处理。
            if let Err(error) = select_for_write(session, &folder.remote_name, timeout).await {
                let message = submit_error_message(&error);

                // 这一条的探测结果本身就是「不确定」→ 报探测的原因，
                // 否则报切回来的原因。
                let reason = match &probe {
                    Ok(_) => message,
                    Err(probe_message) => probe_message.as_str(),
                };
                operation::mark_operation_uncertain(pool, &op.id, "unknown", reason, now_ms)
                    .await?;
                report.uncertain += 1;

                for rest in &transfers[index + 1..] {
                    operation::mark_operation_uncertain(pool, &rest.id, "unknown", message, now_ms)
                        .await?;
                    report.uncertain += 1;
                }
                return Ok(report);
            }

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

        match submit(session, op.uid, &route, copy_already_done, timeout).await {
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

                // 计数**互斥**（R36）：一封「服务器上本来就没有了」的信
                // 一条命令都没发，只能算 `noop`，不能同时算 `moved`/`deleted`。
                // 否则界面上的「N 封已移动」会把没发生的事也算进去。
                match (result, op.kind) {
                    (SubmitResult::AlreadyGone, _) => report.noop += 1,
                    (SubmitResult::Submitted, OperationKind::Delete) => report.deleted += 1,
                    (SubmitResult::Submitted, _) => report.moved += 1,
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
    timeout: Duration,
) -> Result<SubmitResult, SubmitError> {
    let uid32 = u32::try_from(uid).map_err(|_| SubmitError::Failed {
        message: format!("UID {uid} 超出 32 位范围，不能用 IMAP 命令表达"),
    })?;

    // 先看一眼它还在不在。
    //
    // 不在 → 上一次尝试多半已经生效了（应答丢了）。**不能重发**：
    // `MOVE` 不是幂等的，`COPY` 重发会在目标文件夹里多出一份。
    if !source_has_uid(session, uid32, timeout).await? {
        return Ok(SubmitResult::AlreadyGone);
    }

    match route {
        Route::Move { destination } => {
            move_messages_with_timeout(session, &[uid32], destination, timeout)
                .await
                .map_err(classify_submit_error)?;
            confirm(session, uid32, timeout).await?;
        }
        Route::CopyDelete { destination } => {
            // 探测已经确认目标文件夹里有这封，再发一次 `COPY` 就多一份。
            if !copy_already_done {
                copy_messages_with_timeout(session, &[uid32], destination, timeout)
                    .await
                    .map_err(classify_submit_error)?;
            }
            mark_deleted_with_timeout(session, &[uid32], timeout)
                .await
                .map_err(classify_submit_error)?;
            // 能走到这里就一定宣过 `UIDPLUS`（见 [`route_for`]）。
            uid_expunge_with_timeout(session, &[uid32], timeout)
                .await
                .map_err(classify_submit_error)?;
            confirm(session, uid32, timeout).await?;
        }
        Route::Expunge => {
            mark_deleted_with_timeout(session, &[uid32], timeout)
                .await
                .map_err(classify_submit_error)?;
            uid_expunge_with_timeout(session, &[uid32], timeout)
                .await
                .map_err(classify_submit_error)?;
            confirm(session, uid32, timeout).await?;
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
    timeout: Duration,
) -> Result<bool, SubmitError> {
    examine_for_probe(session, destination, timeout).await?;

    // `Message-ID` 理论上不含引号，但服务器给的东西不能信：
    // 去掉引号、空白与控制字符，避免拼出一条坏命令。
    let sanitized: String = message_id
        .chars()
        .filter(|c| *c != '"' && !c.is_whitespace() && !c.is_control())
        .collect();

    let command = format!("UID SEARCH HEADER Message-ID \"{sanitized}\"");
    let outcome = run_checked_with_timeout_for(session, &command, timeout)
        .await
        .map_err(classify_submit_error)?;

    match outcome {
        CommandOutcome::Search(uids) => Ok(!uids.is_empty()),
        other => Err(SubmitError::Uncertain {
            message: format!("期望 UID SEARCH 返回命中列表，实际是 {other:?}"),
        }),
    }
}

/// 回读确认：源文件夹里必须**没有**这封邮件了。
///
/// 只有这一种期望。曾经还有「还在但带 `\Deleted` 也算成功」——那是给没有
/// `UIDPLUS` 的服务器准备的终点，但它意味着**信没走**（R35），
/// 现在这种服务器直接走 [`NO_UIDPLUS`] 报失败，不再有这种终点。
///
/// 服务器回 `OK` 不等于改动生效（见 [ARCHITECTURE §6](../../../docs/ARCHITECTURE.md)）。
async fn confirm(session: &mut Session, uid: u32, timeout: Duration) -> Result<(), SubmitError> {
    if source_has_uid(session, uid, timeout).await? {
        return Err(SubmitError::Retryable {
            message: format!("回读确认失败：UID {uid} 仍在源文件夹里"),
        });
    }
    Ok(())
}

/// 源文件夹里还有没有这个 UID。
async fn source_has_uid(
    session: &mut Session,
    uid: u32,
    timeout: Duration,
) -> Result<bool, SubmitError> {
    let command = format!("UID FETCH {uid} (UID)");
    let outcome = run_checked_with_timeout_for(session, &command, timeout)
        .await
        .map_err(classify_submit_error)?;

    Ok(match outcome {
        CommandOutcome::Fetches(messages) => !messages.is_empty(),
        _ => false,
    })
}

/// 以**可写**方式选中一个文件夹，并给这条命令加上限。
///
/// 不加超时的 `SELECT` 会在服务器收下命令却不回话时**永久**挂住，
/// 而外层唯一的兜底是「重连」——可重连本身也在这一轮里（R27）。
///
/// 超时按 **`uncertain` 而不是普通失败**处理：`SELECT` 超时意味着会话里
/// 已经有一条没等到应答的命令，连接不能再拿来发移动/删除——发出去就说不清
/// 是哪条命令的结果。留成 `uncertain` 会让人工/下一轮的重连来处理（R27/R29）。
async fn select_for_write(
    session: &mut Session,
    mailbox: &str,
    timeout: Duration,
) -> Result<(), SubmitError> {
    let select = tokio::time::timeout(timeout, session.select(mailbox)).await;

    match select {
        Err(_elapsed) => Err(SubmitError::Uncertain {
            message: format!(
                "以可写方式打开 {mailbox} 超时（{timeout:?}），连接状态已不可信，本轮不再提交"
            ),
        }),
        Ok(Err(error)) => Err(SubmitError::Uncertain {
            message: format!("以可写方式打开 {mailbox} 失败: {error}"),
        }),
        Ok(Ok(_mailbox)) => Ok(()),
    }
}

/// 探测用 `EXAMINE`（只读打开），同样加上限。
async fn examine_for_probe(
    session: &mut Session,
    mailbox: &str,
    timeout: Duration,
) -> Result<(), SubmitError> {
    let examine = session.examine(mailbox);
    tokio::time::timeout(timeout, examine)
        .await
        .map_err(|_| SubmitError::Uncertain {
            message: format!("打开目标文件夹 {mailbox} 超时（{timeout:?}）"),
        })?
        .map(|_| ())
        .map_err(|error| SubmitError::Uncertain {
            message: format!("打开目标文件夹 {mailbox} 失败: {error}"),
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
                destination: DST.to_string()
            }
        );
    }

    #[test]
    fn without_uidplus_a_move_is_refused_instead_of_faked() {
        // R35：没有 UIDPLUS 时 `COPY` + 标删**搬不走**信，目标多一份、源里还在。
        // 那不能叫成功——必须报失败让人决定。
        let route = route_for(OperationKind::Move, false, false, Some(DST), None);
        assert_eq!(route, Err(NO_UIDPLUS));
    }

    #[test]
    fn without_uidplus_a_move_into_trash_is_refused_too() {
        // 删除走「挪进垃圾箱」那条路，同样离不开 UIDPLUS。
        let route = route_for(OperationKind::Delete, false, false, None, Some("Trash"));
        assert_eq!(route, Err(NO_UIDPLUS));
    }

    #[test]
    fn without_move_but_with_uidplus_a_single_move_command_is_not_needed() {
        // 163 那一档：没有 MOVE 但有 UIDPLUS → 三步走。
        let route = route_for(OperationKind::Move, false, true, Some(DST), None).unwrap();
        assert!(matches!(route, Route::CopyDelete { .. }));
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
        assert_eq!(route, Route::Expunge);
    }

    #[test]
    fn delete_without_trash_or_uidplus_is_refused() {
        // R35：没有 UIDPLUS 就只能标删，那封信还在原地。不许记成「已删除」。
        let route = route_for(OperationKind::Delete, true, false, None, None);
        assert_eq!(route, Err(NO_UIDPLUS));
    }

    #[test]
    fn a_report_is_only_noop_when_every_counter_is_zero() {
        // R36：`uncertain` / `quarantined` / `noop` 也是「这一轮有东西要报告」。
        assert!(TransferPushReport::default().is_noop());
        for report in [
            TransferPushReport {
                uncertain: 1,
                ..Default::default()
            },
            TransferPushReport {
                quarantined: 1,
                ..Default::default()
            },
            TransferPushReport {
                noop: 1,
                ..Default::default()
            },
        ] {
            assert!(!report.is_noop(), "{report:?} 不该被当成空转");
        }
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
