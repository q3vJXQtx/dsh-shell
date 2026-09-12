//! 任务完成通知 —— DSH 会话跑完时弹一条 Windows 原生通知。
//!
//! # 事件从哪来（对齐 DSH 真实协议）
//!
//! 旧实现连的是 `ws://…/api/events.host` 并自行发明了一套
//! `host/session-status` 帧格式——这个端点**根本不存在**（整个 `/api` 是
//! 统一鉴权门，401 什么都证明不了），通知功能因此从未真正工作过。
//!
//! DSH 的权威协议（源码位于
//! `$DSH_HOME/profiles/node_modules/@deepseek-ai/`，以 `dsh-api-gateway`、
//! `dsh-client-connection`、`dsh-api-remotes` 三个包为准）：
//!
//! 1. **token → cookie**：`dsh web` 打印的 `?token=` 只在 `GET /` 上被接受
//!    （`authorizeIndex`），成功时 303 + `Set-Cookie` 换出一张签名、
//!    HttpOnly、绑定 Host authority 的会话 cookie；token 换 cookie 之外
//!    一律 401，Host/Origin 不对是 403。
//! 2. **WS 升级**：`/api/remote.mux` 的升级请求与所有 `/api` 请求一样过
//!    Host/Origin 同源围栏 + cookie 校验（`requestRejection`）。
//! 3. **逻辑流**：连上后发一帧 `open` 打开 `$events` 逻辑流——这是 DSH
//!    转发会话事件的唯一通道。首帧 `value.type == "ready"`，之后的
//!    `{type:"emit", event, args}` 帧就是转发事件，其中
//!    `api-session/status(sessionId, running)` 携带会话运行状态。
//!
//! # 旁观者义务：waterfall 必须弃权
//!
//! `$events` 上还有 `waterfall` 帧（审批请求、用户提问等）。Gateway 要等
//! **所有**收到请求的客户端都回应后才以 `next` 结算——旁观者若只收不回，
//! 会把真实浏览器端的审批流程卡死。所以这里收到 waterfall 立即通过
//! `POST /api/$events/result` 回 `{kind:"next"}`（弃权），**绝不**回
//! `result`/`rejected`（那等于替用户做决定）。
//!
//! # 边沿判定：为什么必须先播种
//!
//! 与旧实现假设的相反，`$events` 重连后**不会重放**存量会话状态，只推
//! 增量。所以每次连接成功后先用 `session/list` 快照给基线播种，再打开
//! 事件流——否则会漏掉"连接之前就在跑、之后跑完"的会话。通知只认
//! `true → false` 的那一步边沿。
//!
//! # 相对参考项目的修正
//!
//! 参考项目判断"用户是否正看着"只用了 `is_visible()`，但**窗口最小化时
//! `is_visible()` 仍然返回 `true`** —— 于是用户最小化窗口后去干别的，
//! 任务完成反而**不会**收到通知，恰好漏掉了最需要通知的场景。
//! 这里改成"可见 **且** 未最小化 **且** 处于前台"才算用户正看着。
//!
//! # 端口与 token 都不是硬编码的
//!
//! 每次重连都从当前配置重新取端口、从 `Shell` 状态重新取 token——
//! 后端重启（token 换代）、换端口后通知会自动跟上。

use tauri::AppHandle;

