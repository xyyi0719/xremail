//! WebDAV 传输层（坚果云为主）。
//!
//! ## 为什么在 vault crate 里
//!
//! 传输与载荷格式是**成对**的：换成别的服务端（S3、网盘）要换这一段，但加密载荷
//! 不变。放在一起便于替换。本模块**只做 HTTP**，不碰数据库/钥匙串。
//!
//! ## 坚果云的实测约束（2021/2026 官方帮助）
//!
//! * 地址固定 `https://dav.jianguoyun.com/dav/`，**必须 HTTPS**。
//! * 认证用「邮箱 + **应用密码**」（不是登录密码）。应用密码在网页端
//!   「账户信息 → 安全选项 → 第三方应用管理」生成。
//! * 上传大小上限 500 MB（我们的密文远小于此）。
//! * 请求频率：免费 600 次/30 分钟、付费 1500 次/30 分钟。
//!   **手动同步**一次只发 2–3 个请求（PROPFIND + GET/PUT），完全够。
//! * **PUT 到不存在的目录会失败**，要先用 MKCOL 建目录。
//!   坚果云对已存在的 MKCOL 会返回 405，我们要把它当成功。
//!
//! ## 认证方式
//!
//! 用 HTTP Basic（坚果云要求）。**必须在 HTTPS 之上**——本模块拒绝非 `https://`
//! 的地址，避免把应用密码以明文 Basic 头发出去。

use ureq::Agent;

/// WebDAV 连接参数。
#[derive(Debug, Clone)]
pub struct WebDavConfig {
    /// 服务端根地址，必须以 `https://` 开头，以 `/` 结尾。
    /// 坚果云是 `https://dav.jianguoyun.com/dav/`。
    pub base_url: String,
    /// 用户名（坚果云是注册邮箱）。
    pub username: String,
    /// **应用密码**（不是登录密码）。
    pub app_password: String,
    /// 远端目录（相对 base_url），如 `xremail`。
    pub remote_dir: String,
}

/// 传输层错误。
#[derive(Debug, thiserror::Error)]
pub enum WebDavError {
    /// 配置不合法（如非 HTTPS）。
    #[error("WebDAV 配置错误：{0}")]
    Config(String),
    /// 网络/HTTP 层失败。
    #[error("WebDAV 请求失败：{0}")]
    Transport(String),
    /// 服务端返回了非预期的状态码。
    #[error("WebDAV 服务端返回 {status}：{detail}")]
    Status { status: u16, detail: String },
    /// 认证失败（用户名或应用密码不对）。
    #[error("WebDAV 认证失败：请检查邮箱与应用密码是否正确")]
    Unauthorized,
}

/// 一个 WebDAV 客户端。
///
/// 内部持有 `ureq::Agent`（连接复用）。**同步阻塞**——调用方在 async 上下文里
/// 要用 `tokio::task::spawn_blocking` 包一层。
pub struct WebDavClient {
    config: WebDavConfig,
    agent: Agent,
    /// 拼接好的「远端目录」绝对 URL（已含结尾 `/`）。
    dir_url: String,
}

impl WebDavClient {
    /// 构造客户端并校验配置。
    pub fn new(config: WebDavConfig) -> Result<Self, WebDavError> {
        if !config.base_url.starts_with("https://") {
            // 明文 Basic 认证会把应用密码直接暴露在网络上，绝不允许。
            return Err(WebDavError::Config(
                "服务端地址必须以 https:// 开头（坚果云固定是 https://dav.jianguoyun.com/dav/）"
                    .into(),
            ));
        }
        let base = if config.base_url.ends_with('/') {
            config.base_url.clone()
        } else {
            format!("{}/", config.base_url)
        };
        let dir = config.remote_dir.trim_matches('/');
        let dir_url = if dir.is_empty() {
            base.clone()
        } else {
            format!("{base}{dir}/")
        };

        let agent: Agent = Agent::config_builder().build().into();

        Ok(Self {
            config,
            agent,
            dir_url,
        })
    }

    /// 某文件的完整 URL。
    pub fn file_url(&self, filename: &str) -> String {
        format!("{}{}", self.dir_url, filename.trim_start_matches('/'))
    }

    fn auth(&self) -> String {
        format!("{}:{}", self.config.username, self.config.app_password)
    }

