//! 假 IMAP 服务器的配置。
//!
//! 配置项刻意做成**行为开关**，而不是「录制回放」：
//! 真实服务商的怪癖（163 拒绝 EXAMINE、要求先发 `ID`……）都能用开关复现，
//! 而录制回放只能复现一种情况。

/// 一封假邮件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageFixture {
    /// UID。**不保证**从 1 开始——163 的 UID 空间就很大。
    ///
    /// 这点很重要：客户端如果把 FETCH 响应里的**序号**（第几封）
    /// 当成 UID 用，在这种数据上就会立刻出错。
    pub uid: u32,
    /// 标记，如 `\Seen`。
    pub flags: Vec<String>,
    /// 完整邮件（RFC 822 文本：头 + 空行 + 正文）。
    pub content: String,
    /// 整封邮件的**原始字节**（覆盖 `content`）。
    ///
    /// 用于测试非 UTF-8 编码：头部是 ASCII，正文是 GBK 字节。
    pub raw: Option<Vec<u8>>,
    /// 修改序号（CONDSTORE）。
    ///
    /// 每次标记变更都会 +1；`UNCHANGEDSINCE` 条件写就是比对这个值。
    pub modseq: u64,
}

impl MessageFixture {
    /// 新建一封未读邮件（带默认的头，正文为空）。
    pub fn new(uid: u32) -> Self {
        Self {
            uid,
            flags: Vec::new(),
            content: default_content(uid, "测试邮件", ""),
            raw: None,
            modseq: 1,
        }
    }

    /// 加上一个标记。
    pub fn with_flag(mut self, flag: &str) -> Self {
        self.flags.push(flag.to_string());
        self
    }

    /// 设置修改序号。
    pub fn with_modseq(mut self, modseq: u64) -> Self {
        self.modseq = modseq;
        self
    }

    /// 替换整封邮件内容。
    pub fn with_content(mut self, content: &str) -> Self {
        self.content = content.to_string();
        self
    }

    /// 设置主题。
    pub fn with_subject(self, subject: &str) -> Self {
        self.with_header("Subject", subject)
    }

    /// 设置发件人。
    pub fn with_from(self, from: &str) -> Self {
        self.with_header("From", from)
    }

    /// 设置收件人。
    pub fn with_to(self, to: &str) -> Self {
        self.with_header("To", to)
    }

    /// 设置日期（`DD Mon YYYY HH:MM:SS +0800` 形式）。
    pub fn with_date(self, date: &str) -> Self {
        self.with_header("Date", date)
    }

    /// 整封邮件用给定的**原始字节**。
    pub fn with_raw(mut self, raw: Vec<u8>) -> Self {
        self.raw = Some(raw);
        self
    }

    /// 设置正文。
    pub fn with_body(self, body: &str) -> Self {
        let separator = head_body_separator(&self.content);
        let (head, _) = split_head_body(&self.content);
        let mut content = head.to_string();
        content.push_str(separator);
        content.push_str(body);
        self.with_content(&content)
    }

    /// 替换一个头字段。
    fn with_header(self, field: &str, value: &str) -> Self {
        let ending = line_ending(&self.content);
        let separator = head_body_separator(&self.content);
        let (head, body) = split_head_body(&self.content);
        let mut lines: Vec<String> = head
            .lines()
            .filter(|line| {
                !line
                    .to_ascii_lowercase()
                    .starts_with(&field.to_ascii_lowercase())
            })
            .map(str::to_string)
            .collect();
        lines.push(format!("{field}: {value}"));
        // 头字段之间用单换行，末尾补一个空行把头与正文隔开。
        let mut content = lines.join(ending);
        content.push_str(separator);
        content.push_str(body);
        self.with_content(&content)
    }

    /// 整封邮件文本。
    pub fn content(&self) -> &str {
        &self.content
    }

