//! `xremail-store` 的本地全文搜索测试。
//!
//! 覆盖的不变量：
//! * 索引**跟着 `message` 表走**：写邮件 / 删邮件，索引自动同步。
//! * trigram 分词器在自带的 SQLite 里可用，中文短语能搜到。
//! * 短于 3 个字符的查询词走 LIKE 退化路径，语义仍是子串匹配。
//! * 用户输入里的 FTS5 语法符号**不会**被当成语法。
//! * 同一封邮件重复索引不产生重复结果。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, Address, AuthKind, Folder, Message, MessageBody, MessageFlags, ProviderKind,
    ServerConfig, SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::{account, folder, message, search};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory, open_pool_path};

/// 建一个已应用迁移的内存库。
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
        from: vec![Address {
            name: Some("发件人".to_string()),
            email: "alice@example.com".to_string(),
        }],
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

fn body_of(message_id: &str, text: &str) -> MessageBody {
    MessageBody {
        message_id: message_id.to_string(),
        text_plain: Some(text.to_string()),
        // 原始 HTML 是噪音，不该进索引。
        html_raw: Some("<html><body><p>ignored</p></body></html>".to_string()),
        charset: Some("utf-8".to_string()),
    }
}

/// 建一个账户 + 文件夹，并写入三封邮件（英文 / 中文 / 只含 foo）。
///
/// 正文刻意避开含 `or` 的英文单词（over / for / work …）：
/// LIKE 路径对 ASCII 大小写不敏感，`%or%` 会命中它们，
/// 那会让「OR 是字面量」的用例说不清。
async fn seed(pool: &SqlitePool) {
    account::upsert_account(pool, &sample_account("acct"))
        .await
        .expect("写账户失败");
    folder::upsert_folder(pool, &sample_folder("fld", "acct"))
        .await
        .expect("写文件夹失败");

    let mut m1 = sample_message("m1", "acct", "fld", 1);
    m1.subject = Some("Quarterly budget review".to_string());
    m1.to = vec![Address::new("bob@example.com")];
    message::upsert_message(pool, &m1)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(
        pool,
        &body_of("m1", "the quick brown fox jumped past a lazy dog"),
    )
    .await
    .expect("写正文失败");

    let mut m2 = sample_message("m2", "acct", "fld", 2);
    m2.subject = Some("项目进度报告".to_string());
    m2.from = vec![Address {
        name: Some("张三".to_string()),
        email: "zhang@example.com".to_string(),
    }];
    m2.to = vec![Address::new("li@example.com")];
    message::upsert_message(pool, &m2)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(
        pool,
        &body_of("m2", "这是一封关于项目进度的中文邮件，预算超支需要讨论。"),
    )
    .await
    .expect("写正文失败");

    let mut m3 = sample_message("m3", "acct", "fld", 3);
    m3.subject = Some("foo only".to_string());
    // 收件人跟 m1 不同：m1 的收件人是 bob@example.com，
    // 否则「按收件人搜」和「OR 是字面量」两个用例会互相干扰。
    m3.to = vec![Address::new("dave@example.com")];
    message::upsert_message(pool, &m3)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(pool, &body_of("m3", "foo and nothing else"))
        .await
        .expect("写正文失败");
}

// ===========================================================================
// 分词器与 SQLite 版本
// ===========================================================================

#[tokio::test]
async fn bundled_sqlite_has_trigram_tokenizer() {
    let pool = fresh_pool().await;

    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&pool)
        .await
        .expect("读版本失败");
    // trigram 分词器自 SQLite 3.34 起提供。
    let major: i64 = version.split('.').next().unwrap().parse().unwrap();
    assert!(
        major >= 3,
        "自带的 SQLite 版本过低，trigram 不可用: {version}"
    );

    // 真建一张 trigram 表并验证中文短语能命中。
    sqlx::query("CREATE VIRTUAL TABLE probe USING fts5(body, tokenize='trigram')")
        .execute(&pool)
        .await
        .expect("建 trigram 表失败：自带的 SQLite 不支持 trigram 分词器");
    sqlx::query("INSERT INTO probe (body) VALUES ('项目进度说明')")
        .execute(&pool)
        .await
        .expect("写探针失败");

    let hits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM probe WHERE probe MATCH '\"项目进度\"'")
            .fetch_one(&pool)
            .await
            .expect("查探针失败");
    assert_eq!(hits, 1, "trigram 索引应当能按中文短语命中");
}

// ===========================================================================
// 基本检索
// ===========================================================================

