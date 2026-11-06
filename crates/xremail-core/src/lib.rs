//! Xremail 领域模型。
//!
//! 本 crate 是整个 workspace 的公共底座，**不得**依赖任何存储、网络或 UI crate
//! （不得出现 `sqlx` / `tokio` / `tauri` / `reqwest` 等引用）。
//! 其余所有 crate 都依赖它，从而保证依赖方向单一、无环。

pub mod address;
pub mod attachment;
pub mod error;
pub mod event;
pub mod html;
pub mod inline_error;
pub mod inline_image;
pub mod model;

pub use address::{
    AddressProblem, AddressReason, AddressValidation, NormalizedAddress, validate_addresses,
    validate_addresses_detailed,
};
pub use attachment::{
    AttachmentInput, AttachmentProblem, check_attachment_limits, check_attachment_limits_all,
};
pub use error::{AppError, AppResult};
pub use event::AppEvent;
pub use html::{html_to_text, sanitize_html};
pub use inline_error::InlineImageError;
pub use inline_image::{
    InlineImage, MAX_INLINE_IMAGE_BYTES, MAX_INLINE_TOTAL_BYTES, Rewrite, SAFE_IMAGE_CLOSE,
    SAFE_IMAGE_OPEN, inline_replacements, rewrite_inline_images,
};
pub use model::{
    Account, Address, Attachment, AuthKind, Folder, Message, MessageBody, MessageFlags,
    ProviderKind, ServerConfig, SmtpAuth, SyncMode, SyncState, TlsMode,
};
