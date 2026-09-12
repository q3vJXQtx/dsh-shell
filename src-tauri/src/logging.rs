//! 日志 —— 面向"用户能看懂"，而非只给开发者看。
//!
//! # 参考项目的问题
//!
//! 它的日志只记壳自身事件（README 明确说"只记壳自身事件"），
//! 内容形如 `supervised dsh web exited unexpectedly; healing`，
//! 普通用户既想不到去看，也看不懂。
//!
//! 本模块做两件事：
//! 1. **每条日志都带一个面向用户的可读摘要**（`LogEntry::user_text`），
//!    界面默认展示摘要，需要时再展开技术细节；
//! 2. 保留子进程输出的环形缓冲，用于崩溃诊断——但**独立于用户日志**，
//!    避免把原始输出混进日志文件造成噪音。
//!
//! 另外，任何凭证（token）写日志前都必须打码，因为日志可能被用户
//! 导出后发到公开渠道求助。

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

/// 日志级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(&self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// 一条日志
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    /// 时间戳（本地时间，秒级）
    pub at: String,
    pub level: Level,
    /// 面向用户的说明
    pub user_text: String,
    /// 技术细节（可为空）
    pub detail: String,
}

impl LogEntry {
    /// 渲染成单行，写入日志文件
    fn render(&self) -> String {
        if self.detail.is_empty() {
            format!("[{}] [{}] {}", self.at, self.level.tag(), self.user_text)
        } else {
            format!(
                "[{}] [{}] {} | {}",
                self.at,
                self.level.tag(),
                self.user_text,
                self.detail
            )
        }
    }
}

/// 子进程输出环形缓冲上限。
///
/// 与原项目保持一致（60 行）：足够定位崩溃原因，又不会无限增长。
const CHILD_TAIL_CAP: usize = 60;

/// 内存中保留的用户日志条数上限。
///
/// # 为什么必须封顶
///
/// [`all`] 会被前端**每秒一次**的 `get_snapshot` 调用，而那是把全量日志
/// 克隆 + 序列化 + 走一遍 IPC。原先这里的 `Vec` 无上限也从不清理，
/// 于是内存与每次快照的开销都会随时间线性上涨 —— 程序定位是常驻托盘跑几天，
/// 这个增长没有终点。
///
/// 2000 条远超排障所需（真实运行 90 分钟也只积累 29 条），
/// 同时把最坏情况下的单次快照压在几百 KB 量级。
const ENTRIES_CAP: usize = 2000;

/// 日志文件轮转阈值（2MB）。
///
/// 环形缓冲管的是**内存**，这个管的是**磁盘**：程序常驻跑几周，
/// 日志文件不轮转就会一直长下去。轮转时保留一份 `.1` 作为历史。
const LOG_FILE_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 距上次文件大小检查又写了多少行。
///
/// 没必要每写一行都去 `stat` 一次；日志量本身不大，100 行查一次足够及时。
static LINES_SINCE_ROTATE_CHECK: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
static ENTRIES: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
static CHILD_TAIL: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn entries() -> &'static Mutex<VecDeque<LogEntry>> {
    ENTRIES.get_or_init(|| Mutex::new(VecDeque::with_capacity(ENTRIES_CAP)))
}

fn child_tail() -> &'static Mutex<VecDeque<String>> {
    CHILD_TAIL.get_or_init(|| Mutex::new(VecDeque::with_capacity(CHILD_TAIL_CAP)))
}

/// 初始化日志目录
pub fn init(dir: impl AsRef<Path>) {
    let dir = dir.as_ref().to_path_buf();
    let _ = fs::create_dir_all(&dir);
    let _ = LOG_DIR.set(dir);
}

/// 当前日志文件路径
pub fn log_file() -> Option<PathBuf> {
    LOG_DIR.get().map(|d| d.join("dsh-shell.log"))
}

/// 获取本地时间字符串（`YYYY-MM-DD HH:MM:SS`）。
///
/// 刻意不引入 `chrono`/`time` 依赖：本项目是 Windows-only，
/// 直接调用系统 API 取本地时间即可，省掉一个依赖树。
#[cfg(windows)]
fn now_string() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    unsafe {
        let mut st = std::mem::zeroed();
        GetLocalTime(&mut st);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        )
    }
}

#[cfg(not(windows))]
fn now_string() -> String {
    // 非 Windows 仅用于让代码可编译；本项目实际只支持 Windows。
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch+{secs}")
}