    /// 整封邮件的**字节**（有原始字节就用它）。
    pub fn bytes(&self) -> Vec<u8> {
        match &self.raw {
            Some(raw) => raw.clone(),
            None => self.content.as_bytes().to_vec(),
        }
    }

    /// 头部分的**字节**。
    ///
    /// 原始字节模式下，头也按字节切（头一般是 ASCII，但正文不一定是）。
    pub fn header_bytes(&self) -> Vec<u8> {
        match &self.raw {
            Some(raw) => split_head_body_bytes(raw).0,
            None => self.header().as_bytes().to_vec(),
        }
    }

    /// 正文部分的**字节**。
    pub fn body_bytes(&self) -> Vec<u8> {
        match &self.raw {
            Some(raw) => split_head_body_bytes(raw).1,
            None => self.body().as_bytes().to_vec(),
        }
    }

    /// 头部分（到第一个空行为止）。
    pub fn header(&self) -> &str {
        split_head_body(&self.content).0
    }

    /// 正文部分。
    pub fn body(&self) -> &str {
        split_head_body(&self.content).1
    }

    /// 邮件大小（字节）。
    pub fn size(&self) -> u32 {
        self.content.len() as u32
    }

    /// 是否带某个标记（大小写不敏感）。
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }

    /// 某个头字段的所有值。
    pub fn header_values(&self, field: &str) -> Vec<&str> {
        self.header()
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.trim().eq_ignore_ascii_case(field) {
                    Some(value.trim())
                } else {
                    None
                }
            })
            .collect()
    }

    /// 邮件日期（可比较的整数形式）。
    pub fn date(&self) -> Option<i32> {
        let values = self.header_values("Date");
        let value = values.first()?;
        parse_header_date(value)
    }
}

/// 拆出头与正文（以第一个空行为界）。
fn split_head_body(content: &str) -> (&str, &str) {
    let separator = head_body_separator(content);

    match content.find(separator) {
        Some(index) => {
            let head = &content[..index];
            let body = &content[index + separator.len()..];
            (head, body)
        }
        None => (content, ""),
    }
}

/// 行尾（`\r\n` 或 `\n`）。
fn line_ending(content: &str) -> &str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// 头与正文之间的空行（`\r\n\r\n` 或 `\n\n`）。
fn head_body_separator(content: &str) -> &str {
    if content.contains("\r\n\r\n") {
        "\r\n\r\n"
    } else if content.contains("\n\n") {
        "\n\n"
    } else {
        "\r\n\r\n"
    }
}

/// 生成一封默认邮件。
fn default_content(uid: u32, subject: &str, body: &str) -> String {
    // 日期由 UID 派生：同一天里的不同邮件落在不同日期上，
    // 这样 `SINCE` / `BEFORE` 这类日期搜索才有意义。
    let day = 1 + (uid % 28);
    let date = format!("{} Jan 2024 00:00:00 +0800", day);
    let weekday = weekday_for(2024, 1, day);

    format!(
        "From: sender@example.com\r\n\
         To: tester@example.com\r\n\
         Subject: {subject}\r\n\
         Date: {weekday}, {date}\r\n\
         Message-ID: <fixture-{uid}@example.com>\r\n\
         \r\n\
         {body}"
    )
}

/// 算星期几（Sakamoto 算法）。
fn weekday_for(year: i64, month: u32, day: u32) -> &'static str {
    const TABLE: [i64; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    let y = if month < 3 { year - 1 } else { year };
    let index = (y + y / 4 - y / 100 + y / 400 + TABLE[(month - 1) as usize] + day as i64) % 7;
    NAMES[index.rem_euclid(7) as usize]
}

/// 解析 RFC 2822 日期里的日期部分（`Mon, 01 Jan 2024 00:00:00 +0800`）。
fn parse_header_date(value: &str) -> Option<i32> {
    let without_day_name = match value.find(',') {
        Some(index) => value[index + 1..].trim(),
        None => value.trim(),
    };
    let mut parts = without_day_name.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: i32 = parts.next()?.parse().ok()?;
    Some(year * 10_000 + month * 100 + day as i32)
}

