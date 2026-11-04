//! 账户调度器（AccountActor）与后台任务的 panic 边界。
//!
//! ## 为什么要有调度器
//!
//! [`crate::sync_account`] 只负责「**一轮**同步」：连上、走完所有文件夹、断开。
//! 什么时候再跑下一轮、上一个账户卡住时别的账户还能不能跑、
//! 某一轮 panic 了会不会把整个应用带走——这些问题不属于同步逻辑本身，
//! 而属于**调度**。本模块就是这一层。
//!
//! ## 形状：一个账户一个 actor
//!
//! 每个账户一个独立的任务（actor），只通过一个命令通道与外界通信：
//!
//! ```text
//! Scheduler ──start/wake/stop──▶ mpsc ──▶ AccountActor(账户 A) ──▶ EventSink
//!           ──start/wake/stop──▶ mpsc ──▶ AccountActor(账户 B) ──▶ EventSink
//! ```
//!
//! 这样做的理由：
//!
//! * **隔离**：账户 A 的服务器挂了（连接超时 30s × N 个文件夹），
//!   不会让账户 B 的同步排队等它；
//! * **各自节奏**：`account.poll_interval_secs` 是**每账户**的设置，
//!   163 想 10 分钟一次、QQ 想 1 分钟一次，互不干扰；
//! * **可被叫醒**：用户点「刷新」时把一轮同步插到队首，不必等轮询到点。
//!
//! ## R13：panic 不能拖垮应用
//!
//! `[profile.release] panic = "unwind"` 已经改过来了（见仓库根 `Cargo.toml`），
//! 但**只改 profile 不够**——tokio 里 panic 会顺着任务往上走，
//! 没有边界的话一个 panic 就是一个 `JoinError`，重试与否全靠运气。
//!
//! 所以每一轮同步都包在 [`AssertUnwindSafe`] + `catch_unwind` 里：
//!
//! * panic **只停在那一轮**，actor 本身活着，按退避重试；
//! * 事件通道照常报告 `Panicked`，UI 能显示「这个账户出错了」；
//! * 连续 panic 超过 [`RetryPolicy::max_consecutive_panics`] 次才**放弃**这个账户，
//!   避免「panic → 立刻重试 → 立刻 panic」的热循环把 CPU 打满。
//!
//! 事件回调（[`EventSink`]）自己也包了一层 `catch_unwind`：
//! UI 层的回调出错不该让同步停摆。
//!
//! ## 还没做的
//!
//! 本轮只做**轮询**（含手动触发与退避重试）。`IDLE`（长连接推送）需要
//! 客户端侧的 `IDLE` 保持与假服务器侧的支持，见 ROADMAP P2 的残余说明；
//! 这里已经把「唤醒 → 立刻同步」的通道留好了，接 IDLE 时不用改调用方。

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use xremail_core::model::Account;
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::{IdleConfig, IdleEvent, Session, idle_once};

use crate::net::SyncOptions;
use crate::sync::{SyncSummary, sync_account_as_with};

/// 一轮同步被触发的原因。
///
/// 只影响日志与事件载荷——处置逻辑完全一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    /// 应用刚启动 / 账户刚被加入调度。
    Startup,
    /// 用户点了「刷新」。
    Manual,
    /// 轮询到点。
    Poll,
    /// 服务器通过 `IDLE` 主动推了变更。
    ///
    /// 与 `Poll` 的区别只在于「谁先叫醒的」：`Poll` 是我们等到了定时器，
    /// `Push` 是服务器先说话了。两者会触发**完全一样**的同步。
    Push,
    /// 上一轮失败后的退避重试。
    Retry,
}

impl WakeReason {
    /// 稳定的短名称，给日志与前端用。
    pub fn as_str(&self) -> &'static str {
        match self {
            WakeReason::Startup => "startup",
            WakeReason::Manual => "manual",
            WakeReason::Poll => "poll",
            WakeReason::Push => "push",
            WakeReason::Retry => "retry",
        }
    }
}

