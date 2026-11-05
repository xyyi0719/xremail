//! 同步编排：把一个账户的全部文件夹同步进本地。
//!
//! ## 流程
//!
//! 1. `LIST` 列出服务器上的文件夹（**安全命令层**，服务器拒绝会报错）；
//! 2. 每个文件夹 upsert 一条本地记录（冲突键 `(account_id, remote_name)`，
//!    **不覆盖**本地 `id`——`message` 的外键指着它）；
//! 3. 逐个文件夹跑 [`crate::pull::pull_folder`]（差分 → epoch 判定 → 落库）。
//!
//! ## 凭据不落盘
//!
//! 授权码只作为**参数**传进来，用完即弃。
//! 落盘需要平台钥匙串（P4）或主密码方案——那时 `account.secret_ref`
//! 才是有意义的不透明引用。
//!
//! ## 只支持隐式 TLS
//!
//! 协议层只走隐式 TLS 且强制校验证书（见 SECURITY §2.2），
//! 所以 `TlsMode::None` / `Startls` 的账户**直接报错**，
//! 绝不静默降级成明文。

use sqlx::SqlitePool;
use xremail_core::model::{Account, Folder, TlsMode};
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::folders::list_folders_with_timeout;
use xremail_protocol_imap::{ImapConfig, Session};
use xremail_store::dao::folder as folder_dao;

use crate::net::SyncOptions;
use crate::pull::{FolderSyncReport, pull_folder_with_options};

/// 一轮账户同步的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// 同步的文件夹数。
    pub folders: usize,
    /// 新拉到多少封。
    pub added: usize,
    /// 服务器已删、清掉多少封。
    pub removed: usize,
    /// 逐个文件夹的详情。
    pub reports: Vec<FolderSyncReport>,
}

/// 同步一个账户的全部文件夹。
///
/// `session` 必须是**已登录**的会话。
///
/// 认领时用本进程的启动标识（[`crate::boot::boot_id`]），
/// 用来标记「这批待提交操作是我在发」（见迁移 `0006_operation_owner.sql`）。
/// 它不参与同步语义，也不落日志。
pub async fn sync_account(
    pool: &SqlitePool,
    session: &mut Session,
    account: &Account,
    now_ms: i64,
) -> AppResult<SyncSummary> {
    sync_account_as(pool, session, account, now_ms, crate::boot::boot_id()).await
}

/// 同 [`sync_account`]，但显式指定认领归属。
pub async fn sync_account_as(
    pool: &SqlitePool,
    session: &mut Session,
    account: &Account,
    now_ms: i64,
    boot_id: &str,
) -> AppResult<SyncSummary> {
    sync_account_as_with(
        pool,
        session,
        account,
        now_ms,
        boot_id,
        SyncOptions::default(),
    )
    .await
}

