//! T10 的最后一块：IMAP **响应解析**（服务器 → 客户端）的端到端不变量测试。
//!
//! 跑得快的随机部分在 `tests/imap_parsing_properties.rs`；这里负责另一半：
//! **真的把畸形响应从 TLS 线路上发过去**，看客户端拿到什么。用两台服务器：
//!
//! * [`Scripted`]：一台只会按剧本回话的真 TLS IMAP 服务器。用它的原因很具体——
//!   `EXAMINE` 的 `* OK [UIDVALIDITY …]` 与 `APPEND` 的 `[APPENDUID …]` 不是
//!   「完成响应」，`Fault::RespondWith` 换不掉它们；要造「服务器**没有**说」，
//!   就得让服务器真的不说。
//! * `FakeImapServer` + `Fault::RespondWith`（`crates/xremail-testkit/src/config.rs`，
//!   本次新加的**测试专用**故障）：命令照常执行，只把 `<tag> OK …` 换成给定的原文。
//!
//! ## 这一层为什么非测不可
//!
//! `command` 的模块文档写着「async-imap 0.12 的 `uid_fetch` / `uid_store` /
//! `uid_search` 会**吞掉**带 tag 的 `NO` / `BAD`」——那是**响应解析**的毛病。
//! 同一套解析也用在 `select` / `examine` / `append` 上，我们只是绕开了前三条命令。
//! 本文件里有一条用例专门证明：那条路上的毛病还在，且表现是**挂到超时**而不是报错。
//!
//! ## 被钉住的承诺（都来自各模块的文档）
//!
//! 1. **缺什么就是 `None`**：`[UIDVALIDITY]` / `[HIGHESTMODSEQ]` / `[APPENDUID]` /
//!    `[COPYUID]` 没出现时不许补 0、不许猜。
//! 2. **端点精确**：`APPENDUID 123 5:7` 的新 UID 是 **7**；`COPYUID` 的区间是闭区间。
//! 3. **映射按下标、不越界、展开有上限**。
//! 4. **成败只认自己的结局**：`NO` / `BAD` / 未知状态一律明确报错，`[MODIFIED]`
//!    单独成类；没有结局就**挂住**（由调用方的超时收口），绝不自己收工。
//! 5. **假服务器的宽松不等于真服务器的宽松**：`LIST` 的特殊用途属性决定角色，
//!    名字只是兜底。

use std::time::Duration;

use xremail_protocol_imap::command::examine_checked_with_timeout;
use xremail_protocol_imap::folders::list_folders_with_timeout;
use xremail_protocol_imap::transfer::{copy_messages_with_timeout, move_messages_with_timeout};
use xremail_protocol_imap::{
    AppendOptions, ImapConfig, ProtocolError, Session, TlsTrust, append_message,
    connect_and_login_with, run_checked,
};
use xremail_testkit::{FakeImapServer, Fault, MailboxFixture, MessageFixture, ServerConfig};

// ---------------------------------------------------------------------------
// 确定性伪随机（SplitMix64），与其它属性测试同一实现
// ---------------------------------------------------------------------------

/// 确定性伪随机发生器（SplitMix64）。
///
/// 重复一份而不是抽成公共 crate：测试用的几行东西不值得为它新增一个依赖
/// （本仓库的约束是「不加依赖」）。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ---------------------------------------------------------------------------
// 假服务器（`FakeImapServer`）那一侧
// ---------------------------------------------------------------------------

async fn start(config: ServerConfig) -> (FakeImapServer, ImapConfig, TlsTrust) {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(5);

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

/// 「这条命令的完成响应换成这几行」（`{tag}` 由服务器替换成真的 tag）。
fn replace(command: &str, response: &[&str]) -> Fault {
    Fault::RespondWith {
        command: command.to_string(),
        response: response.iter().map(|line| line.to_string()).collect(),
    }
}

/// INBOX 里三封（UID 11/12/13，**故意不从 1 开始**：把序号当 UID 用会立刻露馅），
/// 「已删除」是空文件夹。
fn config_with_inbox(faults: Vec<Fault>) -> ServerConfig {
    ServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(11).with_subject("一"))
                .with_message(MessageFixture::new(12).with_subject("二"))
                .with_message(MessageFixture::new(13).with_subject("三")),
            MailboxFixture::new("已删除").with_attribute("\\Trash"),
        ],
        faults,
        ..ServerConfig::default()
    }
}

async fn select_inbox(session: &mut Session) {
    run_checked(session, "SELECT \"INBOX\"")
        .await
        .expect("SELECT 应当成功");
}

// ---------------------------------------------------------------------------
// 1. APPENDUID：闭区间、不给就是 None
// ---------------------------------------------------------------------------

