//! Xremail 邮件内容解析与构建。
//!
//! 职责边界（见 [ARCHITECTURE §2](../../../docs/ARCHITECTURE.md)）：
//! * MIME 结构、字符集解码、RFC 5322 头字段；
//! * **发信侧的 MIME 构建**（见 [`compose`]）：把收件人 / 标题 / 正文 / 附件
//!   拼成一份合规的邮件字节与 SMTP envelope 收件人；
//! * **不做** HTML 清洗（渲染层的事，见 ARCHITECTURE §7）——
//!   本 crate 只给出**未经清洗的**原始 HTML；
//! * **不落库、不发网络请求**（SMTP 传输不在本 crate）。
//!
//! ## 为什么需要这个 crate
//!
//! 2026-10-23 实测三家国内邮箱：
//!
//! | | 正文 charset | 主题 charset | 顶层是 multipart |
//! |---|---|---|---|
//! | 163 | UTF-8 | UTF-8 | 4/4 封是 |
//! | QQ | **GB2312** + UTF-8 | **GB2312** + UTF-8 | 15/20 封是 |
//! | 沃 | **GBK** + UTF-8 | **GBK** + UTF-8 | 5/6 封是 |
//!
//! 三个必须处理的现实：
//!
//! 1. **GB2312 / GBK 必须支持**——不能只做 UTF-8；
//! 2. **大量邮件没有 charset**（QQ 20 封里 11 封、沃 6 封里 5 封）——
//!    **猜**是必须的能力；
//! 3. **顶层几乎都是 multipart**——只看顶层 `Content-Type` 看不到正文的
//!    charset，必须**递归**拆解。

pub mod attachment;
pub mod body;
pub mod charset;
pub mod compose;
pub mod encoded_word;
pub mod mime;
pub mod transfer;

pub use attachment::{AttachmentMeta, attachment_meta, filename_from_headers};
pub use body::{
    MAX_ATTACHMENT_PARTS, MessageError, ParsedHeaders, ParsedMessage, parse_addresses,
    parse_headers, parse_message,
};
pub use charset::{decode, decode_lossy, encoding_for_label, guess_encoding, normalize_label};
pub use compose::{
    BuildOptions, BuiltMessage, ComposeError, OutgoingAttachment, OutgoingMessage, build_message,
};
// 附件类型与领域模型共用**同一个定义**：领域层（xremail-core）与构建层
// 各写一份的话，就得在中间加一层无意义的转换。
pub use encoded_word::{decode_encoded_words, encode_header_value};
pub use mime::{
    ContentType, HeaderMap, Part, parse_content_type, split_parts, split_parts_with_paths,
};
pub use xremail_core::model::SendAttachment;
