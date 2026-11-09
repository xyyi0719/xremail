//! MIME **构建**：把收件人 / 标题 / 正文 / 附件拼成一份符合
//! RFC 5322 + RFC 2045 / 2046 / 2047 的邮件字节。
//!
//! ## 为什么单独一个模块
//!
//! 同 crate 里的 [`crate::mime`] 等模块解决的是「**读**邮件」，这个模块解决「**写**邮件」。
//! 两者的难点正好相反：解析要**容忍**各种不规范，构建要**严格**——
//! 生成的字节必须能被各家客户端正确解析，尤其是**绝不能被头部注入**。
//!
//! ## 安全
//!
//! 任何会进邮件头的字段（发件人、收件人、标题、附件名……）只要含 CR/LF，
//! 一律返回 [`ComposeError::HeaderInjection`]，**不产出任何字节**。
//! 少了这道检查，攻击者就能用标题伪造出额外的头字段（例如 `Bcc:`），
//! 把邮件抄送到别处——这是发信路径上最典型、也最致命的漏洞。
//!
//! ## 与 SMTP 的分工
//!
//! 本模块只负责**产出字节**与**算出 envelope**；
//! envelope 与字节怎么送出去是 SMTP 层的事，不在这里。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use encoding_rs::Encoding;
// 附件类型与领域模型共用**同一个定义**（见 xremail-core 的 model::SendAttachment）：
// 领域层与构建层各写一份的话，中间就得加一层无意义的转换。
pub use xremail_core::model::SendAttachment;

/// 附件。
///
/// 与领域模型 [`xremail_core::model::SendAttachment`] 是**同一个类型**：
/// 构建层与领域层共用一份定义，中间不需要一层无意义的转换。
/// 这里再给一个别名，是为了让「发信」语义下也能按 `OutgoingAttachment` 引用它。
pub type OutgoingAttachment = SendAttachment;

use crate::charset;
use crate::encoded_word::encode_header_value;
use crate::transfer::{BASE64_LINE_LEN, encode_base64_wrapped, encode_quoted_printable};

/// 全局自增序号。
///
/// 用来派生 **Message-ID** 与 **multipart 边界**：两者都要求「同一封邮件内唯一、
/// 不同邮件之间不重复」。这里用进程内自增计数器而不是随机数——
/// 一来保证单调不重复，二来让同一进程内的行为可复现（便于测试与排查）。
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// RFC 5322 §2.1：一条头字段行的硬上限（不含行尾 CRLF）。
///
/// 超过它就必须折行，否则部分 MTA 会直接拒收。
const MAX_HEADER_LINE: usize = 998;

/// 一份待发送邮件的构造输入。
#[derive(Debug, Clone, Default)]
pub struct OutgoingMessage {
    /// 发件人，形如 `显示名 <addr@x>` 或 `addr@x`。
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// 密送收件人：**只进 envelope，绝不写进邮件字节**。
    pub bcc: Vec<String>,
    pub subject: String,
    /// 纯文本正文（UTF-8 字符串）。
    pub body_text: Option<String>,
    /// HTML 正文（UTF-8 字符串）。
    pub body_html: Option<String>,
    pub attachments: Vec<OutgoingAttachment>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

/// 构建选项。
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// 服务器支持 `8BITMIME` 时为 true：正文可以原样 8 位传输。
    /// false 时正文必须编码成 7bit 安全（quoted-printable）。
    pub eight_bit: bool,
    /// 当前时间（epoch 秒），用于 `Date` 与 `Message-ID`。
    pub now_epoch_secs: i64,
    /// `Message-ID` 的域部分（如 `xremail.local`）。
    pub message_id_domain: String,
    /// 字符集标签，默认 `utf-8`。
    pub charset: String,
}

