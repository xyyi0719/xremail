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
//! * **会话掉了就立刻重建**（B1）：`BYE` / 掉线之后不等满轮询周期，
//!   认证被拒仍然终态，连续掉线仍然退避。**注意**：这里不测「推送唤醒」——
//!   假服务器的观测模型不足以可靠地证明它（见 R42）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

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

/// 测试脚手架：镜像生产外壳 `SyncService::connect` 的错误分类。
///
/// 外壳那边是 `map_protocol_error` + `classify_session_error`（见
/// `apps/shell/src-tauri/src/service/sync.rs`）。测试里如果只写
/// `AppError::Protocol(format!("连接或登录失败: {error}"))`，就把「对面把连接关了」
/// 抹成了一句不含任何证据的文本，B1 的判据根本看不见它——那测到的就不是生产行为。
fn classified(error: AppError) -> AppError {
    xremail_sync::classify_session_error(error)
}

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

/// 收集事件的接收方，顺便记下每条事件到达的时刻。
struct Recorder {
    events: std::sync::Mutex<Vec<(AccountEvent, Duration)>>,
    base: std::time::Instant,
}

impl Recorder {
    fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
            base: std::time::Instant::now(),
        }
    }
}

impl EventSink for Recorder {
    fn emit(&self, event: AccountEvent) {
        self.events
            .lock()
            .expect("事件表加锁失败")
            .push((event, self.base.elapsed()));
    }
}

impl Recorder {
    fn snapshot(&self) -> Vec<AccountEvent> {
        self.timed().into_iter().map(|(event, _)| event).collect()
    }

    fn timed(&self) -> Vec<(AccountEvent, Duration)> {
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
    let start = std::time::Instant::now();
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

/// `always_fail` 时 `connect` 回什么错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// 连不上：语义类错误，**不**含断线证据。
    Unreachable,
    /// 连上了，但对面立刻把连接关了（`ProtocolError::Io`，走真实的
    /// [`xremail_sync::map_protocol_error`]）。
    SessionDropped,
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
    ///
    /// 拿出来当字段**公开**是为了让用例自己决定「会话类」还是「非会话类」：
    /// 这两种错误的退避序列在 B1 之后完全不一样。
    always_fail: bool,
    /// `always_fail` 时回的那个错误。
    failure: Failure,
    /// 每次 `connect` 都回「认证被拒」（授权码被改掉 / 过期）。
    always_auth_fail: bool,
    /// 这些账户 ID 只在**等待期** panic（第 1 次之后每次连接都炸）。
    ///
    /// 第 1 次必须成功：否则第一轮 `tick` 就先炸了，测到的就不是等待期。
    fail_from_second: Vec<String>,
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
            failure: Failure::Unreachable,
            always_auth_fail: false,
            fail_from_second: Vec::new(),
            attempts: AtomicU32::new(0),
        }
    }

    /// 连不上：每次 `connect` 都回一个**非**会话类的错误。
    ///
    /// `AppError::Protocol("测试要求：连接失败")` 里不含任何断线证据
    /// （判据见 `xremail_sync::is_session_recoverable`），所以它走**普通**退避。
    /// 要测「会话掉了、立刻重连」用 [`Self::session_dropped`]。
    fn failing(mut self) -> Self {
        self.always_fail = true;
        self.failure = Failure::Unreachable;
        self
    }

    /// 连上了又掉：每次 `connect` 都回「对面把连接关了」。
    ///
    /// 走的是真实链路——`ProtocolError::Io` → [`xremail_sync::map_protocol_error`]
    /// → `AppError::Network`，和假服务器真的 `DROP` 掉一条连接时 actor 拿到的
    /// 东西一模一样。用它来测**连续掉线时的退避序列**：真实 `DROP` 有「只一次」
    /// 的语义（第二次再断就没有 web 意义上的「连上过」可言了），
    /// 而退避序列要的恰恰是「每次都断」。
    fn session_dropped(mut self) -> Self {
        self.always_fail = true;
        self.failure = Failure::SessionDropped;
        self
    }

    /// 连上就回 `Auth`（等价于服务器回 `NO [AUTHENTICATIONFAILED]`）。
    fn auth_rejected(mut self) -> Self {
        self.always_auth_fail = true;
        self
    }

    fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::Relaxed)
    }
}

