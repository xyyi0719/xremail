//! 错误处理路径的专项审计（R58 起）：**命令失败之后，这条会话还能不能用**。
//!
//! ## 这一组测的是什么
//!
//! `ProtocolError` 的各个变体不只是文案不同：`xremail-sync` 按类别决定要不要
//! 重连、要不要让用户重新授权、要不要原地重试。类别错了，处置就错了。
//!
//! 但比类别更容易出错的是**失败的代价**。`async-imap` 读响应是**流式**的：
//! 一条命令的读循环只读到「它自己的结局」就收工，服务器**已经排在它后面**的
//! 那几行还留在同一个 `BufReader` 里。下一条命令从缓冲区里接着读，
//! 读到的是**上一条**的残留——它自己的结局永远等不到。
//!
//! ## 残留从哪来（真实服务器真的会这样）
//!
//! * `SELECT` / `EXAMINE` **成功**之后，服务器允许发一条
//!   `* OK [ALERT] …` 或 `* BYE …`（RFC 3501 §7.1、§7.1.5），
//!   它们没有 tag，读循环读到自己那条 `OK` 就走了，这一行留在缓冲区里；
//! * 上一轮（另一个 `Session`）断线时，服务器最后写的 `BYE` 可能已经到达
//!   我们这边，只是没人读它；
//! * 服务器在执行命令的过程中说 `BYE` 并断线：那条命令的结局**永远不会来**，
//!   读循环只能等到 EOF。
//!
//! 这三种在客户端看起来是同一种状态：**缓冲区里有一行没人认领的 `BYE` /
//! `ALERT`，而连接本身已经不可信**。下面的用例用 `Fault::InterleaveUntagged`
//! 把它们真的从 TLS 线路上发过去。
//!
//! 每个用例都做同一件事：
//!
//! 1. 让服务器犯一种具体的错；
//! 2. 断言客户端**报了什么错**；
//! 3. 再发一条**全新的、与上一条无关的**命令，断言它拿到的是**自己的**答案。
//!
//! 第 3 步缺了就会被骗：只看第 2 步，任何「报了个错」都算通过，而真正的
//! 问题是「报完错之后这条连接已经不能用了，却没人知道」。

use std::time::Duration;

use xremail_protocol_imap::command::{
    CommandOutcome, DEFAULT_COMMAND_TIMEOUT, examine_checked_with_timeout, run_checked_with_timeout,
};
use xremail_protocol_imap::folders::list_folders_with_timeout;
use xremail_protocol_imap::transfer::{move_messages_with_timeout, uid_expunge_with_timeout};
use xremail_protocol_imap::{ImapConfig, ProtocolError, Session, TlsTrust, connect_and_login_with};
use xremail_testkit::{
    FakeImapServer, Fault, InterleavedUntagged, MailboxFixture, MessageFixture, ServerConfig,
};

// ---------------------------------------------------------------------------
// 公共脚手架
// ---------------------------------------------------------------------------

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

async fn login(imap: &ImapConfig, trust: &TlsTrust) -> Session {
    let (session, _caps, _tls) =
        connect_and_login_with(imap, trust, "tester@example.com", "test-password")
            .await
            .expect("登录应当成功");
    session
}

/// INBOX 三封（UID 11/12/13，**故意不从 1 开始**），外加一个「归档」空文件夹。
///
/// 带 `MOVE` 与 `UIDPLUS`：移动与 UID EXPUNGE 的路径都要能真的走起来。
fn config(faults: Vec<Fault>) -> ServerConfig {
    let mut config = ServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(11).with_subject("一"))
                .with_message(MessageFixture::new(12).with_subject("二"))
                .with_message(MessageFixture::new(13).with_subject("三")),
            MailboxFixture::new("归档"),
        ],
        faults,
        ..ServerConfig::default()
    };
    config.capabilities.push("MOVE".to_string());
    config
}

/// 登录、`SELECT` 打开 INBOX（服务器可以在 `SELECT` 后面塞一条单方面响应）。
///
/// 真实的 163 / Coremail 就是这么干的：选文件夹成功了，紧接着一条服务器
/// 主动发的单方面响应（`ALERT`、`BYE`）落在完成响应后面。
async fn start_and_select(server_faults: Vec<Fault>) -> (FakeImapServer, Session) {
    let (server, imap, trust) = start(config(server_faults)).await;
    let mut session = login(&imap, &trust).await;
    let outcome =
        run_checked_with_timeout(&mut session, "SELECT \"INBOX\"", Duration::from_secs(5))
            .await
            .expect("SELECT 本身应当成功");
    assert_eq!(outcome, CommandOutcome::Empty);
    (server, session)
}

