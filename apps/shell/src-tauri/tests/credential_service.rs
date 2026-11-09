//! 凭据服务（[`CredentialService`]）的契约测试。
//!
//! 补的是 R07 的缺口：`resolve_credential` / `save_credential` / `forget_credential`
//! 这三条此前只有「账户不存在」两条前置分支有覆盖（在 `tests/command_ipc.rs` 里），
//! 真正要守的约定一条都没测：
//!
//! | 承诺 | 为什么值得单测 |
//! |---|---|
//! | 库里只有**引用**、没有授权码明文 | 数据库会被备份、同步、拷走 |
//! | 没有 `secret_ref` 就**不打扰**钥匙串 | 钥匙串在 Windows 上可能弹窗 / 加锁，白白去读一次会拖慢同步 |
//! | 「没有」与「取不到」分开 | 前者 `Ok(None)`，后者 `Err`——混在一起，用户就分不清「我没存」和「存了但读不出来」 |
//! | 钥匙串写失败时 `secret_ref` **不被写脏** | 否则下次同步会拿着一条指向空条目的引用去连服务器 |
//! | `forget_credential` 幂等、返回值是「真的删过」 | 前端拿它提示「已清除」还是「本来就没有」 |
//! | 平台不支持时**不静默降级** | 见 `secrets/unsupported.rs`：宁可报错，也不假装存下了 |
//!
//! ## 钥匙串从哪来
//!
//! 处理器里**不碰**真实钥匙串（平台、机器、并行都是变数）：每条用例先经
//! [`serial_guard`] 拿闸，再 [`install`] 一份内存实现或故障替身。
//! 注入是进程级的，必须串行——原因写在 `secrets/test_hooks.rs` 里。
//!
//! 唯一一条**故意**不注入的用例是
//! [`the_platform_default_store_does_not_silently_degrade_on_unsupported_platforms`]：
//! 它验的正是平台默认实现本身，经过覆盖槽就验成别人的实现了。

//! ## 关于串行闸与 `await`
//!
//! 会碰钥匙串的用例用一把静态 `Mutex` 串行化（原因见
//! `secrets/test_hooks.rs`），并且**故意**把守卫带过 `await`：整条用例就是一个单元，
//! 而它几乎每一步都是 `await`，只在同步片段里拿闸等于没串行化。
//! 多线程 `#[tokio::test]` 运行时下阻塞式 `Mutex` 不会自锁，这一点在这里是有意为之，
//! 不是漏网之鱼。
#![allow(clippy::await_holding_lock)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlx::SqlitePool;
use xremail_api::mock::InMemorySecretStore;
use xremail_api::{NewAccountRequest, SecretStore, SecretStoreError, SecretValue};
use xremail_core::AppError;
use xremail_core::model::{Account, ProviderKind, SmtpAuth, SyncMode, TlsMode};
use xremail_shell_lib::secrets::test_hooks::{Injection, install, serial_guard};
// `self`（即 `secrets::`）只在下面那条 `#[cfg(not(windows))]` 的用例里用到：
// 它验的是「非 Windows 的默认实现不静默降级」。Windows 上没有这个分支，
// 不带 cfg 的话那边就是一条 unused import，而 CI 的 Windows job 带 -D warnings。
#[cfg(not(windows))]
use xremail_shell_lib::secrets;
use xremail_shell_lib::secrets::secret_ref_for;
use xremail_shell_lib::service::{AccountService, CredentialService};

/// 测试里用的授权码。**不是**真口令，但当成真口令那样对待：它不该出现在库里。
const AUTH_CODE: &str = "AUTOTEST-AUTH-CODE-9f3a";

// ---------------------------------------------------------------- 套件

/// 一套「内存库 + 账户服务 + 凭据服务 + 一个已存在的账户」。
struct Fixture {
    pool: SqlitePool,
    credentials: CredentialService,
    account_id: String,
}

impl Fixture {
    /// 造套件。账户还没存过授权码（`secret_ref` 为 `None`）。
    async fn start() -> Self {
        let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
            .await
            .expect("打开内存库失败");
        xremail_store::migrate(&pool).await.expect("应用迁移失败");

        let accounts = AccountService::new(pool.clone());
        let account_id = accounts
            .create(NewAccountRequest {
                email: "user@163.com".to_string(),
                provider: ProviderKind::Netease163,
                display_name: None,
                imap_host: "imap.163.com".to_string(),
                imap_port: 993,
                imap_tls: TlsMode::Implicit,
                smtp_host: "smtp.163.com".to_string(),
                smtp_port: 465,
                smtp_tls: TlsMode::Implicit,
                smtp_auth: SmtpAuth::Login,
                username: "user@163.com".to_string(),
                sync_mode: SyncMode::Poll,
                poll_interval_secs: 300,
            })
            .await
            .expect("建账户失败")
            .id;

        Self {
            pool,
            credentials: CredentialService::new(accounts),
            account_id,
        }
    }

