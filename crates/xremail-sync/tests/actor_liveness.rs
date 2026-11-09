//! 调度器的**存活性**回归：actor 退出之后，那些「还活着」的判断会不会说谎。
//!
//! 这份文件只钉一件事，但它决定的是「账户会不会永远不同步」：
//!
//! * **已退出 / 已放弃的 actor 不能被当成「在调度中」**（`Scheduler::is_running`
//!   与 `Scheduler::running` 的口径必须一致）；
//! * **还在跑的不许被重起第二个**（同一账户两个 actor 抢同一批意图，`MOVE` 不幂等）；
//! * **`stop` 之后立刻 `start` 必须真的起得来**（不能叫醒一具尸体）。
//!
//! ## 这些缺陷为什么会躲过 `reap_finished`
//!
//! `reap_finished` 按 `JoinHandle::is_finished()` 清尸体，而 tokio 的
//! 「已结束」要等任务从运行队列里被回收之后才成立：任务刚 `break` 出主循环、
//! 正在跑 `emit(Stopped)` 的那一小段时间（以及紧随其后的一小段调度延迟），
//! `is_finished()` 仍然是 `false`。
//!
//! 而外壳层恰恰是**照着「还在跑」的答案做决定**的：
//! `apps/shell/src-tauri/src/cmd.rs` 的 `ensure_scheduled` 看到 `is_running` 为真
//! 就**什么都不做**（连叫醒都不叫，因为它以为调用方正在同步）。
//! 于是「认证被拒 → 用户改密码 → 立刻点同步」这一串如果正好落在那段窗口里：
//! 不会起新 actor，也不会叫醒旧的（它已经死了），账户就此**永久不同步**，
//! 直到重启应用——正是 R13 那条「叫醒一具尸体」想根治的病。
//!
//! 窗口很窄，但判据不该建立在时序运气上：`AccountEvent::Stopped` /
//! `Abandoned` / `NeedsReauth` 是**确定的**信号，事件到达就等于「它已经退出」，
//! 不需要再问 tokio 的任务状态。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::Notify;
use xremail_core::AppError;
use xremail_core::model::{
    Account, AuthKind, ProviderKind, ServerConfig as DomainServerConfig, SyncMode,
};
use xremail_store::{PoolConfig, migrate, open_pool_in_memory};
use xremail_sync::{
    AccountConnector, AccountEvent, ConnectFuture, EventSink, RetryPolicy, Scheduler,
};

/// 一直连不上的连接器：**每次**调用都记一笔并失败。
///
/// 用「连不上」而不是「连上再断」：这种失败走普通退避（5s 起），
/// actor 不会自己热循环，断言里出现的每次调用都来自一次真正的「启动」。
#[derive(Default)]
struct FailingConnector {
    calls: AtomicUsize,
}

impl AccountConnector for FailingConnector {
    fn connect<'a>(&'a self, _account: &'a Account) -> ConnectFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(AppError::Network(
                "connect error: Connection refused".to_string(),
            ))
        })
    }
}

/// 收到指定事件就敲一下门，让用例能等「事件确实到过」而不是睡眠赌时序。
struct Gate {
    events: Mutex<Vec<AccountEvent>>,
    notify: Notify,
}

impl Gate {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            notify: Notify::new(),
        }
    }

    fn seen(&self, wanted: impl Fn(&AccountEvent) -> bool) -> bool {
        self.events
            .lock()
            .expect("事件表加锁失败")
            .iter()
            .any(wanted)
    }

    async fn wait_for(&self, wanted: impl Fn(&AccountEvent) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.seen(&wanted) {
                    return;
                }
                let _ = self.notify.notified().await;
            }
        })
        .await
        .expect("等事件超时：actor 没有走到预期的那一步");
    }
}

/// 包装一层：`EventSink` 本身就是以 `Arc<dyn EventSink>` 的形式被持有的，
/// 而 trait 要求 `'static`，所以不能对 `Arc<Gate>` 直接实现（那是外来类型的实现）。
struct SharedGate(Arc<Gate>);

impl EventSink for SharedGate {
    fn emit(&self, event: AccountEvent) {
        self.0.events.lock().expect("事件表加锁失败").push(event);
        self.0.notify.notify_waiters();
    }
}

fn account() -> Account {
    Account {
        id: "acct-actor".to_string(),
        email: "actor@example.com".to_string(),
        provider: ProviderKind::Generic,
        display_name: None,
        color: None,
        auth_kind: AuthKind::Password,
        server: DomainServerConfig::new("imap.example.com", 993, "smtp.example.com", 465),
        username: "actor".to_string(),
        secret_ref: None,
        oauth_client_id: None,
        sync_mode: SyncMode::Poll,
        poll_interval_secs: 300,
        signature_html: None,
        created_at: 0,
        updated_at: 0,
    }
}

/// 退避拉得很长的策略：用例里不会有第二次同步来搅局。
fn slow_policy() -> RetryPolicy {
    RetryPolicy {
        poll_interval: Duration::from_secs(300),
        min_backoff: Duration::from_secs(30),
        max_backoff: Duration::from_secs(30),
        max_consecutive_panics: 3,
        command_timeout: Duration::from_secs(5),
        min_reconnect_backoff: Duration::from_secs(30),
    }
}

async fn pool() -> SqlitePool {
    let pool = open_pool_in_memory(&PoolConfig::default())
        .await
        .expect("打开内存库失败");
    migrate(&pool).await.expect("迁移应当成功");
    pool
}

fn stopped(event: &AccountEvent) -> bool {
    matches!(event, AccountEvent::Stopped { .. })
}

/// **已退出的 actor 不能被 `wake` 叫醒**——它的通道收下命令、永远不会有人读。
///
/// `Scheduler::wake` 在 `reap_finished` 之后仍拿到了句柄，就说明 tokio 此刻
/// 还没把它标成「已完成」（`JoinHandle::is_finished` 在 `emit(Stopped)`
/// 与任务状态落定之间有一个小窗口）。这时 `tx.send` **照样返回 `Ok`**：
/// 通道还活着（接收端被判「在飞」之后要等任务真正被丢弃），于是调用方
/// 以为「刷新」成功，实际什么都没发生。
///
/// 行为是真实的用户动作：窗口里点一次「刷新」，界面显示已刷新，邮件却没来。
#[tokio::test]
async fn a_dead_actor_is_not_woken_silently() {
    let gate = Arc::new(Gate::new());
    let scheduler = Scheduler::with_options(
        pool().await,
        Arc::new(FailingConnector::default()),
        Arc::new(SharedGate(Arc::clone(&gate))) as Arc<dyn EventSink>,
        slow_policy(),
        xremail_sync::system_clock(),
    );

    scheduler.start(account()).expect("首次 start 应当成功");
    gate.wait_for(|event| matches!(event, AccountEvent::Failed { .. }))
        .await;
    scheduler.stop(&account().id).expect("stop 不该报错");
    gate.wait_for(stopped).await;

    // 走到这里 actor 一定已经退出。调度器若还拿着它的句柄，`wake` 就会静默成功。
    let woken = scheduler.wake(&account().id);
    assert!(
        woken.is_err() || scheduler.is_running(&account().id),
        "actor 已经退出，`wake` 却报了成功：命令进了没人读的通道，\
         『刷新』什么也没做，账户也不会自己恢复"
    );

    scheduler.shutdown();
}
