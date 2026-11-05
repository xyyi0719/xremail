//! 发信服务：写信 → 草稿 → 提交给服务器。
//!
//! ## 三种结局必须分开
//!
//! * **sent**（服务器明确回了 250）：这封信确实发出去了。
//! * **uncertain**（命令发出去了、没拿到最终应答）：**不知道**服务器收没收收。
//!   这种情况下自动重发可能让用户收到**两封**一样的信，所以它**不能**自动重试。
//! * **failed**（服务器明确拒收 / 连都没连上）：明确没发出去，可以改完再试。
//!
//! 把后两种揉成一句「发送失败」是错的：前者重试可能重复发信，后者重试却是安全的
//! （见 [ARCHITECTURE §5](../../../../../docs/ARCHITECTURE.md)）。
//!
//! ## 为什么要先落库再发送
//!
//! 邮件字节一旦冻结（`message_bytes`）就不再变：
//! 重试发的是**同一份字节**，所以 `Message-ID` 不变，
//! 对端能认出「这是同一封信」而不是新的两封。
//!
//! 顺带的好处是：进程在中途崩掉，库里还留着这封信的状态，
//! 下次启动能接着处理（`running` 会被收成 `uncertain`，不是 `retryable`——
//! 服务器可能已经收下了它）。
//!
//! ## 8BITMIME：这里一律用 quoted-printable
//!
//! 服务器支不支持 `8BITMIME` 要等 `EHLO` 之后才知道，
//! 而字节必须在**发送之前**就冻结（不然崩溃了就没法恢复）。
//! 两个信息凑不到一起，所以这里选**到处都能用的那一个**：
//! 正文按 quoted-printable 编码（RFC 2045 标准），任何客户端都认。
//! 代价是中文正文会膨胀到 3 倍左右——但正确性优先。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use xremail_api::SecretValue;
use xremail_core::address::validate_addresses;
use xremail_core::attachment::{AttachmentInput, check_attachment_limits_all};
use xremail_core::model::{Account, SendAttachment, SendQueueEntry, SendState, TlsMode};
use xremail_core::{AppError, AppResult};
use xremail_message::compose::{BuildOptions, OutgoingMessage, build_message};
use xremail_protocol_imap::TlsTrust as ImapTlsTrust;
use xremail_protocol_smtp::{
    Envelope, SmtpConfig, SmtpError, SubmitOutcome, SubmitRequest, TlsTrust, submit_message,
};
use xremail_store::dao::send_queue as send_queue_dao;

use super::account::AccountService;
use super::sent_copy::{SentCopyOutcome, SentCopyService};
use super::{current_time_ms, new_operation_id};

/// 前端传来的草稿内容。
///
/// 字段名是 camelCase（前端直接 `invoke` 这个结构）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftInput {
    /// 用哪个账户发信。
    pub account_id: String,
    /// 已有草稿的 id；新建时为 `None`。
    #[serde(default)]
    pub id: Option<String>,
    /// 收件人。
    #[serde(default)]
    pub to: Vec<String>,
    /// 抄送。
    #[serde(default)]
    pub cc: Vec<String>,
    /// 密送。**只进信封，不进邮件头**。
    #[serde(default)]
    pub bcc: Vec<String>,
    /// 主题。
    #[serde(default)]
    pub subject: String,
    /// 纯文本正文。
    #[serde(default)]
    pub body_text: String,
    /// HTML 正文（前端目前还没有富文本编辑器）。
    #[serde(default)]
    pub body_html: Option<String>,
    /// 附件。
    #[serde(default)]
    pub attachments: Vec<DraftAttachment>,
}

/// 一个附件（前端传**路径**，字节由后端读）。
///
/// ## 为什么不让前端传字节
///
/// 几十 MB 的文件读成 `number[]` 塞进 IPC 参数，参数体直接被撑爆
/// （而且还得 base64 来回一趟）。传路径、后端自己读文件，
/// 参数体与前端内存都小得多。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftAttachment {
    /// 文件名（显示用）。
    pub name: String,
    /// MIME 类型（前端可能给空串；后端会按扩展名兜底）。
    #[serde(default)]
    pub mime: String,
    /// 前端看到的字节数（**可能不准**——后端读完文件后会用真实大小覆盖）。
    #[serde(default)]
    pub size: u64,
    /// 本机绝对路径。
    ///
    /// **空串**表示「这是已存草稿里的附件，字节在库里」：
    /// 前端回填草稿时拿不到路径，后端按文件名复用库里那份字节。
    #[serde(default)]
    pub path: String,
}

/// 附件的样子（给前端看的）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftAttachmentDto {
    /// 文件名。
    pub name: String,
    /// MIME 类型。
    pub mime: String,
    /// 真实字节数。
    pub size: u64,
    /// 已存草稿的附件没有路径（字节在库里）→ 空串。
    pub path: String,
}

/// 草稿（给前端看的形状）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftDto {
    /// 草稿 id。
    pub id: String,
    /// 所属账户。
    pub account_id: String,
    /// 主题。
    pub subject: String,
    /// 收件人。
    pub to: Vec<String>,
    /// 当前状态（`draft` / `queued` / `succeeded` / …）。
    pub state: String,
    /// 最后修改时间（epoch 毫秒）。
    pub updated_at: i64,
}