/// 连上并登录一个账户。
///
/// **凭据从哪来是调用方的事**：`xremail-sync` 不认识钥匙串，也不认识 Tauri。
/// 应用层的实现大概是「先查内存里本次会话的授权码，再查平台钥匙串」。
///
/// 返回的 [`Session`] 必须是**已登录**的（后续命令都是安全命令层，
/// 服务器拒绝时会报错，不会被当成「服务器上什么都没了」）。
pub trait AccountConnector: Send + Sync + 'static {
    /// 连上并登录。
    fn connect<'a>(&'a self, account: &'a Account) -> ConnectFuture<'a>;
}

/// [`AccountConnector::connect`] 返回的 future。
///
/// 手写 `Box<dyn Future>` 而不是引入 `async-trait`：
/// 少一个依赖，且这里只有一条方法。
pub type ConnectFuture<'a> = Pin<Box<dyn Future<Output = AppResult<Session>> + Send + 'a>>;

impl<F> AccountConnector for F
where
    F: for<'a> Fn(&'a Account) -> ConnectFuture<'a> + Send + Sync + 'static,
{
    fn connect<'a>(&'a self, account: &'a Account) -> ConnectFuture<'a> {
        self(account)
    }
}

/// 调度器对外报告的事件。
///
/// 刻意做成**同步**回调：actor 不该因为 UI 发送事件慢而拖住同步。
#[derive(Debug, Clone)]
pub enum AccountEvent {
    /// 一轮同步开始。
    Started {
        account_id: String,
        reason: WakeReason,
    },
    /// 一轮同步成功。
    Synced {
        account_id: String,
        reason: WakeReason,
        summary: SyncSummary,
    },
    /// 一轮同步失败（可重试）。
    Failed {
        account_id: String,
        reason: WakeReason,
        error: String,
        /// 下一轮在多久之后。
        retry_in: Duration,
    },
    /// 一轮同步 panic 了（已被边界拦住，actor 还活着）。
    Panicked {
        account_id: String,
        reason: WakeReason,
        message: String,
        retry_in: Duration,
    },
    /// 连续 panic 太多次，放弃这个账户。
    Abandoned { account_id: String, message: String },
    /// 服务器拒绝了凭据——**要人动手**，actor 已自行停掉。
    ///
    /// 与 [`AccountEvent::Failed`] 的区别是「要不要人管」：
    /// `Failed` 是「等会儿自己会再试」，这个是「你不换密码就永远好不了」。
    /// 分开它是为了**不让无限重连掩盖一个假授权码**（ARCHITECTURE §4 第 8 步）。
    NeedsReauth { account_id: String, message: String },
    /// actor 退出（正常停止，或放弃之后）。
    Stopped { account_id: String },
}

impl AccountEvent {
    /// 事件对应的账户。
    pub fn account_id(&self) -> &str {
        match self {
            AccountEvent::Started { account_id, .. }
            | AccountEvent::Synced { account_id, .. }
            | AccountEvent::Failed { account_id, .. }
            | AccountEvent::Panicked { account_id, .. }
            | AccountEvent::Abandoned { account_id, .. }
            | AccountEvent::NeedsReauth { account_id, .. }
            | AccountEvent::Stopped { account_id } => account_id,
        }
    }
}

/// 事件的接收方（应用层：转成 Tauri 事件；测试：收进 `Vec`）。
pub trait EventSink: Send + Sync + 'static {
    /// 收到一个事件。**实现方不要阻塞**。
    fn emit(&self, event: AccountEvent);
}

/// 不关心事件的接收方。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: AccountEvent) {}
}