impl AccountConnector for TestConnector {
    fn connect<'a>(&'a self, account: &'a Account) -> ConnectFuture<'a> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;

            // 这两个分支必须在 future **内部** panic——
            // 那才是 actor 的 `catch_unwind` 覆盖到的位置。
            if self.always_panic.iter().any(|id| id == &account.id) {
                panic!("测试要求：账户 {} 每次连接都 panic", account.id);
            }
            if self.fail_from_second.iter().any(|id| id == &account.id) && attempt > 1 {
                panic!("测试要求：账户 {} 在等待期 panic", account.id);
            }
            if self.panics_left.load(Ordering::Relaxed) > 0 {
                self.panics_left.fetch_sub(1, Ordering::Relaxed);
                panic!("测试要求：这一次连接 panic");
            }
            if self.always_auth_fail {
                return Err(AppError::Auth("测试要求：服务器拒绝了这个授权码".into()));
            }
            if self.always_fail {
                return Err(match self.failure {
                    Failure::Unreachable => AppError::Protocol("测试要求：连接失败".into()),
                    Failure::SessionDropped => xremail_sync::map_protocol_error(
                        xremail_protocol_imap::ProtocolError::Io("连接被断开".into()),
                    ),
                });
            }

            let (session, _caps, _tls) = connect_and_login_with(
                &self.imap,
                &self.trust,
                "tester@example.com",
                "test-password",
            )
            .await
            .map_err(|error| classified(xremail_sync::map_protocol_error(error)))?;
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
        // 默认能力集**不含** `IDLE`（照着 163 那类服务器来的）。
        Self::start_with(messages, FakeConfig::default().capabilities).await
    }

    /// 服务器**宣告支持 `IDLE`** 的变体：调度器应当用推送代替纯等待。
    async fn start_with_idle(messages: usize) -> Self {
        let mut capabilities = FakeConfig::default().capabilities;
        capabilities.push("IDLE".to_string());
        Self::start_with(messages, capabilities).await
    }

    /// 只有 INBOX、没有任何邮件的假服务器（只关心「什么时候开始同步」）。
    async fn start_idle_empty() -> Self {
        let mut capabilities = FakeConfig::default().capabilities;
        capabilities.push("IDLE".to_string());
        let server = FakeImapServer::start(FakeConfig {
            capabilities,
            mailboxes: vec![MailboxFixture::new("INBOX")],
            ..Default::default()
        })
        .await
        .expect("启动假服务器失败");
        Self::finish(server)
    }

    async fn start_with(messages: usize, capabilities: Vec<String>) -> Self {
        Self::start_fake_config(FakeConfig {
            capabilities,
            mailboxes: vec![inbox_fixture(messages)],
            ..Default::default()
        })
        .await
    }

    /// 假服务器起好之后，拼出测试要的连接参数。
    fn finish(server: FakeImapServer) -> Self {
        let mut imap = ImapConfig::imaps(server.host());
        imap.port = server.port();
        imap.connect_timeout = Duration::from_secs(5);
        imap.command_timeout = Duration::from_secs(10);
        Self {
            trust: TlsTrust::Roots(server.root_store()),
            imap,
            server,
        }
    }

    /// 直接给一份完整的假服务器配置（故障注入要自己拼）。
    async fn start_fake_config(config: FakeConfig) -> Self {
        let server = FakeImapServer::start(config)
            .await
            .expect("启动假服务器失败");

        Self::finish(server)
    }
}

/// 一个有 `messages` 封邮件的 `INBOX`。
fn inbox_fixture(messages: usize) -> MailboxFixture {
    let mut inbox = MailboxFixture::new("INBOX");
    for index in 0..messages {
        let uid = 1_792_658_701 + index as u32;
        inbox = inbox.with_message(
            MessageFixture::new(uid)
                .with_subject(&format!("第 {uid} 封"))
                .with_body("正文"),
        );
    }
    inbox
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

/// 测试用的快节奏策略：退避 20ms 起，80ms 封顶；掉线重连的起点 5ms。
fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        poll_interval: Duration::from_secs(1),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(80),
        max_consecutive_panics: 3,
        command_timeout: Duration::from_secs(2),
        min_reconnect_backoff: Duration::from_millis(5),
    }
}

