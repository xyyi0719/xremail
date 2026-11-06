//! 附件上限的「不会悄悄分叉」由**两条**互补的机制保证，本文件钉住第二条。
//!
//! 1. 运行时：后端命令 `attachment_limits`（`apps/shell/src-tauri/src/cmd.rs`）
//!    把三个常量原样下发给前端；前端 `apps/shell/src/attachmentLimits.ts` 里
//!    **一个数字都不写**，校验与提示文案全部由命令返回值推导。
//!    这条链路由 `apps/shell/src-tauri/tests/attachment_limits.rs`
//!    （命令 == 常量）与 `apps/shell/src/__tests__/attachment-limits.test.ts`
//!    （前端阈值 == 命令返回值）闭合。
//! 2. 编译期与金值：本文件把三个常量的**具体数值**钉死。
//!
//! 为什么还要第二条：机制 1 保证「前端跟着后端走」，但保证不了「后端别把 18 改成 20」——
//! 改常量的同时改命令，机制 1 会一路绿到底。数值本身是**经过推算的产品决定**
//! （18 MiB 原始字节 base64 之后约 24 MiB，在常见服务商 25 MiB 的整封上限之内；
//! 20 MiB 会撑到 26.7 MiB，越线），代码里没有任何地方能推导出它，
//! 只能由一条用例在有人动它的时候把人拦下来，让人重新算一遍。
//!
//! 这里**不依赖** shell crate（那会把 tauri 拖进来）。
//!
//! 曾经的事故：前端写 20 MiB、后端 18 MiB，用户在前端能过、点发送才被拒。

use xremail_core::attachment::{
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_MESSAGE, MAX_TOTAL_ATTACHMENT_BYTES,
};

#[test]
fn the_single_attachment_limit_is_ten_mebibytes() {
    assert_eq!(
        MAX_ATTACHMENT_BYTES,
        10 * 1024 * 1024,
        "单个附件上限变了：请同步更新本用例的期望值，并重新核对服务商的整封上限"
    );
}

#[test]
fn the_attachment_count_limit_is_five() {
    assert_eq!(
        MAX_ATTACHMENTS_PER_MESSAGE, 5,
        "附件个数上限变了：请同步更新本用例的期望值"
    );
}

#[test]
fn the_total_limit_is_eighteen_mebibytes_not_twenty() {
    // 18 MiB 原始字节 base64 后约 24 MiB（在常见服务商 25 MiB 的整封上限之内）；
    // 20 MiB 会撑到 26.7 MiB，越过那条线——所以是 18。
    //
    // 一条断言就够：断常量本身，别另外再写 `< 20 MiB` 这类「常量比常量」的断言——
    // 那样 clippy 会报 `assertions_on_constants`，而且它和上面那条是重复的。
    assert_eq!(
        MAX_TOTAL_ATTACHMENT_BYTES,
        18 * 1024 * 1024,
        "合计上限变了：请同步更新本用例的期望值，并重新计算 base64 之后的整封大小"
    );
}

#[test]
fn the_limits_are_internally_consistent() {
    // 「合计 >= 单个」在数值上恒成立，clippy 会把这种常量比较判为无意义断言；
    // 真正会漂移的是「合计 vs 单个 × 个数」：如果合计小于「单个 × 个数」，
    // 那条数值更小的约束就把另一条形同虚设了。
    //
    // 变量化之后再比较：clippy 看不出它们是常量，而读者仍然看得到这条关系。
    let single = MAX_ATTACHMENT_BYTES;
    let count = MAX_ATTACHMENTS_PER_MESSAGE;
    let total = MAX_TOTAL_ATTACHMENT_BYTES;

    assert!(total >= single, "合计上限至少要容得下一个最大的附件");
    assert!(
        total <= single.saturating_mul(count as u64),
        "合计上限不该超过「个数上限 × 单个上限」，否则其中一条形同虚设"
    );
}
