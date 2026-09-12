import { useEffect, useRef, useState } from "react";
import type { Snapshot } from "../types";
import { invoke } from "@tauri-apps/api/core";

/**
 * 诊断面板。
 *
 * # 为什么它必须"永不消失"
 *
 * 参考项目把主 WebView 直接导航到 DSH 页面，一旦导航 React 应用就被卸载，
 * 诊断/日志/重试入口全部消失——用户在最需要它们的时候（白屏、404）一个都点不到，
 * 只能关掉整个程序重来。
 *
 * 本项目里 React 所在的 WebView 从不导航，所以这个面板**任何状态下都打得开**。
 * 打开时会把原生子 WebView 隐藏（见 `App.tsx` 的 `chat_hide`），
 * 因为原生 WebView 永远盖在 DOM 之上，靠 CSS 是压不住的。
 */
export default function DiagnosticsPanel({
  snapshot,
  onClose,
}: {
  snapshot: Snapshot;
  onClose: () => void;
}) {
  const [savedPath, setSavedPath] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const logEndRef = useRef<HTMLDivElement>(null);

  // 新日志进来时滚到底部（排障时最关心最新一条）
  useEffect(() => {
    logEndRef.current?.scrollIntoView({ block: "end" });
  }, [snapshot.logs.length]);

  const { state } = snapshot;

  async function handleSave() {
    try {
      const path = await invoke<string>("save_diagnostics");
      setSavedPath(path);
    } catch (e) {
      setSavedPath(`保存失败：${String(e)}`);
    }
  }

  async function handleCopy() {
    try {
      const text = await invoke<string>("export_diagnostics");
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1800);
    } catch {
      setCopied(false);
    }
  }

  return (
    <div className="drawer">
      <div className="drawer-head">
        <span className="title">诊断信息</span>
        <span className="spacer" style={{ flex: 1 }} />
        <button onClick={handleCopy}>{copied ? "已复制" : "复制报告"}</button>
        <button onClick={handleSave}>导出到文件</button>
        <button className="ghost" onClick={onClose}>
          关闭
        </button>
      </div>

      <div className="drawer-body">
        <dl className="kv">
          <dt>壳版本</dt>
          <dd>{snapshot.version}</dd>

          <dt>后端状态</dt>
          <dd>{describeState(state)}</dd>

          <dt>监听端口</dt>
          <dd>{snapshot.settings.port}</dd>

          <dt>数据目录</dt>
          <dd>{snapshot.dataDir}</dd>

          <dt>启动方式</dt>
          <dd>
            {state.status === "starting" && state.method
              ? state.method
              : state.status === "ready"
                ? "（已就绪）"
                : "（未确定）"}
          </dd>

          <dt>访问凭证</dt>
          <dd>{snapshot.readyUrl ? "已取得（日志中已打码）" : "未取得"}</dd>
        </dl>

        {savedPath && (
          <div className="note">
            诊断报告已保存到：{savedPath}
            <div style={{ marginTop: 8 }}>
              <button
                onClick={() => void invoke("reveal_data_dir")}
                className="ghost"
              >
                打开所在文件夹
              </button>
            </div>
          </div>
        )}

        <h2 style={{ fontSize: 13, margin: "0 0 8px" }}>日志</h2>
        <div className="logs">
          {snapshot.logs.length === 0 && (
            <div className="empty">暂无日志</div>
          )}
          {snapshot.logs.map((l, i) => (
            <div key={i} className={`log-row ${l.level}`}>
              <span className="at">{l.at.slice(11)}</span>
              <span className="lvl">{l.level.toUpperCase()}</span>
              <span className="txt">
                {l.userText}
                {l.detail && <span className="det">{l.detail}</span>}
              </span>
            </div>
          ))}
          <div ref={logEndRef} />
        </div>
      </div>
    </div>
  );
}

/** 把状态机状态压成一行可读文本 */
function describeState(s: Snapshot["state"]): string {
  switch (s.status) {
    case "idle":
      return "空闲（尚未启动）";
    case "starting":
      return `启动中（${s.phase}）`;
    case "ready":
      return `已就绪${s.pid ? `，PID ${s.pid}` : ""}`;
    case "healing":
      return `自愈中（第 ${s.attempt}/${s.maxAttempts} 次）`;
    case "failed":
      return `失败（${s.reason.kind}）`;
  }
}