impl DraftDto {
    fn from_domain(entry: &SendQueueEntry) -> Self {
        Self {
            id: entry.id.clone(),
            account_id: entry.account_id.clone(),
            subject: entry.subject.clone(),
            to: entry.to.clone(),
            state: entry.state.as_str().to_string(),
            updated_at: entry.updated_at,
        }
    }
}

/// 草稿全文（给「接着上次那封改」用）。
///
/// 与 [`DraftDto`] 的区别：这里带**正文与附件**，
/// 前端要能把它整个填回编辑区。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftDetail {
    /// 草稿 id。
    pub id: String,
    /// 所属账户。
    pub account_id: String,
    /// 主题。
    pub subject: String,
    /// 收件人。
    pub to: Vec<String>,
    /// 抄送。
    pub cc: Vec<String>,
    /// 密送。
    pub bcc: Vec<String>,
    /// 当前状态。
    pub state: String,
    /// 最后修改时间（epoch 毫秒）。
    pub updated_at: i64,
    /// 纯文本正文。
    pub body_text: String,
    /// HTML 正文。
    pub body_html: Option<String>,
    /// 附件（**不含字节**——前端只要名字与大小）。
    pub attachments: Vec<DraftAttachmentDto>,
}

impl DraftDetail {
    fn from_domain(entry: &SendQueueEntry) -> Self {
        Self {
            id: entry.id.clone(),
            account_id: entry.account_id.clone(),
            subject: entry.subject.clone(),
            to: entry.to.clone(),
            cc: entry.cc.clone(),
            bcc: entry.bcc.clone(),
            state: entry.state.as_str().to_string(),
            updated_at: entry.updated_at,
            body_text: entry.body_text.clone().unwrap_or_default(),
            body_html: entry.body_html.clone(),
            attachments: entry
                .attachments
                .iter()
                .map(|attachment| DraftAttachmentDto {
                    name: attachment.filename.clone(),
                    mime: attachment.content_type.clone(),
                    size: attachment.bytes.len() as u64,
                    // 字节在库里，没有路径可给。
                    path: String::new(),
                })
                .collect(),
        }
    }
}

/// 发送结果的状态。
///
/// 三个值的含义**互不相同**，界面上必须分开说（见 `types.ts` 的 `SendResult`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SendResultState {
    /// 服务器确认接受（250）。
    Sent,
    /// 不确定：命令发出去了，没拿到最终应答。**不能自动重发。**
    Uncertain,
    /// 明确失败：服务器拒收，或压根没连上。可以改完再试。
    Failed,
}

/// 发送结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    /// 邮件的 Message-ID（含尖括号）。
    pub message_id: String,
    /// 发信条目 id。
    ///
    /// 前端要拿它才能做「撤回重发」这类后续操作
    /// （`uncertain` 时不知道服务器收没收到，得靠这个 id 去查）。
    pub entry_id: String,
    /// 三种结局之一。
    pub state: SendResultState,
    /// 给用户看的一句话说明。
    pub detail: String,
}

/// 发信服务。
#[derive(Debug, Clone)]
pub struct ComposeService {
    pool: SqlitePool,
    accounts: AccountService,
    /// TLS 信任根。
    ///
    /// 生产用系统根证书（[`TlsTrust::SystemRoots`]）——
    /// 校验失败就是失败，没有「跳过校验」的开关（见 [SECURITY §2.2](../../../../../docs/SECURITY.md)）。
    ///
    /// 测试用假 SMTP 服务器的自签 CA 换进去（见 `tests/compose_service.rs`）。
    /// 换的是**信任锚**，不是关掉校验：证书照样必须通过验证。
    trust: TlsTrust,
    /// 「发完确保已发送副本」。
    ///
    /// 163 的服务器会自己存副本，QQ 不会——所以这一步是**自适应**的：
    /// 先查「已发送」里有没有这封信，没有才自己补一份（见 [`SentCopyService`]）。
    sent_copy: SentCopyService,
}

impl ComposeService {
    /// 用连接池与账户服务构造。
    pub fn new(pool: SqlitePool, accounts: AccountService) -> Self {
        Self {
            pool,
            accounts,
            trust: TlsTrust::SystemRoots,
            sent_copy: SentCopyService::new(ImapTlsTrust::SystemRoots),
        }
    }

    /// 指定 SMTP 与 IMAP 的 TLS 信任根（**只给测试用**）。
    ///
    /// 两个协议的 `TlsTrust` 是**各自 crate 里的不同类型**——
    /// 假 SMTP 服务器与假 IMAP 服务器各有各的自签 CA，所以得分开给。
    ///
    /// 换的是**信任锚**，不是关掉校验：证书照样必须通过验证。
    pub fn with_trusts(
        pool: SqlitePool,
        accounts: AccountService,
        smtp_trust: TlsTrust,
        imap_trust: ImapTlsTrust,
    ) -> Self {
        Self {
            pool,
            accounts,
            trust: smtp_trust,
            sent_copy: SentCopyService::new(imap_trust),
        }
    }

    /// 改「补副本前等多久」（**给测试用**：`0` = 不等）。
    pub fn with_settle(mut self, settle_ms: u64) -> Self {
        self.sent_copy = self.sent_copy.with_settle(settle_ms);
        self
    }