/// 构建结果。
#[derive(Debug, Clone)]
pub struct BuiltMessage {
    /// 完整邮件字节（头部 + 空行 + 正文），**CRLF** 行尾。
    pub bytes: Vec<u8>,
    /// 生成或传入的 `Message-ID`（含尖括号）。
    pub message_id: String,
    /// envelope 收件人（to + cc + bcc 去重、去空），**不含** from。
    pub envelope_recipients: Vec<String>,
}

/// 构建失败。
#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    /// 头部注入：某字段里含 CR/LF。
    #[error("字段 {field} 里含有换行，可能造成头部注入")]
    HeaderInjection { field: &'static str },
    #[error("没有收件人")]
    EmptyRecipient,
    #[error("缺少发件人")]
    MissingFrom,
    #[error("正文为空：body_text 与 body_html 至少给一个")]
    EmptyBody,
    #[error("地址不合法：{0}")]
    InvalidAddress(String),
}

/// 按 RFC 5322 / 2045-2047 构建一份待发送邮件。
///
/// 产出的字节结构：
///
/// * 没有附件时——单一部分（`text/plain` 或 `text/html`），
///   或 `multipart/alternative`（正文两种都有时）；
/// * 有附件时——最外层 `multipart/mixed`，里面装正文部分与各个附件。
///
/// 密送收件人只进 [`BuiltMessage::envelope_recipients`]，
/// **不会**出现在产出的字节里。
pub fn build_message(
    msg: &OutgoingMessage,
    opts: &BuildOptions,
) -> Result<BuiltMessage, ComposeError> {
    // ---- 1. 头部注入防护：必须最先做 ----
    // 不合法的输入一律不产出任何字节：宁可发不出去，也不能发出一份
    // 被塞了额外头字段的邮件。
    reject_injection("from", &msg.from)?;
    for address in &msg.to {
        reject_injection("to", address)?;
    }
    for address in &msg.cc {
        reject_injection("cc", address)?;
    }
    for address in &msg.bcc {
        reject_injection("bcc", address)?;
    }
    reject_injection("subject", &msg.subject)?;
    if let Some(value) = &msg.in_reply_to {
        reject_injection("in_reply_to", value)?;
    }
    if let Some(value) = &msg.references {
        reject_injection("references", value)?;
    }
    for attachment in &msg.attachments {
        reject_injection("filename", &attachment.filename)?;
        reject_injection("content_type", &attachment.content_type)?;
    }
    reject_injection("message_id_domain", &opts.message_id_domain)?;
    reject_injection("charset", &opts.charset)?;

    // ---- 2. 必填项 ----
    if msg.from.trim().is_empty() {
        return Err(ComposeError::MissingFrom);
    }
    if msg.body_text.is_none() && msg.body_html.is_none() {
        return Err(ComposeError::EmptyBody);
    }

    // ---- 3. 地址解析 ----
    let from = parse_address(&msg.from)?;
    let to = parse_addresses(&msg.to)?;
    let cc = parse_addresses(&msg.cc)?;
    let bcc = parse_addresses(&msg.bcc)?;

    // envelope = to + cc + bcc，去重、去空。
    // 注意这里用**裸地址**（不含显示名）：SMTP envelope 里只有 addr-spec。
    let mut seen: HashSet<&str> = HashSet::new();
    let mut envelope_recipients: Vec<String> = Vec::new();
    for address in to.iter().chain(cc.iter()).chain(bcc.iter()) {
        if seen.insert(address.addr.as_str()) {
            envelope_recipients.push(address.addr.clone());
        }
    }
    if envelope_recipients.is_empty() {
        return Err(ComposeError::EmptyRecipient);
    }

    // ---- 4. 正文与附件结构 ----
    let (charset_label, encoding) = resolve_charset(&opts.charset);
    let body = build_body_node(msg, &charset_label, encoding, opts.eight_bit);
    let attachments = build_attachment_nodes(&msg.attachments);

    let root = if attachments.is_empty() {
        body
    } else {
        // 有附件 → 最外层 multipart/mixed，正文部分放第一个。
        Node::Container {
            subtype: "mixed",
            boundary: next_boundary(),
            children: std::iter::once(body).chain(attachments).collect(),
        }
    };

    // ---- 5. 拼字节 ----
    let domain = sanitize_domain(&opts.message_id_domain);
    let message_id = next_message_id(&domain, opts.now_epoch_secs);

    let mut out: Vec<u8> = Vec::new();
    write_header(&mut out, "From", &format_address(&from, &charset_label));
    if !to.is_empty() {
        write_header(&mut out, "To", &format_address_list(&to, &charset_label));
    }
    if !cc.is_empty() {
        write_header(&mut out, "Cc", &format_address_list(&cc, &charset_label));
    }
    write_header(
        &mut out,
        "Subject",
        &encode_header_value(&msg.subject, &charset_label),
    );
    write_header(&mut out, "Date", &format_date(opts.now_epoch_secs));
    write_header(&mut out, "Message-ID", &message_id);
    if let Some(value) = &msg.in_reply_to {
        write_header(&mut out, "In-Reply-To", &sanitize_msg_id(value));
    }
    if let Some(value) = &msg.references {
        write_header(&mut out, "References", &sanitize_msg_id(value));
    }
    write_header(&mut out, "MIME-Version", "1.0");
    render_part(&mut out, &root);

    Ok(BuiltMessage {
        bytes: out,
        message_id,
        envelope_recipients,
    })
}

