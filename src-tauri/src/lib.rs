//! DSH Shell —— 面向"稳定性与可诊断性"的 DSH 桌面外壳。
//!
//! # 与参考项目（dsh-local-shell v2.0.3）的核心差异
//!
//! | 维度 | 参考项目 | 本项目 |
//! |---|---|---|
//! | 状态管理 | 4 个分散的原子变量 | 单一状态机 + 代际号（[`shell`]）|
//! | 陈旧锁 | **零处理**，用户撞上就是 404 | 启动前自动探测 + 可逆清理（[`selfheal`]）|
//! | 就绪判定 | 任何 401/404/2xx 都算就绪 | 区分 DSH / 代理 / 未知服务（[`probe`]）|
//! | token 抓取 | 精确子串匹配，失败静默回退裸 URL | 放宽锚点 + 失败时明确报错（[`token`]）|
//! | 诊断入口 | 主 WebView 导航走后**整个消失** | 独立 WebView，**永不消失**（[`chat`]）|
//! | 配置损坏 | 未处理 | 自动备份 + 回退默认值（[`settings`]）|
//! | `noOpen` 缓存 | 按路径缓存，**永不过期** | 缓存键含版本，升级自动重探 |
//! | 代码组织 | 单文件 2030 行 | 按职责分模块 |
//! | 单元测试 | **0 个** | 80+ 个（集中在纯逻辑：token/版本/候选链/路径）|
//! | 会话完成通知 | 只查 `is_visible()`，**最小化时漏报** | 可见 + 未最小化 + 前台三者同判（[`notify`]）|
//! | 外链右键菜单 | 注入脚本硬编码 `3080`，换端口静默失效 | 只注入聊天视图，无需守卫（[`inject`]）|
//! | 更新检查 | 查壳自己的 GitHub Release（需自建仓库才生效）| 查 DSH 本体的 npm 版本，开箱即用（[`update`]）|
//!
//! # 关键约定
//!
//! `unsafe` 只允许出现在两处不可避免的 Win32 直调上：日志取本地时间
//! （[`logging`]）和单实例命名互斥体（[`single_instance`]）。
//! 其余平台能力一律走 Tauri 的封装。
//!
//! （原先这里写的是"本 crate 是 `#![forbid(unsafe_code)]` 的（除某处外）"，
//! 那句话自相矛盾——`forbid` 不存在例外；而且 crate 里本来也没写这个属性。）

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod chat;
pub mod inject;
pub mod lifecycle;
pub mod logging;
pub mod notify;
pub mod probe;
pub mod selfheal;
pub mod settings;
pub mod shell;
pub mod single_instance;
pub mod state;
pub mod token;
pub mod tray;
pub mod update;

use std::path::PathBuf;

use serde::Serialize;
use tauri::webview::WebviewBuilder;
use tauri::{AppHandle, Emitter, LogicalPosition, Manager, State, WebviewUrl};

pub use chat::{Bounds, ChatView};
pub use logging::LogEntry;
pub use settings::Settings;
pub use shell::Shell;
pub use state::BackendState;

/// 主窗口内承载 React 控制台的 WebView 标签
const SHELL_WEBVIEW: &str = "shell";

/// 一次性返回给前端的全量快照。
///
/// 刻意做成**单个命令返回全部**而非多个命令并发请求：
/// 前端首屏只需一次 IPC 往返，也避免了"状态与日志来自不同时刻"的错配。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// 后端状态机当前状态
    pub state: BackendState,
    /// 用户可读的日志（最新的在最后）
    pub logs: Vec<LogEntry>,
    /// 配置副本
    pub settings: Settings,
    /// 端口上是否已经加载过聊天页面
    pub chat_loaded: bool,
    /// 后端已就绪时的访问地址（前端据此恢复，避免错过 ready 事件）
    pub ready_url: Option<String>,
    /// 壳自身版本
    pub version: String,
    /// 数据目录（日志/配置所在处，便于用户找到）
    pub data_dir: String,
}

