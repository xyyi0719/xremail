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
/// 部件号按 IMAP 的约定自 1 起：顶层 `multipart` 自己占 1 号，它的第 2 段是 `2`，
/// 那一段的第 1 段是 `2.1`（本函数返回的路径**不含**顶层的 `1`，
/// 拼完整部件号的 `1.` 前缀在 `attachment::attachment_meta` 里补，
/// 否则「整封就是一个部件」的 `TEXT` 会被拼成 `1.TEXT`）。
/// 附件下载靠它去取对应那一段的字节（`UID FETCH <uid> BODY.PEEK[<部件号>]`），
/// 所以**这里的编号必须与服务器一致**。
///
/// 三条容易写错的地方：
/// * 顶层的那个 `multipart` 自己**占 1 号**，它的子部件是 `1.1`、`1.2`……
///   于是「顶层 multipart 里的第二个附件」是 `1.2`，不是 `2`；
/// * 「`multipart` 但拆不出任何段」（没有 boundary、或 boundary 对不上）
///   在 IMAP 里仍然是 1 号部件——它自身就是整个正文，没有子段；
/// * `message/rfc822` 那一层**不占号**（RFC 3501 §6.4.5 只说 `multipart` 与
///   `message/rfc822` 以外的层不计数；两头的 `multipart` 照常计数）。
///
/// 顶层**非** `multipart` 的邮件（整封就是一段，ScannedAttachment / 服务器直接返回
/// 解码后的正文时很常见）在 IMAP 里既能用 `1` 取，也能用 `TEXT` 取；
/// 这里给的是空路径，最终由 `TOP_LEVEL_PART` 记作 `TEXT`。
pub fn split_parts_with_paths(headers: &HeaderMap, body: &[u8]) -> Vec<(Vec<usize>, Part)> {
    // 只收**叶子**：容器层的路径由 `collect_leaves` 顺手跳过。
    let mut leaves = Vec::new();
    collect_leaves(headers, body, &mut Vec::new(), &mut leaves);
    leaves
}

/// 递归收集**叶子**部件，路径里带上每一层（含 `message/rfc822` 那层）。
fn collect_leaves(
    headers: &HeaderMap,
    body: &[u8],
    path: &mut Vec<usize>,
    out: &mut Vec<(Vec<usize>, Part)>,
) {
    let content_type = parse_content_type(headers.get("Content-Type"));

    if content_type.is_multipart()
        && let Some(boundary) = content_type.boundary.as_deref()
    {
        let children = split_multipart(body, boundary);
        if children.is_empty() {
            out.push((path.clone(), make_part(headers, body, &content_type)));
            return;
        }
        for (index, (child_headers, child_body)) in children.into_iter().enumerate() {
            path.push(index + 1);
            collect_leaves(&child_headers, &child_body, path, out);
            path.pop();
        }
        return;
    }

    if is_uncounted(headers) {
        // `message/rfc822`：内容是一整封邮件，拆开继续往里走。
        // 这一层在服务器上的号是 `<前面的号>.0`，所以路径里压一个 `0` 占位；
        // 进来的那封邮件多包一层 `multipart` 时，服务器**不再加**自己的 `1`
        // （`BODY[1.2.0.2]` 而不是 `BODY[1.2.0.1.2]`），所以先压 `0`、
        // 再把这封邮件的顶层容器那层剥掉。
        let (inner_headers, inner_body) = split_head_body(body);
        let inner_type = parse_content_type(inner_headers.get("Content-Type"));
        let multibody = match inner_type.boundary.as_deref() {
            Some(boundary) if inner_type.is_multipart() => split_multipart(&inner_body, boundary),
            _ => Vec::new(),
        };
        if multibody.is_empty() {
            // 内嵌邮件本身就是单个部件：它就是 `<父号>.0`。
            collect_leaves(&inner_headers, &inner_body, path, out);
        } else {
            path.push(EMBEDDED_MESSAGE_NUMBER);
            for (index, (child_headers, child_body)) in multibody.into_iter().enumerate() {
                path.push(index + 1);
                collect_leaves(&child_headers, &child_body, path, out);
                path.pop();
            }
            path.pop();
        }
        return;
    }

    out.push((path.clone(), make_part(headers, body, &content_type)));
}

/// 部件号里「整封邮件就是这一个部件」的记号（IMAP 的 `TEXT`）。
///
/// **不能用 `0`**：`BODY.PEEK[0]` 在 IMAP 里只返回 `BODYSTRUCTURE` 而不返回正文
/// （0 是「只要结构」的法定写法），用它去取附件会永远取不回来。
pub const TOP_LEVEL_PART: &str = "TEXT";

/// 部件号里「`message/rfc822` 里那封内嵌邮件自身」的占位。
///
/// 内嵌邮件**自己不占号**，但它的正文（`.` 右边的那个 `0`）服务器给的是
/// `BODY[<前面的号>.0]`；再往里数子部件时，`BODYSTRUCTURE` 里的第 k 段
/// 对应 `BODY[<前面的号>.k]`，也就是回到父层的号上继续数。
/// 例子：`multipart/mixed` 第 2 段是 `message/rfc822{#2.1 = multipart/alternative{
/// #2.1.1 = text/plain, #2.1.2 = multipart/mixed{#2.1.2.1 …}}}` 时，
/// 最里层第 1 段是 `BODY[2.1.2.1]`。
///
/// 本模块的路径不带这一层（见 [`is_uncounted`]），拼字符串时由
/// [`crate::attachment::attachment_meta`] 补上这个 `0`。
pub const EMBEDDED_MESSAGE: &str = "0";

