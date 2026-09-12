//! 编排层 —— 后端的启动、等待、自愈与关闭。
//!
//! # 核心设计：代际号（generation）
//!
//! 参考项目用 4 个各自为政的原子变量
//! （`INTENTIONAL_STOP` / `QUICK_DEATHS` / `AUTH_WALL_HANDLED` / `BACKEND_RESTARTING`）
//! 来描述"当前在干什么"。多个流程（启动、自愈重启、用户手动重启、退出）交叉时，
//! 这些变量会互相矛盾——典型症状就是 CHANGELOG 里跨十几个版本反复出现的
//! 「卡在正在启动」「白屏」「死页」。
//!
//! 本项目用一个**单调递增的代际号**统一解决：
//!
//! - 每次"重新开始"（用户点重启、崩溃自愈、配置变更）都 `generation += 1`；
//! - 任何后台流程在**每一次可能阻塞的等待后**都检查
//!   `is_current(gen)`，发现自己是上一代就**立即静默退出**；
//! - 因此旧的启动线程绝不可能再去写状态、杀进程、或覆盖新线程的结果。
//!
//! 这比"用一堆 flag 描述反对关系"要可靠得多：flag 之间是 N² 的组合，
//! 而代际号只有"当前/过期"两种，天然互斥。

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};

use crate::lifecycle::{self, CandidateContext, SpawnedChild};
use crate::logging;
use crate::probe::{self, ProbeResult};
use crate::selfheal::{self, HealOutcome, LockProbe};
use crate::settings::Settings;
use crate::state::{BackendState, FailureReason, HealPolicy, StartPhase};
use crate::token;

/// 等待服务就绪的总时长上限。
///
/// 给得比较宽：npx 兜底路径首次运行要下载整包（分钟级）。
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// 等待 token 出现的时长上限
const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
/// 自愈重启前的退避时长
const HEAL_BACKOFF: Duration = Duration::from_secs(2);

