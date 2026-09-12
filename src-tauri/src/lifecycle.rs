//! 生命周期管理 —— 候选链、进程拉起、就绪等待、崩溃自愈。
//!
//! # 与参考项目的差异
//!
//! 1. **预检前置**：在拉起进程之前先检查陈旧锁（见 [`crate::selfheal`]），
//!    从源头避免"启动 → 崩溃 → 抓不到 token → 用户看到 404"这条链路。
//! 2. **每个候选都记录"可读的描述"**，失败时能告诉用户试过哪些路径，
//!    而不是只给一个错误码。
//! 3. **自愈状态显式上报**（`BackendState::Healing`），让用户在
//!    "正在自动恢复（第 n/3 次）"时知道系统在自救，而不是干等。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::logging;
use crate::state::{FailureReason, HealPolicy};
use crate::token;

/// 给子进程加上「不要新建控制台窗口」标志。
///
/// 本程序是 GUI 子系统（`windows_subsystem = "windows"`），从它启动的控制台程序
/// **默认会新建一个黑窗**并一闪而过。对 `dsh --version`、`tasklist`、`netstat`
/// 这类纯后台探测来说，闪窗比不做事还糟，所以每一处 `Command` 都必须过一遍这里。
///
/// 抽成一个函数、而不是在各调用点各写一遍 `creation_flags`，是因为**漏写是完全
/// 静默的**：代码照样编译、功能照样正确，只有用户会看到闪一下的黑框。
/// 这个函数本身就是为此补的 —— `probe_version`（每次启动都会执行）与
/// `selfheal::process_alive` 两处曾经就漏了，而其余四处各写了一遍。
///
/// `update::run_version` 同样调用它，那条路径上写着「闪一个 cmd 窗口比不检查还糟」。
#[cfg(windows)]
pub(crate) fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW`：进程以控制台程序启动，但不分配控制台
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// 非 Windows 无需处理（本项目实际只支持 Windows，留着是为了能编译）
#[cfg(not(windows))]
pub(crate) fn hide_console(_cmd: &mut Command) {}

/// 「问一句就退出」的探测超时。
///
/// `Command::output()` **没有超时**：对方一旦挂住（等 stdin、卡在启动脚本里、
/// 或者本身就是个交互式包装脚本），调用方就永远等下去。而本项目的两处用法
/// 都在关键路径上 —— `probe_version` 在**启动路径**（挂住 = 程序卡在启动），
/// `update::run_version` 在更新检查里（挂住 = 防重入标志永远置位，
/// 此后所有更新检查都被静默吞掉）。所以这两处都必须走带超时的版本。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 执行命令并收取输出，超过 `timeout` 就杀掉它并返回 `None`。
///
/// 用 `try_wait` 轮询而不是 `wait`：std 在 Windows 上没有 `wait_timeout`，
/// 而这里又不值得为了一次 `--version` 引入额外依赖。
///
/// **适用前提**：被调用命令的输出量很小（版本号、help 文本）。
/// 若对方会往 stdout 灌超过管道缓冲区（约 64KB）的数据，在没有持续读取的
/// 情况下它会阻塞在写上 —— 那种命令不能用这个函数。
pub(crate) fn output_with_timeout(
    cmd: &mut Command,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().ok()?;
    let mut child = child;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            // 已退出：把两个管道读干（进程已结束，不会再有写阻塞）
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // 超时必须 kill，否则留下一个永远不退的孤儿进程
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// 一个启动候选
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 面向用户的方法描述（如 "PATH 中的 dsh"）
    pub method: String,
    /// 可执行程序
    pub program: String,
    /// 参数
    pub args: Vec<String>,
    /// 工作目录
    pub cwd: Option<PathBuf>,
}

impl Candidate {
    /// 描述串，用于失败提示与诊断面板
    pub fn describe(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// 候选链构建所需的上下文
pub struct CandidateContext {
    /// 监听端口
    pub port: u16,
    /// 是否抑制自动打开浏览器
    pub no_open: bool,
    /// 用户自定义路径（settings 里配置的）
    pub custom_path: Option<String>,
    /// 是否允许使用 npx（需要用户显式同意过）
    pub allow_npx: bool,
}

/// 构建候选链。
///
/// 顺序与参考项目保持一致（本地优先），但每一项都带可读描述。
pub fn build_candidates(ctx: &CandidateContext) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();

    // 基础参数：端口 + 可选 --no-open
    let base_args = |extra: Vec<String>| -> Vec<String> {
        let mut a = vec!["web".to_string()];
        a.push("--port".into());
        a.push(ctx.port.to_string());
        if ctx.no_open {
            a.push("--no-open".into());
        }
        a.extend(extra);
        a
    };

    // 1) DSH_CMD 环境变量（最高优先级，供开发者/高级用户覆盖）
    if let Ok(cmd) = std::env::var("DSH_CMD") {
        let cmd = cmd.trim().to_string();
        if !cmd.is_empty() {
            // DSH_CMD 可能是 "pnpm dsh" 这样的多词命令
            let mut parts: Vec<String> = cmd.split_whitespace().map(String::from).collect();
            if !parts.is_empty() {
                let program = parts.remove(0);
                let mut args = parts;
                args.extend(base_args(vec![]));
                out.push(Candidate {
                    method: "DSH_CMD 环境变量".into(),
                    program,
                    args,
                    cwd: std::env::var("DSH_CWD").ok().map(PathBuf::from),
                });
            }
        }
    }

    // 2) 用户自定义路径
    if let Some(p) = ctx.custom_path.as_ref().filter(|s| !s.trim().is_empty()) {
        out.push(Candidate {
            method: "用户指定的路径".into(),
            program: p.clone(),
            args: base_args(vec![]),
            cwd: None,
        });
    }

    // 3) PATH 中的 dsh
    if let Some(p) = which("dsh") {
        out.push(Candidate {
            method: "系统 PATH 中的 dsh".into(),
            program: p,
            args: base_args(vec![]),
            cwd: None,
        });
    }

    // 4) 当前工作目录下的本地安装
    if let Some(p) = local_dsh_in_cwd() {
        out.push(Candidate {
            method: "当前目录的本地安装".into(),
            program: p,
            args: base_args(vec![]),
            cwd: None,
        });
    }

    // 5) npx（仅在用户同意过时）
    if ctx.allow_npx {
        out.push(Candidate {
            method: "npx（会临时下载，较慢）".into(),
            program: "npx".into(),
            args: {
                let mut a = vec!["--yes".to_string(), "@deepseek-ai/dsh".to_string()];
                a.extend(base_args(vec![]));
                a
            },
            cwd: None,
        });
    }

    out
}

/// 在 PATH 中查找**可被 CreateProcess 直接启动**的可执行文件。
///
/// 自己实现而不用 `which` crate：逻辑足够简单，省一个依赖。
///
/// # Windows 上必须优先带扩展名的候选（实测踩过的坑）
///
/// npm 全局安装会**同时**放下三个 shim：
///
/// | 文件 | 给谁用 |
/// |---|---|
/// | `dsh`（无扩展名） | Git Bash / WSL 的 POSIX sh 脚本 |
/// | `dsh.cmd` | cmd.exe / CreateProcess |
/// | `dsh.ps1` | PowerShell |
///
/// 早期实现"先查无扩展名再查 PATHEXT"，于是在本机上选中了那个 sh 脚本，
/// 启动时报 `os error 193（不是有效的 Win32 应用程序）`——
/// 而且因为脚本是 `is_file()`，代码还以为自己找对了。
///
/// 因此这里在 Windows 上**完全不考虑无扩展名的文件**：
/// 一个没有扩展名的文件在 Windows 上本来就无法直接执行。
fn which(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();

    #[cfg(windows)]
    {
        find_launchable(&dirs, name, &launchable_exts())
    }

    #[cfg(not(windows))]
    {
        for dir in &dirs {
            let direct = dir.join(name);
            if direct.is_file() {
                return Some(direct.to_string_lossy().into_owned());
            }
        }
        None
    }
}

/// 按"目录顺序优先、同目录内扩展名顺序优先"查找可启动文件。
///
/// 抽成独立函数是为了**可单测**：这是实际踩过坑的地方
/// （选中了 Git Bash 用的无扩展名 sh 脚本，启动报 os error 193），
/// 而 `which` 本身依赖真实环境变量，没法稳定断言。
#[cfg_attr(not(windows), allow(dead_code))]
fn find_launchable(dirs: &[PathBuf], name: &str, exts: &[String]) -> Option<String> {
    for dir in dirs {
        for ext in exts {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// 能够被 `CreateProcess` 直接启动的扩展名（小写，含前导点）。
///
/// 刻意**不直接用 PATHEXT 的全部内容**：PATHEXT 还包含 `.VBS` / `.JS` / `.WSF`，
/// 那些要靠 wscript 解释，`CreateProcess` 同样打不开，选进来等于换个报错。
/// 这里只保留真正能直接启动的四种。
///
/// 读取顺序仍尊重 PATHEXT：某些环境会把 `.CMD` 排在 `.EXE` 前面。
#[cfg(windows)]
fn launchable_exts() -> Vec<String> {
    const ALLOWED: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

    let from_env: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_default()
        .split(';')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| ALLOWED.contains(&s.as_str()))
        .collect();

    if from_env.is_empty() {
        // PATHEXT 缺失或被改坏时的兜底
        ALLOWED.iter().map(|s| s.to_string()).collect()
    } else {
        from_env
    }
}

/// 查找当前目录下本地安装的 dsh
fn local_dsh_in_cwd() -> Option<String> {
    let sub = if cfg!(windows) {
        "node_modules/.bin/dsh.cmd"
    } else {
        "node_modules/.bin/dsh"
    };
    let cwd = std::env::current_dir().ok()?;
    let p = cwd.join(sub);
    if p.is_file() {
        Some(p.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// 已拉起的子进程句柄
pub struct SpawnedChild {
    pub child: Child,
    pub pid: u32,
}

/// 拉起一个候选。
///
/// 子进程的 stdout/stderr 都会被泵入 [`crate::logging`] 的环形缓冲，
/// 这样崩溃时能拿到最后的输出用于诊断。
pub fn spawn(cand: &Candidate) -> std::io::Result<SpawnedChild> {
    logging::info(
        format!("正在通过「{}」启动 DSH", cand.method),
        cand.describe(),
    );

    // 拉起新一代前先清空子进程输出缓冲：里面残留着上一代的 token= 行，
    // token 每代随机，抓到旧的会导致事件流永远连不上（详见 clear_child_tail）。
    logging::clear_child_tail();

    let mut cmd = Command::new(&cand.program);
    cmd.args(&cand.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    if let Some(cwd) = &cand.cwd {
        cmd.current_dir(cwd);
    }

    // Windows 上隐藏控制台窗口
    hide_console(&mut cmd);

    let mut child = cmd.spawn()?;
    let pid = child.id();

    // 泵出 stdout / stderr
    if let Some(out) = child.stdout.take() {
        pump(out);
    }
    if let Some(err) = child.stderr.take() {
        pump(err);
    }

    Ok(SpawnedChild { child, pid })
}

/// 把一个可读流泵到环形缓冲
fn pump<R: std::io::Read + Send + 'static>(reader: R) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        let buf = std::io::BufReader::new(reader);
        for line in buf.lines() {
            match line {
                Ok(l) => logging::push_child_output(l),
                Err(_) => break,
            }
        }
    });
}

/// 等待子进程输出里出现 token。
///
/// `deadline` 是总超时。返回 `None` 表示超时未拿到。
pub fn wait_for_token(deadline: std::time::Duration) -> Option<String> {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let tail = logging::child_tail_snapshot();
        if let Some(t) = token::extract_token(&tail) {
            return Some(t);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

/// 结束进程树。
///
/// 之所以用 `taskkill /T`：候选链里 `npx`/`cmd` 会派生子进程，
/// 只杀直接子进程会留下孤儿 node 进程继续占着端口。
pub fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        hide_console(&mut cmd);
        let _ = cmd.output();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

/// 判定是否"快速崩溃"（存活时间过短）
pub fn is_quick_death(alive_for: std::time::Duration, policy: &HealPolicy) -> bool {
    alive_for.as_secs() < policy.quick_death_window_secs
}

/// 查出**正在监听**指定端口的进程 PID。
///
/// # 为什么用 `netstat -ano` 而不是 `Get-NetTCPConnection`
///
/// 后者要拉起 PowerShell：实测单次 1~2 秒，还受执行策略/首次加载模块影响；
/// `netstat` 是系统自带、单次 30ms 量级，输出格式在 Win10/11 上稳定。
/// 这里只需要"谁在监听"，netstat 完全够用。
///
/// 返回 `None` 表示查不到（端口空着、命令不可用、或非 Windows）。
#[cfg_attr(not(windows), allow(unused_variables))]
pub fn find_listening_pid(port: u16) -> Option<u32> {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("netstat");
        cmd.args(["-ano", "-p", "TCP"]);
        hide_console(&mut cmd);
        let out = cmd.output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        parse_netstat_pid(&text, port)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 从 `netstat -ano` 的输出里解析出 LISTENING 状态的 PID。
///
/// 目标行形如（列宽会随地址长度变化，所以按空白切分而非按列位置截取）：
///
/// ```text
///   TCP    127.0.0.1:3080         0.0.0.0:0              LISTENING       51436
///   TCP    [::1]:3080             [::]:0                 LISTENING       51436
/// ```
///
/// 拆成独立函数是**为了可单测**：真实 netstat 输出取决于机器当前状态，
/// 没法作为稳定断言。
fn parse_netstat_pid(text: &str, port: u16) -> Option<u32> {
    let suffix = format!(":{port}");
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // proto / local / foreign / state / pid
        if f.len() < 5 {
            continue;
        }
        if !f[0].eq_ignore_ascii_case("TCP") {
            continue;
        }
        if !f[1].ends_with(&suffix) {
            continue;
        }
        if !f[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        if let Ok(pid) = f[4].parse::<u32>() {
            return Some(pid);
        }
    }
    None
}

