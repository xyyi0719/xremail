//! T8 的最后一块：**乱序响应**（`Fault::InterleaveUntagged`）× 真客户端。
//!
//! 真实场景：163 / QQ 这类 Coremail 体系的服务器在处理一条命令的**过程中**，
//! 会顺带把别的变更通知插进来——
//!
//! ```text
//! C: a1 UID FETCH 3 (UID BODY.PEEK[HEADER])
//! S: * 3 FETCH (UID 103 …)            ← 我的数据
//! S: * 5 EXISTS                       ← 别人的通知（插队）
//! S: a1 OK UID FETCH completed        ← 我的结局
//! ```
//!
//! 客户端若按「发一条命令，接下来收到的都是它的响应」来读，插队的这一条
//! 就会混进本次结果、把下一次读串。这里要证明的是：命令层**只认自己的 tag**
//! （ARCHITECTURE §2.1「命令必须自证成败」），穿插的 untagged 一律吞掉。
//!
//! 几个用例跑了**两个**服务器：一个不注入（对照组）、一个注入。
//! 「注入的结果 == 不注入的结果」才算吞干净了；只断言「没报错」是不够的。
//!
//! 为什么默认噪音是 `* <n> EXISTS`（n 取真实邮件数）：多一条它等于多一个空的
//! 状态快照，客户端即便当真也只会得到本来就知道的数；换成 `EXPUNGE` 就会真的
//! 让客户端以为邮件被删。理由写在 `xremail_testkit::InterleavedUntagged` 里。

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use xremail_protocol_imap::command::{CommandOutcome, MailboxInfo, examine_checked, run_checked};
use xremail_protocol_imap::transfer::{
    copy_messages_with_timeout, move_messages_with_timeout, uid_expunge_with_timeout,
};
use xremail_protocol_imap::{ImapConfig, ProtocolError, Session, TlsTrust, connect_and_login_with};
use xremail_testkit::{
    FakeImapServer, Fault, InterleavedUntagged, MailboxFixture, MessageFixture, ServerConfig,
};

/// 起一个假服务器并登录好。
async fn start(config: ServerConfig) -> (FakeImapServer, Session) {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(10);

    let trust = TlsTrust::Roots(server.root_store());
    let (session, _, _) =
        connect_and_login_with(&imap, &trust, "tester@example.com", "test-password")
            .await
            .expect("登录应当成功");

    (server, session)
}

/// INBOX 里 5 封邮件，UID 从 101 到 105（**故意不从 1 开始**：
/// 客户端若把序号当 UID 用，这里立刻露馅）。
fn inbox_with_five() -> MailboxFixture {
    let mut inbox = MailboxFixture::new("INBOX");
    for uid in 101..=105 {
        inbox = inbox.with_message(
            MessageFixture::new(uid)
                .with_subject(&format!("第 {uid} 封"))
                .with_body(&format!("正文 {uid}")),
        );
    }
    inbox
}

fn interleave(command: &str, noise: InterleavedUntagged, once: bool) -> Fault {
    Fault::InterleaveUntagged {
        command: command.to_string(),
        noise,
        once,
    }
}

fn config(faults: Vec<Fault>) -> ServerConfig {
    ServerConfig {
        mailboxes: vec![inbox_with_five(), MailboxFixture::new("已删除")],
        faults,
        ..ServerConfig::default()
    }
}

/// 序号 3 的 UID（INBOX 从 101 开始）。
const SEQ3_UID: u32 = 103;

/// 用**可写**的 `SELECT` 打开 INBOX。
///
/// `EXAMINE` 打开的是只读邮箱，之后的 `STORE` 一律被回 `NO [READ-ONLY]`——
/// 想断言「标记真的写进去了」就必须走 `SELECT`。
async fn select_writable(session: &mut Session) {
    let outcome = run_checked(session, "SELECT \"INBOX\"")
        .await
        .expect("SELECT 应当成功");
    assert_eq!(outcome, CommandOutcome::Empty, "SELECT 不该带回数据");
}

