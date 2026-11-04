//! Tauri Command 层。
//!
//! 约定：
//! * 只做 **参数校验 → 调用领域服务 → 返回 DTO**；
//! * 不写业务逻辑（业务逻辑属于 `src/service/`）；
//! * **状态提交后才发事件**（见 [ARCHITECTURE §1.6](../../../../docs/ARCHITECTURE.md)。

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime, State};
use xremail_api::NewAccountRequest;
use xremail_api::SecretValue;
use xremail_core::AppError;
use xremail_core::AppEvent;
use xremail_core::model::{Account, MailOperation, Message, OperationState, SyncState};
use xremail_store::dao::operation as operation_dao;

use crate::service::compose::{DraftDetail, DraftDto, DraftInput, SendResult};
use crate::service::credential::CredentialService;
use crate::service::current_time_ms;
use crate::service::message::SearchHitDto;
use crate::service::message::{FolderDto, MessageBodyView, MessageFlag};
use crate::state::AppState;

/// 应用基本信息，供前端做环境探测与"关于"页。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// 产品名。
    pub name: &'static str,
    /// 版本号。
    pub version: &'static str,
    /// 运行平台。
    pub platform: &'static str,
    /// 数据目录（前端只用于展示，不允许写入）。
    pub data_dir: Option<String>,
}

/// 返回应用基本信息。
#[tauri::command]
pub fn app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        name: "Xremail",
        version: env!("CARGO_PKG_VERSION"),
        platform: std::env::consts::OS,
        data_dir: Some(state.data_dir.display().to_string()),
    }
}

/// 列出已配置的账户。
///
/// 数据来自 SQLite，不是硬编码的空列表。
#[tauri::command]
pub async fn list_accounts(state: State<'_, AppState>) -> Result<Vec<Account>, String> {
    state.accounts.list().await.map_err(|e| user_error(&e))
}

/// 新增一个账户。
///
/// 凭据（P0 尚无平台钥匙串）不在这条链路上，详见
/// [`xremail_api::NewAccountRequest`]。
#[tauri::command]
pub async fn create_account<R: Runtime>(
    state: State<'_, AppState>,
    app: AppHandle<R>,
    request: NewAccountRequest,
) -> Result<Account, String> {
    // 服务层先校验再落库：校验失败时库里不会留下半条记录。
    let account = state
        .accounts
        .create(request)
        .await
        .map_err(|e| user_error(&e))?;

    // 提交成功后才发事件。
    let event = AppEvent::AccountAdded {
        account: account.clone(),
    };
    app.emit(event.name(), &event).map_err(|e| e.to_string())?;

    Ok(account)
}

/// 删除一个账户。返回是否真的删掉了。
#[tauri::command]
pub async fn delete_account<R: Runtime>(
    state: State<'_, AppState>,
    app: AppHandle<R>,
    id: String,
) -> Result<bool, String> {
    let deleted = state
        .accounts
        .delete(&id)
        .await
        .map_err(|e| user_error(&e))?;

    if deleted {
        let event = AppEvent::AccountRemoved { account_id: id };
        app.emit(event.name(), &event).map_err(|e| e.to_string())?;
    }

    Ok(deleted)
}

/// 同步结果（给前端看的）。
///
/// Command 层定义自己的响应形状，不让 `xremail-sync` 依赖 serde。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    /// 同步了几个文件夹。
    pub folders: usize,
    /// 新拉到多少封。
    pub added: usize,
    /// 服务器已删、清掉多少封。
    pub removed: usize,
}

