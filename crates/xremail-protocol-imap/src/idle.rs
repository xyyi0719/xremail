//! IMAP `IDLE`（RFC 2177）的**客户端侧**：连上、选中文件夹、等服务器说话。
//!
//! ## 它解决什么
//!
//! 没有 `IDLE` 时，新邮件最早只能在下一次轮询被发现（默认 5 分钟）。
//! `IDLE` 让服务器在变化发生的**当下**推一条 untagged 响应，
//! 客户端据此立刻同步——延迟从「一个轮询周期」降到「一次网络往返」。
//!
//! ## 它**不**解决什么（重要）
//!
//! `IDLE` 是**加速**，不是**保证**。推送会在这些情况下丢：
//! 连接被中间设备掐断、服务器在 `IDLE` 期间静默丢包、
//! 断线重连的那段时间正好有变化、以及服务器压根不实现 `IDLE`。
//!
//! 因此**轮询必须保留**。本模块的调用方（调度器）仍然按
//! `poll_interval_secs` 计时，`IDLE` 只是让「计时还没到」的时候也能被提前叫醒。
//! 换句话说：`IDLE` 只负责让同步**更早**发生，绝不负责让同步**一定会**发生。
//!
//! ## 为什么用 `EXAMINE` 而不是 `SELECT`
//!
//! `IDLE` 要求先选中一个文件夹。用 `EXAMINE`（只读）而不是 `SELECT`（读写），
//! 是因为观察者不该有副作用：`SELECT` 会清掉 `\Recent` 标记、并且在同一账户
//! 被两处同时选中时可能抢走对方的状态。观察者做只读的事，写留给同步流程。
//!
//! ## 为什么 `idle_timeout` 必须小于 29 分钟
//!
//! RFC 2177 明确要求客户端至少每 29 分钟结束一次 `IDLE` 再重新发起，
//! 否则服务器可以按「不活跃」把连接踢掉。这个超时到了**不是错误**，
//! 只是「这段时间没人说话」，重新发起即可（见 [`IdleEvent::Timeout`]）。

use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::{Response, Status};
use tracing::debug;

use crate::connection::Session;
use crate::error::ProtocolError;

/// 一次 `IDLE` 的配置。
#[derive(Debug, Clone)]
pub struct IdleConfig {
    /// 单条命令（`EXAMINE` / `IDLE` / `DONE`）的超时。
    pub command_timeout: Duration,
    /// 一次 `IDLE` 最长保持多久。
    ///
    /// **必须小于 29 分钟**（RFC 2177 §3 建议），否则服务器可能判定不活跃
    /// 而直接断连。构造时会被 [`IdleConfig::new`] 夹到上限以内。
    pub idle_timeout: Duration,
    /// 在哪个文件夹上 `IDLE`。留空则用 `INBOX`。
    pub mailbox: String,
}

/// `IDLE` 期间服务器推来的东西，归纳成**调用方真正需要区分的三种情况**。
///
/// 刻意不去区分「EXISTS / EXPUNGE / FETCH / RECENT」——
/// 因为调用方对它们的反应**完全一样**：同步一轮。
/// 把区别暴露出去只会诱使调用方写一堆「EXISTS 时做 A、EXPUNGE 时做 B」
/// 的分支，而正确的做法永远是「别猜，去拉一次，以服务器为准」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// 服务器说**有变化**：新邮件、邮件被删、或标记被别的客户端改了。
    MailboxChanged,
    /// 到 `idle_timeout` 了，期间没有任何推送。
    ///
    /// **这不是错误**。RFC 2177 要求定时重新发起 `IDLE`，
    /// 所以这是正常循环的一部分，调用方应当直接再发一次 `IDLE`
    /// （或借机做一轮轮询兜底）。
    Timeout,
    /// 服务器没宣告 `IDLE` 能力。调用方应当安静地退回纯轮询，
    /// **不要**当成失败去退避重试——那会把「服务器不支持」变成
    /// 一串无意义的告警。
    Unsupported,
}

/// 一次 `IDLE` 的结果：事件 + **被归还的 session**。
///
/// `session` 一定要还回来：`IDLE` 是「借走连接、用 `DONE` 还回来」的协议，
/// 拿不回来就只能重连。见 [`idle_once`] 的错误情况。
#[derive(Debug)]
pub struct IdleOutcome {
    /// 本次 `IDLE` 观察到的事件。
    pub event: IdleEvent,
    /// 结束 `IDLE` 后的连接，可以继续用。
    pub session: Session,
}

impl IdleConfig {
    /// RFC 2177 §3 建议的重发上限：29 分钟。
    pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(29 * 60);