    /// 存草稿（新建或更新）。
    ///
    /// 前端每 800ms 防抖存一次，所以这里必须是**幂等**的：
    /// 同一个 id 反复存，只会把内容覆盖成最新的一份。
    ///
    /// ## 哪些状态不能改
    ///
    /// 已经排队、正在发、已发出、结果未确认的信**都不能**再当草稿改：
    /// 用户看到的和实际发出去的就不是同一封了。
    pub async fn save_draft(&self, input: &DraftInput) -> AppResult<DraftDto> {
        let account = self.require_account(&input.account_id).await?;
        let id = draft_id(input);
        let attachments = self.resolve_attachments(input, &id).await?;
        let entry = self.build_entry(input, &account, attachments);
        let id = entry.id.clone();

        let saved = send_queue_dao::save_draft(&self.pool, &entry).await?;
        if !saved {
            return Err(AppError::Invalid(self.explain_uneditable(&id).await?));
        }
        Ok(DraftDto::from_domain(&entry))
    }

    /// 发送一封信。
    ///
    /// 流程：落草稿 → 构建并冻结字节 → 抢占 → 提交 → 记结局 → 回读确认。
    ///
    /// ## 为什么回读
    ///
    /// 「提交成功」不等于「信发出去了」——只有把库里的状态读回来，
    /// 才知道它到底停在哪一步。
    pub async fn send(
        &self,
        input: &DraftInput,
        credential: &SecretValue,
    ) -> AppResult<SendResult> {
        let account = self.require_account(&input.account_id).await?;
        let id = draft_id(input);
        let attachments = self.resolve_attachments(input, &id).await?;
        let entry = self.build_entry(input, &account, attachments);
        let id = entry.id.clone();

        // 地址校验放在**最前面**——地址不合法的话，
        // 后面每一步（落草稿、构建字节、提交）都是白做。
        //
        // 前端也会校验一份，但前端能被绕过（IPC 可以直接调），
        // 所以后端必须自己守一道。
        check_recipient_addresses(&entry)?;

        // 至少要有一个收件人。
        //
        // 只抄送不收件在 RFC 上合法，但在邮件客户端里几乎总是手滑，
        // 与其发出去让用户后悔，不如在这里问一句。
        if entry.to.is_empty() && entry.cc.is_empty() && entry.bcc.is_empty() {
            return Err(AppError::Invalid("至少要填一个收件人".to_string()));
        }

        // 1) 落草稿。存不进去说明这封信已经不在草稿态了（已发出 / 未确认）。
        let saved = send_queue_dao::save_draft(&self.pool, &entry).await?;
        if !saved {
            return Err(AppError::Invalid(self.explain_uneditable(&id).await?));
        }

        // 2) 构建字节（含头部注入防护——在 `build_message` 里）。
        let now = current_time_ms();
        let outgoing = OutgoingMessage {
            from: account.email.clone(),
            to: entry.to.clone(),
            cc: entry.cc.clone(),
            bcc: entry.bcc.clone(),
            subject: entry.subject.clone(),
            body_text: entry.body_text.clone(),
            body_html: entry.body_html.clone(),
            attachments: entry.attachments.clone(),
            in_reply_to: entry.in_reply_to.clone(),
            references: entry.references.clone(),
        };
        let built = build_message(&outgoing, &self.build_options(&account, now))
            .map_err(|e| AppError::Invalid(format!("邮件内容有问题：{e}")))?;

        // 3) 冻结字节：从这一刻起，重试发的都是同一份。
        let queued = send_queue_dao::queue_for_sending(
            &self.pool,
            &id,
            &built.bytes,
            &built.message_id,
            now,
        )
        .await?;
        if !queued {
            return Err(AppError::Invalid(
                "这封信已经进入发送队列，不能重复提交".to_string(),
            ));
        }

        // 4) 抢占：两个发送器同时抢，只有一个能拿到。
        //
        // 认领时记下本进程的启动标识："下次启动对账"只回收**别人**的行，
        // 不会把本进程还在发的信翻成「结果未知」。
        let claimed = self.claim_or_explain(&id, now).await?;
        if !claimed {
            return Err(AppError::Invalid(
                "这封信正在被另一个发送器处理，请稍后再试".to_string(),
            ));
        }

        // 5) 提交 + 记结局 + 回读。
        let recipients = built.envelope_recipients.clone();
        self.submit_and_record(
            &account,
            credential,
            &id,
            &recipients,
            &built.bytes,
            &built.message_id,
        )
        .await
    }

    /// 取发送权；取不到时**分清是哪一种取不到**再决定说什么。
    ///
    /// `claim_for_sending` 返回 `false` 有两种完全不同的意思：
    ///
    /// 1. **别人正在发**——真正的并发，让用户等一会儿。
    /// 2. **重试次数用完了**（`send_queue::MAX_SEND_ATTEMPTS`）——这封信
    ///    再点也不会自动重试。若也对用户说「稍后再试」，他会一直点、
    ///    一直被拒，而信永远发不出去、也没人告诉他为什么（R31）。
    ///
    /// 所以取不到时立刻查一次「是不是次数用完」，是就给一句**能行动**的话：
    /// 信没发出去、改内容或确认账号设置后再发。这里同时把那条收成
    /// `failed / retry_exhausted`，界面才会从「排队中」变成「失败 + 原因」。
    async fn claim_or_explain(&self, id: &str, now_ms: i64) -> AppResult<bool> {
        let claimed = send_queue_dao::claim_for_sending(
            &self.pool,
            id,
            xremail_sync::boot_id(),
            now_ms,
            send_queue_dao::MAX_SEND_ATTEMPTS,
        )
        .await?;
        if claimed {
            return Ok(true);
        }

        let exhausted = send_queue_dao::fail_if_exhausted(
            &self.pool,
            id,
            send_queue_dao::MAX_SEND_ATTEMPTS,
            now_ms,
        )
        .await?;
        if exhausted {
            return Err(AppError::Invalid(format!(
                "这封信已经自动重试了 {} 次都没成功，暂时不再自动重试。\
                 信没有发出去——改一下内容再点「发送」会重新开始计数，\
                 也可以先检查一下账号的服务器设置和密码。",
                send_queue_dao::MAX_SEND_ATTEMPTS
            )));
        }

        Ok(false)
    }

