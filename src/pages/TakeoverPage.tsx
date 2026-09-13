import { useCallback, useEffect, useState } from "react";
import type { Account, JournalEvent, Settings, StealthStatus } from "../types";
import {
  applySettings,
  getSettings,
  takeoverEvents,
  saveSettings,
  stealthStatus,
  stealthStop,
} from "../api";
import type { ConfirmReq, Toast } from "../common";

/** 事件类型 → 界面标签与配色 */
function eventKind(e: JournalEvent): {
  label: string;
  cls: "on" | "off" | "route" | "restart" | "err";
} {
  switch (e.event) {
    case "install":
      return { label: "开启接管", cls: "on" };
    case "uninstall":
      return { label: "关闭接管", cls: "off" };
    case "route_start":
      return { label: "开始使用账号", cls: "route" };
    case "restart_workbuddy":
      return { label: "重启 WorkBuddy", cls: "restart" };
    case "proxy_upstream_error":
    case "proxy_stream_error":
      return { label: "代理错误", cls: "err" };
    default:
      return { label: "事件", cls: "restart" };
  }
}

/**
 * 「无感接管」页。
 *
 * 上：一个大开关控制接管（开 = 监听本机 + 改 WorkBuddy 端点，会安全重启）；
 * 中：扣费备选账号多选 —— 没被勾选的账号不允许扣费，全不勾 = 全部可用；
 * 下：接管动态时间线 —— 开启 / 关闭 / 每个会话开始用哪个账号 / 错误。
 */