/// 取当前时间的函数（epoch 毫秒）。
///
/// 做成可注入的，测试里才能断言落到库里的时间戳。
/// 生产用 [`system_clock`]。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 系统时钟。
pub fn system_clock() -> Clock {
    Arc::new(|| {
        use std::time::{SystemTime, UNIX_EPOCH};

        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// 调度节奏与重试策略。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 账户没配 `poll_interval_secs`（或配成非正数）时的兜底间隔。
    pub poll_interval: Duration,
    /// 失败后的首次退避。
    pub min_backoff: Duration,
    /// 退避上限。
    pub max_backoff: Duration,
    /// 连续 panic 多少次后放弃这个账户。
    ///
    /// 没有上限的话，「panic → 立刻重试 → 立刻 panic」会把 CPU 打满，
    /// 而且日志会被同一条 panic 刷爆。
    pub max_consecutive_panics: u32,
    /// 单条 IMAP 命令的超时（目前只用于 `IDLE` 的 `EXAMINE` / `IDLE` / `DONE`）。
    ///
    /// 与 [`Self::poll_interval`] 分开：前者是「等服务器回话」的耐心，
    /// 后者是「两轮同步之间隔多久」。把两者混用会导致轮询间隔很长时
    /// 一条卡住的命令要等上几分钟才报错。
    pub command_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // 与 `xremail-api` 里账户默认值（300s）对齐。
            poll_interval: Duration::from_secs(300),
            min_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(300),
            max_consecutive_panics: 3,
            command_timeout: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// 退避翻倍，封顶 [`Self::max_backoff`]。
    pub fn grow(&self, current: Duration) -> Duration {
        let doubled = current.saturating_mul(2);
        doubled.min(self.max_backoff).max(self.min_backoff)
    }

    /// 某个账户的正常轮询间隔。
    ///
    /// 库里存的是秒；非正数（历史数据或手改过的库）走兜底，
    /// 不让它变成一个 0 秒的热循环。
    pub fn interval_for(&self, account: &Account) -> Duration {
        if account.poll_interval_secs > 0 {
            Duration::from_secs(account.poll_interval_secs as u64)
        } else {
            self.poll_interval
        }
    }
}

/// 一轮同步失败的原因。
///
/// 不实现 `Clone`：`AppError` 本身不是 `Clone`（它包 `Box<dyn Error>` 之类的载荷），
/// 而这里也确实不需要复制——它只在 `run` 里被消费一次。
#[derive(Debug)]
enum TickFailure {
    /// 连接失败、服务器拒绝、落库出错……
    ///
    /// **保留 `AppError` 而不是提前压成 `String`**：
    /// 这里要分辨的是「能不能重试」——`AppError::Auth` 重试一万次也不会成功，
    /// 压成字符串后只能靠比对错误文本，那是脆的。
    Failed(AppError),
    /// panic 了。已经被边界拦住。
    Panicked(String),
}

/// 调度器发出的命令。
#[derive(Debug)]
enum Command {
    /// 立刻同步一轮。
    Sync(WakeReason),
    /// 停下来（跑完当前这一轮之后）。
    Stop,
}

/// 一个正在跑的 actor。
#[derive(Debug)]
struct ActorHandle {
    tx: mpsc::UnboundedSender<Command>,
    join: JoinHandle<()>,
}

/// 把已经退出的 actor 从表里清掉。
///
/// `tx.send` 到一个死任务不会报错（通道还在），所以光看句柄存在与否
/// 分不清「在跑」和「跑完了」。清掉之后 `start` 才会真的重新起一个：
/// 否则认证被拒后用户改完授权码，`start` 只会叫醒一具尸体，账户再也不同步。
fn reap_finished(actors: &mut HashMap<String, ActorHandle>) {
    actors.retain(|_, handle| !handle.join.is_finished());
}

/// 单个账户的 actor。
///
/// 只被 [`Scheduler`] 构造；`run` 在独立任务里跑。
struct AccountActor {
    account: Account,
    pool: SqlitePool,
    connector: Arc<dyn AccountConnector>,
    sink: Arc<dyn EventSink>,
    policy: RetryPolicy,
    clock: Clock,
    /// 服务器是否支持 `IDLE`。
    ///
    /// `None` = 还没问过（下一轮同步收尾时会问）；`Some(true)` = 支持，
    /// 等待期用 `IDLE` 代替 `sleep`；`Some(false)` = 不支持，永远只用轮询。
    ///
    /// **必须确定是真支持才连第二条连接**：不支持还每轮都试一次，等于把
    /// 「一轮一次连接」变成「一轮两次」，白白多一倍握手。
    ///
    /// 一次失败（网络抖、超时）**不会**关掉 `IDLE`——那只是这一轮运气不好。
    idle_supported: Option<bool>,
}

impl AccountActor {
    /// 跑一轮同步，**所有 panic 都停在这里**。
    ///
    /// 这是 R13 的落点：`catch_unwind` 包住整个 future（含 `connect` 与
    /// `sync_account` 内部的全部 await 点），所以 panic 不会变成
    /// 应用级崩溃，也不会留下一个半死的任务。
    ///
    /// 注意 `Session` 是**用完就 drop**（不发 `LOGOUT`）——与本仓库其它调用方
    /// 一致：连接直接关掉，服务器侧按断开处理。
    async fn tick(&mut self) -> Result<SyncSummary, TickFailure> {
        let account = self.account.clone();
        let pool = self.pool.clone();
        let connector = Arc::clone(&self.connector);
        let now_ms = (self.clock)();
        let command_timeout = self.policy.command_timeout;
        // 还不确定支不支持 `IDLE` 时，顺手在收尾时看一眼能力集。
        // 只问一次：问到了就不再问。
        let probe_idle = self.idle_supported.is_none();

        let body = async move {
            let mut session = connector.connect(&account).await?;
            let summary = sync_account_as_with(
                &pool,
                &mut session,
                &account,
                now_ms,
                crate::boot::boot_id(),
                SyncOptions {
                    // 与上面 `command_timeout` 同源：都是 `self.policy` 的字段，
                    // 这里用局部变量是为了不把 `self` 移进 future。
                    command_timeout,
                },
            )
            .await?;

            // 注意：这里复用**同一条已登录的连接**问 `CAPABILITY`，不是另开一条。
            // 探测失败不影响本轮同步结果，`None` 表示「没问到，下次再问」。
            let idle = if probe_idle {
                tokio::time::timeout(command_timeout, session.capabilities())
                    .await
                    .ok()
                    .and_then(std::result::Result::ok)
                    .map(|capabilities| capabilities.has_str("IDLE"))
            } else {
                None
            };

            Ok::<(SyncSummary, Option<bool>), AppError>((summary, idle))
        };

        // `AssertUnwindSafe`：这里确实跨了 unwind 边界继续用 `pool` / `connector`。
        // 与 `Mutex` 不同，这两者都不持有「被中断的中间状态」——
        // 连接池的 `Connection` 在 drop 时会回滚未提交事务（sqlx 自己保证）。
        match AssertUnwindSafe(body).catch_unwind().await {
            Ok(Ok((summary, idle))) => {
                if let Some(supported) = idle {
                    self.idle_supported = Some(supported);
                }
                Ok(summary)
            }
            Ok(Err(error)) => Err(TickFailure::Failed(error)),
            Err(payload) => Err(TickFailure::Panicked(panic_message(payload.as_ref()))),
        }
    }

    /// 等「下一轮该跑了」，**顺便让服务器有机会提前叫醒我们**。
    ///
    /// 返回本轮该报的 [`WakeReason`]。
    ///
    /// ## 为什么 `IDLE` 取代的是 `sleep`，而不是额外加一条唤醒通道
    ///
    /// 轮询定时器本来就在等 `wait` 这么久。把这段等待换成「`IDLE` 等服务器说话」
    /// 不改变任何语义：
    /// * 服务器先说话 → `Push`，立刻同步；
    /// * 没人说话 → `IDLE` 在 `wait` 到点时返回 `Timeout`，按 `on_timeout` 同步。
    ///
    /// 两条路最终都以「同步一轮」收尾，所以 **`IDLE` 绝不会让同步变少**，
    /// 只可能让它变早。这一点比「新加一条唤醒通道」好管得多：
    /// 推送丢了也仅仅退化成原来的延迟。
    ///
    /// ## 失败一律退回纯等待
    ///
    /// 连接不上、`IDLE` 被拒、连接半死……全都 `sleep(wait)` 之后按
    /// `on_timeout` 走。**绝不提前返回**：提前返回会让「连不上服务器」变成
    /// 「每一轮都多跑一次同步」，反而把错误放大成忙循环。
    async fn wait_for_change(&mut self, wait: Duration, on_timeout: WakeReason) -> WakeReason {
        // 与 [`Self::tick`] 同一条理由：等待期会 `connect` 并 `idle_once`，同样
        // 全是 await 点。这条分支原先**没有**包边界——它 panic 会直接撞到 tokio
        // 任务边界，任务静悄悄死掉，`reap_finished` 把尸体清掉，该账户从此不再
        // 同步，直到用户重启应用。R13 说「所有 panic 停在这里」，那就得真的
        // 盖住这条分支，而不只是 `tick`。
        let result = AssertUnwindSafe(self.wait_for_change_inner(wait, on_timeout))
            .catch_unwind()
            .await;

        match result {
            Ok(reason) => reason,
            Err(payload) => {
                tracing::error!(
                    account = %self.account.email,
                    panic = %panic_message(payload.as_ref()),
                    "等待期 panic，按到点处理"
                );
                // 不提前返回：panic 若出在 `connect` 里会每轮重现，立即返回就成了
                // 忙循环。等满 `wait` 再同步，与其它失败路径的处理保持一致。
                tokio::time::sleep(wait).await;
                on_timeout
            }
        }
    }

    /// [`Self::wait_for_change`] 的本体，只被那个包了 panic 边界的版本调用。
    async fn wait_for_change_inner(
        &mut self,
        wait: Duration,
        on_timeout: WakeReason,
    ) -> WakeReason {
        // 只在一个本该等满 `wait` 的情况下才用 `IDLE`。
        // 不确定 / 已知不支持 → 纯等待，一行代价也没有。
        if self.idle_supported != Some(true) {
            tokio::time::sleep(wait).await;
            return on_timeout;
        }

        let session = match self.connector.connect(&self.account).await {
            Ok(session) => session,
            Err(error) => {
                tracing::debug!(
                    account = %self.account.email,
                    %error,
                    "IDLE 建连失败，退回纯等待"
                );
                tokio::time::sleep(wait).await;
                return on_timeout;
            }
        };

        // `idle_timeout` 就用本轮要等的时间：这样「有人推」和「到点了」
        // 是同一个时间轴上的两个出口，不需要额外协调。
        // （`IdleConfig::new` 会把它夹到 RFC 2177 的 29 分钟以内。）
        let config = IdleConfig::new(self.policy.command_timeout, wait, "INBOX");

        match idle_once(session, &config).await {
            Ok(outcome) => match outcome.event {
                IdleEvent::MailboxChanged => WakeReason::Push,
                IdleEvent::Timeout => on_timeout,
                IdleEvent::Unsupported => {
                    // 记下来，以后不再试。
                    tracing::debug!(
                        account = %self.account.email,
                        "服务器未宣告 IDLE，以后只用轮询"
                    );
                    self.idle_supported = Some(false);
                    // 仍然等满 `wait`：本轮本来就该等到这个时刻才同步。
                    tokio::time::sleep(wait).await;
                    on_timeout
                }
            },
            Err(error) => {
                // 连接在 IDLE 期间坏了。**不**提前返回：把它当「没被叫醒」，
                // 等到点正常同步，否则坏连接会变成忙循环。
                tracing::debug!(
                    account = %self.account.email,
                    %error,
                    "IDLE 失败，本轮退回轮询"
                );
                tokio::time::sleep(wait).await;
                on_timeout
            }
        }
    }

    /// actor 主循环。
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let account_id = self.account.id.clone();
        // `None` = 立刻跑（启动时）。
        let mut delay: Option<Duration> = None;
        let mut reason = WakeReason::Startup;
        // 定时器到点时该报什么原因：正常轮询是 `Poll`，失败后的退避是 `Retry`。
        let mut timeout_reason = WakeReason::Poll;
        let mut backoff = self.policy.min_backoff;
        let mut panics: u32 = 0;
        // 因为「要人重新授权」退出时，不要再报 `Stopped`——
        // 那会把 UI 上的「需要重新授权」盖成「空闲」，等于把要人管的状态藏起来。
        let mut stopped_for_reauth = false;

        loop {
            // ---------- 1. 等到该跑的时刻 ----------
            match delay {
                None => {
                    // 立刻跑，但仍然要能被 Stop 打断。
                    match rx.try_recv() {
                        Ok(Command::Stop) | Err(mpsc::error::TryRecvError::Disconnected) => break,
                        Ok(Command::Sync(next)) => reason = next,
                        Err(mpsc::error::TryRecvError::Empty) => {}
                    }
                }
                Some(wait) => {
                    // `biased`：命令优先于到点，点「刷新」不用等定时器。
                    //
                    // 第二路是 `IDLE`：它与 `sleep(wait)` 二选一（见
                    // `wait_for_change`），所以无论走哪条路，都不会比纯轮询更晚。
                    tokio::select! {
                        biased;
                        command = rx.recv() => match command {
                            None | Some(Command::Stop) => break,
                            Some(Command::Sync(next)) => reason = next,
                        },
                        next = self.wait_for_change(wait, timeout_reason) => reason = next,
                    }
                }
            }

            emit(
                &self.sink,
                AccountEvent::Started {
                    account_id: account_id.clone(),
                    reason,
                },
            );

            // ---------- 2. 跑一轮（这里挡住 panic） ----------
            match self.tick().await {
                Ok(summary) => {
                    panics = 0;
                    backoff = self.policy.min_backoff;
                    timeout_reason = WakeReason::Poll;
                    delay = Some(self.policy.interval_for(&self.account));
                    emit(
                        &self.sink,
                        AccountEvent::Synced {
                            account_id: account_id.clone(),
                            reason,
                            summary,
                        },
                    );
                }
                Err(TickFailure::Failed(error)) => {
                    // 认证被拒是**终态**，不是「再等等就好」。
                    //
                    // 重试一个错密码永远不会成功，只会把日志刷满、
                    // 并在 UI 上一直显示「正在重连」——把「要人管」伪装成「会自愈」。
                    // 所以这里直接停掉 actor：用户改完授权码后重新保存，
                    // 会重新 `start` 一个 actor。
                    if matches!(error, AppError::Auth(_)) {
                        emit(
                            &self.sink,
                            AccountEvent::NeedsReauth {
                                account_id: account_id.clone(),
                                message: error.to_string(),
                            },
                        );
                        stopped_for_reauth = true;
                        break;
                    }

                    let retry_in = backoff;
                    backoff = self.policy.grow(backoff);
                    timeout_reason = WakeReason::Retry;
                    delay = Some(retry_in);
                    emit(
                        &self.sink,
                        AccountEvent::Failed {
                            account_id: account_id.clone(),
                            reason,
                            error: error.to_string(),
                            retry_in,
                        },
                    );
                }
                Err(TickFailure::Panicked(message)) => {
                    panics += 1;
                    tracing::error!(
                        account = %self.account.email,
                        %message,
                        panics,
                        "同步任务 panic（已被任务边界拦住）"
                    );
                    if panics >= self.policy.max_consecutive_panics {
                        emit(
                            &self.sink,
                            AccountEvent::Abandoned {
                                account_id: account_id.clone(),
                                message,
                            },
                        );
                        break;
                    }
                    let retry_in = backoff;
                    backoff = self.policy.grow(backoff);
                    timeout_reason = WakeReason::Retry;
                    delay = Some(retry_in);
                    emit(
                        &self.sink,
                        AccountEvent::Panicked {
                            account_id: account_id.clone(),
                            reason,
                            message,
                            retry_in,
                        },
                    );
                }
            }
        }

        if !stopped_for_reauth {
            emit(
                &self.sink,
                AccountEvent::Stopped {
                    account_id: account_id.clone(),
                },
            );
        }
    }
}

/// 投递事件；**回调自己 panic 也不影响同步**。
///
/// UI 层（Tauri `emit`）在窗口已经销毁等情况下可能出错，
/// 那不该让一个账户的同步停摆。
fn emit(sink: &Arc<dyn EventSink>, event: AccountEvent) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| sink.emit(event)));
}

