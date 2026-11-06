//! 数据访问对象。
//!
//! 约定：
//! * 每个函数只做一件事：参数已由调用方（Command / 服务层）校验。
//! * SQL 全部使用 `sqlx` 的编译期宏，查询与 schema 的一致性在**编译期**校验，
//!   依赖 `.sqlx/` 离线查询数据（见仓库根目录的 `SQLX_OFFLINE` 说明）。
//! * 不在这里做业务规则；业务规则属于服务层。

pub mod account;
pub mod attachment;
pub mod dkim;
pub mod folder;
pub mod message;
pub mod operation;
pub mod search;
pub mod send_queue;
pub mod sync;
