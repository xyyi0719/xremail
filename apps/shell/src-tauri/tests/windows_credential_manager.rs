#![cfg(windows)]
//! Windows 凭据管理器的**真实**往返测试。
//!
//! 这里**不 mock**：真的往这台机器的凭据管理器里写条目，再读回来、删掉。
//! 凭据管理器是操作系统设施，mock 出来的"成功"证明不了任何事——
//! 真正会咬人的地方（blob 是原始字节、条目名要对齐、覆盖写、
//! `ERROR_NOT_FOUND` 的语义）只有真跑一遍才会暴露。
//!
//! ## 不给用户机器留垃圾
//!
//! * 每条用例用**唯一**的条目名（`xremail:test:<标签>:<随机后缀>`），
//!   不会撞上真实账户条目（`xremail:account:<uuid>`），也不会和并行用例互相干扰。
//! * [`TestEntry`] 在 `Drop` 时删掉条目：断言中途失败也要清理干净。
//!
//! ## 为什么这些用例要**串行**跑（R49）
//!
//! 每条用例的条目名本来就不冲突，串行不是为了防撞名，而是因为**凭据管理器
//! 是系统级单点资源**：cargo 默认让同一测试二进制里的用例并行跑，
//! 于是 6 条用例会同时 `CredWriteW`。2026-11-09 的 CI 上撞到过一次
//! 「`set` 报成功、紧接着 `get` 报 `NoEntry`」（此前连续 6 次全绿、
//! 重跑即绿），而同一进程里另外 5 条并行用例都通过了——**看起来是并发写入下的偶发**。
//!
//! 这里**没有**以「加个重试」的方式绕过去：那会把一个没说清的现象变成
//! 一劳永逸的掩盖。改成串行是承认「这些用例验的是凭据管理器的**语义**，
//! 不是它的并发行为」——真要验并发，得另写一组用例并说明期望是什么。
//!
//! 代价：这一组用例从并行变成串行，多花约 0.2 秒（实测 6 条共 0.22 秒）。

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Security::Credentials::CRED_MAX_CREDENTIAL_BLOB_SIZE;
use xremail_api::SecretStoreError;
use xremail_api::secret::{SecretStore, SecretValue};
use xremail_shell_lib::secrets::windows_store::WindowsSecretStore;

/// 驱动一次异步调用。
///
/// 用**每次新建**的运行时：`Drop` 里的清理也要驱动一次异步调用，
/// 若共用一个运行时，测试体里 panic 之后那个运行时就不可用了。
/// 本文件里所有用例共用的串行闸（理由见模块文档的 R49 那段）。
///
/// 用 `Mutex<()>` 而不是把用例改成 `--test-threads=1`：那只影响整个测试
/// 二进制，而这里只想让**碰真实凭据管理器**的这几条互斥。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取得串行闸。锁中毒时照常放行（上一条用例 panic 的原因才是要看的）。
fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建测试运行时失败")
        .block_on(fut)
}

/// 生成一个**唯一**的条目名。
///
/// 后缀是纳秒时间戳 + 进程号 + 自增序号：既不会撞上用户机器上真实存在的
/// 账户条目，也不会和并行跑的另一个用例撞车。
fn unique_ref(tag: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("xremail:test:{tag}:{nanos:x}-{}-{seq}", std::process::id())
}

/// 一条测试用的凭据条目。
///
/// `Drop` 时删掉它——测试失败时也要清理，不能在用户机器上留垃圾。
struct TestEntry {
    store: WindowsSecretStore,
    secret_ref: String,
}

impl TestEntry {
    /// 造一条**唯一**的测试条目（此时还没写进凭据管理器）。
    fn new(tag: &str) -> Self {
        Self {
            store: WindowsSecretStore::new(),
            secret_ref: unique_ref(tag),
        }
    }

    /// 条目名。
    fn secret_ref(&self) -> &str {
        &self.secret_ref
    }
}

impl Drop for TestEntry {
    fn drop(&mut self) {
        let store = self.store;
        let secret_ref = self.secret_ref.clone();
        // 清理失败只警告，不 panic：Drop 里 panic 会把"断言失败"变成"进程 abort"。
        if let Err(err) = block_on(store.delete(&secret_ref)) {
            eprintln!("警告：清理测试条目 {secret_ref} 失败：{err}");
        }
    }
}