/// [`EMBEDDED_MESSAGE`] 的数字形式（路径里存的是数字）。
const EMBEDDED_MESSAGE_NUMBER: usize = 0;

/// 这一层在 IMAP 的部件号里占不占号。
///
/// RFC 3501 §6.4.5：`message/rfc822` 与 `multipart` 以外的容器层不计数，
/// 所以 `multipart/mixed{text/plain, message/rfc822{multipart/mixed{text,image}}}`
/// 里的 `image` 是 `2.1.1`……而不是 `2.1.1.1` 之类带上一层空号的形式。
///
/// 这里只按「这一层是不是 `message/rfc822`」判断：乘号的另一头（`multipart`）
/// 由调用方保证。
fn is_uncounted(headers: &HeaderMap) -> bool {
    parse_content_type(headers.get("Content-Type")).mime_type == "message/rfc822"
}

/// 把一个部件拆成「头 + 体」两半：空行之前是头，之后是体。
///
/// 对外公开是因为诊断工具（`xremail-diagnostics` 的 charset-probe）也要按同一规则切。
///
/// 没有空行时按「整段都是头」处理（与「整段都是体」相比，
/// 这样一封只有头的空正文邮件不会把 `Subject:` 当成正文）。
pub fn split_head_body(bytes: &[u8]) -> (HeaderMap, Vec<u8>) {
    // 空行可以是 CRLF CRLF 或 LF LF。
    let mut split = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let next = i + 1;
            if bytes[next..].starts_with(b"\r\n") {
                split = Some((i, next + 2));
                break;
            }
            if bytes[next..].starts_with(b"\n") {
                split = Some((i, next + 1));
                break;
            }
        }
        i += 1;
    }

    let (head, body) = match split {
        Some((head_end, body_start)) => (&bytes[..head_end], bytes[body_start..].to_vec()),
        None => (bytes, Vec::new()),
    };

    let text = String::from_utf8_lossy(head);
    let fields = Vec::new();
    (parse_header_fields(&text, fields), body)
}

/// 解析头字段：折行（以空白开头的续行）并入上一行。
fn parse_header_fields(text: &str, mut fields: Vec<(String, String)>) -> HeaderMap {
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // 续行：接回上一个字段（RFC 5322 §2.2.3 规定折叠处等价于一个空格）。
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        fields.push((name.trim().to_string(), value.trim().to_string()));
    }
    HeaderMap { fields }
}

/// 从 Content-Type / Content-Disposition 参数里取一个值（大小写不敏感）。
pub(crate) fn extract_param<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    for part in source.split(';').skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case(name) {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

/// 按 boundary 切开 multipart 体，返回每个子部件的「头 + 体」。
///
/// 全程按**字节**切（不能先转 `String`：正文常是 GBK，`from_utf8_lossy`
/// 会把非 ASCII 字节换成 U+FFFD，后面按 `charset` 解码就再也还原不了）。
fn split_multipart(body: &[u8], boundary: &str) -> Vec<(HeaderMap, Vec<u8>)> {
    let opening = format!("--{boundary}").into_bytes();
    let closing = format!("--{boundary}--").into_bytes();

    let mut parts = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    // 第一个分隔线之前是前言，丢掉。
    let mut seen_opening = false;
    let mut pos = 0;

    while pos <= body.len() {
        // 取一整行（含行尾的 `\r\n` 或 `\n`），`end` 指向下一个字节。
        let (line_end, next) = match body[pos..].iter().position(|&b| b == b'\n') {
            Some(index) => (pos + index, pos + index + 1),
            None => (body.len(), body.len() + 1),
        };
        let mut line = &body[pos..line_end];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }

        if line == opening.as_slice() || line == closing.as_slice() {
            if let Some(bytes) = current.take() {
                // 分隔线**前面**的换行属于分界线，不属于这一段的内容。
                parts.push(split_head_body(trim_final_newline(&bytes)));
            }
            seen_opening = true;
            if line == closing.as_slice() {
                break;
            }
            current = Some(Vec::new());
        } else if seen_opening && let Some(bytes) = current.as_mut() {
            bytes.extend_from_slice(&body[pos..line_end]);
            if next <= body.len() {
                bytes.push(b'\n');
            }
        }

        pos = next;
    }

    // 没有结束分隔线时，最后一段也要收下。
    if let Some(bytes) = current {
        parts.push(split_head_body(trim_final_newline(&bytes)));
    }

    parts
}

/// 去掉段尾那一处换行（它是分界线的行尾，不是内容）。
fn trim_final_newline(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes)
}

/// 把一个已解析好的部件收进 `Part`。
fn make_part(headers: &HeaderMap, body: &[u8], content_type: &ContentType) -> Part {
    let transfer_encoding = headers
        .get("Content-Transfer-Encoding")
        .map(|value| value.trim().to_ascii_lowercase());
    let content = crate::transfer::decode_transfer(body, transfer_encoding.as_deref());
    Part {
        headers: headers.clone(),
        content_type: content_type.clone(),
        transfer_encoding,
        filename: None,
        content,
    }
}
