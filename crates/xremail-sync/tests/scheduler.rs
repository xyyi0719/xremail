//! 调度器的端到端测试：假 IMAP 服务器 × 真 actor × 真实 SQLite。
//!
//! 要证明的东西（对应 ROADMAP P2 与 ISSUES R13）：
//!
//! * 账户被交给调度后**立刻同步一轮**，之后按 `poll_interval_secs` 继续；
//! * 用户点「刷新」**不用等到轮询到点**；
//! * **一轮同步 panic 不会杀掉 actor**——边界拦住它，按退避重试，之后还能成功（R13）；
//! * 连续 panic 到上限 → **放弃这个账户**，而不是热循环；
//! * 连接失败 → `Failed` 且退避时间**逐次变长**；
//! * **一个账户 panic 不影响另一个账户**（actor 隔离）；
//! * `stop` 之后 actor 退出，不再同步。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;
use xremail_core::AppError;
use xremail_core::model::{
    Account, AuthKind, ProviderKind, ServerConfig as DomainServerConfig, SyncMode,
};
use xremail_protocol_imap::{ImapConfig, TlsTrust, connect_and_login_with};
use xremail_store::dao::account as account_dao;
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::{
    AccountConnector, AccountEvent, ConnectFuture, EventSink, RetryPolicy, Scheduler, WakeReason,
};
use xremail_testkit::{FakeImapServer, MailboxFixture, MessageFixture, ServerConfig as FakeConfig};

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

/// 收集事件的接收方。
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<AccountEvent>>,
}

impl EventSink for Recorder {
    fn emit(&self, event: AccountEvent) {
        self.events.lock().expect("事件表加锁失败").push(event);
    }
}

impl Recorder {
    fn snapshot(&self) -> Vec<AccountEvent> {
        self.events.lock().expect("事件表加锁失败").clone()
    }

    fn synced(&self, account_id: &str) -> usize {
        self.snapshot()
            .iter()
            .filter(|event| {
                matches!(event, AccountEvent::Synced { account_id: id, .. } if id == account_id)
            })
            .count()
    }
}

/// 等到事件满足条件，超时就失败（并打印已有事件，便于定位）。
async fn wait_until(
    recorder: &Recorder,
    what: &str,
    predicate: impl Fn(&[AccountEvent]) -> bool,
) -> Vec<AccountEvent> {
    let start = Instant::now();
    let timeout = Duration::from_secs(20);
    loop {
        let events = recorder.snapshot();
        if predicate(&events) {
            return events;
        }
        if start.elapsed() > timeout {
            panic!(
                "等待「{what}」超时（{:?}）；已有事件：{events:#?}",
                start.elapsed()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 一个可控的连接器。
///
/// 三种行为：
/// * 正常连上假服务器；
/// * 前 N 次 `connect` **panic**（验证任务边界）；
/// * 名单里的账户**永远 panic**（验证账户之间的隔离）。
struct TestConnector {
    imap: ImapConfig,
    trust: TlsTrust,
    /// 还剩几次要 panic。
    panics_left: AtomicU32,
    /// 这些账户 ID 每次都 panic。
    always_panic: Vec<String>,
    /// 每次 `connect` 都返回错误（不 panic）。
    always_fail: bool,
    /// 被调用了几次。
    attempts: AtomicU32,
}

impl TestConnector {
    fn new(imap: ImapConfig, trust: TlsTrust) -> Self {
        Self {
            imap,
            trust,
            panics_left: AtomicU32::new(0),
            always_panic: Vec::new(),
            always_fail: false,
            attempts: AtomicU32::new(0),
        }
    }

    fn panicking(mut self) -> Self {
        self.always_fail = true;
        self
    }

    fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::Relaxed)
    }
}

impl AccountConnector for TestConnector {
    fn connect<'a>(&'a self, account: &'a Account) -> ConnectFuture<'a> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::Relaxed);

            // 这两个分支必须在 future **内部** panic——
            // 那才是 actor 的 `catch_unwind` 覆盖到的位置。
            if self.always_panic.iter().any(|id| id == &account.id) {
                panic!("测试要求：账户 {} 每次连接都 panic", account.id);
            }
            if self.panics_left.load(Ordering::Relaxed) > 0 {
                self.panics_left.fetch_sub(1, Ordering::Relaxed);
                panic!("测试要求：这一次连接 panic");
            }
            if self.always_fail {
                return Err(AppError::Protocol("测试要求：连接失败".into()));
            }

            let (session, _caps, _tls) = connect_and_login_with(
                &self.imap,
                &self.trust,
                "tester@example.com",
                "test-password",
            )
            .await
            .map_err(|error| AppError::Protocol(format!("连接或登录失败: {error}")))?;
            Ok(session)
        })
    }
}

