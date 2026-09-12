# DSH Shell 设计决策全录

> 本文是 [DSH Shell](../README.md) 的设计叙事：每个机制防的是什么、当年在哪里踩的坑。
> 实现以代码为准，本文负责回答「为什么」。

---

## 关键设计

### 1. 陈旧文件锁：从"零处理"到"自动修复"

这是实际踩到的最致命的故障，链路是：

```text
dsh 启动
  → 申请 $DSH_HOME/.credentials.yaml.lock 写锁
  → 锁文件的持有者 PID 早已消亡，但锁没被清理
  → 一直等到超时 → 插件树加载失败 → 进程直接崩溃
  → 壳抓不到 token（dsh 崩溃时不产生任何输出）
  → 停在无 token 的 URL
  → 用户看到 404 / 白屏
```

用户侧的表现（404）与成因（锁文件）毫无表面关联，极难自行排查。
参考项目对此**完全没有处理**——整个 Rust 源码里连锁文件名都没出现过。

本项目（[`selfheal.rs`](../src-tauri/src/selfheal.rs)）：

- 启动**前**预检所有可能的 dsh home（`DSH_HOME` → `~/.dsh` → 当前目录）；
- 锁文件内容就是持有者 PID，据此用 `tasklist` 判断进程是否还活着；
- 判定为陈旧锁时**改名备份**而非删除（`.lock.stale-<时间戳>`），完全可逆；
- 崩溃后若发现是锁问题，会先清理再自动重试。

### 2. 状态管理：4 个原子变量 → 单一状态机 + 代际号

参考项目用 `INTENTIONAL_STOP` / `QUICK_DEATHS` / `AUTH_WALL_HANDLED` /
`BACKEND_RESTARTING` 四个分散的原子变量描述生命周期。多个流程交叉时它们会互相矛盾。

本项目（[`state.rs`](../src-tauri/src/state.rs) + [`shell.rs`](../src-tauri/src/shell.rs)）：

- 所有状态转换集中在一个 `BackendState` 枚举里，**单一可信状态源**；
- 引入**单调递增的代际号**：任何后台流程在每次可能阻塞的等待后都检查
  `is_current(gen)`，发现自己过期就立即静默退出。
  这样旧的启动线程**绝不可能**再写状态、杀进程或覆盖新线程的结果。

### 3. 就绪判定：不能只看状态码

参考项目的探测逻辑把 **任何** 401/403/404/2xx 都当作"就绪"
（注释理由是"认证墙也证明服务在跑"）。副作用很大：任何占用该端口的东西都会被
当成 DSH。实际排查中就撞见过——本机环境变量代理会把访问 `127.0.0.1` 的请求吃掉
并返回 `502 Bad Gateway / upstream connect failed`，这在原逻辑下同样算"就绪"。

本项目（[`probe.rs`](../src-tauri/src/probe.rs)）把判断拆成两层：

- `ProbeResult` —— 端口有没有响应（用于轮询）；
- `ServiceKind` —— 这个响应**到底是什么**（DSH / 代理错误页 / 未知服务）。

并且显式 `try_proxy_from_env(false)`，绕过本机代理环境变量。

### 4. 诊断入口永不消失

参考项目把主 WebView **直接导航**到 DSH 页面。一旦导航，React 应用就被卸载，
诊断、日志、重试入口全部消失——用户在最需要它们的时候（白屏、404）一个都点不到，
只能关掉整个程序重来。

本项目改用**同窗口多 WebView**（[`chat.rs`](../src-tauri/src/chat.rs)）：

```text
┌─ Window "main" ──────────────────────────────┐
│  ┌─ Webview "shell" (React 控制台) ────────┐ │
│  │  状态机可视化 / 诊断面板 / 设置 / 日志   │ │  ← 永不导航
│  └────────────────────────────────────────┘ │
│  ┌─ Webview "chat" (DSH webchat) ──────────┐ │
│  │  http://127.0.0.1:3080/?token=...       │ │  ← 仅在就绪后创建
│  └────────────────────────────────────────┘ │
└──────────────────────────────────────────────┘
```

诊断面板打开时把 `chat` 隐藏，关闭后恢复——因为原生子 WebView 永远绘制在
DOM 之上，CSS 的 `z-index` 对它无效。

