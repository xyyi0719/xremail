//! 启动对账：把上一次进程留下的残骸收干净。
//!
//! ## 它解决什么问题
//!
//! 进程可能在**任何**时刻死掉（断电、崩溃、被系统杀掉），而死的位置决定了
//! 服务器上到底发生了什么：
//!
//! * 死在**发命令之前** → 命令没发出去，可以安全重放；
//! * 死在**发出之后、收到确认之前** → 命令可能已经执行了，重放就是重复执行。
//!
//! 本地能观测到的只有「这一行是不是还停在 `running`」。所以对账的目标不是
//! 「猜服务器做了什么」，而是**把没人负责的行标成没人负责**，
//! 交给下一轮的回读去核对（`push_flags` / `push_transfers` 天然幂等）。
//!
//! ## 三条硬规则
//!
//! 1. **绝不把 `uncertain` 改回 `queued` / `retryable`**。
//!    那等于替服务器判断「上次没执行」，而那正是我们不知道的事。
//!    唯一能把 `uncertain` 变回可提交状态的，是**人的显式决定**
//!    （与 `send_queue::retry_uncertain` 同一套哲学）。
//! 2. **只碰 `mail_operation` / `send_queue` 的状态与错误字段，不碰 `message`**。
//!    邮件本身的标记由回读确认后对齐，对账不能凭空造第二套真相。
//! 3. **按 boot 归属回收**。`running` 只说明有人认领了，不说明认领者还活着；
//!    同一个 `boot_id` 的在途操作不能被自己扫掉（见迁移 `0006_operation_owner.sql`）。
//!
//! ## 顺序本身是语义
//!
//! 先回收（别人的 `running` → `uncertain`），再判过期孤儿。
//! 反过来的话，一条属于上次进程、次数也已到顶的 `running` 会被
//! [`operation::expire_exhausted`] 的 `state IN (...)` 漏掉，永远留在 `running`。

use sqlx::SqlitePool;
use std::collections::HashMap;
use xremail_core::AppResult;
use xremail_core::model::{MailOperation, OperationKind, OperationState};
use xremail_store::dao::{folder as folder_dao, operation, send_queue};

/// 一条待提交操作最多尝试几次就彻底放弃。
///
/// 计数点只有一个：`mark_operation_retryable`（认领不自增，见该函数的注释）。
/// 所以这个数字就是「服务器拒绝了几次」。
///
/// 取 5：足以扛过服务器抖动、注入的 `[MODIFIED]` 冲突重算与短暂维护窗口，
/// 又不会让一条永远做不成的意图在队列里无限重放。
pub const MAX_OPERATION_ATTEMPTS: i64 = 5;

/// 一条待提交操作在**对账视角**下的观测。
///
/// 刻意只放对账需要的事实：它是纯数据，测试可以直接构造，
/// 不需要连服务器、也不需要真的建一个库。
#[derive(Debug, Clone)]
pub struct OperationObservation {
    pub id: String,
    pub kind: OperationKind,
    pub state: OperationState,
    /// 操作绑定的 epoch。
    pub op_uidvalidity: i64,
    /// 该操作所在文件夹**当前**的 epoch。
    ///
    /// `None` 表示本地根本没有这个文件夹（被删了），或文件夹还没同步过。
    /// 两者要靠 `folder_exists` 区分。
    pub folder_uidvalidity: Option<i64>,
    /// 本地是否还存在这个文件夹的行。
    pub folder_exists: bool,
    /// 移动操作的目标文件夹是否还在（非移动操作恒为 `true`）。
    pub destination_exists: bool,
}

/// 对账对**一条**操作的处置。
///
/// 注意没有 `MarkUncertain`：`running → uncertain` 由 SQL
/// [`operation::reclaim_running`] 在同一个对账回合里先一步完成，
/// 因为它要按 `owner_boot` 过滤，而那是**存储层**的簿记，不该漏到纯函数里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileDecision {
    /// 没什么可判的：留给下一轮回写去核对。
    Keep,
    /// 这条意图再也做不成了，标成终态失败。
    MarkFailed {
        error_kind: &'static str,
        message: String,
    },
}

