//! 附件部件的解析。
//!
//! 这一层要做的事只有一件：**从一封邮件的 MIME 结构里挑出附件部件，
//! 给每个部件一份可信的元数据**——文件名、类型、解码后字节数、
//! Content-ID、是不是内联图，以及它在 MIME 树里的**部件号**
//! （`1`、`1.2`、`2.1`……），下载时靠这个号码去取那一段字节。
//!
//! ## 为什么文件名要单独一个模块
//!
//! 真实邮件里的附件名有三种写法，三种都得处理：
//!
//! | 写法 | 例子 | 谁在用 |
//! |---|---|---|
//! | 直接写 | `filename="报告.pdf"` | 国内邮箱常见（头字段直接是 UTF-8/GBK 字节） |
//! | RFC 2047 编码字 | `filename="=?UTF-8?B?5oql5ZGKLnBkZg==?="` | Outlook、部分脚本 |
//! | RFC 2231 扩展 | `filename*=utf-8''%E6%8A%A5%E5%91%8A.pdf` | 新客户端；可带语言前缀与分段续写 |
//!
//! 处理顺序：**先看 `filename*`，再看 `filename`**（RFC 6266 §4.3 规定扩展参数优先）；
//! 两者都没有时退回 `Content-Type; name=`（RFC 2046 允许在那里给名字）。
//!
//! ## 与 `Part::is_attachment` 的区别
//!
//! [`crate::mime::Part::is_attachment`] 是「有名字或不是文本」的粗判，
//! 只用来给列表页打 `has_attachment` 标记。真正的附件判定在
//! [`is_attachment_header`]：按 RFC 2183 只看 `Content-Disposition`，
//! 另外把「`inline` 但有文件名」也算进来（微信/QQ 导出的图片常这样）。

use crate::encoded_word::decode_encoded_words;
use crate::mime::HeaderMap;

/// 一个附件部件的描述（**不含字节**）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentMeta {
    /// MIME 树里的部件号：顶层是 `1`，嵌套是 `1.2`（自 1 起，与 IMAP 的 `BODY[1.2]` 一致）。
    pub part_number: String,
    /// 原始文件名（尽力而为；拿不到就是 `None`）。
    pub filename: Option<String>,
    /// `Content-Type` 的媒体类型，小写。
    pub content_type: String,
    /// **解码后**的字节数。
    pub size: i64,
    /// `Content-ID`，**已去掉尖括号**。
    pub content_id: Option<String>,
    /// 是不是 `inline` 部件（内联图片）。
    pub is_inline: bool,
    /// `Content-Transfer-Encoding`（小写）：`base64` / `quoted-printable` / `8bit` …
    ///
    /// 下载时按它解码，所以**必须**存下来——按内容猜是猜不准的。
    pub transfer_encoding: Option<String>,
}