    /// 造一份配置，`idle_timeout` 会被夹到 1 秒..=[`Self::MAX_IDLE_TIMEOUT`]。
    ///
    /// 夹紧而不是报错：`idle_timeout` 来自账户配置，用户填个 0 或填个
    /// 两小时都属于「意图合理但数值不对」，静默修正比让同步彻底不可用更好。
    pub fn new(
        command_timeout: Duration,
        idle_timeout: Duration,
        mailbox: impl Into<String>,
    ) -> Self {
        let mailbox = mailbox.into();
        Self {
            command_timeout,
            idle_timeout: idle_timeout.clamp(Duration::from_secs(1), Self::MAX_IDLE_TIMEOUT),
            mailbox: if mailbox.trim().is_empty() {
                "INBOX".to_string()
            } else {
                mailbox
            },
        }
    }
}

/// 在 `session` 上做**一次** `IDLE`，返回被归还的连接。
///
/// 成功返回时 session 一定可用（`DONE` 已被服务器确认）。
/// **失败返回时 session 已经丢了**——`IDLE` 借走连接之后，
/// 只有 `DONE` 成功才能拿回来（`async-imap` 的 `Handle::done` 消费 `self`，
/// 失败路径里它被直接 drop 掉，连接随之关闭）。
/// 所以调用方**不要**想着「失败了再用同一个 session 做点别的」，
/// 直接丢掉、下一轮重新 `connect` 才是对的。
///
/// ## 返回 `Unsupported` 时 session 仍然可用
///
/// 能力检查发生在 `IDLE` 之前，那时连接还在手上，所以会**原样归还**。
/// 调用方可以继续拿它做别的事。
pub async fn idle_once(
    mut session: Session,
    config: &IdleConfig,
) -> Result<IdleOutcome, ProtocolError> {
    // ---------- 1. 能力检查 ----------
    //
    // 放在最前面：不支持的服务器占多数（163、沃邮箱都不支持），
    // 那些连接没必要为一个注定失败的 `IDLE` 付超时代价。
    let capabilities = tokio::time::timeout(config.command_timeout, session.capabilities())
        .await
        .map_err(|_| ProtocolError::Timeout("读取能力集超时（IDLE 前）".into()))?
        .map_err(|e| ProtocolError::Imap(format!("读取能力集失败（IDLE 前）: {e}")))?;

    if !capabilities.has_str("IDLE") {
        debug!(mailbox = %config.mailbox, "服务器未宣告 IDLE，跳过");
        return Ok(IdleOutcome {
            event: IdleEvent::Unsupported,
            session,
        });
    }

    // ---------- 2. 选中文件夹 ----------
    //
    // `IDLE` 必须在「已选中某个文件夹」的状态下发起，
    // 否则服务器回 `NO IDLE not in selected state`（假服务器照此实现）。
    //
    // 先试只读的 `EXAMINE`：观察者不该有副作用（`SELECT` 会清掉 `\Recent`）。
    // 但有的服务器/代理会拒 `EXAMINE`，那就退回 `SELECT`——
    // **拿不到推送比丢个 `\Recent` 标记严重得多**。
    let examined = tokio::time::timeout(config.command_timeout, session.examine(&config.mailbox))
        .await
        .map_err(|_| ProtocolError::Timeout(format!("EXAMINE {} 超时", config.mailbox)))?;

    if examined.is_err() {
        debug!(
            mailbox = %config.mailbox,
            "EXAMINE 被拒，退回 SELECT（放弃只读，但拿到 IDLE 更重要）"
        );
        tokio::time::timeout(config.command_timeout, session.select(&config.mailbox))
            .await
            .map_err(|_| ProtocolError::Timeout(format!("SELECT {} 超时", config.mailbox)))?
            .map_err(|e| ProtocolError::Imap(format!("SELECT {} 失败: {e}", config.mailbox)))?;
    }

    // ---------- 3. 发 IDLE，等 `+ idling` ----------
    //
    // `idle()` 消耗 session。从这里开始，它只能靠 `done()` 拿回来。
    let mut handle = session.idle();

    // `init()` 不能省：`done()` 里有断言，没 `init()` 过就直接调用会 panic
    // （"Cannot call DONE on a non initialized idle connection"）。
    // 这个坑在 `probe.rs` 里已经踩过一次，见那里的注释。
    tokio::time::timeout(config.command_timeout, handle.init())
        .await
        .map_err(|_| ProtocolError::Timeout("IDLE 初始化超时（等 + idling）".into()))?
        .map_err(|e| ProtocolError::Imap(format!("IDLE 初始化失败: {e}")))?;

    // ---------- 4. 等推送 ----------
    //
    // `wait_with_timeout` 的两个返回值必须**都绑到具名变量**上：
    // `_interrupt` 是一个 `StopSource`，一旦被 drop，对应的 token 就「已停止」，
    // `wait` 会立刻返回 `ManualInterrupt`。写成 `_` 会让等待瞬间结束。
    // （写成 `_interrupt` 是具名绑定，活到函数作用域结束，这才是对的。）
    let (wait, _interrupt) = handle.wait_with_timeout(config.idle_timeout);

    // 外层再来一道超时：`wait_with_timeout` 自己的定时器靠 tokio 的
    // `timeout`，理论上不会挂住；但连接半死时读操作可能永远不回，
    // 多一道保险把「卡死」变成「可重试的错误」。
    let waited = tokio::time::timeout(config.idle_timeout + config.command_timeout, wait).await;

    // 外层超时（`Err`）与内层返回 `Timeout` 都归为「没人说话」。
    let event = match waited {
        Ok(Ok(response)) => classify(response)?,
        Ok(Err(error)) => {
            return Err(ProtocolError::Imap(format!("IDLE 等待失败: {error}")));
        }
        Err(_) => IdleEvent::Timeout,
    };

    // ---------- 5. DONE，把 session 还回来 ----------
    let session = tokio::time::timeout(config.command_timeout, handle.done())
        .await
        .map_err(|_| ProtocolError::Timeout("IDLE 结束超时（DONE）".into()))?
        .map_err(|e| ProtocolError::Imap(format!("IDLE 结束失败: {e}")))?;

    Ok(IdleOutcome { event, session })
}