/// 「掉线重连」用的慢节奏策略：轮询五分钟、两条退避曲线差得很开。
///
/// 它存在的意义是把两件事**分开**：
///
/// * `poll_interval` 五分钟——比任何测试超时都长，等满它就是红；
/// * 普通失败退避 5s 起（第一次等 10s）；掉线重连 100ms 起（第一次等 100ms）。
///
/// 于是「10 秒内又同步了一轮」只可能是**掉线重连**做到的：等满轮询要五分钟，
/// 普通失败的第一次重试是 10 秒。
fn reconnect_policy() -> RetryPolicy {
    RetryPolicy {
        poll_interval: Duration::from_secs(300),
        min_backoff: Duration::from_secs(5),
        max_backoff: Duration::from_secs(30),
        max_consecutive_panics: 3,
        // 比默认的 30s 短得多：假服务器断线后读都超时，要靠这个值把一轮失败
        // 限定在秒级，否则「10 秒内重连」的断言跟超时本身比长短。
        command_timeout: Duration::from_secs(2),
        min_reconnect_backoff: Duration::from_millis(100),
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
    let recorder = Arc::new(Recorder::new());
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
            assert_eq!(reason, &WakeReason::Poll, "第二轮应当由轮询触发");
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
    let recorder = Arc::new(Recorder::new());
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
    let recorder = Arc::new(Recorder::new());
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
    let recorder = Arc::new(Recorder::new());
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

    // `always_fail` 回的是 `AppError::Protocol("测试要求：连接失败")`：里面**没有**
    // 任何「会话死了」的证据（见 `xremail_sync::is_session_recoverable`），
    // 所以这条用例走的是**普通**退避，与 B1 之前一模一样。
    let connector =
        Arc::new(TestConnector::new(fixture.imap.clone(), fixture.trust.clone()).failing());
    let recorder = Arc::new(Recorder::new());
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

    // 普通退避：首次 40ms（`grow` 是唯一一层翻倍，`grow(20ms)`），之后 80ms 封顶。
    // 会话类的短序列见 `repeated_session_failures_back_off_instead_of_hot_looping`。
    assert_eq!(retries[0], Duration::from_millis(40));
    assert_eq!(retries[1], Duration::from_millis(80));
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
// 认证被拒：终态，不是「再等等就好」
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_rejection_stops_the_actor_instead_of_retrying_forever() {
    let fixture = Fixture::start(0).await;
    let account = account("acct-1", 3600);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector =
        Arc::new(TestConnector::new(fixture.imap.clone(), fixture.trust.clone()).auth_rejected());
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    let events = wait_until(&recorder, "一次认证被拒", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::NeedsReauth { .. }))
    })
    .await;

    // 报的是「要人管」，不是「可重试」。
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AccountEvent::Failed { .. })),
        "认证被拒不该报成可重试失败：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AccountEvent::Stopped { .. })),
        "退出原因是「要重新授权」时不该再报 Stopped——那会把 UI 上的\
         ReauthRequired 盖成 Idle：{events:?}"
    );

    // 关键断言：**只试了一次**。错密码重试一万次也不会对。
    // 给足时间（远超过退避的 20ms / 40ms）仍应停在 1。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        connector.attempts(),
        1,
        "认证被拒后不应再重试，实际连接了 {} 次",
        connector.attempts()
    );

    // actor 已经退出，调度表里不该再留着它。
    assert!(
        !scheduler.is_running("acct-1"),
        "认证被拒后 actor 已退出，不应还算在调度中"
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

#[tokio::test]
async fn a_reauth_required_account_can_be_started_again() {
    // 这条用例锁的是 `reap_finished`：actor 退出后句柄还留在表里的话，
    // 用户改完授权码再 `start` 只会叫醒一具尸体，账户从此再不同步——
    // 而且**没有任何报错**，是最难查的那类故障。
    let fixture = Fixture::start(0).await;
    let account = account("acct-1", 3600);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector =
        Arc::new(TestConnector::new(fixture.imap.clone(), fixture.trust.clone()).auth_rejected());
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account.clone()).expect("应当能启动");
    wait_until(&recorder, "第一次认证被拒", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::NeedsReauth { .. }))
    })
    .await;

    // 模拟「用户重新保存了授权码」：再 start 一次。
    scheduler.start(account).expect("重启应当成功");

    wait_until(&recorder, "第二次也真的连了", |_| {
        connector.attempts() >= 2
    })
    .await;

    assert!(
        connector.attempts() >= 2,
        "重启后应当真的重新去连服务器，实际只连了 {} 次",
        connector.attempts()
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
    let recorder = Arc::new(Recorder::new());
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
    let recorder = Arc::new(Recorder::new());
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
        Arc::new(Recorder::new()),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    // 静默什么也不做是最坏的选择：调用方会以为同步排上了。
    assert!(scheduler.wake("没这个账户").is_err());

    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// IDLE：服务器能推的时候，不要傻等到轮询定时器
// ---------------------------------------------------------------------------

// `IDLE` 的推送叫醒**不在这里测**，原因值得写下来：
//
// * 假服务器只在一段很短的时间里接收推送（推送会连同 `IDLE` 一起被「消费」），
//   而这里要等第一轮同步跑完才第一次推——那时推送已经被丢掉了；
// * 调用 `push_exists` 本身会先抢一遍服务器状态，连带影响了紧接着建立起来的
//   那条 `IDLE` 连接（现象：推送之后出现的 `Started` 原因为 `Push`，但之后
//   再也没有第二轮推送能进来）。
//
// 所以推送语义在 `crates/xremail-sync/src/actor.rs` 的进程内单测里验
// （`idle_wait_*` 那组，直接调 `wait_for_change_inner`，不经过假服务器）。
//
// ---------------------------------------------------------------------------
// IDLE：服务器能推的时候，不要傻等到轮询定时器
// ---------------------------------------------------------------------------
/// 服务器不宣告 `IDLE` 时，仍然按轮询跑，且**不会**多出第二种唤醒原因。
///
/// 同时守住一条代价约束：不支持 `IDLE` 就**不能每轮多连一次**去确认
/// （`attempts` 应当与同步轮数同阶，而不是它的两倍）。
#[tokio::test]
async fn server_without_idle_still_polls_and_does_not_pay_an_extra_connection() {
    let fixture = Fixture::start(1).await;
    let account = account("acct-1", 1);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account.clone()).expect("应当能启动");

    wait_until(&recorder, "三轮同步", |events| {
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Synced { .. }))
            .count()
            >= 3
    })
    .await;

    let snapshot = recorder.snapshot();
    let synced: Vec<_> = snapshot
        .iter()
        .filter(|event| matches!(event, AccountEvent::Synced { .. }))
        .collect();

    for event in &synced {
        match event {
            AccountEvent::Synced { reason, .. } => assert_ne!(
                *reason,
                WakeReason::Push,
                "服务器不支持 IDLE，不该出现推送原因"
            ),
            other => panic!("应当是 Synced，实际是 {other:?}"),
        }
    }

    // 每轮一次连接；不支持 IDLE 时不允许出现「额外探测连接」。
    // 留一点余量：读取这两个数字之间可能刚好又跑完一轮。
    assert!(
        connector.attempts() as usize <= synced.len() + 1,
        "不支持 IDLE 时不该多连：attempts={}, 同步轮数={}",
        connector.attempts(),
        synced.len()
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// R28：等待期的 panic 也要看得见
// ---------------------------------------------------------------------------

/// 等待期（`IDLE` 建连）每轮都 panic → 账户必须被**放弃**，而不是假装在轮询。
///
/// 错误实现（R28 之前）：`wait_for_change` 的 `Err(payload)` 分支只写一行
/// `error!` 就当「到点」处理，于是每轮都：等 `wait` → 等待期 panic → 同步一轮 →
/// 等 `wait` → …… 账户表面上一直在轮询，实际上**一次连续失败的信号都没有**，
/// `panics` 计数永远是 0，`Abandoned` 永远不出现。
#[tokio::test]
async fn panics_during_the_wait_are_counted_and_abandon_the_account() {
    let fixture = Fixture::start_with_idle(1).await;
    let account = account("acct-wait-boom", 1);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let mut connector = TestConnector::new(fixture.imap.clone(), fixture.trust.clone());
    // 第 1 次连接成功（那是第一轮 `tick`），之后的都只在等待期炸。
    connector.fail_from_second = vec!["acct-wait-boom".to_string()];
    let connector = Arc::new(connector);
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    let events = wait_until(&recorder, "等待期 panic 导致放弃", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::Abandoned { .. }))
    })
    .await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Synced { .. }))
            .count(),
        1,
        "第一轮应当同步成功，之后才在等待期炸：{events:#?}"
    );

    // 每一次等待期 panic 都要有对应的事件（达到上限那一次直接发 `Abandoned`）。
    let panicked = events
        .iter()
        .filter(|event| matches!(event, AccountEvent::Panicked { .. }))
        .count();
    assert_eq!(
        panicked,
        fast_policy().max_consecutive_panics as usize - 1,
        "放弃之前应当逐次报告 panic：{events:#?}"
    );

    let attempts = connector.attempts();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(connector.attempts(), attempts, "放弃之后不该再连接");
    assert!(!scheduler.is_running("acct-wait-boom"), "actor 应当已退出");

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// R37：IDLE 被拒一次就降级，不要每轮都付一次握手
// ---------------------------------------------------------------------------

