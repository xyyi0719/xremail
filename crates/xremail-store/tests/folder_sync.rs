//! `commit_folder_sync` 的原子性（R54）。
//!
//! 跨表提交的**全部**价值就在「同时成功或同时失败」：一轮同步要清旧世代、
//! 写新邮件、删服务器已删的、推进游标，这四件事只做一半会留下
//! 「新 epoch 的游标 + 旧 epoch 的邮件」——下一轮差分会拿着错的 epoch 去算。
//!
//! `store.rs` 里已有一条「事务失败整体回滚」，钉的是**语句失败**（约束冲突）那条路。
//! 这里补的是另一条：参数校验失败导致的**提前 `return`**。它同样必须什么都不留下——
//! 不能因为「反正 sqlx 的 `Transaction` drop 时会回滚」就把正确性押在异步 drop 上。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, Address, AuthKind, Folder, Message, MessageFlags, ProviderKind, ServerConfig,
    SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::sync::{FolderSyncWrite, commit_folder_sync};
use xremail_store::dao::{account, folder, message};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};

/// 建一个跑完迁移的内存库，种好账户与一个已同步过的文件夹（epoch 42）。
async fn seeded() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("应用迁移失败");

    let now = 1_700_000_000_000;
    let account = Account {
        id: "a1".to_string(),
        email: "a1@example.com".to_string(),
        provider: ProviderKind::Generic,
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
        username: "a1@example.com".to_string(),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: now,
        updated_at: now,
    };
    account::upsert_account(&pool, &account)
        .await
        .expect("账户应当写入");

    let mut inbox = folder_of("inbox", "INBOX", Some(42));
    inbox.last_uid = Some(1);
    folder::upsert_folder(&pool, &inbox)
        .await
        .expect("文件夹应当写入");

    message::upsert_message(&pool, &mail("old", "inbox", 1, 42))
        .await
        .expect("旧世代邮件应当写入");

    pool
}

fn folder_of(id: &str, remote_name: &str, uidvalidity: Option<i64>) -> Folder {
    Folder {
        id: id.to_string(),
        account_id: "a1".to_string(),
        remote_name: remote_name.to_string(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".to_string()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        uidvalidity,
        role: None,
    }
}

fn mail(id: &str, folder_id: &str, uid: i64, uidvalidity: i64) -> Message {
    Message {
        id: id.to_string(),
        account_id: "a1".to_string(),
        folder_id: folder_id.to_string(),
        uid,
        uidvalidity,
        message_id: Some(format!("<{id}@example.com>")),
        subject: Some("新世代的信".to_string()),
        snippet: None,
        from: vec![Address::new("sender@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        date_sent: None,
        internal_date: None,
        flags: MessageFlags::empty(),
        size: None,
        has_attachment: false,
    }
}

#[tokio::test]
async fn a_rejected_epoch_switch_leaves_the_old_generation_untouched() {
    // `drop_other_epochs = true` 却**没有**给新 `uidvalidity`：这是一个自相矛盾的
    // 输入（「切世代」却不知道切到哪个世代），函数在校验这一步就返回 `Invalid`。
    //
    // 此时它可能已经先动了手——清旧世代 + 写新邮件都在校验之后、但仍在同一笔事务里，
    // 唯一能保证「什么都没发生」的是回滚。回滚必须是**确定的**，不能靠
    // `Transaction` 在异步 drop 里补做。
    let pool = seeded().await;

    let target = folder_of("inbox", "INBOX", None);
    let write = FolderSyncWrite {
        upserts: vec![mail("new", "inbox", 1, 77)],
        delete_uids: vec![1],
        drop_other_epochs: true,
    };
    let err = commit_folder_sync(&pool, &target, &write)
        .await
        .expect_err("没有新 epoch 的世代切换必须被拒绝");
    assert!(
        !err.is_retryable(),
        "这是数据问题，重试同一份输入不可能成功"
    );

    let stored = folder::get_folder(&pool, "inbox")
        .await
        .expect("读文件夹")
        .expect("文件夹应当还在");
    assert_eq!(stored.uidvalidity, Some(42), "游标不该被动过");
    assert_eq!(stored.last_uid, Some(1));

    let uids = message::list_message_uids(&pool, "a1", "inbox", 42)
        .await
        .expect("读 UID");
    assert_eq!(uids, vec![1], "旧世代的邮件不该被删");
    assert!(
        message::list_message_uids(&pool, "a1", "inbox", 77)
            .await
            .expect("读 UID")
            .is_empty(),
        "被拒绝的那一轮不该把新世代的邮件写进来"
    );
}

#[tokio::test]
async fn an_epoch_switch_clears_the_old_generation_and_moves_the_cursor_in_one_step() {
    // 正向对照：同样的输入加上新 epoch 之后就应当整体生效——
    // 否则上面那条「什么都没发生」可能只是「这个函数什么都不做」。
    let pool = seeded().await;

    let target = folder_of("inbox", "INBOX", Some(77));
    let write = FolderSyncWrite {
        upserts: vec![mail("new", "inbox", 1, 77)],
        delete_uids: Vec::new(),
        drop_other_epochs: true,
    };
    commit_folder_sync(&pool, &target, &write)
        .await
        .expect("世代切换应当成功");

    assert!(
        message::list_message_uids(&pool, "a1", "inbox", 42)
            .await
            .expect("读 UID")
            .is_empty(),
        "旧世代必须清干净"
    );
    assert_eq!(
        message::list_message_uids(&pool, "a1", "inbox", 77)
            .await
            .expect("读 UID"),
        vec![1]
    );
    let stored = folder::get_folder(&pool, "inbox")
        .await
        .expect("读文件夹")
        .expect("文件夹应当还在");
    assert_eq!(stored.uidvalidity, Some(77));
}
