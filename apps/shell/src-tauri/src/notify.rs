//! 新邮件通知（P5-L 的 D-Bus 通知）。
//!
//! ## 策略：什么时候该弹通知
//!
//! 这几条是**产品取舍**，不是实现细节，所以写在这里：
//!
//! 1. **只通知 `INBOX`。** 别的文件夹有新信（已发送的自副本、归档、垃圾箱里
//!    新来的垃圾邮件）都不该打断用户。判据是 `FolderSyncReport.remote_name`
//!    与 `INBOX` 比较——IMAP 规定收件箱的名字就是 `INBOX`（大小写不敏感），
//!    而且它**不做** modified UTF-7 编码（RFC 3501 §5.1.3 明确排除了它），
//!    所以这里可以直接按名字比，不需要走 `utf7::decode`。
//! 2. **应用在前台时不通知。** 用户就盯着界面，再弹一个系统通知是噪音；
//!    而且前台时列表本来就在刷新，他会直接看到。判据是主窗口是否
//!    `is_focused()`——注意不是 `is_visible()`：窗口被托盘藏起来时
//!    `visible == false`，那正是**需要**通知的时候。
//! 3. **首次同步不通知。** 第一次接一个邮箱时本地会一口气拉几百上千封，
//!    那不是「新邮件」，那是历史。`FolderSyncReport.first_sync` 正是这个信号；
//!    epoch 切换（`epoch_changed`）同理——服务器重建了文件夹，全量重拉，
//!    一封都不是新的。
//! 4. **聚合显示。** 一轮同步来了 50 封，弹 50 个通知等于骚扰。只弹一条，
//!    标题写清账号与封数；超过 3 封时正文改成「还有 N 封」。
//!    **不逐封弹**是这一条的全部意义。
//!
//!    正文里**没有**发件人与主题：`FolderSyncReport` 里只有计数，要拿发件人
//!    与主题得多查一次库。这点信息对一条通知不值一次额外的数据库往返，
//!    而且查库会让「同步结果」和「通知内容」之间多一个可能不一致的时刻。
//!    （这一段的**上一版写着「正文取最新那封的发件人与主题」——那是没实现的
//!    描述，已按实际改掉。）
//!
//! ## 未做：点击通知
//!
//! 现在**没有**处理通知的点击：`notify_new_mail` 只做「申请权限 + show」，
//! 点通知大概率没有任何反应。要做到「点一下聚焦主窗口」甚至「跳到那一封」，
//! 需要插件的点击回调 + 前端配合（切换账户 → 文件夹 → 选中邮件），
//! 是一条独立的交互链路，**本轮没有实现**。
//! （上一版这里写着「先保证用户点一下至少能回到应用」——同样是没有实现
//! 的承诺，已删掉。）
//!
//! ## 为什么不做「去重」
//!
//! 同一封信在相邻两轮同步里不会被算成两次新邮件：`added` 来自
//! `UidDiff`，本地已经有了就不会再算。所以这里不需要额外的去重状态——
//! 加一份「已经通知过哪些 UID」的表，反而要处理清理、epoch 失效、
//! 崩溃恢复，代价远大于收益。
//!
//! ## 失败怎么办
//!
//! **只记日志，不影响同步。** 通知是锦上添花：没有通知守护进程的机器
//! （无头 CI、没装通知服务的桌面）本来就发不出来，那不该让一轮同步失败。

use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_notification::NotificationExt;

/// 收件箱的线路名（IMAP 规定它就是 `INBOX`，且**不做** modified UTF-7 编码）。
const INBOX: &str = "INBOX";

/// 正文里最多列几封的明细。超过就改成「还有 N 封」。
const MAX_LISTED: usize = 3;

/// 一条待发的通知。
///
/// 抽成纯数据是为了**能单测**：真正的 `show()` 要一个图形会话，
/// 而「该不该通知、通知写什么」这些判断才是容易写错、也最该钉住的部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMailNotice {
    /// 通知标题。
    pub title: String,
    /// 通知正文。
    pub body: String,
}

