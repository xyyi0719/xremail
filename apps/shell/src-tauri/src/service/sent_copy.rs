//! 「发完确保已发送副本」。
//!
//! ## 为什么需要它
//!
//! 实测（2026-10-29，163 ↔ QQ 双向真发信）：
//!
//! * **163 的服务器会自己**把 SMTP 提交的邮件存一份到「已发送」；
//! * **QQ 不会**——发出去的信在「已发送」里**找不到**。
//!
//! 所以程序不能「一律自己存」（163 会变成两份），
//! 也不能「一律不存」（QQ 就看不到自己发出去的信）。
//!
//! 做法是**自适应**：发完去「已发送」里找这封信（按 `Message-ID` 匹配），
//! **找到了就什么都不做，没找到才自己 `APPEND` 一份**。
//! 两家都对，而且不用硬编码品牌。
//!
//! ## 为什么按 `Message-ID` 找
//!
//! 这是**唯一**能跨两家通用的标记：服务器自己存的那份，
//! 保留的就是我们发出去的原文，头里的 `Message-ID` 一模一样。
//! 用 `SEARCH` 找是**行不通**的——163 的 IMAP 只支持 `SEARCH ALL`（见 R23），
//! 服务器端搜索在那儿会**静默返回空**。
//!
//! ## 但不能只看 `Message-ID` 那个头
//!
//! 实测（2026-10-30，QQ 真发信）：
//!
//! * 我们发出去的头是 `Message-ID: <1793324987.1@qq.com>`；
//! * QQ 存下来的那份把 `Message-ID` **改写**成了
//!   `<tencent_D9434133699DE03606FA423AAFB3E7A4AF07@qq.com>`；
//! * 但它把**原值原封不动**地放进了另一个头：`X-OQ-MSGID: <1793324987.1@qq.com>`。
//!
//! 所以只比 `Message-ID` 那个头，永远对不上——就会以为「服务器没存」，
//! 于是去 `APPEND`，结果补出**第二份**。
//! （QQ 的服务器会拦下来回 `NO [CANNOT] Mail already saved to Sent by SMTP`，
//! 才没真的补重；但拦不拦是服务器的善意，不能指望。）
//!
//! 所以匹配规则是：**任何一个头的值等于我们的 `Message-ID`**（归一化后）就算命中。
//! 这样既不用硬编码厂商的头名，又能把 QQ 这种「改写 `Message-ID`、另留一份」的做派覆盖到。
//!
//! ## 找不到的风险
//!
//! 如果服务器是**慢一拍**才存副本（先提交、后落盘），
//! 我们可能在它落盘前就查不到，于是**补了第二份**。
//! 后果是「已发送」里多一封同样的信——**难看，但不丢数据、也不会重复发给收件人**。
//! 真要更稳，就得在补之前等一小会儿再查一次。

use std::time::Duration;

use xremail_api::SecretValue;
use xremail_core::model::Account;
use xremail_core::{AppError, AppResult};
use xremail_protocol_imap::{
    AppendOptions, DEFAULT_COMMAND_TIMEOUT, ImapConfig, Session, TlsTrust, append_message,
    connect_and_login_with, examine_checked_with_timeout, list_folders, run_checked_with_timeout,
};

/// 存副本的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentCopyOutcome {
    /// 服务器自己已经存了一份（**没有**再补）。
    AlreadyPresent,
    /// 补了一份。
    Appended {
        /// 新副本的 UID（服务器没报就是 `None`）。
        uid: Option<u32>,
    },
    /// 这个账户**没有**「已发送」文件夹。
    NoSentFolder,
}

/// 回看多少封。
///
/// 服务器自己存的那份一定是**最新**的几封之一（它就在这次发送之后落盘），
/// 所以回看 20 封足够。回看太多会把整个文件夹的头发下来，浪费。
const LOOKBACK: u32 = 20;

/// 补副本之前**等多久**再去查（毫秒）。
///
/// ## 为什么必须等
///
/// 163 是**先提交、后落盘**：SMTP 回完 250，服务器还要一会儿才把副本写进「已发送」。
/// 刚提交完就去查，多半查不到——于是补了第二份，163 的「已发送」里就有两封一样的信。
///
/// 代价是每次发送多花一点时间（QQ 那种服务器永远等不到，只能等到超时）。
/// 这笔买卖划算：多一封副本难看，缺一封副本只是「已发送」里少一封。
const SETTLE_MS: u64 = 2500;

