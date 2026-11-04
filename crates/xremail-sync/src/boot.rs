//! 本进程的启动标识。
//!
//! ## 为什么是进程级的单例
//!
//! 这个值只有一个用途：在认领待提交操作时记一笔「这批行是这个进程在提交」，
//! 好让**下一次启动**的对账能区分「别人留下的残骸」和「我自己的在途操作」。
//!
//! 因此它必须满足一个**结构性**要求：同一进程内，所有会认领的地方拿到**同一个值**。
//! 如果让每个 actor、每个服务各生成一个，那么启动对账会把同进程另一个 actor
//! 正在提交的行当成残骸收成 `uncertain`——用户白确认一次，还烧掉一次尝试配额。
//!
//! 把「一致性」交给类型系统而不是约定：单例一旦初始化就不会再变，
//! 调用方没有机会传错。
//!
//! ## 为什么不用心跳 / 不落 meta 表
//!
//! 见迁移 `0006_operation_owner.sql`：这个值只在内存与该列之间流转，
//! 没有跨进程共享的价值。真正判「还活着吗」的是认领本身，
//! 不是一个后台心跳任务。
//!
//! ## 测试怎么造两代进程
//!
//! 需要模拟「上次进程」的用例不用碰这个单例——所有认领与对账函数都有
//! 显式接收 `boot_id` 的 `*_as` 变体，测试直接传 `"boot-a"` / `"boot-b"`。

use std::sync::OnceLock;

/// 本进程的启动标识。
static BOOT_ID: OnceLock<String> = OnceLock::new();

/// 取本进程的启动标识（首次调用时生成）。
pub fn boot_id() -> &'static str {
    BOOT_ID.get_or_init(generate_boot_id)
}

/// 生成一个进程启动标识。
///
/// 只需要满足两件事：**同一进程内唯一且稳定**、**不同进程（含重启后）几乎不撞**。
///
/// 用 `pid + 纳秒时间` 就够：撞上的前提是同一纳秒内 pid 被回收过，
/// 而那时数据库早已是另一次运行了。
/// 不为一个诊断字段引入随机数依赖。
pub fn generate_boot_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_id_is_stable_within_a_process() {
        // 这条钉的是本模块存在的理由：同一个进程里两次取必须相等，
        // 否则同进程的 actor 会互相把对方的在途操作当残骸回收。
        assert_eq!(boot_id(), boot_id());
    }

    #[test]
    fn the_boot_id_carries_the_process_id() {
        // 不要求能解析，只要求包含 pid：排查时能据此认出是哪一次运行。
        assert!(
            boot_id().starts_with(&std::process::id().to_string()),
            "boot id 应以 pid 开头，实际是 {}",
            boot_id()
        );
    }

    #[test]
    fn generated_ids_differ_between_calls() {
        // 不同进程用不同值，靠的是时间分量。
        // 同一纳秒内连调两次可能相等，所以只断言「不会永远相等」。
        let a = generate_boot_id();
        let b = generate_boot_id();
        let c = generate_boot_id();
        assert!(
            a != b || b != c,
            "连生成三个都完全相同，时间分量失效了：{a} / {b} / {c}"
        );
    }
}
