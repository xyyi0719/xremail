//! `xremail-store` 的 T1 测试：迁移与事务。
//!
//! 测试预期来自**协议与存储语义**，不来自实现本身
//! （见 [AI 开发工作规范 §1.3](../../../docs/AI_DEVELOPMENT_GUIDE.md)）。
//!
//! 覆盖的不变量：
//! * 身份不变量 `(account_id, folder_id, uidvalidity, uid)`（ARCHITECTURE §4.1）。
//! * RFC 5322 Message-ID 仅作线索，不作唯一键。
//! * 外键删除策略逐表声明的行为（ARCHITECTURE §2、§5）。
//! * SQLite 外键约束**确实生效**（不显式 `PRAGMA foreign_keys = ON` 就全是摆设）。
//! * 约束冲突不是可重试错误（ARCHITECTURE §9）。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, Address, AuthKind, Folder, Message, MessageFlags, ProviderKind, ServerConfig,
    SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::{account, folder};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory, open_pool_path};

/// 建一个已应用迁移的内存库。
async fn fresh_pool() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("应用迁移失败");
    pool
}

/// 断言这是**约束冲突**（唯一键 / 外键 / CHECK），而不是别的错误。
///
/// 直接用 `sqlx` 宏的调用返回 `sqlx::Error`，不是 `AppError`，
/// 因此这里断言的是底层错误类别；DAO 层再验一遍 `is_retryable()`。
fn assert_constraint_violation(err: &sqlx::Error) {
    let sqlx::Error::Database(db) = err else {
        panic!("期望数据库约束冲突，实际是: {err}");
    };
    assert!(
        db.is_unique_violation() || db.is_foreign_key_violation() || db.is_check_violation(),
        "期望约束冲突，实际是: {err}"
    );
}

/// 造一个测试账户。
fn sample_account(id: &str) -> Account {
    Account {
        id: id.to_string(),
        email: format!("{id}@example.com"),
        provider: ProviderKind::Netease163,
        display_name: Some(format!("账户 {id}")),
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
        secret_ref: Some(format!("keychain::{id}")),
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

/// 造一个测试文件夹。
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

/// 造一封测试邮件。邮件 DAO 属于 P2 范围，这里只用它构造领域对象做往返校验。
#[allow(dead_code)]
fn sample_message(id: &str, account_id: &str, folder_id: &str, uid: i64) -> Message {
    Message {
        id: id.to_string(),
        account_id: account_id.to_string(),
        folder_id: folder_id.to_string(),
        uid,
        uidvalidity: 42,
        message_id: Some("<same@example.com>".to_string()),
        subject: Some("主题".to_string()),
        snippet: Some("摘要".to_string()),
        from: vec![Address {
            name: Some("发件人".to_string()),
            email: "from@example.com".to_string(),
        }],
        to: vec![Address::new("to@example.com")],
        cc: vec![],
        bcc: vec![],
        date_sent: Some(1_700_000_000_000),
        internal_date: Some(1_700_000_000_000),
        flags: MessageFlags::empty().with(MessageFlags::SEEN),
        size: Some(2048),
        has_attachment: false,
    }
}

// ===========================================================================
// 迁移
// ===========================================================================

#[tokio::test]
async fn migration_creates_all_expected_tables() {
    let pool = fresh_pool().await;

    let rows = sqlx::query!(
        r#"SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"#
    )
    .fetch_all(&pool)
    .await
    .expect("读取 sqlite_master 失败");

    let names: Vec<String> = rows.into_iter().filter_map(|r| r.name).collect();

    for expected in [
        "account",
        "attachment",
        "folder",
        "label",
        "local_secret",
        "mail_operation",
        "message",
        "message_body",
        "rule",
        "send_attachment",
        "send_queue",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "缺少表 {expected}：{names:?}"
        );
    }
}

#[tokio::test]
async fn migration_is_idempotent_and_preserves_data() {
    let pool = fresh_pool().await;

    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    // 再跑两次迁移：应无操作，且不破坏已有数据。
    migrate(&pool).await.expect("第二次迁移失败");
    migrate(&pool).await.expect("第三次迁移失败");

    let accounts = account::list_accounts(&pool).await.unwrap();
    assert_eq!(accounts.len(), 1, "重复迁移后数据不应丢失或重复");
    assert_eq!(accounts[0].id, "a1");
}

#[tokio::test]
async fn foreign_keys_are_actually_enforced() {
    let pool = fresh_pool().await;

    // 不存在的账户 id 插入文件夹：必须被外键拒绝。
    let err = folder::upsert_folder(&pool, &sample_folder("f1", "no-such-account"))
        .await
        .expect_err("外键未生效：插入了不存在的账户");

    // 约束冲突不是可重试错误：重试同一份输入不可能成功。
    assert!(!err.is_retryable(), "约束冲突被当成可重试错误");
    assert!(err.requires_user_action());
}

// ===========================================================================
// 身份不变量
// ===========================================================================

#[tokio::test]
async fn duplicate_identity_is_rejected_by_unique_constraint() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();

    // 直接插两封 uid 相同的邮件（绕过 DAO，邮件 DAO 属于 P2 范围）。
    // 注意：sqlx 的宏要求 SQL 是字面量，不能用变量传入。
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES (?, ?, ?, ?, ?)"#,
        "m1",
        "a1",
        "f1",
        7i64,
        42i64
    )
    .execute(&pool)
    .await
    .expect("首封邮件插入失败");

    let err = sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES (?, ?, ?, ?, ?)"#,
        "m2",
        "a1",
        "f1",
        7i64,
        42i64
    )
    .execute(&pool)
    .await
    .expect_err("唯一约束未生效：相同身份的两封邮件都插入了");

    assert_constraint_violation(&err);
}