/// 月份名 → 月份号。
fn month_number(name: &str) -> Option<i32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| month.eq_ignore_ascii_case(name))
        .map(|index| index as i32 + 1)
}

/// 一个假文件夹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxFixture {
    /// 文件夹名（明文）。
    ///
    /// 传输时会按 IMAP 规则编码成 **modified UTF-7**（RFC 3501 §5.1.3），
    /// 中文名在线上是 `&XfJT0ZAB-` 这种形态。
    pub name: String,
    /// 特殊用途属性，如 `\Sent`。
    pub attributes: Vec<String>,
    /// 邮件。
    pub messages: Vec<MessageFixture>,
}

impl MailboxFixture {
    /// 新建一个空文件夹。
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            attributes: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// 加上特殊用途属性。
    pub fn with_attribute(mut self, attribute: &str) -> Self {
        self.attributes.push(attribute.to_string());
        self
    }

    /// 放一封邮件。
    pub fn with_message(mut self, message: MessageFixture) -> Self {
        self.messages.push(message);
        self
    }

    /// 最大的 UID（`UID` 搜索与 sequence set 里的 `*` 用它）。
    pub fn max_uid(&self) -> u32 {
        self.messages.iter().map(|m| m.uid).max().unwrap_or(0)
    }
}

/// 故障注入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// 登录成功后立刻断开连接。
    DropAfterLogin,
    /// 收到某条命令后**不响应**（客户端应当超时）。
    ///
    /// 命令名大写匹配，如 `"SELECT"`。
    HangOn(String),
    /// 对某条命令回 `NO`。
    Reject(String),
    /// 认证直接失败（模拟授权码错误）。
    RejectLogin,
    /// 认证成功但**不**回 `CAPABILITY`（模拟 163 认证前拒绝一切命令）。
    NoCapabilityResponse,
    /// 收到 `STORE` 后**回 OK 但不真的改**（“听话的假服务器”）。
    ///
    /// 模拟「服务器收了命令却没执行」——客户端只有**回读确认**
    /// 才能发现（见 ARCHITECTURE §6）。
    ///
    /// ## 为什么没有 `IgnoreAppend` / `IgnoreExpunge`
    ///
    /// 这两个对应形态**暂时不加**：产品侧还没有 `APPEND` / `EXPUNGE`
    /// 的代码路径（`xremail-sync` 目前只实现了 flag 回写，见
    /// ARCHITECTURE §5 的实现落点），加了就是没人用的死代码。
    /// 等真支持了再补——那时它们才有意义。
    IgnoreStore,
    /// 收到某条命令后**直接断开连接**（不回任何响应）。
    ///
    /// 模拟「命令发出去了，但没收到服务器的确认」——
    /// 客户端**不知道**服务器到底执行了没（见 ARCHITECTURE §5）。
    DropOn(String),
    /// 收到某条命令后**延迟 N 毫秒**才回。
    ///
    /// 模拟「服务器还在处理 / 网络很慢」——用来逼出客户端的
    /// 超时与重试逻辑（见 ARCHITECTURE §4.2 对超时的要求）。
    ///
    /// 命令名大写匹配，如 `"SELECT"`；`UID FETCH` 这类要写子命令名
    /// `"FETCH"`（与 [`Fault::DropOn`] 的匹配规则一致）。
    SlowResponse {
        /// 命令名。
        command: String,
        /// 延迟多少毫秒才回。
        delay_ms: u64,
    },
    /// FETCH 返回的**整封正文**被**截断**（只发前 `bytes` 个字节）。
    ///
    /// 模拟「收到一半就断了」：客户端拿到的字面量长度对不上，
    /// 必须当作失败处理，**不能**把半截正文写进库
    /// （见 ARCHITECTURE §4.2 的同步事务）。
    ///
    /// 只截断**整封正文**（`BODY[]`）；头很小，截断它只会挡住
    /// 「先同步元数据、再按需拉正文」这条正常链路。
    ///
    /// `once = true` 时只截断第一次，之后恢复正常——用来测「失败后重试」。
    TruncateFetch {
        /// 只发前多少个字节。
        bytes: usize,
        /// 是否只截断第一次。
        once: bool,
    },
    /// 登录成功后，收到匹配的第一条命令就**断开连接**（不回响应）。
    ///
    /// 模拟「认证后才失效」：token 过期、服务商在认证通过后
    /// 立刻把会话掐掉。比 [`Fault::DropAfterLogin`] 更贴近现实——
    /// 它掐在**业务命令**上，而不是紧跟在 `LOGIN` 后面。
    DropAfterLoginOn {
        /// 命令名。
        command: String,
    },
    /// 只在**第一次**遇到某条命令时断开连接（之后恢复正常）。
    ///
    /// 模拟「抖了一下」的网络故障：用来测「断线后重连能不能续上」
    /// （见 ARCHITECTURE §5 的恢复要求）。
    DropOnOnce {
        /// 命令名。
        command: String,
    },
    /// 只在**第一次** `STORE` 时「回 OK 但不真的改」（之后恢复正常）。
    ///
    /// 模拟「服务器收了命令却没执行」但**只发生了一次**：
    /// 用来测「回读发现没生效 → 重新入队 → 重放成功」
    /// （见 ARCHITECTURE §6）。
    IgnoreStoreOnce,
    /// 收到某条命令后**执行**它，但**不回响应**（连接被掐断）。
    ///
    /// 模拟「服务器执行了，确认丢了」——这是 ARCHITECTURE §5
    /// 「命令发出去了但没收到确认」的**服务器侧孪生**：
    /// 效果已经在服务器上了，只是确认没送回来，
    /// 客户端**不知道**服务器执行了没，只能靠**回读**收敛
    /// （见 ARCHITECTURE §5 / §6）。
    ///
    /// 与 [`Fault::DropOn`] 的区别：`DropOn` 是**执行前**断线
    /// （命令根本没生效），这个是**执行后**断线。
    /// 只有后者能验证「重放不产生重复副作用」——
    /// 客户端必须先回读，确认意图已生效，才不再发第二条 `STORE`。
    DropAfterApply {
        /// 命令名。
        command: String,
        /// 是否只发生一次（之后恢复正常）。
        once: bool,
    },
    /// 收到**命令原文里包含某段文本**的命令后直接断开连接。
    ///
    /// 模拟「同步进行到一半，连接断了」——而且能精确命中**某一条**命令：
    /// `UID FETCH 1:* (UID)`（取远端 UID 集合）与
    /// `UID FETCH <uid> (UID FLAGS ... BODY.PEEK[HEADER])`（批量拉元数据）
    /// **都叫 `FETCH`**，按命令名匹配分不开。
    ///
    /// 文本比对**大小写不敏感**。`once = true` 时只断第一次
    /// （之后恢复正常），用来测「重连后能不能续上」
    /// （见 ARCHITECTURE §5 的恢复要求）。
    DropOnContains {
        /// 命令原文里要包含的片段。
        text: String,
        /// 是否只断第一次。
        once: bool,
    },
    /// 某条命令的**正常响应之间**（untagged 数据之后、tagged 完成响应之前）
    /// 插入一条**别的** untagged 响应；发完继续正常处理这条命令。
    ///
    /// 模拟什么：Coremail 体系（163 / QQ）在处理一条命令的过程中，会**顺带**
    /// 把另一个变更通知插进来——`* 5 EXPUNGE`、`* 3 FETCH (FLAGS (\Seen))`——
    /// 这样客户端收到的顺序就是「我的响应 → 别人的通知 → 我的完成响应」，
    /// 而不是「我的响应 → 我的完成响应」。
    ///
    /// 为什么需要它：客户端的读循环若用「每读到一条 untagged 就攒起来」的写法，
    /// 攒下的东西会**混进下一条命令的结果**。只有把噪音塞在**别人的**命令
    /// 还没发出去之前，才能证明命令层是**按 tag 自证成败**的
    /// （见 ARCHITECTURE §2.1），而不是「收到的都是我的」。
    ///
    /// 和相邻变体的区别：[`Fault::SlowResponse`] 是「晚回」，
    /// [`Fault::DropOn`] 是「不回」，本条是**顺序**问题——响应一条不少、
    /// 值也都对，只是**插了队**。所以验证的重点在「读到的内容对不对」，
    /// 而不是「成没成功」。
    ///
    /// 匹配规则与 [`Fault::SlowResponse`] 一致：命令名大写，`UID FETCH` 写子命令名
    /// `"FETCH"`。
    ///
    /// 插入的响应**不改变服务器状态**：它只是线路上的一条「假」通知，
    /// 服务器自己并不认为有过一次删除或一次标记变更——否则用例验到的就是
    /// 「服务器真的变了」，而不是「客户端能吞掉噪音」。
    /// 具体插什么由 [`InterleavedUntagged`] 决定。
    InterleaveUntagged {
        /// 命令名。
        command: String,
        /// 插什么。
        noise: InterleavedUntagged,
        /// 是否只插一次（之后恢复正常）。
        once: bool,
    },
}

