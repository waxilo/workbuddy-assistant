import { useEffect, useState } from "react";
import type { Account, RouteLog, Settings, StealthStatus } from "../types";
import {
  applySettings,
  getSettings,
  proxyRoutes,
  saveSettings,
  stealthStatus,
  stealthStop,
} from "../api";
import type { ConfirmReq, Toast } from "../common";

/**
 * 「无感接管」页。
 *
 * 它牵涉改 WorkBuddy 全局配置 + 重启桌面端与 CLI host，
 * 是整个应用里唯一的高危操作，值得一个专属页面和完整的状态面板
 * （开关 / 端口 / 优先扣费账号 / 心跳状态 / 最近路由），不与普通设置混排。
 */
export function TakeoverPage({
  settings,
  accounts,
  askConfirm,
  onSettings,
  onToast,
}: {
  settings: Settings;
  /** 账号列表：优先扣费账号下拉需要展示账号名 */
  accounts: Account[];
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 保存 / 停止接管后把最新 settings 同步回外层（后端可能已代为改写字段） */
  onSettings: (s: Settings) => void;
  onToast: (t: Toast) => void;
}) {
  const [proxyOn, setProxyOn] = useState(settings.proxy_enabled);
  const [proxyPort, setProxyPort] = useState(String(settings.proxy_port || 8787));
  // 优先扣费账号：空串 = 自动（粘滞 + 最旧积分优先）
  const [preferred, setPreferred] = useState(settings.preferred_account_id ?? "");
  // 接管状态（是否真的装上、心跳是否还活着）不属于 settings，要单独查
  const [stealth, setStealth] = useState<StealthStatus | null>(null);
  const [routes, setRoutes] = useState<RouteLog[]>([]);
  const [stealthBusy, setStealthBusy] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  /** 刷新接管状态与最近路由。接管是后台线程异步装卸的，所以要给用户一个「刷新」。 */
  const refreshStealth = async () => {
    try {
      setStealth(await stealthStatus());
      setRoutes(await proxyRoutes());
    } catch (e) {
      onToast({ kind: "err", text: "读取接管状态失败：" + String(e) });
    }
  };

  useEffect(() => {
    void refreshStealth();
    const t = window.setInterval(() => void refreshStealth(), 15000);
    return () => window.clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /**
   * 立即停止接管。不弹保存、不等确认字段 —— 后端直接摘端点并把
   * proxy_enabled=false 落盘（含安全重启），这是「万一出问题我要马上恢复」的按钮。
   */
  const doStopStealth = async () => {
    const ok = await askConfirm({
      title: "立即停止无感接管",
      body:
        "会摘除接管端点，并在代理仍可用时重启 WorkBuddy 与长驻 CLI host，随后停止监听。" +
        "本应用的数据不会受影响，随时可以再开。继续？",
      okText: "停止接管",
      danger: true,
    });
    if (!ok) return;
    setStealthBusy(true);
    try {
      setStealth(await stealthStop());
      setProxyOn(false); // 后端已落盘，本地开关同步，避免再点保存时状态打架
      try {
        onSettings(await getSettings());
      } catch {
        /* 外层同步失败不影响停止本身 */
      }
      await refreshStealth();
      onToast({ kind: "ok", text: "已停止接管，WorkBuddy 恢复直连" });
    } catch (e) {
      onToast({ kind: "err", text: "停止失败：" + String(e) });
    } finally {
      setStealthBusy(false);
    }
  };

  // 只覆盖接管相关字段，其余设置原样透传 —— 本页对它们没有编辑权
  const snapshot = (): Settings => ({
    ...settings,
    proxy_enabled: proxyOn,
    proxy_port: Number(proxyPort) || 8787,
    preferred_account_id: preferred.trim() || null,
  });

  const doSave = async () => {
    setBusy(true);
    setErr("");
    try {
      const s = snapshot();
      const takeoverChanged =
        s.proxy_enabled !== settings.proxy_enabled ||
        (s.proxy_enabled && s.proxy_port !== settings.proxy_port);
      if (takeoverChanged) {
        const action = !settings.proxy_enabled
          ? "开启接管"
          : !s.proxy_enabled
          ? "关闭接管"
          : "切换接管端口";
        const ok = await askConfirm({
          title: `${action}并重启 WorkBuddy`,
          body:
            `${action}需要重启 WorkBuddy 与长驻 CLI host，才能安全清除旧端点。` +
            "代理会在整个切换过程中保持可用，不会留下死端口。现在继续吗？（请先保存未提交的输入）",
          okText: action,
        });
        if (!ok) {
          setBusy(false);
          return;
        }
      }
      const saved = takeoverChanged
        ? await applySettings(s)
        : await saveSettings(s);
      onSettings(saved);
      onToast({
        kind: "ok",
        text: takeoverChanged ? "已应用，WorkBuddy 已安全重启" : "已保存",
      });
      await refreshStealth();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const pending =
    proxyOn !== settings.proxy_enabled ||
    Number(proxyPort) !== settings.proxy_port ||
    (preferred.trim() || null) !== (settings.preferred_account_id ?? null);

  return (
    <section className="panel-page">
      <div className="opt-col">
        <label>
          优先扣费账号
          <select value={preferred} onChange={(e) => setPreferred(e.target.value)}>
            <option value="">自动（最旧积分优先轮换）</option>
            {accounts.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </select>
        </label>
        <p className="hint">
          指定后，凡是走代理的会话（含老会话）下一个请求就坚决用它——优先级压过会话粘滞与智能轮换，便于定向测试扣费。仅在会话尚未走代理时（接管开启前的老对话，直连中）不受影响，重启 WorkBuddy 后才会进入代理。
        </p>
      </div>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={proxyOn}
          onChange={(e) => setProxyOn(e.target.checked)}
        />
        开启后 WorkBuddy 的对话自动按「积分最早过期」的账号分流
      </label>

      {proxyOn ? (
        <>
          <div className="opt-col">
            <label>
              监听端口
              <input
                type="number"
                value={proxyPort}
                min={1024}
                max={65535}
                onChange={(e) => setProxyPort(e.target.value)}
              />
            </label>
          </div>

          <p className="hint danger-hint">
            开启后会把 <code>~/.workbuddy/settings.json</code> 的
            <code>env.CODEBUDDY_BASE_URL</code> 指向本机
            <code>127.0.0.1:{proxyPort || 8787}</code>，WorkBuddy 的对话请求即自动分流。
            <strong>本应用退出、或反代停掉时会自动摘掉该配置</strong>；万一异常，
            用下面的「立即停止接管」或「网络急救 → 一键恢复」都能一步恢复。
            应用时会先确认；确认后由本应用安全重启 WorkBuddy 与长驻 CLI host，并原子完成端点切换。
          </p>

          <div className="opt-row">
            <button
              className="btn small ghost"
              disabled={stealthBusy}
              onClick={() => void refreshStealth()}
            >
              刷新状态
            </button>
            <button
              className="btn small danger"
              disabled={stealthBusy}
              onClick={() => void doStopStealth()}
            >
              立即停止接管
            </button>
          </div>

          {stealth && (
            <p
              className={`stealth-state ${
                stealth.installed && stealth.alive
                  ? "ok"
                  : stealth.installed
                  ? "bad"
                  : "idle"
              }`}
            >
              <span className="dot" />
              {stealth.installed && stealth.alive
                ? "接管生效中"
                : stealth.installed
                ? "心跳已停：请点「立即停止接管」"
                : "尚未装载（应用设置后几秒内生效）"}
              <span className="stealth-note">{stealth.note}</span>
            </p>
          )}

          {routes.length > 0 && (
            <div className="routes">
              <div className="routes-head">最近路由</div>
              <ul>
                {routes.slice(0, 6).map((r, i) => (
                  <li key={`${r.at}-${r.account}-${i}`}>
                    <span className="r-at">{r.at}</span>
                    <span className="r-acct">{r.account}</span>
                    <span className="r-path">
                      {r.path}
                      {r.stream && <em title="SSE 流式">流</em>}
                    </span>
                    <span className="r-conv">{r.conv}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </>
      ) : (
        <p className="hint">
          这是 WorkBuddy 专用通道：只监听本机（127.0.0.1）、无鉴权 Key，
          开启、关闭或换端口时，本应用会自动安全重启 WorkBuddy 与长驻 CLI host，无需手工处理。
        </p>
      )}

      {err && <p className="form-err">{err}</p>}
      <div className="page-actions">
        <button className="btn primary" disabled={busy || !pending} onClick={() => void doSave()}>
          {busy
            ? "应用中…"
            : proxyOn !== settings.proxy_enabled
            ? "应用并重启 WorkBuddy"
            : "保存"}
        </button>
      </div>
    </section>
  );
}
