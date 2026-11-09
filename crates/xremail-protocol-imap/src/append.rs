//! APPEND：把一封信存进服务器上的文件夹。
//!
//! ## 为什么自己写命令
//!
//! APPEND 的正文是**字面量**（literal），必须分两步发：
//! 先发命令头（`APPEND mbox (\Seen) {123+}`），
//! 等服务器回 `+` 让咱们继续，再发正文。
//!
//! async-imap 的 `run_command` 表达不了这个流程——它只接受 `&str`，
//! 而且写完命令自带一个 CRLF（那会把命令行变成两行）。
//! 所以这里直接写 socket，tag 也自己带（见 [`next_tag`]）。
//!
//! ## 为什么用**同步**字面量（`{n+}`）而不是 `{n}`
//!
//! `{n}` 是非同步字面量：客户端不等 `+` 就把正文连同命令一起发出去。
//! 规范允许，但**有些服务器仍会先回 `+`**——那时正文会被当成一条新命令，
//! 服务器报 `BAD`，而且客户端还以为发出去了。
//! `{n+}` 没有这个歧义：服务器一定先回 `+`，我们再发正文。
//!
//! ## 幂等
//!
//! APPEND **不幂等**：同一封信 append 两次就是两封。
//! 所以调用方必须先确认「文件夹里还没有这封信」再调本函数
//! （见服务层的「发完确保已发送副本」逻辑）。

use std::sync::atomic::{AtomicU32, Ordering};

use imap_proto::rfc4315::UidSetMember;
use imap_proto::{Response, ResponseCode, Status};
use tokio::io::AsyncWriteExt;

use crate::connection::Session;
use crate::error::ProtocolError;

/// 自用 tag 的前缀。
///
/// async-imap 用 `A0001` 这种 tag（见它的 `IdGenerator`），
/// 我们用 `X` 开头——**两套不会撞**。
/// 撞了就糟了：会把别人的响应当成自己的，命令结果全错。
const TAG_PREFIX: &str = "X";

/// 自用 tag 的计数器。
static TAG_COUNTER: AtomicU32 = AtomicU32::new(0);

/// 取下一个 tag。
fn next_tag() -> String {
    let n = TAG_COUNTER.fetch_add(1, Ordering::Relaxed) % 10_000;
    format!("{TAG_PREFIX}{n:04}")
}

/// APPEND 的选项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppendOptions {
    /// 要设的标记（例如 `\Seen`）。
    ///
    /// **不要**放 `\Recent`——那是服务器自己管的，客户端设了会被拒。
    pub flags: Vec<String>,
    /// 内部日期（服务器拿它排序）。
    ///
    /// RFC 3501 的格式：`26-Oct-2026 12:00:00 +0800`。
    /// `None` 就用服务器当前时间。
    pub internal_date: Option<String>,
}

/// APPEND 的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppendOutcome {
    /// 新邮件的 UID。
    ///
    /// 来自 `APPENDUID` 响应码（RFC 4315）——服务器不给就是 `None`。
    /// **注意**：这不是「这封信的标识」——IMAP 里同一封信可以有多个 UID
    /// （比如服务器自己存了一份、我们又 append 了一份）。
    pub uid: Option<u32>,
    /// 目标文件夹的 UIDVALIDITY。
    pub uid_validity: Option<u32>,
    /// 服务器的说明文字。
    pub message: Option<String>,
}

/// 把一封信追加到文件夹末尾。
///
/// `mailbox` 传**线路上的原名**（modified UTF-7），
/// 不是解码后的名字——拿解码后的名字去开文件夹会得到
/// `NO EXAMINE Folder not exist`（见 [`crate::folders`]）。
pub async fn append_message(
    session: &mut Session,
    mailbox: &str,
    content: &[u8],
    options: &AppendOptions,
) -> Result<AppendOutcome, ProtocolError> {
    if content.is_empty() {
        // 空正文会让某些服务器直接 `BAD`，而且它一定是调用方的 bug。
        return Err(ProtocolError::Imap("APPEND 的正文是空的".into()));
    }

    let tag = next_tag();
    let command = build_append_command(mailbox, content.len(), options);

    {
        let stream = session.get_mut();
        stream.write_all(tag.as_bytes()).await.map_err(io_error)?;
        stream.write_all(b" ").await.map_err(io_error)?;
        stream
            .write_all(command.as_bytes())
            .await
            .map_err(io_error)?;
        stream.write_all(b"\r\n").await.map_err(io_error)?;
        stream.flush().await.map_err(io_error)?;
    }

    // 等服务器的续传指示（`+`）。
    wait_for_continuation(session, &tag).await?;

    {
        let stream = session.get_mut();
        stream.write_all(content).await.map_err(io_error)?;
        // 字面量之后必须跟 CRLF，命令才算结束。
        stream.write_all(b"\r\n").await.map_err(io_error)?;
        stream.flush().await.map_err(io_error)?;
    }

    read_append_result(session, &tag).await
}

/// 拼出 APPEND 的命令头（不含 tag，也不含结尾的 CRLF）。
///
/// 单独拎出来是为了能**直接测**——命令拼错了在真实服务器上
/// 表现为一句没头没尾的 `BAD`，很难查。
pub fn build_append_command(mailbox: &str, content_len: usize, options: &AppendOptions) -> String {
    let mut command = format!("APPEND {}", quote_mailbox(mailbox));

    if !options.flags.is_empty() {
        command.push_str(" (");
        command.push_str(&options.flags.join(" "));
        command.push(')');
    }

    if let Some(date) = &options.internal_date {
        command.push_str(&format!(" \"{}\"", date));
    }

    // 同步字面量：服务器先回 `+`，我们再发正文。
    command.push_str(&format!(" {{{}+}}", content_len));
    command
}

