//! RFC 2047 编码字。
//!
//! 邮件头里的非 ASCII 文本会被编码成 `=?charset?B?...?=`（base64）
//! 或 `=?charset?Q?...?=`（quoted-printable）。
//!
//! 实测（2026-10-23）：三家的主题**全都**用编码字，
//! charset 有 UTF-8，也有 **GB2312**（QQ）与 **GBK**（沃）。
//!
//! ## 相邻编码字之间的空白要丢掉
//!
//! RFC 2047 §6.2：两个编码字之间只有空白时，那个空白**必须**被丢弃
//! （否则解码后会多出空格）。这个细节不处理，主题里会出现
//! 一堆莫名空格。

use crate::charset;
use crate::transfer;

/// RFC 2047 §2：单个编码字最长 75 个字符（`=?charset?B?` + 载荷 + `?=`）。
const MAX_ENCODED_WORD_LEN: usize = 75;

/// 头字段里的一段：要么是可打印 ASCII（**原样保留**），要么需要编码。
#[derive(Debug, Clone)]
enum Run {
    /// 可打印 ASCII 段。
    Ascii(String),
    /// 非 ASCII 段（要编码成编码字）。
    Encoded(String),
}

/// 把一段可能含非 ASCII 的头字段文本编码成 RFC 2047 形式。
///
/// * **可打印 ASCII 部分原样保留**——RFC 2047 只要求非 ASCII 部分编码，
///   这样 `Re: 主题` 编码出来是 `Re: =?utf-8?B?...?=` 而不是整条都变成编码字，
///   对方客户端显示得更正常；
/// * 非 ASCII 部分编码成 base64 编码字 `=?charset?B?...?=`；
/// * 太长的一段会拆成多个编码字（RFC 2047 §2 规定单个编码字不超过 75 字符）。
///
/// ## 相邻编码字之间的空白会被解码方丢掉
///
/// RFC 2047 §6.2 规定相邻编码字之间的空白**必须被丢弃**。
/// 所以本函数把「编码字 → 空白 → 编码字」里的那段空白**挪进后一个编码字的载荷**，
/// 否则解码后中间的空格就没了。解码方丢掉的是**分隔用的**空白，
/// 内容里的空白因为在载荷里所以能保住。
pub fn encode_header_value(text: &str, charset_label: &str) -> String {
    let label = charset::normalize_label(charset_label);
    let encoding = charset::encoding_for_label(&label);

    // 纯 ASCII 且全是可打印字符 → 原样返回。
    if text.chars().all(is_printable_ascii) {
        return text.to_string();
    }

    let mut runs = split_into_runs(text);
    absorb_whitespace_between_runs(&mut runs);
    runs = merge_runs_for_delimiters(runs);

    let mut out = String::with_capacity(text.len() + 16);
    let mut previous_was_encoded = false;
    for run in &runs {
        match run {
            Run::Ascii(chunk) => {
                if chunk.is_empty() {
                    // 空段（空白被挪走了）→ 前后两个编码字仍然相邻。
                    continue;
                }
                out.push_str(chunk);
                previous_was_encoded = false;
            }
            Run::Encoded(chunk) => {
                if previous_was_encoded {
                    // RFC 2047 §6.2：相邻编码字之间必须用空白隔开。
                    // 解码方会把这个空白丢掉（所以不会污染内容）。
                    out.push(' ');
                }
                out.push_str(&encode_encoded_words(chunk, &label, encoding));
                previous_was_encoded = true;
            }
        }
    }
    out
}

/// 把一段非 ASCII 文本编码成一个或多个 base64 编码字。
fn encode_encoded_words(
    text: &str,
    label: &str,
    encoding: Option<&'static encoding_rs::Encoding>,
) -> String {
    let max_bytes = max_payload_bytes(label);
    let chunks = split_run(text, max_bytes, encoding);

    let mut out = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            // 同一段拆出来的多个编码字之间也要有分隔空白。
            out.push(' ');
        }
        let bytes = encode_to_bytes(chunk, encoding);
        out.push_str("=?");
        out.push_str(label);
        out.push_str("?B?");
        out.push_str(&transfer::encode_base64(&bytes));
        out.push_str("?=");
    }
    out
}