// ---------------------------------------------------------------------------
// 地址
// ---------------------------------------------------------------------------

/// 内部：解析后的地址。
#[derive(Debug, Clone)]
struct Address {
    /// 显示名（已剥掉外层引号）。
    display: Option<String>,
    /// 裸地址（addr-spec）。
    addr: String,
}

/// 解析一个地址：接受 `显示名 <addr@x>` 与 `addr@x` 两种写法。
fn parse_address(raw: &str) -> Result<Address, ComposeError> {
    let trimmed = raw.trim();
    if let Some(open) = trimmed.rfind('<')
        && trimmed.ends_with('>')
    {
        let display = unquote(trimmed[..open].trim());
        let addr = validate_addr(trimmed[open + 1..trimmed.len() - 1].trim())?;
        return Ok(Address {
            display: if display.is_empty() {
                None
            } else {
                Some(display)
            },
            addr,
        });
    }

    Ok(Address {
        display: None,
        addr: validate_addr(trimmed)?,
    })
}

/// 解析一组地址（空串直接跳过）。
fn parse_addresses(raw: &[String]) -> Result<Vec<Address>, ComposeError> {
    raw.iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_address(value))
        .collect()
}

/// 校验裸地址。
///
/// 只做**基本的**合法性检查：不能为空、不能含空白或控制字符、
/// 必须有且只有一个 `@` 且两侧都非空。这样能挡住明显的垃圾输入，
/// 又不会因为少见但合法的写法（引号本地部分等）误伤。
fn validate_addr(addr: &str) -> Result<String, ComposeError> {
    let invalid = || ComposeError::InvalidAddress(addr.to_string());
    if addr.is_empty() {
        return Err(invalid());
    }
    if addr.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(invalid());
    }
    if addr.contains('<') || addr.contains('>') {
        return Err(invalid());
    }
    let (local, domain) = addr.split_once('@').ok_or_else(invalid)?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(invalid());
    }
    Ok(addr.to_string())
}

