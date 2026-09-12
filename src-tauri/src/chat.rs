//! 内嵌的 DSH 聊天视图管理。
//!
//! # 架构：多 WebView（同一窗口内的两个 WebView）
//!
//! ```text
//! ┌─ Window "main" ──────────────────────────────┐
//! │  ┌─ Webview "shell" (React 控制台) ────────┐ │
//! │  │  状态机可视化 / 诊断面板 / 设置 / 日志   │ │  ← 永不导航
//! │  └────────────────────────────────────────┘ │
//! │  ┌─ Webview "chat" (DSH webchat) ──────────┐ │
//! │  │  http://127.0.0.1:3080/?token=...       │ │  ← 仅在就绪后创建
//! │  └────────────────────────────────────────┘ │
//! └──────────────────────────────────────────────┘
//!
//! # 为什么不是"把主 WebView 导航到 DSH 地址"
//!
//! 参考项目就是这么做的，代价是：**一旦导航，React 应用就被卸载**，
//! 于是诊断/日志/重试入口全部消失。用户在"白屏/404"时恰恰最需要这些入口，
//! 却一个都点不到——只能关掉整个程序重来。这是它最严重的可用性缺陷。
//!
//! 本项目把两者拆成同窗口内的两个独立 WebView：
//! - `shell` 始终是本地 React 应用，**任何情况下都不会被导航走**；
//! - `chat` 独立加载 DSH 页面，出问题只影响它自己，控制台照常可用。
//!
//! 诊断面板打开时调用 [`hide`] 把 chat 隐藏，面板关闭再 [`show`] ——
//! 两个 WebView 在同一窗口内互不干扰地切换，用户感知仍是"一个窗口"。
//!
//! # 为什么不用 iframe
//!
//! 实测 DSH 响应头没有 `X-Frame-Options` 也没有 CSP，技术上允许 iframe。
//! 但 iframe 里 `127.0.0.1:3080` 相对 `tauri.localhost` 属于**第三方上下文**，
//! Chromium 默认拦截第三方 Cookie，DSH 的登录态会丢。
//! 子 WebView 是真正的顶层浏览上下文，完全没有这个问题。
//!
//! # Windows 注意事项
//!
//! Tauri 文档明确提示：在**同步命令或事件处理程序**中创建 WebView 会导致死锁
//! （WebView2 的限制）。因此本模块的所有创建动作都必须从**异步命令**或
//! 独立线程调用，绝不能放在同步 `#[tauri::command]` 里。

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::webview::WebviewBuilder;
use tauri::{LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindowBuilder};

/// 聊天 WebView 的标签
pub const CHAT_LABEL: &str = "chat";
/// 主窗口标签
pub const MAIN_WINDOW: &str = "main";

/// 聊天视图的尺寸与位置（逻辑像素）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    /// 尺寸是否可用（宽高都得有实际面积）
    pub fn is_usable(&self) -> bool {
        self.width >= 1.0 && self.height >= 1.0
    }
}

/// 聊天视图状态
#[derive(Default)]
struct ChatState {
    /// 前端报告的显示区域
    bounds: Option<Bounds>,
    /// 当前已加载的 URL
    loaded_url: Option<String>,
    /// 是否处于隐藏状态
    hidden: bool,
    /// 窗口内嵌入**连续失败**了几次（成功一次即清零）
    embed_failures: u32,
}

/// 连续失败多少次之后才彻底放弃窗口内嵌入。
///
/// 为什么不"失败一次就永久降级"：`add_child` 会因**瞬时**原因失败
/// （窗口尚未就绪、WebView2 正在初始化），一次偶发失败就锁死整个会话，
/// 用户会莫名其妙地一直看到一个独立窗口。但也不能无限重试 ——
/// 真不可用时每次加载都白试一遍、还刷一条警告日志，所以给个小上限。
const EMBED_RETRY_LIMIT: u32 = 3;

/// 聊天视图管理器（作为 Tauri 托管状态）
#[derive(Default)]
pub struct ChatView {
    inner: Mutex<ChatState>,
}

impl ChatView {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ChatState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 前端上报显示区域
    pub fn set_bounds(&self, b: Bounds) {
        self.lock().bounds = Some(b);
    }

    /// 记录加载过的 URL
    fn remember(&self, url: &str) {
        self.lock().loaded_url = Some(url.to_string());
    }

    /// 当前是否已加载过页面
    pub fn has_content(&self) -> bool {
        self.lock().loaded_url.is_some()
    }
}