/// 一个编码字最多能装多少**字节**（按目标字符集编码后的长度）。
fn max_payload_bytes(label: &str) -> usize {
    // `=?` + 标签 + `?B?` + 载荷 + `?=`
    let overhead = 2 + label.chars().count() + 3 + 2;
    let payload_chars = MAX_ENCODED_WORD_LEN.saturating_sub(overhead) / 4 * 4;
    // base64：4 个字符编码 3 个字节。
    payload_chars / 4 * 3
}

/// 把文本按目标字符集编码成字节。
fn encode_to_bytes(text: &str, encoding: Option<&'static encoding_rs::Encoding>) -> Vec<u8> {
    match encoding {
        Some(enc) => enc.encode(text).0.into_owned(),
        None => text.as_bytes().to_vec(),
    }
}

/// 单个字符编码后的字节数。
fn char_encoded_len(ch: char, encoding: Option<&'static encoding_rs::Encoding>) -> usize {
    let mut buffer = [0u8; 4];
    let text = ch.encode_utf8(&mut buffer);
    match encoding {
        Some(enc) => enc.encode(text).0.len(),
        None => text.len(),
    }
}

/// 把文本切成「可打印 ASCII 段」与「非 ASCII 段」。
fn split_into_runs(text: &str) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for ch in text.chars() {
        let ascii = is_printable_ascii(ch);
        match runs.last_mut() {
            Some(Run::Ascii(chunk)) if ascii => chunk.push(ch),
            Some(Run::Encoded(chunk)) if !ascii => chunk.push(ch),
            _ => {
                if ascii {
                    runs.push(Run::Ascii(ch.to_string()));
                } else {
                    runs.push(Run::Encoded(ch.to_string()));
                }
            }
        }
    }
    runs
}

/// 把「编码字 → 空白 → 编码字」中间的空白挪进**后一个**编码字的载荷。
///
/// 见 [`encode_header_value`] 的说明：解码方会丢掉相邻编码字之间的空白，
/// 只有把内容里的空白放进载荷才保得住。
fn absorb_whitespace_between_runs(runs: &mut [Run]) {
    let mut index = 1;
    while index + 1 < runs.len() {
        let should_move = match (&runs[index - 1], &runs[index], &runs[index + 1]) {
            (Run::Encoded(previous), Run::Ascii(middle), Run::Encoded(_)) => {
                middle.chars().all(char::is_whitespace)
                    && previous
                        .chars()
                        .next_back()
                        .is_some_and(|ch| !ch.is_ascii())
            }
            _ => false,
        };

        if should_move {
            let middle = match &runs[index] {
                Run::Ascii(chunk) => chunk.clone(),
                Run::Encoded(_) => String::new(),
            };
            if let Run::Encoded(next) = &mut runs[index + 1] {
                next.insert_str(0, &middle);
            }
            if let Run::Ascii(chunk) = &mut runs[index] {
                chunk.clear();
            }
        }

        index += 1;
    }
}

/// 把「编码字与相邻文本之间没有空白」的相邻段合并成一个编码段。
///
/// RFC 2047 §4.2 要求编码字与相邻文本之间必须用空白隔开，
/// 否则很多解码器根本不认这个编码字（解码器靠空白判断编码字的边界）。
/// 合并后整段都编码，解码回来还是原文——比硬塞一个多余的空格好。
///
/// 两个相邻的编码段也合并：合并后由 [`split_run`] 统一决定切分点，
/// 切点保证落在非 ASCII 字符之后，这样解码方丢弃相邻编码字之间的
/// 空白时不会丢内容。
fn merge_runs_for_delimiters(runs: Vec<Run>) -> Vec<Run> {
    let mut result: Vec<Run> = Vec::new();
    for run in runs {
        let merge = match (&result.last(), &run) {
            (None, _) => false,
            (Some(Run::Ascii(left)), Run::Encoded(_)) => {
                // 左段是明文：它必须以空白结尾，否则右边的编码字没有分隔。
                !left.chars().next_back().is_some_and(char::is_whitespace)
            }
            (Some(Run::Encoded(_)), Run::Ascii(right)) => {
                // 右段是明文：它必须以空白开头，否则左边的编码字没有分隔。
                !right.chars().next().is_some_and(char::is_whitespace)
            }
            (Some(Run::Encoded(_)), Run::Encoded(_)) => true,
            _ => false,
        };

        if merge {
            let left = match result.pop() {
                Some(Run::Ascii(text)) | Some(Run::Encoded(text)) => text,
                None => String::new(),
            };
            let right = match &run {
                Run::Ascii(text) | Run::Encoded(text) => text.clone(),
            };
            result.push(Run::Encoded(format!("{left}{right}")));
        } else {
            result.push(run);
        }
    }
    result
}

