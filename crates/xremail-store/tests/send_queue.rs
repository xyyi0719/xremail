//! `xremail-store` 的发信队列（`send_queue`）测试。
//!
//! 测试预期来自**协议与存储语义**，不来自实现本身
//! （见 [AI 开发工作规范 §1.3](../../../docs/AI_DEVELOPMENT_GUIDE.md)）。
//!
//! 覆盖的不变量：
//! * **Bcc 不泄漏**：密送地址只出现在 `envelope` 列，绝不出现在 `headers` 列
//!   （ARCHITECTURE §5）。
//! * **信封收件人 = To ∪ Cc ∪ Bcc**（去重、保持首次出现顺序）。
//! * **状态机只允许规定的迁移**：每次迁移都是 CAS，两个发送器只有一个能取到信。
//! * **不确定不等于可重试**：进程中断把 `running` 收成 `uncertain`，
//!   不是 `retryable`——服务器可能已经收下了那封信。
//! * **重试发的是同样的字节**：`message_bytes` 一旦冻结就不再变，
//!   所以 `Message-ID` 不变（对端能认出这是同一封）。
//! * **幂等键唯一**：同一账户里同一个幂等键不能有两行。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, ProviderKind, SendAttachment, SendQueueEntry, SendState, ServerConfig,
    SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::account;
use xremail_store::dao::send_queue::{self, MAX_SEND_ATTEMPTS};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};

/// 建一个已应用迁移的内存库。
async fn fresh_pool() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("应用迁移失败");
    pool
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

/// 造一封草稿。
fn draft(id: &str, account_id: &str) -> SendQueueEntry {
    SendQueueEntry {
        id: id.to_string(),
        account_id: account_id.to_string(),
        from: format!("{account_id}@example.com"),
        to: vec!["to@example.com".to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "测试标题".to_string(),
        body_text: Some("正文".to_string()),
        body_html: None,
        attachments: Vec::new(),
        in_reply_to: None,
        references: None,
        state: SendState::Draft,
        idempotency_key: format!("key-{id}"),
        message_bytes: None,
        message_id: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

// ===========================================================================
// 草稿：存 / 取 / 改 / 删
// ===========================================================================

#[tokio::test]
async fn draft_round_trips_through_the_database() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut entry = draft("s1", "a1");
    entry.cc = vec!["cc@example.com".to_string()];
    entry.bcc = vec!["bcc@example.com".to_string()];
    entry.body_html = Some("<p>正文</p>".to_string());
    entry.in_reply_to = Some("<orig@example.com>".to_string());

    assert!(send_queue::save_draft(&pool, &entry).await.unwrap());

    let back = send_queue::get_entry(&pool, "s1")
        .await
        .unwrap()
        .expect("草稿应存在");

    assert_eq!(back.id, "s1");
    assert_eq!(back.account_id, "a1");
    assert_eq!(back.from, "a1@example.com");
    assert_eq!(back.to, vec!["to@example.com".to_string()]);
    assert_eq!(back.cc, vec!["cc@example.com".to_string()]);
    assert_eq!(back.bcc, vec!["bcc@example.com".to_string()]);
    assert_eq!(back.subject, "测试标题");
    assert_eq!(back.body_text.as_deref(), Some("正文"));
    assert_eq!(back.body_html.as_deref(), Some("<p>正文</p>"));
    assert_eq!(back.in_reply_to.as_deref(), Some("<orig@example.com>"));
    assert_eq!(back.state, SendState::Draft);
    assert_eq!(back.idempotency_key, "key-s1");
}

#[tokio::test]
async fn bcc_only_lives_in_the_envelope_column() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut entry = draft("s1", "a1");
    entry.bcc = vec!["secret@example.com".to_string()];
    send_queue::save_draft(&pool, &entry).await.unwrap();

    // 直接看列：headers（显示用头部）里不能出现密送地址，
    // envelope（实际投递地址）里必须有它。
    let headers: String = sqlx::query_scalar("SELECT headers FROM send_queue WHERE id = 's1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let envelope: String = sqlx::query_scalar("SELECT envelope FROM send_queue WHERE id = 's1'")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(
        !headers.contains("secret@example.com"),
        "Bcc 泄漏进了 headers 列：{headers}"
    );
    assert!(
        envelope.contains("secret@example.com"),
        "envelope 列里没有 Bcc，信封会漏掉这个收件人：{envelope}"
    );
}

#[tokio::test]
async fn envelope_recipients_union_to_cc_bcc_without_duplicates() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut entry = draft("s1", "a1");
    entry.to = vec!["a@example.com".to_string(), "b@example.com".to_string()];
    entry.cc = vec!["b@example.com".to_string(), "c@example.com".to_string()];
    entry.bcc = vec!["d@example.com".to_string(), "a@example.com".to_string()];
    send_queue::save_draft(&pool, &entry).await.unwrap();

    let envelope: String = sqlx::query_scalar("SELECT envelope FROM send_queue WHERE id = 's1'")
        .fetch_one(&pool)
        .await
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&envelope).unwrap();
    let recipients: Vec<String> = serde_json::from_value(parsed["recipients"].clone()).unwrap();

    assert_eq!(
        recipients,
        vec![
            "a@example.com".to_string(),
            "b@example.com".to_string(),
            "c@example.com".to_string(),
            "d@example.com".to_string(),
        ],
        "收件人集合应为 To ∪ Cc ∪ Bcc 去重，且保持首次出现顺序"
    );
}