    /// 用户确认「服务器上确实没有这封信」之后，重新提交**同一封**。
    ///
    /// 重发的是**原来那份字节**（`Message-ID` 不变），
    /// 不是重新构建的一封——否则用户会收到两封内容相同但 `Message-ID` 不同的信。
    pub async fn resend_uncertain(
        &self,
        account_id: &str,
        id: &str,
        credential: &SecretValue,
    ) -> AppResult<SendResult> {
        let account = self.require_account(account_id).await?;
        let entry = send_queue_dao::get_entry(&self.pool, id)
            .await?
            .filter(|entry| entry.account_id == account_id)
            .ok_or_else(|| AppError::NotFound(format!("发信记录 {id} 不存在")))?;

        if entry.state != SendState::Uncertain {
            return Err(AppError::Invalid(format!(
                "只有「结果未确认」的信才能重新提交，这一封的状态是 {}",
                entry.state.as_str()
            )));
        }

        let now = current_time_ms();
        let requeued = send_queue_dao::retry_uncertain(&self.pool, id, now).await?;
        if !requeued {
            return Err(AppError::Invalid("重新提交失败：状态已经变了".to_string()));
        }
        let claimed = self.claim_or_explain(id, now).await?;
        if !claimed {
            return Err(AppError::Invalid(
                "这封信正在被另一个发送器处理，请稍后再试".to_string(),
            ));
        }

        let bytes = entry.message_bytes.clone().ok_or_else(|| {
            AppError::Invalid("这封信没有留出发送用的字节，无法重新提交".to_string())
        })?;
        let recipients = entry.envelope_recipients();
        let message_id = entry.message_id.clone().unwrap_or_default();

        self.submit_and_record(&account, credential, id, &recipients, &bytes, &message_id)
            .await
    }

    /// 确保「已发送」里有这封信的一份。
    ///
    /// **幂等**：已经有一份就什么都不做（163 的服务器会自己存，
    /// 再存一份就变成两封）。
    ///
    /// 字节用的是**库里冻结的那一份**——和发出去的一模一样，
    /// 所以补出来的副本和服务器收下的是同一封信。
    pub async fn ensure_sent_copy(
        &self,
        account_id: &str,
        entry_id: &str,
        credential: &SecretValue,
    ) -> AppResult<SentCopyOutcome> {
        let account = self.require_account(account_id).await?;
        let entry = send_queue_dao::get_entry(&self.pool, entry_id)
            .await?
            .filter(|entry| entry.account_id == account_id)
            .ok_or_else(|| AppError::NotFound(format!("发信记录 {entry_id} 不存在")))?;

        let bytes = entry.message_bytes.clone().ok_or_else(|| {
            AppError::Invalid("这封信没有留出发送用的字节，无法存副本".to_string())
        })?;
        let message_id = entry.message_id.clone().unwrap_or_default();

        self.sent_copy
            .ensure_sent_copy(&account, credential, &message_id, &bytes)
            .await
    }

    /// 列出某账户的**草稿**（只列 `draft` 状态）。
    ///
    /// 排队中 / 结果未确认的那些**不算**草稿——
    /// 它们的内容已经冻结了，不能当草稿改。
    pub async fn list_drafts(&self, account_id: &str) -> AppResult<Vec<DraftDto>> {
        let entries = send_queue_dao::list_entries(&self.pool, account_id).await?;
        Ok(entries
            .iter()
            .filter(|entry| entry.state == SendState::Draft)
            .map(DraftDto::from_domain)
            .collect())
    }

