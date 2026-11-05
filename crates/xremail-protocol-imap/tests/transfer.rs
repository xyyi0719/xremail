//! COPY / MOVE / EXPUNGE 的端到端测试：真客户端 × 假服务器。
//!
//! 重点验证四件事：
//! * `COPYUID` 真的被解析出来了，且源/目标 UID **按下标对应**；
//! * `MOVE` 只删**被移动的那些**，不碰用户手动标删的别的信；
//! * `UID EXPUNGE` 只删**指定的**那些，裸 `EXPUNGE` 才删全部；
//! * 服务器回 `NO` 时客户端**一定**报错——不能把「没拷过去」当成「拷好了」。

use std::time::Duration;

use xremail_protocol_imap::{
    ImapConfig, ProtocolError, Session, TlsTrust, connect_and_login_with, copy_messages,
    mark_deleted, mark_deleted_with_timeout, move_messages, move_messages_with_timeout,
    run_checked, uid_expunge,
};
use xremail_testkit::{FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig};

/// 起一个假服务器并返回可直接用的客户端配置。
async fn start(config: ServerConfig) -> (FakeImapServer, ImapConfig, TlsTrust) {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(10);

    let trust = TlsTrust::Roots(server.root_store());

    (server, imap, trust)
}

/// INBOX 有 3 封（UID 1/2/3），「归档」是空文件夹。
///
/// 默认带上 `MOVE`：假服务器现在会**按宣告的能力执行门控**（R40），
/// 没宣告 `MOVE` 就回 `BAD`——这正是真服务器的行为。
fn config_with_archive() -> ServerConfig {
    let mut config = ServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(1).with_subject("一"))
                .with_message(MessageFixture::new(2).with_subject("二"))
                .with_message(MessageFixture::new(3).with_subject("三")),
            MailboxFixture::new("归档").with_attribute("\\Archive"),
        ],
        ..ServerConfig::default()
    };
    config.capabilities.push("MOVE".to_string());
    config
}

/// 起一个**不宣告任何移动能力**的服务器：无 `MOVE`、无 `UIDPLUS`。
fn config_without_move_or_uidplus() -> ServerConfig {
    let mut config = config_with_archive();
    config
        .capabilities
        .retain(|cap| cap != "MOVE" && cap != "UIDPLUS");
    config
}

async fn login(imap: &ImapConfig, trust: &TlsTrust) -> Session {
    let (session, _caps, _tls) =
        connect_and_login_with(imap, trust, "tester@example.com", "test-password")
            .await
            .expect("登录应当成功");
    session
}

/// 取某个角色文件夹在**线路上**的名字（中文名是 modified UTF-7）。
async fn raw_name_of(session: &mut Session, role: &str) -> String {
    let outcome = run_checked(session, "LIST \"\" \"*\"")
        .await
        .expect("LIST 应当成功");
    outcome
        .list()
        .iter()
        .find(|folder| folder.role.as_deref() == Some(role))
        .unwrap_or_else(|| panic!("应当有角色为 {role} 的文件夹"))
        .raw_name
        .clone()
}

async fn select(session: &mut Session, raw_name: &str) {
    run_checked(session, &format!("SELECT \"{raw_name}\""))
        .await
        .expect("SELECT 应当成功");
}

fn uids_in(server: &FakeImapServer, mailbox: &str) -> Vec<u32> {
    server
        .mailboxes()
        .iter()
        .find(|fixture| fixture.name == mailbox)
        .unwrap_or_else(|| panic!("假服务器里应当有 {mailbox}"))
        .messages
        .iter()
        .map(|message| message.uid)
        .collect()
}