/// 组装快照
fn snapshot_of(app: &AppHandle) -> Snapshot {
    let shell = app.state::<Shell>();
    let chat = app.state::<ChatView>();
    Snapshot {
        state: shell.snapshot(),
        logs: logging::all(),
        settings: shell.settings(),
        chat_loaded: chat.has_content(),
        ready_url: shell.ready_url(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: shell.data_dir().display().to_string(),
    }
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 获取全量快照
#[tauri::command]
fn get_snapshot(app: AppHandle) -> Snapshot {
    snapshot_of(&app)
}

/// 启动或重启后端。
///
/// 标为 `async`：命令体里会创建/销毁 WebView 与进程，
/// 而 Tauri 的同步命令运行在主线程上，做这些事会阻塞 UI。
#[tauri::command]
async fn start_backend(app: AppHandle, restart: bool) {
    shell::start_backend(&app, restart);
}

/// 只导出诊断文本（不落盘），供界面直接展示
#[tauri::command]
fn export_diagnostics(app: AppHandle) -> String {
    let shell = app.state::<Shell>();
    let extra = diagnostics_env_summary(&app, &shell);
    logging::export_diagnostics(&extra)
}

/// 把诊断报告写入数据目录，返回文件路径。
///
/// 之所以要落盘：用户遇到问题时，最需要的是**能直接发出去的文件**，
/// 而不是让他从界面上手动全选复制一段长文本。
#[tauri::command]
fn save_diagnostics(app: AppHandle) -> Result<String, String> {
    let shell = app.state::<Shell>();
    let extra = diagnostics_env_summary(&app, &shell);
    let report = logging::export_diagnostics(&extra);

    let dir = shell.data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let name = format!("diagnostics-{}.txt", timestamp_compact());
    let path = dir.join(name);
    std::fs::write(&path, report).map_err(|e| format!("写入失败：{e}"))?;
    logging::info(
        "诊断报告已保存",
        path.display().to_string(),
    );
    Ok(path.display().to_string())
}

/// 诊断报告里的环境摘要段
fn diagnostics_env_summary(app: &AppHandle, shell: &Shell) -> String {
    let settings = shell.settings();
    let mut s = String::new();
    s.push_str(&format!("数据目录: {}\n", shell.data_dir().display()));
    s.push_str(&format!("监听端口: {}\n", settings.port));
    s.push_str(&format!("抑制自动打开浏览器: {}\n", settings.no_open));
    s.push_str(&format!("允许 npx 兜底: {}\n", settings.allow_npx));
    s.push_str(&format!(
        "用户指定 dsh 路径: {}\n",
        settings
            .custom_dsh_path
            .clone()
            .unwrap_or_else(|| "(未指定)".into())
    ));
    s.push_str(&format!(
        "no-open 探测缓存条目: {}\n",
        settings.no_open_support.len()
    ));
    // 这两项在排障时经常被忽略：「任务完成时没收到通知」很可能只是开关关了，
    // 「装插件失败」很可能只是镜像开着但对面不通。写在报告里省一轮来回问。
    s.push_str(&format!(
        "任务完成通知: {}\n",
        if settings.notify_on_finish {
            "已启用"
        } else {
            "已关闭"
        }
    ));
    s.push_str(&format!(
        "GitHub 加速镜像: {}\n",
        if settings.gh_mirror {
            "已启用"
        } else {
            "已关闭"
        }
    ));
    if let Some(t) = shell.current_token() {
        s.push_str(&format!("当前凭证: {}\n", token::mask_token(&t)));
    } else {
        s.push_str("当前凭证: (无)\n");
    }
    // 候选链回放：排障时最想知道"到底找到了哪些启动方式"
    let ctx = lifecycle::CandidateContext {
        port: settings.port,
        no_open: settings.no_open,
        custom_path: settings.custom_dsh_path.clone(),
        allow_npx: settings.allow_npx,
    };
    let cands = lifecycle::build_candidates(&ctx);
    s.push_str("\n候选启动方式:\n");
    for (i, c) in cands.iter().enumerate() {
        s.push_str(&format!("  {}. {} -> {}\n", i + 1, c.method, c.describe()));
    }
    if cands.is_empty() {
        s.push_str("  (无)\n");
    }
    // 聊天视图是否处于降级模式（独立窗口）
    let _ = app;
    s
}

/// 紧凑时间戳（用于诊断文件名）
fn timestamp_compact() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("{}", d.as_secs()),
        Err(_) => "unknown".into(),
    }
}

/// 读取配置
#[tauri::command]
fn get_settings(shell: State<'_, Shell>) -> Settings {
    shell.settings()
}

