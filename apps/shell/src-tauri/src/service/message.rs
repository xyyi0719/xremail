//! 邮件应用服务。
//!
//! 把「库里的邮件」变成「前端要的 DTO」，并按需拉取正文。
//! 中间不出现 Tauri 类型。
//!
//! ## 标记变更：快照 + 意图
//!
//! 服务器的标记**快照**与本地「用户还没提交的操作」是**两份不同的数据**
//! （见 [ARCHITECTURE §6](../../../../../docs/ARCHITECTURE.md)）：
//!
//! * 快照：服务器现在认为这封信是什么状态（存在 `message.flags` 里）；
//! * 意图：用户在本机做了什么、还没被服务器确认（存在 `mail_operation` 里）。
//!
//! UI 显示的是**两者叠加**的结果，所以用户点一下星标，界面立刻变；
//! 但这**不代表**服务器已经接受——叠加只是显示层的事。

use serde::{Deserialize, Deserializer, Serialize};
use sqlx::SqlitePool;
use xremail_core::AppResult;
use xremail_core::model::{
    FlagIntentPayload, Folder, MailOperation, Message, MessageBody, MessageFlags, OperationKind,
    OperationState,
};
use xremail_store::dao::folder as folder_dao;
use xremail_store::dao::message as message_dao;
use xremail_store::dao::operation as operation_dao;
use xremail_store::dao::search as search_dao;
use xremail_sync::intent::{FlagIntent, apply_intents};

use super::{current_time_ms, new_operation_id};

/// 一个文件夹（给前端看的形状）。
///
/// **不要**直接返回领域模型 [`Folder`]：它的 `remote_name` 是
/// **线路上的原样**（modified UTF-7，如 `&XfJT0ZAB-`），
/// 那是发命令时要用的，不是给人看的。
/// 一条搜索命中（给前端结果列表用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHitDto {
    /// 邮件的本地 id。
    pub message_id: String,
    pub account_id: String,
    pub folder_id: String,
    /// IMAP UID —— 打开邮件时用它（与 `open_message` 的参数一致）。
    pub uid: i64,
    pub subject: String,
    /// 发件人显示串（多个人用「, 」隔开）。
    pub from: String,
    /// 发信时间（epoch 毫秒）。
    pub date_sent: Option<i64>,
    /// 命中片段（命中词用 `**` 包住）。
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDto {
    /// 本地 ID。
    pub id: String,
    /// 显示名（已解码的 modified UTF-7，如「草稿箱」）。
    pub name: String,
    /// 线路上的原样（发命令时用）。
    pub raw_name: String,
    /// 层级分隔符。
    pub delimiter: Option<String>,
    /// 服务器当前的 epoch（`None` = 还没同步过）。
    pub uidvalidity: Option<i64>,
    /// 已同步到的最大 UID。
    pub last_uid: Option<i64>,
    /// 上次同步时间（epoch 毫秒）。
    pub last_sync_at: Option<i64>,
}

impl FolderDto {
    /// 把领域模型变成 DTO：显示名解码，`local_name` 优先。
    pub fn from_domain(folder: &Folder) -> Self {
        let decoded = xremail_protocol_imap::decode_utf7(&folder.remote_name)
            .unwrap_or_else(|_| folder.remote_name.clone());
        Self {
            id: folder.id.clone(),
            name: folder.local_name.clone().unwrap_or(decoded),
            raw_name: folder.remote_name.clone(),
            delimiter: folder.delimiter.clone(),
            uidvalidity: folder.uidvalidity,
            last_uid: folder.last_uid,
            last_sync_at: folder.last_sync_at,
        }
    }
}

/// 用户可以操作的标记位。
///
/// **只**开放「已读」与「星标」两个：它们是 IMAP 的标准系统标记
/// （`\Seen` / `\Flagged`），语义在所有服务商上都是一样的。
/// 别的位（`\Deleted`、`\Draft`、`\Answered`）要么不该由用户随手点，
/// 要么本客户端还没实现对应的工作流——放进来只会做出「点了但没有后果」的按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageFlag {
    /// 已读 / 未读（IMAP `\Seen`）。
    Seen,
    /// 星标 / 取消星标（IMAP `\Flagged`）。
    Flagged,
}

impl MessageFlag {
    /// 该标记对应的 IMAP 系统标记位。
    pub fn bit(self) -> u32 {
        match self {
            MessageFlag::Seen => MessageFlags::SEEN,
            MessageFlag::Flagged => MessageFlags::FLAGGED,
        }
    }

