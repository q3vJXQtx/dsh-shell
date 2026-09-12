//! 后端状态机 —— 单一可信状态源（single source of truth）。
//!
//! # 为什么要有这个模块
//!
//! 参考项目（dsh-local-shell v2.0.3）用 **4 个分散的原子变量**
//! （`INTENTIONAL_STOP` / `QUICK_DEATHS` / `AUTH_WALL_HANDLED` / `BACKEND_RESTARTING`）
//! 来描述后端生命周期，导致自愈、重启、接管、401 流程交叉时状态错乱。
//! 其 CHANGELOG 中「卡在正在启动 / 白屏 / 死页」一类问题**跨越十几个版本反复出现**
//! （v1.4.2 → v1.5.7 → v1.5.9 → v1.6.4 → v1.6.5），说明这是架构缺陷而非偶发 bug。
//!
//! 本模块的做法：**所有状态转换都集中在一处**，任何代码想改变后端状态
//! 都必须经过 `BackendState`，由此消除竞态与"状态自相矛盾"。
//!
//! 另一个关键设计：状态里**显式携带阶段信息**（`StartPhase`）与
//! **结构化的失败原因**（`FailureReason`），这样前端才能：
//! 1. 把启动过程可视化（用户知道"卡在哪一步"）
//! 2. 针对具体失败给出**可操作的引导**，而不是丢一坨开发者日志

use serde::Serialize;

/// 启动阶段 —— 让"正在启动…"这句话变成可追踪的进度。
///
/// 参考项目只显示一句"正在启动 DSH…"，而 `dsh web --help` 探测
/// 单次可达 9 秒、npx 首次安装可达数分钟，期间用户完全不知道在等什么。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StartPhase {
    /// 解析 dsh 命令位置（候选链）
    Resolving,
    /// 预检：陈旧锁、端口占用等可自愈的前置问题
    Preflight,
    /// 拉起子进程
    Spawning,
    /// 等待 HTTP 服务就绪
    WaitingReady,
    /// 获取访问 token
    AcquiringToken,
}

impl StartPhase {
    /// 面向用户的阶段描述
    pub fn label(&self) -> &'static str {
        match self {
            StartPhase::Resolving => "正在定位 DSH 命令",
            StartPhase::Preflight => "正在检查运行环境",
            StartPhase::Spawning => "正在启动 DSH 服务",
            StartPhase::WaitingReady => "正在等待服务就绪",
            StartPhase::AcquiringToken => "正在获取访问凭证",
        }
    }
}

/// 结构化失败原因。
///
/// 关键点：**每种失败都带上"可操作的信息"**，前端据此给出针对性引导。
/// 参考项目的做法是把一坨多行技术文本塞进一个灰色 `<div>`，
/// 用户看得到却看不懂、也无从下手。
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    // 必须显式声明：`rename_all` 只改**变体名**，不改**变体字段名**。
    // 少了它，`holder_pid` 会原样序列化成蛇形，前端按 camelCase 读取
    // 就是 `undefined` —— 症状是界面上永远不显示"持有者 PID"。
    rename_all_fields = "camelCase"
)]
pub enum FailureReason {
    /// 候选链全部落空，找不到 dsh
    DshNotFound {
        /// 尝试过的候选（前端可展示，便于用户自查）
        searched: Vec<String>,
    },
    /// 陈旧文件锁。
    ///
    /// 这是实际踩到的最严重故障：dsh 启动时申请 `.credentials.yaml.lock` 写锁
    /// 超时 → 插件树加载失败 → 进程直接崩溃 → 壳抓不到 token → 用户看到 404。
    /// **参考项目对此零处理**（整个 Rust 源码里连锁文件名都没出现）。
    StaleLock {
        path: String,
        /// 锁文件里记录的持有者 PID（可能早已消亡）
        holder_pid: Option<u32>,
    },
    /// 端口被非 DSH 进程占用
    PortOccupied { port: u16, pid: Option<u32> },
    /// 端口上已经有**另一个 DSH 实例**在运行（不是本程序拉起的）。
    ///
    /// # 为什么必须单独建模
    ///
    /// 这种情况**在原理上无法自动接管**：`dsh` 的访问凭证由
    /// `processLaunchToken()` 在进程内生成（`randomBytes(32)` → 43 字符 base64url），
    /// 存在一个以进程为 key 的 `WeakMap` 里，**既不落盘也不对外暴露**，
    /// 只打印到自己那份 stdout。所以外部程序再怎么找也拿不到别人的 token，
    /// 直接开窗口只会是 401。
    ///
    /// 参考项目在这里静默回退到裸 URL，用户看到 401 却完全不知道原因；
    /// 本项目的做法是把选择权交给用户：
    /// 要么**关掉已有实例后接管**，要么**换个端口起自己的实例**。
    ForeignDshRunning { port: u16, pid: Option<u32> },
    /// 子进程反复快速崩溃
    CrashLoop {
        attempts: u32,
        /// 子进程最后的输出，供诊断
        tail: String,
    },
    /// 等待就绪超时
    ReadyTimeout { seconds: u64 },
    /// 服务起来了但没拿到 token。
    ///
    /// 单独建模的原因：这时**服务其实是好的**，只是缺凭证，
    /// 用户看到的是 404/401，极易误解为"服务没起来"。
    TokenMissing { hint: String },
    /// 其它未归类失败
    Other { message: String },
}

