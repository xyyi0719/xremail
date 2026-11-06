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
    /// 一轮同步成功，**并且对账也跑完了**。
    Synced {
        account_id: String,
        reason: WakeReason,
        summary: SyncSummary,
        /// 这一轮同步之后的对账结果。
        ///
        /// 放在 `Synced` 里而不是另发一个事件：两者是同一轮的状态，
        /// 拆成两个事件会让 UI 出现「已同步」但队列里的东西还没被收掉的
        /// 中间态。`None` 表示对账本身失败了（同步照旧成功），
        /// 详细原因在日志里。
        reconcile: Option<crate::reconcile::ReconcileReport>,
    },
    /// 一轮同步失败（可重试）。
    Failed {
        account_id: String,
        reason: WakeReason,
        /// **展示用**的错误文本（会话类错误的内部标记已剥掉）。
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
    /// 「**会话已经死了**」的那类失败（`BYE`、`Io`、超时、IDLE 期间断开）
    /// 之后的首次重连间隔。
    ///
    /// ## 为什么要和 `min_backoff` 分开
    ///
    /// 两者回答的是不同的问题：
    ///
    /// * `min_backoff` 是「**这一轮失败了**，服务器现在可不可用」——服务器在维护、
    ///   我们在被限流、库被锁……这时候慢一点是**善意**的（别去砸人家）；
    /// * 这里是「**连接断了**，服务器本身好好的」——`BYE` 是部署、NAT 超时、
    ///   手机切网、笔记本合盖，服务器下一秒就能接受新连接。
    ///
    /// 用 `min_backoff`（默认 5s，且退避马上翻倍，一轮失败就是 5/10/20/40…）
    /// 去等一个 `BYE`，用户体验就是「合上盖子再打开，邮件十几秒才回来」，
    /// 而且 `poll_interval`（默认 300s）根本救不了它。所以这里给会话类失败
    /// 一条**更短**的序列：1s → 2s → 4s → 8s → 16s → 32s →（之后由
    /// [`Self::max_backoff`] 封顶，默认 300s）。
    ///
    /// ## 为什么不叠加成两套节奏
    ///
    /// 用的**不是**两套计数器：会话类失败只是**换了一个起点**，序列本身
    /// 还是同一条（同一条 [`Self::grow`]，同一个 [`Self::max_backoff`] 封顶）：
    ///
    /// ```text
    /// 会话类失败第 1 次： min_reconnect_backoff                （默认 1s）
    /// 会话类失败第 2 次： grow(min_reconnect_backoff)          （2s）
    /// 会话类失败第 n 次： 连续 grow n-1 次                     （4s、8s……封顶 max_backoff）
    /// 其它失败：          grow(上一个 backoff)，与 B1 之前完全一致（5s、10s……）
    /// 成功：              两个计数都清零
    /// ```
    ///
    /// 「连续掉了几次」由 `run` 里的 `reconnect_count` 记着，落在
    /// [`Self::reconnect_wait`] 里算，成功一轮清零。这样**任何时刻只有一个
    /// 退避值**，谁也不会把另一个的节奏带偏；连着掉时会翻倍、会封顶，
    /// 不会变成热循环。
    ///
    /// 特别是**没有**给短序列单独设一个更小的上限：两个上限叠在一起会让
    /// 长故障里的「断线重连」反而不比普通失败快，甚至更慢（短上限先封顶），
    /// 那就真是两套互相打架的节奏了。
    pub min_reconnect_backoff: Duration,
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
            min_reconnect_backoff: Duration::from_secs(1),
        }
    }
}

impl RetryPolicy {
    /// 退避翻倍，封顶 [`Self::max_backoff`]。
    ///
    /// **没有**下限：下限是「起点」的事（主循环从 [`Self::min_backoff`] 起步，
    /// [`Self::reconnect_wait`] 从 `min_reconnect_backoff` 出发），
    /// `grow` 只管「这次比上次长一倍」。
    /// 这里曾经夹了一个 `.max(min_backoff)`，B1 之后它会把短序列的第二次、
    /// 第三次直接抬到 `min_backoff` 以上，等于短序列只短了一次。
    pub fn grow(&self, current: Duration) -> Duration {
        let doubled = current.saturating_mul(2);
        doubled.min(self.max_backoff)
    }