/// 拉一封（只读），返回 `(结果, EXAMINE 拿到的概况)`。
async fn fetch_one(session: &mut Session, uid: u32) -> (CommandOutcome, MailboxInfo) {
    let info = examine_checked(session, "INBOX")
        .await
        .expect("EXAMINE 应当成功");
    let outcome = run_checked(
        session,
        &format!("UID FETCH {uid} (UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])"),
    )
    .await
    .expect("UID FETCH 应当成功");
    (outcome, info)
}

#[tokio::test]
async fn uid_fetch_survives_an_interleaved_untagged_response() {
    let (baseline, mut base_session) = start(config(Vec::new())).await;
    let (expected, expected_info) = fetch_one(&mut base_session, SEQ3_UID).await;

    // 对照组的前提：真拿到了 1 封，而且是 UID 103 那一封。
    assert_eq!(expected.fetches().len(), 1, "对照组就应当只拿到 1 封");
    assert_eq!(expected_info.exists, 5);
    baseline.shutdown().await;

    let (server, mut session) = start(config(vec![interleave(
        "FETCH",
        InterleavedUntagged::Exists,
        false,
    )]))
    .await;

    let (injected, injected_info) = fetch_one(&mut session, SEQ3_UID).await;

    assert_eq!(
        injected, expected,
        "穿插的 untagged 被读进了 UID FETCH 的结果：噪音（`* 5 EXISTS`）不能算作一封邮件"
    );
    assert_eq!(injected_info, expected_info, "EXAMINE 的结果也被噪音带偏了");

    let fetches = injected.fetches();
    assert_eq!(fetches.len(), 1, "只能有 1 封，噪音不能多出一封");
    assert_eq!(fetches[0].seq, 3, "序号必须是 3（INBOX 的第 3 封）");
    assert_eq!(fetches[0].uid, Some(SEQ3_UID), "不能把序号当 UID");
    assert!(
        fetches[0]
            .header
            .as_deref()
            .is_some_and(|head| String::from_utf8_lossy(head).contains("第 103 封")),
        "拿到的必须是第 3 封的头，实际: {:?}",
        fetches[0]
            .header
            .as_ref()
            .map(|h| String::from_utf8_lossy(h))
    );
    // `BODY.PEEK[]` 不该置 `\Seen`：噪音也不能伪造出一个标记变更。
    assert!(!fetches[0].has_flag("\\Seen"));

    server.shutdown().await;
}

/// 噪音里塞一条**像数据**的 `* 3 FETCH (FLAGS (\Seen))`：
/// 它没有 UID、没有内容，混进结果就会多出一封「UID 未知」的邮件。
#[tokio::test]
async fn a_stray_fetch_flag_update_is_not_counted_as_a_fetched_message() {
    let (server, mut session) = start(config(vec![interleave(
        "FETCH",
        InterleavedUntagged::FetchFlags,
        false,
    )]))
    .await;

    let (outcome, _) = fetch_one(&mut session, SEQ3_UID).await;

    let fetches = outcome.fetches();
    assert_eq!(
        fetches.len(),
        1,
        "服务器回了 `* 3 FETCH (FLAGS …)` 的噪音，客户端把它当成了本次结果：{fetches:?}"
    );
    assert_eq!(fetches[0].uid, Some(SEQ3_UID));
    assert!(
        fetches[0].header.is_some(),
        "结果里必须是我们请求的头，而不是那条只有 FLAGS 的记录"
    );

    server.shutdown().await;
}

