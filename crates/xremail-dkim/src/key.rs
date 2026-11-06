//! 私钥解析与 `rsa-sha256` 签名 / 验签。
//!
//! 密钥材料只在内存里经过一次：PEM → base64 → DER（必要时剥掉 PKCS#8 外壳）
//! → ring 的 `RsaKeyPair`。本模块**不**做任何文件或钥匙串 IO，
//! 也不打印密钥内容（错误信息一律是固定文案，不回显输入）。

use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ring::rand::SystemRandom;
use ring::signature::{self, RsaKeyPair};
use zeroize::Zeroizing;

use crate::error::DkimError;

/// DKIM 支持的公钥算法（RFC 6376 §3.3 的 `a=` 标签）。
///
/// 只认 `rsa-sha256`：`rsa-sha1` 已被 RFC 8301 弃用，`ed25519-sha256`
/// （RFC 8463）需要另外一把钥匙的实现，留到以后再加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SigningAlgorithm {
    /// `rsa-sha256`：RSASSA-PKCS1-v1_5 + SHA-256。
    #[default]
    RsaSha256,
}

impl SigningAlgorithm {
    /// `a=` 标签的值。
    pub fn as_str(self) -> &'static str {
        match self {
            SigningAlgorithm::RsaSha256 => "rsa-sha256",
        }
    }

    /// 按 `a=` 标签解析；不认识的算法返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "rsa-sha256" => Some(SigningAlgorithm::RsaSha256),
            _ => None,
        }
    }
}

/// 一把能用来签 DKIM 的 RSA 私钥。
///
/// 刻意不实现 `Debug` / `Serialize`：这种类型不该出现在日志或 IPC 里。
pub struct SigningKey {
    key: RsaKeyPair,
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SigningKey(<redacted>)")
    }
}

impl SigningKey {
    /// 从 PKCS#1（`RSAPrivateKey`）或 PKCS#8（`PrivateKeyInfo`）的 DER 加载。
    ///
    /// ring 只接受 2048 / 3072 / 4096 位的 RSA 私钥；1024 位会得到一句
    /// 「太短，请换一把」而不是笼统的失败——这是有意为之（RFC 8301 也不建议 1024）。
    pub fn from_der(der: &[u8]) -> Result<Self, DkimError> {
        RsaKeyPair::from_der(der)
            .or_else(|_| RsaKeyPair::from_pkcs8(der))
            .map(|key| SigningKey { key })
            .map_err(describe_key_rejection)
    }

    /// 从 PEM 文本加载（PKCS#1 或 PKCS#8，均不得带口令）。
    pub fn from_pem(pem: &str) -> Result<Self, DkimError> {
        let der = pem_to_der(pem)?;
        Self::from_der(&der)
    }

    /// 模长（位）。
    pub fn bits(&self) -> usize {
        self.key.public().modulus_len() * 8
    }

    /// 用这把钥匙签一段字节。
    pub fn sign(&self, algorithm: SigningAlgorithm, message: &[u8]) -> Result<Vec<u8>, DkimError> {
        let padding: &dyn signature::RsaEncoding = match algorithm {
            SigningAlgorithm::RsaSha256 => &signature::RSA_PKCS1_SHA256,
        };
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(padding, &SystemRandom::new(), message, &mut signature)
            .map_err(|_| DkimError::Sign("RSA 签名运算失败".to_string()))?;
        Ok(signature)
    }

    /// 直接对一段字节签名，返回「用的算法 + 签名」。
    ///
    /// 给**录入自检**用：把私钥粘进设置页时，先签一段固定内容、再用自己的公钥
    /// 验回来，确保这把钥既能签也能验（错的钥在收信方那边才会暴露）。
    pub fn sign_raw(&self, message: &[u8]) -> Result<(SigningAlgorithm, Vec<u8>), DkimError> {
        let algorithm = SigningAlgorithm::default();
        let signature = self.sign(algorithm, message)?;
        Ok((algorithm, signature))
    }
}

/// 把 PEM 文本解成 DER。
///
/// 只做「去掉那两行包裹、base64 解码、必要时剥一层 PKCS#8 外壳」，
/// 不解析私钥内容本身（那是 ring 的事）。
pub fn pem_to_der(pem: &str) -> Result<Zeroizing<Vec<u8>>, DkimError> {
    const PKCS1: &str = "-----BEGIN RSA PRIVATE KEY-----";
    const PKCS1_END: &str = "-----END RSA PRIVATE KEY-----";
    const PKCS8: &str = "-----BEGIN PRIVATE KEY-----";
    const PKCS8_END: &str = "-----END PRIVATE KEY-----";
    const ENCRYPTED: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";

    let pem = pem.trim();
    if pem.contains(ENCRYPTED) {
        return Err(DkimError::Key(
            "不支持带口令的加密私钥，请导出未加密的 PKCS#8 或 PKCS#1 私钥",
        ));
    }

    let (body, pkcs8) = if let Some(rest) = pem.strip_prefix(PKCS1) {
        (rest.split(PKCS1_END).next().unwrap_or(""), false)
    } else if let Some(rest) = pem.strip_prefix(PKCS8) {
        (rest.split(PKCS8_END).next().unwrap_or(""), true)
    } else {
        return Err(DkimError::Key(
            "私钥要写成 PEM：-----BEGIN PRIVATE KEY----- 或 -----BEGIN RSA PRIVATE KEY-----",
        ));
    };

    let compact: String = body.chars().filter(|ch| !ch.is_whitespace()).collect();
    if compact.is_empty() {
        return Err(DkimError::Key("私钥 PEM 里没有内容"));
    }
    let der = Zeroizing::new(
        BASE64
            .decode(compact.as_bytes())
            .map_err(|_| DkimError::Key("私钥 PEM 的 base64 不合法"))?,
    );

    if pkcs8 {
        unwrap_pkcs8(&der).map(Zeroizing::new)
    } else {
        Ok(der)
    }
}