#[tokio::test]
async fn draft_must_have_at_least_one_recipient() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut entry = draft("s1", "a1");
    entry.to.clear();

    let err = send_queue::save_draft(&pool, &entry).await.unwrap_err();
    assert!(!err.is_retryable(), "没有收件人不是可重试的错误");
}

#[tokio::test]
async fn saving_a_draft_never_keeps_stale_message_bytes() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();

    // 直接把字节写到草稿行上（模拟历史遗留 / 未来新路径留下的脏状态），
    // 再存一次草稿 → 旧字节必须被清掉。
    sqlx::query("UPDATE send_queue SET message_bytes = ?, message_id = ? WHERE id = 's1'")
        .bind(b"stale bytes".to_vec())
        .bind("<stale@x>")
        .execute(&pool)
        .await
        .unwrap();

    let mut edited = entry.clone();
    edited.subject = "改过的标题".to_string();
    assert!(send_queue::save_draft(&pool, &edited).await.unwrap());

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Draft);
    assert_eq!(back.subject, "改过的标题");
    assert!(
        back.message_bytes.is_none(),
        "旧字节没清掉：内容改了却发出同一份字节，对端会当成同一封信"
    );
    assert!(back.message_id.is_none());
}

#[tokio::test]
async fn draft_cannot_be_edited_once_it_is_queued() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();

    let mut edited = entry.clone();
    edited.subject = "改过的标题".to_string();
    assert!(
        !send_queue::save_draft(&pool, &edited).await.unwrap(),
        "已排队的信不该被草稿保存覆盖：用户看到的和发出去的就不是同一封了"
    );

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Queued);
    assert_eq!(back.subject, "测试标题", "被拒绝的保存不该改到内容");
}

#[tokio::test]
async fn only_drafts_can_be_deleted() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    assert!(send_queue::delete_draft(&pool, "s1").await.unwrap());
    assert!(send_queue::get_entry(&pool, "s1").await.unwrap().is_none());

    // 已经排队的那封不能删。
    let entry = draft("s2", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s2", b"bytes", "<m2@x>", 1)
        .await
        .unwrap();
    assert!(
        !send_queue::delete_draft(&pool, "s2").await.unwrap(),
        "已排队的信不该能删掉"
    );
    assert!(send_queue::get_entry(&pool, "s2").await.unwrap().is_some());
}

