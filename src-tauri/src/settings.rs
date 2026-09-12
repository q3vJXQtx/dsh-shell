//! 用户配置 —— 读写 `settings.json`。
//!
//! # 参考项目在这里踩的两个坑
//!
//! ## 1. `noOpenCache` 永不过期
//!
//! 参考项目把「dsh 是否支持 `--no-open`」的探测结果按**可执行文件路径**缓存。
//! 路径不变，缓存就永远有效——于是用户升级 dsh（同一路径、新版二进制）后，
//! 壳仍沿用旧结论，行为与预期不符，且**没有任何界面入口可以清除它**。
//! 作者自己的注释都写着"删除 settings.json 里的 noOpenCache 即可重探"，
//! 也就是把修复责任推给了用户去手改 JSON。
//!
//! 本项目的对策：缓存键由 `lifecycle::supports_no_open` 构造为
//! `路径@版本号`，**版本一变键就变**，自动重探，无需用户干预。
//! 另外提供 `clear_no_open_cache()`，界面上也留了一个"重新探测"入口，
//! 双保险。
//!
//! ## 2. 配置损坏 = 直接崩
//!
//! 参考项目对 `settings.json` 的读取失败是不加处理的。
//! 本项目的策略是：**损坏不阻塞启动**——把坏文件改名备份，
//! 用默认值继续跑，并在日志里明确告诉用户"你的配置读不了，已备份到 X"。

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 默认监听端口（与 dsh 默认一致）
pub const DEFAULT_PORT: u16 = 3080;

/// 版本检查等**外网请求**的代理方式。
///
/// 注意作用范围：只影响壳自己发起的出站请求（目前就是 DSH 版本检查）。
/// 壳与本地 DSH 后端之间的通信（token 交换、事件流、RPC）**永远直连
/// 127.0.0.1**，绝不走代理——本机回环走代理只会收获一层假 502。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// 直连，不走任何代理
    Direct,
    /// 自动：先直连，失败后依次尝试环境变量代理、常见本地端口（默认，与历史行为一致）
    Auto,
    /// 只走用户在界面上填写的代理地址
    Custom,
}

impl Default for ProxyMode {
    fn default() -> Self {
        Self::Auto
    }
}

/// 应用配置。
///
/// 所有字段都带 `#[serde(default)]`，以便**旧版本配置文件缺字段时仍能读取**
/// （向前兼容：新增配置项不会让老用户的配置文件失效）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// dsh 监听端口
    pub port: u16,
    /// 用户手动指定的 dsh 可执行文件路径
    pub custom_dsh_path: Option<String>,
    /// 是否允许使用 npx 兜底（会临时下载，较慢，需用户同意）
    pub allow_npx: bool,
    /// 是否固定抑制自动打开浏览器
    pub no_open: bool,
    /// `--no-open` 支持情况缓存。
    ///
    /// 键的格式见 [`crate::lifecycle::supports_no_open`]：`<可执行文件路径>@<版本>`。
    /// **注意键里带版本**，这是与参考项目的关键差异。
    pub no_open_support: HashMap<String, bool>,
    /// 诊断面板是否常驻展开
    pub diagnostics_expanded: bool,
    /// 关闭窗口时是否最小化到托盘而非退出
    pub close_to_tray: bool,
    /// 会话跑完时是否发送 Windows 通知（见 [`crate::notify`]）。
    ///
    /// 做成开关而不是永远开启：通知是**主动打扰**用户的能力，
    /// 必须给一个关掉它的地方。且关闭后连事件流都不必订阅，
    /// 顺带省掉一个常驻的 WebSocket 连接。
    pub notify_on_finish: bool,
    /// 是否把 DSH 页面发往 GitHub 的请求改走加速镜像（见 [`crate::inject`]）。
    ///
    /// 默认开启（国内直连 GitHub 基本不通），但**留了口子**：
    /// 有些网络直连更快，而走第三方镜像既多一跳，
    /// 也把请求的仓库地址暴露给了镜像服务。
    pub gh_mirror: bool,
    /// 版本检查等外网请求的代理方式（见 [`ProxyMode`]）
    pub proxy_mode: ProxyMode,
    /// `proxy_mode == Custom` 时使用的代理地址，如 `http://127.0.0.1:7890`
    pub proxy_url: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            custom_dsh_path: None,
            allow_npx: false,
            // 默认就抑制自动打开浏览器：壳内嵌显示才是本应用的存在意义。
            // 参考项目默认 false，导致用户一开就弹出系统浏览器，
            // 还得去 issue 里找怎么关。
            no_open: true,
            no_open_support: HashMap::new(),
            diagnostics_expanded: false,
            close_to_tray: true,
            notify_on_finish: true,
            gh_mirror: true,
            proxy_mode: ProxyMode::default(),
            proxy_url: None,
        }
    }
}

