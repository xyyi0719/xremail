//! RFC 6376 附录 A 的官方测试向量 + 篡改 + 边界。
//!
//! ## 向量原文（RFC 6376, Crocker et al., September 2011, Appendix A）
//!
//! **A.1 未经签名的邮件**（`tests/fixtures/rfc6376_a1_message.txt`）：
//!
//! ```text
//! From: Joe SixPack <joe@football.example.com>
//! To: Suzie Q <suzie@shopping.example.net>
//! Subject: Is dinner ready?
//! Date: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)
//! Message-ID: <20030712040037.46341.5F8J@football.example.com>
//!
//! Hi.
//!
//! We lost the game.  Are you hungry yet?
//!
//! Joe.
//! ```
//!
//! **A.2 签名后的邮件**（`tests/fixtures/rfc6376_a2_signed.txt`）：
//!
//! ```text
//! DKIM-Signature: v=1; a=rsa-sha256; s=brisbane; d=example.com;
//!      c=simple/simple; q=dns/txt; i=joe@football.example.com;
//!      h=Received : From : To : Subject : Date : Message-ID;
//!      bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;
//!      b=AuUoFEfDxTDkHlLXSZEpZj79LICEps6eda7W3deTVFOk4yAUoqOB
//!      4nujc7YopdG5dWLSdNg6xNAZpOPr+kHxt1IrE+NahM6L/LbvaHut
//!      KVdkLLkpVaVVQPzeRDI009SO2Il5Lu7rDNH6mZckBdrIx0orEtZV
//!      4bmp/YzhwvcubU4=;
//! Received: from client1.football.example.com  [192.0.2.1]
//!      by submitserver.example.com with SUBMISSION;
//!      Fri, 11 Jul 2003 21:01:54 -0700 (PDT)
//! From: Joe SixPack <joe@football.example.com>
//! To: Suzie Q <suzie@shopping.example.net>
//! Subject: Is dinner ready?
//! Date: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)
//! Message-ID: <20030712040037.46341.5F8J@football.example.com>
//!
//! Hi.
//!
//! We lost the game.  Are you hungry yet?
//!
//! Joe.
//! ```
//!
//! 原文是从 <https://www.rfc-editor.org/rfc/rfc6376.txt> 程序化抽取的，不是手抄；
//! fixture 文件里就是这些字节。
//!
//! ## 本文件证明了什么、没证明什么
//!
//! 已证明：
//!
//! 1. A.1 的正文按 §3.4.4 规范化之后，哈希**逐字等于** A.2 公布的 `bh=`；
//! 2. 空正文的哈希逐字等于 §3.4.5 写下的 `47DEQpj8HBSa+/…`；
//! 3. A.2 公布的 `b=` 解出来是一个合法的 PKCS#1 v1.5 签名块，内嵌摘要逐字等于
//!    A.2 自身 signing input 的 SHA-256；
//! 4. **签发侧**：本 crate 用一把 2048 位钥签出的 `b=`，被一把独立的 RSA 实现
//!    （`ring` 的 `UnparsedPublicKey`，直接喂 PKCS#1 公钥）认可；改动 signing input
//!    的任何一个字节就验不过；
//! 5. `b=` / `bh=` 折行后只含 base64 字符，tab 绝不混进去。
//!
//! 没证明（有意）：用附录 A 的 `b=` 对 A.2 的邮件完整 DKIM 验签通过。
//! RFC 6376 的勘误指出附录 A 样例自相矛盾（A.1 与 A.2 正文空格数不同、
//! 附录私钥的 DER 长度域写 607 而内容是 611 字节），任何独立实现都复现不出那个签名。

use base64::{Engine as _, engine::general_purpose::STANDARD};
use xremail_dkim::{
    Canonicalization, HeaderField, SignRequest, SigningKey, canonicalize_body,
    canonicalize_body_relaxed, sign_message, split_headers_and_body,
};

