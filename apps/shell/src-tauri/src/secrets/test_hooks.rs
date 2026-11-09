//! 测试注入点的配套工具：串行闸与自动还原的安装器。
//!
//! ## 为什么需要一把闸
//!
//! 注入是**进程级**的（见 [`super::set_platform_store_for_test`]），而
//! `cargo test` 默认多线程跑同一个测试二进制里的用例。两条用例同时注入
//! 各自的实现，后装的会把先装的顶掉——先装的那条会突然发现自己写进去的钥
//! 读不出来了，报出与它要验的东西毫不相干的失败。
//!
//! 仓库不引第三方依赖，所以不用 `serial_test`，用一把**静态**
//! [`Mutex`] 当串行闸：凡是会碰钥匙串的用例，进门前先拿闸。
//!
//! ## 闸的粒度必须覆盖**整个二进制**
//!
//! 闸是**每个测试二进制各一份**（`tests/*.rs` 各编成一个独立的进程）。
//! 所以同一个文件里所有会碰钥匙串的用例都要用**同一把**闸——
//! 只给「直接注入的那几条」上闸，而漏掉「间接写钥匙串的那几条」（比如先把 DKIM 配置
//! 存进钥匙串再发信），它们就会在别人注入期间跑，写进别人的内存实现里。
//! `command_ipc.rs` 里那几条 DKIM 用例就是这样：它们自己不直接调注入点，
//! 却要经过 `DkimService` → 钥匙串。
//!
//! ## 为什么安装器要返回守卫
//!
//! 断言失败会 panic。若「注入」与「还原」写成两行，panic 时第二行不会执行，
//! 覆盖就一直留着——在 Windows 上，后面跑的**真实**凭据管理器用例会静默地
//! 走内存实现：全绿，但什么也没证明。守卫的 `Drop` 在 unwind 里照样执行，
//! 把「忘了还原」这个坑从人身上挪到了类型上。
//!
//! ## 闸会被一直持有到 `await` 之后——这是**故意的**
//!
//! 异步用例里把 `MutexGuard` 带过 `await` 通常是个坑（单线程运行时会自锁），
//! 所以 clippy 默认报 `await_holding_lock`。这里必须这么做：单元就是整条用例，
//! 而用例的绝大多数步骤都是 `await`（写库、发信、读钥匙串）——
//! 只在同步片段里拿闸，等于没串行化。安全性来自运行时的选择：
//!
//! * 集成测试用的是 `#[tokio::test]` 默认的**多线程**运行时，
//!   阻塞的 `std::sync::Mutex` 在别的 worker 上拿锁不会把持锁的那个线程卡死；
//! * 用 `block_on` 驱动（`windows_credential_manager.rs` 那种）的用例是单线程运行时的，
//!   而那几条**不碰**这把闸——覆盖槽在生产路径上永远是空的，它们走的是真实设施。
//!
//! 别把本文件的用例改成 `#[tokio::test(flavor = "current_thread")]`，
//! 也别把一把闸跨两个不同的 `block_on` 持有：前者会全进程自锁，后者是死等。

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use xremail_api::SecretStore;

use super::{clear_platform_store_for_test, platform_store, set_platform_store_for_test};

/// 全局串行闸。
///
/// 只是**串行化**，没有别的意思：拿它的顺序无所谓，也不需要重入。
pub static SERIAL: Mutex<()> = Mutex::new(());

/// 取得串行闸。
///
/// 锁中毒时**照常放行**：中毒说明上一条用例在持闸期间 panic 了（多半是断言失败），
/// 那正是测试要报告的东西；让后面每条用例都再 panic 一次
/// （`PoisonError`）只会把真正的原因淹没在一堆"锁中毒"里。
pub fn serial_guard() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一次已安装的注入；**drop 时还原**成平台默认实现。
#[derive(Debug)]
pub struct Injection {
    /// 是哪一个实现被装进去了（便于断言里核对）。
    label: &'static str,
}

impl Injection {
    /// 这次注入的名字（安装时传入）。
    pub fn label(&self) -> &'static str {
        self.label
    }
}

impl Drop for Injection {
    fn drop(&mut self) {
        clear_platform_store_for_test();
    }
}

/// 把 `store` 设为进程级的平台钥匙串实现，返回的守卫 drop 时自动还原。
///
/// 调用方必须**已经**持有 [`serial_guard`]——本函数不替你拿闸：
/// 闸要覆盖「注入 → 断言 → 还原」整段，包括中途创建的异步任务；
/// 在安装器里拿闸会把守卫的生命周期和它绑在一起，读代码时看不出谁在保护谁。
pub fn install(store: Arc<dyn SecretStore>, label: &'static str) -> Injection {
    set_platform_store_for_test(store);
    Injection { label }
}

/// 断言「现在拿到的是注入进去的那个实现」。
///
/// 用指针相等判断：`platform_store()` 原样返回 `Arc`，不会再包一层。
/// 少了这一步，注入没生效时用例会在很远的地方失败（比如"配置写不进去"），
/// 排查的人得先猜一圈才知道是闸没拿到。
pub fn assert_installed(store: &Arc<dyn SecretStore>) {
    let current = platform_store();
    assert!(
        Arc::ptr_eq(&current, store),
        "注入没有生效：platform_store() 返回的不是装进去的那个实现（是不是没拿串行闸？）"
    );
}
