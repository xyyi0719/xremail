//! COPY / MOVE / EXPUNGE：把邮件移走、删掉。
//!
//! ## 为什么单独一层
//!
//! 这两条命令的**成败判定**和 `STORE` 一样容易被骗：async-imap 的
//! `uid_copy` / `uid_mv` 在服务器回带 tag 的 `NO` / `BAD` 时**不会**报错
//! （同一个响应解析问题，见 [`crate::command`] 的模块注释）。
//! 所以这里自己发命令、自己读 tagged 响应、自己判定。
//!
//! ## `MOVE` 不是每家都有
//!
//! 三家实测（[PROTOCOLS.md](../../../docs/PROTOCOLS.md)）：
//! **只有 QQ 有 `MOVE`**；163 与沃（Coremail 体系）都**没有**。
//! 所以调用方必须先看 [`crate::ExtensionSupport::move`]：
//!
//! * 有 `MOVE` → 一步 `UID MOVE`
//! * 没有 → 保守路径：`UID COPY` → `UID STORE +FLAGS (\Deleted)`
//!   → **有 `UIDPLUS` 才** `UID EXPUNGE`
//!
//! ## 删除为什么不能一律 `EXPUNGE`
//!
//! 裸 `EXPUNGE` 会把这个文件夹里**所有**带 `\Deleted` 的邮件都删掉，
//! 包括别的客户端（手机、网页版）标了 `\Deleted` 但还没提交的。
//! 那也是用户的数据，不能替别人做决定。
//!
//! 只有支持 `UIDPLUS`（RFC 4315）时才有 `UID EXPUNGE <set>`——它只删指定的那些。
//! 没有 `UIDPLUS` 时本模块**只标 `\Deleted` 不 expunge**，把「真正消失」的时机
//! 交还给服务器与用户自己的客户端。
//!
//! 注意别把这条读成「163 没有 `UIDPLUS`」：本仓实测 163 / QQ / 沃三家**都有**
//! `UIDPLUS`（见 `docs/PROTOCOLS.md` 的认证后能力集），163 缺的是 `MOVE`。
//! 判断一律走能力集，不要凭服务商名字猜。

use std::time::Duration;

use imap_proto::rfc4315::UidSetMember;
use imap_proto::{Response, ResponseCode, Status};

use crate::command::DEFAULT_COMMAND_TIMEOUT;
use crate::connection::Session;
use crate::error::ProtocolError;

/// 展开 `COPYUID` / `APPENDUID` 里 UID 区间的**上限**。
///
/// 服务器可能回 `1:4294967295` 这种区间。照着展开会瞬间吃掉几个 GB
/// （R09 有前科：`1:*` 被展开成 1.79 亿个元素，撑爆过假服务器 8 GiB）。
/// 超过这个上限就截断——真实用途里一次移动几千封已经是极端值了。
const UID_EXPANSION_LIMIT: usize = 10_000;

/// 一次 COPY / MOVE 的结果。
///
/// 字段大多来自 `COPYUID` 响应码（RFC 4315）——**服务器不给就是 `None`**，
/// 这很正常：163 与沃都不保证给。拿不到时调用方只能靠重新拉取确认。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferOutcome {
    /// 源文件夹的 UIDVALIDITY。
    pub source_uid_validity: Option<u32>,
    /// 目标文件夹的 UIDVALIDITY。
    pub destination_uid_validity: Option<u32>,
    /// 源文件夹里被复制/移动的那些 UID。
    pub source_uids: Vec<u32>,
    /// 目标文件夹里对应的新 UID。
    ///
    /// 与 [`Self::source_uids`] **按下标一一对应**（RFC 4315 §3）。
    /// 长度不等时说明服务器回的区间被截断了，别硬套。
    pub destination_uids: Vec<u32>,
    /// 服务器的说明文字。
    pub message: Option<String>,
}

impl TransferOutcome {
    /// 服务器有没有告诉我们「新 UID 是什么」。
    pub fn has_uid_mapping(&self) -> bool {
        !self.destination_uids.is_empty() && self.destination_uids.len() == self.source_uids.len()
    }