/// 等待端口不再被监听（用于"接管"后确认端口已释放）。
///
/// 返回 `true` 表示在 `timeout` 内释放了。
pub fn wait_listen_released(port: u16, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if find_listening_pid(port).is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    find_listening_pid(port).is_none()
}

/// 根据子进程尾部输出，推断结构化失败原因。
///
/// 这是与参考项目最大的差别之一：参考项目只把原始输出塞进消息里，
/// 用户在界面上看到的仍是一堆技术文本。这里做**签名识别**，
/// 转成能给出操作建议的结构化原因。
pub fn classify_failure(tail: &str) -> FailureReason {
    let lower = tail.to_ascii_lowercase();

    // 陈旧锁 —— 实际踩到的最致命故障
    if lower.contains("writer lock") || lower.contains("credentials.yaml.lock") {
        return FailureReason::StaleLock {
            path: extract_lock_path(tail)
                .unwrap_or_else(|| ".credentials.yaml.lock".into()),
            holder_pid: None,
        };
    }

    // profile bundle 缺失
    if lower.contains("cannot resolve profile bundle")
        || lower.contains("failed to resolve profile bundle")
    {
        return FailureReason::Other {
            message: "DSH 的插件包不完整，可能需要修复 profile（通常与网络或包管理冷却期有关）"
                .into(),
        };
    }

    if lower.contains("eaddrinuse") || lower.contains("address already in use") {
        return FailureReason::PortOccupied { port: 3080, pid: None };
    }

    FailureReason::Other {
        message: if tail.trim().is_empty() {
            "DSH 进程异常退出，且没有留下任何输出".into()
        } else {
            format!("DSH 进程异常退出：{}", first_meaningful_line(tail))
        },
    }
}

