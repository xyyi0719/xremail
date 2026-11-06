//! 账户 DKIM 配置的 DAO。
//!
//! 对应 `account_dkim` 表（`0008_account_dkim.sql`）。
//!
//! **这里不出现任何密钥材料**：`private_key_ref` 是钥匙串条目的不透明引用，
//! 私钥本身由 `apps/shell` 的 DKIM 服务写进平台钥匙串
//! （见 `apps/shell/src-tauri/src/secrets/`）。
//!
//! 「没有行」= 这个账户没配 DKIM = 发信字节与以前完全一致。
//!
//! 查询用运行期 `query_as`（不是编译期宏）：`account_dkim` 是新增表，
//! 用宏就要为它重生 `.sqlx/` 离线数据；列清单是字面量，改 schema 时
//! 与之配套的 DAO 用例（`tests` 模块）会立刻发现漂移。

use sqlx::FromRow;
use sqlx::SqlitePool;
use xremail_core::{AppError, AppResult};

use crate::error::map_sqlx;

/// `account_dkim` 表的一行。
///
/// 用 `FromRow` 派生 + 显式列清单（与 `account.rs` 同一写法）。
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct DkimConfigRow {
    pub account_id: String,
    /// DNS 选择器（`s=`）。
    pub selector: String,
    /// 签名域（`d=`）。
    pub domain: String,
    /// 签名算法（`a=`），目前只有 `rsa-sha256`。
    pub header_alg: String,
    /// 钥匙串里那条私钥的引用（**不是**私钥）。
    pub private_key_ref: String,
    /// 发信时是否真的签名。
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 读某个账户的 DKIM 配置。没有配置时返回 `None`。
pub async fn get_dkim_config(
    pool: &SqlitePool,
    account_id: &str,
) -> AppResult<Option<DkimConfigRow>> {
    // 列清单必须是编译期字面量（sqlx 0.9 的 `SqlSafeStr` 只接受 `&'static str`）。
    // 好处是这条 SELECT 可以被 `cargo sqlx prepare` 的离线数据覆盖。
    sqlx::query_as::<_, DkimConfigRow>(
        "SELECT account_id, selector, domain, header_alg, private_key_ref, enabled, \
         created_at, updated_at FROM account_dkim WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(|err| map_sqlx(err, "get_dkim_config"))
}

/// 写入（或覆盖）某个账户的 DKIM 配置。
///
/// 覆盖时 `created_at` 保留第一次写入的时间，`updated_at` 取传入值。
/// 账户不存在时外键约束会失败（`AppError::Invalid`），
/// 而不是留下一条没有主人的配置。
#[allow(clippy::too_many_arguments)]
pub async fn upsert_dkim_config(
    pool: &SqlitePool,
    account_id: &str,
    selector: &str,
    domain: &str,
    header_alg: &str,
    private_key_ref: &str,
    enabled: bool,
    now: i64,
) -> AppResult<DkimConfigRow> {
    sqlx::query(
        r#"
        INSERT INTO account_dkim
            (account_id, selector, domain, header_alg, private_key_ref, enabled,
             created_at, updated_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (account_id) DO UPDATE SET
            selector        = excluded.selector,
            domain          = excluded.domain,
            header_alg      = excluded.header_alg,
            private_key_ref = excluded.private_key_ref,
            enabled         = excluded.enabled,
            updated_at      = excluded.updated_at
        "#,
    )
    .bind(account_id)
    .bind(selector)
    .bind(domain)
    .bind(header_alg)
    .bind(private_key_ref)
    .bind(enabled)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|err| map_sqlx(err, "upsert_dkim_config"))?;

    get_dkim_config(pool, account_id)
        .await?
        .ok_or_else(|| AppError::Storage(format!("DKIM 配置写入后读不回来：{account_id}")))
}

/// 删除某个账户的 DKIM 配置。返回是否真的删掉了。
pub async fn delete_dkim_config(pool: &SqlitePool, account_id: &str) -> AppResult<bool> {
    let result = sqlx::query("DELETE FROM account_dkim WHERE account_id = ?")
        .bind(account_id)
        .execute(pool)
        .await
        .map_err(|err| map_sqlx(err, "delete_dkim_config"))?;
    Ok(result.rows_affected() > 0)
}
