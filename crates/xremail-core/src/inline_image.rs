//! 正文里的内联图片（`cid:`）安全替换。
//!
//! ## 问题
//!
//! [`crate::html::sanitize_html`] 会把 `img` **整个删掉**，包括
//! `<img src="cid:logo@example.com">`。这是正确的：邮件里的 `<img>` 绝大多数
//! 是远程跟踪像素（见 `docs/ISSUES_AND_RISKS.md` R06）。
//!
//! 但**内联图片**（`Content-Disposition: inline` 且带 `Content-ID`，也就是信里夹着的那张图）
//! 不是远程资源——图片字节就在这封邮件里，加载它不会向发件方泄露任何东西。
//! 把它们也删掉，认证邮件里的公司 logo、截图、签名图就全没了。
//!
//! ## 做法
//!
//! 在**后端**把 `<img src="cid:xxx">` 换成 `data:` URI：
//!
//! 1. 在**原始** HTML 里找 `<img>`，取出它的 `src`（属性值先解实体）；
//! 2. `src` 必须是 `cid:` 引用（大小写不敏感、允许 `%3C`/`%3E` 表示尖括号），
//!    且能在这封邮件的内联部件里**按 `Content-ID` 匹配**（精确优先，忽略大小写兜底）；
//! 3. 匹配到的部件**必须**是 `image/*`（SVG 除外），且字节不大于
//!    [`MAX_INLINE_IMAGE_BYTES`]；这些判定由 [`InlineImage::new`] /
//!    [`InlineImage::from_attachment`] 完成，本模块只接受已经构造好的 [`InlineImage`]；
//! 4. 换成一个 `<img src="data:image/…;base64,…" alt="…">`。
//!
//! 其余 `<img>`（远程 `http(s)`、`cid:` 匹配不上、`file:`、`javascript:` …）
//! **原样留在这里**，交给 [`crate::html::sanitize_html`] 删掉——本模块不承担
//! 消毒职责，它只是「把安全的那个特例提前换掉」。
//!
//! ## 为什么是 `data:` 而不是自定义协议
//!
//! CSP（`apps/shell/src-tauri/tauri.conf.json`）的 `img-src` 已含 `data:`，
//! 所以**不需要放开任何 CSP 项**；而自定义协议要么得注册 Tauri 的 `asset` 协议
//! （等于把「本机哪个文件能被页面读到」交���路径参数决定），要么自己写
//! `register_asynchronous_uri_scheme_protocol`，安全面都更大。
//! `data:` URI 不含任何远程请求，因而没有跟踪像素、DNS/IP 泄露的问题。
//! `img-src` 里的 `http:` / `https:` 一个字都不改——远程图片照样删。
//!
//! ## 顺序为什么重要，以及返回值里的标记
//!
//! 必须先换、后消毒：`img` 不在消毒器的白名单里，先消毒就无从替换；
//! 而换出来的 `data:` `<img>` 若再交给消毒器，照样会被删掉。
//!
//! 所以 [`rewrite_inline_images`] 返回的字符串分**两段语义**：
//! 替换成功的图片被 [`SAFE_IMAGE_OPEN`] … [`SAFE_IMAGE_CLOSE`] **包住**，
//! 其余原文逐字节保留。调用方按这两个标记切开——
//!
//! * 标记内的片段由本模块拼出（只含 `src="data:…"` 与 `alt`），可以直接输出；
//! * 标记外的片段仍是不可信的原始 HTML，**必须**继续过 `sanitize_html`。
//!
//! ## 大小上限
//!
//! `data:` URI 会把图片放大成 base64（4/3），并且整段进前端内存与 DOM。
//! 一张 10 MB 的图变成 13 MB 的字符串，渲染一次就能让界面卡死。所以：
//!
//! * 单张超过 [`MAX_INLINE_IMAGE_BYTES`] 不构造（构造期就拒绝）；
//! * 整封邮件替换后的总量超过 [`MAX_INLINE_TOTAL_BYTES`] 就停止替换——
//!   宁可后面的图不显示（回退成「删掉」），也不要让一封邮件吃掉几百 MB 内存。

use std::collections::HashMap;
use std::path::Path;

use crate::inline_error::InlineImageError;
use crate::model::Attachment;