/// 从 panic 载荷里取一段人能看的文字。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "未知 panic 载荷".to_string()
    }
}

/// 多账户调度器。
///
/// 一个账户一个 actor，彼此隔离：谁卡住、谁 panic、谁放弃，都不牵连别人。
///
/// 不实现 `Debug`：里面装的是 trait object（连接器与事件接收方）。
pub struct Scheduler {
    pool: SqlitePool,
    connector: Arc<dyn AccountConnector>,
    sink: Arc<dyn EventSink>,
    policy: RetryPolicy,
    clock: Clock,
    /// 正在跑的 actor。只在**不跨 await** 的临界区里加锁。
    actors: Mutex<HashMap<String, ActorHandle>>,
}

impl Scheduler {
    /// 用连接池与连接器构造（无事件接收方、默认策略、系统时钟）。
    pub fn new(pool: SqlitePool, connector: Arc<dyn AccountConnector>) -> Self {
        Self::with_options(
            pool,
            connector,
            Arc::new(NoopSink),
            RetryPolicy::default(),
            system_clock(),
        )
    }

    /// 全量构造。
    pub fn with_options(
        pool: SqlitePool,
        connector: Arc<dyn AccountConnector>,
        sink: Arc<dyn EventSink>,
        policy: RetryPolicy,
        clock: Clock,
    ) -> Self {
        Self {
            pool,
            connector,
            sink,
            policy,
            clock,
            actors: Mutex::new(HashMap::new()),
        }
    }

