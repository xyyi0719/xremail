//! 平台凭据存储（[ARCHITECTURE §9](../../../../../docs/ARCHITECTURE.md)）。
//!
//! ## 这层只管一件事
//!
//! 把一条机密安全地放进**操作系统的凭据保管设施**，并能取回来。
//!
//! **不是** SQLite 里的 `local_secret` 表——那是 `xremail-api` 的 `vault` 的事。
//! **不是** `xremail-crypto` 的密文封装——那是加解密的事。
//!
//! ## 库里只存不透明引用
//!
//! 库里 [`xremail_core::model::Account::secret_ref`] 存的是**引用**
//! （如 `xremail:account:<account_id>`），不是凭据本身。
//! 凭据只待在平台钥匙串里。这样做的收益：
//!
//! * 数据库文件即使被拷走，也**不含**可直接使用的口令；
//! * 换机器 / 删账户时，可以精确地删掉那一条钥匙串记录；
//! * 同一套代码在 Windows / Android / macOS 上只是换掉本模块的实现。
//!
//! ## 平台差异
//!
//! * Windows：凭据管理器（Credential Manager）——见 [`windows_store`]。
//! * Linux：Secret Service（D-Bus）——见 [`linux_store`]。GNOME Keyring / KWallet
//!   都实现了这个接口；没有守护进程时**明确报错**，而不是落盘。
//! * Android：Keystore（JNI，经 `android-keyring` 构建器）——见 [`android_store`]。
//!   **注意：这一层还没有真机验证**（R08）。
//! * 其它平台（macOS 等）：**暂不支持**（见 [`unsupported`]）——存取都明确报错，
//!   **不静默降级**成"内存里存着"或"存到文件里"。
//!
//! 静默降级会让人以为凭据已经落盘，实际却没有——那正是本模块要避免的事。
//!
//! ## 测试注入点（只为测试而存在）
//!
//! [`platform_store`] 会先看进程级的那一格覆盖槽：
//!
//! * 覆盖槽**为空**（生产里永远是空的）→ 按平台分发，即上面的语义；
//! * 覆盖槽**被设过** → 直接返回注入的实现。
//!
//! 为什么需要它：钥匙串是**机器级**的设施。集成测试若真往 Windows 凭据管理器里
//! 写条目，跑在开发机 / CI 上的用例就会互相抢同一个条目名（测试是并行跑的），
//! 还会在用户机器上留下 `xremail:account:<随机 uuid>` 这种垃圾条目；
//! 而 Linux 上根本没有实现，那几条 DKIM 用例就只能靠「跳过」来装绿——
//! 可它们验的是「签的字节 == 发的字节」「私钥不出门」这种与平台无关的承诺，
//! 跳过等于让承诺在所有平台上都失去看护。
//!
//! 用法与**禁用区**写在 [`set_platform_store_for_test`] 的文档里，务必先读。
//! 真实凭据管理器的往返测试（`tests/windows_credential_manager.rs`）**不走**这个口子，
//! 它要验的恰恰是系统设施本身，注入进去就成了自欺欺人。

use std::sync::{Arc, RwLock};

use xremail_api::SecretStore;

/// Android Keystore 适配（R08）。见该文件的模块文档——**那里写明了
/// 这一层还没有真机验证**。
#[cfg(target_os = "android")]
pub mod android_store;
#[cfg(target_os = "linux")]
pub mod linux_store;
pub mod test_hooks;
pub mod unsupported;
#[cfg(windows)]
pub mod windows_store;

/// 测试注入的覆盖槽。
///
/// 生产构建里它**永远是 `None`**：没有任何非测试代码写它。
/// 之所以不写成 `#[cfg(test)]`，是因为集成测试（`tests/*.rs`）是**另一个 crate**，
/// 看不到被测 crate 里 `#[cfg(test)]` 的项；用 `#[doc(hidden)]` 把它从文档里藏起来，
/// 比让它在 Linux 上「不存在」要诚实。
///
/// 用 `RwLock` 而不是 `Mutex`：[`platform_store`] 每次调用都读一次锁，
/// 读多写极少，读锁不阻塞读锁。
static PLATFORM_STORE_OVERRIDE: RwLock<Option<Arc<dyn SecretStore>>> = RwLock::new(None);

/// 平台凭据存储。
///
/// 返回平台对应的实现：Windows 是凭据管理器，Linux 是 Secret Service，
/// 其余平台是 [`unsupported::UnsupportedSecretStore`]（存取都报"不可用"）。
///
/// 若测试通过 [`set_platform_store_for_test`] 设了覆盖，则返回那个实现——
/// 这是**唯一**能让返回值偏离平台默认的地方。
pub fn platform_store() -> Arc<dyn SecretStore> {
    // 注入优先；没注入时（生产里永远如此）落到平台分发。
    if let Some(store) = injected_store() {
        return store;
    }
    // 生产里返回的就是下面这一句：按平台分发。
    platform_default_store()
}