    /// 取一份草稿的全文（给「接着上次那封改」用）。
    ///
    /// 不是草稿（比如已经发出去了）就报错——
    /// 让用户以为能改、实际改了没用，比直接报错更糟。
    pub async fn get_draft(&self, id: &str) -> AppResult<DraftDetail> {
        let entry = send_queue_dao::get_entry(&self.pool, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("草稿 {id} 不存在")))?;
        if entry.state != SendState::Draft {
            return Err(AppError::Invalid(format!(
                "这条发信记录已经不是草稿了（状态 {}），不能当草稿改",
                entry.state.as_str()
            )));
        }
        Ok(DraftDetail::from_domain(&entry))
    }

    /// 把「结果未确认」的信撤回来继续改。
    ///
    /// **这是用户的判断，不是自动重试**——前端要弹二次确认，
    /// 因为万一服务器其实已经收下了，改完再发就变成两封。
    pub async fn withdraw_draft(&self, id: &str) -> AppResult<bool> {
        let now = current_time_ms();
        send_queue_dao::withdraw_to_draft(&self.pool, id, now).await
    }

    /// 列出某账户还**没发出去**的信（草稿 / 已排队 / 结果未确认）。
    pub async fn list_pending(&self, account_id: &str) -> AppResult<Vec<DraftDto>> {
        let entries = send_queue_dao::list_entries(&self.pool, account_id).await?;
        Ok(entries
            .iter()
            .filter(|entry| entry.state.is_pending())
            .map(DraftDto::from_domain)
            .collect())
    }

    // ---- 内部实现 ----------------------------------------------------

    /// 取账户，不存在就报错。
    async fn require_account(&self, account_id: &str) -> AppResult<Account> {
        self.accounts
            .get(account_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("账户 {account_id} 不存在")))
    }

    /// 把前端传来的草稿组装成领域模型。
    ///
    /// **不落库**——落库由调用方决定（保存草稿 / 发送）。
    fn build_entry(
        &self,
        input: &DraftInput,
        account: &Account,
        attachments: Vec<SendAttachment>,
    ) -> SendQueueEntry {
        let now = current_time_ms();
        let id = draft_id(input);

        SendQueueEntry {
            id: id.clone(),
            account_id: account.id.clone(),
            from: account.email.clone(),
            to: clean_addresses(&input.to),
            cc: clean_addresses(&input.cc),
            bcc: clean_addresses(&input.bcc),
            subject: input.subject.clone(),
            body_text: Some(input.body_text.clone()).filter(|text| !text.is_empty()),
            // 出站 HTML **在这里**消毒一次。
            //
            // 前端也有一份消毒（编辑器里的），但前端能被绕过（IPC 可以直接调），
            // 后端必须自己守一道——不然一个绕过前端的调用就能把
            // `<script>` 之类的内容发出去。
            body_html: input
                .body_html
                .as_deref()
                .map(xremail_core::html::sanitize_html)
                .filter(|html| !html.is_empty()),
            attachments,
            in_reply_to: None,
            references: None,
            state: SendState::Draft,
            idempotency_key: format!("draft-{id}"),
            message_bytes: None,
            message_id: None,
            attempts: 0,
            error_kind: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// 把前端传来的附件（**路径**）变成能存库的字节。
    ///
    /// ## 两种来源
    ///
    /// - **有路径**：新选的文件 → 后端自己读（顺便查大小上限）。
    /// - **没路径**：已存草稿里的附件 → 字节已经在库里，按文件名复用。
    ///
    /// 复用按**文件名**匹配：附件没有别的稳定标识，
    /// 而前端回填草稿时也不会给附件改名。
    async fn resolve_attachments(
        &self,
        input: &DraftInput,
        id: &str,
    ) -> AppResult<Vec<SendAttachment>> {
        let stored = send_queue_dao::list_attachments(&self.pool, id).await?;
        let mut resolved = Vec::with_capacity(input.attachments.len());

        for attachment in &input.attachments {
            let path = attachment.path.trim();
            if path.is_empty() {
                if let Some(found) = stored
                    .iter()
                    .find(|saved| saved.filename == attachment.name)
                {
                    resolved.push(found.clone());
                }
                continue;
            }
            let bytes = read_attachment_file(path, &attachment.name).await?;
            resolved.push(SendAttachment {
                filename: attachment.name.clone(),
                content_type: if attachment.mime.trim().is_empty() {
                    guess_content_type(&attachment.name)
                } else {
                    attachment.mime.clone()
                },
                bytes,
            });
        }

        // 限额检查：个数 / 单个大小 / 总大小 / 扩展名黑名单。
        //
        // **在入库之前**做：超限的附件不该被冻进草稿字节里。
        // 上面的 `read_attachment_file` 只挡了「单个太大」（读之前先看大小），
        // 剩下的规则归 core 管（前后端引用同一处限额）。
        let inputs: Vec<AttachmentInput> = resolved
            .iter()
            .map(|attachment| {
                AttachmentInput::new(attachment.filename.clone(), attachment.bytes.len() as u64)
                    .with_mime(attachment.content_type.clone())
            })
            .collect();
        let problems = check_attachment_limits_all(&inputs);
        if !problems.is_empty() {
            return Err(AppError::Invalid(
                problems
                    .iter()
                    .map(|problem| problem.to_string())
                    .collect::<Vec<_>>()
                    .join("；"),
            ));
        }

        Ok(resolved)
    }

    /// MIME 构建选项。
    fn build_options(&self, account: &Account, now_ms: i64) -> BuildOptions {
        BuildOptions {
            // 见文件头的说明：一律用 quoted-printable。
            eight_bit: false,
            now_epoch_secs: now_ms / 1000,
            message_id_domain: message_id_domain(&account.email),
            charset: "utf-8".to_string(),
        }
    }

    /// 把提交结果记进库，并返回给前端的结局。
    ///
    /// ## 为什么 4xx 和 5xx 要分开
    ///
    /// 4xx 的意思是「服务器**现在**没收下这封信」→ 重发安全 → `retryable`；
    /// 5xx 是「服务器不收这封信」→ 重发多少次都一样 → `failed`。
    /// 两者对用户来说是完全不同的处境，不能揉成一句。
    async fn record_outcome(
        &self,
        id: &str,
        outcome: &SubmitOutcome,
    ) -> AppResult<SendResultState> {
        let now = current_time_ms();

        match outcome {
            SubmitOutcome::Accepted { .. } => {
                send_queue_dao::mark_succeeded(&self.pool, id, now).await?;
                Ok(SendResultState::Sent)
            }
            SubmitOutcome::Rejected {
                code,
                message,
                stage,
            } => {
                let stage_text = stage_name(*stage);
                let text = format!("服务器在{stage_text}拒绝了这封信：{code} {message}");
                if (400..500).contains(code) {
                    // 4xx：临时故障，服务器**没有**收下这封信，重发是安全的。
                    send_queue_dao::mark_retryable(&self.pool, id, "rejected_4xx", &text, now)
                        .await?;
                } else {
                    // 5xx：永久拒收，重发也没用。
                    send_queue_dao::mark_failed(&self.pool, id, "rejected_5xx", &text, now).await?;
                }
                Ok(SendResultState::Failed)
            }
            SubmitOutcome::NotSent { reason, stage } => {
                // `DATA` 之前就断了 → 服务器手里没有这封信的任何一个字节
                // → 重发是安全的 → 记成可重试。
                let stage_text = stage_name(*stage);
                let text = format!("连接在{stage_text}断开了，这封信没有发出去：{reason}");
                send_queue_dao::mark_retryable(&self.pool, id, "not_sent", &text, now).await?;
                Ok(SendResultState::Failed)
            }
            SubmitOutcome::Uncertain { reason, stage } => {
                let stage_text = stage_name(*stage);
                let text =
                    format!("命令已经发出去了，但在{stage_text}没收到服务器的最终应答：{reason}");
                // 不确定 ≠ 可重试：服务器可能已经收下了这封信。
                send_queue_dao::mark_uncertain(&self.pool, id, "no_reply", &text, now).await?;
                Ok(SendResultState::Uncertain)
            }
        }
    }

    /// 提交 → 记结局 → 回读，返回给前端的发送结果。
    ///
    /// ## 为什么前置失败也算一个「结果」
    ///
    /// 连不上服务器、TLS 握手失败——这些都在**任何命令发出去之前**发生，
    /// 所以这封信**肯定没发出去**，重发是安全的。
    ///
    /// 但它**不能**当成「什么都没发生」：
    /// 如果不记账，这一行就会永远停在 `running`，
    /// 下次启动的回收逻辑会把它当成「发到一半」，从而变成 `uncertain`
    /// ——那就把一个「肯定没发」的信说成「可能发了」，用户不敢重发。
    async fn submit_and_record(
        &self,
        account: &Account,
        credential: &SecretValue,
        entry_id: &str,
        recipients: &[String],
        bytes: &[u8],
        fallback_message_id: &str,
    ) -> AppResult<SendResult> {
        let now = current_time_ms();

        let (outcome, note) = match self.submit(account, credential, recipients, bytes).await {
            Ok(pair) => pair,
            Err(error) => {
                let text = describe_smtp_error(&error);
                let (kind, permanent) = classify_smtp_error(&error);
                if permanent {
                    let _ =
                        send_queue_dao::mark_failed(&self.pool, entry_id, kind, &text, now).await;
                } else {
                    let _ = send_queue_dao::mark_retryable(&self.pool, entry_id, kind, &text, now)
                        .await;
                }
                let stored = send_queue_dao::get_entry(&self.pool, entry_id).await?;
                return Ok(SendResult {
                    message_id: stored
                        .and_then(|entry| entry.message_id)
                        .unwrap_or_else(|| fallback_message_id.to_string()),
                    entry_id: entry_id.to_string(),
                    state: SendResultState::Failed,
                    detail: text,
                });
            }
        };

        let state = self.record_outcome(entry_id, &outcome).await?;

        let stored = send_queue_dao::get_entry(&self.pool, entry_id)
            .await?
            .ok_or_else(|| AppError::Storage("发信记录已落库，但读不回来".to_string()))?;
        let message_id = stored
            .message_id
            .clone()
            .unwrap_or_else(|| fallback_message_id.to_string());

        // 发出去了 → 确保「已发送」里有这份。
        //
        // **这一步失败不能把发送算成失败**：信已经发出去了，
        // 存不存副本只影响「已发送」文件夹里看不看得到它。
        // 所以失败只变成一句给用户看的补充说明，不改结论。
        let copy_note = if state == SendResultState::Sent {
            match self
                .ensure_sent_copy(&account.id, entry_id, credential)
                .await
            {
                Ok(SentCopyOutcome::AlreadyPresent) => None,
                Ok(SentCopyOutcome::Appended { .. }) => None,
                Ok(SentCopyOutcome::NoSentFolder) => {
                    Some("这个账户没有「已发送」文件夹，没法存副本".to_string())
                }
                Err(error) => Some(format!("信已发出，但没能存一份到「已发送」：{error}")),
            }
        } else {
            None
        };

        let mut detail =
            describe_outcome(&outcome, stored.error_message.as_deref(), note.as_deref());
        if let Some(extra) = copy_note {
            detail = format!("{detail}（{extra}）");
        }

        Ok(SendResult {
            message_id,
            entry_id: entry_id.to_string(),
            state,
            detail,
        })
    }

    /// 真正提交给服务器。
    ///
    /// ## TLS 1.3 → 1.2 回退
    ///
    /// 有些国内邮箱的 SMTP 服务器（实测：`smtp.wo.cn`）**只认 TLS 1.2**：
    /// 见到 TLS 1.3 的 ClientHello 直接回 fatal alert，握手就断了。
    ///
    /// 这时换版本重试一次——**证书校验一点都没有放松**
    /// （换版本与「不校验证书」是两回事，见 [SECURITY §2.2](../../../../../docs/SECURITY.md)）。
    /// 而且回退这件事会写进给用户看的说明里，不偷偷降级。
    ///
    /// 回退是安全的：握手失败发生在任何命令发出**之前**，那封信肯定没发出去。
    async fn submit(
        &self,
        account: &Account,
        credential: &SecretValue,
        recipients: &[String],
        bytes: &[u8],
    ) -> Result<(SubmitOutcome, Option<String>), SmtpError> {
        let config = smtp_config(account)?;
        let password = credential
            .as_str()
            .ok_or_else(|| SmtpError::Invalid("授权码不是合法的 UTF-8 文本".to_string()))?;
        let request = SubmitRequest {
            envelope: Envelope {
                sender: account.email.clone(),
                recipients: recipients.to_vec(),
            },
            message: bytes.to_vec(),
            // 字节是按 7bit 安全方式构建的（quoted-printable），
            // 所以**不能**宣告 8BITMIME——宣告了却发 7bit 内容虽然合法，
            // 但没必要，让服务器按它理解的走更稳。
            eight_bit: false,
        };

        let first =
            submit_message(&config, &self.trust, &account.username, password, &request).await;

        match first {
            Err(SmtpError::TlsVersionRejected(reason)) => {
                let retry = submit_message(
                    &config.with_tls12_only(),
                    &self.trust,
                    &account.username,
                    password,
                    &request,
                )
                .await?;
                let note = format!("服务器拒绝了 TLS 1.3（{reason}），已回退到 TLS 1.2 重新提交");
                Ok((retry, Some(note)))
            }
            other => Ok((other?, None)),
        }
    }

    /// 说明这封信为什么不能当草稿改。
    async fn explain_uneditable(&self, id: &str) -> AppResult<String> {
        let current = send_queue_dao::get_entry(&self.pool, id).await?;
        Ok(match current.map(|entry| entry.state) {
            Some(SendState::Succeeded) => "这封信已经发出去了，不能再改".to_string(),
            Some(SendState::Uncertain) => {
                "这封信的发送结果还没确认，不能再改（确认服务器上没有它之后，可以重新提交）"
                    .to_string()
            }
            Some(SendState::Queued) | Some(SendState::Running) => {
                "这封信正在发送，不能再改".to_string()
            }
            _ => "这封信已经不在草稿状态，不能再改".to_string(),
        })
    }
}

/// 去掉空地址与首尾空白。
fn clean_addresses(addresses: &[String]) -> Vec<String> {
    addresses
        .iter()
        .map(|address| address.trim().to_string())
        .filter(|address| !address.is_empty())
        .collect()
}

/// `Message-ID` 的域部分：取发件地址的域名。
///
/// 域名不对不会导致退信，但用发件方自己的域名更符合惯例，
/// 收件方的客户端也能把它和这封信对应起来。
fn message_id_domain(email: &str) -> String {
    email
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim())
        .filter(|domain| !domain.is_empty())
        .unwrap_or("xremail.local")
        .to_string()
}