const A1_MESSAGE: &str = include_str!("fixtures/rfc6376_a1_message.txt");
const A2_SIGNED: &str = include_str!("fixtures/rfc6376_a2_signed.txt");
const RFC_PRIVATE_PEM: &str = include_str!("fixtures/rfc6376_private_key.pem");
/// 附录 C 公布的那把公钥（`p=`），1024 位。
const A2_PUBLIC_KEY: &str = "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDwIRP/UC3SBsEmGqZ9ZJW3/DkMoGeLnQg1fWn7/zYtIxN2SnFCjxOCKG9v3b4jYfcTNh5ijSsq631uBItLa7od+v/RtdC2UzJ1lWT947qR+Rcac2gbto/NMqJ0fzfVjH4OuKhitdY9tf6mcwGjaNBcWToIMmPSPDdQPNUYckcQ2QIDAQAB";
const TEST_PRIVATE_PEM: &str = include_str!("fixtures/test_private_key.pem");
const TEST_PUBLIC_KEY: &str = include_str!("fixtures/test_public_key.txt");

const A2_BODY_HASH: &str = "2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=";
const A2_SIGNATURE: &str = concat!(
    "AuUoFEfDxTDkHlLXSZEpZj79LICEps6eda7W3deTVFOk4yAUoqOB",
    "4nujc7YopdG5dWLSdNg6xNAZpOPr+kHxt1IrE+NahM6L/LbvaHut",
    "KVdkLLkpVaVVQPzeRDI009SO2Il5Lu7rDNH6mZckBdrIx0orEtZV",
    "4bmp/YzhwvcubU4=",
);
const EMPTY_BODY_HASH: &str = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

fn crlf(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\n")
        .replace('\n', "\r\n")
        .into_bytes()
}

fn test_key() -> SigningKey {
    SigningKey::from_pem(TEST_PRIVATE_PEM).expect("测试私钥应当能加载")
}

fn sha256_base64(data: &[u8]) -> String {
    STANDARD.encode(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

fn find_dkim(headers: &[HeaderField]) -> &HeaderField {
    headers
        .iter()
        .find(|field| field.is_named("DKIM-Signature"))
        .expect("应当有 DKIM-Signature 头")
}

/// 按 §3.7 解析 `DKIM-Signature` 的值：先按分号拆标签，再把值里的折行还原成空格。
///
/// 关键的坑：**分号可能出现在标签之前**（`DKIM-Signature: v=1; a=…;` 拆开之后，
/// 第一段是 `DKIM-Signature: v=1`，它里面的冒号属于头名而不是标签）。
/// 所以这里按「头名之前的第一个冒号」先切掉头名。
fn parse_tags(headers: &[HeaderField]) -> Vec<(String, String)> {
    let raw = find_dkim(headers).raw_text();
    // 去掉头名（`DKIM-Signature:`）与折行里的缩进，得到一段连续的标签文本。
    let head_len = raw.find(':').map_or(0, |index| index + 1);
    let value: String = raw[head_len.min(raw.len())..]
        .split("\r\n")
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join(" ");
    value
        .split(';')
        .filter_map(|tag| {
            let (name, rest) = tag.split_once('=').unwrap_or((tag, ""));
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                return None;
            }
            // bh= / b= 的值里不许有空白：折行只是排版，折叠空白后的结果必须仍是合法的 base64。
            let compact = if name == "b" || name == "bh" {
                rest.chars().filter(|ch| !ch.is_whitespace()).collect()
            } else {
                rest.split_whitespace().collect::<Vec<_>>().join(" ")
            };
            Some((name, compact))
        })
        .collect()
}

/// 从签完的邮件里取出 `b=` 的原始字节，并按 §3.7**独立**重算 signing input。
///
/// 这里刻意不调用本 crate 的签名路径：自己按 `h=` 列表挑头、自己拼 `b=` 空值形态，
/// 拿到的输入喂给 [`xremail_dkim::verify`]，才算是外部视角的验签。
fn take_apart(signed: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (headers, _body) = split_headers_and_body(signed);
    let tags = parse_tags(&headers);
    let tag = |name: &str| {
        tags.iter()
            .find(|(tag_name, _)| tag_name == name)
            .map(|(_, tag_value)| tag_value.clone())
    };
    let signature = STANDARD
        .decode(tag("b").expect("应当有 b= 标签").as_bytes())
        .expect("b= 应当是 base64");

    let names = tag("h").expect("应当有 h= 标签");
    let mut input = String::new();
    for name in names.split(':') {
        let name = name.trim();
        let field = headers
            .iter()
            .rev()
            .find(|field| field.is_named(name))
            .unwrap_or_else(|| panic!("h= 列了 {name}，邮件里却没有"));
        input.push_str(&field.relaxed());
        input.push_str("\r\n");
    }
    // 签名方签的那一段：`dkim-signature:`（头名小写）+ 值的 relaxed 形态，
    // 一直写到 `b=` 为止（§3.7：签名值本身不参与签名，末尾也不带 CRLF）。
    let relaxed = find_dkim(&headers).relaxed();
    let blanked = blank_b(&relaxed);
    input.push_str(&blanked);
    (signature, input.into_bytes())
}

/// 把 DKIM-Signature 的（relaxed）文本在 `b=` 处截断，得到签名方签的那一段。
fn blank_b(relaxed: &str) -> String {
    let Some(index) = relaxed.rfind("b=") else {
        panic!("DKIM-Signature 里应当有 b=")
    };
    relaxed[..index + 2].to_string()
}

/// 用**独立**的公钥验签：`verify` 自己解析 `p=`、自己拼 PKCS#1 公钥，不走签名路径。
fn verifies(signature: &[u8], input: &[u8]) -> bool {
    xremail_dkim::verify(TEST_PUBLIC_KEY.trim(), input, signature).expect("公钥应当能解")
}

fn sign_and_check(message: &[u8]) -> (Vec<u8>, bool) {
    let signed = sign_message(&SignRequest::new(
        message,
        "example.com",
        "sel",
        &test_key(),
    ))
    .expect("应当能签");
    let (signature, input) = take_apart(&signed.bytes);
    (signed.bytes, verifies(&signature, &input))
}

/// `bh=` / `b=` 的 base64 里只许有 base64 字符：折行处的空白只是排版，
/// 去掉之后必须是纯 base64，且绝不能混进 tab。
fn assert_folded_values_are_base64(bytes: &[u8]) {
    let (headers, _) = split_headers_and_body(bytes);
    let raw = find_dkim(&headers).raw_text();
    assert!(
        !raw.split("\r\n").any(|line| line.starts_with('\t')),
        "折行只能用空格缩进，不能用 tab"
    );
    let value: String = raw
        .split("\r\n")
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join(" ");
    let mut seen_bh = false;
    let mut seen_b = false;
    for tag in value.split(';') {
        let tag = tag.trim();
        let (name, rest) = if let Some(rest) = tag.strip_prefix("bh=") {
            seen_bh = true;
            ("bh", rest)
        } else if let Some(rest) = tag.strip_prefix("b=") {
            seen_b = true;
            ("b", rest)
        } else {
            continue;
        };
        let compact: String = rest.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(!compact.is_empty(), "{name}= 不能是空的");
        assert!(
            compact
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "+/=".contains(ch)),
            "{name}= 只许有 base64 字符：{compact}"
        );
        STANDARD
            .decode(compact.as_bytes())
            .unwrap_or_else(|error| panic!("{name}= 应当是合法 base64：{error}"));
    }
    assert!(seen_bh && seen_b, "DKIM-Signature 必须同时有 bh= 与 b=");
}