    /// 库里的账户记录（**新查一次**，不是内存里的旧对象）。
    async fn account(&self) -> Account {
        AccountService::new(self.pool.clone())
            .get(&self.account_id)
            .await
            .expect("查账户失败")
            .expect("账户应当还在")
    }

    /// 把账户的 `secret_ref` 直接改成 `value`（造「引用指向别处 / 指向空」的前置状态）。
    async fn set_secret_ref(&self, value: Option<&str>) {
        let mut account = self.account().await;
        account.secret_ref = value.map(str::to_string);
        AccountService::new(self.pool.clone())
            .update(account)
            .await
            .expect("改账户失败");
    }
}

/// 测试用的授权码（每次现造一份，不共享可变的全局状态）。
fn authorization_code() -> SecretValue {
    SecretValue::from_str_lossless(AUTH_CODE)
}

// ---------------------------------------------------------------- 故障替身

/// 记录调用次数、可以在任何一步失败的内存钥匙串。
///
/// 为什么自己写而不复用 [`InMemorySecretStore`]：这几条用例要验的是**服务对失败的
/// 反应**（引用不被写脏、错误文案、不打扰钥匙串），需要精确控制失败注入点、
/// 并数清楚「有没有被调用」。`InMemorySecretStore` 永远成功，看不见这些。
#[derive(Debug, Default)]
struct FlakyStore {
    inner: InMemorySecretStore,
    /// `get` 要不要报错（模拟钥匙串被锁 / 权限不足）。
    fail_get: bool,
    /// `set` 要不要报错（同时一个字节都不写进去——真实钥匙串失败时就是这样）。
    fail_set: bool,
    /// `delete` 要不要报错。
    fail_delete: bool,
    /// 各种调用的次数。验「不打扰钥匙串」要靠它，光看返回值看不出来。
    calls: Mutex<Calls>,
}

/// 调用计数。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Calls {
    get: usize,
    set: usize,
    delete: usize,
}

impl FlakyStore {
    /// 全部成功的内存实现。
    fn new() -> Self {
        Self::default()
    }

    /// 读会失败。
    fn with_failing_get() -> Self {
        Self {
            fail_get: true,
            ..Self::default()
        }
    }

    /// 写会失败。
    fn with_failing_set() -> Self {
        Self {
            fail_set: true,
            ..Self::default()
        }
    }

    /// 删会失败。
    fn with_failing_delete() -> Self {
        Self {
            fail_delete: true,
            ..Self::default()
        }
    }

    /// 当前的调用计数。
    fn calls(&self) -> Calls {
        *self
            .calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 钥匙串故障文案里的一句特征串。
///
/// 断言时会找它，以证明服务把**底层说明**透了出来，而不是吞成一句干巴巴的「失败」。
const STORE_TROUBLE: &str = "钥匙串被系统锁住了";

#[async_trait]
impl SecretStore for FlakyStore {
    async fn get(&self, secret_ref: &str) -> Result<Option<SecretValue>, SecretStoreError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get += 1;
        if self.fail_get {
            return Err(SecretStoreError::Unavailable(STORE_TROUBLE.to_string()));
        }
        self.inner.get(secret_ref).await
    }

    async fn set(&self, secret_ref: &str, value: &SecretValue) -> Result<(), SecretStoreError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set += 1;
        if self.fail_set {
            // 失败就是失败：不落半条。真实钥匙串也是这个语义。
            return Err(SecretStoreError::Unavailable(STORE_TROUBLE.to_string()));
        }
        self.inner.set(secret_ref, value).await
    }

    async fn delete(&self, secret_ref: &str) -> Result<bool, SecretStoreError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .delete += 1;
        if self.fail_delete {
            return Err(SecretStoreError::Unavailable(STORE_TROUBLE.to_string()));
        }
        self.inner.delete(secret_ref).await
    }
}