/// 单张内联图片的字节上限（解码后）。
pub const MAX_INLINE_IMAGE_BYTES: u64 = 2 * 1024 * 1024;

/// 一封邮件里所有内联图片替换后的总字节上限（base64 后估算）。
pub const MAX_INLINE_TOTAL_BYTES: u64 = 8 * 1024 * 1024;

/// 替换后片段的包裹标记（开始）。
pub const SAFE_IMAGE_OPEN: &str = "\u{1}IMG\u{1}";

/// 替换后片段的包裹标记（结束）。
pub const SAFE_IMAGE_CLOSE: &str = "\u{1}/IMG\u{1}";

/// 一张可以安全内联的图片。
///
/// 由调用方（服务层）从「已下载的内联附件」构造：只有确实落到本地、
/// MIME 是 `image/*`、大小对得上的部件才能变成它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    /// `Content-ID`，**已去掉尖括号**。
    content_id: String,
    /// 已验证为安全的 `image/*` MIME（小写）。
    mime: String,
    /// 解码后的字节。
    bytes: Vec<u8>,
}

impl InlineImage {
    /// 校验并构造。
    ///
    /// 拒绝了这些情况（它们都不该走到渲染里）：
    /// * `content_id` 为空——空 CID 无从匹配，容易被误配；
    /// * MIME 不是 `image/*`，或 MIME 里夹带 `;` 之外的怪字符；
    /// * 字节为空，或超过 [`MAX_INLINE_IMAGE_BYTES`]。
    ///
    /// ## 为什么拒绝 SVG
    ///
    /// SVG 是能装脚本的 XML（`<svg onload=…>` / `<script>`）。现代浏览器在
    /// `<img src="data:image/svg+xml…">` 里不执行脚本，但这是**依赖浏览器实现**
    /// 的安全边界。本客户端的原则是「边界不靠运气」，所以 SVG 不内联——
    /// 它退化成今天的「删掉」，不引入任何新风险。
    pub fn new(
        content_id: impl AsRef<str>,
        mime: impl AsRef<str>,
        bytes: Vec<u8>,
    ) -> Result<Self, InlineImageError> {
        let content_id = normalize_content_id(content_id.as_ref());
        if content_id.is_empty() {
            return Err(InlineImageError::EmptyContentId);
        }
        let mime = normalize_image_mime(mime.as_ref()).ok_or(InlineImageError::NotAnImage)?;
        if bytes.is_empty() {
            return Err(InlineImageError::Empty);
        }
        if bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
            return Err(InlineImageError::TooLarge {
                actual: bytes.len() as u64,
                limit: MAX_INLINE_IMAGE_BYTES,
            });
        }
        Ok(Self {
            content_id,
            mime,
            bytes,
        })
    }

    /// `Content-ID`（去尖括号）。
    pub fn content_id(&self) -> &str {
        &self.content_id
    }

    /// 已验证的 MIME。
    pub fn mime(&self) -> &str {
        &self.mime
    }

    /// 解码后的字节数。
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// 是否没有字节（构造时已拒绝，这里只为对称的 clippy 约定）。
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// 字节。
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `data:` URI。
    pub fn data_uri(&self) -> String {
        let mut out = String::with_capacity(self.bytes.len() / 3 * 4 + 32);
        out.push_str("data:");
        out.push_str(&self.mime);
        out.push_str(";base64,");
        out.push_str(&base64_encode(&self.bytes));
        out
    }

    /// 从「已下载的内联附件」+ 本地文件构造。
    ///
    /// 只接受：`inline == true`、有 `content_id`、`fetched == true` 且有 `local_path`、
    /// MIME 是安全的 `image/*`。读文件时若**实际大小**与库里记的 `size` 不一致，
    /// 视为数据损坏（文件被外部改动）而拒绝——渲染层不拿一个来路不明的文件当图片。
    ///
    /// `read` 传进来而不是直接读盘，是为了让测试不需要真文件；
    /// 服务层传 `std::fs::read`。
    pub fn from_attachment(
        attachment: &Attachment,
        read: impl FnOnce(&Path) -> std::io::Result<Vec<u8>>,
    ) -> Result<Self, InlineImageError> {
        if !attachment.inline {
            return Err(InlineImageError::NotInline);
        }
        let content_id = attachment
            .content_id
            .as_deref()
            .ok_or(InlineImageError::EmptyContentId)?;
        let mime = attachment
            .mime
            .as_deref()
            .ok_or(InlineImageError::NotAnImage)?;
        if normalize_image_mime(mime).is_none() {
            return Err(InlineImageError::NotAnImage);
        }
        if !attachment.fetched {
            return Err(InlineImageError::NotDownloaded);
        }
        let Some(local_path) = attachment.local_path.as_deref() else {
            return Err(InlineImageError::NotDownloaded);
        };
        let bytes = read(Path::new(local_path)).map_err(|e| InlineImageError::Io(e.to_string()))?;
        if let Some(expected) = attachment.size
            && expected >= 0
            && bytes.len() as i64 != expected
        {
            return Err(InlineImageError::SizeMismatch {
                expected,
                actual: bytes.len() as u64,
            });
        }
        Self::new(content_id, mime, bytes)
    }
}