    /// 把一个账户交给调度（启动后**立刻**同步一轮）。
    ///
    /// 已经在跑的账户**不重复启动**，只叫醒一轮——
    /// 这样「改完设置再 start 一次」不会留下两个 actor 抢同一个账户。
    pub fn start(&self, account: Account) -> AppResult<()> {
        let account_id = account.id.clone();

        {
            let mut actors = self.lock()?;
            reap_finished(&mut actors);
            if let Some(handle) = actors.get(&account_id) {
                let _ = handle.tx.send(Command::Sync(WakeReason::Manual));
                return Ok(());
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let actor = AccountActor {
            account,
            pool: self.pool.clone(),
            connector: Arc::clone(&self.connector),
            sink: Arc::clone(&self.sink),
            policy: self.policy.clone(),
            clock: Arc::clone(&self.clock),
            idle_supported: None,
        };
        let join = tokio::spawn(actor.run(rx));

        self.lock()?.insert(account_id, ActorHandle { tx, join });
        Ok(())
    }

    /// 叫醒某个账户，立刻同步一轮（用户点「刷新」）。
    ///
    /// 账户没在调度里 → 报错，而不是静默什么都不做。
    pub fn wake(&self, account_id: &str) -> AppResult<()> {
        let mut actors = self.lock()?;
        reap_finished(&mut actors);
        let handle = actors
            .get(account_id)
            .ok_or_else(|| AppError::Invalid(format!("账户 {account_id} 没有在调度中")))?;
        handle
            .tx
            .send(Command::Sync(WakeReason::Manual))
            .map_err(|_| AppError::Invalid(format!("账户 {account_id} 的调度任务已经退出")))
    }

    /// 停掉某个账户（等当前这一轮跑完）。
    pub fn stop(&self, account_id: &str) -> AppResult<()> {
        let mut actors = self.lock()?;
        if let Some(handle) = actors.remove(account_id) {
            let _ = handle.tx.send(Command::Stop);
            // 不 await：调用方通常在同步的 Tauri command 里，
            // 等一轮完整的网络同步（可能几十秒）没有意义。
        }
        Ok(())
    }

    /// 正在调度的账户。
    pub fn running(&self) -> Vec<String> {
        match self.lock() {
            Ok(mut actors) => {
                reap_finished(&mut actors);
                let mut ids: Vec<String> = actors.keys().cloned().collect();
                ids.sort();
                ids
            }
            Err(_) => Vec::new(),
        }
    }

    /// 某个账户是否在调度中。
    ///
    /// **已经退出的 actor 不算在调度中**：认证被拒、连续 panic 放弃之后，
    /// 任务已经结束了，但句柄还留在表里。不清掉的话，
    /// 用户改完授权码再 `start` 只会叫醒一个死 actor，账户从此再不同步。
    pub fn is_running(&self, account_id: &str) -> bool {
        self.lock()
            .map(|mut actors| {
                reap_finished(&mut actors);
                actors.contains_key(account_id)
            })
            .unwrap_or(false)
    }

    /// 停掉全部（应用退出时用）。
    ///
    /// 先发 `Stop` 让各 actor 在下一个检查点自行退出，**再 abort** 掉
    /// 还在等网络的——退出时不该等任何一轮同步跑完。
    pub fn shutdown(&self) {
        let handles: Vec<ActorHandle> = match self.lock() {
            Ok(mut actors) => actors.drain().map(|(_, handle)| handle).collect(),
            Err(_) => return,
        };
        for handle in handles {
            let _ = handle.tx.send(Command::Stop);
            handle.join.abort();
        }
    }

    fn lock(&self) -> AppResult<std::sync::MutexGuard<'_, HashMap<String, ActorHandle>>> {
        self.actors
            .lock()
            .map_err(|_| AppError::Storage("调度器的账户表被有毒锁保护住了".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            poll_interval: Duration::from_secs(300),
            min_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(40),
            max_consecutive_panics: 3,
            command_timeout: Duration::from_secs(5),
        }
    }

    /// 一个最小可用的账户（只有 `poll_interval_secs` 会被这段逻辑读）。
    fn account(id: &str, email: &str) -> Account {
        Account {
            id: id.to_string(),
            email: format!("{email}@example.com"),
            provider: xremail_core::model::ProviderKind::Generic,
            display_name: None,
            color: None,
            auth_kind: xremail_core::model::AuthKind::Password,
            server: xremail_core::model::ServerConfig::new(
                "imap.example.com",
                993,
                "smtp.example.com",
                465,
            ),
            username: email.to_string(),
            secret_ref: None,
            oauth_client_id: None,
            sync_mode: xremail_core::model::SyncMode::Poll,
            poll_interval_secs: 300,
            signature_html: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = policy();
        assert_eq!(p.grow(Duration::from_secs(5)), Duration::from_secs(10));
        assert_eq!(p.grow(Duration::from_secs(10)), Duration::from_secs(20));
        assert_eq!(p.grow(Duration::from_secs(20)), Duration::from_secs(40));
        // 封顶，不会越过 max_backoff。
        assert_eq!(p.grow(Duration::from_secs(40)), Duration::from_secs(40));
        assert_eq!(p.grow(Duration::from_secs(1000)), Duration::from_secs(40));
    }

    #[test]
    fn backoff_never_goes_below_the_floor() {
        let p = policy();
        // 手改过的库可能给出 0；退避不能变成热循环。
        assert_eq!(p.grow(Duration::ZERO), Duration::from_secs(5));
    }

    #[test]
    fn non_positive_poll_interval_falls_back() {
        let p = policy();
        let mut account = account("a", "poll");
        account.poll_interval_secs = 0;
        assert_eq!(p.interval_for(&account), p.poll_interval);

        account.poll_interval_secs = -5;
        assert_eq!(p.interval_for(&account), p.poll_interval);

        account.poll_interval_secs = 60;
        assert_eq!(p.interval_for(&account), Duration::from_secs(60));
    }

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        let payload = std::panic::catch_unwind(|| panic!("静态字符串")).unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "静态字符串");

        let payload = std::panic::catch_unwind(|| {
            let owned = String::from("动态字符串");
            std::panic::panic_any(owned)
        })
        .unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "动态字符串");

        let payload = std::panic::catch_unwind(|| std::panic::panic_any(42u8)).unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "未知 panic 载荷");
    }

    #[test]
    fn wake_reason_names_are_stable() {
        // 这些字符串会出现在日志与前端事件里，改动等于破坏兼容。
        assert_eq!(WakeReason::Startup.as_str(), "startup");
        assert_eq!(WakeReason::Manual.as_str(), "manual");
        assert_eq!(WakeReason::Poll.as_str(), "poll");
        assert_eq!(WakeReason::Retry.as_str(), "retry");
    }
}
