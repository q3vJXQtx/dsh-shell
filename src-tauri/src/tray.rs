//! 托盘图标与右键菜单。
//!
//! # 为什么托盘是这个项目的必需品，而不是"锦上添花"
//!
//! 壳程序的核心价值是**在后端出问题时依然可用**（见 [`crate::chat`] 的模块文档）。
//! 但窗口一旦被关掉或最小化，这些能力就全都够不着了；而且设置里
//! `close_to_tray` 的默认值本来就是 `true`——也就是说"关闭窗口 = 藏进托盘"
//! 是既定设计，没有托盘就会出现**关掉之后再也叫不回来**的死局。
//!
//! 因此这里必须提供三样东西，缺一不可：
//!
//! 1. **托盘图标本身** —— 取应用图标（[`Manager::default_window_icon`]），
//!    取不到就明确记一条 warn，而不是静默留个透明图标让用户以为程序没起来；
//! 2. **右键菜单** —— 常用操作不必先开窗口；
//! 3. **左键单击唤回窗口** —— 这是用户被"藏起来"之后的第一反应。
//!
//! 菜单项里的「显示主窗口」放在最上面也是刻意的：托盘最容易踩的坑就是
//! "窗口没了、不知道从哪儿找回"，把它放在第一条对应用户的直觉。

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use crate::{logging, shell};

/// 托盘图标 id（用于后续取回句柄更新提示文字）
const TRAY_ID: &str = "dsh-shell-tray";

/// 前端监听的事件名：请求打开某个面板
pub const OPEN_PANEL_EVENT: &str = "shell://open-panel";

/// 菜单项 id
mod id {
    pub const SHOW: &str = "show";
    pub const OPEN_BROWSER: &str = "open-browser";
    pub const RESTART_ALL: &str = "restart-all";
    pub const RESTART: &str = "restart";
    pub const STOP: &str = "stop";
    pub const DIAGNOSTICS: &str = "diagnostics";
    pub const CHECK_UPDATE: &str = "check-update";
    pub const SETTINGS: &str = "settings";
    pub const QUIT: &str = "quit";
}

/// 创建托盘图标与右键菜单。
///
/// 由 `setup` 调用一次。重复调用会创建出第二个托盘图标，
/// 因此这里用固定 id，并由 Tauri 保证同 id 只有一个。
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    // ---- 菜单 --------------------------------------------------------------
    let item = |mid: &str, text: &str, enabled: bool| {
        MenuItem::with_id(app, mid, text, enabled, None::<&str>)
    };

    let show = item(id::SHOW, "显示主窗口", true)?;
    let open_browser = item(id::OPEN_BROWSER, "用浏览器打开", true)?;
    let restart_all = item(id::RESTART_ALL, "重启前后端", true)?;
    let restart = item(id::RESTART, "重启后端", true)?;
    let stop = item(id::STOP, "停止后端", true)?;
    let diagnostics = item(id::DIAGNOSTICS, "诊断…", true)?;
    // 名字里带「DSH」是刻意的：它检查的是 DSH 本体的版本，
    // 不是壳自己。参考项目这个菜单项叫「检查更新(DSH)」但实际查的是壳的
    // GitHub Release —— 名实不符，这里不沿用。
    let check_update = item(id::CHECK_UPDATE, "检查 DSH 更新", true)?;
    let settings = item(id::SETTINGS, "设置…", true)?;
    let quit = item(id::QUIT, "退出", true)?;

    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &show,
            &sep1,
            &restart_all,
            &restart,
            &stop,
            &open_browser,
            &sep2,
            &diagnostics,
            &check_update,
            &settings,
            &sep3,
            &quit,
        ],
    )?;

    // ---- 图标本体 ----------------------------------------------------------
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("DSH Shell")
        .menu(&menu)
        // 左键单击用来唤回窗口，右键（或长按）才弹菜单：
        // 这是 Windows 托盘应用的通行习惯，反过来的话用户会频繁误触菜单。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| dispatch(app, event.id().as_ref()))
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

    match app.default_window_icon().cloned() {
        Some(icon) => builder = builder.icon(icon),
        // 明确告警而不是静默继续：托盘图标不可见时，
        // 用户会以为"程序没启动"，而实际原因只是拿不到图标资源。
        None => logging::warn(
            "没有可用的应用图标，托盘图标可能不可见",
            "请确认 tauri.conf.json 的 bundle.icon 与 icons/ 目录",
        ),
    }

    builder.build(app)?;
    logging::info("托盘图标已创建", "左键单击唤回窗口，右键打开菜单");
    Ok(())
}

