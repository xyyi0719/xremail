//! 用户手里的公钥：PEM 里那种（`BEGIN PUBLIC KEY`）。
//!
//! 两处用到：
//!
//! * **验签**：拿一把公钥 PEM 检查一封邮件上的签名对不对；
//! * **录入自检**：用户把私钥粘进设置页时，顺手问一句「与公钥文件对得上吗」，
//!   免得配好之后发出去的每封信都被退信——私钥抄错一个字符，
//!   签名过程不会报错，只有收信方会告诉你不对劲。
//!
//! 与 [`crate::key::verify`] 的差别：那边的入参是 DKIM 的 `p=`（base64），
//! 给验签方用；这里吃的是 PEM 文本，给「用户手上有什么」这一侧用。
//! 两个方向的验签都只调用 `ring`，不自己实现 RSA / Ed25519 的数学。

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

use crate::error::DkimError;

/// 一把公钥：只用来验签，不承载私钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyingKey {
    /// RSA（PKCS#1 v1.5 + SHA-256）。
    Rsa {
        /// SubjectPublicKeyInfo 的 base64（即 DKIM 记录里的 `p=`）。
        spki_base64: String,
    },
}

impl VerifyingKey {
    /// 从 PEM 文本加载（`-----BEGIN PUBLIC KEY-----`，SubjectPublicKeyInfo）。
    ///
    /// 私钥 PEM 会被明确拒绝：把私钥粘进「公钥」框里是很常见的误操作，
    /// 静默接受会让用户以为配置好了。
    pub fn from_pem(pem: &str) -> Result<Self, DkimError> {
        let compact = body_of(pem)?;
        let der = BASE64
            .decode(compact.as_bytes())
            .map_err(|_| DkimError::Config("公钥 PEM 的 base64 不合法".to_string()))?;
        Self::from_der(&der)
    }

    /// 从一把私钥（PKCS#8 或 PKCS#1 的 DER）算出它对应的公钥。
    ///
    /// 设置页用的：用户只粘私钥，公钥由这里现算出来给 DNS 用，
    /// 不必让用户再多贴一份（两份对不上是最常见的配置事故）。
    pub fn from_private(private_der: &[u8]) -> Result<Self, DkimError> {
        // 私钥 DER 有两种形状，都先化成 PKCS#1 的 `RSAPrivateKey`：
        //   PKCS#1：SEQUENCE { version, n, e, d, p, q, … }
        //   PKCS#8：SEQUENCE { version, AlgorithmIdentifier, OCTET STRING(上面的 PKCS#1) }
        // 然后取前两个 INTEGER（n、e）。
        let (tag, outer) =
            crate::key::read_tlv(private_der).ok_or(DkimError::Key("私钥不是 DER"))?;
        if tag != 0x30 {
            return Err(DkimError::Key("私钥顶层不是 SEQUENCE"));
        }
        let after_version = match crate::key::read_tlv(outer) {
            Some((0x02, _)) => outer,
            Some((0x30, _)) => {
                // PKCS#8：version 之后是 AlgorithmIdentifier，再之后是 OCTET STRING。
                let after_version = crate::key::tlv_rest(outer, 1)
                    .ok_or(DkimError::Key("PKCS#8 缺 privateKey 字段"))?;
                let (octet_tag, pkcs1) = crate::key::read_tlv(after_version)
                    .ok_or(DkimError::Key("PKCS#8 缺 privateKey 字段"))?;
                let (key_tag, pkcs1_body) = crate::key::read_tlv(pkcs1)
                    .ok_or(DkimError::Key("PKCS#8 缺 privateKey 字段"))?;
                if octet_tag != 0x04 || key_tag != 0x30 {
                    return Err(DkimError::Key("PKCS#8 的 privateKey 不是 RSA 私钥"));
                }
                pkcs1_body
            }
            _ => return Err(DkimError::Key("私钥结构不认识")),
        };
        // 跳过 PKCS#1 的 version，取 n 与 e。
        let after_version = crate::key::tlv_rest(after_version, 1)
            .ok_or(DkimError::Key("私钥结构不完整（缺版本号）"))?;
        let (n_tag, n) =
            crate::key::read_tlv(after_version).ok_or(DkimError::Key("私钥里缺模数"))?;
        let after_n =
            crate::key::tlv_rest(after_version, 1).ok_or(DkimError::Key("私钥里缺指数"))?;
        let (e_tag, e) = crate::key::read_tlv(after_n).ok_or(DkimError::Key("私钥里缺指数"))?;
        if n_tag != 0x02 || e_tag != 0x02 {
            return Err(DkimError::Key("私钥的模数或指数不是 INTEGER"));
        }
        // DER 给正数补的 0x00 前导位要去掉再重编。
        let n = n.strip_prefix(&[0u8][..]).unwrap_or(n);
        let e = e.strip_prefix(&[0u8][..]).unwrap_or(e);
        Ok(VerifyingKey::Rsa {
            spki_base64: crate::key::rsa_public_key_spki_base64(n, e),
        })
    }

    /// 从 SubjectPublicKeyInfo 的 DER 加载。
    pub fn from_der(der: &[u8]) -> Result<Self, DkimError> {
        let spki_base64 = BASE64.encode(der);
        // 形状不对就当场报错（而不是等到验签时才发现）。
        crate::key::rsa_public_components(&spki_base64)?;
        Ok(VerifyingKey::Rsa { spki_base64 })
    }

    /// 这把公钥对应的 DKIM `p=`（base64 的 SubjectPublicKeyInfo）。
    pub fn selector_public_key(&self) -> &str {
        match self {
            VerifyingKey::Rsa { spki_base64 } => spki_base64,
        }
    }

    /// 验一个签名；`Ok(false)` 表示签名与这把公钥 / 这段字节对不上。
    ///
    /// `message` 是**被签的原始字节**，不做任何 DKIM 规范化。
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> Result<bool, DkimError> {
        match self {
            VerifyingKey::Rsa { spki_base64 } => {
                crate::key::verify(spki_base64, message, signature)
            }
        }
    }
}

/// 取 PEM 的正文部分（去掉 `-----BEGIN/END-----` 行与所有空白）。
fn body_of(pem: &str) -> Result<String, DkimError> {
    let pem = pem.trim();
    if pem.contains("-----BEGIN PRIVATE KEY-----")
        || pem.contains("-----BEGIN RSA PRIVATE KEY-----")
    {
        return Err(DkimError::Config(
            "这里要的是公钥（-----BEGIN PUBLIC KEY-----），不是私钥".to_string(),
        ));
    }
    let rest = pem
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .ok_or_else(|| {
            DkimError::Config(
                "公钥要写成 PEM：-----BEGIN PUBLIC KEY----- … -----END PUBLIC KEY-----".to_string(),
            )
        })?;
    let body = rest.split("-----END PUBLIC KEY-----").next().unwrap_or("");
    let compact: String = body.chars().filter(|ch| !ch.is_whitespace()).collect();
    if compact.is_empty() {
        return Err(DkimError::Config("公钥 PEM 里没有内容".to_string()));
    }
    Ok(compact)
}
