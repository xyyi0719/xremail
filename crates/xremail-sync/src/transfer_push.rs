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
//! 没有才就地标删。垃圾箱按下面的顺序认（[`trash_candidates`]）：
//!
//! 1. **本地库里记着的 `trash` 角色**——`LIST` 宣告了 `\Trash`，同步时写进
//!    `folder.role`（0010 迁移）。这是**服务器自己说的**，最可靠；
//! 2. 库里的角色是 `NULL`（这一轮同步没认出来，或者是升级前就落库的老行）
//!    时，**才**退回按文件夹名推断（[`role_by_name`]）。
//!
//! 顺序不能反过来：名字推断是猜测，宣告是事实。也不能省掉第 2 步：
//! `NULL` 的语义是「还不知道」，不是「没有垃圾箱」——省掉它的话，
//! 升级后还没同步过的账户会突然从「删除 = 进垃圾箱」变成「就地标删」，
//! 用户那边看起来就是**改了个版本，删除再也撤销不回来了**。
//!
//! 两道线索都没有时，删除就地标删（可撤销性无从谈起时，至少不把信挪到
//! 一个名字碰巧像垃圾箱的文件夹里去）。
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
//!
//! ## 目标文件夹的新 UID：拿得到就连同 epoch 一起写回（R39）
//!
//! 移动成功后，源文件夹的本地行删掉、目标文件夹立刻补一行——
//! 不补的话，那封信要等**下一轮全量同步**才出现在目标文件夹里，
//! 这期间本地对目标侧的 UID 一无所知：用户看到的是「挪了，但哪也找不到」，
//! 甚至可能被重复下载。
//!
//! 写回需要三样东西，缺一样就不写（**宁可空，不造假**）：
//!
//! 1. **目标侧的新 UID**：来自 `COPYUID`（RFC 4315）。`MOVE` 与 `COPY` 两条路由
//!    都可能有它；服务器不给就没有。旧 UID 在目标文件夹里是**另一个 UID 空间**，
//!    拿源 UID 去写等于凭空造一行。
//! 2. **目标文件夹的 epoch**：`COPYUID` 只带**源**文件夹的 UIDVALIDITY，
//!    目标的一律不给（见 `xremail_protocol_imap::transfer` 的说明）。
//!    本地 `message` 的身份是 `(account_id, folder_id, uidvalidity, uid)`，
//!    没有 epoch 就没有地方放这个 UID。于是一次 `EXAMINE` 目标文件夹把它取出来
//!    （见 [`destination_epoch`]）。取不到就不写。
//! 3. **本地库认识的同一个 epoch**：`EXAMINE` 拿到的 epoch 与库里目标文件夹
//!    记着的那个不一样时，说明目标文件夹刚被服务器重建过，下一个 UID 的含义
//!    已经不再是这次 `COPYUID` 说的那个（见 [`write_back`]）。同样不写。
//!
//! 三种情况都只记日志、不报错：移动**本身**已经成功了，一行缓存没写回去
//! 不是失败——代价是那封信晚一轮同步才出现（下一轮拉取会把它补上），
//! 而不是把操作记成失败让用户重试（`MOVE` 重发就是第二份副本）。
//!
//! 写回的那一行**新生成 `id`**，不搬源端那封的 `id`：`id` 是本地生成的，
//! 与 `(folder_id, uid)` 绑在一起、被 `message_body` / `attachment` 用外键引用，
//! 搬过去等于让「目标文件夹里的这封」顶着「源文件夹里那封」的名字，
//! 正文与附件都会挂到错的地方（理由见 [`write_back`]）。

use std::time::Duration;

use sqlx::SqlitePool;

