//! `IDLE` 客户端侧的端到端测试：真客户端 × 假服务器。
//!
//! 这些用例钉住的是「**客户端**有没有按 RFC 2177 做对」，而不是假服务器
//! （假服务器自己的 IDLE 行为由 `xremail-testkit/tests/idle.rs` 负责）。
//!
//! 每条用例都写明了它在什么**错误实现**上会失败——只跑绿的用例没有价值。

use std::sync::Arc;
use std::time::Duration;

use xremail_protocol_imap::{
    IdleConfig, IdleEvent, ImapConfig, Session, TlsTrust, connect_and_login_with, idle_once,
};
use xremail_testkit::{FakeImapServer, MailboxFixture, MessageFixture, ServerConfig};

/// 起一个假服务器并返回可直接用的客户端配置。
///
/// 服务器用 `Arc` 包着：推送要在一个 `spawn` 出去的任务里做（模拟「服务器在
/// 客户端等待期间说话」），而服务器的生命周期得比那个任务长。
async fn start(config: ServerConfig) -> (Arc<FakeImapServer>, ImapConfig, TlsTrust) {
    let server = FakeImapServer::start(config)
        .await
        .expect("启动假服务器失败");

    let mut imap = ImapConfig::imaps(server.host());
    imap.port = server.port();
    imap.connect_timeout = Duration::from_secs(5);
    imap.command_timeout = Duration::from_secs(10);

    let trust = TlsTrust::Roots(server.root_store());

    (Arc::new(server), imap, trust)
}

/// 默认能力集**不含** `IDLE`（见 `ServerConfig::default`，默认照着 163 那类
/// 服务器来的）。这里显式加上，因为 IDLE 用例测的就是「服务器支持时客户端怎么做」。
fn config_with_inbox() -> ServerConfig {
    ServerConfig {
        capabilities: vec![
            "IMAP4rev1".to_string(),
            "ID".to_string(),
            "UIDPLUS".to_string(),
            "IDLE".to_string(),
        ],
        mailboxes: vec![
            MailboxFixture::new("INBOX")
                .with_message(MessageFixture::new(1).with_subject("一"))
                .with_message(MessageFixture::new(2).with_subject("二")),
        ],
        ..ServerConfig::default()
    }
}

async fn login(imap: &ImapConfig, trust: &TlsTrust) -> Session {
    let (session, _caps, _tls) =
        connect_and_login_with(imap, trust, "tester@example.com", "test-password")
            .await
            .expect("登录应当成功");
    session
}

/// 短超时：IDLE 的等待不该在测试里拖时间。
fn idle_config(idle_timeout: Duration) -> IdleConfig {
    IdleConfig::new(Duration::from_secs(5), idle_timeout, "INBOX")
}

// ---------------------------------------------------------------------------
// 正常路径
// ---------------------------------------------------------------------------

/// 没人推送时，到点返回 `Timeout`，并且 **session 被还回来、还能继续用**。
///
/// 错误实现：`done()` 之后丢掉返回的 session（或干脆不发 `DONE`），
/// 那样后面的 `NOOP` 会失败。
#[tokio::test]
async fn no_push_times_out_and_returns_a_usable_session() {
    let (_server, imap, trust) = start(config_with_inbox()).await;
    let session = login(&imap, &trust).await;

    let outcome = idle_once(session, &idle_config(Duration::from_millis(300)))
        .await
        .expect("IDLE 不该失败");

    assert_eq!(
        outcome.event,
        IdleEvent::Timeout,
        "300ms 内没人说话，应当是 Timeout"
    );

    // 关键：session 真的还回来了，而且协议状态干净。
    let mut session = outcome.session;
    xremail_protocol_imap::run_checked(&mut session, "NOOP")
        .await
        .expect("DONE 之后 session 应当仍然可用");
}

/// 服务器推 `EXISTS` → 客户端应当在**超时之前**醒来，并报 `MailboxChanged`。
///
/// 错误实现：`wait_with_timeout` 的 `_interrupt` 绑成 `_`（立刻 drop），
/// 等待会瞬间返回 `ManualInterrupt`——那样这条用例会因为「事件不对」而失败。
#[tokio::test]
async fn exists_push_wakes_before_the_timeout() {
    let (server, imap, trust) = start(config_with_inbox()).await;
    let session = login(&imap, &trust).await;

    // 10 秒超时；推送在 200ms 后到——只要醒来就算过，
    // 因此在「醒来时机」上这条用例是严格的，在「总耗时」上是宽松的。
    let config = idle_config(Duration::from_secs(10));

    let pusher = {
        let server = server.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            server.push_exists("INBOX", 3);
        })
    };

    let started = std::time::Instant::now();
    let outcome = idle_once(session, &config).await.expect("IDLE 不该失败");
    let elapsed = started.elapsed();

    pusher.await.expect("推送任务不该 panic");

    assert_eq!(outcome.event, IdleEvent::MailboxChanged);
    assert!(
        elapsed < Duration::from_secs(5),
        "应当在超时（10s）之前被推送叫醒，实际用了 {elapsed:?}"
    );

    // session 仍然可用。
    let mut session = outcome.session;
    xremail_protocol_imap::run_checked(&mut session, "NOOP")
        .await
        .expect("推送之后 session 应当仍然可用");
}