/// 一次对账的汇总，供日志与事件使用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 收成 `uncertain` 的残骸操作数。
    pub reclaimed_operations: usize,
    /// 收成 `uncertain` 的残骸发信数。
    pub reclaimed_sends: usize,
    /// 尝试次数到顶、判为 `retry_exhausted` 的操作数。
    pub expired: usize,
    /// 因为 epoch 变化而隔离的操作数。
    pub quarantined: usize,
    /// 因为文件夹消失而放弃的操作数。
    pub failed_orphans: usize,
}

impl ReconcileReport {
    /// 这一轮什么都没做（用于「连跑两次是安全的」这类断言）。
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }
}

/// 纯函数：给出每条操作的处置。不碰库、不连服务器。
///
/// 判定顺序就是优先级——一条操作可能同时满足多个条件（既是孤儿、又次数到顶），
/// 先命中的先算，因为文案与 `error_kind` 要指向**最根本**的那个原因：
/// 文件夹都不存在了，说「重试次数用完」会误导人。
pub fn plan_reconcile(observations: &[OperationObservation]) -> Vec<ReconcileDecision> {
    observations
        .iter()
        .map(|observation| {
            // 1. 文件夹行没了：这条操作的 UID 没有任何可对照的对象。
            if !observation.folder_exists {
                return ReconcileDecision::MarkFailed {
                    error_kind: "folder_missing",
                    message: "操作所在文件夹已不存在（可能被删除或改名），这条意图无法再提交"
                        .to_string(),
                };
            }

            // 2. epoch 变了：同一个 UID 在新 epoch 下指向另一封信。
            //
            // 只有在**两边都知道 epoch** 时才判：文件夹还没同步过
            // （`folder_uidvalidity` 为 `None`）时不比，留给拉取建立 epoch 后
            // 由正常路径处理——凭空判死会误伤第一次同步前的合法意图。
            if let Some(current) = observation.folder_uidvalidity
                && current != observation.op_uidvalidity
            {
                return ReconcileDecision::MarkFailed {
                    error_kind: "epoch_changed",
                    message: format!(
                        "操作绑定的是 epoch {}，当前 epoch 是 {current}；旧 UID 指向另一封信，不能套用",
                        observation.op_uidvalidity
                    ),
                };
            }

            // 3. 移动的目标文件夹没了：这条意图做不成。
            //    删除不受影响（垃圾箱是提交时现找的，没有也能降级）。
            if observation.kind == OperationKind::Move && !observation.destination_exists {
                return ReconcileDecision::MarkFailed {
                    error_kind: "destination_missing",
                    message: "目标文件夹已不存在（可能被删除或改名），请重新选择".to_string(),
                };
            }

            ReconcileDecision::Keep
        })
        .collect()
}

/// 对账一个账户。幂等：连跑两次，第二次的报告应当是空的。
///
/// 触发点是应用启动时（`apps/shell/src-tauri/src/lib.rs` 的 `setup`）——
/// **必须**在这里做，而不是只靠调度器：调度器只对「已交给它调度」的账户跑，
/// 而崩溃恢复必须对**每个**账户都发生，否则没被调度的账户共残骸会
/// 永久停在 `running`，谁也推不动它。
///
/// 幂等是为了容忍重复调用（未来在 actor 的 `Startup` 轮再补一次也不会出错），
/// **不是**因为现在已经有两处触发点——目前只有启动一处。
pub async fn reconcile_account(
    pool: &SqlitePool,
    account_id: &str,
    now_ms: i64,
) -> AppResult<ReconcileReport> {
    reconcile_account_as(pool, account_id, crate::boot::boot_id(), now_ms).await
}

