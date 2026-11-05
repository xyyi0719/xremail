//! Xremail 应用外壳（Tauri）。
//!
//! 职责边界（见 [ARCHITECTURE §9](../../../../docs/ARCHITECTURE.md)）：
//! * Command 只做 **参数校验 → 调用领域服务 → 返回 DTO**，不写业务逻辑。
//! * 跨线程的工作（IMAP 拉取、发信等）投递给 `xremail-sync` 的 Actor。
//! * 平台相关能力（托盘 / 通知 / 前台服务）收敛到 `platform` 模块。
//!
//! ## 启动顺序
//!
//! 1. 解析数据目录（桌面 `%APPDATA%/com.xremail.app`，Android app 内部存储）
//! 2. 打开 SQLite 连接池并**应用迁移**
//! 3. 注入 `AppState`，注册 Command
//!
//! 迁移失败会让应用**启动失败**并报错，而不是降级成一个空库或旧库：
//! 静默降级会让人以为数据还在，实际却是另一份（ARCHITECTURE §9）。

pub mod cmd;
pub mod secrets;
pub mod service;
pub mod state;

use std::sync::Arc;

use tauri::Manager;

use crate::service::scheduler::{AppConnector, TauriSink};
use crate::service::{AccountService, ComposeService, MessageService, SyncService};
use crate::state::{AppState, Credentials, Services};

/// 数据库文件名（位于数据目录下）。
pub const DB_FILE_NAME: &str = "xremail.db";