    /// 解析前端传来的标记名。
    ///
    /// 只认 `seen` 与 `flagged`（大小写不敏感）；别的名字一律报错，
    /// 并把可选值**列出来**——用户看到的错误要能告诉他"能填什么"。
    pub fn parse(raw: &str) -> AppResult<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "seen" => Ok(MessageFlag::Seen),
            "flagged" => Ok(MessageFlag::Flagged),
            other => Err(xremail_core::AppError::Invalid(format!(
                "不支持的标记 {other:?}：只支持 \"seen\"（已读）和 \"flagged\"（星标）"
            ))),
        }
    }
}

/// 手写的反序列化：把 serde 自己的英文报错换成中文说明。
///
/// serde 派生出来的版本会报
/// `unknown variant 'xxx', expected 'seen' or 'flagged'`——
/// 用户看不懂。派生版本会丢掉「能填什么」这条最有用的信息。
impl<'de> Deserialize<'de> for MessageFlag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        MessageFlag::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// 邮件服务。
/// 给渲染层的正文。
///
/// **只有消毒过的 HTML**。
///
/// 原始 HTML（`html_raw`）留在数据库里，**不出后端**：
/// 前端拿到的是 `html_safe`——它过了 [`xremail_core::html::sanitize_html`]，
/// 标签与属性都在白名单里，脚本、事件处理器、远程图片、样式都没了。
///
/// ## 为什么不让前端自己消毒
///
/// 前端那份消毒（编辑器的 DOM 版）只对**编辑器里的内容**有效；
/// 渲染邮件正文必须用后端这一份：后端消毒器是手写扫描器，
/// 不依赖浏览器的解析行为，绕不过去。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBodyView {
    /// 邮件 id。
    pub message_id: String,
    /// 纯文本正文（HTML-only 的邮件是从 HTML 派生的）。
    pub text_plain: Option<String>,
    /// **已消毒**的 HTML。可以安全地交给 `dangerouslySetInnerHTML`。
    pub html_safe: Option<String>,
    /// 正文字符集。
    pub charset: Option<String>,
}

impl MessageBodyView {
    /// 从领域对象构造（顺带消毒 HTML）。
    pub fn from_domain(body: MessageBody) -> Self {
        Self::from_domain_with_images(body, &[])
    }

    /// 从领域对象构造，并把 `cid:` 内联图片换成 `data:`。
    ///
    /// `images` 是**已经读进内存**的内联图片（见
    /// [`xremail_core::inline_image`]）。处理顺序是刻意的：
    /// 1. 先把能对上的 `<img src="cid:…">` 换成 `data:`，并用
    ///    [`xremail_core::inline_image::SAFE_IMAGE_OPEN`] 之类的一次性控制字符
    ///    把这段**圈起来**；
    /// 2. 圈外的一切照旧过消毒器（远程图片、脚本、样式都照删）；
    /// 3. 圈内的片段是**我们自己拼的**（只有 `src="data:image/…"` 与转义过的 `alt`），
    ///    直接把圈去掉。
    ///
    /// 这样即使消毒器的行为再变，也不会出现「没经过消毒的原文混进输出」：
    /// 圈内只可能是本函数自己写出去的字节。
    pub fn from_domain_with_images(
        body: MessageBody,
        images: &[xremail_core::inline_image::InlineImage],
    ) -> Self {
        let html_safe = body
            .html_raw
            .as_deref()
            .map(|html| sanitize_with_inline_images(html, images))
            .filter(|html| !html.is_empty());
        MessageBodyView {
            message_id: body.message_id,
            text_plain: body.text_plain,
            html_safe,
            charset: body.charset,
        }
    }
}