/// 同步一个账户的全部文件夹。
///
/// `credential` 是 IMAP 授权码：
///
/// * **传了** → 记进**内存**（本次会话用），并继续同步；
/// * **没传** → 去**平台钥匙串**里取这个账户保存过的授权码；
/// * 两处都没有 → 报错，提示用户先同步一次。
///
/// 授权码在任何情况下都**不落盘进数据库**、**不进日志**、**不回显**。
#[tauri::command]
pub async fn sync_account<R: Runtime>(
    state: State<'_, AppState>,
    app: AppHandle<R>,
    account_id: String,
    credential: Option<String>,
) -> Result<SyncResult, String> {
    let account = state
        .accounts
        .get(&account_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("账户 {account_id} 不存在"))?;

    // 传了凭据就记进内存，供后续的「按需拉正文」「回写标记」用。
    if let Some(raw) = credential {
        state.remember_credential(&account_id, SecretValue::new(raw.into_bytes()));
    }

    // 内存里没有就去平台钥匙串取。两处都没有 → 这个账户还没保存授权码。
    let Some(credential) = resolve_credential(&state, &account).await? else {
        return Err("这个账户还没保存授权码，请先同步一次".to_string());
    };

    // 先报「连接中」。
    let event = AppEvent::SyncStateChanged {
        account_id: account_id.clone(),
        folder_id: None,
        state: SyncState::Connecting,
    };
    let _ = app.emit(event.name(), &event);

    let result = state
        .sync
        .sync_account(&account, &credential, current_time_ms())
        .await;

    let event = AppEvent::SyncStateChanged {
        account_id: account_id.clone(),
        folder_id: None,
        state: match &result {
            Ok(_) => SyncState::Idle,
            Err(_) => SyncState::Failed,
        },
    };
    let _ = app.emit(event.name(), &event);

    result
        .map(|summary| SyncResult {
            folders: summary.folders,
            added: summary.added,
            removed: summary.removed,
        })
        .map_err(|e| user_error(&e))
}

/// 列出某账户的全部文件夹。
#[tauri::command]
pub async fn list_folders(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<FolderDto>, String> {
    state
        .messages
        .list_folders(&account_id)
        .await
        .map_err(|e| user_error(&e))
}

/// 列出某文件夹里的邮件。
///
/// 返回前会把该文件夹**未完结**的标记意图叠到服务器快照上：
/// 用户点完星标，界面立刻就变。**这只是显示层**——
/// 服务器还没确认，界面应当把它显示成「待提交」。
#[tauri::command]
pub async fn list_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_id: String,
) -> Result<Vec<Message>, String> {
    state
        .messages
        .list_messages(&account_id, &folder_id)
        .await
        .map_err(|e| user_error(&e))
}

/// 打开一封邮件：本地有正文就直接返回，没有就**按需**去服务器拉。
///
/// 拉正文用 `BODY.PEEK[]`——**不置** `\Seen`
/// （「看正文」不等于「标记已读」）。
#[tauri::command]
pub async fn open_message(
    state: State<'_, AppState>,
    account_id: String,
    folder_id: String,
    uid: i64,
) -> Result<Option<MessageBodyView>, String> {
    // 先在本地找这封邮件。
    //
    // 本地**根本没有**它 → 返回 None（「没这封邮件」与「拉失败」是两回事）。
    let Some(_message) = state
        .messages
        .get_message(&account_id, &folder_id, uid)
        .await
        .map_err(|e| user_error(&e))?
    else {
        return Ok(None);
    };

    // 本地有正文 → 直接返回（**HTML 已消毒**）。
    if let Some(body) = state
        .messages
        .get_body(&account_id, &folder_id, uid)
        .await
        .map_err(|e| user_error(&e))?
    {
        return Ok(Some(MessageBodyView::from_domain(body)));
    }

    // 有邮件但没正文 → **按需**去服务器拉。需要凭据与连接。
    let account = state
        .accounts
        .get(&account_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("账户 {account_id} 不存在"))?;

    // 内存里没有就去平台钥匙串取。
    let Some(credential) = resolve_credential(&state, &account).await? else {
        return Err("这个账户还没保存授权码，请先同步一次".to_string());
    };

    let password = credential
        .as_str()
        .ok_or_else(|| "授权码不是合法的 UTF-8 文本".to_string())?
        .to_string();

    let mut config = xremail_protocol_imap::ImapConfig::imaps(&account.server.imap_host);
    config.port = account.server.imap_port;

    let (mut session, _caps, _tls) = xremail_protocol_imap::connect_and_login_with(
        &config,
        &xremail_protocol_imap::TlsTrust::SystemRoots,
        &account.username,
        &password,
    )
    .await
    .map_err(|e| format!("连接或登录失败: {e}"))?;

    // 需要 epoch 才能定位邮件。
    let folder = state
        .messages
        .list_folders(&account_id)
        .await
        .map_err(|e| user_error(&e))?
        .into_iter()
        .find(|folder| folder.id == folder_id)
        .ok_or_else(|| "文件夹不存在".to_string())?;
    let uidvalidity = folder
        .uidvalidity
        .ok_or_else(|| "该文件夹还没同步过，不知道 epoch".to_string())?;

    xremail_sync::fetch_body(
        state.pool(),
        &mut session,
        &account_id,
        &folder_id,
        &folder.raw_name,
        uid,
        uidvalidity,
    )
    .await
    .map_err(|e| user_error(&e))?;

    state
        .messages
        .get_body_view(&account_id, &folder_id, uid)
        .await
        .map_err(|e| user_error(&e))
}

