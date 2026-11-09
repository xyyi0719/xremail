//! 不支持平台凭据存储的实现。
//!
//! 非 Windows 平台（开发机上的 Linux CI、Android、macOS）暂时没有落地钥匙串适配。
//! 这里给出的是**明确的失败**，不是"假装成功"：
//!
//! * `get` 返回 `Ok(None)`——"没有存过"不是错误；
//! * `set` 返回 [`SecretStoreError::Unavailable`]——告诉调用方"存不进去"；
//! * `delete` 返回 `Ok(false)`——本来就没有。
//!
//! ## 为什么不退化成"存到文件里"
//!
//! 明文口令落在磁盘上，等于把"凭据已落盘"这件事变成假的。
//! 调用方（UI）据此提示用户"授权码没保存"，比悄悄降级诚实得多。
//!
//! ## 接入真正的钥匙串时
//!
//! 在本文件之外新增一个实现（Android Keystore / macOS Keychain），
//! 然后改 [`super::platform_store`](super::platform_store) 的分发即可。
//! 契约是 [`xremail_api::SecretStore`]，**不**要改它。

use async_trait::async_trait;
use xremail_api::SecretStoreError;
use xremail_api::secret::{SecretStore, SecretValue};

/// 不支持平台钥匙串时的占位实现。
#[derive(Debug, Clone, Copy, Default)]
pub struct UnsupportedSecretStore;

impl UnsupportedSecretStore {
    /// 构造。
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SecretStore for UnsupportedSecretStore {
    async fn get(&self, _secret_ref: &str) -> Result<Option<SecretValue>, SecretStoreError> {
        // 存不进去，也就取不出来。
        Ok(None)
    }

    async fn set(&self, _secret_ref: &str, _value: &SecretValue) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Unavailable(
            // 文案里**不要**再举「只有 Windows 已适配」这种例子：
            // Linux 的 Secret Service 适配（`linux_store.rs`）早已落地，
            // 这句话会把 macOS / Android 用户引向一个过时的印象。
            // 说「本平台」就够了——调用方拿到的就是这个平台的结果。
            concat!(
                "当前平台暂不支持保存凭据（本平台还没有钥匙串适配；",
                "Windows 用凭据管理器、Linux 用 Secret Service）"
            )
            .into(),
        ))
    }

    async fn delete(&self, _secret_ref: &str) -> Result<bool, SecretStoreError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_returns_none_because_nothing_can_be_stored() {
        let store = UnsupportedSecretStore::new();
        assert_eq!(
            store.get("xremail:account:a").await.expect("不该报错"),
            None
        );
    }

    #[tokio::test]
    async fn set_fails_loudly_instead_of_silently_degrading() {
        let store = UnsupportedSecretStore::new();
        let value = SecretValue::from_str_lossless("授权码");
        let error = store
            .set("xremail:account:a", &value)
            .await
            .expect_err("存不进去就该报错");
        assert!(
            error.to_string().contains("暂不支持"),
            "错误信息应当说清楚是平台不支持：{error}"
        );
    }

    #[tokio::test]
    async fn delete_reports_that_nothing_was_removed() {
        let store = UnsupportedSecretStore::new();
        assert!(!store.delete("xremail:account:a").await.expect("不该报错"));
    }
}
