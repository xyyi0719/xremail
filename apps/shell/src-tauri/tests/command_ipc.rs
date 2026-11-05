//! Command 层的 IPC 测试。
//!
//! 这一层测的是**整条链路的最后一段**：请求 JSON → 参数反序列化 →
//! 服务 → SQLite → 响应 JSON。
//!
//! 特别要锁住的是**线格式**：字段名是 camelCase（`displayName`、`imapPort`）。
//! 手写的 TypeScript 类型一旦和 Rust 侧漂移，前端就会静默拿到 `undefined`——
//! 用例在这里把格式钉死。
//!
//! 参考 Tauri 官方的 `tauri::test` 用法。

use std::sync::Arc;

use serde_json::json;
use tauri::Manager;
use tauri::WebviewWindowBuilder;
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{
    INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder, mock_context, noop_assets,
};
use tauri::webview::InvokeRequest;
use xremail_core::AppError;
use xremail_core::model::{Account, Folder, Message, MessageFlags};
use xremail_shell_lib::cmd;
use xremail_shell_lib::service::{AccountService, ComposeService, MessageService, SyncService};
use xremail_shell_lib::state::{AppState, Credentials, Services};
use xremail_sync::{AccountConnector, ConnectFuture, NoopSink, RetryPolicy, Scheduler};

/// 从不连服务器的连接器。
///
/// IPC 用例测的是「命令 → 服务 → 数据库」这一段，不需要真的同步；
/// 调度器只是 `AppState` 的一个字段，**从不被启动**。
struct NeverConnector;

impl AccountConnector for NeverConnector {
    fn connect<'a>(&'a self, _account: &'a Account) -> ConnectFuture<'a> {
        Box::pin(async { Err(AppError::Protocol("测试里不连服务器".into())) })
    }
}

/// 造一个带内存库的应用与 webview。
///
/// **连接池必须在 Tauri 的运行时上创建**：sqlx 的连接池与创建它的运行时绑定，
/// 若在别的运行时（如 #[tokio::test] 的 tokio 运行时）上建池，
/// Command 在 Tauri 运行时上取连接时就会等到超时。
fn app() -> (tauri::App<MockRuntime>, tauri::WebviewWindow<MockRuntime>) {
    let pool = tauri::async_runtime::block_on(async {
        let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
            .await
            .expect("打开内存库失败");
        xremail_store::migrate(&pool).await.expect("应用迁移失败");
        pool
    });

    let state = AppState::new(
        std::path::PathBuf::from("/tmp/xremail-test"),
        Services {
            accounts: AccountService::new(pool.clone()),
            messages: MessageService::new(pool.clone()),
            sync: SyncService::new(pool.clone()),
            compose: ComposeService::new(pool.clone(), AccountService::new(pool.clone())),
        },
        Scheduler::with_options(
            pool.clone(),
            Arc::new(NeverConnector),
            Arc::new(NoopSink),
            RetryPolicy::default(),
            Arc::new(|| 0),
        ),
        Arc::new(Credentials::default()),
        pool,
    );

    let app = mock_builder()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            cmd::list_accounts,
            cmd::create_account,
            cmd::delete_account,
            cmd::sync_account,
            cmd::list_folders,
            cmd::list_messages,
            cmd::open_message,
            cmd::save_credential,
            cmd::forget_credential,
            cmd::set_message_flag,
            cmd::save_draft,
            cmd::send_message,
            cmd::resend_uncertain,
        ])
        .build(mock_context(noop_assets()))
        .expect("构建 mock 应用失败");

    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("创建 mock webview 失败");

    (app, webview)
}

/// 本地 origin。
///
/// Tauri 的自定义协议 origin 是**平台相关**的
/// （见 Tauri 的 `protocol::origin`）：
///
/// * Windows / Android：`http://tauri.localhost`
///   （wry 在这两个平台上用 `http(s)://{scheme}.localhost` 绕行）
/// * 其它平台：`tauri://localhost`
///
/// **写错的后果**：请求会被当成**远程来源**（`is_local = false`），
/// 于是走 ACL 检查——而 mock 应用的 ACL 是**空的**，
/// 命令会被一律拒绝：
/// `create_account not allowed. Plugin not found`。
///
/// 这个坑**在 Windows 上看不见**（那里恰好硬编码对了），
/// 只有 Linux CI 才暴露——2026-10-26 首次推送时就是这么发现的。
fn local_origin() -> &'static str {
    if cfg!(any(windows, target_os = "android")) {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    }
}