/// 按账户配置拼出 SMTP 连接参数。
fn smtp_config(account: &Account) -> Result<SmtpConfig, SmtpError> {
    let server = &account.server;
    match server.smtp_tls {
        TlsMode::Implicit => Ok(SmtpConfig::direct(&server.smtp_host, server.smtp_port)),
        TlsMode::Starttls => Ok(SmtpConfig::starttls(&server.smtp_host, server.smtp_port)),
        TlsMode::None => Err(SmtpError::Invalid(format!(
            "账户 {} 的 SMTP 配置是明文（不加密），出于安全考虑不支持发信",
            account.email
        ))),
    }
}

/// 把 SMTP 层的错误翻译成给用户看的一句话。
///
/// 重点：每一条都明说**这封信有没有发出去**。
/// 前端的 `failed` 状态会直接展示这句话，用户靠它决定要不要重发。
fn describe_smtp_error(error: &SmtpError) -> String {
    match error {
        SmtpError::Connect(detail) => {
            format!("连不上发信服务器（{detail}）。这封信没有发出去，可以稍后重试。")
        }
        SmtpError::Tls(detail) => {
            format!("发信服务器的 TLS 校验没通过（{detail}）。这封信没有发出去。")
        }
        SmtpError::TlsVersionRejected(detail) => {
            format!("发信服务器拒绝了 TLS 握手（{detail}）。这封信没有发出去。")
        }
        SmtpError::Io(detail) => format!("发信时网络中断（{detail}）。这封信没有发出去。"),
        SmtpError::Timeout(detail) => format!("发信服务器超时（{detail}）。这封信没有发出去。"),
        SmtpError::Smtp(detail) => {
            format!("发信服务器返回了无法理解的响应（{detail}）。这封信没有发出去。")
        }
        SmtpError::Auth(detail) => {
            format!("发信服务器拒绝了登录（{detail}）。请检查邮箱密码 / 授权码，这封信没有发出去。")
        }
        SmtpError::Invalid(detail) => format!("信件内容有问题（{detail}），没有发出去。"),
        SmtpError::Unsupported(detail) => {
            format!("发信服务器不支持所需的加密方式（{detail}），这封信没有发出去。")
        }
        // `SmtpError` 是 `#[non_exhaustive]`：以后可能加新变体。
        // 兜底归为「永久失败」——宁可让用户手动重试，也不要自动重试一封
        // 可能已经发出去的信。
        other => format!("发信失败（{other}）。这封信没有发出去。"),
    }
}