/// 服务器宣告了 `IDLE` 却拒绝 `IDLE` 命令 → 降回纯轮询，不再每轮多连一次。
///
/// 错误实现（R37 之前）：`idle_once` 的 `Err` 分支只写日志，不把
/// `idle_supported` 置成 `false`，于是**每一轮**都：建一条新连接 → 登录 →
/// `EXAMINE` → 失败。连接数是同步轮数的两倍，失败信息只出现在 `debug` 日志里。
#[tokio::test]
async fn a_rejected_idle_falls_back_to_polling_after_one_attempt() {
    // 163 / 沃邮箱这类服务器会宣告 `IDLE`，认证后却回带标签的 `NO`。
    let mut config = FakeConfig::default();
    config.capabilities.push("IDLE".to_string());
    config
        .faults
        .push(xremail_testkit::Fault::Reject("IDLE".to_string()));
    let fixture = Fixture::start_fake_config(config).await;

    let account = account("acct-1", 1);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    wait_until(&recorder, "至少三轮同步", |events| {
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Synced { .. }))
            .count()
            >= 3
    })
    .await;

    // 每轮同步一次连接；`IDLE` 只该在首轮之后的那次等待里试**一次**。
    // 没有降级时，每次等待都要多一条连接：4 轮同步会是 7 次连接。
    let attempts = connector.attempts() as usize;
    assert!(
        attempts <= 5,
        "IDLE 被拒一次就应当降回纯轮询，实际连接了 {attempts} 次（≈每轮两次）"
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}
// ---------------------------------------------------------------------------
// R42：`IDLE` 不会比纯轮询更晚
// ---------------------------------------------------------------------------