    /// 会话类失败之后，下一轮等多久。
    ///
    /// **只有下限**，没有独立的上限：短序列的封顶就是普通退避的封顶
    /// （[`Self::max_backoff`]）。不另设一个小得多的上限是有意的——
    /// 两套上限叠在一起会让「会话类失败」在长故障里**比**普通失败还慢
    /// （短上限早就封顶了，而那时普通退避还在往上长），
    /// 那就成了两套互相打架的节奏。所以这里只做一件事：把「起点」压短。
    ///
    /// 下限就是 [`Self::min_reconnect_backoff`]（默认 1s），**不**再夹一个
    /// `min_backoff`：后者是普通失败的起点，夹进来会让短序列一上来就是 5s，
    /// 「短起点」就白设了。末尾的 `1ms` 是给「两个下限都被配成 0」的手滑兜底：
    /// 0 秒的重连就是忙循环。
    ///
    /// **不看当前退避值**（所以没有入参）：这正是「起点」的含义——不管上一轮
    /// 已经长到 12s 还是 40s，掉线之后的这一次都从 1s 重新算。连续失败时的
    /// 翻倍由 [`Self::grow`] 负责，这里再掺一脚就会变成「一次失败翻两倍」。
    pub fn reconnect_delay(&self) -> Duration {
        self.min_reconnect_backoff.max(Duration::from_millis(1))
    }

