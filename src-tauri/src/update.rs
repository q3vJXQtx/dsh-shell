//! DSH 版本检查。
//!
//! # 为什么检查的是 DSH 而不是壳自己
//!
//! 参考项目的 `update.rs`（623 行）做的是**壳自更新**：查它自己的
//! GitHub Release、下载新 exe、改名替换、重启。那套代码高度依赖
//! "有一个持续发布 exe 资产的仓库"这个前提。
//!
//! 本项目目前**没有远端仓库**（`git remote` 为空），照搬过来只会得到
//! 一个永远查不到东西的空壳功能——比没有更糟，因为它会让人以为
//! "自动更新是开着的"。
//!
//! 所以这里做的是真正能生效的那一半：**检查 DSH 本身有没有新版本**。
//! DSH 是 npm 包（`@deepseek-ai/dsh`），registry 随时可查，
//! 本地版本也能从正在用的命令问出来，对比结果立刻就有意义。
//!
//! # 为什么不自动执行更新
//!
//! 各人的 DSH 安装方式不同（全局 npm / pnpm / 项目内 / npx 缓存），
//! 对应的升级命令也不同。猜错了会悄悄改坏用户的 node 环境——
//! 这种代价远高于"用户自己复制一行命令"的麻烦。
//! 因此这里只报告 + 给出命令，**动手这一步交给用户**。
//!
//! # 版本号带预发布后缀
//!
//! DSH 目前发的是 `0.1.5-rc.1` 这类版本。按点号切分再 `parse::<i64>()`
//! 会把 `5-rc` 解析成 0，得出错误结论（参考项目正是这么比的，只是它比的是
//! 自己不带后缀的壳版本，所以没暴露）。这里实现完整的 semver 预发布比较。

use serde::Serialize;
// `try_state` 是 `Manager` trait 的方法，必须显式引入
use tauri::Manager;

use crate::logging;
use crate::settings::Settings;

/// DSH 的 npm 包名
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";

/// npm registry 上查最新版；`%2F` 是 `/` 的转义（scoped 包必须转义）
const REGISTRY_URL: &str = "https://registry.npmjs.org/@deepseek-ai%2Fdsh/latest";

/// 常见本地代理端口（**仅 HTTP 系**）。
///
/// 国内用户的 npm registry 访问常需要代理，而 Clash/v2rayN 的默认端口
/// 就这几个。先探测再使用（1 秒 TCP 连接），避免对着死端口白等 8 秒超时。
///
/// **刻意排除了纯 Socks 端口**（Clash 的 7891、v2rayN 的 10808）：
/// 本程序的 HTTP 客户端（ureq，未启用 socks feature）只会对代理说
/// HTTP CONNECT，对着 Socks 端口说 HTTP 只会得到 Unexpected EOF——
/// 实测踩过：混合端口没开、只有 Socks 端口开着时，自动探测选中 7891，
/// 每次版本检查都失败。可用的混合/HTTP 端口对应关系：
/// Clash 7890（混合）、Clash Verge 7897（混合）、7892（部分客户端的 Http 端口）、
/// v2rayN 10809（http）。
const LOCAL_PROXY_PORTS: &[&str] = &["7890", "7897", "7892", "10809"];

/// 一次检查的结果（发给前端）
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    // 与 `state::FailureReason` 同理：只写 `rename_all` 不会改变体字段名，
    // 少了这行前端读到的 `current`/`latest` 就会是 undefined。
    rename_all_fields = "camelCase"
)]
pub enum UpdateStatus {
    /// 正在检查
    Checking,
    /// 已是最新
    UpToDate { current: String },
    /// 有新版本可用
    Available {
        current: String,
        latest: String,
        /// 建议用户执行的升级命令
        command: String,
    },
    /// 无法判断（例如本地 DSH 版本取不到，或只有 npx 候选）
    Skipped { reason: String },
    /// 检查失败（网络等原因）
    Failed { message: String },
}

/// 检查结果事件名
pub const UPDATE_EVENT: &str = "shell://update";

