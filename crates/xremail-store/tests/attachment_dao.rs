//! 附件 DAO 的行为测试。
//!
//! 用内存库 + 真迁移（`xremail_store::migrate`），覆盖：
//! 写入/读回、重拉正文时保留下载状态、部件消失时删行、无 `part_key` 的项被跳过、
//! 内联列表、`mark_downloaded` 的落库效果、以及两条「重拉不许覆盖」的语义
//! （`created_at` / `downloaded_at` 不被刷新，`fetched=1` 时 `size` 以落盘字节为准）。

use sqlx::SqlitePool;
use xremail_core::model::{Account, Attachment, AuthKind, Folder, Message, MessageFlags};
use xremail_store::dao::attachment::{
    attachment_timestamps, count_pending_downloads, get_attachment, list_attachments,
    list_inline_attachments, mark_downloaded, replace_attachments,
};
use xremail_store::{PoolConfig, open_pool_in_memory};

/// 内存库 + 跑完全部迁移。
async fn setup() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("内存库");
    xremail_store::migrate(&pool).await.expect("迁移");
    seed(&pool, "m1").await;
    pool
}

/// 种一个账号 / 文件夹 / 邮件（attachment 表有外键）。
async fn seed(pool: &SqlitePool, message_id: &str) {
    let account = Account {
        id: "a1".into(),
        email: "me@example.com".into(),
        provider: xremail_core::ProviderKind::Generic,
        display_name: None,
        color: None,
        auth_kind: AuthKind::Password,
        server: xremail_core::ServerConfig::new("imap.example.com", 993, "smtp.example.com", 465),
        username: "me@example.com".into(),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: xremail_core::SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 1,
        updated_at: 1,
    };
    xremail_store::dao::account::upsert_account(pool, &account)
        .await
        .expect("account");

    let folder = Folder {
        id: "f1".into(),
        account_id: "a1".into(),
        remote_name: "INBOX".into(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".into()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        uidvalidity: Some(1),
        role: None,
    };
    xremail_store::dao::folder::upsert_folder(pool, &folder)
        .await
        .expect("folder");

    let message = Message {
        id: message_id.into(),
        account_id: "a1".into(),
        folder_id: "f1".into(),
        uid: 1,
        uidvalidity: 1,
        message_id: Some(format!("<{message_id}@example.com>")),
        subject: Some("你好".into()),
        snippet: None,
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        date_sent: None,
        internal_date: None,
        flags: MessageFlags::empty(),
        size: None,
        has_attachment: true,
    };
    xremail_store::dao::message::upsert_message(pool, &message)
        .await
        .expect("message");
}

/// 造一个附件描述。
fn attachment(id: &str, message_id: &str, part_key: &str, filename: &str) -> Attachment {
    Attachment {
        id: id.into(),
        message_id: message_id.into(),
        filename: Some(filename.into()),
        mime: Some("application/pdf".into()),
        size: Some(1024),
        content_id: None,
        inline: false,
        local_path: None,
        sha256: None,
        fetched: false,
        part_key: Some(part_key.into()),
        downloaded_at_ms: None,
        transfer_encoding: Some("base64".into()),
    }
}

#[tokio::test]
async fn replace_then_list_round_trips_metadata() {
    let pool = setup().await;

    let written = replace_attachments(
        &pool,
        "m1",
        &[
            attachment("at2", "m1", "2", "报告.pdf"),
            attachment("at1", "m1", "1", "合同.pdf"),
        ],
        1_000,
    )
    .await
    .expect("replace");

    assert_eq!(written, 2);

    let list = list_attachments(&pool, "m1").await.expect("list");
    assert_eq!(list.len(), 2);
    // 按 part_key 排序，与写入顺序无关。
    assert_eq!(list[0].part_key.as_deref(), Some("1"));
    assert_eq!(list[0].filename.as_deref(), Some("合同.pdf"));
    assert_eq!(list[1].filename.as_deref(), Some("报告.pdf"));
    assert!(!list[0].fetched);
    assert!(list[0].local_path.is_none());
    assert!(list[0].downloaded_at_ms.is_none());
}

#[tokio::test]
async fn marking_downloaded_survives_a_refetch_of_the_same_structure() {
    let pool = setup().await;
    replace_attachments(&pool, "m1", &[attachment("at1", "m1", "1", "a.pdf")], 1)
        .await
        .expect("replace");

    let marked = mark_downloaded(
        &pool,
        "m1",
        "at1",
        "/data/attachments/x/a",
        "deadbeef",
        2048,
        5_000,
    )
    .await
    .expect("mark");
    assert!(marked);

    // 再拉一次同样的结构（文件名带了新内容，元数据要更新）。
    replace_attachments(
        &pool,
        "m1",
        &[attachment("at1", "m1", "1", "a-final.pdf")],
        9_000,
    )
    .await
    .expect("replace again");

    let list = list_attachments(&pool, "m1").await.expect("list");
    assert_eq!(list.len(), 1);
    let item = &list[0];
    assert_eq!(
        item.filename.as_deref(),
        Some("a-final.pdf"),
        "元数据要刷新"
    );
    assert!(item.fetched, "下载状态不能被重拉清掉");
    assert_eq!(item.local_path.as_deref(), Some("/data/attachments/x/a"));
    assert_eq!(item.sha256.as_deref(), Some("deadbeef"));
    assert_eq!(item.size, Some(2048), "size 以落盘字节为准");
    assert_eq!(item.downloaded_at_ms, Some(5_000));
}

#[tokio::test]
async fn parts_that_disappear_are_pruned() {
    let pool = setup().await;
    replace_attachments(
        &pool,
        "m1",
        &[
            attachment("at1", "m1", "1", "a.pdf"),
            attachment("at2", "m1", "2", "b.pdf"),
        ],
        1,
    )
    .await
    .expect("replace");

    replace_attachments(&pool, "m1", &[attachment("at2", "m1", "2", "b.pdf")], 2)
        .await
        .expect("replace");

    let list = list_attachments(&pool, "m1").await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "at2");
}