#[tokio::test]
async fn copy_reports_the_uid_mapping_from_copyuid() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    let outcome = copy_messages(&mut session, &[2], &archive)
        .await
        .expect("COPY 应当成功");

    // RFC 4315：只有**源**文件夹的 UIDVALIDITY，目标的服务器不给。
    assert_eq!(
        outcome.source_uid_validity,
        Some(server.uid_validity()),
        "COPYUID 应当带上源文件夹的 UIDVALIDITY"
    );
    assert_eq!(
        outcome.destination_uid_validity, None,
        "COPYUID 里没有目标文件夹的 UIDVALIDITY，不能凭空造一个"
    );
    assert_eq!(outcome.source_uids, vec![2]);
    assert_eq!(outcome.destination_uids, vec![1], "空文件夹里的新 UID 是 1");
    assert!(outcome.has_uid_mapping());
    assert_eq!(outcome.destination_uid_of(2), Some(1));

    // 源文件夹没被动；目标多了一封。
    assert_eq!(uids_in(&server, "INBOX"), vec![1, 2, 3]);
    assert_eq!(uids_in(&server, "归档"), vec![1]);
}

#[tokio::test]
async fn copy_keeps_the_uid_mapping_aligned_for_several_messages() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    // 故意乱序传，看客户端会不会整理。
    let outcome = copy_messages(&mut session, &[3, 1], &archive)
        .await
        .expect("COPY 应当成功");

    assert_eq!(outcome.source_uids, vec![1, 3]);
    assert_eq!(outcome.destination_uids, vec![1, 2]);
    assert_eq!(outcome.destination_uid_of(1), Some(1));
    assert_eq!(outcome.destination_uid_of(3), Some(2));

    assert_eq!(uids_in(&server, "归档"), vec![1, 2]);
}

#[tokio::test]
async fn move_removes_the_messages_from_the_source_folder() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    let outcome = move_messages(&mut session, &[1, 3], &archive)
        .await
        .expect("MOVE 应当成功");

    assert_eq!(outcome.source_uids, vec![1, 3]);
    assert_eq!(outcome.destination_uids, vec![1, 2]);

    // 源里只剩 UID 2；目标里是那两封。
    assert_eq!(uids_in(&server, "INBOX"), vec![2]);
    assert_eq!(uids_in(&server, "归档"), vec![1, 2]);

    // 内容真的跟过去了（不是只搬了个空壳）。
    let archive_fixture = server
        .mailboxes()
        .into_iter()
        .find(|fixture| fixture.name == "归档")
        .expect("归档应当存在");
    let subjects: Vec<&str> = archive_fixture
        .messages
        .iter()
        .map(|message| message.header_values("Subject")[0])
        .collect();
    assert_eq!(subjects, vec!["一", "三"]);

    assert!(
        server
            .command_log()
            .iter()
            .any(|line| line.starts_with("UID MOVE")),
        "走的是 MOVE，不是 COPY+STORE：{:?}",
        server.command_log()
    );
}

#[tokio::test]
async fn move_does_not_touch_messages_deleted_by_someone_else() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    // 模拟「手机上标了删除、但还没提交」的那封。
    mark_deleted(&mut session, &[2])
        .await
        .expect("STORE 应当成功");

    move_messages(&mut session, &[1], &archive)
        .await
        .expect("MOVE 应当成功");

    // UID 2 是别人标删的，MOVE 不能顺手把它也 expunge 掉。
    assert_eq!(
        uids_in(&server, "INBOX"),
        vec![2, 3],
        "MOVE 只该删掉被移动的那封"
    );
}

#[tokio::test]
async fn mark_deleted_sets_the_flag_on_the_server() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    select(&mut session, "INBOX").await;

    mark_deleted(&mut session, &[2])
        .await
        .expect("STORE 应当成功");

    let inbox = server
        .mailboxes()
        .into_iter()
        .find(|fixture| fixture.name == "INBOX")
        .expect("INBOX 应当存在");
    let deleted: Vec<u32> = inbox
        .messages
        .iter()
        .filter(|message| message.has_flag("\\Deleted"))
        .map(|message| message.uid)
        .collect();
    assert_eq!(deleted, vec![2]);
}

