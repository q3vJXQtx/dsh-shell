import { PHASE_HINT, PHASE_LABEL, PHASE_ORDER, type StartPhase } from "../types";

/**
 * 启动阶段面板。
 *
 * 存在的意义：参考项目在整个启动期间只显示一句「正在启动 DSH…」，
 * 而 `dsh web --help` 探测单次可达 9 秒、npx 首次安装可达数分钟，
 * 期间用户完全不知道卡在哪、还要等多久。
 *
 * 这里把 5 个阶段全部列出并标注进度，让等待变成"可追踪的"。
 */
export default function PhasePanel({
  phase,
  method,
}: {
  phase: StartPhase;
  /** 已选定的启动方式（尚未确定时为空串） */
  method: string;
}) {
  const current = PHASE_ORDER.indexOf(phase);

  return (
    <div className="panel">
      <h2>正在启动 DSH 服务</h2>
      <p className="lead">
        {method ? `启动方式：${method}` : "正在按顺序查找可用的 DSH 命令"}
      </p>

      <ul className="phases">
        {PHASE_ORDER.map((p, i) => {
          const cls = i < current ? "done" : i === current ? "active" : "";
          return (
            <li key={p} className={cls}>
              <span className="marker">{i < current ? "✓" : i + 1}</span>
              <span>
                <div className="name">{PHASE_LABEL[p]}</div>
                {i >= current && <div className="hint">{PHASE_HINT[p]}</div>}
              </span>
            </li>
          );
        })}
      </ul>

      <p className="lead">
        首次启动、或需要临时下载依赖时可能较慢。此过程无需人工干预，
        若长时间没有进展，可随时点右上角「诊断」查看详情。
      </p>
    </div>
  );
}

/**
 * 自愈面板。
 *
 * 参考项目在连续快速崩溃后会**静默停止重试**，界面毫无提示，
 * 用户只能干等；且停止时没有清理内部状态，`DshState` 仍持有已死的 PID。
 * 这里显式告知"正在第几次自动恢复"，让用户知道系统在自救。
 */
export function HealingPanel({
  attempt,
  maxAttempts,
  lastError,
}: {
  attempt: number;
  maxAttempts: number;
  lastError: string;
}) {
  return (
    <div className="panel">
      <h2>DSH 意外退出，正在自动恢复</h2>
      <p className="lead">
        第 {attempt} / {maxAttempts} 次尝试。超过上限后将停止自动重试，
        并给出可操作的排查建议。
      </p>

      <div className="failure">
        <h2>上次退出的原因</h2>
        <p className="detail">{lastError}</p>
      </div>
    </div>
  );
}