// ===========================================================================
// 状态机
// ===========================================================================

#[tokio::test]
async fn sending_walks_the_documented_state_machine() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();

    // draft → queued（字节在此刻冻结）
    assert!(
        send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
            .await
            .unwrap()
    );
    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Queued);
    assert_eq!(back.message_bytes.as_deref(), Some(&b"bytes"[..]));
    assert_eq!(back.message_id.as_deref(), Some("<m1@x>"));

    // queued → running → succeeded
    assert!(
        send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    assert!(send_queue::mark_succeeded(&pool, "s1", 3).await.unwrap());

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Succeeded);
    assert_eq!(back.attempts, 1, "发送过一次就该记一次");
}

#[tokio::test]
async fn only_one_sender_can_claim_a_queued_message() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();

    // 两个发送器同时取同一条：只有一个能改到行。
    let first = send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
        .await
        .unwrap();
    let second = send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
        .await
        .unwrap();

    assert!(first, "第一个发送器应该取到这封信");
    assert!(!second, "第二个发送器不该取到同一封信——那会发两遍");
}

#[tokio::test]
async fn retryable_failure_can_be_claimed_again() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();
    send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
        .await
        .unwrap();
    assert!(
        send_queue::mark_retryable(&pool, "s1", "connection", "连不上", 3)
            .await
            .unwrap()
    );

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Retryable);
    assert_eq!(back.error_kind.as_deref(), Some("connection"));
    assert!(back.message_bytes.is_some(), "重试要能拿到原来那份字节");

    // 可重试 → 再取一次（发同样的字节）。
    assert!(
        send_queue::claim_for_sending(&pool, "s1", "boot-a", 4, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.attempts, 2);
    assert_eq!(back.message_bytes.as_deref(), Some(&b"bytes"[..]));
    assert_eq!(back.message_id.as_deref(), Some("<m1@x>"));
}

#[tokio::test]
async fn uncertain_message_is_not_auto_retryable() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();
    send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
        .await
        .unwrap();
    assert!(
        send_queue::mark_uncertain(&pool, "s1", "no_reply", "没收到应答", 3)
            .await
            .unwrap()
    );

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Uncertain);
    assert!(
        !SendState::Uncertain.is_auto_retryable(),
        "不确定的信不能自动重试：服务器可能已经收下了"
    );

    // 自动发送器不会去取它。
    let pending =
        send_queue::list_entries_in_states(&pool, "a1", &[SendState::Queued, SendState::Retryable])
            .await
            .unwrap();
    assert!(pending.is_empty(), "不确定的信不该被自动发送器捞走");
}

#[tokio::test]
async fn interrupted_send_becomes_uncertain_not_retryable() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();
    send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
        .await
        .unwrap();

    // 进程死在发信途中，下一次启动（boot-b）对账。
    // 必须换一个 boot：同一个 boot 意味着「那个进程还活着」，不应被回收
    // （见 `the_live_process_rows_are_not_reclaimed`）。
    let reclaimed = send_queue::reclaim_running(&pool, "a1", "boot-b", 3)
        .await
        .unwrap();
    assert_eq!(reclaimed, 1);

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(
        back.state,
        SendState::Uncertain,
        "中断的发送是「不知道」，不是「没发成功」——标成可重试就会重复投递"
    );
}

#[tokio::test]
async fn the_live_process_rows_are_not_reclaimed() {
    // 这条钉的是 `owner_boot` 守卫的全部价值：
    // 若启动时无条件扫所有 `running`，同一台机器上另一个**还活着**的进程
    // 正在发的信会被翻成「结果未知」，用户白确认一次、还烧掉一次尝试配额。
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();
    send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
        .await
        .unwrap();

    let reclaimed = send_queue::reclaim_running(&pool, "a1", "boot-a", 3)
        .await
        .unwrap();
    assert_eq!(reclaimed, 0, "自己认领的行不是残骸，不该被自己回收");

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(
        back.state,
        SendState::Running,
        "活着的发送应留在 running，被回收就变成白让人确认一次"
    );
}