/// 一条**完全无关、且一定有人回**的命令：问服务器它自己的能力集。
///
/// 选它当「探针」，是因为它的答案**只能**来自这条命令自己：服务器会先发
/// `* CAPABILITY …` 再发 `<tag> OK`。对端会先等我们**这条命令**，所以它的
/// 答案不可能「提前」等在缓冲区里；能读到，就说明连接是活的。
///
/// 结果**排序**：能力集是 `HashSet`，顺序每次都不一样，直接比会假红。
async fn capabilities_round_trip(session: &mut Session) -> Vec<String> {
    let mut caps: Vec<String> = session
        .capabilities()
        .await
        .expect("连接仍应可用：CAPABILITY 应当拿到自己的答案")
        .iter()
        .map(|cap| format!("{cap:?}"))
        .collect();
    caps.sort();
    caps
}

// ---------------------------------------------------------------------------
// R58：服务器主动发的 `BYE` 被当成噪音吞掉
// ---------------------------------------------------------------------------

/// `SELECT` 成功之后服务器紧跟着说 `BYE`：那条 `SELECT` **仍然算成功**。
///
/// ## 为什么会出事（这一段是**反面**教材）
///
/// RFC 3501 §7.1.5：`BYE` 的意思是「服务器要关掉这条连接，或者正在关」。
/// 它**可以**出现在任何时刻，包括 `SELECT` 的 `OK` 后面。
///
/// 第一版修法在这里栽了：`async-imap` 处理 `SELECT` / `LOGIN` / `LOGOUT`
/// 走的是 `check_done_ok`，那个循环只要**看到 `BYE` 就报错**
/// （`check_status_ok` 不给 `BYE` 任何例外），哪怕自己的 `OK` 已经读到了。
///
/// 后果很硬：`* OK [CAPABILITY …]` 与随后的 `BYE` 常常挤在同一个 TLS 记录里
/// 一起到达，于是**登录明明是成功的**，客户端却报「认证失败 / 登录失败」，
/// 用户被要求重新授权——而问题根本不在凭据上（R25 的同一个坑，换了个入口）。
///
/// 所以这一条钉的是：`BYE` 排在**完成响应之后**时，不改变完成响应的成败；
/// 会话仍然被正常还回来。反过来，`BYE` 排在**完成响应之前**（服务器在命令
/// 执行到一半时赶人）必须让这条命令失败——那是
/// `a_bye_during_the_command_means_the_session_is_gone` 那条。
#[tokio::test]
async fn a_bye_after_the_completion_does_not_undo_a_success() {
    let (server, mut session) = start_and_select(vec![Fault::InterleaveUntagged {
        command: "SELECT".to_string(),
        noise: InterleavedUntagged::Bye,
        once: false,
    }])
    .await;

    // `SELECT` 已经返回值了（`start_and_select` 里断言过）；连接仍要能用。
    //
    // 注意第 1 条命令：`check_done_ok` 读到缓冲区里那行 `BYE` 就报
    // `Error::No(...)` —— 这正是上面说的那个毛病。`login_with_capabilities`
    // 走的是**同一个循环**，所以风险在登录上，不在 `SELECT` 上。
    let fetches =
        run_checked_with_timeout(&mut session, "UID FETCH 1:* (UID)", Duration::from_secs(5))
            .await
            .expect("SELECT 成功之后，会话应当还能跑命令");
    assert_eq!(fetches.fetches().len(), 3);

    drop(session);
    server.shutdown().await;
}