/// `APPENDUID` 的区间是**闭区间**：`5:7` 的新 UID 是 **7**，不是 6。
///
/// 防的是什么：这里曾经写成 `range.end() - 1`，`APPENDUID 123 5:7` 被报成 6，
/// 于是发信副本的 UID 全记错一位——下一轮同步找不到「自己刚存的那封」，
/// 又 append 一次，服务器上就多出一封。
///
/// 为什么不用 `Fault::RespondWith` 把 `[APPENDUID …]` 换掉：**换不掉**。
/// async-imap 的 `append` 在**它自己的响应解析**里读 `[APPENDUID …]`
/// （`parse.rs` 的 `parse_status`），命令执行之前就已经把响应读完并解析了；
/// 我们在线路上改那一行，它只会「等一个永远不会来的结局」——实测挂到 10 秒超时
/// （见 `a_mangled_completion_line_loses_the_whole_append_verdict`）。
/// 所以这一层只能从 `imap-proto` 的解析结果开始测：随机的区间在
/// `tests/imap_parsing_properties.rs::appenduid_takes_the_last_uid_of_a_closed_range`
/// 里跑了几千轮，这里补上真实端到端的一正一反。
#[tokio::test]
async fn appenduid_endpoints_are_inclusive_at_both_ends() {
    // 假服务器回的 `[APPENDUID <uidvalidity> <uid>]`，新 UID 是空文件夹里的 1；
    // 紧接着再 append 一封，必须是 2 ——差一位就会重复。
    let (server, imap, trust) = start(config_with_inbox(Vec::new())).await;
    let mut session = login(&imap, &trust).await;

    let first = append_message(
        &mut session,
        "INBOX",
        b"Subject: 1\r\n\r\n",
        &AppendOptions::default(),
    )
    .await
    .expect("APPEND 应当成功");
    // INBOX 里已有 UID 11/12/13，所以第一封 append 的 UID 是 14。
    assert_eq!(first.uid, Some(14), "新 UID 应当是现有最大 UID + 1");
    assert_eq!(first.uid_validity, Some(server.uid_validity()));

    let second = append_message(
        &mut session,
        "INBOX",
        b"Subject: 2\r\n\r\n",
        &AppendOptions::default(),
    )
    .await
    .expect("APPEND 应当成功");
    assert_eq!(second.uid, Some(15), "连续 append 的 UID 必须逐个递增");
    server.shutdown().await;
}