/// 只放行 http/https 交给系统浏览器。
///
/// # 为什么真正的关口在这里
///
/// `NewWindowResponse::Deny` 挡的是「在应用内开一个新窗口」，
/// **它并不拦截 URL 本身** —— 返回 `Deny` 之前我们已经把 URL 交给了系统 opener
/// （Windows 上最终落到 `ShellExecute`）。而 DSH 页面加载的是本机服务返回的
/// 内容，一旦被注入（第三方插件、XSS），一句
/// `window.open('file:///C:/Windows/System32/calc.exe')` 就能拉起本机程序；
/// 自定义协议同理 —— 注册表里任何一个协议处理器都是一条入口。
///
/// [`crate::inject`] 的注入脚本里也有同样的判断，但那只覆盖**用户点击链接**这条路；
/// 页面直接调 `window.open` 会绕过去。所以必须在 `on_new_window` 再拦一道。
///
/// 注意 `tauri::Url` 已做过规范化：`JaVaScRiPt:` 这类大小写变形在
/// `scheme()` 里已经是小写，直接比较即可。
fn is_external_web_url(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// 在窗口内创建（或复用）聊天 WebView 并导航到给定 URL。
///
/// **必须从异步上下文调用**（见模块文档的 Windows 说明）。
///
/// 若多 WebView 创建失败，会**降级**为打开一个独立的聊天窗口，
/// 保证功能可用（宁可多一个窗口，也不能让用户面对空白界面）。
pub fn load(app: &tauri::AppHandle, chat: &ChatView, url: &str) -> Result<(), String> {
    let parsed: tauri::Url = url.parse().map_err(|e| format!("URL 无效：{e}"))?;
    // 落日志前先给 token 打码：日志会被导出分享，完整 token 等于访问凭证
    let shown = crate::token::mask_url(url);

    // 连续失败到上限才固定走独立窗口（见 EMBED_RETRY_LIMIT）
    if chat.lock().embed_failures >= EMBED_RETRY_LIMIT {
        crate::logging::info("正在以独立窗口加载 DSH 页面", shown.clone());
        return navigate_detached(app, &parsed);
    }

    // 已存在则直接导航（复用同一个 WebView，避免反复创建造成的资源泄漏）
    if let Some(wv) = app.get_webview(CHAT_LABEL) {
        wv.navigate(parsed.clone())
            .map_err(|e| format!("导航失败：{e}"))?;
        chat.remember(url);
        if !chat.lock().hidden {
            let _ = wv.show();
        }
        crate::logging::info("聊天视图已重新导航", shown);
        return Ok(());
    }

    let window = app
        .get_window(MAIN_WINDOW)
        .ok_or_else(|| "主窗口不存在".to_string())?;

    let bounds = chat.lock().bounds.unwrap_or(Bounds {
        x: 0.0,
        y: 0.0,
        width: 800.0,
        height: 600.0,
    });

    // 注入内容取决于用户的 GitHub 镜像开关，因此在建视图前先读一次配置
    let gh_mirror = app
        .try_state::<crate::shell::Shell>()
        .map(|s| s.settings().gh_mirror)
        .unwrap_or(true);

    // `on_new_window` 的闭包要求 `'static`，所以这里把句柄克隆进去。
    // 这个回调负责把页面里的外链交给系统浏览器：
    // 返回 `Deny` 是刻意的——绝不允许 DSH 页面在应用内弹出新窗口，
    // 否则用户的浏览上下文会被一个没有地址栏、退不回去的窗口劫走。
    let opener_app = app.clone();

    let builder = WebviewBuilder::new(CHAT_LABEL, WebviewUrl::External(parsed.clone()))
        // 链接右键菜单 +（可选的）GitHub 镜像，只注入给聊天视图，
        // 因此不需要 origin 守卫。详见 `inject` 模块文档。
        .initialization_script(crate::inject::scripts(gh_mirror))
        .on_new_window(move |url, _features| {
            // 先过协议校验再交给系统浏览器（见 is_external_web_url）
            if !is_external_web_url(&url) {
                crate::logging::warn(
                    "已拦截非网页协议的外链",
                    format!(
                        "{}（只允许 http/https）",
                        crate::token::mask_url(&url.to_string())
                    ),
                );
                return tauri::webview::NewWindowResponse::Deny;
            }
            let app = opener_app.clone();
            let url = url.to_string();
            tauri::async_runtime::spawn(async move {
                use tauri_plugin_opener::OpenerExt;
                if let Err(e) = app.opener().open_url(url.clone(), None::<&str>) {
                    crate::logging::warn("打开外部链接失败", format!("{e}（{url}）"));
                }
            });
            tauri::webview::NewWindowResponse::Deny
        })
        // 页面加载的成败同样要落日志：模块文档里说的"白屏"如果只是
        // 不显示、却没有任何记录，排障就只能靠猜。
        // 这里 URL 先打码，日志会被导出。
        .on_page_load(|_wv, payload| {
            let url = crate::token::mask_url(&payload.url().to_string());
            match payload.event() {
                tauri::webview::PageLoadEvent::Started => {
                    crate::logging::info("DSH 页面开始加载", url)
                }
                tauri::webview::PageLoadEvent::Finished => {
                    crate::logging::info("DSH 页面加载完成", url)
                }
            }
        });

    match window.add_child(
        builder,
        LogicalPosition::new(bounds.x, bounds.y),
        LogicalSize::new(bounds.width, bounds.height),
    ) {
        Ok(wv) => {
            // 成功一次就清零：上一次的瞬时失败不该继续影响后续加载
            chat.lock().embed_failures = 0;
            chat.remember(url);
            // 若当前诊断面板是打开的，创建后立即隐藏，避免遮挡
            if chat.lock().hidden {
                let _ = wv.hide();
            }
            // 这几行是刻意加的：WebView 创建成功与否必须留下痕迹。
            // 参考项目在"白屏"时日志里什么都没有，只能靠猜。
            crate::logging::info(
                "已创建聊天视图并开始加载 DSH 页面",
                format!(
                    "{} @ ({:.0},{:.0}) {:.0}×{:.0}",
                    shown, bounds.x, bounds.y, bounds.width, bounds.height
                ),
            );
            Ok(())
        }
        Err(e) => {
            // 降级：多 WebView 不可用时改用独立窗口。
            // 失败计数而不是布尔锁 —— 只有连续失败到上限才彻底放弃窗口内嵌入
            // （见 EMBED_RETRY_LIMIT），否则一次瞬时失败会锁死整个会话。
            let failed = {
                let mut st = chat.lock();
                st.embed_failures = st.embed_failures.saturating_add(1);
                st.embed_failures
            };
            crate::logging::warn(
                "无法在窗口内嵌入页面，本次改用独立窗口显示",
                format!(
                    "{e}（第 {failed}/{EMBED_RETRY_LIMIT} 次，达到上限后将固定使用独立窗口）"
                ),
            );
            let r = navigate_detached(app, &parsed);
            if r.is_ok() {
                crate::logging::info("已用独立窗口加载 DSH 页面", shown);
            }
            r
        }
    }
}

/// 降级路径：把 DSH 页面放进一个独立窗口
///
/// 脚本注入与 `on_new_window` 在这里要**再配一遍**：
/// 降级窗口中同样是 DSH 页面，用户期待的行为没有理由不同。
/// （参考项目只在一处配置，日后再加一个入口就很容易漏掉。）
fn navigate_detached(app: &tauri::AppHandle, url: &tauri::Url) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("dsh-chat") {
        w.navigate(url.clone())
            .map_err(|e| format!("导航失败：{e}"))?;
        let _ = w.set_focus();
        return Ok(());
    }

    let gh_mirror = app
        .try_state::<crate::shell::Shell>()
        .map(|s| s.settings().gh_mirror)
        .unwrap_or(true);
    let opener_app = app.clone();

    WebviewWindowBuilder::new(app, "dsh-chat", WebviewUrl::External(url.clone()))
        .title("DSH")
        .inner_size(1100.0, 800.0)
        .initialization_script(crate::inject::scripts(gh_mirror))
        .on_new_window(move |url, _features| {
            // 与主路径同样的协议校验与失败记录（见 is_external_web_url）。
            // 降级窗口里同样是 DSH 页面，安全边界没有理由更松。
            if !is_external_web_url(&url) {
                crate::logging::warn(
                    "已拦截非网页协议的外链",
                    format!(
                        "{}（只允许 http/https）",
                        crate::token::mask_url(&url.to_string())
                    ),
                );
                return tauri::webview::NewWindowResponse::Deny;
            }
            let app = opener_app.clone();
            let url = url.to_string();
            tauri::async_runtime::spawn(async move {
                use tauri_plugin_opener::OpenerExt;
                if let Err(e) = app.opener().open_url(url.clone(), None::<&str>) {
                    crate::logging::warn("打开外部链接失败", format!("{e}（{url}）"));
                }
            });
            tauri::webview::NewWindowResponse::Deny
        })
        .build()
        .map_err(|e| format!("打开独立窗口失败：{e}"))?;
    Ok(())
}