/// 防重入用的「上次检查开始时刻」（毫秒时间戳；0 表示当前没有检查在跑）。
///
/// # 为什么是时间戳而不是布尔量
///
/// 布尔量一旦因为任何意外（命令挂住、线程被卡、提前 return 漏了收尾）
/// 没有复位，就会**永远**停在"进行中" —— 此后用户托盘点多少次、
/// 界面点多少次，都只会在日志里留一句"已有一次更新检查在进行中"然后被丢弃，
/// 而且**没有任何面向用户的提示**，功能等于静默死掉。
///
/// 带上时间戳就自然过期了：超过 [`CHECK_STALE_AFTER_MS`] 就当作
/// 上一次已经卡死，放行新的检查。卡死最多影响一次检查，不会锁死功能。
static CHECK_STARTED_AT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// 认为上一次检查已经卡死的时间阈值。
///
/// 正常一次检查最长也就几十秒（直连 8s + 若干代理各 8s + 端口探测），
/// 5 分钟足够宽裕，不会把正常的慢检查误判成卡死。
const CHECK_STALE_AFTER_MS: u64 = 5 * 60 * 1000;

/// 取当前毫秒时间戳
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 尝试占住"检查进行中"。已经有一次在跑则返回 `false`。
///
/// 用 `compare_exchange` 而不是先 load 再 store：两个入口
/// （启动时的自动检查、托盘/界面的手动检查）可能同时进来。
fn try_begin_check() -> bool {
    let now = now_ms();
    let started = CHECK_STARTED_AT_MS.load(std::sync::atomic::Ordering::SeqCst);

    // 0 = 没人在跑；非 0 且未过期 = 确实在跑
    let running =
        started != 0 && now.saturating_sub(started) < CHECK_STALE_AFTER_MS;
    if running {
        return false;
    }

    // 只在"值仍是我们观察到的那个"时才占位，避免并发同时通过
    CHECK_STARTED_AT_MS
        .compare_exchange(
            started,
            now,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

/// 释放"检查进行中"
fn finish_check() {
    CHECK_STARTED_AT_MS.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// 启动后延迟多久做第一次检查。
///
/// 不放在启动瞬间：那时正忙着拉 DSH 子进程、抢端口，
/// 网络请求和进程启动叠在一起会让"启动慢"的观感变差。
const FIRST_CHECK_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// 启动时的自动检查（后台线程，不阻塞）
///
/// 也走防重入：否则用户在启动检查还没跑完时点一下托盘，
/// 两个检查会并发去拉 registry（多花好几秒、日志也重复）。
pub fn spawn_check(app: tauri::AppHandle) {
    if !try_begin_check() {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK_DELAY);
        run_check(&app, false);
        finish_check();
    });
}

/// 托盘菜单 / 界面按钮触发的检查
pub fn check_now(app: tauri::AppHandle) {
    if !try_begin_check() {
        logging::info("已有一次更新检查在进行中", "忽略本次请求");
        return;
    }
    std::thread::spawn(move || {
        run_check(&app, true);
        finish_check();
    });
}

/// 检查主流程。
///
/// `interactive` 为真表示用户主动点的——此时失败也要说出来（托盘气泡 + 日志）；
/// 为假表示启动时的静默检查，失败只记日志，不打扰用户
/// （开机时网络没起来是很常见的事，不该弹一串报错）。
fn run_check(app: &tauri::AppHandle, interactive: bool) {
    use tauri::Emitter;

    let emit = |status: UpdateStatus| {
        let _ = app.emit(UPDATE_EVENT, status);
    };
    emit(UpdateStatus::Checking);

    let settings = app
        .try_state::<crate::shell::Shell>()
        .map(|s| s.settings())
        .unwrap_or_default();

    let Some(current) = local_dsh_version(&settings) else {
        let reason = "无法确定本地 DSH 版本（可能只配置了 npx 兜底）。请手动执行 dsh --version 确认。";
        logging::info("跳过 DSH 版本检查", reason.to_string());
        emit(UpdateStatus::Skipped {
            reason: reason.to_string(),
        });
        return;
    };

    match registry_latest(&settings) {
        Ok(latest) => {
            let cmp = compare_versions(&latest, &current);
            if cmp > 0 {
                let command = format!("npm install -g {DSH_PACKAGE}@latest");
                logging::info(
                    "发现 DSH 新版本",
                    format!("本地 {current} → 最新 {latest}"),
                );
                emit(UpdateStatus::Available {
                    current,
                    latest,
                    command,
                });
            } else {
                logging::info("DSH 已是最新版本", current.clone());
                emit(UpdateStatus::UpToDate { current });
            }
        }
        Err(message) => {
            if interactive {
                logging::warn("检查 DSH 更新失败", message.clone());
            } else {
                logging::info("暂时查不到 DSH 最新版本", message.clone());
            }
            emit(UpdateStatus::Failed { message });
        }
    }
}

/// 问出本地 `dsh` 的版本号。
///
/// 走的是**和启动后端同一条候选链**（[`crate::lifecycle::build_candidates`]），
/// 所以"检查更新看到的版本"和"实际在跑的版本"必然是同一个，
/// 不会出现"界面说你装的是 A、跑的却是 B"这种错位。
///
/// npx 候选被跳过：`npx --version` 返回的是 npx 自己的版本，不是 DSH 的；
/// 而 npx 每次都拉最新的，本来也不存在"版本落后"一说。
fn local_dsh_version(settings: &Settings) -> Option<String> {
    let ctx = crate::lifecycle::CandidateContext {
        port: settings.port,
        no_open: settings.no_open,
        custom_path: settings.custom_dsh_path.clone(),
        allow_npx: settings.allow_npx,
    };

    for cand in crate::lifecycle::build_candidates(&ctx) {
        // npx 候选问不出 DSH 的版本（见函数文档）
        if is_npx(&cand.program) {
            continue;
        }
        if let Some(v) = run_version(&cand.program, cand.cwd.as_deref()) {
            return Some(v);
        }
    }
    None
}

/// 判断某个程序是不是 npx
fn is_npx(program: &str) -> bool {
    let stem = std::path::Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| program.to_lowercase());
    stem == "npx"
}