> **为什么不用 iframe**：实测 DSH 响应头没有 `X-Frame-Options` 也没有 CSP，
> 技术上允许 iframe。但 iframe 里 `127.0.0.1:3080` 相对 `tauri.localhost`
> 属于**第三方上下文**，Chromium 默认拦截第三方 Cookie，DSH 的登录态会丢。
> 子 WebView 是真正的顶层浏览上下文，完全没有这个问题。

### 5. token 抓取：失败了要说出来

`dsh web` 在 stdout 打印一行带 token 的 URL。该 token 由
`processLaunchToken()` 在**进程内**生成（`randomBytes(32)` → base64url，43 字符）
并存入 `WeakMap`，**不落盘**。因此除了抓 stdout 没有别的获取途径。

> 容易踩的坑：`.credentials.yaml` 里
> `client-connection/browser-session → payload → secret` **不是**这个 token。
> 两者长度同为 43，但用途不同——实测用 secret 当 token 访问会返回 401。

参考项目在这里只做精确子串匹配，抓不到时**静默回退裸 URL**，
用户看到 401/404 却完全不知道是"没抓到 token"。

本项目（[`token.rs`](../src-tauri/src/token.rs)）放宽锚点（只认 `token=`，
大小写不敏感，容忍前缀变化），失败时走
`FailureReason::TokenMissing` 并明确告知用户；写日志前 token 一律打码。

### 6. `noOpenCache` 永不过期

参考项目把「dsh 是否支持 `--no-open`」按**可执行文件路径**缓存。
路径不变缓存就永远有效——用户升级 dsh 后行为异常，且**没有任何界面入口能清除它**。
作者自己的注释都写着"删除 settings.json 里的 noOpenCache 即可重探"，
等于把修复责任推给用户去手改 JSON。

本项目：缓存键构造为 `路径@版本号`，**版本一变键就变**，自动重探；
界面上另有「重新探测」按钮作为双保险。

### 7. 配置损坏不阻塞启动

参考项目对 `settings.json` 读取失败不加处理。本项目
（[`settings.rs`](../src-tauri/src/settings.rs)）遇到损坏的配置会**改名备份**、
回退默认值继续启动，并在日志里明确告诉用户备份到了哪里。
写入走"临时文件 + 改名"，避免写一半被杀导致配置残缺。

### 8. 退出时不留孤儿进程

参考项目退出后 DSH 的 node 进程仍在跑并占着端口，用户下次打开就撞上
"端口被占用"。本项目在窗口关闭与 `RunEvent::Exit` 时都会清理**自己拉起的**
子进程（用 `taskkill /T /F` 杀整棵进程树，只杀自己拉起的，绝不动用户的其它实例）。

#### 崩溃路径上的兜底

上面两条清理路径都依赖**正常退出流程**，而 release 配置是 `panic = "abort"`
（见 `Cargo.toml`）：任意线程 panic 都会让进程**立刻**终止，而且是 abort ——
不走栈展开，所以 `RunEvent::Exit`、窗口事件、清理函数一概不会执行。
DSH 子进程于是成了孤儿，继续占着端口，日志里除了一条 panic 什么线索都没有。

`lib.rs` 因此装了 `std::panic::set_hook`，在 abort 之前把子进程收掉。
hook 里有两条硬约束，都是踩过才知道的：

- **绝不加锁**。子进程句柄存在 `Shell` 的互斥锁里，而 panic 的很可能是
  "正持有那把锁"的线程 —— 再去抢同一把锁就是死锁，而死锁比直接终止更糟：
  进程挂在那里不退，端口一直占着。所以 `shell.rs` 额外用一个
  `AtomicU32` 冗余记录 PID（`OWNED_CHILD_PID`），读它永不阻塞。
- **日志走 `try_lock` 版本**（`logging::log_from_panic_hook`）。panic 有可能
  就发生在日志代码内部，那时锁被同一个线程占着，普通 `lock()` 会自锁死。
  这个版本拿不到锁就跳过内存那份，但**一定写文件** —— release 是 GUI 子系统，
  默认 hook 往 stderr 打的信息根本没有控制台能看到。

