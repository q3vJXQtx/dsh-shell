//! 自愈机制 —— 启动前的预检与故障修复。
//!
//! # 为什么这是本项目最重要的一块
//!
//! 参考项目（dsh-local-shell v2.0.3）实现了两类自愈：`settings_yaml_selfheal`
//! （补 `key:value` 缺空格）与 `repair_profile_bundle`（pnpm 发布冷却期导致缺包）。
//! 但**完全没有处理陈旧文件锁**——整个 Rust 源码里连锁文件名都没出现过。
//!
//! 而陈旧锁恰恰是最致命的：
//!
//! ```text
//! dsh 启动 → 申请 $DSH_HOME/.credentials.yaml.lock 写锁
//!          → 锁文件的持有者 PID 早已消亡，但锁没被清理
//!          → 一直等到超时 → 插件树加载失败 → 进程直接崩溃
//!          → 壳抓不到 token（dsh 崩溃时不产生任何输出）
//!          → 停在无 token 的 URL → 用户看到 404
//! ```
//!
//! 用户侧的表现是"打不开、白屏、404"，跟"锁文件"毫无表面关联，
//! 极难自行排查。本模块把它变成一条**自动检测 + 自动修复**的路径。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 锁探测结果
#[derive(Debug, Clone)]
pub enum LockProbe {
    /// 没有锁文件，正常
    Absent,
    /// 有锁，且持有者仍存活 —— 不能动（另一个 dsh 正在跑）
    Held { pid: u32 },
    /// 陈旧锁：持有者已消亡，可安全移除
    Stale { pid: Option<u32> },
    /// 锁文件存在但内容不是 PID（不同版本可能格式不同）
    Unrecognized { raw: String },
}

/// 修复动作的结果
#[derive(Debug, Clone)]
pub enum HealOutcome {
    /// 无需处理
    Nothing,
    /// 已移除陈旧锁（返回备份路径，便于回滚）
    RemovedStaleLock { backup: PathBuf },
    /// 想修但失败了
    Failed { message: String },
}

/// dsh 的凭证锁文件名（与 dsh 源码 `dsh-atomic-write` 保持一致）
const CREDENTIALS_LOCK: &str = ".credentials.yaml.lock";

/// 构造 dsh home 下的锁文件路径
pub fn lock_path(dsh_home: &Path) -> PathBuf {
    dsh_home.join(CREDENTIALS_LOCK)
}

/// 探测锁状态。
///
/// 锁文件内容是**持有者的 PID**（已在真实环境验证：内容形如 `67848`）。
/// 因此判定逻辑很直接：读 PID → 看进程还在不在。
pub fn probe_lock(dsh_home: &Path) -> LockProbe {
    let path = lock_path(dsh_home);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        // 文件不存在，或读取失败（后者按"没有锁"处理，不阻塞启动）
        Err(_) => return LockProbe::Absent,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // 空锁文件同样视为陈旧：没有任何进程在持有它
        return LockProbe::Stale { pid: None };
    }
    match trimmed.parse::<u32>() {
        Ok(pid) => {
            if process_alive(pid) {
                LockProbe::Held { pid }
            } else {
                LockProbe::Stale { pid: Some(pid) }
            }
        }
        Err(_) => LockProbe::Unrecognized {
            raw: trimmed.chars().take(64).collect(),
        },
    }
}

/// 判定进程是否存活。
///
/// 用 `tasklist` 而不是 Win32 API：调用点只在启动路径上出现一次，
/// 开销可接受，且避免了 FFI 签名出错的风险（本项目 `panic = "abort"`，
/// 一次 FFI 错误就会整崩应用）。
///
/// 注意必须走 [`crate::lifecycle::hide_console`]：本程序是 GUI 子系统，
/// 从这里启动控制台程序会**新开一个黑窗**。这条路径在有锁文件时每次启动都会跑到，
/// 原先漏了这个标志，用户就会看到闪一下的黑框。
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
        crate::lifecycle::hide_console(&mut cmd);
        let out = cmd.output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                // 无匹配时 tasklist 会输出 "信息: 没有运行的任务匹配指定标准。"
                // 有匹配时输出形如 `"node.exe","1234","Console","1","100,000 K"`
                text.contains(&format!("\"{pid}\""))
            }
            // tasklist 不可用时保守处理：当作"存活"，避免误删他人正在用的锁
            Err(_) => true,
        }
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 走 /proc
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// 清理陈旧锁（**可逆**：改名保留而非删除）。
///
/// 采用改名而非删除，理由：
/// 1. 万一判定失误，用户可手动改回
/// 2. 保留现场便于事后排查
pub fn heal_stale_lock(dsh_home: &Path) -> HealOutcome {
    let path = lock_path(dsh_home);
    match probe_lock(dsh_home) {
        LockProbe::Absent => HealOutcome::Nothing,
        LockProbe::Held { pid } => HealOutcome::Failed {
            message: format!("锁被仍在运行的进程 {pid} 持有，未做处理"),
        },
        LockProbe::Stale { pid } => {
            let backup = path.with_extension(format!(
                "lock.stale-{}",
                timestamp_suffix()
            ));
            match fs::rename(&path, &backup) {
                Ok(_) => HealOutcome::RemovedStaleLock { backup },
                Err(e) => HealOutcome::Failed {
                    message: format!(
                        "清理陈旧锁失败（持有者 PID {:?}）：{e}",
                        pid
                    ),
                },
            }
        }
        LockProbe::Unrecognized { raw } => HealOutcome::Failed {
            message: format!("锁文件内容无法识别为 PID：{raw}"),
        },
    }
}

/// 生成用于备份文件名的后缀。
///
/// 刻意不引入 `chrono`：这里只是给备份文件一个不冲突的名字，
/// 用系统时间戳即可，省一个依赖。
fn timestamp_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_when_no_file() {
        let dir = std::env::temp_dir().join("dsh_shell_test_absent");
        let _ = fs::create_dir_all(&dir);
        let _ = fs::remove_file(lock_path(&dir));
        assert!(matches!(probe_lock(&dir), LockProbe::Absent));
    }

    #[test]
    fn stale_when_pid_not_alive() {
        let dir = std::env::temp_dir().join("dsh_shell_test_stale");
        let _ = fs::create_dir_all(&dir);
        // 用一个几乎不可能存在的 PID
        fs::write(lock_path(&dir), "4000000").unwrap();
        match probe_lock(&dir) {
            LockProbe::Stale { pid } => assert_eq!(pid, Some(4_000_000)),
            other => panic!("期望 Stale，实际 {other:?}"),
        }
    }

    #[test]
    fn heal_renames_instead_of_deleting() {
        let dir = std::env::temp_dir().join("dsh_shell_test_heal");
        let _ = fs::create_dir_all(&dir);
        fs::write(lock_path(&dir), "4000000").unwrap();
        match heal_stale_lock(&dir) {
            HealOutcome::RemovedStaleLock { backup } => {
                // 原位置应已无锁
                assert!(!lock_path(&dir).exists());
                // 备份仍在（可逆）
                assert!(backup.exists());
                let _ = fs::remove_file(&backup);
            }
            other => panic!("期望成功清理，实际 {other:?}"),
        }
    }

    #[test]
    fn pid_zero_is_not_alive() {
        assert!(!process_alive(0));
    }
}