/// 服务器推 `EXPUNGE` 同样算「有变化」。
///
/// 这条单独写，是因为 `EXPUNGE` 在 RFC 3501 里是**独立**的响应类型
/// （不是 `MailboxData`），实现容易只处理 `EXISTS` 而漏掉它。
#[tokio::test]
async fn expunge_push_also_counts_as_a_change() {
    let (server, imap, trust) = start(config_with_inbox()).await;
    let session = login(&imap, &trust).await;

    let config = idle_config(Duration::from_secs(10));

    let pusher = {
        let server = server.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            server.push_expunge("INBOX", 1);
        })
    };

    let outcome = idle_once(session, &config).await.expect("IDLE 不该失败");
    pusher.await.expect("推送任务不该 panic");

    assert_eq!(outcome.event, IdleEvent::MailboxChanged);
}

/// 连做两次 IDLE，中间用同一个 session——证明「借出去、还回来」是可循环的。
///
/// 这是调度器真正需要的性质：它会在同一个连接上一遍遍重发 `IDLE`。
/// 错误实现：第二次 `init()` 因为连接状态没复位而失败。
#[tokio::test]
async fn idle_can_be_reissued_on_the_same_session() {
    let (_server, imap, trust) = start(config_with_inbox()).await;
    let session = login(&imap, &trust).await;

    let config = idle_config(Duration::from_millis(200));

    let first = idle_once(session, &config).await.expect("第一次 IDLE");
    assert_eq!(first.event, IdleEvent::Timeout);

    let second = idle_once(first.session, &config)
        .await
        .expect("第二次 IDLE 应当能在同一 session 上发起");
    assert_eq!(second.event, IdleEvent::Timeout);

    let mut session = second.session;
    xremail_protocol_imap::run_checked(&mut session, "NOOP")
        .await
        .expect("两次 IDLE 之后 session 应当仍然可用");
}

// ---------------------------------------------------------------------------
// 能力与降级
// ---------------------------------------------------------------------------

/// 服务器不宣告 `IDLE` → 返回 `Unsupported`，而且 **session 没被消耗**。
///
/// 这条最重要：`Unsupported` 必须是**正常返回**而不是错误。
/// 若把它做成 `Err`，调度器会对每一台不支持 IDLE 的服务器
/// （163、沃邮箱都是）走失败退避，日志里刷满无意义的告警。
#[tokio::test]
async fn server_without_idle_capability_reports_unsupported() {
    let config = ServerConfig {
        mailboxes: vec![MailboxFixture::new("INBOX")],
        // 去掉 IDLE，模拟 163 / 沃邮箱那类服务器。
        capabilities: vec!["IMAP4rev1".to_string(), "UIDPLUS".to_string()],
        ..ServerConfig::default()
    };

    let (_server, imap, trust) = start(config).await;
    let session = login(&imap, &trust).await;

    let outcome = idle_once(session, &idle_config(Duration::from_secs(10)))
        .await
        .expect("不支持 IDLE 不该是错误");

    assert_eq!(outcome.event, IdleEvent::Unsupported);

    // session 还回来了，而且可用——能力检查发生在借出连接之前。
    let mut session = outcome.session;
    xremail_protocol_imap::run_checked(&mut session, "NOOP")
        .await
        .expect("Unsupported 之后 session 应当仍然可用");
}

/// 服务器拒绕 `EXAMINE` 时，退回 `SELECT` 仍能拿到推送。
///
/// 错误实现：只试 `EXAMINE`，被拒就报错——那样这类服务器（有代理/网关的
/// 环境里很常见）就永远用不上 `IDLE`，而失败理由（“EXAMINE 失败”）对用户
/// 完全不可读。
#[tokio::test]
async fn falls_back_to_select_when_examine_is_rejected() {
    let config = ServerConfig {
        reject_examine: true,
        ..config_with_inbox()
    };

    let (_server, imap, trust) = start(config).await;
    let session = login(&imap, &trust).await;

    let outcome = idle_once(session, &idle_config(Duration::from_millis(200)))
        .await
        .expect("EXAMINE 被拒也应当能拿到 IDLE");

    assert_eq!(outcome.event, IdleEvent::Timeout);
}

/// 配置里的 `idle_timeout` 再离谱，也不会超过 RFC 2177 的 29 分钟。
///
/// 这是防回归：真有人把 `idle_timeout` 配成两小时，服务器会先按不活跃踢掉连接，
/// 表现为「每天固定掉线几次」，很难归因。
#[tokio::test]
async fn oversized_idle_timeout_is_clamped_by_config() {
    let config = IdleConfig::new(
        Duration::from_secs(5),
        Duration::from_secs(3 * 60 * 60),
        "INBOX",
    );
    assert_eq!(config.idle_timeout, IdleConfig::MAX_IDLE_TIMEOUT);
}