/// 假服务器 + 连它的客户端参数。
struct Fixture {
    server: FakeImapServer,
    imap: ImapConfig,
    trust: TlsTrust,
}

impl Fixture {
    async fn start(messages: usize) -> Self {
        let mut inbox = MailboxFixture::new("INBOX");
        for index in 0..messages {
            let uid = 1_792_658_701 + index as u32;
            inbox = inbox.with_message(
                MessageFixture::new(uid)
                    .with_subject(&format!("第 {uid} 封"))
                    .with_body("正文"),
            );
        }

        let server = FakeImapServer::start(FakeConfig {
            mailboxes: vec![inbox],
            ..Default::default()
        })
        .await
        .expect("启动假服务器失败");

        let mut imap = ImapConfig::imaps(server.host());
        imap.port = server.port();
        imap.connect_timeout = Duration::from_secs(5);
        imap.command_timeout = Duration::from_secs(10);

        let trust = TlsTrust::Roots(server.root_store());

        Self {
            server,
            imap,
            trust,
        }
    }
}

/// 一个内存库，账户已落库（`folder` / `message` 的外键要指着它）。
async fn local_store(accounts: &[Account]) -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");
    for account in accounts {
        account_dao::upsert_account(&pool, account)
            .await
            .expect("插入账户失败");
    }
    pool
}

/// 一个账户（`poll_interval_secs` 由调用方决定）。
fn account(id: &str, poll_interval_secs: i64) -> Account {
    Account {
        id: id.to_string(),
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
        poll_interval_secs,
        signature_html: None,
        created_at: 0,
        updated_at: 0,
    }
}

/// 测试用的快节奏策略：退避 20ms 起，80ms 封顶。
fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        poll_interval: Duration::from_secs(1),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(80),
        max_consecutive_panics: 3,
    }
}

// ---------------------------------------------------------------------------
// 正常节奏
// ---------------------------------------------------------------------------

#[tokio::test]
async fn startup_syncs_at_once_then_keeps_polling() {
    let fixture = Fixture::start(2).await;
    let account = account("acct-1", 1);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account.clone()).expect("应当能启动");

    // 第一轮：启动即同步，不需要等轮询。
    let events = wait_until(&recorder, "第一轮同步", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Synced { .. }))
    })
    .await;
    match &events[0] {
        AccountEvent::Started { reason, .. } => assert_eq!(*reason, WakeReason::Startup),
        other => panic!("第一个事件应当是 Started，实际是 {other:?}"),
    }
    match events
        .iter()
        .find(|event| matches!(event, AccountEvent::Synced { .. }))
        .expect("应当有 Synced")
    {
        AccountEvent::Synced { summary, .. } => {
            assert_eq!(summary.added, 2, "首次同步应当拉到 2 封");
            assert_eq!(summary.folders, 1);
        }
        other => panic!("应当是 Synced，实际是 {other:?}"),
    }

    // 第二轮：轮询到点后又跑了一轮，这次没有新邮件。
    wait_until(&recorder, "第二轮轮询", |events| {
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Synced { .. }))
            .count()
            >= 2
    })
    .await;
    let synced: Vec<_> = recorder
        .snapshot()
        .into_iter()
        .filter(|event| matches!(event, AccountEvent::Synced { .. }))
        .collect();
    match &synced[1] {
        AccountEvent::Synced {
            reason, summary, ..
        } => {
            assert_eq!(*reason, WakeReason::Poll, "第二轮应当由轮询触发");
            assert_eq!(summary.added, 0, "第二轮不该重复拉");
        }
        other => panic!("应当是 Synced，实际是 {other:?}"),
    }

    // 真的连了两次服务器（不是复用了「已完成」的状态）。
    assert!(connector.attempts() >= 2, "应当至少连接两次");

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

