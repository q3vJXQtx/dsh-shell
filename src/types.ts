/**
 * 与 Rust 侧 serde 输出一一对应的类型定义。
 *
 * 注意：Rust 上这些类型用了 `rename_all = "camelCase"` +
 * `rename_all_fields = "camelCase"`，因此这里全部按 camelCase 书写。
 * 若两端不一致，症状是"界面上永远显示不出错误原因"——
 * 所以这份文件是前后端契约，改 Rust 侧序列化时必须同步这里。
 */

/** 启动阶段（对应 Rust 的 `StartPhase`） */
export type StartPhase =
  | "resolving"
  | "preflight"
  | "spawning"
  | "waitingReady"
  | "acquiringToken";

/** 结构化失败原因（对应 Rust 的 `FailureReason`，内部标签为 kind） */
export type FailureReason =
  | { kind: "dshNotFound"; searched: string[] }
  | { kind: "staleLock"; path: string; holderPid: number | null }
  | { kind: "portOccupied"; port: number; pid: number | null }
  | { kind: "foreignDshRunning"; port: number; pid: number | null }
  | { kind: "crashLoop"; attempts: number; tail: string }
  | { kind: "readyTimeout"; seconds: number }
  | { kind: "tokenMissing"; hint: string }
  | { kind: "other"; message: string };

/** 后端状态（对应 Rust 的 `BackendState`，内部标签为 status） */
export type BackendState =
  | { status: "idle" }
  | { status: "starting"; phase: StartPhase; method: string }
  | { status: "ready"; owned: boolean; pid: number | null }
  | { status: "healing"; attempt: number; maxAttempts: number; lastError: string }
  | { status: "failed"; reason: FailureReason };

/** 一条日志 */
export interface LogEntry {
  at: string;
  level: "info" | "warn" | "error";
  /** 面向用户的说明 */
  userText: string;
  /** 技术细节，可能为空串 */
  detail: string;
}

/** 版本检查等外网请求的代理方式（对应 Rust 的 ProxyMode） */
export type ProxyMode = "direct" | "auto" | "custom";

/** 用户配置 */
export interface Settings {
  port: number;
  customDshPath: string | null;
  allowNpx: boolean;
  noOpen: boolean;
  noOpenSupport: Record<string, boolean>;
  diagnosticsExpanded: boolean;
  closeToTray: boolean;
  /** 会话跑完时是否发 Windows 通知 */
  notifyOnFinish: boolean;
  /** 是否把 DSH 页面发往 GitHub 的请求改走加速镜像 */
  ghMirror: boolean;
  /** 版本检查等外网请求的代理方式 */
  proxyMode: ProxyMode;
  /** proxyMode === "custom" 时的代理地址，如 http://127.0.0.1:7890 */
  proxyUrl: string | null;
}

/** 代理方式的展示名与一句话说明（设置面板用） */
export const PROXY_MODE_OPTIONS: { value: ProxyMode; label: string; hint: string }[] = [
  { value: "auto", label: "自动探测", hint: "先直连，失败后依次尝试环境变量代理与常见本地代理端口（默认）" },
  { value: "direct", label: "直连", hint: "不走任何代理，直接访问 npm registry" },
  { value: "custom", label: "自定义代理", hint: "只走你填写的代理地址" },
];

/**
 * DSH 版本检查结果（对应 Rust 的 `UpdateStatus`，内部标签为 status）。
 *
 * 注意 `upToDate` / `available` 带的是**DSH 的版本号**，不是壳自己的 ——
 * 这是与参考项目语义上的一个重要区别。
 */
export type UpdateStatus =
  | { status: "checking" }
  | { status: "upToDate"; current: string }
  | { status: "available"; current: string; latest: string; command: string }
  | { status: "skipped"; reason: string }
  | { status: "failed"; message: string };

/** 全量快照 */
export interface Snapshot {
  state: BackendState;
  logs: LogEntry[];
  settings: Settings;
  chatLoaded: boolean;
  /** 后端已就绪时为访问地址；未就绪为 null */
  readyUrl: string | null;
  version: string;
  dataDir: string;
}

/** 聊天视图应占据的矩形（CSS 像素，等价于 Tauri 的 LogicalPosition） */
export interface Bounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** 5 个阶段的顺序与中文名，用于渲染进度条 */
export const PHASE_ORDER: StartPhase[] = [
  "resolving",
  "preflight",
  "spawning",
  "waitingReady",
  "acquiringToken",
];

