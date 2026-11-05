//! 标记回写：把本地的 flag 意图提交给服务器并**确认**。
//!
//! ## 为什么必须「回读确认」
//!
//! 服务器回 `OK` **不等于**改动生效了。要确认，得**再读一次**：
//! 读回来的标记与意图一致，才算成功（见 [ARCHITECTURE §6](../../../docs/ARCHITECTURE.md)）。
//!
//! 163 没有 CONDSTORE（实测），所以不能用 `UNCHANGEDSINCE` 做条件写，
//! 只能走「读快照 → 重放意图 → **回读确认**」这条路。
//!
//! ## 三种失败要分清
//!
//! | 情况 | 状态 | 理由 |
//! |---|---|---|
//! | 服务器明确回了 `NO` | `retryable` | 服务器告诉我们**没执行** |
//! | 服务器回了 `NO [MODIFIED]` | `retryable` | 条件不满足，重新读快照重算 |
//! | 连接断了 / 超时 | `uncertain` | **不知道**服务器执行了没，盲目重试可能重复执行 |
//!
//! `uncertain` 是这个模块最要紧的分类：把它当成 `retryable`，
//! 就会出现「用户标记已读，结果服务器上被写了两次」这类脏状态。
//!
//! ## 意图必须绑定 epoch
//!
//! epoch 变了以后，旧 UID 指向的是**另一封信**。
//! 绑定旧 epoch 的操作必须**隔离**（标记为 `failed`），
//! 绝不能套用到新 UID 上（见 [ARCHITECTURE §4.3](../../../docs/ARCHITECTURE.md)）。

use std::collections::BTreeMap;
use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::AppError;
use xremail_core::AppResult;
use xremail_core::model::{Folder, MailOperation, MessageFlags, OperationKind, OperationState};
use xremail_protocol_imap::{CommandOutcome, ProtocolError, Session};
use xremail_store::dao::message as message_dao;
use xremail_store::dao::operation;

use crate::intent::{FlagIntent, MAX_REPLAN_ATTEMPTS, apply_intents, plan_flag_update};
use crate::net::{SyncOptions, run_checked_with_timeout_for};

/// 一轮标记回写的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlagPushReport {
    /// 发了多少条 `STORE` 命令。
    pub submitted: usize,
    /// 服务器已确认成功几封。
    pub succeeded: usize,
    /// 失败但可重试几封。
    pub retryable: usize,
    /// 失败且不该重试几封。
    pub failed: usize,
    /// 不知道服务器执行了没几封。
    pub uncertain: usize,
    /// 因 epoch 变化被隔离几封。
    pub quarantined: usize,
    /// 其中因净变化为零而**没发命令**几封。
    ///
    /// 这不是失败：意图已经在服务器上生效了（或者本来就没变）。
    /// 它是 `succeeded` 的**子集**——
    /// 服务器上已经是那个状态了，只是没必要再发命令。
    pub noop: usize,
}

impl FlagPushReport {
    /// 是否没有需要服务器处理的东西。
    pub fn is_noop(&self) -> bool {
        self.submitted == 0
    }
}

/// 把某个文件夹里待提交的标记意图回写到服务器。
///
/// 流程：
/// 1. 取出该文件夹的未完结操作；
/// 2. 绑定**旧 epoch** 的隔离掉（不套用到新 UID 上）；
/// 3. **认领**每条操作（`queued`/`retryable`/`uncertain` → `running`，记下 `owner_boot`）；
/// 4. 按 UID 分组，逐封：
///    读快照 → 重放意图算出净变化 → 发 `STORE` → **回读确认**；
/// 5. 按结果更新操作状态，并把本地邮件的标记对齐到服务器确认过的状态。
///
/// ## 为什么要先认领
///
/// 认领是**唯一的提交权闸门**（[`operation::claim_for_push`]）。
/// 两个提交者（例如 actor 的定时轮与 API 层的即时提交）同时看到同一批意图时，
/// 只有认领到的一方发命令；另一方什么都不做，并且**不算失败**。
/// 没有这道闸门时，同一批 `STORE` 会被发两遍——能活下来全靠服务器的条件写兼容，
/// 那是靠外部兵兜底，不是靠我们不犯错。
/// 把某个文件夹里待提交的标记意图回写到服务器。
///
/// 认领时用本进程的启动标识（[`crate::boot::boot_id`]）。
/// 需要模拟「另一代进程」的测试用 [`push_flags_as`]。
pub async fn push_flags(
    pool: &SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
) -> AppResult<FlagPushReport> {
    push_flags_as(pool, session, folder, now_ms, crate::boot::boot_id()).await
}