/// 处理菜单点击
fn dispatch(app: &AppHandle, menu_id: &str) {
    match menu_id {
        id::SHOW => show_main_window(app),

        id::OPEN_BROWSER => {
            // 失败时把窗口叫回来，否则用户点完毫无反应、也不知道为什么
            if let Err(e) = crate::open_browser_url(app) {
                logging::warn("从托盘打开浏览器失败", e.clone());
                show_main_window(app);
            }
        }

        id::RESTART_ALL => {
            show_main_window(app);
            // 先刷新壳的控制台页面（React 状态一并重置）；
            // 后端就绪后聊天视图会由既有流程自动重建并加载新 token 的页面。
            if let Err(e) = crate::chat::reload(app) {
                logging::warn("重启前后端：刷新页面失败（继续重启后端）", e);
            }
            spawn_backend_restart(app.clone(), true);
        }

        id::RESTART => {
            show_main_window(app);
            spawn_backend_restart(app.clone(), true);
        }

        id::STOP => {
            crate::stop_backend_impl(app);
        }

        id::DIAGNOSTICS => open_panel(app, "diagnostics"),
        id::SETTINGS => open_panel(app, "settings"),

        id::CHECK_UPDATE => {
            // 检查本身是异步的（要走网络、还可能挨个试代理），结果通过
            // `shell://update` 事件推给前端。所以这里必须**先把窗口和设置面板
            // 调出来**——否则用户点了菜单，几秒钟内看不到任何反应，
            // 会以为「点了没用」然后再点一次。
            open_panel(app, "settings");
            crate::update::check_now(app.clone());
        }

        id::QUIT => {
            logging::info("接到托盘退出请求", "");
            crate::cleanup_on_exit(app);
            app.exit(0);
        }

        other => logging::warn("收到未处理的托盘菜单项", other.to_string()),
    }
}

/// 显示并把主窗口拉到前台。
///
/// 三个调用缺一不可：`show` 负责从"隐藏到托盘"恢复，
/// `unminimize` 负责从"最小化"恢复，`set_focus` 让它真正到最前面——
/// 只调用其中一个都会出现"点了没反应"的观感。
pub fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_window(crate::chat::MAIN_WINDOW) else {
        logging::warn("找不到主窗口，无法显示", "");
        return;
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// 请前端打开某个面板（诊断 / 设置）。
///
/// 面板状态由 React 管理，后端不直接干预 DOM；
/// 这里只发一个事件，前端收到后置位即可。
fn open_panel(app: &AppHandle, panel: &str) {
    show_main_window(app);
    if let Err(e) = app.emit(OPEN_PANEL_EVENT, panel) {
        logging::warn("通知前端打开面板失败", e.to_string());
    }
}

/// 把后端启动/重启挪到异步运行时执行。
///
/// [`shell::start_backend`] 会创建/销毁 WebView 与子进程——`lib.rs` 里
/// 对应的 Tauri 命令特意标了 `async` 就是因为这些操作**会阻塞 UI**，
/// 而托盘菜单事件回调恰恰跑在主线程上。直接同步调用会让菜单点击
/// 之后整个窗口卡住几秒（后端冷启动要等端口和 token）。
fn spawn_backend_restart(app: AppHandle, restart: bool) {
    tauri::async_runtime::spawn(async move {
        shell::start_backend(&app, restart);
    });
}

/// 托盘是否真的建起来了。
///
/// 判断"关闭窗口时要不要藏进托盘"必须先问这一句：
/// 托盘不可用却把窗口藏起来，用户就**再也叫不回窗口**了
/// —— 这比"关不干净"严重得多。
pub fn is_available(app: &AppHandle) -> bool {
    app.tray_by_id(TRAY_ID).is_some()
}

/// 把后端状态同步到托盘提示文字。
///
/// 悬停看一眼就知道后端是"已就绪"还是"启动失败"，不必先把窗口叫出来。
///
/// 可以安全地在后台线程调用：Tauri 内部的 `set_tooltip` 会自行
/// 把操作转投到主线程（`run_item_main_thread!`），
/// 而状态变更恰恰大都发生在启动/自愈的 worker 线程上。
pub fn set_status(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(format!("DSH Shell · {text}")));
    }
}