/// 保存配置。
///
/// 端口发生变化时会自动重启后端——否则用户改了端口却发现"没生效"，
/// 只能自己猜到要点一下重启。
#[tauri::command]
async fn update_settings(app: AppHandle, next: Settings) -> Result<(), String> {
    // 端口校验。
    //
    // 前端输入框有 `min=1` 兜着，但配置文件是可以手改的 ——
    // 本程序是绿色的，`settings.json` 就躺在 exe 旁边，改它是最自然的操作。
    // 而 `port: 0` 会让 DSH 自己挑一个随机端口，壳随后去探测 0 号端口必然失败，
    // 用户只会看到一句莫名其妙的"启动失败"，完全联想不到是端口写错了。
    // 在这里挡下来，给出的是一条能直接照做的提示。
    if next.port == 0 {
        return Err(
            "端口不能是 0：0 表示由系统随机分配，本程序无法据此去访问服务。\
             请填 1024~65535 之间的端口（默认 3080）。"
                .into(),
        );
    }

    let (old, path) = {
        let shell = app.state::<Shell>();
        (shell.settings(), shell.settings_path())
    };

    next.save(&path).map_err(|e| format!("保存配置失败：{e}"))?;
    logging::info("配置已更新", path.display().to_string());

    // 写回内存中的副本
    app.state::<Shell>().replace_settings(next.clone());

    // 哪些改动必须重启后端才生效？
    //
    // 原先只判端口，于是改了「DSH 可执行文件路径」或「允许 npx 兜底」之后，
    // 界面看着已经保存，实际跑的还是旧配置 —— 用户得自己猜到要点一下重启。
    // 这几项都决定"用哪个命令、带哪些参数去拉起 DSH"，所以一并纳入。
    //
    // 通知开关与镜像开关**不在此列**：它们在读取时即时生效
    // （前者每次判定都读配置，后者只影响新建的聊天视图），不需要重启。
    let mut reasons = Vec::new();
    if old.port != next.port {
        reasons.push(format!("端口 {} -> {}", old.port, next.port));
    }
    if old.no_open != next.no_open {
        reasons.push(format!("抑制浏览器弹窗 {} -> {}", old.no_open, next.no_open));
    }
    if old.custom_dsh_path != next.custom_dsh_path {
        reasons.push("DSH 可执行文件路径已变更".to_string());
    }
    if old.allow_npx != next.allow_npx {
        reasons.push(format!("npx 兜底 {} -> {}", old.allow_npx, next.allow_npx));
    }

    if !reasons.is_empty() {
        logging::info("配置变更需要重启后端，已自动重启", reasons.join("；"));
        shell::start_backend(&app, true);
    }
    Ok(())
}

/// 清空 `--no-open` 探测缓存并立刻重探（界面上"重新探测"按钮）
#[tauri::command]
fn recheck_no_open(app: AppHandle) -> Result<bool, String> {
    let settings = {
        let shell = app.state::<Shell>();
        let mut s = shell.settings();
        s.clear_no_open_cache();
        shell.replace_settings(s.clone());
        s
    };

    let ctx = lifecycle::CandidateContext {
        port: settings.port,
        no_open: settings.no_open,
        custom_path: settings.custom_dsh_path.clone(),
        allow_npx: settings.allow_npx,
    };
    let cands = lifecycle::build_candidates(&ctx);
    let Some(first) = cands.first() else {
        return Err("没有找到可用的 DSH 命令".into());
    };

    let supported = {
        let shell = app.state::<Shell>();
        let mut s = shell.settings();
        let r = lifecycle::supports_no_open(&first.program, &mut s);
        // 把探测结果写回内存与磁盘
        shell.replace_settings(s.clone());
        let _ = s.save(&shell.data_dir().join("settings.json"));
        r
    };

    logging::info(
        if supported {
            "探测完成：当前 DSH 支持 --no-open"
        } else {
            "探测完成：当前 DSH 不支持 --no-open（将无法抑制浏览器弹窗）"
        },
        first.describe(),
    );
    Ok(supported)
}

/// 停止后端（仅停止本程序拉起的实例）
#[tauri::command]
fn stop_backend(app: AppHandle) {
    stop_backend_impl(&app);
}

