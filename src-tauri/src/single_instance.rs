//! 单实例保护。
//!
//! # 为什么这个项目必须有它
//!
//! 托盘 + `close_to_tray`（默认开启，见 [`crate::tray`]）带来一个很自然的
//! 使用序列：用户点关闭 → 窗口藏进托盘 → **过一会儿忘了，又从开始菜单点了一次**。
//!
//! 没有单实例保护的话，第二个进程会：
//!
//! 1. 发现 3080 上已经有 DSH 在跑，且"不是本程序启动的"（那是第一个实例的）；
//! 2. 于是提示用户"接管"，或者自己另起一个实例去抢；
//! 3. 两个壳各自把对方的后端当成可疑进程，互相重启，越搞越乱。
//!
//! 所以第二次启动应该做的只有一件事：**把已经存在的那个窗口叫回来，然后自己退出**。
//!
//! # 实现选择
//!
//! 用 Win32 具名互斥体（`CreateMutexW`）而不是引入
//! `tauri-plugin-single-instance`：后者要额外拉一个 crate，
//! 而我们只需要"判断是否已有实例"这一件事，`windows-sys` 已经在依赖里了。
//!
//! 唤回窗口用 `FindWindowW` 按窗口标题查找。之所以敢用标题匹配：
//! 本程序的窗口标题恒为 `DSH`（用户无法改名），且它必须是**顶层窗口**——
//! 聊天视图是子窗口，不会被误匹配到。找不到时退化为一个说明性弹窗，
//! 绝不静默退出（静默退出会让用户以为程序坏了）。

use crate::logging;

/// 互斥体名称。
///
/// `Local\` 前缀表示**按登录会话**隔离：多用户同时登录同一台机器时，
/// 各自可以跑一个实例，这正是我们想要的语义。
#[cfg(windows)]
const MUTEX_NAME: &str = "Local\\DSH-Shell-SingleInstance";

/// 主窗口标题（与 `lib.rs` 里创建窗口时用的标题必须一致）
const WINDOW_TITLE: &str = "DSH";

/// 尝试取得单实例所有权。
///
/// 返回 `true` 表示我们是**第一个**实例，可以继续启动；
/// 返回 `false` 表示已有实例在运行（此时应当直接把窗口叫回来并退出）。
#[cfg(windows)]
pub fn acquire() -> bool {
    use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: 传空的安全属性指针表示使用默认安全性；
    // 名字是合法的以 NUL 结尾的 UTF-16 串；返回值只用于判重与保持存活。
    unsafe {
        // 第二个参数为 0：不要求立即持有所有权。我们要的只是"这个名字是否存在"。
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            // 连互斥体都建不出来（极少见）：不能因为保护机制本身失败
            // 就拒绝启动，放行并记一条日志。
            logging::warn("单实例互斥体创建失败，将按多实例运行", "");
            return true;
        }

        if windows_sys::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
            // 已有实例。**不关闭** handle —— 我们马上就要退出了，
            // 关掉它反而会误释放别人的实例标识。
            return false;
        }

        // 拿到所有权。
        //
        // 句柄**故意不关闭**：它必须活到进程结束——一旦 CloseHandle，
        // 互斥体就被释放，别的进程又能起一个实例，保护也就失效了。
        // 这里无需 `mem::forget`：`HANDLE` 是 Copy 的裸值、不实现 Drop，
        // 不保存它并不会导致任何资源被回收。
        true
    }
}

/// 非 Windows 平台不做限制（本项目实际只支持 Windows）。
#[cfg(not(windows))]
pub fn acquire() -> bool {
    true
}

/// 把已有实例的窗口唤到前台；实在找不到就用弹窗说明。
#[cfg(windows)]
pub fn focus_existing() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONINFORMATION, MB_OK,
        MB_SETFOREGROUND, MB_TOPMOST, SW_RESTORE,
    };

    let title: Vec<u16> = WINDOW_TITLE.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: 类名传空表示"不限制类名"；标题是以 NUL 结尾的 UTF-16 串。
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };

    if !hwnd.is_null() {
        // SW_RESTORE 一次搞定"最小化"和"藏在托盘"两种恢复场景
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
        logging::info("已唤回正在运行的窗口", "本进程随即退出");
        return;
    }

    // 找不到窗口（可能正卡在启动早期）：把情况说清楚，不要静默退出。
    let text: Vec<u16> = "DSH Shell 已经在运行。\n\n如果看不到窗口，请点击任务栏右下角托盘区域的 DSH 图标把它唤回。"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let caption: Vec<u16> = "DSH Shell"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
    logging::info("检测到已有实例在运行，本进程退出", "未能定位到已有窗口");
}

#[cfg(not(windows))]
pub fn focus_existing() {}
