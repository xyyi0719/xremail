//! 托盘图标与「退出 / 恢复」语义。
//!
//! ## 语义（这就是这条规则的唯一出处，别处只引用）
//!
//! * **关闭主窗口 = 退到托盘**：窗口 `hide()`、进程与后台同步继续跑。
//! * **托盘菜单「退出」/ 系统发起的退出（注销、关机、任务管理器）= 真退出**：
//!   `app.exit(0)` → `RunEvent::Exit` → `Scheduler::shutdown()`。
//! * **托盘左键单击 = 显示并聚焦主窗口**，与单实例插件的「第二次启动」一致。
//!
//! 为什么关窗口不退出：后台同步（`Scheduler`）是这个应用的长期存在理由——
//! 没了它，「关掉窗口」就等于「今天不再收信」。所以窗口藏起来、同步留着。
//!
//! 但**必须留一个能真正退出的口子**，否则用户被「关不掉」困住：这是托盘
//! 应用最常见的坑（任务管理器里一堆杀不掉的进程，多半就是这种）。
//! 这里的口子就是菜单里的「退出」，且它走的是 `app.exit(0)` 而不是
//! `std::process::exit`——后者**不会**触发 `RunEvent::Exit`，`Scheduler::shutdown`
//! 就成了一段永不执行的代码，退出时 actor 可能还在 `IDLE` 或写库，
//! 未提交的写操作会以 `running` 的状态留在库里（见 `lib.rs` 里那段注释）。
//!
//! 为什么用 `CloseRequested` + `prevent_close` 而不是 Tauri 的
//! `WindowEvent::CloseRequested { api }`？Tauri 2 的这个事件**不暴露** `api`，
//! 由应用全局监听的 `WindowEvent` 在窗口关闭时只要事件循环里还有托盘图标驻留
//! 就会让进程活着——于是窗口关掉后进程还在，但**没有人能把窗口再叫回来**
//! （窗口已销毁，托盘左键只会聚焦一个不存在的窗口）。所以这里必须在窗口
//! 真正关闭之前把它拦住藏起来。
//!
//! ## 开机自启时**仍然弹窗口**
//!
//! 能干净做到「自启只驻留托盘」——插件把启动参数传成 `--flag <autostart>`，
//! 在 `setup` 里读一下就够了。这里**刻意不做**：`setup` 里窗口还没显示，
//! 要先判断参数再决定「显示不显示」，而 `tauri.conf.json` 里窗口默认是显示的，
//! 我们只能去关它——一旦参数解析出错或用户从开始菜单、任务栏钉住启动，
//! 表现就是「点了没反应」，比多弹一个窗口难查得多。开机自启的诉求是
//! 「登录后后台同步已经在跑」，这个在当前实现下已经满足（窗口开着，
//! 同样是后台同步在跑）。

use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

use crate::state::AppState;

/// 托盘图标的 id。
pub const TRAY_ID: &str = "main";

/// 托盘菜单项。
///
/// 抽成枚举是为了让「菜单 id → 动作」这一步是**纯映射**、能直接单测：
/// 菜单回调本身在 Linux 上没有事件（见 [`install`] 的说明），拿不到真机反馈，
/// 那至少要把「哪个 id 对应哪个动作」用用例钉住。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// 显示并聚焦主窗口。
    Show,
    /// 叫醒所有在调度的账户，立刻同步一轮。
    SyncNow,
    /// 真退出进程（经 `RunEvent::Exit`，不是 `process::exit`）。
    Quit,
}

/// 菜单 id 常量。菜单项与回调各引用一次，不写字面量。
pub const MENU_SHOW: &str = "tray-show";
pub const MENU_SYNC_NOW: &str = "tray-sync-now";
pub const MENU_QUIT: &str = "tray-quit";

impl TrayAction {
    /// 菜单 id → 动作。未知 id（不是我们的菜单项）→ `None`，**不猜**。
    ///
    /// 不猜的理由：托盘菜单的回调挂在应用级，别的菜单项（将来某个右键菜单）
    /// 的 id 也会经过这里。把不认识的 id 当成「退出」是最糟的猜法。
    pub fn from_menu_id(id: &str) -> Option<Self> {
        match id {
            MENU_SHOW => Some(Self::Show),
            MENU_SYNC_NOW => Some(Self::SyncNow),
            MENU_QUIT => Some(Self::Quit),
            _ => None,
        }
    }

    /// 这一下要不要去叫醒后台同步。
    ///
    /// 单独拆出来是为了让用例能断言「显示/退出**不**顺带触发一次同步」——
    /// 显示窗口时叫醒一轮会让用户以为「打开应用就能收信」，
    /// 那是一次没人要的全账户网络请求。
    pub fn wakes_sync(self) -> bool {
        matches!(self, Self::SyncNow)
    }
}

/// 显示并聚焦主窗口。窗口被藏起来（关闭窗口走的是 [`install`] 的钩子）时先显示。
///
/// 失败只记日志：托盘已经不可用的机器上，点一下没反应比 panic 好——
/// panic 在托盘回调线程里会直接带崩进程。
pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("托盘：找不到主窗口");
        return;
    };
    if let Err(error) = window.show() {
        tracing::warn!(%error, "托盘：显示主窗口失败");
    }
    // 顺序要紧：先 `show` 再 `set_focus`。对隐藏窗口直接聚焦，Windows 上
    // 窗口不会自己冒出来。
    let _ = window.unminimize();
    if let Err(error) = window.set_focus() {
        tracing::warn!(%error, "托盘：聚焦主窗口失败");
    }
}