/// 把 MIME 树里的路径拼成 IMAP 的 `BODY[<部件号>]` 里的那个号。
///
/// 空路径 = 整封就是一个部件 → `TEXT`；非空路径前面补上顶层 `multipart` 的 `1`。
/// 路径里的 `0` 是 `message/rfc822` 那层的占位，原样保留（见 `mime::EMBEDDED_MESSAGE`）。
///
/// **不能用 `0` 当顶层号**：`BODY.PEEK[0]` 只返回结构，不返回正文。
fn to_part_number(path: &[usize]) -> String {
    if path.is_empty() {
        return crate::mime::TOP_LEVEL_PART.to_string();
    }
    std::iter::once(1)
        .chain(path.iter().copied())
        .map(|piece| piece.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// 决定一个部件是不是附件、以及它的 `part_number`。
///
/// `path` 是 `mime::split_parts_with_paths` 给出的相对路径（**不含**顶层那层，
/// 但 `message/rfc822` 那层用 `0` 占位——见 `mime::EMBEDDED_MESSAGE`），
/// `top_is_single_part` 说明**整封邮件是不是只有一个部件**：
/// * 是：整封就是 1 号，记作 `"TEXT"`——它与 `BODY.PEEK[1]` 等价，
///   又不会撞上「`BODY.PEEK[0]` 只取结构」那个坑（见 `mime::TOP_LEVEL_PART`）。
/// * 否（顶层是 `multipart`，或者顶层是内嵌着一整封邮件的 `message/rfc822`）：
///   保留路径。`multipart` 自己占 1 号，于是 `[]` → `"1"`、`[2]` → `"1.2"`。
///
/// ## 为什么判据是「叶子自己有没有路径」而不是「顶层是不是 multipart」
///
/// 这里曾经用「顶层 `Content-Type` 是不是 `multipart/*`」决定要不要丢路径，
/// 结果**顶层是 `message/rfc822`** 时（整封转发、邮件作为附件的那种形态）
/// 判定为假，路径被整个清空，内嵌邮件里各个子部件的号**全部塌成 `TEXT`**。
///
/// 后果不只是编号难看：`xremail-sync` 的附件 id 是
/// `{message_id}/{part_number}`，落库靠 `ON CONFLICT (message_id, part_key)`
/// 去重。两个附件都叫 `TEXT` 就是同一个 key，它们会在库里**互相顶掉**。
///
/// 真正的判据应该是「这个叶子自己是不是整封的全部」——那正是 `path` 是否为空
/// 所表达的东西：`collect_leaves` 对顶层单部件给出的就是空路径。
///
/// `transfer_encoding` 原样记下来给下载侧用。
///
/// 判定规则（按优先级）：
/// 1. 有 `Content-Disposition: attachment` → 附件；
/// 2. 有 `Content-Disposition: inline` **且**带文件名 → 附件（`is_inline = true`）；
/// 3. 其余一律不算——包括「没有 `Content-Disposition`、只是 `image/png`」的部件：
///    那通常是正文的一部分，不该出现在附件栏里。
pub fn attachment_meta(
    headers: &HeaderMap,
    path: Option<&[usize]>,
    top_is_single_part: bool,
    content_type: &str,
    size: i64,
    transfer_encoding: Option<&str>,
) -> Option<AttachmentMeta> {
    // 整封只有一个部件：路径本就该是空的，记作 `TEXT`。
    // 否则**保留**调用方给的路径——包括顶层 `message/rfc822` 压出来的那个 `0`。
    let path: &[usize] = if top_is_single_part {
        &[]
    } else {
        path.unwrap_or(&[])
    };
    let part_number = to_part_number(path);
    let disposition = headers.get("Content-Disposition");
    let filename = filename_from_headers(headers);

    let kind = disposition_kind(disposition)?;
    let is_inline = kind == InlineKind::Inline;

    if is_inline && filename.is_none() {
        // `inline` 又没有名字：是正文的行内资源，不列进附件表。
        return None;
    }

    debug_assert!(size >= 0, "附件大小不该是负数");

    Some(AttachmentMeta {
        part_number,
        filename,
        content_type: normalize_media_type(content_type),
        size: size.max(0),
        content_id: headers
            .get("Content-ID")
            .map(normalize_content_id)
            .filter(|value| !value.is_empty()),
        is_inline,
        transfer_encoding: transfer_encoding
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty()),
    })
}

/// `Content-Disposition` 的首个 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineKind {
    /// `attachment`
    Attachment,
    /// `inline`
    Inline,
}

/// 解析 `Content-Disposition` 的首个 token。
///
/// 没有这个头、或者首 token 不是 `attachment`/`inline`（有的客户端把整个头写坏）
/// 时返回 `None`。
fn disposition_kind(value: Option<&str>) -> Option<InlineKind> {
    let value = value?.trim_start();
    let token = value
        .split([';', ','])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match token.as_str() {
        "attachment" => Some(InlineKind::Attachment),
        "inline" => Some(InlineKind::Inline),
        _ => None,
    }
}

