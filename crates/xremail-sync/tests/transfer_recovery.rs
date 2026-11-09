//! 移动回写的**第二轮**：上一轮已经把源端删掉了，探测还认不认得出「已经搬过了」。
//!
//! ## 为什么这件事要单独测
//!
//! `copy+delete` 路由（163 那一档：有 `UIDPLUS`、没有 `MOVE`）**不幂等**：
//! 重发一次 `COPY`，目标文件夹里就多一份。所以 `attempts > 0` 时，
//! `transfer_push` 要先拿 `Message-ID` 去目标文件夹里问一句「上一轮那份在不在」——
//! 探到了就跳过 `COPY`，只补完源端的删除；探不到就不许重发。
//!
//! 判据的**输入**是 `message_dao::find_message_id_by_uid(account, folder, epoch, uid)`，
//! 查的是**源文件夹的本地行**。而恰好在这条路径上，源文件夹的本地行只有两种命运：
//!
//! 1. **上一轮提交成功了**（`transfer_push` 里「服务器确认搬走 → 删源端本地行」）——
//!    那一轮并没有结束这条操作：写完目标行之后 `select_for_write` 切回源文件夹，
//!    **切不回来**就把剩下的（含它自己）记成 `uncertain`。这时源端本地行已经没了。
//! 2. 上一轮没走到提交（`uncertain` 来自别处）——源端本地行还在。
//!
//! 于是第 1 种情况下的下一次尝试必然「查不到 `Message-ID`」→
//! 「无法确认 → 不重发」→ 再记一次 `uncertain` → `attempts` + 1。
//! 每轮同步都会重复一遍，直到 `attempts` 到顶被对账收成
//! `failed / retry_exhausted`，文案是「服务器一直拒绝这条意图」——
//! 可服务器从来没拒绝过，那封信**早就搬好了**。
//!
//! ## 正确性要求
//!
//! 「查不到 `Message-ID`」只说明**我们自己不知道**，不说明「再发一次会导致副本」。
//! `COPY` 的目标是一封**服务器上还在的源邮件**：要发 `COPY` 必须先通过
//! `submit` 里的 `source_has_uid` 检查——源已经不在时那一步直接返回
//! `AlreadyGone`（本来就是本条路由的保守处理）。所以「无法确认」应该收成
//! `uncertain`（**没有证据说它执行过**，与「上一轮真的发过 COPY」区分开），
//! 而不是在 `attempts` 上再烧一次配额、把一条已经成功的搬运判死。

use std::time::Duration;

use sqlx::SqlitePool;
use xremail_core::model::{
    Account, AuthKind, FlagIntentPayload, Folder, MailOperation, OperationKind, OperationState,
    ProviderKind, ServerConfig as DomainServerConfig, SyncMode,
};
use xremail_protocol_imap::{ImapConfig, Session, TlsTrust, connect_and_login_with};
use xremail_store::dao::{
    account as account_dao, folder as folder_dao, message as message_dao,
    operation as operation_dao,
};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::SyncOptions;
use xremail_sync::transfer_push::push_transfers_as_with_options;
use xremail_testkit::{
    FakeImapServer, MailboxFixture, MessageFixture, ServerConfig as FakeServerConfig,
};

const INBOX_UID: i64 = 1_792_658_702;
const OTHER_UID: i64 = 1_792_658_701;
const INBOX_LOCAL: &str = "folder-1";
const ARCHIVE_LOCAL: &str = "folder-archive";
/// 探测要用到的命令超时：短，好让用例跑得快。
const TIMEOUT: Duration = Duration::from_secs(5);

/// 假服务器 + 客户端配置。
struct Fixture {
    server: FakeImapServer,
    imap: ImapConfig,
    trust: TlsTrust,
}

async fn start(config: FakeServerConfig) -> Fixture {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(10);

    let trust = TlsTrust::Roots(server.root_store());

    Fixture {
        server,
        imap,
        trust,
    }
}

