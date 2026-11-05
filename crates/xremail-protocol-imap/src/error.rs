//! 协议层错误。
//!
//! 分类原则（见 [ARCHITECTURE §9](../../../docs/ARCHITECTURE.md)）：
//! 错误按**可恢复原因**分类，且**不得**把凭据带进错误信息。

use thiserror::Error;

/// IMAP 协议层错误。
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// TCP / TLS 建连失败。
    #[error("连接失败: {0}")]
    Connect(String),

    /// TLS 握手或证书校验失败。
    ///
    /// **不降级**：证书校验失败就是失败，不提供「跳过校验」的开关。
    #[error("TLS 失败: {0}")]
    Tls(String),

    /// 底层 IO 错误。
    #[error("IO 失败: {0}")]
    Io(String),

    /// IMAP 协议层错误（服务器返回了非法或意外的响应）。
    #[error("IMAP 协议错误: {0}")]
    Imap(String),

    /// 认证被拒绝。
    ///
    /// **不重试**：需要用户重新确认凭据。
    #[error("认证失败: {0}")]
    Auth(String),

    /// 操作超时。
    #[error("操作超时: {0}")]
    Timeout(String),

    /// 入参不合法。
    #[error("无效的输入: {0}")]
    Invalid(String),

    /// 服务器不支持所需能力。
    #[error("服务器不支持该能力: {0}")]
    Unsupported(String),

    /// 服务器**回了 `NO`**：命令被明确拒绝，没执行。
    ///
    /// 与其它错误分开，是因为「`NO`」是一句关于**状态**的话：
    /// 命令确实没生效，调用方可以安全地换一条路重试，而不用把结果记成
    /// 「未知」。典型场景：服务器拒 `EXAMINE` 时，改用 `SELECT` 读同一个
    /// 文件夹是安全的，而超时或 IO 错误说明连接状态已经不可信，
    /// **不能**这样「补救」。
    #[error("服务器拒绝了命令: {0}")]
    Rejected(String),

    /// 条件写失败：服务器的 MODSEQ 变了（`NO [MODIFIED]`）。
    ///
    /// 与普通的 `NO` 分开：这一种**不是失败**，而是「你的基线过期了」。
    /// 正确处置是**重新读取快照并按意图重算**，而不是原样重试
    /// （见 [ARCHITECTURE §6](../../../docs/ARCHITECTURE.md)）。
    #[error("条件写失败（MODSEQ 已变）: {0}")]
    ConditionFailed(String),
}

impl ProtocolError {
    /// 是否值得重试。
    ///
    /// 认证失败与协议错误**不**在列：重试同样的输入只会得到同样的错误。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProtocolError::Connect(_) | ProtocolError::Io(_) | ProtocolError::Timeout(_)
        )
    }

    /// 是否需要用户介入（重新提供凭据等）。
    pub fn requires_user_action(&self) -> bool {
        matches!(self, ProtocolError::Auth(_))
    }
}