/// 停止后端的实际实现。
///
/// 抽出来是因为托盘菜单也要做同一件事：命令与托盘必须共用一份实现，
/// 否则两处逻辑会各自演化，出现"从界面停得掉、从托盘停不掉"这种怪事。
pub(crate) fn stop_backend_impl(app: &AppHandle) {
    let shell = app.state::<Shell>();
    shell.begin_shutdown();
    shell.kill_owned_child();
    let _ = app.emit("shell://state", BackendState::Idle);
    logging::info("已停止由本程序启动的 DSH 进程", "");
}

/// 手动触发一次 DSH 版本检查。
///
/// 结果**不从这里返回**，而是通过 `shell://update` 事件推给前端：
/// 检查要走网络（还可能挨个试代理），同步返回会把命令挂住好几秒。
/// 事件形式还让"托盘点检查"和"界面点检查"复用同一套前端展示。
#[tauri::command]
fn check_dsh_update(app: AppHandle) {
    update::check_now(app);
}

/// 用系统默认浏览器打开当前 DSH 地址（命令与托盘共用）
pub(crate) fn open_browser_url(app: &AppHandle) -> Result<(), String> {
    let shell = app.state::<Shell>();
    let token = shell
        .current_token()
        .ok_or_else(|| "当前还没有可用的访问地址：后端尚未就绪".to_string())?;
    let url = token::build_url(shell.port(), &token);
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| format!("打开失败：{e}"))
}

/// 接管：关掉端口上那个**不是本程序启动的** DSH 实例，
/// 然后由本程序拉起一个自己托管的实例。
///
/// # 为什么必须由用户显式触发
///
/// 端口上那个实例可能是用户正在用的（例如另一个终端里的 `dsh web`）。
/// 静默杀掉会打断用户正在进行的工作，所以本项目把它做成一个**显式操作**；
/// 参考项目则是在重启时无条件"清理 3080 监听者"，用户无从预期。
///
/// # 两道安全闸门
///
/// 1. 动手前**再探一次**该端口，确认上面的服务此刻仍然是 DSH ——
///    从用户看到错误到点下按钮之间可能有任意久，期间服务可能已经换人。
/// 2. 只杀 `netstat` 查到的**LISTENING** 进程，不做进程名模糊匹配，
///    避免误伤恰好同名的无关进程。
#[tauri::command]
async fn takeover_foreign_dsh(app: AppHandle) -> Result<(), String> {
    let port = app.state::<Shell>().settings().port;

    let pid = lifecycle::find_listening_pid(port).ok_or_else(|| {
        format!("没有查到正在监听端口 {port} 的进程，它可能已经退出了。请直接点「重试」。")
    })?;

    // 闸门 1
    match probe::probe(port, None) {
        probe::ProbeResult::Responding(kind) if kind.is_dsh() => {}
        _ => {
            return Err(format!(
                "端口 {port} 上的服务现在看起来已经不是 DSH 了，已取消接管。请点「重试」重新检测。"
            ))
        }
    }

    logging::warn(
        "正在接管：关闭已有的 DSH 实例",
        format!("PID {pid}（连同其子进程）"),
    );

    // 杀进程 + 等端口释放都放到后台线程：等端口最多要 ~6 秒，
    // 放在命令里会把界面卡住。
    std::thread::spawn(move || {
        lifecycle::kill_tree(pid);
        let freed = lifecycle::wait_listen_released(port, std::time::Duration::from_secs(6));
        if !freed {
            logging::error(
                "已有实例的端口仍未释放",
                format!("端口 {port}：对方可能还在退出中，下一步可能报端口占用"),
            );
        }
        // 走正常启动流程。`restart = false`：此时已经没有属于我们的子进程了。
        shell::start_backend(&app, false);
    });

    Ok(())
}

