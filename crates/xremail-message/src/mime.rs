//! MIME 结构解析。
//!
//! 实测（2026-10-23）：**三家的邮件顶层都是 multipart**
//! （QQ 20 封里 15 封、沃 6 封里 5 封、163 4 封全是）。
//! 所以**只看顶层的 `Content-Type` 是看不到正文 charset 的**——
//! 必须递归进子部分。
//!
//! 这一层只做**结构**解析（哪些部分是文本、什么编码、什么内容），
//! 不做 HTML 清洗（那是渲染层的事，见 ARCHITECTURE §7）。

/// 一个 MIME 部件的头字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderMap {
    /// 头字段（保持原顺序）。
    pub fields: Vec<(String, String)>,
}

impl HeaderMap {
    /// 取第一个匹配的头字段的值。
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 取所有匹配的头字段的值。
    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// 是否有这个头。
    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

/// `Content-Type` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentType {
    /// MIME 类型，小写（如 `text/plain`、`multipart/alternative`）。
    pub mime_type: String,
    /// `charset` 参数。
    pub charset: Option<String>,
    /// `multipart/*` 的 `boundary` 参数。
    pub boundary: Option<String>,
}

impl Default for ContentType {
    fn default() -> Self {
        Self {
            mime_type: "text/plain".to_string(),
            charset: None,
            boundary: None,
        }
    }
}

impl ContentType {
    /// 是不是 `multipart/*`。
    pub fn is_multipart(&self) -> bool {
        self.mime_type.starts_with("multipart/")
    }

    /// 是不是 `text/*`。
    pub fn is_text(&self) -> bool {
        self.mime_type.starts_with("text/")
    }
}

/// 解析 `Content-Type` 的值。
///
/// 没有值时按 RFC 2045 §5.2 默认 `text/plain; charset=us-ascii`。
pub fn parse_content_type(value: Option<&str>) -> ContentType {
    let Some(value) = value else {
        return ContentType::default();
    };

    let mut parts = value.split(';');
    let mime_type = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("text/plain")
        .to_ascii_lowercase();

    let mut charset = None;
    let mut boundary = None;

    for part in parts {
        let Some((key, val)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim().trim_matches('"').trim().to_string();

        match key.as_str() {
            "charset" if !val.is_empty() => charset = Some(val.to_ascii_lowercase()),
            "boundary" if !val.is_empty() => boundary = Some(val),
            _ => {}
        }
    }

    ContentType {
        mime_type,
        charset,
        boundary,
    }
}

/// 一个 MIME 部件（叶子）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// 头字段。
    pub headers: HeaderMap,
    /// 内容类型。
    pub content_type: ContentType,
    /// 传输编码。
    pub transfer_encoding: Option<String>,
    /// 文件名（`Content-Disposition: attachment; filename=…`）。
    pub filename: Option<String>,
    /// **已解码**的内容字节。
    pub content: Vec<u8>,
}

impl Part {
    /// 是不是附件（有文件名，或不是文本类型）。
    pub fn is_attachment(&self) -> bool {
        self.filename.is_some() || !self.content_type.is_text()
    }
}

/// 把一封邮件拆成所有叶子部件。
///
/// `multipart/*` 会按 `boundary` 递归拆开。
pub fn split_parts(headers: &HeaderMap, body: &[u8]) -> Vec<Part> {
    split_parts_with_paths(headers, body)
        .into_iter()
        .map(|(_, part)| part)
        .collect()
}

/// 与 [`split_parts`] 相同，但**额外给出每个叶子的部件号**。
///
/// 部件号按 IMAP 的约定自 1 起：顶层唯一部件是 `1`，`multipart` 的第 2 段的
/// 第 1 段是 `2.1`。附件下载靠它去取对应那一段的字节，
/// 所以**这里的编号必须与渲染层的遍历顺序一致**。
pub fn split_parts_with_paths(headers: &HeaderMap, body: &[u8]) -> Vec<(Vec<usize>, Part)> {
    let mut out = Vec::new();
    collect_parts(headers, body, &mut Vec::new(), &mut out);
    out
}

