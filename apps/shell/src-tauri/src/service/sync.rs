//! 同步应用服务。
//!
//! 把「前端说：把这个账户同步一下」变成「连上服务器、同步、落库」。
//!
//! ## 凭据从哪来
//!
//! 授权码由前端在同步时传入，存在 [`crate::state::AppState`] 的
//! **内存**里（[`xremail_api::SecretValue`]，丢弃时清零、Debug 不打印内容）；
//! 也可能保存在**平台钥匙串**里，库里只留一条不透明引用
//! （见 [`crate::service::credential`]）。
//!
//! 两种来源都在**内存**里用完即弃——不回显、不落盘、不进日志。
//!
//! ## 连接参数来自账户配置
//!
//! 服务器地址、端口、用户名都取自库里那条账户记录，
//! **不按品牌推定**（163/QQ/沃 的参数虽已实测，但用户可以改）。

use sqlx::SqlitePool;
use xremail_api::SecretValue;
use xremail_core::model::{Account, Folder};
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::{ImapConfig, Session, TlsTrust, connect_and_login_with};
use xremail_sync::{
    FlagPushReport, SyncSummary, TransferPushReport, push_flags, push_transfers, sync_account,
};

/// 同步服务。
#[derive(Debug, Clone)]
pub struct SyncService {
    pool: SqlitePool,
}

impl SyncService {
    /// 用连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 连上服务器并登录。
    ///
    /// 单独暴露出来，是为了让调用方能把「连不上」与「连上了但提交失败」
    /// 分开处理——这两者的处置**完全相反**：
    ///
    /// * 连不上 → 命令**肯定没发出去** → 记成可重试是安全的；
    /// * 连上了但提交途中断了 → 命令**可能已经执行了** → 盲目重试会重复执行。
    ///
    /// 生产路径：系统根证书 + 强制校验，**没有**跳过校验的开关。
    pub async fn connect(&self, account: &Account, credential: &SecretValue) -> AppResult<Session> {
        let password = credential
            .as_str()
            .ok_or_else(|| AppError::Invalid("授权码不是合法的 UTF-8 文本".into()))?;

        // 连接参数来自账户配置。
        let mut config = ImapConfig::imaps(&account.server.imap_host);
        config.port = account.server.imap_port;

        let (session, _caps, _tls) =
            connect_and_login_with(&config, &TlsTrust::SystemRoots, &account.username, password)
                .await
                .map_err(|e| AppError::Protocol(format!("连接或登录失败: {e}")))?;

        Ok(session)
    }

    /// 同步一个账户的全部文件夹。
    ///
    /// `credential` 是 IMAP 授权码（明文口令对多数服务商无效）。
    /// **用完即弃**——不回显、不落盘、不进日志。
    pub async fn sync_account(
        &self,
        account: &Account,
        credential: &SecretValue,
        now_ms: i64,
    ) -> AppResult<SyncSummary> {
        let mut session = self.connect(account, credential).await?;
        sync_account(&self.pool, &mut session, account, now_ms).await
    }

    /// 把某个文件夹里排队的标记意图回写到服务器，并**回读确认**。
    ///
    /// 失败语义由 [`xremail_sync::push_flags`] 自己分类：
    /// 服务器明确拒绝 → `retryable`；条件写被拒 → 重算（有界）；
    /// 连接断了 / 超时 → `uncertain`（**不知道**服务器执行了没，不能盲目重试）。
    pub async fn push_flags(
        &self,
        session: &mut Session,
        folder: &Folder,
        now_ms: i64,
    ) -> AppResult<FlagPushReport> {
        push_flags(&self.pool, session, folder, now_ms).await
    }

    /// 把某个文件夹里排队的移动/删除意图回写到服务器，并**回读确认**。
    ///
    /// 失败分类同 [`Self::push_flags`]；额外一条：源文件夹里已经没那封邮件时
    /// 算成功（上次尝试可能已生效，`MOVE` 不幂等，重发会多出一份）。
    pub async fn push_transfers(
        &self,
        session: &mut Session,
        folder: &Folder,
        now_ms: i64,
    ) -> AppResult<TransferPushReport> {
        push_transfers(&self.pool, session, folder, now_ms).await
    }
}