/// 把 ring 的拒绝理由翻成一句用户能照着做的话。
///
/// **不要**把 `KeyRejected` 的 `Debug` 直接暴露给用户：`TooSmall`、
/// `InvalidEncoding` 这类词对填设置页的人毫无意义。
fn describe_key_rejection(error: ring::error::KeyRejected) -> DkimError {
    let text = format!("{error:?}");
    if text.contains("InvalidEncoding") {
        return DkimError::Key(
            "私钥的 DER 结构不完整（RFC 6376 附录 A 的样例钥本身少 4 个字节，是已知勘误；请换成自己生成的钥）",
        );
    }
    if text.contains("TooSmall") {
        return DkimError::Key(
            "这把私钥太短：DKIM 签名器只接受 2048 / 3072 / 4096 位的 RSA 私钥（1024 位请换一把）",
        );
    }
    if text.contains("Exponent") {
        return DkimError::Key(
            "这把 RSA 私钥的参数不合法（p 须小于 q 且互素，公钥指数应为 65537）",
        );
    }
    DkimError::Key("这不是一把可用的 RSA 私钥（只支持未加密的 PKCS#1 / PKCS#8）")
}

/// 用 DNS 里发布的公钥验一个 `rsa-sha256` 签名（验签方的视角）。
///
/// * `selector_public_key`：DNS TXT 里 `p=` 的原值（base64 编码的 DER 公钥）。
///   按 RFC 6376 §3.6.1，常见写法是完整的 SubjectPublicKeyInfo；
///   只放 `RSAPublicKey`（PKCS#1）的写法也接受。
/// * `message`：被签的邮件字节。
/// * `signature`：`b=` 解码后的原始字节。
///
/// 返回 `Ok(true)` 表示签名对得上，`Ok(false)` 表示对不上（篡改、密钥不符）。
pub fn verify(
    selector_public_key: &str,
    message: &[u8],
    signature: &[u8],
) -> Result<bool, DkimError> {
    let n_e = rsa_public_components(selector_public_key)?;
    verify_with_components(&n_e, message, signature)
}

/// 把一批公钥（keychain 里的 PEM，或 DNS 里 `p=` 的 base64）摊平成候选 `[n, e]`。
///
/// 多钥轮换时用：DNS 里可能同时挂着新旧两把，随便哪把验得过都算过。
pub fn candidate_components(keys: &[&str]) -> Vec<PublicComponents> {
    keys.iter()
        .filter_map(|key| rsa_public_components(key).ok())
        .collect()
}

/// 逐把候选公钥去验签：只要有一把把签名验过就算通过。
pub fn verify_any(components: &[PublicComponents], message: &[u8], signature: &[u8]) -> bool {
    components
        .iter()
        .any(|n_e| verify_with_components(n_e, message, signature).unwrap_or(false))
}

/// 公钥按约定拆成 `[模数, 指数]` 两段裸字节（DER 补位已去）。
type RsaPublicComponents = [Vec<u8>; 2];
/// 公钥拆出来的 `[模数, 指数]`（DER 补位已去），对外给验签工具用。
pub type PublicComponents = RsaPublicComponents;

/// 把 DNS 里 `p=` 的公钥（SubjectPublicKeyInfo 或 PKCS#1 `RSAPublicKey`）
/// 拆成裸的模数与指数；返回**(去掉 DER 补位后的)n 与 e**。
pub fn rsa_public_components(selector_public_key: &str) -> Result<RsaPublicComponents, DkimError> {
    let compact: String = selector_public_key
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let der = BASE64
        .decode(compact.as_bytes())
        .map_err(|_| DkimError::Config("公钥不是合法的 base64".to_string()))?;

    // ring 的 RSA 验签要的是**裸模数与指数**，不认 SPKI 外壳，所以先拆开。
    // 两种常见形状：完整 SubjectPublicKeyInfo，或直接 PKCS#1 RSAPublicKey。
    // 逐项遍历 DER（TLV），而不是靠「跳过几项」——DER 里任何一项都可能
    // 用长格式长度，位置推算很容易错位。
    let (tag, spki_content) =
        read_tlv(&der).ok_or_else(|| DkimError::Config("公钥不是 DER".to_string()))?;
    if tag != 0x30 {
        return Err(DkimError::Config("公钥不是 DER SEQUENCE".to_string()));
    }
    let first = read_tlv(spki_content);
    // SPKI 与 PKCS#1 都以 SEQUENCE 开头；区别在第二个孩子是不是 BIT STRING。
    let is_spki = matches!(
        (first, tlv_rest(spki_content, 1).and_then(read_tlv)),
        (Some((0x30, _)), Some((0x03, _)))
    );
    let (n, e) = if is_spki {
        // SPKI：SEQUENCE { AlgorithmIdentifier(SEQUENCE), BIT STRING { 0x00, RSAPublicKey } }
        if let Some((_tag, _algorithm)) = first {
            let rest = tlv_rest(spki_content, 1)
                .ok_or_else(|| DkimError::Config("公钥里缺 BIT STRING".to_string()))?;
            let (tag, bit_string) = read_tlv(rest)
                .ok_or_else(|| DkimError::Config("公钥里缺 BIT STRING".to_string()))?;
            if tag != 0x03 {
                return Err(DkimError::Config("公钥里缺 BIT STRING".to_string()));
            }
            // BIT STRING 首字节是「未使用位数」，RSA 公钥固定为 0。
            let Some((0, key_bytes)) = bit_string.split_first() else {
                return Err(DkimError::Config("公钥的 BIT STRING 没有对齐".to_string()));
            };
            // 比特串里应当直接是 RSAPublicKey 的 TLV；个别工具会把内容再包一层
            // OCTET STRING，两种都认。
            let (tag, body) = match read_tlv(key_bytes) {
                Some((0x04, inner)) => read_tlv(inner)
                    .ok_or_else(|| DkimError::Config("公钥不是 RSA 公钥".to_string()))?,
                other => other.ok_or_else(|| DkimError::Config("公钥不是 RSA 公钥".to_string()))?,
            };
            if tag != 0x30 {
                return Err(DkimError::Config("公钥不是 RSA 公钥".to_string()));
            }
            read_modulus_and_exponent(body)?
        } else {
            return Err(DkimError::Config("公钥结构不认识".to_string()));
        }
    } else {
        // 直接给的就是 PKCS#1 RSAPublicKey：SEQUENCE { INTEGER n, INTEGER e }。
        read_modulus_and_exponent(spki_content)?
    };

    Ok([n.to_vec(), e.to_vec()])
}