/// 装一份钥匙串替身：拿闸、注入、返回（替身本身, 自动还原的守卫）。
///
/// 守卫必须被调用方**绑住**（`let (store, _injection) = …`），
/// 绑成 `_` 会立刻 drop——注入就提前还原了。
fn install_store(store: Arc<dyn SecretStore>) -> Injection {
    install(store, "credential_service 的钥匙串替身")
}

/// 全成功的内存钥匙串。
fn memory() -> Arc<FlakyStore> {
    Arc::new(FlakyStore::new())
}

// ---------------------------------------------------------------- 保存

#[tokio::test]
async fn save_puts_the_code_in_the_keychain_and_only_a_reference_in_the_database() {
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;

    fixture
        .credentials
        .save_credential(&fixture.account_id, authorization_code())
        .await
        .expect("保存授权码应当成功");

    // 钥匙串里：条目名就是规范引用，内容逐字节等于授权码。
    let secret_ref = secret_ref_for(&fixture.account_id);
    let stored = store
        .inner
        .get(&secret_ref)
        .await
        .expect("读钥匙串失败")
        .expect("保存之后钥匙串里应当有这条授权码");
    assert_eq!(stored.as_bytes(), AUTH_CODE.as_bytes());

    // 库里：只有那条不透明引用，而且**新查一次**确认真的落库了
    // （不是内存里那个对象自己以为改了）。
    let account = fixture.account().await;
    assert_eq!(
        account.secret_ref.as_deref(),
        Some(secret_ref.as_str()),
        "库里应当记下规范引用"
    );
    assert_eq!(
        secret_ref,
        format!("xremail:account:{}", fixture.account_id)
    );

    // 授权码明文**不在库里**：把整库每一张表、每一列都翻一遍。
    //
    // 为什么不只查 `account.secret_ref`：将来谁要是「顺手」把授权码塞进别的表
    // （比如先落一份"草稿"再转正），只盯一列是看不出来的。
    assert_database_has_no_plaintext(&fixture.pool).await;
}

/// 逐表逐列扫一遍库，确认找不到授权码明文。
///
/// 用 `CAST(col AS TEXT)` 统一类型：BLOB 里的授权码同样要被抓到——
/// 二进制列里躺着口令，和文本列里躺着一样糟。
async fn assert_database_has_no_plaintext(pool: &SqlitePool) {
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await
    .expect("列出表失败");
    assert!(!tables.is_empty(), "一张表都没查到，这条断言就没意义了");

    let mut scanned_columns = 0usize;
    for (table,) in &tables {
        // sqlx 0.9 要求动态 SQL 显式「声明已审」：表名来自 sqlite_master，
        // 不是用户输入，拼进 SQL 没有注入面。
        let columns: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT name FROM pragma_table_info('{table}')"
        )))
        .fetch_all(pool)
        .await
        .unwrap_or_else(|error| panic!("取表 {table} 的列失败：{error}"));

        for (column,) in &columns {
            // 表名 / 列名来自 sqlite_master，不是用户输入，拼进 SQL 是安全的。
            let hits: Vec<(i64,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM \"{table}\" \
                 WHERE CAST(\"{column}\" AS TEXT) LIKE '%' || ? || '%'"
            )))
            .bind(AUTH_CODE)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|error| panic!("扫 {table}.{column} 失败：{error}"));
            scanned_columns += 1;
            assert_eq!(
                hits[0].0, 0,
                "表 {table}.{column} 里出现了授权码明文——凭据只能待在钥匙串里"
            );
        }
    }
    assert!(
        scanned_columns > 10,
        "只扫了 {scanned_columns} 列，扫描逻辑大概率坏了"
    );
}

// ---------------------------------------------------------------- 读取

#[tokio::test]
async fn resolve_without_a_secret_ref_returns_none_without_touching_the_keychain() {
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;

    let account = fixture.account().await;
    assert!(account.secret_ref.is_none(), "前置：这个账户还没存过授权码");

    let got = fixture
        .credentials
        .resolve_credential(&account)
        .await
        .expect("没有引用不是错误");

    assert!(got.is_none());
    // 「没有引用就不打扰钥匙串」只能靠计数证明：返回值是 `None` 既不说明读了、
    // 也不说明没读。去读一次在 Windows 上可能触发弹窗或加锁，白白拖慢每一次同步。
    assert_eq!(
        store.calls(),
        Calls::default(),
        "账户没有 secret_ref 时不该碰钥匙串"
    );
}