### 9. 托盘图标：把"够不着"的风险补回来

`close_to_tray` 默认开启，也就是**关闭窗口 = 藏进托盘**。这带来两个硬要求：

1. **托盘必须能唤回窗口**。左键单击恢复并置前，右键弹菜单
   （显示主窗口 / 重启后端 / 停止后端 / 用浏览器打开 / 诊断 / 检查 DSH 更新 / 设置 / 退出）。
   悬停时的提示文字会跟着后端状态走（"已就绪" / "启动失败"），
   不必先把窗口叫出来就知道后端好不好。
2. **托盘建不起来时绝不允许隐藏窗口**。关闭处理里有一道保命闸门
   （`tray::is_available`）：没有托盘还把窗口藏起来，用户就**再也叫不回来**了。

窗口图标也是**显式**设置的。直接 `WindowBuilder` 建窗口时 `hIcon` 为空，
Windows 会退回窗口类的默认图标——任务栏上就是一个空白方块，
哪怕 exe 里已经嵌好了图标资源。

### 10. 单实例：托盘时代的必需品

有了托盘，用户很容易这样操作：点关闭（藏进托盘）→ 过一会儿忘了 → 又从开始菜单点一次。
没有单实例保护的话，第二个进程会把第一个实例的 DSH 当成"别人的实例"，
两个壳互相重启对方的后端。

本项目的做法是用一个具名互斥体（`Local\DSH-Shell-SingleInstance`）判断，
第二次启动**只做一件事**：按窗口标题找到已有窗口、`ShowWindow(SW_RESTORE)` +
`SetForegroundWindow` 把它叫回来，然后自己退出。
刻意不引入 `tauri-plugin-single-instance`——我们只需要"判断是否已有实例"这一件事，
而 `windows-sys` 已经在依赖里了。

找不到窗口时（比如第一个实例还卡在启动早期）会弹一个说明性提示框，
**绝不静默退出**——静默退出会被当成"程序坏了"。

### 11. 任务完成通知

DSH 会话跑完时弹一条 Windows 通知，点通知上的「打开窗口」即可回到窗口。

判定依据是 DSH 的 `$events` 事件流（`/api/remote.mux` WebSocket 上的逻辑流），
而不是轮询 `session/list`——后者只有快照、没有状态迁移，拿不到
"刚刚完成"这个时刻。整条链路以 DSH 真实源码为准
（`$DSH_HOME/profiles/node_modules/@deepseek-ai/` 下的
`dsh-client-connection` / `dsh-api-gateway` / `dsh-api-remotes`）：

1. **token 换 cookie**：`dsh web` 打印的 `?token=` 只在 `GET /` 上被接受，
   成功时 303 + `Set-Cookie`（签名的 HttpOnly 会话 cookie）；
2. **升级鉴权**：`/api/remote.mux` 的 WS 升级与所有 `/api` 请求一样过
   Host/Origin 同源围栏 + cookie 校验，缺 cookie 是 401、Origin 不对是 403；
3. **打开逻辑流**：发一帧 `open` 打开 `$events`，首帧 `ready` 之后的
   `emit` 帧即转发事件，`api-session/status(sessionId, running)` 携带状态。

并且只在 `running` 由**真变假**的那一步触发。由于 `$events` 重连后**不重放**
存量状态，每次连接成功先用 `session/list` 快照给基线播种——
否则会漏掉"连接之前就在跑、之后跑完"的会话。

还有一个容易被忽略的**旁观者义务**：`$events` 上还有审批请求等
`waterfall` 帧，Gateway 要等所有收到请求的客户端都回应才结算——
旁观者只收不回会把真实浏览器端的审批流程卡死。所以这里收到 waterfall
立即回 `{kind:"next"}` 弃权（绝不回 `result`/`rejected` 替用户做决定）。

判断"用户是否正看着"用了三个条件：可见 **且** 未最小化 **且** 处于前台。
参考项目只查了 `is_visible()`，而**窗口最小化时它仍然返回 `true`**——
于是用户最小化窗口去干别的，任务完成反而收不到通知，
恰好漏掉了最需要通知的场景。