/// 写入再读回来，内容必须**逐字节**相同。
#[test]
fn set_then_get_returns_the_same_bytes() {
    let _guard = serial_guard();
    let entry = TestEntry::new("roundtrip");
    let store = entry.store;
    // 内容故意带非 ASCII：通用凭据的 blob 是原始字节，
    // 这里顺带证明中文/多字节内容不会被"按宽字符"搞坏。
    let value = SecretValue::from_str_lossless("授权码-测试-秘密");

    block_on(store.set(entry.secret_ref(), &value)).expect("写入凭据应当成功");

    let got = block_on(store.get(entry.secret_ref())).expect("读取凭据不该报错");
    let got = got.expect("刚写进去的条目应当读得到");
    assert_eq!(
        got.as_bytes(),
        value.as_bytes(),
        "读回来的内容必须与写进去的逐字节相同"
    );
}

/// 同名条目再写一次：**后写的**内容生效（覆盖）。
#[test]
fn set_twice_overwrites_the_previous_value() {
    let _guard = serial_guard();
    let entry = TestEntry::new("overwrite");
    let store = entry.store;
    let first = SecretValue::from_str_lossless("第一次写的值");
    let second = SecretValue::from_str_lossless("第二次写的值");

    block_on(store.set(entry.secret_ref(), &first)).expect("第一次写入应当成功");
    block_on(store.set(entry.secret_ref(), &second)).expect("第二次写入应当成功");

    let got = block_on(store.get(entry.secret_ref()))
        .expect("读取不该报错")
        .expect("条目应当存在");
    assert_eq!(
        got.as_bytes(),
        second.as_bytes(),
        "同名条目应当被覆盖成后写的内容"
    );
    assert_ne!(got.as_bytes(), first.as_bytes(), "旧内容不该还在");
}

/// 删除：第一次 `true`（真删掉了），第二次 `false`（本来就没有），之后读不到。
#[test]
fn delete_reports_whether_something_was_removed() {
    let _guard = serial_guard();
    let entry = TestEntry::new("delete");
    let store = entry.store;
    let value = SecretValue::from_str_lossless("待删除的值");
    block_on(store.set(entry.secret_ref(), &value)).expect("写入应当成功");

    assert!(
        block_on(store.delete(entry.secret_ref())).expect("删除不该报错"),
        "第一次删除应当真的删掉了"
    );
    assert!(
        !block_on(store.delete(entry.secret_ref())).expect("再删一次不该报错"),
        "再删一次应当报告没有东西可删，而不是报错"
    );
    assert!(
        block_on(store.get(entry.secret_ref()))
            .expect("读取不该报错")
            .is_none(),
        "删掉之后应当读不到"
    );
}

/// 从没写过的条目：**读不到**，但**不是错误**。
#[test]
fn get_on_a_never_written_ref_returns_none() {
    let _guard = serial_guard();
    let store = WindowsSecretStore::new();
    let secret_ref = unique_ref("never-written");

    assert!(
        block_on(store.get(&secret_ref))
            .expect("读取不该报错")
            .is_none(),
        "从没写过的条目应当读不到，而不是报错"
    );
}

/// 超过 Windows 凭据管理器上限的内容：**明确报错**，不截断。
///
/// 截断等于存下一个读回来就是错值的凭据——那比存不进去更糟。
#[test]
fn set_rejects_content_larger_than_the_platform_limit() {
    let _guard = serial_guard();
    let entry = TestEntry::new("too-large");
    let store = entry.store;
    let big = SecretValue::new(vec![b'x'; CRED_MAX_CREDENTIAL_BLOB_SIZE as usize + 1]);

    let err = block_on(store.set(entry.secret_ref(), &big)).expect_err("超限内容应当被拒绝");
    assert!(
        matches!(err, SecretStoreError::Invalid(_)),
        "应当报 Invalid（条目不合法），实际是 {err:?}"
    );
}

/// 空的凭据引用：明确报错，而不是让系统去接受一个无名条目。
#[test]
fn set_rejects_an_empty_secret_ref() {
    let _guard = serial_guard();
    let store = WindowsSecretStore::new();
    let value = SecretValue::from_str_lossless("值");

    let err = block_on(store.set("", &value)).expect_err("空引用应当被拒绝");
    assert!(
        matches!(err, SecretStoreError::Invalid(_)),
        "应当报 Invalid，实际是 {err:?}"
    );
}