#[tokio::test]
async fn same_uid_under_different_epoch_can_coexist() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();

    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES (?, ?, ?, ?, ?)"#,
        "m1",
        "a1",
        "f1",
        7i64,
        42i64
    )
    .execute(&pool)
    .await
    .expect("epoch 42 插入失败");
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES (?, ?, ?, ?, ?)"#,
        "m2",
        "a1",
        "f1",
        7i64,
        99i64
    )
    .execute(&pool)
    .await
    .expect("epoch 99 插入失败：不同 epoch 的相同 UID 应可共存");

    let count: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM message"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn rfc_message_id_is_only_a_clue_not_a_unique_key() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();

    // 同一个 RFC Message-ID，不同 uid：必须都能插入。
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity, message_id)
           VALUES (?, ?, ?, ?, ?, ?)"#,
        "m1",
        "a1",
        "f1",
        1i64,
        42i64,
        "<dup@example.com>"
    )
    .execute(&pool)
    .await
    .expect("第一封插入失败");
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity, message_id)
           VALUES (?, ?, ?, ?, ?, ?)"#,
        "m2",
        "a1",
        "f1",
        2i64,
        42i64,
        "<dup@example.com>"
    )
    .execute(&pool)
    .await
    .expect("Message-ID 重复不应被拒：它只是线索");
}

#[tokio::test]
async fn composite_foreign_key_rejects_mismatched_account() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    account::upsert_account(&pool, &sample_account("a2"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();

    // 邮件声称属于 a2，却挂在 a1 的文件夹下：必须被复合外键拒绝。
    let err = sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES ('m1', 'a2', 'f1', 1, 42)"#
    )
    .execute(&pool)
    .await
    .expect_err("复合外键未生效：账户与文件夹不匹配的邮件被插入了");

    assert_constraint_violation(&err);
}

// ===========================================================================
// CHECK 约束
// ===========================================================================

#[tokio::test]
async fn zero_uid_is_rejected() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();

    let err = sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES ('m1', 'a1', 'f1', 0, 42)"#
    )
    .execute(&pool)
    .await
    .expect_err("uid = 0 未被拒绝：IMAP 中 UID 0 是非法的");

    assert_constraint_violation(&err);
}

#[tokio::test]
async fn out_of_range_port_is_rejected() {
    let pool = fresh_pool().await;
    let mut acct = sample_account("a1");
    acct.server.imap_port = 0;

    let err = account::upsert_account(&pool, &acct)
        .await
        .expect_err("端口 0 未被拒绝");

    assert!(!err.is_retryable());
}

#[tokio::test]
async fn non_positive_poll_interval_is_rejected() {
    let pool = fresh_pool().await;
    let mut acct = sample_account("a1");
    acct.poll_interval_secs = 0;

    let err = account::upsert_account(&pool, &acct)
        .await
        .expect_err("轮询间隔 0 未被拒绝");

    assert!(!err.is_retryable());
}

// ===========================================================================
// 删除策略（逐表声明的行为）
// ===========================================================================

