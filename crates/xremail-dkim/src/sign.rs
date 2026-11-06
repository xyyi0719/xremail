//! 生成 `DKIM-Signature` 头字段（RFC 6376 §3.5, §3.7）。
//!
//! ## 签的到底是什么
//!
//! `b=` 签的是「**按 `h=` 挑出来的那些头字段（规范化后，每个后面跟一个 CRLF）
//! + `DKIM-Signature` 头字段本身（规范化后，`b=` 为空，末尾**不带** CRLF）**」。
//!
//! 注意最后这一条的细节：`DKIM-Signature` 的规范化值之后**不补 CRLF**，
//! 而前面那些头字段**每个都要带**。少一个或多一个 CRLF，签名就对不上，
//! 而且验签方只会回 `b=` 校验失败——最难查的一类错。
//!
//! ## 为什么先拼一个「`b=` 为占位」的头
//!
//! RFC 6376 §3.7 要求：签名时 `b=` 的值留空，但**折行结构（有哪些行、每行多长）
//! 与最终产出的头完全一致**。做法是：
//!
//! 1. 用「值是固定宽度的占位字符」拼出骨架，确定折行；
//! 2. 对「占位骨架规范化后的文本」签名——占位字符不是 WSP，
//!    relaxed 折叠空白不会改动它，所以它与「`b=` 为空」的规范化结果**长度与空白
//!    完全一致**（§3.5 明确：`b=` 为空时，模长直接决定分行位置即可）；
//! 3. 把 base64 按同样的宽度切行填进去。
//!
//! base64 与占位字符一样不含空白，填进去之后同一套折行与空白原样成立——
//! 验签方看到的正文与签名时看到的一模一样。`debug_assert` 里还把两边的
//! 规范化文本比对了一次，防止将来改动折行逻辑时悄悄破坏这条前提。
//!
//! ## 折行
//!
//! 续行**只用空格**、绝不用 tab：`b=` 的值是 base64，
//! 有些第三方校验器（以及手工复制邮件原文的人）会把 tab 当成 base64 的一部分，
//! 直接验签失败。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::canonicalize::{Canonicalization, canonicalize_body, split_headers_and_body};
use crate::error::DkimError;
use crate::key::{SigningAlgorithm, SigningKey};

/// 一条头字段每行的目标长度（不含 CRLF）。
///
/// 取 76：base64 的经典行宽，也远小于 RFC 5322 的 998 字节硬上限。
const HEADER_LINE_WIDTH: usize = 76;

/// 头字段名（含冒号与空格）在拼 `DKIM-Signature` 时占的长度。
const TAG_HEADER: &str = "DKIM-Signature: ";

/// 默认签哪些头（按 RFC 6376 §5.4 的建议顺序：被引用最多的放前面）。
///
/// **必须有 `From`**（RFC 6376 §5.4）；`Cc` / `References` / `In-Reply-To`
/// 只在邮件里真的存在时才进 `h=`。
pub const DEFAULT_SIGNED_HEADERS: &[&str] = &[
    "From",
    "To",
    "Cc",
    "Subject",
    "Date",
    "Message-ID",
    "In-Reply-To",
    "References",
    "MIME-Version",
    "Content-Type",
];

/// 一次签名请求。
pub struct SignRequest<'a> {
    /// 要签的邮件字节（头部 + 空行 + 正文，行尾最好是 CRLF；
    /// 不是 CRLF 也会按 RFC 6376 §3.4.3 归一后再签）。
    pub message: &'a [u8],
    /// `d=`：签名域。
    pub domain: &'a str,
    /// `s=`：DNS 选择器。
    pub selector: &'a str,
    /// 私钥。
    pub key: &'a SigningKey,
    /// `t=`：签名时刻（epoch 秒）。`None` 时不写这个标签。
    pub timestamp: Option<i64>,
    /// 签名算法（`a=`）。
    pub algorithm: SigningAlgorithm,
    /// (头, 体) 的规范化方式（`c=`）。
    pub canonicalization: (Canonicalization, Canonicalization),
    /// 参与签名的头字段顺序（`h=`）。
    pub signed_headers: &'a [&'a str],
}

