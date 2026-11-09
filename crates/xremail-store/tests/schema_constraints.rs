//! 建库时真正落进 SQLite 的约束（R51）。
//!
//! ## 为什么要有这一个文件
//!
//! `dao` 里每一个 `ON CONFLICT (...)`、`WHERE state IN (...)` 都**假定**
//! schema 里那一条唯一索引 / 那个取值集合存在。SQLite 有三个特性让这种假定
//! 很容易变成幻觉：
//!
//! * `ALTER TABLE … ADD COLUMN` 加不了 `UNIQUE`——只能事后 `CREATE UNIQUE INDEX`；
//! * `CREATE UNIQUE INDEX` 对 `NULL` 是「互不相等」，部分索引更是直接跳过不合格的行；
//! * 表上根本没有 `CHECK` 时，任何字符串都写得进去，`enum_from_text` 只会在**读**
//!   的时候才报「未知枚举值」——那时一条永远读不出来的行已经躺在库里了。
//!
//! 所以这里不看 DAO、也不看迁移的注释，只看 `PRAGMA index_list` / `PRAGMA index_info`：
//! 建完库之后，**数据库自己**记住了什么。

use sqlx::SqlitePool;
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};

/// 建一个跑完全部迁移的内存库。
async fn migrated() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("应用迁移失败");
    pool
}

/// 某个表上所有**唯一**索引覆盖的列（按列顺序，逗号拼接）。
///
/// 用 `PRAGMA index_list` + `PRAGMA index_info` 而不是 `sqlite_master.sql`
/// 的文本：前者是 SQLite 解析后的结论，后者还要自己去读 SQL。
///
/// `PRAGMA` 不接受绑定参数，表名/索引名只能拼进 SQL；sqlx 0.9 因此要求显式
/// `AssertSqlSafe`。拼进去的字符串全部来自 `sqlite_master`（SQLite 自己给的标识符），
/// 没有一个来自用户输入。
async fn unique_indexes(pool: &SqlitePool, table: &str) -> Vec<String> {
    let list_sql = format!("PRAGMA index_list('{table}')");
    let list = sqlx::query(sqlx::AssertSqlSafe(list_sql))
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| panic!("读 {table} 的索引列表失败：{e}"));

    let mut out = Vec::new();
    for row in list {
        use sqlx::Row;
        let unique: i64 = row.get("unique");
        if unique == 0 {
            continue;
        }
        let name: String = row.get("name");
        let info_sql = format!("PRAGMA index_info('{name}')");
        let info = sqlx::query(sqlx::AssertSqlSafe(info_sql))
            .fetch_all(pool)
            .await
            .expect("读索引列失败");
        let columns: Vec<String> = info
            .iter()
            .map(|cell| cell.get::<String, _>("name"))
            .collect();
        out.push(columns.join(","));
    }
    out
}

#[tokio::test]
async fn the_unique_index_the_upserts_rely_on_really_exists() {
    // 两张表的 upsert 都写死了冲突键；冲突键对应的唯一约束一旦不在，
    // `ON CONFLICT (...)` 会直接报错（而不是悄悄失效），这条用例就是那道闸。
    let pool = migrated().await;

    let message = unique_indexes(&pool, "message").await;
    assert!(
        message
            .iter()
            .any(|cols| cols == "account_id,folder_id,uidvalidity,uid"),
        "message 上缺 (account_id, folder_id, uidvalidity, uid) 的唯一约束：{message:?}"
    );

    let operation = unique_indexes(&pool, "mail_operation").await;
    assert!(
        operation
            .iter()
            .any(|cols| cols == "account_id,folder_id,seq"),
        "mail_operation 上缺 (account_id, folder_id, seq) 的唯一索引：{operation:?}"
    );

    let folder = unique_indexes(&pool, "folder").await;
    assert!(
        folder.iter().any(|cols| cols == "account_id,remote_name"),
        "folder 上缺 (account_id, remote_name) 的唯一约束：{folder:?}"
    );
    assert!(
        folder.iter().any(|cols| cols == "account_id,id"),
        "folder 上缺 (account_id, id)——message 的复合外键靠它：{folder:?}"
    );
}

