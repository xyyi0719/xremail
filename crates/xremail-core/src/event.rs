//! 应用事件。
//!
//! 事件是 Rust 侧 → 前端唯一的通知通道。前端**不轮询数据库**，
//! 而是在收到事件后做增量更新（见 [ARCHITECTURE §1](../../../docs/ARCHITECTURE.md)）。

use serde::{Deserialize, Serialize};

use crate::model::{Account, Folder, Message, SyncState};

/// 前端可订阅的事件。
///
/// 采用 `serde` 的 adjacent tagging（`type` + `payload`），
/// 方便在 TypeScript 侧用判别联合（discriminated union）消费。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum AppEvent {
    /// 新增账户。
    AccountAdded {
        /// 新账户。
        account: Account,
    },

    /// 账户配置变更（含凭据更新、状态变化等）。
    AccountUpdated {
        /// 更新后的账户。
        account: Account,
    },

    /// 账户被删除，其本地数据应一并清理。
    AccountRemoved {
        /// 被删除的账户 ID。
        account_id: String,
    },

    /// 文件夹新增或变更。
    FolderUpserted {
        /// 文件夹。
        folder: Folder,
    },

    /// 文件夹被删除。
    FolderRemoved {
        /// 所属账户 ID。
        account_id: String,
        /// 文件夹 ID。
        folder_id: String,
    },

    /// 邮件新增或元数据变更（含已读、星标等状态位）。
    MessageUpserted {
        /// 邮件。
        message: Message,
    },

    /// 邮件被删除（服务器 EXPUNGE 或本地删除）。
    MessageRemoved {
        /// 所属账户 ID。
        account_id: String,
        /// 所属文件夹 ID。
        folder_id: String,
        /// 邮件 ID。
        message_id: String,
    },

    /// 账户/文件夹的同步状态变化。
    SyncStateChanged {
        /// 所属账户 ID。
        account_id: String,
        /// 相关文件夹 ID，`None` 表示账户级状态。
        folder_id: Option<String>,
        /// 新状态。
        state: SyncState,
    },

    /// 需要弹系统通知。
    Notification {
        /// 通知标题。
        title: String,
        /// 通知正文。
        body: String,
        /// 关联账户 ID，用于点击后跳转。
        account_id: Option<String>,
        /// 关联邮件 ID，用于点击后打开。
        message_id: Option<String>,
    },

    /// 同步错误（需要展示给用户）。
    SyncError {
        /// 所属账户 ID。
        account_id: String,
        /// 人类可读的错误描述。
        message: String,
    },

    /// 操作队列被系统改动过，界面应重查（目前只有「同步后对账」会发）。
    ///
    /// 不带明细：对账的产物是「哪几行换了状态」，而界面本来就要按账户重拉
    /// 一次队列——多传一份可能过期的快照只会多一处对不上的来源。
    OperationsChanged {
        /// 所属账户 ID。
        account_id: String,
    },
}

impl AppEvent {
    /// 事件名称（前端按此字符串订阅）。
    pub fn name(&self) -> &'static str {
        match self {
            AppEvent::AccountAdded { .. } => "account://added",
            AppEvent::AccountUpdated { .. } => "account://updated",
            AppEvent::AccountRemoved { .. } => "account://removed",
            AppEvent::FolderUpserted { .. } => "folder://upserted",
            AppEvent::FolderRemoved { .. } => "folder://removed",
            AppEvent::MessageUpserted { .. } => "message://upserted",
            AppEvent::MessageRemoved { .. } => "message://removed",
            AppEvent::SyncStateChanged { .. } => "sync://state-changed",
            AppEvent::Notification { .. } => "notify://new-mail",
            AppEvent::SyncError { .. } => "sync://error",
            AppEvent::OperationsChanged { .. } => "operations://changed",
        }
    }
}