/// `dsh --version` 的超时。
///
/// 这个调用**必须有超时**：`run_version` 跑在检查线程上，一旦命令挂住，
/// 线程就永不返回、防重入标记也不会释放。虽然后者带了过期兜底
/// （见 [`CHECK_STALE_AFTER_MS`]），但让检查本身能及时结束才是正解。
const VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 执行 `<program> --version` 并解析出版本号
fn run_version(program: &str, cwd: Option<&std::path::Path>) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.arg("--version");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // 不能弹黑窗：检查更新是后台行为，闪一个 cmd 窗口比不检查还糟
    crate::lifecycle::hide_console(&mut cmd);

    let output = crate::lifecycle::output_with_timeout(&mut cmd, VERSION_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_version_line(&text)
}

/// 从 `--version` 的输出里挑出版本号。
///
/// 不假设整行就是版本号：有的 CLI 会打成 `dsh 0.1.5-rc.1`。
/// 逐词扫描，取第一个"看起来像版本号"的词。
fn parse_version_line(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let cleaned = token.trim_start_matches('v');
        if looks_like_version(cleaned) {
            return Some(cleaned.to_string());
        }
    }
    None
}

/// 形如 `1`、`1.2`、`1.2.3`、`1.2.3-rc.1` 的字符串
fn looks_like_version(s: &str) -> bool {
    let core = s.split('-').next().unwrap_or("");
    if core.is_empty() {
        return false;
    }
    let mut parts = 0;
    for seg in core.split('.') {
        if seg.is_empty() || !seg.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

/// 查 npm registry 上的最新版本。
///
/// 代理行为由设置决定（[`crate::settings::ProxyMode`]）：
/// - `Direct`：只直连；
/// - `Custom`：只走用户填写的代理；
/// - `Auto`（默认，历史行为）：直连优先，失败后依次尝试环境变量里的代理、
///   再探测本地常见代理端口——为了"办公室/家里网络环境不同"的同一台电脑也能用。
fn registry_latest(settings: &Settings) -> Result<String, String> {
    match settings.proxy_mode {
        crate::settings::ProxyMode::Direct => fetch_latest(None).map_err(|e| {
            format!(
                "直连失败（{e}）；当前代理设置为「直连」。\
                 若本机需要代理才能访问 npm registry，请到设置里改为「自动探测」或「自定义代理」。"
            )
        }),
        crate::settings::ProxyMode::Custom => {
            let url = settings
                .proxy_url
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            if url.is_empty() {
                return Err(
                    "代理设置为「自定义代理」但没有填写地址，请到设置里补上。".to_string(),
                );
            }
            fetch_latest(Some(url)).map_err(|e| format!("自定义代理 {url}：{e}"))
        }
        crate::settings::ProxyMode::Auto => {
            match fetch_latest(None) {
                Ok(v) => return Ok(v),
                Err(direct) => {
                    let mut tried = Vec::new();
                    for proxy in proxy_candidates() {
                        match fetch_latest(Some(&proxy)) {
                            Ok(v) => {
                                logging::info("已通过代理取得 DSH 最新版本", proxy);
                                return Ok(v);
                            }
                            Err(e) => tried.push(format!("{proxy}: {e}")),
                        }
                    }
                    if tried.is_empty() {
                        Err(format!(
                            "直连失败（{direct}），且没有探测到可用的本地代理。\
                             若本机需要代理才能访问外网，可到设置里把代理改为「自定义代理」\
                             并填入地址（用混合或 Http 端口，如 http://127.0.0.1:7890），\
                             或设置 HTTPS_PROXY 环境变量。"
                        ))
                    } else {
                        // 把**每一级**代理的失败原因都带上。
                        //
                        // 原先这里只把 `tried` 用来判空，消息里只剩直连那条错误 ——
                        // 于是"代理明明在跑却连不上"这种最常见的情况，用户看到的
                        // 只有一句笼统的网络超时，真正的原因（代理端口连不上、
                        // 代理返回错误响应）全被吞掉，只能靠猜。对一个主打
                        // "可诊断"的项目，这属于把已经拿到的线索又扔了。
                        //
                        // 消息里额外提示 Socks 端口的坑：探测列表已排除纯 Socks
                        // 端口，但用户自定义填错（对着 Socks 端口说 HTTP）时
                        // 表现就是 Unexpected EOF，这里给一句能对照自查的话。
                        Err(format!(
                            "直连与 {} 个代理均失败。直连：{direct}；{}\
                             \n注意：本程序只支持 HTTP 系代理——\
                             若填的是 Socks 端口（如 Clash 的 7891、v2rayN 的 10808），\
                             请改用混合端口（如 7890）或 Http 端口。",
                            tried.len(),
                            tried.join("；")
                        ))
                    }
                }
            }
        }
    }
}

/// 可用的代理列表：先环境变量，再本地端口探测
fn proxy_candidates() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_string();
            if !value.is_empty() && !out.contains(&value) {
                out.push(value);
            }
        }
    }

    for port in LOCAL_PROXY_PORTS {
        let proxy = format!("http://127.0.0.1:{port}");
        if !out.contains(&proxy) && port_is_open(port) {
            out.push(proxy);
        }
    }
    out
}