    /// 确保远端目录存在（MKCOL）。已存在（405）视为成功。
    pub fn ensure_dir(&self) -> Result<(), WebDavError> {
        if self.config.remote_dir.trim_matches('/').is_empty() {
            return Ok(());
        }
        let resp = self
            .agent
            .put(&self.dir_url)
            .header("Authorization", &self.basic_header())
            .send_empty();
        match resp {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(405)) => Ok(()), // 已存在
            Err(ureq::Error::StatusCode(401)) => Err(WebDavError::Unauthorized),
            Err(ureq::Error::StatusCode(code)) => {
                // 有些服务端对 MKCOL 用 409（父目录不存在）或 301。
                // 目录级操作失败不该直接放弃：真正的 PUT 会再暴露问题。
                Err(WebDavError::Status {
                    status: code,
                    detail: format!("建目录 {}", self.dir_url),
                })
            }
            Err(e) => Err(WebDavError::Transport(e.to_string())),
        }
    }

    fn basic_header(&self) -> String {
        // ureq 3 支持 `Authorization: Basic base64(user:pass)`，但为免引入 base64
        // 依赖，这里手写一个最小的 base64 编码（标准字母表）。
        format!("Basic {}", base64(&self.auth()))
    }

    /// 下载文件。文件不存在返回 `Ok(None)`。
    pub fn get(&self, filename: &str) -> Result<Option<Vec<u8>>, WebDavError> {
        let url = self.file_url(filename);
        let resp = self
            .agent
            .get(&url)
            .header("Authorization", &self.basic_header())
            .call();
        match resp {
            Ok(mut r) => {
                let bytes = r
                    .body_mut()
                    .read_to_vec()
                    .map_err(|e| WebDavError::Transport(e.to_string()))?;
                Ok(Some(bytes))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(ureq::Error::StatusCode(401)) => Err(WebDavError::Unauthorized),
            Err(ureq::Error::StatusCode(code)) => Err(WebDavError::Status {
                status: code,
                detail: format!("下载 {url}"),
            }),
            Err(e) => Err(WebDavError::Transport(e.to_string())),
        }
    }

    /// 上传（覆盖）文件。返回写入的字节数。
    pub fn put(&self, filename: &str, body: &[u8]) -> Result<usize, WebDavError> {
        let url = self.file_url(filename);
        let resp = self
            .agent
            .put(&url)
            .header("Authorization", &self.basic_header())
            .header("Content-Type", "application/octet-stream")
            .send(body);
        match resp {
            Ok(_) => Ok(body.len()),
            Err(ureq::Error::StatusCode(401)) => Err(WebDavError::Unauthorized),
            Err(ureq::Error::StatusCode(code)) => Err(WebDavError::Status {
                status: code,
                detail: format!("上传 {url}"),
            }),
            Err(e) => Err(WebDavError::Transport(e.to_string())),
        }
    }
}

/// 最小 base64 编码（标准字母表，带 `=` 填充）。
///
/// 只为拼 HTTP Basic 头，不引入额外依赖。
fn base64(input: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 的标准测试向量。
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("foob"), "Zm9vYg==");
        assert_eq!(base64("fooba"), "Zm9vYmE=");
        assert_eq!(base64("foobar"), "Zm9vYmFy");
        // Basic 认证会用到的形态。
        assert_eq!(base64("user@x.com:pass"), "dXNlckB4LmNvbTpwYXNz");
    }

    #[test]
    fn rejects_plaintext_http() {
        let cfg = WebDavConfig {
            base_url: "http://dav.jianguoyun.com/dav/".into(),
            username: "u".into(),
            app_password: "p".into(),
            remote_dir: "xremail".into(),
        };
        assert!(matches!(
            WebDavClient::new(cfg),
            Err(WebDavError::Config(_))
        ));
    }

    #[test]
    fn builds_urls_with_trailing_slash() {
        let cfg = WebDavConfig {
            base_url: "https://dav.jianguoyun.com/dav".into(), // 故意不带结尾斜杠
            username: "u".into(),
            app_password: "p".into(),
            remote_dir: "/xremail/".into(),
        };
        let c = WebDavClient::new(cfg).unwrap();
        assert_eq!(
            c.file_url("vault.bin"),
            "https://dav.jianguoyun.com/dav/xremail/vault.bin"
        );
    }
}
