//! 账户配置的**端到端加密**同步载荷（L2/P6 的最小可用子集）。
//!
//! ## 它解决什么问题
//!
//! 用户在 A 机器上配好 5 个邮箱（含授权码），换到 B 机器希望「同步一下就能用」。
//! 本 crate 定义那个**同步载荷**：把账户配置序列化成 JSON，用用户主密码
//! 加密成密文，交给上层（WebDAV）上传/下载。
//!
//! ## 分层边界（重要）
//!
//! * 本 crate **只做编解码与加解密**：不联网、不碰数据库、不碰钥匙串。
//! * 网络传输（WebDAV）在 `xremail-sync` / shell 层；存取本地账户在 store 层。
//! * 这样它可以在纯单元测试里跑，不需要真的 WebDAV 服务器。
//!
//! ## 明文长什么样
//!
//! `VaultPayload` 是明文 JSON，包含账户的**全部配置**（含授权码）。
//! 加密前它只存在于内存；落盘/上传的**永远**是 [`seal_payload`] 产出的密文。
//!
//! ## 加密格式
//!
//! 直接用 `xremail-crypto` 的 [`SecretEnvelope`]：argon2id 派生密钥 + ChaCha20-Poly1305
//! AEAD。`to_bytes()` 产出的自包含字节**本来就是为「设备间搬运」设计的**
//! （见 `xremail-crypto` 的 `to_bytes` 文档），我们不再自造格式。
//!
//! ## 版本
//!
//! 明文里带 `vault_version`，加密封装也带自己的版本。两层分开演进：
//! 前者管「载荷字段变了」，后者管「加密算法变了」。

pub mod webdav;

use serde::{Deserialize, Serialize};
use xremail_crypto::SecretEnvelope;

/// 重新导出 KDF 参数类型，避免调用方额外依赖 `xremail-crypto`。
pub use xremail_crypto::KdfParams;

/// 明文载荷的格式版本。
///
/// * `1`：首版。字段见 [`VaultPayload`]。
///
/// **加字段**可以兼容（`serde(default)`）；**改字段含义或删字段**必须升版本，
/// 并在 [`parse_payload`] 里处理旧版本。
pub const VAULT_VERSION: u32 = 1;

/// 载荷里的一份账户配置。
///
/// 字段与服务端地址一并保存：换到新设备后**不需要**再问用户任何服务器信息。
/// **含授权码明文**——所以整个载荷必须加密后才允许离开内存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultAccount {
    /// 账户邮箱地址。
    pub email: String,
    /// 服务商标识（`netease_163` / `tencent` / `wo` …）。
    pub provider: String,
    /// 显示名。
    #[serde(default)]
    pub display_name: Option<String>,
    /// IMAP 服务器。
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_tls: String,
    /// SMTP 服务器。
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: String,
    pub smtp_auth: String,
    /// 登录用户名（通常是完整邮箱）。
    pub username: String,
    /// 同步方式与轮询间隔。
    pub sync_mode: String,
    pub poll_interval_secs: i64,
    /// 签名 HTML。
    #[serde(default)]
    pub signature_html: Option<String>,
    /// **授权码明文**。
    ///
    /// 这是本功能存在的理由（换设备免重填），也是它必须端到端加密的理由。
    /// 序列化进明文 JSON 后立刻被 [`seal_payload`] 加密；**任何明文落盘都是 bug**。
    pub credential: String,
}

/// 一次同步的完整明文载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultPayload {
    /// 载荷格式版本。
    pub vault_version: u32,
    /// 生成这份载荷的时间（毫秒，UTC）。
    pub updated_at_ms: i64,
    /// 生成它的设备标识（用于「谁覆盖了谁」的排查，不参与冲突判定）。
    pub device_id: String,
    /// 全部账户配置。
    pub accounts: Vec<VaultAccount>,
}

/// 载荷编解码的错误。
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// 载荷解析失败（JSON 坏了、字段缺失、或格式不认识）。
    #[error("同步载荷解析失败：{0}")]
    Malformed(String),
    /// 载荷版本不认识（比本程序新）。
    #[error("同步载荷版本 {found} 不受支持（本程序支持到 {supported}）")]
    UnsupportedVersion { found: u32, supported: u32 },
    /// 加密失败。
    #[error("加密失败：{0}")]
    Seal(String),
    /// 解密失败（口令错误或密文被篡改）。
    ///
    /// **口令错与密文篡改在 AEAD 下无法区分**，这里如实合并成一个错误：
    /// 告诉用户「口令错或数据损坏」，让他先怀疑口令。
    #[error("解密失败：口令不对，或同步数据已损坏")]
    Open,
}

impl VaultPayload {
    /// 构造一份新载荷。
    pub fn new(accounts: Vec<VaultAccount>, now_ms: i64, device_id: impl Into<String>) -> Self {
        Self {
            vault_version: VAULT_VERSION,
            updated_at_ms: now_ms,
            device_id: device_id.into(),
            accounts,
        }
    }

    /// 序列化成明文 JSON 字节。
    ///
    /// **只在内存里用**：调用方拿到后应立刻交给 [`seal_payload`]，
    /// 不要把返回值写到任何持久化位置。
    pub fn to_plaintext(&self) -> Result<Vec<u8>, VaultError> {
        serde_json::to_vec(self).map_err(|e| VaultError::Malformed(e.to_string()))
    }