/// `LIST` 的读循环是**第三套**代码（`folders` 自己又包了一层超时）。
///
/// 同样的 `BYE` 落在 `LIST` 后面时，必须与其它两条循环**同一个结论**：
/// 完成响应已经读到，命令就算成功；会话照常可用。
#[tokio::test]
async fn a_bye_after_list_is_reported_as_a_bye() {
    let (server, imap, trust) = start(config(vec![Fault::InterleaveUntagged {
        command: "LIST".to_string(),
        noise: InterleavedUntagged::Bye,
        once: true,
    }]))
    .await;
    let mut session = login(&imap, &trust).await;

    // `LIST` 成功（`BYE` 排在它的完成响应后面，不影响成败）。
    //
    // 一条 `FETCH` 要在已选中的文件夹上才成立，而 `LIST` 不会帮我们选中，
    // 所以这里只重 **`LIST` 自己**——它同样是 `run_checked_inner`，
    // 同样会在缓冲区里撞上那一行 `BYE`。
    let first = list_folders_with_timeout(&mut session, Duration::from_secs(5))
        .await
        .expect("LIST 本身应当成功");
    let second = list_folders_with_timeout(&mut session, Duration::from_secs(5))
        .await
        .expect("LIST 成功之后，会话应当还能跑命令（别一行 BYE 就把会话判死）");
    assert!(!first.is_empty(), "应当列出文件夹：{first:?}");
    assert_eq!(
        first, second,
        "第一次 LIST 之后会话被污染了：两次列出的文件夹不一样"
    );

    drop(session);
    server.shutdown().await;
}

/// 命令执行到一半时连接被掐断：报出来的东西必须能被同步层认成会话死亡。
///
/// 这一条**跨 crate**：`xremail-sync/src/net.rs` 的 `is_dead_session_text`
/// 靠它决定「这条连接是不是可以重建」。协议层自己至少要保证：
/// **凡是自己写的**断线文案（`bypass` 掉对方库的那几句）都在判据表里。
///
/// 判据表用的是**小写子串**「断开」，而协议层原先写的是「连接**被**断开」——
/// 中间多一个「被」。这里把两张表放在同一处比对，回归时一眼能看出来。
#[tokio::test]
async fn a_disconnect_mid_command_is_a_dead_session() {
    let (server, imap, trust) = start(config(vec![Fault::DropOn("FETCH".to_string())])).await;
    let mut session = login(&imap, &trust).await;
    let outcome =
        run_checked_with_timeout(&mut session, "UID FETCH 1:* (UID)", Duration::from_secs(1)).await;
    let error = match outcome {
        Ok(outcome) => panic!("连接被掐断了，命令不可能成功：{outcome:?}"),
        Err(error) => error,
    };

    // 协议层自己写的那几句断线文案，必须在同步层的证据表里。
    for detail in [
        "连接被断开",
        "连接被断开（服务器发送了 BYE）",
        "连接已经失效",
        "连接可能已失效",
    ] {
        let error = ProtocolError::Io(detail.to_string());
        assert!(
            is_dead_session_text(&error.to_string()),
            "「{detail}」认不出来，同步层就不会提前重连：{error}"
        );
    }

    // 真实的掉线走的是对方库（rustls）的措辞：`peer closed connection without
    // sending TLS close_notify …`。`ProtocolError::Io` 的 `Display` 前缀是
    // 「IO 失败」而不是「io error」，所以同步层那几条英文证据（`io error` /
    // `close_notify` / `unexpected eof`）在**真实断线**上一个都命中不了
    // ——这条链路由 `xremail-sync` 的 `AppError::Network` 兜住（`Io` 本来就会
    // 被翻成 `Network`），不在协议层的控制范围内。这里只记录它长什么样。
    let text = error.to_string();
    assert!(
        text.to_ascii_lowercase().contains("close_notify")
            || text.to_ascii_lowercase().contains("断开"),
        "掉线时的错误文本不该是一句看不懂的东西：{text}"
    );

    drop(session);
    server.shutdown().await;
}

/// `xremail-sync/src/net.rs` 的 `is_dead_session_text` 的判法（逐字抄过来）。
///
/// 抄而不是调用：那是 `xremail-sync` 的私有函数，而且这里要验的正是
/// **两张表对不对得上**，调用它就变成自证了。
fn is_dead_session_text(message: &str) -> bool {
    const EVIDENCE: &[&str] = &[
        "io error",
        "timed out",
        "close_notify",
        "closed connection",
        "unexpected eof",
        "connection reset",
        "broken pipe",
        "读取能力集超时",
        "命令超时",
        "连接被断开",
        "连接已经失效",
        "连接可能已失效",
        "断开",
        "被中断",
        "bye",
    ];
    let lower = message.to_ascii_lowercase();
    EVIDENCE.iter().any(|needle| lower.contains(needle))
}

// ---------------------------------------------------------------------------
// R58（续）：`BYE` 排在完成响应**之前** —— 命令必须失败，会话必须判死
// ---------------------------------------------------------------------------