/// 往**0001 的 schema**（没有 `folder.role`）里塞一个文件夹。
async fn insert_old_schema_folder(pool: &SqlitePool, id: &str, account_id: &str) {
    sqlx::query(
        "INSERT INTO folder (id, account_id, remote_name, subscribed, selectable) \
         VALUES (?, ?, 'INBOX', 1, 1)",
    )
    .bind(id)
    .bind(account_id)
    .execute(pool)
    .await
    .expect("写文件夹失败");
}

#[tokio::test]
async fn existing_mail_is_backfilled_on_upgrade() {
    // 老库升级场景：邮件在 0004 迁移**之前**就已经落库，
    // 触发器管不到它们，只能靠迁移里的回填。
    let dir = std::path::Path::new("target").join("test-dbs");
    std::fs::create_dir_all(&dir).expect("建目录失败");
    let db_path = dir.join(format!("search-upgrade-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db_path);

    let pool = open_pool_path(&db_path, &PoolConfig::default())
        .await
        .expect("打开文件库失败");

    // 只应用基础迁移（还没有搜索表）。
    sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
        .execute(&pool)
        .await
        .expect("应用 0001 失败");

    account::upsert_account(&pool, &sample_account("acct"))
        .await
        .expect("写账户失败");
    // 文件夹只能用**0001 的列**手工插：DAO 现在会写 `role`（0010 才加），
    // 而这个库故意停在 0001 —— 这正是「升级前」该有的样子。
    insert_old_schema_folder(&pool, "fld", "acct").await;
    let mut m1 = sample_message("m1", "acct", "fld", 1);
    m1.subject = Some("Quarterly budget review".to_string());
    message::upsert_message(&pool, &m1)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(&pool, &body_of("m1", "the quick brown fox"))
        .await
        .expect("写正文失败");

    // 升级：应用搜索迁移。
    sqlx::raw_sql(include_str!("../migrations/0004_message_search.sql"))
        .execute(&pool)
        .await
        .expect("应用 0004 失败");

    let hits = search::search(&pool, "quick", 10).await.expect("搜索失败");
    assert_eq!(hits.len(), 1, "老邮件应当被回填进索引: {hits:?}");

    pool.close().await;
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

#[tokio::test]
async fn english_word_is_found() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let hits = search::search(&pool, "quick", 10).await.expect("搜索失败");

    assert_eq!(hits.len(), 1, "应当只命中 m1: {hits:?}");
    assert_eq!(hits[0].message_id, "m1");
}

#[tokio::test]
async fn chinese_phrase_is_found() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 「项目进度」是 4 个字符，走 trigram 索引。
    let hits = search::search(&pool, "项目进度", 10)
        .await
        .expect("搜索失败");

    assert_eq!(hits.len(), 1, "应当只命中 m2: {hits:?}");
    assert_eq!(hits[0].message_id, "m2");
}

#[tokio::test]
async fn chinese_two_char_falls_back_to_like() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 「预算」只有 2 个字符，trigram 服务不了，走 LIKE 扫描。
    let hits = search::search(&pool, "预算", 10).await.expect("搜索失败");

    assert_eq!(hits.len(), 1, "应当只命中 m2: {hits:?}");
    assert_eq!(hits[0].message_id, "m2");
}

#[tokio::test]
async fn recipient_address_is_found() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 收件人地址在 people 列里。
    let hits = search::search(&pool, "bob@example.com", 10)
        .await
        .expect("搜索失败");

    assert_eq!(hits.len(), 1, "应当只命中 m1: {hits:?}");
    assert_eq!(hits[0].message_id, "m1");
}

#[tokio::test]
async fn subject_is_searchable() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let hits = search::search(&pool, "Quarterly", 10)
        .await
        .expect("搜索失败");

    assert_eq!(hits.len(), 1, "应当只命中 m1: {hits:?}");
}

#[tokio::test]
async fn empty_query_returns_no_hits() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    assert!(
        search::search(&pool, "", 10)
            .await
            .expect("搜索失败")
            .is_empty()
    );
    assert!(
        search::search(&pool, "   \t ", 10)
            .await
            .expect("搜索失败")
            .is_empty()
    );
}

// ===========================================================================
// 与 message 表的同步
// ===========================================================================

#[tokio::test]
async fn index_follows_message_writes_without_explicit_indexing() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 只写了 message / message_body，没调过 index_message：
    // 索引由触发器同步，应当立刻能搜到。
    let hits = search::search(&pool, "中文邮件", 10)
        .await
        .expect("搜索失败");

    assert_eq!(hits.len(), 1, "应当命中 m2: {hits:?}");
}