/// 剥掉显示名外层的引号，并还原被转义的引号与反斜杠。
fn unquote(display: &str) -> String {
    if display.len() >= 2 && display.starts_with('"') && display.ends_with('"') {
        let inner = &display[1..display.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some(next) => out.push(next),
                    None => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        display.to_string()
    }
}

/// 把地址渲染成头字段里的样子。
///
/// 显示名含非 ASCII 时用 RFC 2047 编码字——**不能**放进引号里，
/// 因为 RFC 2047 的编码字不允许出现在 quoted-string 中。
fn format_address(address: &Address, charset_label: &str) -> String {
    let Some(display) = &address.display else {
        return address.addr.clone();
    };

    if display.is_ascii() {
        if needs_quoting(display) {
            format!("\"{}\" <{}>", escape_quotes(display), address.addr)
        } else {
            format!("{} <{}>", display, address.addr)
        }
    } else {
        format!(
            "{} <{}>",
            encode_header_value(display, charset_label),
            address.addr
        )
    }
}

/// 渲染一组地址（用 `, ` 分隔）。
fn format_address_list(addresses: &[Address], charset_label: &str) -> String {
    addresses
        .iter()
        .map(|address| format_address(address, charset_label))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 显示名里出现这些字符时必须加引号（RFC 5322 §3.4）。
fn needs_quoting(display: &str) -> bool {
    display.starts_with(' ') || display.ends_with(' ') || {
        display.chars().any(|ch| {
            matches!(
                ch,
                '(' | ')' | '<' | '>' | '@' | ',' | ';' | ':' | '\\' | '"' | '.' | '[' | ']'
            )
        })
    }
}

/// 引号串内部的转义。
fn escape_quotes(display: &str) -> String {
    display.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// 正文与附件
// ---------------------------------------------------------------------------

/// 内部：正文树的一个节点。
#[derive(Debug)]
enum Node {
    /// 叶子：正文或附件。
    Leaf {
        /// 完整 `Content-Type` 值（含参数）。
        content_type: String,
        /// 传输编码。
        transfer: Transfer,
        /// 传输编码后的字节（**不含**结尾 CRLF）。
        payload: Vec<u8>,
        /// `Content-Disposition` 值（附件才有）。
        disposition: Option<String>,
    },
    /// 容器：`multipart/*`。
    Container {
        /// 子类型（`mixed` / `alternative`）。
        subtype: &'static str,
        /// 分隔符。
        boundary: String,
        /// 子节点。
        children: Vec<Node>,
    },
}

/// 内部：传输编码。
#[derive(Debug, Clone, Copy)]
enum Transfer {
    /// quoted-printable（7bit 安全）。
    QuotedPrintable,
    /// 8bit（原样字节）。
    EightBit,
    /// base64（附件）。
    Base64,
}

impl Transfer {
    /// 传输编码对应的头字段值。
    fn header_value(self) -> &'static str {
        match self {
            Transfer::QuotedPrintable => "quoted-printable",
            Transfer::EightBit => "8bit",
            Transfer::Base64 => "base64",
        }
    }
}

/// 构造正文节点。
///
/// * 只有纯文本 → `text/plain`；
/// * 只有 HTML → `text/html`；
/// * 两种都有 → `multipart/alternative`，**纯文本在前**（RFC 2046 §5.1.4：
///   从最纯的格式排到最花哨的格式，接收方自上而下取第一个能显示的）。
fn build_body_node(
    msg: &OutgoingMessage,
    charset_label: &str,
    encoding: Option<&'static Encoding>,
    eight_bit: bool,
) -> Node {
    let mut alternatives: Vec<Node> = Vec::new();

    if let Some(text) = &msg.body_text {
        let (transfer, payload) = encode_body_payload(text, encoding, eight_bit);
        alternatives.push(Node::Leaf {
            content_type: format!("text/plain; charset={charset_label}"),
            transfer,
            payload,
            disposition: None,
        });
    }
    if let Some(html) = &msg.body_html {
        let (transfer, payload) = encode_body_payload(html, encoding, eight_bit);
        alternatives.push(Node::Leaf {
            content_type: format!("text/html; charset={charset_label}"),
            transfer,
            payload,
            disposition: None,
        });
    }

    if alternatives.len() == 1 {
        // 单一部分：去掉唯一的元素（此时必然存在）。
        alternatives.remove(0)
    } else {
        Node::Container {
            subtype: "alternative",
            boundary: next_boundary(),
            children: alternatives,
        }
    }
}

/// 构造附件节点。
///
/// 附件**一律 base64**：二进制内容没有「7bit 安全」的选项，
/// 而且各家客户端对附件的 base64 支持度最好。
fn build_attachment_nodes(attachments: &[OutgoingAttachment]) -> Vec<Node> {
    attachments
        .iter()
        .map(|attachment| {
            let content_type = normalize_content_type(&attachment.content_type);
            Node::Leaf {
                content_type,
                transfer: Transfer::Base64,
                payload: encode_base64_wrapped(&attachment.bytes, BASE64_LINE_LEN).into_bytes(),
                disposition: Some(format_disposition(&attachment.filename)),
            }
        })
        .collect()
}

/// 正文编码：先按字符集转成字节并统一行尾，再按需要做传输编码。
fn encode_body_payload(
    text: &str,
    encoding: Option<&'static Encoding>,
    eight_bit: bool,
) -> (Transfer, Vec<u8>) {
    let raw = match encoding {
        Some(enc) => enc.encode(text).0.into_owned(),
        None => text.as_bytes().to_vec(),
    };
    // RFC 5322 §2.1 要求行尾一律 CRLF；正文里混着裸 LF 会让部分客户端解析错乱。
    let normalized = normalize_crlf(&raw);

    if eight_bit {
        (Transfer::EightBit, normalized)
    } else {
        // 服务器不支持 8BITMIME 时必须走 quoted-printable——
        // 直接塞 8 位字节，有些 MTA 会按 7bit 截断，把正文弄乱码。
        (
            Transfer::QuotedPrintable,
            encode_quoted_printable(&normalized),
        )
    }
}

/// 把裸 LF / 裸 CR 统一成 CRLF。
fn normalize_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                out.extend_from_slice(b"\r\n");
                index += if bytes.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            }
            b'\n' => {
                out.extend_from_slice(b"\r\n");
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

/// 附件的 `Content-Type`：空或非 ASCII 时退回 `application/octet-stream`。
fn normalize_content_type(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.is_ascii() {
        return "application/octet-stream".to_string();
    }
    trimmed.to_string()
}

/// 附件的 `Content-Disposition`。
///
/// 文件名是 ASCII 时直接写 `filename="..."`；
/// 含非 ASCII 时额外给一个 RFC 2231 的 `filename*=utf-8''...`
/// ——旧客户端读前者，新客户端读后者。
fn format_disposition(filename: &str) -> String {
    let fallback = sanitize_filename(filename);
    if filename.is_ascii() {
        format!("attachment; filename=\"{fallback}\"")
    } else {
        format!(
            "attachment; filename=\"{fallback}\"; filename*=utf-8''{}",
            percent_encode(filename)
        )
    }
}

/// 文件名的 ASCII 兜底：把引号、反斜杠与非 ASCII 字符换成 `_`。
fn sanitize_filename(filename: &str) -> String {
    filename
        .chars()
        .map(|ch| {
            if ch == '"' || ch == '\\' || !ch.is_ascii() {
                '_'
            } else {
                ch
            }
        })
        .collect()
}

/// RFC 2231 的百分号编码。
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '~') {
            out.push(ch);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 序列化
// ---------------------------------------------------------------------------

/// 渲染一个部分（头字段 + 空行 + 内容）。
fn render_part(out: &mut Vec<u8>, node: &Node) {
    match node {
        Node::Leaf {
            content_type,
            transfer,
            payload,
            disposition,
        } => {
            write_header(out, "Content-Type", content_type);
            write_header(out, "Content-Transfer-Encoding", transfer.header_value());
            if let Some(disposition) = disposition {
                write_header(out, "Content-Disposition", disposition);
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(trim_one_trailing_crlf(payload));
            out.extend_from_slice(b"\r\n");
        }
        Node::Container {
            subtype,
            boundary,
            children,
        } => {
            write_header(
                out,
                "Content-Type",
                &format!("multipart/{subtype}; boundary=\"{boundary}\""),
            );
            out.extend_from_slice(b"\r\n");
            for child in children {
                out.extend_from_slice(b"--");
                out.extend_from_slice(boundary.as_bytes());
                out.extend_from_slice(b"\r\n");
                render_part(out, child);
            }
            out.extend_from_slice(b"--");
            out.extend_from_slice(boundary.as_bytes());
            out.extend_from_slice(b"--\r\n");
        }
    }
}

/// 去掉载荷结尾的**一个** CRLF。
///
/// 渲染时统一补一个 CRLF，这样每个部分都以 CRLF 结束（RFC 5322 §2.1），
/// 又不会多出空行。只去一个：正文里本来就有的空行要保留。
fn trim_one_trailing_crlf(payload: &[u8]) -> &[u8] {
    if payload.ends_with(b"\r\n") {
        &payload[..payload.len() - 2]
    } else {
        payload
    }
}

/// 写一个头字段。
///
/// 只有超过 RFC 5322 §2.1 的 998 字节硬上限时才折行；
/// 正常长度的标题不会被折，接收端拿到的仍是原样的一段。
fn write_header(out: &mut Vec<u8>, name: &str, value: &str) {
    let limit = MAX_HEADER_LINE.saturating_sub(name.len() + 2);
    let lines = fold_value(value, limit);
    for (index, line) in lines.iter().enumerate() {
        if index == 0 {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
        } else {
            // 续行以一个空格开头：折行处的空白折叠回来正好是一个空格。
            out.push(b' ');
        }
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
}

/// 把一个头字段值折成若干行，每行不超过 `limit` 个字符。
///
/// ## 两条硬要求
///
/// 1. **任何一行都不能是空的，也不能「只有空白」**：接收端把「空行」当作邮件头的
///    结束、正文的开始（RFC 5322 §2.1），把「续行只有一个空格」也一样折叠成空行。
///    折出那样一行，后面的 `To:` / `Date:` / `Content-Type:` 就全部变成正文。
///    这里曾经按**单个空格**切词，一段比 `limit` 还长的连续空白会折出一整行空格（R62）。
/// 2. **任何一行都不能超出预算**：RFC 5322 §2.1.1 的硬上限是 998 字节。
///    这里曾经只按空格切词，一个比限制还长的「词」会被原样写成一整行。
///
/// 做法是逐行贪婪地切：
///
/// * 一行能放 `content_width` 个字符（`limit` 扣掉续行前面那个前导空格）；
/// * 优先切在**第一个**「后面是空白、且空白之后还有内容」的位置，把那段空白留在
///   本行末尾——接收端折叠折行处的 FWS 后正好还原成原来那一个空格。
///
///   取**第一个**而不是最后一个：前端渲染出来的一长串空格（原来的换行被压成空格）
///   如果全留在行尾，整行就被填满，一个很长的空白段又会撞出「整行刚好用光」
///   的形态（见下一条）；
/// * **任何一行都不能用光预算**：接收端会把行尾空白 trim 掉，某个续行剩下的
///   内容如果恰好紧贴在下一行，trim + 折叠前导空格之后就成了空行。
///   所以「整段纯空白」每行只放 `content_width - 1` 个空格。
///
///   **已知残余（R62）**：`limit` 是逐行变化的（第一行要扣掉 `Subject: `，
///   续行只扣一个前导空格），而 `fold_value` 拿不到「本行的实际前缀」，
///   主题里出现几百个连续空格时仍可能折出「只有空白」的一行。
///   见 `tests/decoding_regressions.rs` 里钉住现状的那条用例；
/// * 没有可切的空白就在字符边界上硬切（**不能**按字节切：切在汉字中间，
///   单独看是合法 UTF-8，但接收端按字节拼回来就成了乱码）。
fn fold_value(value: &str, limit: usize) -> Vec<String> {
    // 一行**总共**能放多少字符（含续行前面那个前导空格）。
    let row_width = limit.max(2);
    // 去掉前导空格之后还能放多少内容。
    let content_width = row_width - 1;
    // 纯空白段再少放一个（理由见函数文档最后一条）。
    let space_width = content_width.saturating_sub(1).max(1);

    let mut lines: Vec<String> = Vec::new();
    let mut rest = value;

    while !rest.is_empty() {
        // 本行能放下的前缀：**字符数**不超过 `content_width`，返回字节长度。
        // 用 `take_chars` 而不是切片下标，是因为 `limit` 的口径一直是字符数，
        // 而 `str` 的下标是字节——两者只在纯 ASCII 时才一致。
        let prefix_bytes = take_chars(rest, content_width);
        if prefix_bytes == rest.len() {
            lines.push(rest.to_string());
            break;
        }

        // 首选切点：前缀里**第一个**「后面是空白、空白之后还有内容」的位置，
        // 切点落在空白之前——那段空白留在本行末尾，接收端折叠折行处的 FWS 后
        // 正好还原成原来那一个空格。
        //
        // 从下标 1 开始找：0 处切一刀会让「空格 + 一长串内容」在每一个空格处都
        // 切一刀，普通标题被切成碎条。`rest` 以空白开头时交给下面的纯空白分支，
        // 它会按宽度一次切走一大段，不会退化成一次一个字符。
        let preferred = rest[..prefix_bytes]
            .char_indices()
            .map(|(byte, _)| byte)
            .skip(1)
            .find(|byte| rest[*byte..].starts_with(' ') && !rest[*byte..].trim_start().is_empty());

        // 以空白开头、又切不动（后面全是空白）：把开头的空白单独切成一行。
        //
        // `rest` 此时**全是空格**——按「内容宽度」切一定会把行填满，
        // 续行剩下的那点空格在接收端被 trim 掉、再折叠前导空格之后就是空行
        // （R62 的第二种形态）。所以每行只放 `space_width` 个。
        //
        // 注意只在「全是空白」时这样切：`rest` 是「空格 + 一长串内容」时，
        // 空白后面紧接着会走硬切，行是填满的，不会剩下孤立的一行。
        let only_spaces = rest.chars().all(|ch| ch == ' ' || ch == '\t');
        let cut = match preferred {
            Some(byte) => byte,
            None if rest.starts_with(' ') && only_spaces => take_chars(rest, space_width),
            None if rest.starts_with(' ') => take_chars(rest, content_width),
            // 一个比整行还长的「词」：在字符边界上硬切。
            None => prefix_bytes,
        };

        lines.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 取 `text` 开头最多 `count` 个字符的**字节长度**。
///
/// `count` 为 0 时返回 0：调用方自己保证「至少切一个字符」，免得这里悄悄多切一个
/// 字符、把行宽预算撑破（曾经就是这么错的）。
fn take_chars(text: &str, count: usize) -> usize {
    text.char_indices()
        .take(count)
        .map(|(byte, ch)| byte + ch.len_utf8())
        .last()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 杂项
// ---------------------------------------------------------------------------

/// 检查一个会进邮件头的字段里有没有 CR/LF。
///
/// CR/LF 是**头部注入**的载体：只要有一个字段带了换行，攻击者就能伪造出
/// 额外的头字段（例如 `Bcc:`），把邮件抄送到别处。
/// 所以这里一律拒绝，而不是「转义后继续发」。
fn reject_injection(field: &'static str, value: &str) -> Result<(), ComposeError> {
    if value.contains('\r') || value.contains('\n') {
        return Err(ComposeError::HeaderInjection { field });
    }
    Ok(())
}

/// 解析字符集标签。
///
/// 标签不认识或者不是 ASCII 时退回 `utf-8`——宁可声明成 UTF-8
/// （客户端基本都认），也不能声明一个客户端不认识的字符集。
fn resolve_charset(label: &str) -> (String, Option<&'static Encoding>) {
    let normalized = charset::normalize_label(label);
    if normalized.is_empty() || !normalized.is_ascii() {
        let fallback = "utf-8".to_string();
        return (fallback.clone(), charset::encoding_for_label(&fallback));
    }
    match charset::encoding_for_label(&normalized) {
        Some(encoding) => (normalized, Some(encoding)),
        None => {
            let fallback = "utf-8".to_string();
            (fallback.clone(), charset::encoding_for_label(&fallback))
        }
    }
}

/// 清洗 `Message-ID` 的域部分。
fn sanitize_domain(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_graphic() && !matches!(ch, '<' | '>' | '@' | ' ') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "xremail.local".to_string()
    } else {
        cleaned
    }
}

/// `Message-ID` 里的 msg-id 必须是 ASCII；非 ASCII 字节替换成 `?`。
fn sanitize_msg_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii() { ch } else { '?' })
        .collect()
}

/// 下一个 multipart 边界。
///
/// RFC 2046 §5.1.1 允许的边界字符里没有 `=`（它是参数分隔符），
/// 这里只用字母、数字与下划线。
fn next_boundary() -> String {
    format!("xremail_{}_{}", std::process::id(), next_sequence())
}

/// 下一个 `Message-ID`。
///
/// 用**时间戳 + 进程内序号**而不是随机数：时间戳保证跨进程也不容易撞，
/// 序号保证同一进程内连续两次构建一定不同。
fn next_message_id(domain: &str, now_epoch_secs: i64) -> String {
    format!("<{now_epoch_secs}.{}@{domain}>", next_sequence())
}

/// 取一个自增序号。
fn next_sequence() -> u64 {
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// 日期
// ---------------------------------------------------------------------------

/// 星期几（索引 0 = 周一）。
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// 月份（索引 0 = 一月）。
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// 把 epoch 秒格式化成 RFC 5322 的 `Date` 值。
///
/// 例：`Tue, 28 Oct 2026 02:30:19 +0000`。
///
/// 这里**统一用 UTC**：客户端会按收件人所在时区显示，自己算时区要引入
/// 地区数据库，既重又容易出错；发信端给 UTC 是完全合规的做法。
pub fn format_date(epoch_secs: i64) -> String {
    let (year, month, day, hour, minute, second, weekday) = civil_from_epoch(epoch_secs);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        WEEKDAYS[weekday],
        day,
        MONTHS[(month - 1) as usize],
        year,
        hour,
        minute,
        second
    )
}

/// 把 epoch 秒换算成 UTC 的公历日期时间。
///
/// 用的是 Howard Hinnant 的 `civil_from_days` 算法：
/// 先把 epoch 秒拆成「天数 + 当天秒数」，再用纯整数运算把天数换成年月日。
/// 这样不依赖 chrono / time 之类的库，跨平台结果也完全一致。
fn civil_from_epoch(epoch_secs: i64) -> (i64, u32, u32, u32, u32, u32, usize) {
    const SECONDS_PER_DAY: i64 = 86_400;

    let days = epoch_secs.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = epoch_secs.rem_euclid(SECONDS_PER_DAY);

    let hour = (seconds_of_day / 3600) as u32;
    let minute = ((seconds_of_day % 3600) / 60) as u32;
    let second = (seconds_of_day % 60) as u32;

    // 以 0000-03-01 为基准的「以 400 年为周期」的换算。
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    if month <= 2 {
        year += 1;
    }

    // 1970-01-01 是周四：索引 0 = 周一，所以偏移 3。
    let weekday = (days + 3).rem_euclid(7) as usize;

    (year, month, day, hour, minute, second, weekday)
}