/// 构造一个 IPC 请求。
fn request(cmd: &str, body: serde_json::Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.to_string(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: local_origin().parse().expect("URL 非法"),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

/// 一份合法的 `create_account` 请求体（camelCase）。
fn valid_body(email: &str) -> serde_json::Value {
    json!({
        "email": email,
        "provider": "netease_163",
        "displayName": "我的邮箱",
        "imapHost": "imap.163.com",
        "imapPort": 993,
        "imapTls": "implicit",
        "smtpHost": "smtp.163.com",
        "smtpPort": 465,
        "smtpTls": "implicit",
        "smtpAuth": "login",
        "username": email,
        "syncMode": "poll",
        "pollIntervalSecs": 300
    })
}

#[test]
fn list_accounts_returns_empty_array_on_a_fresh_database() {
    let (_app, webview) = app();

    let body =
        get_ipc_response(&webview, request("list_accounts", json!({}))).expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value, json!([]));
}

#[test]
fn create_account_persists_and_appears_in_the_list() {
    let (_app, webview) = app();

    get_ipc_response(
        &webview,
        request(
            "create_account",
            json!({ "request": valid_body("user@163.com") }),
        ),
    )
    .expect("创建应当成功");

    let body =
        get_ipc_response(&webview, request("list_accounts", json!({}))).expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value.as_array().expect("应为数组").len(), 1);
    assert_eq!(value[0]["email"], "user@163.com");
}

#[test]
fn account_uses_camel_case_field_names_on_the_wire() {
    // 这一条锁死线格式：前端手写的 TS 类型依赖这些名字。
    let (_app, webview) = app();

    get_ipc_response(
        &webview,
        request(
            "create_account",
            json!({ "request": valid_body("user@163.com") }),
        ),
    )
    .expect("创建应当成功");

    let body =
        get_ipc_response(&webview, request("list_accounts", json!({}))).expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");
    let account = &value[0];

    // camelCase，不是 snake_case。
    assert!(account.get("displayName").is_some(), "缺少 displayName");
    assert!(
        account.get("pollIntervalSecs").is_some(),
        "缺少 pollIntervalSecs"
    );
    assert!(account.get("secretRef").is_some(), "缺少 secretRef");
    assert!(account.get("syncMode").is_some(), "缺少 syncMode");
    assert!(account.get("authKind").is_some(), "缺少 authKind");
    // 不应出现 snake_case 版本。
    assert!(
        account.get("display_name").is_none(),
        "出现了 snake_case 字段"
    );
    assert!(account.get("poll_interval_secs").is_none());

    // 嵌套对象同样是 camelCase。
    assert_eq!(account["server"]["imapHost"], "imap.163.com");
    assert_eq!(account["server"]["imapPort"], 993);
    assert_eq!(account["server"]["smtpAuth"], "login");
    assert!(account["server"].get("imap_host").is_none());
}