/// 把某个账户的授权码保存到**平台钥匙串**。
///
/// 保存成功时，账户记录上的 `secretRef` 会指向那条钥匙串条目
/// （数据库里**不存**授权码本身）。
///
/// 失败行为：平台没有适配钥匙串、或钥匙串不可用时**如实报错**——
/// 不降级成「存到内存里」或「存到文件里」，那样用户会以为已经保存了。
#[tauri::command]
pub async fn save_credential(
    state: State<'_, AppState>,
    account_id: String,
    credential: String,
) -> Result<(), String> {
    // 账户必须存在：否则会写进一条谁也用不了的钥匙串条目。
    state
        .accounts
        .get(&account_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("账户 {account_id} 不存在"))?;

    let trimmed = credential.trim();
    if trimmed.is_empty() {
        return Err("授权码不能为空".to_string());
    }

    CredentialService::new(state.accounts.clone())
        .save_credential(&account_id, SecretValue::from_str_lossless(trimmed))
        .await
        .map_err(|e| user_error(&e))
}

/// 忘掉某个账户的授权码。
///
/// 从平台钥匙串删掉那条，并把账户记录上的 `secretRef` 清空。
/// 返回是否**真的**删掉了钥匙串里的条目。
#[tauri::command]
pub async fn forget_credential(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<bool, String> {
    let deleted = CredentialService::new(state.accounts.clone())
        .forget_credential(&account_id)
        .await
        .map_err(|e| user_error(&e))?;

    // 内存里那份也要清掉：本次会话的凭据表属于应用状态，
    // 凭据服务只管钥匙串与账户记录这两样。
    state.forget_credential(&account_id);

    Ok(deleted)
}

/// 标记变更的结果（给前端看的）。
///
/// 三个字段回答三个不同的问题：
/// * `queued`：意图**记下来了吗**（已落进本地操作队列）；
/// * `applied`：服务器**回读确认**过改动生效了吗；
/// * `state`：这条意图现在到哪一步了。
///
/// `state` 取值与 `mail_operation.state` 列一致：
/// `queued` / `succeeded` / `retryable` / `failed` / `uncertain`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagIntentResult {
    /// 意图已落库进本地操作队列。
    pub queued: bool,
    /// 服务器已回读确认改动生效。
    pub applied: bool,
    /// 状态：`queued` / `succeeded` / `retryable` / `failed` / `uncertain`。
    pub state: String,
    /// 给用户看的中文说明。
    pub message: Option<String>,
}