/// 同 [`reconcile_account`]，但显式指定「哪个 boot 算自己」。
///
/// 测试用它模拟「上一次进程留下的残骸被这一次收掉」。
pub async fn reconcile_account_as(
    pool: &SqlitePool,
    account_id: &str,
    boot_id: &str,
    now_ms: i64,
) -> AppResult<ReconcileReport> {
    // 第 1 步：回收残骸。**必须在任何提交之前**——否则下面判过期时，
    // 一条属于上次进程、次数到顶的 `running` 会被 `expire_exhausted`
    // 的状态过滤漏掉，永远留在 `running`。
    //
    // 刻意拆成独立的 `let` 而不是塞进结构体初始化：这里的**执行顺序本身是语义**
    // （先回收、后判过期），写成初始化表达式会把顺序藏起来，
    // 让以后整改变成「看不出为什么变错」。
    let reclaimed_sends = send_queue::reclaim_running(pool, account_id, boot_id, now_ms).await?;
    let reclaimed_operations =
        operation::reclaim_running(pool, account_id, boot_id, now_ms).await?;

    // 第 2 步：次数到顶的未完结操作收成终态。
    let expired =
        operation::expire_exhausted(pool, account_id, MAX_OPERATION_ATTEMPTS, now_ms).await?;

    let mut report = ReconcileReport {
        reclaimed_sends,
        reclaimed_operations,
        expired,
        ..Default::default()
    };

    // 第 3 步：孤儿与 epoch 隔离。这一步要跨表看，所以先把观测攒成纯数据。
    let pending = operation::list_all_pending_operations(pool, account_id).await?;
    if pending.is_empty() {
        return Ok(report);
    }

    let observations = build_observations(pool, &pending).await?;
    let decisions = plan_reconcile(&observations);

    for (observation, decision) in observations.iter().zip(decisions) {
        if let ReconcileDecision::MarkFailed {
            error_kind,
            message,
        } = decision
        {
            operation::update_operation_state(
                pool,
                &observation.id,
                OperationState::Failed,
                Some(error_kind),
                Some(&message),
                now_ms,
            )
            .await?;

            if error_kind == "epoch_changed" {
                report.quarantined += 1;
            } else {
                report.failed_orphans += 1;
            }
        }
    }

    Ok(report)
}