/// 显示聊天视图（诊断面板关闭时调用）
pub fn show(app: &tauri::AppHandle, chat: &ChatView) {
    chat.lock().hidden = false;
    if let Some(wv) = app.get_webview(CHAT_LABEL) {
        let _ = wv.show();
    }
}

/// 隐藏聊天视图（诊断面板打开时调用）。
///
/// 这样 React 侧的面板能完整可见，而不必去和子 WebView 争 z 序
/// （子 WebView 是原生控件，永远盖在 DOM 之上，靠 CSS z-index 是压不住的）。
pub fn hide(app: &tauri::AppHandle, chat: &ChatView) {
    chat.lock().hidden = true;
    if let Some(wv) = app.get_webview(CHAT_LABEL) {
        let _ = wv.hide();
    }
}

/// 更新聊天视图的位置与尺寸（窗口缩放时前端会重报）
pub fn set_bounds(app: &tauri::AppHandle, chat: &ChatView, b: Bounds) {
    chat.set_bounds(b);
    if let Some(wv) = app.get_webview(CHAT_LABEL) {
        let _ = wv.set_position(LogicalPosition::new(b.x, b.y));
        let _ = wv.set_size(LogicalSize::new(b.width, b.height));
    }
}

/// 关闭并销毁聊天视图（后端失败/重启时调用，避免残留一个陈旧的页面）
pub fn destroy(app: &tauri::AppHandle, chat: &ChatView) {
    if let Some(wv) = app.get_webview(CHAT_LABEL) {
        let _ = wv.close();
    }
    if let Some(w) = app.get_webview_window("dsh-chat") {
        let _ = w.close();
    }
    let mut st = chat.lock();
    st.loaded_url = None;
    st.hidden = false;
    // 后端重启/失败后重新来过：给窗口内嵌入一个新的机会
    st.embed_failures = 0;
}

