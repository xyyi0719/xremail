//! Xremail 同步引擎。
//!
//! ## 阶段说明
//!
//! **当前（P0 / T2）只包含纯函数**：UID 差分、epoch 切换计算、bit 意图合并。
//! 它们无网络依赖、无 IO，可直接在 CI 跑（见
//! [TESTING_STRATEGY §5.1](../../../docs/TESTING_STRATEGY.md)）。
//!
//! 账户调度、IMAP 会话、操作队列持久化属于 P2。
//! **调度器已落地（2026-11-03）**：见 [`actor`]——一个账户一个 actor，
//! 轮询 + 手动触发 + 退避重试，每一轮同步都包在 `catch_unwind` 里（R13）。
//!
//! ## 为什么这些逻辑必须是纯函数
//!
//! 同步是整个应用最容易出错、代价最高的部分（错一次可能丢邮件或发错信）。
//! 把判定逻辑抽成纯函数，意味着：
//!
//! * 边界情况可以在 CI 里穷举，而不是靠真邮箱试错；
//! * 测试预期可以来自 **IMAP 语义与架构不变量**，而不是复述实现；
//! * 错误实现会让用例失败，而不是安静地放过。
//!
//! ## 三个不变量
//!
//! 1. **远端邮件的定位是 `(account_id, folder_id, uidvalidity, uid)`**，
//!    本地 ID 独立生成；RFC 5322 Message-ID 仅作线索
//!    （见 [ARCHITECTURE §4.1](../../../docs/ARCHITECTURE.md)）。
//! 2. **epoch（UIDVALIDITY）变了就不是增量更新，而是重建**：
//!    旧 UID 全部作废，绑定旧 epoch 的待提交操作必须隔离
//!    （见 [ARCHITECTURE §4.3](../../../docs/ARCHITECTURE.md)）。
//! 3. **flag 只做 bit 级增删，不整组替换**；`+Seen` 与 `-Seen` 可以冲突，
//!    靠本地顺序定序（见 [ARCHITECTURE §5 与 §6](../../../docs/ARCHITECTURE.md)）。
//!
//! ## 依赖方向
//!
//! `xremail-sync` → `xremail-core`。不认识 Tauri，也不认识 UI。

pub mod actor;
pub mod body;
pub mod boot;
pub mod diff;
pub mod epoch;
pub mod error;
pub mod intent;
pub mod net;
pub mod pull;
pub mod push;
pub mod reconcile;
pub mod sync;
pub mod transfer_push;

pub use actor::{
    AccountConnector, AccountEvent, Clock, ConnectFuture, EventSink, NoopSink, RetryPolicy,
    Scheduler, WakeReason, system_clock,
};
pub use body::{fetch_attachment_part, fetch_body, read_body};
pub use boot::boot_id;
pub use diff::{UidDiff, UidSet, diff_uids};
pub use epoch::{EpochTransition, PendingOperationBinding, StoredEpoch, plan_epoch_transition};
pub use error::{SESSION_CLASS_MARKER, classify_session_error, user_facing as user_facing_error};
pub use intent::{
    FlagDelta, FlagIntent, FlagPlan, MAX_REPLAN_ATTEMPTS, apply_intents, plan_flag_update,
    stale_intents,
};
pub use net::is_session_recoverable;
pub use pull::{FolderSyncReport, flags_from_imap, map_protocol_error, pull_folder};
pub use push::{FlagPushReport, push_flags, push_flags_as};
pub use reconcile::{
    MAX_OPERATION_ATTEMPTS, OperationObservation, ReconcileDecision, ReconcileReport,
    UNCERTAIN_STALE_MS, plan_reconcile, reconcile_account, reconcile_account_as,
};
pub use sync::{SyncSummary, sync_account, sync_account_as};
pub use transfer_push::{TransferPushReport, push_transfers, push_transfers_as};
