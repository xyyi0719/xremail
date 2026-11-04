//! 安全命令层：服务器回 `NO` / `BAD` 时**必须**报错。
//!
//! ## 为什么需要它
//!
//! async-imap 0.12 的 `uid_fetch` / `uid_store` / `uid_search` 会**吞掉**
//! 带 tag 的 `NO` / `BAD` 响应。
//!
//! 原因在库的响应解析里：`parse_fetches` / `parse_ids` 用
//! `take_while(filter(...))` 读响应，而 `filter` 遇到**自己那个 tag**的
//! `Done` 响应时返回 `false`——于是 `take_while` 直接停住，
//! 那个携带 `NO` 的响应**从未被解析成错误**，就被丢掉了。
//!
//! 后果很严重：
//!
//! | 命令 | 服务器拒绝时 | 实际表现 |
//! |---|---|---|
//! | `UID STORE` | `NO` | 看起来像「成功，但没有变更」 |
//! | `UID SEARCH` | `NO` | 看起来像「没有匹配」 |
//! | `UID FETCH`  | `NO` | 看起来像「这个范围没有邮件」 |
//!
//! 同步引擎若把「服务器拒绝了」当成「成功」，本地状态就会与远端悄悄分叉。
//! 这正是 [ARCHITECTURE §6](../../../docs/ARCHITECTURE.md) 要求
//! 「读快照 → 重放意图 → **回读确认**」的原因之一。
//!
//! 本模块自己跑命令、自己读响应、自己判定 `OK` / `NO` / `BAD`，
//! 不依赖 async-imap 那条会吞错误的路径。

use imap_proto::{AttributeValue, MessageSection, Response, SectionPath, Status};

use std::time::Duration;

use crate::connection::Session;
use crate::error::ProtocolError;

/// 单条 IMAP 命令的默认超时。
///
/// async-imap **没有**命令超时（见 [`crate::command`] 与 ARCHITECTURE §5），
/// 所以每一条命令都必须由调用方套一层超时；这里是「没指定时」的兜底值，
/// 与 [`crate::connection::ImapConfig::imaps`] 的默认值一致。
///
/// 真正生效的超时应当来自调用方（例如同步引擎的
/// `SyncPolicy::command_timeout`），本常量只做默认值。
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// 把 async-imap 的错误分类成 [`ProtocolError`]。
///
/// 关键区分：**“密码不对”与“网络断了”不是一回事**
/// （详见 `connection` 模块里的同名函数）。
fn classify_imap_error(error: async_imap::error::Error) -> ProtocolError {
    match error {
        async_imap::error::Error::No(detail) => ProtocolError::Auth(detail),
        other => crate::connection::classify_imap_error(other),
    }
}

/// 一条 FETCH 响应（已解析）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedMessage {
    /// 序号（这封是文件夹里的第几封）。
    ///
    /// **不是 UID**——两者常常差得很远（163 的 UID 空间就很大）。
    pub seq: u32,
    /// UID。
    pub uid: Option<u32>,
    /// 标记。
    pub flags: Vec<String>,
    /// 字节数。
    pub size: Option<u32>,
    /// 修改序号（CONDSTORE）。
    pub modseq: Option<u64>,
    /// 头。
    pub header: Option<Vec<u8>>,
    /// 正文。
    pub body: Option<Vec<u8>>,
    /// 纯文本正文。
    pub text: Option<Vec<u8>>,
}

impl FetchedMessage {
    /// 是否带某个标记（大小写不敏感）。
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f.eq_ignore_ascii_case(flag))
    }
}

/// 一条命令的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CommandOutcome {
    /// 没有数据（例如 `STORE` 只改标记）。
    #[default]
    Empty,
    /// `FETCH` 的结果。
    Fetches(Vec<FetchedMessage>),
    /// `SEARCH` 的结果（UID 或序号，取决于命令）。
    Search(Vec<u32>),
    /// `LIST` 的结果。
    List(Vec<ListedMailbox>),
}

/// `LIST` 返回的一个文件夹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedMailbox {
    /// 文件夹名（**未解码**——线路上是 modified UTF-7）。
    pub raw_name: String,
    /// 层级分隔符。
    pub delimiter: Option<String>,
    /// RFC 6154 特殊用途属性（`\Sent` / `\Drafts` / …）。
    pub attributes: Vec<String>,
    /// 由属性或名字识别出的特殊用途角色。
    pub role: Option<String>,
}

impl CommandOutcome {
    /// FETCH 的结果（不是 FETCH 就返回空切片）。
    pub fn fetches(&self) -> &[FetchedMessage] {
        match self {
            CommandOutcome::Fetches(messages) => messages,
            _ => &[],
        }
    }