#[tokio::test]
async fn deleting_account_cascades_to_folders_and_messages() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES ('m1', 'a1', 'f1', 1, 42)"#
    )
    .execute(&pool)
    .await
    .unwrap();

    assert!(account::delete_account(&pool, "a1").await.unwrap());

    let folders: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM folder"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    let messages: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM message"#)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(folders, 0, "删账户后文件夹应级联删除");
    assert_eq!(messages, 0, "删账户后邮件应级联删除");
}

#[tokio::test]
async fn deleting_folder_cascades_to_its_messages() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES ('m1', 'a1', 'f1', 1, 42)"#
    )
    .execute(&pool)
    .await
    .unwrap();

    assert!(folder::delete_folder(&pool, "f1").await.unwrap());

    let messages: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM message"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(messages, 0);
}

#[tokio::test]
async fn deleting_account_removes_its_pending_operations() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES ('m1', 'a1', 'f1', 1, 42)"#
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        r#"INSERT INTO mail_operation
             (id, account_id, folder_id, uid, uidvalidity, kind, intent, state, created_at, updated_at)
           VALUES ('op1', 'a1', 'f1', 1, 42, 'set_flags', '{"seen":true}', 'queued', 1, 1)"#
    )
    .execute(&pool)
    .await
    .unwrap();

    // 删账户：操作记录随账户级联删除（用户显式放弃该账户的待提交操作）。
    account::delete_account(&pool, "a1").await.unwrap();

    let ops: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM mail_operation"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ops, 0, "操作记录随账户级联删除");
}

#[tokio::test]
async fn deleting_folder_keeps_pending_operations_for_recovery() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();
    sqlx::query!(
        r#"INSERT INTO message (id, account_id, folder_id, uid, uidvalidity)
           VALUES ('m1', 'a1', 'f1', 1, 42)"#
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        r#"INSERT INTO mail_operation
             (id, account_id, folder_id, uid, uidvalidity, kind, intent, state, created_at, updated_at)
           VALUES ('op1', 'a1', 'f1', 1, 42, 'set_flags', '{"seen":true}', 'queued', 1, 1)"#
    )
    .execute(&pool)
    .await
    .unwrap();

    // 删文件夹：邮件被级联删除，但操作记录**保留**。
    // 它对邮件只做逻辑引用，需由同步层核对后置为 uncertain / failed。
    folder::delete_folder(&pool, "f1").await.unwrap();

    let messages: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM message"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    let ops: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM mail_operation"#)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(messages, 0, "删文件夹后邮件应级联删除");
    assert_eq!(ops, 1, "删文件夹后待提交操作必须保留以供恢复核对");
}

// ===========================================================================
// DAO 行为
// ===========================================================================

#[tokio::test]
async fn account_round_trip_preserves_every_field() {
    let pool = fresh_pool().await;
    let acct = sample_account("a1");
    account::upsert_account(&pool, &acct).await.unwrap();

    let got = account::get_account(&pool, "a1")
        .await
        .unwrap()
        .expect("账户应存在");

    assert_eq!(got.email, acct.email);
    assert_eq!(got.provider, acct.provider);
    assert_eq!(got.auth_kind, acct.auth_kind);
    assert_eq!(got.server.imap_host, acct.server.imap_host);
    assert_eq!(got.server.imap_port, acct.server.imap_port);
    assert_eq!(got.server.imap_tls, acct.server.imap_tls);
    assert_eq!(got.server.smtp_host, acct.server.smtp_host);
    assert_eq!(got.server.smtp_port, acct.server.smtp_port);
    assert_eq!(got.server.smtp_tls, acct.server.smtp_tls);
    assert_eq!(got.server.smtp_auth, acct.server.smtp_auth);
    assert_eq!(got.sync_mode, acct.sync_mode);
    assert_eq!(got.secret_ref, acct.secret_ref);
    assert_eq!(got.oauth_client_id, acct.oauth_client_id);
    assert_eq!(got.poll_interval_secs, acct.poll_interval_secs);
    assert_eq!(got.display_name, acct.display_name);
    assert_eq!(got.color, acct.color);
    assert_eq!(got.signature_html, acct.signature_html);
    assert_eq!(got.username, acct.username);
    assert_eq!(got.created_at, acct.created_at);
    assert_eq!(got.updated_at, acct.updated_at);
}