    /// 连续掉了第 `dropped` 次（0 起）会话之后，下一次重连等多久。
    ///
    /// 就是「从短起点出发，连续 [`Self::grow`] 几脚」：
    /// `dropped = 0` → 1s，`1` → 2s，`2` → 4s……照样封顶 [`Self::max_backoff`]。
    ///
    /// 这样短序列**只有一条**，而且和普通退避共用同一个翻倍与封顶：
    /// 不是两套计数器、两套上限。`dropped` 由主循环记，成功一轮清零，
    /// 所以「掉一次就回来」是常态，「反复掉」才会真的退避。
    ///
    /// 用循环而不是 `1 << n` 之类的移位：`Duration` 的 `saturating_mul` 已经
    /// 管住了溢出，而这里的 `n` 在封顶之后就只等于 `max_backoff` 的 log2，
    /// 迭代次数很小，可读性比位运算重要。
    pub fn reconnect_wait(&self, dropped: u32) -> Duration {
        (0..dropped).fold(self.reconnect_delay(), |wait, _| self.grow(wait))
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

/// 等待期结束的方式。
///
/// 两者都在 `wait_for_change` 的 `catch_unwind` **里面**产生（R42）：命令抵达时
/// 那条 `IDLE` 连接可能还挂着，得先把它放掉，否则下一轮只能排队等它。
#[derive(Debug)]
enum Change {
    /// 到了该同步的时刻（或服务器推来了一封信）。
    Wake(WakeReason),
    /// 等待期里**连接断了**（不是到点，也不是有人推）。
    ///
    /// B1「断线重连」：只可能由 `IDLE` 那条连接在等待期间断掉产生。
    /// 主循环看到它就按短重连退避**立刻**来一轮，而不是把它当「没被叫醒」
    /// 等满整个 `poll_interval`——那等于「开着就不掉线」这个目标的正面失败。
    ///
    /// 为什么不像以前那样顺手把 `idle_supported` 关掉（R37）：那次关掉的
    /// 理由是「`IDLE` 失败几乎是服务器侧的确定性拒绝」。`BYE` / 读帧失败 / 超时
    /// 完全不是那回事——服务器上一秒还好好的，下一秒就断开了，`IDLE` 本身没毛病。
    /// 真把它关了，那么在 `poll_interval` 设成 10 分钟时，用户要**等 10 分钟**
    /// 才可能收到推送；开着则下一次握手就恢复了。
    ///
    /// 防忙循环：这里只**触发一次**短退避重连，退避照样翻倍、封顶 30s，
    /// 服务器连续掐断时节奏会自己慢下来（见 [`RetryPolicy::min_reconnect_backoff`]）。
    Disconnected(WakeReason),
    /// 等的时候来了一条命令。
    Command(Option<Command>),
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
                    .map(|capabilities| {
                        xremail_protocol_imap::has_capability(&capabilities, "IDLE")
                    })
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

    /// 一轮同步收尾之后对一次账（R33）。
    ///
    /// 以前对账只在**应用启动**时跑一次，于是常驻运行期间：
    ///
    /// * `attempts` 涨到 [`crate::reconcile::MAX_OPERATION_ATTEMPTS`] 的操作
    ///   会被 `claim_for_push` 一直拒绝、`transfer_push` 那边只是 `continue`，
    ///   而把它收成 `failed` 的 `expire_exhausted` 只在对账里调——
    ///   要等到**下次重启**才真的标失败，期间 UI 既不移动也不提示；
    /// * 本进程自己卡在 `running` 的行更是没人管（R32）。
    ///
    /// 放在「同步成功之后」而不是「每轮都跑」：对账要读若干张表，
    /// 一轮失败重试的间隔里（5 秒、10 秒…）再跑一遍只是白花 IO。
    /// 同步成功说明库与服务器此刻对得上，正是判断「哪些意图该收掉」的好时机。
    ///
    /// **尽力而为**：对账失败不影响本轮同步的成功，只让那几行多等一轮
    /// （对账是幂等的）。
    async fn reconcile_after_tick(&self) -> Option<crate::reconcile::ReconcileReport> {
        let now_ms = (self.clock)();
        match crate::reconcile::reconcile_account(&self.pool, &self.account.id, now_ms).await {
            Ok(report) => {
                if !report.is_noop() {
                    tracing::info!(
                        account = %self.account.email,
                        reclaimed_operations = report.reclaimed_operations,
                        reclaimed_sends = report.reclaimed_sends,
                        expired = report.expired,
                        quarantined = report.quarantined,
                        failed_orphans = report.failed_orphans,
                        stale_uncertain = report.stale_uncertain,
                        "同步后对账：收掉了一批该有结论的意图"
                    );
                }
                Some(report)
            }
            Err(error) => {
                tracing::warn!(account = %self.account.email, %error, "同步后对账失败");
                None
            }
        }
    }

    /// 等「下一轮该跑了」，**顺便让服务器有机会提前叫醒我们**。
    ///
    /// 返回值与 panic 的处置见 [`Self::wait_for_change`]。
    ///
    /// ## 为什么 `IDLE` 取代的是 `sleep`，而不是额外加一条唤醒通道
    ///
    /// 轮询定时器本来就在等 `wait` 这么久。把这段等待换成「`IDLE` 等服务器说话」
    /// 不改变任何语义：
    /// * 服务器先说话 → `Push`，立刻同步；
    /// * 没人说话 → `IDLE` 到点后把本轮剩下的时间等满，再按 `on_timeout` 同步。
    ///
    /// 两条路最终都以「同步一轮」收尾，所以 **`IDLE` 绝不会让同步变少**，
    /// 只可能让它变早。这一点比「新加一条唤醒通道」好管得多：
    /// 推送丢了也仅仅退化成原来的延迟。
    ///
    /// ## 失败怎么办：按「会话死没死」分两种
    ///
    /// * **连不上**（`connect` 失败）——服务器现在可能是真不可用，重复握手只会
    ///   更糟。所以**不提前返回**，等到 `deadline` 之后按 `on_timeout` 走
    ///   （老行为，未改动）；
    /// * **连上了又断**（`idle_once` 期间 `BYE` / 读帧失败 / 超时）——这不是
    ///   「连不上」，是「掉线了」。B1：立刻返回 [`Change::Disconnected`]，
    ///   由主循环按短重连退避（1s 起、翻倍、封顶 30s）重连，**不等满 `wait`**。
    ///   忙循环的风险由那条退避兜住，不需要靠「空等满一个轮询周期」来兜。
    ///
    /// 两种情况都要让主循环看得见（R28）：
    /// * 正常：本轮该报的 [`WakeReason`]；
    /// * 等待期 panic：**原样上报消息**，由主循环与 `tick` 里的 panic 合并计数。
    ///
    /// 以前这里 panic 只写一行 `error!` 就当「到点」处理，主循环一无所知：
    /// 若 `connect` 里有个必现的 panic，每轮都会撞一次，账户**永远**不会同步，
    /// 而 UI 上显示的还是「正常轮询中」，连 `Abandoned` 都不会出现。
    async fn wait_for_change(
        &mut self,
        wait: Duration,
        on_timeout: WakeReason,
        awake_cmd: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Result<Change, String> {
        // 与 [`Self::tick`] 同一条理由：等待期会 `connect` 并 `idle_once`，同样
        // 全是 await 点。这条分支原先**没有**包边界——它 panic 会直接撞到 tokio
        // 任务边界，任务静悄悄死掉，`reap_finished` 把尸体清掉，该账户从此不再
        // 同步，直到用户重启应用。R13 说「所有 panic 停在这里」，那就得真的
        // 盖住这条分支，而不只是 `tick`。
        let attempt = async {
            // 「等」与「命令」放进**同一个** future，再由**一次** `catch_unwind`
            // 盖住（R42）：以前命令在外层 `select!` 里，等到点就直接丢掉这个
            // future——那条 `IDLE` 连接既没发 `DONE`，也**没被 drop**
            // （`assert_unwind_safe` 拿到的是一整个 `self &mut` 的借用），推后
            // 才在原地被悄悄放弃。下一次等待只能排队等它收尾，每轮多出整整一个
            // `command_timeout`。
            //
            // 现在命令先到，就把 `wait_for_change_inner` 的结果丢掉并让它正常
            // 收尾（`IDLE` 发 `DONE`、连接关闭），不再有「被遗弃的借用」。
            tokio::select! {
                biased;
                command = awake_cmd.recv() => Change::Command(command),
                next = AssertUnwindSafe(self.wait_for_change_inner(wait, on_timeout)) => next,
            }
        };

        let result = AssertUnwindSafe(attempt).catch_unwind().await;

        match result {
            Ok(change) => Ok(change),
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                tracing::error!(
                    account = %self.account.email,
                    %message,
                    "等待期 panic（已被边界拦住）"
                );
                // 不在这里 sleep：交给主循环按 panic 退避，原因见 [`Self::run`]。
                Err(message)
            }
        }
    }

    /// [`Self::wait_for_change`] 的本体，只被那个包了 panic 边界的版本调用。
    async fn wait_for_change_inner(&mut self, wait: Duration, on_timeout: WakeReason) -> Change {
        // 本轮「本该等到」的那个时刻：从这里开始等，就**正好**等满 `wait`（R42）。
        //
        // 这不是「可有可无的保险」，是唯一正确的算法。以前是「`IDLE` 结束后再睡
        // 剩下的差值」，而差值只从 `idle_once` 内部算起——建连 + 登录 +
        // `CAPABILITY` + `EXAMINE` 每一步都可能等满 `command_timeout`，却全被
        // 算在周期之外；等到 `IDLE` 自己超时（同样要一整个 `wait`），一轮就被拉成
        // 「握手 + wait + wait」。
        //
        // 所以 `IDLE` 的超时**不等于**本轮到点：它只是「暂时没推送」，之后还要
        // 接着等到 `deadline`。两者是同一个时间轴上的两个出口，「有人推」是提前
        // 出局的那个。
        let deadline = tokio::time::Instant::now() + wait;

        // 只在一个本该等满 `wait` 的情况下才用 `IDLE`。
        // 不确定 / 已知不支持 → 纯等待，一行代价也没有。
        if self.idle_supported != Some(true) {
            wait_until(deadline).await;
            return Change::Wake(on_timeout);
        }

        let session = match self.connector.connect(&self.account).await {
            Ok(session) => session,
            Err(error) => {
                // **连不上**（不是「连上了又断」）：服务器现在可能真的不可用，
                // 每轮多握手一次只会更糟。所以仍然等满本轮、按 `on_timeout` 走，
                // 由 `tick` 那一轮去报 `Failed` 并走普通退避（B1 只改了
                // 「连上了又断」这一种，见 [`Change::Disconnected`]）。
                tracing::debug!(
                    account = %self.account.email,
                    %error,
                    "IDLE 建连失败，退回纯等待"
                );
                wait_until(deadline).await;
                return Change::Wake(on_timeout);
            }
        };

        // `idle_timeout` 就是本轮要等的时间：这样「有人推」和「到点了」
        // 是同一个时间轴上的两个出口，不需要额外协调。
        //
        // 但要按 `IdleConfig` 自己的下限夹一下（R42）：`IdleConfig::new` 会把
        // 小于下限的 `idle_timeout` 抬到下限，若这里直接传 `wait`，那么 `wait`
        // 比下限还短时 `IDLE` 会比纯等待**更晚**结束，违反本函数的承诺
        // （「只可能让同步变早」）。提前夹好、`IDLE` 结束后再把剩下的时间等满，
        // 两条路径就以同一个时刻为终点。
        let idle_wait = idle_wait_for(wait);
        let config = IdleConfig::new(self.policy.command_timeout, idle_wait, "INBOX");

        let idle_result = idle_once(session, &config).await;
        match idle_result {
            Ok(outcome) => match outcome.event {
                IdleEvent::MailboxChanged => Change::Wake(WakeReason::Push),
                IdleEvent::Timeout => {
                    // `IDLE` 到点只是「这段时间没人推」，**不是**本轮到点：
                    // 还要等满整个周期（R42）。握手已经花掉的时间从这里扣。
                    wait_until(deadline).await;
                    Change::Wake(on_timeout)
                }
                IdleEvent::Unsupported => {
                    // 记下来，以后不再试。
                    tracing::debug!(
                        account = %self.account.email,
                        "服务器未宣告 IDLE，以后只用轮询"
                    );
                    self.idle_supported = Some(false);
                    // 仍然等满 `wait`：本轮本来就该等到这个时刻才同步。
                    wait_until(deadline).await;
                    Change::Wake(on_timeout)
                }
            },
            Err(error) => {
                // 连接在 IDLE 期间坏了。B1（断线重连）：这**不是**「没被叫醒」，
                // 这是「会话已经死了」——立刻交给主循环走短重连退避，而不是等满
                // 这一轮的 `wait`（`poll_interval` 可能是 5 分钟）。
                //
                // 这里判断的是「这次失败说明会话死了吗」，而不是「IDLE 这个功能
                // 还能不能信」：
                // * 会话类（`BYE` / 读帧失败 / 超时 / 半死连接）→
                //   [`Change::Disconnected`]，主循环按 1s→2s→4s… 重连，
                //   **保留** `idle_supported`（服务器下一轮照样能推）；
                // * 语义类（`IDLE` 初始化被 `NO` 拒了）→ 仍是 R37 的处置：
                //   降回纯轮询，本轮等到点，下次 `start` 再探测。
                //
                // 为什么「不提前返回」这条老理由在这里不成立：老注释担心的是
                // 「坏连接会变成忙循环」。忙循环的成因是**零等待**重试，而这里
                // 主循环拿到 `Disconnected` 之后要等 `min_reconnect_backoff`
                // （1s 起、翻倍、封顶 30s）才重连——与失败退避同一条曲线。
                let error = crate::pull::map_protocol_error(error);
                if crate::net::is_session_recoverable(&error) {
                    tracing::debug!(
                        account = %self.account.email,
                        %error,
                        "IDLE 期间连接断了，立刻走短退避重连"
                    );
                    return Change::Disconnected(on_timeout);
                }
                tracing::debug!(
                    account = %self.account.email,
                    %error,
                    "IDLE 失败（不是断线），以后只用轮询，本轮仍然等到点"
                );
                self.idle_supported = Some(false);
                wait_until(deadline).await;
                Change::Wake(on_timeout)
            }
        }
    }

    /// actor 主循环。
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let account_id = self.account.id.clone();
        // 注意：这是「本轮结束 → 下一次**开始**等」的延后量，不是两个周期之间的
        // 完整间隔。真正的周期由 `wait_for_change` 里的 deadline 兜底（R42）。
        let mut delay: Option<Duration> = None;
        let mut reason = WakeReason::Startup;
        // 定时器到点时该报什么原因：正常轮询是 `Poll`，失败后的退避是 `Retry`。
        let mut timeout_reason = WakeReason::Poll;
        // **唯一**的退避基数。会话类失败会把它压回短序列
        // （见 [`RetryPolicy::min_reconnect_backoff`]），成功时一律重置。
        let mut backoff = self.policy.min_backoff;
        // 连续掉了几次会话。会话类失败用它决定短序列走了几脚
        // （1s → 2s → 4s ……），成功一轮或换了别的失败就清零。
        let mut reconnect_count: u32 = 0;
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
                    // 命令与「等到点」的赛跑在 `wait_for_change` 里面（R42）：
                    // 到点时它负责先把 `IDLE` 连接放掉，主循环这边只处理结果。
                    match self.wait_for_change(wait, timeout_reason, &mut rx).await {
                        Ok(Change::Command(command)) => match command {
                            None | Some(Command::Stop) => break,
                            Some(Command::Sync(next)) => reason = next,
                        },
                        Ok(Change::Wake(next)) => reason = next,
                        // 等待期连接断了（B1）：**立刻**来一轮，不等满这一轮的 `wait`。
                        Ok(Change::Disconnected(next)) => {
                            // 等待期断线（B1）：**立刻**来一轮，不等满这一轮的
                            // `wait`；等待时间用短序列（1s → 2s → 4s …），因为
                            // 「刚同步完、服务器好好的，只是 IDLE 那条连接被掐了」
                            // 完全可以下一秒就回来。
                            //
                            // 与 `tick` 失败共用同一个 `reconnect_count`：
                            // 反复掉线才退避，掉一次就回来。
                            backoff = self.policy.reconnect_wait(reconnect_count);
                            reconnect_count = reconnect_count.saturating_add(1);
                            let retry_in = backoff;
                            reason = next;
                            timeout_reason = WakeReason::Retry;
                            delay = Some(retry_in);
                            continue;
                        }
                        // 等待期 panic（R28）：和 `tick` 里的 panic 走**同一套**
                        // 计数与退避。否则「每轮都 panic 的等待」会让账户
                        // 表面正常、实际一轮都同步不了，还永远不会被放弃。
                        Err(message) => {
                            panics += 1;
                            tracing::error!(
                                account = %self.account.email,
                                %message,
                                panics,
                                "同步任务在等待期 panic（已被任务边界拦住）"
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
                            // 退避后再跑一轮：上一轮可能压根没开始，
                            // 立刻重跑就是忙循环。
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
                            continue;
                        }
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
                    reconnect_count = 0;
                    timeout_reason = WakeReason::Poll;
                    delay = Some(self.policy.interval_for(&self.account));
                    // R33：先把该有结论的意图收掉，再报「这一轮已完成」，
                    // 免得 UI 上出现「已同步」但队列里的东西还挂着的中间态。
                    let reconcile = self.reconcile_after_tick().await;
                    emit(
                        &self.sink,
                        AccountEvent::Synced {
                            account_id: account_id.clone(),
                            reason,
                            summary,
                            reconcile,
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

                    let session_recoverable = crate::net::is_session_recoverable(&error);
                    // 结算：**只有**会话类失败走短序列
                    // （[`RetryPolicy::reconnect_wait`]：1s → 2s → 4s …）；
                    // 其它失败（连不上、语义错误）接着当前的 `backoff` 翻倍，
                    // 与 B1 之前一模一样。
                    //
                    // 两条路都只调一次 [`RetryPolicy::grow`]-等价的翻倍，
                    // 而且共用主循环里**同一个** `backoff` 变量：
                    // 任何时刻只有一个退避值，不是两套互相打架的节奏。
                    // 理由见 [`RetryPolicy::min_reconnect_backoff`]。
                    if session_recoverable {
                        backoff = self.policy.reconnect_wait(reconnect_count);
                        reconnect_count = reconnect_count.saturating_add(1);
                    } else {
                        backoff = self.policy.grow(backoff);
                        reconnect_count = 0;
                    }
                    let retry_in = backoff;
                    timeout_reason = WakeReason::Retry;
                    delay = Some(retry_in);
                    emit(
                        &self.sink,
                        AccountEvent::Failed {
                            account_id: account_id.clone(),
                            reason,
                            // 会话类错误带的内部标记在**这里**剥掉：
                            // 用户看到的文案与打标记之前一字不差。
                            error: crate::error::user_facing(error).to_string(),
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

/// 等到 `deadline` 为止；已经过了就立刻返回。
///
/// 本轮该结束的时刻在建连 `IDLE` **之前**就定下了，所以握手花掉的时间、
/// `IDLE` 到点返回的时间都自动从这段等待里扣掉——每轮同步严格按轮询周期
/// 发生，`IDLE` 只可能让它变早（R37/R42）。
async fn wait_until(deadline: tokio::time::Instant) {
    tokio::time::sleep_until(deadline).await;
}

/// 交给 `IDLE` 的等待时长：就是轮询周期本身，只在 `IdleConfig` 的边界上夹一下。
///
/// 下限**绝不能**用 `command_timeout`（R42）：那会把「每秒轮询」变成
/// 「每 30 秒才醒来一次」，与用户设的周期无关。
fn idle_wait_for(wait: Duration) -> Duration {
    wait.clamp(IdleConfig::MIN_IDLE_TIMEOUT, IdleConfig::MAX_IDLE_TIMEOUT)
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
            // 短重连序列：100ms → 200ms → 400ms → 800ms，之后按 `max_backoff`
            // （40s）封顶——短序列**没有**独立的上限，理由见字段文档。
            min_reconnect_backoff: Duration::from_millis(100),
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
        // 手改过的库 / 手滑的配置可能给出 0；退避不能变成热循环。
        // 下限在**起点**：普通失败从 `min_backoff` 起步……
        assert_eq!(p.grow(p.min_backoff), Duration::from_secs(10));
        // ……会话类失败从短起点起步，同样不会降到 0。
        assert_eq!(p.reconnect_wait(0), Duration::from_millis(100));
        let zero = RetryPolicy {
            min_backoff: Duration::ZERO,
            min_reconnect_backoff: Duration::ZERO,
            ..policy()
        };
        // 两个下限都被配成 0 的手滑兜底：`reconnect_delay` 仍然给 1ms，
        // 不会退化成 0 秒重连（忙循环）。
        assert_eq!(zero.reconnect_delay(), Duration::from_millis(1));
        assert_eq!(zero.grow(zero.reconnect_delay()), Duration::from_millis(2));
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
    fn idle_wait_tracks_the_poll_interval_not_the_command_timeout() {
        // R42：以前直接把 `wait` 传给 `IdleConfig::new`，它按自己的下限夹，
        // 小于下限的轮询被静默抬长——`IDLE` 反而比纯轮询更晚结束。
        assert_eq!(
            idle_wait_for(Duration::from_secs(300)),
            Duration::from_secs(300)
        );
        assert_eq!(
            idle_wait_for(Duration::from_millis(300)),
            Duration::from_millis(300)
        );
        // 策略里的 `command_timeout` 不参与：它是单条命令的上限，不是轮询周期。
        assert_ne!(
            idle_wait_for(Duration::from_secs(1)),
            policy().command_timeout
        );
    }

    #[test]
    fn idle_wait_stays_within_the_idle_config_bounds() {
        assert_eq!(idle_wait_for(Duration::ZERO), IdleConfig::MIN_IDLE_TIMEOUT);
        assert_eq!(
            idle_wait_for(Duration::from_secs(86_400)),
            IdleConfig::MAX_IDLE_TIMEOUT
        );
    }

    #[test]
    fn wake_reason_names_are_stable() {
        // 这些字符串会出现在日志与前端事件里，改动等于破坏兼容。
        assert_eq!(WakeReason::Startup.as_str(), "startup");
        assert_eq!(WakeReason::Manual.as_str(), "manual");
        assert_eq!(WakeReason::Poll.as_str(), "poll");
        assert_eq!(WakeReason::Retry.as_str(), "retry");
    }

    /// B1 的正文（一）：会话类失败走**短**序列，而不是 5s 起的普通退避。
    ///
    /// 钉住整条短序列：1s → 2s → 4s……（测试 policy 是 100ms 的同一形状），
    /// 而且**掉一次就回来**：第 0 次掉线等的是 `min_reconnect_backoff` 本身，
    /// 不是它的两倍。
    #[test]
    fn a_dead_session_reconnects_on_the_short_sequence() {
        let p = policy();
        assert_eq!(p.min_reconnect_backoff, Duration::from_millis(100));

        // 短序列：100 → 200 → 400 → 800ms（测试 policy 的 max_backoff 是 40s）。
        assert_eq!(p.reconnect_wait(0), Duration::from_millis(100));
        assert_eq!(p.reconnect_wait(1), Duration::from_millis(200));
        assert_eq!(p.reconnect_wait(2), Duration::from_millis(400));
        assert_eq!(p.reconnect_wait(3), Duration::from_millis(800));
        assert_eq!(p.reconnect_delay(), Duration::from_millis(100));

        // 而普通失败从当前位置继续长，封顶 max_backoff（主循环自己接着 `backoff`）。
        assert_eq!(p.grow(Duration::from_secs(12)), Duration::from_secs(24));
        assert_eq!(p.grow(Duration::from_secs(40)), Duration::from_secs(40));

        // 默认 policy（生产）：会话类第一次 1s，普通失败第一次 10s——
        // 短一档且**只有一档**，不会热循环。见 `RetryPolicy::min_reconnect_backoff`。
        let d = RetryPolicy::default();
        assert_eq!(d.min_reconnect_backoff, Duration::from_secs(1));
        assert_eq!(d.reconnect_wait(0), Duration::from_secs(1));
        assert_eq!(d.reconnect_wait(1), Duration::from_secs(2));
        assert_eq!(d.reconnect_wait(2), Duration::from_secs(4));
        assert_eq!(d.reconnect_wait(5), Duration::from_secs(32));
        assert_eq!(d.grow(d.min_backoff), Duration::from_secs(10));
    }

    /// B1 的正文（二）：非会话类失败**不**被短序列带快。
    ///
    /// 否则「服务器在维护」会被 1 秒一次的握手砸得更惨。
    #[test]
    fn a_server_side_failure_keeps_the_ordinary_backoff() {
        let p = policy();
        // 普通失败：主循环接着当前的 `backoff` 翻倍，5s → 10s → 20s……
        assert_eq!(p.grow(p.min_backoff), Duration::from_secs(10));
        assert_eq!(p.grow(Duration::from_secs(20)), Duration::from_secs(40));
        // 会话类是短的：同一时刻它从 100ms 起，按 200ms / 400ms 翻倍。
        assert_eq!(p.reconnect_wait(0), Duration::from_millis(100));
        assert_eq!(p.reconnect_wait(1), Duration::from_millis(200));
    }

    /// 配置手滑的兜底：短序列不会排进一个 0 秒的重试，也踩不出 `max_backoff`。
    #[test]
    fn the_reconnect_sequence_is_clamped_inside_the_ordinary_backoff() {
        let p = RetryPolicy {
            min_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(40),
            // 手滑：短序列的起点比普通的下限还大。
            min_reconnect_backoff: Duration::from_secs(10),
            ..policy()
        };
        // 用户显式配了 10s，就按 10s——不因为「短序列」这个名义被悄悄改掉；
        // 但也没法越过 `max_backoff`（10s 翻两次就碰到 40s 的天花板）。
        assert_eq!(p.reconnect_delay(), Duration::from_secs(10));
        assert_eq!(p.reconnect_wait(0), Duration::from_secs(10));
        assert_eq!(p.reconnect_wait(2), Duration::from_secs(40));
        // 第 3 次、第 30 次掉线都封在 `max_backoff`，不会越翻越界。
        assert_eq!(p.reconnect_wait(3), Duration::from_secs(40));
        assert_eq!(p.reconnect_wait(30), Duration::from_secs(40));

        let zero = RetryPolicy {
            min_reconnect_backoff: Duration::ZERO,
            ..policy()
        };
        // 0 秒的短起点不能变成「0 秒重连」——那正是热循环。
        assert_eq!(zero.reconnect_delay(), Duration::from_millis(1));
        assert_eq!(zero.reconnect_wait(0), Duration::from_millis(1));
    }

    /// 判据本身接进了重试计时：`BYE`（带标记）走短序列，`Auth` 根本不进循环。
    #[test]
    fn the_classifier_decides_which_sequence_is_used() {
        let p = policy();
        let bye = crate::error::classify_session_error(AppError::Protocol(
            "IDLE 期间服务器主动断开（BYE）".into(),
        ));
        let decided = crate::net::is_session_recoverable(&bye);
        assert!(decided);
        // 断线之后的这一次按短序列算，不管之前长到哪儿。
        assert_eq!(p.reconnect_wait(0), Duration::from_millis(100));
        assert_eq!(p.reconnect_wait(1), Duration::from_millis(200));
        // 反证：普通失败走的是「当前位置接着长」那条路。
        assert_eq!(p.grow(p.min_backoff), Duration::from_secs(10));

        // 语义类 `NO`（不是断线）不该被当成会话死亡。
        let no = crate::pull::map_protocol_error(xremail_protocol_imap::ProtocolError::Rejected(
            "[INUSE] mailbox locked".into(),
        ));
        assert!(!crate::net::is_session_recoverable(&no));

        let auth = AppError::Auth("NO [AUTHENTICATIONFAILED]".into());
        assert!(!crate::net::is_session_recoverable(&auth));

        // 传输层：`Network` 直接算会话可恢复。
        let io = AppError::Network("连接被断开".into());
        assert!(crate::net::is_session_recoverable(&io));
    }

    /// `Failed` 事件里的文案不能带内部标记（那是给判据用的，不是给人看的）。
    #[test]
    fn a_failed_event_carries_display_text() {
        let error = crate::error::classify_session_error(AppError::Protocol("BYE".into()));
        let shown = crate::error::user_facing(error).to_string();
        assert!(!shown.contains(crate::error::SESSION_CLASS_MARKER));
        assert!(shown.contains("BYE"));
    }
}