/// 递归收集叶子部件，`position` 只对叶子有意义。
fn collect_parts(
    headers: &HeaderMap,
    body: &[u8],
    parent: &mut Vec<usize>,
    out: &mut Vec<(Vec<usize>, Part)>,
) {
    let content_type = parse_content_type(headers.get("Content-Type"));

    if content_type.is_multipart()
        && let Some(boundary) = content_type.boundary.as_deref()
    {
        let children = split_multipart(body, boundary);
        if children.is_empty() {
            // 拆不出来（boundary 不对）→ 整块当成一个部件。
            out.push((parent.clone(), make_part(headers, body, &content_type)));
            return;
        }
        for (index, (child_headers, child_body)) in children.into_iter().enumerate() {
            parent.push(index + 1);
            collect_parts(&child_headers, &child_body, parent, out);
            parent.pop();
        }
        return;
    }

    out.push((parent.clone(), make_part(headers, body, &content_type)));
}

/// 造一个叶子部件（解码传输编码）。
fn make_part(headers: &HeaderMap, body: &[u8], content_type: &ContentType) -> Part {
    let transfer_encoding = headers
        .get("Content-Transfer-Encoding")
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let content = crate::transfer::decode_transfer(body, transfer_encoding.as_deref());

    let filename = headers
        .get("Content-Disposition")
        .and_then(|value| extract_param(value, "filename"));

    Part {
        headers: headers.clone(),
        content_type: content_type.clone(),
        transfer_encoding,
        filename,
        content,
    }
}