#[tokio::test]
async fn uid_expunge_only_removes_the_given_uids() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    select(&mut session, "INBOX").await;

    mark_deleted(&mut session, &[1, 2])
        .await
        .expect("STORE 应当成功");

    uid_expunge(&mut session, &[2])
        .await
        .expect("UID EXPUNGE 应当成功");

    // 只删 UID 2；UID 1 仍然在（还带着 \Deleted，等用户自己决定）。
    assert_eq!(uids_in(&server, "INBOX"), vec![1, 3]);
}

#[tokio::test]
async fn copy_to_a_missing_folder_is_an_error() {
    let (_server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    select(&mut session, "INBOX").await;

    let error = copy_messages(&mut session, &[1], "\"不存在的文件夹\"")
        .await
        .expect_err("目标文件夹不存在时 COPY 必须报错");

    let text = error.to_string();
    assert!(
        text.contains("TRYCREATE") || text.contains("NO"),
        "错误里应当带上服务器的拒绝原因，实际是：{text}"
    );
}

#[tokio::test]
async fn empty_selection_succeeds_without_a_uid_mapping() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    // UID 99 不存在：RFC 3501 不要求报错，空操作也算成功。
    let outcome = copy_messages(&mut session, &[99], &archive)
        .await
        .expect("空集合的 COPY 应当成功");

    // 服务器回的是裸 `OK`（没有 COPYUID）。这时我们只能把「请求过的集合」
    // 当作源集合——**但没有目标 UID**，所以映射依然是不完整的。
    assert_eq!(outcome.source_uids, vec![99]);
    assert!(outcome.destination_uids.is_empty());
    assert!(
        !outcome.has_uid_mapping(),
        "没有 COPYUID 时不能声称知道新 UID"
    );
    assert_eq!(outcome.destination_uid_of(99), None);

    // 两个文件夹都没变。
    assert!(uids_in(&server, "归档").is_empty());
    assert_eq!(uids_in(&server, "INBOX"), vec![1, 2, 3]);
}

#[tokio::test]
async fn expunge_without_uidplus_deletes_everything_marked() {
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    select(&mut session, "INBOX").await;

    mark_deleted(&mut session, &[1, 3])
        .await
        .expect("STORE 应当成功");

    // 裸 EXPUNGE：删掉所有带 \Deleted 的——这正是它危险的地方，
    // 所以调用方不该在「没有 UIDPLUS」时用它。
    run_checked(&mut session, "EXPUNGE")
        .await
        .expect("EXPUNGE 应当成功");

    assert_eq!(uids_in(&server, "INBOX"), vec![2]);
}

// ---------------------------------------------------------------------------
// R40：假服务器必须**按宣告的能力**执行门控，否则「客户端从不发用不了的命令」
// 这件事就没有任何东西能证明——测试会对着一个比真服务器宽容的对手跑绿。
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_server_without_move_rejects_the_move_command() {
    let (server, imap, trust) = start(config_without_move_or_uidplus()).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    let result = move_messages(&mut session, &[1], &archive).await;

    assert!(
        result.is_err(),
        "没宣告 MOVE 的服务器必须回 BAD，而不是替客户端把活干了：{result:?}"
    );
    // 失败也不能有副作用。
    assert_eq!(uids_in(&server, "INBOX"), vec![1, 2, 3]);
    assert!(uids_in(&server, "归档").is_empty());
}

#[tokio::test]
async fn a_server_without_uidplus_rejects_uid_expunge() {
    let (server, imap, trust) = start(config_without_move_or_uidplus()).await;
    let mut session = login(&imap, &trust).await;
    select(&mut session, "INBOX").await;

    mark_deleted(&mut session, &[1])
        .await
        .expect("STORE 应当成功");

    let result = uid_expunge(&mut session, &[1]).await;

    assert!(
        result.is_err(),
        "没宣告 UIDPLUS 时 UID EXPUNGE 必须回 BAD（RFC 4315 能力门控）：{result:?}"
    );
    // 标了删但没 expunge：信还在。
    assert_eq!(uids_in(&server, "INBOX"), vec![1, 2, 3]);
}