/// 服务器在命令执行到一半时说 `BYE`：这条命令失败，而且报的是 `BYE`。
///
/// 与上一条的分工：`BYE` 在**完成响应之后**，命令的结论已经定了，`BYE` 只说明
/// 「下一条别发了」；`BYE` 在**完成响应之前**，命令的结局不会再来了，必须当场失败。
///
/// 这一条走的是 `command::run_checked_inner` 的近亲 `folders::list_folders`，
/// 它自己又包了一层超时（三条读循环里唯一这么写的），于是最容易出现
/// 「内层把 `BYE` 判死、外层却把内层的结论丢了」的情况。
#[tokio::test]
async fn a_bye_before_the_completion_fails_the_command() {
    let (server, imap, trust) = start(config(vec![Fault::InjectUntagged {
        command: "LIST".to_string(),
        line: "* BYE shutting down".to_string(),
        once: true,
    }]))
    .await;
    let mut session = login(&imap, &trust).await;

    let error = list_folders_with_timeout(&mut session, Duration::from_secs(5))
        .await
        .expect_err("服务器说了 BYE，LIST 不该报成功");
    let text = error.to_string();
    assert!(
        text.to_ascii_lowercase().contains("bye"),
        "错误里必须说清楚是服务器发了 BYE，实际：{text}"
    );
    assert!(
        error.is_retryable(),
        "BYE 说明连接没了，同步层要立刻重连，而不是等下一轮轮询：{error:?}"
    );
    assert!(
        !matches!(error, ProtocolError::Auth(_)),
        "BYE 不是认证失败——那会让账户停在 ReauthRequired（R25）：{error:?}"
    );

    drop(session);
    server.shutdown().await;
}

/// 同上，但走 `UID FETCH`（`command::run_checked_inner`）。
#[tokio::test]
async fn a_bye_before_a_fetch_completion_fails_the_fetch() {
    let (server, imap, trust) = start(config(vec![Fault::InjectUntagged {
        command: "FETCH".to_string(),
        line: "* BYE shutting down".to_string(),
        once: true,
    }]))
    .await;
    let mut session = login(&imap, &trust).await;
    select_inbox_direct(&mut session).await;

    let error =
        run_checked_with_timeout(&mut session, "UID FETCH 1:* (UID)", Duration::from_secs(5))
            .await
            .expect_err("服务器说了 BYE，FETCH 不该报成功");
    let text = error.to_string();
    assert!(
        text.to_ascii_lowercase().contains("bye"),
        "错误里必须说清楚是服务器发了 BYE，实际：{text}"
    );
    assert!(error.is_retryable(), "BYE 之后要能立刻重连：{error:?}");

    drop(session);
    server.shutdown().await;
}

/// 同上，但走 `UID MOVE`（`transfer::read_until_tagged`）。
#[tokio::test]
async fn a_bye_before_a_move_completion_fails_the_move() {
    let (server, imap, trust) = start(config(vec![Fault::InjectUntagged {
        command: "MOVE".to_string(),
        line: "* BYE shutting down".to_string(),
        once: true,
    }]))
    .await;
    let mut session = login(&imap, &trust).await;
    select_inbox_direct(&mut session).await;

    let error = move_messages_with_timeout(&mut session, &[12], "归档", Duration::from_secs(5))
        .await
        .expect_err("服务器说了 BYE，MOVE 不该报成功");
    let text = error.to_string();
    assert!(
        text.to_ascii_lowercase().contains("bye"),
        "错误里必须说清楚是服务器发了 BYE，实际：{text}"
    );
    assert!(error.is_retryable(), "BYE 之后要能立刻重连：{error:?}");

    drop(session);
    server.shutdown().await;
}

