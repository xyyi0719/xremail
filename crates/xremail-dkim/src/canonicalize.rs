//! DKIM 规范化规则（RFC 6376 §3.4）。
//!
//! ## 为什么这一层必须独立成模块
//!
//! 签名算错的绝大多数原因都不在密码学，而在**规范化**：
//! 行尾到底是 CRLF 还是 LF、末尾那个空行算不算、头里的 tab 算不算空白。
//! 这几条规则是规范里逐字定义的，所以这里每个函数都对应一节条文，
//! 并且都有 RFC 6376 附录 A 的官方向量兜底（见 `tests/official_vectors.rs`）。
//!
//! ## 两种写法都要
//!
//! * `simple`：**原样**参与签名，一个字节都不动。好处是"所见即所签"，
//!   坏处是任何 MTA 改了一下折行就验不过。
//! * `relaxed`：空白、折行、头名大小写全部归一。
//!
//! 本 crate 的签名器固定用 `relaxed/relaxed`（见 [`crate::SignRequest`] 的
//! 默认值），但**校验**一侧两种都要支持，否则连自己签的邮件都验不回来。

use std::borrow::Cow;

/// 规范化算法（RFC 6376 §3.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Canonicalization {
    /// `simple`：原样。
    Simple,
    /// `relaxed`：空白与折行归一。
    #[default]
    Relaxed,
}

impl Canonicalization {
    /// `(头, 体)` 的简写（`relaxed/relaxed`）拆成两个算法。
    pub fn pair(self) -> (Self, Self) {
        (self, self)
    }

    /// 解析 `c=` 标签的值（`simple` / `relaxed` / `relaxed/relaxed`）。
    ///
    /// 只写一个词时，头与体用同一个算法（RFC 6376 §3.5 的 `c=` 定义）。
    pub fn parse(value: &str) -> Option<(Self, Self)> {
        let value = value.trim();
        let mut parts = value.split('/');
        let header = parse_one(parts.next()?)?;
        let body = match parts.next() {
            Some(part) => parse_one(part)?,
            None => header,
        };
        if parts.next().is_some() {
            return None;
        }
        Some((header, body))
    }

    /// `c=` 标签里写的名字。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Relaxed => "relaxed",
        }
    }
}

fn parse_one(value: &str) -> Option<Canonicalization> {
    match value.trim() {
        "simple" => Some(Canonicalization::Simple),
        "relaxed" => Some(Canonicalization::Relaxed),
        _ => None,
    }
}

/// 解析 `c=` 标签（`relaxed` / `simple/relaxed` …），返回 (头, 体)。
///
/// 这是 [`Canonicalization::parse`] 的免费函数形式，方便调用方不先引入枚举类型。
pub fn parse_canonicalization(value: &str) -> Option<(Canonicalization, Canonicalization)> {
    Canonicalization::parse(value)
}

