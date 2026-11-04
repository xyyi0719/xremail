//! 应用共享状态。
//!
//! 由 Tauri `manage` 注入。持有应用数据目录与各领域服务。

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sqlx::SqlitePool;
use xremail_api::SecretValue;
use xremail_sync::Scheduler;

use crate::service::{AccountService, ComposeService, MessageService, SyncService};

/// 本次会话的账户凭据（**只在内存里**，不落盘）。
///
/// 单独成结构，是因为**调度器也要用它**：后台的同步循环需要一个
/// 「给我这个账户的授权码」的来源，而这个来源不该是整个 `AppState`
/// （那会让调度器反过来依赖装着它自己的状态）。
///
/// 平台钥匙串是另一条来源（按账户的 `secret_ref` 取），在
/// [`crate::service::CredentialService`] 里；这里只放**本次会话**
/// 用户手输过的那份。
#[derive(Debug, Default)]
pub struct Credentials {
    map: Mutex<HashMap<String, SecretValue>>,
}

impl Credentials {
    /// 记下某个账户的凭据（覆盖旧的）。
    pub fn remember(&self, account_id: &str, credential: SecretValue) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(account_id.to_string(), credential);
        }
    }

    /// 取某个账户的凭据。
    pub fn get(&self, account_id: &str) -> Option<SecretValue> {
        let map = self.map.lock().ok()?;
        map.get(account_id).cloned()
    }

    /// 忘掉某个账户的凭据。
    pub fn forget(&self, account_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(account_id);
        }
    }
}

/// 领域服务包。
///
/// 单独打包只是因为 `AppState::new` 的参数已经太多了（4 个服务 + 4 个其它），
/// 平铺开既难读也容易被调错位置。
#[derive(Debug, Clone)]
pub struct Services {
    /// 账户服务。
    pub accounts: AccountService,
    /// 邮件服务。
    pub messages: MessageService,
    /// 同步服务。
    pub sync: SyncService,
    /// 发信服务。
    pub compose: ComposeService,
}

/// 应用共享状态。
pub struct AppState {
    /// 数据目录（数据库、附件、日志的根目录）。
    pub data_dir: PathBuf,
    /// 账户服务。
    pub accounts: AccountService,
    /// 邮件服务。
    pub messages: MessageService,
    /// 同步服务。
    pub sync: SyncService,
    /// 发信服务（写信 → 草稿 → 提交给服务器）。
    pub compose: ComposeService,
    /// 后台同步调度器：一个账户一个 actor。
    ///
    /// 它是**唯一的长期同步循环**。Command 里那种「点一下同步一次」
    /// 是一次性动作，跑完就结束；这个负责让邮箱自己跟上服务器。
    pub scheduler: Scheduler,
    /// 数据库连接池。
    pool: SqlitePool,
    /// 本次会话的账户凭据。
    ///
    /// 用 [`SecretValue`]：丢弃时清零，`Debug` 只打印长度不打印内容。
    credentials: Arc<Credentials>,
}

impl AppState {
    /// 构造状态。
    pub fn new(
        data_dir: PathBuf,
        services: Services,
        scheduler: Scheduler,
        credentials: Arc<Credentials>,
        pool: SqlitePool,
    ) -> Self {
        Self {
            data_dir,
            accounts: services.accounts,
            messages: services.messages,
            sync: services.sync,
            compose: services.compose,
            scheduler,
            pool,
            credentials,
        }
    }

    /// 数据库文件路径。
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join(crate::DB_FILE_NAME)
    }

    /// 数据库连接池。
    ///
    /// Command 需要它做「按需拉正文」这种跨服务的操作。
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// 记下某个账户的凭据（覆盖旧的）。
    pub fn remember_credential(&self, account_id: &str, credential: SecretValue) {
        self.credentials.remember(account_id, credential);
    }

    /// 取某个账户的凭据。
    pub fn credential(&self, account_id: &str) -> Option<SecretValue> {
        self.credentials.get(account_id)
    }

    /// 忘掉某个账户的凭据。
    pub fn forget_credential(&self, account_id: &str) {
        self.credentials.forget(account_id);
    }
}

impl fmt::Debug for AppState {
    /// 手写而不是 `derive`：[`Scheduler`] 里装的是 trait object，没有 `Debug`。
    ///
    /// 只列**能安全打印**的字段：凭据那个 map 刻意不列，
    /// 免得哪天有人 `{:?}` 一整个 state 就把口令打进日志。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppState")
            .field("data_dir", &self.data_dir)
            .field("pool", &self.pool)
            .finish_non_exhaustive()
    }
}