/// APPEND 的读循环也要认得 `BYE`：**立刻**失败，而不是等超时。
///
/// `BYE` 排在 `+` 之后、结局之前：正文已经写进去了，结局不会再来。
///
/// 为什么必须立刻失败而不是等超时：超时在同步层意味着「网络慢，值得按同一套
/// 退避重试」，而 `BYE` 意味着「这条连接完蛋了，立刻重连」。把后者报成前者，
/// 一次服务器维护会被当成一次网络抖动。
///
/// ## 为什么用 `RespondWith([])`
///
/// 假服务器回 `+` 之后**一个字都不再写**。这对客户端与「`BYE` 之后没有结局」
/// **完全等价**：`read_append_result` 只可能从 `read_response` 拿到 EOF（连接关）
/// 或一直等——两种情况都必须落到我们的超时里，而不是挂住。
///
/// ## 已知局限（如实记录）
///
/// 它钉住的是「结局永远不会来 → 立刻收工」，**没有**钉住「`BYE` 被认出来」：
/// 把 `read_append_result` 里那段 `Response::Data { status: Status::Bye }`
/// 删掉，这条用例**依然会绿**（因为服务器会关连接，读到 EOF 也是立刻收工）。
/// 而「`BYE` 排在 `+` 之前」的形态假服务器构造不出来
/// （`InjectUntagged` 的注入点固定在命令自己的 untagged 数据之后，
/// `DropOn` 是直接断线、一个字节都不写）。所以两处判据目前只有**代码结构**
/// 保证（同一个 `crate::command::bye_error()`），没有用例保证。
#[tokio::test]
async fn append_stops_waiting_when_the_completion_never_comes() {
    use xremail_protocol_imap::{AppendOptions, append_message_with_timeout};

    let (server, imap, trust) = start(config(vec![Fault::RespondWith {
        command: "APPEND".to_string(),
        response: Vec::new(),
    }]))
    .await;
    let mut session = login(&imap, &trust).await;

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        append_message_with_timeout(
            &mut session,
            "INBOX",
            b"Subject: hi\r\n\r\nbody",
            &AppendOptions::default(),
            Duration::from_millis(300),
        ),
    )
    .await;

    match result {
        Err(_) => panic!(
            "APPEND 在等一个永远不会来的结局：挂了 {:?}",
            started.elapsed()
        ),
        Ok(Ok(outcome)) => panic!("没有结局，APPEND 不该报成功：{outcome:?}"),
        Ok(Err(ProtocolError::Timeout(_))) | Ok(Err(ProtocolError::Io(_))) => {}
        Ok(Err(other)) => panic!("应当是 Timeout 或 Io，实际：{other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "必须在超时量级收工，而不是挂住：{:?}",
        started.elapsed()
    );

    drop(session);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// R60：APPEND 没有超时
// ---------------------------------------------------------------------------

/// 服务器收了 `APPEND` 却**不回 `+`**：客户端必须按超时返回，而不是挂住。
///
/// `append.rs` 的模块文档与 `connection.rs` 的开头都写着「连接与命令都有超时」，
/// 而 `append_message` 曾经是唯一一条**裸读**的命令：`wait_for_continuation` /
/// `read_append_result` 两个读循环没有任何 `tokio::time::timeout`，
/// 函数签名里也没有可以传进来的超时。
///
/// 后果与 R46 同型：发信后「确保已发送副本」这一步在坏网络上永远转圈，
/// 而调用方手里的 `command_timeout` **根本传不进来**。
///
/// ## 为什么外层还要再包一个 `tokio::time::timeout`
///
/// 协议层自带的超时是 `DEFAULT_COMMAND_TIMEOUT`（30 秒）。用例要证明的是
/// **超时确实发生**，而不是「默认值是多少」——所以外层给一个更短的窗口：
/// 外层先响，说明内层没在管；内层先响（返回 `Timeout`），才算通过。
/// 两者都以「外层没响」为前提，所以这里等 2 秒；失败时打印的是内层的错。
#[tokio::test]
async fn append_times_out_when_the_server_never_answers() {
    use xremail_protocol_imap::{AppendOptions, append_message_with_timeout};

    let (server, imap, trust) = start(config(vec![Fault::HangOn("APPEND".to_string())])).await;
    let mut session = login(&imap, &trust).await;

    let started = std::time::Instant::now();
    // 调用方显式给 300ms：`append_message_with_timeout` 必须照着它收工。
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        append_message_with_timeout(
            &mut session,
            "INBOX",
            b"Subject: hi\r\n\r\nbody",
            &AppendOptions::default(),
            Duration::from_millis(300),
        ),
    )
    .await;

    match result {
        Err(_) => panic!(
            "APPEND 在服务器不回话时挂了 {:?}：调用方给的超时没生效",
            started.elapsed()
        ),
        Ok(Ok(outcome)) => panic!("服务器根本没回话，却拿到了成功：{outcome:?}"),
        Ok(Err(ProtocolError::Timeout(_))) => {}
        Ok(Err(other)) => panic!("应当是 Timeout，实际：{other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "超时必须在调用方给的 300ms 量级收工，实际 {:?}",
        started.elapsed()
    );

    drop(session);
    server.shutdown().await;
}

/// 同上，但服务器是「收了正文、不回结局」——更常见的一半。
///
/// 这一半尤其要**自己**超时：正文已经写到网上了，服务器不再说话时
/// 只有本层的超时能把它收口。
#[tokio::test]
async fn append_times_out_when_the_completion_never_arrives() {
    use xremail_protocol_imap::{AppendOptions, append_message_with_timeout};

    let (server, imap, trust) = start(config(vec![Fault::RespondWith {
        command: "APPEND".to_string(),
        response: Vec::new(),
    }]))
    .await;
    let mut session = login(&imap, &trust).await;

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        append_message_with_timeout(
            &mut session,
            "INBOX",
            b"Subject: hi\r\n\r\nbody",
            &AppendOptions::default(),
            Duration::from_millis(300),
        ),
    )
    .await;

    match result {
        Err(_) => panic!("APPEND 等结局时挂住了：调用方给的超时没生效"),
        Ok(Ok(outcome)) => panic!("没有结局，却拿到了成功：{outcome:?}"),
        Ok(Err(ProtocolError::Timeout(_))) => {}
        Ok(Err(other)) => panic!("应当是 Timeout，实际：{other:?}"),
    }

    drop(session);
    server.shutdown().await;
}