/// 重新加载聊天视图
pub fn reload(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(wv) = app.get_webview(CHAT_LABEL) {
        // 用 Tauri 原生的 `reload()`，而不是 `eval("location.reload()")`：
        // 后者依赖页面里 JS 正常运行，而"页面卡死"正是需要重新加载的典型场景。
        wv.reload().map_err(|e| format!("重新加载失败：{e}"))?;
        return Ok(());
    }
    if let Some(w) = app.get_webview_window("dsh-chat") {
        w.reload().map_err(|e| format!("重新加载失败：{e}"))?;
        return Ok(());
    }
    Err("当前没有已加载的页面".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_usability() {
        assert!(Bounds {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0
        }
        .is_usable());
        // 宽度为 0（例如面板占满全宽）时不应去创建 WebView
        assert!(!Bounds {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 50.0
        }
        .is_usable());
    }

    #[test]
    fn new_chat_view_has_no_content() {
        let c = ChatView::new();
        assert!(!c.has_content());
    }

    #[test]
    fn set_bounds_is_recorded() {
        let c = ChatView::new();
        let b = Bounds {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
        };
        c.set_bounds(b);
        assert_eq!(c.lock().bounds, Some(b));
    }

    #[test]
    fn remember_stores_url() {
        let c = ChatView::new();
        c.remember("http://127.0.0.1:3080/?token=x");
        assert!(c.has_content());
    }

    /// 把"哪些协议可以交给系统浏览器"钉死。
    ///
    /// 这是**安全边界**而不是格式偏好：放行 `file:` 就等于允许 DSH 页面
    /// 拉起本机任意可执行文件，放行自定义协议则等于把注册表里所有
    /// 协议处理器都变成页面的可调用入口。改动这里必须有意识。
    #[test]
    fn only_web_schemes_reach_the_system_browser() {
        let check = |s: &str| {
            let url: tauri::Url = s.parse().unwrap_or_else(|e| panic!("{s} 应当能解析：{e}"));
            is_external_web_url(&url)
        };

        // 正常外链
        assert!(check("http://127.0.0.1:3080/"));
        assert!(check("https://github.com/deepseek-ai/dsh"));

        // 以下一旦放行，页面就能驱动本机
        assert!(!check("file:///C:/Windows/System32/calc.exe"));
        assert!(!check("mailto:someone@example.com"));
        assert!(!check("javascript:alert(1)"));

        // 大小写变形同样拦下（Url 会把 scheme 规范化成小写）
        assert!(!check("FILE:///C:/Windows/notepad.exe"));
    }

    /// 一次瞬时失败不该把整个会话锁死成"独立窗口"。
    ///
    /// 原先是个布尔标志，`add_child` 失败一次就永久置位，用户此后一直
    /// 莫名其妙看到独立窗口。改成计数后，这里把语义钉住。
    #[test]
    fn a_single_failure_does_not_lock_the_layout() {
        let c = ChatView::new();
        assert_eq!(c.lock().embed_failures, 0, "新建视图不应处于降级状态");

        // 模拟一次 add_child 失败
        c.lock().embed_failures += 1;
        assert!(
            c.lock().embed_failures < EMBED_RETRY_LIMIT,
            "失败一次就达到上限，等于退回了「一次失败即永久降级」的旧行为"
        );
        assert!(
            EMBED_RETRY_LIMIT >= 2,
            "上限必须 >= 2，否则计数没有任何意义"
        );
    }
}