#[tokio::test]
async fn items_without_part_key_are_skipped() {
    let pool = setup().await;
    let mut orphan = attachment("at9", "m1", "1", "a.pdf");
    orphan.part_key = None;

    let written = replace_attachments(&pool, "m1", &[orphan], 1)
        .await
        .expect("replace");

    assert_eq!(written, 0);
    assert!(
        list_attachments(&pool, "m1")
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn inline_listing_only_returns_inline_parts() {
    let pool = setup().await;
    let mut image = attachment("at1", "m1", "1.1", "logo.png");
    image.mime = Some("image/png".into());
    image.inline = true;
    image.content_id = Some("logo@example.com".into());
    let mut file = attachment("at2", "m1", "2", "报告.pdf");

    replace_attachments(&pool, "m1", &[image, file.clone()], 1)
        .await
        .expect("replace");

    let inline = list_inline_attachments(&pool, "m1").await.expect("inline");
    assert_eq!(inline.len(), 1);
    assert_eq!(inline[0].content_id.as_deref(), Some("logo@example.com"));

    assert_eq!(
        count_pending_downloads(&pool, "m1").await.expect("count"),
        2
    );

    mark_downloaded(&pool, "m1", "at2", "/tmp/x", "abc", 10, 2)
        .await
        .expect("mark");
    assert_eq!(
        count_pending_downloads(&pool, "m1").await.expect("count"),
        1
    );

    // 同一个部件只留一条（唯一索引 + upsert）。
    file.filename = Some("报告-v2.pdf".into());
    replace_attachments(&pool, "m1", &[file], 3)
        .await
        .expect("replace");
    assert_eq!(list_attachments(&pool, "m1").await.expect("list").len(), 1);
}

#[tokio::test]
async fn get_attachment_is_scoped_to_its_message() {
    let pool = setup().await;
    seed(&pool, "m2").await;
    replace_attachments(&pool, "m1", &[attachment("at1", "m1", "1", "a.pdf")], 1)
        .await
        .expect("replace");

    let found = get_attachment(&pool, "m1", "at1").await.expect("get");
    assert!(found.is_some());

    // 换个 message_id 就取不到——命令层用它防止跨邮件「借道」取附件。
    let cross = get_attachment(&pool, "m2", "at1").await.expect("get");
    assert!(cross.is_none());
}

#[tokio::test]
async fn marking_an_unknown_attachment_reports_false() {
    let pool = setup().await;

    let marked = mark_downloaded(&pool, "m1", "nope", "/tmp/x", "abc", 1, 1)
        .await
        .expect("mark");

    assert!(!marked);
}

#[tokio::test]
async fn created_at_survives_a_refetch_while_downloaded_at_keeps_its_value() {
    let pool = setup().await;

    // 第一次见到这个部件：created_at = 100，还没下载。
    replace_attachments(&pool, "m1", &[attachment("at1", "m1", "1", "a.pdf")], 100)
        .await
        .expect("replace");
    let first = attachment_timestamps(&pool, "m1", "at1")
        .await
        .expect("timestamps")
        .expect("row");
    assert_eq!(first.created_at, Some(100), "第一次写入就记下创建时间");
    assert!(first.downloaded_at.is_none());

    // 下载完成：downloaded_at = 200。
    mark_downloaded(&pool, "m1", "at1", "/data/x", "abc", 7, 200)
        .await
        .expect("mark");

    // 重拉正文（同一部件、元数据有变）：created_at 与 downloaded_at 都不能被改写。
    replace_attachments(
        &pool,
        "m1",
        &[attachment("at1", "m1", "1", "a-v2.pdf")],
        300,
    )
    .await
    .expect("replace again");
    let after = attachment_timestamps(&pool, "m1", "at1")
        .await
        .expect("timestamps")
        .expect("row");
    assert_eq!(after.created_at, Some(100), "重拉不能刷新创建时间");
    assert_eq!(after.downloaded_at, Some(200), "重拉不能刷新下载时间");
}

#[tokio::test]
async fn a_parsed_size_never_overrides_the_size_on_disk() {
    let pool = setup().await;

    let mut file = attachment("at1", "m1", "2", "big.zip");
    file.size = Some(999_999);
    replace_attachments(&pool, "m1", &[file], 1)
        .await
        .expect("replace");

    // 落盘完成：实际是 1024 字节（传输编码解出来的真实长度）。
    mark_downloaded(&pool, "m1", "at1", "/data/x", "abc", 1024, 10)
        .await
        .expect("mark");

    // 重拉：解析侧又报 999_999（比如服务器换了编码、或者邮件被改了）。
    let mut again = attachment("at1", "m1", "2", "big.zip");
    again.size = Some(999_999);
    replace_attachments(&pool, "m1", &[again], 20)
        .await
        .expect("replace again");

    let list = list_attachments(&pool, "m1").await.expect("list");
    assert_eq!(
        list[0].size,
        Some(1024),
        "已经下载过的行，size 以落盘字节为准，不能被解析长度覆盖"
    );
}

#[tokio::test]
async fn a_downloaded_part_keeps_its_download_state_even_if_the_caller_passes_other_values() {
    // 「重拉正文不丢下载状态」原本只靠 `ON CONFLICT DO UPDATE` 的 SET 列表里
    // **没有**那几列。那是写给读者看的约定，数据库里没有任何东西拦住它；
    // 而 0008 明确要求 `fetched` 与 `local_path` 同时成立，不留半截状态。
    //
    // 现在这条语义钉在 SQL 里：已下载的行，`local_path` / `sha256` / `fetched` /
    // `downloaded_at` 无论入参传什么都不变。
    let pool = setup().await;
    replace_attachments(&pool, "m1", &[attachment("at1", "m1", "1", "a.pdf")], 1)
        .await
        .expect("replace");
    mark_downloaded(&pool, "m1", "at1", "/data/x/a", "abc123", 10, 2)
        .await
        .expect("mark");

    // 解析侧「以为」这个部件没下载过，并且带着另一套下载字段。
    let mut repulled = attachment("at1", "m1", "1", "a-renamed.pdf");
    repulled.local_path = Some("/somewhere/else".into());
    repulled.sha256 = Some("zzz".into());
    repulled.fetched = false;
    repulled.downloaded_at_ms = Some(99);
    replace_attachments(&pool, "m1", &[repulled], 3)
        .await
        .expect("replace again");

    let list = list_attachments(&pool, "m1").await.expect("list");
    let item = &list[0];
    assert_eq!(
        item.filename.as_deref(),
        Some("a-renamed.pdf"),
        "元数据照常刷新"
    );
    assert!(item.fetched, "已下载的部件不能被重拉翻成未下载");
    assert_eq!(item.local_path.as_deref(), Some("/data/x/a"));
    assert_eq!(item.sha256.as_deref(), Some("abc123"));
    assert_eq!(item.downloaded_at_ms, Some(2));

    // 反方向：新行**不能**靠入参「假装已下载」——下载状态只能由
    // `mark_downloaded` 写，否则会出现「fetched=1 但盘上没有这个文件」。
    let mut fake = attachment("at2", "m1", "2", "b.pdf");
    fake.local_path = Some("/nope".into());
    fake.fetched = true;
    replace_attachments(
        &pool,
        "m1",
        &[attachment("at1", "m1", "1", "a.pdf"), fake],
        4,
    )
    .await
    .expect("insert fake");
    let fresh = get_attachment(&pool, "m1", "at2")
        .await
        .expect("get")
        .expect("at2 应当存在");
    assert!(
        !fresh.fetched && fresh.local_path.is_none(),
        "新行被写成了「已下载」而文件并不存在：fetched={} path={:?}",
        fresh.fetched,
        fresh.local_path
    );
}