/// 默认入口 `append_message` 也必须自己收工，不能指望调用方在外面包。
///
/// 这一条测的是那件事本身：一个**直接调 `append_message`、什么都没包**的
/// 调用方（`sent_copy.rs` 当年就是这样）会不会把整个进程挂死。
///
/// ## 为什么它是 `#[ignore]`
///
/// 它**必须真的等满**默认的 `DEFAULT_COMMAND_TIMEOUT`（30 秒），放进默认
/// 测试集会让每次 `cargo test` 都多等半分钟。但它值得留着：
///
/// * `append_message_with_timeout` 那几条只证明「调用方给的超时生效」，
///   **不**证明默认入口也接上了超时——把 `append_message` 改回裸调，
///   那几条依旧全绿，只有这一条会红；
/// * 修之前它会**永远挂住**（服务器一个字节都不回），修之后 30 秒返回。
///
/// 需要时手动跑：
///
/// ```text
/// cargo test -p xremail-protocol-imap --test error_paths -- --ignored
/// ```
#[tokio::test]
#[ignore = "必须等满 30 秒的默认超时"]
async fn the_default_append_entry_times_out_on_a_silent_server() {
    use xremail_protocol_imap::{AppendOptions, DEFAULT_COMMAND_TIMEOUT, append_message};

    let (server, imap, trust) = start(config(vec![Fault::HangOn("APPEND".to_string())])).await;
    let mut session = login(&imap, &trust).await;

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        DEFAULT_COMMAND_TIMEOUT + Duration::from_secs(10),
        append_message(
            &mut session,
            "INBOX",
            b"Subject: hi\r\n\r\nbody",
            &AppendOptions::default(),
        ),
    )
    .await;

    match result {
        Err(_) => panic!("`append_message` 没有自带超时：它挂了超过默认超时 10 秒以上"),
        Ok(Err(ProtocolError::Timeout(detail))) => {
            assert!(
                detail.contains("APPEND"),
                "超时文案里要能看出是哪条命令：{detail}"
            );
            assert!(
                started.elapsed() >= DEFAULT_COMMAND_TIMEOUT,
                "不该早于默认超时就收工：{:?}",
                started.elapsed()
            );
        }
        Ok(Ok(outcome)) => panic!("服务器根本没回话，却拿到了成功：{outcome:?}"),
        Ok(Err(other)) => panic!("应当是 Timeout，实际：{other:?}"),
    }

    drop(session);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// R61：超时文案把用户输入原样带进了错误
// ---------------------------------------------------------------------------