/// 等多久查一次（毫秒）。
const POLL_MS: u64 = 400;

/// 存「已发送」副本的服务。
#[derive(Debug, Clone)]
pub struct SentCopyService {
    /// TLS 信任根（生产用系统根证书）。
    trust: TlsTrust,
    /// 回看多少封。
    lookback: u32,
    /// 补副本之前等多久再查（毫秒）。
    settle_ms: u64,
    /// 每条 IMAP 命令的超时。
    ///
    /// 有一条**必须交代的现实**（R46）：`run_checked` 的读循环是裸的
    /// `read_response().await`，async-imap 也没有命令超时。服务器收了命令
    /// 却不回话时，没有超时就会**永久挂住**——用户看到的是写信面板上
    /// 永远的「正在发送」。所以这里每条命令都过超时。
    command_timeout: Duration,
}

impl SentCopyService {
    /// 用 TLS 信任根构造。
    pub fn new(trust: TlsTrust) -> Self {
        Self {
            trust,
            lookback: LOOKBACK,
            settle_ms: SETTLE_MS,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }

    /// 改命令超时（**给测试用**：默认 30 秒，用例里要短到能等）。
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// 改「补副本前等多久」（**给测试用**：`0` = 不等）。
    pub fn with_settle(mut self, settle_ms: u64) -> Self {
        self.settle_ms = settle_ms;
        self
    }

    /// 确保「已发送」里有这封信的一份。
    ///
    /// `message_id` 是这封信的 `Message-ID`（带不带尖括号都行），
    /// `bytes` 是**已经发出去的那份字节**（和服务器收到的一模一样）。
    pub async fn ensure_sent_copy(
        &self,
        account: &Account,
        credential: &SecretValue,
        message_id: &str,
        bytes: &[u8],
    ) -> AppResult<SentCopyOutcome> {
        // 没有 `Message-ID` 就没法确认文件夹里是不是已经有这份——
        // 这时**不动**：补出第二份比缺一份更难解释。
        if normalize_message_id(message_id).is_empty() {
            return Err(AppError::Invalid(
                "这封信没有 Message-ID，没法确认「已发送」里是不是已经有它，所以没动「已发送」"
                    .into(),
            ));
        }

        let password = credential
            .as_str()
            .ok_or_else(|| AppError::Invalid("授权码不是合法的 UTF-8 文本".into()))?;

        let mut config = ImapConfig::imaps(&account.server.imap_host);
        config.port = account.server.imap_port;

        // 登录错误不能拍平：登录被拒要变 `AppError::Auth`，否则「授权码错了」
        // 会被当成可重试的协议错而永久重试。见 R25。
        let (mut session, _caps, _tls) =
            connect_and_login_with(&config, &self.trust, &account.username, password)
                .await
                .map_err(xremail_sync::map_protocol_error)?;

        let folders = list_folders(&mut session)
            .await
            .map_err(|e| AppError::Protocol(format!("列文件夹失败: {e}")))?;

        // 找「已发送」。认角色要靠 RFC 6154 的属性（`\Sent`），
        // 拿**线路上**的名字去开——服务器只认自己给出的那个形式。
        let Some(sent) = folders
            .iter()
            .find(|folder| folder.role.as_deref() == Some("sent"))
        else {
            return Ok(SentCopyOutcome::NoSentFolder);
        };
        let raw_name = sent.raw_name.clone();

        if self
            .wait_for_copy(&mut session, &raw_name, message_id)
            .await?
        {
            return Ok(SentCopyOutcome::AlreadyPresent);
        }

        let outcome = match append_message(
            &mut session,
            &raw_name,
            bytes,
            &AppendOptions {
                // 存进去的是**已经发出去的**信，标成已读——
                // 它不是新到的邮件，不该在「未读」里冒出来。
                flags: vec!["\\Seen".to_string()],
                internal_date: None,
            },
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                // 服务器明说「已经存过了，别再补」——那就是**已经有了**，
                // 不是失败。
                //
                // 这条兜底很重要：万一我们的匹配规则没认出服务器存的那份
                // （服务器改写了头、换了个标记……），
                // 至少不会补出第二份。
                let text = error.to_string();
                if text.to_ascii_lowercase().contains("already saved") {
                    return Ok(SentCopyOutcome::AlreadyPresent);
                }
                return Err(AppError::Protocol(format!("存副本失败: {error}")));
            }
        };

        Ok(SentCopyOutcome::Appended { uid: outcome.uid })
    }