/// 当前由本程序拉起的 DSH 进程 PID（0 表示没有）。
///
/// # 为什么要冗余记一份
///
/// 真正的记录在 `Inner::child` 里，但那是锁保护的。而
/// [`kill_owned_child_panic_safe`] 要在**进程因 panic 终止之前**收掉子进程，
/// 那个时刻很可能正是"持有 `Inner` 锁的线程在 panic" —— 再去抢同一把锁就是死锁，
/// 而死锁比直接终止更糟：进程挂在那里不退出，托盘点不动、端口也一直占着。
///
/// 原子量没有这个问题，读它永不阻塞。代价只是多维护一个字段。
static OWNED_CHILD_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 记录/清除当前托管的子进程 PID（见 [`OWNED_CHILD_PID`]）
fn note_owned_child(pid: Option<u32>) {
    OWNED_CHILD_PID.store(pid.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
}

/// 供 panic hook 调用的收尾：**只依赖原子量，绝不加任何锁**。
///
/// 背景见 `lib.rs` 的 `install_panic_cleanup`：release 是 `panic = "abort"`，
/// 不走栈展开，`RunEvent::Exit` 与 `cleanup_on_exit` 都不会执行，
/// 于是 DSH 子进程会变成孤儿并继续占着端口。这是唯一还能收尾的时机。
pub fn kill_owned_child_panic_safe() {
    let pid = OWNED_CHILD_PID.swap(0, std::sync::atomic::Ordering::SeqCst);
    if pid != 0 {
        // kill_tree 只是拉起 taskkill 进程，不碰任何锁
        lifecycle::kill_tree(pid);
    }
}

/// 编排层内部可变状态
struct Inner {
    /// 单一可信状态源
    backend: BackendState,
    /// 代际号
    generation: u64,
    /// 我们拉起的子进程（仅当 `owned == true`）
    child: Option<SpawnedChild>,
    /// 当前可用的访问 token（仅存内存，绝不落盘）
    token: Option<String>,
    /// 子进程拉起时刻，用于判定"快速崩溃"
    started_at: Option<Instant>,
    /// 已记录的快速崩溃次数
    quick_deaths: u32,
    /// 用户配置
    settings: Settings,
    /// 配置文件路径
    settings_path: PathBuf,
    /// 数据目录（日志、诊断导出都放这里）
    data_dir: PathBuf,
    /// 用户是否主动停止了后端（退出应用时用）
    shutting_down: bool,
    /// 复用外部实例模式下的访问地址（不带 token）。
    ///
    /// 依据：DSH 的 cookie 签名密钥持久化在 `DSH_HOME/auth/store.json`，
    /// WebView2 的登录态（cookie）跨后端进程存活——因此即使端口上的
    /// DSH 不是本程序拉起的（拿不到它的 launch token），只要 WebView2
    /// 里还有有效登录态，直接导航到不带 token 的首页即可进入。
    foreign_ready_url: Option<String>,
}

/// 编排层句柄，作为 Tauri 的托管状态
pub struct Shell {
    inner: Mutex<Inner>,
}

/// 加锁辅助：锁中毒（某线程 panic 时）不应让整个应用失效。
///
/// 本项目的 release profile 是 `panic = "abort"`，实际不会出现中毒；
/// 但测试与调试构建下会，因此这里统一用 `unwrap_or_else` 恢复。
fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shell {
    /// 构造。`exe_dir` 用于定位配置与日志目录（保证应用是绿色可搬移的）。
    pub fn new(exe_dir: PathBuf) -> Self {
        let data_dir = crate::settings::data_dir(&exe_dir);
        let settings_path = crate::settings::settings_path(&exe_dir);

        // 日志目录先就位，后续所有步骤都要往里写
        logging::init(&data_dir);

        let (settings, warning) = Settings::load(&settings_path);
        if let Some(w) = warning {
            logging::warn(w, settings_path.display().to_string());
        }
        logging::info(
            "DSH Shell 已启动",
            format!("配置目录: {}", data_dir.display()),
        );

        Self {
            inner: Mutex::new(Inner {
                backend: BackendState::Idle,
                generation: 0,
                child: None,
                token: None,
                started_at: None,
                quick_deaths: 0,
                settings,
                settings_path,
                data_dir,
                shutting_down: false,
                foreign_ready_url: None,
            }),
        }
    }

    /// 当前状态快照（给前端用）
    pub fn snapshot(&self) -> BackendState {
        lock(&self.inner).backend.clone()
    }

    /// 数据目录
    pub fn data_dir(&self) -> PathBuf {
        lock(&self.inner).data_dir.clone()
    }

    /// 当前 token（供 webview 导航使用）
    pub fn current_token(&self) -> Option<String> {
        lock(&self.inner).token.clone()
    }

    /// 就绪时的访问地址。
    ///
    /// 存在的意义：后端可能比前端**更早就绪**（前端挂载需要时间），
    /// 那样 `shell://ready` 事件会被完全错过，前端就永远停在"启动中"。
    /// 快照里带上这个字段，前端首屏拉一次快照即可自恢复。
    pub fn ready_url(&self) -> Option<String> {
        let inner = lock(&self.inner);
        if !inner.backend.is_ready() {
            return None;
        }
        // 复用外部实例模式：地址由 mark_foreign_ready 直接给定（不带 token）
        if let Some(url) = &inner.foreign_ready_url {
            return Some(url.clone());
        }
        inner
            .token
            .as_ref()
            .map(|t| token::build_url(inner.settings.port, t))
    }

    /// 标记进入「复用外部实例」模式。
    ///
    /// 状态与自启动就绪相同（`Ready { owned: false }`），但访问地址由调用方
    /// 直接给定（**不带 token**）——登录态依赖 WebView2 里此前的持久 cookie。
    /// 同时清空 token：复用模式下既没有也拿不到本代 launch token，
    /// 事件流监听器会保持等待（任务完成通知在此模式不可用，属已知取舍；
    /// 要恢复请用「重启后端」，它会接管外部实例并由本程序自己拉起）。
    pub fn mark_foreign_ready(app: &AppHandle, shell: &Shell, url: String, pid: Option<u32>) {
        {
            let mut inner = lock(&shell.inner);
            inner.backend = BackendState::Ready { owned: false, pid };
            inner.foreign_ready_url = Some(url);
            inner.token = None;
            inner.started_at = None;
        }
        // 走统一的 set_state：托盘提示文字与前端状态广播都不缺
        set_state(app, shell, BackendState::Ready { owned: false, pid });
    }

    /// 监听端口
    pub fn port(&self) -> u16 {
        lock(&self.inner).settings.port
    }

    /// 读取配置副本
    pub fn settings(&self) -> Settings {
        lock(&self.inner).settings.clone()
    }

    /// 替换内存中的配置副本（磁盘写入由调用方负责，以便统一处理错误）
    pub fn replace_settings(&self, next: Settings) {
        let mut inner = lock(&self.inner);
        inner.settings = next;
    }

    /// 配置文件路径
    pub fn settings_path(&self) -> PathBuf {
        lock(&self.inner).settings_path.clone()
    }

    /// 标记"应用正在退出"，避免退出流程被自愈逻辑打断成重启
    pub fn begin_shutdown(&self) {
        let mut inner = lock(&self.inner);
        inner.shutting_down = true;
        // 代际号 +1，让所有在跑的启动/自愈线程立刻失效
        inner.generation = inner.generation.wrapping_add(1);
    }

    /// 杀掉我们拉起的子进程（只杀自己拉起的，绝不动用户的其它实例）
    pub fn kill_owned_child(&self) {
        let mut inner = lock(&self.inner);
        if let Some(mut c) = inner.child.take() {
            // 先清掉 panic 兜底那份记录，再动手（见 OWNED_CHILD_PID）
            note_owned_child(None);
            lifecycle::kill_tree(c.pid);
            let _ = c.child.wait();
        }
        inner.token = None;
        inner.started_at = None;
    }
}

/// 更新状态并广播给前端。
fn set_state(app: &AppHandle, shell: &Shell, next: BackendState) {
    {
        let mut inner = lock(&shell.inner);
        inner.backend = next.clone();
    }
    // 托盘提示文字跟着状态走：窗口藏进托盘时，悬停就能知道
    // 后端是"已就绪"还是"启动失败"，不必先把它叫出来。
    crate::tray::set_status(app, state_label(&next));
    // 广播失败不影响后端运行（前端可能尚未挂载）
    let _ = app.emit("shell://state", next);
}

/// 状态的一句话标签（用于托盘提示文字）
fn state_label(state: &BackendState) -> &'static str {
    match state {
        BackendState::Idle => "未启动",
        BackendState::Starting { .. } => "启动中",
        BackendState::Ready { .. } => "已就绪",
        BackendState::Healing { .. } => "自动恢复中",
        BackendState::Failed { .. } => "启动失败",
    }
}