/// 把错误分成两类：能不能靠「等一会儿再发」解决。
///
/// 返回 `(error_kind, permanent)`：
/// - `permanent = false` → 临时问题（网络、握手版本）→ 记 `retryable`，可以自动重试。
/// - `permanent = true`  → 要人改凭据或改配置 → 记 `failed`，重试多少次都一样。
///
/// 分类以 crate 自己的 [`SmtpError::is_retryable`] 为准，不另起一套标准。
fn classify_smtp_error(error: &SmtpError) -> (&'static str, bool) {
    match error {
        SmtpError::Connect(_) => ("connect_failed", false),
        SmtpError::Io(_) => ("io_error", false),
        SmtpError::Timeout(_) => ("timeout", false),
        SmtpError::TlsVersionRejected(_) => ("tls_version_rejected", false),
        SmtpError::Tls(_) => ("tls_failed", true),
        SmtpError::Auth(_) => ("auth_failed", true),
        SmtpError::Invalid(_) => ("invalid_input", true),
        SmtpError::Unsupported(_) => ("unsupported", true),
        SmtpError::Smtp(_) => ("smtp_protocol", true),
        // `#[non_exhaustive]` 兜底：归为永久失败，不自动重试。
        _ => ("unknown", true),
    }
}

/// 提交阶段的中文名（给用户看的）。
fn stage_name(stage: xremail_protocol_smtp::SubmitStage) -> &'static str {
    use xremail_protocol_smtp::SubmitStage;
    match stage {
        SubmitStage::Connect => "连接时",
        SubmitStage::Auth => "认证时",
        SubmitStage::MailFrom => "写发件人时",
        SubmitStage::RcptTo => "写收件人时",
        SubmitStage::Data => "发送正文时",
        SubmitStage::Quit => "收尾时",
    }
}