    /// SEARCH 的结果。
    pub fn search(&self) -> &[u32] {
        match self {
            CommandOutcome::Search(ids) => ids,
            _ => &[],
        }
    }

    /// LIST 的结果。
    pub fn list(&self) -> &[ListedMailbox] {
        match self {
            CommandOutcome::List(mailboxes) => mailboxes,
            _ => &[],
        }
    }
}

/// 打开一个文件夹（只读）后的概况。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MailboxInfo {
    /// 邮件总数。
    pub exists: u32,
    /// 带 `\Recent` 的邮件数。
    pub recent: u32,
    /// 未读数。
    ///
    /// `None` 表示服务器没报——163 的 `EXAMINE` 就不报这个。
    pub unseen: Option<u32>,
    /// **epoch**（UIDVALIDITY）。
    ///
    /// `None` 表示服务器没给——**不能**拿它当 0 用：
    /// 没有 epoch 就无法定位邮件（见 ARCHITECTURE §4.3）。
    pub uid_validity: Option<u32>,
    /// 下一个 UID。
    pub uid_next: Option<u32>,
    /// 最高 MODSEQ（CONDSTORE）。
    pub highest_modseq: Option<u64>,
}

/// 安全地打开一个文件夹（只读，`EXAMINE`）。
///
/// 与 [`run_checked`] 一样，服务器回 `NO` / `BAD` 时**一定**返回错误。
///
/// 为什么单独写一个：async-imap 的 `examine` 走的是 `parse_status`，
/// 它**会**把 `NO` / `BAD` 变成错误（不像 `uid_fetch` 那样吞掉），
/// 但错误类型是 `async_imap::Error`——分类工作交给本层做。
///
/// 超时用默认值（[`DEFAULT_COMMAND_TIMEOUT`]）；能拿到具体超时的调用方
/// 用 [`examine_checked_with_timeout`]。
pub async fn examine_checked(
    session: &mut Session,
    mailbox: &str,
) -> Result<MailboxInfo, ProtocolError> {
    examine_checked_with_timeout(session, mailbox, DEFAULT_COMMAND_TIMEOUT).await
}

/// 同 [`examine_checked`]，但用**调用方给的**超时。
///
/// async-imap 的命令**没有**超时：服务器接受了连接却不回话时，
/// `EXAMINE` 会一直挂着。同步引擎必须传自己的
/// `command_timeout` 进来，否则超时配置形同虚设。
pub async fn examine_checked_with_timeout(
    session: &mut Session,
    mailbox: &str,
    timeout: Duration,
) -> Result<MailboxInfo, ProtocolError> {
    match tokio::time::timeout(timeout, examine_inner(session, mailbox)).await {
        Ok(result) => result,
        Err(_) => Err(ProtocolError::Timeout(format!(
            "命令超时（{timeout:?}）: EXAMINE {mailbox}"
        ))),
    }
}

/// [`examine_checked`] 的实际调用（不带超时）。
async fn examine_inner(session: &mut Session, mailbox: &str) -> Result<MailboxInfo, ProtocolError> {
    let result = session.examine(mailbox).await;

    match result {
        Ok(mailbox) => Ok(MailboxInfo {
            exists: mailbox.exists,
            recent: mailbox.recent,
            unseen: mailbox.unseen,
            uid_validity: mailbox.uid_validity,
            uid_next: mailbox.uid_next,
            highest_modseq: mailbox.highest_modseq,
        }),
        Err(error) => Err(classify_imap_error(error)),
    }
}

