import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import DiagnosticsPanel from "./components/DiagnosticsPanel";
import FailurePanel from "./components/FailurePanel";
import PhasePanel, { HealingPanel } from "./components/PhasePanel";
import SettingsPanel from "./components/SettingsPanel";
import type { BackendState, Bounds, Snapshot, UpdateStatus } from "./types";

/**
 * 应用外壳。
 *
 * # 关于"为什么用轮询同时还要监听事件"
 *
 * 只用事件：如果后端在前端挂载**之前**就绪，`shell://ready` 会被完全错过，
 * 前端就永远停在"启动中"——这类"时序性卡死"正是参考项目最难查的一类问题。
 * 只用轮询：状态变化会有最多 1 秒延迟，点按钮后的反馈显得迟钝。
 *
 * 因此两者都用：事件负责**即时性**，轮询负责**兜底与自恢复**。
 * 轮询是这里的"真相来源"，即使事件全丢也不会卡死。
 */
export default function App() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [diagOpen, setDiagOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  /** 操作失败时的提示（例如接管被安全闸门拦下） */
  const [notice, setNotice] = useState<string | null>(null);
  /**
   * DSH 版本检查结果。
   *
   * 由后端通过 `shell://update` 事件推来 —— 检查要走网络，
   * 做成同步命令会把调用方挂住好几秒。
   */
  const [update, setUpdate] = useState<UpdateStatus | null>(null);
  /** 「复制升级命令」的短暂回执 */
  const [copied, setCopied] = useState(false);

  const hostRef = useRef<HTMLDivElement>(null);
  /** 已经加载到聊天视图里的 URL，避免重复导航 */
  const loadedUrl = useRef<string | null>(null);

  // ---- 快照刷新 ----------------------------------------------------------
  const refresh = useCallback(async () => {
    try {
      setSnap(await invoke<Snapshot>("get_snapshot"));
    } catch {
      // 启动极早期命令可能尚未注册，忽略即可（下一轮会成功）
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 1000);

    let unlisten: (() => void) | undefined;
    void listen("shell://state", () => void refresh()).then((fn) => {
      unlisten = fn;
    });

    // 托盘菜单要求打开某个面板。
    // 面板的开关状态归 React 管，后端只发意图，不直接操作界面。
    let unlistenPanel: (() => void) | undefined;
    void listen<string>("shell://open-panel", (e) => {
      if (e.payload === "diagnostics") setDiagOpen(true);
      else if (e.payload === "settings") setSettingsOpen(true);
    }).then((fn) => {
      unlistenPanel = fn;
    });

    // DSH 版本检查结果。启动后会自动查一次，托盘的「检查 DSH 更新」
    // 和设置面板里的按钮也走同一条事件 —— 三处共用一个展示逻辑。
    let unlistenUpdate: (() => void) | undefined;
    void listen<UpdateStatus>("shell://update", (e) => {
      setUpdate(e.payload);
    }).then((fn) => {
      unlistenUpdate = fn;
    });

    return () => {
      window.clearInterval(timer);
      unlisten?.();
      unlistenPanel?.();
      unlistenUpdate?.();
    };
  }, [refresh]);

  // ---- 上报聊天视图应占的矩形 --------------------------------------------
  //
  // 用 ResizeObserver 而不是只监听 window.resize：面板展开/收起、
  // 布局微调都会改变这个容器的尺寸，只监听窗口 resize 会漏掉。
  const reportBounds = useCallback(() => {
    const el = hostRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const bounds: Bounds = {
      x: r.left,
      y: r.top,
      width: r.width,
      height: r.height,
    };
    void invoke("chat_set_bounds", { bounds });
  }, []);

  useEffect(() => {
    reportBounds();
    const el = hostRef.current;
    const ro = new ResizeObserver(reportBounds);
    if (el) ro.observe(el);
    window.addEventListener("resize", reportBounds);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", reportBounds);
    };
  }, [reportBounds]);

  // ---- 就绪后把 DSH 页面装进聊天视图 -------------------------------------
  const readyUrl = snap?.readyUrl ?? null;

  useEffect(() => {
    if (!readyUrl) {
      // 后端退回了未就绪（失败或重启中）：把旧页面销毁，
      // 否则会残留一个连着已经死掉的后端的陈旧页面
      if (loadedUrl.current) {
        loadedUrl.current = null;
        void invoke("chat_destroy");
      }
      return;
    }
    if (loadedUrl.current === readyUrl) return;
    loadedUrl.current = readyUrl;
    reportBounds();
    void invoke("chat_load", { url: readyUrl });
  }, [readyUrl, reportBounds]);

  // ---- 打开面板时隐藏原生视图 --------------------------------------------
  //
  // 原生子 WebView 永远绘制在 DOM 之上，CSS 的 z-index 对它无效。
  // 因此面板打开时必须把原生视图隐藏，关闭后再恢复。
  const overlayOpen = diagOpen || settingsOpen;
  useEffect(() => {
    void invoke(overlayOpen ? "chat_hide" : "chat_show");
  }, [overlayOpen]);

  // ---- 操作 --------------------------------------------------------------
  const restart = useCallback(() => {
    void invoke("start_backend", { restart: true }).then(refresh);
  }, [refresh]);

  const stop = useCallback(() => {
    void invoke("stop_backend").then(refresh);
  }, [refresh]);

  /**
   * 接管端口上已有的那个 DSH 实例。
   *
   * 失败时必须让用户看到原因（比如"端口上的服务已经不是 DSH 了"），
   * 否则按钮点下去界面毫无变化，用户只会以为程序卡住了。
   */
  const takeover = useCallback(async () => {
    setNotice(null);
    try {
      await invoke("takeover_foreign_dsh");
    } catch (e) {
      setNotice(typeof e === "string" ? e : String(e));
    } finally {
      await refresh();
    }
  }, [refresh]);

  const state = snap?.state;
  const isReady = state?.status === "ready";
  const busy = state?.status === "starting" || state?.status === "healing";

  /**
   * 复制升级命令到剪贴板。
   *
   * 刻意**只复制、不自动执行**：各人的 DSH 安装方式不同
   * （全局 npm / pnpm / 项目内 / npx 缓存），对应的升级命令也不同，
   * 猜错了会悄悄改坏 node 环境。这个代价远高于"用户自己粘贴一次"的麻烦。
   */
  const copyCommand = useCallback(async (command: string) => {
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      setNotice("复制失败，请手动选中命令复制");
    }
  }, []);

  return (
    <div className="app">
      <header className="header">
        <span className="brand">DSH</span>
        <StatusBadge state={state} />

        <span className="spacer" />

        {isReady && (
          <button
            className="ghost"
            onClick={() => void invoke("open_in_browser")}
            title="用系统默认浏览器打开同一个页面"
          >
            用浏览器打开
          </button>
        )}
        <button className="ghost" onClick={restart} disabled={busy}>
          {busy ? "启动中…" : "重启后端"}
        </button>
        <button className="ghost" onClick={stop} disabled={busy || !isReady}>
          停止
        </button>
        <button onClick={() => setDiagOpen(true)}>诊断</button>
        <button onClick={() => setSettingsOpen(true)}>设置</button>
      </header>

      <div className="main">
        {/* 原生子 WebView 覆盖在这块区域上；这里只负责提供尺寸 */}
        <div className="chat-host" ref={hostRef} />

        {(notice || update?.status === "available") && (
          <div className="notices">
            {notice && (
              <div className="notice" role="alert">
                <span>{notice}</span>
                <span className="spacer" />
                <button className="ghost" onClick={() => setNotice(null)}>
                  知道了
                </button>
              </div>
            )}

            {update?.status === "available" && (
              <div className="notice info" role="status">
                <span>
                  DSH 有新版本：v{update.current} → <strong>v{update.latest}</strong>
                </span>
                <span className="spacer" />
                <button
                  className="ghost"
                  onClick={() => void copyCommand(update.command)}
                  title="复制升级命令，在终端里自行执行"
                >
                  {copied ? "已复制" : "复制升级命令"}
                </button>
                <button className="ghost" onClick={() => setUpdate(null)}>
                  忽略
                </button>
              </div>
            )}
          </div>
        )}

        {!isReady && (
          <div className="overlay">
            {state?.status === "starting" && (
              <PhasePanel phase={state.phase} method={state.method} />
            )}
            {state?.status === "healing" && (
              <HealingPanel
                attempt={state.attempt}
                maxAttempts={state.maxAttempts}
                lastError={state.lastError}
              />
            )}
            {state?.status === "failed" && (
              <FailurePanel
                reason={state.reason}
                onRetry={restart}
                onDiagnostics={() => setDiagOpen(true)}
                onSettings={() => setSettingsOpen(true)}
                onTakeover={takeover}
              />
            )}
            {(!state || state.status === "idle") && (
              <div className="panel">
                <h2>{snap ? "尚未启动" : "正在初始化…"}</h2>
                <p className="lead">
                  {snap
                    ? "点上方「重启后端」即可开始启动 DSH 服务。"
                    : "正在读取本程序的后端状态。"}
                </p>
              </div>
            )}
          </div>
        )}

        {diagOpen && snap && (
          <DiagnosticsPanel snapshot={snap} onClose={() => setDiagOpen(false)} />
        )}
        {settingsOpen && snap && (
          <SettingsPanel
            settings={snap.settings}
            update={update}
            onCheckUpdate={() => void invoke("check_dsh_update")}
            onClose={() => setSettingsOpen(false)}
            onSaved={() => void refresh()}
          />
        )}
      </div>
    </div>
  );
}

/** 顶栏状态徽章：把状态机的 5 个状态映射成一眼可懂的标签 */
function StatusBadge({ state }: { state: BackendState | undefined }) {
  if (!state) {
    return (
      <span className="badge idle">
        <span className="dot" />
        连接中
      </span>
    );
  }
  switch (state.status) {
    case "idle":
      return (
        <span className="badge idle">
          <span className="dot" />
          未启动
        </span>
      );
    case "starting":
      return (
        <span className="badge warn">
          <span className="dot" />
          启动中
        </span>
      );
    case "ready":
      return (
        <span className="badge ok">
          <span className="dot" />
          已就绪
        </span>
      );
    case "healing":
      return (
        <span className="badge warn">
          <span className="dot" />
          自动恢复中 {state.attempt}/{state.maxAttempts}
        </span>
      );
    case "failed":
      return (
        <span className="badge err">
          <span className="dot" />
          启动失败
        </span>
      );
  }
}
