//! Linux 的 Secret Service 钥匙串适配（P5-L）。
//!
//! 契约（`SecretStore` 的语义与错误约定）见 [`super`](super) 的模块文档；
//! Windows 侧的对照实现见 [`super::windows_store`]。
//!
//! ## 为什么用 D-Bus 而不是直接链 libsecret
//!
//! Secret Service 本身就是一条 D-Bus 接口（org.freedesktop.secrets）。
//! 直接链 `libsecret` 会把可执行文件钉死在某个发行版的 SONAME 上；走 D-Bus
//! 只要求「有个实现了该接口的守护进程在跑」，GNOME Keyring、KWallet、
//! `gnome-keyring-daemon` 独立启动等等都能用。
//!
//! ## 条目长什么样
//!
//! * **服务名**：[`SERVICE`]（`xremail`），固定值。它让本应用的条目在钥匙串
//!   里**可辨认、可批量清理**，与 Windows 那边的 `xremail:` 前缀是同一个用途。
//! * **用户名（attribute `username`）**：**原样**用传进来的 `secret_ref`
//!   （如 `xremail:account:<uuid>`）。`get` / `set` / `delete` 对同一个
//!   `secret_ref` 必须给出**完全一样**的属性——差一个字符就是另一个条目。
//! * **机密内容**：原始字节。`SecretValue` 已经是字节（文本机密由调用方按
//!   UTF-8 构造），这里**原样写入**，读回来也原样返回：再编一次 UTF-8
//!   会让读回来的字节对不上原来的值。
//!
//! ## 失败语义（与 Windows 实现逐条对齐）
//!
//! * 条目**不存在**不是错误：`get` 返回 `Ok(None)`、`delete` 返回 `Ok(false)`。
//! * 没有 D-Bus / 没有守护进程 / 会话被锁定 → [`SecretStoreError::Unavailable`]，
//!   **不落盘、不降级**。这条对无头环境尤其重要：CI 上没有钥匙串守护进程，
//!   调用方必须看到"存不进去"，而不是以为存好了。
//! * 错误信息里**不含**机密内容。
//!
//! ## 会话被锁定时会怎样
//!
//! `gnome-keyring` 的 collection 可以是锁着的。此时写入会触发一个**解锁提示**
//! （图形环境里弹框）；没有图形界面时 D-Bus 调用会失败。本实现**不去尝试自动解锁**——
//! 解锁是用户的决定，不是程序能替他做的。

use async_trait::async_trait;
use keyring::{Entry, Error as KeyringError};

use xremail_api::SecretStoreError;
use xremail_api::secret::{SecretStore, SecretValue};

/// 钥匙串里的服务名（对应 Windows 那边条目名的 `xremail:` 前缀）。
///
/// 固定值，不含账户信息：具体是哪个账户由 attribute 表达（见模块文档）。
const SERVICE: &str = "xremail";

/// Linux Secret Service 适配。
///
/// 无状态：钥匙串是用户会话级的，全进程共用一份。
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxSecretStore;

impl LinuxSecretStore {
    /// 构造。
    pub const fn new() -> Self {
        Self
    }

    /// 按 `secret_ref` 打开一个条目。
    ///
    /// `keyring::Entry::new` 只做参数校验，不碰钥匙串；真正的失败
    /// （没有守护进程等）发生在后面的 `get_password` / `set_password` 上。
    fn entry(secret_ref: &str) -> Result<Entry, SecretStoreError> {
        Entry::new(SERVICE, secret_ref).map_err(|error| map_error(&error, secret_ref))
    }
}

#[async_trait]
impl SecretStore for LinuxSecretStore {
    async fn get(&self, secret_ref: &str) -> Result<Option<SecretValue>, SecretStoreError> {
        let entry = Self::entry(secret_ref)?;
        match entry.get_secret() {
            Ok(bytes) => Ok(Some(SecretValue::new(bytes))),
            // 「没存过」与「取不到」必须分清，和 Windows 那边一致。
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(map_error(&error, secret_ref)),
        }
    }