#[tokio::test]
async fn manual_wake_does_not_wait_for_the_poll_interval() {
    let fixture = Fixture::start(1).await;
    // 轮询间隔故意设得很长（1 小时）：只有手动叫醒才可能这么快跑第二轮。
    let account = account("acct-1", 3600);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::with_options(
        pool,
        connector,
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");
    wait_until(&recorder, "第一轮同步", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Synced { .. }))
    })
    .await;

    scheduler.wake("acct-1").expect("应当能叫醒");

    wait_until(&recorder, "手动触发的第二轮", |events| {
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Synced { .. }))
            .count()
            >= 2
    })
    .await;

    let second = recorder
        .snapshot()
        .into_iter()
        .filter(|event| matches!(event, AccountEvent::Synced { .. }))
        .nth(1)
        .expect("应当有第二轮");
    match second {
        AccountEvent::Synced { reason, .. } => assert_eq!(reason, WakeReason::Manual),
        other => panic!("应当是 Synced，实际是 {other:?}"),
    }

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// R13：panic 边界
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_panicking_round_is_contained_and_the_actor_retries() {
    let fixture = Fixture::start(1).await;
    let account = account("acct-1", 3600);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = TestConnector::new(fixture.imap.clone(), fixture.trust.clone());
    // 前两次连接 panic，第三次正常——证明 actor 没被打死。
    connector.panics_left.store(2, Ordering::Relaxed);
    let connector = Arc::new(connector);
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::with_options(
        pool,
        connector,
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    let events = wait_until(&recorder, "panic 之后仍然同步成功", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Synced { .. }))
    })
    .await;

    let panics = events
        .iter()
        .filter(|event| matches!(event, AccountEvent::Panicked { .. }))
        .count();
    assert_eq!(panics, 2, "应当报告 2 次 panic，实际事件：{events:#?}");

    // panic 之后仍然是同一次调度，没有放弃。
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AccountEvent::Abandoned { .. })),
        "2 次 panic 不该放弃账户"
    );
    assert!(scheduler.is_running("acct-1"), "actor 应当还活着");

    // `reason` 记录的是「**这一轮**是被什么触发的」：
    // 第一次是启动（Startup），之后的退避重试才是 Retry。
    let retry_reasons: Vec<WakeReason> = events
        .iter()
        .filter_map(|event| match event {
            AccountEvent::Panicked { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        retry_reasons,
        vec![WakeReason::Startup, WakeReason::Retry],
        "重试轮次应当被标成 Retry"
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

#[tokio::test]
async fn repeated_panics_abandon_the_account_instead_of_hot_looping() {
    let fixture = Fixture::start(0).await;
    let account = account("acct-boom", 3600);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let mut connector = TestConnector::new(fixture.imap.clone(), fixture.trust.clone());
    connector.always_panic = vec!["acct-boom".to_string()];
    let connector = Arc::new(connector);
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    wait_until(&recorder, "放弃账户", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Abandoned { .. }))
    })
    .await;

    // 放弃之后 actor 退出，事件流以 Stopped 结尾。
    wait_until(&recorder, "actor 退出", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Stopped { .. }))
    })
    .await;

    // 连续 panic 3 次（策略上限）就停，不会无限重试。
    assert_eq!(
        connector.attempts(),
        3,
        "应当恰好试 3 次就放弃（max_consecutive_panics）"
    );

    // 留给调度器一点时间；确认它真的不再跑了。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(connector.attempts(), 3, "放弃之后不该再连接");

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 失败与退避
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connection_failures_back_off_and_retry() {
    let fixture = Fixture::start(0).await;
    let account = account("acct-1", 3600);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector =
        Arc::new(TestConnector::new(fixture.imap.clone(), fixture.trust.clone()).panicking());
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::with_options(
        pool,
        connector,
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    let events = wait_until(&recorder, "至少 3 次失败", |events| {
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Failed { .. }))
            .count()
            >= 3
    })
    .await;

    let retries: Vec<Duration> = events
        .iter()
        .filter_map(|event| match event {
            AccountEvent::Failed { retry_in, .. } => Some(*retry_in),
            _ => None,
        })
        .collect();

    // 20ms → 40ms → 80ms（封顶）。
    assert_eq!(retries[0], Duration::from_millis(20));
    assert_eq!(retries[1], Duration::from_millis(40));
    assert_eq!(retries[2], Duration::from_millis(80));

    // 失败是「可重试」，不是「放弃」。
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AccountEvent::Abandoned { .. }))
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 隔离
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_panicking_account_does_not_stop_the_other() {
    let fixture = Fixture::start(1).await;
    let healthy = account("acct-healthy", 3600);
    let broken = account("acct-broken", 3600);
    let pool = local_store(&[healthy.clone(), broken.clone()]).await;

    let mut connector = TestConnector::new(fixture.imap.clone(), fixture.trust.clone());
    connector.always_panic = vec!["acct-broken".to_string()];
    let connector = Arc::new(connector);
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::with_options(
        pool,
        connector,
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(broken).expect("应当能启动坏账户");
    scheduler.start(healthy).expect("应当能启动好账户");

    // 坏账户被放弃……
    wait_until(&recorder, "坏账户被放弃", |events| {
        events.iter().any(|event| {
            matches!(event, AccountEvent::Abandoned { account_id, .. } if account_id == "acct-broken")
        })
    })
    .await;

    // ……与此同时好账户同步成功。
    wait_until(&recorder, "好账户同步成功", |events| {
        events.iter().any(|event| {
            matches!(event, AccountEvent::Synced { account_id, .. } if account_id == "acct-healthy")
        })
    })
    .await;

    assert_eq!(recorder.synced("acct-healthy"), 1);
    assert_eq!(recorder.synced("acct-broken"), 0);

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// 停止
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_ends_the_actor_and_no_further_sync_happens() {
    let fixture = Fixture::start(1).await;
    let account = account("acct-1", 1);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");
    wait_until(&recorder, "至少一轮同步", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Synced { .. }))
    })
    .await;

    scheduler.stop("acct-1").expect("应当能停止");
    wait_until(&recorder, "actor 退出", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Stopped { .. }))
    })
    .await;

    assert!(!scheduler.is_running("acct-1"));
    assert!(scheduler.running().is_empty());

    let attempts_after_stop = connector.attempts();
    // 轮询间隔 1s，等 2s 看它是否还会自己跑起来。
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        connector.attempts(),
        attempts_after_stop,
        "停止之后不该再连接服务器"
    );

    fixture.server.shutdown().await;
}

#[tokio::test]
async fn waking_an_unscheduled_account_is_an_error() {
    let fixture = Fixture::start(0).await;
    let pool = local_store(&[]).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let scheduler = Scheduler::with_options(
        pool,
        connector,
        Arc::new(Recorder::default()),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    // 静默什么也不做是最坏的选择：调用方会以为同步排上了。
    assert!(scheduler.wake("没这个账户").is_err());

    fixture.server.shutdown().await;
}