/// [`Fault::InterleaveUntagged`] 插进去的那条 untagged 响应是什么。
///
/// 所有形态都必须是**合法且无害**的：客户端把它当噪音丢掉时，
/// **不能**因此误判「邮件被删了」「状态变了」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterleavedUntagged {
    /// `* <n> EXISTS`，n 取**当前文件夹的真实邮件数**。
    ///
    /// 为什么选它当默认：`EXISTS` 是「到这一刻为止共有几封」的**快照**，
    /// 报当前值等于报了个空操作——客户端即便照着它更新计数，也只会得到
    /// 本来就知道的数。它**不引用任何具体邮件**（没有序号、没有 UID），
    /// 所以既不会让客户端以为某封邮件没了，也不会让它去拉一封不存在的邮件。
    ///
    /// 对比：`EXPUNGE` 会告诉客户端「序号 1 没了」，客户端若当真会把本地
    /// 邮件删掉，还会让其后所有序号左移——那是改状态，不是噪音；
    /// `FETCH` 只对「碰巧在等 `FETCH`」的命令是噪音，对 `STORE` / `SEARCH`
    /// 这种用别的形态。
    ///
    /// 服务器必须处于**已选中文件夹**的状态才会插；未选中时
    /// （RFC 3501 下 `EXISTS` 只对选中的邮箱有意义）改插 [`Self::Bye`]。
    Exists,
    /// `* 3 FETCH (FLAGS (\Seen))`。
    ///
    /// 模拟「别的客户端刚把第 3 封标成已读」。用**固定的序号 3 + 固定标记**
    /// 而不是真实邮件：它不携带 UID，也不携带任何内容，
    /// 即便客户端把它当成本次 `FETCH` 的结果收下，也只会得到一条
    /// 「第 3 封带 `\Seen`」的空记录——用例一断言 UID / 正文就会露馅。
    /// 文件夹里不足 3 封时退回 [`Self::Exists`]：序号越界的 `FETCH`
    /// 是非法响应，不能拿来当噪音。
    FetchFlags,
    /// `* OK [ALERT] …`。
    ///
    /// 服务器主动发的告警，RFC 3501 允许它出现在任何时刻。
    /// 用来验证客户端**不会**把一条「像完成响应」的 `OK` 当成自己那条的结局
    /// ——它没有 tag。
    OkAlert,
    /// `* BYE …`。
    ///
    /// 模拟「服务器宣布要关机」。它**不会**真的断开连接，所以对继续跑命令的
    /// 客户端只是噪音；但它是 untagged 响应里最「吓人」的一种
    /// （RFC 3501 说收到 `BYE` 就该准备断线），拿它验证客户端不会因此
    /// 把会话判死。
    Bye,
}