/// 163 那一档：有 `UIDPLUS`、**没有** `MOVE` —— 走 `COPY` + 标删 + `EXPUNGE`。
fn config_with_mail() -> FakeServerConfig {
    FakeServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(
                    MessageFixture::new(OTHER_UID as u32)
                        .with_flag("\\Seen")
                        .with_subject("留着的一封"),
                )
                .with_message(MessageFixture::new(INBOX_UID as u32).with_subject("要被搬走的一封")),
            MailboxFixture::new("Archive"),
        ],
        ..FakeServerConfig::default()
    }
}

async fn login(fixture: &Fixture) -> Session {
    connect_and_login_with(
        &fixture.imap,
        &fixture.trust,
        "tester@example.com",
        "test-password",
    )
    .await
    .expect("登录假服务器失败")
    .0
}

async fn local_store() -> (SqlitePool, Folder) {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");

    let account = Account {
        id: "acct-1".to_string(),
        email: "tester@example.com".to_string(),
        provider: ProviderKind::Generic,
        display_name: None,
        color: None,
        auth_kind: AuthKind::Password,
        server: DomainServerConfig::new("imap.example.com", 993, "smtp.example.com", 465),
        username: "tester@example.com".to_string(),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 0,
        updated_at: 0,
    };
    account_dao::upsert_account(&pool, &account)
        .await
        .expect("插入账户失败");

    let inbox = make_folder(INBOX_LOCAL, "INBOX");
    folder_dao::upsert_folder(&pool, &inbox)
        .await
        .expect("插入 INBOX 失败");
    folder_dao::upsert_folder(&pool, &make_folder(ARCHIVE_LOCAL, "Archive"))
        .await
        .expect("插入 Archive 失败");

    (pool, inbox)
}

fn make_folder(id: &str, remote_name: &str) -> Folder {
    Folder {
        id: id.to_string(),
        account_id: "acct-1".to_string(),
        remote_name: remote_name.to_string(),
        remote_id: None,
        local_name: None,
        delimiter: Some("/".to_string()),
        subscribed: true,
        selectable: true,
        last_sync_at: None,
        last_uid: None,
        highest_modseq: None,
        uidvalidity: None,
        role: None,
    }
}

/// 让本地 INBOX 与 Archive 都已经是「同步过」的状态（epoch 取服务器此刻的值）。
async fn seed_synced_folders(pool: &SqlitePool, fixture: &Fixture) -> Folder {
    let epoch = i64::from(fixture.server.uid_validity());

    let mut inbox = make_folder(INBOX_LOCAL, "INBOX");
    inbox.uidvalidity = Some(epoch);
    inbox.last_sync_at = Some(1_000);
    folder_dao::upsert_folder(pool, &inbox)
        .await
        .expect("写入 INBOX 失败");

    let mut archive = make_folder(ARCHIVE_LOCAL, "Archive");
    archive.uidvalidity = Some(epoch);
    archive.last_sync_at = Some(1_000);
    folder_dao::upsert_folder(pool, &archive)
        .await
        .expect("写入 Archive 失败");

    inbox
}

async fn enqueue_move(pool: &SqlitePool, folder: &Folder, now_ms: i64) -> String {
    let id = format!("op-move-{now_ms}");
    let operation = MailOperation {
        id: id.clone(),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: INBOX_UID,
        uidvalidity: folder.uidvalidity.expect("测试里应当已经同步过"),
        kind: OperationKind::Move,
        intent: FlagIntentPayload::default(),
        destination_folder_id: Some(ARCHIVE_LOCAL.to_string()),
        state: OperationState::Queued,
        seq: 0,
        base_version: None,
        attempts: 0,
        error_kind: None,
        error_message: None,
        created_at: now_ms,
        updated_at: now_ms,
    };
    operation_dao::enqueue_operation(pool, &operation)
        .await
        .expect("入队应当成功");
    id
}