    /// 按源 UID 查目标 UID（没有对应关系时返回 `None`）。
    pub fn destination_uid_of(&self, source_uid: u32) -> Option<u32> {
        if !self.has_uid_mapping() {
            return None;
        }
        self.source_uids
            .iter()
            .position(|uid| *uid == source_uid)
            .map(|index| self.destination_uids[index])
    }
}

/// 把 `UID COPY` 搬到 `mailbox` 里的命令（不含 tag，也不含结尾 CRLF）。
///
/// `mailbox` 传**线路上的原名**（modified UTF-7），不是解码后的名字——
/// 理由同 [`crate::append::build_append_command`]。
pub fn build_copy_command(uids: &[u32], mailbox: &str) -> String {
    format!("UID COPY {} {}", uid_list(uids), quote_mailbox(mailbox))
}

/// 把 `UID MOVE` 搬到 `mailbox` 里的命令（RFC 6851）。
///
/// 服务器没有 `MOVE` 能力时这条命令会得到 `BAD`——调用方要先用
/// [`crate::ExtensionSupport::move`] 判断，别指望它自己降级。
pub fn build_move_command(uids: &[u32], mailbox: &str) -> String {
    format!("UID MOVE {} {}", uid_list(uids), quote_mailbox(mailbox))
}

/// 给这些 UID 打上 `\Deleted`（删除的第一步）。
///
/// 用 `.SILENT` 是为了省掉每条 untagged `FETCH` 回复；
/// 需要确认标记是否真的生效时，调用方应当自己再 `UID FETCH` 一次
/// （ARCHITECTURE §6 的「回读确认」）。
pub fn build_store_deleted_command(uids: &[u32]) -> String {
    format!("UID STORE {} +FLAGS.SILENT (\\Deleted)", uid_list(uids))
}

/// `UID EXPUNGE`（RFC 4315）：只删**指定的**那些 `\Deleted`。
///
/// 没有 `UIDPLUS` 的服务器会回 `BAD`，别调。
pub fn build_uid_expunge_command(uids: &[u32]) -> String {
    format!("UID EXPUNGE {}", uid_list(uids))
}

/// 把一批 UID 复制到另一个文件夹。
pub async fn copy_messages(
    session: &mut Session,
    uids: &[u32],
    mailbox: &str,
) -> Result<TransferOutcome, ProtocolError> {
    copy_messages_with_timeout(session, uids, mailbox, DEFAULT_COMMAND_TIMEOUT).await
}

/// 把一批 UID 复制到另一个文件夹，超时由调用方给。
pub async fn copy_messages_with_timeout(
    session: &mut Session,
    uids: &[u32],
    mailbox: &str,
    timeout: Duration,
) -> Result<TransferOutcome, ProtocolError> {
    ensure_usable(uids)?;
    let command = build_copy_command(uids, mailbox);
    run_transfer_with_timeout(session, &command, uids, timeout).await
}

/// 把一批 UID 移动到另一个文件夹（需要服务器支持 `MOVE`）。
pub async fn move_messages(
    session: &mut Session,
    uids: &[u32],
    mailbox: &str,
) -> Result<TransferOutcome, ProtocolError> {
    move_messages_with_timeout(session, uids, mailbox, DEFAULT_COMMAND_TIMEOUT).await
}

/// 把一批 UID 移动到另一个文件夹，超时由调用方给。
pub async fn move_messages_with_timeout(
    session: &mut Session,
    uids: &[u32],
    mailbox: &str,
    timeout: Duration,
) -> Result<TransferOutcome, ProtocolError> {
    ensure_usable(uids)?;
    let command = build_move_command(uids, mailbox);
    run_transfer_with_timeout(session, &command, uids, timeout).await
}

/// 给一批 UID 打 `\Deleted`。
pub async fn mark_deleted(session: &mut Session, uids: &[u32]) -> Result<(), ProtocolError> {
    ensure_usable(uids)?;
    let command = build_store_deleted_command(uids);
    run_simple(session, &command, "UID STORE \\Deleted").await
}