/// 一轮同步里 `INBOX` 新增的封数。
///
/// 返回 `None` 表示「这一轮没有值得通知的新邮件」，原因可能是：
/// 不是 INBOX、首次同步、epoch 切换、或者一封都没新增。
pub fn inbox_added(reports: &[xremail_sync::pull::FolderSyncReport]) -> Option<usize> {
    let inbox = reports.iter().find(|report| {
        // `INBOX` 与服务器实际返回的线路名可能大小写不同（RFC 3501 §5.1.3：
        // 收件箱名大小写不敏感）。别的文件夹名**是**大小写敏感的，
        // 但这里只关心收件箱，所以只对这一个名字放宽。
        report.remote_name.eq_ignore_ascii_case(INBOX)
    })?;

    // 首次同步 / epoch 切换都是「全量重拉历史」，不是新邮件。
    if inbox.first_sync || inbox.epoch_changed {
        return None;
    }

    (inbox.added > 0).then_some(inbox.added)
}

/// 由同步结果拼一条通知；没有值得通知的东西就返回 `None`。
///
/// `account_address` 是账号的邮箱地址（用来在标题里区分是哪个邮箱）。
pub fn compose_notice(
    account_address: &str,
    reports: &[xremail_sync::pull::FolderSyncReport],
) -> Option<NewMailNotice> {
    let added = inbox_added(reports)?;

    // 取最新的那几封：`last_uid` 越大越新，列表里也是按它排的。
    //
    // 这里拿不到主题与发件人（`FolderSyncReport` 只有计数），所以正文只能
    // 用「N 封新邮件」这种说法。**不为了好看去多查一次库**——通知是一条
    // 低价值的旁路，为一句话多打一次数据库往返不划算，而且查库会让
    // 「同步结果」与「通知内容」之间多一个可能不一致的时刻。
    let title = if added == 1 {
        format!("{account_address} 收到 1 封新邮件")
    } else {
        format!("{account_address} 收到 {added} 封新邮件")
    };

    let body = if added > MAX_LISTED {
        format!("还有 {} 封未读", added - MAX_LISTED)
    } else {
        "打开 Xremail 查看".to_string()
    };

    Some(NewMailNotice { title, body })
}

/// 主窗口是不是正被用户盯着。
///
/// 判据用 `is_focused()` 而不是 `is_visible()`：窗口被托盘藏起来时
/// `visible == false`，那正是**需要**通知的场景；反过来窗口开着但在后台
/// （`visible == true, focused == false`），用户看不见，也该通知。
///
/// 拿不到窗口时返回 `false`（当作「不在前台」）——宁可多弹一条通知，
/// 也不要因为窗口状态读不出来就让用户彻底收不到提醒。
fn window_is_focused<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.get_webview_window("main")
        .and_then(|window| window.is_focused().ok())
        .unwrap_or(false)
}