/// 把一段非 ASCII 文本切成若干块，每块编码后能塞进一个编码字。
///
/// 切分点优先选在**非 ASCII 字符之后**：这样每个编码字都以非 ASCII 字符结尾，
/// 解码方「丢掉相邻编码字之间的空白」的启发式才成立。
fn split_run(
    text: &str,
    max_bytes: usize,
    encoding: Option<&'static encoding_rs::Encoding>,
) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks: Vec<String> = Vec::new();

    let mut start = 0usize;
    let mut len = 0usize;
    // 当前段里最后一个非 ASCII 字符的下标。
    let mut last_non_ascii: Option<usize> = None;

    for index in 0..chars.len() {
        let ch = chars[index];
        let ch_len = char_encoded_len(ch, encoding);

        if index > start && len + ch_len > max_bytes {
            let cut = match last_non_ascii {
                Some(pos) => pos + 1,
                None => index,
            };
            chunks.push(chars[start..cut].iter().collect());
            start = cut;
            len = 0;
            last_non_ascii = None;
            for (offset, &current) in chars[cut..=index].iter().enumerate() {
                len += char_encoded_len(current, encoding);
                if !current.is_ascii() {
                    last_non_ascii = Some(cut + offset);
                }
            }
        } else {
            len += ch_len;
            if !ch.is_ascii() {
                last_non_ascii = Some(index);
            }
        }
    }

    if start < chars.len() {
        chunks.push(chars[start..].iter().collect());
    }
    chunks
}

/// 是不是「头字段里可以原样保留」的 ASCII 字符。
///
/// 可打印 ASCII 加制表符。控制字符（CR/LF 除外）不允许原样出现在头里。
fn is_printable_ascii(ch: char) -> bool {
    ch.is_ascii() && (ch == ' ' || ch == '\t' || ('!'..='~').contains(&ch))
}

/// 解码一个头字段里的所有编码字。
///
/// 不是编码字的部分原样保留。
pub fn decode_encoded_words(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    /// 上一段输出是从哪个编码字解出来的。
    ///
    /// RFC 2047 §6.2 只说「两个**编码字**之间只隔空白时，那段空白要丢掉」，
    /// 与编码字解出来是什么字符**无关**。这里曾经用「输出末尾是不是非 ASCII」
    /// 去猜相邻关系：一旦前一个编码字解出纯 ASCII（很常见——
    /// `=?utf-8?B?UmU6IA==?=` 解出来是 `Re: `），「相邻」就判不出来，
    /// 中间那段本该丢掉的空白被保留，主题里多出一个空格（R60）。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Previous {
        /// 输出里还没有东西。
        Nothing,
        /// 上一段是编码字。
        EncodedWord,
        /// 上一段是明文（或者被跳过的空白，见下）。
        Plain,
    }
    let mut previous = Previous::Nothing;

    loop {
        let Some(start) = rest.find("=?") else {
            out.push_str(rest);
            break;
        };

        // 编码字必须是 `=?charset?enc?text?=`。
        let Some(parsed) = parse_encoded_word(&rest[start..]) else {
            // 不是合法的编码字 → 把 `=?` 当普通文本，继续找下一个。
            out.push_str(&rest[..start + 2]);
            rest = &rest[start + 2..];
            previous = Previous::Plain;
            continue;
        };

        let (charset_label, encoded, text, consumed) = parsed;

        // 编码字**前面**的空白：如果上一段输出是编码字，按 RFC 2047 §6.2 丢掉。
        //
        // 这里**不改** `previous`：编码字与编码字之间的连接是「穿过空白」的
        // 直接相邻（所以三四个编码字之间的每一段空白都要丢），
        // 而紧跟在后面的那个编码字自己就会把 `previous` 设回 `EncodedWord`。
        let before = &rest[..start];
        if !before.is_empty() && !before.chars().all(char::is_whitespace) {
            // 中间有非空白内容 → 不是「相邻」，原样保留。
            out.push_str(before);
        } else if previous != Previous::EncodedWord {
            // 前面没有编码字（或者是明文）→ 这是明文与编码字之间的空白，保留。
            out.push_str(before);
        }

        // 解码。
        let bytes = match encoded {
            Encoding::Base64 => transfer::decode_base64(text.as_bytes()),
            Encoding::QuotedPrintable => transfer::decode_q_encoded_word(text.as_bytes()),
        };
        out.push_str(&charset::decode_lossy(&bytes, Some(&charset_label)));
        previous = Previous::EncodedWord;

        rest = &rest[start + consumed..];
    }

    out
}