#[tokio::test]
async fn user_can_explicitly_resend_an_uncertain_message_with_same_bytes() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();
    send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
        .await
        .unwrap();
    send_queue::mark_uncertain(&pool, "s1", "no_reply", "没收到应答", 3)
        .await
        .unwrap();

    assert!(send_queue::retry_uncertain(&pool, "s1", 4).await.unwrap());

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Queued);
    assert_eq!(
        back.message_bytes.as_deref(),
        Some(&b"bytes"[..]),
        "重发要发同样的字节，Message-ID 才不会变"
    );
    assert_eq!(back.message_id.as_deref(), Some("<m1@x>"));
}

#[tokio::test]
async fn state_transitions_only_apply_to_the_state_they_come_from() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();

    // 还在草稿态时，running 相关的迁移都不该生效。
    assert!(
        !send_queue::claim_for_sending(&pool, "s1", "boot-a", 1, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    assert!(!send_queue::mark_succeeded(&pool, "s1", 1).await.unwrap());
    assert!(
        !send_queue::mark_failed(&pool, "s1", "e", "m", 1)
            .await
            .unwrap()
    );
    assert!(
        !send_queue::mark_uncertain(&pool, "s1", "e", "m", 1)
            .await
            .unwrap()
    );
    assert!(!send_queue::retry_uncertain(&pool, "s1", 1).await.unwrap());

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Draft);
}

// ===========================================================================
// 附件
// ===========================================================================

#[tokio::test]
async fn attachments_round_trip_in_order() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut entry = draft("s1", "a1");
    entry.attachments = vec![
        SendAttachment {
            filename: "一.txt".to_string(),
            content_type: "text/plain".to_string(),
            bytes: vec![0xE4, 0xB8, 0x80],
        },
        SendAttachment {
            filename: "二.bin".to_string(),
            content_type: "application/octet-stream".to_string(),
            bytes: vec![0, 1, 2, 255],
        },
    ];
    send_queue::save_draft(&pool, &entry).await.unwrap();

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.attachments.len(), 2);
    assert_eq!(back.attachments[0].filename, "一.txt");
    assert_eq!(back.attachments[0].bytes, vec![0xE4, 0xB8, 0x80]);
    assert_eq!(back.attachments[1].filename, "二.bin");
    assert_eq!(back.attachments[1].bytes, vec![0, 1, 2, 255]);
}

#[tokio::test]
async fn saving_again_replaces_the_whole_attachment_list() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut entry = draft("s1", "a1");
    entry.attachments = vec![SendAttachment {
        filename: "旧.txt".to_string(),
        content_type: "text/plain".to_string(),
        bytes: b"old".to_vec(),
    }];
    send_queue::save_draft(&pool, &entry).await.unwrap();

    entry.attachments = vec![SendAttachment {
        filename: "新.txt".to_string(),
        content_type: "text/plain".to_string(),
        bytes: b"new".to_vec(),
    }];
    send_queue::save_draft(&pool, &entry).await.unwrap();

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.attachments.len(), 1);
    assert_eq!(back.attachments[0].filename, "新.txt");
}

#[tokio::test]
async fn deleting_a_draft_deletes_its_attachments() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut entry = draft("s1", "a1");
    entry.attachments = vec![SendAttachment {
        filename: "a.txt".to_string(),
        content_type: "text/plain".to_string(),
        bytes: b"x".to_vec(),
    }];
    send_queue::save_draft(&pool, &entry).await.unwrap();

    send_queue::delete_draft(&pool, "s1").await.unwrap();

    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM send_attachment")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "草稿删了，附件不该留在库里");
}

// ===========================================================================
// 重试次数上限（R31）
// ===========================================================================