/// 标记一封邮件（已读 / 未读、星标 / 取消星标）。
///
/// ## 链路
///
/// 1. **意图落库**：先记一条 `mail_operation`（`queued`）。
///    这一步**必须**先成功——否则用户的点击就丢了。
/// 2. **立刻回写**：能拿到凭据（内存里的或平台钥匙串里的）就连服务器，
///    提交这条意图并**回读确认**。
/// 3. **读回状态**：把**这一条**操作的状态读出来告诉前端。
///
/// 拿不到凭据 → 只落库不提交，返回 `state = "queued"`，
/// 提示用户保存授权码后会自动提交。
///
/// ## 失败行为
///
/// * 找不到文件夹 / 邮件 → 报错；
/// * 文件夹**没同步过**（没有 epoch）→ 报错（不知道该把意图绑到哪个 epoch）；
/// * 连不上服务器 → 记成 `retryable`（命令肯定没发出去，重试安全）；
/// * 服务器明确拒绝 → `retryable`（服务器告诉我们**没执行**）；
/// * 回读确认不通过 → `retryable`（服务器那边与意图不一致）；
/// * 连接中途断了 → `uncertain`（**不知道**服务器执行了没，不会重复执行）。
#[tauri::command]
pub async fn set_message_flag(
    state: State<'_, AppState>,
    account_id: String,
    folder_id: String,
    uid: i64,
    flag: MessageFlag,
    value: bool,
) -> Result<FlagIntentResult, String> {
    // 1. 意图落库：这一步必须先成功。
    let operation = state
        .messages
        .set_flag_intent(&account_id, &folder_id, uid, flag, value)
        .await
        .map_err(|e| user_error(&e))?;

    let account = state
        .accounts
        .get(&account_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("账户 {account_id} 不存在"))?;

    // 2. 凭据：内存里的优先，没有再去平台钥匙串取。
    let Some(credential) = resolve_credential(&state, &account).await? else {
        return Ok(FlagIntentResult {
            queued: true,
            applied: false,
            state: "queued".to_string(),
            message: Some("已排队：保存授权码后会自动提交给服务器".to_string()),
        });
    };

    let now = current_time_ms();

    // 3. 立刻回写并回读确认。
    let mut session = match state.sync.connect(&account, &credential).await {
        Ok(session) => session,
        Err(error) => {
            // 连都没连上 → 命令**肯定没发出去** → 记成可重试是安全的。
            let detail = user_error(&error);
            let _ = operation_dao::mark_operation_retryable(
                state.pool(),
                &operation.id,
                "connect_failed",
                &detail,
                now,
            )
            .await;
            return Ok(FlagIntentResult {
                queued: true,
                applied: false,
                state: "retryable".to_string(),
                message: Some(format!("连不上服务器，已排队稍后重试：{detail}")),
            });
        }
    };

    let folder = state
        .messages
        .get_folder(&account_id, &folder_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("文件夹 {folder_id} 不存在"))?;

    // `push_flags` 内部会把**每一封**邮件的失败分好类；
    // 走到这里的错误都是「文件夹都没打开」这类**前置**失败——
    // 没有任何 STORE 命令被发出去，所以记成可重试是安全的。
    let submit_error = state
        .sync
        .push_flags(&mut session, &folder, now)
        .await
        .err()
        .map(|error| user_error(&error));

    // 4. 读回**这一条**操作的状态（`push_flags` 只给汇总报告）。
    let stored = operation_dao::get_operation(state.pool(), &operation.id)
        .await
        .map_err(|e| user_error(&e))?;

    let Some(stored) = stored else {
        return Ok(FlagIntentResult {
            queued: true,
            applied: false,
            state: "queued".to_string(),
            message: Some("已排队，等待提交给服务器".to_string()),
        });
    };

    // 还停在 `queued` 说明它压根没被处理过（提交阶段整体失败）→ 可重试。
    if stored.state == OperationState::Queued {
        let detail = submit_error
            .clone()
            .unwrap_or_else(|| "提交没有完成".to_string());
        let _ = operation_dao::mark_operation_retryable(
            state.pool(),
            &operation.id,
            "submit_failed",
            &detail,
            now,
        )
        .await;
        if let Some(updated) = operation_dao::get_operation(state.pool(), &operation.id)
            .await
            .map_err(|e| user_error(&e))?
        {
            return Ok(flag_result(&updated));
        }
    }

    Ok(flag_result(&stored))
}

/// 找一个账户的授权码：先看内存，再看平台钥匙串。
///
/// 两处都没有 → `None`（"这个账户还没保存授权码"是**状态**，不是错误）。
/// 移动/删除意图的回写结果。
#[derive(Debug, Clone, Serialize)]
pub struct TransferIntentResult {
    /// 意图已落库进本地操作队列。
    pub queued: bool,
    /// 服务器已回读确认生效的封数（移动 + 删除）。
    pub applied: usize,
    /// 失败但可重试几封。
    pub retryable: usize,
    /// 失败且不该重试几封。
    pub failed: usize,
    /// 不知道服务器执行了没几封。
    pub uncertain: usize,
    /// 给用户看的中文说明。
    pub message: Option<String>,
}

/// 把一批邮件移进另一个文件夹。
///
/// ## 链路
///
/// 1. 意图落库（`mail_operation`，`kind = move`，带目标文件夹）；
/// 2. 能连上就立刻回写并**回读确认**；连不上就留在队列里，
///    由同步轮次（[`xremail_sync::sync_account`]）自动排空。
///
/// 目标文件夹传**本地 `folder_id`**，不是邮箱名：邮箱名会被服务器改写。
#[tauri::command]
pub async fn move_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_id: String,
    uids: Vec<i64>,
    destination_folder_id: String,
) -> Result<TransferIntentResult, String> {
    state
        .messages
        .move_intent(&account_id, &folder_id, &uids, &destination_folder_id)
        .await
        .map_err(|e| user_error(&e))?;

    flush_transfer_intents(&state, &account_id, &folder_id).await
}