/// 给文件夹名加引号。
///
/// 线路上的文件夹名（modified UTF-7）一般只有 `&`、`=` 这类字符，
/// 但**不能假设**：带空格、带引号的名字也得能开。
fn quote_mailbox(mailbox: &str) -> String {
    let mut quoted = String::with_capacity(mailbox.len() + 2);
    quoted.push('"');
    for ch in mailbox.chars() {
        if ch == '"' || ch == '\\' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

/// 把 IO 错误分类成协议错误。
fn io_error(error: std::io::Error) -> ProtocolError {
    ProtocolError::Io(error.to_string())
}

/// 等服务器回续传指示（`+`）。
///
/// 服务器如果**直接**给了结果（`NO` / `BAD`），说明命令本身被拒了——
/// 这时不能接着发正文（那会被当成新命令），直接报错。
async fn wait_for_continuation(session: &mut Session, tag: &str) -> Result<(), ProtocolError> {
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|e| ProtocolError::Io(e.to_string()))?;

        let Some(response) = response else {
            return Err(ProtocolError::Io("连接被断开".into()));
        };

        match response.parsed() {
            Response::Continue(_) => return Ok(()),
            Response::Done {
                tag: done_tag,
                status,
                outcome,
            } if done_tag.0 == tag => {
                return Err(match status {
                    Status::Ok => {
                        ProtocolError::Imap("服务器没要续传就直接回了 OK（不该发生）".to_string())
                    }
                    other => ProtocolError::Imap(format!(
                        "服务器拒绝（{other:?}）: {}",
                        crate::command::outcome_information(outcome)
                    )),
                });
            }
            // 别的命令的响应，或与本命令无关的单方面响应：忽略，继续等。
            _ => {}
        }
    }
}

/// 读 APPEND 的最终结果。
async fn read_append_result(
    session: &mut Session,
    tag: &str,
) -> Result<AppendOutcome, ProtocolError> {
    loop {
        let response = session
            .read_response()
            .await
            .map_err(|e| ProtocolError::Io(e.to_string()))?;

        let Some(response) = response else {
            return Err(ProtocolError::Io("连接被断开".into()));
        };

        match response.parsed() {
            Response::Done {
                tag: done_tag,
                status,
                outcome,
            } if done_tag.0 == tag => {
                return match status {
                    Status::Ok => Ok(AppendOutcome {
                        uid: appended_uid(outcome),
                        uid_validity: appended_uid_validity(outcome),
                        message: outcome.information.as_ref().map(|text| text.to_string()),
                    }),
                    Status::No => Err(ProtocolError::Imap(format!(
                        "服务器拒绝（NO）: {}",
                        crate::command::outcome_information(outcome)
                    ))),
                    Status::Bad => Err(ProtocolError::Imap(format!(
                        "服务器拒绝（BAD）: {}",
                        crate::command::outcome_information(outcome)
                    ))),
                    other => Err(ProtocolError::Imap(format!("意外的响应状态: {other:?}"))),
                };
            }
            // 别的命令的响应：忽略，继续读。
            _ => {}
        }
    }
}

/// 从响应里取出 `APPENDUID` 里的 UID。
///
/// 对测试公开（`tests/imap_parsing_properties.rs`）：闭区间的端点差一位，
/// 发信副本的 UID 就会全记错。
pub fn appended_uid(outcome: &imap_proto::Outcome<'_>) -> Option<u32> {
    let Some(ResponseCode::AppendUid(_, members)) = &outcome.code else {
        return None;
    };
    // `APPENDUID` 只可能给一个 UID；带区间时取**最后一个**：
    // `UidRange` 是闭区间（imap-proto 用 `fst..=snd` 构造），所以 `.end()` 就是末个 UID。
    // 曾经写成 `end().checked_sub(1)`：`APPENDUID 123 5:7` 会被错报成 6，
    // 于是发信副本的 UID 全记错一位。
    members.first().map(|member| match member {
        UidSetMember::Uid(uid) => *uid,
        UidSetMember::UidRange(range) => *range.end(),
    })
}

/// 从响应里取出 `APPENDUID` 里的 UIDVALIDITY。
fn appended_uid_validity(outcome: &imap_proto::Outcome<'_>) -> Option<u32> {
    match &outcome.code {
        Some(ResponseCode::AppendUid(validity, _)) => Some(*validity),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_contains_mailbox_and_synchronizing_literal() {
        let options = AppendOptions {
            flags: vec!["\\Seen".to_string()],
            internal_date: Some("26-Oct-2026 12:00:00 +0800".to_string()),
        };

        let command = build_append_command("&XfJT0ZAB-", 1234, &options);

        assert_eq!(
            command,
            "APPEND \"&XfJT0ZAB-\" (\\Seen) \"26-Oct-2026 12:00:00 +0800\" {1234+}"
        );
    }

    #[test]
    fn command_without_options_omits_flags_and_date() {
        let command = build_append_command("INBOX", 7, &AppendOptions::default());

        assert_eq!(command, "APPEND \"INBOX\" {7+}");
    }

    #[test]
    fn mailbox_with_quote_or_space_is_quoted() {
        let command = build_append_command("my folder", 1, &AppendOptions::default());
        assert_eq!(command, "APPEND \"my folder\" {1+}");

        let command = build_append_command("we\"ird", 1, &AppendOptions::default());
        assert_eq!(command, "APPEND \"we\\\"ird\" {1+}");
    }

    #[test]
    fn tags_do_not_collide_with_async_imap() {
        // async-imap 用 `A0001` 这种 tag；我们必须不一样。
        let tag = next_tag();
        assert!(tag.starts_with(TAG_PREFIX));
        assert!(!tag.starts_with('A'));
        assert_ne!(tag, next_tag());
    }
}
