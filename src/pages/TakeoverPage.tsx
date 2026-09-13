import { useCallback, useEffect, useMemo, useState } from "react";
import type { Account, JournalEvent, Settings, StealthStatus } from "../types";
import {
  applySettings,
  takeoverEvents,
  clearTakeoverEvents,
  saveSettings,
  stealthStatus,
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
 * 「无感接管」页（自上而下单列）：
 * 1. 大开关控制接管（开 = 监听本机 + 改 WorkBuddy 端点，会安全重启）；
 * 2. 扣费备选账号：外面只显示「勾选了几个 / 未勾选几个」，点「选择账号」弹框细选；
 *    默认全部勾选（billing 为空 = 全部可用，智能轮换）。
 * 3. 接管动态时间线 —— 开启 / 关闭 / 每个会话开始用哪个账号 / 错误。
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
  /** 保存后把最新 settings 同步回外层（后端可能已代为改写字段） */
  onSettings: (s: Settings) => void;
  onToast: (t: Toast) => void;
}) {
  const [proxyOn, setProxyOn] = useState(settings.proxy_enabled);
  const [proxyPort, setProxyPort] = useState(String(settings.proxy_port || 8787));
  // 扣费备选池：空 = 默认全部勾选（智能轮换）；非空 = 只有勾选的账号允许扣费
  const [billing, setBilling] = useState<string[]>(settings.billing_account_ids);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [stealth, setStealth] = useState<StealthStatus | null>(null);
  const [events, setEvents] = useState<JournalEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const allIds = useMemo(() => accounts.map((a) => a.id), [accounts]);
  /** 实际生效的勾选集：未指定时视为全选 */
  const effective = billing.length === 0 ? allIds : billing;

  /** 刷新接管状态与事件流（15 秒自动轮询，无需手动刷新按钮） */
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

  /** 清空接管动态（不可恢复），清完刷新本地列表 */
  const doClearEvents = async () => {
    const ok = await askConfirm({
      title: "清空接管动态",
      body: "将删除全部接管事件记录（开启/关闭/账号启用/错误），此操作不可恢复。继续？",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearTakeoverEvents();
      setEvents([]);
      onToast({ kind: "ok", text: "接管动态已清空" });
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
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

  /**
   * 弹框里勾/去勾。基准：billing 为空视为「当前全选」，
   * 勾回全满时归一为空列表（= 默认全选，新增账号也自动可扣费）。
   */
  const toggleBilling = (id: string) =>
    setBilling((list) => {
      const base = list.length === 0 ? allIds : list;
      const next = base.includes(id)
        ? base.filter((x) => x !== id)
        : [...base, id];
      return next.length === allIds.length && allIds.length > 0 ? [] : next;
    });

  const pending =
    proxyOn !== settings.proxy_enabled ||
    Number(proxyPort) !== settings.proxy_port ||
    JSON.stringify(billing) !== JSON.stringify(settings.billing_account_ids);

  const live = stealth?.installed && stealth.alive;

  /**
   * 连续相同（类型 + 内容都一样）的事件聚合为一条，附重复次数。
   * 事件流是「新的在前」，相邻即时间连续——重启风暴、心跳重复这类刷屏只会占一行。
   */
  const groupedEvents = useMemo(() => {
    const out: { e: JournalEvent; count: number }[] = [];
    for (const e of events) {
      const last = out[out.length - 1];
      if (last && last.e.event === e.event && last.e.detail === e.detail) {
        last.count += 1;
      } else {
        out.push({ e, count: 1 });
      }
    }
    return out.slice(0, 80);
  }, [events]);

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
            ? "状态异常：把开关关一下再保存，即可恢复直连"
            : "尚未装载（应用设置后几秒内生效）"}
          <span className="stealth-note">{stealth.note}</span>
        </p>
      )}

      {/* ── 扣费备选账号（外部只显示摘要，点开弹框细选） ── */}
      <div className="tk-section">
        <div className="tk-sec-head">
          <h3>扣费备选账号</h3>
          <span className="tk-sec-meta">
            {billing.length === 0
              ? `已勾选全部 ${accounts.length} 个（默认，智能轮换）`
              : accounts.length - billing.length === 0
              ? `已勾选全部 ${accounts.length} 个`
              : `已勾选 ${billing.length} 个 · 未勾选 ${
                  accounts.length - billing.length
                } 个（不允许扣费）`}
          </span>
          <span className="spacer" />
          <button className="btn small" onClick={() => setPickerOpen(true)}>
            选择账号
          </button>
        </div>
        <p className="hint">
          只有勾选的账号会被反代用于扣费（会话粘滞 + 积分最早过期优先轮换），未勾选的账号会被排除。
          端点写入 <code>~/.workbuddy/settings.json</code> 的
          <code>env.CODEBUDDY_BASE_URL</code>，应用退出或反代停止时会自动摘掉；万一异常，
          把开关关闭再保存，或走「网络急救 → 一键恢复」，都能一步恢复。
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

      {/* ── 接管动态（事件时间线，内部滚动） ── */}
      <div className="tk-section">
        <div className="tk-sec-head">
          <h3>接管动态</h3>
          <span className="tk-sec-meta">开启 / 关闭 / 每个会话开始用哪个账号 / 异常</span>
          <span className="spacer" />
          <button
            className="btn small ghost"
            disabled={events.length === 0}
            title="删除全部接管事件记录，不可恢复"
            onClick={() => void doClearEvents()}
          >
            清空
          </button>
        </div>
        {events.length === 0 ? (
          <p className="hint">
            暂无事件。开启接管并产生对话后，这里会记录每一次账号启用与开关动作。
          </p>
        ) : (
          <ul className="evt-list">
            {groupedEvents.map(({ e, count }, i) => {
              const k = eventKind(e);
              return (
                <li
                  key={`${e.at_ms}-${i}`}
                  className={`evt evt-${k.cls}`}
                  title={count > 1 ? `相同事件连续出现 ${count} 次` : undefined}
                >
                  <span className="e-at">{e.at}</span>
                  <span className={`e-tag tag-${k.cls}`}>{k.label}</span>
                  {count > 1 && <span className="e-count">×{count}</span>}
                  <span className="e-detail">{e.detail}</span>
                </li>
              );
            })}
          </ul>
        )}
      </div>

      {/* ── 扣费账号选择弹框 ── */}
      {pickerOpen && (
        <div className="modal-mask" onClick={() => setPickerOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>选择扣费账号</h2>
            <p className="hint">
              勾选的账号才允许被扣费，未勾选的账号会被排除；默认全部勾选（智能轮换）。
            </p>
            {accounts.length === 0 ? (
              <p className="hint">还没有账号。先到「账号签到」页登录或导入账号。</p>
            ) : (
              <ul className="acct-multi">
                {accounts.map((a) => (
                  <li
                    key={a.id}
                    className={effective.includes(a.id) ? "picked" : ""}
                    onClick={() => toggleBilling(a.id)}
                  >
                    <input
                      type="checkbox"
                      checked={effective.includes(a.id)}
                      onChange={() => toggleBilling(a.id)}
                      onClick={(e) => e.stopPropagation()}
                    />
                    <span className="am-name">{a.name}</span>
                    {a.phone && <span className="am-phone">{a.phone}</span>}
                    <span className="am-state">
                      {effective.includes(a.id) ? "可扣费" : "已排除"}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            <div className="modal-actions">
              <button className="btn" onClick={() => setBilling([])}>
                全部勾选
              </button>
              <span className="spacer" />
              <button className="btn primary" onClick={() => setPickerOpen(false)}>
                完成
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
