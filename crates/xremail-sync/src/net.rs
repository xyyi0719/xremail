//! 同步引擎共用的网络辅助。

use std::time::Duration;

use xremail_core::AppError;
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

/// 这轮同步的失败是不是「**会话已经死了，重建会话就好**」。
///
/// ## 为什么要单独判一次（而不是看 [`AppError::is_retryable`]）
///
/// `is_retryable` 回答的是「再试一次值不值得」，答案针对的是**错误本身**；
/// 这里要回答的是「**连接**还活不活」，针对的是**会话**。两者不等价：
///
/// * `AppError::Protocol` 里既有「文件夹被删了」（会话没死，重连也一样失败），
///   也有「`BYE`」「协议帧解析失败」「IDLE 期间被中断」（会话已经不可信）——
///   只看错误类别会把后者当成「等下一轮轮询就好」，于是一次网络抖动要让用户
///   等满一整个 `poll_interval`（可能是 5 分钟）才恢复。
///
/// 注意「命令超时」与「`Io`」这两类**本来就会被翻成 [`AppError::Network`]**
/// （见 [`crate::pull::map_protocol_error`]），已经是可恢复的；真正需要打标记的
/// 是那些被 `ProtocolError::Imap` 吞掉的断线信号。
/// * `AppError::Storage` / `Io`（本地磁盘、内存库）是**本机**的问题，
///   重连不会让它变好，重连只会白白多打一次 TLS 握手。
///
/// ## 判据
///
/// 见 [`crate::error::SESSION_CLASS_MARKER`]：会话类错误在转换成 [`AppError`] 时
/// 打一个前缀标记。用标记而不是给 `AppError` 加变体，是因为 `AppError` 被 UI、
/// 命令层、存储层一起匹配：加一个变体，等于让全仓的 `match` 都重新审一遍。
/// 标记只挂在 `Protocol` 的载荷上，**类别不变**，事件出口会把它剥掉再展示。
///
/// * [`AppError::Network`]：传输层失败（`ProtocolError::{Io, Timeout, Connect}`
///   都落在这一类，见 [`crate::pull::map_protocol_error`]）——**可恢复**；
/// * 带标记的 [`AppError::Protocol`]：断线信号被协议层归成了 `Imap`——
///   服务器 `BYE`、IDLE 期间被中断、读帧失败——**可恢复**；
/// * [`AppError::Auth`]：**不可恢复**。它是终态，actor 发 `NeedsReauth` 后退出，
///   绝不进重连循环（重试一个错授权码永远不会成功，只会把「要人管」伪装成「会自愈」）；
/// * 不带标记的 [`AppError::Protocol`]：语义错误（文件夹不存在、加密方式不支持、
///   本地解析失败……）——**不可恢复**，急着重连只是白花一次握手；
/// * [`AppError::Storage`] / [`AppError::Io`]：本机问题，**不可恢复**；
/// * 其余（`Invalid` / `NotFound` / `Crypto` / `Unsupported` / `Other`）：**不可恢复**。
pub fn is_session_recoverable(error: &AppError) -> bool {
    match error {
        AppError::Network(_) => true,
        AppError::Auth(_) => false,
        AppError::Protocol(message) => {
            crate::error::strip_session_class(message).is_some() || is_dead_session_text(message)
        }
        _ => false,
    }
}