use xremail_core::model::{Folder, MailOperation, Message, OperationKind, OperationState};
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::folders::role_by_name;
use xremail_protocol_imap::transfer::TransferOutcome;
use xremail_protocol_imap::{
    CommandOutcome, Session, copy_messages_with_timeout, examine_checked_with_timeout,
    has_capability, mark_deleted_with_timeout, move_messages_with_timeout,
    uid_expunge_with_timeout,
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
    /// 服务器报告搬走了、但目标侧的新 UID 没能写回本地几封（R39）。
    ///
    /// **不是失败**：移动已经生效，只是目标文件夹的本地行要等下一轮同步才出现。
    /// 单独计数是为了让它能从日志里查出来——否则「挪过去之后有一阵子找不到」
    /// 只能靠用户反馈才知道。
    ///
    /// 它**不计入** [`Self::total`]：那一项数的是「本轮处理了几条操作」，
    /// 而这里是其中几封操作的一个附注。算进去的话，一条 `uncertain` 操作
    /// 会被数两遍，界面上的合计就对不上了。
    pub not_written_back: usize,
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
    ///
    /// 只数七个互斥的结局，**不含** [`Self::not_written_back`]（那不是结局，
    /// 是 `moved` / `deleted` 的附注，见那个字段的说明）。
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
        operation::update_operation_state_as(
            pool,
            &op.id,
            Some(boot_id),
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
            operation::mark_operation_retryable(
                pool,
                &op.id,
                boot_id,
                "select_failed",
                message,
                now_ms,
            )
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
                        operation::update_operation_state_as(
                            pool,
                            &op.id,
                            Some(boot_id),
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
                operation::update_operation_state_as(
                    pool,
                    &op.id,
                    Some(boot_id),
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

            // 探测结果先收起来，**切回源文件夹之后再处理**。
            //
            // `destination_has_message_id` 会 `EXAMINE` 目标文件夹，无论成功还是失败
            // 会话都停在目标文件夹上。若带着这个状态 `continue`，下一条操作会对着
            // **错的文件夹**搜 UID，被误判成 `AlreadyGone` 后静默丢掉——那正是本条
            // 要修的那类「邮件没搬、队列却空了」。
            let probe = probe_copy_already_done(
                pool,
                session,
                &folder.account_id,
                &folder.id,
                epoch,
                op.uid,
                destination,
                timeout,
            )
            .await;
            let probed_unreachable = probe.probed && probe.found.is_none();

            // 探测换了选中的文件夹，提交之前必须切回**可写**的源文件夹。
            //
            // 切不回去就别再动这个会话了：后面的每一条操作都建立在「选中了源文件夹」
            // 这个前提上，在错的状态上继续发命令会是另一类「邮件没搬、队列却空了」。
            // 剩下的操作统一记成 `uncertain`，等下一轮重连再处理——**包括这一条**：
            // 它的 `COPY` 有没有发出去，本轮的探测已经给不出答案了，多烧一次配额
            // 只会让一条可能已经成功的操作离「重试用完」更近一步。
            if let Err(error) = select_for_write(session, &folder.remote_name, timeout).await {
                let message = submit_error_message(&error);

                // 这一条的探测结果本身就是「不确定」→ 报探测的原因，
                // 否则报切回来的原因。
                let reason = if probed_unreachable {
                    probe.reason.as_deref().unwrap_or(message)
                } else {
                    message
                };
                operation::mark_operation_uncertain(
                    pool, &op.id, boot_id, "unknown", reason, now_ms,
                )
                .await?;
                report.uncertain += 1;

                for rest in &transfers[index + 1..] {
                    operation::mark_operation_uncertain(
                        pool, &rest.id, boot_id, "unknown", message, now_ms,
                    )
                    .await?;
                    report.uncertain += 1;
                }
                return Ok(report);
            }

            match probe.verdict() {
                // 已经在目标文件夹里了：`COPY` 那一半不必再发。
                CopyProbeVerdict::AlreadyThere => copy_already_done = true,
                CopyProbeVerdict::StartOver => copy_already_done = false,
                CopyProbeVerdict::Unknown(message) => {
                    // 这里仍按 `uncertain` 处理（不判成功——我们确实没有证据）。
                    //
                    // 计次的口径：**真发出过命令**的那一次照旧走 `mark_operation_uncertain`
                    // （R26：`COPY` 可能已经生效，不计次就会每轮重发一份副本）。
                    // 唯一免计的是 `probe.probed == false`——本地**根本没有**这封信的
                    // `Message-ID`，于是这一轮连 `UID SEARCH` 都没发、更不可能发 `COPY`。
                    // 它看着像「无法确认」，实际几乎总是「源端本地行已经被上一轮**成功**
                    // 搬走之后删掉了」（`find_message_id_by_uid` 查的就是源文件夹）。
                    // 对它也计次的话，一条已经搬完、只是本地没收尾的操作会每轮同步
                    // 烧一次配额，直到被收成 `failed / retry_exhausted`——文案说
                    // 「服务器一直拒绝」，可服务器一次都没拒绝过。
                    //
                    // 顺带把上一轮问到的结论留着（见 [`target_has_copy_reason`]）：
                    // 免计的路径会反复走到这里，把「目标里已经有副本」这个事实丢掉的话，
                    // 就再也走不回「跳过 `COPY`、只补源端删除」那条收尾路了。
                    let reason =
                        if !probe.probed && probe.last == Some(CopyProbeVerdict::AlreadyThere) {
                            target_has_copy_reason()
                        } else {
                            message.to_owned()
                        };
                    if probe.probed {
                        operation::mark_operation_uncertain(
                            pool, &op.id, boot_id, "unknown", &reason, now_ms,
                        )
                        .await?;
                    } else {
                        mark_uncertain_without_attempt(
                            pool, &op.id, boot_id, "unknown", &reason, now_ms,
                        )
                        .await?;
                    }
                    report.uncertain += 1;
                    continue;
                }
            }
        }

        // 本地那封的内容**先抄下来**，再提交。
        //
        // 顺序不能反：源端本地行在提交成功后要删（那是「本地跟得上服务器」
        // 的那一步），删了就没有内容可抄给目标文件夹了。先抄进内存不动库，
        // 提交失败时直接丢掉——它只是个值，不是写了一半的行。
        let source = load_message(pool, &folder.account_id, &folder.id, epoch, op.uid).await;

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

                // 源端清理之后，把目标文件夹的本地行补上（R39）。
                //
                // 排在这里而不是提交之前：先补目标再删源，万一中间出错，
                // 同一封信会短暂地出现在两个文件夹里；反过来最坏的情况是
                // 两个文件夹里都没有——而那种状态下一轮同步会自己修好。
                //
                // 只有**移动**才有目标侧那一行。就地删除（`Route::Expunge`）的
                // `transfer` 本来就是 `None`，它不该走进写回，更不该被计成
                // 「没写回」——那会让界面上出现一笔莫名其妙的「1 封没跟上」。
                let writeback_target = match (&result, op.kind) {
                    (
                        SubmitResult::Submitted {
                            transfer: Some(transfer),
                        },
                        OperationKind::Move,
                    ) => Some((transfer, op.destination_folder_id.as_deref())),
                    _ => None,
                };

                if let Some((transfer, destination_folder_id)) = writeback_target
                    && let Err(not_written) = write_back(
                        pool,
                        session,
                        destination_folder_id,
                        source.as_ref(),
                        op.uid,
                        Some(transfer),
                        timeout,
                    )
                    .await
                {
                    report.not_written_back += 1;
                    tracing::debug!(
                        account = %folder.account_id,
                        source = %folder.remote_name,
                        uid = op.uid,
                        reason = %not_written,
                        "移动已生效，但目标文件夹的本地行没能写回，等下一轮同步补上"
                    );
                }

                // 写回多半 `EXAMINE` 过目标文件夹，会话现在停在**别的**文件夹上
                // （就连 `EXAMINE` 失败时停在哪里也不确定）。
                //
                // 下一条操作的前提是「选中的是源文件夹」：带着错的选中状态继续跑，
                // `source_has_uid` 会对着**目标**文件夹查**源** UID，把它误判成
                // 「已经不在了」而记成 `noop`——一封没搬的信被当成搬走了。
                // 这一条搬运**已经成功**，只是收不了尾，所以记 `uncertain`：
                // 不确定的是**连接状态**，不是邮件在哪。剩下的留给重连后再处理。
                //
                // 切不回去就直接收口（见下面 `return`）：连「这条操做的状态」
                // 都写不进去时，继续往下跑只会让报告与库里对不上。
                let reselected = select_for_write(session, &folder.remote_name, timeout).await;

                // 计数**互斥**（R36）：一封「服务器上本来就没有了」的信
                // 一条命令都没发，只能算 `noop`，不能同时算 `moved`/`deleted`。
                // 否则界面上的「N 封已移动」会把没发生的事也算进去。
                match &result {
                    SubmitResult::AlreadyGone => report.noop += 1,
                    SubmitResult::Submitted { .. } => match op.kind {
                        OperationKind::Delete => report.deleted += 1,
                        _ => report.moved += 1,
                    },
                }

                operation::update_operation_state_as(
                    pool,
                    &op.id,
                    Some(boot_id),
                    OperationState::Succeeded,
                    None,
                    None,
                    now_ms,
                )
                .await?;

                if let Err(error) = reselected {
                    let message = submit_error_message(&error);
                    for rest in &transfers[index + 1..] {
                        operation::mark_operation_uncertain(
                            pool, &rest.id, boot_id, "unknown", message, now_ms,
                        )
                        .await?;
                        report.uncertain += 1;
                    }
                    return Ok(report);
                }
            }
            Err(SubmitError::Retryable { message }) => {
                operation::mark_operation_retryable(
                    pool,
                    &op.id,
                    boot_id,
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
                operation::mark_operation_uncertain(
                    pool, &op.id, boot_id, "unknown", &message, now_ms,
                )
                .await?;
                report.uncertain += 1;
            }
            Err(SubmitError::Failed { message }) => {
                operation::update_operation_state_as(
                    pool,
                    &op.id,
                    Some(boot_id),
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
#[derive(Debug, Clone, PartialEq, Eq)]
enum SubmitResult {
    /// 命令生效了。
    Submitted {
        /// 服务器回的 `COPYUID`（含目标侧的新 UID）。
        ///
        /// `None` 有三种情况，对写回来说都一样（不写）：
        /// 就地删除（没有目标）、服务器没给 `COPYUID`、这一轮跳过了 `COPY`
        /// 那一半（副本是上一轮复制的，那次的 `COPYUID` 早随应答一起丢了）。
        transfer: Option<TransferOutcome>,
    },
    /// 服务器上源文件夹里已经没有这封邮件了，不必再发命令。
    AlreadyGone,
}

/// 按路由发命令，并**回读确认**。
///
/// `copy_already_done` 为真时跳过 `COPY` 那一半（调用方已经用 `Message-ID`
/// 在目标文件夹里探到副本了），只补完源文件夹的删除。
///
/// 返回值里带上 `COPYUID` 的解析结果（`destination_uids` 与 `source_uids`
/// 按下标一一对应，RFC 4315）：调用方要靠它把目标侧的新 UID 写回本地（R39）。
/// 以前这里把它解构掉扔了——「解析得很完整」和「生产路径用上了」是两件事。
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

    // 只有 `COPY` / `MOVE` 会给 `COPYUID`；就地删除没有目标，也就没有映射。
    let mut transfer: Option<TransferOutcome> = None;

    match route {
        Route::Move { destination } => {
            transfer = Some(
                move_messages_with_timeout(session, &[uid32], destination, timeout)
                    .await
                    .map_err(classify_submit_error)?,
            );
            confirm(session, uid32, timeout).await?;
        }
        Route::CopyDelete { destination } => {
            // 探测已经确认目标文件夹里有这封，再发一次 `COPY` 就多一份。
            //
            // 跳过的那一次也意味着没有 `COPYUID`：目标侧的新 UID 是**上一轮**
            // 复制出来的，当时应答就丢了。猜一个（例如「目标文件夹的 max+1」）不行：
            // 那种猜测一旦落库就是一个指着别的信的 UID，下一轮同步还未必能纠正它
            // （见 [`write_back`] 的说明），所以这一轮不写回。
            if !copy_already_done {
                transfer = Some(
                    copy_messages_with_timeout(session, &[uid32], destination, timeout)
                        .await
                        .map_err(classify_submit_error)?,
                );
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

    Ok(SubmitResult::Submitted { transfer })
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

/// 重发前那次探测的**结论**。
///
/// 做成枚举而不是 `Result<bool, String>`，是为了让调用方分得清三种「不是『在』」：
/// 「上一轮确认目标里还没有」「问过但没问到」「压根没去问」。
/// 这三者在**要不要计一次尝试**上完全不同（见 [`CopyProbe`] 的文档），
/// 而 `Err(String)` 会把它们拌成一句错误文本，没人分得出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyProbeVerdict<'a> {
    /// 目标文件夹里已经有这封：`COPY` 那一半不必再发。
    AlreadyThere,
    /// 上一轮确认过「目标里还没有」：这一次要正常发 `COPY`。
    StartOver,
    /// 没问到（或者根本没去问）。
    Unknown(&'a str),
}

/// 一次幂等探测的完整记录：**问过没有**、**问到了什么**、**为什么没问到**。
///
/// 单独记住「问过没有」是因为计次点的判据要看它：[`CopyProbe::verdict`] 把
/// 「没问」和「问了但没答」都归到 [`CopyProbeVerdict::Unknown`]（这一轮的动作
/// 一样：不发 `COPY`），但计次必须分开——没问 = 一条命令都没发出去，
/// 问了没答 = 可能发过，必须照旧计次（R26）。
#[derive(Debug, Clone)]
struct CopyProbe {
    /// 真的把 `Message-ID` 发出去问过目标文件夹了吗（`EXAMINE` 成功并发了 `SEARCH`）。
    probed: bool,
    /// 问到时的答案：目标里有没有。
    found: Option<bool>,
    /// 没问到时的原因（直接写给用户看的那一句）。
    reason: Option<String>,
    /// 上一轮探测已经记下的结论。
    last: Option<CopyProbeVerdict<'static>>,
}

impl CopyProbe {
    /// 这一轮该怎么办。
    ///
    /// 答案有两个来源：
    /// * 这一轮的探测真的问到了 → 用新答案；
    /// * 问不到 → 退回到**上一轮**记下的结论（它写在 `error_message` 里）。
    ///
    /// 为什么退回去是有意义的：源文件夹的本地行在上一次**成功**提交之后就被删了，
    /// 于是 `find_message_id_by_uid` 永远查不到 `Message-ID`。若一律按「没问到」处理，
    /// 一条早就搬好的操作会每轮同步都烧一次配额，最后被判成「服务器一直拒绝」。
    /// 而「`COPY` 已经进了目标文件夹」是一件**有状态**的事：目标里的副本不会自己
    /// 消失，上一轮问到了就一直是「有」，不必再问第二遍。
    fn verdict(&self) -> CopyProbeVerdict<'_> {
        match self.found {
            Some(true) => CopyProbeVerdict::AlreadyThere,
            Some(false) => CopyProbeVerdict::StartOver,
            None => match self.last {
                Some(CopyProbeVerdict::AlreadyThere) => CopyProbeVerdict::AlreadyThere,
                Some(CopyProbeVerdict::StartOver) => CopyProbeVerdict::StartOver,
                // 上一轮也没结论（或者根本没有结论可看）：
                // 这一刻我们知道的全部就是「不知道」，照旧不重发。
                _ => CopyProbeVerdict::Unknown(
                    self.reason.as_deref().unwrap_or(DEFAULT_PROBE_REASON),
                ),
            },
        }
    }
}

/// 没有更具体理由时的兜底文案（`Unknown` 总要有一句话给人看）。
const DEFAULT_PROBE_REASON: &str = "无法确认目标文件夹里是否已有上一轮复制的副本，因此本轮不重发";

/// 走一遍重发前的幂等探测。
///
/// 返回值同时带上「问过没有」与「上一轮的结论」，由 [`CopyProbe::verdict`]
/// 合成这一轮的动作；计次的宽窄见 [`CopyProbe`] 与调用点的注释。
#[allow(clippy::too_many_arguments)]
async fn probe_copy_already_done(
    pool: &sqlx::SqlitePool,
    session: &mut Session,
    account_id: &str,
    folder_id: &str,
    epoch: i64,
    uid: i64,
    destination: &str,
    timeout: Duration,
) -> CopyProbe {
    let stored = operation::find_pending_transfer(pool, account_id, folder_id, uid, epoch)
        .await
        .ok()
        .flatten();
    let last = stored
        .as_ref()
        .and_then(|operation| operation_verdict(operation.error_message.as_deref()));

    // 读库失败当成「查不到」：这条路径**不重发**，所以失败的方向是安全的，
    // 没必要把一个本地读错误升级成整轮 `push_transfers` 的 `Err`。
    let message_id: Option<String> =
        message_dao::find_message_id_by_uid(pool, account_id, folder_id, epoch, uid)
            .await
            .unwrap_or_default();

    let Some(message_id) = message_id else {
        // 本地没有这封信的 `Message-ID`。两种原因，这一层分不出来：
        // 源端本地行已经被删（上一轮**成功**搬走之后的样子），或者这封信本来就没有。
        // 所以**不探测**——探测会把会话切到目标文件夹，为一条注定发不出 `COPY`
        // 的操作白付一次 `EXAMINE` 与一次切回来。
        return CopyProbe {
            probed: false,
            found: None,
            reason: Some(
                "本地查不到这封信的 Message-ID（源端本地行可能已被上一轮删掉），无法确认目标文件夹里是否已有副本，因此本轮不重发"
                    .to_owned(),
            ),
            last,
        };
    };

    match destination_has_message_id(session, destination, &message_id, timeout).await {
        Ok(found) => CopyProbe {
            probed: true,
            found: Some(found),
            reason: None,
            last,
        },
        Err(error) => CopyProbe {
            probed: true,
            found: None,
            reason: Some(format!(
                "重发前探测目标文件夹失败，因此不重发：{}",
                submit_error_message(&error)
            )),
            last,
        },
    }
}

/// 上一轮探测记下的结论，写在 `error_message` 的**开头**。
///
/// 用一个不会出现在别处的前缀而不是新列：`error_kind` 仍要表达「这条操作现在
/// 处于什么状态」（`unknown` = 结果未知），而「探测问到了什么」是给下一轮
/// **自己读**的簿记，不是给用户看的分类。前缀之外的原文照旧显示给用户。
const PROBE_VERDICT_TAG: &str = "[probe:target-has-copy] ";

/// 「探测到目标里已经有副本、但这一轮没能收尾」的那句话。
///
/// 前缀见 [`PROBE_VERDICT_TAG`]：下一轮据此**不必再问一遍**目标文件夹。
fn target_has_copy_reason() -> String {
    format!(
        "{PROBE_VERDICT_TAG}目标文件夹里已经有这封信的副本，剩下的只是源端的删除；本轮没能收尾，下轮接着办"
    )
}

/// 从上一轮的错误文本里读出探测结论。
fn operation_verdict(message: Option<&str>) -> Option<CopyProbeVerdict<'static>> {
    let message = message?;
    if message.starts_with(PROBE_VERDICT_TAG) {
        Some(CopyProbeVerdict::AlreadyThere)
    } else {
        None
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

/// 为什么目标文件夹的本地行没能补上（R39）。
///
/// **不是错误类型**：移动已经生效了，这里每一条都只是「这一轮不写回,
/// 等下一轮同步补上」的理由。做成枚举是为了让调用方能计数、也让日志一眼看出
/// 是「服务器没给」还是「我们不敢用」——两者的下一步完全不同（前者没救，
/// 后者可能下一轮就正常了）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotWritten {
    /// 操作没记目标文件夹（删除，或者移动的目标行在本地没了）。
    NoDestination,
    /// 服务器没给 `COPYUID`（163 与沃都不保证给）。
    NoUidMapping,
    /// 打不开目标文件夹，拿不到它的 epoch。
    DestinationUnreachable,
    /// 服务器没给目标文件夹的 `UIDVALIDITY`。
    NoEpoch,
    /// 库里目标文件夹的 epoch 与服务器此刻的不一致（目标刚被重建过）。
    EpochMismatch,
    /// 本地连源端那一行都没有：没有可抄的内容。
    SourceMissing,
    /// 本地库读写失败。
    Store,
}

impl std::fmt::Display for NotWritten {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NoDestination => "操作没有记录目标文件夹",
            Self::NoUidMapping => "服务器没有给出 COPYUID 映射",
            Self::DestinationUnreachable => "打不开目标文件夹，拿不到它的 epoch",
            Self::NoEpoch => "服务器没有给出目标文件夹的 UIDVALIDITY",
            Self::EpochMismatch => "目标文件夹的 epoch 与库里记着的不一致（刚被重建过）",
            Self::SourceMissing => "本地已经没有源端那一行，没有可抄的内容",
            Self::Store => "本地库读写失败",
        };
        f.write_str(text)
    }
}

/// 把「挪过去的那封信」写进目标文件夹的本地库（R39）。
///
/// 调用的前提：服务器已经确认搬走，源端的本地行也已经删了。
///
/// ## 为什么必须拿到 epoch 才写
///
/// `message` 的身份是 `(account_id, folder_id, uidvalidity, uid)`，而 `COPYUID`
/// 只带**源**文件夹的 UIDVALIDITY（RFC 4315 §3）。没有目标 epoch 就没有地方
/// 放这个 UID：填一个凑数的（源 epoch、或 0）会让这一行看起来属于某个**别的**
/// 世代。那种行下一轮同步既不会认成「已经有」，也不会被当成「服务器已删」清掉
/// （差分是**在服务器给出的 epoch 之下**算的，别的世代的行走不到那一步），
/// 只有当目标文件夹**恰好**正好之后被重建时才顺带清掉——在那之前它会一直
/// 挂在列表里，点开就是一封对不上的信。而目标文件夹的 UID 空间与源毫无关系，
/// 拿源 epoch 去猜它更没意义。
///
/// 所以：拿不到 epoch 就什么都不写。代价是这封信晚一轮同步才出现在目标文件夹，
/// 而不是库里先多一条错的行、再指望某次对账删掉它。
///
/// ## 为什么不把源端那封的 `id` 搬过来
///
/// `id` 是本地生成的，和 `(folder_id, uid)` 绑在一起，而且 `message_body` /
/// `attachment` 都以它为外键。搬过去的话，目标文件夹里这一行会顶着源端那封
/// 名字——而源端那行刚被删掉，它的正文与附件正**级联删除**着。于是「同一封信」
/// 在两个文件夹里各自只对了一半：目标侧行看着对，正文却随源端一起没了；
/// 更糟的是源端那行删除的那一刻起，任何还拿着旧 `id` 的引用（界面、请求、
/// 发送队列）指向的都是不存在的行。
///
/// 所以这里**新生成 `id`**、只抄内容字段。正文**不**抄：`upsert_message_body`
/// 对已存在的行是覆盖，抄过去等于改写那一行已有的正文；要正文时按需重新拉一次
/// （与同步拉下来的那封一样）。
///
/// ## 为什么目标 epoch 与库里记着的不一致就不写
///
/// 这里 `EXAMINE` 拿到的是**此刻**服务器上的 epoch，库里记着的是**上一个看到它
/// 的地方**（通常是上一轮同步）的。不一致只有一种解释：目标文件夹在这中间被
/// 服务器重建过（RFC 3501 §6.3.1：UIDVALIDITY 只在重建时变）。此时 `COPYUID`
/// 里的那个 UID 到底属于新旧哪个世代、本地该按哪个世代落库，两边都说不准——
/// 而下面「写回之后下一轮同步不会重复」这件事恰恰建立在「本地与服务器对
/// epoch 的看法一致」之上。按对的写能省一次拉取，按错的写就是一行永远对不上的
/// 数据；两者收益不对称，所以这里不赌：等下一轮同步自己 `EXAMINE`、自己按
/// 它看到的 epoch 拉下来——那一轮没有「中间隔了一次同步」这个时间差。
///
/// ## 会碰会话的那一步：`EXAMINE`
///
/// 只读，不改 UID、不改标记，但它会**换掉会话上选中的文件夹**（失败时停在哪里
/// 更是不确定）。调用方在之后必须 [`select_for_write`] 切回源文件夹，
/// 否则下一次 `source_has_uid` 会对着目标文件夹查源 UID，把没搬的信判成
/// 「已经不在了」。
///
/// `source` 是**提交之前**从本地抄下来的那封（提交成功后源端的行已经删了）。
///
/// 返回 `Err(NotWritten)` 表示「这一轮没写，原因见枚举」；写成功返回 `Ok(())`。
async fn write_back(
    pool: &sqlx::SqlitePool,
    session: &mut Session,
    destination_folder_id: Option<&str>,
    source: Option<&Message>,
    source_uid: i64,
    transfer: Option<&TransferOutcome>,
    timeout: Duration,
) -> Result<(), NotWritten> {
    // 就地删除：压根没有目标侧，不是「本该补一行却没补上」，不该计进
    // `not_written_back`（那条计数说的是「挪过去了但本地没跟上」）。
    let transfer = transfer.ok_or(NotWritten::NoUidMapping)?;
    let destination_folder_id = destination_folder_id.ok_or(NotWritten::NoDestination)?;

    // 1. 目标侧的新 UID。与 `source_uids` **按下标**对应（RFC 4315），
    //    不是按大小、也不是「服务器那边的 `{uid}`」。
    let source_u32 = u32::try_from(source_uid).ok();
    let destination_uid = match source_u32.and_then(|uid| transfer.destination_uid_of(uid)) {
        Some(uid) => i64::from(uid),
        None => return Err(NotWritten::NoUidMapping),
    };

    // 2. 内容：调用方在提交之前抄下来的那一行。
    //
    // 拿不到不是错误——上面已经说了，代价只是晚一轮。
    let Some(source) = source else {
        return Err(NotWritten::SourceMissing);
    };

    // 3. 目标文件夹的 epoch。
    let epoch = destination_epoch(session, pool, destination_folder_id, timeout).await?;

    // 4. 新行：新 `id`，新 key，内容从源端那一行抄过来。
    let moved = Message {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: source.account_id.clone(),
        folder_id: destination_folder_id.to_string(),
        uid: destination_uid,
        uidvalidity: epoch,
        message_id: source.message_id.clone(),
        subject: source.subject.clone(),
        snippet: source.snippet.clone(),
        from: source.from.clone(),
        to: source.to.clone(),
        cc: source.cc.clone(),
        bcc: source.bcc.clone(),
        date_sent: source.date_sent,
        internal_date: source.internal_date,
        flags: source.flags,
        size: source.size,
        has_attachment: source.has_attachment,
    };

    // `upsert_message` 的冲突键就是那个四元组：万一目标文件夹里已经有同一 key
    // 的行（例如目标文件夹恰好被同步过、这一轮又把它当成新邮件写进来了），
    // 它会**覆盖**那一行而不是插重复——这正是「写回的 key 与下一轮同步算出来的
    // key 一致」的保证：两边都只认 `(folder, epoch, uid)`。
    message_dao::upsert_message(pool, &moved)
        .await
        .map_err(|_| NotWritten::Store)?;

    Ok(())
}

/// 本地源端那一行（按远端身份找）。
///
/// 先按文件夹列出、再按 `(uid, epoch)` 挑：DAO 没有「按 uid 取整行」的入口，
/// 而 `list_messages` 只碰元数据（正文在另一张表），一次移动查一下不心疼。
async fn load_message(
    pool: &sqlx::SqlitePool,
    account_id: &str,
    folder_id: &str,
    epoch: i64,
    uid: i64,
) -> Option<Message> {
    let messages = message_dao::list_messages(pool, account_id, folder_id)
        .await
        .ok()?;
    messages
        .into_iter()
        .find(|message| message.uid == uid && message.uidvalidity == epoch)
}

/// 目标文件夹此刻的 epoch，且**必须**与库里记着的一致。
///
/// 分两件事，两种失败都各有理由，见 [`write_back`] 的文档。
async fn destination_epoch(
    session: &mut Session,
    pool: &sqlx::SqlitePool,
    destination_folder_id: &str,
    timeout: Duration,
) -> Result<i64, NotWritten> {
    let destination = match folder_dao::get_folder(pool, destination_folder_id).await {
        Ok(folder) => folder.ok_or(NotWritten::NoDestination)?,
        Err(_) => return Err(NotWritten::Store),
    };

    let info = examine_checked_with_timeout(session, &destination.remote_name, timeout)
        .await
        .map_err(|_| NotWritten::DestinationUnreachable)?;

    let observed = info
        .uid_validity
        .map(i64::from)
        .filter(|value| *value > 0)
        .ok_or(NotWritten::NoEpoch)?;

    match destination.uidvalidity {
        Some(stored) if stored == observed => Ok(observed),
        _ => Err(NotWritten::EpochMismatch),
    }
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
/// 认法见模块文档的「删除 = 进垃圾箱」：库里的 `role` 优先，`NULL` 才按名字猜。
/// 都认不出来就返回 `None`——那时候删除会就地标删，**不会**乱挪到某个
/// 名字碰巧像垃圾箱的文件夹里去。
async fn find_trash(pool: &sqlx::SqlitePool, folder: &Folder) -> AppResult<Option<String>> {
    let folders = folder_dao::list_folders(pool, &folder.account_id).await?;
    Ok(trash_candidates(&folders, &folder.id)
        .first()
        .map(|folder| folder.remote_name.clone()))
}

/// 哪些本地文件夹可以当垃圾箱，**按可信程度排好序**。
///
/// 排序即决策（[`find_trash`] 取第一个）：
///
/// 1. 库里记着 `role = 'trash'` 的——服务器宣告过 `\Trash`，是事实；
/// 2. 库里的角色未知（`None`）且名字像垃圾箱的——名字是猜测，但比什么都没有强。
///
/// 为什么第 2 档只收 `role` 为空的文件夹：角色**已知**却**不是** `trash` 的
/// 文件夹（比如服务器宣告它是 `\Archive`），名字再像垃圾箱也不该去。
/// 服务器的宣告比名字权威，两条线索冲突时以宣告为准。
///
/// 为什么 `None` 不能直接当成「没有垃圾箱」：升级上来的老行、以及本轮
/// `LIST` 没认出来的文件夹，`role` 都是 `None`。把它当成否定答案，
/// 这些账户的删除就会从「进垃圾箱」悄悄降级成「就地标删」——
/// 一个**不可撤销**的动作，而用户没有任何提示。
///
/// 当前文件夹自己排除在外：在垃圾箱里点「删除」就是真删，没有更软的去处，
/// 挪进自己更是无意义的自环。
fn trash_candidates<'a>(folders: &'a [Folder], current_folder_id: &str) -> Vec<&'a Folder> {
    let others = || {
        folders
            .iter()
            .filter(move |candidate| candidate.id != current_folder_id)
    };

    let declared = others().filter(|candidate| candidate.is_trash());
    let guessed = others().filter(|candidate| {
        candidate.role.is_none() && role_by_name(&candidate.remote_name) == Some("trash")
    });

    declared.chain(guessed).collect()
}

// ===========================================================================
// 一条本模块自己的状态写入
// ===========================================================================

/// 把一条操作收成「结果未知」，**不**增加尝试次数。
///
/// ## 为什么不直接用 `operation::mark_operation_uncertain`
///
/// 那个函数把「改写状态」与「尝试次数 +1」绑在一起（R26），对**真发过命令**
/// 的出口是对的：`claim_for_push` 靠 `attempts < ?` 卡重发，不自增就永远重认领。
///
/// 但有**一个出口**它不适用：探测没给出答案，而这一轮连一条命令都还没发出去
/// （`probe.probed == false`，见调用点的注释）。对它也计次的话，一条「上一轮
/// 已经搬完、只是本地没收尾」的操作会每轮同步烧一次配额，最终被对账收成
/// `failed / retry_exhausted` —— 文案说「服务器一直拒绝这条意图」，
/// 可服务器一次都没拒绝过，信也早就搬好了。
///
/// ## 为什么写在这里而不是加进 `xremail-store`
///
/// 它是**这一条路径**的簿记（与探测结论前缀配套，见 [`PROBE_VERDICT_TAG`]），
/// 不是通用的状态迁移：存储层负责「状态机允许什么」，这里负责「这一轮到底
/// 算不算试过一次」。把口径留在用它的地方，才看得出宽窄。
///
/// ## 归属守卫照抄存储层
///
/// `AND (state <> 'running' OR owner_boot IS ?)` 与 `update_operation_state_as`
/// （R50）一字不差：只有**认领这一行的那个提交者**才能给它写结论。少了它，
/// 一台服务器卡住时，另一条通道（例如同一封邮件的 flag 行）刚写进去的结论
/// 会被这里覆盖掉。
async fn mark_uncertain_without_attempt(
    pool: &SqlitePool,
    id: &str,
    boot_id: &str,
    error_kind: &str,
    error_message: &str,
    now_ms: i64,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE mail_operation
        SET state = 'uncertain',
            error_kind = ?,
            error_message = ?,
            owner_boot = NULL,
            started_at = NULL,
            updated_at = ?
        WHERE id = ?
          AND state IN ('queued', 'running', 'retryable', 'uncertain')
          AND (state <> 'running' OR owner_boot IS ?)
        "#,
    )
    .bind(error_kind)
    .bind(error_message)
    .bind(now_ms)
    .bind(id)
    .bind(boot_id)
    .execute(pool)
    .await
    .map_err(|error| AppError::Storage(format!("标记操作结果未知失败: {error}")))?;

    Ok(())
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

    /// 造一个只关心名字与角色的文件夹。
    fn folder_with(id: &str, remote_name: &str, role: Option<&str>) -> Folder {
        Folder {
            id: id.to_string(),
            account_id: "acct-1".to_string(),
            remote_name: remote_name.to_string(),
            remote_id: None,
            local_name: None,
            delimiter: Some("/".to_string()),
            subscribed: true,
            selectable: true,
            last_sync_at: None,
            last_uid: None,
            highest_modseq: None,
            uidvalidity: Some(1),
            role: role.map(str::to_string),
        }
    }

    #[test]
    fn a_declared_trash_wins_over_a_name_that_looks_like_trash() {
        // 排序即决策：库里的 role 优先。
        let folders = vec![
            folder_with("f-name", "Deleted Items", None),
            folder_with("f-role", "Rubbish", Some("trash")),
            folder_with("f-inbox", "INBOX", Some("inbox")),
        ];
        let candidates = trash_candidates(&folders, "f-inbox");
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates[0].id, "f-role",
            "服务器宣告的 \\Trash 必须排在最前面"
        );
        assert_eq!(candidates[1].id, "f-name", "角色未知时才退回名字兜底");
    }

    #[test]
    fn an_unknown_role_is_not_the_same_as_no_trash() {
        // NULL = 不知道。它必须仍然有机会被名字兜底认出来——
        // 否则升级后还没同步过的账户，删除会从「进垃圾箱」悄悄退化成「就地标删」。
        let folders = vec![
            folder_with("f-inbox", "INBOX", Some("inbox")),
            folder_with("f-old", "已删除", None),
        ];
        let candidates = trash_candidates(&folders, "f-inbox");
        assert_eq!(candidates.len(), 1, "老行的角色为空，但名字能认出来");
        assert_eq!(candidates[0].id, "f-old");
    }

    #[test]
    fn a_known_non_trash_role_is_not_overridden_by_its_name() {
        // 服务器说它是别的角色，名字再像垃圾箱也不算。
        let folders = vec![
            folder_with("f-inbox", "INBOX", Some("inbox")),
            folder_with("f-archive", "Trash", Some("archive")),
        ];
        assert!(
            trash_candidates(&folders, "f-inbox").is_empty(),
            "宣告优先于名字：\\Archive 不该被当成垃圾箱"
        );
    }

    #[test]
    fn the_current_folder_is_never_its_own_trash() {
        // 在垃圾箱里点「删除」就是真删，挪进自己更是无意义的自环。
        let folders = vec![
            folder_with("f-trash", "Trash", Some("trash")),
            folder_with("f-inbox", "INBOX", Some("inbox")),
        ];
        assert!(trash_candidates(&folders, "f-trash").is_empty());
        assert_eq!(trash_candidates(&folders, "f-inbox")[0].id, "f-trash");
    }

    #[test]
    fn no_clue_at_all_means_no_trash() {
        let folders = vec![
            folder_with("f-inbox", "INBOX", Some("inbox")),
            folder_with("f-bin", "Rubbish", None),
        ];
        assert!(
            trash_candidates(&folders, "f-inbox").is_empty(),
            "既没宣告、名字也不像 → 就地标删，不猜"
        );
    }

    #[test]
    fn flags_never_go_through_transfer_channel() {
        assert!(route_for(OperationKind::Flags, true, true, Some(DST), Some("Trash")).is_err());
    }
}