/// 这个头集合能不能算「附件头」（不看内容，只看 `Content-Disposition`）。
///
/// 留给上层做一致性判断（比如 `has_attachment` 与附件表条数对不上时排查用）。
pub fn is_attachment_header(headers: &HeaderMap) -> bool {
    match disposition_kind(headers.get("Content-Disposition")) {
        Some(InlineKind::Attachment) => true,
        Some(InlineKind::Inline) => headers.contains("filename*") || headers.contains("filename"),
        None => false,
    }
}

/// 从头字段里取文件名（静态入口，给不经过解析器的场景用）。
pub fn filename_from_headers(headers: &HeaderMap) -> Option<String> {
    // 1. RFC 2231：`filename*0*=`、`filename*1*=` 这样的续行先拼起来。
    if let Some(name) = rfc2231_from_headers(headers, "filename") {
        let name = clean_filename(name);
        if !name.is_empty() {
            return Some(name);
        }
    }

    // 2. 普通 `filename=` / `name=`。
    for field in ["Content-Disposition", "Content-Type"] {
        let Some(value) = headers.get(field) else {
            continue;
        };
        for key in ["filename", "name"] {
            if let Some(raw) = crate::mime::extract_param(value, key) {
                let name = clean_filename(decode_any(raw.to_string()));
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }

    None
}

/// 解析 RFC 2231 的重名参数（`filename*`、`filename*0*`、`filename*1`…）。
///
/// 分段规则（RFC 2231 §3）：
/// * `filename*=utf-8''%E6%8A%A5` —— 单段，`charset'language'value`；
/// * `filename*0*=utf-8''%E6%8A%A5` + `filename*1=%91` —— 多段，
///   **第一段**带 charset，后续段只带是否百分号编码（`*`）。
fn rfc2231_from_headers(headers: &HeaderMap, name: &str) -> Option<String> {
    for field in ["Content-Disposition", "Content-Type"] {
        let Some(value) = headers.get(field) else {
            continue;
        };
        if let Some(decoded) = rfc2231_from_value(value, name) {
            return Some(decoded);
        }
    }
    None
}

/// 从单个头字段值里解析 RFC 2231 扩展参数。
fn rfc2231_from_value(value: &str, name: &str) -> Option<String> {
    let mut single: Option<String> = None;
    // (段号, 这一段是否百分号编码, 原始值)
    let mut sections: Vec<(usize, bool, String)> = Vec::new();

    for piece in split_parameters(value) {
        let Some((key, val)) = piece.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        let val = strip_quotes(val).to_string();

        if key == format!("{name}*") {
            single = Some(val);
            continue;
        }

        // `name*N` / `name*N*`
        let Some(rest) = key.strip_prefix(&format!("{name}*")) else {
            continue;
        };
        let (digits, encoded) = match rest.strip_suffix('*') {
            Some(digits) => (digits, true),
            None => (rest, false),
        };
        let Ok(index) = digits.parse::<usize>() else {
            continue;
        };
        sections.push((index, encoded, val));
    }

    if let Some(raw) = single {
        return Some(decode_extended_value(&raw, true));
    }
    if sections.is_empty() {
        return None;
    }

    sections.sort_by_key(|(index, _, _)| *index);

    let mut charset: Option<String> = None;
    let mut out: Vec<u8> = Vec::new();
    for (position, (_, encoded, raw)) in sections.into_iter().enumerate() {
        // 第一段才带 `charset'language'` 前缀。
        if position == 0 {
            let (decoded, found) = strip_extended_prefix(&raw);
            charset = found;
            extend_percent(&mut out, decoded, encoded);
        } else {
            extend_percent(&mut out, &raw, encoded);
        }
    }

    Some(decode_with_charset(&out, charset.as_deref()))
}

/// 把参数串切成一个个 `key=value`（尊重引号里的分号）。
fn split_parameters(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '\\' if in_quotes => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ';' if !in_quotes => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    out.push(current);
    out
}

/// 去掉一层成对的双引号。
fn strip_quotes(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// 拆 `charset'language'value` 前缀。
fn strip_extended_prefix(raw: &str) -> (&str, Option<String>) {
    let Some(first) = raw.find('\'') else {
        return (raw, None);
    };
    let Some(second) = raw[first + 1..].find('\'').map(|i| i + first + 1) else {
        return (raw, None);
    };
    let charset = &raw[..first];
    let value = &raw[second + 1..];
    if charset.is_empty() {
        (value, None)
    } else {
        (value, Some(charset.to_string()))
    }
}

/// 按 RFC 2231 解一段（可能需要百分号解码）并追加到 `out`。
fn extend_percent(out: &mut Vec<u8>, raw: &str, encoded: bool) {
    if encoded {
        out.extend(percent_decode(raw.as_bytes()));
    } else {
        out.extend_from_slice(raw.as_bytes());
    }
}

/// 单段扩展值的解码：可能带 `charset''`，也可能不带。
fn decode_extended_value(raw: &str, encoded: bool) -> String {
    // 只有**看起来像**前缀时才剥：`utf-8''xxx`。
    let (value, charset) = match strip_extended_prefix(raw) {
        (value, Some(charset)) => (value, Some(charset)),
        (value, None) => (value, None),
    };
    let mut bytes = Vec::new();
    extend_percent(&mut bytes, value, encoded);
    decode_with_charset(&bytes, charset.as_deref())
}

/// 把没有声明的非 ASCII 文件名解成文本。
///
/// 顺序：
/// 1. 能被 RFC 2047 编码字解出来的，先用它（`filename="=?utf-8?B?…?="`）；
/// 2. 已经是合法 UTF-8 的，直接用；
/// 3. 不是 UTF-8 的——国内邮箱常直接塞 GBK 字节——按 **GBK** 解
///    （GB18030 是 GBK 的超集，用 GBK 解会得到更「干净」的结果）。
pub fn decode_any(raw: String) -> String {
    if raw.contains("=?") {
        let decoded = decode_encoded_words(&raw);
        if decoded != raw {
            return decoded;
        }
    }
    if raw.is_ascii() || std::str::from_utf8(raw.as_bytes()).is_ok() {
        return raw;
    }
    decode_with_charset(raw.as_bytes(), None)
}

/// 按 charset（可选）解字节；charset 缺失或认不出来时按 GBK 兜底。
fn decode_with_charset(bytes: &[u8], charset: Option<&str>) -> String {
    if let Some(charset) = charset
        && let Some(encoding) = crate::charset::encoding_for_label(charset)
    {
        return encoding.decode_without_bom_handling(bytes).0.into_owned();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    crate::charset::decode_lossy(bytes, None)
}

/// `%XX` 解码（非法转义原样保留）。
fn percent_decode(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' && index + 2 < raw.len() {
            let high = (raw[index + 1] as char).to_digit(16);
            let low = (raw[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(raw[index]);
        index += 1;
    }
    out
}

/// 规范化文件名的空白。
fn clean_filename(name: String) -> String {
    name.replace(['\r', '\n', '\0'], "").trim().to_string()
}

/// `Content-ID` 去尖括号、去空白。
pub fn normalize_content_id(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_string()
}

/// 媒体类型小写化、丢掉 `;` 之后的参数（正常解析时不带参数，这里兜底）。
pub fn normalize_media_type(raw: &str) -> String {
    let trimmed = raw.split(';').next().unwrap_or("").trim();
    if trimmed.is_empty() {
        "application/octet-stream".to_string()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        HeaderMap {
            fields: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    #[test]
    fn plain_utf8_filename() {
        let h = headers(&[("Content-Disposition", "attachment; filename=\"报告.pdf\"")]);

        assert_eq!(
            filename_from_headers(&h).as_deref(),
            Some("报告.pdf"),
            "头里直接写 UTF-8 时要用原文"
        );
    }

    #[test]
    fn rfc2231_single_segment_wins_over_plain() {
        let h = headers(&[(
            "Content-Disposition",
            "attachment; filename=\"__.pdf\"; filename*=utf-8''%E6%8A%A5%E5%91%8A.pdf",
        )]);

        assert_eq!(filename_from_headers(&h).as_deref(), Some("报告.pdf"));
    }

    #[test]
    fn rfc2231_continuation_segments_are_joined() {
        let h = headers(&[(
            "Content-Disposition",
            "attachment; filename*0*=utf-8''%E6%8A%A5%E5%91%8A; filename*1*=%2Epdf",
        )]);

        assert_eq!(filename_from_headers(&h).as_deref(), Some("报告.pdf"));
    }

    #[test]
    fn rfc2231_with_language_prefix_and_gbk() {
        // `报告.pdf` 的 GBK 字节：B1 A8 B8 E6 2E 70 64 66
        let h = headers(&[(
            "Content-Disposition",
            "attachment; filename*=gbk'zh-CN'%B1%A8%B8%E6.pdf",
        )]);

        assert_eq!(filename_from_headers(&h).as_deref(), Some("报告.pdf"));
    }

    #[test]
    fn rfc2047_encoded_word_filename() {
        // "报告.pdf" 的 UTF-8 base64。
        let h = headers(&[(
            "Content-Disposition",
            "attachment; filename=\"=?UTF-8?B?5oql5ZGKLnBkZg==?=\"",
        )]);

        assert_eq!(filename_from_headers(&h).as_deref(), Some("报告.pdf"));
    }

    #[test]
    fn filename_with_raw_gbk_bytes_is_best_effort() {
        // 解析层（`parse_headers`）用 `from_utf8_lossy`，GBK 字节到这一步已经变成
        // U+FFFD。这里钉住的是**不崩、仍给出什么**，而不是解出中文——
        // 真正的 GBK 解码路径见 `raw_gbk_bytes_are_decoded`。
        let bytes: [u8; 11] = [0xB1, 0xA8, 0xB8, 0xE6, b'.', b't', b'x', b't', 0, 0, 0];
        let lossy = String::from_utf8_lossy(&bytes[..8]).into_owned();
        let h = headers(&[(
            "Content-Disposition",
            &format!("attachment; filename=\"{lossy}\""),
        )]);

        let name = filename_from_headers(&h).expect("仍要给出一个名字");

        assert!(name.ends_with(".txt"), "{name}");
    }

    #[test]
    fn raw_gbk_bytes_are_decoded() {
        // 头字段解析层用 `String::from_utf8_lossy`，真实邮件里的 GBK 字节到这一步
        // 已经被换成 U+FFFD 了（这是既有解析器的限制，见模块文档）。
        // 这里验证的是**兜底路径**：拿到的字节不是 UTF-8 时按 GBK 解。
        let bytes: [u8; 4] = [0xB1, 0xA8, 0xB8, 0xE6]; // “报告”的 GBK
        assert_eq!(decode_with_charset(&bytes, None), "报告");
        // 声明了 charset 时优先用它。
        assert_eq!(decode_with_charset(&bytes, Some("gbk")), "报告");
        // 合法 UTF-8 不会被当 GBK。
        assert_eq!(decode_with_charset("报告".as_bytes(), None), "报告");
    }

    #[test]
    fn lossy_utf8_names_still_produce_something() {
        // 解析层把 GBK 字节解坏之后的形态：全是 U+FFFD。
        let decoded = decode_any("????.txt".replace('?', "\u{FFFD}"));

        assert!(decoded.ends_with(".txt"));
    }

    #[test]
    fn name_falls_back_to_content_type() {
        let h = headers(&[
            ("Content-Disposition", "attachment"),
            ("Content-Type", "image/png; name=\"logo.png\""),
        ]);

        assert_eq!(filename_from_headers(&h).as_deref(), Some("logo.png"));
    }

    #[test]
    fn missing_filename_is_none() {
        let h = headers(&[("Content-Disposition", "attachment")]);

        assert_eq!(filename_from_headers(&h), None);
    }

    #[test]
    fn attachment_meta_numbers_parts() {
        let h = headers(&[
            ("Content-Disposition", "attachment; filename=a.pdf"),
            ("Content-ID", " <part1@example.com> "),
        ]);

        // `top_is_single_part = false`：这封邮件有不止一个部件，
        // 所以路径要保留、拼成 `1.2`（顶层 multipart 自己占 1 号）。
        let meta = attachment_meta(
            &h,
            Some(&[2]),
            false,
            "Application/PDF",
            1024,
            Some("Base64"),
        )
        .expect("是附件");

        assert_eq!(meta.part_number, "1.2");
        assert_eq!(meta.filename.as_deref(), Some("a.pdf"));
        assert_eq!(meta.content_type, "application/pdf");
        assert_eq!(meta.size, 1024);
        assert_eq!(meta.content_id.as_deref(), Some("part1@example.com"));
        assert!(!meta.is_inline);
        assert_eq!(
            meta.transfer_encoding.as_deref(),
            Some("base64"),
            "传输编码要小写保存，下载时按它解码"
        );
    }

    #[test]
    fn top_level_part_is_numbered_text() {
        let h = headers(&[("Content-Disposition", "attachment; filename=a.pdf")]);

        let meta = attachment_meta(&h, None, false, "application/pdf", 10, None).expect("是附件");

        // 整封就是一个部件：IMAP 里是 1 号，记作 `TEXT`。
        // **不能**记成 `0`——`BODY.PEEK[0]` 只返回结构，不返回正文。
        eprintln!("DBG meta={meta:?} const={}", crate::mime::TOP_LEVEL_PART);
        assert_eq!(meta.part_number, crate::mime::TOP_LEVEL_PART);
        assert_eq!(meta.part_number, "TEXT");
        assert_ne!(meta.part_number, "0");
    }

    #[test]
    fn an_empty_path_is_also_the_whole_message() {
        let h = headers(&[("Content-Disposition", "attachment; filename=a.pdf")]);

        let meta =
            attachment_meta(&h, Some(&[]), false, "application/pdf", 10, None).expect("是附件");

        assert_eq!(meta.part_number, crate::mime::TOP_LEVEL_PART);
    }

    #[test]
    fn inline_with_filename_is_listed() {
        let h = headers(&[
            ("Content-Disposition", "inline; filename=\"图片.png\""),
            ("Content-ID", "<img1@example.com>"),
        ]);

        let meta = attachment_meta(&h, Some(&[2]), true, "image/png", 42, None).expect("是附件");

        assert!(meta.is_inline);
        assert_eq!(meta.content_id.as_deref(), Some("img1@example.com"));
    }

    #[test]
    fn inline_without_filename_is_not_listed() {
        let h = headers(&[
            ("Content-Disposition", "inline"),
            ("Content-ID", "<img1@example.com>"),
        ]);

        assert_eq!(
            attachment_meta(&h, Some(&[2]), true, "image/png", 42, None),
            None
        );
    }

    #[test]
    fn parts_without_disposition_are_not_attachments() {
        let h = headers(&[("Content-Type", "image/png")]);

        assert_eq!(
            attachment_meta(&h, Some(&[2]), true, "image/png", 42, None),
            None
        );
        assert!(!is_attachment_header(&h));
    }

    #[test]
    fn disposition_kind_ignores_case_and_leading_space() {
        let h = headers(&[("Content-Disposition", "  ATTACHMENT; filename=a.txt")]);

        let meta = attachment_meta(&h, Some(&[1]), true, "text/plain", 3, None).expect("是附件");

        assert_eq!(meta.filename.as_deref(), Some("a.txt"));
    }

    #[test]
    fn content_id_without_brackets_is_kept() {
        let h = headers(&[
            ("Content-Disposition", "inline; filename=a.png"),
            ("Content-ID", "plain@example.com"),
        ]);

        let meta = attachment_meta(&h, Some(&[1]), true, "image/png", 1, None).expect("是附件");

        assert_eq!(meta.content_id.as_deref(), Some("plain@example.com"));
    }

    #[test]
    fn null_bytes_and_newlines_are_stripped_from_names() {
        let h = headers(&[("Content-Disposition", "attachment; filename=\"a\r\nb.txt\"")]);

        assert_eq!(filename_from_headers(&h).as_deref(), Some("ab.txt"));
    }
}