    /// 从明文 JSON 解析。
    pub fn from_plaintext(bytes: &[u8]) -> Result<Self, VaultError> {
        let payload: VaultPayload =
            serde_json::from_slice(bytes).map_err(|e| VaultError::Malformed(e.to_string()))?;
        if payload.vault_version > VAULT_VERSION {
            return Err(VaultError::UnsupportedVersion {
                found: payload.vault_version,
                supported: VAULT_VERSION,
            });
        }
        Ok(payload)
    }
}

/// 用主密码把载荷加密成自包含密文字节（可直接上传 WebDAV）。
///
/// `params` 用 [`KdfParams::DEFAULT`] 即可；解密方从密文头部读回参数，
/// **不需要**你另外传。
pub fn seal_payload(
    payload: &VaultPayload,
    passphrase: &[u8],
    params: KdfParams,
) -> Result<Vec<u8>, VaultError> {
    let plaintext = payload.to_plaintext()?;
    let envelope = SecretEnvelope::seal(&plaintext, passphrase, params)
        .map_err(|e| VaultError::Seal(e.to_string()))?;
    Ok(envelope.to_bytes())
}

/// 用主密码把密文字节解回载荷。
///
/// 口令错或密文被篡改都会返回 [`VaultError::Open`]（AEAD 下两者不可区分）。
pub fn open_payload(ciphertext: &[u8], passphrase: &[u8]) -> Result<VaultPayload, VaultError> {
    let envelope =
        SecretEnvelope::parse(ciphertext).map_err(|e| VaultError::Malformed(e.to_string()))?;
    let plaintext = envelope.open(passphrase).map_err(|_| VaultError::Open)?;
    VaultPayload::from_plaintext(&plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VaultPayload {
        VaultPayload::new(
            vec![VaultAccount {
                email: "a@163.com".into(),
                provider: "netease_163".into(),
                display_name: Some("工作".into()),
                imap_host: "imap.163.com".into(),
                imap_port: 993,
                imap_tls: "implicit".into(),
                smtp_host: "smtp.163.com".into(),
                smtp_port: 465,
                smtp_tls: "implicit".into(),
                smtp_auth: "login".into(),
                username: "a@163.com".into(),
                sync_mode: "poll".into(),
                poll_interval_secs: 300,
                signature_html: None,
                credential: "AUTHCODE-16-CHARS".into(),
            }],
            1_700_000_000_000,
            "device-A",
        )
    }

    #[test]
    fn roundtrip_preserves_everything() {
        let payload = sample();
        let params = KdfParams::DEFAULT;
        let sealed = seal_payload(&payload, b"correct horse battery", params).unwrap();
        let opened = open_payload(&sealed, b"correct horse battery").unwrap();
        assert_eq!(payload, opened, "往返之后字段必须逐一相等（含授权码）");
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext_secret() {
        let payload = sample();
        let sealed = seal_payload(&payload, b"pw-123456789", KdfParams::DEFAULT).unwrap();
        // 授权码绝不能以明文出现在密文里。
        let needle = b"AUTHCODE-16-CHARS";
        assert!(
            !sealed.windows(needle.len()).any(|w| w == needle),
            "密文里出现了明文授权码 —— 加密没生效"
        );
        // 邮箱地址同理（避免整份配置可被字符串搜索出来）。
        let email = b"a@163.com";
        assert!(!sealed.windows(email.len()).any(|w| w == email));
    }

    #[test]
    fn wrong_passphrase_fails() {
        let sealed = seal_payload(&sample(), b"right-password", KdfParams::DEFAULT).unwrap();
        assert!(matches!(
            open_payload(&sealed, b"wrong-password"),
            Err(VaultError::Open)
        ));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut sealed = seal_payload(&sample(), b"pw", KdfParams::DEFAULT).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF; // 翻转最后一字节（属于认证标签）
        assert!(matches!(
            open_payload(&sealed, b"pw"),
            Err(VaultError::Open)
        ));
    }

    #[test]
    fn unknown_future_version_rejected() {
        let mut payload = sample();
        payload.vault_version = VAULT_VERSION + 1;
        let plain = payload.to_plaintext().unwrap();
        assert!(matches!(
            VaultPayload::from_plaintext(&plain),
            Err(VaultError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        // 旧版载荷可能没有 signature_html / display_name，应能读进来。
        let json = br#"{"vault_version":1,"updated_at_ms":1,"device_id":"d","accounts":[
            {"email":"x@qq.com","provider":"tencent","imap_host":"imap.qq.com","imap_port":993,
             "imap_tls":"implicit","smtp_host":"smtp.qq.com","smtp_port":465,"smtp_tls":"implicit",
             "smtp_auth":"login","username":"x@qq.com","sync_mode":"poll","poll_interval_secs":300,
             "credential":"c"}]}"#;
        let p = VaultPayload::from_plaintext(json).unwrap();
        assert_eq!(p.accounts.len(), 1);
        assert!(p.accounts[0].display_name.is_none());
        assert!(p.accounts[0].signature_html.is_none());
    }
}