/// 消毒 HTML，但保留**本机已下载**的内联图片。
///
/// 没有内联图片时与 [`xremail_core::html::sanitize_html`] 完全等价。
pub fn sanitize_with_inline_images(
    html: &str,
    images: &[xremail_core::inline_image::InlineImage],
) -> String {
    use xremail_core::inline_image::{SAFE_IMAGE_CLOSE, SAFE_IMAGE_OPEN, rewrite_inline_images};

    if images.is_empty() {
        return xremail_core::html::sanitize_html(html);
    }

    let rewritten = rewrite_inline_images(html, images);
    let mut out = String::with_capacity(rewritten.html.len());
    let mut rest = rewritten.html.as_str();

    while let Some(start) = rest.find(SAFE_IMAGE_OPEN) {
        out.push_str(&xremail_core::html::sanitize_html(&rest[..start]));
        let after_open = &rest[start + SAFE_IMAGE_OPEN.len()..];
        let Some(end) = after_open.find(SAFE_IMAGE_CLOSE) else {
            // 理论上不会发生（改写与扫描是同一个函数），兜底：剩下的全交给消毒器。
            rest = after_open;
            break;
        };
        out.push_str(&after_open[..end]);
        rest = &after_open[end + SAFE_IMAGE_CLOSE.len()..];
    }
    out.push_str(&xremail_core::html::sanitize_html(rest));

    out
}

#[derive(Debug, Clone)]
pub struct MessageService {
    pool: SqlitePool,
}

impl MessageService {
    /// 用连接池构造。
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 列出某封邮件的附件（只含元数据，**不含字节**）。
    pub async fn list_attachments(
        &self,
        message_id: &str,
    ) -> AppResult<Vec<xremail_core::model::Attachment>> {
        xremail_store::dao::attachment::list_attachments(&self.pool, message_id).await
    }

    /// 列出某账户的全部文件夹（**显示名已解码**）。
    pub async fn list_folders(&self, account_id: &str) -> AppResult<Vec<FolderDto>> {
        let folders = folder_dao::list_folders(&self.pool, account_id).await?;
        Ok(folders.iter().map(FolderDto::from_domain).collect())
    }

    /// 取某文件夹的**领域模型**（含 epoch 与线路上的原样名）。
    ///
    /// 回写标记要用它：`push_flags` 需要的是 [`Folder`]（有 `remote_name`
    /// 与 `uidvalidity`），不是给人看的 DTO。
    pub async fn get_folder(&self, account_id: &str, folder_id: &str) -> AppResult<Option<Folder>> {
        let folder = folder_dao::get_folder(&self.pool, folder_id).await?;
        Ok(folder.filter(|folder| folder.account_id == account_id))
    }

    /// 列出某文件夹里的邮件（按 UID 升序）。
    ///
    /// 返回前会把该文件夹**未完结**的标记意图叠到服务器快照上：
    /// 用户点完星标，界面立刻就变，不用等服务器回话。
    ///
    /// 但这**只是显示层**——服务器还没确认过这些改动。
    pub async fn list_messages(
        &self,
        account_id: &str,
        folder_id: &str,
    ) -> AppResult<Vec<Message>> {
        let mut messages = message_dao::list_messages(&self.pool, account_id, folder_id).await?;

        // 未完结的操作 = 用户改了、服务器还没确认的那些。
        let pending =
            operation_dao::list_pending_operations(&self.pool, account_id, folder_id).await?;
        let epoch = self
            .get_folder(account_id, folder_id)
            .await?
            .and_then(|f| f.uidvalidity);
        overlay_pending_intents(&mut messages, &pending, epoch)?;

        Ok(messages)
    }