/// 应用入口。桌面与移动共用。
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        // 选附件用的系统对话框（选路径，字节由后端读）。
        .plugin(tauri_plugin_dialog::init());

    // 单实例与开机自启只有桌面端实现，这两个插件在 Android 上不提供 API。
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 第二次启动时聚焦已有窗口，避免多开导致服务端连接数超限。
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));

    builder
        .setup(|app| {
            let state = init_storage(app.handle())?;
            // 启动后台同步：把「有授权码的账户」交给调度器。
            //
            // 必须在 `manage` 之前做不了反——调度器与状态是分开的：
            // 状态里拿的是调度器的句柄，先 `manage` 再起 actor，
            // 这样 actor 一上来发的事件就能找到窗口。
            app.manage(state);
            start_background_sync(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd::app_info,
            cmd::list_accounts,
            cmd::create_account,
            cmd::delete_account,
            cmd::sync_account,
            cmd::list_folders,
            cmd::list_messages,
            cmd::search_messages,
            cmd::open_message,
            cmd::save_credential,
            cmd::forget_credential,
            cmd::set_message_flag,
            cmd::move_messages,
            cmd::delete_messages,
            cmd::save_draft,
            cmd::list_drafts,
            cmd::get_draft,
            cmd::withdraw_draft,
            cmd::send_message,
            cmd::resend_uncertain,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Xremail application")
        .run(|app, event| {
            // 退出时把 actor 全停掉。
            //
            // 不做这件事的后果：进程退出时 actor 还在 `IDLE` 或写库，
            // 那条连接会被服务器当成「客户端消失」，未提交的写操作
            // 就会以 `running` 的状态留在库里（下次启动靠对账收拾，
            // 但那本来是可以避免的一次）。
            if let tauri::RunEvent::Exit = event {
                app.state::<AppState>().scheduler.shutdown();
            }
        });
}

/// 把已有账户交给后台调度器。
///
/// * 只在**有授权码**时才起 actor：没有口令的账户每轮都只会失败，
///   白白在日志里刷错误，也浪费退避重试。
/// * 起失败的账户不影响其他账户（一个连不上的邮箱不该拦住另外两个）。
fn start_background_sync(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();

    let accounts = match tauri::async_runtime::block_on(state.accounts.list()) {
        Ok(accounts) => accounts,
        Err(error) => {
            tracing::warn!(%error, "启动后台同步：读账户失败");
            return;
        }
    };

    for account in accounts {
        // 有会话凭据或有钥匙串引用才值得起。
        if state.credential(&account.id).is_none() && account.secret_ref.is_none() {
            tracing::debug!(
                account = %account.email,
                "还没有授权码，先不起后台同步（手动同步一次就会起）"
            );
            continue;
        }

        if let Err(error) = state.scheduler.start(account.clone()) {
            tracing::warn!(
                account = %account.email,
                %error,
                "启动后台同步失败"
            );
        }
    }
}

/// 解析数据目录、打开数据库、应用迁移。
fn init_storage(app: &tauri::AppHandle) -> Result<AppState, Box<dyn std::error::Error>> {
    // `app_data_dir()` 在桌面端是 `%APPDATA%/<identifier>`，
    // 在 Android 上是 app 的内部存储目录。
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;

    let db_path = data_dir.join(DB_FILE_NAME);

    // 打开连接池并应用迁移。`block_on` 用的是 Tauri 自己的全局运行时；
    // 此时运行时已就绪，迁移是纯 IO，不会回调主线程。
    let pool = tauri::async_runtime::block_on(async {
        let pool =
            xremail_store::open_pool_path(&db_path, &xremail_store::PoolConfig::default()).await?;
        xremail_store::migrate(&pool).await?;
        // 补派生正文：升级之前入库的 HTML-only 邮件没有纯文本，搜不到。
        //
        // 写路径已经会派生（`upsert_message_body`），这里补的是**老数据**。
        //
        // **尽力而为**：这一步失败不应该拦住启动——
        // 补不上只是那几封老邮件搜不到，邮箱照样能用。
        match xremail_store::dao::message::backfill_derived_text(&pool).await {
            Ok(patched) if patched > 0 => {
                tracing::info!(patched, "已为历史邮件补出纯文本正文");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "补派生正文失败（老邮件可能搜不到）");
            }
        }
        Ok::<_, xremail_core::AppError>(pool)
    })?;

    // 启动对账：把上一次进程死在 `running` 上的行收成 `uncertain`。
    //
    // 为什么必须在**这里**做，而不只依赖调度器：
    // 调度器只对「已交给它调度」的账户跑（目前靠用户主动触发），
    // 而崩溃恢复必须对**每个**账户都发生——否则那些没被调度的账户，
    // 其残骸会永久停在 `running`，谁也推不动它。
    //
    // **尽力而为**：对账失败不拦启动。它只是「整理上次的残骸」，
    // 失败最坏的结果是那几行多停一轮，下一轮同步或下次启动会再收一遍
    // （`reconcile_account` 幂等）。
    tauri::async_runtime::block_on(async {
        let accounts = match xremail_store::dao::account::list_accounts(&pool).await {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::warn!(%error, "启动对账：读账户失败");
                return;
            }
        };

        for account in accounts {
            match xremail_sync::reconcile_account(
                &pool,
                &account.id,
                crate::service::current_time_ms(),
            )
            .await
            {
                Ok(report) if !report.is_noop() => {
                    tracing::info!(
                        account = %account.email,
                        reclaimed_operations = report.reclaimed_operations,
                        reclaimed_sends = report.reclaimed_sends,
                        expired = report.expired,
                        quarantined = report.quarantined,
                        failed_orphans = report.failed_orphans,
                        "启动对账：收掉上次进程留下的残骸"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, account = %account.email, "启动对账失败");
                }
            }
        }
    });

    // 凭据容器要在建调度器之前存在：连接器靠它取授权码。
    let credentials = Arc::new(Credentials::default());

    let accounts = AccountService::new(pool.clone());
    let sync = SyncService::new(pool.clone());

    // 调度器：一个账户一个 actor，`poll_interval_secs` 决定节奏。
    //
    // 时钟用系统时间：时间戳要跟库里其它表（`created_at` 等）一致，
    // 不能另起一套「进程启动后的相对时间」。
    let scheduler = xremail_sync::Scheduler::with_options(
        pool.clone(),
        Arc::new(AppConnector::new(
            Arc::clone(&credentials),
            accounts.clone(),
            sync.clone(),
        )),
        Arc::new(TauriSink::new(app.clone())),
        xremail_sync::RetryPolicy::default(),
        Arc::new(crate::service::current_time_ms),
    );

    Ok(AppState::new(
        data_dir,
        Services {
            accounts,
            messages: MessageService::new(pool.clone()),
            sync,
            compose: ComposeService::new(pool.clone(), AccountService::new(pool.clone())),
        },
        scheduler,
        credentials,
        pool,
    ))
}

/// 初始化结构化日志。
///
/// 默认级别 `info`；可用 `RUST_LOG=xremail_shell=debug,xremail_core=debug` 覆盖。
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