/// `UID STORE`：穿插的 untagged 不能改变 `run_checked` 的判定，
/// 而后面的**回读**拿到的必须是真生效后的状态（噪音不能伪造标记）。
#[tokio::test]
async fn uid_store_verdict_is_unaffected_by_interleaved_noise() {
    let (server, mut session) = start(config(vec![interleave(
        "STORE",
        InterleavedUntagged::Exists,
        false,
    )]))
    .await;

    select_writable(&mut session).await;

    let outcome = run_checked(&mut session, "UID STORE 101,104 +FLAGS.SILENT (\\Seen)")
        .await
        .expect("UID STORE 应当成功");
    assert_eq!(
        outcome,
        CommandOutcome::Empty,
        "STORE 的 untagged FLAGS 被静默掉了，结果必须保持 Empty（不能被噪音填成别的形态）"
    );

    // 回读：这正是同步引擎「改完再看一眼」的写法（ARCHITECTURE §6）。
    let readback = run_checked(&mut session, "UID FETCH 101:105 (UID FLAGS)")
        .await
        .expect("回读应当成功");
    let readback = readback.fetches();
    assert_eq!(readback.len(), 5, "回读必须看到 5 封，实际: {readback:?}");
    for message in readback {
        let uid = message.uid.expect("回读必须带 UID");
        let expected_seen = uid == 101 || uid == 104;
        assert_eq!(
            message.has_flag("\\Seen"),
            expected_seen,
            "UID {uid} 的 \\Seen 不对——穿插的噪音要么丢了真的标记变更、要么伪造了标记"
        );
    }

    server.shutdown().await;
}

/// 噪音是一条**没有 tag 的 `OK`**。
///
/// 这是最像「完成响应」的一种：只看状态字、不看 tag，就会把一条告警
/// 当成本命令的结局。RFC 3501 里只有 `<tag> OK` 才是结局。
#[tokio::test]
async fn an_untagged_ok_is_not_mistaken_for_the_completion() {
    let (server, mut session) = start(config(vec![interleave(
        "FETCH",
        InterleavedUntagged::OkAlert,
        true,
    )]))
    .await;

    let (outcome, _) = fetch_one(&mut session, SEQ3_UID).await;
    let fetches = outcome.fetches();
    assert_eq!(
        fetches.len(),
        1,
        "没等到自己的 tagged 就收工了：{fetches:?}"
    );
    assert_eq!(fetches[0].uid, Some(SEQ3_UID));
    assert!(
        fetches[0].header.is_some(),
        "数据必须来自 tagged 之前的情形"
    );

    server.shutdown().await;
}

/// 别的 tag 的完成响应插在中间时，**不能**被当成本命令的结果。
///
/// 按 ARCHITECTURE §2.1 直接验证到字节：用**裸连接**发一条命令，
/// 自己插一条 `x9 NO [SERVERBUG] someone else's failure`，服务器照常给本命令回 `OK`。
/// 客户端的读循环必须**跳过**别的 tag、等到自己那条 `OK`——而不是见到第一条
/// tagged 就收工、把别人的 `NO` 当成本命令被拒绝。
///
/// 为什么用裸连接而不是 `Fault`：要插入的那一行必须带上**客户端这次用的 tag**
/// 之外的 tag，而 tag 由客户端生成，只有读线路的人才拿得到。
#[tokio::test]
async fn another_commands_completion_is_ignored_not_adopted() {
    let baseline = FakeImapServer::start(config(Vec::new()))
        .await
        .expect("启动假服务器失败");
    let mut session = raw_session(&baseline).await;
    let clean = uid_fetch_lines(&mut session, None).await;
    baseline.shutdown().await;

    let server = FakeImapServer::start(config(Vec::new()))
        .await
        .expect("启动假服务器失败");
    let mut session = raw_session(&server).await;
    let injected = uid_fetch_lines(
        &mut session,
        Some("x9 NO [SERVERBUG] someone else's failure"),
    )
    .await;

    // 注入的那条确实在线上、而且夹在「我的数据」与「我的结局」之间。
    let fetched = injected
        .iter()
        .position(|line| line.starts_with("* 3 FETCH"))
        .expect("应当有一条 FETCH");
    let stolen = injected
        .iter()
        .position(|line| line.starts_with("x9 NO"))
        .expect("应当插入别的 tag 的 NO");
    let done = injected
        .iter()
        .position(|line| line.starts_with("r3 "))
        .expect("应当有自己的 tagged 响应");
    assert!(
        fetched < stolen && stolen < done,
        "噪声必须插在「我的数据」与「我的结局」之间，实际: {injected:#?}"
    );
    assert!(injected[done].contains("OK"), "本命令确实成功了");

    // 去掉注入的那一行，客户端看到的字节应当与对照组**完全一致**。
    let stripped: Vec<&String> = injected
        .iter()
        .filter(|line| !line.starts_with("x9 "))
        .collect();
    let clean_refs: Vec<&String> = clean.iter().collect();
    assert_eq!(
        stripped, clean_refs,
        "客户端拿到的应当与不注入时一模一样：别的 tag 的结局不能被当成本命令的"
    );

    server.shutdown().await;
}