通知是主动打扰用户的能力，所以设置里有开关；关掉后连事件流都不订阅。

便携 exe 还需要自己注册 AppUserModelID：Windows 只为"已注册 AUMID"的进程显示
toast，正常途径是安装器建快捷方式，而本程序刻意不做安装器。
这里按微软文档的注册表替代方案写 HKCU，否则 toast 会被**静默丢弃**
（不报错、不显示，最难查的那种失败）。

### 12. 页面增强：链接行为与 GitHub 加速

DSH 的 webchat 是它自己的前端产物，我们既不该也不能改它的源码。
但 WebView2 支持在文档创建时先跑一段我们的脚本，于是有了这个合法的旁路：

- **链接右键菜单**：在链接上换成「在浏览器中打开 / 复制链接」两项。
  非链接的右键保持 WebView2 默认菜单（用户可能要用「刷新」「检查」）。
  同时接管 `target="_blank"` 与 Ctrl/Shift+点击的外链，导向系统浏览器。
- **GitHub 加速镜像**：把页面里发往 `github.com` 等域名的 `fetch`/`XHR`
  请求改写为 `gh-proxy.com` 前缀。插件常要从 GitHub 拉文件，国内直连基本不通。
  只影响 GitHub 域名，本机与其它站点一律放行；设置里可关闭。

两个细节值得单独说明，都是"看起来能用、实际埋雷"的类型：

- **镜像列表刻意不含 `api.github.com`**。那上面的请求经常带
  `Authorization: Bearer <token>`，而镜像是**第三方服务** ——
  加进去就等于把用户的 GitHub 凭据交给对方。releases / raw / codeload
  这些下载路径靠公开地址或签名查询串即可访问，不需要认证头，走镜像才是安全的。
- **`fetch` 包装整体包在 `try/catch` 里**。`new Request(url, request)` 在
  同一个 Request 的 body 已被消费时会**同步抛异常**，而 `fetch` 的契约是
  "永远返回 Promise、绝不同步抛"。不包住的话，页面里写的
  `fetch(...).catch(...)` 根本接不到，会变成未捕获异常。出错就原样放行：
  宁可这一跳不过镜像，也不能把页面弄崩。

两处都只在**聊天视图**上注入，所以不需要 origin 守卫。
参考项目把脚本注入到主窗口，必须靠硬编码的 `location.origin === '...:3080'` 自保——
那个守卫在用户改了端口之后会**静默失效**，右键菜单和外链接管一起失灵且不报错。
本项目端口可配置，脚本里不出现端口号。

另外 `tauri-plugin-opener` 必须用 `open_js_links_on_click(false)` 注册：
它默认注入的拦截脚本会在冒泡阶段抢走外链点击并改走页面内 IPC，
而 DSH 页面是远程上下文、没有可靠的 IPC 桥，结果是原生新窗口被压掉、
IPC 又调不通，**两头落空**（参考项目 v2.0.2 修的正是这个）。

### 13. 更新检查

检查的是 **DSH 本身**有没有新版本，而不是壳自己。
参考项目的 `update.rs`（623 行）做的是壳自更新：查它自己的 GitHub Release、
下载新 exe、改名替换。那套代码依赖"有一个持续发布 exe 资产的仓库"这个前提，
而本项目尚未发布壳自身的固定版本，照搬只会得到一个多半查不到东西的空壳功能——
比没有更糟，因为它会让人以为"自动更新是开着的"。

DSH 是 npm 包（`@deepseek-ai/dsh`），registry 随时可查；本地版本则通过
**和启动后端同一条候选链**问出来（`dsh --version`），
所以"检查看到的版本"和"实际在跑的版本"必然是同一个。

直连不通时依次尝试环境变量里的代理、再探测本地常见代理端口
（Clash/v2rayN 那套 7890/7897/10808…，先做 1 秒 TCP 探测免得对着死端口白等）。
每一级代理的失败原因都会**带进最终的错误消息**，而不是只报一句笼统的超时。

发现新版本只**报告 + 给出命令**，不代为安装：各人的 DSH 安装方式不同
（全局 npm / pnpm / 项目内 / npx 缓存），猜错了会悄悄改坏 node 环境。
版本比较实现了完整的 semver 规则，包括预发布后缀——
DSH 目前发的是 `0.1.5-rc.1` 这类版本，按点号切分再 `parse::<i64>()`
会把 `5-rc` 解析成 0，得出错误结论。