#[tokio::test]
async fn a_send_cannot_be_claimed_more_than_max_attempts_times() {
    // 这条钉的是 R31 的核心：`attempts` 不再是只增不查的死计数器。
    // 上限用小值（2）而不是 `MAX_SEND_ATTEMPTS`，是为了让用例在「第几次」上
    // 一眼可读——上限本身是多少由 `max_attempts` 参数说话，与数字无关。
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();

    // 第 1、2 次：正常认领，每次回去都是「可重试」。
    for round in 0..2 {
        assert!(
            send_queue::claim_for_sending(&pool, "s1", "boot-a", 10 + round, 2)
                .await
                .unwrap(),
            "第 {} 次提交应当能取到信",
            round + 1
        );
        assert!(
            send_queue::mark_retryable(&pool, "s1", "not_sent", "连不上", 20 + round)
                .await
                .unwrap()
        );
    }

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.attempts, 2, "提交了两次就该记两次");
    assert_eq!(back.state, SendState::Retryable);

    // 第 3 次：到顶了，认领必须失败——否则「重试 N 次」是句空话。
    assert!(
        !send_queue::claim_for_sending(&pool, "s1", "boot-a", 30, 2)
            .await
            .unwrap(),
        "已经到顶的信不该还能被认领"
    );

    // 认领失败只说明「没取到」，它自己不改状态：处置由调用方说清楚。
    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Retryable);
    assert_eq!(back.attempts, 2, "没认领到就不该再加一次");
}