/// 检查某代际是否仍然有效
fn is_current(shell: &Shell, gen: u64) -> bool {
    let inner = lock(&shell.inner);
    inner.generation == gen && !inner.shutting_down
}

/// 开始（或重启）后端。
///
/// `restart = true` 时会先杀掉我们自己拉起的子进程。
///
/// 注意签名只取 `AppHandle`：编排过程运行在独立线程上，而 `State<'_, Shell>`
/// 的生命周期绑定在调用栈上，无法直接送进 `thread::spawn`。
/// 因此线程内部通过 `app.state::<Shell>()` 重新取得句柄
/// （`Shell` 由 Tauri 托管，生命周期与应用一致）。
pub fn start_backend(app: &AppHandle, restart: bool) {
    let gen = {
        let shell = app.state::<Shell>();
        let mut inner = lock(&shell.inner);
        inner.generation = inner.generation.wrapping_add(1);
        inner.quick_deaths = 0;
        inner.token = None;
        inner.started_at = None;
        // 退出复用模式（若有）：接下来要么自己拉起、要么进入失败态，
        // 陈旧的复用地址不能残留在快照里
        inner.foreign_ready_url = None;
        // 重启场景下先收掉旧进程，避免端口占用
        if restart {
            if let Some(mut c) = inner.child.take() {
                note_owned_child(None);
                logging::info("正在停止当前 DSH 进程", format!("PID {}", c.pid));
                lifecycle::kill_tree(c.pid);
                let _ = c.child.wait();
            } else if let BackendState::Ready { owned: false, pid: Some(fpid) } =
                inner.backend.clone()
            {
                // 复用模式下点「重启后端」等于接管：收掉外部实例（连同子进程），
                // 等端口释放后由本程序自己拉起并取得 token
                logging::warn(
                    "正在接管：停止复用的 DSH 实例",
                    format!("PID {fpid}（连同其子进程）"),
                );
                lifecycle::kill_tree(fpid);
                let freed =
                    lifecycle::wait_listen_released(inner.settings.port, std::time::Duration::from_secs(6));
                if !freed {
                    logging::error(
                        "被接管的实例端口仍未释放",
                        format!("端口 {}：对方可能还在退出中，下一步可能报端口占用", inner.settings.port),
                    );
                }
            }
        }
        inner.generation
    };

    logging::info(
        if restart {
            "正在重启 DSH 服务"
        } else {
            "正在启动 DSH 服务"
        },
        // 代际号写进日志，排障时能一眼看出这行属于哪一轮编排
        format!("第 {gen} 代"),
    );

    let shell = app.state::<Shell>();
    set_state(
        app,
        &shell,
        BackendState::Starting {
            phase: StartPhase::Resolving,
            method: String::new(),
        },
    );
    drop(shell);

    let app = app.clone();
    std::thread::spawn(move || {
        // 在线程内重新取得句柄（见上方说明）
        let shell = app.state::<Shell>();
        // 出错已由状态机表达，无需返回 Result
        orchestrate(&app, &shell, gen);
    });
}