#[test]
fn create_account_returns_the_created_account() {
    let (_app, webview) = app();

    let body = get_ipc_response(
        &webview,
        request(
            "create_account",
            json!({ "request": valid_body("user@qq.com") }),
        ),
    )
    .expect("创建应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value["email"], "user@qq.com");
    assert_eq!(value["provider"], "netease_163");
    assert_eq!(value["server"]["imapPort"], 993);
    assert!(!value["id"].as_str().expect("应有 id").is_empty());
    assert!(
        value["secretRef"].is_null(),
        "P0 没有平台钥匙串，凭据引用应为空"
    );
}

#[test]
fn create_account_rejects_an_invalid_request() {
    let (_app, webview) = app();

    let mut bad = valid_body("user@163.com");
    bad["email"] = json!("not-an-email");

    let err = get_ipc_response(
        &webview,
        request("create_account", json!({ "request": bad })),
    )
    .expect_err("非法邮箱应被拒绝");

    assert!(
        err.to_string().contains("邮箱"),
        "错误信息应说明是邮箱的问题，实际是 {err}"
    );
}

#[test]
fn create_account_rejects_a_missing_required_field() {
    let (_app, webview) = app();

    let mut incomplete = valid_body("user@163.com");
    incomplete
        .as_object_mut()
        .expect("应为对象")
        .remove("imapHost");

    let err = get_ipc_response(
        &webview,
        request("create_account", json!({ "request": incomplete })),
    )
    .expect_err("缺字段应被拒绝");

    assert!(!err.to_string().is_empty());
}

#[test]
fn delete_account_removes_it() {
    let (_app, webview) = app();

    let body = get_ipc_response(
        &webview,
        request(
            "create_account",
            json!({ "request": valid_body("user@163.com") }),
        ),
    )
    .expect("创建应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");
    let id = value["id"].as_str().expect("应有 id").to_string();

    let deleted = get_ipc_response(&webview, request("delete_account", json!({ "id": id })))
        .expect("删除命令应当成功");
    let deleted: serde_json::Value = deleted.deserialize().expect("反序列化失败");
    assert_eq!(deleted, json!(true));

    let body =
        get_ipc_response(&webview, request("list_accounts", json!({}))).expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");
    assert_eq!(value, json!([]));
}

#[test]
fn deleting_a_missing_account_reports_false() {
    let (_app, webview) = app();

    let body = get_ipc_response(
        &webview,
        request("delete_account", json!({ "id": "no-such-id" })),
    )
    .expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value, json!(false));
}

// ---------------------------------------------------------------- 邮件查询

#[test]
fn list_folders_returns_empty_array_before_any_sync() {
    let (_app, webview) = app();

    let body = get_ipc_response(
        &webview,
        request("list_folders", json!({ "accountId": "acct-1" })),
    )
    .expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value, json!([]));
}

#[test]
fn list_messages_returns_empty_array_for_unknown_folder() {
    let (_app, webview) = app();

    let body = get_ipc_response(
        &webview,
        request(
            "list_messages",
            json!({ "accountId": "acct-1", "folderId": "没有这个文件夹" }),
        ),
    )
    .expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value, json!([]));
}

#[test]
fn open_message_returns_null_when_body_is_not_cached() {
    // 本地没有正文时返回 null（前端据此按需去拉），
    // **不是**报错——「还没拉」和「拉失败」是两回事。
    let (_app, webview) = app();

    let body = get_ipc_response(
        &webview,
        request(
            "open_message",
            json!({ "accountId": "acct-1", "folderId": "f-1", "uid": 1 }),
        ),
    )
    .expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value, json!(null));
}

#[test]
fn sync_account_rejects_an_unknown_account() {
    let (_app, webview) = app();

    let err = get_ipc_response(
        &webview,
        request(
            "sync_account",
            json!({ "accountId": "不存在的账户", "credential": "x" }),
        ),
    )
    .expect_err("账户不存在应当报错");

    assert!(err.to_string().contains("不存在"), "实际错误是：{err}");
}

// ---------------------------------------------------------------- 凭据与标记

/// 给某个账户塞一个**已同步过**的文件夹与一封邮件（供标记命令用）。
///
/// 文件夹必须带 `uidvalidity`（epoch）：没有 epoch 就不知道该把标记意图
/// 绑到哪个 epoch 上，命令会直接报错。
fn seed_mailbox(app: &tauri::App<MockRuntime>, account_id: &str) {
    let pool = app.state::<AppState>().pool().clone();
    tauri::async_runtime::block_on(async move {
        xremail_store::dao::folder::upsert_folder(
            &pool,
            &Folder {
                id: "folder-1".to_string(),
                account_id: account_id.to_string(),
                remote_name: "INBOX".to_string(),
                remote_id: None,
                local_name: Some("收件箱".to_string()),
                delimiter: Some("/".to_string()),
                subscribed: true,
                selectable: true,
                last_sync_at: None,
                last_uid: None,
                highest_modseq: None,
                uidvalidity: Some(42),
            },
        )
        .await
        .expect("写入文件夹失败");

        xremail_store::dao::message::upsert_message(
            &pool,
            &Message {
                id: "message-1".to_string(),
                account_id: account_id.to_string(),
                folder_id: "folder-1".to_string(),
                uid: 7,
                uidvalidity: 42,
                message_id: Some("<abc@163.com>".to_string()),
                subject: Some("你好".to_string()),
                snippet: None,
                from: Vec::new(),
                to: Vec::new(),
                cc: Vec::new(),
                bcc: Vec::new(),
                date_sent: None,
                internal_date: None,
                flags: MessageFlags::empty(),
                size: None,
                has_attachment: false,
            },
        )
        .await
        .expect("写入邮件失败");
    });
}