fn options() -> SyncOptions {
    SyncOptions {
        command_timeout: TIMEOUT,
    }
}

/// 本地 INBOX 里还有没有这一封（探测取 `Message-ID` 就靠它）。
async fn local_has_source(pool: &SqlitePool) -> bool {
    message_dao::list_messages(pool, "acct-1", INBOX_LOCAL)
        .await
        .expect("读本地邮件失败")
        .iter()
        .any(|message| message.uid == INBOX_UID)
}

/// 目标文件夹里已经不重复：整条用例的「不变量」。
fn archive_count(fixture: &Fixture) -> usize {
    fixture
        .server
        .mailboxes()
        .into_iter()
        .find(|mailbox| mailbox.name == "Archive")
        .map(|mailbox| mailbox.messages.len())
        .unwrap_or(0)
}

#[tokio::test]
async fn a_retried_copy_move_finishes_when_the_source_row_is_already_gone() {
    // 场景（上一轮的遗留）：`COPY` 已经生效、源端也已经标删+EXPUNGE，
    // 本地源端行也删了——只有目标文件夹本地那一行没写成，于是操作停在 `uncertain`。
    // 此刻服务器上：INBOX 里没有它，Archive 里有它。
    let mut config = config_with_mail();
    // 服务器上的现状：INBOX 里没有它了，Archive 里有一份**同 `Message-ID`** 的副本
    // ——那就是上一轮 `COPY` 的成果（后面逐字节对照过 INBOX 那一封的头）。
    let inbox_copy = config
        .mailboxes
        .iter()
        .find(|mailbox| mailbox.name == "INBOX")
        .and_then(|mailbox| mailbox.messages.iter().find(|m| m.uid == INBOX_UID as u32))
        .cloned()
        .expect("INBOX 里应当有那封");
    for mailbox in &mut config.mailboxes {
        if mailbox.name == "INBOX" {
            mailbox.messages.retain(|m| m.uid != INBOX_UID as u32);
        }
        if mailbox.name == "Archive" {
            mailbox
                .messages
                .push(MessageFixture::new(900).with_subject("要被搬走的一封"));
            if let Some(last) = mailbox.messages.last_mut() {
                last.content = inbox_copy.content.clone();
            }
        }
    }
    let fixture = start(config).await;
    let (pool, _) = local_store().await;
    let mut session = login(&fixture).await;
    let folder = seed_synced_folders(&pool, &fixture).await;

    // 本地源端行不在（上一轮提交成功后删的）——这正是「查不到 Message-ID」的样子。
    assert!(!local_has_source(&pool).await, "前提：源端本地行已经没了");

    let op_id = enqueue_move(&pool, &folder, 2_000).await;
    // 上一轮试过一次：`attempts = 1`（认领不自增，失败才自增）、`started_at`
    // 还在租约内（不会被对账回收）。
    sqlx::query(
        "UPDATE mail_operation SET state = 'uncertain', attempts = 1, started_at = ? WHERE id = ?",
    )
    .bind(2_500_i64)
    .bind(&op_id)
    .execute(&pool)
    .await
    .expect("模拟上一轮的结果失败");

    let report =
        push_transfers_as_with_options(&pool, &mut session, &folder, 3_000, "boot-a", options())
            .await
            .expect("回写本身不该报错");

    let (state, error_kind, attempts) = operation_outcome(&pool, &op_id).await;
    assert_eq!(
        archive_count(&fixture),
        1,
        "目标文件夹里不能出现第二份：{report:?}"
    );
    assert_eq!(state, "uncertain", "没有证据说搬完了，就不许判成功");
    assert_eq!(
        attempts, 1,
        "这一轮一条命令都没发出去（源里根本没有这封），却把 `attempts` 从 1 烧到 {attempts}\
         （error_kind = {error_kind:?}）。每轮同步都会这样烧，五次之后对账把它收成 \
         `failed / retry_exhausted`，文案是「服务器一直拒绝这条意图」——\
         可服务器一次都没拒绝过，信也早就搬好了"
    );
    fixture.server.shutdown().await;
}

