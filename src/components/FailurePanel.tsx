import { useState } from "react";

import { failureView, type FailureReason } from "../types";

/**
 * 失败引导面板。
 *
 * 参考项目的做法是把一坨多行技术文本塞进灰色 `<div>`，
 * 用户看得到却看不懂、也无从下手。这里把结构化的失败原因
 * 翻译成「发生了什么 + 你可以做什么」，操作建议按顺序排列。
 *
 * # 关于「接管并启动」为什么要二次确认
 *
 * 那是个**破坏性操作**：会杀掉端口上另一个 DSH 进程（连同它的子进程）。
 * 它可能是用户正在用的会话，所以不能一次点击就执行。
 * 这里用组件内的两步确认、而不是 `window.confirm()`——
 * 原生对话框在部分 WebView 环境下会被静默忽略，
 * 那会导致"点了没反应"，比没有确认更糟。
 */
export default function FailurePanel({
  reason,
  onRetry,
  onDiagnostics,
  onSettings,
  onTakeover,
}: {
  reason: FailureReason;
  onRetry: () => void;
  onDiagnostics: () => void;
  onSettings: () => void;
  /** 仅「端口上已有别的 DSH」时需要；返回的 Promise 便于展示进行中状态 */
  onTakeover?: () => Promise<void>;
}) {
  const view = failureView(reason);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);

  const canTakeover = reason.kind === "foreignDshRunning" && !!onTakeover;

  const doTakeover = async () => {
    if (!onTakeover) return;
    setBusy(true);
    try {
      await onTakeover();
    } finally {
      // 无论成败都复位：失败时面板会换成新的状态，
      // 若被再次渲染出来，按钮必须是可点的
      setBusy(false);
      setConfirming(false);
    }
  };

  return (
    <div className="panel">
      <div className="failure">
        <h2>{view.title}</h2>
        <p className="detail">{view.detail}</p>

        {reason.kind === "dshNotFound" && reason.searched.length > 0 && (
          <ul>
            {reason.searched
              .flatMap((s) => s.split("\n"))
              .filter((s) => s.trim())
              .map((s, i) => (
                <li key={i}>{s}</li>
              ))}
          </ul>
        )}

        {reason.kind === "crashLoop" && reason.tail.trim() && (
          <p className="detail">{reason.tail.trim().split("\n").slice(-8).join("\n")}</p>
        )}
      </div>

      <h2>可以这样处理</h2>
      <ul>
        {view.actions.map((a, i) => (
          <li key={i}>{a}</li>
        ))}
      </ul>

      <div className="actions">
        {canTakeover && !confirming && (
          <button className="primary" onClick={() => setConfirming(true)} disabled={busy}>
            接管并启动
          </button>
        )}

        {canTakeover && confirming && (
          <>
            <span className="confirm-hint">
              会终止 PID {reason.kind === "foreignDshRunning" && reason.pid ? reason.pid : "未知"} 的进程，
              确认吗？
            </span>
            <button className="primary danger" onClick={() => void doTakeover()} disabled={busy}>
              {busy ? "正在接管…" : "确认接管"}
            </button>
            <button onClick={() => setConfirming(false)} disabled={busy}>
              取消
            </button>
          </>
        )}

        {!confirming && (
          <>
            <button className={canTakeover ? undefined : "primary"} onClick={onRetry}>
              重试
            </button>
            <button onClick={onDiagnostics}>打开诊断</button>
            <button onClick={onSettings}>打开设置</button>
          </>
        )}
      </div>
    </div>
  );
}