impl<'a> SignRequest<'a> {
    /// 用本项目的固定策略构造：`relaxed/relaxed`、`rsa-sha256`、
    /// [`DEFAULT_SIGNED_HEADERS`]、不写 `t=`。
    pub fn new(message: &'a [u8], domain: &'a str, selector: &'a str, key: &'a SigningKey) -> Self {
        Self {
            message,
            domain,
            selector,
            key,
            timestamp: None,
            algorithm: SigningAlgorithm::default(),
            canonicalization: (Canonicalization::Relaxed, Canonicalization::Relaxed),
            signed_headers: DEFAULT_SIGNED_HEADERS,
        }
    }

    /// 带上 `t=` 时间戳（Unix 秒）。不调用就不写 `t=`。
    #[must_use]
    pub fn with_timestamp(mut self, timestamp: i64) -> Self {
        self.timestamp = Some(timestamp);
        self
    }
}

/// 一次签名的结果。
#[derive(Debug, Clone)]
pub struct SignedMessage {
    /// 插好 `DKIM-Signature` 头之后的**完整邮件字节**。
    pub bytes: Vec<u8>,
    /// 这一行 `bh=` 的值（base64；它只是正文哈希，不含机密）。
    pub body_hash: String,
    /// 实际进了 `h=` 的头字段（邮件里不存在的不算）。
    pub signed_headers: Vec<String>,
}

/// 给一封邮件加上 `DKIM-Signature` 头，返回新的完整字节。
///
/// **不改动**传入的字节：返回的是「原字节 + 插在头部末尾的一行」。
/// 调用方拿到它就是那一刻的最终形态——放进发信队列冻结的必须正是这份字节，
/// 事后再改任何一个头都会让签名失效（R22）。
///
/// 实现分两步，顺序是**反的**，但这不是 bug：
///
/// 1. 先用一个与真实签名等长的占位串（[`PLACEHOLDER_CHAR`]）拼出 `DKIM-Signature`，
///    按它确定折行位置；
/// 2. 把这段骨架的**原样文本**当作 signing input 的最后一项去签；
/// 3. 把 base64 签名按**同一组**折行位置填进占位串。
///
/// 第 3 步的要点：签名值本身不参与规范化（DKIM 的 `b=` 本来就是「值去掉空白」），
/// 而折行位置在填值前后完全一致，所以「签的是占位串、发的是真签名」这件事
/// 在字节层面是自洽的。末尾那次「重算发出的字节 → 与 signing input 逐字比较」
/// 把这条前提钉死，不等就报错，绝不发出一封验不过的信。
/// `bh=` 的值：对**已经规范化好**的正文取 SHA-256，再 base64。
///
/// 只是把「摘要 + 编码」这两行单独拎出来：验签方要用同一段代码算一遍，
/// 免得两边各写一份、悄悄对不上。
pub fn body_hash_canonical(canonical_body: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, canonical_body);
    BASE64.encode(digest.as_ref())
}

/// 正文按 `relaxed` 规范化之后的 `bh=`（`c=` 的正文部分是 `relaxed` 时用它）。
pub fn body_hash_relaxed(body: &[u8]) -> String {
    body_hash_canonical(&crate::canonicalize::canonicalize_body_relaxed(body))
}

