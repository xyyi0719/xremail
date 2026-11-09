//! 把 `xremail-sync` 的调度器接到 Tauri 上。
//!
//! 这一层只做三件翻译，不含任何同步逻辑：
//!
//! 1. **凭据**：调度器不认识钥匙串，只认识「给我一个已登录的 `Session`」。
//!    这里按「本次会话内存 → 平台钥匙串 → 放弃」的顺序取授权码。
//! 2. **事件**：`AccountEvent` → [`AppEvent`] → Tauri 事件。
//! 3. **时钟**：账户的 `poll_interval_secs` 决定节奏，时钟就是系统时间。
//!
//! 为什么连接器要自己拿 [`Credentials`] 而不是回头查 `AppState`：
//! `AppState` 里装着调度器，调度器里装着连接器——让连接器反过来去
//! `app.state::<AppState>()` 取凭据会形成一条「状态 → 调度器 → 连接器 → 状态」
//! 的环。环形依赖本身能跑（`AppHandle` 是句柄，不是借用），但它让
//! 「连接器需要什么」变得不可见。这里改成显式传入，依赖是单向的。

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Runtime};
use xremail_api::SecretValue;
use xremail_core::model::{Account, SyncState};
use xremail_core::{AppError, AppEvent};
use xremail_sync::{AccountConnector, AccountEvent, ConnectFuture, EventSink};

use crate::service::{AccountService, CredentialService, SyncService};
use crate::state::Credentials;

/// 叫醒**所有**正在调度的账户，立刻同步一轮（托盘菜单「立即同步」用）。
///
/// 这是对所有在调度账户的一次性「醒一下」，所以逐账户走 [`Scheduler::wake`]，
/// 而**不是**逐个 `start`——`start` 对已在跑的账户也会叫醒，但它做的是
/// 「确保在调度」，调用方读起来像被加了一份新调度。
///
/// 单个账户叫醒失败**不打断其余账户**（某个 actor 刚自己退出是正常情形，
/// `wake` 那时会报错），只记日志。返回真正叫醒了的账户数：
/// 托盘那边不弹窗，一条日志就是全部反馈，调用方不该假装「都醒了」。
pub fn wake_all(scheduler: &xremail_sync::Scheduler) -> usize {
    let mut woken = 0;
    for account_id in scheduler.running() {
        match scheduler.wake(&account_id) {
            Ok(()) => woken += 1,
            Err(error) => {
                tracing::debug!(account_id = %account_id, %error, "叫醒账户失败（跳过）");
            }
        }
    }
    woken
}

/// 按账户取凭据的连接器。
///
/// 取不到凭据就返回错误——**不**返回一个空 Session 让后续命令去撞墙。
pub struct AppConnector {
    credentials: Arc<Credentials>,
    accounts: AccountService,
    sync: SyncService,
}

impl AppConnector {
    /// 构造。
    pub fn new(credentials: Arc<Credentials>, accounts: AccountService, sync: SyncService) -> Self {
        Self {
            credentials,
            accounts,
            sync,
        }
    }

    /// 取这个账户的授权码：先看本次会话的内存，再问平台钥匙串。
    async fn resolve(&self, account: &Account) -> Result<SecretValue, AppError> {
        if let Some(credential) = self.credentials.get(&account.id) {
            return Ok(credential);
        }

        // 钥匙串里没存过（`secret_ref` 为空）时这里返回 `None`。
        let stored = CredentialService::new(self.accounts.clone())
            .resolve_credential(account)
            .await?;

        stored.ok_or_else(|| {
            AppError::Auth(format!(
                "账户 {} 还没有可用的授权码，请先手动同步一次",
                account.email
            ))
        })
    }
}

impl AccountConnector for AppConnector {
    fn connect<'a>(&'a self, account: &'a Account) -> ConnectFuture<'a> {
        Box::pin(async move {
            let credential = self.resolve(account).await?;
            self.sync.connect(account, &credential).await
        })
    }
}

/// 把调度器事件转发成 Tauri 事件。
///
/// `emit` 是同步的且**必须不阻塞**：它在 actor 的任务里被调用，
/// 卡住就等于卡住那个账户的同步循环。
pub struct TauriSink<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> TauriSink<R> {
    /// 构造。
    pub fn new(app: AppHandle<R>) -> Self {
        Self { app }
    }

    fn send(&self, event: AppEvent) {
        // 前端可能已经退出（窗口关了但进程还在收尾），失败就丢掉。
        let _ = self.app.emit(event.name(), &event);
    }

    fn state(&self, account_id: &str, state: SyncState) {
        self.send(AppEvent::SyncStateChanged {
            account_id: account_id.to_string(),
            folder_id: None,
            state,
        });
    }
}

impl<R: Runtime> EventSink for TauriSink<R> {
    fn emit(&self, event: AccountEvent) {
        match event {
            AccountEvent::Started { account_id, .. } => {
                self.state(&account_id, SyncState::Connecting);
            }
            AccountEvent::Synced {
                account_id,
                reconcile,
                ..
            } => {
                // 同步完就是「连上了且在等下一轮」（`IDLE` 或轮询）。
                self.state(&account_id, SyncState::Online);
                // 对账可能刚把某条操作收成 `failed`（R29 的「三天没结论」等），
                // 队列面板得知道，否则它要等下一次轮询才变。
                if let Some(report) = reconcile
                    && !report.is_noop()
                {
                    self.send(AppEvent::OperationsChanged {
                        account_id: account_id.clone(),
                    });
                }
            }
            AccountEvent::Failed {
                account_id,
                error,
                retry_in,
                ..
            } => {
                // 「退避重试」是 `Reconnecting`，不是 `Failed`——
                // 前者意思是「还会自己再试」，后者在 UI 上是「要人管」。
                self.state(&account_id, SyncState::Reconnecting);
                self.send(AppEvent::SyncError {
                    account_id,
                    message: format!("同步失败（{} 秒后重试）：{error}", retry_in.as_secs()),
                });
            }
            AccountEvent::Panicked {
                account_id,
                message,
                retry_in,
                ..
            } => {
                self.state(&account_id, SyncState::Reconnecting);
                self.send(AppEvent::SyncError {
                    account_id,
                    message: format!(
                        "同步内部错误（已拦下，{} 秒后重试）：{message}",
                        retry_in.as_secs()
                    ),
                });
            }
            AccountEvent::NeedsReauth {
                account_id,
                message,
            } => {
                // 「服务器拒了凭据」—→ UI 上必须是一个**停在那里等人**的状态，
                // 不是「正在重连」。重试一个错密码永远不会成功。
                self.state(&account_id, SyncState::ReauthRequired);
                self.send(AppEvent::SyncError {
                    account_id,
                    message: format!("授权码被拒绝，已暂停自动同步，请重新保存授权码：{message}"),
                });
            }
            AccountEvent::Abandoned {
                account_id,
                message,
            } => {
                self.state(&account_id, SyncState::Failed);
                self.send(AppEvent::SyncError {
                    account_id,
                    message: format!("这个账户已停止自动同步：{message}"),
                });
            }
            AccountEvent::Stopped { account_id } => {
                self.state(&account_id, SyncState::Idle);
            }
        }
    }
}