/// 给一批 UID 打 `\Deleted`，超时由调用方给。
pub async fn mark_deleted_with_timeout(
    session: &mut Session,
    uids: &[u32],
    timeout: Duration,
) -> Result<(), ProtocolError> {
    ensure_usable(uids)?;
    let command = build_store_deleted_command(uids);
    run_simple_with_timeout(session, &command, "UID STORE \\Deleted", timeout).await
}

/// `UID EXPUNGE` 一批 UID（需要服务器支持 `UIDPLUS`）。
pub async fn uid_expunge(session: &mut Session, uids: &[u32]) -> Result<(), ProtocolError> {
    uid_expunge_with_timeout(session, uids, DEFAULT_COMMAND_TIMEOUT).await
}

/// `UID EXPUNGE` 一批 UID，超时由调用方给。
pub async fn uid_expunge_with_timeout(
    session: &mut Session,
    uids: &[u32],
    timeout: Duration,
) -> Result<(), ProtocolError> {
    ensure_usable(uids)?;
    let command = build_uid_expunge_command(uids);
    run_simple_with_timeout(session, &command, "UID EXPUNGE", timeout).await
}

/// 入参检查：空集合与超长集合都**不能**发出去。
///
/// 空的 UID 集合在线上是语法错误（服务器回 `BAD`），
/// 而且它一定是调用方的 bug——与其发出去换一句看不懂的 `BAD`，不如在这里说清楚。
fn ensure_usable(uids: &[u32]) -> Result<(), ProtocolError> {
    if uids.is_empty() {
        return Err(ProtocolError::Invalid(
            "要移动/删除的 UID 集合是空的".to_string(),
        ));
    }
    if uids.len() > UID_EXPANSION_LIMIT {
        return Err(ProtocolError::Invalid(format!(
            "一次最多处理 {UID_EXPANSION_LIMIT} 封，收到 {} 封",
            uids.len()
        )));
    }
    Ok(())
}