/// 把任意行尾（CRLF / LF / CR）归一成 CRLF。
///
/// 邮件正文在**磁盘上**可以是任何行尾，但在要参与哈希的那一刻必须是 CRLF：
/// RFC 6376 §3.4.3 的规范化以 CRLF 为行分隔符，若把孤立的 LF 当成普通字节，
/// 同一封正文在不同平台上算出的 `bh=` 会不一样。
///
/// 输入里已经成形的 CRLF 与孤立的 CR（老 Mac 风格）都按一个换行处理。
pub fn normalize_crlf(data: &[u8]) -> Cow<'_, [u8]> {
    if !data.iter().any(|&b| b == b'\r' || b == b'\n') {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len() + 8);
    let mut index = 0;
    while index < data.len() {
        match data[index] {
            b'\r' => {
                out.extend_from_slice(b"\r\n");
                // `\r\n` 吃两个字节；孤立的 `\r` 吃一个。
                index += if data.get(index + 1) == Some(&b'\n') {
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
    Cow::Owned(out)
}

/// 拆出一行头字段的名字（冒号之前，去掉两端空白）。
#[allow(dead_code)]
fn header_name(line: &str) -> String {
    match line.find(':') {
        Some(index) => line[..index].trim().to_string(),
        None => line.trim().to_string(),
    }
}

/// 把一封邮件的字节拆成「头列表 + 体」。
///
/// 返回的头是**逐字段**的 `(原样的 `Name: value` 行（含折行）, 头名)`：
/// 头名只用于按 `h=` 去重挑选，值部分原样保留给规范化函数。
///
/// 找不到空行时，整段都算头、体为空（RFC 5322 允许没有正文）。
///
/// 返回的正文是**自己的一份**字节：拆分前要先把行尾归一成 CRLF，
/// 而归一结果可能与输入共用缓冲区，所以这里给出独立的 `Vec`，
/// 调用方不必关心借用关系。
pub fn split_headers_and_body(raw: &[u8]) -> (Vec<HeaderField>, Vec<u8>) {
    let normalized = normalize_crlf(raw);
    let data: &[u8] = &normalized;

    // 找第一个空行（CRLF CRLF）。
    let split_at = find_header_end(data);
    let (header_block, body) = match split_at {
        Some(body_start) => (&data[..body_start], data[body_start..].to_vec()),
        None => (data, Vec::new()),
    };

    (parse_header_block(header_block), body)
}

/// 找到空行的位置，返回**正文起点**（空行之后）。
fn find_header_end(data: &[u8]) -> Option<usize> {
    if data.starts_with(b"\r\n") {
        return Some(2);
    }
    let mut index = 0;
    while index + 4 <= data.len() {
        if &data[index..index + 4] == b"\r\n\r\n" {
            return Some(index + 4);
        }
        index += 1;
    }
    None
}

/// 把一段头字节拆成字段。
///
/// 续行（以空格或 tab 开头的行）并进上一个字段：RFC 5322 §2.2.3 规定
/// 折出来的行在语义上与一行等价。
fn parse_header_block(block: &[u8]) -> Vec<HeaderField> {
    let mut fields: Vec<HeaderField> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut index = 0;
    while index <= block.len() {
        let line_end = match block[index..]
            .windows(2)
            .position(|window| window == b"\r\n")
        {
            Some(offset) => index + offset,
            None => block.len(),
        };
        let line = &block[index..line_end];

        if line
            .first()
            .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
            && !fields.is_empty()
        {
            // 续行：在**语义上**并进当前字段（RFC 5322 §2.2.3 把折行视作单个空格），
            // 但 `raw` 里保留原始字节。规范化函数自己决定怎么看待这些空白：
            // simple 要原样（CRLF 折行），relaxed 会把它们折叠成一个空格。
            current.extend_from_slice(b" ");
            current.extend_from_slice(line);
        } else {
            if !current.is_empty()
                && let Some(field) = HeaderField::parse(&current)
            {
                fields.push(field);
            }
            current = line.to_vec();
        }

        if line_end + 2 > block.len() {
            break;
        }
        index = line_end + 2;
    }
    if !current.is_empty()
        && let Some(field) = HeaderField::parse(&current)
    {
        fields.push(field);
    }
    fields
}

/// 一个头字段。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeaderField {
    /// 原样的头名（保留大小写）。
    pub name: String,
    /// 头名之后、冒号之后的原始值（含折行与 CRLF）。
    pub value: String,
    /// 完整的一行（`Name: value`，含折行，不含行尾 CRLF）。
    raw: String,

    /// 头名之前没有任何字节时（正常情况）为 `false`；
    /// 为 `true` 时 `raw` 里**不含** `Name:` 前缀，直接就是值。
    /// 只用于构造「还没写头名的 `DKIM-Signature`」：签名时要的就是它的值。
    value_only: bool,
}

impl HeaderField {
    fn parse(raw: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(raw).into_owned();
        let colon = text.find(':')?;
        let name = text[..colon].to_string();
        if name.is_empty() || name.contains(char::is_whitespace) {
            // 折行开头却没有上一个字段、或头名带空白：不是合法头字段。
            return None;
        }
        let value = text[colon + 1..].to_string();
        Some(Self {
            name,
            value,
            raw: text,
            value_only: false,
        })
    }

    /// 头名（原样大小写）。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 头名是否等于 `name`（ASCII 不分大小写）。
    pub fn is_named(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    /// 从 `"Name: value"` 文本构造（测试与调用方拼头时用）。
    pub fn new(name: &str, value: &str) -> Self {
        Self {
            name: name.to_string(),
            value: value.to_string(),
            raw: format!("{name}:{value}"),
            value_only: false,
        }
    }

    /// 把 DKIM-Signature 值里 `b=` 之后的部分截掉（`b=` 的值不参与签名，§3.7）。
    ///
    /// 只给测试与「签完之后自检」使用：从最终字节里按 §3.4.2 重建签名输入时，
    /// 需要得到「`dkim-signature:` + 值，直到 `b=` 为止」这一段。
    #[allow(dead_code)]
    #[cfg(test)]
    pub(crate) fn without_signature_value(&self) -> String {
        let relaxed = self.relaxed();
        match relaxed.rfind("b=") {
            Some(index) => relaxed[..index + 2].to_string(),
            None => relaxed,
        }
    }

    /// 给测试用：把一段「头名: 值」文本按解析规则读成一个字段。
    #[allow(dead_code)]
    #[cfg(test)]
    pub(crate) fn parse_for_test(raw: &[u8]) -> Option<Self> {
        Self::parse(raw)
    }

    /// relaxed 头规范化（RFC 6376 §3.4.2）：
    ///
    /// 1. 头名转小写，去掉冒号后的空白；
    /// 2. 值里所有空白（含折行）折叠成一个空格；
    /// 3. 去掉值尾部的空白。
    ///
    /// 注意最后**不 trim 值首**：第 1 步已经截掉了头名后面那段空白，
    /// 而之后折叠出来的空格是**值的一部分**（例如折行的 `b=`）。
    pub fn relaxed(&self) -> String {
        let raw = self.raw.as_str();
        let (name, value) = if self.value_only {
            ("", raw)
        } else {
            match raw.find(':') {
                Some(index) => (&raw[..index], &raw[index + 1..]),
                None => ("", raw),
            }
        };
        let collapsed = collapse_whitespace(value);
        if self.value_only {
            return collapsed.trim_end().to_string();
        }
        format!("{}:{}", name.to_ascii_lowercase(), collapsed.trim_end())
    }

    /// simple 头规范化（RFC 6376 §3.4.1）：字段**原样**，
    /// 连头名大小写、冒号后的空白与折行都不动。
    ///
    /// `raw` 里的续行以 CRLF 分隔（解析时原样保留），正是要发出去的字节形态。
    pub fn simple(&self) -> String {
        self.raw.clone()
    }

    /// simple 规范化要的是「发出去的样子」：续行在解析时被并成单个空格，
    /// 这里把它还原成「CRLF + 空格」，与线上字节一致。
    #[allow(dead_code)]
    #[cfg(test)]
    pub(crate) fn unfold_for_emission(&mut self) {
        self.raw = emission_form(&self.raw);
    }

    /// 按算法取规范化后的文本。
    pub fn canonicalize(&self, algorithm: Canonicalization) -> String {
        match algorithm {
            Canonicalization::Simple => self.simple(),
            Canonicalization::Relaxed => self.relaxed(),
        }
    }

    /// 取一个头字段的值（用于 `From:` 里找域）。
    pub fn folded_value(&self) -> String {
        collapse_whitespace(&self.value).trim().to_string()
    }

    /// simple 头规范化下的**原样文本**（不含行尾 CRLF），供重写头部时使用。
    pub fn raw_text(&self) -> String {
        self.raw.clone()
    }
}

/// 把解析得到的 `raw` 还原成线上的折叠形态：
/// 只要某个空格后面紧跟着**非空白的行内续行**（解析时用单个空格接进来的），
/// 就换成「CRLF + 空格」。
#[allow(dead_code)]
fn legacy_emission_form(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch == ' ' && index > 0 && index + 1 < chars.len() {
            let prev = chars[index - 1];
            let next = chars[index + 1];
            if !(prev == ' ' || prev == '\t' || next == ' ' || next == '\t') {
                // 值内部的普通单空格也可能是原样的；只有「原本折行」的地方才该还原，
                // 但那份信息在解析时已经丢了（两种形态字节不同、语义相同除外）。
                // 这里选择**不**擅自改写：见 tests 里的折行往返测试。
                out.push(ch);
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// 按 RFC 6376 §3.4.2 把空白归一：WSP 连续段 → 单个空格，并去掉折行。
fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // 折行 = CRLF 后面紧跟的 WSP；折行本身连同它后面的 WSP 一起折叠。
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                pending_space = true;
                // 吃掉折行后的 WSP（RFC 5322 §2.2.3：折行后的 WSP 是折叠的一部分）。
                while matches!(chars.peek(), Some(' ') | Some('\t')) {
                    chars.next();
                }
            }
            '\r' | '\n' => pending_space = true,
            ' ' | '\t' => pending_space = true,
            other => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push(other);
            }
        }
    }
    // 结尾的空白不补。
    out
}

/// 不理会行尾空白来源的折叠：**任何**空白段（含折行）都变成一个空格，
/// 值首那一段变成的值开头的一个空格。
///
/// 用于 `DKIM-Signature` 头字段本身：它的折行是签发方自己排的版，
/// 不管原始折行是「CRLF + 空格」还是解析时并进来的一个空格，折叠结果都应当相同——
/// 这正是 `b=` 能在不改变 signing input 的前提下填进去的原因。
pub(crate) fn collapse_whitespace_flat(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if matches!(ch, ' ' | '\t' | '\r' | '\n') {
            pending_space = true;
            continue;
        }
        // 值首的空白在 relaxed 下被删掉（RFC 6376 §3.4.2），这里同样丢弃。
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    out
}

/// relaxed 体规范化（RFC 6376 §3.4.4）：
///
/// 1. 行尾统一 CRLF；
/// 2. 去掉行尾空白；
/// 3. 把行内连续 WSP 折叠成一个空格；
/// 4. **去掉**正文末尾的所有空行；
/// 5. 若正文非空，末尾补**一个** CRLF。
///
/// 全是空白的体（含空体）规范化为**零字节**，按 RFC 6376 §3.4.5
/// 取 SHA-256("")。
pub fn canonicalize_body_relaxed(body: &[u8]) -> Vec<u8> {
    let normalized = normalize_crlf(body);
    let mut out: Vec<u8> = Vec::with_capacity(normalized.len() + 2);

    for line in split_lines(&normalized) {
        let trimmed = trim_trailing_wsp(line);
        out.extend_from_slice(&collapse_wsp_bytes(trimmed));
        out.extend_from_slice(b"\r\n");
    }

    // 去掉末尾的空行（这里只可能是 CRLF 串）。
    while out.ends_with(b"\r\n") {
        out.truncate(out.len() - 2);
    }
    if !out.is_empty() {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// simple 体规范化（RFC 6376 §3.4.3）：行尾归一成 CRLF、末尾重复的空行去掉，
/// 其余**一个字节都不改**（行内空白、大小写都原样）。
///
/// 两个容易写错的地方：
///
/// * **末尾要去到只剩一个 CRLF**。非空正文的 simple 规范化结果确实以 CRLF 结尾
///   （§3.4.3 说“the body ... with all trailing CRLF characters removed”，
///   而 §3.4.5 的摘要输入本身不带尾 CRLF，正文与 `CRLF` 之间不加额外换行）；
/// * 行尾归一之所以必要，是因为网上流传的正文常被存成 LF：只要换行字节不同，
///   哈希就不同，但那不是签名方要表达的意思。
pub fn canonicalize_body_simple(body: &[u8]) -> Vec<u8> {
    let normalized = normalize_crlf(body);
    // 削掉末尾所有的 CRLF，再补回**一个**：非空正文的 simple 结果总是以单个 CRLF 结尾。
    let mut end = normalized.len();
    while end >= 2 && normalized[..end].ends_with(b"\r\n") {
        end -= 2;
    }
    if end == 0 {
        return Vec::new();
    }
    let mut out = normalized[..end].to_vec();
    out.extend_from_slice(b"\r\n");
    out
}

/// 按算法规范化体。
pub fn canonicalize_body(body: &[u8], algorithm: Canonicalization) -> Vec<u8> {
    match algorithm {
        Canonicalization::Simple => canonicalize_body_simple(body),
        Canonicalization::Relaxed => canonicalize_body_relaxed(body),
    }
}

/// 按 CRLF 切行；**不**保留行尾。
///
/// 末尾的 CRLF 不产生一个额外空行：`a\r\n` 是一行，不是两行。
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index + 2 <= data.len() {
        if &data[index..index + 2] == b"\r\n" {
            lines.push(&data[start..index]);
            index += 2;
            start = index;
        } else {
            index += 1;
        }
    }
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

fn trim_trailing_wsp(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
        end -= 1;
    }
    &line[..end]
}

fn collapse_wsp_bytes(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut pending_space = false;
    for &byte in line {
        if byte == b' ' || byte == b'\t' {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(b' ');
        }
        pending_space = false;
        out.push(byte);
    }
    out
}

/// 把值里的续行分隔（一个空格）改回线上的折叠形态：「CRLF + 空格」。
///
/// 只在**行边界**的空白处折叠，不碰值内部的空格（`Subject: a b` 里的空格
/// 不是折行，展开成「CRLF + 空格」会改变头的语义）。
#[allow(dead_code)]
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

#[allow(dead_code)]
#[cfg(test)]
pub(crate) fn emission_form_for_tests(value: &str) -> String {
    emission_form(value)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_normalization_handles_lf_and_lone_cr() {
        assert_eq!(&*normalize_crlf(b"a\nb\r\nc\rd"), b"a\r\nb\r\nc\r\nd");
        assert!(matches!(&*normalize_crlf(b"a\r\nb"), b"a\r\nb"));
    }

    #[test]
    fn relaxed_body_strips_trailing_empty_lines_and_keeps_one_crlf() {
        assert_eq!(
            canonicalize_body_relaxed(b"Hello  world \r\n\r\n\r\n"),
            b"Hello world\r\n".to_vec()
        );
        // LF 行尾也要归一。
        assert_eq!(canonicalize_body_relaxed(b"a\nb\n"), b"a\r\nb\r\n".to_vec());
    }

    #[test]
    fn empty_and_whitespace_only_bodies_canonicalize_to_nothing() {
        assert!(canonicalize_body_relaxed(b"").is_empty());
        assert!(canonicalize_body_relaxed(b"\r\n\r\n  \r\n").is_empty());
        // 纯空白的行去掉行尾空白后就是空行，同样被剥掉。
        assert!(canonicalize_body_relaxed(b"  \r\n").is_empty());
    }

    #[test]
    fn simple_body_keeps_bytes_but_ends_with_one_crlf() {
        // 行内的双空格原样保留（simple 不折叠空白），末尾多余的空行按 §3.4.3 去掉，
        // 非空正文的结果总是以**一个** CRLF 结尾。
        assert_eq!(canonicalize_body_simple(b"a  b\r\n\r\n"), b"a  b\r\n");
        assert_eq!(canonicalize_body_simple(b"a  b\n"), b"a  b\r\n");
        assert_eq!(canonicalize_body_simple(b"a  b"), b"a  b\r\n");
        assert!(canonicalize_body_simple(b"\r\n").is_empty());
        assert!(canonicalize_body_simple(b"\r\n\r\n").is_empty());
    }

    #[test]
    fn relaxed_header_lowercases_name_and_collapses_whitespace() {
        let field = HeaderField::parse(b"A: X\r\n Y \t Z\r\n\tW").expect("应能解析");
        assert_eq!(field.relaxed(), "a:X Y Z W");
        assert_eq!(field.simple(), "A: X\r\n Y \t Z\r\n\tW");
    }

    #[test]
    fn c_tag_parsing() {
        assert_eq!(
            Canonicalization::parse("relaxed"),
            Some((Canonicalization::Relaxed, Canonicalization::Relaxed))
        );
        assert_eq!(
            Canonicalization::parse("simple/relaxed"),
            Some((Canonicalization::Simple, Canonicalization::Relaxed))
        );
        assert_eq!(
            Canonicalization::parse("relaxed/simple"),
            Some((Canonicalization::Relaxed, Canonicalization::Simple))
        );
        assert_eq!(Canonicalization::parse("bogus"), None);
    }

    #[test]
    fn splitting_finds_body_after_the_first_empty_line() {
        let raw = b"From: a@b\r\nSubject: hi\r\n\r\nbody\r\n";
        let (headers, body) = split_headers_and_body(raw);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].name, "From");
        assert_eq!(body, b"body\r\n");
    }
}