/// `COPY` 走的是 `transfer` 的读循环（与 `run_checked` 是两条独立的实现）。
#[tokio::test]
async fn copy_verdict_is_unaffected_by_interleaved_noise() {
    let (server, mut session) = start(config(vec![interleave(
        "COPY",
        InterleavedUntagged::Exists,
        true,
    )]))
    .await;

    examine_checked(&mut session, "INBOX")
        .await
        .expect("EXAMINE 应当成功");

    let outcome =
        copy_messages_with_timeout(&mut session, &[102], "已删除", Duration::from_secs(10))
            .await
            .expect("COPY 应当成功");
    assert_eq!(
        outcome.source_uids,
        vec![102],
        "服务器没给 COPYUID 时源集合应当回落到请求的那几个"
    );

    // 副本真的到了目标文件夹。
    let destination = server
        .mailboxes()
        .into_iter()
        .find(|mailbox| mailbox.name == "已删除")
        .expect("应当有「已删除」文件夹");
    assert_eq!(destination.messages.len(), 1, "目标文件夹里应当有 1 封");
    // 假服务器宣告了 `UIDPLUS`，所以目标 UID 由服务器重新分配（这里是 1），
    // 并随 `COPYUID` 告诉客户端——`TransferOutcome` 里应当能查到这个映射。
    assert_eq!(
        outcome.destination_uid_of(102),
        Some(destination.messages[0].uid),
        "COPYUID 的映射必须与目标文件夹里的真实 UID 对得上"
    );

    server.shutdown().await;
}

/// `once = true`：只插一次。之后的命令线上应当干干净净——
/// 否则同步引擎每拉一次都被插一条噪音，真实服务商不是这样。
#[tokio::test]
async fn once_true_interleaves_only_a_single_time() {
    let (server, mut session) = start(config(vec![interleave(
        "FETCH",
        InterleavedUntagged::Exists,
        true,
    )]))
    .await;

    let (first, _) = fetch_one(&mut session, SEQ3_UID).await;
    assert_eq!(first.fetches().len(), 1, "插了一次噪音，但结果不能被污染");

    // 第二次走**裸连接**看线路：一次性故障是整个服务器范围的，
    // 已经被上面那次用掉了，新连接上也应当看不到噪音。
    let mut raw = raw_session(&server).await;
    let reply = raw_command(&mut raw, "c1 UID FETCH 102 (UID)").await;
    assert!(
        !reply.contains("EXISTS"),
        "`once = true` 只该插一次，第二次 FETCH 上还有噪音: {reply}"
    );
    assert!(reply.contains("UID 102"), "{reply}");

    server.shutdown().await;
}

/// 噪音是 `* BYE …`（服务器宣布要走，但没真走）。
///
/// 它是 untagged 响应里最「吓人」的一种：RFC 3501 说收到 `BYE` 就该准备断线。
/// 客户端不能因此把会话判死——命令之后的**下一条**命令依旧要能用。
#[tokio::test]
async fn a_stray_bye_does_not_kill_the_session() {
    let (server, mut session) = start(config(vec![interleave(
        "FETCH",
        InterleavedUntagged::Bye,
        false,
    )]))
    .await;

    let (outcome, _) = fetch_one(&mut session, SEQ3_UID).await;
    assert_eq!(outcome.fetches().len(), 1);

    // 连接没断：同一会话还能继续用。
    let next = run_checked(&mut session, "UID FETCH 105 (UID)").await;
    let next = next.expect("收到 `* BYE` 之后会话仍应可用");
    assert_eq!(next.fetches()[0].uid, Some(105));

    server.shutdown().await;
}