/// 记录一条日志
pub fn log(level: Level, user_text: impl Into<String>, detail: impl Into<String>) {
    let entry = LogEntry {
        at: now_string(),
        level,
        user_text: user_text.into(),
        detail: detail.into(),
    };

    if let Ok(mut v) = entries().lock() {
        // 与 CHILD_TAIL 同样的取舍：溢出时丢最早的那条，永远保留最新的
        if v.len() >= ENTRIES_CAP {
            v.pop_front();
        }
        v.push_back(entry.clone());
    }

    if let Some(path) = log_file() {
        // 每 100 行才查一次文件大小，避免给每行日志都加一次 stat
        if LINES_SINCE_ROTATE_CHECK.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 100 == 0 {
            rotate_if_needed(&path);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", entry.render());
        }
    }
}

/// panic hook 专用的日志：**永不阻塞，也绝不再 panic**。
///
/// # 为什么不直接用 [`error`]
///
/// [`log`] 里是 `entries().lock()`。而 panic **很可能就发生在 `log()` 内部**
/// （分配失败、格式化 panic 之类），那一刻 ENTRIES 锁被同一个线程持有着 ——
/// `std::sync::Mutex` 不可重入，同线程再 lock 一次就是**死锁**。
/// 放在 panic hook 里死锁比直接终止更糟：`panic = "abort"` 本该让进程立刻退出，
/// 结果却挂在那里不退，端口一直占着，用户看着一个没有任何界面的僵尸进程。
///
/// 所以这里用 `try_lock`：拿不到就跳过内存这一份，**不做任何等待**。
///
/// 另外这一段**必须落盘**：release 是 GUI 子系统（`windows_subsystem = "windows"`），
/// 默认 panic hook 往 stderr 打的那串信息根本没有控制台能看到，
/// 不写文件的话 panic 现场就彻底丢了。
pub fn log_from_panic_hook(user_text: &str, detail: &str) {
    let entry = LogEntry {
        at: now_string(),
        level: Level::Error,
        user_text: user_text.to_string(),
        detail: detail.to_string(),
    };

    // 拿得到就记进内存（拿不到说明有人正持着锁，跳过即可）
    if let Ok(mut v) = entries().try_lock() {
        if v.len() >= ENTRIES_CAP {
            v.pop_front();
        }
        v.push_back(entry.clone());
    }

    if let Some(path) = log_file() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", entry.render());
            // 立刻刷盘：进程马上要 abort 了，缓冲里的内容留不下来
            let _ = f.flush();
        }
    }
}

/// 日志文件超过阈值就轮转成 `.1`（只保留一份历史）。
///
/// 失败一律忽略：轮转只是防止磁盘无限增长，不该因为文件被占用
/// （比如用户正开着它看）就让写日志这一步失败。
fn rotate_if_needed(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return; // 文件还不存在，没得轮转
    };
    if meta.len() < LOG_FILE_MAX_BYTES {
        return;
    }
    let backup = path.with_extension("log.1");
    let _ = fs::remove_file(&backup);
    let _ = fs::rename(path, &backup);
}

/// 便捷方法 —— 三个级别签名统一为 (user_text, detail)。
///
/// detail 用于放「技术细节」：路径、PID、命令、原始错误串。
/// 传空串表示没有额外细节，前端会隐藏详情行。
pub fn info(user_text: impl Into<String>, detail: impl Into<String>) {
    log(Level::Info, user_text, detail);
}

pub fn warn(user_text: impl Into<String>, detail: impl Into<String>) {
    log(Level::Warn, user_text, detail);
}

pub fn error(user_text: impl Into<String>, detail: impl Into<String>) {
    log(Level::Error, user_text, detail);
}

/// 追加子进程输出（stdout/stderr 都往里灌）
pub fn push_child_output(line: impl Into<String>) {
    if let Ok(mut q) = child_tail().lock() {
        if q.len() >= CHILD_TAIL_CAP {
            q.pop_front();
        }
        q.push_back(line.into());
    }
}