/// 把一次 `IDLE` 期间收到的响应归类。
///
/// ## 为什么默认是 `MailboxChanged` 而不是「忽略」
///
/// 判错的代价是不对称的：
/// * **误报**（其实没变化却去同步）：多一轮同步，幂等，几毫秒到几百毫秒；
/// * **漏报**（真有变化却没同步）：用户看不到新邮件，直到下一个轮询周期，
///   最长 5 分钟。
///
/// 所以除了「服务器要断开」和「明确的保活」，其余一律当成「该同步了」。
fn classify(response: IdleResponse) -> Result<IdleEvent, ProtocolError> {
    match response {
        IdleResponse::Timeout => Ok(IdleEvent::Timeout),
        IdleResponse::NewData(data) => match data.parsed() {
            // 服务器要断开。`done()` 之后也拿不回连接了，直接报错让上游重连。
            // 单独判出来是为了让错误信息说清楚是**服务器主动断开**，
            // 而不是含糊的「IDLE 结束失败」。
            Response::Data {
                status: Status::Bye,
                ..
            } => Err(ProtocolError::Imap("IDLE 期间服务器主动断开（BYE）".into())),
            // 其余一律「有变化」：
            // `MailboxData::Exists` / `Recent`（新邮件）、
            // `Response::Expunge`（被删）、
            // `Response::Fetch`（别的客户端改了标记）、
            // `Response::Vanished`（QRESYNC）……
            _ => Ok(IdleEvent::MailboxChanged),
        },
        // 我们从不主动中断（没有把 stop token 停下来），
        // 所以拿到这个只可能是底层流结束了——当作连接失效。
        IdleResponse::ManualInterrupt => {
            Err(ProtocolError::Imap("IDLE 被中断（连接可能已失效）".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_clamps_idle_timeout() {
        // 用户填 0：夹到 1 秒，而不是「立刻超时」的忙循环。
        let config = IdleConfig::new(Duration::from_secs(10), Duration::ZERO, "INBOX");
        assert_eq!(config.idle_timeout, Duration::from_secs(1));

        // 用户填两小时：夹到 RFC 2177 的 29 分钟。
        let config = IdleConfig::new(
            Duration::from_secs(10),
            Duration::from_secs(2 * 60 * 60),
            "INBOX",
        );
        assert_eq!(config.idle_timeout, IdleConfig::MAX_IDLE_TIMEOUT);
    }

    #[test]
    fn config_defaults_mailbox_to_inbox() {
        for empty in ["", "   "] {
            let config = IdleConfig::new(Duration::from_secs(10), Duration::from_secs(60), empty);
            assert_eq!(config.mailbox, "INBOX");
        }
    }

    #[test]
    fn max_idle_timeout_is_under_29_minutes() {
        // RFC 2177 §3：至少每 29 分钟重发一次。这是防回归的硬约束。
        assert!(IdleConfig::MAX_IDLE_TIMEOUT <= Duration::from_secs(29 * 60));
    }
}
