//! Xremail 的 DKIM 签名（RFC 6376）。
//!
//! ## 职责边界（见 [ARCHITECTURE §2](../../../docs/ARCHITECTURE.md)）
//!
//! * 只做**离线、纯函数**的那一半：规范化、算哈希、生成 `DKIM-Signature` 头。
//! * 不碰网络（DNS 取公钥是验签方的事）、不碰 SQLite、不碰钥匙串、不碰进程时钟。
//! * 与 `xremail-message` 的分工：那边决定「一封邮件长什么样」（MIME 结构），
//!   这边决定「这串字节怎么签」。DKIM 要签的是**最终字节**，
//!   所以它必须排在 `build_message` 之后、冻结之前，而不是塞进 MIME 构造里。
//!
//! ## 为什么单独一个 crate 而不是放进 `xremail-crypto`
//!
//! `xremail-crypto` 的边界写明是「封装成熟密码库，不自行实现密码算法」，
//! 里面是 KDF 与 AEAD。DKIM 的难点不在密码学，而在**文本规范化**——
//! 它需要理解头字段、折行、CRLF、正文空行，那是「消息格式」的知识，
//! 与「加密」不是一回事。混进去会让一个只该依赖密码库的 crate 长出 MIME 语义。
//! 单独成 crate 也让它能纯同步地测：官方向量、篡改、边界都在本地跑完，
//! 不需要数据库或服务器。
//!
//! ## 用法
//!
//! ```no_run
//! use xremail_dkim::{SignRequest, SigningKey, sign_message};
//!
//! # fn demo(message: &[u8], pem: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
//! let key = SigningKey::from_pem(pem)?;
//! let signed = sign_message(&SignRequest::new(message, "example.com", "sel", &key))?;
//! // `signed.bytes` 就是可以冻结、可以提交的那份字节。
//! # Ok(signed.bytes)
//! # }
//! ```

pub mod canonicalize;
pub mod error;
pub mod key;
pub mod pubkey;
pub mod sign;

pub use canonicalize::{
    Canonicalization, HeaderField, canonicalize_body, canonicalize_body_relaxed, normalize_crlf,
    parse_canonicalization, split_headers_and_body,
};
pub use error::DkimError;
pub use key::{
    PublicComponents, SigningAlgorithm, SigningKey, pem_to_der, rsa_public_components, verify,
    verify_with_components,
};
pub use pubkey::VerifyingKey;
pub use sign::{
    DEFAULT_SIGNED_HEADERS, SignRequest, SignedMessage, body_hash_relaxed, sign_message,
};

/// 给一封邮件签名的便捷入口：`relaxed/relaxed`、`rsa-sha256`、不写 `t=`。
///
/// 返回插入 `DKIM-Signature` 头之后的**完整邮件字节**。调用方拿到它之后
/// 必须原样冻结——签名覆盖的正是这些字节，事后改动任何一个头、
/// 甚至只改一个行尾，都会让签名失效。
///
/// 返回值与 [`SignRequest`] 一样**不含**私钥：私钥只在调用期间存在于内存里。
pub fn sign(
    message: &[u8],
    key: &SigningKey,
    selector: &str,
    domain: &str,
) -> Result<Vec<u8>, DkimError> {
    let signed = sign_message(&SignRequest::new(message, domain, selector, key))?;
    Ok(signed.bytes)
}

/// 这一封邮件签完之后是否**仍然完整**：内部一致性自检。
///
/// 只检查「签名时用到的头、用到的体，在产出的字节里确实还在」，
/// 不做密码学校验（那是验签方的事，需要 DNS 里的公钥）。
///
/// 这条存在的意义是把 R22（冻结字节后不可改内容）写成可断言的东西：
/// 发信服务在冻结之前调用它，能抓住「某个中间层在签名后又动过字节」这类错误。
pub fn signed_bytes_are_consistent(signed: &SignedMessage) -> bool {
    let (headers, _body) = split_headers_and_body(&signed.bytes);
    headers.iter().any(|field| field.is_named("DKIM-Signature"))
        && headers.len() > signed.signed_headers.len()
}