export const PHASE_LABEL: Record<StartPhase, string> = {
  resolving: "定位 DSH 命令",
  preflight: "检查运行环境",
  spawning: "启动 DSH 服务",
  waitingReady: "等待服务就绪",
  acquiringToken: "获取访问凭证",
};

/** 阶段的一句话说明（告诉用户"这一步在干什么、为什么可能慢"） */
export const PHASE_HINT: Record<StartPhase, string> = {
  resolving: "按顺序查找可用的 dsh：环境变量 → 自定义路径 → PATH → 本地安装",
  preflight: "检查残留的锁文件与端口占用，能自动修复的会在这里修掉",
  spawning: "拉起 dsh 子进程，它的输出会被实时收集以便排障",
  waitingReady: "等待端口开始正常响应（首次运行可能较慢）",
  acquiringToken: "从 dsh 的启动输出中读取本次的访问凭证",
};

/** 失败原因的标题与可操作建议 */
export function failureView(reason: FailureReason): {
  title: string;
  detail: string;
  actions: string[];
} {
  switch (reason.kind) {
    case "dshNotFound":
      return {
        title: "没有找到可用的 DSH 命令",
        detail: "以下启动方式都没有命中：",
        actions: [
          "确认已全局安装：npm i -g @deepseek-ai/dsh",
          "或在「设置」里手动指定 dsh 的可执行文件路径",
          "若使用 pnpm/Volta 等工具链，可尝试打开「允许 npx 兜底」",
        ],
      };
    case "staleLock":
      return {
        title: "DSH 被一个残留的锁文件阻塞",
        detail: `锁文件：${reason.path}${
          reason.holderPid ? `（记录持有者 PID ${reason.holderPid}，该进程已不存在）` : ""
        }`,
        actions: [
          "本程序会尝试自动清理，点「重试」即可",
          "清理方式是**改名备份**而非删除，可随时人工恢复",
        ],
      };
    case "portOccupied":
      return {
        title: `端口 ${reason.port} 被其它程序占用`,
        detail: "该端口上的服务不是 DSH，因此不能直接使用。",
        actions: [
          "在「设置」里改一个监听端口",
          "或关闭占用该端口的程序后点「重试」",
        ],
      };
    case "foreignDshRunning":
      return {
        title: `端口 ${reason.port} 上已有一个 DSH 在运行`,
        detail:
          `它${
            reason.pid ? `（PID ${reason.pid}）` : ""
          }不是本程序启动的，而 DSH 的访问凭证是「进程内生成、只写到自己的输出里」的，` +
          "外部程序拿不到，所以直接打开只会是 401。要么关掉它，要么换个端口各跑各的。",
        actions: [
          "点「复用实例」：不动对方进程，借助浏览器里的持久登录态直接进入页面" +
            "（此模式下任务完成通知不可用；登录态过期时会看到 401）",
          "点「接管并启动」：关掉上面那个实例，再由本程序拉起一个自己托管的（通知、自愈齐全）",
          "或点「打开设置」换一个端口，两个实例互不干扰",
          "接管会终止那个进程，请先确认它不是你正在用的会话",
        ],
      };
    case "crashLoop":
      return {
        title: `DSH 连续 ${reason.attempts} 次启动后异常退出`,
        detail: "已停止自动重试，以免反复崩溃。",
        actions: [
          "打开「诊断」查看 DSH 的最后输出",
          "常见原因：profile 包不完整、Node 版本不兼容、配置损坏",
        ],
      };
    case "readyTimeout":
      return {
        title: `等待服务就绪超过 ${reason.seconds} 秒`,
        detail: "进程还在运行，但端口始终没有给出正常响应。",
        actions: [
          "首次通过 npx 启动需要下载整包，可能较慢，可再等一会儿后点「重试」",
          "打开「诊断」确认进程是否仍在运行",
        ],
      };
    case "tokenMissing":
      return {
        title: "DSH 已启动，但未能取得访问凭证",
        detail: reason.hint,
        actions: [
          "点「重启后端」让本程序重新拉起一个由它自己托管的实例",
          "若反复出现，请点「导出诊断」反馈",
        ],
      };
    case "other":
      return {
        title: "启动失败",
        detail: reason.message,
        actions: ["打开「诊断」查看完整输出", "点「重试」重新开始"],
      };
  }
}