    /// 等服务器的副本出现（最多等 `settle_ms` 毫秒）。
    ///
    /// 服务器可能是**慢一拍**才把自己的副本落盘——
    /// 刚提交完就查会查不到，于是补出第二份。
    /// 所以这里轮询一会儿，等到就收手。
    async fn wait_for_copy(
        &self,
        session: &mut Session,
        raw_name: &str,
        message_id: &str,
    ) -> AppResult<bool> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(self.settle_ms);

        loop {
            if self
                .folder_has_message_id(session, raw_name, message_id)
                .await?
            {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;
        }
    }

    /// 文件夹里有没有这封信（按 `Message-ID` 匹配）。
    async fn folder_has_message_id(
        &self,
        session: &mut Session,
        raw_name: &str,
        message_id: &str,
    ) -> AppResult<bool> {
        let wanted = normalize_message_id(message_id);
        if wanted.is_empty() {
            return Ok(false);
        }

        let max_uid = self.max_uid(session, raw_name).await?;
        if max_uid == 0 {
            // 文件夹是空的——肯定没有。
            return Ok(false);
        }

        let start = max_uid.saturating_sub(self.lookback - 1).max(1);
        // 显式带超时（R46）：`run_checked` 现在自带默认超时，但这里写明时长，
        // 让「这一步会挂」这件事在代码里是显式的，而不是靠记住默认值。
        let outcome = run_checked_with_timeout(
            session,
            &format!("UID FETCH {start}:{max_uid} (BODY.PEEK[HEADER])"),
            self.command_timeout,
        )
        .await
        .map_err(|e| AppError::Protocol(format!("读「已发送」失败: {e}")))?;

        Ok(outcome
            .fetches()
            .iter()
            .filter_map(|item| item.header.as_ref())
            .any(|raw| headers_contain_message_id(raw, &wanted)))
    }

    /// 文件夹里最大的 UID。
    ///
    /// 先看 `EXAMINE` 报的 `UIDNEXT`（减一就是最大的 UID）；
    /// 有些服务器不报这个，那就退一步用 `UID FETCH 1:* (UID)` 自己数。
    async fn max_uid(&self, session: &mut Session, raw_name: &str) -> AppResult<u32> {
        let info = examine_checked_with_timeout(session, raw_name, self.command_timeout)
            .await
            .map_err(|e| AppError::Protocol(format!("打开「已发送」失败: {e}")))?;

        if let Some(next) = info.uid_next {
            return Ok(next.saturating_sub(1));
        }

        let outcome =
            run_checked_with_timeout(session, "UID FETCH 1:* (UID)", self.command_timeout)
                .await
                .map_err(|e| AppError::Protocol(format!("读「已发送」失败: {e}")))?;
        Ok(outcome
            .fetches()
            .iter()
            .filter_map(|item| item.uid)
            .max()
            .unwrap_or(0))
    }
}