/// 错误文本里有没有「连接已经不可用」的证据。
///
/// `pub(crate)` 给 [`crate::pull::map_protocol_error`] 用：`ProtocolError::Imap`
/// 一个类别里装着「读帧/解析失败、`BYE`」和「`NO` / `BAD` 命令被拒」两件
/// 性质相反的事，只能靠这条判据把断线的那几支挑出来打标记。
///
/// 判据取的是我们**自己**写的中文措辞与 `ProtocolError` / async-imap 的固定
/// `Display` 前缀，不是服务器的自由文本。**有疑义时按会话已死处理**：
/// 多认一例的代价是多连一次（一次 TLS 握手），漏认一例的代价是掉线到下一个
/// `poll_interval`。
///
/// ## 一个反例：`connect` **不**在表里
///
/// 直觉上会想把 `connect` 放进来以便覆盖 `connection lost` / `connect error`。
/// 但 `connection refused` 也匹配它，而那是「**根本没连上**」——服务器侧问题，
/// [`crate::actor`] 特意为它保留了「连不上就不提前返回」的普通退避（10s 起、翻倍）。
/// 一旦被认成会话死亡，主循环会按短序列（1s→2s→4s…）反复握手，把维护中的服务器
/// 砸得更狠。所以判据的反方向**必须**是：「命令还在飞、连接没了」→ 重建会话；
/// 「要求握手时就没戏」→ 普通退避。已有的那几个词（`connection reset` /
/// `closed connection` / `io error`）已经覆盖了「连上了又断」的真实现象。
pub(crate) fn is_dead_session_text(message: &str) -> bool {
    const EVIDENCE: &[&str] = &[
        // `ProtocolError::Io` 的 `Display` 前缀：连上了、读写时传输层失败。
        "io error",
        "timed out",
        // TLS 的另一头把连接关了。rustls 的措辞是
        // `peer closed connection without sending TLS close_notify`，
        // `native-tls` / OpenSSL 一侧是 `unexpected eof` / `connection reset`。
        "close_notify",
        "closed connection",
        "unexpected eof",
        "connection reset",
        "broken pipe",
        // 对面**不发 FIN 也不发 `close_notify`** 直接消失了（NAT 老化、机器休眠、
        // 中间设备丢状态）。读端什么都收不到，于是我们自己的 `read_line` 超时先响：
        // `folders.rs` 的 `LIST` 超时文案、`idle.rs` 的「读取能力集超时（IDLE 前）」。
        // 这两种都是「连上了但对面不回话」，重建会话就能好——不认出来的话，
        // 一次休眠唤醒就要按普通失败退避（10s 起、翻倍），而不是 1s 重连。
        "读取能力集超时",
        "命令超时",
        // 我们自己的包装与协议层的固定措辞。
        "连接被断开",
        "连接已经失效",
        "连接可能已失效",
        "断开",
        "被中断",
        "bye",
    ];
    let lower = message.to_ascii_lowercase();
    EVIDENCE.iter().any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SESSION_CLASS_MARKER;

    fn classified(detail: &str) -> AppError {
        crate::error::classify_session_error(AppError::Protocol(detail.to_string()))
    }

    /// rustls 断线的**原样**措辞：`Network` 一定要算可恢复。
    #[test]
    fn a_peer_that_hung_up_is_network_class() {
        let text = "peer closed connection without sending TLS close_notify: \
                    https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof";
        assert!(is_session_recoverable(&AppError::Network(text.to_string())));
    }

    #[test]
    fn transport_failures_are_recoverable() {
        // `map_protocol_error` 把 Io / Timeout / Connect 都翻成 `Network`。
        assert!(is_session_recoverable(&AppError::Network(
            "连接被断开".into()
        )));
        assert!(is_session_recoverable(&AppError::Network(
            "命令超时（30s）: UID FETCH".into()
        )));
    }

    #[test]
    fn authentication_failures_are_terminal() {
        // 认证失败是终态：actor 发 `NeedsReauth` 后退出，**不进**重连循环。
        assert!(!is_session_recoverable(&AppError::Auth(
            "NO [AUTHENTICATIONFAILED]".into()
        )));
    }

    #[test]
    fn protocol_failures_on_a_dead_connection_are_recoverable() {
        // BYE / 读帧失败 / IDLE 被中断：协议层把它们归成 `Imap`，
        // `map_protocol_error` 会给它们打上会话标记。
        for detail in [
            "IDLE 期间服务器主动断开（BYE）",
            "IDLE 等待失败: connection lost",
            "IDLE 被中断（连接可能已失效）",
            "IDLE 初始化失败: 读取失败（连接已关闭）",
        ] {
            assert!(
                is_session_recoverable(&classified(detail)),
                "{detail} 应当算会话可恢复"
            );
        }
    }

    #[test]
    fn timeouts_and_io_errors_are_recoverable_without_a_marker() {
        // 命令超时与 `Io` 本来就会被翻成 `Network`，不需要标记。
        assert!(is_session_recoverable(&AppError::Network(
            "命令超时（30s）".into()
        )));
        assert!(is_session_recoverable(&AppError::Network(
            "连接被断开".into()
        )));
    }

    #[test]
    fn semantic_protocol_failures_are_not_recoverable() {
        // 会话好好的，是这条命令本身不成立——重连只会让下一轮一模一样地失败。
        // 尤其：`NO [INUSE]` 这类服务器回复**明确说过**命令没执行，连接可能好得很。
        for detail in [
            "本地解析失败: 未知编码",
            "NO [INUSE] mailbox locked",
            "BAD Invalid UID command",
            "账户 a@example.com 的 IMAP 加密方式是 None，目前只支持隐式 TLS（implicit）",
        ] {
            assert!(
                !is_session_recoverable(&AppError::Protocol(detail.to_string())),
                "{detail} 不该算会话可恢复"
            );
        }
    }

    #[test]
    fn local_failures_are_not_recoverable() {
        // 本机问题：重连不会让磁盘变好。
        assert!(!is_session_recoverable(&AppError::Storage(
            "库被锁住了".into()
        )));
        assert!(!is_session_recoverable(&AppError::Io("磁盘满了".into())));
        assert!(!is_session_recoverable(&AppError::Unsupported(
            "IDLE".into()
        )));
        assert!(!is_session_recoverable(&AppError::Invalid("参数".into())));
    }

    #[test]
    fn unclassified_errors_fall_back_to_the_text() {
        // 没人打标记的手工拼串路径：`ProtocolError::Io` 的 `Display` 是 `io error: …`。
        for detail in [
            "列文件夹失败: io error: connection reset by peer",
            "以可写方式打开文件夹失败: io error: connection lost",
            "IDLE 期间服务器主动断开（BYE）",
        ] {
            assert!(
                is_session_recoverable(&AppError::Protocol(detail.to_string())),
                "{detail} 应当算会话可恢复"
            );
        }
    }

    #[test]
    fn a_refused_handshake_is_not_a_dead_session() {
        // 反例（B1 首版曾把 `connect` 放进证据表，被这组用例截住）：
        // 「根本没连上」和「连上了又断」是**相反**的处置——
        // 前者要退到普通退避（别砸维护中的服务器），后者要立刻重建会话。
        // 证据表只收「命令还在飞、连接没了」的措辞，不能收握手阶段的失败。
        // 注意 `connect timed out` **会**命中 `timed out`，这是有意的：握手超时
        // 之后立刻再握一次，代价也只是再超时一次，而漏认「连上了但对面不回话」
        // 的代价是掉线到下一个 `poll_interval`。这里只钉住「明确被拒」这一类。
        for detail in [
            "connect error: Connection refused (os error 10061)",
            "connection refused",
            "dns error: no such host",
        ] {
            assert!(
                !is_dead_session_text(detail),
                "{detail} 是握手失败，不该算会话已死"
            );
        }
    }

    #[test]
    fn a_peer_that_hung_up_is_a_dead_session() {
        // 真机上最常看到的两种「对面把连接关了」：rustls 说 `close_notify` 没来，
        // OpenSSL 一侧说 `unexpected eof`。它们都是 `Network`，本来就可恢复——
        // 这里钉住的是**文本**，防止以后有人把这两句换成别的措辞而判据没跟上。
        for detail in [
            "peer closed connection without sending TLS close_notify: https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof",
            "unexpected eof: 对面没打招呼就走了",
            "connection reset by peer",
            "broken pipe",
        ] {
            assert!(
                is_session_recoverable(&AppError::Network(detail.into())),
                "{detail} 应当算会话可恢复"
            );
            assert!(is_dead_session_text(detail), "{detail} 应当在证据表里");
        }
    }

    #[test]
    fn a_peer_that_went_silent_is_a_dead_session() {
        // 第二种断法：对面**不发 FIN 也不发 `close_notify`**，只是不再回话
        // （NAT 老化、休眠唤醒、中间设备丢连接状态）。这时不会是 `Io`，
        // 而是我们自己的读超时先响——协议层把它包成 `ProtocolError::Timeout` 或
        // `Imap`，最终都落在 `AppError::Protocol` 上。这几句是本仓自己写的中文文案，
        // 必须被认成会话死亡，否则休眠唤醒后要按 10s 起的普通退避慢慢爬。
        for detail in [
            "命令超时（10s）: LIST \"\" \"*\"",
            "读取能力集超时（IDLE 前）",
        ] {
            let error = crate::pull::map_protocol_error(
                xremail_protocol_imap::ProtocolError::Timeout(detail.to_string()),
            );
            assert!(
                is_session_recoverable(&error),
                "{detail} 应当算会话可恢复，实际：{error:?}"
            );
            assert!(is_dead_session_text(detail), "{detail} 应当在证据表里");
        }
        // 反证：真的语义错误不该被这几条证据词连带认进去。
        let rejected = crate::pull::map_protocol_error(
            xremail_protocol_imap::ProtocolError::Rejected("[INUSE] mailbox locked".into()),
        );
        assert!(!is_session_recoverable(&rejected));
    }

    #[test]
    fn the_marker_is_not_observable_after_one_strip() {
        let error = classified("BYE");
        let text = error.to_string();
        assert!(text.contains(SESSION_CLASS_MARKER));
        // 对**已经打过标记**的错误再判一次仍然是真（不会因为前缀叠了两层就翻脸）。
        let twice = crate::error::classify_session_error(error);
        assert!(is_session_recoverable(&twice));
    }
}