/// 创建托盘图标。**只在桌面端**调用（Android 上没有托盘）。
///
/// 图标用 `app.default_window_icon()`：它来自 `tauri.conf.json` 的 `bundle.icon`
/// 里那张 32x32，并且**已经被 tauri-build 解码进二进制**（`Context::default_window_icon`）。
/// 不走 `include_bytes!` 的理由：那要额外打开解码路径（`Image::from_bytes`
/// 还得开 `image-png` feature），而这里同一个文件已经被打进二进制一次了。
///
/// ## 平台差异（**本机无显示器，下面每条都未真机验证**）
///
/// * **Linux**：`TrayIconEvent` 根本不发（libayatana-appindicator 只提供菜单），
///   所以「左键显示窗口」在 Linux 上**不生效**，要用菜单里的「显示主窗口」。
///   这是平台能力，不是缺省实现。
/// * **Windows**：左键单击 → 显示并聚焦；右键 → 菜单。
/// * **macOS**：左键单击会先弹菜单（除非 `show_menu_on_left_click(false)`），
///   所以单击也要走菜单里的「显示主窗口」。
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let menu = MenuBuilder::new(app)
        .text(MENU_SHOW, "显示主窗口")
        .text(MENU_SYNC_NOW, "立即同步")
        .separator()
        .text(MENU_QUIT, "退出")
        .build()?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        // 左键**不**弹菜单：左键留给「显示/聚焦」（见本模块顶部）。
        // Linux 上这个设置不生效（没有左键事件，点图标就是弹菜单）。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let Some(action) = TrayAction::from_menu_id(event.id().as_ref()) else {
                return;
            };
            run_action(app, action);
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    } else {
        // 没有图标也能建出来（Windows 上会是一个空白格），总比整个启动失败好。
        tracing::warn!("托盘：没有默认窗口图标，托盘图标将为空");
    }

    builder.build(app)?;
    Ok(())
}

/// 「关闭窗口 = 退到托盘」的那一半：关闭窗口时把它藏起来，进程与后台同步继续跑。
///
/// 由 `lib.rs` 交给 `Builder::on_window_event`（Tauri 只提供 **builder** 级的
/// 全局窗口事件钩子，`AppHandle` 上没有——所以这一步不能塞进 [`install`]）。
///
/// 为什么拦在 `CloseRequested` 上藏窗口、而不是让窗口关掉、靠托盘图标把进程留住：
/// Tauri 2 的 `WindowEvent::CloseRequested` **不暴露** `api`，应用拦不住关闭；
/// 窗口一旦真的销毁，托盘左键就只会去聚焦一个不存在的窗口——进程还在、
/// 后台同步还在跑，但**用户打不开界面**了。那就是「关不掉也回不去」。
///
/// 只处理 `label == "main"`：将来若有多窗口，关一个辅助窗口不该把它藏起来。
pub fn handle_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if let tauri::WindowEvent::CloseRequested { .. } = event
        && window.label() == "main"
        && let Err(error) = window.hide()
    {
        tracing::warn!(%error, "托盘：隐藏主窗口失败（窗口可能已被销毁）");
    }
}

/// 执行一个托盘动作。
fn run_action<R: Runtime>(app: &AppHandle<R>, action: TrayAction) {
    if action.wakes_sync() {
        // 托盘菜单没有可以弹提示的地方，反馈全在日志与前端事件里：
        // 叫醒成功后每个账户自己会发 `sync://state-changed`，前端收到就刷新。
        let state = app.state::<AppState>();
        let woken = crate::service::scheduler::wake_all(&state.scheduler);
        if woken == 0 {
            tracing::info!("托盘「立即同步」：没有正在调度的账户");
        } else {
            tracing::info!(woken, "托盘「立即同步」：已叫醒账户");
        }
    }

    match action {
        TrayAction::Show => show_main_window(app),
        // `app.exit` **不是** `process::exit`：它会让事件循环收尾并发出
        // `RunEvent::Exit`，`Scheduler::shutdown` 才跑得到。换成
        // `std::process::exit(0)` 就等于悄悄丢掉那段收尾。
        TrayAction::Quit => app.exit(0),
        TrayAction::SyncNow => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(TrayAction::from_menu_id(MENU_SHOW), Some(TrayAction::Show));
        assert_eq!(
            TrayAction::from_menu_id(MENU_SYNC_NOW),
            Some(TrayAction::SyncNow)
        );
        assert_eq!(TrayAction::from_menu_id(MENU_QUIT), Some(TrayAction::Quit));
    }

    #[test]
    fn unknown_menu_ids_are_ignored() {
        // 别的菜单（例如将来窗口的右键菜单）的 id 也会经过托盘回调。
        // 把不认识的 id 猜成「退出」是最糟的猜法。
        assert_eq!(TrayAction::from_menu_id(""), None);
        assert_eq!(TrayAction::from_menu_id("copy"), None);
        assert_eq!(TrayAction::from_menu_id("tray-show-"), None);
    }

    #[test]
    fn only_sync_now_wakes_the_background_sync() {
        // 「显示主窗口」顺手开一轮同步 = 用户打开应用就发一次全网请求；
        // 「退出」更不该在关门前拉起网络。
        assert!(TrayAction::SyncNow.wakes_sync());
        assert!(!TrayAction::Show.wakes_sync());
        assert!(!TrayAction::Quit.wakes_sync());
    }

    #[test]
    fn menu_ids_are_distinct() {
        // 两个菜单项共用一个 id，等于其中一个永远不触发——而这个错误
        // 只有真去点才会发现，本机又点不了。
        let ids = [MENU_SHOW, MENU_SYNC_NOW, MENU_QUIT];
        for (i, left) in ids.iter().enumerate() {
            for right in &ids[i + 1..] {
                assert_ne!(left, right);
            }
        }
    }
}