#[tokio::test]
async fn resolve_with_a_ref_the_keychain_does_not_have_returns_none() {
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;
    // 引用在库里，条目却不在钥匙串里（用户重装过系统 / 手工删了条目 /
    // 从别的机器恢复了数据库）。
    fixture
        .set_secret_ref(Some(&secret_ref_for(&fixture.account_id)))
        .await;

    let got = fixture
        .credentials
        .resolve_credential(&fixture.account().await)
        .await
        .expect("「没存过」不是错误");

    assert!(got.is_none(), "钥匙串里没有就是没有");
    assert_eq!(store.calls().get, 1, "有引用就该去读一次");
}

#[tokio::test]
async fn resolve_returns_the_stored_code() {
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;

    fixture
        .credentials
        .save_credential(&fixture.account_id, authorization_code())
        .await
        .expect("保存应当成功");

    let got = fixture
        .credentials
        .resolve_credential(&fixture.account().await)
        .await
        .expect("读取应当成功")
        .expect("刚存进去的授权码应当读得到");

    assert_eq!(got.as_bytes(), AUTH_CODE.as_bytes());
}

#[tokio::test]
async fn resolve_reports_a_broken_keychain_as_an_error_not_as_missing() {
    let _guard = serial_guard();
    let store = Arc::new(FlakyStore::with_failing_get());
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;
    fixture
        .set_secret_ref(Some(&secret_ref_for(&fixture.account_id)))
        .await;

    let error = fixture
        .credentials
        .resolve_credential(&fixture.account().await)
        .await
        .expect_err("钥匙串故障不能吞成「没有凭据」");

    // 「取不到」与「没有」必须分得开：前者要提示用户重新授权 / 解锁钥匙串，
    // 后者只要提示「先存一次」。
    assert!(
        matches!(error, AppError::Storage(_)),
        "钥匙串故障应当归到 Storage，实际是 {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("读取授权码失败"),
        "错误文案应当说清楚是哪一步失败：{message}"
    );
    assert!(
        message.contains(STORE_TROUBLE),
        "底层说明不能丢——用户要靠它判断是锁了还是没权限：{message}"
    );
    assert!(
        !message.contains(AUTH_CODE),
        "错误信息里不能出现授权码：{message}"
    );
}

// ---------------------------------------------------------------- 保存失败

#[tokio::test]
async fn a_failed_save_leaves_the_secret_ref_untouched() {
    let _guard = serial_guard();
    let store = Arc::new(FlakyStore::with_failing_set());
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;
    // 前置：账户原本引用的那条条目「在钥匙串里是好的」——这里用另一个能读的账户状态
    // 模拟「之前存过、这次想换授权码」。换不成就必须保持原样。
    let previous_ref = "xremail:account:previous-installation";
    fixture.set_secret_ref(Some(previous_ref)).await;

    let error = fixture
        .credentials
        .save_credential(&fixture.account_id, authorization_code())
        .await
        .expect_err("钥匙串写不进去就必须报错，不能假装存下了");

    assert!(
        matches!(error, AppError::Storage(_)),
        "保存失败应当归到 Storage，实际是 {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("没有存进系统凭据保管设施"),
        "文案要明说「没存进钥匙串」，用户才知道要换个时间重试：{message}"
    );
    assert!(message.contains(STORE_TROUBLE), "底层说明不能丢：{message}");
    assert!(
        !message.contains(AUTH_CODE),
        "错误信息里不能出现授权码：{message}"
    );

    // 关键：库里那条引用**没被写脏**。
    let account = fixture.account().await;
    assert_eq!(
        account.secret_ref.as_deref(),
        Some(previous_ref),
        "钥匙串写失败时不能把 secret_ref 改成一条指向空条目的引用"
    );
    assert_eq!(store.calls().set, 1, "应当尝试写过一次");
    assert_database_has_no_plaintext(&fixture.pool).await;
}

#[tokio::test]
async fn a_failed_save_on_a_fresh_account_keeps_the_ref_empty() {
    // 和上一条互补：从来没有引用时，失败之后也不能**凭空**写进一条引用。
    let _guard = serial_guard();
    let store = Arc::new(FlakyStore::with_failing_set());
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;

    fixture
        .credentials
        .save_credential(&fixture.account_id, authorization_code())
        .await
        .expect_err("写不进去应当报错");

    assert!(
        fixture.account().await.secret_ref.is_none(),
        "保存失败之后账户不该出现引用"
    );
}