#[tokio::test]
async fn deleted_message_is_no_longer_found() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 先确认能搜到。
    assert!(
        !search::search(&pool, "quick", 10)
            .await
            .expect("搜索失败")
            .is_empty()
    );

    // 走 DAO 的删除路径：索引必须跟着一起消失。
    message::delete_messages_by_uids(&pool, "acct", "fld", 42, &[1])
        .await
        .expect("删邮件失败");

    let hits = search::search(&pool, "quick", 10).await.expect("搜索失败");
    assert!(hits.is_empty(), "邮件删掉后不该再搜到: {hits:?}");
}

#[tokio::test]
async fn reindexing_does_not_duplicate_hits() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 同一封邮件重复索引两次。
    search::index_message(
        &pool,
        "m1",
        "Quarterly budget review",
        "alice@example.com bob@example.com",
        "the quick brown fox jumped past a lazy dog",
    )
    .await
    .expect("索引失败");
    search::index_message(
        &pool,
        "m1",
        "Quarterly budget review",
        "alice@example.com bob@example.com",
        "the quick brown fox jumped past a lazy dog",
    )
    .await
    .expect("索引失败");

    let hits = search::search(&pool, "quick", 10).await.expect("搜索失败");

    assert_eq!(hits.len(), 1, "重复索引不该产生重复结果: {hits:?}");
}

#[tokio::test]
async fn removed_message_is_no_longer_found() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    search::remove_message(&pool, "m1").await.expect("摘除失败");

    let hits = search::search(&pool, "quick", 10).await.expect("搜索失败");
    assert!(hits.is_empty(), "摘除后不该再搜到: {hits:?}");
}

// ===========================================================================
// FTS5 语法符号必须按字面处理
// ===========================================================================

#[tokio::test]
async fn fts5_operators_are_not_treated_as_syntax() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 一封正文里**字面**写着 "foo OR bar" 的邮件。
    let mut m4 = sample_message("m4", "acct", "fld", 4);
    m4.subject = Some("literal".to_string());
    message::upsert_message(&pool, &m4)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(&pool, &body_of("m4", "foo OR bar"))
        .await
        .expect("写正文失败");

    let hits = search::search(&pool, "foo OR bar", 10)
        .await
        .expect("搜索失败");

    let ids: Vec<&str> = hits.iter().map(|h| h.message_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["m4"],
        "OR 只能当字面量：只有正文里写着 \"foo OR bar\" 的那封该命中: {hits:?}"
    );
}

#[tokio::test]
async fn fts5_keyword_and_is_not_treated_as_syntax() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let mut m5 = sample_message("m5", "acct", "fld", 5);
    m5.subject = Some("literal".to_string());
    message::upsert_message(&pool, &m5)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(&pool, &body_of("m5", "foo AND bar"))
        .await
        .expect("写正文失败");

    // 「AND」有 3 个字符，走 FTS5 路径——但它被引号包着，是字面量。
    let hits = search::search(&pool, "foo AND bar", 10)
        .await
        .expect("搜索失败");

    let ids: Vec<&str> = hits.iter().map(|h| h.message_id.as_str()).collect();
    assert_eq!(ids, vec!["m5"], "AND 只能当字面量: {hits:?}");
}

#[tokio::test]
async fn fts5_special_characters_do_not_break_the_query() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let mut m6 = sample_message("m6", "acct", "fld", 6);
    m6.subject = Some("literal".to_string());
    message::upsert_message(&pool, &m6)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(&pool, &body_of("m6", "foo*bar baz-qux a:b"))
        .await
        .expect("写正文失败");

    // 星号按字面处理：命中含 "foo*bar" 的那封，不命中只有 "foo" 的 m3。
    let hits = search::search(&pool, "foo*", 10).await.expect("搜索失败");
    let ids: Vec<&str> = hits.iter().map(|h| h.message_id.as_str()).collect();
    assert_eq!(ids, vec!["m6"], "星号不该被当成前缀语法: {hits:?}");

    // 连字号按字面处理。
    let hits = search::search(&pool, "baz-qux", 10)
        .await
        .expect("搜索失败");
    let ids: Vec<&str> = hits.iter().map(|h| h.message_id.as_str()).collect();
    assert_eq!(ids, vec!["m6"], "连字号不该被当成减号语法: {hits:?}");

    // 冒号按字面处理。
    let hits = search::search(&pool, "a:b", 10).await.expect("搜索失败");
    let ids: Vec<&str> = hits.iter().map(|h| h.message_id.as_str()).collect();
    assert_eq!(ids, vec!["m6"], "冒号不该被当成列过滤: {hits:?}");

    // 引号按字面处理：不能把查询拆坏，也不能报错。
    // m1 的正文是 "the quick brown fox jumped past a lazy dog"，
    // 正文里没有引号，所以 `\"quick\"` 不该命中 m1。
    let mut m7 = sample_message("m7", "acct", "fld", 7);
    m7.subject = Some("literal".to_string());
    m7.to = vec![Address::new("erin@example.com")];
    message::upsert_message(&pool, &m7)
        .await
        .expect("写邮件失败");
    message::upsert_message_body(&pool, &body_of("m7", "say \"quick\" please"))
        .await
        .expect("写正文失败");

    let hits = search::search(&pool, "\"quick\"", 10)
        .await
        .expect("搜索失败");
    let ids: Vec<&str> = hits.iter().map(|h| h.message_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["m7"],
        "引号不该改变匹配语义：只有正文里字面写着 \"quick\" 的那封该命中: {hits:?}"
    );
}