/// 复用已在端口上运行的 DSH 实例：**不杀进程、不重启**。
///
/// # 为什么现在可以复用（而设计之初不行）
///
/// 当初「接管即杀」的前提是：拿不到外部实例的 launch token，
/// 开窗口必然 401。但后来排查 token 失效问题时证实——DSH 的 cookie
/// 签名密钥持久化在 `DSH_HOME/auth/store.json`，WebView2 的登录态
/// （cookie）**跨后端进程存活**。所以只要 WebView2 里还有有效登录态，
/// 直接导航到不带 token 的首页就能进，根本不必动用户的进程。
///
/// # 已知取舍
///
/// 复用模式没有本代 token：事件流订阅不上，**任务完成通知不可用**；
/// 「重启后端」可随时切换回本程序托管（会接管外部实例）。
/// 另外，若 WebView2 的登录态已过期/被清，页面会显示 401——
/// 同样用「重启后端」恢复。
#[tauri::command]
async fn reuse_foreign_dsh(app: AppHandle) -> Result<(), String> {
    let port = app.state::<Shell>().settings().port;

    // 闸门：端口上得真的是 DSH（与接管同一道校验）
    match probe::probe(port, None) {
        probe::ProbeResult::Responding(kind) if kind.is_dsh() => {}
        _ => {
            return Err(format!(
                "端口 {port} 上现在没有检测到 DSH，无法复用。它可能已经退出了，请点「重试」重新检测。"
            ))
        }
    }

    let pid = lifecycle::find_listening_pid(port);
    let url = format!("http://127.0.0.1:{port}/");

    Shell::mark_foreign_ready(&app, &app.state::<Shell>(), url.clone(), pid);

    logging::info(
        "复用已运行的 DSH 实例（未重启进程）",
        format!(
            "PID {} · {url}（此模式无访问 token：任务完成通知不可用；要恢复请用「重启后端」）",
            pid.map(|p| p.to_string()).unwrap_or_else(|| "未知".into())
        ),
    );

    // 与正常就绪走同一条事件：前端拿 URL 去 chat_load
    use tauri::Emitter;
    let _ = app.emit("shell://ready", url);
    Ok(())
}

// ---- 聊天视图相关 --------------------------------------------------------

/// 上报聊天视图应占据的矩形区域
///
/// 前端在**布局变化时**调用（不只是窗口缩放）。用 `ResizeObserver` 监听
/// 承载容器最可靠，因为面板展开/收起也会改变它的尺寸。
#[tauri::command]
fn chat_set_bounds(app: AppHandle, bounds: Bounds) {
    let chat = app.state::<ChatView>();
    chat::set_bounds(&app, &chat, bounds);
}

/// 加载（或重新导航到）DSH 页面。
///
/// **必须是 async**：内部会创建子 WebView，
/// 而 Windows 上在同步命令里创建 WebView2 会死锁。
#[tauri::command]
async fn chat_load(app: AppHandle, url: String) -> Result<(), String> {
    let chat = app.state::<ChatView>();
    chat::load(&app, &chat, &url)?;
    let _ = app.emit("shell://chat-loaded", url);
    Ok(())
}

/// 显示 / 隐藏聊天视图
#[tauri::command]
fn chat_show(app: AppHandle) {
    let chat = app.state::<ChatView>();
    chat::show(&app, &chat);
}

#[tauri::command]
fn chat_hide(app: AppHandle) {
    let chat = app.state::<ChatView>();
    chat::hide(&app, &chat);
}

/// 重新加载聊天页面
#[tauri::command]
async fn chat_reload(app: AppHandle) -> Result<(), String> {
    chat::reload(&app)
}

/// 销毁聊天视图（后端失败/重启时调用）
#[tauri::command]
fn chat_destroy(app: AppHandle) {
    let chat = app.state::<ChatView>();
    chat::destroy(&app, &chat);
}

/// 用系统默认浏览器打开当前地址（给"我想用浏览器打开"的用户留的出口）
#[tauri::command]
fn open_in_browser(app: AppHandle) -> Result<(), String> {
    open_browser_url(&app)
}

/// 在文件管理器中定位数据目录（用户要"导出诊断"时最常用）
#[tauri::command]
fn reveal_data_dir(app: AppHandle) -> Result<(), String> {
    let shell = app.state::<Shell>();
    let dir = shell.data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
    tauri_plugin_opener::open_path(dir, None::<&str>).map_err(|e| format!("打开失败：{e}"))
}

// ---------------------------------------------------------------------------
// 应用装配
// ---------------------------------------------------------------------------

/// 退出时确保不留孤儿进程。
///
/// 参考项目的一个实际问题是：壳退出后 DSH 的 node 进程仍在跑并占着 3080，
/// 用户下次打开就会撞上"端口被占用"。
pub(crate) fn cleanup_on_exit(app: &AppHandle) {
    if let Some(shell) = app.try_state::<Shell>() {
        shell.begin_shutdown();
        shell.kill_owned_child();
    }
}