    async fn set(&self, secret_ref: &str, value: &SecretValue) -> Result<(), SecretStoreError> {
        let entry = Self::entry(secret_ref)?;
        // 存**原始字节**，不是把它当字符串：`SecretValue` 本来就可能不是合法 UTF-8。
        entry
            .set_secret(value.as_bytes())
            .map_err(|error| map_error(&error, secret_ref))
    }

    async fn delete(&self, secret_ref: &str) -> Result<bool, SecretStoreError> {
        let entry = Self::entry(secret_ref)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(KeyringError::NoEntry) => Ok(false),
            Err(error) => Err(map_error(&error, secret_ref)),
        }
    }
}

/// 把 `keyring` 的错误按语义映射到 [`SecretStoreError`]。
///
/// 与 Windows 实现的原则一致：**说明要留，机密不能留**。
/// `secret_ref` 是不透明引用（`xremail:account:<id>`），不含凭据本身，可以出现在消息里。
fn map_error(error: &KeyringError, secret_ref: &str) -> SecretStoreError {
    match error {
        // 平台不支持该后端 / 没有 D-Bus 会话 / 没有守护进程。
        KeyringError::PlatformFailure(inner) => SecretStoreError::Unavailable(format!(
            "Secret Service 不可用（{secret_ref}）：{inner}。可能没有 D-Bus 会话，或没有 gnome-keyring / KWallet 之类的守护进程"
        )),
        // 被系统或用户拒绝（例如 collection 被锁定后解锁失败）。
        KeyringError::NoStorageAccess(inner) => SecretStoreError::Denied(format!(
            "系统拒绝了钥匙串访问（{secret_ref}）：{inner}。可能钥匙串被锁定了"
        )),
        // 参数问题：条目名不合法之类。
        KeyringError::Invalid(attribute, reason) => SecretStoreError::Invalid(format!(
            "钥匙串条目参数不合法（{secret_ref}，{attribute}）：{reason}"
        )),
        // `NoEntry` 由调用方各自处理（get → None，delete → false），走到这里说明
        // 是别的路径，如实报出来而不是假装成功。
        KeyringError::NoEntry => SecretStoreError::Unavailable(format!(
            "钥匙串条目不存在（{secret_ref}），但调用方期望它存在"
        )),
        other => SecretStoreError::Unavailable(format!("钥匙串操作失败（{secret_ref}）：{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误映射不看实现细节，只看「机密不进消息」。
    #[test]
    fn error_messages_never_leak_the_secret() {
        // 这些错误都只带 `secret_ref`（不透明引用）与系统给的说明，
        // 不含任何写入的机密。这里用一条假机密做反向检查。
        let error = map_error(
            &KeyringError::PlatformFailure(Box::new(std::io::Error::other("no dbus"))),
            "xremail:account:abc",
        );
        let text = error.to_string();
        assert!(!text.contains("AUTOTEST-AUTH-CODE"), "错误信息里不该有机密");
        assert!(text.contains("xremail:account:abc"), "但应当指出是哪个条目");
    }

    #[test]
    fn no_entry_maps_to_unavailable_not_silent_success() {
        // 这个分支在正常流程里走不到（get/delete 各自先拦了 `NoEntry`），
        // 所以它必须是**明确的失败**，不能是 Ok。
        let error = map_error(&KeyringError::NoEntry, "xremail:account:abc");
        assert!(matches!(error, SecretStoreError::Unavailable(_)));
    }

    #[test]
    fn platform_failure_is_unavailable_and_mentions_the_likely_cause() {
        let error = map_error(
            &KeyringError::PlatformFailure(Box::new(std::io::Error::other("no session bus"))),
            "xremail:account:abc",
        );
        let text = error.to_string();
        assert!(matches!(error, SecretStoreError::Unavailable(_)));
        assert!(text.contains("D-Bus"), "要说清最可能的原因：{text}");
    }

    #[test]
    fn denied_is_denied() {
        let error = map_error(
            &KeyringError::NoStorageAccess(Box::new(std::io::Error::other("locked"))),
            "xremail:account:abc",
        );
        assert!(matches!(error, SecretStoreError::Denied(_)));
    }
}
