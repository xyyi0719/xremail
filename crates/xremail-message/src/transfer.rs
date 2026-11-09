//! MIME 传输编码（Content-Transfer-Encoding）。
//!
//! 实测（2026-10-23）QQ 的邮件用 `quoted-printable` 与 `8bit`，
//! 沃用 `7bit`。所以这三种都要处理，`base64` 也要（附件与部分正文）。

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// MIME base64 正文的标准行长（RFC 2045 §6.8）。
///
/// 超过这个长度的行很多客户端读不了，所以附件编码必须折行。
pub const BASE64_LINE_LEN: usize = 76;

/// 编码成 base64（**单行**，不折行）。
///
/// 用于 RFC 2047 的编码字载荷——RFC 2047 §5 明确要求编码字里不能有折行。
pub fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// 编码成 base64 并按 `width` 折行。
///
/// 用于 MIME 正文/附件（RFC 2045 §6.8 建议行长不超过 76）。
/// `width` 为 0 时退化成单行。
pub fn encode_base64_wrapped(bytes: &[u8], width: usize) -> String {
    let encoded = STANDARD.encode(bytes);
    if width == 0 {
        return encoded;
    }

    let mut out = String::with_capacity(encoded.len() + encoded.len() / width + 1);
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let end = (index + width).min(bytes.len());
        out.push_str(&encoded[index..end]);
        out.push('\r');
        out.push('\n');
        index = end;
    }
    out
}

/// 编码成 quoted-printable（RFC 2045 §6.7）。
///
/// 规则：
/// * `=` → `=3D`；
/// * 行尾的空白 → `=20` / `=09`（否则中间的 MTA 会把它吃掉）；
/// * 其它非可打印 ASCII 与全部非 ASCII 字节 → `=XX`；
/// * 行太长时用**软换行**（行尾的 `=`）折行，单行不超过 76 字符。
///
/// 输入里的换行（`\r\n` / `\n`）一律变成 CRLF 硬换行——MIME 正文只认 CRLF。
pub fn encode_quoted_printable(bytes: &[u8]) -> Vec<u8> {
    /// 单行最多 76 个字符（RFC 2045 §6.7）。
    const MAX_LINE: usize = 76;
    /// 一行里留给内容的字符数：76 − 1（软换行末尾的 `=`）− 2（行尾空白可能要变成 `=20`）。
    const MAX_CONTENT: usize = MAX_LINE - 3;

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + bytes.len() / 4);
    // 当前行：`(输出字节, 是否是空白)`。空白要等到行尾才知道要不要编码成 `=20`。
    let mut line: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut line_len = 0usize;

    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];

        // 硬换行：CRLF 或单独的 LF。
        if byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            flush_qp_line(&mut out, &mut line, &mut line_len);
            out.extend_from_slice(b"\r\n");
            index += 2;
            continue;
        }
        if byte == b'\n' {
            flush_qp_line(&mut out, &mut line, &mut line_len);
            out.extend_from_slice(b"\r\n");
            index += 1;
            continue;
        }

        let token = match byte {
            b'=' => (vec![b'=', b'3', b'D'], false),
            b' ' | b'\t' => (vec![byte], true),
            0x21..=0x7e => (vec![byte], false),
            _ => {
                let hex = b"0123456789ABCDEF";
                (
                    vec![b'=', hex[(byte >> 4) as usize], hex[(byte & 0x0f) as usize]],
                    false,
                )
            }
        };

        let token_len = token.0.len();
        if !line.is_empty() && line_len + token_len > MAX_CONTENT {
            // 放不下了：先把当前行写出去，再用软换行（行尾的 `=`）接上。
            flush_qp_line(&mut out, &mut line, &mut line_len);
            out.extend_from_slice(b"=\r\n");
        }
        line_len += token_len;
        line.push(token);
        index += 1;
    }

    flush_qp_line(&mut out, &mut line, &mut line_len);
    out
}

/// 把当前行写进输出。
///
/// 行尾若是空白，必须编码成 `=20` / `=09`——RFC 2045 §6.7 规定
/// 「行尾的空白必须被编码」，否则中间的传输层可能把它删掉。
fn flush_qp_line(out: &mut Vec<u8>, line: &mut Vec<(Vec<u8>, bool)>, line_len: &mut usize) {
    if let Some(last) = line.last_mut()
        && last.1
    {
        let byte = last.0[0];
        let hex = b"0123456789ABCDEF";
        last.0 = vec![b'=', hex[(byte >> 4) as usize], hex[(byte & 0x0f) as usize]];
    }

    for (bytes, _) in line.iter() {
        out.extend_from_slice(bytes);
    }
    line.clear();
    *line_len = 0;
}

/// 解码传输编码后的字节。
///
/// 未知编码按 `8bit` 处理（RFC 2045 §6.4：未知编码按 8bit 解读）。
pub fn decode_transfer(bytes: &[u8], encoding: Option<&str>) -> Vec<u8> {
    match encoding
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("base64") => decode_base64(bytes),
        Some("quoted-printable") => decode_quoted_printable(bytes),
        // 7bit / 8bit / binary / 未知：原样。
        _ => bytes.to_vec(),
    }
}