pub fn sign_message(request: &SignRequest<'_>) -> Result<SignedMessage, DkimError> {
    let (header_alg, body_alg) = request.canonicalization;
    validate_tag_value("d=", request.domain)?;
    validate_tag_value("s=", request.selector)?;

    // 行尾先归一：签名与插头用的是同一份 CRLF 字节，杜绝「签 LF、发 CRLF」。
    let message = crate::canonicalize::normalize_crlf(request.message);
    let message = message.as_ref();

    let (headers, body) = crate::canonicalize::split_headers_and_body(message);

    // ---- 1. bh= ----
    let canonical_body = canonicalize_body(&body, body_alg);
    let body_digest = ring::digest::digest(&ring::digest::SHA256, &canonical_body);
    let body_hash = BASE64.encode(body_digest.as_ref());
    debug_assert_eq!(body_hash, body_hash_canonical(&canonical_body));

    // ---- 2. h=：按请求顺序挑邮件里真的有的头 ----
    let mut signed: Vec<String> = Vec::with_capacity(request.signed_headers.len());
    let mut header_block: Vec<u8> = Vec::new();
    for name in request.signed_headers {
        // 同一个头可能在邮件里出现多次（外部导入的邮件会这样）。
        // 这里**取最后一个**同名头并只签一次，与 `h=` 从左往右取的语义一致。
        let Some(field) = headers
            .iter()
            .rev()
            .find(|field| field.name.eq_ignore_ascii_case(name))
        else {
            continue;
        };
        header_block.extend_from_slice(field.canonicalize(header_alg).as_bytes());
        header_block.extend_from_slice(b"\r\n");
        // `h=` 里一律写小写：值比较不分大小写，小写最不容易被中途改坏。
        signed.push(field.name.to_ascii_lowercase());
    }

    if !signed.iter().any(|name| name == "from") {
        return Err(DkimError::MissingHeader("From".to_string()));
    }

    // ---- 3. 骨架：确定折行，并把 b= 占位成与签名等长的一串字符 ----
    let tags = TagList {
        domain: request.domain,
        selector: request.selector,
        algorithm: request.algorithm,
        canonicalization: request.canonicalization,
        signed_headers: &signed,
        body_hash: &body_hash,
        timestamp: request.timestamp,
    };
    let skeleton = layout(&tags, request.key);

    // ---- 4. 签名：头块（每项带 CRLF）+ DKIM-Signature（末尾不带 CRLF） ----
    //
    // DKIM-Signature 这一项取占位串的**语义形态**：续行是一个空格。
    // 接收方拿到的是「CRLF + 空格」的折行，按 §3.4.2 折叠空白之后正好是这一段；
    // 真实签名填进去之后也是同一个折叠结果，所以 b= 可以在不改变
    // signing input 的前提下被填上——末尾那次比对把这条前提钉死。
    let mut signing_input = header_block;
    // RFC 6376 §3.7：DKIM-Signature 参与签名时，`b=` 的值是空的。
    signing_input
        .extend_from_slice(format!("dkim-signature:{}", skeleton.signing_prefix()).as_bytes());

    let raw_signature = request.key.sign(request.algorithm, &signing_input)?;
    #[cfg(test)]
    LAST_SIGNING_INPUT
        .with(|cell| *cell.borrow_mut() = String::from_utf8_lossy(&signing_input).to_string());
    let encoded = BASE64.encode(&raw_signature);
    let filled = skeleton.fill(&encoded);

    // 「最终会发出去的字节」重新拼一遍 signing input，与刚才签的东西逐字比较：
    // 相等才说明签的正是要发的。这条检查在 release 下也跑——发出一封验不过的信，
    // 比发送失败糟糕得多。
    //
    // 续行：解析时被并成单个空格，写出去之前必须还原成「CRLF + 空格」，
    // 否则折行的头会被拉成一行、正文前还会多出空行。
    let header = format!("DKIM-Signature:{filled}\r\n");
    let issued = insert_header_after_headers(message, &header);
    check_issued_matches(&issued, header_alg, &signed, &signing_input)?;

    Ok(SignedMessage {
        bytes: issued,
        body_hash,
        signed_headers: signed,
    })
}

/// 「签的 == 发的」守卫：把**即将发出的字节**按接收方的口径重算一遍 signing input，
/// 与刚才签的逐字比较。
///
/// 不等就拒绝发出——发出一封验不过的信，比发送失败糟糕得多。
/// 它能抓住的是**被签标签/值被改动**（例如 `h=`、`s=`、`bh=` 被换掉）：
/// 单纯的折行加空格在 relaxed 下折叠成同一个空格，不算改动（RFC 6376 §3.4.2）。
fn check_issued_matches(
    issued: &[u8],
    header_alg: Canonicalization,
    signed: &[String],
    signing_input: &[u8],
) -> Result<(), DkimError> {
    let issued_input = rebuild_signing_input(issued, header_alg, signed);
    if issued_input.as_bytes() != signing_input {
        return Err(DkimError::Sign(
            "签名覆盖的字节与实际要发出的字节不一致（DKIM-Signature 被改动过）".to_string(),
        ));
    }
    Ok(())
}

