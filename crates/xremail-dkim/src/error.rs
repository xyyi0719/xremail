//! DKIM 的错误类型。
//!
//! 所有错误信息都**不含**密钥材料：失败时说的是「哪里不对、该怎么办」，
//! 不把 PEM 片段、密钥长度以外的任何东西拼进去。

use thiserror::Error;

/// DKIM 签名过程中可能出现的错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DkimError {
    /// 私钥不可用（格式不对、不支持的类型）。信息里不含密钥材料。
    #[error("私钥不可用：{0}")]
    Key(&'static str),

    /// 配置不合法（`d=` / `s=` 为空或含非法字符）。
    #[error("DKIM 配置不合法：{0}")]
    Config(String),

    /// 邮件里缺了必须参与签名的头字段。
    #[error("邮件里没有 {0} 头字段，无法签名")]
    MissingHeader(String),

    /// 底层签名失败。
    #[error("DKIM 签名失败：{0}")]
    Sign(String),
}