/// 1 秒 TCP 探测：死端口不值得花 8 秒超时去等
fn port_is_open(port: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let Ok(mut addrs) = ("127.0.0.1", port).to_socket_addrs() else {
        return false;
    };
    match addrs.next() {
        Some(addr) => TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(1)).is_ok(),
        None => false,
    }
}

/// 真正发一次请求
fn fetch_latest(proxy: Option<&str>) -> Result<String, String> {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(8));

    if let Some(proxy) = proxy {
        builder = builder.proxy(ureq::Proxy::new(proxy).map_err(|e| format!("代理无效：{e}"))?);
    }

    let response = builder
        .build()
        .get(REGISTRY_URL)
        // `/latest` 是**版本文档**端点：带 corgi（install-v1）Accept 会被
        // registry 以 406 Not Acceptable 拒绝（corgi 只用于包文档 /pkg），
        // 这里必须用普通 application/json——实测代理链路通了却 406 就是它。
        .set("Accept", "application/json")
        .set("User-Agent", "dsh-shell")
        .call()
        .map_err(|e| format!("{e}"))?;

    let doc: serde_json::Value = response
        .into_json()
        .map_err(|e| format!("响应解析失败：{e}"))?;

    doc.get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "registry 响应里没有 version 字段".to_string())
}

// ---------------------------------------------------------------------------
// 版本比较
// ---------------------------------------------------------------------------

/// 按 semver 比较两个版本号：`a > b` 返回正数，相等返回 0。
///
/// 实现的是 semver 的**排序规则**（不是完整校验）：
///
/// - 先比核心版本 `major.minor.patch`，缺失的段按 0 算（`1.2` == `1.2.0`）；
/// - **有预发布后缀的 < 没有后缀的**：`1.0.0-rc.1 < 1.0.0`；
/// - 两边都有后缀时逐段比：纯数字段按数值比，且**数字段 < 字母段**
///   （`1.0.0-2 < 1.0.0-alpha`）；前缀相同则段数多的更大。
pub fn compare_versions(a: &str, b: &str) -> i32 {
    use std::cmp::Ordering;

    let (a_core, a_pre) = split_prerelease(a);
    let (b_core, b_pre) = split_prerelease(b);

    match compare_core(&a_core, &b_core) {
        Ordering::Equal => {}
        other => return ord_value(other),
    }

    match (a_pre, b_pre) {
        // 没有预发布后缀 > 有后缀
        (None, None) => 0,
        (None, Some(_)) => 1,
        (Some(_), None) => -1,
        (Some(pa), Some(pb)) => ord_value(compare_prerelease(&pa, &pb)),
    }
}