impl Settings {
    /// 从指定路径读取。
    ///
    /// 返回 `(配置, 提示信息)`。提示信息非空时表示发生了可告知用户的情况
    /// （例如配置文件损坏已备份）——**这种情况不应该静默**。
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            // 文件不存在是全新安装的正常情况，无需提示
            Err(e) if e.kind() == io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => {
                return (
                    Self::default(),
                    Some(format!("读取配置失败（{e}），本次使用默认配置")),
                )
            }
        };

        match serde_json::from_str::<Settings>(&raw) {
            Ok(s) => (s, None),
            Err(e) => {
                let msg = match backup_corrupt(path) {
                    Some(b) => format!(
                        "配置文件格式有误，已备份为 {} 并恢复默认配置（原因：{e}）",
                        b.display()
                    ),
                    None => format!("配置文件格式有误（{e}），本次使用默认配置"),
                };
                (Self::default(), Some(msg))
            }
        }
    }

    /// 原子写入。
    ///
    /// 先写临时文件再改名：避免写一半断电/被杀导致配置文件残缺，
    /// 进而下次启动时读不出来。
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, text)?;
        // Windows 上 rename 到已存在的文件会失败，先移除目标
        // （此处可接受：即使移除后 rename 失败，也只是丢配置，不丢数据）
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        fs::rename(&tmp, path)
    }

    /// 清空 `--no-open` 探测缓存（供界面上的"重新探测"入口调用）
    pub fn clear_no_open_cache(&mut self) {
        self.no_open_support.clear();
    }
}

/// 把损坏的配置文件改名备份，返回备份路径
fn backup_corrupt(path: &Path) -> Option<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_extension(format!("json.corrupt-{secs}"));
    fs::rename(path, &backup).ok().map(|_| backup)
}

/// 配置文件的存放目录。
///
/// 放在**可执行文件同级**的 `data/` 下，而非用户的 AppData：
/// 这样整个应用是绿色的，用户可以连同 exe 一起搬走或删除，
/// 不留系统残留。这也顺应了用户"工具都装 D 盘"的偏好。
pub fn data_dir(exe_dir: &Path) -> PathBuf {
    exe_dir.join("dsh-shell-data")
}

/// 配置文件完整路径
pub fn settings_path(exe_dir: &Path) -> PathBuf {
    data_dir(exe_dir).join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dsh_shell_settings_{name}"));
        let _ = fs::create_dir_all(&d);
        d.join("settings.json")
    }

    #[test]
    fn default_suppresses_browser_and_uses_3080() {
        let s = Settings::default();
        assert_eq!(s.port, DEFAULT_PORT);
        assert!(s.no_open, "默认应当抑制自动打开浏览器");
    }

    #[test]
    fn missing_file_yields_default_without_warning() {
        let p = tmp("missing");
        let _ = fs::remove_file(&p);
        let (s, warn) = Settings::load(&p);
        assert_eq!(s.port, DEFAULT_PORT);
        assert!(warn.is_none(), "全新安装不应产生提示");
    }

    #[test]
    fn roundtrip_persists_values() {
        let p = tmp("roundtrip");
        let mut s = Settings::default();
        s.port = 3099;
        s.no_open_support.insert("dsh@1.2.3".into(), true);
        s.save(&p).unwrap();

        let (loaded, warn) = Settings::load(&p);
        assert!(warn.is_none());
        assert_eq!(loaded.port, 3099);
        assert_eq!(loaded.no_open_support.get("dsh@1.2.3"), Some(&true));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn corrupt_file_is_backed_up_not_fatal() {
        let p = tmp("corrupt");
        fs::write(&p, "{ 这不是合法 json").unwrap();
        let (s, warn) = Settings::load(&p);
        // 关键：不崩、不阻塞启动，回退默认值
        assert_eq!(s.port, DEFAULT_PORT);
        let warn = warn.expect("应当提示用户配置已备份");
        assert!(warn.contains("已备份"), "实际提示: {warn}");
        // 坏文件应已改名备份，原位置不再存在
        assert!(!p.exists());
        // 备份文件确实在
        let dir = p.parent().unwrap();
        let has_backup = fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(has_backup, "应留下备份文件");
        // 清理
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.filter_map(Result::ok) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 向前兼容：老配置文件缺字段也应能读
        let p = tmp("partial");
        fs::write(&p, r#"{"port": 3123}"#).unwrap();
        let (s, warn) = Settings::load(&p);
        assert!(warn.is_none());
        assert_eq!(s.port, 3123);
        // 未出现的字段用默认值
        assert!(s.no_open);
        assert!(s.close_to_tray);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn clear_cache_empties_map() {
        let mut s = Settings::default();
        s.no_open_support.insert("a@1".into(), true);
        s.clear_no_open_cache();
        assert!(s.no_open_support.is_empty());
    }

    #[test]
    fn paths_live_under_exe_dir() {
        let exe_dir = Path::new(r"D:\some\app");
        let p = settings_path(exe_dir);
        assert!(p.starts_with(exe_dir), "配置应位于程序目录下");
        assert!(p.ends_with("settings.json"));
    }

    #[test]
    fn proxy_fields_roundtrip_and_default_to_auto() {
        // 老配置没有 proxyMode/proxyUrl 字段 → 默认 Auto（保持历史行为），不报错
        let (s, warn) = Settings::load(&{
            let p = tmp("proxy_default");
            fs::write(&p, r#"{"port": 3099}"#).unwrap();
            p
        });
        assert!(warn.is_none());
        assert_eq!(s.proxy_mode, ProxyMode::Auto);
        assert!(s.proxy_url.is_none());

        // 显式配置 custom + 地址 → 原样往返
        let p = tmp("proxy_custom");
        let mut s = Settings::default();
        s.proxy_mode = ProxyMode::Custom;
        s.proxy_url = Some("http://127.0.0.1:7890".into());
        s.save(&p).unwrap();
        let (loaded, warn) = Settings::load(&p);
        assert!(warn.is_none());
        assert_eq!(loaded.proxy_mode, ProxyMode::Custom);
        assert_eq!(loaded.proxy_url.as_deref(), Some("http://127.0.0.1:7890"));
        let _ = fs::remove_file(&p);
    }
}
