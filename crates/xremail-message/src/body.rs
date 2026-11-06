//! 整封邮件的解析入口。
//!
//! 把 `BODY[]` 拉下来的原始字节变成结构化结果：
//! 头字段 + 纯文本正文 + 原始 HTML + 附件标记。
//!
//! ## 分工
//!
//! * 本 crate 负责 **MIME / charset / 头字段**；
//! * **HTML 清洗不在这里**——那是渲染层的事（见 ARCHITECTURE §7），
//!   本模块只给出**未经清洗的**原始 HTML。

use xremail_core::model::Address;

use crate::attachment::{AttachmentMeta, attachment_meta};
use crate::charset;
use crate::encoded_word::decode_encoded_words;
use crate::mime::{HeaderMap, Part, split_head_body, split_parts_with_paths};

/// 一封邮件最多认多少个附件部件。
///
/// 与发信侧的 `MAX_ATTACHMENTS`（5）不是一个概念：收信时对方想塞多少是多少，
/// 这里只防「恶意邮件把解析吃爆」。1000 远超正常邮件，又小到不会撑爆内存。
pub const MAX_ATTACHMENT_PARTS: usize = 1000;

/// 解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MessageError {
    /// 邮件头不是合法的 UTF-8 且无法按任何编码解码。
    ///
    /// 实际上几乎不会发生（解码器总能给出结果），
    /// 但保留这个变体是为了让「彻底坏掉」的邮件有明确去处。
    #[error("邮件内容无法解码: {0}")]
    Undecodable(String),
}

/// 解析后的**头字段**。
///
/// 单独拆出来是因为两种场景：
/// * 同步时只拉 `BODY[HEADER]`（头）——没有正文；
/// * 按需拉正文时拉整封——头与正文一起。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedHeaders {
    /// 主题（已解码 RFC 2047）。
    pub subject: Option<String>,
    /// 发件人。
    pub from: Vec<Address>,
    /// 收件人。
    pub to: Vec<Address>,
    /// 抄送。
    pub cc: Vec<Address>,
    /// 密送。
    pub bcc: Vec<Address>,
    /// `Date` 头（epoch 毫秒）。
    pub date: Option<i64>,
    /// `Message-ID`（不含尖括号）。
    pub message_id: Option<String>,
}

/// 解析后的邮件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedMessage {
    /// 头字段。
    pub headers: ParsedHeaders,
    /// 纯文本正文。
    pub text_plain: Option<String>,
    /// **原始** HTML（未清洗）。
    pub html_raw: Option<String>,
    /// 正文用的字符集。
    pub charset: Option<String>,
    /// 有附件。
    pub has_attachment: bool,
    /// 附件部件的元数据（**不含字节**），按部件号顺序。
    ///
    /// 最多 [`MAX_ATTACHMENT_PARTS`] 条——这是**解析侧**的硬上限，
    /// 防止一封塞了几万个 part 的邮件把内存吃满；超出的部分直接丢弃，
    /// 上层靠「条数到了上限」判断要不要提示用户。
    pub attachments: Vec<AttachmentMeta>,
    /// 解码时**出现过非法字节**（被替换成 U+FFFD）。
    ///
    /// 用来诊断「解码得对不对」：真邮箱里的邮件如果这里为 `true`，
    /// 说明 charset 猜错了或者该编码不支持。
    /// **不打印内容**——只看这个标记。
    pub decode_errors: bool,
}

/// 只解析头。
///
/// 输入可以是**整封**邮件，也可以只是头块（`BODY[HEADER]` 的结果）。
pub fn parse_headers(raw: &[u8]) -> ParsedHeaders {
    let (headers, _) = split_head_body(raw);
    parse_header_map(&headers)
}

/// 解析头字段块。
fn parse_header_map(headers: &HeaderMap) -> ParsedHeaders {
    ParsedHeaders {
        subject: decode_subject(headers),
        from: parse_address_list(headers, "From"),
        to: parse_address_list(headers, "To"),
        cc: parse_address_list(headers, "Cc"),
        bcc: parse_address_list(headers, "Bcc"),
        date: parse_date(headers),
        message_id: headers
            .get("Message-ID")
            .map(|value| value.trim().trim_matches(['<', '>']).to_string())
            .filter(|value| !value.is_empty()),
    }
}