/// 一轮同步完成后，按需弹一条通知。
///
/// 调用方是 `TauriSink`（它手里有 `AccountEvent::Synced` 的 `summary`）。
/// 所有失败都只记日志：通知发不出去不该影响同步。
pub fn notify_new_mail<R: Runtime>(
    app: &AppHandle<R>,
    account_address: &str,
    reports: &[xremail_sync::pull::FolderSyncReport],
) {
    // 前台不通知：用户就在看，再弹一次是噪音。
    if window_is_focused(app) {
        return;
    }
    let Some(notice) = compose_notice(account_address, reports) else {
        return;
    };

    // 权限：Linux 上 D-Bus 通知通常不需要显式申请，Windows/Android 需要。
    // 失败只记日志——申请被拒绝就说明用户不想要通知，不必再弹。
    if let Err(error) = app.notification().request_permission() {
        tracing::debug!(%error, "通知：申请权限失败，跳过");
        return;
    }

    match app
        .notification()
        .builder()
        .title(&notice.title)
        .body(&notice.body)
        .show()
    {
        Ok(()) => tracing::debug!(title = %notice.title, "已弹出新邮件通知"),
        // 发不出去（没有通知守护进程）是**正常情况**，不是错误：
        // 无头 CI、精简桌面都可能没有。所以是 `debug` 不是 `warn`。
        Err(error) => tracing::debug!(%error, "通知：发送失败（多半是没有通知守护进程）"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xremail_sync::pull::FolderSyncReport;

    /// 造一份文件夹报告。
    fn report(remote_name: &str, added: usize) -> FolderSyncReport {
        FolderSyncReport {
            folder_id: format!("f-{remote_name}"),
            remote_name: remote_name.to_string(),
            uidvalidity: 1,
            full_fetch: false,
            epoch_changed: false,
            first_sync: false,
            added,
            removed: 0,
            retained: 0,
            last_uid: Some(100),
        }
    }

    #[test]
    fn only_the_inbox_produces_a_notice() {
        // 别的文件夹有新信（已发送的自副本、归档、垃圾箱里新来的垃圾邮件）
        // 都不该打断用户。
        assert!(compose_notice("a@163.com", &[report("已发送", 5)]).is_none());
        assert!(compose_notice("a@163.com", &[report("Archives", 3)]).is_none());
        assert!(compose_notice("a@163.com", &[report("INBOX", 2)]).is_some());
    }

    #[test]
    fn the_inbox_name_is_case_insensitive() {
        // RFC 3501 §5.1.3：收件箱名大小写不敏感，服务器可能回 `Inbox`。
        assert!(compose_notice("a@163.com", &[report("Inbox", 1)]).is_some());
        assert!(compose_notice("a@163.com", &[report("inbox", 1)]).is_some());
    }

    #[test]
    fn a_first_sync_never_notifies() {
        // 第一次接邮箱会一口气拉几百上千封历史，那不是「新邮件」。
        let mut first = report("INBOX", 500);
        first.first_sync = true;
        assert!(
            compose_notice("a@163.com", &[first]).is_none(),
            "首次同步拉的历史不能当成新邮件弹通知"
        );
    }

    #[test]
    fn an_epoch_change_never_notifies() {
        // 服务器重建了文件夹，全量重拉，一封都不是新的。
        let mut rebuilt = report("INBOX", 300);
        rebuilt.epoch_changed = true;
        assert!(compose_notice("a@163.com", &[rebuilt]).is_none());
    }

    #[test]
    fn no_new_mail_means_no_notice() {
        assert!(compose_notice("a@163.com", &[report("INBOX", 0)]).is_none());
        assert!(compose_notice("a@163.com", &[]).is_none());
    }

    #[test]
    fn the_notice_aggregates_instead_of_listing_every_message() {
        // 一轮来 50 封：只弹**一条**，不能弹 50 条。
        let notice = compose_notice("a@163.com", &[report("INBOX", 50)]).expect("应当通知");
        assert!(
            notice.title.contains("50"),
            "标题要说清封数：{}",
            notice.title
        );
        assert!(notice.title.contains("a@163.com"), "标题要能认出是哪个邮箱");
        assert!(
            notice.body.contains("47"),
            "超过 3 封时正文改成「还有 N 封」：{}",
            notice.body
        );
    }

    #[test]
    fn a_single_message_gets_a_plain_title() {
        let notice = compose_notice("a@163.com", &[report("INBOX", 1)]).expect("应当通知");
        assert!(notice.title.contains('1'));
        assert!(notice.title.contains("a@163.com"));
    }

    #[test]
    fn several_folders_only_count_the_inbox() {
        // 同一轮里别的文件夹也变了，但通知只看 INBOX 的封数。
        let reports = [report("已发送", 9), report("INBOX", 2), report("Trash", 4)];
        assert_eq!(inbox_added(&reports), Some(2));
        let notice = compose_notice("a@163.com", &reports).expect("应当通知");
        assert!(
            notice.title.contains('2'),
            "只数 INBOX 的：{}",
            notice.title
        );
    }
}