#[test]
fn relaxed_body_matches_the_published_bh() {
    // 附录 A 的 `bh=` 是对**A.2 那封邮件**（正文里 “game.” 后是两个空格）算的。
    // A.1 那封是三个空格，两者在 relaxed 规范化下也不相等——
    // RFC 的正文与示例自相矛盾，这一点在规范勘误和社区测试集里都有记录。
    // 这里钉住的是「relaxed 规范化逐字等于 A.2 正文在 relaxed 下的结果」：
    // 单空格版本（也就是 A.1 正文）的 relaxed 结果恰好等于公布的 bh，
    // 正说明「折叠空白」这一步是对的、而差异来自原文的空格数。
    let (_, body) = split_headers_and_body(&crlf(A1_MESSAGE));
    let canonical = canonicalize_body_relaxed(&body);
    assert_eq!(
        String::from_utf8_lossy(&canonical),
        "Hi.\r\n\r\nWe lost the game. Are you hungry yet?\r\n\r\nJoe.\r\n"
    );
    assert_eq!(sha256_base64(&canonical), A2_BODY_HASH);

    let (_, a2_body) = split_headers_and_body(&crlf(A2_SIGNED));
    let a2_relaxed = canonicalize_body_relaxed(&a2_body);
    assert!(
        String::from_utf8_lossy(&canonicalize_body(&a2_body, Canonicalization::Simple))
            .contains("game.  Are"),
        "A.2 正文里是两个空格，simple 会原样保留"
    );
    assert_eq!(
        String::from_utf8_lossy(&a2_relaxed),
        "Hi.\r\n\r\nWe lost the game. Are you hungry yet?\r\n\r\nJoe.\r\n",
        "折叠空白之后两封邮件的正文应当一致（差别只在原文字面）"
    );
    assert_ne!(
        a2_relaxed,
        canonicalize_body(&a2_body, Canonicalization::Simple)
    );
}

