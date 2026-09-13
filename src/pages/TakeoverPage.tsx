import { useCallback, useEffect, useMemo, useState } from "react";
import type { Account, FreeModelsReport, JournalEvent, Settings, StealthStatus } from "../types";
import {
  applySettings,
  takeoverEvents,
  clearTakeoverEvents,
  saveSettings,
  stealthStatus,
  freeModels,
} from "../api";
import { maskPhone } from "../common";
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
    case "failover":
      return { label: "限流切换", cls: "route" };
    case "restart_workbuddy":
      return { label: "重启 WorkBuddy", cls: "restart" };
    case "proxy_upstream_error":
    case "proxy_stream_error":
      return { label: "代理错误", cls: "err" };
    default:
      return { label: "事件", cls: "restart" };
  }
}

/** 全选归一：列表覆盖全部账号时存空（= 默认全选，新增账号自动可扣费） */
function normalizeBilling(list: string[], allIds: string[]): string[] {
  return allIds.length > 0 && list.length === allIds.length ? [] : list;
}

/**
 * 「无感接管」页：顶部一条紧凑控制条（小开关 + 状态 + 扣费账号摘要 + 端口），
 * 下方「接管动态」铺满剩余空间（列表内部滚动、滚动条隐藏）。
 * 没有保存按钮：开关拨动立即应用；端口仅在关闭时可改（失焦即存）；
 * 扣费账号在弹框里点「保存」立即生效。
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
  // 弹框草稿：打开时复制当前生效值，点「保存」才落库生效
  const [draft, setDraft] = useState<string[] | null>(null);
  const [query, setQuery] = useState("");
  const [stealth, setStealth] = useState<StealthStatus | null>(null);
  const [events, setEvents] = useState<JournalEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  // 限流切换说明弹窗：展示支持无感切换的免费模型，支持手动刷新
  const [fmOpen, setFmOpen] = useState(false);
  const [fm, setFm] = useState<FreeModelsReport | null>(null);
  const [fmBusy, setFmBusy] = useState(false);

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

  /** 拉取限流切换支持的模型列表（refresh=true 忽略缓存强制重拉） */
  const loadFreeModels = useCallback(
    async (refresh: boolean) => {
      setFmBusy(true);
      try {
        setFm(await freeModels(refresh));
      } catch (e) {
        onToast({ kind: "err", text: "拉取模型列表失败：" + String(e) });
      } finally {
        setFmBusy(false);
      }
    },
    [onToast]
  );

  /** 组装一份以当前界面状态为准的设置 */
  const snapshot = (over?: Partial<Settings>): Settings => ({    ...settings,
    proxy_enabled: proxyOn,
    proxy_port: Number(proxyPort) || 8787,
    billing_account_ids: billing,
    ...over,
  });

  /** 开关即拨即用：确认后立即应用（开启/关闭都会安全重启 WorkBuddy） */
  const doToggle = async (next: boolean) => {
    const action = next ? "开启接管" : "关闭接管";
    const ok = await askConfirm({
      title: `${action}并重启 WorkBuddy`,
      body:
        `${action}需要重启 WorkBuddy 与长驻 CLI host，才能安全清除旧端点。` +
        "代理会在整个切换过程中保持可用，不会留下死端口。现在继续吗？（请先保存未提交的输入）",
      okText: action,
    });
    if (!ok) return; // 取消：开关状态不动
    setBusy(true);
    setErr("");
    try {
      const saved = await applySettings(snapshot({ proxy_enabled: next }));
      onSettings(saved);
      setProxyOn(next);
      onToast({ kind: "ok", text: `已${action}，WorkBuddy 已安全重启` });
      await refreshStealth();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  /** 端口只在接管关闭时可改；失焦时若变了就立即落盘（纯配置，无需重启） */
  const onPortBlur = async () => {
    const port = Number(proxyPort) || 8787;
    if (proxyOn || port === settings.proxy_port) return;
    try {
      const saved = await saveSettings(snapshot({ proxy_port: port }));
      onSettings(saved);
      onToast({ kind: "ok", text: "端口已保存" });
    } catch (e) {
      onToast({ kind: "err", text: "端口保存失败：" + String(e) });
    }
  };

  /** 弹框「保存」：草稿落库立即生效（纯账号池调整，不需要重启） */
  const doSaveBilling = async () => {
    if (draft == null) return;
    const next = normalizeBilling(draft, allIds);
    setBusy(true);
    setErr("");
    try {
      const saved = await saveSettings(
        snapshot({ billing_account_ids: next })
      );
      onSettings(saved);
      setBilling(next);
      setDraft(null);
      setPickerOpen(false);
      onToast({ kind: "ok", text: "扣费账号已生效" });
    } catch (e) {
      onToast({ kind: "err", text: "保存失败：" + String(e) });
    } finally {
      setBusy(false);
    }
  };

  /** 弹框草稿里勾/去勾（基准：草稿为空视为「当前全选」） */
  const toggleDraft = (id: string) =>
    setDraft((list) => {
      const base = list == null ? effective : list.length === 0 ? allIds : list;
      return base.includes(id)
        ? base.filter((x) => x !== id)
        : [...base, id];
    });

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
    ? "状态异常：关闭开关即可恢复直连"
    : proxyOn
    ? "应用中：会安全重启 WorkBuddy"
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

  /** 弹框内按用户名 / 手机号过滤 */
  const filteredAccounts = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return accounts;
    return accounts.filter(
      (a) =>
        a.name.toLowerCase().includes(q) || (a.phone ?? "").includes(q)
    );
  }, [accounts, query]);

  const draftEffective =
    draft == null ? effective : draft.length === 0 ? allIds : draft;

  return (
    <section className="panel-page tk-page">
      {/* ── 紧凑控制条：开关 + 状态 + 扣费账号 + 端口 ── */}
      <div className={`tk-bar ${live ? "live" : ""}`}>
        <label
          className="switch"
          title="开启后 WorkBuddy 的对话请求将由本地代理分流扣费"
        >
          <input
            type="checkbox"
            checked={proxyOn}
            disabled={busy}
            onChange={(e) => void doToggle(e.target.checked)}
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
          onClick={() => {
            setDraft(billing);
            setQuery("");
            setPickerOpen(true);
          }}
        >
          <span className="ta-label">扣费账号</span>
          <span className="ta-value">{billingSummary}</span>
          <span className="ta-edit">选择</span>
        </button>
        <label
          className="tk-port"
          title={proxyOn ? "接管开启期间不允许修改端口；请先关闭接管" : "代理监听端口"}
        >
          端口
          <input
            type="number"
            value={proxyPort}
            min={1024}
            max={65535}
            disabled={proxyOn || busy}
            onChange={(e) => setProxyPort(e.target.value)}
            onBlur={() => void onPortBlur()}
          />
        </label>
      </div>

      {err && <p className="form-err">{err}</p>}

      {/* ── 接管动态：铺满剩余空间，列表内部滚动（滚动条隐藏） ── */}
      <div className="tk-section tk-feed">
        <div className="tk-sec-head">
          <h3>接管动态</h3>
          <span className="tk-sec-meta">开启 / 关闭 / 每个会话开始用哪个账号 / 异常</span>
          <span className="spacer" />
          <button
            className="btn small ghost"
            title="查看哪些模型支持限流无感切换（可刷新拉取最新）"
            onClick={() => {
              setFmOpen(true);
              if (!fm) void loadFreeModels(false);
            }}
          >
            限流切换说明
          </button>
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

      {/* ── 扣费账号选择弹框（草稿制：点「保存」才生效） ── */}
      {pickerOpen && (
        <div
          className="modal-mask"
          onClick={() => {
            setDraft(null);
            setPickerOpen(false);
          }}
        >
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>选择扣费账号</h2>
            <p className="hint">
              勾选的账号才允许被扣费（会话粘滞 + 积分最早过期优先轮换），未勾选的账号会被排除；
              默认全部勾选（智能轮换）。点「保存」立即生效，无需重启。
            </p>
            <input
              type="text"
              className="acct-filter"
              placeholder="按用户名或手机号过滤…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            {accounts.length === 0 ? (
              <p className="hint">还没有账号。先到「账号签到」页登录或导入账号。</p>
            ) : filteredAccounts.length === 0 ? (
              <p className="hint">没有匹配「{query}」的账号。</p>
            ) : (
              <ul className="acct-multi">
                {filteredAccounts.map((a) => (
                  <li
                    key={a.id}
                    className={draftEffective.includes(a.id) ? "picked" : ""}
                    onClick={() => toggleDraft(a.id)}
                  >
                    <input
                      type="checkbox"
                      checked={draftEffective.includes(a.id)}
                      onChange={() => toggleDraft(a.id)}
                      onClick={(e) => e.stopPropagation()}
                    />
                    <span className="am-name">{maskPhone(a.name)}</span>
                    {a.phone && <span className="am-phone">{maskPhone(a.phone)}</span>}
                    <span className="am-state">
                      {draftEffective.includes(a.id) ? "可扣费" : "已排除"}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            <div className="modal-actions">
              <button
                className="btn"
                onClick={() => setDraft(allIds)}
                disabled={accounts.length === 0}
              >
                全部勾选
              </button>
              <span className="spacer" />
              <button
                className="btn"
                onClick={() => {
                  setDraft(null);
                  setPickerOpen(false);
                }}
              >
                取消
              </button>
              <button
                className="btn primary"
                disabled={busy || draft == null}
                onClick={() => void doSaveBilling()}
              >
                {busy ? "保存中…" : "保存"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* ── 限流切换说明弹框：免费模型列表 + 手动刷新 ── */}
      {fmOpen && (
        <div className="modal-mask" onClick={() => setFmOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h2>限流切换说明</h2>
            <p className="hint">
              免费模型（积分倍率 x0.00）触发限流（429）时，代理会把该账号冷却 10 分钟、
              自动换备用账号重发同一请求，对话完全无感；付费模型的限流会原样透传。
              以下列表从网关动态拉取（缓存 1 小时），腾讯增删免费模型后点「刷新」即可同步。
            </p>
            {fm == null ? (
              <p className="hint">加载中…</p>
            ) : fm.models.length === 0 ? (
              <p className="hint">暂未发现免费模型。</p>
            ) : (
              <ul className="fm-chips">
                {fm.models.map((m) => (
                  <li key={m} className="fm-chip">
                    {m}
                  </li>
                ))}
              </ul>
            )}
            {fm && (
              <p className="hint fm-source">
                {fm.source === "fetched"
                  ? "来源：刚从网关拉取"
                  : fm.source === "cache"
                  ? "来源：缓存（1 小时内有效）"
                  : "来源：内置兜底列表（网关拉取失败，可点「刷新」重试）"}
              </p>
            )}
            <div className="modal-actions">
              <button
                className="btn"
                disabled={fmBusy}
                onClick={() => void loadFreeModels(true)}
              >
                {fmBusy ? "刷新中…" : "刷新"}
              </button>
              <span className="spacer" />
              <button className="btn primary" onClick={() => setFmOpen(false)}>
                关闭
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
