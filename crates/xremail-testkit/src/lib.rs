//! Xremail 测试替身。
//!
//! 职责边界（见 [ARCHITECTURE §2](../../../docs/ARCHITECTURE.md)）：
//! * 假服务器、合成样本、故障注入。
//! * **测试专用**——不进生产构建。
//!
//! ## 为什么需要假服务器
//!
//! 真实邮箱只能复现**一种**情况，而各家服务商的怪癖各不相同：
//! 163 拒绝 `EXAMINE`、要求先发 `ID`、认证前拒绝一切命令、UID 空间巨大……
//! 把这些行为做成**开关**，就能在 CI 里把它们全部跑一遍，
//! 而不是每次改动都去动真邮箱。
//!
//! ## 用法
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use xremail_testkit::{FakeImapServer, ServerConfig};
//!
//! let server = FakeImapServer::start(ServerConfig::with_mailboxes()).await?;
//! let port = server.port();
//! // 用 port 连客户端；TLS 要用 server.client_config()（只信自签 CA）。
//! server.shutdown().await;
//! # Ok(())
//! # }
//! ```

pub mod certificate;
pub mod config;
pub mod fetch;
pub mod parser;
pub mod search;
pub mod sequence;
pub mod server;
pub mod smtp;

pub use certificate::{CertError, ServerCertificate};
pub use config::{Fault, InterleavedUntagged, MailboxFixture, MessageFixture, ServerConfig};
pub use fetch::{BodySection, FetchItem, StoreOperation, StoreRequest, parse_fetch, parse_store};
pub use parser::Command;
pub use search::SearchKey;
pub use sequence::SequenceSet;
pub use server::{FakeImapServer, ServerError};
pub use smtp::{FakeSmtpServer, SmtpServerConfig, SmtpServerError};