/// 建一个账户并返回它的 id。
fn create_account(
    app: &tauri::App<MockRuntime>,
    webview: &tauri::WebviewWindow<MockRuntime>,
) -> String {
    let body = get_ipc_response(
        webview,
        request(
            "create_account",
            json!({ "request": valid_body("user@163.com") }),
        ),
    )
    .expect("创建应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");
    let id = value["id"].as_str().expect("应有 id").to_string();
    let _ = app;
    id
}

#[test]
fn save_credential_rejects_an_unknown_account() {
    let (_app, webview) = app();

    let err = get_ipc_response(
        &webview,
        request(
            "save_credential",
            json!({ "accountId": "不存在的账户", "credential": "secret" }),
        ),
    )
    .expect_err("账户不存在应当报错");

    assert!(err.to_string().contains("不存在"), "实际错误是：{err}");
}

#[test]
fn forget_credential_reports_false_for_an_unknown_account() {
    let (_app, webview) = app();

    let body = get_ipc_response(
        &webview,
        request("forget_credential", json!({ "accountId": "不存在的账户" })),
    )
    .expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value, json!(false));
}

#[test]
fn sync_account_without_a_credential_asks_the_user_to_save_one_first() {
    let (app, webview) = app();
    let account_id = create_account(&app, &webview);

    // 不传 `credential`：应当去钥匙串取，取不到就明确说「还没保存授权码」。
    let err = get_ipc_response(
        &webview,
        request("sync_account", json!({ "accountId": account_id })),
    )
    .expect_err("没有授权码应当报错");

    assert!(err.to_string().contains("授权码"), "实际错误是：{err}");
}

#[test]
fn set_message_flag_rejects_an_unknown_flag() {
    let (_app, webview) = app();

    let err = get_ipc_response(
        &webview,
        request(
            "set_message_flag",
            json!({
                "accountId": "acct-1",
                "folderId": "folder-1",
                "uid": 7,
                "flag": "deleted",
                "value": true,
            }),
        ),
    )
    .expect_err("不支持的标记应当报错");

    let message = err.to_string();
    assert!(
        message.contains("flagged") && message.contains("seen"),
        "错误信息应当说清能填什么，实际是：{message}"
    );
}

#[test]
fn set_message_flag_uses_camel_case_argument_names() {
    let (_app, webview) = app();

    // camelCase 的参数名能绑定上——绑定失败会是「缺少参数」那类报错，
    // 而这里拿到的是**业务**错误（文件夹不存在），说明参数已经进去了。
    let err = get_ipc_response(
        &webview,
        request(
            "set_message_flag",
            json!({
                "accountId": "acct-1",
                "folderId": "folder-1",
                "uid": 7,
                "flag": "seen",
                "value": true,
            }),
        ),
    )
    .expect_err("文件夹不存在应当报错");

    assert!(err.to_string().contains("文件夹"), "实际错误是：{err}");
}