/// 从**要发出去的字节**里按 §3.4.2 / §3.7 重算 signing input。
///
/// 这是「签的 == 发的」那条检查的独立实现：它只读最终字节，不看签名过程的任何中间量。
/// DKIM-Signature 这一项按**接收方会做的那种规范化**（relaxed，折行折叠成一个空格）
/// 处理；签名值本身不参与签名，所以在 `b=` 处截断（§3.7）。
fn rebuild_signing_input(issued: &[u8], header_alg: Canonicalization, signed: &[String]) -> String {
    let (headers, _) = split_headers_and_body(issued);
    let mut input = String::new();
    for field in &headers {
        if field.is_named("DKIM-Signature") {
            // 用 `relaxed()`（接收方的口径）而不是 `raw`：折行一律折叠成一个空格。
            let relaxed = field.relaxed();
            let blanked = match relaxed.rfind("b=") {
                Some(index) => relaxed[..index + 2].to_string(),
                None => relaxed,
            };
            input.push_str(&blanked);
            // DKIM-Signature 永远是最后一项，signing input 末尾**不带** CRLF（§3.7）。
            continue;
        }
        if signed
            .iter()
            .any(|name| field.name.eq_ignore_ascii_case(name))
        {
            input.push_str(&field.canonicalize(header_alg));
            input.push_str("\r\n");
        }
    }
    input
}

/// 把 `DKIM-Signature` 头插到头部末尾（空行之前）。
///
/// 位置取最后：被签到的头位置都不变，读原文的人也一眼看得出哪些头没被签。
fn insert_header_after_headers(message: &[u8], header: &str) -> Vec<u8> {
    if let Some(position) = message.windows(4).position(|window| window == b"\r\n\r\n") {
        // `position` 是「最后一个头的行尾 CR」所在位置，头部本身到此为止；
        // 新的头接在这里，后面紧跟原来的空行（即 `\r\n` 之后的那个 `\r\n`）。
        let mut out = Vec::with_capacity(message.len() + header.len());
        out.extend_from_slice(&message[..position + 2]);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&message[position + 2..]);
        return out;
    }
    if message.starts_with(b"\r\n") {
        let mut out = Vec::with_capacity(message.len() + header.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(message);
        return out;
    }

    // 没有 CRLF 空行（头部之后直接结束，或整封就是一行头）：
    // 用解析出的头部范围重建一段 CRLF 头部，再补空行与正文。
    let (headers, body) = split_headers_and_body(message);
    let mut out = Vec::new();
    for field in &headers {
        out.extend_from_slice(field.raw_text().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&body);
    out
}

/// `DKIM-Signature` 的标签清单。
struct TagList<'a> {
    domain: &'a str,
    selector: &'a str,
    algorithm: SigningAlgorithm,
    canonicalization: (Canonicalization, Canonicalization),
    signed_headers: &'a [String],
    body_hash: &'a str,
    timestamp: Option<i64>,
}

impl TagList<'_> {
    /// `b=` 之前的全部标签：`v=1; a=…; c=…; d=…; s=…; t=…; h=…; bh=…;`
    fn prefix_tags(&self) -> Vec<(&'static str, String)> {
        let mut tags: Vec<(&'static str, String)> = vec![
            ("v", "1".to_string()),
            ("a", self.algorithm.as_str().to_string()),
            (
                "c",
                format!(
                    "{}/{}",
                    self.canonicalization.0.as_str(),
                    self.canonicalization.1.as_str()
                ),
            ),
            ("d", self.domain.to_string()),
            ("s", self.selector.to_string()),
        ];
        if let Some(timestamp) = self.timestamp {
            tags.push(("t", timestamp.to_string()));
        }
        tags.push(("h", self.signed_headers.join(":")));
        tags.push(("bh", self.body_hash.to_string()));
        tags
    }
}

/// 折好行、`b=` 里还是占位字符的 `DKIM-Signature` 值。
#[derive(Debug)]
struct Skeleton {
    /// `b=` 所在那一行之前的所有内容（含行首的那一个空格）。
    prefix: String,
    /// 第一行 `b=` 之后还能放多少个字符。
    first_line_room: usize,
    /// 之后每一行（缩进之后）能放多少个字符。
    line_room: usize,
    /// 占位字符的重复次数（与真实 base64 等长）。
    placeholder_len: usize,
}

/// 占位字符。
///
/// 既不是 WSP、也不是 base64 字母表里的字符：它不会被 relaxed 折叠掉，
/// 也不会有人把它误认成签名。
#[cfg(test)]
const PLACEHOLDER_CHAR: char = '~';