/// 把 `cid:` 引用换成 `data:` URI 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rewrite {
    /// 替换后的 HTML：安全图片片段被 [`SAFE_IMAGE_OPEN`] … [`SAFE_IMAGE_CLOSE`]
    /// 包住，其余原文原样保留。
    pub html: String,
    /// 替换掉的图片数。
    pub replaced: usize,
}

/// 把正文里的 `<img src="cid:…">` 换成 `data:` URI 片段。
///
/// 语义见模块文档：调用方按 [`SAFE_IMAGE_OPEN`] / [`SAFE_IMAGE_CLOSE`] 切开，
/// 只把标记**外**的片段继续交给 `sanitize_html`。
///
/// 没有成功替换任何图片时，`html` 与输入**逐字节相同**，调用方可以照旧整段消毒。
pub fn rewrite_inline_images(input: &str, images: &[InlineImage]) -> Rewrite {
    if images.is_empty() || !contains_img_tag(input) {
        return Rewrite {
            html: input.to_string(),
            replaced: 0,
        };
    }

    // RFC 2392 里 Content-ID 不区分大小写（它是 `msg-id`，而 `msg-id` 的
    // 域部分按 DNS 规则忽略大小写；整体上也常被各家实现当不透明串）。
    // 这里「精确匹配优先、忽略大小写兜底」：同一封信里出现两个只差大小写的
    // CID 时，精确拼写的那个赢。
    let mut exact: HashMap<&str, &InlineImage> = HashMap::new();
    let mut folded: HashMap<String, &InlineImage> = HashMap::new();
    for image in images {
        exact.entry(image.content_id()).or_insert(image);
        folded
            .entry(image.content_id().to_ascii_lowercase())
            .or_insert(image);
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let mut replaced = 0usize;
    let mut total: u64 = 0;

    loop {
        let Some(lt) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..lt]);
        let tail = &rest[lt..];

        let Some((name, src, alt, end)) = parse_tag(tail) else {
            // 不是能处理的标签（注释、`<?…>`、没有 `>`…）：搬走这个 `<` 继续扫。
            out.push('<');
            rest = &tail[1..];
            continue;
        };

        let consumed = &tail[..end];
        rest = &tail[end..];

        if !name.eq_ignore_ascii_case("img") {
            out.push_str(consumed);
            continue;
        }

        let Some(cid) = src.as_deref().and_then(parse_cid) else {
            out.push_str(consumed);
            continue;
        };
        let found = exact
            .get(cid.as_str())
            .copied()
            .or_else(|| folded.get(&cid.to_ascii_lowercase()).copied());
        let Some(image) = found else {
            out.push_str(consumed);
            continue;
        };

        let cost = image.len() as u64 / 3 * 4 + 64;
        if total.saturating_add(cost) > MAX_INLINE_TOTAL_BYTES {
            // 总量到顶：后面的图不再替换（会被消毒器删掉）。
            out.push_str(consumed);
            continue;
        }
        total += cost;

        out.push_str(SAFE_IMAGE_OPEN);
        push_img_element(&mut out, &image.data_uri(), alt.as_deref());
        out.push_str(SAFE_IMAGE_CLOSE);
        replaced += 1;
    }

    Rewrite {
        html: out,
        replaced,
    }
}