/// 从错误文本中提取锁文件路径
fn extract_lock_path(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(idx) = line.find(".credentials.yaml.lock") {
            // 往前找到路径起点
            let prefix = &line[..idx];
            if let Some(start) = prefix.rfind(|c: char| c.is_whitespace() || c == '"') {
                return Some(format!(
                    "{}.credentials.yaml.lock",
                    &line[start + 1..idx]
                ));
            }
            return Some(".credentials.yaml.lock".into());
        }
    }
    None
}

/// 取第一行有意义（非空、非纯符号）的输出
fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && l.chars().any(|c| c.is_alphanumeric()))
        .unwrap_or("(无输出)")
        .chars()
        .take(160)
        .collect()
}

/// 等待端口上出现响应
pub fn wait_for_ready(
    port: u16,
    deadline: std::time::Duration,
    mut should_abort: impl FnMut() -> bool,
) -> Option<crate::probe::ServiceKind> {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if should_abort() {
            return None;
        }
        if let crate::probe::ProbeResult::Responding(kind) = crate::probe::probe(port, None) {
            return Some(kind);
        }
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    None
}

/// 判断 dsh 是否支持 `--no-open`。
///
/// 参考项目用 `dsh web --help` 探测并把结果**永久缓存**在 settings.json，
/// 结果是升级 dsh 后缓存失效，行为异常且极难排查
/// （作者注释都写着"删除 settings.json 的 noOpenCache 即可重探"，即要用户手动删文件）。
///
/// 这里改为：**缓存键带上 dsh 版本**，版本一变就自动重探。
pub fn supports_no_open(dsh_program: &str, cache: &mut crate::settings::Settings) -> bool {
    // 先取版本，作为缓存键的一部分
    let version = probe_version(dsh_program);
    let key = match &version {
        Some(v) => format!("{dsh_program}@{v}"),
        None => dsh_program.to_string(),
    };

    if let Some(v) = cache.no_open_support.get(&key) {
        return *v;
    }

    let supported = run_help_mentions_no_open(dsh_program);
    cache.no_open_support.insert(key, supported);
    supported
}