#[cfg(windows)]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};
    use tauri::{AppHandle, Manager};
    use tauri_winrt_notification::{Duration as ToastDuration, Toast};
    use tungstenite::client::IntoClientRequest;
    use tungstenite::http::{header, HeaderValue};
    use tungstenite::{connect, Error as WsError, Message};

    use crate::logging;
    use crate::shell::Shell;

    /// 通知使用的 AppUserModelID。
    ///
    /// **必须与 `tauri.conf.json` 的 `identifier` 完全一致**：
    /// 便携 exe 靠 [`ensure_toast_aumid`] 把它写进 HKCU，
    /// 两边对不上 Windows 一样会静默丢弃 toast。
    pub const TOAST_AUMID: &str = "com.dshshell.desktop";

    /// 连接存活多久才算"确实连上了"（而不是对面接受后立刻关闭）。
    ///
    /// 退避计时器只在这个时长之后才重置，见 [`run`]。
    const MIN_STABLE_ALIVE: Duration = Duration::from_secs(5);

    /// 重连退避上限（毫秒）。指数为 500·2^n，n 封顶 4，因此实际峰值就是 8 秒。
    const BACKOFF_CAP_MS: u64 = 8_000;

    /// HTTP（token 交换、RPC、弃权回应）的超时。
    const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

    /// 读空闲超时。服务器默认每 2 秒发一次 WebSocket Ping，空闲这么久
    /// 只能说明连接已经死了（或心跳被配置禁用）——当成断线处理，重连无害。
    const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

    /// 事件流监听器的可变状态。
    #[derive(Default)]
    struct Listener {
        /// 每个会话上一次已知的 `running` 值。
        ///
        /// 外层 `Option` 表示"还没有基线"——`None` 时不判边沿。
        baseline: HashMap<String, Option<bool>>,
        /// 会话展示标题缓存（快照播种与 `added` 事件维护）。
        titles: HashMap<String, String>,
        /// 本次事件流的 clientId（ready 帧给出），waterfall 弃权回应要靠它路由。
        client_id: Option<String>,
        /// 已经记过日志的未知事件类型（去重，见 [`Listener::note_unknown_type`]）。
        logged_unknown: HashSet<String>,
    }

    /// 同时被事件流线程与测试访问的共享状态
    type Shared = Mutex<Listener>;

    /// 最多记录多少种未知事件类型。
    ///
    /// 给去重集合本身也加个上限：万一对面每帧都换一个新名字
    /// （协议变更、或者干脆是个乱发东西的服务），
    /// 集合不能跟着无限长大。
    const UNKNOWN_TYPE_LOG_LIMIT: usize = 100;

    impl Listener {
        /// 记下一个未知事件类型，返回"是不是第一次见"（也就是**该不该写日志**）。
        ///
        /// 抽成方法是为了能直接单测 —— 事件处理主链路需要 `AppHandle`，
        /// 在单元测试里构造不出来。
        fn note_unknown_type(&mut self, kind: &str) -> bool {
            if self.logged_unknown.len() >= UNKNOWN_TYPE_LOG_LIMIT {
                return false;
            }
            // `HashSet::insert` 返回 false 表示已存在，正好就是"不是第一次见"
            self.logged_unknown.insert(kind.to_string())
        }
    }

    /// 注册 AppUserModelID —— 便携 exe 的 toast 能否显示全看这一步。
    ///
    /// Windows 只为"已注册 AUMID"的进程显示 toast。正常途径是安装器
    /// 建一个带 AUMID 的开始菜单快捷方式，而本程序**刻意不做安装器**
    /// （绿色便携是既定设计），所以走微软文档给出的注册表替代方案。
    ///
    /// 幂等；失败只降级为"通知可能不显示"，绝不阻断程序启动。
    pub fn ensure_toast_aumid() {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;

        let Ok(exe) = std::env::current_exe() else {
            logging::warn(
                "无法取得程序路径，通知功能的身份注册已跳过",
                "后果：任务完成时可能不弹通知",
            );
            return;
        };

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu.create_subkey(format!(r"Software\Classes\AppUserModelId\{TOAST_AUMID}"));
        let (key, _) = match key {
            Ok(k) => k,
            Err(e) => {
                logging::warn(
                    "通知身份注册失败",
                    format!("{e}（后果：任务完成时可能不弹通知）"),
                );
                return;
            }
        };

        // `current_exe()` 可能带 `\\?\` 前缀，而 Windows 不接受这种形式
        let path = exe.display().to_string();
        let path = path.strip_prefix(r"\\?\").unwrap_or(&path);

        let _ = key.set_value("DisplayName", &"DSH Shell");
        match key.set_value("IconUri", &path) {
            Ok(()) => logging::info("通知身份已注册", TOAST_AUMID.to_string()),
            Err(e) => logging::warn(
                "通知图标路径写入失败",
                format!("{e}（通知仍可显示，只是没有图标）"),
            ),
        }
    }

    /// 弹一条通知。
    ///
    /// `with_open_action` 为真时附带「打开窗口」按钮，点击即把窗口唤到前台
    /// ——通知的价值有一半在于"点一下就能过去看"。
    pub fn toast(app: &AppHandle, title: &str, body: &str, with_open_action: bool) {
        let mut builder = Toast::new(TOAST_AUMID)
            .title(title)
            .text1(body)
            // 用 Short：通知横幅会自动收进操作中心，不长期霸占屏幕——
            // 用户嫌吵时也更可能只是无视而不是去关掉整个功能。
            .duration(ToastDuration::Short);

        if with_open_action {
            let handle = app.clone();
            // 「明白」按钮不额外处理：任意操作按钮点击都会收起横幅
            builder = builder
                .add_button("打开窗口", "open")
                .add_button("知道了", "ack")
                .on_activated(move |action| {
                    if action.as_deref() == Some("open") {
                        crate::tray::show_main_window(&handle);
                    }
                    Ok(())
                });
        }

        match builder.show() {
            Ok(()) => logging::info("已发送系统通知", body.to_string()),
            Err(e) => logging::warn("发送系统通知失败", e.to_string()),
        }
    }

    /// 常驻线程：连上 `$events` 事件流，在会话 `running` 由真变假时通知。
    ///
    /// 断线后按指数退避重连（0.5s → 最高 8s）。后端重启、切换端口、
    /// 用户手动停掉 DSH —— 这些都会让连接断开，但都不该让通知功能永久失效，
    /// 所以这里选择"一直重试"而不是"失败一次就退出"。
    pub fn run(app: AppHandle) {
        let state: Shared = Mutex::new(Listener::default());
        let mut attempt: u32 = 0;
        let mut logged_no_token = false;

        loop {
            // 端口与 token 每轮都现取：换端口、后端重启换代后自动跟上
            let (port, token) = app
                .try_state::<Shell>()
                .map(|s| (s.settings().port, s.current_token()))
                .unwrap_or((crate::settings::DEFAULT_PORT, None));

            let Some(token) = token else {
                // 拿不到 token：后端不是我们拉起的、或还没走到 token 阶段。
                // 事件流必然连不上，安静等待即可（只记一次，避免刷屏）。
                if !logged_no_token {
                    logging::info("暂无后端访问 token，事件流等待中", "后端就绪后自动开始监听");
                    logged_no_token = true;
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            logged_no_token = false;

            let started = Instant::now();
            let outcome = connect_and_listen(&app, &state, port, &token);
            let alive_for = started.elapsed();

            // 只有"连上并稳定存活过一段时间"才把退避清零。
            //
            // 对面一旦"接受连接后立刻关闭"，每轮都会把计时器归零的话，
            // 重连就永远停在 500ms —— 既刷日志又白耗 CPU。
            if alive_for >= MIN_STABLE_ALIVE {
                attempt = 0;
            }

            let exp = attempt.min(4);
            let delay_ms = (500u64 * 2u64.pow(exp)).min(BACKOFF_CAP_MS);

            match outcome {
                // 正常关闭（例如后端退出时 socket 优雅关闭）
                Ok(()) => logging::info(
                    "会话事件流已断开",
                    format!("将在 {:.1} 秒后重连", delay_ms as f64 / 1000.0),
                ),
                Err(e) => {
                    // 启动期"DSH 还没起来"是正常状态，不必每次都记；
                    // 但**首次**与**此后每 10 次**失败都要带详情落日志——
                    // 只记第一次的话，重连阶段真正出问题时会完全无声。
                    if attempt == 0 || attempt % 10 == 0 {
                        logging::info(
                            "暂时连不上会话事件流",
                            format!("第 {attempt} 次尝试失败：{e}（后端就绪后会自动连上）"),
                        );
                    }
                }
            }

            // 重连后 [`seed_baseline`] 会重建基线，这里先清掉旧值，
            // 防止连接失败路径上残留的旧基线被误用。
            //
            // 注意**不清理** `logged_unknown`：每种未知事件类型在整个进程
            // 生命周期里只该记一次日志，重连不该让它再刷一遍。
            {
                let mut l = state.lock().unwrap_or_else(|e| e.into_inner());
                l.baseline.clear();
                l.titles.clear();
                l.client_id = None;
            }

            attempt = attempt.saturating_add(1);
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }

    /// 完整建链：token 换 cookie → 快照播种 → 连 WS → 打开 `$events` 流 →
    /// 读到断开为止。
    fn connect_and_listen(
        app: &AppHandle,
        state: &Shared,
        port: u16,
        token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cookie = exchange_cookie(port, token)?;
        seed_baseline(state, port, &cookie)?;

        let origin = format!("http://127.0.0.1:{port}");
        let url = format!("ws://127.0.0.1:{port}/api/remote.mux");

        let mut request = url.as_str().into_client_request()?;
        // `/api/remote.mux` 的升级请求同样过同源围栏：Origin 必须与 Host 一致
        // （非浏览器客户端不带 `sec-fetch-site`，补 `Origin` 即可通过），
        // cookie 是上面用 token 换来的那张——少了就是 401。
        request
            .headers_mut()
            .insert(header::ORIGIN, HeaderValue::from_str(&origin)?);
        request
            .headers_mut()
            .insert(header::COOKIE, HeaderValue::from_str(&cookie)?);

        let (mut socket, _response) = connect(request)?;
        // 见 READ_IDLE_TIMEOUT 的说明：心跳停止 = 连接已死。
        // `connect()` 返回 TLS 包装流，要穿透到内层 TcpStream 才能设超时。
        if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_ref() {
            tcp.set_read_timeout(Some(READ_IDLE_TIMEOUT))?;
        }

        // 打开 $events 逻辑流。endpoint 与 payload 的形状是 Gateway 写死的
        // 校验规则（args 必须是**空对象**），发错形状直接
        // `gateway/arguments-invalid`。
        let stream_id = next_rpc_id();
        let open = json!({
            "type": "open",
            "streamId": stream_id,
            "endpoint": "$events",
            "payload": { "args": {} }
        });
        socket.write(Message::Text(open.to_string()))?;
        socket.flush()?;
        logging::info("已连接会话事件流", format!("127.0.0.1:{port}（$events 已请求）"));

        loop {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    if !handle_frame(app, state, &text, &stream_id, port, &cookie) {
                        return Ok(());
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        if !handle_frame(app, state, text, &stream_id, port, &cookie) {
                            return Ok(());
                        }
                    }
                }
                Ok(Message::Ping(payload)) => {
                    // 库层可能已自动回 Pong，但显式回一次无害且不依赖版本行为
                    socket.write(Message::Pong(payload))?;
                    socket.flush()?;
                }
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(e) if is_read_idle(&e) => {
                    return Err("事件流空闲超时（服务器心跳停止）".into());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// 读空闲超时对应的错误形态。
    fn is_read_idle(e: &WsError) -> bool {
        matches!(
            e,
            WsError::Io(io)
                if io.kind() == std::io::ErrorKind::WouldBlock
                    || io.kind() == std::io::ErrorKind::TimedOut
        )
    }

    /// 用 launch token 换取会话 cookie，返回 `Set-Cookie` 里的 `名字=值` 对。
    ///
    /// DSH 只在 `GET /` 上接受 token（`authorizeIndex`：方法 GET、路径恰好
    /// `/`、恰好一个 token 参数），成功时 303 + `Set-Cookie`。
    /// **禁用跟随重定向**——cookie 在 303 那一跳上，跟过去就丢了。
    fn exchange_cookie(port: u16, token: &str) -> Result<String, Box<dyn std::error::Error>> {
        let origin = format!("http://127.0.0.1:{port}");
        let url = format!("{origin}/?token={token}");
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let response = match agent.get(&url).set("Origin", &origin).timeout(HTTP_TIMEOUT).call() {
            // redirects(0) 时 3xx 也可能以 Ok 分支返回，两种路径都要接
            Ok(resp) => resp,
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(e) => return Err(format!("token 交换请求失败：{}", sanitize_ureq(&e)).into()),
        };
        let Some(set_cookie) = response.header("set-cookie") else {
            return Err("token 交换未返回 Set-Cookie（token 无效或已过期？）".into());
        };
        cookie_pair(set_cookie)
            .ok_or_else(|| format!("无法解析 Set-Cookie：{set_cookie}").into())
    }

    /// 从 Set-Cookie 头取 `名字=值`（分号前的第一段）。
    fn cookie_pair(set_cookie: &str) -> Option<String> {
        let pair = set_cookie.split(';').next()?.trim();
        if pair.is_empty() || !pair.contains('=') {
            return None;
        }
        Some(pair.to_string())
    }

    /// 拉取会话列表快照，返回条目数组。
    ///
    /// 一元 RPC 的线格式：端点是 `session/list`（不是 `session.list`），
    /// payload 必须是 `{"args": {"_request": {}}}`（`_request` 是 descriptor
    /// 定义的线上参数名，`cursor` 可省略），结果包在
    /// `{type:"server-response", rpcId, result:{ok, value:{items:[…]}}}` 里。
    fn session_list(port: u16, cookie: &str) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        let origin = format!("http://127.0.0.1:{port}");
        let body = json!({
            "type": "client-request",
            "rpcId": next_rpc_id(),
            "method": "session/list",
            "payload": { "args": { "_request": {} } }
        });
        let response = ureq::post(&format!("{origin}/api/session/list"))
            .set("Origin", &origin)
            .set("Cookie", cookie)
            .timeout(HTTP_TIMEOUT)
            .send_json(body)?;
        let value: Value = response.into_json()?;
        if value.pointer("/result/ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(format!("session/list 调用失败：{}", truncate_json(&value)).into());
        }
        Ok(value
            .pointer("/result/value/items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// 用快照给基线播种（标题一并缓存）。
    ///
    /// `$events` 重连后**不重放**存量状态；不播种就会漏掉
    /// "连接之前就在跑、之后跑完"的会话。播种必须在打开事件流**之前**
    /// 完成，保证基线先于增量事件就位。
    fn seed_baseline(state: &Shared, port: u16, cookie: &str) -> Result<(), Box<dyn std::error::Error>> {
        let items = session_list(port, cookie)?;
        let mut l = state.lock().unwrap_or_else(|e| e.into_inner());
        l.baseline.clear();
        l.titles.clear();
        for item in &items {
            let Some(id) = item.get("sessionId").and_then(|v| v.as_str()) else {
                continue;
            };
            let running = item.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
            l.baseline.insert(id.to_string(), Some(running));
            if let Some(title) = display_title(item) {
                l.titles.insert(id.to_string(), title);
            }
        }
        Ok(())
    }

    /// 处理一帧 mux 消息；返回 false 表示事件流已结束（该重连了）。
    fn handle_frame(
        app: &AppHandle,
        state: &Shared,
        text: &str,
        stream_id: &str,
        port: u16,
        cookie: &str,
    ) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return true;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("item") => {
                if value.get("streamId").and_then(|v| v.as_str()) != Some(stream_id) {
                    return true; // 不是我们的逻辑流（防御：本程序只开这一条）
                }
                match value.get("value") {
                    Some(item) => handle_stream_item(app, state, item, port, cookie),
                    None => true,
                }
            }
            // 服务端正常收流
            Some("end") => false,
            Some("error") => {
                let code = value
                    .pointer("/error/code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let msg = value
                    .pointer("/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                logging::warn("事件流被服务端报错关闭", format!("{code}: {msg}"));
                false
            }
            // ping/pong、未知帧忽略
            _ => true,
        }
    }

    /// 处理一帧逻辑流内容（mux `item.value`）。
    fn handle_stream_item(app: &AppHandle, state: &Shared, item: &Value, port: u16, cookie: &str) -> bool {
        match item.get("type").and_then(|t| t.as_str()) {
            // 首帧：携带本次事件流代次的 clientId，waterfall 弃权要靠它路由
            Some("ready") => {
                if let Some(id) = item.get("clientId").and_then(|v| v.as_str()) {
                    let mut l = state.lock().unwrap_or_else(|e| e.into_inner());
                    l.client_id = Some(id.to_string());
                }
                true
            }
            Some("emit") => {
                let Some(event) = item.get("event").and_then(|v| v.as_str()) else {
                    return true;
                };
                let args = item
                    .get("args")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if let Some(finished) = handle_emit(state, event, &args) {
                    on_session_finished(app, state, port, cookie, &finished);
                }
                true
            }
            // 旁观者义务：审批等 waterfall 必须立刻弃权，否则真实浏览器端
            // 的流程会被我们卡死（见模块文档）
            Some("waterfall") => {
                abstain_waterfall(state, port, cookie, item);
                true
            }
            // 服务端撤销某次 waterfall，无需处理
            Some("cancel") => true,
            _ => true,
        }
    }

    /// 处理一条 emit 事件，返回"要通知完成的会话 id"（`None` = 不通知）。
    ///
    /// 抽成无 `AppHandle` 的纯逻辑是为了可直接单测边沿判定。
    fn handle_emit(state: &Shared, event: &str, args: &[Value]) -> Option<String> {
        match event {
            // 签名（dsh-api-session-controller）：'api-session/status'(sessionId, running)
            "api-session/status" => {
                let id = args.first()?.as_str()?;
                let running = args.get(1)?.as_bool()?;
                edge_detect(state, id, running)
            }
            // 签名：'api-session/added'(summary)，summary 里有 sessionId/running/投影
            "api-session/added" => {
                let summary = args.first()?;
                let id = summary.get("sessionId").and_then(|v| v.as_str())?;
                let running = summary
                    .get("running")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mut l = state.lock().unwrap_or_else(|e| e.into_inner());
                l.baseline.insert(id.to_string(), Some(running));
                if let Some(title) = display_title(summary) {
                    l.titles.insert(id.to_string(), title);
                }
                None
            }
            // 签名：'api-session/removed'(sessionId)
            "api-session/removed" => {
                let id = args.first()?.as_str()?;
                let mut l = state.lock().unwrap_or_else(|e| e.into_inner());
                l.baseline.remove(id);
                l.titles.remove(id);
                None
            }
            other => {
                // 已知但不需要处理的类型直接略过，省掉一次加锁
                if is_known_ignored(other) {
                    return None;
                }
                // 未知类型只记**一次**，用于 DSH 升级后排查事件格式变化
                let first_sighting = {
                    let mut l = state.lock().unwrap_or_else(|e| e.into_inner());
                    l.note_unknown_type(other)
                };
                if first_sighting {
                    logging::info("收到未处理的事件类型", format!("{other}（同类型只记这一次）"));
                }
                None
            }
        }
    }

    /// true→false 边沿检测。返回 `Some(id)` 表示该通知"任务完成"。
    ///
    /// `previous` 为 `None` 说明这条只是建立基线（快照播种或首次见到），
    /// 不是迁移，绝不触发。
    fn edge_detect(state: &Shared, id: &str, running: bool) -> Option<String> {
        let mut l = state.lock().unwrap_or_else(|e| e.into_inner());
        let previous = l.baseline.get(id).copied().flatten();
        l.baseline.insert(id.to_string(), Some(running));
        if previous == Some(true) && !running {
            Some(id.to_string())
        } else {
            None
        }
    }

    /// 对一次 waterfall 事件回"弃权"。
    ///
    /// outcome 只能是 `next`：`result`/`rejected` 会立刻替浏览器定夺审批，
    /// `next` 只表示"我不参与"，让 Gateway 继续等真实客户端回应。
    /// 失败只记日志不断流——错过一次弃权最多让对面的结算多等一位参与者。
    fn abstain_waterfall(state: &Shared, port: u16, cookie: &str, item: &Value) {
        let client_id = {
            let l = state.lock().unwrap_or_else(|e| e.into_inner());
            l.client_id.clone()
        };
        let Some(client_id) = client_id else { return };
        let Some(event_id) = item.get("eventId").and_then(|v| v.as_str()) else {
            return;
        };
        let payload = abstain_payload(&client_id, event_id);
        let origin = format!("http://127.0.0.1:{port}");
        let result = ureq::post(&format!("{origin}/api/$events/result"))
            .set("Origin", &origin)
            .set("Cookie", cookie)
            .timeout(HTTP_TIMEOUT)
            .send_json(payload);
        if let Err(e) = result {
            logging::warn("waterfall 弃权回应失败", sanitize_ureq(&e));
        }
    }

    /// 构造弃权回应的线格式（独立成纯函数以便单测）。
    fn abstain_payload(client_id: &str, event_id: &str) -> Value {
        json!({
            "type": "client-request",
            "rpcId": next_rpc_id(),
            "method": "$events/result",
            "payload": {
                "clientId": client_id,
                "eventId": event_id,
                "outcome": { "kind": "next" }
            }
        })
    }

    /// 明确知道但不需要处理的转发事件（`dsh-api-remotes` 白名单的其余部分）。
    ///
    /// 名单来自 `API_REMOTE_FORWARDED_EVENTS`——不在名单里的才会走到
    /// "未知类型"日志，正好把 DSH 升级新增事件的信号暴露出来。
    fn is_known_ignored(event: &str) -> bool {
        matches!(
            event,
            "agent-preset/selected"
                | "approval/request" // waterfall 模式，正常以 waterfall 帧出现，防御性列出
                | "api-session/activity"
                | "api-session/error"
                | "commands/change"
                | "credentials/reference-updated"
                | "goal/activation-changed"
                | "cordis/request-run"
                | "cordis/request-run-resolved"
                | "cordis/dynamic-package"
                | "cordis/dynamic-retract"
                | "cordis/inspect-query"
                | "cordis/inspect-query-resolved"
                | "llm/adapters-updated"
                | "settings/document-updated"
                | "user-questions/request" // 同上，waterfall 模式
        )
    }

    /// 一个会话跑完了。
    fn on_session_finished(app: &AppHandle, state: &Shared, port: u16, cookie: &str, session_id: &str) {
        // 用户关掉了通知开关就到此为止（在用户可见的设置里可改）
        let enabled = app
            .try_state::<Shell>()
            .map(|s| s.settings().notify_on_finish)
            .unwrap_or(true);
        if !enabled {
            return;
        }

        // 用户正看着就不用打扰。三个条件缺一不可：
        // 只看 `is_visible()` 会漏掉"最小化"（最小化时它仍是 true），
        // 而没有 `is_focused` 则会在窗口被别的程序挡住时误判为"在看"。
        if let Some(window) = app.get_window(crate::chat::MAIN_WINDOW) {
            let visible = window.is_visible().unwrap_or(false);
            let minimized = window.is_minimized().unwrap_or(false);
            let focused = window.is_focused().unwrap_or(false);
            if visible && !minimized && focused {
                return;
            }
        }

        let title = cached_title(state, session_id)
            .or_else(|| fetch_title(port, cookie, session_id))
            .unwrap_or_else(|| "DSH 会话".to_string());
        logging::info(
            "会话已完成，发送通知",
            format!("{}（{session_id}）", title),
        );
        toast(app, "任务已完成", &title, true);
    }

    /// 从缓存取会话标题。
    fn cached_title(state: &Shared, session_id: &str) -> Option<String> {
        let l = state.lock().unwrap_or_else(|e| e.into_inner());
        l.titles.get(session_id).cloned()
    }

    /// 缓存未命中时从 `session/list` 现查标题（例如事件先于快照的竞态窗口）。
    fn fetch_title(port: u16, cookie: &str, session_id: &str) -> Option<String> {
        let items = session_list(port, cookie).ok()?;
        let item = items
            .iter()
            .find(|i| i.get("sessionId").and_then(|v| v.as_str()) == Some(session_id))?;
        display_title(item)
    }

    /// 从 session/list 条目提取展示标题，复刻浏览器端的三级兜底
    /// （`displayTitleOf`）：title 投影 → 工作目录末段 → 无。
    ///
    /// title 投影正常是字符串；防御一层对象形态（`{view}`）。
    fn display_title(item: &Value) -> Option<String> {
        match item.pointer("/projections/values/title") {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(Value::Object(o)) => {
                if let Some(v) = o.get("view").and_then(|v| v.as_str()) {
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
            _ => {}
        }
        let cwd = item.get("cwd").and_then(|v| v.as_str())?;
        let base = cwd.rsplit(['/', '\\']).find(|seg| !seg.is_empty())?;
        Some(base.to_string())
    }

    /// 进程内自增的请求 id（rpcId / streamId 共用；够用且免依赖）
    fn next_rpc_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("dsh-shell-{ms}-{n}")
    }

    /// ureq 错误的安全描述。
    ///
    /// ureq 的错误 Display 可能携带完整 URL——而我们的 URL 上有
    /// `?token=…`，绝不能原样进日志。
    fn sanitize_ureq(e: &ureq::Error) -> String {
        let text = e.to_string();
        match text.find("token=") {
            Some(at) => format!("{}…（token 已打码）", text[..at].trim_end()),
            None => text,
        }
    }

    /// 截断 JSON 调试串，防止把日志环形缓冲刷满。
    fn truncate_json(v: &Value) -> String {
        let text = v.to_string();
        if text.len() > 300 {
            format!("{}…", &text[..300])
        } else {
            text
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rpc_ids_are_unique() {
            assert_ne!(next_rpc_id(), next_rpc_id());
        }

        #[test]
        fn known_ignored_types_are_recognised() {
            assert!(is_known_ignored("api-session/activity"));
            assert!(is_known_ignored("settings/document-updated"));
            // 这三个是通知功能的核心输入，绝不能进忽略名单
            assert!(!is_known_ignored("api-session/status"));
            assert!(!is_known_ignored("api-session/added"));
            assert!(!is_known_ignored("api-session/removed"));
            // 未知类型必须返回 false，这样它才会被记进日志
            assert!(!is_known_ignored("brand-new/event"));
        }

        /// 未知事件类型只记一次。
        #[test]
        fn an_unknown_type_is_only_logged_once() {
            let mut l = Listener::default();
            assert!(l.note_unknown_type("brand-new/one"), "第一次见到应当记日志");
            assert!(!l.note_unknown_type("brand-new/one"), "第二次不该再记");
            assert!(!l.note_unknown_type("brand-new/one"), "之后同样不该再记");
            assert_eq!(l.logged_unknown.len(), 1, "同一种类型只应占一个位置");

            // 换一种新类型仍然要能记下来（否则就漏掉协议变更的信号了）
            assert!(l.note_unknown_type("brand-new/two"));
        }

        /// 去重集合自身也得有上限，防止对面不断换名字把它撑大。
        #[test]
        fn unknown_type_dedup_set_is_bounded() {
            let mut l = Listener::default();
            for i in 0..UNKNOWN_TYPE_LOG_LIMIT {
                assert!(l.note_unknown_type(&format!("type-{i}")));
            }
            assert_eq!(l.logged_unknown.len(), UNKNOWN_TYPE_LOG_LIMIT);

            // 满了之后再出现新类型：既不记日志，集合也不再增长
            assert!(!l.note_unknown_type("one-more"));
            assert_eq!(l.logged_unknown.len(), UNKNOWN_TYPE_LOG_LIMIT);
        }

        /// 边沿判定全生命周期：播种不算边沿、true→false 才通知、
        /// 重复 false 不通知、再跑一轮再完成再通知。
        #[test]
        fn completion_is_only_edge_true_to_false() {
            let state: Shared = Mutex::new(Listener::default());
            let id = "session-1";

            // 播种：false（空闲）——None→Some 不是边沿
            assert_eq!(edge_detect(&state, id, false), None);
            // 开跑：false→true 不通知
            assert_eq!(edge_detect(&state, id, true), None);
            // 跑完：true→false 通知
            assert_eq!(edge_detect(&state, id, false).as_deref(), Some(id));
            // 重复 false：不通知
            assert_eq!(edge_detect(&state, id, false), None);
            // 再跑一轮、再完成：再通知
            assert_eq!(edge_detect(&state, id, true), None);
            assert_eq!(edge_detect(&state, id, false).as_deref(), Some(id));
        }

        /// added 事件会建立基线并缓存标题；removed 会清掉两者。
        #[test]
        fn added_seeds_and_removed_cleans() {
            let state: Shared = Mutex::new(Listener::default());
            let summary = json!({
                "sessionId": "s-1",
                "running": true,
                "projections": { "values": { "title": "我的任务" } }
            });

            assert_eq!(handle_emit(&state, "api-session/added", &[summary]), None);
            {
                let l = state.lock().unwrap();
                assert_eq!(l.baseline.get("s-1"), Some(&Some(true)));
                assert_eq!(l.titles.get("s-1").map(String::as_str), Some("我的任务"));
            }

            // 建基线后跑完：应判为边沿（added 的 running=true 充当起点）
            let removed = json!("s-1");
            assert_eq!(edge_detect(&state, "s-1", false).as_deref(), Some("s-1"));
            assert_eq!(handle_emit(&state, "api-session/removed", &[removed]), None);
            let l = state.lock().unwrap();
            assert!(!l.baseline.contains_key("s-1"));
            assert!(!l.titles.contains_key("s-1"));
        }

        /// 缺参数 / 形状不对的事件帧必须被安全忽略，绝不 panic。
        #[test]
        fn malformed_emit_frames_are_ignored() {
            let state: Shared = Mutex::new(Listener::default());
            assert_eq!(handle_emit(&state, "api-session/status", &[]), None);
            assert_eq!(
                handle_emit(&state, "api-session/status", &[json!("s-1")]),
                None
            );
            assert_eq!(
                handle_emit(&state, "api-session/status", &[json!("s-1"), json!("yes")]),
                None
            );
            assert_eq!(handle_emit(&state, "api-session/removed", &[]), None);
            assert_eq!(handle_emit(&state, "api-session/added", &[json!({})]), None);
        }

        /// 弃权回应的线格式：method 指向 `$events/result`，outcome 只能是 next。
        #[test]
        fn abstain_payload_shape_is_next_only() {
            let p = abstain_payload("client-1", "event-9");
            assert_eq!(p["type"], "client-request");
            assert_eq!(p["method"], "$events/result");
            assert_eq!(p["payload"]["clientId"], "client-1");
            assert_eq!(p["payload"]["eventId"], "event-9");
            assert_eq!(p["payload"]["outcome"]["kind"], "next");
            // 绝不允许出现 result / rejected —— 那会替用户做决定
            assert!(p["payload"]["outcome"].get("value").is_none());
            assert!(p["payload"]["outcome"].get("error").is_none());
        }

        /// Set-Cookie 解析：取分号前的 `名字=值`。
        #[test]
        fn cookie_pair_is_parsed_from_set_cookie() {
            let raw = "dsh-auth-AbC=v1.eyJohnson.sig; Max-Age=2592000; Path=/; \
                       Expires=Wed, 30 Sep 2026 12:00:00 GMT; HttpOnly; SameSite=Strict";
            assert_eq!(
                cookie_pair(raw).as_deref(),
                Some("dsh-auth-AbC=v1.eyJohnson.sig")
            );
            // 无属性段、带空格的形态
            assert_eq!(cookie_pair(" a=b ").as_deref(), Some("a=b"));
            // 没有 = 就不是 cookie 对
            assert_eq!(cookie_pair("garbage"), None);
            assert_eq!(cookie_pair(""), None);
        }

        /// 展示标题的三级兜底：title 投影 → cwd 末段 → None。
        #[test]
        fn display_title_falls_back_to_cwd_basename() {
            // title 投影优先
            let with_title = json!({
                "cwd": r"D:\Work\我的项目",
                "projections": { "values": { "title": "重构通知" } }
            });
            assert_eq!(display_title(&with_title).as_deref(), Some("重构通知"));

            // 无 title 时取 cwd 末段（Windows 反斜杠）
            let cwd_only = json!({ "cwd": r"D:\Work\我的项目" });
            assert_eq!(display_title(&cwd_only).as_deref(), Some("我的项目"));

            // POSIX 斜杠也认
            let posix = json!({ "cwd": "/home/user/proj" });
            assert_eq!(display_title(&posix).as_deref(), Some("proj"));

            // 都没有 → None（调用方再兜底成通用标题）
            assert_eq!(display_title(&json!({ "sessionId": "x" })), None);
        }

        /// 读空闲（心跳停止）要被识别为断线，普通 IO 错误不要误伤。
        #[test]
        fn read_idle_is_detected() {
            let idle = WsError::Io(std::io::Error::new(std::io::ErrorKind::WouldBlock, "x"));
            let timed_out = WsError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "x"));
            let refused = WsError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "x"));
            assert!(is_read_idle(&idle));
            assert!(is_read_idle(&timed_out));
            assert!(!is_read_idle(&refused));
        }

        #[test]
        fn aumid_matches_tauri_identifier() {
            // 这两处一旦不一致，Windows 会静默丢弃所有 toast。
            // 用测试钉住，避免以后改 identifier 时漏改这里。
            let conf = include_str!("../tauri.conf.json");
            assert!(
                conf.contains(&format!("\"identifier\": \"{TOAST_AUMID}\"")),
                "notify::TOAST_AUMID 与 tauri.conf.json 的 identifier 不一致"
            );
        }
    }
}

#[cfg(windows)]
pub use imp::*;

// ---- 非 Windows 平台的空实现 -------------------------------------------------
//
// 依赖是 windows-only 的，这里提供同名空函数，调用点就不必到处写 cfg。

#[cfg(not(windows))]
pub fn ensure_toast_aumid() {}

#[cfg(not(windows))]
pub fn toast(_app: &AppHandle, _title: &str, _body: &str, _with_open_action: bool) {}

#[cfg(not(windows))]
pub fn run(_app: AppHandle) {}

/// 通知功能对外的统一入口：起一个后台线程跑事件流监听。
///
/// 抽成函数是为了让调用点（`lib.rs` 的 `setup`）保持一行，
/// 也方便在这里决定"要不要起线程"。
pub fn spawn(app: AppHandle) {
    #[cfg(windows)]
    {
        ensure_toast_aumid();
        std::thread::spawn(move || run(app));
    }
    #[cfg(not(windows))]
    {
        let _ = app;
    }
}