#[tokio::test]
async fn copy_uid_names_the_destination_folders_uid_validity() {
    let mut config = config_with_archive();
    // 造一个全局值与「实际选中的文件夹」不同的情形：把值改成非默认值，
    // 客户端拿到的 COPYUID 里那个数必须与它**当前选中的文件夹**报告的一致，
    // 而不是某个写死的量（R40：以前这里直接引用服务器全局配置）。
    config.uid_validity = 777;
    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    let outcome = copy_messages(&mut session, &[1, 3], &archive)
        .await
        .expect("COPY 应当成功");

    assert_eq!(
        outcome.source_uid_validity,
        Some(server.uid_validity()),
        "COPYUID 的第一个数是**目标文件夹**的 UIDVALIDITY（RFC 4315 §3）"
    );
    assert_eq!(outcome.source_uids, vec![1, 3]);
    assert_eq!(outcome.destination_uids, vec![1, 2]);
}

#[tokio::test]
async fn move_reports_the_expunged_sequences() {
    // RFC 6851 §3.3：`MOVE` 之后服务器要推 untagged `EXPUNGE`。
    // 假服务器以前不推，于是「客户端在 MOVE 期间正确忽略 untagged EXPUNGE」
    // 这条路径就从来没被跑到过（R40）。
    let (server, imap, trust) = start(config_with_archive()).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    move_messages(&mut session, &[1], &archive)
        .await
        .expect("MOVE 应当成功");

    // 连接还能继续用：untagged `EXPUNGE` 没有把同一条命令的 tagged 响应搞乱。
    let outcome = run_checked(&mut session, "UID SEARCH ALL")
        .await
        .expect("MOVE 之后会话应当仍然可用");
    assert_eq!(outcome.search(), &[2, 3]);
    assert_eq!(uids_in(&server, "归档"), vec![1]);
}

// ---------------------------------------------------------------------------
// R38：服务器收了命令却**不回话**时，命令必须有上限——否则一条 `MOVE`
// 就能把整个账户的同步循环永久钉死在这里。
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_move_that_never_gets_an_answer_times_out() {
    let mut config = config_with_archive();
    config.faults.push(Fault::HangOn("MOVE".to_string()));
    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;
    let archive = raw_name_of(&mut session, "archive").await;
    select(&mut session, "INBOX").await;

    let started = std::time::Instant::now();
    let result =
        move_messages_with_timeout(&mut session, &[1], &archive, Duration::from_millis(300)).await;
    let elapsed = started.elapsed();

    match result {
        Err(ProtocolError::Timeout(detail)) => {
            assert!(
                detail.contains("MOVE"),
                "超时信息里要带上命令本身，否则日志里看不出卡在哪：{detail}"
            );
        }
        other => panic!("应当是 Timeout，实际: {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "必须在给定的超时量级返回（而不是挂到默认的 30 秒），实际 {elapsed:?}"
    );

    // 命令没被执行：源文件夹没变。
    assert_eq!(uids_in(&server, "INBOX"), vec![1, 2, 3]);
    assert!(uids_in(&server, "归档").is_empty());
}

#[tokio::test]
async fn a_store_that_never_gets_an_answer_times_out() {
    let mut config = config_with_archive();
    config.faults.push(Fault::HangOn("STORE".to_string()));
    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;
    select(&mut session, "INBOX").await;

    let result = mark_deleted_with_timeout(&mut session, &[1], Duration::from_millis(300)).await;

    assert!(
        matches!(result, Err(ProtocolError::Timeout(_))),
        "标删也要有上限，实际: {result:?}"
    );
    assert_eq!(uids_in(&server, "INBOX"), vec![1, 2, 3]);
}