/// 取子进程输出的快照，用于崩溃诊断
pub fn child_tail_snapshot() -> String {
    child_tail()
        .lock()
        .map(|q| q.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// 清空子进程输出环形缓冲。
///
/// **必须在拉起新一代后端之前调用。** 这个缓冲跨代复用，上一代遗留的
/// `token=` 行如果不清掉，`token::extract_token` 扫描快照时（即便它取
/// 最后一个匹配，旧行也可能比新行更晚出现于旧线程的延迟泵送）就可能
/// 拿到已死进程的 token——launch token 每代随机，旧 token 交换必 401，
/// 表现为「事件流永远连不上：token 交换未返回 Set-Cookie」，
/// 而 webview 靠持久化签名 cookie 照常能用，把问题完全遮住。
pub fn clear_child_tail() {
    if let Ok(mut q) = child_tail().lock() {
        q.clear();
    }
}

/// 是否包含某个片段（用于识别崩溃签名）
pub fn child_tail_contains(needle: &str) -> bool {
    child_tail()
        .lock()
        .map(|q| q.iter().any(|l| l.contains(needle)))
        .unwrap_or(false)
}

/// 取全部日志（前端展示用）
///
/// 返回顺序为**最早 → 最新**，与 [`Snapshot`](crate::Snapshot) 的约定一致。
pub fn all() -> Vec<LogEntry> {
    entries()
        .lock()
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_default()
}

/// 导出诊断报告 —— 供用户发给开发者排障。
///
/// 包含：环境摘要、全部日志、子进程尾部输出。
/// **注意**：token 已在写入日志前打码，这里不再额外处理。
pub fn export_diagnostics(extra: &str) -> String {
    let mut s = String::new();
    s.push_str("DSH Shell 诊断报告\n");
    s.push_str(&format!("生成时间: {}\n", now_string()));
    s.push_str(&format!("版本: {}\n", env!("CARGO_PKG_VERSION")));
    s.push_str(&format!("OS: {}\n", std::env::consts::OS));
    s.push_str(&format!("ARCH: {}\n", std::env::consts::ARCH));
    s.push_str("\n--- 环境摘要 ---\n");
    s.push_str(extra);
    s.push_str("\n\n--- 日志 ---\n");
    for e in all() {
        s.push_str(&e.render());
        s.push('\n');
    }
    s.push_str("\n--- DSH 子进程输出(尾部) ---\n");
    s.push_str(&child_tail_snapshot());
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_render_includes_level() {
        let e = LogEntry {
            at: "2026-01-01 00:00:00".into(),
            level: Level::Warn,
            user_text: "出问题了".into(),
            detail: "细节".into(),
        };
        let r = e.render();
        assert!(r.contains("WARN"));
        assert!(r.contains("出问题了"));
        assert!(r.contains("细节"));
    }

    #[test]
    fn entry_render_omits_empty_detail() {
        let e = LogEntry {
            at: "t".into(),
            level: Level::Info,
            user_text: "ok".into(),
            detail: String::new(),
        };
        assert!(!e.render().contains("|"));
    }

    #[test]
    fn child_tail_is_ring_buffer() {
        // 灌入超过容量的行，验证只保留最后 N 条
        for i in 0..(CHILD_TAIL_CAP + 20) {
            push_child_output(format!("line{i}"));
        }
        let snap = child_tail_snapshot();
        let lines: Vec<_> = snap.lines().collect();
        assert!(lines.len() <= CHILD_TAIL_CAP);
        // 最新的那行一定在
        assert!(snap.contains(&format!("line{}", CHILD_TAIL_CAP + 19)));
    }

    #[test]
    fn child_tail_contains_works() {
        push_child_output("atomic-write: timed out waiting for the writer lock");
        assert!(child_tail_contains("writer lock"));
        assert!(!child_tail_contains("不存在的片段xyz"));
    }

    #[test]
    fn diagnostics_has_sections() {
        let d = export_diagnostics("env: test");
        assert!(d.contains("诊断报告"));
        assert!(d.contains("--- 日志 ---"));
        assert!(d.contains("env: test"));
    }

    /// 内存日志必须有上限。
    ///
    /// 这条测试守的是一个**性能**性质而非正确性：前端每秒拉一次全量快照，
    /// 无上限的日志会让每次快照的开销随运行时间无限上涨。
    #[test]
    fn entries_are_bounded() {
        for i in 0..(ENTRIES_CAP + 50) {
            info(format!("压力测试 {i}"), "");
        }
        let kept = all();
        assert!(
            kept.len() <= ENTRIES_CAP,
            "日志条数 {} 超过上限 {ENTRIES_CAP}",
            kept.len()
        );
        // 丢的必须是最旧的：最新灌进去的那条要还在
        assert!(
            kept.iter()
                .any(|e| e.user_text.contains(&format!("压力测试 {}", ENTRIES_CAP + 49))),
            "溢出时把最新的日志丢掉了（应当丢最旧的）"
        );
    }

    /// 日志文件超阈值时轮转，避免常驻运行几周把磁盘写满。
    #[test]
    fn oversized_log_file_is_rotated_aside() {
        let dir = std::env::temp_dir().join("dsh-shell-log-rotate-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("dsh-shell.log");
        let backup = path.with_extension("log.1");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&backup);

        // 未超阈值：原地不动
        fs::write(&path, "小文件").unwrap();
        rotate_if_needed(&path);
        assert!(path.exists(), "小文件不该被轮转");

        // 超阈值：改名成 .log.1，原文件让位
        fs::write(&path, vec![b'x'; (LOG_FILE_MAX_BYTES + 1) as usize]).unwrap();
        rotate_if_needed(&path);
        assert!(!path.exists(), "超限文件应被移走");
        assert!(backup.exists(), "应保留一份 .log.1 作为历史");

        let _ = fs::remove_file(&backup);
        let _ = fs::remove_dir_all(&dir);
    }
}