    /// 搜索本地缓存里的邮件。
    ///
    /// 搜索只落在**本地缓存**上（不查服务器）：远端 IMAP 的搜索能力不可靠
    /// ——163 的 `UID SEARCH` 只认 `ALL`，其它关键字静默失效（已知风险 R23）。
    /// 代价是搜不到还没同步下来的邮件。
    pub async fn search(&self, query: &str, limit: usize) -> AppResult<Vec<SearchHitDto>> {
        let hits = search_dao::search(&self.pool, query, limit).await?;

        let mut results = Vec::with_capacity(hits.len());
        for hit in hits {
            // 命中行只带邮件 id；列表要显示主题/发件人/日期，
            // 所以再取一次消息本身（本地 SQLite，逐条查很便宜）。
            let Some(message) = message_dao::get_message(&self.pool, &hit.message_id).await? else {
                // 索引里有、消息表里没有 = 数据不一致（不该发生）。
                // 跳过而不是报错——一条坏命中不该让整个搜索报错。
                continue;
            };

            results.push(SearchHitDto {
                message_id: message.id.clone(),
                account_id: message.account_id.clone(),
                folder_id: message.folder_id.clone(),
                uid: message.uid,
                subject: message.subject.clone().unwrap_or_default(),
                from: message
                    .from
                    .iter()
                    .map(|address| match &address.name {
                        Some(name) if !name.trim().is_empty() => {
                            format!("{name} <{}>", address.email)
                        }
                        _ => address.email.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                date_sent: message.date_sent,
                snippet: hit.snippet,
            });
        }

        Ok(results)
    }

    /// 读一封邮件的正文（**已缓存**的）。
    ///
    /// 返回 `None` 表示本地还没有正文——需要按需去服务器拉。
    pub async fn get_body(
        &self,
        account_id: &str,
        folder_id: &str,
        uid: i64,
    ) -> AppResult<Option<MessageBody>> {
        let messages = message_dao::list_messages(&self.pool, account_id, folder_id).await?;
        let Some(message) = messages.into_iter().find(|message| message.uid == uid) else {
            return Ok(None);
        };
        message_dao::get_message_body(&self.pool, &message.id).await
    }

    /// 读一封邮件的正文，**给渲染层用**（HTML 已消毒）。
    ///
    /// 返回 `None` 表示本地还没有正文——需要按需去服务器拉。
    pub async fn get_body_view(
        &self,
        account_id: &str,
        folder_id: &str,
        uid: i64,
    ) -> AppResult<Option<MessageBodyView>> {
        Ok(self
            .get_body(account_id, folder_id, uid)
            .await?
            .map(MessageBodyView::from_domain))
    }

    /// 按远端身份取一封邮件。
    pub async fn get_message(
        &self,
        account_id: &str,
        folder_id: &str,
        uid: i64,
    ) -> AppResult<Option<Message>> {
        let messages = message_dao::list_messages(&self.pool, account_id, folder_id).await?;
        Ok(messages.into_iter().find(|message| message.uid == uid))
    }

    /// 记录一次标记变更意图（落库排队，等回写）。
    ///
    /// ## 为什么只写意图、不直接改快照
    ///
    /// 本地快照代表**服务器确认过的状态**。如果用户点一下星标就把它改掉，
    /// 服务器拒绝时就没人知道该退回到哪个状态了。所以这里只**追加一条意图**，
    /// 由同步层读服务器快照、重放意图、算出净变化、发命令、回读确认
    /// （见 [ARCHITECTURE §6](../../../../../docs/ARCHITECTURE.md)）。
    ///
    /// ## 只动自己那一个位
    ///
    /// 置位用 `add`、清位用 `remove`，**只**包含被点的那个位。
    /// 整组替换 `FLAGS` 会把别的位（服务器刚打的 `\Recent` 之类）也抹掉。
    ///
    /// 失败行为：
    /// * 文件夹不存在 → [`xremail_core::AppError::NotFound`]；
    /// * 文件夹**没同步过**（没有 epoch）→ [`xremail_core::AppError::Invalid`]，
    ///   因为不知道该把意图绑到哪个 epoch 上；
    /// * 本地没有这封邮件 → [`xremail_core::AppError::NotFound`]。
    pub async fn set_flag_intent(
        &self,
        account_id: &str,
        folder_id: &str,
        uid: i64,
        flag: MessageFlag,
        value: bool,
    ) -> AppResult<MailOperation> {
        let folder = self
            .get_folder(account_id, folder_id)
            .await?
            .ok_or_else(|| {
                xremail_core::AppError::NotFound(format!("文件夹 {folder_id} 不存在"))
            })?;

        // epoch 必须已知：UID 只在 (文件夹, epoch) 内有意义。
        let epoch = folder.uidvalidity.ok_or_else(|| {
            xremail_core::AppError::Invalid(
                "该文件夹还没同步过，不知道 epoch，无法安全地标记邮件".into(),
            )
        })?;

        let message = message_dao::list_messages(&self.pool, account_id, folder_id)
            .await?
            .into_iter()
            .find(|message| message.uid == uid)
            .ok_or_else(|| {
                xremail_core::AppError::NotFound(format!(
                    "本地没有 UID {uid} 这封邮件，可能还没同步下来"
                ))
            })?;

        // 邮件行与文件夹的 epoch 不一致 → 这行是旧 epoch 的残留，
        // 拿它当目标就是把意图套到另一封信上。
        if message.uidvalidity != epoch {
            return Err(xremail_core::AppError::Invalid(format!(
                "这封邮件的 epoch（{}）与文件夹当前 epoch（{epoch}）不一致，请先重新同步一次",
                message.uidvalidity
            )));
        }

        let bit = flag.bit();
        let (add, remove) = if value {
            (MessageFlags::empty().with(bit), MessageFlags::empty())
        } else {
            (MessageFlags::empty(), MessageFlags::empty().with(bit))
        };

        let now = current_time_ms();
        let operation = MailOperation {
            id: new_operation_id(),
            account_id: account_id.to_string(),
            folder_id: folder_id.to_string(),
            uid,
            uidvalidity: epoch,
            kind: OperationKind::Flags,
            intent: FlagIntentPayload::new(add, remove),
            destination_folder_id: None,
            state: OperationState::Queued,
            // `seq` 由存储层分配（同一账户 + 文件夹内严格递增），这里先占位。
            seq: 0,
            base_version: None,
            attempts: 0,
            error_kind: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        };

        operation_dao::enqueue_operation(&self.pool, &operation).await?;

        // 回读一次：把存储层分配的 `seq` 拿回来（提交时按它定序）。
        //
        // 不能按自己构造的 id 去读：入队是幂等的（R34），同一条通道已有未完结
        // 意图时不会插入新行，于是这个 id 根本不在库里。也**不能**按「这封邮件
        // 有没有在途的移动」去读（`find_pending_transfer`）：同一封邮件完全可以
        // 同时挂着一条在途的移动，那会被误当成刚落库的旗标。
        //
        // 取**这一位**上 `seq` 最大的未完结意图——`apply_intents` 就是按 `seq`
        // 定序、后发覆盖先发的。只取「最早那条」会把刚点的取消读成「还要置位」。
        let pending =
            operation_dao::list_pending_operations(&self.pool, account_id, folder_id).await?;
        let stored = pending
            .into_iter()
            .filter(|candidate| {
                candidate.uid == uid
                    && candidate.uidvalidity == epoch
                    && candidate.kind == xremail_core::model::OperationKind::Flags
                    && (candidate.intent.add.contains(bit) || candidate.intent.remove.contains(bit))
            })
            .max_by_key(|candidate| candidate.seq)
            .ok_or_else(|| xremail_core::AppError::Storage("操作已落库，但读不回来".into()))?;
        Ok(stored)
    }

    /// 把一批邮件摄进另一个文件夹（**只入队**，提交由同步轮次做）。
    ///
    /// 为什么不在命令里直接发 `MOVE`：用户可能离线、可能刚断网，
    /// 队列的意义就是「意图先被记住」。排空队列的地方在
    /// [`xremail_sync::sync_account`]（每轮拉取之后）——同步器在后台跑，
    /// 不需要界面一直开着。
    pub async fn move_intent(
        &self,
        account_id: &str,
        folder_id: &str,
        uids: &[i64],
        destination_folder_id: &str,
    ) -> AppResult<Vec<MailOperation>> {
        // `get_folder` 本身就是按账户查的：别的账户的文件夹在这里查不到，
        // 所以「跨账户移动」根本不可能走到下去（不需要再比 `account_id`，
        // 比了反而是永远不会被执行的分支）。
        let destination = self
            .get_folder(account_id, destination_folder_id)
            .await?
            .ok_or_else(|| {
                xremail_core::AppError::NotFound(format!(
                    "目标文件夹 {destination_folder_id} 在这个账户里不存在"
                ))
            })?;

        if destination.id == folder_id {
            return Err(xremail_core::AppError::Invalid(
                "目标文件夹就是源文件夹，没必要移动".into(),
            ));
        }

        self.enqueue_transfer(
            account_id,
            folder_id,
            uids,
            OperationKind::Move,
            Some(destination.id),
        )
        .await
    }

    /// 删除一批邮件（**只入队**）。
    ///
    /// 目标文件夹这里不写：有垃圾箱就往里摄、没有就就地标删，
    /// 由同步层按服务器能力与本地文件夹名判断
    /// （见 `xremail_sync::push_transfers`）。
    pub async fn delete_intent(
        &self,
        account_id: &str,
        folder_id: &str,
        uids: &[i64],
    ) -> AppResult<Vec<MailOperation>> {
        self.enqueue_transfer(account_id, folder_id, uids, OperationKind::Delete, None)
            .await
    }

    /// 一批邮件的共性校验 + 入队。
    ///
    /// 校验与 [`Self::set_flag_intent`] 一模一样（epoch 必须已知、
    /// 邮件必须在本地且属于当前 epoch）——原因也一样：
    /// UID 只在（文件夹, epoch）内有意义，套错 epoch 就是动错了信。
    async fn enqueue_transfer(
        &self,
        account_id: &str,
        folder_id: &str,
        uids: &[i64],
        kind: OperationKind,
        destination_folder_id: Option<String>,
    ) -> AppResult<Vec<MailOperation>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let folder = self
            .get_folder(account_id, folder_id)
            .await?
            .ok_or_else(|| {
                xremail_core::AppError::NotFound(format!("文件夹 {folder_id} 不存在"))
            })?;

        let epoch = folder.uidvalidity.ok_or_else(|| {
            xremail_core::AppError::Invalid(
                "该文件夹还没同步过，不知道 epoch，无法安全地移动或删除邮件".into(),
            )
        })?;

        let known: Vec<i64> = message_dao::list_messages(&self.pool, account_id, folder_id)
            .await?
            .into_iter()
            .filter(|message| message.uidvalidity == epoch)
            .map(|message| message.uid)
            .collect();

        let mut enqueued = Vec::with_capacity(uids.len());
        for &uid in uids {
            if !known.contains(&uid) {
                return Err(xremail_core::AppError::NotFound(format!(
                    "本地没有 UID {uid} 这封邮件（可能还没同步下来，或 epoch 已变），请重新同步后再试"
                )));
            }

            let now = current_time_ms();
            let operation = MailOperation {
                id: new_operation_id(),
                account_id: account_id.to_string(),
                folder_id: folder_id.to_string(),
                uid,
                uidvalidity: epoch,
                kind,
                // 移动/删除不看 flag 意图，留空载荷（空集不是矛盾意图）。
                intent: FlagIntentPayload::default(),
                destination_folder_id: destination_folder_id.clone(),
                state: OperationState::Queued,
                // `seq` 由存储层分配，这里先占位。
                seq: 0,
                base_version: None,
                attempts: 0,
                error_kind: None,
                error_message: None,
                created_at: now,
                updated_at: now,
            };

            operation_dao::enqueue_operation(&self.pool, &operation).await?;

            // 入队是**幂等**的（R34）：这封邮件上已经有一条未完结的移动 / 删除
            // 时，上面那句什么都没插。所以这里**不能**拿自己构造的 id 去读回
            // （那种情况下它在库里根本不存在），而要按「这封邮件」查——
            // 查到的可能是刚插的那条，也可能是已有的那条。
            //
            // 对界面来说是好事：用户点了两次「移动」，看到的是**同一条**操作，
            // 而不是两条各发一次 `MOVE`、目标文件夹里出两份。
            let stored =
                operation_dao::find_pending_transfer(&self.pool, account_id, folder_id, uid, epoch)
                    .await?
                    .ok_or_else(|| {
                        xremail_core::AppError::Storage(format!(
                            "UID {uid} 的操作入队后既查不到新插入的那条、也查不到已有的在途操作"
                        ))
                    })?;
            enqueued.push(stored);
        }

        Ok(enqueued)
    }
}

/// 把未完结的标记意图叠到服务器快照上，得到**本地视图**。
///
/// 这是**显示层**的事（见 [ARCHITECTURE §6](../../../../../docs/ARCHITECTURE.md)）：
/// 快照与意图分开存，这里只是把它们合起来给 UI 看。
/// 服务器**还没**确认这些改动——所以不能因此说"已生效"。
///
/// 绑定**旧 epoch** 的意图会被跳过：旧 UID 在新 epoch 下指向另一封信。
fn overlay_pending_intents(
    messages: &mut [Message],
    pending: &[MailOperation],
    epoch: Option<i64>,
) -> AppResult<()> {
    let Some(epoch) = epoch else {
        // 还没同步过的文件夹没有 epoch，也就没有可叠加的意图。
        return Ok(());
    };

    let intents: Vec<FlagIntent> = pending
        .iter()
        .filter(|operation| {
            operation.uidvalidity == epoch && operation.kind == OperationKind::Flags
        })
        .map(|operation| {
            FlagIntent::new(
                operation.uid,
                operation.uidvalidity,
                operation.intent.add,
                operation.intent.remove,
                operation.seq as u64,
                operation.base_version,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    if intents.is_empty() {
        return Ok(());
    }

    for message in messages.iter_mut() {
        let own: Vec<FlagIntent> = intents
            .iter()
            .filter(|intent| intent.uid == message.uid)
            .copied()
            .collect();
        if own.is_empty() {
            continue;
        }
        message.flags = apply_intents(message.flags, &own);
    }

    Ok(())
}