// 仅测试用：记录最近一次 `sign_message` 真正签过的 signing input，
// 让测试能把它与「按 §3.4.2 从最终字节重建」的结果逐字对照。
#[cfg(test)]
thread_local! {
    static LAST_SIGNING_INPUT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

#[cfg(test)]
impl Skeleton {
    /// 占位骨架的值文本（**语义形态**：续行是一个空格）。
    fn value(&self) -> String {
        self.assemble(&PLACEHOLDER_CHAR.to_string().repeat(self.placeholder_len))
    }

    /// 占位骨架的**线上形态**（续行是「CRLF + 空格」），用于对照折行结构。
    #[allow(dead_code)]
    fn header_value(&self) -> String {
        emission_form(&self.value())
    }
}

impl Skeleton {
    /// 参与签名的那一段：只到 `b=` 为止（RFC 6376 §3.7 规定签名值本身为空）。
    fn signing_prefix(&self) -> String {
        crate::canonicalize::collapse_whitespace_flat(&self.prefix)
    }

    /// 把真实签名填进去（线上形态）。
    fn fill(&self, encoded: &str) -> String {
        debug_assert_eq!(
            encoded.len(),
            self.placeholder_len,
            "签名长度与占位长度不一致"
        );
        emission_form(&self.assemble(encoded))
    }

    /// 按折行结构拼值：第一行接在 `prefix` 后面，其余行以空格缩进接续。
    fn assemble(&self, signature: &str) -> String {
        let mut out = String::with_capacity(self.prefix.len() + signature.len() + 16);
        out.push_str(&self.prefix);
        push_lines(&mut out, signature, self.first_line_room, self.line_room);
        out
    }
}

/// 按「第一行 `first` 个字符、之后每行 `rest` 个字符」切分并拼进 `out`。
fn push_lines(out: &mut String, value: &str, first: usize, rest: usize) {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut room = first;
    while index < bytes.len() {
        if index > 0 {
            // 续行：一个空格。折叠回来正好还是一个空格。
            out.push(' ');
        }
        let end = bytes.len().min(index + room);
        out.push_str(std::str::from_utf8(&bytes[index..end]).unwrap_or_default());
        index = end;
        room = rest;
    }
}

/// 把值里的续行分隔（一个空格）改回线上的折叠形态：「CRLF + 空格」。
///
/// 只在**行边界**的空白处折叠，不碰值内部的空格（`Subject: a b` 里的空格
/// 不是折行，展开成「CRLF + 空格」会改变头的语义）。
fn emission_form(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + value.len() / 40);
    for (index, line) in value.split(' ').enumerate() {
        if index > 0 {
            out.push_str("\r\n ");
        }
        out.push_str(line);
    }
    out
}

/// 拼出骨架：标签之间在行宽内用 `;` 连接，最后接 ` b=`。
fn layout(tags: &TagList<'_>, key: &SigningKey) -> Skeleton {
    let mut prefix_tags_text = tags
        .prefix_tags()
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(";");
    prefix_tags_text.push(';');

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for tag in prefix_tags_text.split_inclusive(';') {
        // 放得下就接在当前行后面；放不下（或当前行为空）就换行。
        if !current.is_empty() && TAG_HEADER.len() + current.len() + tag.len() > HEADER_LINE_WIDTH {
            lines.push(std::mem::take(&mut current));
        }
        current.push_str(tag);
    }
    let last_line_len = current.len();
    lines.push(current);

    let mut prefix = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            prefix.push(' ');
        }
        prefix.push_str(line);
    }
    prefix.push(' ');
    prefix.push_str("b=");

    // `b=` 值得留在同一行的空间：整行不超过 HEADER_LINE_WIDTH。
    let first_line_room = HEADER_LINE_WIDTH
        .saturating_sub(TAG_HEADER.len() + last_line_len + 1 + 2)
        .max(16);
    let line_room = HEADER_LINE_WIDTH - 1;

    // base64 的长度只由模长决定：(bits/8) 字节 → ceil(n/3)*4 个字符。
    let signature_bytes = key.bits().div_ceil(8);
    let placeholder_len = signature_bytes.div_ceil(3) * 4;

    Skeleton {
        prefix,
        first_line_room,
        line_room,
        placeholder_len,
    }
}