#[tokio::test]
async fn upserting_same_account_id_updates_instead_of_duplicating() {
    let pool = fresh_pool().await;
    let mut acct = sample_account("a1");
    account::upsert_account(&pool, &acct).await.unwrap();

    acct.display_name = Some("改过的名字".to_string());
    acct.updated_at = 1_700_000_999_999;
    account::upsert_account(&pool, &acct).await.unwrap();

    let all = account::list_accounts(&pool).await.unwrap();
    assert_eq!(all.len(), 1, "同 id 应为更新，不应产生第二条");
    assert_eq!(all[0].display_name.as_deref(), Some("改过的名字"));
    assert_eq!(all[0].updated_at, 1_700_000_999_999);
    assert_eq!(
        all[0].created_at, 1_700_000_000_000,
        "created_at 不应被更新覆盖"
    );
}

#[tokio::test]
async fn missing_account_returns_none_not_error() {
    let pool = fresh_pool().await;

    let got = account::get_account(&pool, "nope").await.unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn deleting_missing_account_returns_false() {
    let pool = fresh_pool().await;

    assert!(!account::delete_account(&pool, "nope").await.unwrap());
}

#[tokio::test]
async fn unknown_enum_value_errors_instead_of_silently_defaulting() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    // 模拟数据与代码不一致：库里塞了一个不认识的 provider。
    sqlx::query!(r#"UPDATE account SET provider = 'not_a_provider' WHERE id = 'a1'"#)
        .execute(&pool)
        .await
        .unwrap();

    let err = account::list_accounts(&pool)
        .await
        .expect_err("未知枚举应报错，而不是静默回退");

    assert!(!err.is_retryable());
    assert!(err.requires_user_action());
}

#[tokio::test]
async fn folder_is_unique_per_account_and_keeps_local_id() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();

    // 同一 (account, remote_name)，但带一个不同的 id：
    // 冲突时不能换 id——id 已被 message 的外键引用。
    let mut renamed = sample_folder("f2", "a1");
    renamed.local_name = Some("收件箱".to_string());
    folder::upsert_folder(&pool, &renamed).await.unwrap();

    let all = folder::list_folders(&pool, "a1").await.unwrap();
    assert_eq!(all.len(), 1, "同一远端名不应产生第二个文件夹");
    assert_eq!(all[0].id, "f1", "冲突时不得替换本地 id");
    assert_eq!(all[0].local_name.as_deref(), Some("收件箱"));
}

#[tokio::test]
async fn folder_round_trip_preserves_cursor_and_epoch() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut f = sample_folder("f1", "a1");
    f.last_sync_at = Some(1_700_000_100_000);
    f.last_uid = Some(1234);
    f.highest_modseq = Some(99);
    f.uidvalidity = Some(42);
    folder::upsert_folder(&pool, &f).await.unwrap();

    let got = folder::get_folder(&pool, "f1").await.unwrap().unwrap();
    assert_eq!(got.last_sync_at, Some(1_700_000_100_000));
    assert_eq!(got.last_uid, Some(1234));
    assert_eq!(got.highest_modseq, Some(99));
    assert_eq!(got.uidvalidity, Some(42), "epoch 必须往返保留");
    assert!(got.subscribed);
    assert!(got.selectable);
    assert_eq!(got.delimiter.as_deref(), Some("/"));
}

#[tokio::test]
async fn same_remote_name_in_different_accounts_does_not_collide() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();
    account::upsert_account(&pool, &sample_account("a2"))
        .await
        .unwrap();

    folder::upsert_folder(&pool, &sample_folder("f1", "a1"))
        .await
        .unwrap();
    folder::upsert_folder(&pool, &sample_folder("f2", "a2"))
        .await
        .unwrap();

    assert_eq!(folder::list_folders(&pool, "a1").await.unwrap().len(), 1);
    assert_eq!(folder::list_folders(&pool, "a2").await.unwrap().len(), 1);
}

// ===========================================================================
// 事务
// ===========================================================================