/// 启动编排主流程。
fn orchestrate(app: &AppHandle, shell: &Shell, gen: u64) {
    if !is_current(shell, gen) {
        return;
    }

    let port = shell.port();
    let settings = shell.settings();

    // ---- 阶段 1：解析候选链 -------------------------------------------------
    let ctx = CandidateContext {
        port,
        no_open: settings.no_open,
        custom_path: settings.custom_dsh_path.clone(),
        allow_npx: settings.allow_npx,
    };
    let candidates = lifecycle::build_candidates(&ctx);

    if candidates.is_empty() {
        logging::error(
            "没有找到可用的 DSH 命令",
            "候选链为空：PATH、自定义路径、本地 node_modules 均未命中",
        );
        fail(
            app,
            shell,
            FailureReason::DshNotFound {
                searched: vec!["系统 PATH".into()],
            },
        );
        return;
    }

    let names: Vec<String> = candidates.iter().map(|c| c.describe()).collect();
    logging::info(
        format!("找到 {} 个可用的启动方式", candidates.len()),
        names.join("\n"),
    );

    // ---- 阶段 2：预检（陈旧锁 / 端口占用）----------------------------------
    if !is_current(shell, gen) {
        return;
    }
    set_state(
        app,
        shell,
        BackendState::Starting {
            phase: StartPhase::Preflight,
            method: String::new(),
        },
    );
    preflight_locks();

    // 端口上是否已经有 DSH 在跑？
    if let ProbeResult::Responding(kind) = probe::probe(port, None) {
        if kind.is_dsh() {
            // 服务是好的，但它不是我们拉起的 —— 拿不到 token，开窗口只会是 401。
            //
            // 这里**不能**假装能接管：dsh 的 token 是进程内生成、只打印到
            // 自己 stdout 的（见 `FailureReason::ForeignDshRunning` 的说明），
            // 外部进程无法获取。参考项目此时静默回退裸 URL，用户看到 401
            // 却完全不知道发生了什么；本项目把实情和两个可行选择交给用户。
            let pid = lifecycle::find_listening_pid(port);
            logging::warn(
                "端口上已有一个 DSH 实例在运行，但它不是本程序启动的",
                match pid {
                    Some(p) => format!("端口 {port}，占用进程 PID {p}"),
                    None => format!("端口 {port}"),
                },
            );
            fail(
                app,
                shell,
                FailureReason::ForeignDshRunning { port, pid },
            );
            return;
        }
    }

    // 端口是否被**其它**服务占用（含"代理误答 502"的情况）。
    // 注意参考项目把任何 401/404/2xx 都当作就绪，会把代理错误页当成 DSH —— 本项目明确区分。
    if let Some(kind) = port_occupied_by_other(port) {
        logging::error(
            "端口已被其它程序占用",
            format!("端口 {port}：{}", kind.describe()),
        );
        fail(
            app,
            shell,
            FailureReason::PortOccupied { port, pid: None },
        );
        return;
    }

    // ---- 阶段 3：拉起子进程 -----------------------------------------------
    if !is_current(shell, gen) {
        return;
    }

    let mut spawned: Option<SpawnedChild> = None;
    let mut used_method = String::new();
    let mut last_error = String::new();

    for cand in &candidates {
        if !is_current(shell, gen) {
            return;
        }
        set_state(
            app,
            shell,
            BackendState::Starting {
                phase: StartPhase::Spawning,
                method: cand.method.clone(),
            },
        );
        match lifecycle::spawn(cand) {
            Ok(child) => {
                used_method = cand.method.clone();
                spawned = Some(child);
                break;
            }
            Err(e) => {
                last_error = format!("{} 启动失败：{e}", cand.describe());
                logging::warn(
                    format!("「{}」无法启动，尝试下一种方式", cand.method),
                    e.to_string(),
                );
            }
        }
    }

    let Some(child) = spawned else {
        fail(
            app,
            shell,
            FailureReason::DshNotFound {
                searched: vec![format!(
                    "{}\n最后错误：{last_error}",
                    candidates
                        .iter()
                        .map(|c| c.method.clone())
                        .collect::<Vec<_>>()
                        .join("\n")
                )],
            },
        );
        return;
    };

    let pid = child.pid;
    // **先**登记 panic 兜底记录，**再**入锁存 child（见 OWNED_CHILD_PID）。
    // 顺序反过来就会留一个空档：子进程已经起来了却还没登记，
    // 偏偏在这个瞬间 panic 的话，收尾就找不到它，孤儿照样占着端口。
    note_owned_child(Some(pid));
    {
        let mut inner = lock(&shell.inner);
        inner.child = Some(child);
        inner.started_at = Some(Instant::now());
    }

    // ---- 阶段 4：等待就绪（同时盯着子进程别偷偷死了）------------------------
    set_state(
        app,
        shell,
        BackendState::Starting {
            phase: StartPhase::WaitingReady,
            method: used_method.clone(),
        },
    );

    let deadline = Instant::now() + READY_TIMEOUT;
    let mut ready = false;
    while Instant::now() < deadline {
        if !is_current(shell, gen) {
            return;
        }

        // 子进程是否已经退出？
        if let Some(code) = poll_child_exit(shell) {
            let tail = logging::child_tail_snapshot();
            let reason = lifecycle::classify_failure(&tail);
            logging::error(
                format!("DSH 进程已退出（退出码 {:?}）", code),
                reason.summary(),
            );
            handle_unexpected_death(app, shell, gen, reason);
            return;
        }

        if let ProbeResult::Responding(kind) = probe::probe(port, None) {
            if kind.is_dsh() {
                ready = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(600));
    }

    if !ready {
        logging::error("等待服务就绪超时", format!("{READY_TIMEOUT:?}"));
        handle_unexpected_death(
            app,
            shell,
            gen,
            FailureReason::ReadyTimeout {
                seconds: READY_TIMEOUT.as_secs(),
            },
        );
        return;
    }

    // ---- 阶段 5：获取 token ----------------------------------------------
    set_state(
        app,
        shell,
        BackendState::Starting {
            phase: StartPhase::AcquiringToken,
            method: used_method.clone(),
        },
    );

    match lifecycle::wait_for_token(TOKEN_TIMEOUT) {
        Some(t) => {
            logging::info(
                "已取得访问凭证",
                format!("token={}", token::mask_token(&t)),
            );
            {
                let mut inner = lock(&shell.inner);
                inner.token = Some(t.clone());
                // 后端稳定跑起来了，重置崩溃计数
                inner.quick_deaths = 0;
            }
            set_state(
                app,
                shell,
                BackendState::Ready {
                    owned: true,
                    pid: Some(pid),
                },
            );
            // 成功路径也要落日志。之前只有失败才有记录，
            // 于是"到底跑起来了没有"在日志里看不出来，只能靠界面。
            let url = token::build_url(port, &t);
            logging::info(
                "服务已就绪",
                format!("PID {pid} · {}", token::mask_url(&url)),
            );
            // 注意：发给前端的必须是**完整** URL（它要拿去导航），
            // 打码只针对会落盘/被导出的日志。
            let _ = app.emit("shell://ready", url);
        }
        None => {
            // 特别重要的一种失败：**服务其实是好的**，只是缺凭证。
            // 用户看到的是 404/401，极易误判为"没启动"。
            let tail = logging::child_tail_snapshot();
            logging::error(
                "服务已就绪，但未能从输出中取得访问凭证",
                tail.lines().rev().take(5).collect::<Vec<_>>().join("\n"),
            );
            fail(
                app,
                shell,
                FailureReason::TokenMissing {
                    hint: "DSH 已在运行，但本程序没能在它的启动输出里读到访问凭证。\
                           这通常意味着 DSH 的输出格式有变化，可先点「导出诊断」反馈。"
                        .into(),
                },
            );
        }
    }
}

/// 非 Windows 上无进程句柄检查的意义；这里统一实现。
/// 返回 `Some(退出码)` 表示子进程已退出。
fn poll_child_exit(shell: &Shell) -> Option<Option<i32>> {
    let mut inner = lock(&shell.inner);
    let child = inner.child.as_mut()?;
    match child.child.try_wait() {
        Ok(Some(status)) => Some(status.code()),
        Ok(None) => None,
        // 查询失败按"已退出"处理，避免死等
        Err(_) => Some(None),
    }
}

/// 处理非预期退出：先尝试自愈，超出策略上限才进入失败终态。
fn handle_unexpected_death(
    app: &AppHandle,
    shell: &Shell,
    gen: u64,
    reason: FailureReason,
) {
    if !is_current(shell, gen) {
        return;
    }

    let policy = HealPolicy::default();
    let (alive_for, deaths) = {
        let mut inner = lock(&shell.inner);
        // 清掉已死的子进程句柄，避免它被后续逻辑当成"仍在运行"
        inner.child = None;
        inner.token = None;
        // 同步清掉 panic 兜底记录：进程已经死了，收尾时不该再去杀一次
        note_owned_child(None);
        let alive = inner
            .started_at
            .map(|t| t.elapsed())
            .unwrap_or_default();
        inner.quick_deaths = inner.quick_deaths.saturating_add(1);
        (alive, inner.quick_deaths)
    };

    // 存活时间够长 => 不是崩溃循环，计数归零后正常重启
    let is_quick = lifecycle::is_quick_death(alive_for, &policy);
    if !is_quick {
        let mut inner = lock(&shell.inner);
        inner.quick_deaths = 1;
    }

    let attempt = if is_quick { deaths } else { 1 };

    // 陈旧锁是个特例：它是**可自动修复**的，症状又最致命，
    // 因此这里在重启前再做一次修复尝试（预检时可能还没产生这个锁）。
    if matches!(reason, FailureReason::StaleLock { .. }) {
        let healed = preflight_locks();
        if healed {
            logging::info("已清理残留的锁文件，正在自动重试", "");
        }
    }

    if attempt > policy.max_quick_deaths {
        logging::error(
            format!("连续 {attempt} 次启动后失败，已停止自动重试"),
            reason.summary(),
        );
        fail(app, shell, reason);
        return;
    }

    logging::warn(
        format!(
            "DSH 意外退出，正在自动恢复（第 {attempt}/{} 次）",
            policy.max_quick_deaths
        ),
        reason.summary(),
    );

    set_state(
        app,
        shell,
        BackendState::Healing {
            attempt,
            max_attempts: policy.max_quick_deaths,
            last_error: reason.summary(),
        },
    );

    std::thread::sleep(HEAL_BACKOFF);
    if !is_current(shell, gen) {
        return;
    }

    // 自愈：沿用**同一代际号**继续跑（这不是"用户重启"，因此不清零计数）。
    //
    // 计数只在 `start_backend`（用户主动发起）里重置，所以自愈路径天然会
    // 累积 quick_deaths、最终停在上限 —— 不会陷入无限重启。
    // 这里刻意不复用 `start_backend`，因为那会 +1 代际号并把
    // `shutting_down` 之外的状态全部重置，反而丢掉了"这是第几次自愈"的语义。
    orchestrate(app, shell, gen);
}

/// 进入失败终态
fn fail(app: &AppHandle, shell: &Shell, reason: FailureReason) {
    // 终态必须进日志。
    //
    // 这是**所有**失败路径的唯一收口，在这里记一笔，日志时间线才不会
    // 停在半途（之前实测过：日志停在"正在通过…启动 DSH"，
    // 后面再无下文，事后完全看不出是失败结束了、还是程序被杀了）。
    logging::error(
        format!("启动失败：{}", reason.summary()),
        reason.detail(),
    );
    set_state(app, shell, BackendState::Failed { reason });
}

/// 判定端口上是否被**非 DSH** 的服务占用。
///
/// 之所以要"等一下再下结论"：自愈重启时，刚被杀掉的子进程可能还握着
/// 端口几百毫秒，此时探测会看到连接被拒或残留响应。若直接判定为
/// "端口被占用"，自愈就会在第一步误判成终态失败——参考项目里
/// 「重启后卡在启动中」的一类问题正来源于此。
///
/// 返回 `Some(kind)` 表示确认被非 DSH 占用。
fn port_occupied_by_other(port: u16) -> Option<crate::probe::ServiceKind> {
    const RECHECKS: u32 = 3;
    let mut last: Option<crate::probe::ServiceKind> = None;

    for i in 0..RECHECKS {
        match probe::probe(port, None) {
            ProbeResult::Responding(kind) if kind.is_dsh() => return None,
            ProbeResult::Responding(kind) => {
                last = Some(kind);
                // 还有机会：等一下看它会不会消失
                if i + 1 < RECHECKS {
                    std::thread::sleep(Duration::from_millis(1200));
                }
            }
            // 端口空着 / 超时 / 其它错误，都按"没被占用"处理
            _ => return None,
        }
    }
    last
}

/// 预检：清理所有已知 dsh home 下的陈旧锁。
///
/// 返回是否真的清理掉了至少一个锁。
///
/// 之所以要检查**多个** home：实测锁会同时出现在 `~/.dsh` 与项目工作目录，
/// 只查一个会漏。候选顺序：`DSH_HOME` 环境变量 → `~/.dsh` → 当前工作目录。
fn preflight_locks() -> bool {
    let mut healed_any = false;

    for home in dsh_homes() {
        match selfheal::probe_lock(&home) {
            LockProbe::Absent => {}
            LockProbe::Held { pid } => {
                logging::info(
                    format!("检测到 DSH 正在运行（PID {pid}），锁文件正常"),
                    home.display().to_string(),
                );
            }
            LockProbe::Stale { pid } => {
                logging::warn(
                    "检测到残留的锁文件，正在清理",
                    format!(
                        "路径 {}，原持有者 PID {:?}（该进程已不存在）",
                        home.display(),
                        pid
                    ),
                );
                match selfheal::heal_stale_lock(&home) {
                    HealOutcome::RemovedStaleLock { backup } => {
                        logging::info(
                            "已清理残留锁文件",
                            format!("原文件已备份为 {}", backup.display()),
                        );
                        healed_any = true;
                    }
                    HealOutcome::Failed { message } => {
                        logging::warn("清理残留锁文件失败", message);
                    }
                    HealOutcome::Nothing => {}
                }
            }
            LockProbe::Unrecognized { raw } => {
                logging::warn(
                    "锁文件内容无法识别，未做处理",
                    format!("{}：{raw}", home.display()),
                );
            }
        }
    }

    healed_any
}

/// 列出所有可能的 dsh home 目录
fn dsh_homes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    if let Some(h) = std::env::var_os("DSH_HOME") {
        out.push(PathBuf::from(h));
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        out.push(PathBuf::from(home).join(".dsh"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd);
    }

    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsh_homes_are_deduped() {
        let h = dsh_homes();
        let mut sorted = h.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(h.len(), sorted.len(), "不应有重复的 home 目录");
    }

    #[test]
    fn dsh_homes_includes_user_profile_dot_dsh() {
        // 测试环境有 USERPROFILE 时应包含 ~/.dsh
        if std::env::var_os("USERPROFILE").is_some() {
            let h = dsh_homes();
            assert!(
                h.iter().any(|p| p.ends_with(".dsh")),
                "应包含 ~/.dsh，实际 {h:?}"
            );
        }
    }

    #[test]
    fn shell_starts_idle() {
        let dir = std::env::temp_dir().join("dsh_shell_orch_test");
        let _ = std::fs::create_dir_all(&dir);
        let s = Shell::new(dir);
        assert!(matches!(s.snapshot(), BackendState::Idle));
        assert_eq!(s.port(), crate::settings::DEFAULT_PORT);
    }

    #[test]
    fn shutdown_invalidates_generation() {
        let dir = std::env::temp_dir().join("dsh_shell_orch_test2");
        let _ = std::fs::create_dir_all(&dir);
        let s = Shell::new(dir);
        let gen = lock(&s.inner).generation;
        s.begin_shutdown();
        assert!(!is_current(&s, gen), "退出后旧代际必须立即失效");
    }
}