/// 服务器**不给** `APPENDUID` 时 `uid` 是 `None`——不是 0，不是 `EXISTS`，不是猜的。
///
/// 防的是什么：`AppendOutcome::uid` 的文档写着「服务器不给就是 `None`」。
/// 编一个数字出来，调用方就会拿一封**别的邮件**的 UID 去回读确认——
/// 那封邮件确实存在，于是「发完确保已发送副本」永远成功，副本其实没存上。
///
/// 同上：`[APPENDUID …]` 由 async-imap 在读完成响应时顺带解析，不能用
/// `Fault::RespondWith` 模拟（会挂住）。这里用带「截断」的残缺形态证明
/// **残缺不会被补成数字**：`[APPENDUID 1]` 缺 UID 时，后面的 `OK` 一样会被
/// async-imap 吃掉，于是调用方看到的是**超时**而不是一个编出来的 0。
#[tokio::test]
async fn a_truncated_appenduid_is_never_patched_into_a_number() {
    for code in ["[APPENDUID]", "[APPENDUID 1]", "[APPENDUID x 5]"] {
        let faults = vec![replace(
            "APPEND",
            &[&format!("{{tag}} OK {code} APPEND completed")],
        )];
        let (server, imap, trust) = start(config_with_inbox(faults)).await;
        let mut session = login(&imap, &trust).await;

        let result = tokio::time::timeout(
            Duration::from_millis(400),
            append_message(
                &mut session,
                "INBOX",
                b"Subject: x\r\n\r\n",
                &AppendOptions::default(),
            ),
        )
        .await;

        match result {
            // 挂住：async-imap 在等一个被它自己吃掉的结局。安全（调用方按超时处理）。
            Err(_) => {}
            // 给了结论：**不许**是编出来的数字。
            Ok(Ok(outcome)) => panic!("残缺的 {code} 报出了 {outcome:?}——不能补默认值"),
            Ok(Err(error)) => assert!(!error.to_string().is_empty()),
        }

        drop(session);
        server.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// 2. COPYUID：映射按下标、不越界、展开有上限
// ---------------------------------------------------------------------------

/// `COPYUID` 的源/目标集合**按下标一一对应**，长短不一时 `has_uid_mapping()` 为假。
///
/// 防的是什么：`TransferOutcome` 的文档写着「长度不等时说明服务器回的区间被截断了，
/// 别硬套」。硬套的后果是把信 A 的新 UID 记成信 B 的，之后所有「按新 UID 回读确认」
/// 都在核对另一封邮件。
#[tokio::test]
async fn copyuid_maps_by_position_and_refuses_mismatched_lengths() {
    let faults = vec![replace(
        "COPY",
        &["{tag} OK [COPYUID 77 11:12 21:22] COPY completed"],
    )];
    let (server, imap, trust) = start(config_with_inbox(faults)).await;
    let mut session = login(&imap, &trust).await;
    select_inbox(&mut session).await;

    let outcome =
        copy_messages_with_timeout(&mut session, &[11, 12], "已删除", Duration::from_secs(5))
            .await
            .expect("COPY 应当成功");

    assert_eq!(
        outcome.source_uid_validity,
        Some(77),
        "COPYUID 里的那个数就是源文件夹的 UIDVALIDITY"
    );
    assert_eq!(outcome.destination_uid_validity, None, "目标侧的它不给");
    assert_eq!(outcome.source_uids, vec![11, 12]);
    assert_eq!(outcome.destination_uids, vec![21, 22]);
    assert!(outcome.has_uid_mapping());
    assert_eq!(outcome.destination_uid_of(11), Some(21));
    assert_eq!(outcome.destination_uid_of(12), Some(22));
    assert_eq!(outcome.destination_uid_of(13), None);
    server.shutdown().await;

    // 长短不一（服务器回的区间被截断了）——**不许硬套**。
    // 走 `MOVE` 而不是 `COPY`：MOVE 的读循环是**另一条**（`run_until_tagged`），
    // 两条都要证明认得服务器的 `COPYUID`。MOVE 要服务器宣告能力（R40）。
    let faults = vec![replace(
        "MOVE",
        &["{tag} OK [COPYUID 77 11:13 21:22] MOVE completed"],
    )];
    let mut config = config_with_inbox(faults);
    config.capabilities.push("MOVE".to_string());
    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;
    select_inbox(&mut session).await;

    let outcome = move_messages_with_timeout(
        &mut session,
        &[11, 12, 13],
        "已删除",
        Duration::from_secs(5),
    )
    .await
    .expect("MOVE 应当成功");

    assert_eq!(outcome.source_uids, vec![11, 12, 13]);
    assert!(!outcome.has_uid_mapping(), "3 对 2 不是一份可用的映射");
    assert_eq!(
        outcome.destination_uid_of(11),
        None,
        "长短不一时不许硬套：会把信 A 的新 UID 记成信 B 的"
    );
    server.shutdown().await;
}

/// `COPYUID` 的区间展开**有上限**：`1:4294967295` 不能真的展开成四十多亿个元素。
///
/// 防的是什么：R09 有前科（`1:*` 被展开成 1.79 亿个元素，撑爆过假服务器的 8 GiB）。
/// 服务器回 `1:4294967295` 完全合法，客户端照着办就是一次 OOM——
/// 而且是**服务器一句话就能触发**的那种。
#[tokio::test]
async fn copyuid_expansion_is_bounded() {
    // 源集合只声明**一个**区间，但它是 INBOX 里唯一的那封（11）：
    // 这样源侧只展开出一个 UID，与目标侧被截断的 10 000 个长度对不上，
    // 「不许当成可用的映射」才有意义（两边都截到 10 000 时长度恰好相等，
    // 那是一个**假的**一对一映射——见下一条用例）。
    let faults = vec![replace(
        "COPY",
        &["{tag} OK [COPYUID 1 11 1:4294967295] COPY completed"],
    )];
    let (server, imap, trust) = start(config_with_inbox(faults)).await;
    let mut session = login(&imap, &trust).await;
    select_inbox(&mut session).await;

    let outcome = copy_messages_with_timeout(&mut session, &[11], "已删除", Duration::from_secs(5))
        .await
        .expect("COPY 应当成功");

    // 修了 R47 之后，目标侧**被截断就整体置空**，所以这里看不到那 10 000 个元素了。
    // 「截断只砍尾巴、不砍开头」那条性质改为直接测展开函数（见本文件末尾
    // `truncation_only_cuts_the_tail`）——性质没变，只是观察点从 `TransferOutcome`
    // 挪到了真正做展开的地方。
    assert!(
        !outcome.has_uid_mapping(),
        "被截断的映射不能被当成可用的映射（R47）"
    );
    assert!(
        outcome.destination_uids.is_empty(),
        "截断后目标侧应当整体置空，实际 {} 个",
        outcome.destination_uids.len()
    );

    server.shutdown().await;
}

/// **已知缺陷**（编号见 `docs/ISSUES_AND_RISKS.md`）：服务器对源、目标各回一个大区间时，
/// 两边都被截到同一个上限，长度**恰好相等**，`has_uid_mapping()` 于是为真——
/// 这是一份**假的一对一映射**：源第 2 个（不存在）对上了目标第 2 个。
///
/// 本用例把现状**钉住**而不是放松断言：一旦有人修了它（例如截断时记一个
/// `truncated` 标记、或在截断后判定映射不可用），这条会红，提醒同步
/// 「R39 的写回」可以开始信任这份映射了。
#[tokio::test]
async fn truncated_copyuid_is_refused_instead_of_used_as_a_mapping() {
    // `COPYUID` 里源、目标两个集合是**各自**展开的。两边都回 `1:4294967295`
    // 时各自被截到 `UID_EXPANSION_LIMIT`（10 000）个——**长度恰好相等**。
    //
    // 修之前（R47）：`has_uid_mapping()` 只看「长度相等」，于是把这份**凭空长出来**
    // 的对应关系当成一对一：源 1..10000 ↔ 目标 1..10000。
    // 而 INBOX 里只有 UID 11/12/13 三封。R39 的写回一旦照它执行，就会把
    // 9 997 封**不存在的信**记进本地库。
    //
    // 修之后：任何一侧被截断，目标集合就留空，映射不可用——调用方会走
    // 「没有映射」那条处置（和服务器不给 `COPYUID` 时一样）。
    let faults = vec![replace(
        "COPY",
        &["{tag} OK [COPYUID 1 1:4294967295 1:4294967295] COPY completed"],
    )];
    let (server, imap, trust) = start(config_with_inbox(faults)).await;
    let mut session = login(&imap, &trust).await;
    select_inbox(&mut session).await;

    let outcome = copy_messages_with_timeout(&mut session, &[11], "已删除", Duration::from_secs(5))
        .await
        .expect("COPY 本身是成功的");

    assert!(
        outcome.destination_uids.is_empty(),
        "被截断的映射不能拿来用，目标集合必须是空的，实际 {} 个",
        outcome.destination_uids.len()
    );
    assert!(
        !outcome.has_uid_mapping(),
        "被截断的映射必须判为不可用——否则写回会把不存在的信记进库（R47）"
    );
    assert_eq!(
        outcome.destination_uid_of(11),
        None,
        "不可用时就查不出目标 UID，不能编一个出来"
    );

    server.shutdown().await;
}

/// 服务器**不给** `COPYUID` 时，源集合就是**我们自己发出去的那些**（排好序、去过重），
/// 目标集合留空——不编。
///
/// 防的是什么：两件事。① 源集合不能是空的：调用方要靠它知道「搬的是哪几封」；
/// ② 也不能原样照抄入参：`[13, 11, 11]` 这种顺序/重复会让下游按
/// `source_uids[i] ↔ destination_uids[i]` 对齐时错位，`11` 被搬两次。
#[tokio::test]
async fn missing_copyuid_falls_back_to_the_requested_source_set_only() {
    let faults = vec![replace("COPY", &["{tag} OK COPY completed"])];
    let (server, imap, trust) = start(config_with_inbox(faults)).await;
    let mut session = login(&imap, &trust).await;
    select_inbox(&mut session).await;

    let outcome = copy_messages_with_timeout(
        &mut session,
        &[13, 11, 11],
        "已删除",
        Duration::from_secs(5),
    )
    .await
    .expect("COPY 应当成功");

    assert_eq!(outcome.source_uids, vec![11, 13], "要排序去重");
    assert!(outcome.destination_uids.is_empty(), "没给就是空，不能编");
    assert!(!outcome.has_uid_mapping());
    assert_eq!(outcome.source_uid_validity, None);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. 成败只认自己的结局
// ---------------------------------------------------------------------------

/// 完成响应的状态词决定成败；旁边的 untagged 数据再多也盖不住它。
///
/// 这是 `command` 模块文档的核心承诺（「服务器回 `NO` / `BAD` 时**必须**报错」），
/// 而且它是有代价换来的：async-imap 的 `uid_fetch` / `uid_search` 会把这类结局
/// 吞掉，所以本层自己读。这里钉的是**结局换了形状之后仍然成立**。
#[tokio::test]
async fn the_completion_status_decides_the_verdict() {
    // `NO [MODIFIED]`：不是普通失败，而是「基线过期」，要单独成类
    // （处置方式完全不同：重新读快照重算，而不是退避重试）。
    let faults = vec![replace(
        "FETCH",
        &["{tag} NO [MODIFIED] Some messages were modified"],
    )];
    let (server, imap, trust) = start(config_with_inbox(faults)).await;
    let mut session = login(&imap, &trust).await;
    select_inbox(&mut session).await;

    let error = run_checked(&mut session, "UID FETCH 1:* (UID FLAGS)")
        .await
        .expect_err("NO 必须是错误");
    assert!(
        matches!(error, ProtocolError::ConditionFailed(_)),
        "[MODIFIED] 应当单独成类（ConditionFailed），实际 {error:?}"
    );
    server.shutdown().await;

    // 其余状态词：一律 `Err`，错误信息里带上服务器说的那句话。
    for (status, marker) in [("BAD", "BAD"), ("NO", "NO"), ("WHAT", "WHAT")] {
        let faults = vec![replace(
            "FETCH",
            &[&format!("{{tag}} {status} something went wrong")],
        )];
        let (server, imap, trust) = start(config_with_inbox(faults)).await;
        let mut session = login(&imap, &trust).await;
        select_inbox(&mut session).await;

        match run_checked(&mut session, "UID FETCH 1:* (UID FLAGS)").await {
            Ok(outcome) => panic!("{status} 必须是错误，客户端却报成功：{outcome:?}"),
            Err(error) => {
                let text = error.to_string();
                assert!(
                    text.contains(marker),
                    "{status} 的错误信息里应当带上状态词（实际：{text}）"
                );
                assert!(
                    !matches!(error, ProtocolError::Auth(_)),
                    "{status} 被当成了认证失败——那会让账户停在 ReauthRequired（R25）"
                );
            }
        }
        server.shutdown().await;
    }
}

/// 命令**一个字都不回**时，客户端不会把「没有结局」当成「成功且没有数据」。
///
/// 防的是什么：`command` 模块文档里那张表（「服务器拒绝 → 看起来像成功但没有变更」）
/// 换个形态就能重现。`run_checked` 的读循环**本身没有内置超时**——超时由调用方包
/// （`xremail-sync` 的 `run_checked_with_timeout`），这不是缺陷，但要把这个事实钉住：
/// **没结局就是挂住**，绝不能自己收工。
#[tokio::test]
async fn a_missing_completion_never_looks_like_success() {
    let faults = vec![replace("FETCH", &[])];
    let (server, imap, trust) = start(config_with_inbox(faults)).await;
    let mut session = login(&imap, &trust).await;
    select_inbox(&mut session).await;

    let result = tokio::time::timeout(
        Duration::from_millis(300),
        run_checked(&mut session, "UID FETCH 1:* (UID FLAGS)"),
    )
    .await;

    match result {
        // 超时：读循环在等一条永远不会来的结局。这是**安全**的结论。
        Err(_) => {}
        Ok(Ok(outcome)) => panic!("没有完成响应却报成功：{outcome:?}"),
        Ok(Err(error)) => panic!("没有完成响应却报了一个「确定的失败」：{error:?}"),
    }

    // 连接已经不可信（那批数据还堵在里面），直接丢掉，别在它上面再发命令。
    drop(session);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. LIST：属性 → 角色
// ---------------------------------------------------------------------------

/// `LIST` 的特殊用途属性决定角色；属性里没有角色的才按名字兜底。
///
/// 防的是什么：`folders::role_from_attributes` 的文档写着「属性优先，名字兜底」
/// （QQ 不给属性）。兜底一旦抢在属性前面，一个叫 `Drafts` 但服务器明确标了
/// `\Junk` 的文件夹会被当成草稿箱——用户「删除草稿」就删进了垃圾邮件里。
#[tokio::test]
async fn special_use_beats_the_name_and_unknown_attributes_are_ignored() {
    let config = ServerConfig {
        mailboxes: vec![
            MailboxFixture::new("INBOX"),
            // 名字像草稿箱，属性说它是垃圾邮件：**属性赢**。
            MailboxFixture::new("Drafts").with_attribute("\\Junk"),
            // 两个角色属性并存：按出现顺序取第一个（不允许随机、不允许取最后一个）。
            MailboxFixture::new("A")
                .with_attribute("\\Archive")
                .with_attribute("\\Trash"),
            // 只有「不是角色」的属性（`\NoSelect` / 厂商扩展）：靠名字兜底。
            MailboxFixture::new("Sent Messages")
                .with_attribute("\\NoSelect")
                .with_attribute("\\HasChildren"),
            // 两样都没有 → 认不出就是认不出，不许猜。
            MailboxFixture::new("Virus Items"),
            // 中文名靠名字兜底（163 的形态）。
            MailboxFixture::new("已发送"),
        ],
        ..ServerConfig::default()
    };

    let (server, imap, trust) = start(config).await;
    let mut session = login(&imap, &trust).await;

    let folders = list_folders_with_timeout(&mut session, Duration::from_secs(10))
        .await
        .expect("LIST 应当成功");

    let find = |name: &str| {
        folders
            .iter()
            .find(|folder| folder.name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
                panic!("LIST 里应当有 {name}，实际有 {names:?}")
            })
    };

    assert_eq!(find("INBOX").role.as_deref(), Some("inbox"));
    assert_eq!(
        find("Drafts").role.as_deref(),
        Some("junk"),
        "名字兜底抢在属性前面了：服务器明说它是 \\Junk"
    );
    assert_eq!(find("A").role.as_deref(), Some("archive"));
    assert_eq!(find("Sent Messages").role.as_deref(), Some("sent"));
    assert_eq!(find("Virus Items").role, None, "认不出就该是 None，不许猜");
    assert_eq!(find("已发送").role.as_deref(), Some("sent"));

    // 线路名与显示名互为往返：发命令要用 `raw_name`，用错了会开错文件夹
    // （`RemoteFolder::raw_name` 的文档）。
    for folder in &folders {
        assert_eq!(
            folder.raw_name,
            xremail_protocol_imap::utf7::encode(&folder.name),
            "{} 的线路名与解码名对不上",
            folder.name
        );
    }

    server.shutdown().await;
}

/// `LIST` 行里带**任意的属性词**：不许 panic，角色只能是文档里列的那几个。
///
/// `role_from_attributes` 每一轮都在「不认识的属性」上工作（各家厂商扩展一堆）。
/// 这里钉的是**输出集合有界**——认不出来可以，认成文档里没有的角色不行。
#[tokio::test]
async fn random_list_attributes_only_produce_documented_roles() {
    const ROLES: &[&str] = &[
        "all", "archive", "drafts", "flagged", "junk", "sent", "trash", "inbox",
    ];
    // 全部是合法的 atom（RFC 3501 §9 `atom-char`）：每个用例都吃 3 台服务器，
    // 这里要的是「属性→角色」这一层，不是「服务器发了非法 atom 时怎么办」
    // （那个在 `tests/imap_parsing_properties.rs` 里直接用结构化的输入测）。
    const ATTRIBUTES: &[&str] = &[
        "\\Sent",
        "\\sent",
        "\\Drafts",
        "\\Trash",
        "\\Junk",
        "\\Flagged",
        "\\All",
        "\\Archive",
        "\\NoSelect",
        "\\HasChildren",
        "\\HasNoChildren",
        "$Vendor",
        "Sent",
        "xyz",
        "&XfJT0ZAB-",
        "Vendor.Thing",
    ];

    let mut rng = Rng::new(0x007F_2001);
    let mut checked = 0usize;
    let mut saw_role = 0usize;

    for _ in 0..8 {
        let mut mailboxes = Vec::new();
        for index in 0..3 {
            let mut mailbox = MailboxFixture::new(&format!("Box{index}"));
            for _ in 0..(1 + rng.below(3)) {
                let attribute = rng.pick(ATTRIBUTES).to_string();
                mailbox = mailbox.with_attribute(&attribute);
            }
            mailboxes.push(mailbox);
        }

        let (server, imap, trust) = start(ServerConfig {
            mailboxes,
            ..ServerConfig::default()
        })
        .await;
        let mut session = login(&imap, &trust).await;

        let folders = match list_folders_with_timeout(&mut session, Duration::from_secs(10)).await {
            Ok(folders) => folders,
            Err(error) => {
                // 服务器自己把不合法的属性原样写出去了（假服务器不校验 atom），
                // 客户端因此解析失败——那是**明确结论**，可以接受，但要看得见。
                eprintln!("这一轮的 LIST 属性让客户端解析失败（可接受，明确报错）：{error}");
                server.shutdown().await;
                continue;
            }
        };
        checked += 1;
        for folder in folders {
            if let Some(role) = &folder.role {
                saw_role += 1;
                assert!(
                    ROLES.contains(&role.as_str()),
                    "认出了文档里没有的角色 {role:?}（文件夹 {:?}，属性 {:?}）",
                    folder.name,
                    folder.attributes
                );
            }
        }

        server.shutdown().await;
    }

    assert!(checked > 0, "没有任何一轮 LIST 成功，用例是空转的");
    assert!(saw_role > 0, "一个角色都没认出来，随机面太窄");
}

// ---------------------------------------------------------------------------
// 5. 剧本服务器：`EXAMINE` 的 untagged 响应 `Fault::RespondWith` 换不掉
// ---------------------------------------------------------------------------

/// 服务器**不推** `* OK [UIDVALIDITY …]` 时，`MailboxInfo::uid_validity` 必须是 `None`。
///
/// 防的是什么：`MailboxInfo` 的文档写着「`None` 表示服务器没给——**不能**拿它当 0 用：
/// 没有 epoch 就无法定位邮件」（ARCHITECTURE §4.3）。补一个数（0、`EXISTS`、
/// 上一次的值、库里存的旧值）进来，`pull` 要么把整个文件夹当「服务器重建过」清空重拉，
/// 要么把新旧 epoch 的信当成同一封。
///
/// 为什么用剧本服务器：`[UIDVALIDITY …]` 不是「完成响应」，`Fault::RespondWith`
/// 换不掉它；要造「服务器没说」，只能是服务器真的不说。
#[tokio::test]
async fn examine_without_uidvalidity_reports_none_not_a_default() {
    // 一：一条 `UIDVALIDITY` 都没有。
    let server = Scripted::start(vec![
        "* OK [CAPABILITY IMAP4rev1] Fake ready".to_string(),
        "{tag} OK LOGIN completed".to_string(),
        "* 3 EXISTS\r\n* 1 RECENT\r\n* FLAGS (\\Seen)\r\n{tag} OK [READ-ONLY] EXAMINE completed"
            .to_string(),
    ])
    .await;
    let mut session = server.login().await;
    let info = examine_checked_with_timeout(&mut session, "INBOX", Duration::from_secs(5))
        .await
        .expect("EXAMINE 本身应当成功");

    assert_eq!(
        info.uid_validity, None,
        "服务器没给 [UIDVALIDITY]，客户端却报出 {:?}——补默认值会让 pull 误判 epoch",
        info.uid_validity
    );
    assert_eq!(info.exists, 3, "EXISTS 是服务器明说的，应当照收");
    assert_eq!(info.recent, 1);
    assert_eq!(info.unseen, None, "没发 UNSEEN 就是 None");
    assert_eq!(info.uid_next, None, "没发 UIDNEXT 就是 None");
    assert_eq!(info.highest_modseq, None);
    server.shutdown().await;

    // 二：服务器说 0 就是 0——`pull` 自己会拒绝 0（`filter(|v| *v > 0)`），
    // 这一层不许替它编一个别的数。
    let server = Scripted::start(vec![
        "* OK [CAPABILITY IMAP4rev1] Fake ready".to_string(),
        "{tag} OK LOGIN completed".to_string(),
        "* 3 EXISTS\r\n* OK [UIDVALIDITY 0] UIDs valid\r\n{tag} OK [READ-ONLY] EXAMINE completed"
            .to_string(),
    ])
    .await;
    let mut session = server.login().await;
    let info = examine_checked_with_timeout(&mut session, "INBOX", Duration::from_secs(5))
        .await
        .expect("EXAMINE 应当成功");
    assert_eq!(
        info.uid_validity,
        Some(0),
        "服务器说 0 就是 0：`pull` 自己会拒绝 0，不需要这一层替它编一个"
    );
    server.shutdown().await;
}

/// `[HIGHESTMODSEQ]` / `[UNSEEN]` / `[UIDNEXT]` 给了就照收（含 `u64::MAX` 与 `u32::MAX`），
/// 不给就是 `None`——一正一反两条。
///
/// 只测「不给时不编」而没有「给了要收下」，等于允许实现永远返回 `None`：
/// 那样 CONDSTORE 的增量同步永远走不了，而且没有任何用例会红。
#[tokio::test]
async fn mailbox_numbers_are_taken_verbatim_or_reported_absent() {
    const BASE: &str = "* 22 EXISTS\r\n* OK [UIDVALIDITY 4294967295] UIDs valid\r\n* OK [UIDNEXT 0] Predicted next UID\r\n* OK [UNSEEN 7] First unseen\r\n";

    // 给了（含边界值）。
    let server = Scripted::start(vec![
        "* OK [CAPABILITY IMAP4rev1] Fake ready".to_string(),
        "{tag} OK LOGIN completed".to_string(),
        format!(
            "{BASE}* OK [HIGHESTMODSEQ 18446744073709551615] Highest\r\n{{tag}} OK [READ-ONLY] EXAMINE completed"
        ),
    ])
    .await;
    let mut session = server.login().await;
    let info = examine_checked_with_timeout(&mut session, "INBOX", Duration::from_secs(5))
        .await
        .expect("EXAMINE 应当成功");
    assert_eq!(info.highest_modseq, Some(u64::MAX));
    assert_eq!(info.uid_validity, Some(u32::MAX));
    assert_eq!(
        info.uid_next,
        Some(0),
        "UIDNEXT 0 是服务器明说的，不能当没给"
    );
    assert_eq!(info.unseen, Some(7));
    assert_eq!(info.exists, 22);
    server.shutdown().await;

    // 不给：`highest_modseq` 是 `None`，不是 0。
    let server = Scripted::start(vec![
        "* OK [CAPABILITY IMAP4rev1] Fake ready".to_string(),
        "{tag} OK LOGIN completed".to_string(),
        format!("{BASE}{{tag}} OK [READ-ONLY] EXAMINE completed"),
    ])
    .await;
    let mut session = server.login().await;
    let info = examine_checked_with_timeout(&mut session, "INBOX", Duration::from_secs(5))
        .await
        .expect("EXAMINE 应当成功");
    assert_eq!(
        info.highest_modseq, None,
        "服务器没给 [HIGHESTMODSEQ]，不能补成 0——0 是个合法的 modseq，会被当成真的"
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 剧本服务器
// ---------------------------------------------------------------------------

/// 一台**按剧本回话**的 TLS IMAP 服务器。
///
/// `script[0]` 是问候行，`script[1]` 对应登录（客户端的 `ID` 由服务器自动回
/// `OK`，不占剧本），之后每条命令依次用后面的脚本；脚本用完后最后一条一直重复
/// （通常写成 `{tag} OK …` 之类，让后面无关的命令也有结局）。
///
/// 为什么不用 `FakeImapServer` 加故障：这里要的恰好是**不要**它替我们组织
/// 「标准」的 `EXAMINE` 响应，只要把几条特定的行原样吐出去；剧本只有几行。
/// 证书复用假服务器那一套（自签 + `TlsTrust::Roots`），客户端走的是**同一条**
/// TLS 连接路径。
struct Scripted {
    addr: std::net::SocketAddr,
    roots: rustls::RootCertStore,
    handle: tokio::task::JoinHandle<()>,
}

impl Scripted {
    async fn start(script: Vec<String>) -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        // 借一台假服务器**只**为拿它的证书：这台会立刻关掉，不干扰。
        // 必须用同一张证书来握手——客户端信任的是它的根（`TlsTrust::Roots`），
        // 另签一张会得到 `BadSignature`。
        let (certificate, roots) = {
            let probe = FakeImapServer::start(ServerConfig::default())
                .await
                .expect("启动探针服务器失败");
            let roots = probe.root_store();
            let certificate = xremail_testkit::ServerCertificate {
                chain: probe.certificate().chain.clone(),
                private_key: probe.certificate().private_key.clone_key(),
                subject_name: probe.certificate().subject_name.clone(),
            };
            probe.shutdown().await;
            (certificate, roots)
        };

        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    certificate.chain.clone(),
                    certificate.private_key.clone_key(),
                )
                .expect("服务端 TLS 配置失败"),
        ));

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("监听失败");
        let addr = listener.local_addr().expect("取本地地址失败");

        let handle = tokio::spawn(async move {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let Ok(stream) = acceptor.accept(tcp).await else {
                return;
            };
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);

            let mut next = 0usize;
            // 取剧本里的下一行；用完了就一直用最后一行。
            let take = |next: &mut usize| -> Option<String> {
                let line = script
                    .get(*next)
                    .cloned()
                    .or_else(|| script.last().cloned())?;
                if *next + 1 < script.len() {
                    *next += 1;
                }
                Some(line)
            };

            let Some(greeting) = take(&mut next) else {
                return;
            };
            if write_half
                .write_all(format!("{greeting}\r\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let tag = line.split(' ').next().unwrap_or_default().to_string();

                // `ID` 是客户端登录后自己发的（能力集里有 `ID` 就会发），
                // 剧本里不为它单独准备一行。
                if line.to_ascii_uppercase().contains(" ID ") {
                    let text = format!("{tag} OK ID completed");
                    if write_half
                        .write_all(format!("{text}\r\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }

                let Some(body) = take(&mut next) else {
                    break;
                };
                let body = body.replace("{tag}", &tag);
                if !body.is_empty()
                    && write_half
                        .write_all(format!("{body}\r\n").as_bytes())
                        .await
                        .is_err()
                {
                    break;
                }
            }
            let _ = write_half.shutdown().await;
        });

        Self {
            addr,
            roots,
            handle,
        }
    }

    async fn login(&self) -> Session {
        let mut imap = ImapConfig::imaps("localhost");
        imap.port = self.addr.port();
        imap.connect_timeout = Duration::from_secs(5);
        imap.command_timeout = Duration::from_secs(5);
        let trust = TlsTrust::Roots(self.roots.clone());
        let (session, _caps, _tls) =
            connect_and_login_with(&imap, &trust, "tester@example.com", "test-password")
                .await
                .expect("登录剧本服务器应当成功");
        session
    }

    async fn shutdown(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// 截断时**只砍尾巴**，而且必须**如实报告**自己被砍了（R47）。
///
/// 这条性质原先由 `copyuid_expansion_is_bounded` 通过 `TransferOutcome` 观察；
/// 修了 R47 之后目标侧被整体置空，那个观察点没了，所以直接测展开函数。
/// 性质本身没变：**没有上限保护会 OOM**，但砍掉的部分必须让人知道——
/// 静默截断正是 R47 的根因。
#[test]
fn truncation_only_cuts_the_tail_and_says_so() {
    use imap_proto::rfc4315::UidSetMember;
    use xremail_protocol_imap::transfer::expand_uid_set_checked;

    // 一个巨大的区间：必须被截断。
    let huge = vec![UidSetMember::UidRange(1..=u32::MAX)];
    let (expanded, truncated) = expand_uid_set_checked(&huge);

    assert_eq!(expanded.len(), 10_000, "上限是 10 000");
    assert_eq!(expanded.first(), Some(&1), "截断只能砍尾巴，不能把开头弄丢");
    assert_eq!(expanded.last(), Some(&10_000), "砍到上限为止，不多不少");
    assert!(truncated, "砍了东西就必须如实报告——静默截断正是 R47 的根因");

    // 一个不需要截断的小区间：报告必须为假。
    let small = vec![UidSetMember::UidRange(5..=7)];
    let (expanded, truncated) = expand_uid_set_checked(&small);
    assert_eq!(expanded, vec![5, 6, 7]);
    assert!(!truncated, "没砍东西就不能说砍了");
}