/// 解析一封邮件。
pub fn parse_message(raw: &[u8]) -> Result<ParsedMessage, MessageError> {
    let (headers, body) = split_head_body(raw);
    let numbered = split_parts_with_paths(&headers, &body);
    let parts: Vec<&Part> = numbered.iter().map(|(_, part)| part).collect();

    let mut parsed = ParsedMessage {
        headers: parse_header_map(&headers),
        ..Default::default()
    };

    // 找文本部分。
    //
    // **优先 text/plain**；没有 plain 才有 html。
    // 多个候选时取**第一个**（服务器一般按「纯文本在前」排）。
    let plain: Option<&Part> = parts
        .iter()
        .copied()
        .find(|part| part.content_type.mime_type == "text/plain");
    let html: Option<&Part> = parts
        .iter()
        .copied()
        .find(|part| part.content_type.mime_type == "text/html");

    parsed.has_attachment = parts.iter().any(|part| part.is_attachment());

    // 附件元数据：只认 `Content-Disposition`，不看「内容看起来像不像文件」。
    // 有序遍历，保证 `part_number` 与 IMAP 的编号一致。
    let top_is_multipart =
        crate::mime::parse_content_type(headers.get("Content-Type")).is_multipart();
    for (path, part) in &numbered {
        if parsed.attachments.len() >= MAX_ATTACHMENT_PARTS {
            break;
        }
        let size = i64::try_from(part.content.len()).unwrap_or(i64::MAX);
        if let Some(meta) = attachment_meta(
            &part.headers,
            Some(path.as_slice()),
            top_is_multipart,
            &part.content_type.mime_type,
            size,
            part.transfer_encoding.as_deref(),
        ) {
            parsed.attachments.push(meta);
        }
    }

    if let Some(part) = plain {
        let charset = part.content_type.charset.clone();
        let (text, _, had_errors) = charset::decode(&part.content, charset.as_deref());
        parsed.text_plain = Some(text);
        parsed.decode_errors |= had_errors;
        parsed.charset = charset;
    }

    if let Some(part) = html {
        let charset = part.content_type.charset.clone();
        let (text, _, had_errors) = charset::decode(&part.content, charset.as_deref());
        parsed.html_raw = Some(text);
        parsed.decode_errors |= had_errors;
        if parsed.charset.is_none() {
            parsed.charset = charset;
        }
    }

    // 只有 HTML、没有纯文本时，**从 HTML 派生一份纯文本**。
    //
    // 真实邮箱里的邮件大多是 HTML-only（实测 163：纯文本部分为空）。
    // 不派生的话：
    // * 搜索索引只吃 `text_plain` → 这类邮件的正文**一个字都搜不到**；
    // * 阅读界面只能显示 HTML 源码（除非渲染消毒后的 HTML）。
    //
    // 派生出来的文本**只作投影**：原始 HTML 仍在 `html_raw` 里，
    // 渲染时用它（先消毒）。
    if parsed.text_plain.is_none()
        && let Some(html) = parsed.html_raw.as_deref()
    {
        let derived = xremail_core::html::html_to_text(html);
        if !derived.is_empty() {
            parsed.text_plain = Some(derived);
        }
    }

    Ok(parsed)
}

/// 解码主题。
fn decode_subject(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("Subject")?;
    let decoded = decode_encoded_words(raw);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 解析地址列表。
///
/// `From: "名字" <a@example.com>, b@example.com`
fn parse_address_list(headers: &HeaderMap, field: &str) -> Vec<Address> {
    let Some(raw) = headers.get(field) else {
        return Vec::new();
    };

    let decoded = decode_encoded_words(raw);
    parse_addresses(&decoded)
}

/// 解析一个地址列表。
///
/// 支持：
/// * `a@example.com`
/// * `名字 <a@example.com>`
/// * `"名字" <a@example.com>`
/// * 逗号分隔的多个
pub fn parse_addresses(input: &str) -> Vec<Address> {
    let mut out = Vec::new();

    for part in split_address_list(input) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let (name, email) = match (part.find('<'), part.rfind('>')) {
            (Some(start), Some(end)) if end > start => {
                let name = part[..start].trim().trim_matches('"').trim().to_string();
                let email = part[start + 1..end].trim().to_string();
                (name, email)
            }
            _ => (String::new(), part.to_string()),
        };

        let email = email.trim_matches(['<', '>']).trim().to_string();
        if email.is_empty() {
            continue;
        }

        out.push(Address {
            name: if name.is_empty() { None } else { Some(name) },
            email,
        });
    }

    out
}

/// 按逗号切地址列表（但不吃掉引号里的逗号）。
fn split_address_list(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_angle = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '<' if !in_quotes => {
                in_angle = true;
                current.push(ch);
            }
            '>' if !in_quotes => {
                in_angle = false;
                current.push(ch);
            }
            ',' if !in_quotes && !in_angle => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    parts.push(current);

    parts
}

/// 解析 `Date` 头（RFC 5322 §3.3）。
///
/// 支持 `Mon, 01 Jan 2024 00:00:00 +0800` 与 `1 Jan 2024 00:00:00 +0800`。
///
/// **不用** `chrono`：这里只需要一个足够好的日期解析，
/// 而引入完整日期库对 P0 没有必要。解析不了就返回 `None`——
/// 日期不是关键路径。
fn parse_date(headers: &HeaderMap) -> Option<i64> {
    let raw = headers.get("Date")?;
    parse_rfc5322_date(raw.trim())
}