/// 纯函数便利入口（与 [`rewrite_inline_images`] 相同）。
pub fn inline_replacements(input: &str, images: &[InlineImage]) -> Rewrite {
    rewrite_inline_images(input, images)
}

/// 输入里有没有 `<img`（大小写不敏感）。没有就整段绕过，省一次扫描。
fn contains_img_tag(input: &str) -> bool {
    let bytes = input.as_bytes();
    const NEEDLE: &[u8] = b"<img";
    if bytes.len() < NEEDLE.len() {
        return false;
    }
    (0..=bytes.len() - NEEDLE.len())
        .any(|i| bytes[i] == b'<' && bytes[i..i + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE))
}

/// 解析一个开始标签，返回 `(标签名, src, alt, 这一段的字节长度)`。
///
/// 只处理「简单的开始标签」：读到 `>` 为止，属性按 HTML5 的规则切分
/// （双引号 / 单引号 / 无引号）。属性值先解实体再比较，
/// 这样 `src="&#99;id:x"` 也能被认成 `cid:x`（与浏览器的解释一致）。
///
/// 遇到 `<` 后面不是字母、属性没闭合（没找到 `>`）的情况返回 `None`，
/// 调用方原样搬走那个 `<`。
fn parse_tag(tail: &str) -> Option<(String, Option<String>, Option<String>, usize)> {
    let bytes = tail.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    // `<` 后面必须是 ASCII 字母（否则是文本里的 `<`）。
    if !bytes.get(1)?.is_ascii_alphabetic() {
        return None;
    }

    let mut i = 1usize;
    while i < bytes.len() && !is_tag_name_end(bytes[i]) {
        i += 1;
    }
    let name = std::str::from_utf8(&bytes[1..i]).ok()?.to_string();

    let mut src: Option<String> = None;
    let mut alt: Option<String> = None;

    loop {
        while i < bytes.len() && (is_ws(bytes[i]) || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'>' {
            return Some((name, src, alt, i + 1));
        }

        let name_start = i;
        while i < bytes.len() && !is_attr_name_end(bytes[i]) {
            i += 1;
        }
        if name_start == i {
            // 认不出的字符（例如 `=`），跳过以免死循环。
            i += 1;
            continue;
        }
        let attr_name = std::str::from_utf8(&bytes[name_start..i]).ok()?;

        while i < bytes.len() && is_ws(bytes[i]) {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && is_ws(bytes[i]) {
                i += 1;
            }
            if i >= bytes.len() {
                return None;
            }
            match bytes[i] {
                q @ (b'"' | b'\'') => {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != q {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return None;
                    }
                    value = std::str::from_utf8(&bytes[start..i]).ok()?.to_string();
                    i += 1;
                }
                _ => {
                    let start = i;
                    while i < bytes.len() && !is_ws(bytes[i]) && bytes[i] != b'>' {
                        i += 1;
                    }
                    value = std::str::from_utf8(&bytes[start..i]).ok()?.to_string();
                }
            }
            value = decode_entities(&value);
        }

        if attr_name.eq_ignore_ascii_case("src") && src.is_none() {
            src = Some(value);
        } else if attr_name.eq_ignore_ascii_case("alt") && alt.is_none() {
            alt = Some(value);
        }
    }
}

/// 标签名读到哪儿为止。
fn is_tag_name_end(b: u8) -> bool {
    is_ws(b) || b == b'/' || b == b'>'
}

/// 属性名读到哪儿为止。
fn is_attr_name_end(b: u8) -> bool {
    is_ws(b) || b == b'=' || b == b'/' || b == b'>'
}

/// 判断 HTML 空白。
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// `cid:` 引用 → 去掉尖括号的纯 `Content-ID`。
///
/// 认这些写法：`cid:abc`、`CID:abc`、`cid:<abc>`、`cid:abc@example.com`、
/// `cid:%3Cabc%3E`（`%3C`/`%3E` 是尖括号）。其余返回 `None`。
///
/// 返回的是**没做大小写折叠**的原始串；匹配时先精确、再忽略大小写。
fn parse_cid(src: &str) -> Option<String> {
    // 浏览器解析 URL 前会去掉 TAB/CR/LF。
    let cleaned: String = src
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.len() < 4 || !cleaned[..4].eq_ignore_ascii_case("cid:") {
        return None;
    }
    let id = normalize_content_id(&cleaned[4..]);
    if id.is_empty() { None } else { Some(id) }
}

/// 去尖括号、把 `%3C`/`%3E` 还原成尖括号、trim。
fn normalize_content_id(raw: &str) -> String {
    let text = raw
        .trim()
        .replace("%3C", "<")
        .replace("%3c", "<")
        .replace("%3E", ">")
        .replace("%3e", ">");
    let text = text.trim();
    let text = text.strip_prefix('<').unwrap_or(text);
    let text = text.strip_suffix('>').unwrap_or(text);
    text.trim().to_string()
}

/// 校验并规范化图片 MIME：必须是 `image/<子类型>`，子类型只含字母数字与 `+ - .`。
/// SVG 拒绝（见 [`InlineImage::new`]）。
fn normalize_image_mime(raw: &str) -> Option<String> {
    // 去掉参数（`image/png; name=x.png` 这种）。
    let mime = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let (kind, sub) = mime.split_once('/')?;
    if kind != "image" || sub.is_empty() {
        return None;
    }
    if !sub
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    if sub == "svg+xml" || sub == "svg" {
        return None;
    }
    Some(mime)
}

/// 输出 `<img>` 元素：只写 `src`（已是我们拼的 `data:`）与 `alt`。
fn push_img_element(out: &mut String, data_uri: &str, alt: Option<&str>) {
    out.push_str("<img src=\"");
    push_escaped_attr(out, data_uri);
    out.push('"');
    if let Some(alt) = alt {
        out.push_str(" alt=\"");
        push_escaped_attr(out, alt);
        out.push('"');
    }
    out.push('>');
}

/// 属性值转义：`&` `<` `>` `"` 与控制字符。
fn push_escaped_attr(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => {}
            _ => out.push(ch),
        }
    }
}