/// 拼 UID 集合：排序去重，逗号分隔。
///
/// 排序去重不是洁癖：`1,1,2` 这种重复在某些服务器上会让 `STORE`
/// 重复执行同一封（浪费往返），而乱序会让 `COPYUID` 的
/// 「源集合 ↔ 目标集合按下标对应」变得难以核对。
fn uid_list(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// 给文件夹名加引号（与 [`crate::append`] 同一套规则）。
fn quote_mailbox(mailbox: &str) -> String {
    let mut quoted = String::with_capacity(mailbox.len() + 2);
    quoted.push('"');
    for ch in mailbox.chars() {
        if ch == '"' || ch == '\\' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

/// 跑一条「只看成败」的命令（超时用 [`DEFAULT_COMMAND_TIMEOUT`]）。
async fn run_simple(
    session: &mut Session,
    command: &str,
    label: &str,
) -> Result<(), ProtocolError> {
    match run_until_tagged(session, command, DEFAULT_COMMAND_TIMEOUT).await? {
        TaggedOutcome::Ok(_) => Ok(()),
        TaggedOutcome::Failed(detail) => {
            Err(ProtocolError::Imap(format!("{label} 失败: {detail}")))
        }
    }
}

/// 跑一条「只看成败」的命令，并给出调用方的命令超时。
///
/// `command_timeout` 来自 `SyncPolicy`；传进来才能让配置生效，
/// 而不是让一次 `STORE` 卡住整个账户（R27/R38）。
pub async fn run_simple_with_timeout(
    session: &mut Session,
    command: &str,
    label: &str,
    timeout: Duration,
) -> Result<(), ProtocolError> {
    match run_until_tagged(session, command, timeout).await? {
        TaggedOutcome::Ok(_) => Ok(()),
        TaggedOutcome::Failed(detail) => {
            Err(ProtocolError::Imap(format!("{label} 失败: {detail}")))
        }
    }
}

/// 跑一条 COPY / MOVE，并从 `COPYUID` 里取 UID 映射，超时由调用方给。
async fn run_transfer_with_timeout(
    session: &mut Session,
    command: &str,
    requested: &[u32],
    timeout: Duration,
) -> Result<TransferOutcome, ProtocolError> {
    match run_until_tagged(session, command, timeout).await? {
        TaggedOutcome::Ok(details) => {
            // 服务器没给 COPYUID 时，源集合就是我们自己发出去的那些
            // ——这个信息不需要服务器确认。
            let mut outcome = match details.copy_uid {
                Some(code) => code.into_outcome(requested),
                None => CopyUid::absent_outcome(requested),
            };
            outcome.message = details.information;
            Ok(outcome)
        }
        TaggedOutcome::Failed(detail) => Err(ProtocolError::Imap(detail)),
    }
}

/// 一条带 tag 命令的结局。
enum TaggedOutcome {
    /// 服务器回了 `OK`。
    Ok(OkDetails),
    /// 服务器回了 `NO` / `BAD`（已带上下文）。
    Failed(String),
}

/// `OK` 里我们真正用得上的东西（已脱离响应缓冲区的生命周期）。
#[derive(Debug, Default)]
struct OkDetails {
    /// `COPYUID` 里的 UID 映射（服务器没给就是 `None`）。
    copy_uid: Option<CopyUid>,
    /// 服务器的说明文字。
    information: Option<String>,
}

/// `COPYUID` 的已解析形态。
///
/// 对测试公开（`tests/imap_parsing_properties.rs`）：`COPYUID` 是服务器给的
/// 「源 ↔ 目标」对应关系，错一位就会去核对另一封邮件，值得用随机的集合去撞。
/// 字段刻意保持私有——构造只能用 [`expand_uid_set`] 与 [`CopyUid::state`]
/// 这两个「服务器怎么说就怎么读」的入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyUid {
    source_validity: u32,
    source_uids: Vec<u32>,
    destination_uids: Vec<u32>,
}

impl CopyUid {
    /// 从 `COPYUID` 的组成部分建一份。
    ///
    /// 生产路径只有 [`copy_uid_of`] 一个调用点；单独露出来是为了让测试能
    /// **在不起服务器**的情况下喂随机集合（否则每条随机样本都要一次 TLS 握手）。
    pub fn new(source_validity: u32, source_uids: Vec<u32>, destination_uids: Vec<u32>) -> Self {
        Self {
            source_validity,
            source_uids,
            destination_uids,
        }
    }

    /// 服务器的原话：`(源 UIDVALIDITY, 源集合, 目标集合)`。
    pub fn state(&self) -> (u32, &[u32], &[u32]) {
        (
            self.source_validity,
            &self.source_uids,
            &self.destination_uids,
        )
    }

    /// 按服务器给的映射组装 [`TransferOutcome`]。
    ///
    /// `requested` 是调用方原本要求处理的 UID：源集合为空时（服务器给了
    /// `COPYUID` 却一个 UID 都没解析出来）就用它——与
    /// [`run_transfer_with_timeout`] 对「没有 `COPYUID`」的处理**同一套规则**。
    pub fn into_outcome(self, requested: &[u32]) -> TransferOutcome {
        let source_uids = if self.source_uids.is_empty() {
            let mut requested = requested.to_vec();
            requested.sort_unstable();
            requested.dedup();
            requested
        } else {
            self.source_uids
        };

        TransferOutcome {
            source_uid_validity: Some(self.source_validity),
            // RFC 4315 的 COPYUID 只带**源**文件夹的 UIDVALIDITY。
            destination_uid_validity: None,
            source_uids,
            destination_uids: self.destination_uids,
            message: None,
        }
    }

    /// 「服务器没给 `COPYUID`」时的结果：源集合退回调用方要求处理的那几个。
    pub fn absent_outcome(requested: &[u32]) -> TransferOutcome {
        let mut requested = requested.to_vec();
        requested.sort_unstable();
        requested.dedup();

        TransferOutcome {
            source_uid_validity: None,
            destination_uid_validity: None,
            source_uids: requested,
            destination_uids: Vec::new(),
            message: None,
        }
    }
}

/// 发一条命令并读到**属于它的**那个 tagged 响应为止。
///
/// 关键点：别的命令发来的 untagged 响应（`EXISTS` / `EXPUNGE` / `FETCH` …）
/// 与本命令无关，**忽略并继续读**。移动/删除会让文件夹变空，
/// 服务器会连着推好几条 untagged 响应，不能遇到一条就收工。
///
/// ## 为什么要超时
///
/// 以前这里是**无上限**的读循环：服务器收了命令却不回话（网络半开、
/// 中间设备吞包、服务器内部卡死），这个 await 就永远不返回，
/// 整个账户的 actor 被一条 `COPY` 拖死——连「失败」都报不出来（R38）。
/// 现在整条命令（发送 + 读到 tagged）都包在 `timeout` 里。
///
/// 超时返回 [`ProtocolError::Timeout`]，**调用方必须把它当成「结果未知」**：
/// 命令可能已经在服务器上执行了，衔接其后的未知 untagged 响应也可能还堵在
/// 连接里。绝不能在超时之后接着用这条会话发下一条命令。
async fn run_until_tagged(
    session: &mut Session,
    command: &str,
    timeout: Duration,
) -> Result<TaggedOutcome, ProtocolError> {
    match tokio::time::timeout(timeout, read_until_tagged(session, command)).await {
        Ok(result) => result,
        Err(_) => Err(ProtocolError::Timeout(format!(
            "命令超时（{timeout:?}）: {command}"
        ))),
    }
}

/// [`run_until_tagged`] 的本体（不含超时）。
async fn read_until_tagged(
    session: &mut Session,
    command: &str,
) -> Result<TaggedOutcome, ProtocolError> {
    let id = session
        .run_command(command)
        .await
        .map_err(|e| ProtocolError::Imap(format!("发送命令失败: {e}")))?;

    loop {
        let response = session
            .read_response()
            .await
            .map_err(|e| ProtocolError::Io(e.to_string()))?;

        let Some(response) = response else {
            return Err(ProtocolError::Io("连接被断开".into()));
        };

        // 服务器说 `BYE`：这条连接已经不可信（R58）。
        //
        // 与 `command::run_checked_inner` 同一个判据、同一个错误——两条读循环
        // 分开写是因为它们要读的东西不同，但「`BYE` 意味着别再用这条连接了」
        // 这件事没有第二种解释。以前这里落进下面的 `continue`，
        // 于是 `UID MOVE` 之后的一切都在一条死连接上跑。
        if matches!(
            response.parsed(),
            Response::Data {
                status: Status::Bye,
                ..
            }
        ) {
            return Err(crate::command::bye_error());
        }

        // untagged 响应（`EXISTS` / `EXPUNGE` / `FETCH` …）：与成败无关，忽略。
        let Response::Done {
            tag,
            status,
            outcome,
        } = response.parsed()
        else {
            continue;
        };

        // 别的命令的响应：忽略，继续读。
        if tag != &id {
            continue;
        }

        let result = match status {
            Status::Ok => TaggedOutcome::Ok(OkDetails {
                copy_uid: copy_uid_of(outcome),
                information: outcome.information.as_ref().map(|text| text.to_string()),
            }),
            Status::No => TaggedOutcome::Failed(format!(
                "服务器拒绝（NO）: {}",
                crate::command::outcome_information(outcome)
            )),
            Status::Bad => TaggedOutcome::Failed(format!(
                "服务器拒绝（BAD）: {}",
                crate::command::outcome_information(outcome)
            )),
            other => TaggedOutcome::Failed(format!("意外的响应状态: {other:?}")),
        };

        return Ok(result);
    }
}

/// 从 `OK` 响应里抠出 `COPYUID`（没有就是 `None`）。
///
/// RFC 4315 的 `COPYUID` 语法是
/// `COPYUID <源 UIDVALIDITY> <源 uid-set> <目标 uid-set>`——
/// **只有一个** UIDVALIDITY，目标文件夹的它不告诉客户端。
fn copy_uid_of(outcome: &imap_proto::Outcome<'_>) -> Option<CopyUid> {
    match &outcome.code {
        Some(ResponseCode::CopyUid(validity, source, destination)) => {
            let (source_uids, source_truncated) = expand_uid_set_checked(source);
            let (destination_uids, destination_truncated) = expand_uid_set_checked(destination);

            // 任何一侧被截断，这份映射就是**残缺的**，不能当对应关系用（R47）。
            // 把目标侧置空，`has_uid_mapping()` 自然返回假，调用方会退回
            // 「没有映射」的处置（`absent_outcome` 那套），而不是照着编出来的
            // 对应关系去写本地库。
            let destination_uids = if source_truncated || destination_truncated {
                Vec::new()
            } else {
                destination_uids
            };

            Some(CopyUid::new(*validity, source_uids, destination_uids))
        }
        _ => None,
    }
}

/// 把 `UID SET` 展开成具体 UID 列表（有上限，见 [`UID_EXPANSION_LIMIT`]）。
///
/// 对测试公开（`tests/imap_parsing_properties.rs`）：这里的每一个端点
/// 都来自服务器，差一位就会去核对另一封邮件。
pub fn expand_uid_set(members: &[UidSetMember]) -> Vec<u32> {
    expand_uid_set_checked(members).0
}

/// 同 [`expand_uid_set`]，但**同时报告有没有被截断**。
///
/// ## 为什么必须把「截断」这件事传出去（R47）
///
/// `COPYUID` 里源、目标两个集合是**各自**展开的。服务器回
/// `COPYUID 1 1:4294967295 1:4294967295` 时，两边都被截到
/// `UID_EXPANSION_LIMIT`（10 000）个——长度**恰好相等**，于是
/// [`TransferOutcome::has_uid_mapping`] 那句「长度相等即可用」把它们当成
/// 一对一的有效映射：源 1..10000 ↔ 目标 1..10000。
/// 可源文件夹里可能只有 3 封。R39 的写回一旦接上，就会照着这份**编出来的**
/// 对应关系把根本不存在的信记进本地库。
///
/// 只靠「长度是否相等」判断映射是否可信是不够的：**必须知道展开过程中有
/// 没有丢东西**。截断是静默的，所以这里把它显式返回。
///
/// 返回值第二个分量：`true` 表示至少有一个成员没被完整展开。
pub fn expand_uid_set_checked(members: &[UidSetMember]) -> (Vec<u32>, bool) {
    let mut out: Vec<u32> = Vec::new();
    let mut truncated = false;

    let total = members.len();
    for (index, member) in members.iter().enumerate() {
        match member {
            UidSetMember::Uid(uid) => {
                if out.len() >= UID_EXPANSION_LIMIT {
                    // 上限已满、而这个成员放不下了：确实丢了一个。
                    truncated = true;
                    break;
                }
                out.push(*uid);
            }
            UidSetMember::UidRange(range) => {
                // `range` 是**闭区间**（imap-proto 用 `fst..=snd`），
                // 所以 `.end()` 就是最后一个 UID，**不要**再减 1。
                let remaining = UID_EXPANSION_LIMIT.saturating_sub(out.len());
                let wanted = range.clone().count();
                out.extend(range.clone().take(remaining));
                if wanted > remaining {
                    truncated = true;
                }
            }
        }

        if out.len() >= UID_EXPANSION_LIMIT {
            // 满了：**只有后面还有成员才算丢东西**。恰好用满、后面一个都不剩时
            // 什么都没丢——那时置 `truncated` 就是谎报（会让一份完好的
            // `COPYUID` 被判成不可用，写回白丢一次）。
            if index + 1 < total {
                truncated = true;
            }
            break;
        }
    }

    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_command_quotes_mailbox_and_sorts_uids() {
        assert_eq!(
            build_copy_command(&[3, 1, 2], "Trash"),
            "UID COPY 1,2,3 \"Trash\""
        );
    }

    #[test]
    fn copy_command_dedups_repeated_uids() {
        assert_eq!(
            build_copy_command(&[7, 7, 7], "Trash"),
            "UID COPY 7 \"Trash\""
        );
    }

    #[test]
    fn move_command_uses_the_move_extension() {
        assert_eq!(
            build_move_command(&[42], "&XfJT0ZAB-"),
            "UID MOVE 42 \"&XfJT0ZAB-\""
        );
    }

    #[test]
    fn mailbox_with_quote_or_backslash_is_escaped() {
        assert_eq!(
            build_copy_command(&[1], "we\"ird"),
            "UID COPY 1 \"we\\\"ird\""
        );
        assert_eq!(
            build_copy_command(&[1], "back\\slash"),
            "UID COPY 1 \"back\\\\slash\""
        );
    }

    #[test]
    fn store_deleted_command_is_silent() {
        assert_eq!(
            build_store_deleted_command(&[5, 4]),
            "UID STORE 4,5 +FLAGS.SILENT (\\Deleted)"
        );
    }

    #[test]
    fn uid_expunge_command_targets_only_the_given_set() {
        assert_eq!(build_uid_expunge_command(&[9]), "UID EXPUNGE 9");
    }

    #[test]
    fn empty_uid_set_is_rejected_before_hitting_the_wire() {
        let error = ensure_usable(&[]).expect_err("空集合必须被拒绝");
        assert!(
            matches!(error, ProtocolError::Invalid(_)),
            "应当是 Invalid，实际是 {error:?}"
        );
    }

    #[test]
    fn oversized_uid_set_is_rejected() {
        let uids: Vec<u32> = (1..=(UID_EXPANSION_LIMIT as u32 + 1)).collect();
        let error = ensure_usable(&uids).expect_err("超长集合必须被拒绝");
        assert!(matches!(error, ProtocolError::Invalid(_)));
    }

    #[test]
    fn uid_range_is_expanded_inclusively() {
        // 这是 append.rs 里那个 `end() - 1` 反例的正解：
        // `5:7` 是闭区间，包含 7。
        let members = vec![UidSetMember::UidRange(5..=7)];
        assert_eq!(expand_uid_set(&members), vec![5, 6, 7]);
    }

    #[test]
    fn uid_range_expansion_is_bounded() {
        let members = vec![UidSetMember::UidRange(1..=u32::MAX)];
        let expanded = expand_uid_set(&members);
        assert_eq!(
            expanded.len(),
            UID_EXPANSION_LIMIT,
            "区间必须被截断，不能照着展开"
        );
    }

    #[test]
    fn mixed_uid_set_expands_in_wire_order() {
        let members = vec![
            UidSetMember::Uid(3),
            UidSetMember::UidRange(10..=11),
            UidSetMember::Uid(20),
        ];
        assert_eq!(expand_uid_set(&members), vec![3, 10, 11, 20]);
    }

    #[test]
    fn outcome_without_copy_uid_reports_no_mapping() {
        let outcome = TransferOutcome {
            source_uids: vec![1, 2],
            destination_uids: Vec::new(),
            ..TransferOutcome::default()
        };
        assert!(!outcome.has_uid_mapping());
        assert_eq!(outcome.destination_uid_of(1), None);
    }

    #[test]
    fn mapping_is_positional_not_value_based() {
        // 服务器给的源集合可能是乱序的：`5,1` → `103,101`。
        // 对应关系是**按下标**，不是按大小。
        let outcome = TransferOutcome {
            source_uids: vec![5, 1],
            destination_uids: vec![103, 101],
            ..TransferOutcome::default()
        };
        assert!(outcome.has_uid_mapping());
        assert_eq!(outcome.destination_uid_of(5), Some(103));
        assert_eq!(outcome.destination_uid_of(1), Some(101));
        assert_eq!(outcome.destination_uid_of(9), None);
    }

    #[test]
    fn mismatched_lengths_are_not_treated_as_a_mapping() {
        let outcome = TransferOutcome {
            source_uids: vec![1, 2, 3],
            destination_uids: vec![100],
            ..TransferOutcome::default()
        };
        assert!(!outcome.has_uid_mapping());
        assert_eq!(outcome.destination_uid_of(1), None);
    }
}
