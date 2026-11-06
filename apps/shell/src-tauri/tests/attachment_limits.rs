//! `attachment_limits` 命令：前端拿到的上限必须**就是**领域层的常量。
//!
//! 前端的写信面板不写死任何上限数字（见 `apps/shell/src/attachmentLimits.ts`），
//! 它启动时调这个命令拿数字，再据此校验、再据此显示「单个不超过 X」。
//! 所以这个命令必须原样转发 `xremail_core::attachment` 的常量——
//! 这里用一条贪心断言把三者钉死：谁在 `cmd.rs` 里手写一个 20 MiB，
//! 或者谁改了 `core` 的常量却忘了别处，这条用例会立刻红。

use xremail_core::attachment::{
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_MESSAGE, MAX_TOTAL_ATTACHMENT_BYTES,
};

#[test]
fn the_command_forwards_the_domain_constants_verbatim() {
    let limits = xremail_shell_lib::cmd::attachment_limits();

    assert_eq!(limits.max_attachment_bytes, MAX_ATTACHMENT_BYTES);
    assert_eq!(
        limits.max_attachments_per_message,
        MAX_ATTACHMENTS_PER_MESSAGE
    );
    assert_eq!(
        limits.max_total_attachment_bytes,
        MAX_TOTAL_ATTACHMENT_BYTES
    );
}

#[test]
fn the_serialized_shape_is_what_the_frontend_reads() {
    // 前端按 camelCase 读（`maxAttachmentBytes` 等），字段名改了前端就静默失效
    // （`isUsable` 会把它们当脏数据丢掉，于是前端永远拿不到上限）。
    let limits = xremail_shell_lib::cmd::attachment_limits();
    let json = serde_json::to_value(&limits).expect("上限结构必须能序列化");

    for field in [
        "maxAttachmentBytes",
        "maxAttachmentsPerMessage",
        "maxTotalAttachmentBytes",
    ] {
        assert!(
            json.get(field).is_some_and(|one| one.is_number()),
            "序列化结果里缺少数值字段 `{field}`：{json}"
        );
    }
}

#[test]
fn the_total_limit_is_eighteen_mebibytes() {
    // 用户拍板的口径：合计上限 18 MiB（不是 20 MiB）。
    // base64 会把 18 MiB 原始字节撑成约 24 MiB，仍在常见服务商 25 MiB 的整封上限之内。
    let limits = xremail_shell_lib::cmd::attachment_limits();
    assert_eq!(limits.max_total_attachment_bytes, 18 * 1024 * 1024);
}