/// 同 [`sync_account_as`]，但**显式指定超时等选项**。
///
/// [`crate::actor::AccountActor`] 走这一条：它的
/// [`crate::actor::SyncPolicy::command_timeout`] 必须真的落到每一条命令上，
/// 否则「policy 里配了超时」只是一句空话。
pub async fn sync_account_as_with(
    pool: &SqlitePool,
    session: &mut Session,
    account: &Account,
    now_ms: i64,
    boot_id: &str,
    options: SyncOptions,
) -> AppResult<SyncSummary> {
    // 只支持隐式 TLS：不静默降级。
    if account.server.imap_tls != TlsMode::Implicit {
        return Err(AppError::Unsupported(format!(
            "账户 {} 的 IMAP 加密方式是 {:?}，目前只支持隐式 TLS（implicit）",
            account.email, account.server.imap_tls
        )));
    }

    // 1. 服务器上的文件夹。
    let remote = list_folders_with_timeout(session, options.command_timeout)
        .await
        .map_err(|e| AppError::Protocol(format!("列文件夹失败: {e}")))?;

    let mut summary = SyncSummary::default();

    for item in remote {
        // 2. 本地文件夹记录。
        //
        // 先看本地有没有同名文件夹——有就复用它的 `id`（`message` 的外键指着它，
        // 换 id 会让已缓存的邮件失去归属）。
        //
        // 比对用**线路上的原样**（modified UTF-7）。
        let existing = folder_dao::list_folders(pool, &account.id)
            .await?
            .into_iter()
            .find(|folder| folder.remote_name == item.raw_name);

        let folder = Folder {
            id: existing
                .as_ref()
                .map(|folder| folder.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            account_id: account.id.clone(),
            remote_name: item.raw_name.clone(),
            remote_id: None,
            local_name: existing
                .as_ref()
                .and_then(|folder| folder.local_name.clone()),
            delimiter: item.delimiter.clone(),
            subscribed: existing.as_ref().map(|f| f.subscribed).unwrap_or(true),
            selectable: true,
            last_sync_at: existing.as_ref().and_then(|folder| folder.last_sync_at),
            last_uid: existing.as_ref().and_then(|folder| folder.last_uid),
            highest_modseq: existing.as_ref().and_then(|folder| folder.highest_modseq),
            uidvalidity: existing.as_ref().and_then(|folder| folder.uidvalidity),
        };
        folder_dao::upsert_folder(pool, &folder).await?;

        // 3. 拉取这个文件夹。
        //
        // 单个文件夹失败**不该**让整轮同步失败：邮箱里常有一个文件夹
        // 是坏的（权限不足之类）。逐个记录，继续下一个。
        match pull_folder_with_options(pool, session, &folder, now_ms, options).await {
            Ok(report) => {
                summary.folders += 1;
                summary.added += report.added;
                summary.removed += report.removed;
                summary.reports.push(report);

                // 3.5 排空这个文件夹的待提交操作。
                //
                // 放在**拉取之后**：UID 与 epoch 刚刷新过，
                // 队列里的意图应该按最新的现实去提交。
                //
                // 这一步让 `retryable` 的操作有了自动重试的机会——
                // 在此之前只有用户手动点一下才会回写，失败一次就永远躺着。
                flush_pending(pool, session, &folder, now_ms, boot_id, options).await;
            }
            Err(error) => {
                tracing::warn!(
                    account = %account.email,
                    folder = %item.name,
                    %error,
                    "文件夹同步失败，继续下一个"
                );
            }
        }
    }

    Ok(summary)
}

/// 把一个文件夹里待提交的操作回写到服务器。
///
/// 失败**不影响**拉取结果：发不出去就留在队列里，下一轮再试。
/// 这里刻意不向上抛错：一个文件夹提交不了不该让整轮同步失败，
/// 更不该把已拉回来的数据丢掉。
async fn flush_pending(
    pool: &SqlitePool,
    session: &mut Session,
    folder: &Folder,
    now_ms: i64,
    boot_id: &str,
    options: SyncOptions,
) {
    // 重新读一遍：`pull_folder` 可能刚换过 epoch。
    let fresh = match folder_dao::get_folder(pool, &folder.id).await {
        Ok(Some(fresh)) => fresh,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, folder = %folder.remote_name, "读文件夹失败，跳过回写");
            return;
        }
    };

    match crate::push::push_flags_as_with_options(pool, session, &fresh, now_ms, boot_id, options)
        .await
    {
        Ok(report) if !report.is_noop() => tracing::debug!(
            folder = %fresh.remote_name,
            succeeded = report.succeeded,
            retryable = report.retryable,
            failed = report.failed,
            uncertain = report.uncertain,
            "标记意图已回写"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            folder = %fresh.remote_name,
            "标记回写失败，意图留在队列里"
        ),
    }

    match crate::transfer_push::push_transfers_as_with_options(
        pool, session, &fresh, now_ms, boot_id, options,
    )
    .await
    {
        Ok(report) if !report.is_noop() => tracing::debug!(
            folder = %fresh.remote_name,
            moved = report.moved,
            deleted = report.deleted,
            retryable = report.retryable,
            failed = report.failed,
            uncertain = report.uncertain,
            "移动/删除意图已回写"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            folder = %fresh.remote_name,
            "移动/删除回写失败，意图留在队列里"
        ),
    }
}

/// 从账户配置构造 IMAP 连接参数。
pub fn imap_config_for(account: &Account) -> ImapConfig {
    let mut config = ImapConfig::imaps(&account.server.imap_host);
    config.port = account.server.imap_port;
    config
}