/// 原始头里有没有哪个头的值等于我们的 `Message-ID`（归一化后）。
///
/// ## 为什么要看**所有**头
///
/// QQ 会把 `Message-ID` 改写成 `tencent_xxx@qq.com`，
/// 但把原值留在 `X-OQ-MSGID` 里（实测 2026-10-30）。
/// 只看 `Message-ID` 那个头就会漏掉它。
///
/// 所以：**任何一个头的值**（归一化后）等于我们要找的，就算命中。
///
/// ## 折叠行
///
/// 一个头的值可能跨行（以空白开头的续行），
/// 所以要把续行接在上一行的值后面再比。
fn headers_contain_message_id(raw: &[u8], wanted: &str) -> bool {
    if wanted.is_empty() {
        return false;
    }
    let text = String::from_utf8_lossy(raw);

    let mut current: Option<String> = None;
    for line in text.split("\r\n") {
        if line.is_empty() {
            // 头区结束。
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // 续行：接到上一行的值后面。
            if let Some(value) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        // 新的一行头：先把上一行结账。
        if let Some(value) = current.take()
            && header_value_matches(&value, wanted)
        {
            return true;
        }
        current = line
            .split_once(':')
            .map(|(_name, value)| value.trim().to_string());
    }
    if let Some(value) = current
        && header_value_matches(&value, wanted)
    {
        return true;
    }
    false
}

/// 一个头的值是不是就是我们要找的那个 `Message-ID`。
///
/// ## 怎么比
///
/// `Message-ID` 在 RFC 5322 里**一定是尖括号形态**（`<id@domain>`），
/// 所以比的是**尖括号里的那一段**。
///
/// 这比「整值相等」准：QQ 的 `X-OQ-MSGID` 头里就正好是尖括号形态；
/// 而 `Received:` 那种头里什么都有，不会误撞。
fn header_value_matches(value: &str, wanted: &str) -> bool {
    // 整个值就是它（服务器可能存了不带尖括号的形式）。
    if normalize_message_id(value) == wanted {
        return true;
    }
    // 尖括号里的那一段。
    bracketed_tokens(value)
        .iter()
        .any(|token| normalize_message_id(token) == wanted)
}

/// 抽出值里所有 `<...>` 的内容。
fn bracketed_tokens(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

/// 把 `Message-ID` 归一化，好比较。
///
/// 服务器给的和本地存的可能一个带尖括号一个不带，
/// 而且 `Message-ID` 的**大小写理论上敏感**——但这里放宽成不敏感：
/// 宁可多找出一封，也不愿意**补出第二份**。
fn normalize_message_id(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{headers_contain_message_id, normalize_message_id};

    #[test]
    fn message_ids_compare_regardless_of_brackets_and_case() {
        assert_eq!(
            normalize_message_id("<ABC@Example.com>"),
            normalize_message_id("abc@example.com")
        );
        assert_eq!(
            normalize_message_id(" <abc@example.com> "),
            normalize_message_id("abc@example.com")
        );
    }

    #[test]
    fn different_message_ids_stay_different() {
        assert_ne!(normalize_message_id("<a@x>"), normalize_message_id("<b@x>"));
    }

    #[test]
    fn the_standard_message_id_header_is_found() {
        let raw = b"From: a@x\r\nMessage-ID: <abc@x>\r\nSubject: hi\r\n\r\n";
        assert!(headers_contain_message_id(raw, "abc@x"));
        assert!(!headers_contain_message_id(raw, "other@x"));
    }

    #[test]
    fn a_message_id_kept_in_another_header_is_also_found() {
        // QQ 的做法：`Message-ID` 被改写，原值在 `X-OQ-MSGID` 里。
        let raw = b"Message-ID: <tencent_D943@qq.com>\r\nX-OQ-MSGID: <abc@qq.com>\r\n\r\n";
        assert!(
            headers_contain_message_id(raw, "abc@qq.com"),
            "原值在别的头里也得找到，否则会以为服务器没存副本、进而补重"
        );
        assert!(
            headers_contain_message_id(raw, "tencent_d943@qq.com"),
            "改写后的那个值也在头里——问什么就答什么"
        );
        assert!(!headers_contain_message_id(raw, "someone_else@qq.com"));
    }

    #[test]
    fn a_folded_header_value_is_still_matched_by_its_bracketed_part() {
        let raw = b"X-OQ-MSGID: <abc@x>\r\n\tcontinued\r\nSubject: hi\r\n\r\n";
        // 续行接上去了，值变成「<abc@x> continued」；
        // 但 `Message-ID` 是尖括号形态，所以照样认得出。
        assert!(headers_contain_message_id(raw, "abc@x"));
        assert!(!headers_contain_message_id(raw, "abc@x continued"));
    }

    #[test]
    fn an_unrelated_header_never_matches() {
        let raw = b"Received: from a by b id C7038482; Fri, 30 Oct 2026\r\nSubject: hi\r\n\r\n";
        assert!(!headers_contain_message_id(raw, "c7038482"));
        assert!(!headers_contain_message_id(raw, "fri, 30 oct 2026"));
    }
}