/// 解析 RFC 5322 日期 → epoch 毫秒。
pub fn parse_rfc5322_date(raw: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    // 去掉星期几。
    let rest = match raw.find(',') {
        Some(index) => raw[index + 1..].trim(),
        None => raw.trim(),
    };

    let mut parts = rest.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(month_name))? as u32
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;

    let time = parts.next().unwrap_or("00:00:00");
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next().unwrap_or("0").parse().ok()?;
    let second: i64 = time_parts.next().unwrap_or("0").parse().ok()?;

    // 时区（`+0800` / `-0500` / `GMT`）。
    let offset_minutes: i64 = match parts.next() {
        Some(zone) if zone.len() == 5 && (zone.starts_with('+') || zone.starts_with('-')) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone[1..3].parse().ok()?;
            let minutes: i64 = zone[3..5].parse().ok()?;
            sign * (hours * 60 + minutes)
        }
        _ => 0,
    };

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;

    Some(seconds * 1000)
}

/// 1970-01-01 到 `year-month-day` 的天数（Howard Hinnant 的算法）。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = i64::from(month);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_utf8_message() {
        let raw = "Subject: =?UTF-8?B?5Li76aKY?=\r\n\
                   From: sender@example.com\r\n\
                   To: me@example.com\r\n\
                   Date: Mon, 01 Jan 2024 00:00:00 +0800\r\n\
                   Message-ID: <abc@example.com>\r\n\
                   Content-Type: text/plain; charset=utf-8\r\n\
                   \r\n\
                   正文内容"
            .as_bytes();

        let parsed = parse_message(raw).expect("应当能解析");

        assert_eq!(parsed.headers.subject.as_deref(), Some("主题"));
        assert_eq!(parsed.headers.from.len(), 1);
        assert_eq!(parsed.headers.from[0].email, "sender@example.com");
        assert_eq!(parsed.headers.to[0].email, "me@example.com");
        assert_eq!(
            parsed.headers.message_id.as_deref(),
            Some("abc@example.com")
        );
        assert_eq!(parsed.text_plain.as_deref(), Some("正文内容"));
        assert_eq!(parsed.charset.as_deref(), Some("utf-8"));
        assert!(!parsed.has_attachment);
    }

    #[test]
    fn parses_a_gbk_multipart_message() {
        // GB2312 的“中文”与 multipart/alternative。
        let mut raw: Vec<u8> = Vec::new();
        raw.extend_from_slice(
            b"Subject: =?gb2312?B?1tDOxA==?=\r\n\
              From: a@example.com\r\n\
              Content-Type: multipart/alternative; boundary=\"XX\"\r\n\
              \r\n",
        );
        raw.extend_from_slice(b"--XX\r\nContent-Type: text/plain; charset=gb2312\r\n\r\n");
        raw.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
        raw.extend_from_slice(b"\r\n--XX\r\nContent-Type: text/html; charset=gb2312\r\n\r\n<p>");
        raw.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]);
        raw.extend_from_slice(b"</p>\r\n--XX--\r\n");

        let parsed = parse_message(&raw).expect("应当能解析");

        assert_eq!(parsed.headers.subject.as_deref(), Some("中文"));
        assert_eq!(parsed.text_plain.as_deref(), Some("中文"));
        assert_eq!(parsed.html_raw.as_deref(), Some("<p>中文</p>"));
        assert_eq!(parsed.charset.as_deref(), Some("gb2312"));
    }

    #[test]
    fn detects_attachments() {
        let raw = b"Content-Type: multipart/mixed; boundary=b\r\n\r\n\
                    --b\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
                    --b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\r\n%PDF\r\n\
                    --b--\r\n";

        let parsed = parse_message(raw).expect("应当能解析");

        assert!(parsed.has_attachment);
        assert_eq!(parsed.text_plain.as_deref(), Some("hi"));
    }

    #[test]
    fn parses_addresses_with_display_names() {
        let addresses = parse_addresses("\"张三\" <a@example.com>, b@example.com");

        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].name.as_deref(), Some("张三"));
        assert_eq!(addresses[0].email, "a@example.com");
        assert_eq!(addresses[1].name, None);
        assert_eq!(addresses[1].email, "b@example.com");
    }

    #[test]
    fn parses_addresses_with_comma_in_display_name() {
        let addresses = parse_addresses("\"Doe, John\" <a@example.com>");

        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].name.as_deref(), Some("Doe, John"));
        assert_eq!(addresses[0].email, "a@example.com");
    }

    #[test]
    fn parses_dates() {
        let millis = parse_rfc5322_date("Mon, 01 Jan 2024 00:00:00 +0800").expect("应当能解析");

        // 2024-01-01T00:00:00+08:00 == 2023-12-31T16:00:00Z
        assert_eq!(millis, 1_704_038_400_000);
    }

    #[test]
    fn parses_date_without_weekday() {
        let millis = parse_rfc5322_date("1 Jan 2024 00:00:00 +0000").expect("应当能解析");

        assert_eq!(millis, 1_704_067_200_000);
    }

    #[test]
    fn unparsable_date_is_none() {
        assert_eq!(parse_rfc5322_date("不是日期"), None);
    }
}