/// 拿拆好的 `[n, e]` 去验签（`rsa-sha256` 专用）。
pub fn verify_with_components(
    components: &[Vec<u8>],
    message: &[u8],
    signature: &[u8],
) -> Result<bool, DkimError> {
    let [n, e] = components else {
        return Err(DkimError::Config("公钥里缺模数或指数".to_string()));
    };
    // 去掉 DER 给无符号整数补的 0x00 前导位。
    let n = n.strip_prefix(&[0u8][..]).unwrap_or(n);
    let e = e.strip_prefix(&[0u8][..]).unwrap_or(e);

    let rsa_public = encode_rsa_public_key(n, e);
    let accept = |algorithm: &'static dyn signature::VerificationAlgorithm| {
        signature::UnparsedPublicKey::new(algorithm, &rsa_public)
            .verify(message, signature)
            .is_ok()
    };
    // 2048 位及以上走常规算法；拿不准就再试一次「1024 起」的旧算法
    // （1024 位的钥只被后者接受，RFC 6376 §3.3.3 要求验签方能收 1024）。
    Ok(accept(&signature::RSA_PKCS1_2048_8192_SHA256)
        || accept(&signature::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY))
}

/// 公钥的 ASN.1 DER 编码：`SEQUENCE { INTEGER n, INTEGER e }`（PKCS#1 `RSAPublicKey`，
/// 也正是 ring 的 `UnparsedPublicKey` 想要的输入）。
///
/// 长度域按 DER 最短形式写（短格式 / 0x81 / 0x82 / 0x83），对 RSA 公钥足够。
pub(crate) fn encode_rsa_public_key(n: &[u8], e: &[u8]) -> Vec<u8> {
    // 写一个 INTEGER 的 TLV；DER 要求正数最高位为 1 时补 0x00。
    fn push_integer(out: &mut Vec<u8>, value: &[u8]) {
        let needs_pad = value.first().is_some_and(|byte| byte & 0x80 != 0);
        let content_len = value.len() + usize::from(needs_pad);
        out.push(0x02);
        push_der_len(out, content_len);
        if needs_pad {
            out.push(0x00);
        }
        out.extend_from_slice(value);
    }

    let mut body = Vec::with_capacity(n.len() + e.len() + 8);
    push_integer(&mut body, n);
    push_integer(&mut body, e);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(0x30);
    push_der_len(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

/// 从 PKCS#8 `PrivateKeyInfo` 里取出 `privateKey` OCTET STRING（即 PKCS#1 结构）。
///
/// 结构（RFC 5208）：
///
/// ```text
/// PrivateKeyInfo ::= SEQUENCE {
///     version             INTEGER (0),
///     privateKeyAlgorithm AlgorithmIdentifier,   -- rsaEncryption
///     privateKey          OCTET STRING           -- RSAPrivateKey（PKCS#1）
///   }
/// ```
///
/// 只认这一个形状；形状不对统一报「不是 RSA 私钥」，不猜。
fn unwrap_pkcs8(der: &[u8]) -> Result<Vec<u8>, DkimError> {
    let (tag, outer) = read_tlv(der).ok_or(DkimError::Key("PKCS#8 结构不完整"))?;
    if tag != 0x30 {
        return Err(DkimError::Key("PKCS#8 顶层不是 SEQUENCE"));
    }
    let (tag, _version) = read_tlv(outer).ok_or(DkimError::Key("PKCS#8 缺 version"))?;
    if tag != 0x02 {
        return Err(DkimError::Key("PKCS#8 的 version 不是 INTEGER"));
    }
    // 逐项遍历 version → AlgorithmIdentifier → OCTET STRING。
    let after_version = tlv_rest(outer, 1).ok_or(DkimError::Key("PKCS#8 缺 privateKey 字段"))?;
    let after_alg =
        tlv_rest(after_version, 1).ok_or(DkimError::Key("PKCS#8 缺 privateKey 字段"))?;
    let (tag, inner) = read_tlv(after_alg).ok_or(DkimError::Key("PKCS#8 缺 privateKey"))?;
    if tag != 0x04 {
        return Err(DkimError::Key("PKCS#8 的 privateKey 不是 OCTET STRING"));
    }
    // 里面必须还是 SEQUENCE（RSAPrivateKey），否则是把别的算法当成了 RSA。
    match read_tlv(inner) {
        Some((0x30, _)) => Ok(inner.to_vec()),
        _ => Err(DkimError::Key("私钥不是 RSA 私钥")),
    }
}

/// 按 DER 最短形式写长度域（短格式 / 0x81 / 0x82 / 0x83）。
fn push_der_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else if len <= 0xff {
        out.push(0x81);
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    } else {
        out.push(0x83);
        out.push((len >> 16) as u8);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    }
}

/// 读一个 DER TLV，返回 `(tag, 内容)`；内容只取本层的字节。
pub(crate) fn read_tlv(data: &[u8]) -> Option<(u8, &[u8])> {
    let (&tag, rest) = data.split_first()?;
    let (&first_len, rest) = rest.split_first()?;
    let (len, rest) = if first_len < 0x80 {
        (usize::from(first_len), rest)
    } else {
        let count = usize::from(first_len & 0x7f);
        if count == 0 || count > 3 || rest.len() < count {
            return None;
        }
        let mut len = 0usize;
        for &byte in &rest[..count] {
            len = len.checked_mul(256)?.checked_add(usize::from(byte))?;
        }
        (len, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len]))
}

/// 跳过数据开头的一个 TLV，返回其后剩下的部分。
pub(crate) fn skip_tlv(data: &[u8]) -> Option<&[u8]> {
    let (&tag, rest) = data.split_first()?;
    let (&first_len, rest) = rest.split_first()?;
    let (header_extra, len) = if first_len < 0x80 {
        (0usize, usize::from(first_len))
    } else {
        let count = usize::from(first_len & 0x7f);
        if count == 0 || count > 3 || rest.len() < count {
            return None;
        }
        let mut len = 0usize;
        for &byte in &rest[..count] {
            len = len.checked_mul(256)?.checked_add(usize::from(byte))?;
        }
        (count, len)
    };
    let _ = tag;
    let start = 2 + header_extra + len;
    if start > data.len() {
        return None;
    }
    Some(&data[start..])
}

/// 跳过数据开头的 `count` 个 TLV，返回其后剩下的部分。
pub(crate) fn tlv_rest(data: &[u8], count: usize) -> Option<&[u8]> {
    let mut rest = data;
    for _ in 0..count {
        rest = skip_tlv(rest)?;
    }
    Some(rest)
}

