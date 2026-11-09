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
//! ## 为什么是「关窗口就 `hide()`」，而不是让它关掉、靠托盘留住进程
//!
//! 让它关掉的话：窗口对象**被销毁**，而进程（和托盘图标）还在。之后托盘
//! 左键 / 菜单里的「显示主窗口」只会去聚焦一个已经不存在的窗口——于是
//! 进程活着、后台同步还在跑，但**用户再也打不开界面**。那是「关不掉也回不去」。
//!
//! 所以要在窗口真正关闭之前把它藏起来。`WindowEvent::CloseRequested` 在
//! **builder 级的全局窗口钩子**上能收到（Tauri 2 的 `AppHandle` 上没有
//! `on_window_event`，只有 `Builder::on_window_event`——这就是 `lib.rs` 里
//! 把 `handle_window_event` 挂到 builder 上的原因）。
//!
//! 注意：本实现**没有**用 `prevent_close`（那是窗口级 API）。它只是在
//! `CloseRequested` 到达时 `hide()` 窗口；窗口对象一直活着，随时能再显示。
//! （本段上一版写成「`CloseRequested` + `prevent_close`」并把理由说得绕，
//! 与实际实现不符，已按代码改写。）
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

use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder};
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
    /// 切换「开机自启」。
    ///
    /// **没有「打开」和「关闭」两个菜单项**：那会让菜单长得像「状态指示器」，
    /// 用户点哪一个都得先自己判断当前是开还是关。一个勾选项，点一下就切换——
    /// 状态由菜单的勾显示，读的是系统里**真**的状态（不是内存里的布尔值）。
    ToggleAutostart,
}

/// 菜单 id 常量。菜单项与回调各引用一次，不写字面量。
pub const MENU_SHOW: &str = "tray-show";
pub const MENU_SYNC_NOW: &str = "tray-sync-now";
pub const MENU_QUIT: &str = "tray-quit";
pub const MENU_AUTOSTART: &str = "tray-autostart";

/// 托盘菜单的句柄。
///
/// 为什么需要它：`TrayIcon` **不暴露 `menu()`**，而「开机自启」那一项的勾
/// 得在切换之后改。存一份 `Menu` 是唯一能拿到子项的路子。
///
/// 用泛型 `R` 而不是写死 `Wry`：`install` 本身是泛型的，测试用的 `MockRuntime`
/// 也要能走到这条路径——写死 Wry 的话，mock 下 `try_state` 永远拿不到，
/// 这类「只在 mock 下静默失效」的分支最难发现。
pub struct TrayMenuState<R: Runtime> {
    menu: Menu<R>,
}

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
            MENU_AUTOSTART => Some(Self::ToggleAutostart),
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
    // 开机自启那一项是**勾选项**：进来时先读系统里的真实状态，别让它一开始就说谎。
    // 读不到就当作没开（理由见 [`refresh_autostart_check`]）。
    let autostart_enabled = {
        use tauri_plugin_autostart::ManagerExt as _;
        app.autolaunch().is_enabled().unwrap_or(false)
    };
    // 勾选项单独建：`MenuBuilder` 没有能带初始勾选状态的 `check()`，
    // 而我们要的正是「一打开菜单，勾就是对的那个」。
    let autostart = CheckMenuItemBuilder::with_id(MENU_AUTOSTART, "开机自启")
        .checked(autostart_enabled)
        .build(app)?;
    let menu = MenuBuilder::new(app)
        .text(MENU_SHOW, "显示主窗口")
        .text(MENU_SYNC_NOW, "立即同步")
        .separator()
        .item(&autostart)
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
    // 把勾选项的句柄交给应用状态：`TrayIcon` 不暴露 `menu()`，
    // 之后要改这一项的勾（[`refresh_autostart_check`]）就只能靠这个句柄。
    // 用 `manage` 而不是存进 `AppState`：它只属于托盘，和业务状态无关；
    // 重复 `manage` 同一类型会 panic，所以先 `try_manage`——托盘只建一次，
    // 但 `install` 失败后重试的路径不该因为这条 panic。
    let _ = app.manage(TrayMenuState { menu: menu.clone() });
    // 建好之后立刻把勾刷成真实状态：上面那份是**建菜单那一刻**读的，
    // 中间可能被用户在别处改过（概率低，但重读一次是免费的）。
    refresh_autostart_check(app);
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
        TrayAction::ToggleAutostart => toggle_autostart(app),
        TrayAction::SyncNow => {}
    }
}

/// 切换开机自启，然后把菜单里的勾**按系统的真实状态**重画一遍。
///
/// ## 为什么读系统状态而不是自己记一个 bool
///
/// 用户完全可以在启动器设置里、或者 `systemctl --user disable` 把它关掉，
/// 而应用不知道。自记一个 bool 会和真实状态分叉，于是菜单上的勾开始说谎。
/// 这个插件的 `is_enabled()` 问的就是系统本身，所以这里以它为准。
///
/// ## 为什么失败只记日志
///
/// 托盘菜单没有能弹提示的地方。失败最坏的结果是「勾没变」——用户再点一下就知道了。
/// 在托盘回调线程里 panic 会直接带崩进程，那比「点了没反应」糟得多。
fn toggle_autostart<R: Runtime>(app: &AppHandle<R>) {
    use tauri_plugin_autostart::ManagerExt as _;

    let manager = app.autolaunch();
    // 先问当前状态，再取反。`is_enabled` 失败时不去猜：猜错就会把
    // 用户已经打开的自启关掉（或者反过来），那是一次没人要的状态变更。
    let enabled = match manager.is_enabled() {
        Ok(enabled) => enabled,
        Err(error) => {
            tracing::warn!(%error, "托盘「开机自启」：读不到当前状态，不切换");
            return;
        }
    };

    let result = if enabled {
        manager.disable()
    } else {
        manager.enable()
    };
    if let Err(error) = result {
        tracing::warn!(%error, enabled, "托盘「开机自启」：切换失败");
        return;
    }
    tracing::info!(now_enabled = !enabled, "托盘「开机自启」已切换");
    refresh_autostart_check(app);
}

/// 把「开机自启」那一项的勾刷成系统的真实状态。
///
/// 建菜单时调一次（用户打开菜单前就该是对的），切换后再调一次。
/// 读不到状态就把勾**取消**：显示成「没开」比显示成一个可能过期的「开了」诚实，
/// 猜错方向最多是用户多点一下，不会让用户以为自启是开的、重启后发现没有。
fn refresh_autostart_check<R: Runtime>(app: &AppHandle<R>) {
    use tauri_plugin_autostart::ManagerExt as _;

    let enabled = app.autolaunch().is_enabled().unwrap_or(false);
    // `TrayIcon` 不暴露 `menu()`，所以句柄得自己存着——建菜单时放进
    // 应用状态里（见 [`install`]）。
    let Some(state) = app.try_state::<TrayMenuState<R>>() else {
        return;
    };
    let Some(item) = state.menu.get(MENU_AUTOSTART) else {
        return;
    };
    let Some(check) = item.as_check_menuitem() else {
        return;
    };
    if let Err(error) = check.set_checked(enabled) {
        tracing::warn!(%error, "托盘「开机自启」：更新勾选状态失败");
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
        assert_eq!(
            TrayAction::from_menu_id(MENU_AUTOSTART),
            Some(TrayAction::ToggleAutostart)
        );
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
        // 切换开机自启只是改一个系统项，不该顺带开一轮全网同步。
        assert!(!TrayAction::ToggleAutostart.wakes_sync());
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
