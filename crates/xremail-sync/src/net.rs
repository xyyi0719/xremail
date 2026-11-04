//! 同步引擎共用的网络辅助。

use std::time::Duration;

use xremail_protocol_imap::{CommandOutcome, ProtocolError, Session, run_checked};

/// 单条 IMAP 命令的**默认**超时。
///
/// **必须有**：async-imap 自己**没有**命令超时，
/// 服务器不回应时客户端会一直挂着，同步任务就卡死在那儿。
///
/// 它**只是默认值**：实际超时由调用方传进来的
/// [`SyncOptions::command_timeout`] 决定，最终来源是
/// [`crate::actor::SyncPolicy::command_timeout`]。
/// 值与协议层的默认值同源，避免出现两套数值。
pub const COMMAND_TIMEOUT: Duration = xremail_protocol_imap::command::DEFAULT_COMMAND_TIMEOUT;

/// 一轮同步的可调选项。
///
/// 目前只有超时：它必须从 [`crate::actor::SyncPolicy`] 一路传到
/// 每一条命令，否则「policy 里配了超时」就是句空话。
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    /// 单条 IMAP 命令的超时。
    pub command_timeout: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            command_timeout: COMMAND_TIMEOUT,
        }
    }
}

/// 跑一条命令，带**默认**超时。
///
/// 没有额外选项可传的调用点用它；能拿到 [`SyncOptions`] 的调用点
/// 必须用 [`run_checked_with_timeout_for`]，否则 policy 形同虚设。
///
/// 超时按**传输层故障**处理——命令**可能**已经发出去了，
/// 所以调用方要按「不确定服务器执行了没」来对待（见 ARCHITECTURE §5）。
pub async fn run_checked_with_timeout(
    session: &mut Session,
    command: &str,
) -> Result<CommandOutcome, ProtocolError> {
    run_checked_with_timeout_for(session, command, COMMAND_TIMEOUT).await
}

/// 跑一条命令，带**指定的**超时。
///
/// 超时的处置同 [`run_checked_with_timeout`]。
pub async fn run_checked_with_timeout_for(
    session: &mut Session,
    command: &str,
    timeout: Duration,
) -> Result<CommandOutcome, ProtocolError> {
    match tokio::time::timeout(timeout, run_checked(session, command)).await {
        Ok(result) => result,
        Err(_) => Err(ProtocolError::Timeout(format!(
            "命令超时（{timeout:?}）: {command}"
        ))),
    }
}