#### 防重入用的是时间戳，不是布尔量

`dsh --version` 带**超时**（`Command::output()` 是没有超时的）。即便如此，
"当前有检查在跑"这个状态仍然不用布尔标志，而是记**开始时刻的时间戳**：
布尔量一旦因为任何意外没有复位，就会永远停在"进行中"，
此后用户点多少次都只会在日志里留一句"已有一次在进行中"然后被丢弃，
**没有任何面向用户的提示**，功能等于静默死掉。
时间戳超过 5 分钟就当作上一次已经卡死并放行新的检查 ——
卡死最多影响一次检查，不会锁死功能。

### 14. 权限配置：聊天视图一个字都不给

`capabilities/default.json` 只把权限授给 React 控制台（`shell` 这个 WebView）。
原因是 Tauri 的匹配逻辑有个很容易踩的地方
（`tauri::ipc::RuntimeAuthority::resolve_access`）：

```rust
origin.matches(&cmd.context)
  && (cmd.webviews.iter().any(|w| w.matches(webview))
      || cmd.windows.iter().any(|w| w.matches(window)))
```

中间是 **`||` 而不是 `&&`** —— `webviews` 和 `windows` 命中**任意一个**就授权。
而聊天视图与 React 控制台**同处一个窗口 `main`**，
所以 `windows` 里只要留着 `"main"`，那个加载远程 DSH 页面的视图
就会跟着拿到整套权限，其中包括 `opener:default`：
页面一旦被 XSS，就能驱动本机打开任意 URL。

因此这里 `windows` 是**空的**，只靠 `webviews: ["shell"]` 匹配。

第二道锁是来源限制：`is_local_url` 只认 `tauri://` 与 `devUrl`/`frontendDist`，
聊天视图的 `http://127.0.0.1:3080` 会被判为 `Remote`，而 capability
不声明 `remote` 就等于拒绝。但这道锁**依赖默认值**，不够明确，
所以配置里显式写了 `local: true`。

两条约束都由 `lib.rs` 里的 `capability_guard` 测试钉住：配置被改错时
`cargo test` 直接失败，而不是等出事之后才发现。

#### 第三道锁：外链出口

权限收紧**并不能**覆盖全部路径。`NewWindowResponse::Deny` 挡的是
"在应用内开一个新窗口"，它**并不拦截 URL 本身** —— 返回 `Deny` 之前，
URL 已经交给了系统 opener（Windows 上最终落到 `ShellExecute`）。
于是 DSH 页面只要一句 `window.open('file:///C:/Windows/System32/calc.exe')`，
或者调一个注册表里登记过的自定义协议，就能拉起本机程序。

所以 `chat.rs` 的 `on_new_window` 会先校验 scheme，非 http/https 一律拦下并记日志
（降级出来的独立窗口走同一套判断，安全边界没有理由更松）。

> 注意：`inject.rs` 的注入脚本里也有同样的 http/https 判断，但那只覆盖
> 用户**点击链接**这条路。页面直接调 `window.open` 会绕过它。
> 真正拦得住的是 `on_new_window`。

### 15. 两个"漏了也不报错"的地方

有一类缺陷不会让代码编译失败、也不会让功能出错，只会让用户觉得"这软件有点糙"，
或者让程序在跑了几周之后悄悄变慢。这两处就是。

#### 子进程一律不许弹控制台窗口

本程序是 GUI 子系统（`windows_subsystem = "windows"`），从它启动的控制台程序
**默认会新建一个黑窗**并一闪而过。对 `dsh --version`、`tasklist`、`netstat`
这些纯后台探测来说，闪窗比不做事还糟。

原先这类调用散落在 6 个地方、各自抄一遍 `creation_flags`，结果**漏了 2 处**：

- `lifecycle::probe_version` —— 它**每次启动都会执行**（要先拿版本号才能算缓存键、
  再去查缓存），所以是必然闪窗；
- `selfheal::process_alive` —— 有锁文件时触发。