/// 删除一批邮件。
///
/// 有垃圾箱就挪进去（等于移动，能撤销），没有就就地标删——
/// 具体走哪条路由由同步层按服务器能力决定（见 `xremail_sync::push_transfers`）。
#[tauri::command]
pub async fn delete_messages(
    state: State<'_, AppState>,
    account_id: String,
    folder_id: String,
    uids: Vec<i64>,
) -> Result<TransferIntentResult, String> {
    state
        .messages
        .delete_intent(&account_id, &folder_id, &uids)
        .await
        .map_err(|e| user_error(&e))?;

    flush_transfer_intents(&state, &account_id, &folder_id).await
}

/// 立刻排空一个文件夹的移动/删除队列，并把结果翻成人话。
///
/// 连不上不算错：意图已经在队列里，同步轮次会接着试。
async fn flush_transfer_intents(
    app: &AppState,
    account_id: &str,
    folder_id: &str,
) -> Result<TransferIntentResult, String> {
    let queued = |message: &str| TransferIntentResult {
        queued: true,
        applied: 0,
        retryable: 0,
        failed: 0,
        uncertain: 0,
        message: Some(message.to_string()),
    };

    let account = app
        .accounts
        .get(account_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("账户 {account_id} 不存在"))?;

    // 没凭据就静静排队——不是错误，用户可能还没填授权码。
    let Some(credential) = resolve_credential(app, &account).await? else {
        return Ok(queued("已排队：保存授权码后会自动提交给服务器"));
    };

    let folder = app
        .messages
        .get_folder(account_id, folder_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("文件夹 {folder_id} 不存在"))?;

    let mut session = match app.sync.connect(&account, &credential).await {
        Ok(session) => session,
        Err(error) => {
            return Ok(queued(&format!(
                "连不上服务器，已排队稍后重试：{}",
                user_error(&error)
            )));
        }
    };

    let now = current_time_ms();
    let report = app
        .sync
        .push_transfers(&mut session, &folder, now)
        .await
        .map_err(|e| user_error(&e))?;

    let applied = report.moved + report.deleted;
    let mut parts = vec![format!("{applied} 封已提交")];
    if report.noop > 0 {
        parts.push(format!("其中 {} 封服务器上本来就不在了", report.noop));
    }
    if report.retryable > 0 {
        parts.push(format!("{} 封可重试", report.retryable));
    }
    if report.uncertain > 0 {
        parts.push(format!("{} 封结果未确认", report.uncertain));
    }
    if report.failed > 0 {
        parts.push(format!("{} 封失败", report.failed));
    }

    Ok(TransferIntentResult {
        queued: false,
        applied,
        retryable: report.retryable,
        failed: report.failed,
        uncertain: report.uncertain,
        message: Some(parts.join("，")),
    })
}

async fn resolve_credential(
    state: &AppState,
    account: &Account,
) -> Result<Option<SecretValue>, String> {
    if let Some(credential) = state.credential(&account.id) {
        return Ok(Some(credential));
    }

    CredentialService::new(state.accounts.clone())
        .resolve_credential(account)
        .await
        .map_err(|e| user_error(&e))
}

/// 存草稿（新建或更新）。
///
/// 前端每 800ms 防抖存一次，所以这条命令**必须幂等**：
/// 同一个 id 反复存，只会把内容覆盖成最新的一份。
///
/// 已经发出去或结果未确认的信**不能**再当草稿改——那时用户看到的
/// 和实际发出去的就不是同一封了（见 [ARCHITECTURE §5](../../../../docs/ARCHITECTURE.md)）。
#[tauri::command]
pub async fn save_draft(state: State<'_, AppState>, draft: DraftInput) -> Result<DraftDto, String> {
    state
        .compose
        .save_draft(&draft)
        .await
        .map_err(|e| user_error(&e))
}

/// 搜索本地缓存里的邮件。
///
/// 搜索只落在**本地缓存**上（不查服务器）：163 的 `UID SEARCH` 只认 `ALL`，
/// 其它关键字静默失效（已知风险 R23）。
///
/// `limit` 有上限（默认 50，最多 200）——避免一个宽泛的查询把整库拖出来。
#[tauri::command]
pub async fn search_messages(
    state: State<'_, AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<SearchHitDto>, String> {
    let limit = limit.unwrap_or(50).clamp(1, 200);
    state
        .messages
        .search(&query, limit)
        .await
        .map_err(|e| user_error(&e))
}