/// 从 `RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }`
/// 的内容里取出 `(n, e)` 的裸字节。
fn read_modulus_and_exponent(body: &[u8]) -> Result<(&[u8], &[u8]), DkimError> {
    let (n_tag, n) = read_tlv(body).ok_or_else(|| DkimError::Config("公钥里缺模数".to_string()))?;
    let after_n = tlv_rest(body, 1).ok_or_else(|| DkimError::Config("公钥里缺模数".to_string()))?;
    let (e_tag, e) =
        read_tlv(after_n).ok_or_else(|| DkimError::Config("公钥里缺指数".to_string()))?;
    if n_tag != 0x02 {
        return Err(DkimError::Config("公钥的模数不是 INTEGER".to_string()));
    }
    if e_tag != 0x02 {
        return Err(DkimError::Config("公钥的指数不是 INTEGER".to_string()));
    }
    Ok((n, e))
}

/// 从**私钥**里取出它的模数与指数（`n`、`e`）。
///
/// `RSAPrivateKey ::= SEQUENCE { version, n, e, d, p, q, … }`：
/// 跳过 version 之后的前两个 INTEGER 就是 `(n, e)`。
#[allow(dead_code)]
pub(crate) fn read_rsa_private_modulus_and_exponent(
    pkcs1_content: &[u8],
) -> Result<(&[u8], &[u8]), DkimError> {
    let after_version =
        tlv_rest(pkcs1_content, 1).ok_or(DkimError::Key("私钥结构不完整（缺版本号）"))?;
    let (n_tag, n) = read_tlv(after_version).ok_or(DkimError::Key("私钥里缺模数"))?;
    let after_n = tlv_rest(after_version, 1).ok_or(DkimError::Key("私钥里缺模数"))?;
    let (e_tag, e) = read_tlv(after_n).ok_or(DkimError::Key("私钥里缺指数"))?;
    if n_tag != 0x02 || e_tag != 0x02 {
        return Err(DkimError::Key("私钥的模数或指数不是 INTEGER"));
    }
    Ok((n, e))
}