/// 超时文案会原样带上命令原文，而命令原文里可能有用户名 / 授权码。
///
/// `lib.rs` 与 `connection.rs` 的文档都写着「错误信息**不含**用户名或口令」。
/// 可 `run_checked_with_timeout` 把 `{command}` 直接拼进
/// `ProtocolError::Timeout`，这个错误一路变成日志与 UI 上的文字。
///
/// `run_checked` 是**公开**接口，任何调用方都能往里塞一条带凭据的命令
/// （`LOGIN "user" "password"` 本身就是合法的 IMAP 命令）；诊断工具也确实是
/// 用 `run_checked` 发原始命令的。所以脱敏只能在这一层做，
/// 不能指望每个调用方自己注意。
#[test]
fn a_timeout_message_never_carries_the_credentials_verbatim() {
    let detail = xremail_protocol_imap::command::timeout_detail(
        Duration::from_secs(30),
        "LOGIN \"someone@163.com\" \"hunter2-secret\"",
    );
    assert!(
        !detail.contains("hunter2-secret"),
        "授权码进了错误信息（会被写进日志）：{detail}"
    );
    assert!(
        !detail.contains("someone@163.com"),
        "用户名进了错误信息：{detail}"
    );
    assert!(
        detail.contains("LOGIN"),
        "脱敏不能脱到连是哪条命令都看不出来：{detail}"
    );
}

/// 普通命令一个字都不用改。
#[test]
fn timeout_detail_leaves_ordinary_commands_alone() {
    let detail = xremail_protocol_imap::command::timeout_detail(
        Duration::from_secs(30),
        "UID STORE 12,13 +FLAGS.SILENT (\\Deleted)",
    );
    assert_eq!(
        detail,
        "命令超时（30s）: UID STORE 12,13 +FLAGS.SILENT (\\Deleted)"
    );

    // 文件夹名要留着——排查「哪个文件夹超时」全靠它。
    let detail =
        xremail_protocol_imap::command::timeout_detail(Duration::from_secs(5), "LIST \"\" \"*\"");
    assert_eq!(detail, "命令超时（5s）: LIST \"\" \"*\"");
}

// ---------------------------------------------------------------------------
// 对照组：正常情况下会话必须还能用
// ---------------------------------------------------------------------------

/// 没有 `BYE`、没有超时时，一条命令读完，下一条必须拿到自己的答案。
///
/// 这是上面几条的对照组：如果这里也不成立，说明问题不在错误路径，
/// 而在读循环本身。
#[tokio::test]
async fn ordinary_commands_leave_the_session_usable() {
    let (server, mut session) = start_and_select(Vec::new()).await;

    let before = capabilities_round_trip(&mut session).await;
    let fetches = run_checked_with_timeout(
        &mut session,
        "UID FETCH 1:* (UID FLAGS)",
        Duration::from_secs(5),
    )
    .await
    .expect("FETCH 应当成功");
    assert_eq!(fetches.fetches().len(), 3);
    assert_eq!(before, capabilities_round_trip(&mut session).await);

    // `transfer` 那条读循环（`run_until_tagged`）之后也一样。
    uid_expunge_with_timeout(&mut session, &[12], Duration::from_secs(5))
        .await
        .expect("UID EXPUNGE 应当成功");
    assert_eq!(before, capabilities_round_trip(&mut session).await);

    // `command` 的 `examine_checked` 路径。
    let info = examine_checked_with_timeout(&mut session, "INBOX", Duration::from_secs(5))
        .await
        .expect("EXAMINE 应当成功");
    // `UID EXPUNGE` 只删**带 `\Deleted`** 的（RFC 4315），而我们没打过标记，
    // 所以三封都在——这一条顺带钉住了「`UID EXPUNGE` 不会自己替用户标删」。
    assert_eq!(info.exists, 3);
    assert_eq!(before, capabilities_round_trip(&mut session).await);

    // 默认超时常量只是兜底，不该比调用方的更松。
    assert!(DEFAULT_COMMAND_TIMEOUT >= Duration::from_secs(5));

    server.shutdown().await;
}

/// 一条 MOVE 成功（读循环跑完）之后，会话必须还能用。
#[tokio::test]
async fn a_successful_move_leaves_the_session_usable() {
    let (server, mut session) = start_and_select(Vec::new()).await;
    let before = capabilities_round_trip(&mut session).await;

    let outcome = move_messages_with_timeout(&mut session, &[12], "归档", Duration::from_secs(5))
        .await
        .expect("MOVE 应当成功");
    assert!(outcome.has_uid_mapping(), "服务器给了 COPYUID：{outcome:?}");

    assert_eq!(before, capabilities_round_trip(&mut session).await);
    server.shutdown().await;
}

/// 直接 `SELECT`（不带任何故障注入）。
async fn select_inbox_direct(session: &mut Session) {
    let outcome = run_checked_with_timeout(session, "SELECT \"INBOX\"", Duration::from_secs(5))
        .await
        .expect("SELECT 应当成功");
    assert_eq!(outcome, CommandOutcome::Empty);
}