#[tokio::test]
async fn an_empty_idempotency_key_does_not_reserve_the_slot() {
    // 部分索引的 `WHERE idempotency_key <> ''`：空串根本不进索引。
    // 这意味着**两条空串**可以共存——写库之前必须把空串归一成 NULL，
    // 否则「同一封逻辑邮件只能有一行」在空键这一档上完全不设防。
    //
    // 这里只钉住 DB 层的语义（DAO 层的归一在 `send_queue.rs` 的用例里）。
    let pool = migrated().await;
    seed_account(&pool).await;

    for (id, key) in [("s1", ""), ("s2", "")] {
        sqlx::query(
            r#"
            INSERT INTO send_queue (id, account_id, envelope, headers, state,
                                    idempotency_key, created_at, updated_at)
            VALUES (?, 'a1', '{"from":"a1@example.com","recipients":[],"bcc":[]}',
                    '{"to":[],"cc":[],"subject":"","inReplyTo":null,"references":null}',
                    'draft', ?, 1, 1)
            "#,
        )
        .bind(id)
        .bind(key)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("空幂等键的 {id} 写不进去：{e}"));
    }

    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM send_queue WHERE idempotency_key = ''")
            .fetch_one(&pool)
            .await
            .expect("计数失败");
    assert_eq!(
        rows, 2,
        "空串在唯一索引之外（部分索引的条件），所以它们能共存——这正是为什么 \
         DAO 必须写 NULL 而不是空串"
    );
}

/// 种一个最小账户（`send_queue.account_id` 有外键）。
async fn seed_account(pool: &SqlitePool) {
    sqlx::query(
        r#"
        INSERT INTO account (
            id, email, provider, auth_kind, imap_host, imap_port, imap_tls,
            smtp_host, smtp_port, smtp_tls, smtp_auth, username, sync_mode,
            poll_interval_secs, created_at, updated_at
        ) VALUES ('a1', 'a1@example.com', 'generic', 'password',
                  'imap.example.com', 993, 'implicit',
                  'smtp.example.com', 465, 'implicit', 'login',
                  'a1@example.com', 'poll', 300, 1, 1)
        "#,
    )
    .execute(pool)
    .await
    .expect("种账户失败");
}

#[tokio::test]
async fn an_unknown_state_cannot_be_written_at_all() {
    // 状态列没有 CHECK（SQLite 的 `ALTER TABLE` 加不了约束），取值集合原先只写在
    // 注释里。写错一个值的后果不是「写入时报错」，而是**读的时候**整张表都读不出来：
    // `enum_from_text` 遇到未知值返回 `Invalid`，而 `list_pending_operations`
    // 是一行一行解码的——一条坏行就让整个文件夹的队列看不见。
    // 0001 / 0011 的注释里把这件事说成「schema 层挡住了」，这里钉住它真的挡住了。
    let pool = migrated().await;
    seed_account(&pool).await;

    let bad_operation = sqlx::query(
        r#"
        INSERT INTO mail_operation (
            id, account_id, folder_id, uid, uidvalidity, kind, intent, state,
            created_at, updated_at
        ) VALUES ('op-bad', 'a1', 'f1', 1, 1, 'flags', '{"add":0,"remove":0}',
                  'paused', 1, 1)
        "#,
    )
    .execute(&pool)
    .await;
    assert!(
        bad_operation.is_err(),
        "mail_operation.state 写进了未知取值：读的时候整张表的解码都会失败"
    );

    let bad_send = sqlx::query(
        r#"
        INSERT INTO send_queue (id, account_id, envelope, headers, state,
                                idempotency_key, created_at, updated_at)
        VALUES ('s-bad', 'a1', '{"from":"a1@example.com","recipients":[],"bcc":[]}',
                '{"to":[],"cc":[],"subject":"","inReplyTo":null,"references":null}',
                'sending', NULL, 1, 1)
        "#,
    )
    .execute(&pool)
    .await;
    assert!(bad_send.is_err(), "send_queue.state 写进了未知取值");

    // 合法取值照常写得进（别把守卫写成「一律拒绝」）。
    sqlx::query(
        r#"
        INSERT INTO mail_operation (
            id, account_id, folder_id, uid, uidvalidity, kind, intent, state,
            created_at, updated_at
        ) VALUES ('op-ok', 'a1', 'f1', 1, 1, 'flags', '{"add":0,"remove":0}',
                  'queued', 1, 1)
        "#,
    )
    .execute(&pool)
    .await
    .expect("合法状态应当写得进去");

    // UPDATE 也必须被挡（只看 INSERT 的话，`update_operation_state` 这类路径没护栏）。
    let bad_update = sqlx::query("UPDATE mail_operation SET state = 'done' WHERE id = 'op-ok'")
        .execute(&pool)
        .await;
    assert!(bad_update.is_err(), "UPDATE 绕过了状态取值守卫");
}
