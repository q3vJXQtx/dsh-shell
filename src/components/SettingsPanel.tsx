import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Settings, UpdateStatus, ProxyMode } from "../types";
import { PROXY_MODE_OPTIONS } from "../types";

/**
 * 设置面板。
 *
 * 这里有四处是**专门为了修参考项目的坑**而存在的：
 *
 * 1. 「重新探测 --no-open 支持情况」按钮。
 *    参考项目把探测结果按可执行文件路径缓存、永不过期，用户升级 dsh 后
 *    行为异常却毫无办法（作者注释让他们自己去删 settings.json 里的字段）。
 *    本项目除了让缓存键带上版本号自动失效外，再额外给一个手动入口。
 *
 * 2. 端口变更后自动重启后端。
 *    改了端口却"没生效"是很典型的困惑点，这里由后端自动处理。
 *
 * 3. 「任务完成通知」开关。
 *    通知是主动打扰用户的能力，必须给出关掉它的地方。
 *
 * 4. 「GitHub 加速镜像」开关。
 *    镜像确实能让国内用户装上插件，但它同时也是"把所有 GitHub 请求
 *    交给第三方中转"——有些网络直连更快、也不愿意让请求经手第三方。
 *    默认开启，但开关必须存在。
 *
 * 界面按「启动 / 行为 / 网络」分组：项目多了以后平铺一列会让用户
 * 找不到东西，分组 + 开关卡片是最省解释成本的组织方式。
 */