/// 取 dsh 版本号（用于缓存键）
///
/// **每次启动都会执行** —— 缓存键要先拿到版本才能算，所以躲不开。
/// 因此这里两件事都不能出错：不能闪黑窗（[`hide_console`]），也不能卡住
/// （[`output_with_timeout`]）。原先这两样漏了，用户每次启动都会看到一闪而过的黑框。
fn probe_version(program: &str) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.arg("--version");
    hide_console(&mut cmd);

    let out = output_with_timeout(&mut cmd, PROBE_TIMEOUT)?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|s| s.to_string())
}

/// 执行 `<dsh> web --help` 并检查是否列出 `--no-open`
fn run_help_mentions_no_open(program: &str) -> bool {
    let mut cmd = Command::new(program);
    cmd.args(["web", "--help"]);
    hide_console(&mut cmd);

    // 超时（None）与执行失败同样按"探测不出支持"处理
    match output_with_timeout(&mut cmd, PROBE_TIMEOUT) {
        Some(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            text.contains("--no-open")
        }
        None => false,
    }
}

/// 确保目录存在
pub fn ensure_dir(p: &Path) -> std::io::Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_stale_lock_signature() {
        let tail = "Error: dsh: plugin tree failed to load: atomic-write: timed out \
                    waiting for the writer lock at C:\\Users\\x\\.dsh\\.credentials.yaml.lock";
        match classify_failure(tail) {
            FailureReason::StaleLock { path, .. } => {
                assert!(path.contains("credentials.yaml.lock"), "实际: {path}");
            }
            other => panic!("应识别为陈旧锁，实际 {other:?}"),
        }
    }

    #[test]
    fn detects_port_in_use_signature() {
        let r = classify_failure("Error: listen EADDRINUSE: address already in use :::3080");
        assert!(matches!(r, FailureReason::PortOccupied { .. }));
    }

    #[test]
    fn empty_tail_gives_explicit_message() {
        let r = classify_failure("   ");
        match r {
            // 强调"没有任何输出"这件事本身：这是崩溃循环里最常见、
            // 也最容易让人一头雾水的形态，必须给出明确措辞而不是空白。
            FailureReason::Other { message } => {
                assert!(message.contains("没有留下任何输出"), "实际: {message}")
            }
            other => panic!("实际 {other:?}"),
        }
    }

    #[test]
    fn unknown_failure_quotes_first_meaningful_line() {
        let r = classify_failure("some weird error 12345\nmore");
        match r {
            FailureReason::Other { message } => {
                assert!(message.contains("some weird error 12345"), "实际: {message}")
            }
            other => panic!("实际 {other:?}"),
        }
    }

    #[test]
    fn first_meaningful_line_skips_noise() {
        // 纯分隔线（`===`）里没有任何字母数字，属于噪声，应跳过
        let t = "\n\n   \n===\n真实错误信息\n更多";
        assert_eq!(first_meaningful_line(t), "真实错误信息");
    }

    #[test]
    fn first_meaningful_line_falls_back_when_all_noise() {
        assert_eq!(first_meaningful_line("\n \n===\n---\n"), "(无输出)");
    }

    #[test]
    fn first_meaningful_line_truncates() {
        let long = "x".repeat(500);
        assert_eq!(first_meaningful_line(&long).chars().count(), 160);
    }

    #[test]
    fn parses_netstat_listening_pid() {
        let text = "\
活动连接

  协议  本地地址          外部地址        状态           PID
  TCP    127.0.0.1:3080         0.0.0.0:0              LISTENING       51436
  TCP    127.0.0.1:1420         0.0.0.0:0              LISTENING       9001";
        assert_eq!(parse_netstat_pid(text, 3080), Some(51436));
        assert_eq!(parse_netstat_pid(text, 1420), Some(9001));
        assert_eq!(parse_netstat_pid(text, 8080), None);
    }

    #[test]
    fn netstat_ignores_non_listening_and_lookalike_ports() {
        // ESTABLISHED 不算监听
        let established =
            "  TCP    127.0.0.1:3080         127.0.0.1:55000        ESTABLISHED     777";
        assert_eq!(parse_netstat_pid(established, 3080), None);

        // 13080 不能因为后缀相同而被误匹配成 3080
        let lookalike = "  TCP    127.0.0.1:13080        0.0.0.0:0              LISTENING       555";
        assert_eq!(parse_netstat_pid(lookalike, 3080), None);
        assert_eq!(parse_netstat_pid(lookalike, 13080), Some(555));
    }

    #[test]
    fn netstat_handles_ipv6_binding() {
        let text = "  TCP    [::1]:3080             [::]:0                 LISTENING       4242";
        assert_eq!(parse_netstat_pid(text, 3080), Some(4242));
    }

    /// 复刻 npm 全局安装产生的目录布局，用于下面两个解析测试。
    fn npm_shim_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-which-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn win_exts() -> Vec<String> {
        [".com", ".exe", ".bat", ".cmd"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn prefers_extensioned_shim_over_posix_script() {
        // npm 全局安装会同时放 `dsh`（sh 脚本）、`dsh.cmd`、`dsh.ps1`
        let dir = npm_shim_dir("prefer");
        std::fs::write(dir.join("dsh"), "#!/bin/sh\nexec node \"$basedir/../...\"\n").unwrap();
        std::fs::write(dir.join("dsh.cmd"), "@ECHO off\r\n").unwrap();
        std::fs::write(dir.join("dsh.ps1"), "#!/usr/bin/env pwsh\r\n").unwrap();

        let found = find_launchable(std::slice::from_ref(&dir), "dsh", &win_exts())
            .expect("应命中带扩展名的 shim");
        // 关键断言：绝不能选中无扩展名那个（Windows 打不开，会报 os error 193）
        assert!(found.ends_with("dsh.cmd"), "实际选中: {found}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_select_unlaunchable_extensionless_file() {
        // 目录里**只有** sh 脚本时应该返回 None。
        // 选它只会把"没找到 dsh"变成用户更难懂的 `os error 193`。
        let dir = npm_shim_dir("noshim");
        std::fs::write(dir.join("dsh"), "#!/bin/sh\n").unwrap();

        assert!(find_launchable(std::slice::from_ref(&dir), "dsh", &win_exts()).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn respects_path_directory_order() {
        // 目录顺序优先于扩展名顺序：PATH 里靠前的目录先被采用
        let first = npm_shim_dir("order-first");
        let second = npm_shim_dir("order-second");
        std::fs::write(first.join("dsh.cmd"), "@ECHO off\r\n").unwrap();
        std::fs::write(second.join("dsh.exe"), "MZ").unwrap();

        let found = find_launchable(&[first.clone(), second.clone()], "dsh", &win_exts())
            .expect("应命中靠前目录里的 shim");
        assert!(found.ends_with("dsh.cmd"), "实际选中: {found}");

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    #[cfg(windows)]
    fn launchable_exts_excludes_script_host_types() {
        // .vbs/.js/.wsf 要靠 wscript 解释，CreateProcess 打不开，不能算"可启动"
        for e in launchable_exts() {
            assert!(
                [".com", ".exe", ".bat", ".cmd"].contains(&e.as_str()),
                "不该包含 {e}"
            );
        }
    }

    #[test]
    fn quick_death_threshold() {
        let p = HealPolicy::default();
        assert!(is_quick_death(std::time::Duration::from_secs(5), &p));
        assert!(!is_quick_death(std::time::Duration::from_secs(60), &p));
    }

    #[test]
    fn candidate_chain_always_has_entries_or_is_empty() {
        let ctx = CandidateContext {
            port: 3080,
            no_open: true,
            custom_path: Some("C:\\fake\\dsh.exe".into()),
            allow_npx: false,
        };
        let c = build_candidates(&ctx);
        // 自定义路径一定会被加进去
        assert!(c.iter().any(|x| x.method.contains("用户指定")));
        // --no-open 应体现在参数里
        assert!(c.iter().any(|x| x.args.iter().any(|a| a == "--no-open")));
    }

    #[test]
    fn candidate_describe_readable() {
        let c = Candidate {
            method: "x".into(),
            program: "dsh".into(),
            args: vec!["web".into(), "--no-open".into()],
            cwd: None,
        };
        assert_eq!(c.describe(), "dsh web --no-open");
    }
}