/// 拆成 `(核心版本, 预发布后缀)`
fn split_prerelease(v: &str) -> (String, Option<String>) {
    let v = v.trim().trim_start_matches('v');
    match v.split_once('-') {
        Some((core, pre)) => (core.to_string(), Some(pre.to_string())),
        None => (v.to_string(), None),
    }
}

/// 比 `major.minor.patch`（缺失按 0）
fn compare_core(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let pa: Vec<i64> = a.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    let pb: Vec<i64> = b.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    for i in 0..pa.len().max(pb.len()) {
        let d = pa.get(i).copied().unwrap_or(0) - pb.get(i).copied().unwrap_or(0);
        if d != 0 {
            return if d > 0 { Ordering::Greater } else { Ordering::Less };
        }
    }
    Ordering::Equal
}

/// 比预发布后缀（如 `rc.1` vs `rc.2`）
fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let pa: Vec<&str> = a.split('.').collect();
    let pb: Vec<&str> = b.split('.').collect();

    for i in 0..pa.len().max(pb.len()) {
        match (pa.get(i), pb.get(i)) {
            (None, None) => return Ordering::Equal,
            // 前缀相同则段数多的更大：rc.1 < rc.1.1
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let nx = x.parse::<i64>();
                let ny = y.parse::<i64>();
                let ord = match (nx, ny) {
                    // 都是数字：按数值
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    // 数字段 < 非数字段（semver 规定）
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    // 都是非数字：字典序
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
    Ordering::Equal
}

/// 把 `Ordering` 映射成 `i32`
fn ord_value(o: std::cmp::Ordering) -> i32 {
    match o {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_versions_compare_correctly() {
        assert!(compare_versions("0.2.0", "0.1.0") > 0);
        assert!(compare_versions("0.1.0", "0.2.0") < 0);
        assert_eq!(compare_versions("1.2.3", "1.2.3"), 0);
    }

    #[test]
    fn missing_segments_count_as_zero() {
        assert_eq!(compare_versions("1.2", "1.2.0"), 0);
        assert_eq!(compare_versions("1", "1.0.0"), 0);
    }

    #[test]
    fn prerelease_is_older_than_release() {
        // 这是参考项目那个按点号切分的实现会算错的地方
        assert!(compare_versions("1.0.0-rc.1", "1.0.0") < 0);
        assert!(compare_versions("1.0.0", "1.0.0-rc.1") > 0);
    }

    #[test]
    fn prerelease_segments_compare_numerically() {
        assert!(compare_versions("0.1.5-rc.2", "0.1.5-rc.1") > 0);
        assert!(compare_versions("0.1.5-rc.10", "0.1.5-rc.9") > 0);
        assert_eq!(compare_versions("0.1.5-rc.1", "0.1.5-rc.1"), 0);
    }

    #[test]
    fn real_world_dsh_versions() {
        // 本机实际装的就是这个版本，且 registry 上也是它
        assert_eq!(compare_versions("0.1.5-rc.1", "0.1.5-rc.1"), 0);
        // rc.1 之后发正式版，应视为升级
        assert!(compare_versions("0.1.5", "0.1.5-rc.1") > 0);
        // 下一个补丁的 rc 也应视为升级
        assert!(compare_versions("0.1.6-rc.1", "0.1.5-rc.1") > 0);
    }

    #[test]
    fn version_line_parsing_is_tolerant() {
        assert_eq!(parse_version_line("0.1.5-rc.1\n"), Some("0.1.5-rc.1".into()));
        assert_eq!(parse_version_line("dsh 0.1.5\n"), Some("0.1.5".into()));
        assert_eq!(parse_version_line("v1.2.3\n"), Some("1.2.3".into()));
        assert_eq!(parse_version_line("command not found\n"), None);
    }

    #[test]
    fn looks_like_version_rejects_noise() {
        assert!(looks_like_version("1.2.3"));
        assert!(looks_like_version("0.1.5-rc.1"));
        assert!(!looks_like_version("abc"));
        assert!(!looks_like_version("1"));
        assert!(!looks_like_version(""));
    }

    #[test]
    fn npx_detection() {
        assert!(is_npx("npx"));
        assert!(is_npx("npx.cmd"));
        assert!(is_npx(r"C:\Program Files\nodejs\npx.cmd"));
        assert!(!is_npx("dsh.cmd"));
    }
}