/// 把 `(n, e)` 编成 DKIM 记录里 `p=` 要的那串 base64：
/// SubjectPublicKeyInfo DER 的 base64（`rsaEncryption`，参数 NULL）。
pub(crate) fn rsa_public_key_spki_base64(n: &[u8], e: &[u8]) -> String {
    let rsa_public = encode_rsa_public_key(n, e);
    // AlgorithmIdentifier ::= SEQUENCE { OID rsaEncryption, NULL }
    const ALGORITHM: [u8; 15] = [
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    // BIT STRING：首字节是「未使用位数」，RSA 公钥为 0。
    let mut bit_string = Vec::with_capacity(rsa_public.len() + 2);
    bit_string.push(0x03);
    push_der_len(&mut bit_string, rsa_public.len() + 1);
    bit_string.push(0x00);
    bit_string.extend_from_slice(&rsa_public);

    let mut out = Vec::with_capacity(bit_string.len() + ALGORITHM.len() + 4);
    out.push(0x30);
    push_der_len(&mut out, bit_string.len() + ALGORITHM.len());
    out.extend_from_slice(&ALGORITHM);
    out.extend_from_slice(&bit_string);
    BASE64.encode(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6376 附录 A 的私钥（1024 位，**公开**的测试向量，不是任何人的真密钥）。
    ///
    /// 原文（附录 A.1，「test.exmple 选择的私钥」）：
    ///
    /// ```text
    /// -----BEGIN RSA PRIVATE KEY-----
    /// MIICXwIBAAKBgQDwIRP/UC3SBsEmGqZ9ZJW3/DkMoGeLnQg1fWn7/zYtIxN2SnFC
    /// jxOCKG9v3b4jYfcTNh5ijSsq631uBItLa7od+v/RtdC2UzJ1lWT947qR+Rcac2gb
    /// to/NMqJ0fzfVjH4OuKhitdY9tf6mcwGjaNBcWToIMmPSPDdQPNUYckcQ2QIDAQAB
    /// AoGBALmn+XwWk7akvkUlqb+dOxyLB9i5VBVfje89Teolwc9YJT36BGN/l4e0l6QX
    /// /1//6DWUTB3KI6wFcm7TWJcxbS0tcKZX7FsJvUz1SbQnkS54DJck1EZO/BLa5ckJ
    /// gAYIaqlA9C0ZwM6i58lLlPadX/rtHb7pWzeNcZHjKrjM461ZAkEA+itss2nRlmyO
    /// n1/5yDyCluST4dQfO8kAB3toSEVc7DeFeDhnC1mZdjASZNvdHS4gbLIA1hUGEF9m
    /// 3hKsGUMMPwJBAPW5v/U+AWTADFCS22t72NUurgzeAbzb1HWMqO4y4+9Hpjk5wvL/
    /// eVYizyuce3/fGke7aRYw/ADKygMJdW8H/OcCQQDz5OQb4j2QDpPZc0Nc4QlbvMsj
    /// 7p7otWRO5xRa6SzXqqV3+F0VpQPWqsLyIIFqOvy15ZJoYjGVNSSW4QnQyB47AkEA
    /// 6Q9NDhLHP/1VubUdqUnMvZIyvYl2Fsq1dMZT4SzDq0pjQ7jP3RjMnKGT6aSRBQWL
    /// jHGkRdvC/NqRmHDNqB/HUQJBAO0bE0RkcLDLKnsLI3D9S6KaJUw4lP1ckRROZCL/
    /// p5o2IsdFuW8TyCTB5ThoVMLKJVUmQwhtwD2Fk6+IVKOwqOo=
    /// -----END RSA PRIVATE KEY-----
    /// ```
    const RFC_KEY: &str = "\
-----BEGIN RSA PRIVATE KEY-----
MIICXwIBAAKBgQDwIRP/UC3SBsEmGqZ9ZJW3/DkMoGeLnQg1fWn7/zYtIxN2SnFC
jxOCKG9v3b4jYfcTNh5ijSsq631uBItLa7od+v/RtdC2UzJ1lWT947qR+Rcac2gb
to/NMqJ0fzfVjH4OuKhitdY9tf6mcwGjaNBcWToIMmPSPDdQPNUYckcQ2QIDAQAB
AoGBALmn+XwWk7akvkUlqb+dOxyLB9i5VBVfje89Teolwc9YJT36BGN/l4e0l6QX
/1//6DWUTB3KI6wFcm7TWJcxbS0tcKZX7FsJvUz1SbQnkS54DJck1EZO/BLa5ckJ
gAYIaqlA9C0ZwM6i58lLlPadX/rtHb7pWzeNcZHjKrjM461ZAkEA+itss2nRlmyO
n1/5yDyCluST4dQfO8kAB3toSEVc7DeFeDhnC1mZdjASZNvdHS4gbLIA1hUGEF9m
3hKsGUMMPwJBAPW5v/U+AWTADFCS22t72NUurgzeAbzb1HWMqO4y4+9Hpjk5wvL/
eVYizyuce3/fGke7aRYw/ADKygMJdW8H/OcCQQDz5OQb4j2QDpPZc0Nc4QlbvMsj
7p7otWRO5xRa6SzXqqV3+F0VpQPWqsLyIIFqOvy15ZJoYjGVNSSW4QnQyB47AkEA
6Q9NDhLHP/1VubUdqUnMvZIyvYl2Fsq1dMZT4SzDq0pjQ7jP3RjMnKGT6aSRBQWL
jHGkRdvC/NqRmHDNqB/HUQJBAO0bE0RkcLDLKnsLI3D9S6KaJUw4lP1ckRROZCL/
p5o2IsdFuW8TyCTB5ThoVMLKJVUmQwhtwD2Fk6+IVKOwqOo=
-----END RSA PRIVATE KEY-----";

    /// 一把 **2048 位** 的测试私钥（PKCS#1 `RSAPrivateKey` 的 base64）。
    ///
    /// **这不是任何人的真密钥**：它由仓库外的确定性脚本生成（固定种子，
    /// 小素数指数不用，p<q，CRT 参数齐全），只用于跑单测。
    /// 之所以自造而不用 ring 自带的示例钥：ring 的示例钥公钥指纹含
    /// 「有趣」的数值（`e = 2491461`、p>q），不代表现实中的密钥形状，
    /// 拿它测「能不能加载一把正常私钥」会掩盖问题。
    const TEST_KEY_PKCS1: &str = concat!(
        "MIIEpQIBAAKCAQEAnylzRCKuLBPAmR37c4vTNUlFY3UIRM9CefwK0DRAko9KHTYUJ043JJ8hZceA",
        "f96kn/ajQtU7DrtUN9nL8P9N4xJ7wIQOCFJiUnFQuY6TOWQ5tW5oI/YLofgHERorGsBBHFE3nUxt",
        "86S2xRyUZNs7zr2p7BhwA2VpZIi6e66vQAOic92NsoAzUgceYZiAhc8RkxbD7/blU23HC9r5y151",
        "BEZ3Un3VRoU7gDWgnoW5/uFuingOT7rl0wT5gLTiKCAZUMn6ycFuQdNUe4DhTFsNR+zt3SY58NKc",
        "GH0UtTthlDSalTzRbjv7J9MiAa6q6Teg+cvuZo9V5yLjvCejx+YWpQIDAQABAoIBAAEI2fD1cMBk",
        "mHnmUf4cSJEuGr9cwWMBHeOBGejkjg7l04uBEHx4667XQfZRZ6ptHhDUH5eNn0anPRLn3YWBg1YY",
        "hqlnslengXAbwHSUUqyq1B0zzIV/UfAJlrIrw1/3I/U1KfFhz80novCBEQ3exvK/FN2uAvxiql62",
        "7M9lAUqfOM0tj12MmxNO+sVvWKRhapFQBtu/XUQTpnpNHCC23f7TGIUs7g/nYlL5gYvNGhSfGXn0",
        "aRfSAhIjiz12KSKLasIE7WGmscwjN0ivLso+1HQ8cUSLnNySOLFh0Z0Q2dgOMdRSfXkedB25E4cU",
        "Hc3umf2I25nxpRT7lBcnsojkNrkCgYEAxeIFD88TyfJrStRp/BTl83/3fA4Kdecmlm9uIMSxCIh3",
        "2ZwyMM1kx2xxd1xRLHJ9TTlW0cNJof6KLG5rkE/6R7wkUNqW5QyPvrrw2U0hgPaNrtTZNmuPIVeB",
        "6Lm++j6eZWs2/WFKBZW77FL36zzgJdLwXN4tDR9skMqkv1v+QSMCgYEAzegp5JVRmExYeSla8MY1",
        "d8cL8q9PlQRdsT25gpi1aRAvKPeG/hzqO1+WLUfTRZoawbwqOM3iTo8AlS5KKcIupqbdT3IgDOoj",
        "e17uF/cKQSkr3RYxuhhxl9ZOtHcAEEEhsdlOEGEe42+4tjC7Bo4NAuHXR7yEhbLiJgh5drJd2ZcC",
        "gYEAu+/cxNX0QnF1iz1QZ3TlrK0VvIGNV8swb+Hm40JLrQIHaeqaFJWOKIULAsTv51nZPZW+qjxi",
        "W3NngalhiU8cMjMy/oOMmn+iqg/cHVXPAy+nAqvunGvtJUp5/DEle4AracE5Gc7jwYgjUeC+y3Em",
        "pWXAmUL94J++uYlsjL3vLacCgYEAxsf8MZ3psNL7krTXA50Gn0S9ByQx+xXE8cg6IJcWg4BJVw5n",
        "90io5HrqO3qcvxQAAsqKJF62vMqXcG3I0TmS2fY7r2fpAy7C5GaICwDGmJOLshusFBFnEIOnZdeA",
        "azDFu7GjCHMKY91GgDTZSYG3k/G9aJbZhU1lv/vsbyfRLK8CgYEAkQdHKjBa31gOw2JHjPQ23TEZ",
        "mm6naWCwHJ34VS/gtnUkln8R9dC5fKqv31OuG2WZmCP94ZVk8q/lT/Ztn8sdcGJojEpIN6zF80MV",
        "OM/We17wf80v9yoignv511BbzdQ/UBnaVpGXiuglOIzX0OFEEvYMNtpQUe62hoAAMc57dkM=",
    );

    /// [`TEST_KEY_PKCS1`] 的 PKCS#8 外壳（同一把钥，只多了一层 `PrivateKeyInfo`）。
    const TEST_KEY_PKCS8: &str = concat!(
        "MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQCfKXNEIq4sE8CZHftzi9M1SUVj",
        "dQhEz0J5/ArQNECSj0odNhQnTjcknyFlx4B/3qSf9qNC1TsOu1Q32cvw/03jEnvAhA4IUmJScVC5",
        "jpM5ZDm1bmgj9guh+AcRGisawEEcUTedTG3zpLbFHJRk2zvOvansGHADZWlkiLp7rq9AA6Jz3Y2y",
        "gDNSBx5hmICFzxGTFsPv9uVTbccL2vnLXnUERndSfdVGhTuANaCehbn+4W6KeA5PuuXTBPmAtOIo",
        "IBlQyfrJwW5B01R7gOFMWw1H7O3dJjnw0pwYfRS1O2GUNJqVPNFuO/sn0yIBrqrpN6D5y+5mj1Xn",
        "IuO8J6PH5halAgMBAAECggEAAQjZ8PVwwGSYeeZR/hxIkS4av1zBYwEd44EZ6OSODuXTi4EQfHjr",
        "rtdB9lFnqm0eENQfl42fRqc9EufdhYGDVhiGqWeyV6eBcBvAdJRSrKrUHTPMhX9R8AmWsivDX/cj",
        "9TUp8WHPzSei8IERDd7G8r8U3a4C/GKqXrbsz2UBSp84zS2PXYybE076xW9YpGFqkVAG279dRBOm",
        "ek0cILbd/tMYhSzuD+diUvmBi80aFJ8ZefRpF9ICEiOLPXYpIotqwgTtYaaxzCM3SK8uyj7UdDxx",
        "RIuc3JI4sWHRnRDZ2A4x1FJ9eR50HbkThxQdze6Z/YjbmfGlFPuUFyeyiOQ2uQKBgQDF4gUPzxPJ",
        "8mtK1Gn8FOXzf/d8Dgp15yaWb24gxLEIiHfZnDIwzWTHbHF3XFEscn1NOVbRw0mh/oosbmuQT/pH",
        "vCRQ2pblDI++uvDZTSGA9o2u1Nk2a48hV4Houb76Pp5lazb9YUoFlbvsUvfrPOAl0vBc3i0NH2yQ",
        "yqS/W/5BIwKBgQDN6CnklVGYTFh5KVrwxjV3xwvyr0+VBF2xPbmCmLVpEC8o94b+HOo7X5YtR9NF",
        "mhrBvCo4zeJOjwCVLkopwi6mpt1PciAM6iN7Xu4X9wpBKSvdFjG6GHGX1k60dwAQQSGx2U4QYR7j",
        "b7i2MLsGjg0C4ddHvISFsuImCHl2sl3ZlwKBgQC779zE1fRCcXWLPVBndOWsrRW8gY1XyzBv4ebj",
        "QkutAgdp6poUlY4ohQsCxO/nWdk9lb6qPGJbc2eBqWGJTxwyMzL+g4yaf6KqD9wdVc8DL6cCq+6c",
        "a+0lSnn8MSV7gCtpwTkZzuPBiCNR4L7LcSalZcCZQv3gn765iWyMve8tpwKBgQDGx/wxnemw0vuS",
        "tNcDnQafRL0HJDH7FcTxyDoglxaDgElXDmf3SKjkeuo7epy/FAACyookXra8ypdwbcjROZLZ9juv",
        "Z+kDLsLkZogLAMaYk4uyG6wUEWcQg6dl14BrMMW7saMIcwpj3UaANNlJgbeT8b1oltmFTWW/++xv",
        "J9EsrwKBgQCRB0cqMFrfWA7DYkeM9DbdMRmabqdpYLAcnfhVL+C2dSSWfxH10Ll8qq/fU64bZZmY",
        "I/3hlWTyr+VP9m2fyx1wYmiMSkg3rMXzQxU4z9Z7XvB/zS/3KiKCe/nXUFvN1D9QGdpWkZeK6CU4",
        "jNfQ4UQS9gw22lBR7raGgAAxznt2Qw==",
    );

    /// [`TEST_KEY_PKCS1`] 对应的公钥，按 RFC 6376 §3.6.1 的常见写法
    /// 做成完整 SubjectPublicKeyInfo（DNS 记录里 `p=` 就是它的 base64）。
    const TEST_PUBLIC_KEY: &str = concat!(
        "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAnylzRCKuLBPAmR37c4vTNUlFY3UIRM9C",
        "efwK0DRAko9KHTYUJ043JJ8hZceAf96kn/ajQtU7DrtUN9nL8P9N4xJ7wIQOCFJiUnFQuY6TOWQ5",
        "tW5oI/YLofgHERorGsBBHFE3nUxt86S2xRyUZNs7zr2p7BhwA2VpZIi6e66vQAOic92NsoAzUgce",
        "YZiAhc8RkxbD7/blU23HC9r5y151BEZ3Un3VRoU7gDWgnoW5/uFuingOT7rl0wT5gLTiKCAZUMn6",
        "ycFuQdNUe4DhTFsNR+zt3SY58NKcGH0UtTthlDSalTzRbjv7J9MiAa6q6Teg+cvuZo9V5yLjvCej",
        "x+YWpQIDAQAB",
    );

    /// 一把「模数 1024 位、其他参数形状合理」的 PKCS#1 私钥（base64），只为验证「太短」这条错误分支。
    ///
    /// 模数取自 [`TEST_KEY_PKCS1`] 的高 1024 位，d/p/q 只是为了把 DER 结构填满——
    /// ring 对 PKCS#1 私钥**先查模数位长**（`BitLength::from_bits(2048)`），
    /// 位长不够时走不到后面的一致性校验，所以它不需要真是一把能用的钥。
    /// 不用 RFC 6376 附录 A 的那把：它的 base64 少了 4 个字节（勘误），
    /// 解出来的 DER 长度域与内容对不上，ring 报的是**编码错误**而不是「太短」，
    /// 测不到这条分支（另有 `rfc_erratum_key_is_reported_as_truncated` 覆盖那份）。
    const SHORT_KEY: &str = concat!(
        "MIIB5wIBAAKBgQCAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQIDAQABAkwBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABI0VniavN7xNXm98kaKzgJGAkEAgAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB/f1paDSTGsZN3obLD1OX2BxgpOktcbX6PkKGy",
        "w9Tl9gJBAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAxM38aO1x9nh81JGis4BNXm9",
        "8kaKzgE1eb3yRorOATUCQH///////////////////////////wEBS0vltnKc2RC9GfewjiEWll8e",
        "4OjXxrXExNXm+AkeO160CU6T+U6i6DsCQH////////////////////////////nZkBy4lHBMPBlb",
        "df2b7zjCS/VU3bZEH9qFYyD+zt7frPJq83vkjRWcA3oCAQE=",
    );

    /// [`TEST_PUBLIC_KEY`] 的另一种 `p=` 写法：裸 PKCS#1 `RSAPublicKey`
    /// （没有 SubjectPublicKeyInfo 的 AlgorithmIdentifier 外壳）。
    const TEST_PUBLIC_KEY_PKCS1: &str = concat!(
        "MIIBCgKCAQEAnylzRCKuLBPAmR37c4vTNUlFY3UIRM9CefwK0DRAko9KHTYUJ043JJ8hZceAf96k",
        "n/ajQtU7DrtUN9nL8P9N4xJ7wIQOCFJiUnFQuY6TOWQ5tW5oI/YLofgHERorGsBBHFE3nUxt86S2",
        "xRyUZNs7zr2p7BhwA2VpZIi6e66vQAOic92NsoAzUgceYZiAhc8RkxbD7/blU23HC9r5y151BEZ3",
        "Un3VRoU7gDWgnoW5/uFuingOT7rl0wT5gLTiKCAZUMn6ycFuQdNUe4DhTFsNR+zt3SY58NKcGH0U",
        "tTthlDSalTzRbjv7J9MiAa6q6Teg+cvuZo9V5yLjvCejx+YWpQIDAQAB",
    );

    /// 同一个签名，由 python 的裸 RSA（`pow(m, d, n)`）独立算出来。
    ///
    /// 作用：把「ring 签出来的东西」和「任何人按 PKCS#1 v1.5 算出来的东西」钉在一起。
    /// 如果哪天 ring 升级换了填充/编码，这条会先红。
    const REFERENCE_SIGNATURE: &str = concat!(
        "409bb2e1f72b0d0c897988c2ba51d836b8867f11466b4c27b9be96f2a297fa17",
        "babd3e4171adec106d6fc6ff0a7b10c91b1f2b367d535d2379a2df32069950dc",
        "75d2fa3298abee9fa095424304c47d8790d8eb744bc80bb9f742329ff201ffee",
        "bd1cfb833c7af0a2c7649da3018e073c90438f7cfa1617e06edd088f2b71367f",
        "cb388b136ba5bf1ff785a4be0f93a822961be430c62c3aba064727e8c5810ee6",
        "3d7ecddf862d9585757602048ed25da900232d23fb24fa9d67f32f0be60645d7",
        "6a2f0ab9ddcef7ee20f153c419ad7486993a8da4b363b47a5835c0389d349c4d",
        "870c5d384ef584e6420db604eaf2e4a0616887c1b64a3127284bb09f96155043",
    );

    fn test_key() -> SigningKey {
        SigningKey::from_pem(&pem(TEST_KEY_PKCS1, "RSA PRIVATE KEY")).expect("测试私钥应当能加载")
    }

    /// 测试里需要「同一份钥材料、不同折行」的变体，所以**拆行由代码做**，
    /// 常量里只放 base64 原文：手抄 64 列折行太容易漏一个字符。
    fn pem(base64: &str, label: &str) -> String {
        let compact: String = base64.chars().filter(|ch| !ch.is_whitespace()).collect();
        let mut out = format!("-----BEGIN {label}-----\n");
        for chunk in compact.as_bytes().chunks(64) {
            out.push_str(&String::from_utf8_lossy(chunk));
            out.push('\n');
        }
        out.push_str(&format!("-----END {label}-----\n"));
        out
    }

    #[test]
    fn signing_matches_an_independently_computed_pkcs1_signature() {
        // RSA PKCS#1 v1.5 是确定性的：同一把钥、同一条消息，签出来必须逐字节相同。
        let signature = test_key()
            .sign(SigningAlgorithm::RsaSha256, b"probe")
            .expect("应当能签");
        assert_eq!(hex(&signature), REFERENCE_SIGNATURE);
    }

    /// 和 [`REFERENCE_SIGNATURE`] 比较用：小写 hex，出错时一眼能看出哪一段不同。
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn test_key_pairs_with_its_published_public_key() {
        let key = test_key();
        assert_eq!(key.bits(), 2048);
        let signature = key
            .sign(SigningAlgorithm::RsaSha256, b"probe")
            .expect("应当能签");
        assert_eq!(signature.len(), 256, "2048 位 RSA 的签名是 256 字节");
        assert!(verify(TEST_PUBLIC_KEY, b"probe", &signature).expect("公钥应当能解"));
        assert!(!verify(TEST_PUBLIC_KEY, b"probE", &signature).expect("公钥应当能解"));

        // DKIM 的 `p=` 两种写法（完整 SPKI / 只有 PKCS#1）都要认。
        let spki = BASE64.decode(TEST_PUBLIC_KEY).expect("SPKI 应当能解");
        assert!(verify(&BASE64.encode(&spki), b"probe", &signature).expect("SPKI 公钥应当能解"));
        assert!(verify(TEST_PUBLIC_KEY_PKCS1, b"probe", &signature).expect("PKCS#1 公钥应当能解"));
    }

    #[test]
    fn signing_key_never_prints_its_material() {
        let debug = format!("{:?}", test_key());
        assert_eq!(debug, "SigningKey(<redacted>)");
    }

    #[test]
    fn parses_the_rfc_key_into_der_of_the_expected_length() {
        let der = pem_to_der(RFC_KEY).expect("RFC 6376 附录 A 的私钥应当能解析");
        // 1024 位 + 5 个 CRT 参数的 PKCS#1 结构解出来是 611 字节
        // （附录 A 那段 base64 共 816 个字符 → 612 字节，长度域占 4 字节，
        // 因此 SEQUENCE 内容 611 字节）。
        assert_eq!(der.len(), 611);
    }

    #[test]
    fn rejects_encrypted_keys_with_a_clear_message() {
        let pem =
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----";
        let err = match SigningKey::from_pem(pem) {
            Ok(_) => panic!("加密私钥应当被拒绝"),
            Err(err) => err,
        };
        assert!(matches!(err, DkimError::Key(_)));
        assert!(err.to_string().contains("加密"));
    }

    #[test]
    fn rejects_garbage_without_echoing_key_material() {
        let pem = "-----BEGIN PRIVATE KEY-----\nnot-base64!!\n-----END PRIVATE KEY-----";
        let err = match SigningKey::from_pem(pem) {
            Ok(_) => panic!("坏 PEM 应当被拒绝"),
            Err(err) => err,
        };
        assert!(!err.to_string().contains("not-base64"));
    }

    #[test]
    fn pkcs8_wrapper_is_peeled_back_to_pkcs1() {
        let pkcs1 = pem_to_der(RFC_KEY).expect("PKCS#1 应当能解");
        let wrapped = wrap_pkcs8(&pkcs1);
        let peeled = unwrap_pkcs8(&wrapped).expect("PKCS#8 应当能剥");
        assert_eq!(&*peeled, &pkcs1[..]);
    }

    #[test]
    fn pkcs8_pem_is_accepted_by_the_key_loader() {
        let key = SigningKey::from_pem(&pem(TEST_KEY_PKCS8, "PRIVATE KEY"))
            .expect("PKCS#8 私钥应当能加载");
        assert_eq!(key.bits(), 2048);
        let signature = key
            .sign(SigningAlgorithm::RsaSha256, b"pkcs8")
            .expect("应当能签");
        assert!(verify(TEST_PUBLIC_KEY, b"pkcs8", &signature).expect("公钥应当能解"));
        // PKCS#1 与 PKCS#8 是同一把钥的两件外衣：解出来的 DER 必须一模一样。
        assert_eq!(
            &*pem_to_der(&pem(TEST_KEY_PKCS8, "PRIVATE KEY")).expect("应当能解"),
            &*pem_to_der(&pem(TEST_KEY_PKCS1, "RSA PRIVATE KEY")).expect("应当能解")
        );
    }

    #[test]
    fn rejects_der_that_is_not_a_private_key() {
        let err = match SigningKey::from_der(&[0x30, 0x03, 0x02, 0x01, 0x00]) {
            Ok(_) => panic!("随便一段 DER 不应当是私钥"),
            Err(err) => err,
        };
        assert!(!err.to_string().contains("私钥材料"), "{err}");
    }

    /// ring 只接受 2048 / 3072 / 4096 位——这是本 crate 的硬事实，写进测试免得以后有人以为漏了。
    ///
    /// RFC 6376 附录 A 的那把私钥恰好是 1024 位；**所以附录 A 的 `b=` 无法用本 crate 的
    /// 签名器复现**（向量验证走的是 `bh=` 与验签方向，见 `tests/official_vectors.rs`）。
    /// 规范 §3.3.3 要求支持 1024 位的是**验证器**（本 crate 的 `verify` 支持），
    /// 签发方用更长密钥没有兼容问题（RFC 8301 要求签发方至少 1024 位、推荐 2048）。
    #[test]
    fn keys_smaller_than_2048_bits_get_an_actionable_message() {
        // PKCS#1 外壳：RFC 附录 A 的私钥 base64 本身少 4 个字节（RFC 6376 勘误），
        // DER 长度域与内容对不上，ring 报的是编码错误而不是「太短」；
        // 这条分支只要求错误可读、且不回显密钥材料。
        let err = match SigningKey::from_pem(RFC_KEY) {
            Ok(_) => panic!("ring 不应接受 1024 位私钥"),
            Err(err) => err,
        };
        assert!(matches!(err, DkimError::Key(_)), "{err}");
        assert!(
            !err.to_string().contains("MIICXw"),
            "错误信息不得回显密钥材料"
        );

        // PKCS#8 外壳走的是本 crate 自己的剥离逻辑，剥出来的 PKCS#1 长度域仍然对不上，
        // 所以错误应当还是「结构不完整」，而不是「太短」——这一处区分能让用户少走弯路。
        let pkcs8 = pem(
            &BASE64.encode(wrap_pkcs8(&pem_to_der(RFC_KEY).unwrap())),
            "PRIVATE KEY",
        );
        let err = match SigningKey::from_pem(&pkcs8) {
            Ok(_) => panic!("ring 不应接受 1024 位私钥"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("DER"), "{err}");

        // 直接用一把结构合法的 1024 位 PKCS#1 钥，走的也是同一句提示。
        let err = match SigningKey::from_pem(&pem(SHORT_KEY, "RSA PRIVATE KEY")) {
            Ok(_) => panic!("ring 不应接受 1024 位私钥"),
            Err(err) => err,
        };
        assert!(!err.to_string().is_empty(), "{err}");
        assert!(
            !err.to_string().contains("InvalidEncoding"),
            "错误信息不该把 ring 的枚举名甩给用户：{err}"
        );
    }

    /// 附录 A 的私钥 base64 短了 4 个字节（RFC 6376 勘误），错误信息要说出「结构不完整」，
    /// 而不是笼统的「不是 RSA 私钥」——否则用户换了编码工具还是同样的错。
    #[test]
    fn rfc_erratum_key_is_reported_as_truncated() {
        let err = match SigningKey::from_pem(RFC_KEY) {
            Ok(_) => panic!("附录 A 的钥不该被接受"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("DER"), "{err}");
    }

    #[test]
    fn tolerates_trailing_newlines_and_indentation() {
        let messy = format!("\n  {}  \n\n", RFC_KEY.replace('\n', "  \r\n "));
        // 空白容错要与 `pem_to_der` 的结果一致：能不能加载由密钥本身决定，
        // 不由「怎么换行」决定。
        assert_eq!(
            &*pem_to_der(&messy).expect("脏 PEM 应当能解"),
            &*pem_to_der(RFC_KEY).expect("标准 PEM 应当能解")
        );
    }

    /// 把一个 PKCS#1 私钥打成 PKCS#8 外壳（测试用；rsaEncryption OID 固定）。
    fn wrap_pkcs8(pkcs1: &[u8]) -> Vec<u8> {
        // rsaEncryption: 1.2.840.113549.1.1.1 + NULL 参数。
        const ALG: &[u8] = &[
            0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05,
            0x00,
        ];
        let version = [0x02, 0x01, 0x00];
        let mut inner = Vec::new();
        inner.extend_from_slice(&version);
        inner.extend_from_slice(ALG);
        inner.push(0x04);
        inner.extend_from_slice(&encode_len(pkcs1.len()));
        inner.extend_from_slice(pkcs1);
        let mut out = vec![0x30];
        out.extend_from_slice(&encode_len(inner.len()));
        out.extend_from_slice(&inner);
        out
    }

    fn encode_len(len: usize) -> Vec<u8> {
        if len < 0x80 {
            vec![len as u8]
        } else {
            let mut bytes = Vec::new();
            let mut value = len;
            while value > 0 {
                bytes.insert(0, (value & 0xff) as u8);
                value >>= 8;
            }
            let mut out = vec![0x80 | bytes.len() as u8];
            out.extend_from_slice(&bytes);
            out
        }
    }
}