#[tokio::test]
async fn failed_transaction_rolls_back_entirely() {
    let pool = fresh_pool().await;

    let mut tx = pool.begin().await.unwrap();
    sqlx::query!(
        r#"INSERT INTO account
             (id, email, provider, auth_kind, imap_host, imap_port, imap_tls,
              smtp_host, smtp_port, smtp_tls, smtp_auth, username, sync_mode,
              poll_interval_secs, created_at, updated_at)
           VALUES ('a1', 'a1@example.com', 'netease_163', 'password',
                   'imap.example.com', 993, 'implicit',
                   'smtp.example.com', 465, 'implicit', 'login',
                   'a1@example.com', 'poll', 300, 1, 1)"#
    )
    .execute(&mut *tx)
    .await
    .unwrap();

    // 故意违反 CHECK 约束，让事务失败。
    let bad = sqlx::query!(
        r#"INSERT INTO account
             (id, email, provider, auth_kind, imap_host, imap_port, imap_tls,
              smtp_host, smtp_port, smtp_tls, smtp_auth, username, sync_mode,
              poll_interval_secs, created_at, updated_at)
           VALUES ('a2', 'a2@example.com', 'netease_163', 'password',
                   'imap.example.com', 0, 'implicit',
                   'smtp.example.com', 465, 'implicit', 'login',
                   'a2@example.com', 'poll', 300, 1, 1)"#
    )
    .execute(&mut *tx)
    .await;
    assert!(bad.is_err(), "CHECK 约束应使事务失败");

    // 事务未提交即丢弃 → 第一个账户也必须不存在。
    drop(tx);

    let count: i64 = sqlx::query_scalar!(r#"SELECT COUNT(*) AS c FROM account"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "失败的事务必须整体回滚");
}

// ===========================================================================
// 文件库
// ===========================================================================

#[tokio::test]
async fn file_database_survives_reopen() {
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    let path = dir.path().join("xremail-test.db");

    {
        let pool = open_pool_path(&path, &PoolConfig::default())
            .await
            .expect("打开文件库失败");
        migrate(&pool).await.expect("应用迁移失败");
        account::upsert_account(&pool, &sample_account("a1"))
            .await
            .unwrap();
    }

    // 重新打开同一个文件，数据应在。
    let pool = open_pool_path(&path, &PoolConfig::default())
        .await
        .expect("重新打开文件库失败");
    let accounts = account::list_accounts(&pool).await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, "a1");
}

// ===========================================================================
// 游标不被「列文件夹」抹掉（R53）
// ===========================================================================

#[tokio::test]
async fn a_folder_upsert_without_a_cursor_does_not_erase_the_stored_epoch() {
    // `upsert_folder` 吃的是 `LIST` 的结论，而 `LIST` **不**给 UIDVALIDITY / UIDNEXT。
    // 调用方本该把库里旧值抄进来，但那是约定；约定一旦抄漏（或换了调用方），
    // 游标就被写成 NULL，而 `reconcile` 把 `folder_uidvalidity = None` 解释成
    // 「还没同步过，不比对 epoch」——绑定旧 epoch 的意图于是永远不会被隔离，
    // 旧 UID 会被套到新世代里另一封信上。
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut synced = sample_folder("f1", "a1");
    synced.uidvalidity = Some(42);
    synced.last_uid = Some(100);
    synced.highest_modseq = Some(500);
    folder::upsert_folder(&pool, &synced).await.unwrap();

    // 第二轮 `LIST`：只认得名字与角色，游标一个都不知道。
    let mut listed = sample_folder("f1", "a1");
    listed.uidvalidity = None;
    listed.last_uid = None;
    listed.highest_modseq = None;
    listed.role = Some("inbox".to_string());
    folder::upsert_folder(&pool, &listed).await.unwrap();

    let back = folder::get_folder(&pool, "f1")
        .await
        .unwrap()
        .expect("应当还在");
    assert_eq!(back.uidvalidity, Some(42), "epoch 被一次 LIST 抹掉了");
    assert_eq!(back.last_uid, Some(100), "游标被一次 LIST 抹掉了");
    assert_eq!(back.highest_modseq, Some(500));
    assert_eq!(
        back.role.as_deref(),
        Some("inbox"),
        "role 相反：NULL 是「这一轮没认出来」，必须照写"
    );

    // 反过来：拉取给出了新游标，就必须推进。
    let mut pulled = sample_folder("f1", "a1");
    pulled.uidvalidity = Some(43);
    pulled.last_uid = Some(5);
    folder::upsert_folder(&pool, &pulled).await.unwrap();
    let back = folder::get_folder(&pool, "f1").await.unwrap().unwrap();
    assert_eq!(back.uidvalidity, Some(43), "非空的游标必须能推进");
    assert_eq!(back.last_uid, Some(5));
}