/// 把 `MailOperation` 补上「文件夹还在不在、epoch 是多少、目标还在不在」。
///
/// 每个不同的 `folder_id` / 目标只查一次：一轮对账里同一文件夹的操作通常成批出现。
async fn build_observations(
    pool: &SqlitePool,
    pending: &[MailOperation],
) -> AppResult<Vec<OperationObservation>> {
    // folder_id -> (是否存在, 当前 epoch)
    let mut folders: HashMap<&str, (bool, Option<i64>)> = HashMap::new();
    let mut destinations: HashMap<&str, bool> = HashMap::new();

    for operation in pending {
        if !folders.contains_key(operation.folder_id.as_str()) {
            let found = folder_dao::get_folder(pool, &operation.folder_id).await?;
            let entry = match &found {
                Some(folder) => (true, folder.uidvalidity),
                None => (false, None),
            };
            folders.insert(operation.folder_id.as_str(), entry);
        }

        if operation.kind == OperationKind::Move
            && let Some(destination) = operation.destination_folder_id.as_deref()
            && !destinations.contains_key(destination)
        {
            let exists = folder_dao::get_folder(pool, destination).await?.is_some();
            destinations.insert(destination, exists);
        }
    }

    Ok(pending
        .iter()
        .map(|operation| {
            let (folder_exists, folder_uidvalidity) = folders
                .get(operation.folder_id.as_str())
                .copied()
                .unwrap_or((false, None));

            // 没有目标（`None`）对移动来说就是「做不成」——与提交路径
            // `destination_of` 返回 `None` 时的处置一致。
            let destination_exists = match operation.kind {
                OperationKind::Move => operation
                    .destination_folder_id
                    .as_deref()
                    .and_then(|id| destinations.get(id).copied())
                    .unwrap_or(false),
                _ => true,
            };

            OperationObservation {
                id: operation.id.clone(),
                kind: operation.kind,
                state: operation.state,
                op_uidvalidity: operation.uidvalidity,
                folder_uidvalidity,
                folder_exists,
                destination_exists,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(kind: OperationKind) -> OperationObservation {
        OperationObservation {
            id: "op-1".to_string(),
            kind,
            state: OperationState::Queued,
            op_uidvalidity: 100,
            folder_uidvalidity: Some(100),
            folder_exists: true,
            destination_exists: true,
        }
    }

    #[test]
    fn a_healthy_operation_is_kept() {
        let decisions = plan_reconcile(&[observation(OperationKind::Flags)]);
        assert_eq!(decisions, vec![ReconcileDecision::Keep]);
    }

    #[test]
    fn an_operation_on_a_missing_folder_is_failed_not_kept() {
        let mut op = observation(OperationKind::Flags);
        op.folder_exists = false;
        op.folder_uidvalidity = None;

        match &plan_reconcile(&[op])[0] {
            ReconcileDecision::MarkFailed { error_kind, .. } => {
                assert_eq!(*error_kind, "folder_missing")
            }
            other => panic!("文件夹都没了却判成 {other:?}：这条意图会永远没人管"),
        }
    }

    #[test]
    fn a_stale_epoch_is_quarantined_even_when_the_folder_never_resynced() {
        // 这条钉的是「懒隔离」的漏洞：只隔离拉到过的文件夹，会让
        // 一个再也不会被拉取的文件夹里的旧 epoch 操作无限期留在队列里。
        let mut op = observation(OperationKind::Flags);
        op.op_uidvalidity = 100;
        op.folder_uidvalidity = Some(200);

        match &plan_reconcile(&[op])[0] {
            ReconcileDecision::MarkFailed {
                error_kind,
                message,
            } => {
                assert_eq!(*error_kind, "epoch_changed");
                assert!(
                    message.contains("100") && message.contains("200"),
                    "文案要把两个 epoch 都写出来，否则用户不知道发生了什么：{message}"
                );
            }
            other => panic!("epoch 变了却判成 {other:?}：旧 UID 会被套到另一封信上"),
        }
    }

    #[test]
    fn an_unknown_folder_epoch_is_not_compared() {
        // 文件夹还没同步过 → 不知道 epoch → 不许凭空判死。
        // 错误实现（`if let Some(cur) = ... else MarkFailed`）会让
        // 「第一次同步之前就排队的意图」全部被误杀。
        let mut op = observation(OperationKind::Flags);
        op.folder_uidvalidity = None;

        assert_eq!(plan_reconcile(&[op]), vec![ReconcileDecision::Keep]);
    }

    #[test]
    fn a_move_without_a_destination_is_failed() {
        let mut op = observation(OperationKind::Move);
        op.destination_exists = false;

        match &plan_reconcile(&[op])[0] {
            ReconcileDecision::MarkFailed { error_kind, .. } => {
                assert_eq!(*error_kind, "destination_missing")
            }
            other => panic!("目标没了却判成 {other:?}：这条移动会一直被重试到次数用尽"),
        }
    }

    #[test]
    fn a_delete_does_not_need_a_destination() {
        // 删除的垃圾箱是提交时现找的，没有也能降级成「打 Deleted 标记」，
        // 所以不该因为「目标文件夹」不存在就判死。
        let mut op = observation(OperationKind::Delete);
        op.destination_exists = false;

        assert_eq!(plan_reconcile(&[op]), vec![ReconcileDecision::Keep]);
    }

    #[test]
    fn an_uncertain_operation_is_never_requeued() {
        // 本模块最要紧的红线：对账只能判「做不成」，不能替服务器
        // 判断「上次没执行」然后把意图重发一次。
        let mut op = observation(OperationKind::Flags);
        op.state = OperationState::Uncertain;

        let decisions = plan_reconcile(&[op]);
        assert_eq!(decisions, vec![ReconcileDecision::Keep]);
    }

    #[test]
    fn a_missing_folder_wins_over_a_stale_epoch() {
        // 两个条件同时成立时，文案要指向更根本的那个原因。
        let mut op = observation(OperationKind::Flags);
        op.folder_exists = false;
        op.folder_uidvalidity = Some(999);

        match &plan_reconcile(&[op])[0] {
            ReconcileDecision::MarkFailed { error_kind, .. } => {
                assert_eq!(*error_kind, "folder_missing")
            }
            other => panic!("应优先报「文件夹没了」，实际 {other:?}"),
        }
    }

    #[test]
    fn the_report_says_noop_only_when_every_counter_is_zero() {
        assert!(ReconcileReport::default().is_noop());

        let touched = ReconcileReport {
            reclaimed_operations: 1,
            ..Default::default()
        };
        assert!(
            !touched.is_noop(),
            "凡是动过行就必须报出来，否则「连跑两次应当为空」的断言会变成空话"
        );
    }
}
