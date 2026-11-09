//! 「HTML-only 邮件也得有可搜的纯文本」这条不变量。
//!
//! 背景：真实邮箱发出来的邮件**大多是 HTML-only**（实测 163：
//! `text/plain` 部分为空，只有 `text/html`）。
//! 如果正文只认 `text_plain`，这类邮件**一个字都搜不到**——
//! 搜索等于只搜标题，邮件正文等于读不到。
//!
//! 覆盖的行为：
//! * 写入时缺纯文本 → 从 HTML **派生**一份；
//! * 已经有纯文本 → **不动**它（不覆盖）；
//! * 派生出来的文本**进了全文索引**（能搜到）；
//! * 升级前入库的老数据 → 启动时的补写能**补上**。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, Address, AuthKind, Folder, Message, MessageBody, MessageFlags, ProviderKind,
    ServerConfig, SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::{account, folder, message, search};
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
        subject: Some(format!("主题 {id}")),
        snippet: None,
        from: vec![Address::new("alice@example.com")],
        to: vec![Address::new("bob@example.com")],
        cc: vec![],
        bcc: vec![],
        date_sent: Some(1_700_000_000_000 + uid),
        internal_date: Some(1_700_000_000_000),
        flags: MessageFlags::empty(),
        size: None,
        has_attachment: false,
    }
}

/// 造一个能装下邮件的库（账户 + 文件夹）。
async fn seed_skeleton(pool: &SqlitePool) {
    account::upsert_account(pool, &sample_account("acct"))
        .await
        .expect("写账户失败");
    folder::upsert_folder(pool, &sample_folder("fld", "acct"))
        .await
        .expect("写文件夹失败");
}

fn html_body(message_id: &str, html: &str) -> MessageBody {
    MessageBody {
        message_id: message_id.to_string(),
        text_plain: None,
        html_raw: Some(html.to_string()),
        charset: Some("utf-8".to_string()),
    }
}

/// 直接写库，模拟「派生规则上线之前」入库的老数据。
///
/// 这里用运行时 API 而不是编译期宏：这是一条**测试专用**的 SQL，
/// 不值得为它生成离线查询数据。
async fn insert_legacy_body(pool: &SqlitePool, message_id: &str, html: &str) {
    sqlx::query(
        r#"
        INSERT INTO message_body (message_id, text_plain, html_raw, charset)
        VALUES (?, NULL, ?, 'utf-8')
        "#,
    )
    .bind(message_id)
    .bind(html)
    .execute(pool)
    .await
    .expect("写老正文失败");
}

#[tokio::test]
async fn html_only_body_gets_derived_plain_text() {
    let pool = fresh_pool().await;
    seed_skeleton(&pool).await;

    let mut msg = sample_message("m1", "acct", "fld", 1);
    msg.subject = Some("季度预算评审".to_string());
    message::upsert_message(&pool, &msg)
        .await
        .expect("写邮件失败");

    message::upsert_message_body(
        &pool,
        &html_body(
            "m1",
            "<html><body><p>本周的<span style=\"color:red\">重要</span>决定</p></body></html>",
        ),
    )
    .await
    .expect("写正文失败");

    let body = message::get_message_body(&pool, "m1")
        .await
        .expect("读正文失败")
        .expect("正文不该丢");

    let text = body.text_plain.expect("应该派生出纯文本");
    assert!(
        text.contains("重要"),
        "派生文本里应该有正文内容，实际是：{text}"
    );
    assert!(
        text.contains("决定"),
        "派生文本里应该有正文内容，实际是：{text}"
    );
    assert!(
        !text.contains("style"),
        "派生文本不该带样式，实际是：{text}"
    );
}

#[tokio::test]
async fn existing_plain_text_is_not_overwritten() {
    let pool = fresh_pool().await;
    seed_skeleton(&pool).await;

    let msg = sample_message("m1", "acct", "fld", 1);
    message::upsert_message(&pool, &msg)
        .await
        .expect("写邮件失败");

    message::upsert_message_body(
        &pool,
        &MessageBody {
            message_id: "m1".to_string(),
            text_plain: Some("原作者写的纯文本".to_string()),
            html_raw: Some("<p>HTML 里的另一份内容</p>".to_string()),
            charset: Some("utf-8".to_string()),
        },
    )
    .await
    .expect("写正文失败");

    let body = message::get_message_body(&pool, "m1")
        .await
        .expect("读正文失败")
        .expect("正文不该丢");

    assert_eq!(
        body.text_plain.as_deref(),
        Some("原作者写的纯文本"),
        "已有的纯文本不该被 HTML 覆盖"
    );
}

#[tokio::test]
async fn derived_text_makes_the_message_searchable() {
    let pool = fresh_pool().await;
    seed_skeleton(&pool).await;

    let mut msg = sample_message("m1", "acct", "fld", 1);
    msg.subject = Some("一封只有 HTML 的邮件".to_string());
    message::upsert_message(&pool, &msg)
        .await
        .expect("写邮件失败");

    message::upsert_message_body(
        &pool,
        &html_body("m1", "<html><body><p>季度预算评审结论</p></body></html>"),
    )
    .await
    .expect("写正文失败");

    let hits = search::search(&pool, "预算评审", 10)
        .await
        .expect("搜索失败");

    assert_eq!(
        hits.len(),
        1,
        "HTML-only 邮件的正文应该能被搜到，实际命中：{hits:?}"
    );
}

#[tokio::test]
async fn backfill_fills_rows_written_before_the_rule_existed() {
    let pool = fresh_pool().await;
    seed_skeleton(&pool).await;

    let mut msg = sample_message("m1", "acct", "fld", 1);
    msg.subject = Some("老邮件".to_string());
    message::upsert_message(&pool, &msg)
        .await
        .expect("写邮件失败");

    // 模拟老数据：纯文本是空的，只有 HTML。
    insert_legacy_body(
        &pool,
        "m1",
        "<html><body><p>补写之前的正文</p></body></html>",
    )
    .await;

    // 补写之前搜不到。
    let before = search::search(&pool, "补写之前", 10)
        .await
        .expect("搜索失败");
    assert!(before.is_empty(), "补写前不该搜到");

    let patched = message::backfill_derived_text(&pool)
        .await
        .expect("补写失败");
    assert_eq!(patched, 1, "应该补写 1 封");

    let body = message::get_message_body(&pool, "m1")
        .await
        .expect("读正文失败")
        .expect("正文不该丢");
    assert!(
        body.text_plain
            .as_deref()
            .is_some_and(|t| t.contains("补写之前")),
        "补写后应该有纯文本"
    );

    // 索引也跟着补上。
    let after = search::search(&pool, "补写之前", 10)
        .await
        .expect("搜索失败");
    assert_eq!(after.len(), 1, "补写后应该搜得到");
}

#[tokio::test]
async fn backfill_skips_rows_that_already_have_text() {
    let pool = fresh_pool().await;
    seed_skeleton(&pool).await;

    let msg = sample_message("m1", "acct", "fld", 1);
    message::upsert_message(&pool, &msg)
        .await
        .expect("写邮件失败");

    message::upsert_message_body(
        &pool,
        &MessageBody {
            message_id: "m1".to_string(),
            text_plain: Some("已经有纯文本了".to_string()),
            html_raw: Some("<p>HTML</p>".to_string()),
            charset: Some("utf-8".to_string()),
        },
    )
    .await
    .expect("写正文失败");

    let patched = message::backfill_derived_text(&pool)
        .await
        .expect("补写失败");
    assert_eq!(patched, 0, "有纯文本的行不该被补写");
}