/// 可执行文件所在目录。
///
/// 本程序是**绿色**的：全部运行时数据都放在 exe 旁边的
/// `dsh-shell-data/`，不碰 AppData。
///
/// **唯一的注册表例外**是任务完成通知：为了能弹出 Windows 通知，必须往
/// `HKCU\Software\Classes\AppUserModelId\<identifier>` 写 `DisplayName` 与
/// `IconUri` 两个值（见 [`notify::ensure_toast_aumid`]）。这是微软给
/// **无安装器的便携程序**指定的唯一途径，不写的话系统会**静默丢弃**
/// 所有 toast。它只是 HKCU 下的一个键，卸载时删掉即可，没有其它残留。
pub(crate) fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 在 panic 终止进程之前，把本程序拉起的 DSH 子进程收掉。
///
/// # 为什么需要这一步
///
/// release 配置是 `panic = "abort"`（见 `Cargo.toml`）。这意味着**任意线程**
/// 一旦 panic —— 包括 `notify`、`update` 这些后台线程 —— 进程都会**立刻**终止，
/// 而且是 abort：不走栈展开，因此 `RunEvent::Exit`、窗口事件、
/// [`cleanup_on_exit`] 全都不会执行。
///
/// 后果很具体：DSH 的 node 进程变成孤儿并继续占着端口。用户下次打开程序
/// 撞上"端口被占用"，而日志里除了一条 panic 什么线索都没有 ——
/// 正是这个项目最想避免的那类故障。
///
/// panic hook 是 abort 之前**唯一**还会被调用的地方，所以收尾只能放这里。
fn install_panic_cleanup() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // 先让默认 hook 照常打一遍，保留原有的可观测性
        default_hook(info);

        // 这一次收尾必须**绝不再 panic、也绝不加锁**：
        // 加锁会和"正持有那把锁的 panic 线程"死锁，而死锁比直接终止更糟 ——
        // 进程挂在那里不退，端口一直被占。见 `shell::OWNED_CHILD_PID`。
        crate::shell::kill_owned_child_panic_safe();

        // 再留一条给用户看的记录。走 logging 里那个 try_lock 版本，
        // 因为 panic 很可能就发生在日志代码内部（那时锁被自己占着）。
        logging::log_from_panic_hook(
            "程序遇到内部错误，即将退出",
            &format!("{info}（已尽力收掉 DSH 子进程，避免它继续占用端口）"),
        );
    }));
}