/// 反向保护：真的发过 `COPY`、探测又断了的那条路，**必须**照旧计次。
///
/// 计数点收窄只能收窄到「确定没有命令飞出去」的那一支；一旦挪宽，R26 的病就回来了：
/// 一条探测一直失败的 `COPY` 会被无限重试，目标文件夹里一份变两份。
#[tokio::test]
async fn an_unreachable_probe_after_a_real_copy_still_counts_as_an_attempt() {
    // 服务器上有源邮件（上一轮的 `COPY` 可能已经生效），本地也有源端那一行
    // （所以 `Message-ID` 查得到），但**目标文件夹打不开**（探测失败）。
    let mut config = config_with_mail();
    // 目标文件夹根本不存在：`EXAMINE` 回 `NO`，探测拿不到答案。
    //
    // 为什么不用 `reject_examine`：那台服务器连**拉取**都只能 `SELECT`，
    // 本地就建不起源端那一行，用例也就测不到「真可能发过 `COPY`」这一支。
    for mailbox in &mut config.mailboxes {
        mailbox.name = match mailbox.name.as_str() {
            "Archive" => "Archive-Gone".to_string(),
            other => other.to_string(),
        };
    }
    let fixture = start(config).await;
    let (pool, _) = local_store().await;
    let mut session = login(&fixture).await;
    let folder = seed_synced_folders(&pool, &fixture).await;
    seed_source_row(&pool, &folder).await;
    assert!(local_has_source(&pool).await, "前提：本地有源端那一行");

    let op_id = enqueue_move(&pool, &folder, 2_000).await;
    sqlx::query("UPDATE mail_operation SET state = 'uncertain', attempts = 1 WHERE id = ?")
        .bind(&op_id)
        .execute(&pool)
        .await
        .expect("模拟上一轮的结果失败");

    let report =
        push_transfers_as_with_options(&pool, &mut session, &folder, 3_000, "boot-a", options())
            .await
            .expect("回写本身不该报错");

    let (state, error_kind, attempts) = operation_outcome(&pool, &op_id).await;
    assert_eq!(report.uncertain, 1, "探测失败 → 不重发、记 uncertain");
    assert_eq!(state, "uncertain", "error_kind = {error_kind:?}");
    assert_eq!(
        attempts, 2,
        "这一轮真有可能发过 `COPY`，必须照旧计次，否则会无限重发、目标里出多份副本"
    );
    fixture.server.shutdown().await;
}

/// 在本地 INBOX 里写一行「源端那封」：`Message-ID` 与服务器上的一致。
async fn seed_source_row(pool: &SqlitePool, folder: &Folder) {
    let message = xremail_core::model::Message {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        uid: INBOX_UID,
        uidvalidity: folder.uidvalidity.expect("要先有 epoch"),
        // 假服务器给每封邮件拼的 `Message-ID` 就是这一形状（见 `xremail-testkit`）。
        message_id: Some(format!("<fixture-{INBOX_UID}@example.com>")),
        subject: Some("要被搬走的一封".to_string()),
        snippet: None,
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        date_sent: None,
        internal_date: None,
        flags: xremail_core::model::MessageFlags::empty(),
        size: None,
        has_attachment: false,
    };
    message_dao::upsert_message(pool, &message)
        .await
        .expect("写本地行失败");
}

/// 本地库的状态、认领时间与 `error_message`（断言用）。
async fn operation_outcome(pool: &SqlitePool, id: &str) -> (String, Option<String>, i64) {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT state, error_kind, attempts, started_at FROM mail_operation WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("应当有这条操作");
    (
        row.get::<String, _>("state"),
        row.get::<Option<String>, _>("error_kind"),
        row.get::<i64, _>("attempts"),
    )
}
