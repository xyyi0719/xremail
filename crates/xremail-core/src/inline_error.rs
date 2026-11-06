//! 内联图片（`cid:`）相关的错误类型。
//!
//! 与 [`crate::error::AppError`] 分开：这一层是「这张图能不能安全内联」的
//! **纯判定**，不涉及网络与存储。调用方可以把它们翻成 `AppError::Invalid`，
//! 或者干脆当作「跳过这张图」——两种处理都不会让不可信内容进到正文里。

use std::fmt;

use crate::inline_image::MAX_INLINE_IMAGE_BYTES;

/// 内联图片不能成立的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InlineImageError {
    /// `Content-ID` 为空。
    EmptyContentId,
    /// MIME 不是 `image/*`（或是不内联的 SVG）。
    NotAnImage,
    /// 部件不是内联部件（`Content-Disposition: attachment`）。
    NotInline,
    /// 部件还没下载到本地。
    NotDownloaded,
    /// 字节为空。
    Empty,
    /// 超过单张上限。
    TooLarge {
        /// 实际字节数。
        actual: u64,
        /// 允许的上限。
        limit: u64,
    },
    /// 本地文件大小与库里记的不一致（文件被外部改动或损坏）。
    SizeMismatch {
        /// 库里记的大小。
        expected: i64,
        /// 实际读到的大小。
        actual: u64,
    },
    /// 读本地文件失败。
    Io(String),
}

impl fmt::Display for InlineImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InlineImageError::EmptyContentId => f.write_str("内联图片没有 Content-ID"),
            InlineImageError::NotAnImage => f.write_str("内联部件的类型不是安全的图片"),
            InlineImageError::NotInline => f.write_str("该附件不是内联部件"),
            InlineImageError::NotDownloaded => f.write_str("该内联部件还没有下载到本地"),
            InlineImageError::Empty => f.write_str("内联图片的字节为空"),
            InlineImageError::TooLarge { actual, limit } => {
                write!(f, "内联图片太大：{actual} 字节，上限 {limit} 字节")
            }
            InlineImageError::SizeMismatch { expected, actual } => {
                write!(f, "内联图片文件大小对不上：库里 {expected}，实际 {actual}")
            }
            InlineImageError::Io(message) => write!(f, "读取内联图片失败：{message}"),
        }
    }
}

impl std::error::Error for InlineImageError {}

impl InlineImageError {
    /// 单张上限（便于调用方展示）。
    pub const fn limit() -> u64 {
        MAX_INLINE_IMAGE_BYTES
    }
}