impl FailureReason {
    /// 面向用户的简短说明
    pub fn summary(&self) -> String {
        match self {
            FailureReason::DshNotFound { .. } => "没有找到 DSH 命令".into(),
            FailureReason::StaleLock { .. } => "DSH 被一个残留的锁文件阻塞".into(),
            FailureReason::PortOccupied { port, .. } => format!("端口 {port} 被其它程序占用"),
            FailureReason::ForeignDshRunning { port, .. } => {
                format!("端口 {port} 上已有一个 DSH 在运行")
            }
            FailureReason::CrashLoop { attempts, .. } => {
                format!("DSH 连续 {attempts} 次启动后异常退出")
            }
            FailureReason::ReadyTimeout { seconds } => format!("等待服务就绪超过 {seconds} 秒"),
            FailureReason::TokenMissing { .. } => "DSH 已启动，但未能取得访问凭证".into(),
            FailureReason::Other { message } => message.clone(),
        }
    }

    /// 面向诊断的**技术细节**（写进日志；前端展示的是 [`Self::summary`]）。
    ///
    /// 有它日志才完整：只记一句"启动失败"等于没记，
    /// 事后回看无法判断是哪一步、因为什么结束的。
    pub fn detail(&self) -> String {
        match self {
            FailureReason::DshNotFound { searched } => searched.join("\n"),
            FailureReason::StaleLock { path, holder_pid } => match holder_pid {
                Some(pid) => format!("{path}\n锁文件记录的持有者 PID {pid}"),
                None => path.clone(),
            },
            FailureReason::PortOccupied { port, pid } => format!("端口 {port}，占用 PID {}", fmt_pid(*pid)),
            FailureReason::ForeignDshRunning { port, pid } => {
                format!("端口 {port}，已有实例 PID {}", fmt_pid(*pid))
            }
            FailureReason::CrashLoop { attempts, tail } => {
                format!("连续 {attempts} 次快速退出\n最后输出：\n{tail}")
            }
            FailureReason::ReadyTimeout { seconds } => {
                format!("等待 {seconds} 秒后端口仍未给出正常响应")
            }
            FailureReason::TokenMissing { hint } => hint.clone(),
            FailureReason::Other { message } => message.clone(),
        }
    }
}

/// PID 的展示形式：拿不到时也得说清楚是"拿不到"而不是空白
fn fmt_pid(pid: Option<u32>) -> String {
    pid.map(|p| p.to_string()).unwrap_or_else(|| "未知".into())
}

/// 后端状态。
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BackendState {
    /// 尚未开始
    Idle,
    /// 启动中
    Starting {
        phase: StartPhase,
        /// 用哪种方式启动的（如 "PATH 中的 dsh"），便于诊断
        method: String,
    },
    /// 就绪。
    ///
    /// `owned` 区分"我们拉起的"与"附加到已有实例的"——
    /// 这个区分在参考项目里是靠 PowerShell 脚本匹配 exe 文件名来猜的
    /// （硬编码 `dsh-desktop-windowos.exe`，改名即失效）。
    Ready { owned: bool, pid: Option<u32> },
    /// 自愈中（子进程异常退出后自动重启）
    Healing {
        attempt: u32,
        max_attempts: u32,
        last_error: String,
    },
    /// 失败（终态，需用户介入）
    Failed { reason: FailureReason },
}

impl BackendState {
    /// 是否处于"后端可用"状态（前端据此决定是否显示 webchat）
    pub fn is_ready(&self) -> bool {
        matches!(self, BackendState::Ready { .. })
    }

    /// 是否已进入终态（不再自动重试）
    pub fn is_terminal(&self) -> bool {
        matches!(self, BackendState::Failed { .. })
    }
}

/// 自愈策略参数。
///
/// 参考项目的教训：连续 3 次快速崩溃后**静默停止重试**，
/// 界面无任何提示，用户只能干等；而且停止时**没有清空内部状态**，
/// 导致 `DshState` 仍持有一个已经死掉的 PID。
pub struct HealPolicy {
    /// 崩溃判定窗口：存活时间短于此值视为"快速崩溃"
    pub quick_death_window_secs: u64,
    /// 连续快速崩溃多少次后放弃
    pub max_quick_deaths: u32,
}

impl Default for HealPolicy {
    fn default() -> Self {
        Self {
            quick_death_window_secs: 30,
            max_quick_deaths: 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_state_is_ready() {
        let s = BackendState::Ready {
            owned: true,
            pid: Some(1),
        };
        assert!(s.is_ready());
        assert!(!s.is_terminal());
    }

    #[test]
    fn failed_state_is_terminal() {
        let s = BackendState::Failed {
            reason: FailureReason::ReadyTimeout { seconds: 30 },
        };
        assert!(s.is_terminal());
        assert!(!s.is_ready());
    }

    #[test]
    fn stale_lock_has_readable_summary() {
        let r = FailureReason::StaleLock {
            path: r"C:\x\.credentials.yaml.lock".into(),
            holder_pid: Some(1234),
        };
        assert!(r.summary().contains("锁文件"));
    }

    #[test]
    fn phases_have_labels() {
        assert!(!StartPhase::AcquiringToken.label().is_empty());
    }
}