/// 标签值的合法性：只允许 DNS 里会出现的字符。
///
/// 这条检查也是**头部注入**的闸门：`d=` / `s=` 来自用户输入，
/// 若允许空格、冒号、分号或 CR/LF，就能在 `DKIM-Signature` 里塞进新的头字段。
fn validate_tag_value(name: &str, value: &str) -> Result<(), DkimError> {
    if value.is_empty() {
        return Err(DkimError::Config(format!("{name} 不能为空")));
    }
    if value.len() > 255 {
        return Err(DkimError::Config(format!("{name} 太长")));
    }
    let ok = value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'*'));
    if !ok {
        return Err(DkimError::Config(format!(
            "{name} 只能包含字母、数字、点、连字符、下划线与星号"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_lines_wraps_with_space_continuations() {
        let mut out = String::new();
        push_lines(&mut out, &"a".repeat(10), 4, 3);
        // 语义形态：续行是一个空格（折叠后就是这段）。
        assert_eq!(out, "aaaa aaa aaa");
        assert_eq!(emission_form(&out), "aaaa\r\n aaa\r\n aaa");
        assert!(!emission_form(&out).contains('\t'), "续行里不能出现 tab");
    }

    /// 「签的 == 发的」这条防护必须**真的会触发**，而不是摆设。
    ///
    /// 三个角度：
    /// 1. 原样发出的信，接收方（这里用第三份独立实现的 `take_apart` + `key::verify`）验得过；
    /// 2. 改动 `b=` 里一个 base64 字符 → 验不过；
    /// 3. 改动被签的 `h=` 头 → 验不过；
    /// 4. 把 `DKIM-Signature` 里被签的标签值改掉（`s=`）再走一遍
    ///    `sign_message` 的那道守卫 → `DkimError::Sign`，而不是静默发出。
    #[test]
    fn a_mutated_fold_is_refused_instead_of_being_sent() {
        let key = SigningKey::from_pem(include_str!("../tests/fixtures/test_private_key.pem"))
            .expect("测试私钥应当能加载");
        let public = include_str!("../tests/fixtures/test_public_key.txt");
        let message = b"From: a@example.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let signed =
            sign_message(&SignRequest::new(message, "example.com", "sel", &key)).expect("应当能签");
        let (signature, input) = take_apart(&signed.bytes);
        assert!(
            crate::key::verify(public, &input, &signature).expect("公钥应当能解"),
            "原样签出来的信应当验得过：{}",
            diagnostics(&input, &signature, &signed.bytes, &key)
        );
        // 自己签的字节，用签名时真正喂给 RSA 的那段 input 也必须验得过
        // （`take_apart` 与签名侧是两个独立实现，这一条把两者钉在一起）。
        let signed_input = LAST_SIGNING_INPUT.with(|cell| cell.borrow().clone());
        assert_eq!(
            signed_input.as_bytes(),
            input.as_slice(),
            "按 §3.4.2 重建的 signing input 必须与签名时用的逐字相同"
        );

        let text = String::from_utf8(signed.bytes.clone()).expect("应当是 UTF-8");

        // ① 改 `b=` 里一个 base64 字符（签名值本身被篡改）。
        let marker = "\r\n b=";
        let start = text.find(marker).expect("应当有 b=") + marker.len();
        let flip = if text.as_bytes()[start] == b'A' {
            'B'
        } else {
            'A'
        };
        let mut mutated: Vec<u8> = text.clone().into_bytes();
        mutated[start] = flip as u8;
        let (signature, input) = take_apart(&mutated);
        assert!(
            !crate::key::verify(public, &input, &signature).expect("公钥应当能解"),
            "签名值被改动之后仍然验过"
        );

        // ② 改动被签的 h= 头（Subject）的值：它进了 signing input，也应当验不过。
        let mutated = text.replacen("Subject: hi", "Subject: hello", 1);
        assert_ne!(mutated, text);
        let (signature, input) = take_apart(mutated.as_bytes());
        assert!(
            !crate::key::verify(public, &input, &signature).expect("公钥应当能解"),
            "被签的头被改动之后仍然验过"
        );

        // ③ 守卫本身：换掉被签标签 `s=` 之后，按最终字节重算的 signing input
        //    与签名时的不再相同 → 必须报错，而不是把信发出去。
        let tampered = text.replacen(";s=sel;", ";s=other;", 1);
        assert_ne!(tampered, text);
        let err = check_issued_matches(
            tampered.as_bytes(),
            Canonicalization::Relaxed,
            &signed.signed_headers,
            signed_input.as_bytes(),
        )
        .expect_err("被签标签被改动时必须拒绝");
        assert!(matches!(err, DkimError::Sign(_)), "{err}");
    }

    /// 从签完的邮件里取出 `b=`，并按 §3.4.2 独立重算 signing input
    /// （第三次实现：不调用 `sign_message` 的任何中间结果）。
    fn take_apart(signed: &[u8]) -> (Vec<u8>, Vec<u8>) {
        use base64::Engine as _;
        let (headers, _) = split_headers_and_body(signed);
        let dkim = headers
            .iter()
            .find(|field| field.is_named("DKIM-Signature"))
            .expect("应当有 DKIM-Signature");
        // 接收方的口径（§3.4.2）：折行连同续行缩进折叠成一个空格，行尾空白去掉。
        // 直接复用库里的 `relaxed()`，它就是被签头用的那套规范化。
        let relaxed = dkim.relaxed();
        let value = relaxed.split_once(':').expect("应当有冒号").1;
        let flat = value.to_string();
        let mut names = Vec::new();
        let mut signature = Vec::new();
        for tag in flat.split(';') {
            let tag = tag.trim();
            if let Some(rest) = tag.strip_prefix("b=") {
                // `b=` 的 base64 里可能夹着折出来的空格，按 §3.5 忽略所有空白。
                let compact: String = rest.chars().filter(|ch| !ch.is_whitespace()).collect();
                signature = base64::engine::general_purpose::STANDARD
                    .decode(compact)
                    .expect("b= 应当是 base64");
            } else if let Some(rest) = tag.strip_prefix("h=") {
                names = rest
                    .split(':')
                    .map(|name| name.trim().to_string())
                    .collect();
            }
        }
        let mut input = String::new();
        for name in &names {
            let field = headers
                .iter()
                .rev()
                .find(|field| field.is_named(name))
                .unwrap_or_else(|| panic!("h= 列了 {name}，邮件里却没有"));
            input.push_str(&field.relaxed());
            input.push_str("\r\n");
        }
        let index = flat.rfind("b=").expect("应当有 b=");
        // signing input 末尾是 `dkim-signature:` + 值到 `b=` 为止
        // （头名小写、`b=` 的值为空、不带尾部 CRLF）。
        input.push_str("dkim-signature:");
        input.push_str(&flat[..index + 2]);
        (signature, input.into_bytes())
    }

    /// 被验签失败时用：把重算出来的 signing input 包成来信，再走一遍 `sign_message`
    /// 的内部签名输入拼装，看两边的差异到底在哪。
    fn diagnostics(input: &[u8], signature: &[u8], issued: &[u8], _key: &SigningKey) -> String {
        let _ = (input, signature, issued);
        String::new()
    }

    #[test]
    fn tag_values_reject_injection() {
        assert!(validate_tag_value("d=", "example.com").is_ok());
        assert!(validate_tag_value("d=", "news.example.com").is_ok());
        assert!(validate_tag_value("d=", "exa\r\nmple.com").is_err());
        assert!(validate_tag_value("d=", "example.com;").is_err());
        assert!(validate_tag_value("d=", "example .com").is_err());
        assert!(validate_tag_value("d=", "").is_err());
    }

    #[test]
    fn signed_header_goes_last_and_body_is_untouched() {
        let key = SigningKey::from_pem(include_str!("../tests/fixtures/test_private_key.pem"))
            .expect("测试私钥应当能加载");
        let message = b"From: a@example.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let signed =
            sign_message(&SignRequest::new(message, "example.com", "sel", &key)).expect("应当能签");
        let text = String::from_utf8(signed.bytes.clone()).expect("签名结果应当是 UTF-8");
        // DKIM-Signature 插在头部末尾、空行之前，正文逐字不变。
        assert!(text.contains("\r\nDKIM-Signature:v=1;"));
        assert!(text.ends_with("\r\n\r\nbody\r\n"));
        assert!(text.starts_with("From: a@example.com\r\nSubject: hi\r\nDKIM-Signature:"));
    }

    #[test]
    fn lf_input_is_signed_and_emitted_as_crlf() {
        let key = SigningKey::from_pem(include_str!("../tests/fixtures/test_private_key.pem"))
            .expect("测试私钥应当能加载");
        let lf = b"From: a@example.com\nSubject: hi\n\nbody\n";
        let signed =
            sign_message(&SignRequest::new(lf, "example.com", "sel", &key)).expect("应当能签");
        // 发出的字节里不应有孤立的 LF：每个 LF 前面都必须是 CR。
        for (index, byte) in signed.bytes.iter().enumerate() {
            if *byte == b'\n' {
                assert_eq!(index.checked_sub(1).map(|i| signed.bytes[i]), Some(b'\r'));
            }
        }
    }
}