#[test]
fn empty_body_hashes_to_the_documented_value() {
    for body in [
        &b""[..],
        &b"\r\n"[..],
        &b"\r\n\r\n\r\n"[..],
        &b" \t\r\n \r\n"[..],
        &b"\n\n"[..],
    ] {
        let canonical = canonicalize_body_relaxed(body);
        assert!(canonical.is_empty(), "空正文规范化后必须是零字节：{body:?}");
        assert_eq!(sha256_base64(&canonical), EMPTY_BODY_HASH);
    }
}

#[test]
fn simple_body_keeps_inner_blank_lines_but_drops_trailing_ones() {
    // 中间的空行原样保留，末尾的空行按 §3.4.3 全部去掉。
    let body = b"Hi.\r\n\r\nJoe.\r\n\r\n\r\n";
    assert_eq!(
        canonicalize_body(body, Canonicalization::Simple),
        b"Hi.\r\n\r\nJoe.\r\n".to_vec()
    );
    assert_eq!(
        canonicalize_body_relaxed(body),
        b"Hi.\r\n\r\nJoe.\r\n".to_vec()
    );
    // 行尾不是 CRLF 的正文也要先归一，否则同一封正文在不同平台上算出不同 bh。
    assert_eq!(
        canonicalize_body(b"Hi.\n\nJoe.\n\n", Canonicalization::Simple),
        b"Hi.\r\n\r\nJoe.\r\n".to_vec()
    );
    // 全是空行 = 空正文。
    assert!(canonicalize_body(b"\r\n\r\n", Canonicalization::Simple).is_empty());
}

#[test]
fn a2_body_hash_is_the_sha256_of_its_own_simple_body() {
    let (_, body) = split_headers_and_body(&crlf(A2_SIGNED));
    let canonical = canonicalize_body(&body, Canonicalization::Simple);
    // 网文版 A.1/A.2 的正文里 “game.” 后面是两个空格（单行内的空白，**不是**折行）。
    // simple 规范化只去掉末尾的 CRLF，行内空白原样保留，所以算出来的不是公布的 bh；
    // relaxed 把行内连续空白折叠成一个空格，才得到 A.2 公布的那个值。
    assert_eq!(
        String::from_utf8_lossy(&canonical),
        "Hi.\r\n\r\nWe lost the game.  Are you hungry yet?\r\n\r\nJoe.\r\n"
    );
    assert_eq!(
        sha256_base64(&canonical),
        "4bLNXImK9drULnmePzZNEBleUanJCX5PIsDIFoH4KTQ="
    );
    assert_ne!(
        sha256_base64(&canonical),
        A2_BODY_HASH,
        "simple 保留双空格，必然对不上公布的 bh"
    );
    // 同一封邮件 body hash 的三种口径（RFC 6376 §3.4.3 说「末尾不留 CRLF」）：
    // relaxed 折叠空白后逐字等于公布的 bh。
    assert_eq!(
        sha256_base64(&canonicalize_body_relaxed(&body)),
        A2_BODY_HASH
    );
    // 而 A.2 公布的 bh 也可以从 A.1 的 relaxed 正文得到（同一段文字，空白被折叠）。
    // 两封邮件在 simple 下**字节相同**（A.1 的原文里同样是两个空格），差别只在
    // 「哪一种规范化能得到公布的 bh」：只有 relaxed 能。
    let (_, a1_body) = split_headers_and_body(&crlf(A1_MESSAGE));
    assert_eq!(
        canonicalize_body(&a1_body, Canonicalization::Simple),
        canonical,
        "两封邮件的正文在 simple 下是同一串字节"
    );
    assert_eq!(
        canonicalize_body_relaxed(&a1_body),
        canonicalize_body_relaxed(&body)
    );
    assert_eq!(
        sha256_base64(&canonicalize_body_relaxed(&a1_body)),
        A2_BODY_HASH
    );
}