/// 构建并运行应用
pub fn run() {
    // 日志必须**先于**单实例判断初始化：
    // 第二个实例会在启动早期就退出，如果不先接上日志文件，
    // 那"为什么点了没反应"就没有任何记录可查。
    logging::init(settings::data_dir(&exe_dir()));

    // panic hook 要尽早装：装得越晚，能覆盖到的代码越少。
    // （它依赖日志目录已初始化，所以放在 logging::init 之后。）
    install_panic_cleanup();

    // 单实例保护要放在最前面：晚一步就会先把后端拉起来再退出，
    // 白白和已有实例抢一次端口（详见 `single_instance` 模块文档）。
    if !single_instance::acquire() {
        single_instance::focus_existing();
        return;
    }

    tauri::Builder::default()
        // **必须关掉 opener 的 JS 链接拦截**（`open_js_links_on_click: false`）。
        //
        // 它默认会注入一段脚本，在**冒泡阶段**抢走 `target="_blank"` 与
        // Ctrl/Shift+点击，`preventDefault()` 掉页面原生新窗口，改走页面内的
        // `plugin:opener|open_url` IPC —— 而 DSH 页面在 `http://127.0.0.1:3080`
        // 这个远程上下文里没有可靠的 IPC 桥，于是**两头落空**：用户点外链毫无反应。
        // （参考项目 v2.0.2 的 CHANGELOG 记的正是这个坑。）
        //
        // 关掉之后，这类点击回归原生的 `on_new_window` → 系统浏览器通道，
        // 与右键菜单走的是同一条已验证可靠的路径。
        // 参见 `chat.rs` 的 `on_new_window` 与 `inject.rs` 的捕获阶段兜底。
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            start_backend,
            stop_backend,
            takeover_foreign_dsh,
            reuse_foreign_dsh,
            export_diagnostics,
            save_diagnostics,
            get_settings,
            update_settings,
            recheck_no_open,
            chat_set_bounds,
            chat_load,
            chat_show,
            chat_hide,
            chat_reload,
            chat_destroy,
            open_in_browser,
            reveal_data_dir,
            check_dsh_update,
        ])
        .setup(|app| {
            // ---- 托管状态 -------------------------------------------------
            app.manage(shell::Shell::new(exe_dir()));
            app.manage(chat::ChatView::new());

            // ---- 主窗口 + React 控制台 WebView ----------------------------
            //
            // 必须用 `WindowBuilder` 而不是 `WebviewWindowBuilder`：
            // 多 WebView 模式下窗口本身**不内置** WebView，
            // 所有 WebView（控制台、聊天）都作为子视图显式添加。
            let mut window_builder = tauri::window::WindowBuilder::new(app, chat::MAIN_WINDOW)
                .title("DSH")
                .inner_size(1280.0, 860.0)
                .min_inner_size(720.0, 520.0);

            // 窗口图标必须**显式**设置。
            //
            // 直接建窗口时 hIcon 为空，Windows 会退回窗口类的默认图标，
            // 任务栏上就是一个空白方块 —— 即使 exe 里已经嵌好了图标资源。
            match app.default_window_icon().cloned() {
                Some(icon) => window_builder = window_builder.icon(icon)?,
                None => logging::warn(
                    "没有可用的应用图标，任务栏图标会显示为空白",
                    "请确认 tauri.conf.json 的 bundle.icon 与 icons/ 目录",
                ),
            }

            let window = window_builder.build()?;

            let shell_view = WebviewBuilder::new(
                SHELL_WEBVIEW,
                WebviewUrl::App("index.html".into()),
            )
            // 随窗口自动伸缩，省掉手工同步尺寸的竞态
            .auto_resize()
            // 控制台的加载状态必须留在日志里：
            // 前端若没起来，用户看到的是一片空白，而日志里毫无线索——
            // 这正是"白屏"类故障最难查的原因。
            .on_page_load(|_wv, payload| {
                let url = payload.url().to_string();
                match payload.event() {
                    tauri::webview::PageLoadEvent::Started => {
                        logging::info("控制台页面开始加载", url)
                    }
                    tauri::webview::PageLoadEvent::Finished => {
                        logging::info("控制台页面加载完成", url)
                    }
                }
            });

            let scale = window.scale_factor()?;
            let logical = window.inner_size()?.to_logical::<f64>(scale);
            window.add_child(
                shell_view,
                LogicalPosition::new(0.0, 0.0),
                logical,
            )?;

            // ---- 托盘 ------------------------------------------------------
            // 失败不阻断启动：没有托盘顶多是少了一种交互方式，
            // 而窗口本身仍然可用，不该因此让整个程序起不来。
            if let Err(e) = tray::build(app.handle()) {
                logging::error(
                    "创建托盘图标失败",
                    format!("{e}（程序仍可正常使用，只是没有托盘入口）"),
                );
            }

            // ---- 启动后端 -------------------------------------------------
            shell::start_backend(app.handle(), false);

            // ---- 会话完成通知 ---------------------------------------------
            // 用户关掉了就既不起线程、也不连 WebSocket（见 `notify` 模块文档）。
            if app
                .state::<Shell>()
                .settings()
                .notify_on_finish
            {
                notify::spawn(app.handle().clone());
            } else {
                logging::info("任务完成通知已关闭", "可在设置中重新开启");
            }

            // ---- 更新检查 -------------------------------------------------
            // 延迟若干秒后自查一次；失败只记日志，绝不打扰用户
            // （开机时网络尚未就绪是常态）。
            update::spawn_check(app.handle().clone());

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();

                // `close_to_tray` 默认 true：关闭窗口 = 藏进托盘。
                // 这个设置之前一直没被实现（当时没有托盘，实现了就等于
                // "关掉再也叫不回来"），现在托盘齐了才敢接上。
                let to_tray = app
                    .try_state::<Shell>()
                    .map(|s| s.settings().close_to_tray)
                    .unwrap_or(false)
                    // 保命闸门：托盘不在就绝不能隐藏窗口，
                    // 否则用户点一下关闭，程序就彻底够不着了。
                    && tray::is_available(app);

                if to_tray {
                    // 关键：拦下关闭动作，否则窗口真的会被销毁，
                    // 托盘上的"显示主窗口"就再也找不到窗口了。
                    api.prevent_close();
                    let _ = window.hide();
                    logging::info("窗口已隐藏到托盘", "左键单击托盘图标可唤回，右键可退出");
                } else {
                    // 关窗即退出：一并收掉子进程，避免留下占着端口的孤儿
                    cleanup_on_exit(app);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("DSH Shell 初始化失败")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                cleanup_on_exit(app);
            }
        });
}