/// 同 [`push_flags`]，但显式指定认领归属。
///
/// `boot_id` 只影响「这行算谁的」：对账时只回收**别人**的行。
pub async fn push_flags_as(
    pool: &SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
    boot_id: &str,
) -> AppResult<FlagPushReport> {
    push_flags_as_with_options(
        pool,
        session,
        folder,
        now_ms,
        boot_id,
        SyncOptions::default(),
    )
    .await
}

/// 同 [`push_flags_as`]，但用**调用方给的**选项（主要是命令超时）。
pub async fn push_flags_as_with_options(
    pool: &SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
    boot_id: &str,
    options: SyncOptions,
) -> AppResult<FlagPushReport> {
    let timeout = options.command_timeout;
    let mut report = FlagPushReport::default();

    let Some(epoch) = folder.uidvalidity else {
        // 还没同步过 epoch → 不知道操作该往哪个 UID 上套。
        // 不是错误：没有 epoch 就没有可提交的东西。
        return Ok(report);
    };

    let pending = operation::list_pending_operations(pool, &folder.account_id, &folder.id).await?;

    // 2. 隔离绑定旧 epoch 的操作。
    let (stale, live): (Vec<MailOperation>, Vec<MailOperation>) =
        pending.into_iter().partition(|op| op.uidvalidity != epoch);

    for op in &stale {
        // 移动/删除的隔离归 `push_transfers` 管（那边要先按 kind 过滤，
        // 因为它还要区分「目标文件夹还在不在」）。两边都隔离一遍是幂等的，
        // 但会让 `quarantined` 计数翻倍，所以这里只处理标记操作。
        if op.kind != OperationKind::Flags {
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

    // 3. 按 UID 分组。
    //
    // **只取标记操作**：移动/删除的意图载荷是空的（它们不看 flag），
    // 若让它们进 `plan_flag_update`，净变化为零会被当成「已经在服务器上生效」
    // 直接标成成功——结果是邮件既没搬走也没删掉，队列却空了。
    // 那条通道在 [`crate::transfer_push`]。
    let mut by_uid: BTreeMap<i64, Vec<&MailOperation>> = BTreeMap::new();
    for op in live.iter().filter(|op| op.kind == OperationKind::Flags) {
        by_uid.entry(op.uid).or_default().push(op);
    }

    if by_uid.is_empty() {
        return Ok(report);
    }

    // 4. 以**可写**方式打开邮箱。
    //
    // 拉取那一轮用的是 `EXAMINE`（只读），而只读邮箱拒绝 `STORE`。
    // 同一个 session 上必须重新以 `SELECT` 打开才能写。
    //
    // 必须带超时：async-imap 的 `select` 不会自己超时，
    // 服务器收了命令不回话时，回写就永久停在这一行。
    // 超时按**传输层故障**处理：服务器可能已经切了，但我们不知道，
    // 与 [`classify_submit_error`] 对 `ProtocolError::Timeout` 的归类一致。
    match tokio::time::timeout(timeout, session.select(&folder.remote_name)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            return Err(AppError::Protocol(format!(
                "以可写方式打开文件夹失败: {error}"
            )));
        }
        Err(_) => {
            return Err(AppError::Network(format!(
                "以可写方式打开文件夹超时（{timeout:?}）: {}",
                folder.remote_name
            )));
        }
    }

    for (uid, ops) in by_uid {
        // 认领这一组：只对认领到的操作发命令。
        //
        // 认领失败有两种原因，处置相同（都不做事）：
        // * 被另一个提交者抢到手了——它会发现净变化为零或自己提交，不差我一个；
        // * `attempts` 已到顶——由对账收成 `failed / retry_exhausted`。
        // 两者都**不是错误**，不能计进 `retryable`。
        let mut claimed: Vec<&MailOperation> = Vec::with_capacity(ops.len());
        for op in &ops {
            if operation::claim_for_push(
                pool,
                &op.id,
                boot_id,
                crate::reconcile::MAX_OPERATION_ATTEMPTS,
                now_ms,
            )
            .await?
            {
                claimed.push(op);
            }
        }
        if claimed.is_empty() {
            continue;
        }

        let intents: Vec<FlagIntent> = claimed
            .iter()
            .map(|op| {
                FlagIntent::new(
                    op.uid,
                    op.uidvalidity,
                    op.intent.add,
                    op.intent.remove,
                    op.seq as u64,
                    op.base_version,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;

        match push_one(session, epoch, uid, &intents, timeout).await {
            Ok(Outcome::NoChange { flags }) => {
                // 净变化为零 → 意图已经在服务器上生效了，不必发命令。
                // 算成功：读到的快照本身就是服务器的确认。
                report.succeeded += 1;
                report.noop += 1;
                for op in &claimed {
                    operation::update_operation_state(
                        pool,
                        &op.id,
                        OperationState::Succeeded,
                        None,
                        None,
                        now_ms,
                    )
                    .await?;
                }
                // 本地视图对齐到**服务器上**的状态。
                message_dao::update_message_flags_by_uid(
                    pool,
                    &folder.account_id,
                    &folder.id,
                    epoch,
                    uid,
                    flags,
                )
                .await?;
            }
            Ok(Outcome::Confirmed { flags, submitted }) => {
                report.succeeded += 1;
                report.submitted += submitted;
                for op in &claimed {
                    operation::update_operation_state(
                        pool,
                        &op.id,
                        OperationState::Succeeded,
                        None,
                        None,
                        now_ms,
                    )
                    .await?;
                }
                // 本地视图对齐到**服务器确认过的**状态。
                message_dao::update_message_flags_by_uid(
                    pool,
                    &folder.account_id,
                    &folder.id,
                    epoch,
                    uid,
                    flags,
                )
                .await?;
            }
            Err(SubmitError::Retryable { message }) => {
                report.retryable += 1;
                for op in &claimed {
                    operation::mark_operation_retryable(
                        pool,
                        &op.id,
                        "server_rejected",
                        &message,
                        now_ms,
                    )
                    .await?;
                }
            }
            Err(SubmitError::Uncertain { message }) => {
                report.uncertain += 1;
                for op in &claimed {
                    // 必须走 `mark_operation_uncertain` 而不是 `update_operation_state`（R26）：
                    // 只有它会自增 `attempts`，而认领条件里有 `attempts < ?`。
                    // 不自增 = 这条永远可以被重新认领 = 每轮同步都重发一次命令，
                    // 「最多试 N 次就停」的上限永远到不了顶。
                    operation::mark_operation_uncertain(pool, &op.id, "unknown", &message, now_ms)
                        .await?;
                }
            }
            Err(SubmitError::Failed { message }) => {
                report.failed += 1;
                for op in &claimed {
                    operation::update_operation_state(
                        pool,
                        &op.id,
                        OperationState::Failed,
                        Some("permanent"),
                        Some(&message),
                        now_ms,
                    )
                    .await?;
                }
            }
        }
    }

    Ok(report)
}

/// 一封邮件的回写结果。
enum Outcome {
    /// 净变化为零，没发命令。
    ///
    /// 附带**服务器上的**标记：本地视图要照它对齐——
    /// 「意图已经在服务器上生效」是一句关于**服务器**的话，
    /// 本地视图不跟着改，操作一完成叠加层就撤了，
    /// 界面上的星标会**自己弹回去**。
    NoChange { flags: MessageFlags },
    /// 服务器已确认，附带**服务器上的**最终标记与发出的命令数。
    Confirmed {
        flags: MessageFlags,
        /// 实际发出的 `STORE` 命令条数。
        submitted: usize,
    },
}

/// 提交失败的三种分类。
///
/// 与 [`push_transfers`](crate::transfer_push) 共用：
/// 移动/删除与标记的失败语义完全一样（明确拒绝可重试、断线不确定、认证失败不重试）。
pub(crate) enum SubmitError {
    /// 服务器明确拒绝：可重试。
    Retryable { message: String },
    /// 不知道服务器执行了没：不能盲目重试。
    Uncertain { message: String },
    /// 不该重试。
    Failed { message: String },
}

/// 回写一封邮件。
///
/// 条件写被服务器以 `[MODIFIED]` 拒绝时，**重新读快照并按意图重算**，
/// 最多 `MAX_REPLAN_ATTEMPTS` 次（重试必须有界，否则会变成活锁）。
async fn push_one(
    session: &mut Session,
    epoch: i64,
    uid: i64,
    intents: &[FlagIntent],
    timeout: Duration,
) -> Result<Outcome, SubmitError> {
    for _attempt in 0..MAX_REPLAN_ATTEMPTS {
        // 读快照。
        let snapshot = read_snapshot(session, uid, timeout).await?;
        let plan = plan_flag_update(uid, snapshot.flags, epoch, snapshot.modseq, intents);

        let Some(delta) = plan.delta else {
            // 净变化为零：服务器上已经是目标状态。
            // 把服务器上的标记带回去，让本地视图跟着对齐。
            return Ok(Outcome::NoChange {
                flags: snapshot.flags,
            });
        };

        // 发命令。
        match send_delta(session, &delta, timeout).await {
            Ok(sent) => {
                // **回读确认**。
                let after = read_snapshot(session, uid, timeout).await?;

                // 只核对「我们动过的那些位」——服务器上别的位（比如 \Recent）
                // 不归我们管。
                let touched = delta.add.union(delta.remove);
                let expected = apply_intents(snapshot.flags, intents).intersection(touched);
                let actual = after.flags.intersection(touched);

                if expected == actual {
                    return Ok(Outcome::Confirmed {
                        flags: after.flags,
                        submitted: sent,
                    });
                }

                // 回读与意图不一致 → 服务器没接受（或被别人改了）。
                return Err(SubmitError::Retryable {
                    message: format!("回读确认失败：期望 {expected:?}，服务器上是 {actual:?}"),
                });
            }
            Err(SubmitError::Retryable { message }) => {
                // 条件写被拒（`[MODIFIED]`）：基线过期，重新读快照重算。
                if message.to_lowercase().contains("modified") {
                    continue;
                }
                return Err(SubmitError::Retryable { message });
            }
            Err(other) => return Err(other),
        }
    }

    Err(SubmitError::Retryable {
        message: format!("条件写重算次数超过上限（{MAX_REPLAN_ATTEMPTS}）"),
    })
}

/// 服务器上某封邮件的快照。
struct Snapshot {
    /// 标记。
    flags: MessageFlags,
    /// MODSEQ（无 CONDSTORE 时为 `None`）。
    modseq: Option<i64>,
}

/// 读一封邮件的标记。
async fn read_snapshot(
    session: &mut Session,
    uid: i64,
    timeout: Duration,
) -> Result<Snapshot, SubmitError> {
    let command = format!("UID FETCH {uid} (UID FLAGS)");
    let outcome = run_checked_with_timeout_for(session, &command, timeout)
        .await
        .map_err(classify_submit_error)?;

    let messages = match outcome {
        CommandOutcome::Fetches(messages) => messages,
        _ => Vec::new(),
    };

    let Some(message) = messages.first() else {
        // 邮件不在服务器上了（被删了）。
        return Err(SubmitError::Failed {
            message: format!("服务器上已经没有 UID {uid} 这封邮件"),
        });
    };

    Ok(Snapshot {
        flags: MessageFlags::from_imap_names(&message.flags),
        modseq: message.modseq.map(|value| value as i64),
    })
}

/// 把协议错误分类成提交失败的三种。
pub(crate) fn classify_submit_error(error: ProtocolError) -> SubmitError {
    match error {
        // 服务器明确拒绝 → 它**没执行** → 可重试。
        ProtocolError::Imap(detail) | ProtocolError::ConditionFailed(detail) => {
            SubmitError::Retryable { message: detail }
        }
        // 认证失败：用户得介入，不该反复重试。
        ProtocolError::Auth(detail) => SubmitError::Failed { message: detail },
        // 传输层故障：命令**可能**已经发出去了 → 不知道。
        ProtocolError::Io(detail)
        | ProtocolError::Timeout(detail)
        | ProtocolError::Connect(detail) => SubmitError::Uncertain { message: detail },
        other => SubmitError::Failed {
            message: other.to_string(),
        },
    }
}

/// 发一条 flag 变更命令。
///
/// **只发变化的位**，不整组替换 `FLAGS`（见 ARCHITECTURE §6）。
/// 增删同时存在时发**两条**命令：IMAP 的 `STORE` 一次只能做一种操作。
async fn send_delta(
    session: &mut Session,
    delta: &crate::intent::FlagDelta,
    timeout: Duration,
) -> Result<usize, SubmitError> {
    let mut sent = 0usize;
    let condition = match delta.unchangedsince {
        Some(value) => format!(" UNCHANGEDSINCE ({value})"),
        None => String::new(),
    };

    if !delta.add.is_empty() {
        let names = delta.add.to_imap_names().join(" ");
        let command = format!("UID STORE {} +FLAGS ({names}){condition}", delta.uid);
        run_checked_with_timeout_for(session, &command, timeout)
            .await
            .map_err(classify_submit_error)?;
        sent += 1;
    }

    if !delta.remove.is_empty() {
        let names = delta.remove.to_imap_names().join(" ");
        let command = format!("UID STORE {} -FLAGS ({names}){condition}", delta.uid);
        run_checked_with_timeout_for(session, &command, timeout)
            .await
            .map_err(classify_submit_error)?;
        sent += 1;
    }

    Ok(sent)
}