impl InterleavedUntagged {
    /// 在线路上要写的那一行。
    ///
    /// `exists` 是插这条响应时文件夹里的**真实**邮件数，`messages` 是同一时刻的
    /// 邮件数——`FetchFlags` 靠它判断序号 3 是否合法。
    pub fn line(self, exists: u32) -> String {
        match self {
            // `EXISTS` 只在选中邮箱时有意义；没选中的场景由调用方退回 `Bye`。
            InterleavedUntagged::Exists => format!("* {exists} EXISTS"),
            // 序号 3 越界时退回 `EXISTS`：非法响应会让客户端解析失败，
            // 那测的就不是「能吞噪音」，而是「能扛住非法输入」了。
            InterleavedUntagged::FetchFlags if exists < 3 => format!("* {exists} EXISTS"),
            InterleavedUntagged::FetchFlags => "* 3 FETCH (FLAGS (\\Seen))".to_string(),
            InterleavedUntagged::OkAlert => {
                "* OK [ALERT] Fake IMAP: interleaved untagged response".to_string()
            }
            InterleavedUntagged::Bye => "* BYE Fake IMAP says goodbye (but stays)".to_string(),
        }
    }
}

/// 假服务器的配置。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 宣告的能力集。
    ///
    /// 模拟不同服务商的能力差异就改这里。
    ///
    /// 含 `CONDSTORE` 时才会启用 modseq / `UNCHANGEDSINCE` 语义。
    pub capabilities: Vec<String>,
    /// 允许登录的用户名。
    pub user: String,
    /// 允许登录的口令。
    pub password: String,
    /// 是否**要求**先发 `ID` 才能访问邮箱。
    ///
    /// 163 就是这样：不发 `ID` 时 `EXAMINE` / `SELECT` 一律被回
    /// `NO … Unsafe Login`。
    pub require_id: bool,
    /// 是否拒绝 `EXAMINE`（只允许 `SELECT`）。
    pub reject_examine: bool,
    /// UIDVALIDITY（epoch）。
    pub uid_validity: u32,
    /// 文件夹的**初始**内容。
    ///
    /// 服务器启动后会被复制成「活的状态」；`STORE` 改的是活的状态，
    /// 不会回写这里。
    pub mailboxes: Vec<MailboxFixture>,
    /// 故障注入。
    pub faults: Vec<Fault>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            capabilities: vec![
                "IMAP4rev1".to_string(),
                "ID".to_string(),
                "UIDPLUS".to_string(),
                "SPECIAL-USE".to_string(),
                "LITERAL+".to_string(),
            ],
            user: "tester@example.com".to_string(),
            password: "test-password".to_string(),
            require_id: false,
            reject_examine: false,
            uid_validity: 1,
            mailboxes: Vec::new(),
            faults: Vec::new(),
        }
    }
}