#[tokio::test]
async fn saving_for_an_unknown_account_reports_not_found_and_never_touches_the_keychain() {
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;

    let error = fixture
        .credentials
        .save_credential("no-such-account", authorization_code())
        .await
        .expect_err("账户不存在应当报错");

    assert!(
        matches!(error, AppError::NotFound(_)),
        "应当报 NotFound，实际是 {error:?}"
    );
    // 先查账户、再碰钥匙串：否则会给一个不存在的账户留下一条谁也用不了的条目。
    assert_eq!(store.calls(), Calls::default(), "账户不存在时不该碰钥匙串");
}

// ---------------------------------------------------------------- 忘记

#[tokio::test]
async fn forget_removes_the_entry_clears_the_ref_and_is_idempotent() {
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;
    fixture
        .credentials
        .save_credential(&fixture.account_id, authorization_code())
        .await
        .expect("保存应当成功");
    let secret_ref = secret_ref_for(&fixture.account_id);

    let deleted = fixture
        .credentials
        .forget_credential(&fixture.account_id)
        .await
        .expect("忘记授权码应当成功");
    assert!(deleted, "第一次忘记应当真的删掉了钥匙串里那条");

    assert!(
        store
            .inner
            .get(&secret_ref)
            .await
            .expect("读钥匙串失败")
            .is_none(),
        "钥匙串条目应当没了"
    );
    assert!(
        fixture.account().await.secret_ref.is_none(),
        "库里的引用也要清掉：留着它，下次同步会去找一条已经不存在的条目"
    );

    // 再忘一次：幂等，返回 false（「本来就没有」也是成功的一次忘记）。
    let again = fixture
        .credentials
        .forget_credential(&fixture.account_id)
        .await
        .expect("重复忘记不该报错");
    assert!(!again, "第二次忘记应当报告没有东西可删");
    assert!(
        fixture.account().await.secret_ref.is_none(),
        "重复忘记之后引用仍应为空"
    );
    assert!(
        store
            .inner
            .get(&secret_ref)
            .await
            .expect("读钥匙串失败")
            .is_none()
    );
}

#[tokio::test]
async fn forget_also_cleans_the_canonical_entry_when_the_ref_points_elsewhere() {
    // 残骸场景：库里的引用被改成了别处（或曾经指向别处），规范条目却还在钥匙串里。
    // 只删「记着的那条」会留下孤儿——下一个用同一账户 id 的配置会捡到旧钥。
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;
    let secret_ref = secret_ref_for(&fixture.account_id);
    store
        .inner
        .set(&secret_ref, &authorization_code())
        .await
        .expect("预置规范条目失败");
    fixture
        .set_secret_ref(Some("xremail:account:elsewhere"))
        .await;

    let deleted = fixture
        .credentials
        .forget_credential(&fixture.account_id)
        .await
        .expect("忘记应当成功");

    assert!(deleted, "规范条目被删掉了，就该报告 true");
    assert!(
        store
            .inner
            .get(&secret_ref)
            .await
            .expect("读钥匙串失败")
            .is_none(),
        "规范引用那条也要清掉"
    );
}

#[tokio::test]
async fn forget_reports_false_when_there_was_nothing_in_the_keychain() {
    let _guard = serial_guard();
    let store = memory();
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;
    fixture
        .set_secret_ref(Some(&secret_ref_for(&fixture.account_id)))
        .await;

    let deleted = fixture
        .credentials
        .forget_credential(&fixture.account_id)
        .await
        .expect("现在忘掉应当成功");

    assert!(!deleted, "钥匙串里本来就没有，就不该报告删过");
    // 引用照样清：它指向的东西已经不存在了。
    assert!(fixture.account().await.secret_ref.is_none());
}

#[tokio::test]
async fn a_broken_keychain_makes_forget_fail_instead_of_silently_forgetting() {
    let _guard = serial_guard();
    let store = Arc::new(FlakyStore::with_failing_delete());
    let _injection = install_store(store.clone());
    let fixture = Fixture::start().await;
    fixture
        .credentials
        .save_credential(&fixture.account_id, authorization_code())
        .await
        .expect("保存应当成功");

    let error = fixture
        .credentials
        .forget_credential(&fixture.account_id)
        .await
        .expect_err("删不掉就必须报错");

    assert!(
        matches!(error, AppError::Storage(_)),
        "删除失败应当归到 Storage，实际是 {error:?}"
    );
    assert!(
        error.to_string().contains("删除授权码失败"),
        "错误文案应当说清楚是哪一步：{error}"
    );
    // 删除失败时**不能**先把引用清掉：条目还在钥匙串里，引用却没了，就成了没人能清理的孤儿。
    assert!(
        fixture.account().await.secret_ref.is_some(),
        "删除失败时引用必须原样保留"
    );
}