/// 执行一条命令，并把服务器的结果读回来。
///
/// 与 async-imap 的同名方法不同：**服务器回 `NO` / `BAD` 时一定返回错误**。
///
/// ```
/// # use xremail_protocol_imap::command::run_checked;
/// # async fn demo() {}
/// ```
pub async fn run_checked(
    session: &mut Session,
    command: &str,
) -> Result<CommandOutcome, ProtocolError> {
    let id = session
        .run_command(command)
        .await
        .map_err(|e| ProtocolError::Imap(format!("发送命令失败: {e}")))?;

    let mut fetches: Vec<FetchedMessage> = Vec::new();
    let mut search: Vec<u32> = Vec::new();
    let mut list: Vec<ListedMailbox> = Vec::new();

    loop {
        let response = session
            .read_response()
            .await
            .map_err(|e| ProtocolError::Io(e.to_string()))?;

        let Some(response) = response else {
            return Err(ProtocolError::Io("连接被断开".into()));
        };

        match response.parsed() {
            Response::Fetch(seq, attributes) => {
                fetches.push(parse_fetch(*seq, attributes));
            }
            Response::MailboxData(imap_proto::MailboxDatum::Search(ids)) => {
                search.extend(ids.iter().copied());
            }
            Response::MailboxData(imap_proto::MailboxDatum::List(data)) => {
                list.push(parse_listed_mailbox(data));
            }
            Response::Done {
                tag,
                status,
                outcome,
            } => {
                // 别的命令的响应：忽略，继续读。
                if tag != &id {
                    continue;
                }
                return match status {
                    Status::Ok => Ok(if !fetches.is_empty() {
                        CommandOutcome::Fetches(fetches)
                    } else if !search.is_empty() {
                        CommandOutcome::Search(search)
                    } else if !list.is_empty() {
                        CommandOutcome::List(list)
                    } else {
                        CommandOutcome::Empty
                    }),
                    Status::No => {
                        // `[MODIFIED]` 不是失败，是「基线过期」——
                        // 处置方式完全不同（重新读快照重算）。
                        if is_modified(outcome) {
                            Err(ProtocolError::ConditionFailed(outcome_information(outcome)))
                        } else {
                            Err(ProtocolError::Imap(format!(
                                "服务器拒绝（NO）: {}",
                                outcome_information(outcome)
                            )))
                        }
                    }
                    Status::Bad => Err(ProtocolError::Imap(format!(
                        "服务器拒绝（BAD）: {}",
                        outcome_information(outcome)
                    ))),
                    other => Err(ProtocolError::Imap(format!("意外的响应状态: {other:?}"))),
                };
            }
            // 单方面响应（`EXISTS` / `RECENT` / `EXPUNGE` 等）与本命令无关，忽略。
            _ => {}
        }
    }
}

/// 响应码是否是 `MODIFIED`（条件写失败）。
///
/// **注意**：imap-proto 0.17 **不认识** `MODIFIED` 这个 response code
/// （它的 `ResponseCode` 枚举里没有这一项），
/// 而它对未知 code 的解析用的是 `opt()`——于是整个
/// `[MODIFIED] …` 会原样落进 `information` 文本。
///
/// 所以只能按文本判别。这也意味着：服务器回了
/// `NO [MODIFIED] …` 时，客户端**不会**解析失败，
/// 但也拿不到结构化的 response code。
fn is_modified(outcome: &imap_proto::Outcome<'_>) -> bool {
    outcome
        .information
        .as_ref()
        .is_some_and(|text| text.to_lowercase().contains("modified"))
}

/// 取出响应里的信息。
///
/// **带上 response code**——`[READ-ONLY]` / `[MODIFIED]` / `[ALERT]` 这些
/// 才是真正决定下一步怎么做的信息，光看文字容易漏掉。
pub(crate) fn outcome_information(outcome: &imap_proto::Outcome<'_>) -> String {
    let code = match &outcome.code {
        Some(code) => format!("{code:?} "),
        None => String::new(),
    };
    let text = outcome
        .information
        .as_ref()
        .map(|text| text.to_string())
        .unwrap_or_else(|| "(无)".to_string());
    format!("{code}{text}")
}

/// 解析一条 `LIST` 响应。
fn parse_listed_mailbox(data: &imap_proto::MailboxListData<'_>) -> ListedMailbox {
    let raw_name = data.name.to_string();
    let attributes: Vec<String> = data
        .name_attributes
        .iter()
        .map(|attribute| format!("{attribute:?}"))
        .collect();

    // 角色识别：属性优先，名字兜底（QQ 不给属性）。
    let role = crate::folders::role_from_attributes(&data.name_attributes).or_else(|| {
        let decoded = crate::utf7::decode(&raw_name).unwrap_or_else(|_| raw_name.clone());
        if decoded.eq_ignore_ascii_case("INBOX") {
            Some("inbox".to_string())
        } else {
            crate::folders::role_by_name(&decoded).map(str::to_string)
        }
    });

    ListedMailbox {
        raw_name,
        delimiter: data.delimiter.as_ref().map(|d| d.to_string()),
        attributes,
        role,
    }
}