/// `IDLE` 到点之后，同步必须**接着等满**这一轮，而不是立刻开跑。
///
/// R42 的现场：`wait` 没按 `IdleConfig` 的边界夹，`idle_timeout` 被静默抬到
/// 下限，`IDLE` 实际保持 31 秒，一轮就是 31 秒——`IDLE` 不但没让同步变早，
/// 反而比纯轮询晚得多。
///
/// 用「**本轮到点**」这个语义来钉，不与同步本身耗时赛跑：轮询周期四秒、
/// `IDLE` 一次都没收到推送，第二轮必须落在「第一轮结束 + 四秒」之后。
/// 若把 `IDLE` 自己的超时（这里约两秒）当成到点，同步会提前一整段
/// `idle_timeout` 开跑，这条断言就失败。
///
/// 这不只是理论上的差别：`IDLE` 超时后本来还要「等满到点」，把超时当成
/// 到点是 Gmail 这类服务器下的常见退化——只要 `EXAMINE` 或 `IDLE` 稍微慢一点，
/// 就会一轮拆成两次同步。
#[tokio::test]
async fn an_idle_timeout_is_not_mistaken_for_the_end_of_the_wait() {
    // 空邮箱：同步只做一次握手，量出来的差基本就是「等」。
    let fixture = Fixture::start_idle_empty().await;
    let account = account("acct-1", 4);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector,
        recorder.clone(),
        fast_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    wait_until(&recorder, "两轮同步", |events| {
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Synced { .. }))
            .count()
            >= 2
    })
    .await;

    // `Recorder::emit` 是同步的：`Synced` 记在 `tick` 返回之后、`Started` 记在
    // 下一轮开始等之前，所以两者之差就是「等」本身花了多久。
    let timed = recorder.timed();
    let first_end = timed
        .iter()
        .find(|(event, _)| matches!(event, AccountEvent::Synced { .. }))
        .map(|(_, at)| *at)
        .expect("应当有第一轮 Synced");
    let second_start = timed
        .iter()
        .filter(|(_, at)| *at > first_end)
        .find(|(event, _)| matches!(event, AccountEvent::Started { .. }))
        .map(|(_, at)| *at)
        .expect("应当有第二轮 Started");

    let waited = second_start.saturating_sub(first_end);
    let interval = Duration::from_secs(4);
    assert!(
        waited >= interval,
        "四秒的轮询周期只等了一部分（{waited:?}）：IDLE 到点被当成了本轮到点"
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// B1：断线重连（`BYE` / 掉线之后重建会话，不是等下一轮轮询）
// ---------------------------------------------------------------------------

/// 一轮同步**中途**连接被服务器掐掉 → actor 立刻重建会话并续上，而不是等满轮询。
///
/// 现场（B1 之前）：每个 `Session` 的寿命就是一次 `tick` 或一次 `IDLE`。
/// 命令层面上掉线表现为 `ProtocolError::Imap`（协议层把「对面把连接关了」
/// 一律折成 `Imap`，见 `connection::classify_imap_error` 与 `idle::classify`），
/// 于是它被当成「这一轮失败了」：退避 5s 起、翻被再翻、一等就是一个轮询周期
/// （默认 300s）。用户看到的是「合上盖子再打开，邮件好几分钟才回来」。
///
/// 这里钉两件事：
/// * **续拉**：断线之前已经拉到的那 2 封不会丢，重连之后补齐 3 封（不是重头再来）；
/// * **快**：断线发生在第一轮 `DROP` 之后，重连同步必须在 10 秒内完成——
///   而这一份 policy 的轮询周期是 300 秒、普通失败第一次也要等 10 秒。
///
/// 阈值留得很松（10s 对 100ms 的短退避）：慢机器上 `idle_once` 空邮箱也要
/// 一秒、一次 `tick` 两三秒，卡 `<= 1s` 只会变成随机红。
#[tokio::test]
async fn a_dropped_session_is_rebuilt_at_once_instead_of_waiting_for_the_poll() {
    // `DROP-ONCE`：拿到 `UID FETCH 1:* (UID)`（拉远端 UID 集合）就断线，
    // 而且只断第一次。这个口径稳定：`FLAGS` 里没有 `U`，不会误命中别的命令。
    // 空邮箱时仍然要发这条命令，所以断线一定发生在**一轮同步的中途**。
    let fixture = Fixture::start_fake_config(FakeConfig {
        capabilities: FakeConfig::default().capabilities,
        mailboxes: vec![inbox_fixture(0)],
        faults: vec![xremail_testkit::Fault::DropOnContains {
            text: "1:* (UID)".to_string(),
            once: true,
        }],
        ..Default::default()
    })
    .await;

    let account = account("acct-drop", 30000);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        reconnect_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    // 第一轮在 `DROP` 之后失败：会话没了，但**不该**等 300 秒的轮询。
    let events = wait_until(
        &recorder,
        "掉线之后立刻重连并同步成功",
        |events| {
            events
                .iter()
                .any(|event| matches!(event, AccountEvent::Synced { .. }))
        },
    )
    .await;

    let first_synced_at = recorder
        .timed()
        .into_iter()
        .find(|(event, _)| matches!(event, AccountEvent::Synced { .. }))
        .map(|(_, at)| at)
        .expect("应当有 Synced");
    assert!(
        first_synced_at < Duration::from_secs(10),
        "掉线后 {first_synced_at:?} 才同步成功：重连没有走短退避（轮询周期 300s）\n{events:#?}"
    );

    // 失败事件里的错误要是「能给人看」的文本，不带内部标记。
    for event in &events {
        if let AccountEvent::Failed { error, .. } = event {
            assert!(
                !error.contains(xremail_sync::SESSION_CLASS_MARKER),
                "Failed 事件泄露了内部标记：{error}"
            );
        }
    }

    // 同一条连接上掉线 → 至少要连两次（第一次断在 `DROP` 上）。
    assert!(
        connector.attempts() >= 2,
        "掉线之后应当重建会话，实际只连了 {} 次",
        connector.attempts()
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

/// 认证被拒是**终态**：既不触发重连，也不该在短退避上打转（B1 不许打破它）。
///
/// 与 [`auth_rejection_stops_the_actor_instead_of_retrying_forever`] 的区别在于
/// 这里用的是「掉线重连」那一份 policy（100ms 的短退避）：如果判据把 `Auth`
/// 也归进「会话可恢复」，300ms 内就会看到好几条 `Failed { retry_in: 100ms }`。
#[tokio::test]
async fn an_auth_rejection_never_enters_the_reconnect_loop() {
    let fixture = Fixture::start(0).await;
    let account = account("acct-auth", 300);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector =
        Arc::new(TestConnector::new(fixture.imap.clone(), fixture.trust.clone()).auth_rejected());
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        reconnect_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    let events = wait_until(&recorder, "认证被拒", |events| {
        events
            .iter()
            .any(|event| matches!(event, AccountEvent::NeedsReauth { .. }))
    })
    .await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AccountEvent::Failed { .. })),
        "认证被拒不该进重连循环：{events:#?}"
    );

    // 短退避是 100ms：真是热循环的话，300ms 里至少好几轮。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(connector.attempts(), 1, "认证被拒之后不该再连接");
    assert!(!scheduler.is_running("acct-auth"), "actor 应当已退出");

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

/// 会话类失败（连上了又被掐断）照样**退避**、封顶，而不是热循环。
///
/// 现场（B1 之前）：一次掉线之后要等满 `poll_interval` 才重来；B1 把会话类
/// 失败的起点压短了，于是必须钉住另一半：短起点不等于热循环——同一套 `grow`
/// 仍然在翻倍、仍然由 `max_backoff` 封顶，绝不停在短起点上打转。
#[tokio::test]
async fn repeated_session_failures_back_off_instead_of_hot_looping() {
    let fixture = Fixture::start(0).await;
    let account = account("acct-hot", 300);
    let pool = local_store(std::slice::from_ref(&account)).await;

    // 每次 `connect` 都「连上又掉」：`ProtocolError::Io` → `AppError::Network`，
    // 按 B1 的判据算「会话可恢复」，走的是短序列。
    let connector =
        Arc::new(TestConnector::new(fixture.imap.clone(), fixture.trust.clone()).session_dropped());
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        reconnect_policy(),
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

    let timed = recorder.timed();
    let retries: Vec<Duration> = events
        .iter()
        .filter_map(|event| match event {
            AccountEvent::Failed { retry_in, .. } => Some(*retry_in),
            _ => None,
        })
        .collect();
    // 短起点 100ms（`min_reconnect_backoff`）→ 200ms → 400ms……每次翻倍；
    // 这条用例只看前三步：只要求「确实在翻倍」，不把 5s/10s 的长等待塞进一次测试。
    assert_eq!(retries[0], Duration::from_millis(100));
    assert_eq!(retries[1], Duration::from_millis(200));
    assert_eq!(retries[2], Duration::from_millis(400));

    // 相邻两次失败之间**确实**等过：不是热循环。
    let failed_at: Vec<Duration> = timed
        .iter()
        .filter(|(event, _)| matches!(event, AccountEvent::Failed { .. }))
        .map(|(_, at)| *at)
        .collect();
    for index in 1..failed_at.len().min(3) {
        let gap = failed_at[index].saturating_sub(failed_at[index - 1]);
        let reported = retries[index - 1];
        assert!(
            gap >= reported.mul_f64(0.9),
            "第 {index} 次失败按 {reported:?} 退避，实际只隔了 {gap:?}：退避没生效"
        );
    }

    scheduler.shutdown();
    fixture.server.shutdown().await;
}

// ---------------------------------------------------------------------------
// B1：`IDLE` 期间掉线
// ---------------------------------------------------------------------------

/// `IDLE` 期间服务器把会话掐了 → 重连**不等满这一轮的等待**。
///
/// 现场（B1 之前）：`wait_for_change_inner` 的 `Err` 分支只写一行 `debug!`，
/// 然后把 `idle_supported` 置 `false`、**等满到点**。于是一次 `BYE` /
/// 一次半死的连接就把用户按在轮询周期上（默认 300s）——
/// 而这正是「开着就不掉线」最常被打破的那一种。
///
/// 第 3 次连接之后（`IDLE` 的 `EXAMINE`）断开，而且**只断一次**：
/// * 重连成功 → 忘记 `IDLE` 已经失败过（这是掉线，不是「服务器拒绝 IDLE」），
///   所以之后的等待还能用 `IDLE`，不会因为一次掉线永久降级成纯轮询；
/// * 等待期的掉线退避是**绝对**的（按 `backoff` 的当前值算），重连一次就成功，
///   不会在最底下打转。
#[tokio::test]
async fn an_idle_disconnect_reconnects_instead_of_waiting_out_the_interval() {
    let fixture = Fixture::start_fake_config(FakeConfig {
        capabilities: {
            let mut capabilities = FakeConfig::default().capabilities;
            capabilities.push("IDLE".to_string());
            capabilities
        },
        mailboxes: vec![inbox_fixture(0)],
        faults: vec![xremail_testkit::Fault::DropOnOnce {
            command: "IDLE".to_string(),
        }],
        ..Default::default()
    })
    .await;

    // 轮询周期一分钟：比任何测试超时都长，于是「又同步了一轮」只可能是重连做的。
    let account = account("acct-idle-drop", 30000);
    let pool = local_store(std::slice::from_ref(&account)).await;

    let connector = Arc::new(TestConnector::new(
        fixture.imap.clone(),
        fixture.trust.clone(),
    ));
    let recorder = Arc::new(Recorder::new());
    let scheduler = Scheduler::with_options(
        pool,
        connector.clone(),
        recorder.clone(),
        reconnect_policy(),
        Arc::new(|| 1_792_658_000_000),
    );

    scheduler.start(account).expect("应当能启动");

    let events = wait_until(&recorder, "IDLE 掉线后不再等满一分钟", |events| {
        events
            .iter()
            .filter(|event| matches!(event, AccountEvent::Synced { .. }))
            .count()
            >= 2
    })
    .await;

    let synced_at: Vec<Duration> = recorder
        .timed()
        .into_iter()
        .filter(|(event, _)| matches!(event, AccountEvent::Synced { .. }))
        .map(|(_, at)| at)
        .collect();
    let gap = synced_at[1].saturating_sub(synced_at[0]);
    assert!(
        gap < Duration::from_secs(10),
        "IDLE 期间掉线之后隔了 {gap:?} 才同步：重连被压到了轮询周期上（60s）\n{events:#?}"
    );

    // 两次同步 + 一次掉线的 `IDLE`：至少三次连接。
    assert!(
        connector.attempts() >= 3,
        "掉线之后应当重建会话再等，实际只连了 {} 次",
        connector.attempts()
    );

    scheduler.shutdown();
    fixture.server.shutdown().await;
}