/// 列出某账户的草稿（给「接着上次那封改」用）。
///
/// 只列 `draft` 状态的——排队中 / 结果未确认的信内容已冻结，
/// 不能当草稿改。
#[tauri::command]
pub async fn list_drafts(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<DraftDto>, String> {
    state
        .compose
        .list_drafts(&account_id)
        .await
        .map_err(|e| user_error(&e))
}

/// 取一份草稿的全文（正文与附件）。
#[tauri::command]
pub async fn get_draft(state: State<'_, AppState>, id: String) -> Result<DraftDetail, String> {
    state
        .compose
        .get_draft(&id)
        .await
        .map_err(|e| user_error(&e))
}

/// 把「结果未确认」的信撤回来继续改。
///
/// **必须由用户确认**（前端弹二次确认）：
/// 万一服务器其实已经收下了这封信，改完再发就变成两封。
#[tauri::command]
pub async fn withdraw_draft(state: State<'_, AppState>, entry_id: String) -> Result<bool, String> {
    state
        .compose
        .withdraw_draft(&entry_id)
        .await
        .map_err(|e| user_error(&e))
}

/// 发送一封信。
///
/// 返回值把三种结局分开说（`sent` / `uncertain` / `failed`），
/// 不要在 Command 层把它们揉成一句「发送失败」——
/// 前端要靠这个区分来决定能不能重发（见 `types.ts` 的 `SendResult`）。
#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    draft: DraftInput,
) -> Result<SendResult, String> {
    let account = state
        .accounts
        .get(&draft.account_id)
        .await
        .map_err(|e| user_error(&e))?
        .ok_or_else(|| format!("账户 {} 不存在", draft.account_id))?;

    // 凭据：内存里的优先，没有再去平台钥匙串取。
    let Some(credential) = resolve_credential(&state, &account).await? else {
        return Err("还没有这个账户的授权码，请先保存后再发送".to_string());
    };

    state
        .compose
        .send(&draft, &credential)
        .await
        .map_err(|e| user_error(&e))
}

/// 把一条操作的状态翻译成给用户看的结果。
///
/// `applied` 只在**服务器回读确认过**之后才为真：
/// 服务器回了 `OK` 不等于改动生效了，得再读一次确认（见 ARCHITECTURE §6）。
fn flag_result(operation: &MailOperation) -> FlagIntentResult {
    let detail = operation
        .error_message
        .as_deref()
        .filter(|message| !message.is_empty());

    let message = match operation.state {
        OperationState::Succeeded => "服务器已确认生效".to_string(),
        OperationState::Queued => "已排队，等待提交给服务器".to_string(),
        OperationState::Running => "正在提交给服务器".to_string(),
        OperationState::Retryable => {
            format!("这次修改没能提交，已排队稍后重试{}", with_detail(detail))
        }
        OperationState::Failed => format!("这次修改没能生效{}", with_detail(detail)),
        OperationState::Uncertain => format!(
            "连接中断，不知道服务器执行了没；已记账，不会重复执行{}",
            with_detail(detail)
        ),
    };

    FlagIntentResult {
        queued: true,
        applied: matches!(operation.state, OperationState::Succeeded),
        state: operation_state_text(operation.state),
        message: Some(message),
    }
}

/// 状态的线格式：与 `mail_operation.state` 列的取值一致。
fn operation_state_text(state: OperationState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "queued".to_string())
}

/// 把失败详情拼到说明后面。
fn with_detail(detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("：{detail}"),
        None => String::new(),
    }
}

/// 把领域错误翻译成给用户看的中文说明。
///
/// [`AppError`] 的 `Display` 带一段英文前缀（`storage error: …`），
/// 那是内部术语；用户要看到的是「哪里错了、怎么办」。
/// 这里按类别取出里面的说明，去掉前缀。
fn user_error(error: &AppError) -> String {
    match error {
        AppError::Invalid(detail) => detail.clone(),
        AppError::NotFound(detail) => detail.clone(),
        AppError::Storage(detail) => detail.clone(),
        AppError::Auth(detail) => format!("认证失败：{detail}"),
        AppError::Protocol(detail) => format!("服务器通信失败：{detail}"),
        AppError::Network(detail) => format!("网络错误：{detail}"),
        AppError::Io(detail) => format!("读写失败：{detail}"),
        AppError::Crypto(detail) => format!("加密失败：{detail}"),
        AppError::Unsupported(detail) => format!("当前版本不支持这项能力：{detail}"),
        other => other.to_string(),
    }
}