/// 解析一条 FETCH 响应。
fn parse_fetch(seq: u32, attributes: &[AttributeValue<'_>]) -> FetchedMessage {
    let mut message = FetchedMessage {
        seq,
        ..Default::default()
    };

    for attribute in attributes {
        match attribute {
            AttributeValue::Uid(uid) => message.uid = Some(*uid),
            AttributeValue::Flags(flags) => {
                message.flags = flags.iter().map(|flag| flag.to_string()).collect();
            }
            AttributeValue::Rfc822Size(size) => message.size = Some(*size),
            AttributeValue::ModSeq(modseq) => message.modseq = Some(*modseq),
            AttributeValue::Rfc822Header(data) => {
                message.header = data.as_ref().map(|bytes| bytes.to_vec());
            }
            AttributeValue::Rfc822(data) => {
                message.body = data.as_ref().map(|bytes| bytes.to_vec());
            }
            AttributeValue::Rfc822Text(data) => {
                message.text = data.as_ref().map(|bytes| bytes.to_vec());
            }
            AttributeValue::BodySection { section, data, .. } => {
                let Some(data) = data.as_ref() else {
                    continue;
                };
                match section {
                    // `BODY[]` —— 整封。
                    None => message.body = Some(data.to_vec()),
                    Some(SectionPath::Full(MessageSection::Header)) => {
                        message.header = Some(data.to_vec());
                    }
                    Some(SectionPath::Full(MessageSection::Text)) => {
                        message.text = Some(data.to_vec());
                    }
                    // `HEADER.FIELDS (...)` 之类：算作头的一部分。
                    Some(SectionPath::Full(MessageSection::Mime)) => {
                        message.header = Some(data.to_vec());
                    }
                    Some(SectionPath::Part(_, _)) => {
                        message.body = Some(data.to_vec());
                    }
                }
            }
            _ => {}
        }
    }

    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fetch_extracts_common_attributes() {
        let attributes = vec![
            AttributeValue::Uid(42),
            AttributeValue::Flags(vec![std::borrow::Cow::Borrowed("\\Seen")]),
            AttributeValue::Rfc822Size(1234),
            AttributeValue::ModSeq(7),
        ];

        let message = parse_fetch(3, &attributes);

        assert_eq!(message.seq, 3);
        assert_eq!(message.uid, Some(42));
        assert_eq!(message.flags, vec!["\\Seen"]);
        assert_eq!(message.size, Some(1234));
        assert_eq!(message.modseq, Some(7));
        assert!(message.has_flag("\\seen"));
    }

    #[test]
    fn parse_fetch_extracts_body_section() {
        let attributes = vec![AttributeValue::BodySection {
            section: None,
            index: None,
            data: Some(std::borrow::Cow::Borrowed(b"body bytes")),
        }];

        let message = parse_fetch(1, &attributes);

        assert_eq!(message.body.as_deref(), Some(&b"body bytes"[..]));
    }

    #[test]
    fn outcome_helpers() {
        let outcome = CommandOutcome::Fetches(vec![FetchedMessage {
            seq: 1,
            ..Default::default()
        }]);
        assert_eq!(outcome.fetches().len(), 1);
        assert!(outcome.search().is_empty());

        let outcome = CommandOutcome::Search(vec![1, 2, 3]);
        assert_eq!(outcome.search(), &[1, 2, 3]);
        assert!(outcome.fetches().is_empty());
    }

    /// 服务器收了 `EXAMINE` 却**不回话**时，`examine_checked_with_timeout`
    /// 必须按超时返回，而不是永久挂住。
    #[tokio::test]
    async fn examine_times_out_when_server_never_answers() {
        use std::time::{Duration, Instant};

        use xremail_testkit::{FakeImapServer, Fault, ServerConfig};

        use crate::connection::{ImapConfig, TlsTrust, connect_and_login_with};

        let server = FakeImapServer::start(ServerConfig {
            faults: vec![Fault::HangOn("EXAMINE".to_string())],
            ..ServerConfig::default()
        })
        .await
        .expect("启动假服务器失败");

        let mut config = ImapConfig::imaps(server.host());
        config.port = server.port();
        config.connect_timeout = Duration::from_secs(5);
        let trust = TlsTrust::Roots(server.root_store());
        let (mut session, _, _) =
            connect_and_login_with(&config, &trust, "tester@example.com", "test-password")
                .await
                .expect("登录应当成功");

        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let result = examine_checked_with_timeout(&mut session, "INBOX", timeout).await;
        let elapsed = started.elapsed();

        match result {
            Ok(_) => panic!("服务器不回话时必须超时，而不是返回邮箱信息"),
            Err(ProtocolError::Timeout(detail)) => {
                assert!(
                    detail.contains("EXAMINE") && detail.contains("INBOX"),
                    "超时文案里应带上命令与邮箱名，实际: {detail}"
                );
            }
            Err(other) => panic!("应当是 Timeout，实际: {other:?}"),
        }
        assert!(
            elapsed >= timeout,
            "超时不应早于 {timeout:?}，实际 {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "超时必须在 {timeout:?} 量级返回（而不是挂到默认的 30 秒），实际 {elapsed:?}"
        );
    }
}