export default function SettingsPanel({
  settings,
  update,
  onCheckUpdate,
  onClose,
  onSaved,
}: {
  settings: Settings;
  /** DSH 版本检查的当前状态（由顶层通过事件维护） */
  update: UpdateStatus | null;
  onCheckUpdate: () => void;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [draft, setDraft] = useState<Settings>(settings);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  async function save() {
    setBusy(true);
    setMsg(null);
    try {
      await invoke("update_settings", { next: draft });
      setMsg("已保存");
      onSaved();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function recheck() {
    setBusy(true);
    setMsg(null);
    try {
      const ok = await invoke<boolean>("recheck_no_open");
      setMsg(
        ok
          ? "当前 DSH 支持 --no-open，启动时可抑制浏览器弹窗"
          : "当前 DSH 不支持 --no-open，启动时无法抑制浏览器弹窗",
      );
      onSaved();
    } catch (e) {
      setMsg(String(e));
    } finally {
      setBusy(false);
    }
  }

  function set<K extends keyof Settings>(key: K, value: Settings[K]) {
    setDraft((prev) => ({ ...prev, [key]: value }));
  }

  const cacheEntries = Object.keys(draft.noOpenSupport);
  const proxyHint =
    PROXY_MODE_OPTIONS.find((o) => o.value === draft.proxyMode)?.hint ?? "";

  return (
    <div className="drawer">
      <div className="drawer-head">
        <span className="title">设置</span>
        <span style={{ flex: 1 }} />
        <button className="primary" onClick={save} disabled={busy}>
          保存
        </button>
        <button className="ghost" onClick={onClose}>
          关闭
        </button>
      </div>

      <div className="drawer-body">
        {msg && <div className="note">{msg}</div>}

        {/* ---------------- 启动 ---------------- */}
        <section className="settings-section">
          <h3>启动</h3>
          <p className="section-desc">
            这几项决定「用哪个命令、带哪些参数去拉起 DSH」，保存后会自动重启后端生效。
          </p>

          <div className="field">
            <label htmlFor="port">监听端口</label>
            <input
              id="port"
              type="number"
              min={1}
              max={65535}
              value={draft.port}
              onChange={(e) => set("port", Number(e.target.value) || 3080)}
            />
            <p className="desc">默认 3080。端口冲突时这里改一个就行。</p>
          </div>

          <div className="field">
            <label htmlFor="dshPath">DSH 可执行文件路径（可选）</label>
            <input
              id="dshPath"
              type="text"
              placeholder="留空则自动查找（环境变量 → 自定义路径 → PATH → 本地安装）"
              value={draft.customDshPath ?? ""}
              onChange={(e) =>
                set(
                  "customDshPath",
                  e.target.value.trim() === "" ? null : e.target.value,
                )
              }
            />
            <p className="desc">
              当自动查找失败时，可在这里直接指定 <code>dsh</code> 的完整路径。
            </p>
          </div>

          <div className="check">
            <Switch
              id="noOpen"
              checked={draft.noOpen}
              onChange={(v) => set("noOpen", v)}
            />
            <label htmlFor="noOpen">
              启动时不要用系统浏览器打开页面
              <p className="desc">
                勾选后会给 dsh 传 <code>--no-open</code>，页面只在当前窗口内显示。
                当前探测缓存
                {Object.values(draft.noOpenSupport).some(Boolean) ? "显示" : "未显示"}
                DSH 支持该参数（缓存 {cacheEntries.length} 条）。
              </p>
            </label>
          </div>

          <div className="check">
            <Switch
              id="allowNpx"
              checked={draft.allowNpx}
              onChange={(v) => set("allowNpx", v)}
            />
            <label htmlFor="allowNpx">
              允许使用 npx 兜底
              <p className="desc">
                当本机没有任何 dsh 安装时，退而用 npx 临时拉取并运行。
                首次会下载整个包，可能比较慢。
              </p>
            </label>
          </div>

          <div className="actions">
            <button onClick={recheck} disabled={busy}>
              重新探测 --no-open 支持情况
            </button>
          </div>
        </section>

        {/* ---------------- 行为 ---------------- */}
        <section className="settings-section">
          <h3>行为</h3>
          <p className="section-desc">即时生效，不需要重启后端。</p>

          <div className="check">
            <Switch
              id="closeToTray"
              checked={draft.closeToTray}
              onChange={(v) => set("closeToTray", v)}
            />
            <label htmlFor="closeToTray">
              关闭窗口时最小化到托盘
              <p className="desc">
                勾选后点关闭只是把窗口藏进托盘，后端继续运行；
                左键单击托盘图标可唤回窗口，右键菜单里有「退出」。
                不勾选则关闭窗口即退出程序，并一并停掉它启动的 DSH 进程。
              </p>
            </label>
          </div>

          <div className="check">
            <Switch
              id="notifyOnFinish"
              checked={draft.notifyOnFinish}
              onChange={(v) => set("notifyOnFinish", v)}
            />
            <label htmlFor="notifyOnFinish">
              任务完成时发送系统通知
              <p className="desc">
                当 DSH 会话跑完、而窗口并没有在前台时，弹一条 Windows 通知，
                点通知上的「打开窗口」即可回到这里。
                窗口已经在前台时不会打扰——你正看着结果，再弹一次是噪音。
                关闭后本程序也不再订阅会话事件流。
              </p>
            </label>
          </div>

          <div className="check">
            <Switch
              id="ghMirror"
              checked={draft.ghMirror}
              onChange={(v) => set("ghMirror", v)}
            />
            <label htmlFor="ghMirror">
              GitHub 请求走加速镜像
              <p className="desc">
                DSH 插件常要从 GitHub 拉文件，国内直连基本不通。
                勾选后页面里发往 github.com 等域名的请求会被改写成
                <code>gh-proxy.com</code> 前缀（本机与其它站点的请求不受影响）。
                改完需要重新加载页面才生效。
              </p>
            </label>
          </div>
        </section>

        {/* ---------------- 网络 ---------------- */}
        <section className="settings-section">
          <h3>网络</h3>
          <p className="section-desc">
            仅影响壳自己访问外网（检查 DSH 更新）；与本地 DSH
            后端的通信始终直连本机回环，不走代理。
          </p>

          <div className="field">
            <label>版本检查代理</label>
            <div className="seg" role="group" aria-label="代理方式">
              {PROXY_MODE_OPTIONS.map((o) => (
                <button
                  key={o.value}
                  type="button"
                  className={draft.proxyMode === o.value ? "on" : ""}
                  onClick={() => set("proxyMode", o.value as ProxyMode)}
                >
                  {o.label}
                </button>
              ))}
            </div>
            <p className="desc">{proxyHint}</p>
            {draft.proxyMode === "custom" && (
              <>
                <input
                  type="text"
                  placeholder="http://127.0.0.1:7890"
                  value={draft.proxyUrl ?? ""}
                  onChange={(e) =>
                    set(
                      "proxyUrl",
                      e.target.value.trim() === "" ? null : e.target.value,
                    )
                  }
                  style={{ marginTop: 6 }}
                />
                <p className="desc">
                  支持 http/https 代理地址，填<b>混合端口</b>或 <b>Http 端口</b>
                  （如 Clash 混合 7890、Http 7892）。
                  <b>纯 Socks 端口不支持</b>（如 Clash 的 7891、v2rayN 的 10808）——
                  对 Socks 端口说 HTTP 协议只会连上即断。
                </p>
              </>
            )}
          </div>

          <div className="field">
            <label>DSH 版本</label>
            <p className="desc">{updateLine(update)}</p>
            <div className="actions">
              <button
                onClick={onCheckUpdate}
                disabled={update?.status === "checking"}
              >
                {update?.status === "checking" ? "检查中…" : "检查 DSH 更新"}
              </button>
            </div>
            {update?.status === "available" && (
              <p className="desc">
                升级命令：<code className="cmd">{update.command}</code>
                <br />
                本程序只做检查、不代为安装：各人的 DSH 安装方式不同
                （全局 npm / pnpm / 项目内 / npx 缓存），对应的命令也不同。
              </p>
            )}
          </div>
        </section>

        <p className="desc" style={{ margin: "4px 2px 12px" }}>
          配置保存在程序目录下的 <code>dsh-shell-data/settings.json</code>，
          整个应用是绿色的，删除该目录即可恢复出厂状态。
        </p>
      </div>
    </div>
  );
}

/** 滑动开关（视觉化的 checkbox）：原生 input 保留可访问性，只做视觉隐藏 */
function Switch({
  id,
  checked,
  onChange,
}: {
  id: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <span className="switch">
      <input
        id={id}
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span className="track" aria-hidden="true" />
    </span>
  );
}

/**
 * 把版本检查结果渲染成一行可读文本。
 *
 * 五种状态都要有交代，尤其是 `skipped` 和 `failed`——
 * 参考项目失败时界面什么都不显示，用户无法区分
 * "已经是最新"和"根本没查成"。
 */
function updateLine(u: UpdateStatus | null): string {
  if (!u) return "尚未检查。程序启动 10 秒后会自动检查一次。";
  switch (u.status) {
    case "checking":
      return "正在检查…";
    case "upToDate":
      return `已是最新版本 v${u.current}`;
    case "available":
      return `发现新版本：v${u.current} → v${u.latest}`;
    case "skipped":
      return u.reason;
    case "failed":
      return `检查失败：${u.message}`;
  }
}
