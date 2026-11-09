//! Xremail IMAP 协议层。
//!
//! 职责边界（见 [ARCHITECTURE §2](../../../docs/ARCHITECTURE.md)）：
//! * IMAP 会话、能力与命令结果。
//! * **不写库**，**不决定重试**（那是 `xremail-sync` 的事）。
//! * 不认识 Tauri，也不认识 UI。
//!
//! ## 安全约定
//!
//! * 只走隐式 TLS，强制校验服务端证书，**不提供**明文或跳过校验的开关
//!   （见 [SECURITY §2.2](../../../docs/SECURITY.md)）。
//! * 错误信息**不含**用户名或口令。
//! * 探测流程严格只读（见 [`probe`]）。

pub mod append;
pub mod command;
pub mod connection;
pub mod credential;
pub mod error;
pub mod folders;
pub mod idle;
pub mod probe;
pub mod transfer;
pub mod utf7;

pub use append::{AppendOptions, AppendOutcome, append_message, build_append_command};
pub use command::{
    CommandOutcome, DEFAULT_COMMAND_TIMEOUT, FetchedMessage, ListedMailbox, MailboxInfo,
    examine_checked, examine_checked_with_timeout, run_checked, run_checked_with_timeout,
};
pub use connection::{
    ImapConfig, ImapStream, Session, TlsInfo, TlsTrust, connect_and_login, connect_and_login_with,
};
pub use credential::{
    AccountCredential, CredentialError, CredentialStore, default_accounts_path, mask_user,
};
pub use error::ProtocolError;
pub use folders::{RemoteFolder, list_folders, list_folders_with_timeout};
pub use idle::{IdleConfig, IdleEvent, IdleOutcome, idle_once};
pub use probe::{
    ExtensionSupport, FolderSummary, MailboxSummary, ProbeConfig, ProbeReport, auth_mechanisms,
    capability_names, extension_support, probe,
};
pub use transfer::{
    TransferOutcome, build_copy_command, build_move_command, build_store_deleted_command,
    build_uid_expunge_command, copy_messages, copy_messages_with_timeout, mark_deleted,
    mark_deleted_with_timeout, move_messages, move_messages_with_timeout, uid_expunge,
    uid_expunge_with_timeout,
};
pub use utf7::{Utf7Error, decode as decode_utf7, encode as encode_utf7};

/// 重新导出 async-imap 的能力类型，供调用方判断扩展支持度。
pub use async_imap::types::Capabilities;

/// 能力集里有没有 `name`，**大小写不敏感**。
///
/// `Capabilities::has_str` 对普通 atom 是区分大小写的精确比较
/// （只有 `IMAP4rev1` 与 `AUTH=` 例外）。RFC 3501 的能力名是 atom，
/// 服务器大小写写花并不罕见——按精确比较会把「有」认成「没有」，
/// 然后走上错误的路由（例如把有 `MOVE` 的服务器当成没有）。
#[must_use]
pub fn has_capability(caps: &Capabilities, name: &str) -> bool {
    caps.iter().any(|cap| match cap {
        async_imap::types::Capability::Atom(atom) => atom.eq_ignore_ascii_case(name),
        async_imap::types::Capability::Imap4rev1 => name.eq_ignore_ascii_case("IMAP4rev1"),
        async_imap::types::Capability::Auth(mechanism) => {
            name.split_once('=').is_some_and(|(key, value)| {
                key.eq_ignore_ascii_case("AUTH") && mechanism.eq_ignore_ascii_case(value)
            })
        }
    })
}
