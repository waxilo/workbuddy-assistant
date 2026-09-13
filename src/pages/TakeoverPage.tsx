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
 * 「无感接管」页：顶部一条紧凑控制条（小开关 + 状态 + 扣费账号摘要 + 端口 + 保存），
 * 下方「接管动态」铺满剩余空间（列表内部滚动，页面不出滚动条）。
 * 扣费账号默认全部勾选（billing 为空 = 全选，智能轮换），点摘要弹框细选。
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

  /** 刷新接管状态与事件流（15 秒自动轮询） */
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

  /** 扣费账号摘要（控制条上的那颗胶囊按钮） */
  const billingSummary =
    accounts.length === 0
      ? "暂无账号"
      : billing.length === 0
      ? `全部 ${accounts.length} 个（默认）`
      : `已选 ${billing.length} · 未选 ${accounts.length - billing.length}`;

  /** 状态副文案 */
  const stateText = live
    ? "接管生效中，对话正按备选账号扣费"
    : stealth?.installed
    ? "状态异常：关闭开关再保存即可恢复直连"
    : proxyOn
    ? "保存后生效：会安全重启 WorkBuddy"
    : "开启后对话自动按备选账号分流扣费";

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
      {/* ── 紧凑控制条：开关 + 状态 + 扣费账号 + 端口 + 保存 ── */}
      <div className={`tk-bar ${live ? "live" : ""}`}>
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
        <div className="tk-bar-text" title={stealth?.note}>
          <strong>{proxyOn ? "接管已开启" : "接管已关闭"}</strong>
          <span className={`tk-bar-sub ${live ? "ok" : ""}`}>{stateText}</span>
        </div>
        <span className="spacer" />
        <button
          className="tk-accts"
          title="勾选的账号才允许被扣费，未勾选的会被排除；点击细选"
          onClick={() => setPickerOpen(true)}
        >
          <span className="ta-label">扣费账号</span>
          <span className="ta-value">{billingSummary}</span>
          <span className="ta-edit">选择</span>
        </button>
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
          className="btn primary small"
          disabled={busy || !pending}
          onClick={() => void doSave()}
        >
          {busy
            ? "应用中…"
            : proxyOn !== settings.proxy_enabled
            ? "应用并重启"
            : "保存"}
        </button>
      </div>

      {err && <p className="form-err">{err}</p>}

      {/* ── 接管动态：铺满剩余空间，列表内部滚动 ── */}
      <div className="tk-section tk-feed">
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
              勾选的账号才允许被扣费（会话粘滞 + 积分最早过期优先轮换），未勾选的账号会被排除；
              默认全部勾选（智能轮换）。万一接管异常，把开关关闭再保存，或走「网络急救 → 一键恢复」。
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