export function TakeoverPage({
  settings,
  accounts,
  askConfirm,
  onSettings,
  onToast,
}: {
  settings: Settings;
  accounts: Account[];
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 保存 / 停止接管后把最新 settings 同步回外层（后端可能已代为改写字段） */
  onSettings: (s: Settings) => void;
  onToast: (t: Toast) => void;
}) {
  const [proxyOn, setProxyOn] = useState(settings.proxy_enabled);
  const [proxyPort, setProxyPort] = useState(String(settings.proxy_port || 8787));
  // 扣费备选池：勾了谁，谁才有资格被扣费；空 = 全部可用（智能轮换）
  const [billing, setBilling] = useState<string[]>(settings.billing_account_ids);
  const [stealth, setStealth] = useState<StealthStatus | null>(null);
  const [events, setEvents] = useState<JournalEvent[]>([]);
  const [stealthBusy, setStealthBusy] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  /** 刷新接管状态与事件流。接管是后台线程异步装卸的，所以要给用户一个「刷新」。 */
  const refreshStealth = useCallback(async () => {
    try {
      const [s, ev] = await Promise.all([stealthStatus(), takeoverEvents()]);
      setStealth(s);
      setEvents(ev);
    } catch (e) {
      onToast({ kind: "err", text: "读取接管状态失败：" + String(e) });
    }
  }, [onToast]);

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
    billing_account_ids: billing,
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

  const toggleBilling = (id: string) =>
    setBilling((list) =>
      list.includes(id) ? list.filter((x) => x !== id) : [...list, id]
    );

  const pending =
    proxyOn !== settings.proxy_enabled ||
    Number(proxyPort) !== settings.proxy_port ||
    JSON.stringify(billing) !== JSON.stringify(settings.billing_account_ids);

  const live = stealth?.installed && stealth.alive;

  return (
    <section className="panel-page tk-page">
      {/* ── 接管开关 ── */}
      <div className={`tk-hero ${live ? "live" : ""}`}>
        <div className="tk-hero-main">
          <label className="switch" title="开启后 WorkBuddy 的对话请求将由本地代理分流扣费">
            <input
              type="checkbox"
              checked={proxyOn}
              onChange={(e) => setProxyOn(e.target.checked)}
            />
            <span className="track">
              <span className="thumb" />
            </span>
          </label>
          <div className="tk-hero-text">
            <strong>{proxyOn ? "接管已开启" : "接管已关闭"}</strong>
            <span className="tk-hero-sub">
              {live
                ? "WorkBuddy 对话正经过本地代理按备选账号扣费"
                : proxyOn
                ? "保存后生效：会安全重启 WorkBuddy 并写入接管端点"
                : "开启后 WorkBuddy 的对话自动按备选账号分流扣费"}
            </span>
          </div>
        </div>
        <div className="tk-hero-side">
          <label className="tk-port">
            端口
            <input
              type="number"
              value={proxyPort}
              min={1024}
              max={65535}
              onChange={(e) => setProxyPort(e.target.value)}
            />
          </label>
          <button
            className="btn small ghost"
            disabled={stealthBusy}
            onClick={() => void refreshStealth()}
          >
            刷新状态
          </button>
          <button
            className="btn small danger"
            disabled={stealthBusy || !settings.proxy_enabled}
            title="立即摘除接管端点并恢复直连（不用走保存）"
            onClick={() => void doStopStealth()}
          >
            立即停止接管
          </button>
        </div>
      </div>

      {stealth && (
        <p
          className={`stealth-state ${
            live ? "ok" : stealth.installed ? "bad" : "idle"
          }`}
        >
          <span className="dot" />
          {live
            ? "接管生效中"
            : stealth.installed
            ? "心跳已停：请点「立即停止接管」"
            : "尚未装载（应用设置后几秒内生效）"}
          <span className="stealth-note">{stealth.note}</span>
        </p>
      )}

      {/* ── 扣费备选账号（多选） ── */}
      <div className="tk-section">
        <div className="tk-sec-head">
          <h3>扣费备选账号</h3>
          <span className="tk-sec-meta">
            {billing.length === 0
              ? `未勾选：全部 ${accounts.length} 个账号都可扣费（智能轮换）`
              : `已选 ${billing.length} 个，未选中的账号不允许扣费`}
          </span>
          <span className="spacer" />
          <button
            className="btn small ghost"
            onClick={() => setBilling(accounts.map((a) => a.id))}
            disabled={accounts.length === 0}
          >
            全选
          </button>
          <button
            className="btn small ghost"
            onClick={() => setBilling([])}
            disabled={billing.length === 0}
          >
            清空
          </button>
        </div>
        {accounts.length === 0 ? (
          <p className="hint">还没有账号。先到「账号签到」页登录或导入账号。</p>
        ) : (
          <ul className="acct-multi">
            {accounts.map((a) => (
              <li
                key={a.id}
                className={billing.includes(a.id) ? "picked" : ""}
                onClick={() => toggleBilling(a.id)}
              >
                <input
                  type="checkbox"
                  checked={billing.includes(a.id)}
                  onChange={() => toggleBilling(a.id)}
                  onClick={(e) => e.stopPropagation()}
                />
                <span className="am-name">{a.name}</span>
                {a.phone && <span className="am-phone">{a.phone}</span>}
                <span className="am-state">
                  {billing.length === 0 || billing.includes(a.id)
                    ? "可扣费"
                    : "已排除"}
                </span>
              </li>
            ))}
          </ul>
        )}
        <p className="hint">
          反代只在勾选的账号里选号（会话粘滞 + 积分最早过期优先轮换）；把不想消耗的账号留在未选中状态即可。
          端点写入 <code>~/.workbuddy/settings.json</code> 的
          <code>env.CODEBUDDY_BASE_URL</code>，应用退出或反代停止时会自动摘掉；万一异常，
          「立即停止接管」或「网络急救 → 一键恢复」都能一步恢复。
        </p>
      </div>

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

      {/* ── 接管动态（事件时间线） ── */}
      <div className="tk-section">
        <div className="tk-sec-head">
          <h3>接管动态</h3>
          <span className="tk-sec-meta">开启 / 关闭 / 每个会话开始用哪个账号 / 异常</span>
        </div>
        {events.length === 0 ? (
          <p className="hint">
            暂无事件。开启接管并产生对话后，这里会记录每一次账号启用与开关动作。
          </p>
        ) : (
          <ul className="evt-list">
            {events.slice(0, 50).map((e, i) => {
              const k = eventKind(e);
              return (
                <li key={`${e.at_ms}-${i}`} className={`evt evt-${k.cls}`}>
                  <span className="e-at">{e.at}</span>
                  <span className={`e-tag tag-${k.cls}`}>{k.label}</span>
                  <span className="e-detail">{e.detail}</span>
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </section>
  );
}
