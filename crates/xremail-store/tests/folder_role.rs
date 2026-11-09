//! `folder.role`（RFC 6154 特殊用途角色）在存储层的行为。
//!
//! 这一列存在的**唯一理由**是「删除 = 进垃圾箱」不再靠文件夹名去猜
//! （R04 残余）。所以这里的断言都围绕两件事：
//!
//! * 角色能**如实**往返（写入、读出、被下一次同步覆盖）；
//! * `NULL` 的语义是「**还不知道**」，升级上来的老行就是 `NULL`，
//!   读取方必须能看见这个「不知道」，而不是被一个默认值糊弄过去。
//!
//! 还有一条最要紧的：**升级路径**。0010 会作用在已经有数据的库上，
//! 老数据必须一个字节都不变地留下来，只多出一列 `NULL`。

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, Folder, ProviderKind, ServerConfig, SmtpAuth, SyncMode, TlsMode,
};
use xremail_store::dao::{account, folder};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory, open_pool_path};

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
        username: format!("{id}@example.com"),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 0,
        updated_at: 0,
    }
}

fn sample_folder(id: &str, account_id: &str, remote_name: &str) -> Folder {
    Folder {
        id: id.to_string(),
        account_id: account_id.to_string(),
        remote_name: remote_name.to_string(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".to_string()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        // 这一列的测试要能看见「还没同步过」的原貌，所以先给 `None`。
        uidvalidity: Some(42),
        role: None,
    }
}

async fn seed_account(pool: &SqlitePool, id: &str) {
    account::upsert_account(pool, &sample_account(id))
        .await
        .expect("写账户失败");
}

// ===========================================================================
// 往返
// ===========================================================================

#[tokio::test]
async fn role_round_trips_through_upsert_list_and_get() {
    let pool = fresh_pool().await;
    seed_account(&pool, "a1").await;

    // 服务器宣告的是 `\Trash`，名字却叫 `Rubbish`——正是「靠名字猜」猜不出来的
    // 那一类。存进去之后，读回来必须**还是 trash**，跟名字没关系。
    let mut trash = sample_folder("f-trash", "a1", "Rubbish");
    trash.role = Some("trash".to_string());
    folder::upsert_folder(&pool, &trash)
        .await
        .expect("写入失败");

    let mut inbox = sample_folder("f-inbox", "a1", "INBOX");
    inbox.role = Some("inbox".to_string());
    folder::upsert_folder(&pool, &inbox)
        .await
        .expect("写入失败");

    let got = folder::get_folder(&pool, "f-trash")
        .await
        .expect("读取失败")
        .expect("文件夹应当还在");
    assert_eq!(got.role.as_deref(), Some("trash"), "get 必须带上 role");
    assert!(got.is_trash());

    let listed = folder::list_folders(&pool, "a1").await.expect("列出失败");
    let listed_trash = listed
        .iter()
        .find(|f| f.id == "f-trash")
        .expect("列表里应当有垃圾箱");
    assert_eq!(
        listed_trash.role.as_deref(),
        Some("trash"),
        "list 必须带上 role——同步层就是靠它找垃圾箱的"
    );
    let listed_inbox = listed
        .iter()
        .find(|f| f.id == "f-inbox")
        .expect("列表里应当有收件箱");
    assert_eq!(listed_inbox.role.as_deref(), Some("inbox"));
    assert!(!listed_inbox.is_trash(), "INBOX 不是垃圾箱");
}

#[tokio::test]
async fn role_is_replaced_by_the_next_sync() {
    // 服务器换了个说法（名字没变，属性变了），本地必须跟着改：
    // 角色是**每次 LIST 的观测**，不是一次认定就永远不变的身份。
    let pool = fresh_pool().await;
    seed_account(&pool, "a1").await;

    let mut f = sample_folder("f1", "a1", "Trash");
    f.role = Some("trash".to_string());
    folder::upsert_folder(&pool, &f).await.expect("写入失败");

    f.role = Some("archive".to_string());
    folder::upsert_folder(&pool, &f).await.expect("更新失败");
    let got = folder::get_folder(&pool, "f1").await.unwrap().unwrap();
    assert_eq!(got.role.as_deref(), Some("archive"));
    assert!(
        !got.is_trash(),
        "服务器说它不是垃圾箱了，按名字猜也不许算数"
    );

    f.role = None;
    folder::upsert_folder(&pool, &f).await.expect("更新失败");
    let got = folder::get_folder(&pool, "f1").await.unwrap().unwrap();
    assert_eq!(
        got.role, None,
        "这一轮没认出来 → 写回 NULL，而不是保留上一次的结论"
    );
}

// ===========================================================================
// NULL 的语义
// ===========================================================================

#[tokio::test]
async fn unknown_role_is_null_not_a_default() {
    // 没有角色就是 `NULL`。任何「给个默认值」的做法都会把「不知道」
    // 伪装成一个结论，从此没法把两者分开——所以这里直接读原始列。
    let pool = fresh_pool().await;
    seed_account(&pool, "a1").await;

    folder::upsert_folder(&pool, &sample_folder("f1", "a1", "其他文件夹"))
        .await
        .expect("写入失败");

    let raw: Option<String> = sqlx::query_scalar("SELECT role FROM folder WHERE id = 'f1'")
        .fetch_one(&pool)
        .await
        .expect("读 role 失败");
    assert_eq!(raw, None, "没认出来的文件夹，role 必须是 NULL");

    let got = folder::get_folder(&pool, "f1").await.unwrap().unwrap();
    assert_eq!(got.role, None);
    assert!(
        !got.is_trash(),
        "NULL 只说明「不知道」，不能当成「是垃圾箱」——否则删除会挪错地方"
    );
}

#[tokio::test]
async fn check_constraint_rejects_an_unknown_role() {
    // 取值集合要和 `folders.rs` 的产出一致；大小写不同（`TRASH`）也不行——
    // 它不会报错，只会在比较时静默不命中，删除于是悄悄退化成就地标删。
    let pool = fresh_pool().await;
    seed_account(&pool, "a1").await;

    let err =
        sqlx::query("INSERT INTO folder (id, account_id, remote_name, role) VALUES (?, ?, ?, ?)")
            .bind("bad")
            .bind("a1")
            .bind("Rubbish")
            .bind("TRASH")
            .execute(&pool)
            .await
            .expect_err("不在取值集合里的 role 应当被 CHECK 拒绝");

    let sqlx::Error::Database(db) = &err else {
        panic!("期望数据库约束冲突，实际是: {err}");
    };
    assert!(db.is_check_violation(), "应当是 CHECK 冲突: {err}");
}

#[tokio::test]
async fn every_role_the_protocol_can_produce_is_accepted() {
    // 这份清单来自 `xremail-protocol-imap` 的 `detect_role` / `role_from_attributes`
    // / `role_by_name`。协议层新增角色而忘了改迁移时，这条会红。
    let pool = fresh_pool().await;
    seed_account(&pool, "a1").await;

    for (index, role) in [
        "inbox", "sent", "drafts", "trash", "junk", "archive", "flagged", "all",
    ]
    .into_iter()
    .enumerate()
    {
        let mut f = sample_folder(&format!("f{index}"), "a1", role);
        f.role = Some(role.to_string());
        folder::upsert_folder(&pool, &f)
            .await
            .unwrap_or_else(|error| panic!("role = {role} 应当被接受: {error}"));
    }
}

// ===========================================================================
// 升级路径：没有该列的旧库
// ===========================================================================

fn test_db_path(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new("target").join("test-dbs");
    std::fs::create_dir_all(&dir).expect("建目录失败");
    let path = dir.join(format!("{name}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

/// 往**老 schema**（还没有 `role` 列的库）里塞一个文件夹。
///
/// 不能用 `folder::upsert_folder`：现在的 DAO 会写 `role`，而老库还没有那一列——
/// 这恰恰是「升级前」的正常状态，所以这里按老库真实的列清单手工插。
async fn insert_old_folder(pool: &SqlitePool, id: &str, account_id: &str, remote_name: &str) {
    sqlx::query(
        "INSERT INTO folder (id, account_id, remote_name, subscribed, selectable, uidvalidity) \
         VALUES (?, ?, ?, 1, 1, 42)",
    )
    .bind(id)
    .bind(account_id)
    .bind(remote_name)
    .execute(pool)
    .await
    .expect("写入老文件夹失败");
}

/// 把库造成「升级前」的样子：schema 与迁移记录都停在 **0009**。
///
/// 用正式迁移器的 `run_to(9)`，而不是自己把 SQL 跑一遍再手工登记：
/// 迁移记录表里存的是每条迁移的 SHA-384 校验和，**校验和由迁移器自己算**，
/// 手工登记的任何占位值都会让下一次 `migrate()` 报
/// 「previously applied but has been modified」，那就测不到真正的升级路径了。
async fn migrate_to_before_role(pool: &SqlitePool) {
    // `Migrator::DEFAULT` 是迁移器「空表」的形状，不会去读 `./migrations`，
    // 离线数据（`.sqlx/`）里也查不到它。所以这里用与 `migrate()` 同一个宏
    // 编译期内嵌的迁移集，只是停在 0009。
    sqlx::migrate!()
        .run_to(9, pool)
        .await
        .expect("应用 0001–0009 失败");
}

#[tokio::test]
async fn upgrade_from_an_old_database_keeps_data_and_adds_a_null_role() {
    // 老库：停在 0009，里面已经有账户、文件夹、邮件。
    // 升级（0010）之后：数据必须一个不少，`role` 一律是 NULL。
    let path = test_db_path("folder-role-upgrade");

    {
        let pool = open_pool_path(&path, &PoolConfig::default())
            .await
            .expect("打开文件库失败");

        migrate_to_before_role(&pool).await;
        seed_account(&pool, "a1").await;
        insert_old_folder(&pool, "f-inbox", "a1", "INBOX").await;
        insert_old_folder(&pool, "f-trash", "a1", "Deleted Messages").await;
        sqlx::query(
            "INSERT INTO message (id, account_id, folder_id, uid, uidvalidity) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind("m1")
        .bind("a1")
        .bind("f-inbox")
        .bind(7_i64)
        .bind(42_i64)
        .execute(&pool)
        .await
        .expect("写邮件失败");

        pool.close().await;
    }

    // 升级：走正式的迁移器（与用户升级客户端时一模一样）。
    let pool = open_pool_path(&path, &PoolConfig::default())
        .await
        .expect("重新打开文件库失败");
    migrate(&pool).await.expect("升级迁移失败");

    let folders = folder::list_folders(&pool, "a1")
        .await
        .expect("列出文件夹失败");
    assert_eq!(folders.len(), 2, "老文件夹不能丢也不能多: {folders:?}");
    for f in &folders {
        assert_eq!(
            f.role, None,
            "升级上来的老行 role 必须是 NULL（还没同步过，不知道）"
        );
    }
    // 名字像垃圾箱的那个仍然是 `NULL`：迁移**不该**顺手按名字回填。
    // 回填等于把猜测写成事实，之后再也分不清「服务器说过」与「我们猜的」。
    let trash = folders
        .iter()
        .find(|f| f.id == "f-trash")
        .expect("垃圾箱应当还在");
    assert_eq!(trash.remote_name, "Deleted Messages");
    assert_eq!(trash.role, None);

    // 其余列原样保留。
    let inbox = folders
        .iter()
        .find(|f| f.id == "f-inbox")
        .expect("收件箱应当还在");
    assert_eq!(inbox.uidvalidity, Some(42), "老数据不能被迁移改写");

    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message")
        .fetch_one(&pool)
        .await
        .expect("数邮件失败");
    assert_eq!(messages, 1, "邮件不能被升级弄丢");

    // 升级完就能正常读写 role 了。
    let mut f = folder::get_folder(&pool, "f-trash")
        .await
        .expect("读取失败")
        .expect("文件夹应当还在");
    f.role = Some("trash".to_string());
    folder::upsert_folder(&pool, &f)
        .await
        .expect("升级后写 role 应当成功");
    assert!(
        folder::get_folder(&pool, "f-trash")
            .await
            .unwrap()
            .unwrap()
            .is_trash()
    );

    pool.close().await;
    cleanup(&path);
}

#[tokio::test]
async fn the_role_migration_fails_loudly_instead_of_silently_when_replayed() {
    // 「迁移可以重复应用」在这个仓库里有一个**物理上限**，这条用例把它钉死，
    // 免得以后有人以为自己做到了、或者悄悄改坏了：
    //
    // SQLite 没有 `ALTER TABLE … ADD COLUMN IF NOT EXISTS`，也没有语句级 `IF`——
    // `CASE` 只能决定**表达式**算不算，决定不了**语句**跑不跑（把 `ALTER` 塞进
    // `CASE` 是语法错误，`probe()` 只能让参数不被求值）。所以「列已存在就跳过
    // ALTER」在 SQL 层做不到。
    //
    // 迁移器本身（`_sqlx_migrations` 按版本记账）保证**正常升级路径上**这一版
    // 只会跑一次，见上一条用例。这条用例管的是剩下那种：有人**手动**把同一段
    // SQL 再跑一遍。这种情况下唯一不可接受的结果是「说了成功、数据却坏了」，
    // 可以接受的结果是「明确报错，而已经写好的 role 原样还在」。
    let path = test_db_path("folder-role-replay");

    let pool = open_pool_path(&path, &PoolConfig::default())
        .await
        .expect("打开文件库失败");
    migrate_to_before_role(&pool).await;
    seed_account(&pool, "a1").await;
    insert_old_folder(&pool, "f1", "a1", "Rubbish").await;

    migrate(&pool).await.expect("升级迁移失败");

    let mut f = folder::get_folder(&pool, "f1").await.unwrap().unwrap();
    assert_eq!(f.role, None);
    f.role = Some("trash".to_string());
    folder::upsert_folder(&pool, &f)
        .await
        .expect("写 role 失败");

    // 手动重跑同一段 SQL：必须报错（`duplicate column name`），不能静默。
    let replay = sqlx::raw_sql(include_str!("../migrations/0010_folder_role.sql"))
        .execute(&pool)
        .await;
    let error = replay.expect_err("重放 ADD COLUMN 必须失败，而不是悄悄跳过");
    assert!(
        error.to_string().contains("duplicate column"),
        "失败原因要说清是列已存在，实际: {error}"
    );

    // 失败之后库里的一切照旧：数据不丢、role 不被冲掉、表结构不变。
    let got = folder::get_folder(&pool, "f1")
        .await
        .expect("读取失败")
        .expect("文件夹应当还在");
    assert_eq!(
        got.role.as_deref(),
        Some("trash"),
        "重放失败不能冲掉已经写好的 role"
    );
    assert_eq!(got.remote_name, "Rubbish");

    // 迁移器本身也不该被这段重跑搞乱：再 `migrate` 一次照常无操作。
    migrate(&pool).await.expect("重复 migrate 应当无操作");

    pool.close().await;
    cleanup(&path);
}
