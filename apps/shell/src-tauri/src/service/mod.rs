//! 应用服务层。
//!
//! 职责边界（见 [ARCHITECTURE §2](../../../../../docs/ARCHITECTURE.md)）：
//! * Command 只做 **参数校验 → 调用领域服务 → 返回 DTO**，不写业务逻辑。
//! * 业务逻辑在服务层。
//!
//! ## 为什么服务层暂时住在 shell crate 里
//!
//! P0 的账户服务只是「store 的 DAO + 入参校验」的组合，还没有任何独立的行为。
//! 为它单开一个 crate 属于过早抽象。等 P2 的同步、收发服务落地、
//! 服务层有了真正的业务规则，再拆成独立的领域 crate 或 `xremail-app`。
//!
//! 服务层**不认识 Tauri**：发事件是 Command 的事（状态提交后发，见
//! [ARCHITECTURE §1.6](../../../../../docs/ARCHITECTURE.md)）。

pub mod account;
pub mod compose;
pub mod credential;
pub mod message;
pub mod scheduler;
pub mod sent_copy;
pub mod sync;

pub use account::AccountService;
pub use compose::{ComposeService, DraftDto, DraftInput, SendResult, SendResultState};
pub use credential::CredentialService;
pub use message::{FolderDto, MessageFlag, MessageService, SearchHitDto};
pub use sent_copy::{SentCopyOutcome, SentCopyService};
pub use sync::SyncService;

/// 当前时间（epoch 毫秒）。
///
/// 落库的时间戳统一走这里，避免每个服务各写一份。
pub(crate) fn current_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 生成本地唯一的操作 ID（UUID v4 的形状）。
///
/// **为什么不用 `uuid` crate**：加依赖要改 `Cargo.toml`，
/// 而那个文件正有别的改动在动它。这里自己拼 16 字节：
/// 随机源用 [`RandomState`]（每个实例的哈希键都是随机的），
/// 再混上纳秒时间戳与进程内自增计数器。
///
/// 这个 ID 只需要**本地唯一**（`mail_operation.id` 是主键），
/// 不需要不可预测，所以随机源不必是密码学安全的。
pub(crate) fn new_operation_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut bytes = [0u8; 16];
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(nanos);
        hasher.write_u64(counter);
        hasher.write_u64(index as u64);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }

    // UUID v4 的形状：版本位与变体位固定，别的位随机。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