/// 结局是 `NO` 时，穿插的噪音不能把结局「洗」成成功。
///
/// 这是「命令必须自证成败」的反面：不管中间夹了什么，自己的 tagged `NO`
/// 就是失败。连接也不能因此读串——关掉故障后的下一条命令必须照常拿到**它自己**的结果。
#[tokio::test]
async fn a_rejected_store_stays_rejected_despite_noise() {
    let (server, mut session) = start(config(vec![
        Fault::Reject("STORE".to_string()),
        interleave("STORE", InterleavedUntagged::Exists, false),
    ]))
    .await;

    select_writable(&mut session).await;

    let error = run_checked(&mut session, "UID STORE 101 +FLAGS (\\Seen)")
        .await
        .expect_err("服务器回了 NO，必须报错");
    assert!(
        matches!(error, ProtocolError::Imap(_)),
        "NO 必须是 Imap 错误，且不能是 Auth（R25 的回归）: {error:?}"
    );
    server.shutdown().await;

    // 换一个**不拒绝**的服务器，证明「上一条被拒之后连接没读串」：
    // 如果客户端把噪音或别人的结局当成了自己的，这里读到的就不是 102。
    let (server, mut session) = start(config(vec![interleave(
        "STORE",
        InterleavedUntagged::Exists,
        false,
    )]))
    .await;
    select_writable(&mut session).await;
    run_checked(&mut session, "UID STORE 102 +FLAGS.SILENT (\\Seen)")
        .await
        .expect("STORE 应当成功");
    let readback = run_checked(&mut session, "UID FETCH 102 (UID FLAGS)")
        .await
        .expect("回读应当成功");
    assert_eq!(readback.fetches().len(), 1);
    assert_eq!(readback.fetches()[0].uid, Some(102));
    assert!(readback.fetches()[0].has_flag("\\Seen"));

    server.shutdown().await;
}

/// 假服务器自己插得对：`UID EXPUNGE` 的**真实** untagged `EXPUNGE` 在前，
/// 噪音在后，tagged 在最后。同时也证明 `transfer` 的读循环吞得掉。
#[tokio::test]
async fn uid_expunge_real_notification_precedes_the_noise() {
    let (server, mut session) = start(config(vec![interleave(
        "EXPUNGE",
        InterleavedUntagged::Exists,
        false,
    )]))
    .await;
    select_writable(&mut session).await;
    run_checked(&mut session, "UID STORE 101 +FLAGS.SILENT (\\Deleted)")
        .await
        .expect("打 \\Deleted 应当成功");

    uid_expunge_with_timeout(&mut session, &[101], Duration::from_secs(10))
        .await
        .expect("UID EXPUNGE 应当成功");

    let inbox = server
        .mailboxes()
        .into_iter()
        .find(|mailbox| mailbox.name == "INBOX")
        .expect("应当有 INBOX");
    assert_eq!(inbox.messages.len(), 4, "真的删掉了一封");
    assert!(
        inbox.messages.iter().all(|message| message.uid != 101),
        "被 UID EXPUNGE 指定的那一封必须没了"
    );

    server.shutdown().await;
}