/// 解码 base64。
///
/// **容错**：RFC 2045 §6.8 规定「不在 base64 字母表里的字符一律忽略」。
/// 真实邮件里偶尔有脏数据（被别的软件改过），直接报错会丢掉整封正文。
pub fn decode_base64(bytes: &[u8]) -> Vec<u8> {
    // 只保留 base64 字母表与填充。
    let cleaned: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/' || *b == b'=')
        .collect();

    match STANDARD.decode(&cleaned) {
        Ok(decoded) => decoded,
        Err(_) => {
            // 逐块试：base64 以 4 字符为一组，丢掉不成组的尾巴。
            let mut out = Vec::new();
            let mut chunk = Vec::with_capacity(4);
            for byte in cleaned {
                chunk.push(byte);
                if chunk.len() == 4 {
                    if let Ok(part) = STANDARD.decode(&chunk) {
                        out.extend_from_slice(&part);
                    }
                    chunk.clear();
                }
            }
            out
        }
    }
}

/// 解码 RFC 2047 `Q` 编码词里的载荷。
///
/// **与 [`decode_quoted_printable`] 差一个字符**：RFC 2047 §4.2 规定 `Q` 编码里
/// `_` **代表空格**（"an underscore ... always represents a 'space'"），要按
/// `%20` / `=20` 一样理解。正文的 quoted-printable 里 `_` 就是字面下划线，
/// 所以这条规则**只能**用在编码词上，不能塞进 `decode_quoted_printable`。
///
/// 于是：先把 `_` 换成 `=20` 再走同一个 QP 解码器。比在解码器内部加参数好，
/// 因为 QP 解码器还要处理软换行、非法转义，两份实现迟早会走岔。
pub fn decode_q_encoded_word(payload: &[u8]) -> Vec<u8> {
    let mut swapped: Vec<u8> = Vec::with_capacity(payload.len());
    for &byte in payload {
        if byte == b'_' {
            swapped.extend_from_slice(b"=20");
        } else {
            swapped.push(byte);
        }
    }
    decode_quoted_printable(&swapped)
}

/// 解码 quoted-printable。
///
/// * `=XX` → 一个字节；
/// * 行尾的 `=` → 软换行，去掉；
/// * 其余原样。
pub fn decode_quoted_printable(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];

        if byte == b'=' {
            // 软换行：`=\r\n` 或 `=\n`
            if index + 1 < bytes.len()
                && bytes[index + 1] == b'\r'
                && index + 2 < bytes.len()
                && bytes[index + 2] == b'\n'
            {
                index += 3;
                continue;
            }
            if index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
                index += 2;
                continue;
            }

            // `=XX`
            if index + 2 < bytes.len() {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    out.push((high * 16 + low) as u8);
                    index += 3;
                    continue;
                }
            }

            // 不合法的 `=`：原样保留。
            out.push(byte);
            index += 1;
            continue;
        }

        out.push(byte);
        index += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        let raw = b"hello world";
        let encoded = STANDARD.encode(raw);

        assert_eq!(decode_base64(encoded.as_bytes()), raw);
    }

    #[test]
    fn base64_ignores_whitespace() {
        let encoded = "aGVs\r\nbG8g\r\nd29y bGQ=";

        assert_eq!(decode_base64(encoded.as_bytes()), b"hello world");
    }

    #[test]
    fn base64_skips_invalid_characters() {
        // 中间插了一个非法字符 `!`（RFC 2045：非字母表字符一律忽略）。
        let encoded = "aGVs!bG8=";

        assert_eq!(decode_base64(encoded.as_bytes()), b"hello");
    }

    #[test]
    fn quoted_printable_decodes_hex_escapes() {
        // "中文" 的 GB2312 字节被编码成 =D6=D0=CE=C4
        let input = b"=D6=D0=CE=C4";

        assert_eq!(decode_quoted_printable(input), vec![0xD6, 0xD0, 0xCE, 0xC4]);
    }

    #[test]
    fn quoted_printable_removes_soft_breaks() {
        let input = b"long=\r\nline";

        assert_eq!(decode_quoted_printable(input), b"longline");
    }

    #[test]
    fn quoted_printable_keeps_invalid_escapes() {
        let input = b"a=Zb";

        assert_eq!(decode_quoted_printable(input), b"a=Zb");
    }

    #[test]
    fn q_encoded_word_underscore_is_a_space() {
        // RFC 2047 §4.2：`_` 代表空格；`=5F` 才是字面下划线。
        assert_eq!(decode_q_encoded_word(b"hello_world"), b"hello world");
        assert_eq!(decode_q_encoded_word(b"a=5Fb"), b"a_b");
        // 正文的 QP 不受影响：那里 `_` 就是下划线本身。
        assert_eq!(decode_quoted_printable(b"hello_world"), b"hello_world");
    }

    #[test]
    fn unknown_encoding_passes_through() {
        let input = b"raw bytes";

        assert_eq!(decode_transfer(input, Some("8bit")), input);
        assert_eq!(decode_transfer(input, None), input);
        assert_eq!(decode_transfer(input, Some("x-nonsense")), input);
    }

    #[test]
    fn transfer_dispatches_to_the_right_decoder() {
        let raw = b"hello";

        assert_eq!(
            decode_transfer(&STANDARD.encode(raw).into_bytes(), Some("base64")),
            raw
        );
        assert_eq!(
            decode_transfer(b"=68=65=6C=6C=6F", Some("quoted-printable")),
            raw
        );
    }
}