现在统一收敛到 `lifecycle::hide_console`，调用点写一行即可。
漏写这件事完全静默，所以只能靠"只有一个地方要写对"来防。

同时这类探测全部走 `lifecycle::output_with_timeout`：`Command::output()`
**没有超时**，对方一旦挂住（等 stdin、卡在启动脚本里）调用方就永远等下去 ——
而 `probe_version` 在**启动路径**上，挂住等于程序卡在启动。

#### 日志是有上限的

- **内存**：`all()` 会被前端每秒一次的 `get_snapshot` 调用，而那是全量克隆 +
  序列化 + 走 IPC。原先的 `Vec` 无上限也从不清理，于是内存和每次快照的开销
  都会随时间线性上涨 —— 程序定位是常驻托盘跑几天，这个增长没有终点。
  现在与子进程输出同样用环形缓冲封顶（2000 条，远超排障所需）。
- **磁盘**：日志文件超过 2MB 就轮转成 `.1`，只保留一份历史。
- **事件流**：未知事件类型**只记一次**（原先注释写着"不能每帧都记"，
  代码却每帧都记 —— DSH 改个事件名就能把有上限的日志缓冲刷满，
  把真正有用的排障记录挤掉）。去重集合自身也有上限，
  免得对面不断换名字把它撑大。

---

## 模块结构

| 文件 | 职责 |
|---|---|
| [`state.rs`](../src-tauri/src/state.rs) | 状态机、失败原因、自愈策略 |
| [`shell.rs`](../src-tauri/src/shell.rs) | 编排：候选链 → 预检 → 拉起 → 等待 → 取凭证；代际号 |
| [`lifecycle.rs`](../src-tauri/src/lifecycle.rs) | 候选链构建、进程拉起、崩溃签名识别、`--no-open` 探测；`hide_console` / `output_with_timeout`（所有子进程调用都要过这两个） |
| [`selfheal.rs`](../src-tauri/src/selfheal.rs) | 陈旧锁探测与可逆清理 |
| [`probe.rs`](../src-tauri/src/probe.rs) | 就绪探测与服务识别（含代理误答识别） |
| [`token.rs`](../src-tauri/src/token.rs) | token 抓取、打码、URL 构造 |
| [`logging.rs`](../src-tauri/src/logging.rs) | 带用户可读摘要的日志、输出环形缓冲、文件轮转、诊断导出、panic hook 专用写入 |
| [`settings.rs`](../src-tauri/src/settings.rs) | 配置读写、损坏回退、路径解析 |
| [`chat.rs`](../src-tauri/src/chat.rs) | 内嵌聊天 WebView 的创建/定位/隐藏 |
| [`tray.rs`](../src-tauri/src/tray.rs) | 托盘图标、右键菜单、状态提示文字 |
| [`single_instance.rs`](../src-tauri/src/single_instance.rs) | 具名互斥体判重 + 唤回已有窗口 |
| [`notify.rs`](../src-tauri/src/notify.rs) | 会话事件流订阅、完成通知、AUMID 注册 |
| [`inject.rs`](../src-tauri/src/inject.rs) | 注入脚本：链接右键菜单、GitHub 镜像 |
| [`update.rs`](../src-tauri/src/update.rs) | DSH 版本检查、semver 比较、代理回退链 |
| [`lib.rs`](../src-tauri/src/lib.rs) | Tauri 装配、命令注册、窗口与 WebView 搭建 |

前端（`src/`）是纯展示层：每 1 秒拉一次全量快照（**兜底，保证任何情况下不卡死**），
同时监听 `shell://state` 事件做即时刷新；两者都用，
因为只用事件会漏掉"后端比前端先就绪"这一类时序问题。

---

## 兼容性说明

本项目与 DSH 的耦合点只有三处，且都做了容错：

1. **子命令与参数** —— 用 `dsh web --port <n> [--no-open]`
   （`web` 是 `--profile web` 的别名，已实测确认）；
2. **stdout 中的 token** —— 放宽为只认 `token=` 锚点；
3. **凭证锁文件名** —— `.credentials.yaml.lock`（与 `dsh-atomic-write` 一致）。

除此外不依赖任何 DSH 内部实现。
