//! Android 的 Keystore 适配（R08 的一部分）。
//!
//! 契约（`SecretStore` 的语义与错误约定）见 [`super`](super) 的模块文档；
//! Linux 侧的对照实现见 [`super::linux_store`]，Windows 侧见
//! [`super::windows_store`]。
//!
//! ## 怎么和 Android Keystore 打交道
//!
//! Android Keystore 是 **Java/Kotlin API**（`android.security.keystore`），
//! 没有可以直接链接的 C 接口。所以这一层要经过 JNI。好在不必自己写 JNI：
//!
//! [`android_keyring`] 是 [`keyring`] crate 的**凭据构建器插件**——它把
//! `keyring::Entry` 的操作转到 Android 侧的实现上。于是本实现的代码**与
//! Linux 那一份几乎逐行相同**（同样的 `Entry::new` / `get_secret` /
//! `set_secret` / `delete_credential`），差别只有：
//!
//! * **构建器不同**：Linux 用 keyring 的 `sync-secret-service` 特性（D-Bus），
//!   Android 由 `android-keyring` 提供的构建器接管（JNI → Keystore）；
//! * **进程初始化**：JNI 需要一个有效的 `JNIEnv` / `JavaVM`。`tauri` /
//!   `wry` 在 Android 上启动时已经持有它们，`android-keyring` 通过
//!   `ndk-context` 拿到全局上下文——**不需要我们在 main 里手动传 JNIEnv**。
//!
//! ## 条目长什么样
//!
//! 与 Linux/Windows 保持同一套命名，便于排查时一眼对上：
//!
//! * **服务名**：[`SERVICE`]（`xremail`），固定值；
//! * **用户名/别名**：**原样**用传进来的 `secret_ref`（如
//!   `xremail:account:<uuid>`）。`get` / `set` / `delete` 对同一个 `secret_ref`
//!   必须给出完全一样的别名——差一个字符就是另一个条目。
//! * **机密内容**：原始字节，原样写入、原样返回。
//!
//! ## 失败语义（与 Linux/Windows 逐条对齐）
//!
//! * 条目**不存在**不是错误：`get` 返回 `Ok(None)`、`delete` 返回 `Ok(false)`。
//! * Keystore 不可用（JNI 未初始化、设备策略禁用、profile 受限等）→
//!   [`SecretStoreError::Unavailable`]，**不落盘、不降级**。调用方必须看到
//!   "存不进去"，而不是以为存好了。
//! * 错误信息里**不含**机密内容。
//!
//! ## 这一层**没有在真机上验过**
//!
//! 到这个提交为止，仓库里没有 Android 设备或模拟器；`cargo check` 能过
//! （见 `gen/android` 的构建），但**Keystore 的实际读写一次都没跑过**。
//! 这是 R08 里明确记着的缺口——代码就位不等于验收通过。
//! 真机验证的步骤写在 `docs/ROADMAP.md` 的 P4 小节。

use async_trait::async_trait;
use keyring::{Entry, Error as KeyringError};

use xremail_api::SecretStoreError;
use xremail_api::secret::{SecretStore, SecretValue};

/// 钥匙串里的服务名（与 Linux/Windows 保持同一个值）。
const SERVICE: &str = "xremail";

/// Android Keystore 适配。
///
/// 无状态：Keystore 是设备/应用级的，全进程共用一份。
#[derive(Debug, Clone, Copy, Default)]
pub struct AndroidSecretStore;

impl AndroidSecretStore {
    /// 构造。
    pub const fn new() -> Self {
        Self
    }

    /// 按 `secret_ref` 打开一个条目。
    ///
    /// `keyring::Entry::new` 只做参数校验，不碰 Keystore；真正的失败
    /// （JNI 没就绪、设备策略拒绝等）发生在后面的读写调用上。
    fn entry(secret_ref: &str) -> Result<Entry, SecretStoreError> {
        Entry::new(SERVICE, secret_ref).map_err(|error| map_error(&error, secret_ref))
    }
}

#[async_trait]
impl SecretStore for AndroidSecretStore {
    async fn get(&self, secret_ref: &str) -> Result<Option<SecretValue>, SecretStoreError> {
        let entry = Self::entry(secret_ref)?;
        match entry.get_secret() {
            Ok(bytes) => Ok(Some(SecretValue::new(bytes))),
            // 「没存过」与「取不到」必须分清，和另外两个平台一致。
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
/// 判据与 Linux 那份保持一致：**能不能重试、要不要用户介入**是两件事。
/// 这里只做「不可用 / 被拒绝 / 参数非法」三分类，其余一律归 `Unavailable`——
/// 宁可让调用方看到"存不进去"，也不要静默吞掉。
fn map_error(error: &KeyringError, secret_ref: &str) -> SecretStoreError {
    match error {
        KeyringError::PlatformFailure(inner) => SecretStoreError::Unavailable(format!(
            "Android Keystore 不可用（{secret_ref}）：{inner}。\
             可能是 JNI 尚未就绪，或设备策略禁用了凭据存储"
        )),
        KeyringError::NoStorageAccess(inner) => SecretStoreError::Denied(format!(
            "系统拒绝了 Keystore 访问（{secret_ref}）：{inner}。可能被设备策略或配置 Profile 限制"
        )),
        KeyringError::Invalid(attribute, reason) => SecretStoreError::Invalid(format!(
            "Keystore 条目参数不合法（{secret_ref}，{attribute}）：{reason}"
        )),
        KeyringError::NoEntry => SecretStoreError::Unavailable(format!(
            "Keystore 条目不存在（{secret_ref}），但调用方期望它存在"
        )),
        other => {
            SecretStoreError::Unavailable(format!("Keystore 操作失败（{secret_ref}）：{other}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误映射的**语义分类**：这三类必须分开，因为它们触发不同的上层动作
    /// （不可用 → 提示用户；被拒绝 → 提示权限；非法 → 程序 bug）。
    ///
    /// 注意：这里测的是**映射函数**，不是 Keystore 本身——后者需要真机。
    #[test]
    fn error_mapping_separates_unavailable_denied_and_invalid() {
        let unavailable = map_error(&KeyringError::PlatformFailure("jni 未就绪".into()), "ref1");
        assert!(matches!(unavailable, SecretStoreError::Unavailable(_)));

        let denied = map_error(&KeyringError::NoStorageAccess("策略禁用".into()), "ref2");
        assert!(matches!(denied, SecretStoreError::Denied(_)));

        let invalid = map_error(&KeyringError::Invalid("alias".into(), "空".into()), "ref3");
        assert!(matches!(invalid, SecretStoreError::Invalid(_)));
    }

    /// 错误信息里**不许**出现机密内容——这里只能验证「secret_ref 会原样出现、
    /// 但不含值」，因为 map_error 拿不到值本身。真正的脱敏由调用方保证。
    #[test]
    fn error_messages_carry_the_ref_for_troubleshooting() {
        let error = map_error(
            &KeyringError::PlatformFailure("boom".into()),
            "xremail:account:abc",
        );
        let text = format!("{error}");
        assert!(
            text.contains("xremail:account:abc"),
            "错误里应能定位到是哪条条目：{text}"
        );
    }
}