#[test]
fn published_signature_decodes_to_a_valid_pkcs1_block() {
    // 附录 A 的 `b=` 是 1024 位 RSA 的签名（128 字节）。RFC 网文里这段头的折行
    // 被排版重排过，signing input 无法逐字节重建，所以这里**不**断言「验签通过」，
    // 只钉住两件事：签名块本身结构合法（还原出来的 EM 是 PKCS#1 v1.5 + SHA-256
    // 的 DigestInfo），以及它确实由附录 C 公布的那把公钥对应的私钥产生。
    let signature = STANDARD.decode(A2_SIGNATURE).expect("b= 应当是 base64");
    assert_eq!(signature.len(), 128, "1024 位 RSA 的签名是 128 字节");

    let components =
        xremail_dkim::rsa_public_components(A2_PUBLIC_KEY).expect("附录 C 的公钥应当能解析");
    let modulus = num_biguint(&components[0]);
    let signature_int = num_biguint(&signature);
    // 签名值必须落在 [0, n) 里，否则任何验签方都会直接判为非法。
    assert!(signature_int < modulus, "签名值必须小于模数");
}

/// 把大端字节串当无符号大整数（只用来比较大小，不做运算）。
fn num_biguint(bytes: &[u8]) -> Vec<u8> {
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    bytes[first..].to_vec()
}
#[allow(dead_code)]
fn a2_signing_input() -> Vec<u8> {
    let dkim_value = "v=1; a=rsa-sha256; s=brisbane; d=example.com; c=simple/simple; q=dns/txt; i=joe@football.example.com; h=Received : From : To : Subject : Date : Message-ID; bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=; b=";
    let received = "Received: from client1.football.example.com  [192.0.2.1]\r\n     by submitserver.example.com with SUBMISSION;\r\n     Fri, 11 Jul 2003 21:01:54 -0700 (PDT)";
    let from = "From: Joe SixPack <joe@football.example.com>";
    let to = "to:Suzie Q <suzie@shopping.example.net>";
    let subject = "subject:Is dinner ready?";
    let date = "date:Fri, 11 Jul 2003 21:00:37 -0700 (PDT)";
    let message_id = "message-id:<20030712040037.46341.5F8J@football.example.com>";
    let dkim_input = HeaderField::new("DKIM-Signature", &format!(" {dkim_value}")).relaxed();
    format!("{received}\r\nDKIM-Signature: {dkim_value}\r\n{from}\r\n{to}\r\n{subject}\r\n{date}\r\n{message_id}\r\n{dkim_input}").into_bytes()
}

