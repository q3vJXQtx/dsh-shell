# DSH Shell

[![Build](https://github.com/q3vJXQtx/dsh-shell/actions/workflows/build.yml/badge.svg)](https://github.com/q3vJXQtx/dsh-shell/actions/workflows/build.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![platform: Windows](https://img.shields.io/badge/platform-Windows-lightgrey)

把 `dsh web`（DeepSeek Harness 的本地 Web UI）装进一个原生 Windows 窗口：
一键启动、崩溃自愈、常驻托盘、诊断面板永不消失。

> 你不需要再开一个终端、记一条命令、盯一个 localhost 端口——
> 双击图标，对话窗口就在那。出了问题，窗口会自己说清楚为什么。

<!-- TODO: 截图 docs/screenshot.png -->

---

## ✨ 特性

- 🚀 **一键启动** —— 自动定位 DSH → 启动 `dsh web` → 等待就绪 → 抓取访问凭证
- 🩺 **故障自愈** —— 陈旧凭证锁预检（改名备份，可逆）、启动即崩自动重试；端口被占 / 代理误判 / 外部 DSH 分得清清楚楚
- 🧭 **常驻托盘** —— 关窗进托盘、单实例唤回、悬停一行字反映后端状态；退出时顺带收掉自己拉起的子进程，不留孤儿
- 🔔 **任务完成通知** —— 订阅 DSH 事件流，会话**真的跑完**才弹 Windows 通知（最小化窗口时也弹；可关）
- 🖥️ **双 WebView 控制台** —— 上半是永远在的 React 控制台（状态 / 诊断 / 设置），下半是 DSH 聊天页；白屏了诊断入口也点得到
- 🌐 **页面增强** —— 外链接交系统浏览器、链接右键菜单、GitHub 资源走国内加速镜像（默认开，一键关）
- 🔄 **检查 DSH 更新** —— 查 npm registry，semver 完整比较（含 `-rc` 预发布），命中给出可直接复制的升级命令
- 📦 **绿色分发** —— 全部数据在 exe 旁的 `dsh-shell-data/`，删目录即恢复出厂；token 只存内存，日志一律打码

## 🧰 系统要求

| | |
|---|---|
| 操作系统 | Windows 10 / 11（x64） |
| WebView2 Runtime | 现代 Windows 自带；没有的话[装一个](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) |
| DSH | 本机已安装 `@deepseek-ai/dsh`（壳负责启动它） |

## 📥 下载与安装

从 [Releases](https://github.com/q3vJXQtx/dsh-shell/releases) 下载：

- **安装版** `DSH.Shell_0.1.0_x64-setup.exe` —— NSIS，CurrentUser 安装、不需要管理员权限
- **便携版** `DSH.Shell_0.1.0_x64-portable.exe` —— 单文件，扔进任意文件夹双击即运行，
  数据落在 exe 旁的 `dsh-shell-data/`，删目录即恢复出厂

首次运行如果出现 SmartScreen 提示「Windows 已保护你的电脑」：
点「更多信息」→「仍要运行」（未做代码签名，属正常现象）。

## 🚀 使用说明

### 第一次启动

1. 运行程序，控制台会依次亮过 5 个阶段：
   **定位 DSH 命令 → 检查运行环境 → 启动后端 → 等待就绪 → 获取访问凭证**
2. 就绪后，窗口下半部分出现 DSH 聊天页，直接开聊；
3. 如果失败，窗口会告诉你失败原因（例如没找到 DSH 时列出**所有搜索过的路径**），
   并提供重试 / 诊断按钮——陈旧锁、启动即崩这类常见故障会先走自动修复。

### 窗口与托盘

- 托盘左键唤回窗口；窗口右上角「关闭」默认是**收进托盘**（托盘不可用时自动降级为真退出，不会出现"藏进一个收不回来的托盘"）
- 托盘右键菜单：显示主窗口 / 重启后端 / 停止后端 / 在浏览器中打开 / 诊断面板 / 检查 DSH 更新 / 设置 / 退出
- 鼠标悬停托盘图标，一行提示文字反映后端状态（启动中会连当前阶段一起显示）
- 想**彻底退出**：托盘右键 → 退出。退出时壳会收掉自己拉起的 `dsh web`；你另外手动开的 DSH 实例它不动

### 设置（启动 / 行为 / 网络 三组）

- **端口** —— 改了自动重启后端，不需要你操心"改完没生效"
- **DSH 定位** —— 可手动指定 dsh 可执行文件路径、控制是否允许 npx 兜底
- **`--no-open`** —— 启动时抑制默认浏览器弹窗；提供「重新探测」按钮，升级 DSH 后不用删配置文件
- **行为** —— 关窗进托盘、任务完成通知（关掉后连事件流都不订阅，零打扰）
- **网络** —— GitHub 加速镜像开关；版本检查的代理方式（自动探测 / 直连 / 自定义）

### 数据放在哪

```text
<exe 所在目录>/dsh-shell-data/
├── settings.json          用户配置
├── dsh-shell.log          运行日志（token 已打码）
└── diagnostics-*.txt      你手动导出的诊断报告
```

删除整个目录即恢复出厂状态。唯一的注册表痕迹是通知所需的
`HKCU\Software\Classes\AppUserModelId\com.dshshell.desktop`，不想要可以删。

## ❓ 常见问题

**Q：启动报"没找到 DSH"？**
失败面板会列出所有搜索过的路径。装好 DSH，或在设置里手动指定可执行文件路径。

**Q：窗口一片空白？**
在 `dsh-shell.log` 里搜「控制台页面」：

| 日志 | 含义 |
|---|---|
| `控制台页面加载完成 \| http://tauri.localhost/` | 正常 |
| `控制台页面加载完成 \| http://localhost:1420/` | 这个 exe 是"编译期 release、运行期 dev"的构建，见「开发」一节的构建提示 |
| 完全没有这两行 | WebView 没起来，检查 WebView2 Runtime |

**Q：端口被占用？**
诊断面板会识别占用者：上次遗留的 dsh（可接管）、外部启动的 dsh（可安全换端口）、还是无关进程（显示 PID）。

**Q：收不到任务完成通知？**
依次检查：设置里「任务完成通知」是否开启 → Windows 通知总开关 → 便携版首次运行会自动注册 AUMID（失败时日志有记录）。

**Q：GitHub 请求走镜像安全吗？**
只改写 GitHub 公开资源的下载请求，**不包含** `api.github.com`（不会把 GitHub 凭据交给第三方服务）；且随时可在设置里关闭。

## 🛠️ 开发

### 环境

- Node.js ≥ 18（含 npm）
- Rust stable，`x86_64-pc-windows-msvc` 目标
- Visual Studio Build Tools（C++ 工作负载）+ Windows 10/11 SDK

### 常用命令

```bash
npm install
npm run tauri dev      # 开发：Vite 热更新 + Tauri 窗口
npm run tauri build    # 打包：产出 NSIS 安装包
npm run build          # 只构建前端（tsc + vite）
cd src-tauri && cargo test --lib   # 跑测试（含 capability 权限守卫）
```

> **⚠️ 不要直接 `cargo build --release`** —— 少了 `--features custom-protocol`
> 会产出一个"运行期是 dev 模式"的二进制：不内嵌前端、去找 `localhost:1420`、
> 没有 dev server 就**白屏**。正规打包请用 `npm run tauri build`。
> 背景见 [设计决策 15](docs/DESIGN.md)。

仓库根的 `build.cmd` 是作者本机环境（Rust 装在非常规路径、Git Bash 的
`link.exe` 会遮蔽 MSVC 链接器）的构建包装，干净环境用不到，直接 `npm run tauri build`。

### CI（GitHub Actions）

| 触发 | 内容 |
|---|---|
| push `main` / PR | `tsc + vite build` → `cargo test --lib` → release 编译 |
| 推送 `v*` tag | CI 通过后产出 NSIS 安装包，自动创建 **draft Release** |

全部跑在 `windows-latest`，见 [`.github/workflows/build.yml`](.github/workflows/build.yml)。

### 想深入"为什么这样设计"？

15 条设计决策的完整叙事——每一条都对应一类真实踩过的坑——在
**[docs/DESIGN.md](docs/DESIGN.md)**。

## 🕘 版本与更新

- 版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)；逐版本的变更记录见
  [CHANGELOG.md](CHANGELOG.md)，构建产物见 [Releases](https://github.com/q3vJXQtx/dsh-shell/releases)。
- **壳的升级**：从 Releases 下载新版覆盖即可（安装版直接装新版，便携版换 exe）。
  壳刻意不做自动替换——各人的 DSH 安装方式不同，悄悄改动环境的收益小于风险。
- **DSH 的升级**：托盘或设置里的「检查 DSH 更新」，命中新版本时给出可复制的
  `npm install -g @deepseek-ai/dsh@latest` 命令，是否执行由你决定。

## 🤝 贡献

Issue 和 PR 都欢迎。改动权限相关（`capabilities/`、`tauri.conf.json` 的 CSP）
请先跑 `cargo test --lib`——有一条守卫测试专门盯着这类配置。

## 📄 License

[MIT](LICENSE)