/// `MOVE` 不在能力集里时服务器回 `BAD`，客户端必须把它当失败（而不是「成功但没动静」）。
#[tokio::test]
async fn move_without_capability_is_reported_as_a_failure() {
    let (server, mut session) = start(config(Vec::new())).await;
    examine_checked(&mut session, "INBOX")
        .await
        .expect("EXAMINE 应当成功");

    let error = move_messages_with_timeout(&mut session, &[101], "已删除", Duration::from_secs(10))
        .await
        .expect_err("没宣告 MOVE 的服务器必须回 BAD，客户端必须当成失败");
    assert!(
        matches!(error, ProtocolError::Imap(_)),
        "应当是 Imap 错误，实际: {error:?}"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 裸连接（绕过协议层）：用来做 `Fault` 管不到的注入——需要知道客户端**这次**
// 用的 tag，而 tag 由客户端生成，只有读线路的人才拿得到。
// ---------------------------------------------------------------------------

type RawStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// 登录 + `SELECT INBOX` 的裸会话。
async fn raw_session(server: &FakeImapServer) -> BufReader<RawStream> {
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(server.client_config()));
    let tcp = tokio::net::TcpStream::connect(server.addr())
        .await
        .expect("连假服务器失败");
    let name = rustls::pki_types::ServerName::try_from(server.host().to_string()).unwrap();
    let stream = connector.connect(name, tcp).await.expect("TLS 握手失败");

    let mut reader = BufReader::new(stream);
    let mut greeting = String::new();
    reader.read_line(&mut greeting).await.expect("读问候行失败");
    reader
        // 口令要加引号：`test-password` 里的 `-` 不是 atom 字符，
        // 不加引号服务器会把整段当成两个参数。
        .write_all(b"r1 LOGIN \"tester@example.com\" \"test-password\"\r\n")
        .await
        .expect("发 LOGIN 失败");
    let login = read_until_tagged(&mut reader, "r1").await;
    assert!(login.contains("OK"), "登录失败: {login}");

    reader
        .write_all(b"r2 SELECT \"INBOX\"\r\n")
        .await
        .expect("发 SELECT 失败");
    let select = read_until_tagged(&mut reader, "r2").await;
    assert!(select.contains("* 5 EXISTS"), "{select}");
    assert!(select.contains("OK"), "SELECT 失败: {select}");

    reader
}

/// 发 `UID FETCH 103 …`，在「数据」与「结局」之间插入 `noise`（如果有），
/// 返回这次 FETCH 的全部响应行（**不含** `\r\n`）。
async fn uid_fetch_lines(reader: &mut BufReader<RawStream>, noise: Option<&str>) -> Vec<String> {
    reader
        .write_all(b"r3 UID FETCH 103 (UID BODY.PEEK[HEADER])\r\n")
        .await
        .expect("发 FETCH 失败");

    let mut lines = Vec::new();
    let mut inserted = noise.is_none();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await.expect("读响应失败");
        assert!(read > 0, "服务器在 FETCH 中途断开了: {lines:#?}");
        let line = line.trim_end_matches("\r\n").to_string();

        // 客户端这边的「空隙」：已经读到自己那条 FETCH 的数据，还没读到自己的 tagged。
        let fetched = line.starts_with("* 3 FETCH") && !inserted;
        let done = line.starts_with("r3 ");
        lines.push(line);
        if fetched {
            // 字面量会跟一行单独的头内容，但那一行不是响应行；
            // 插入点定在 `* 3 FETCH (…)` 这一行**之后**已经足够了。
            if let Some(noise) = noise {
                lines.push(noise.to_string());
            }
            inserted = true;
        }
        if done {
            return lines;
        }
    }
}

/// 发一条命令并读到它的完成响应为止（含）。
async fn raw_command(reader: &mut BufReader<RawStream>, command: &str) -> String {
    reader
        .write_all(format!("{command}\r\n").as_bytes())
        .await
        .expect("发命令失败");
    let tag = command.split(' ').next().unwrap_or_default();
    read_until_tagged(reader, tag).await
}

/// 读到 `tag` 的完成响应为止（含），返回原始文本。
async fn read_until_tagged(reader: &mut BufReader<RawStream>, tag: &str) -> String {
    let mut collected = String::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await.expect("读响应失败");
        assert!(read > 0, "服务器断开了，已收到: {collected}");
        let done = line.starts_with(&format!("{tag} "));
        collected.push_str(&line);
        if done {
            return collected;
        }
    }
}