// ---------------------------------------------------------------- 不静默降级

/// 平台**默认**实现**不静默降级**。
///
/// 这一条**故意不注入**：注入进去就验成别人的实现了。
///
/// 三平台三种情况，承诺是同一条：**失败就说失败，不许假装存下了**。
///
/// * **Windows**：默认实现是真凭据管理器，前提不成立，跳过——
///   那边的真实往返由 `tests/windows_credential_manager.rs` 负责。
/// * **Linux**：默认实现是 Secret Service。**这台机器上有没有守护进程是不一定的**
///   （CI 上通常没有，桌面用户那里有），所以两种结局都要接受，但**必须二选一**：
///   要么真的存进了钥匙串（读完立刻删掉，不污染用户会话），要么明确报
///   `Unavailable`；绝不允许「set 说成功、get 却从别处冒出来」。
/// * **其它平台**（Android / macOS）：仍是桩，存取都必须报错。
#[cfg(not(windows))]
#[tokio::test]
async fn the_platform_default_store_never_silently_degrades() {
    // 走 `platform_default_store()`（不是 `platform_store()`）：后者会被别的用例
    // 的注入顶掉，而这里要验的**就是**默认实现。
    let store = secrets::platform_default_store();
    let secret_ref = secret_ref_for("contract-test");

    let outcome = store
        .set(&secret_ref, &SecretValue::from_str_lossless("不该被存下来"))
        .await;

    match outcome {
        // 有钥匙串守护进程：写成功了。那它就该能读回来——**而且只在这时**才允许读到。
        Ok(()) => {
            let read_back = store
                .get(&secret_ref)
                .await
                .expect("写成功的条目必须能读回来");
            assert_eq!(
                read_back.as_ref().and_then(|v| v.as_str()),
                Some("不该被存下来"),
                "读回来的必须就是刚写进去的那一份"
            );
            // 收拾干净：这是用户的真实钥匙串，不能留垃圾条目。
            store
                .delete(&secret_ref)
                .await
                .expect("删除不该报错")
                .then_some(())
                .expect("刚写进去的条目必须删得掉");
            // 删完之后必须真的读不到。用 `if let` 而不是
            // `.map_or_else(|| (), |v| panic!(…))` 或 `.map(|v| panic!(…))`：
            // 后两种都是「为了副作用而套 Option 组合子」，
            // clippy 会（正确地）报 `unnecessary_lazy_evaluations` / `option_map_unit_fn`。
            let after_delete = store.get(&secret_ref).await.expect("删完再读不该报错");
            if let Some(value) = after_delete {
                panic!("删掉了却还能读出来：{value:?}");
            }
        }
        // 没有守护进程（CI / 无头机器）：必须**明确**报「不可用」。
        Err(error) => {
            assert!(
                matches!(error, SecretStoreError::Unavailable(_)),
                "没有钥匙串时必须报 Unavailable，实际是 {error:?}"
            );
            // 而且取的时候也必须报不可用，不能悄悄给一个 `None` 冒充「没存过」——
            // 「取不到」与「没存过」是两件事（见 secrets/mod.rs 的失败语义）。
            match store.get(&secret_ref).await {
                Err(SecretStoreError::Unavailable(_)) => {}
                Err(other) => panic!("读取应当报 Unavailable，实际是 {other:?}"),
                Ok(Some(value)) => panic!("写都没成功，却从某处冒出来一份：{value:?}"),
                // `Ok(None)` 只在「真的没有这条」时出现；本平台上没有可用的
                // 存储设施，所以这里不该走到——除非平台是纯桩实现（桩的 `get`
                // 恒返回 `None`）。用 cfg 分支而不是 `assert!(cfg!(..))`：
                // 后者在编译期就是常量，clippy 会（正确地）报「断言恒为真」。
                Ok(None) => {
                    #[cfg(target_os = "linux")]
                    panic!("Linux 上取不到钥匙串必须如实报错，不能返回 None 冒充「没存过」");
                    #[cfg(not(target_os = "linux"))]
                    {} // 纯桩平台：`get` 恒为 None，这是它的契约。
                }
            }
        }
    }
}
