//! 同步引擎的错误：**会话已经死了**的错误要能被认出来。
//!
//! ## 为什么需要这个模块
//!
//! [`xremail_core::AppError`] 是 `#[non_exhaustive]`，我们不打算给它加变体
//! （它被 UI、命令层、存储层一起匹配，加一个变体等于让全仓的 `match` 都重新审一遍）。
//! 但调度器需要区分两件**看起来一样**的事：
//!
//! * 「这次失败是因为**连接没了**」→ 立刻重连，从上次 UID 接着拉；
//! * 「这次失败是因为**这条命令本身不成立**」（文件夹不存在、加密方式不支持、
//!   本地解析失败……）→ 急着重连没有意义，按正常退避等下一轮。
//!
//! 两者在 `AppError::Protocol` 里分不开。所以这里给会话类错误**打一个前缀标记**，
//! 让 [`crate::net::is_session_recoverable`] 能认出来。
//!
//! ## 为什么是前缀而不是给 `AppError` 加变体
//!
//! `AppError` 的 `Display` 会一路走到 UI（`cmd.rs` 里按变体给用户文案）。
//! 前缀**不改变变体**，所以：
//!
//! * 内核层（`sync.rs` / `pull.rs`）给错误打标记，只多一层前缀；
//! * UI 层仍然收到 `AppError::Protocol`，文案只多了前缀——所以事件出口
//!   （[`crate::actor`] 的 `AccountEvent`）在 `to_string()` **之前**先剥一次
//!   （[`user_facing`]），用户看到的与以前一字不差；
//! * 其它 `match AppError` 的地方（`cmd.rs:913`、`push.rs` 的分类）不受影响，
//!   不需要改。

use xremail_core::AppError;

/// 会话类错误在 `Protocol` 载荷前打的标记。
///
/// 刻意选一个**用户在中文界面里绝不会看到**的串：`\u{1}`（SOH）不是可打印字符，
/// 就算某个展示路径漏了剥离，界面上也只是一处空白，而不是一串乱码。
pub const SESSION_CLASS_MARKER: &str = "\u{1}session:";

/// 给会话类错误打标记。
///
/// 只处理 [`AppError::Protocol`]：`Network` / `Auth` 等类别自己就说明了问题，
/// 不需要标记（也避免去掉标记时改变它们的语义）。
/// 已经是会话类的不重复叠加。
pub fn classify_session_error(error: AppError) -> AppError {
    match error {
        AppError::Protocol(message) if !message.contains(SESSION_CLASS_MARKER) => {
            AppError::Protocol(format!("{SESSION_CLASS_MARKER}{message}"))
        }
        other => other,
    }
}

/// 剥掉标记，返回原始载荷；不是会话类错误则返回 `None`。
pub fn strip_session_class(message: &str) -> Option<&str> {
    message.strip_prefix(SESSION_CLASS_MARKER)
}

/// 剥掉标记后剩下的文本（给展示用）。
///
/// 给事件出口用：用户看到的文案与打标记之前一模一样。
pub fn user_facing_text(message: &str) -> &str {
    strip_session_class(message).unwrap_or(message)
}

/// 剥掉标记，返回**展示用**的错误（已经剥过的不受影响，类别不变）。
///
/// 接受 `Display` 输出是因为**容易踩的坑**：[`AppError::with_context`]
/// 是 `format!("{prefix}: {m}")`，标记会被挤到中间（`protocol error: 列文件夹失败: \u{1}session: …`）。
/// 所以这里对 `ProtocolError` 的 `Display` 一律按标记**切开再拼回去**，
/// 前缀留在原位、标记消失。
pub fn user_facing(error: AppError) -> AppError {
    match error {
        AppError::Protocol(message) => match sanitize(&message) {
            Some(rest) => AppError::Protocol(rest),
            None => AppError::Protocol(message),
        },
        other => other,
    }
}

/// 把 `message` 里**所有**标记整体删掉（不止开头的那个）。
///
/// 两种落点：
/// * 开头（`\u{1}session:io error: …`）——直接切掉；
/// * 中间（`列文件夹失败: \u{1}session:io error: …`）——这是 `with_context`
///   拼出来的，切掉标记，但那个悬空的分隔符**留着**（`列文件夹失败: io error: …`），
///   这样展示文本与打标记之前**一字不差**。
fn sanitize(message: &str) -> Option<String> {
    if !message.contains(SESSION_CLASS_MARKER) {
        return None;
    }
    let cleaned = message
        .split(SESSION_CLASS_MARKER)
        .map(|part| part.strip_prefix(' ').unwrap_or(part))
        .collect::<Vec<_>>()
        .join("");
    Some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifying_is_idempotent() {
        // 注意 `Display` 自带类别前缀（`protocol error: …`），所以按**载荷**比较。
        let once = classify_session_error(AppError::Protocol("BYE".into()));
        let once_text = once.to_string();
        let payload = once_text
            .strip_prefix("protocol error: ")
            .expect("Display 前缀");
        assert_eq!(
            strip_session_class(payload).map(str::to_string),
            Some("BYE".to_string())
        );
        // 再判一次不会叠两层标记。
        let twice = classify_session_error(once);
        assert_eq!(twice.to_string(), once_text);
    }

    #[test]
    fn stripping_restores_the_exact_text() {
        let original = AppError::Protocol("IDLE 期间服务器主动断开（BYE）".into());
        let expected = original.to_string();
        let classified = classify_session_error(original);
        // 变体没变，只多了前缀。
        assert!(matches!(classified, AppError::Protocol(_)));
        assert_eq!(
            user_facing(classified).to_string(),
            expected,
            "剥完标记之后，用户看到的文案必须与打标记之前一字不差"
        );
    }

    #[test]
    fn non_protocol_errors_keep_their_category() {
        let classified = classify_session_error(AppError::Network("断网".into()));
        assert!(matches!(classified, AppError::Network(_)));
        let classified = classify_session_error(AppError::Auth("被拒".into()));
        assert!(matches!(classified, AppError::Auth(_)));
        let classified = classify_session_error(AppError::Storage("锁".into()));
        assert!(matches!(classified, AppError::Storage(_)));
    }

    #[test]
    fn user_facing_leaves_unmarked_errors_alone() {
        let plain = AppError::Protocol("文件夹不存在".into());
        assert_eq!(
            user_facing(plain).to_string(),
            "protocol error: 文件夹不存在"
        );
        assert_eq!(user_facing_text("文件夹不存在"), "文件夹不存在");
    }

    #[test]
    fn a_context_prefix_does_not_leak_the_marker() {
        // `with_context` 会把标记挤到中间，展示时必须照样干净。
        let err = classify_session_error(AppError::Protocol("io error: reset".into()))
            .with_context("列文件夹失败");
        assert_eq!(
            user_facing(err).to_string(),
            "protocol error: 列文件夹失败: io error: reset"
        );
    }

    #[test]
    fn the_marker_is_not_printable() {
        // 万一某个展示路径漏了剥离，界面也不该冒出乱码：标记以不可打印的
        // `\u{1}` 开头，剩下的部分只是一小段 ASCII 关键字。
        assert!(SESSION_CLASS_MARKER.starts_with('\u{1}'));
        assert!(SESSION_CLASS_MARKER.chars().skip(1).all(|c| c.is_ascii()));
    }
}