// ===========================================================================
// 片段
// ===========================================================================

#[tokio::test]
async fn snippet_marks_the_hit_word() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let hits = search::search(&pool, "项目进度", 10)
        .await
        .expect("搜索失败");

    assert_eq!(hits.len(), 1);
    let snippet = &hits[0].snippet;
    assert!(
        snippet.contains("**项目进度**"),
        "片段应标出命中词: {snippet}"
    );
    assert!(snippet.chars().count() <= 260, "片段过长: {snippet}");
}

// ===========================================================================
// 重建与一致性检查
//
// 这两个函数是**兜底**：正常路径靠触发器同步索引，用不上它们。
// 有了它们，「索引与 message 表脱节」这件事才有得救、也才能查出来。
// ===========================================================================

#[tokio::test]
async fn rebuild_restores_a_broken_index() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 模拟索引被弄坏：直接删掉文档行（触发器管不到这种直接操作）。
    sqlx::query("DELETE FROM message_search_doc")
        .execute(&pool)
        .await
        .expect("清空文档表失败");

    let before = search::search(&pool, "项目进度", 10)
        .await
        .expect("搜索失败");
    assert!(before.is_empty(), "索引坏了就该搜不到");

    let count = search::rebuild_index(&pool).await.expect("重建失败");
    assert_eq!(count, 3, "应该重建出 3 封");

    let after = search::search(&pool, "项目进度", 10)
        .await
        .expect("搜索失败");
    assert_eq!(after.len(), 1, "重建后应该搜得到：{after:?}");
}

#[tokio::test]
async fn rebuild_is_repeatable() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let first = search::rebuild_index(&pool).await.expect("重建失败");
    let second = search::rebuild_index(&pool).await.expect("重建失败");

    assert_eq!(first, second, "重建两次结果应该一样");

    let hits = search::search(&pool, "项目进度", 10)
        .await
        .expect("搜索失败");
    assert_eq!(hits.len(), 1, "重建不该产生重复命中：{hits:?}");
}

#[tokio::test]
async fn consistency_check_is_quiet_on_a_healthy_index() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    let problems = search::check_index_consistency(&pool)
        .await
        .expect("检查失败");
    assert!(
        problems.is_empty(),
        "健康的索引不该报问题，实际：{problems:?}"
    );
}

#[tokio::test]
async fn consistency_check_finds_a_missing_index_row() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    sqlx::query("DELETE FROM message_search_doc WHERE message_id = 'm2'")
        .execute(&pool)
        .await
        .expect("删文档行失败");

    let problems = search::check_index_consistency(&pool)
        .await
        .expect("检查失败");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("漏索引") && p.contains("m2")),
        "该报出 m2 漏索引，实际：{problems:?}"
    );
}

#[tokio::test]
async fn consistency_check_finds_a_stale_body() {
    let pool = fresh_pool().await;
    seed(&pool).await;

    // 正文变了，但索引里的 body 没跟上（这正是触发器要防的情况，
    // 这里手工制造它，验证检查能查出来）。
    sqlx::query("UPDATE message_search_doc SET body = '旧正文' WHERE message_id = 'm2'")
        .execute(&pool)
        .await
        .expect("改文档行失败");

    let problems = search::check_index_consistency(&pool)
        .await
        .expect("检查失败");
    assert!(
        problems
            .iter()
            .any(|p| p.contains("正文过期") && p.contains("m2")),
        "该报出 m2 正文过期，实际：{problems:?}"
    );
}