/// 解码属性值里的实体（数字实体 + 常用命名实体）。
///
/// 只有解开了，`&#99;id:x` 才不会被当成「不是 cid 引用」而漏判。
/// 漏判只会导致不显示（安全侧），但会让写成转义形式的 `cid:` 丢图。
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some((replacement, len)) = decode_entity(&bytes[i..])
        {
            out.push_str(&replacement);
            i += len;
            continue;
        }
        let len = utf8_len(bytes[i]);
        let end = (i + len).min(text.len());
        if let Some(chunk) = text.get(i..end) {
            out.push_str(chunk);
        }
        i = end.max(i + 1);
    }
    out
}

/// 解码单个实体。
fn decode_entity(bytes: &[u8]) -> Option<(String, usize)> {
    let body = &bytes[1..];
    if body.first() == Some(&b'#') {
        let hex = body.len() > 1 && (body[1] == b'x' || body[1] == b'X');
        let digits_start = if hex { 2 } else { 1 };
        let mut i = digits_start;
        while i < body.len()
            && (if hex {
                body[i].is_ascii_hexdigit()
            } else {
                body[i].is_ascii_digit()
            })
        {
            i += 1;
        }
        if i == digits_start || i >= body.len() || body[i] != b';' {
            return None;
        }
        let digits = std::str::from_utf8(&body[digits_start..i]).ok()?;
        let code = if hex {
            u32::from_str_radix(digits, 16).ok()?
        } else {
            digits.parse::<u32>().ok()?
        };
        let ch = char::from_u32(code)?;
        return Some((ch.to_string(), 1 + i + 1));
    }

    let mut i = 0usize;
    while i < body.len() && body[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == 0 || i >= body.len() || body[i] != b';' {
        return None;
    }
    let name = std::str::from_utf8(&body[..i]).ok()?;
    let replacement = match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "colon" => ":",
        "sol" => "/",
        "num" => "#",
        _ => return None,
    };
    Some((replacement.to_string(), 1 + i + 1))
}

/// UTF-8 字符的字节长度。
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// base64 编码（标准字母表 + `=` 填充）。
///
/// 本 crate 不许加依赖，所以自带一份小的编码器。**只用于编码**，
/// 不涉及解码的各种宽容情形。
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}