impl ServerConfig {
    /// 一个带默认文件夹的配置。
    pub fn with_mailboxes() -> Self {
        Self {
            mailboxes: vec![
                MailboxFixture::new("INBOX"),
                MailboxFixture::new("草稿箱").with_attribute("\\Drafts"),
                MailboxFixture::new("已发送").with_attribute("\\Sent"),
                MailboxFixture::new("已删除").with_attribute("\\Trash"),
                MailboxFixture::new("垃圾邮件").with_attribute("\\Junk"),
            ],
            ..Default::default()
        }
    }

    /// 是否宣告了某个能力。
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case(capability))
    }

    /// 是否触发某个故障。
    pub fn has_fault(&self, fault: &Fault) -> bool {
        self.faults.contains(fault)
    }

    /// 找出发送给某条命令的故障。
    pub fn fault_for(&self, command: &str) -> Option<&Fault> {
        let upper = command.to_ascii_uppercase();
        self.faults.iter().find(|fault| match fault {
            Fault::HangOn(cmd) | Fault::Reject(cmd) | Fault::DropOn(cmd) => {
                cmd.eq_ignore_ascii_case(&upper)
            }
            Fault::SlowResponse { command, .. }
            | Fault::DropAfterLoginOn { command }
            | Fault::DropOnOnce { command }
            | Fault::DropAfterApply { command, .. }
            | Fault::InterleaveUntagged { command, .. } => command.eq_ignore_ascii_case(&upper),
            _ => false,
        })
    }

    /// 找出**符合条件**的第一条故障。
    ///
    /// 与 [`Self::fault_for`] 的区别：后者只认「第一条命令名相同的故障」，
    /// 而**不同种类**的故障可能同时挂在同一条命令上（例如既慢又断）。
    /// 这时要按种类分别取，用本方法。
    pub fn find_fault(&self, predicate: impl Fn(&Fault) -> bool) -> Option<&Fault> {
        self.faults.iter().find(|fault| predicate(fault))
    }
}

