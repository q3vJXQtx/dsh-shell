# Changelog

本项目所有值得注意的改动都记录在这里。

格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)。

## [Unreleased]

## [0.1.0] - 2026-09-11

首个公开版本。

### Added

- **启动编排** —— 五阶段可视化（定位 DSH → 环境预检 → 启动 `dsh web` →
  等待就绪 → 抓取访问凭证），失败时给出结构化原因与操作按钮
- **故障自愈** —— 陈旧凭证锁检测与可逆清理（`.lock.stale-<时间戳>` 改名而非删除）、
  启动即崩自动重试（退避）、单实例保护与托盘唤回
- **常驻托盘** —— 关窗进托盘、悬停状态提示；托盘不可用时关窗降级为真退出；
  退出时收掉自己拉起的子进程
- **React 控制台** —— 状态机可视化、诊断面板、设置（端口 / DSH 定位 /
  `--no-open` 重探 / 托盘行为 / 完成通知 / GitHub 镜像 / 代理方式）、日志展示
- **双 WebView 架构** —— 控制台与 DSH 聊天页同窗并存，诊断入口永不因导航消失
- **任务完成通知** —— 订阅 DSH `$events` 事件流，仅会话"真的跑完"才弹
  Windows toast；便携版自带 AUMID 注册兜底；后台判据为「不可见 ∨ 最小化 ∨ 失焦」
- **页面增强** —— 外链接交系统浏览器、链接右键菜单、GitHub 资源镜像加速
  （不含 `api.github.com`，默认开启可关闭）
- **DSH 更新检查** —— npm registry + 与启动同源的版本探测、完整 semver
  （含预发布后缀）、代理回退链，命中时给出复制即用的升级命令
- **诊断导出** —— 状态 / 日志 / 设置 / 进程与命令行 / 运行时目录聚合一份报告落盘
- **CI** —— GitHub Actions：`main`/PR 自动构建与测试；`v*` tag 产出
  NSIS 安装包（`DSH.Shell_<版本>_x64-setup.exe`）并附带单文件便携版
  （`DSH.Shell_<版本>_x64-portable.exe`），CI 通过即自动发布 Release

### Security

- 访问 token 仅存在于内存，日志与诊断报告一律打码
- Tauri capability 最小授权 + 运行时守卫测试锁定（详见 `docs/DESIGN.md`）

[Unreleased]: https://github.com/q3vJXQtx/dsh-shell/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/q3vJXQtx/dsh-shell/releases/tag/v0.1.0