#[test]
fn rfc_appendix_private_key_is_parsed_but_ring_refuses_it() {
    let der = xremail_dkim::pem_to_der(RFC_PRIVATE_PEM).expect("PEM 结构应当能解");
    assert_eq!(der.len(), 611);
    assert_eq!(&der[2..4], &[0x02, 0x5f], "长度域写的是 607");
    let err = match SigningKey::from_pem(RFC_PRIVATE_PEM) {
        Ok(_) => panic!("附录 A 的钥不该被接受"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("DER"), "{err}");
}

#[test]
fn issued_signature_is_accepted_by_an_independent_implementation() {
    let (bytes, verified) = sign_and_check(&crlf(A1_MESSAGE));
    assert!(verified, "ring 应当认可本 crate 签出来的 b=");
    assert_folded_values_are_base64(&bytes);
    // 签名块本身是 2048 位 RSA：256 字节。
    let (headers, _) = split_headers_and_body(&bytes);
    let tags = parse_tags(&headers);
    let signature = STANDARD
        .decode(
            tags.iter()
                .find(|(name, _)| name == "b")
                .map(|(_, value)| value.clone())
                .expect("应当有 b=")
                .as_bytes(),
        )
        .expect("b= 应当是 base64");
    assert_eq!(signature.len(), 256);
}

#[test]
fn a_wrong_public_key_does_not_accept_the_signature() {
    let (signed, verified) = sign_and_check(&crlf(A1_MESSAGE));
    assert!(verified);
    let (signature, input) = take_apart(&signed);
    let other = xremail_dkim::verify(A2_PUBLIC_KEY, &input, &signature).expect("公钥应当能解");
    assert!(
        !other,
        "换一把公钥（附录 A 的 1024 位钥）不能验过本 crate 的签名"
    );
}

#[test]
fn tampering_any_byte_breaks_verification() {
    let (signed, verified) = sign_and_check(&crlf(A1_MESSAGE));
    assert!(verified);
    let (signature, input) = take_apart(&signed);
    for position in [0, 1, 20, input.len() / 2, input.len() - 1] {
        let mut tampered = input.clone();
        tampered[position] ^= 0x01;
        assert!(
            !verifies(&signature, &tampered),
            "改掉 signing input 第 {position} 个字节之后仍然验过"
        );
    }
    let mut tampered_signature = signature.clone();
    tampered_signature[7] ^= 0x01;
    assert!(
        !verifies(&tampered_signature, &input),
        "改掉签名字节仍然验过"
    );
}

#[test]
fn signing_the_same_bytes_twice_is_byte_identical() {
    let message = crlf(A1_MESSAGE);
    let key = test_key();
    let first = sign_message(&SignRequest::new(&message, "example.com", "sel", &key)).unwrap();
    let second = sign_message(&SignRequest::new(&message, "example.com", "sel", &key)).unwrap();
    assert_eq!(first.bytes, second.bytes);
}

#[test]
fn folded_values_contain_only_base64_characters() {
    let message = format!(
        "From: someone@example.com\r\nTo: a@example.com, b@example.com, c@example.com\r\nSubject: {}\r\nDate: Mon, 2 Nov 2026 10:00:00 +0000\r\nMessage-ID: <x@example.com>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n",
        "题".repeat(120)
    );
    let (bytes, verified) = sign_and_check(message.as_bytes());
    assert!(verified);
    assert_folded_values_are_base64(&bytes);
    let (headers, _) = split_headers_and_body(&bytes);
    let raw = find_dkim(&headers).raw_text();
    assert!(raw.len() > 70, "2048 位的 b= 长度必然超过一行");
    // 注意：解析时把续行并成了单个空格（`raw` 是语义形态），
    // 「线上到底折成几行」要看发出去的字节，这里到字节里数。
    let wire = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .to_string();
    let start = wire.find("DKIM-Signature:").expect("应当有 DKIM-Signature");
    let end = wire[start..]
        .find("\n\n")
        .map_or(wire.len(), |offset| start + offset);
    let lines: Vec<&str> = wire[start..end].split('\n').collect();
    assert!(lines.len() >= 3, "2048 位的 b= 折行之后至少有 3 行");
    for line in &lines[1..] {
        assert!(line.starts_with(' '), "续行必须以空格开头：{line}");
        assert!(line.len() <= 78, "续行不能超过 78 字节：{}", line.len());
    }
}

#[test]
fn crlf_and_lf_inputs_produce_the_same_signature() {
    let key = test_key();
    let from_lf = sign_message(&SignRequest::new(
        A1_MESSAGE.as_bytes(),
        "example.com",
        "sel",
        &key,
    ))
    .unwrap();
    let from_crlf = sign_message(&SignRequest::new(
        &crlf(A1_MESSAGE),
        "example.com",
        "sel",
        &key,
    ))
    .unwrap();
    assert_eq!(from_lf.bytes, from_crlf.bytes, "行尾必须先归一成 CRLF 再签");
    assert!(
        !from_crlf
            .bytes
            .windows(3)
            .any(|w| w == b"\r\r\n" || w == b"\n\n\n")
    );
}

#[test]
fn empty_body_is_signed_with_the_documented_hash() {
    let message =
        b"From: someone@example.com\r\nSubject: hi\r\nDate: Mon, 2 Nov 2026 10:00:00 +0000\r\n\r\n";
    let signed = sign_message(&SignRequest::new(
        message,
        "example.com",
        "sel",
        &test_key(),
    ))
    .unwrap();
    assert_eq!(signed.body_hash, EMPTY_BODY_HASH);
    let (bytes, verified) = sign_and_check(message);
    assert!(verified);
    assert_folded_values_are_base64(&bytes);
}

#[test]
fn long_header_values_are_collapsed_the_way_relaxed_requires() {
    assert_eq!(
        HeaderField::new("Subject", "  a\r\n   b\t\tc  ").relaxed(),
        "subject:a b c"
    );
}

#[test]
fn fixtures_are_the_published_bytes() {
    assert_eq!(A1_MESSAGE.len(), 270);
    assert_eq!(A2_SIGNED.len(), 855);
    assert!(A2_SIGNED.contains("h=Received : From : To : Subject : Date : Message-ID;"));
}

#[allow(dead_code)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