/// 把原始字节拆成 `(头, 正文)`（以第一个空行为界）。
fn split_head_body_bytes(raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let separator = if raw.windows(4).any(|w| w == b"\r\n\r\n") {
        4
    } else if raw.windows(2).any(|w| w == b"\n\n") {
        2
    } else {
        return (raw.to_vec(), Vec::new());
    };

    for index in 0..raw.len() {
        if raw[index..].starts_with(b"\r\n\r\n") {
            return (raw[..index].to_vec(), raw[index + separator..].to_vec());
        }
        if raw[index..].starts_with(b"\n\n") {
            return (raw[..index].to_vec(), raw[index + separator..].to_vec());
        }
    }

    (raw.to_vec(), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_content_has_header_and_body() {
        let message = MessageFixture::new(42).with_body("正文内容");

        assert!(message.header().contains("Subject: 测试邮件"));
        assert!(
            message
                .header()
                .contains("Message-ID: <fixture-42@example.com>")
        );
        assert_eq!(message.body(), "正文内容");
        assert!(message.size() > 0);
    }

    #[test]
    fn with_subject_replaces_the_header() {
        let message = MessageFixture::new(1).with_subject("新主题");

        assert_eq!(message.header_values("Subject"), vec!["新主题"]);
        assert_eq!(
            message.header_values("Subject").len(),
            1,
            "不该出现重复的头"
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let message = MessageFixture::new(1).with_from("someone@example.com");

        assert_eq!(message.header_values("from"), vec!["someone@example.com"]);
        assert_eq!(message.header_values("FROM"), vec!["someone@example.com"]);
    }

    #[test]
    fn flags_are_case_insensitive() {
        let message = MessageFixture::new(1).with_flag("\\Seen");

        assert!(message.has_flag("\\seen"));
        assert!(message.has_flag("\\SEEN"));
        assert!(!message.has_flag("\\Flagged"));
    }

    #[test]
    fn dates_are_derived_from_uid() {
        let first = MessageFixture::new(1);
        let second = MessageFixture::new(2);

        assert_eq!(first.date(), Some(20240102));
        assert_eq!(second.date(), Some(20240103));
    }

    #[test]
    fn weekday_is_correct() {
        // 2024-01-01 是星期一。
        assert_eq!(weekday_for(2024, 1, 1), "Mon");
        assert_eq!(weekday_for(2024, 1, 7), "Sun");
        assert_eq!(weekday_for(2024, 2, 29), "Thu");
    }

    #[test]
    fn mailbox_reports_max_uid() {
        let mailbox = MailboxFixture::new("INBOX")
            .with_message(MessageFixture::new(10))
            .with_message(MessageFixture::new(99));

        assert_eq!(mailbox.max_uid(), 99);
        assert_eq!(MailboxFixture::new("空").max_uid(), 0);
    }

    #[test]
    fn capability_lookup_is_case_insensitive() {
        let config = ServerConfig::default();

        assert!(config.has_capability("imap4rev1"));
        assert!(!config.has_capability("CONDSTORE"));
    }

    #[test]
    fn fault_lookup_matches_command_names() {
        let mut config = ServerConfig::default();
        config.faults.push(Fault::SlowResponse {
            command: "FETCH".to_string(),
            delay_ms: 500,
        });
        config.faults.push(Fault::DropOnOnce {
            command: "STORE".to_string(),
        });

        assert!(matches!(
            config.fault_for("FETCH"),
            Some(Fault::SlowResponse { delay_ms: 500, .. })
        ));
        assert!(matches!(
            config.fault_for("fetch"),
            Some(Fault::SlowResponse { .. })
        ));
        assert!(matches!(
            config.fault_for("STORE"),
            Some(Fault::DropOnOnce { .. })
        ));
        assert!(config.fault_for("SEARCH").is_none());
    }

    #[test]
    fn drop_on_contains_is_not_matched_by_name() {
        // `DropOnContains` 按**命令原文**匹配，不能被 `fault_for` 当成命令名命中。
        let mut config = ServerConfig::default();
        config.faults.push(Fault::DropOnContains {
            text: "BODY.PEEK[HEADER]".to_string(),
            once: true,
        });

        assert!(config.fault_for("FETCH").is_none());
        assert!(
            config
                .find_fault(|f| matches!(f, Fault::DropOnContains { .. }))
                .is_some()
        );
    }

    #[test]
    fn interleave_fault_is_matched_by_command_name() {
        let mut config = ServerConfig::default();
        config.faults.push(Fault::InterleaveUntagged {
            command: "FETCH".to_string(),
            noise: InterleavedUntagged::Exists,
            once: true,
        });

        // `UID FETCH` 的子命令名就是 `FETCH`：匹配规则要与 `SlowResponse` 一致。
        assert!(matches!(
            config.fault_for("fetch"),
            Some(Fault::InterleaveUntagged {
                noise: InterleavedUntagged::Exists,
                ..
            })
        ));
        assert!(config.fault_for("STORE").is_none());
    }

    /// 选择噪音形态的约束：**合法**且**不改变任何状态**。
    #[test]
    fn interleaved_untagged_lines_are_legal_and_harmless() {
        // 默认形态报的是当前真实邮件数：等于一个空操作。
        assert_eq!(InterleavedUntagged::Exists.line(7), "* 7 EXISTS");
        // 空文件夹也合法。
        assert_eq!(InterleavedUntagged::Exists.line(0), "* 0 EXISTS");

        // 序号 3 存在时才发 `FETCH`。
        assert_eq!(
            InterleavedUntagged::FetchFlags.line(5),
            "* 3 FETCH (FLAGS (\\Seen))"
        );
        // 不足 3 封时不能发越界的 `FETCH`（非法响应），退回 `EXISTS`。
        assert_eq!(InterleavedUntagged::FetchFlags.line(2), "* 2 EXISTS");
        assert_eq!(InterleavedUntagged::FetchFlags.line(0), "* 0 EXISTS");

        // 这两条是 untagged 的 `OK` / `BYE`：没有 tag，不能被当成任何命令的完成响应。
        assert!(InterleavedUntagged::OkAlert.line(1).starts_with("* OK "));
        assert!(InterleavedUntagged::Bye.line(1).starts_with("* BYE "));

        // 任何形态都**不**是 tagged 响应。
        for noise in [
            InterleavedUntagged::Exists,
            InterleavedUntagged::FetchFlags,
            InterleavedUntagged::OkAlert,
            InterleavedUntagged::Bye,
        ] {
            assert!(noise.line(5).starts_with("* "), "{noise:?} 必须是 untagged");
        }
    }
}