/// 权限配置的自检。
///
/// # 为什么用测试而不是注释来守这条线
///
/// Tauri 判定一个 IPC 命令是否放行时，`RuntimeAuthority::resolve_access`
/// 的过滤条件是：
///
/// ```ignore
/// origin.matches(&cmd.context)
///   && (cmd.webviews.iter().any(|w| w.matches(webview))
///       || cmd.windows.iter().any(|w| w.matches(window)))
/// ```
///
/// 注意中间是 **`||` 而不是 `&&`**：`webviews` 和 `windows` 命中任意一个就授权。
/// 而本项目的聊天视图与 React 控制台**同处一个窗口 `main`**
/// （见 [`chat`] 的模块文档）—— 于是 `windows` 里只要出现 `"main"`，
/// 那个加载远程 DSH 页面的聊天视图就会跟着拿到整套权限，
/// 其中包括 `opener:default`：页面一旦被 XSS，就能驱动本机打开任意 URL。
///
/// 这层约束写在注释里是守不住的——改 `capabilities/*.json` 的人
/// 不会去翻 Rust 源码。所以用测试钉住：配置被改错时，`cargo test` 直接失败。
///
/// 需要说明的是，即便 `windows` 被误改，**当前也仍然拦得住**：聊天视图的
/// URL 是 `http://127.0.0.1:3080`，`is_local_url` 只认 `tauri://` 与
/// `devUrl/frontendDist`，因此它的 origin 是 `Remote`，而 capability 不声明
/// `remote` 就等于拒绝。但那是**依赖默认值**的隐式保护——这里要的是显式的第二道锁。
#[cfg(test)]
mod capability_guard {
    use serde_json::Value;

    /// 加载远程页面的授权目标。它们绝不能出现在任何 capability 里。
    const REMOTE_TARGETS: &[&str] = &["chat", "dsh-chat"];

    /// 聊天视图所在的窗口。列进 `windows` 等于给它授权（见模块文档）。
    const CHAT_HOST_WINDOWS: &[&str] = &["main", "dsh-chat"];

    fn capability_files() -> Vec<Value> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("读不到 capabilities 目录 {}：{e}", dir.display()));

        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("读不了 {}：{e}", path.display()));
            let value: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} 不是合法 JSON：{e}", path.display()));
            out.push(value);
        }
        assert!(!out.is_empty(), "capabilities 目录里一个配置都没有");
        out
    }

    fn string_array(cap: &Value, key: &str) -> Vec<String> {
        cap.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn id(cap: &Value) -> String {
        cap.get("identifier")
            .and_then(|v| v.as_str())
            .unwrap_or("<无 identifier>")
            .to_string()
    }

    #[test]
    fn remote_webviews_never_get_permissions() {
        for cap in capability_files() {
            for key in ["webviews", "windows"] {
                for target in string_array(&cap, key) {
                    assert!(
                        !REMOTE_TARGETS.contains(&target.as_str()),
                        "capability {:?} 的 {key} 里出现了远程页面目标 {:?}",
                        id(&cap),
                        target
                    );
                }
            }
        }
    }

    #[test]
    fn no_capability_targets_a_window_hosting_the_chat_view() {
        for cap in capability_files() {
            for w in string_array(&cap, "windows") {
                assert!(
                    !CHAT_HOST_WINDOWS.contains(&w.as_str()),
                    "capability {:?} 的 windows 里出现了 {:?}。它与聊天视图同窗口，\
                     而 Tauri 的 webviews/windows 是「或」关系，这会让远程页面拿到权限。",
                    id(&cap),
                    w
                );
            }
        }
    }

    #[test]
    fn capabilities_stay_local_only() {
        for cap in capability_files() {
            assert_eq!(
                cap.get("local").and_then(|v| v.as_bool()),
                Some(true),
                "capability {:?} 没有显式声明 local: true —— 不要依赖 serde 的默认值，\
                 将来它若变化，应该让测试失败而不是无声地放开远程来源",
                id(&cap)
            );
            assert!(
                cap.get("remote").is_none(),
                "capability {:?} 声明了 remote —— 本程序没有任何远程页面需要 IPC 权限",
                id(&cap)
            );
        }
    }
}