/// 平台**默认**实现（不看覆盖槽）。
///
/// 有的测试要断言「非 Windows 默认实现不静默降级」，那就不能经过覆盖槽——
/// 否则一个漏重置的注入会让用例验的是别人的实现。
///
/// 之所以单独成一个函数而不写进 [`platform_store`]：`cfg` 分支各返回一个
/// 具体实现，和「注入优先」那段写在一起会搅成一团，也容易在只看一半时读错。
#[cfg(windows)]
pub fn platform_default_store() -> Arc<dyn SecretStore> {
    Arc::new(windows_store::WindowsSecretStore)
}

/// 平台**默认**实现（不看覆盖槽）。Linux 上是 Secret Service。
#[cfg(target_os = "linux")]
pub fn platform_default_store() -> Arc<dyn SecretStore> {
    Arc::new(linux_store::LinuxSecretStore)
}

/// 平台**默认**实现（不看覆盖槽）。Android 上是 Keystore。
///
/// 与 Linux 那份是同一套 `keyring` 代码，只是构建器由 `android-keyring`
/// 接管（JNI → Android Keystore）。见 [`android_store`] 的模块文档。
#[cfg(target_os = "android")]
pub fn platform_default_store() -> Arc<dyn SecretStore> {
    Arc::new(android_store::AndroidSecretStore)
}

/// 平台**默认**实现（不看覆盖槽）。其余非 Windows 平台上是 [`unsupported`]。
///
/// 这里刻意用「既不是 Windows、也不是 Linux」而不是笼统的 `not(windows)`：
/// 两个分支同时存在时，`not(windows)` 会和 Linux 那条**同时成立**，
/// 编译器报重定义。
#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
pub fn platform_default_store() -> Arc<dyn SecretStore> {
    Arc::new(unsupported::UnsupportedSecretStore)
}

/// 读覆盖槽。
///
/// 锁中毒时仍然取出里面的值：注入本身不会在持锁期间 panic，
/// 为了一次测试失败把整个进程的钥匙串调用变成 panic 不值当。
fn injected_store() -> Option<Arc<dyn SecretStore>> {
    let slot = PLATFORM_STORE_OVERRIDE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.clone()
}

/// 让 [`platform_store`] 返回**另一个**实现。
///
/// ## 只有测试可以调用
///
/// 这不是"可配置的钥匙串后端"，而是测试口子。产品代码一旦调用它，
/// 就等于把「凭据到底存进了哪个设施」变成运行期可改的——那正是本模块
/// 最不该有的性质。生产代码里没有任何调用点，`#[doc(hidden)]` 只是让它
/// 不出现在公开文档里。
///
/// ## 用完必须还原
///
/// 覆盖是**进程级全局状态**：同一进程里并行跑的其它用例会一起看见它。
/// 所以：
///
/// 1. 同一条链路上的用例（凡是会碰钥匙串的）用**同一把**串行闸串起来——
///    见 [`test_hooks::serial_guard`]；
/// 2. 用 [`test_hooks::install`] 安装：它返回一个守卫，**drop 时自动还原**
///    成平台默认实现。断言中途 panic 也会还原（`Drop` 在 unwind 里照样跑），
///    不会把「注入一直留着」的锅甩给下一条用例。
///
/// 直接调本函数而不还原的话，Windows 上后面跑的**真实**凭据管理器用例
/// 也会走内存实现——它们会全绿，但什么也没证明。
#[doc(hidden)]
pub fn set_platform_store_for_test(store: Arc<dyn SecretStore>) {
    let mut slot = PLATFORM_STORE_OVERRIDE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(store);
}

/// 清掉覆盖，回到平台默认实现。
///
/// 一般不用直接调：用 [`test_hooks::install`] 的守卫会自动调它。
#[doc(hidden)]
pub fn clear_platform_store_for_test() {
    let mut slot = PLATFORM_STORE_OVERRIDE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = None;
}

/// 账户凭据在平台钥匙串里的条目名。
///
/// 格式固定为 `xremail:account:<account_id>`：
///
/// * 前缀 `xremail:` 让本应用的条目在系统钥匙串里**可辨认、可批量清理**；
/// * 用账户 id（不是邮箱地址）做后缀，邮箱会变，id 不会。
///
/// 这是一个**不透明引用**：应用只拿它去 [`platform_store`] 取值，
/// 不解析它。格式若要改，这里是唯一的地方。
pub fn secret_ref_for(account_id: &str) -> String {
    format!("xremail:account:{account_id}")
}