/// 把提交结果翻译成给用户看的一句话。
fn describe_outcome(
    outcome: &SubmitOutcome,
    stored_error: Option<&str>,
    note: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(note) = note {
        parts.push(note.to_string());
    }
    let body = match outcome {
        SubmitOutcome::Accepted { code, message } => {
            format!("服务器已确认接受（{code} {message}）")
        }
        SubmitOutcome::Rejected {
            code,
            message,
            stage,
        } => {
            let stage_text = stage_name(*stage);
            format!("服务器在{stage_text}拒绝了这封信：{code} {message}")
        }
        SubmitOutcome::NotSent { reason, stage } => {
            let stage_text = stage_name(*stage);
            format!("连接在{stage_text}断开了，这封信没有发出去：{reason}")
        }
        SubmitOutcome::Uncertain { reason, stage } => {
            let stage_text = stage_name(*stage);
            format!("命令已经发出去了，但在{stage_text}没收到最终应答：{reason}")
        }
    };
    parts.push(body);
    if let Some(error) = stored_error
        && !error.is_empty()
        && !parts.iter().any(|part| part.contains(error))
    {
        parts.push(error.to_string());
    }
    parts.join("；")
}

/// 草稿 id：前端给了就用，没给就新生成一个。
fn draft_id(input: &DraftInput) -> String {
    input
        .id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(new_operation_id)
}

/// 单个附件的大小上限（10 MiB）。
///
/// 转发自 [`xremail_core::attachment::MAX_ATTACHMENT_BYTES`]：
/// 限额本身归 core 管（前后端、前后端与服务层都引用同一处）。
const MAX_ATTACHMENT_BYTES: u64 = xremail_core::attachment::MAX_ATTACHMENT_BYTES;

/// 读一个附件文件。
///
/// **先查大小再读**：不然一个几百 MB 的文件会把内存吃光，
/// 而且用户等的是「已发送」，不是「进程被杀了」。
async fn read_attachment_file(path: &str, name: &str) -> AppResult<Vec<u8>> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| AppError::Invalid(format!("读不到附件「{name}」（{path}）：{e}")))?;
    if !metadata.is_file() {
        return Err(AppError::Invalid(format!("附件「{name}」不是普通文件")));
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(AppError::Invalid(format!(
            "附件「{name}」有 {:.1} MiB，超过上限 {:.0} MiB",
            metadata.len() as f64 / 1024.0 / 1024.0,
            MAX_ATTACHMENT_BYTES as f64 / 1024.0 / 1024.0
        )));
    }
    tokio::fs::read(path)
        .await
        .map_err(|e| AppError::Invalid(format!("读不了附件「{name}」（{path}）：{e}")))
}

/// 校验收件人地址（To / Cc / Bcc）。
///
/// **一次报出所有问题**：用户改一个、提交一次、再改一个，体验极差。
fn check_recipient_addresses(entry: &SendQueueEntry) -> AppResult<()> {
    let mut all: Vec<&String> = Vec::new();
    all.extend(entry.to.iter());
    all.extend(entry.cc.iter());
    all.extend(entry.bcc.iter());

    match validate_addresses(&all) {
        Ok(_) => Ok(()),
        Err(problems) => Err(AppError::Invalid(
            problems
                .iter()
                .map(|problem| problem.to_string())
                .collect::<Vec<_>>()
                .join("；"),
        )),
    }
}

/// 按扩展名猜 MIME 类型。
///
/// 文件选择器不一定报得出类型（后端拿到的可能只是路径），
/// 而 MIME 类型决定对方客户端怎么显示这个附件——
/// 猜错成 `application/octet-stream` 的话，图片会变成一个附件块。
fn guess_content_type(name: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_stem, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let guessed = match extension.as_str() {
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    };
    guessed.to_string()
}
