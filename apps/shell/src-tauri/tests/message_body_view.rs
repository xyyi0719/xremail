//! 邮件正文「给渲染层的那一份」的集成测试。
//!
//! 锁住一条不变量：**原始 HTML 不出后端**。
//!
//! 数据库里存的是 `html_raw`（原始邮件正文），
//! 出后端的是 `html_safe`（后端消毒过的）。
//! 渲染层拿到的永远只有消毒过的版本——
//! 前端那份消毒只管编辑器里的内容，管不到「打开一封收到的邮件」。
//!
//! 不依赖真实邮箱、凭据或网络。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, Address, AuthKind, Folder, Message, MessageBody, MessageFlags, ProviderKind,
    ServerConfig, SmtpAuth, SyncMode, TlsMode,
};
use xremail_shell_lib::service::message::{MessageBodyView, MessageService};
use xremail_store::dao::{account, folder, message};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};

async fn fresh_pool() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("应用迁移失败");
    pool
}

fn sample_account(id: &str) -> Account {
    Account {
        id: id.to_string(),
        email: format!("{id}@example.com"),
        provider: ProviderKind::Netease163,
        display_name: None,
        color: None,
        auth_kind: AuthKind::Password,
        server: ServerConfig {
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            imap_tls: TlsMode::Implicit,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 465,
            smtp_tls: TlsMode::Implicit,
            smtp_auth: SmtpAuth::Login,
        },
        username: format!("{id}@example.com"),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

fn sample_folder(id: &str, account_id: &str) -> Folder {
    Folder {
        id: id.to_string(),
        account_id: account_id.to_string(),
        remote_name: "INBOX".to_string(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".to_string()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        uidvalidity: Some(42),
        role: None,
    }
}

fn sample_message(id: &str, account_id: &str, folder_id: &str, uid: i64) -> Message {
    Message {
        id: id.to_string(),
        account_id: account_id.to_string(),
        folder_id: folder_id.to_string(),
        uid,
        uidvalidity: 42,
        message_id: Some(format!("<{id}@example.com>")),
        subject: Some("一封邮件".to_string()),
        snippet: None,
        from: vec![Address::new("alice@example.com")],
        to: vec![Address::new("bob@example.com")],
        cc: vec![],
        bcc: vec![],
        date_sent: Some(1_700_000_000_000),
        internal_date: Some(1_700_000_000_000),
        flags: MessageFlags::empty(),
        size: None,
        has_attachment: false,
    }
}

/// 造一个「账户 + 文件夹 + 一封带恶意 HTML 的邮件」。
async fn seed(pool: &SqlitePool) {
    account::upsert_account(pool, &sample_account("acct"))
        .await
        .expect("写账户失败");
    folder::upsert_folder(pool, &sample_folder("fld", "acct"))
        .await
        .expect("写文件夹失败");
    message::upsert_message(pool, &sample_message("m1", "acct", "fld", 7))
        .await
        .expect("写邮件失败");
}

const NASTY_HTML: &str = concat!(
    r#"<html><body><p onclick="steal()">正文</p>"#,
    r#"<script>fetch('https://evil.example')</script>"#,
    r#"<a href="javascript:alert(1)">点我</a>"#,
    r#"<img src="https://tracker.example/pixel.gif">"#,
    r#"</body></html>"#
);

#[tokio::test]
async fn body_view_returns_sanitized_html() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    message::upsert_message_body(
        &pool,
        &MessageBody {
            message_id: "m1".to_string(),
            text_plain: None,
            html_raw: Some(NASTY_HTML.to_string()),
            charset: Some("utf-8".to_string()),
        },
    )
    .await
    .expect("写正文失败");

    let service = MessageService::new(pool);
    let view = service
        .get_body_view("acct", "fld", 7)
        .await
        .expect("读正文失败")
        .expect("正文不该丢");

    let html = view.html_safe.expect("应该有消毒过的 HTML");

    assert!(html.contains("正文"), "正文应该保留：{html}");
    assert!(!html.contains("script"), "脚本必须删掉：{html}");
    assert!(!html.contains("onclick"), "事件处理器必须删掉：{html}");
    assert!(!html.contains("javascript:"), "危险协议必须删掉：{html}");
    assert!(
        !html.contains("tracker.example"),
        "远程图片必须删掉：{html}"
    );
}

#[tokio::test]
async fn body_view_derives_plain_text_for_html_only_mail() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    message::upsert_message_body(
        &pool,
        &MessageBody {
            message_id: "m1".to_string(),
            text_plain: None,
            html_raw: Some("<html><body><p>季度预算评审结论</p></body></html>".to_string()),
            charset: Some("utf-8".to_string()),
        },
    )
    .await
    .expect("写正文失败");

    let service = MessageService::new(pool);
    let view = service
        .get_body_view("acct", "fld", 7)
        .await
        .expect("读正文失败")
        .expect("正文不该丢");

    assert!(
        view.text_plain
            .as_deref()
            .is_some_and(|text| text.contains("季度预算评审结论")),
        "HTML-only 的邮件也该有纯文本：{:?}",
        view.text_plain
    );
}

#[tokio::test]
async fn body_view_of_a_missing_mail_is_none() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let service = MessageService::new(pool);
    let view = service
        .get_body_view("acct", "fld", 999)
        .await
        .expect("读正文失败");
    assert!(view.is_none(), "没有的邮件该是 None");
}

#[tokio::test]
async fn body_view_serialization_has_no_raw_html_field() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    message::upsert_message_body(
        &pool,
        &MessageBody {
            message_id: "m1".to_string(),
            text_plain: Some("纯文本".to_string()),
            html_raw: Some(NASTY_HTML.to_string()),
            charset: Some("utf-8".to_string()),
        },
    )
    .await
    .expect("写正文失败");

    let service = MessageService::new(pool);
    let view = service
        .get_body_view("acct", "fld", 7)
        .await
        .expect("读正文失败")
        .expect("正文不该丢");

    let json = serde_json::to_value(&view).expect("序列化失败");
    let keys: Vec<&str> = json
        .as_object()
        .expect("该是对象")
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        keys.contains(&"htmlSafe"),
        "该有 htmlSafe 字段，实际：{keys:?}"
    );
    assert!(
        !keys.contains(&"htmlRaw"),
        "不该有 htmlRaw 字段——原始 HTML 不出后端，实际：{keys:?}"
    );
    assert!(
        !json.to_string().contains("script"),
        "序列化结果里不该有脚本：{json}"
    );
}

#[test]
fn body_view_from_domain_sanitizes_without_a_database() {
    let view = MessageBodyView::from_domain(MessageBody {
        message_id: "m1".to_string(),
        text_plain: None,
        html_raw: Some("<p onclick=\"x()\">hi</p><script>alert(1)</script>".to_string()),
        charset: None,
    });

    let html = view.html_safe.expect("应该有 HTML");
    assert!(html.contains("hi"), "正文应该保留：{html}");
    assert!(!html.contains("onclick"), "{html}");
    assert!(!html.contains("script"), "{html}");
}