#[tokio::test]
async fn an_exhausted_send_becomes_failed_with_an_explanation() {
    // 到顶之后必须转 `failed` 并写清原因，而不是让行卡在 `retryable`：
    // 卡着不动的话，用户重试会被一直拒绝，界面上却没有任何一处说为什么。
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();

    for round in 0..MAX_SEND_ATTEMPTS {
        assert!(
            send_queue::claim_for_sending(&pool, "s1", "boot-a", 10 + round, MAX_SEND_ATTEMPTS)
                .await
                .unwrap()
        );
        assert!(
            send_queue::mark_retryable(&pool, "s1", "not_sent", "连不上", 20 + round)
                .await
                .unwrap()
        );
    }

    // 上限用尽：认领失败，于是调用方去收尾。
    assert!(
        !send_queue::claim_for_sending(&pool, "s1", "boot-a", 99, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    assert!(
        send_queue::fail_if_exhausted(&pool, "s1", MAX_SEND_ATTEMPTS, 100)
            .await
            .unwrap(),
        "到顶的这条应当被收成 failed"
    );

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Failed);
    assert_eq!(back.error_kind.as_deref(), Some("retry_exhausted"));
    let message = back.error_message.unwrap_or_default();
    assert!(
        message.contains(&MAX_SEND_ATTEMPTS.to_string()),
        "错误说明该写清试了几次：{message}"
    );
    assert!(
        message.contains("没有发出去"),
        "必须让用户知道信到底发没发出去：{message}"
    );
    assert!(
        back.message_bytes.is_some(),
        "字节不该被清掉，用户可能还要看这封写的是什么"
    );
}

#[tokio::test]
async fn fail_if_exhausted_leaves_other_states_alone() {
    // 收盘只在「没戏了」的时候动手：
    // 还没到顶的不收、正在发的不收、`uncertain`（等人工裁决）更不收。
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    for (id, key) in [("s1", "k1"), ("s2", "k2"), ("s3", "k3")] {
        let mut entry = draft(id, "a1");
        entry.idempotency_key = key.to_string();
        send_queue::save_draft(&pool, &entry).await.unwrap();
        send_queue::queue_for_sending(&pool, id, b"bytes", "<m@x>", 1)
            .await
            .unwrap();
    }

    // s1：只试了一次，远没到顶。
    assert!(
        send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    assert!(
        send_queue::mark_retryable(&pool, "s1", "not_sent", "连不上", 3)
            .await
            .unwrap()
    );
    assert!(
        !send_queue::fail_if_exhausted(&pool, "s1", MAX_SEND_ATTEMPTS, 4)
            .await
            .unwrap(),
        "没到顶的不该被收掉——那等于把还能重试的信用判死"
    );

    // s2：正在发（`running`）。
    assert!(
        send_queue::claim_for_sending(&pool, "s2", "boot-a", 2, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    assert!(
        !send_queue::fail_if_exhausted(&pool, "s2", 1, 4)
            .await
            .unwrap(),
        "有人正在发的行不该被别人改成 failed"
    );

    // s3：`uncertain`——只能在人确认之后再动。
    assert!(
        send_queue::claim_for_sending(&pool, "s3", "boot-a", 2, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    assert!(
        send_queue::mark_uncertain(&pool, "s3", "no_reply", "没收到应答", 3)
            .await
            .unwrap()
    );
    assert!(
        !send_queue::fail_if_exhausted(&pool, "s3", 1, 4)
            .await
            .unwrap(),
        "`uncertain` 要由人裁决，不该被「次数用完」顺手收成 failed"
    );

    assert_eq!(
        send_queue::get_entry(&pool, "s3")
            .await
            .unwrap()
            .unwrap()
            .state,
        SendState::Uncertain
    );
}

#[tokio::test]
async fn an_exhausted_message_can_be_sent_again_after_the_user_edits_it() {
    // 「要求人工」必须留出路：`failed` 是可编辑状态，
    // 用户改内容（`save_draft` 把 attempts 归零）之后就能重新发。
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let entry = draft("s1", "a1");
    send_queue::save_draft(&pool, &entry).await.unwrap();
    send_queue::queue_for_sending(&pool, "s1", b"bytes", "<m1@x>", 1)
        .await
        .unwrap();
    assert!(
        send_queue::claim_for_sending(&pool, "s1", "boot-a", 2, MAX_SEND_ATTEMPTS)
            .await
            .unwrap()
    );
    // 认领之后有一条 `running`，先给它一个结局（这里选「可重试」），
    // 再按「上限 = 已经试过的次数」收盘——这正是调用点在认领失败之后做的事。
    assert!(
        send_queue::mark_retryable(&pool, "s1", "not_sent", "连不上", 3)
            .await
            .unwrap()
    );
    assert!(
        send_queue::fail_if_exhausted(&pool, "s1", 1, 3)
            .await
            .unwrap()
    );

    // 用户改了内容重发：回到草稿，计数归零。
    let mut edited = draft("s1", "a1");
    edited.subject = "改过的标题".to_string();
    send_queue::save_draft(&pool, &edited).await.unwrap();

    let back = send_queue::get_entry(&pool, "s1").await.unwrap().unwrap();
    assert_eq!(back.state, SendState::Draft);
    assert_eq!(back.attempts, 0, "改过内容就该重新开始计数");

    assert!(
        send_queue::queue_for_sending(&pool, "s1", b"bytes2", "<m2@x>", 4)
            .await
            .unwrap(),
        "`failed` 之后要能重新排队"
    );
    assert!(
        send_queue::claim_for_sending(&pool, "s1", "boot-a", 5, MAX_SEND_ATTEMPTS)
            .await
            .unwrap(),
        "重新开始计数之后当然能再发"
    );
}

// ===========================================================================
// 幂等
// ===========================================================================

#[tokio::test]
async fn one_account_cannot_have_two_rows_with_the_same_idempotency_key() {
    let pool = fresh_pool().await;
    account::upsert_account(&pool, &sample_account("a1"))
        .await
        .unwrap();

    let mut first = draft("s1", "a1");
    first.idempotency_key = "same-key".to_string();
    send_queue::save_draft(&pool, &first).await.unwrap();

    let mut second = draft("s2", "a1");
    second.idempotency_key = "same-key".to_string();
    let err = send_queue::save_draft(&pool, &second).await.unwrap_err();

    assert!(
        !err.is_retryable(),
        "幂等键冲突是数据问题，不是可重试的临时故障：{err}"
    );
}