/// 从 `Content-Disposition` 之类的值里取参数。
pub fn extract_param(value: &str, name: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let Some((key, val)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            let val = val.trim().trim_matches('"').trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// 按 boundary 把 multipart 的 body 拆成若干段。
///
/// 每段返回 `(头字段, 内容)`。
fn split_multipart(body: &[u8], boundary: &str) -> Vec<(HeaderMap, Vec<u8>)> {
    let delimiter = format!("--{boundary}");
    let delimiter_bytes = delimiter.as_bytes();

    // 找每一段的起止。
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut index = 0usize;
    let mut current_start: Option<usize> = None;

    while index + delimiter_bytes.len() <= body.len() {
        // 只在行首找分隔符。
        let at_line_start = index == 0 || body[index - 1] == b'\n';
        if at_line_start && body[index..].starts_with(delimiter_bytes) {
            if let Some(start) = current_start.take() {
                segments.push((start, index));
            }
            // 跳过分隔符行。
            let mut cursor = index + delimiter_bytes.len();
            if body[cursor..].starts_with(b"--") {
                // 结束分隔符。
                break;
            }
            while cursor < body.len() && body[cursor] != b'\n' {
                cursor += 1;
            }
            cursor = (cursor + 1).min(body.len());
            current_start = Some(cursor);
            index = cursor;
            continue;
        }
        index += 1;
    }

    // 最后一段。
    if let Some(start) = current_start {
        let end = body.len();
        segments.push((start, end));
    }

    segments
        .into_iter()
        .map(|(start, end)| {
            let mut segment = &body[start..end];
            // RFC 2046 §5.1.1：边界分隔符**前面**的 CRLF 属于分隔符，
            // 不属于内容。不去掉的话每个部件的正文末尾会多一个换行。
            if segment.ends_with(b"\r\n") {
                segment = &segment[..segment.len() - 2];
            } else if segment.ends_with(b"\n") {
                segment = &segment[..segment.len() - 1];
            }
            split_head_body(segment)
        })
        .collect()
}

/// 把一段拆成 `(头字段, 内容)`。
pub fn split_head_body(raw: &[u8]) -> (HeaderMap, Vec<u8>) {
    // 找第一个空行（`\r\n\r\n` 或 `\n\n`）。
    let mut separator = None;
    let mut split_at = 0usize;

    for index in 0..raw.len() {
        if raw[index..].starts_with(b"\r\n\r\n") {
            separator = Some(4);
            split_at = index;
            break;
        }
        if raw[index..].starts_with(b"\n\n") {
            separator = Some(2);
            split_at = index;
            break;
        }
    }

    let Some(separator) = separator else {
        return (parse_headers(raw), Vec::new());
    };

    let head = &raw[..split_at];
    let body = &raw[split_at + separator..];

    (parse_headers(head), body.to_vec())
}

/// 解析头字段块。
///
/// 支持**折行**（RFC 5322 §2.2.3：行首是空白表示接上一行）。
pub fn parse_headers(raw: &[u8]) -> HeaderMap {
    let text = String::from_utf8_lossy(raw);
    let mut fields: Vec<(String, String)> = Vec::new();

    let mut current: Option<(String, String)> = None;

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);

        if line.is_empty() {
            break;
        }

        // 折行：行首是空白 → 接上一行。
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }

        // 存上一行。
        if let Some(entry) = current.take() {
            fields.push(entry);
        }

        match line.split_once(':') {
            Some((name, value)) => {
                current = Some((name.trim().to_string(), value.trim().to_string()));
            }
            None => {
                // 没有冒号的行：忽略（不合法但真实邮件里存在）。
            }
        }
    }

    if let Some(entry) = current {
        fields.push(entry);
    }

    HeaderMap { fields }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_type_with_charset() {
        let parsed = parse_content_type(Some("text/plain; charset=GB2312"));

        assert_eq!(parsed.mime_type, "text/plain");
        assert_eq!(parsed.charset.as_deref(), Some("gb2312"));
        assert!(!parsed.is_multipart());
        assert!(parsed.is_text());
    }

    #[test]
    fn parses_content_type_with_boundary() {
        let parsed = parse_content_type(Some("multipart/alternative; boundary=\"abc\""));

        assert!(parsed.is_multipart());
        assert_eq!(parsed.boundary.as_deref(), Some("abc"));
    }

    #[test]
    fn missing_content_type_defaults_to_text_plain() {
        let parsed = parse_content_type(None);

        assert_eq!(parsed.mime_type, "text/plain");
        assert!(parsed.charset.is_none());
    }

    #[test]
    fn parses_headers() {
        let raw = b"Subject: hello\r\nFrom: a@example.com\r\nX-Empty:\r\n";

        let headers = parse_headers(raw);

        assert_eq!(headers.get("subject"), Some("hello"));
        assert_eq!(headers.get("FROM"), Some("a@example.com"));
        assert!(headers.contains("x-empty"));
    }

    #[test]
    fn parses_folded_headers() {
        let raw = b"Subject: line one\r\n  continued\r\nFrom: a@example.com\r\n";

        let headers = parse_headers(raw);

        assert_eq!(headers.get("subject"), Some("line one continued"));
        assert_eq!(headers.get("from"), Some("a@example.com"));
    }

    #[test]
    fn splits_head_and_body() {
        let raw = b"Subject: x\r\n\r\nbody here";

        let (headers, body) = split_head_body(raw);

        assert_eq!(headers.get("subject"), Some("x"));
        assert_eq!(body, b"body here");
    }

    #[test]
    fn splits_multipart_into_parts() {
        let body = b"--b\r\nContent-Type: text/plain; charset=gbk\r\n\r\nfirst\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>second</p>\r\n--b--\r\n";
        let headers = parse_headers(b"Content-Type: multipart/mixed; boundary=b\r\n");

        let parts = split_parts(&headers, body);

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].content_type.charset.as_deref(), Some("gbk"));
        assert_eq!(parts[0].content, b"first");
        assert_eq!(parts[1].content_type.mime_type, "text/html");
    }

    #[test]
    fn extracts_disposition_filename() {
        let value = "attachment; filename=\"报告.pdf\"";

        assert_eq!(
            extract_param(value, "filename").as_deref(),
            Some("报告.pdf")
        );
    }
}