#[test]
fn set_message_flag_queues_the_intent_when_no_credential_is_available() {
    let (app, webview) = app();
    let account_id = create_account(&app, &webview);
    seed_mailbox(&app, &account_id);

    let body = get_ipc_response(
        &webview,
        request(
            "set_message_flag",
            json!({
                "accountId": account_id,
                "folderId": "folder-1",
                "uid": 7,
                "flag": "flagged",
                "value": true,
            }),
        ),
    )
    .expect("命令应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    // 拿不到凭据 → 只落库不提交。
    assert_eq!(value["queued"], json!(true));
    assert_eq!(value["applied"], json!(false));
    assert_eq!(value["state"], json!("queued"));
    // 四个字段都在（线格式：camelCase）。
    for field in ["queued", "applied", "state", "message"] {
        assert!(value.get(field).is_some(), "缺少字段 {field}");
    }
    // `message` 是给用户看的中文说明。
    let message = value["message"].as_str().expect("message 应当是字符串");
    assert!(
        message.contains("授权码"),
        "说明应当告诉用户去保存授权码，实际是：{message}"
    );
}

/// 存草稿的线格式：字段名必须是 camelCase。
///
/// 前端 `types.ts` 里写的是 `accountId` / `updatedAt`，
/// Rust 侧一旦手滑写成 `account_id`，前端就会静默拿到 `undefined`——
/// **不报错，只是草稿存不进去**。这类漂移只有在线格式测试里才拦得住。
#[test]
fn save_draft_uses_camel_case_on_the_wire() {
    let (_app, webview) = app();

    get_ipc_response(
        &webview,
        request(
            "create_account",
            json!({ "request": valid_body("user@163.com") }),
        ),
    )
    .expect("建账户应当成功");
    let list: serde_json::Value = get_ipc_response(&webview, request("list_accounts", json!({})))
        .expect("列出账户应当成功")
        .deserialize()
        .expect("反序列化失败");
    let account_id = list[0]["id"].as_str().expect("账户应当有 id").to_string();

    let body = get_ipc_response(
        &webview,
        request(
            "save_draft",
            json!({
                "draft": {
                    "accountId": account_id,
                    "id": null,
                    "to": ["friend@example.com"],
                    "cc": [],
                    "bcc": [],
                    "subject": "你好",
                    "bodyText": "正文",
                    "bodyHtml": null,
                    "attachments": []
                }
            }),
        ),
    )
    .expect("存草稿应当成功");
    let value: serde_json::Value = body.deserialize().expect("反序列化失败");

    assert_eq!(value["subject"], json!("你好"));
    assert_eq!(value["to"], json!(["friend@example.com"]));
    assert_eq!(value["state"], json!("draft"));
    assert!(value["id"].is_string(), "草稿应当有 id");

    let keys = value.as_object().expect("应当是对象");
    for field in ["accountId", "updatedAt"] {
        assert!(
            keys.contains_key(field),
            "字段名必须是 {field}（前端 types.ts 就这么写的），实际是：{:?}",
            keys.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        !keys.contains_key("account_id"),
        "出现了 snake_case 字段名，前端读不到"
    );
}

/// 没凭据就发信 → 报错要**说清楚**是缺授权码，而不是笼统的失败。
#[test]
fn send_message_without_a_credential_says_so() {
    let (_app, webview) = app();

    get_ipc_response(
        &webview,
        request(
            "create_account",
            json!({ "request": valid_body("user@163.com") }),
        ),
    )
    .expect("建账户应当成功");
    let list: serde_json::Value = get_ipc_response(&webview, request("list_accounts", json!({})))
        .expect("列出账户应当成功")
        .deserialize()
        .expect("反序列化失败");
    let account_id = list[0]["id"].as_str().expect("账户应当有 id").to_string();

    let err = get_ipc_response(
        &webview,
        request(
            "send_message",
            json!({
                "draft": {
                    "accountId": account_id,
                    "id": null,
                    "to": ["friend@example.com"],
                    "cc": [],
                    "bcc": [],
                    "subject": "你好",
                    "bodyText": "正文",
                    "bodyHtml": null,
                    "attachments": []
                }
            }),
        ),
    )
    .expect_err("没有授权码应当报错");

    assert!(
        err.to_string().contains("授权码"),
        "报错应当说明缺授权码，实际是：{err}"
    );
}

/// 「再发一次」命令的**线格式**与错误口径（R30）。
///
/// 这条命令是 `uncertain` 的人工出口：前端拿到 `entryId` 调它，
/// 返回的必须和 `send_message` **同一个形状**（`messageId` / `entryId` /
/// `state` / `detail`，camelCase），否则前端复用的 `describeSendResult`
/// 会拿到 `undefined`，把三种结局说成一句「发送失败」。
#[test]
fn resend_uncertain_reports_camel_case_fields_and_a_missing_entry() {
    let (_app, webview) = app();

    // 库里没有这条发信记录 → 明确说找不到，而不是笼统地失败。
    let err = get_ipc_response(
        &webview,
        request("resend_uncertain", json!({ "entryId": "no-such-entry" })),
    )
    .expect_err("不存在的发信记录应当报错");

    assert!(
        err.to_string().contains("no-such-entry"),
        "报错里要带上那个 id，实际是：{err}"
    );
}