/// 编码字里的编码方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// `B` —— base64。
    Base64,
    /// `Q` —— quoted-printable。
    QuotedPrintable,
}

/// 解析一个编码字。
///
/// 返回 `(charset, 编码方式, 文本, 消耗的字节数)`。
fn parse_encoded_word(input: &str) -> Option<(String, Encoding, String, usize)> {
    // `=?charset?B?text?=`
    let rest = input.strip_prefix("=?")?;
    let charset_end = rest.find('?')?;
    let charset_label = &rest[..charset_end];

    let rest = &rest[charset_end + 1..];
    let encoding_char = rest.chars().next()?;
    let encoding = match encoding_char.to_ascii_uppercase() {
        'B' => Encoding::Base64,
        'Q' => Encoding::QuotedPrintable,
        _ => return None,
    };

    let rest = &rest[encoding_char.len_utf8()..];
    let rest = rest.strip_prefix('?')?;

    let text_end = rest.find("?=")?;
    let text = &rest[..text_end];

    // charset 不能为空。
    if charset_label.trim().is_empty() {
        return None;
    }

    Some((
        charset::normalize_label(charset_label),
        encoding,
        text.to_string(),
        2 + charset_end + 1 + encoding_char.len_utf8() + 1 + text_end + 2,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(decode_encoded_words("普通主题"), "普通主题");
        assert_eq!(decode_encoded_words("hello"), "hello");
    }

    #[test]
    fn base64_utf8_encoded_word() {
        // "主题" 的 UTF-8 base64
        let input = "=?UTF-8?B?5Li76aKY?=";

        assert_eq!(decode_encoded_words(input), "主题");
    }

    #[test]
    fn base64_gb2312_encoded_word() {
        // "中文" 的 GB2312 → base64
        let input = "=?gb2312?B?1tDOxA==?=";

        assert_eq!(decode_encoded_words(input), "中文");
    }

    #[test]
    fn quoted_printable_encoded_word() {
        let input = "=?UTF-8?Q?主题?=";

        assert_eq!(decode_encoded_words(input), "主题");
    }

    #[test]
    fn mixed_plain_and_encoded() {
        let input = "Re: =?UTF-8?B?5Li76aKY?=";

        assert_eq!(decode_encoded_words(input), "Re: 主题");
    }

    #[test]
    fn adjacent_encoded_words_drop_the_whitespace_between() {
        // RFC 2047 §6.2：相邻编码字之间的空白要丢掉。
        let input = "=?UTF-8?B?5Lit?= =?UTF-8?B?5paH?=";

        assert_eq!(decode_encoded_words(input), "中文");
    }

    #[test]
    fn malformed_encoded_word_is_left_alone() {
        // 缺 `?=` → 不是编码字。
        let input = "=?UTF-8?B?5Li76aKY";

        assert_eq!(decode_encoded_words(input), input);
    }

    #[test]
    fn unknown_encoding_marker_is_left_alone() {
        let input = "=?UTF-8?X?5Li76aKY?=";

        assert_eq!(decode_encoded_words(input), input);
    }
}
