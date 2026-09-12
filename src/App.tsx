import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import type {
  Account,
  Settings,
  LocalAccount,
  OAuthPoll,
  ImportItem,
  ImportReport,
  CheckinLog,
  NetReport,
  NetStep,
  StealthStatus,
  RouteLog,
} from "./types";
import {
  listAccounts,
  importAccounts,
  removeAccount,
  checkinOne,
  checkinAll,
  refreshCredits,
  discoverLocalAccounts,
  oauthStart,
  oauthPoll,
  openExternal,
  getSettings,
  saveSettings,
  applySettings,
  testNotify,
  getAutostart,
  setAutostart,
  getCheckinLogs,
  clearCheckinLogs,
  appVersion,
  netDiagnose,
  netRestore,
  revealPath,
  stealthStatus,
  stealthStop,
  proxyRoutes,
} from "./api";
import { checkAndInstall, type UpdateProgress } from "./updater";

type Toast = { kind: "ok" | "err" | "info"; text: string } | null;

/**
 * 弹窗类型。
 *
 * 账号**没有**「添加 / 编辑」入口：条目只是给用户看的，凭证一律来自
 * 「登录新账号」（OAuth）或「导入本机账号」（本机登录文件），避免手工粘贴 token 出错。
 */
type Modal =
  | { type: "local" }
  | { type: "oauth" }
  | { type: "settings" }
  | { type: "takeover" }
  | { type: "logs"; accountId?: string }
  | null;

/** 自研确认框的请求描述（resolve 由 App 统一收口） */
type ConfirmReq = {
  title: string;
  body?: string;
  okText?: string;
  danger?: boolean;
  resolve: (ok: boolean) => void;
};

function maskToken(t: string): string {
  if (t.length <= 12) return "•".repeat(t.length);
  return t.slice(0, 6) + "…" + t.slice(-4);
}

/** 路径取文件名（兼容 Windows 反斜杠） */
function baseName(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

/** 剩余积分展示：最多两位小数且不留尾随 0（接口给的是 805.14000097 这种精度） */
function formatCredits(v?: number | null): string {
  if (v == null) return "—";
  return String(Math.round(v * 100) / 100);
}

/**
 * 账号最近一次签到结果的徽标。
 *
 * 注意顺序：服务端对「今天已签到」返回 HTTP 400 + `code=10001`，
 * 此时 `success` 也是 true（幂等成功），所以必须**先判 already**，
 * 否则「今日已签」永远显示成「成功」。
 */
function ResultBadge({ last }: { last: Account["last"] }) {
  if (!last) return <span className="badge badge-idle">未签到</span>;
  if (last.already) return <span className="badge badge-already">今日已签</span>;
  if (last.success) return <span className="badge badge-ok">成功</span>;
  if (last.inactive) return <span className="badge badge-idle">活动未开</span>;
  return <span className="badge badge-err">失败</span>;
}

export default function App() {
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [version, setVersion] = useState("");
  const [loading, setLoading] = useState(true);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [busyAll, setBusyAll] = useState(false);
  const [busyRefresh, setBusyRefresh] = useState(false);
  const [modal, setModal] = useState<Modal>(null);
  const [toast, setToast] = useState<Toast>(null);
  const [update, setUpdate] = useState<UpdateProgress | null>(null);
  const [confirmReq, setConfirmReq] = useState<ConfirmReq | null>(null);

  // 自研确认框：不使用 window.confirm —— Tauri 的 WKWebView 未实现原生 confirm 面板，
  // 调用会静默返回 false，导致删除/清空这类操作永远不执行。
  const askConfirm = useCallback(
    (opts: Omit<ConfirmReq, "resolve">) =>
      new Promise<boolean>((resolve) => setConfirmReq({ ...opts, resolve })),
    []
  );
  const resolveConfirm = useCallback(
    (ok: boolean) => {
      confirmReq?.resolve(ok);
      setConfirmReq(null);
    },
    [confirmReq]
  );

  const showToast = useCallback((t: Toast) => {
    setToast(t);
    if (t) window.setTimeout(() => setToast(null), 3200);
  }, []);

  const load = useCallback(async () => {
    try {
      const [acc, set, ver] = await Promise.all([
        listAccounts(),
        getSettings(),
        appVersion(),
      ]);
      setAccounts(acc);
      setSettings(set);
      setVersion(ver);
    } catch (e) {
      showToast({ kind: "err", text: "加载失败：" + String(e) });
    } finally {
      setLoading(false);
    }
  }, [showToast]);

  useEffect(() => {
    load();
  }, [load]);

  // 启动时若开启“自动签到”，则对全部账号执行一次
  useEffect(() => {
    if (!loading && settings?.auto_checkin_on_start && accounts.length > 0) {
      void runCheckinAll();
      // 仅在首次装配完成后触发一次
      // eslint-disable-next-line react-hooks/exhaustive-deps
      setSettings((s) => (s ? { ...s, auto_checkin_on_start: false } : s));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loading]);

  // 定时签到由后端调度线程触发（应用需保持运行），完成后刷新列表并提示
  useEffect(() => {
    const un = listen<{ stage: string; count?: number; message?: string }>(
      "checkin-scheduled",
      (e) => {
        const p = e.payload;
        if (p.stage === "done") {
          void load();
          showToast({
            kind: "info",
            text: `定时签到完成（${p.count ?? 0} 个账号）`,
          });
        } else if (p.stage === "error") {
          showToast({ kind: "err", text: "定时签到异常：" + (p.message ?? "") });
        }
      }
    );
    return () => {
      void un.then((f) => f());
    };
  }, [load, showToast]);

  // 自动续签由后端调度线程完成（有效期不足 48h 触发），完成后刷新列表
  useEffect(() => {
    const un = listen<{ count?: number }>("auto-refreshed", (e) => {
      void load();
      showToast({
        kind: "info",
        text: `已自动续签 ${e.payload.count ?? 0} 个账号的凭证`,
      });
    });
    return () => {
      void un.then((f) => f());
    };
  }, [load, showToast]);

  const runCheckinOne = useCallback(
    async (id: string) => {
      setBusyIds((s) => new Set(s).add(id));
      try {
        const updated = await checkinOne(id);
        setAccounts((list) => list.map((a) => (a.id === id ? updated : a)));
        if (updated.last?.already) {
          showToast({ kind: "info", text: `${updated.name} 今日已签` });
        } else if (updated.last?.success) {
          showToast({ kind: "ok", text: `${updated.name} 签到成功` });
        } else if (updated.last?.inactive) {
          showToast({ kind: "info", text: `${updated.name} 活动未开启` });
        } else {
          showToast({
            kind: "err",
            text: `${updated.name} 签到失败：${updated.last?.message ?? ""}`,
          });
        }
      } catch (e) {
        showToast({ kind: "err", text: "签到异常：" + String(e) });
      } finally {
        setBusyIds((s) => {
          const n = new Set(s);
          n.delete(id);
          return n;
        });
      }
    },
    [showToast]
  );

  const runCheckinAll = useCallback(async () => {
    setBusyAll(true);
    try {
      const updated = await checkinAll();
      setAccounts(updated);
      const { ok, already, fail } = tally(
        updated.map((a) => a.last).filter((r): r is NonNullable<typeof r> => r != null)
      );
      showToast({
        kind: fail > 0 ? "err" : "ok",
        text: `全部完成：成功 ${ok} / 已签 ${already} / 失败 ${fail}`,
      });
    } catch (e) {
      showToast({ kind: "err", text: "批量签到异常：" + String(e) });
    } finally {
      setBusyAll(false);
    }
  }, [showToast]);

  // 一键刷新：只查积分、不触发签到（已签账号再打签到接口只会拿到 400），
  // 后端返回的就是最新账号列表，「刷新积分」与「刷新数据」一次完成。
  const runRefresh = useCallback(async () => {
    setBusyRefresh(true);
    try {
      const updated = await refreshCredits();
      setAccounts(updated);
      const got = updated.filter((a) => a.last?.balance != null).length;
      showToast({
        kind: "ok",
        text: `已刷新 ${updated.length} 个账号（${got} 个取到积分）`,
      });
    } catch (e) {
      showToast({ kind: "err", text: "刷新失败：" + String(e) });
    } finally {
      setBusyRefresh(false);
    }
  }, [showToast]);

  const removeOne = useCallback(
    async (a: Account) => {
      const ok = await askConfirm({
        title: "删除账号",
        body: `确认删除「${accountLabel(a.name, a.phone)}」？该账号的签到日志也会一并删除。`,
        okText: "删除",
        danger: true,
      });
      if (!ok) return;
      try {
        await removeAccount(a.id);
        setAccounts((l) => l.filter((x) => x.id !== a.id));
        showToast({ kind: "ok", text: `已删除 ${a.name}` });
      } catch (e) {
        showToast({ kind: "err", text: "删除失败：" + String(e) });
      }
    },
    [askConfirm, showToast]
  );

  // 批量导入：后端按「手机号或 token」识别已有账号并合并补全凭证，不会产生重复条目
  const importItems = useCallback(
    async (items: ImportItem[]): Promise<ImportReport> => {
      const report = await importAccounts(items);
      if (report.added > 0 || report.updated > 0) await load();
      return report;
    },
    [load]
  );

  const onUpdate = useCallback(async () => {
    await checkAndInstall((p) => {
      setUpdate(p);
      if (p.status === "error" || p.status === "no-update") {
        showToast({ kind: p.status === "error" ? "err" : "info", text: p.message });
        window.setTimeout(() => setUpdate(null), 2600);
      }
    });
  }, [showToast]);

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="logo">✓</span>
          <div>
            <h1>WorkBuddy 助手</h1>
            <p className="sub">多账号签到 · 自动更新 · v{version}</p>
          </div>
        </div>
        <div className="top-actions">
          <button className="btn ghost" onClick={onUpdate}>
            检查更新
          </button>
        </div>
      </header>

      <div className="toolbar">
        <button
          className="btn primary"
          disabled={busyAll || accounts.length === 0}
          onClick={runCheckinAll}
        >
          {busyAll ? "签到中…" : "全部签到"}
        </button>
        <button
          className="btn ghost"
          disabled={busyRefresh || accounts.length === 0}
          title="查询全部账号的最新剩余积分，不触发签到"
          onClick={runRefresh}
        >
          {busyRefresh ? "刷新中…" : "刷新"}
        </button>
        {/* 账号只能通过下面两条通道产生，不提供手工录入入口 */}
        <button
          className="btn ghost"
          onClick={() => setModal({ type: "oauth" })}
        >
          登录新账号
        </button>
        <button className="btn ghost" onClick={() => setModal({ type: "local" })}>
          导入本机账号
        </button>
        <button
          className="btn ghost"
          onClick={() => setModal({ type: "takeover" })}
        >
          无感接管
        </button>
        <button className="btn ghost" onClick={() => setModal({ type: "settings" })}>
          设置
        </button>
        <span className="spacer" />
        {settings?.schedule_enabled && (
          <span className="tag" title="应用保持运行时才会触发；可在「设置」里修改">
            每日 {settings.schedule_time} 自动签到
          </span>
        )}
        {settings?.proxy_enabled && (
          <span
            className="tag"
            title="无感接管生效中，点「无感接管」查看详情"
          >
            接管中 · 端口 {settings.proxy_port}
          </span>
        )}
        <span className="count">共 {accounts.length} 个账号</span>
      </div>

      <main className="content">
        {loading ? (
          <p className="empty">加载中…</p>
        ) : accounts.length === 0 ? (
          <div className="empty">
            <p>还没有账号。</p>
            <p>
              点「登录新账号」用系统浏览器扫码登录，或点「导入本机账号」直接读取 WorkBuddy
              写在本机的登录信息（自动带上昵称与手机号）。
            </p>
          </div>
        ) : (
          <ul className="account-list">
            {accounts.map((a) => (
              <li key={a.id} className="account-card">
                <div className="ac-main">
                  <div className="ac-title">
                    <span className="ac-name">{a.name}</span>
                    {a.phone && <span className="ac-phone">{a.phone}</span>}
                    <ResultBadge last={a.last} />
                  </div>
                  <div className="ac-meta">
                    <code className="tok">{maskToken(a.token)}</code>
                  </div>
                  <div className="ac-result">
                    <span className="ac-balance">
                      剩余积分：{formatCredits(a.last?.balance)}
                    </span>
                    {a.last?.at && <span>{a.last.at}</span>}
                    {a.last?.streak != null && <span>连续 {a.last.streak} 天</span>}
                    {a.last?.credit != null && <span>本次 +{a.last.credit}</span>}
                    {/* 「今日已签」的文案与徽标重复，不再展示；失败原因仍要显示 */}
                    {a.last?.message && !a.last.already && (
                      <span className="msg">{a.last.message}</span>
                    )}
                  </div>
                </div>
                <div className="ac-actions">
                  <button
                    className="btn small"
                    disabled={busyIds.has(a.id)}
                    onClick={() => runCheckinOne(a.id)}
                  >
                    {busyIds.has(a.id) ? "…" : "签到"}
                  </button>
                  <button
                    className="btn small ghost"
                    onClick={() => setModal({ type: "logs", accountId: a.id })}
                  >
                    日志
                  </button>
                  {/* 续签不提供手动按钮：签到前与后台扫描会自动完成（剩余有效期不足 48h） */}
                  <button
                    className="btn small danger"
                    onClick={() => void removeOne(a)}
                  >
                    删除
                  </button>
                </div>
              </li>
            ))}
          </ul>
        )}
      </main>

      {update && (
        <div className="update-bar">
          {update.message}
          {update.status === "downloading" &&
            update.total &&
            update.downloaded != null && (
              <span className="prog">
                {" "}
                {Math.round((update.downloaded / update.total) * 100)}%
              </span>
            )}
        </div>
      )}

      {toast && <div className={`toast toast-${toast.kind}`}>{toast.text}</div>}

      {modal?.type === "local" && (
        <LocalAccountsModal
          accounts={accounts}
          onImport={importItems}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}
      {modal?.type === "oauth" && (
        <OAuthModal
          onImport={importItems}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}
      {modal?.type === "takeover" && settings && (
        <TakeoverModal
          settings={settings}
          accounts={accounts}
          askConfirm={askConfirm}
          onSettings={(s) => setSettings(s)}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}
      {modal?.type === "settings" && settings && (
        <SettingsModal
          settings={settings}
          askConfirm={askConfirm}
          onReloadSettings={async () => setSettings(await getSettings())}
          onSave={async (s) => {
            try {
              setSettings(await saveSettings(s));
              setModal(null);
              showToast({ kind: "ok", text: "设置已保存" });
            } catch (e) {
              showToast({ kind: "err", text: "保存失败：" + String(e) });
            }
          }}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}
      {modal?.type === "logs" && (
        <LogsModal
          accounts={accounts}
          initialAccountId={modal.accountId}
          askConfirm={askConfirm}
          onClose={() => setModal(null)}
          onToast={showToast}
        />
      )}

      {confirmReq && (
        <ConfirmDialog req={confirmReq} onDone={resolveConfirm} />
      )}
    </div>
  );
}

/** 自研确认框：替代 window.confirm（Tauri 的 WKWebView 不支持原生 confirm 面板） */
function ConfirmDialog({
  req,
  onDone,
}: {
  req: ConfirmReq;
  onDone: (ok: boolean) => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDone(false);
      if (e.key === "Enter") onDone(true);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onDone]);

  return (
    <div className="modal-mask confirm-mask" onClick={() => onDone(false)}>
      <div className="modal confirm" onClick={(e) => e.stopPropagation()}>
        <h2>{req.title}</h2>
        {req.body && <p className="confirm-body">{req.body}</p>}
        <div className="modal-actions">
          <button className="btn ghost" autoFocus onClick={() => onDone(false)}>
            取消
          </button>
          <button
            className={req.danger ? "btn danger" : "btn primary"}
            onClick={() => onDone(true)}
          >
            {req.okText ?? "确定"}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * 「导入本机账号」：直接读 WorkBuddy 写在本机的登录信息文件（`auth/*.info`）。
 *
 * 这是最省事的一条路——不需要 WorkBuddy 正在运行、不用改启动方式，
 * 而且一次就能拿到 token + 昵称 + 手机号（导入时自动带上手机号）。
 * 代价是它只能拿到**已经在本机登录过**的账号；要收新账号请用「登录新账号」。
 */
export function LocalAccountsModal({
  accounts,
  onImport,
  onClose,
  onToast,
}: {
  accounts: Account[];
  onImport: (items: ImportItem[]) => Promise<ImportReport>;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [list, setList] = useState<LocalAccount[]>([]);
  const [loading, setLoading] = useState(true);
  const [importing, setImporting] = useState(false);

  const scan = useCallback(async () => {
    setLoading(true);
    try {
      setList(await discoverLocalAccounts());
    } catch {
      setList([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void scan();
  }, [scan]);

  const addedTokens = useMemo(
    () => new Set(accounts.map((a) => a.token)),
    [accounts]
  );
  // 已存在的账号（同 token）会被跳过而不是重复添加
  const pending = list.filter((d) => !addedTokens.has(d.token));

  const toItem = (d: LocalAccount): ImportItem => ({
    token: d.token,
    host: d.host,
    name: d.nickname || d.phone,
    phone: d.phone,
    // 不带这两个字段的话，导入的账号永远无法自动续签
    refresh_token: d.refresh_token,
    expires_at: d.expires_at,
  });

  const doImport = async (items: ImportItem[]) => {
    if (items.length === 0) return;
    setImporting(true);
    try {
      const { added, updated } = await onImport(items);
      if (added > 0 && updated > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号，更新 ${updated} 个已有账号的凭证` });
        onClose();
      } else if (added > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号` });
        onClose();
      } else if (updated > 0) {
        onToast({ kind: "ok", text: `已更新 ${updated} 个账号的凭证（补全续签信息）` });
        onClose();
      } else {
        onToast({ kind: "info", text: "没有需要导入的账号" });
      }
    } catch (e) {
      onToast({ kind: "err", text: "导入失败：" + String(e) });
    } finally {
      setImporting(false);
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <h2>导入本机账号</h2>
        <p className="hint">
          WorkBuddy 登录后会把账号与凭证写到本机
          <code>CodeBuddyExtension/Data/Public/auth/*.info</code>，这里直接读取它 ——
          <strong>不需要 WorkBuddy 正在运行，也不用改启动方式</strong>，而且能一次拿到昵称与手机号。
          仅读取、不外传。
        </p>

        {loading ? (
          <p>读取中…</p>
        ) : list.length === 0 ? (
          <p className="empty">
            未找到登录信息文件。请先在 WorkBuddy 桌面端登录一次（本工具只读，不会改动它）。
          </p>
        ) : (
          <ul className="local-list">
            {list.map((d) => {
              const added = addedTokens.has(d.token);
              return (
                <li key={d.file} className="local-item">
                  <div className="local-info">
                    <div className="local-title">
                      <span className="local-name">
                        {d.nickname || d.uid?.slice(0, 8) || "未命名账号"}
                      </span>
                      {d.phone && <span className="ac-phone">{d.phone}</span>}
                      {d.is_current && (
                        <span className="badge badge-ok">当前登录</span>
                      )}
                    </div>
                    <div className="ac-meta">
                      <code className="tok">{maskToken(d.token)}</code>
                      {d.host && <span className="tag">{d.host}</span>}
                      <span className="tag">{baseName(d.file)}</span>
                    </div>
                    {d.uid && <div className="local-uid">uid {d.uid}</div>}
                  </div>
                  <button
                    className="btn small"
                    disabled={added || importing}
                    onClick={() => void doImport([toItem(d)])}
                  >
                    {added ? "已添加" : "导入"}
                  </button>
                </li>
              );
            })}
          </ul>
        )}

        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
          <button className="btn ghost" onClick={() => void scan()}>
            重新读取
          </button>
          <button
            className="btn primary"
            disabled={importing || pending.length === 0}
            onClick={() => void doImport(pending.map(toItem))}
          >
            {importing ? "导入中…" : `全部导入（${pending.length}）`}
          </button>
        </div>
      </div>
    </div>
  );
}

const DEFAULT_HOST = "https://www.workbuddy.cn";

/**
 * 「登录新账号」：官方 OAuth state 轮询（无感登录）。
 *
 * 独立于「导入本机账号」——后者只能拿到**已经登录过**的账号，
 * 这条通道能主动把新账号签发进来，且不重启、不打断当前 WorkBuddy、不改本机登录文件。
 */
export function OAuthModal({
  onImport,
  onClose,
  onToast,
}: {
  onImport: (items: ImportItem[]) => Promise<ImportReport>;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  // 默认打到「当前登录账号」所属的域（国内版 / 国际版不能混用）；
  // 读不到本机登录文件就退回国内版。
  const [defaultHost, setDefaultHost] = useState(DEFAULT_HOST);
  useEffect(() => {
    void (async () => {
      try {
        const list = await discoverLocalAccounts();
        const cur = list.find((d) => d.is_current) ?? list[0];
        if (cur?.host) setDefaultHost(cur.host);
      } catch {
        /* 读不到就沿用默认域 */
      }
    })();
  }, []);

  const [importing, setImporting] = useState(false);
  const doImport = async (items: ImportItem[]) => {
    if (items.length === 0) return;
    setImporting(true);
    try {
      const { added, updated } = await onImport(items);
      if (added > 0 && updated > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号，更新 ${updated} 个已有账号的凭证` });
        onClose();
      } else if (added > 0) {
        onToast({ kind: "ok", text: `已导入 ${added} 个账号` });
        onClose();
      } else if (updated > 0) {
        onToast({ kind: "ok", text: `已更新 ${updated} 个账号的凭证（补全续签信息）` });
        onClose();
      } else {
        onToast({ kind: "info", text: "该账号已在列表中" });
        onClose();
      }
    } catch (e) {
      onToast({ kind: "err", text: "导入失败：" + String(e) });
    } finally {
      setImporting(false);
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <h2>登录新账号</h2>
        {/* key 让面板在探测到默认域后重建，避免内部 host 状态停留在初始值 */}
        <OAuthPanel
          key={defaultHost}
          defaultHost={defaultHost}
          importing={importing}
          onImport={doImport}
          onToast={onToast}
          onDone={onClose}
        />
      </div>
    </div>
  );
}

/**
 * 「登录新账号」面板：走官方 OAuth state 轮询，在系统浏览器里完成一次登录。
 *
 * 轮询期的 `code=11217 ("login ing")` 是**正常等待态**，不是错误。
 */
function OAuthPanel({
  defaultHost,
  importing,
  onImport,
  onToast,
  onDone,
}: {
  defaultHost: string;
  importing: boolean;
  onImport: (items: ImportItem[]) => Promise<void>;
  onToast: (t: Toast) => void;
  onDone: () => void;
}) {
  const HOSTS = [
    { value: "https://www.workbuddy.cn", label: "国内版 · www.workbuddy.cn" },
    { value: "https://www.workbuddy.ai", label: "国际版 · www.workbuddy.ai" },
    { value: "https://www.codebuddy.cn", label: "CodeBuddy CN · www.codebuddy.cn" },
    { value: "https://www.codebuddy.ai", label: "CodeBuddy 国际 · www.codebuddy.ai" },
  ];
  const [host, setHost] = useState(defaultHost);
  const [phase, setPhase] = useState<"idle" | "waiting" | "done" | "error">("idle");
  const [uri, setUri] = useState("");
  const [result, setResult] = useState<OAuthPoll | null>(null);
  const [err, setErr] = useState("");
  const [waited, setWaited] = useState(0);

  const timer = useRef<number | null>(null);
  const busy = useRef(false);

  const stop = useCallback(() => {
    if (timer.current !== null) {
      window.clearInterval(timer.current);
      timer.current = null;
    }
    busy.current = false;
  }, []);
  useEffect(() => stop, [stop]);

  const begin = async () => {
    stop();
    setResult(null);
    setErr("");
    setWaited(0);
    setUri("");
    setPhase("waiting");
    try {
      const s = await oauthStart(host);
      setUri(s.verification_uri);
      try {
        await openExternal(s.verification_uri);
      } catch {
        onToast({ kind: "info", text: "未能自动打开浏览器，请手动点「重新打开」" });
      }
      const startedAt = Date.now();
      const limitMs = (s.expires_in || 600) * 1000;
      timer.current = window.setInterval(() => {
        const elapsed = Date.now() - startedAt;
        setWaited(Math.round(elapsed / 1000));
        if (elapsed > limitMs) {
          stop();
          setErr("登录超时，请重新发起");
          setPhase("error");
          return;
        }
        // 上一次轮询还没回来就跳过这一拍，避免请求叠加
        if (busy.current) return;
        busy.current = true;
        void (async () => {
          try {
            const r = await oauthPoll(s.login_id);
            if (!r.done) return;
            stop();
            if (r.error || !r.token) {
              setErr(r.error ?? "授权完成但未返回 token");
              setPhase("error");
            } else {
              setResult(r);
              setPhase("done");
            }
          } catch (e) {
            stop();
            setErr(String(e));
            setPhase("error");
          } finally {
            busy.current = false;
          }
        })();
      }, 2000);
    } catch (e) {
      setErr(String(e));
      setPhase("error");
    }
  };

  const reset = () => {
    stop();
    setPhase("idle");
    setUri("");
    setResult(null);
    setErr("");
    setWaited(0);
  };

  return (
    <>
      <p className="hint">
        向官方授权接口申请一个 <code>state</code>，在<strong>系统浏览器</strong>里完成一次登录
        （扫码即可），本工具轮询取得该账号的凭证 ——
        <strong>不重启、不打断当前 WorkBuddy，也不改动本机登录文件</strong>。
        适合把第二个 / 第三个账号收进来。
      </p>

      {phase === "idle" && (
        <div className="opt-col">
          <label className="wide">
            接口域
            <select value={host} onChange={(e) => setHost(e.target.value)}>
              {HOSTS.map((h) => (
                <option key={h.value} value={h.value}>
                  {h.label}
                </option>
              ))}
            </select>
          </label>
        </div>
      )}

      {phase === "waiting" && (
        <>
          <p className="oauth-wait">
            ⏳ 请在弹出的浏览器窗口中完成登录 / 扫码… 已等待 {waited}s（10 分钟内有效）
          </p>
          {uri && (
            <div className="oauth-uri">
              <code>{uri}</code>
              <button className="btn small" onClick={() => void openExternal(uri)}>
                重新打开
              </button>
            </div>
          )}
        </>
      )}

      {phase === "done" && result?.token && (
        <div className="result-card">
          <div className="local-title">
            <span className="local-name">
              {result.nickname || result.uid?.slice(0, 8) || "新账号"}
            </span>
            {result.phone && <span className="ac-phone">{result.phone}</span>}
            <span className="badge badge-ok">授权成功</span>
          </div>
          <div className="ac-meta">
            <code className="tok">{maskToken(result.token)}</code>
            {result.host && <span className="tag">{result.host}</span>}
            {result.uid && <span className="tag">uid {result.uid}</span>}
          </div>
        </div>
      )}

      {phase === "error" && <p className="empty">授权失败：{err}</p>}

      <div className="modal-actions">
        {phase === "done" && result?.token ? (
          <>
            <button className="btn ghost" onClick={onDone} disabled={importing}>
              关闭
            </button>
            <button className="btn ghost" onClick={reset} disabled={importing}>
              再登一个
            </button>
            <button
              className="btn primary"
              disabled={importing}
              onClick={() =>
                void onImport([
                  {
                    token: result.token as string,
                    host: result.host,
                    name: result.nickname || result.phone,
                    phone: result.phone,
                    refresh_token: result.refresh_token,
                    expires_at: result.expires_at,
                  },
                ])
              }
            >
              添加为账号
            </button>
          </>
        ) : phase === "waiting" ? (
          <button className="btn ghost" onClick={reset}>
            取消
          </button>
        ) : (
          <>
            <button className="btn ghost" onClick={onDone}>
              关闭
            </button>
            <button className="btn primary" onClick={() => void begin()}>
              打开授权页并开始
            </button>
          </>
        )}
      </div>
    </>
  );
}

/**
 * 「无感接管」独立弹窗。
 *
 * 从「设置」里提出来单独放：它牵涉改 WorkBuddy 全局配置 + 重启桌面端与 CLI host，
 * 是整个应用里唯一的高危操作，值得一个专属入口和完整的状态面板
 * （开关 / 端口 / 优先扣费账号 / 心跳状态 / 最近路由），不与普通设置混排。
 */
export function TakeoverModal({
  settings,
  accounts,
  askConfirm,
  onSettings,
  onClose,
  onToast,
}: {
  settings: Settings;
  /** 账号列表：优先扣费账号下拉需要展示账号名 */
  accounts: Account[];
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 保存 / 停止接管后把最新 settings 同步回外层（后端可能已代为改写字段） */
  onSettings: (s: Settings) => void;
  onClose: () => void;
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

  // 只覆盖接管相关字段，其余设置原样透传 —— 本弹窗对它们没有编辑权
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
    <div className="modal-mask" onClick={onClose}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <h2>无感接管（WorkBuddy 专用）</h2>

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
              用下面的「立即停止接管」或「设置 → 网络急救 → 一键恢复」都能一步恢复。
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
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
          <button className="btn primary" disabled={busy || !pending} onClick={() => void doSave()}>
            {busy
              ? "应用中…"
              : proxyOn !== settings.proxy_enabled
              ? "应用并重启 WorkBuddy"
              : "保存"}
          </button>
        </div>
      </div>
    </div>
  );
}

export function SettingsModal({
  settings,
  askConfirm,
  onReloadSettings,
  onSave,
  onClose,
  onToast,
}: {
  settings: Settings;
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 恢复/关闭反代后让外层状态跟上（本弹窗的本地 state 不会被父组件重置） */
  onReloadSettings: () => Promise<void>;
  onSave: (s: Settings) => Promise<void>;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [auto, setAuto] = useState(settings.auto_checkin_on_start);
  const [schedOn, setSchedOn] = useState(settings.schedule_enabled);
  const [schedTime, setSchedTime] = useState(settings.schedule_time);
  const [notifyOn, setNotifyOn] = useState(settings.notify_enabled);
  const [webhook, setWebhook] = useState(settings.notify_webhook);
  const [notifySched, setNotifySched] = useState(settings.notify_on_schedule);
  const [notifyManual, setNotifyManual] = useState(settings.notify_on_manual);
  const [testing, setTesting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  // 网络急救：诊断结果 / 恢复步骤 / 备份路径
  const [netReport, setNetReport] = useState<NetReport | null>(null);
  const [netSteps, setNetSteps] = useState<NetStep[] | null>(null);
  const [netBackups, setNetBackups] = useState<string[]>([]);
  const [netBusy, setNetBusy] = useState<"diag" | "restore" | null>(null);
  // 开机自启动是「操作系统状态」，不属于 settings.json，改一次立即生效
  const [autostart, setAutostartOn] = useState<boolean | null>(null);
  const [autoBusy, setAutoBusy] = useState(false);

  const doDiagnose = async () => {
    setNetBusy("diag");
    try {
      setNetReport(await netDiagnose());
      // 上一次的恢复轨迹已经过时了，清掉免得和新的诊断结果混在一起
      setNetSteps(null);
      setNetBackups([]);
    } catch (e) {
      onToast({ kind: "err", text: "诊断失败：" + String(e) });
    } finally {
      setNetBusy(null);
    }
  };

  const doRestore = async () => {
    const ok = await askConfirm({
      title: "一键恢复网络配置",
      body:
        "会依次：安全关闭无感接管并重启 WorkBuddy/CLI host、备份并清除全局调试端点、" +
        "取消 launchd 全局环境变量，最后复检。被改动的文件都会先备份。继续？",
      okText: "恢复",
      danger: true,
    });
    if (!ok) return;
    setNetBusy("restore");
    try {
      const rep = await netRestore();
      // 接管可能是这次恢复关掉的（后端已落盘 proxy_enabled=false），外层状态要跟上
      await onReloadSettings();
      setNetReport(rep.report);
      setNetSteps(rep.steps);
      setNetBackups(rep.backups);
      const failed = rep.steps.filter((s) => !s.ok).length;
      onToast(
        failed
          ? { kind: "err", text: `恢复完成，但有 ${failed} 步失败，见下方详情` }
          : { kind: "ok", text: "已恢复：残留配置已清除" }
      );
    } catch (e) {
      onToast({ kind: "err", text: "恢复失败：" + String(e) });
    } finally {
      setNetBusy(null);
    }
  };

  useEffect(() => {
    getAutostart()
      .then(setAutostartOn)
      .catch(() => setAutostartOn(null));
    // 顺手扫一遍（只读）：有问题时用户一打开设置就能看到，不用先知道要点"诊断"
    void doDiagnose();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const toggleAutostart = async (next: boolean) => {
    setAutoBusy(true);
    setAutostartOn(next); // 乐观更新，失败再回滚
    try {
      setAutostartOn(await setAutostart(next));
      onToast({ kind: "ok", text: next ? "已设置开机自启动" : "已关闭开机自启动" });
    } catch (e) {
      setAutostartOn(!next);
      onToast({ kind: "err", text: String(e) });
    } finally {
      setAutoBusy(false);
    }
  };

  // 默认 Base URL 是内置常量，界面不提供修改入口（改错会让签到打到错误的域）；
  // 接管相关字段本弹窗没有编辑权，原样透传（它们归「无感接管」弹窗管）
  const snapshot = (): Settings => ({
    ...settings,
    auto_checkin_on_start: auto,
    schedule_enabled: schedOn,
    // <input type="time"> 已产出 HH:MM；后端还会再规范化一次
    schedule_time: schedTime.trim(),
    notify_enabled: notifyOn,
    notify_webhook: webhook.trim(),
    notify_on_schedule: notifySched,
    notify_on_manual: notifyManual,
  });

  const doTest = async () => {
    setTesting(true);
    try {
      const res = await testNotify(webhook.trim());
      onToast({ kind: "ok", text: "推送成功：" + res });
    } catch (e) {
      onToast({ kind: "err", text: "推送失败：" + String(e) });
    } finally {
      setTesting(false);
    }
  };

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <h2>设置</h2>

        <label className="checkbox">
          <input
            type="checkbox"
            checked={auto}
            onChange={(e) => setAuto(e.target.checked)}
          />
          启动应用时自动签到全部账号
        </label>

        <h3 className="sec">定时自动签到</h3>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={schedOn}
            onChange={(e) => setSchedOn(e.target.checked)}
          />
          每天定时自动签到全部账号
        </label>
        {schedOn && (
          <>
            <div className="opt-col">
              <label>
                签到时刻（24 小时制）
                <input
                  type="time"
                  value={schedTime}
                  onChange={(e) => setSchedTime(e.target.value)}
                />
              </label>
            </div>
            <p className="hint">
              ⚠️ 定时任务只在<strong>应用运行期间</strong>触发（桌面端退出后没有后台进程可代为执行）。
              错过时刻后 30 分钟内打开应用会自动补签一次。
            </p>
          </>
        )}
        <label className="checkbox">
          <input
            type="checkbox"
            checked={autostart === true}
            disabled={autoBusy || autostart === null}
            onChange={(e) => void toggleAutostart(e.target.checked)}
          />
          开机自启动（登录时自动运行本应用）
        </label>
        {autostart === null ? (
          <p className="hint">未能读取开机自启动状态（该平台可能不支持）。</p>
        ) : (
          schedOn &&
          !autostart && (
            <p className="hint">
              建议同时开启「开机自启动」，否则应用不运行时定时签到不会发生。
            </p>
          )
        )}

        <h3 className="sec">签到通知</h3>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={notifyOn}
            onChange={(e) => setNotifyOn(e.target.checked)}
          />
          开启 webhook 通知
        </label>
        {notifyOn && (
          <>
            <label className="wide">
              Webhook 地址
              <input
                value={webhook}
                placeholder="https://…/hook/&lt;key&gt;"
                onChange={(e) => setWebhook(e.target.value)}
              />
            </label>
            <div className="opt-row">
              <button
                className="btn small"
                disabled={testing || !webhook.trim()}
                onClick={() => void doTest()}
              >
                {testing ? "发送中…" : "测试推送"}
              </button>
              <span className="hint inline">点一下会给这个地址发一条测试消息</span>
            </div>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={notifySched}
                onChange={(e) => setNotifySched(e.target.checked)}
              />
              定时签到后推送
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={notifyManual}
                onChange={(e) => setNotifyManual(e.target.checked)}
              />
              手动「全部签到」后推送
            </label>
          </>
        )}

        <h3 className="sec">网络急救</h3>
        <p className="hint">
          调试「自定义服务端点」时，如果把地址写进了 WorkBuddy 的
          <strong>全局</strong>配置（<code>~/.workbuddy/settings.json</code> 的 <code>env</code>，
          或 launchd 全局环境变量），受影响的会是<strong>整个 WorkBuddy</strong>（含正在运行的桌面端），
          典型表现是「502 连接被拒绝」。这里可以扫出这些残留并一键清掉。
        </p>
        <div className="opt-row">
          <button
            className="btn small"
            disabled={netBusy !== null}
            onClick={() => void doDiagnose()}
          >
            {netBusy === "diag" ? "扫描中…" : "重新诊断"}
          </button>
          <button
            className="btn small danger"
            disabled={netBusy !== null}
            onClick={() => void doRestore()}
          >
            {netBusy === "restore" ? "恢复中…" : "一键恢复（含关闭反代）"}
          </button>
        </div>

        {netReport &&
          (netReport.healthy && netReport.issues.length === 0 ? (
            <p className="net-ok">
              ✓ 未发现残留端点配置，WorkBuddy 的网络链路是干净的。
            </p>
          ) : (
            <>
              <ul className="net-list">
                {netReport.issues.map((it) => (
                  <li key={it.id} className={`net-item ${it.level}`}>
                    <div className="net-head">
                      <span
                        className={`badge ${
                          it.level === "block"
                            ? "badge-err"
                            : it.level === "ok"
                            ? "badge-ok"
                            : "badge-already"
                        }`}
                      >
                        {it.level === "block"
                          ? "会断网"
                          : it.level === "ok"
                          ? "正常"
                          : "残留"}
                      </span>
                      <span className="net-scope">{it.scope}</span>
                      <span className="net-target">{it.target}</span>
                    </div>
                    <div className="net-value">{it.value}</div>
                    <div className="net-note">
                      {it.note}
                      {!it.fixable && "（此项只报告，需要你手动处理）"}
                    </div>
                  </li>
                ))}
              </ul>
              {netReport.issues.some((i) => i.fixable && i.level !== "ok") && (
                <p className="hint">
                  「一键恢复」会清除上表中标记为可自动处理的项；改动前一律先备份。
                </p>
              )}
            </>
          ))}

        {netSteps && (
          <>
            <ul className="net-steps">
              {netSteps.map((s, i) => (
                <li key={`${i}-${s.action}`} className={`net-step ${s.ok ? "ok" : "bad"}`}>
                  <span className="mark">{s.ok ? "✓" : "✕"}</span>
                  <span>
                    <strong>{s.action}</strong>：{s.detail}
                  </span>
                </li>
              ))}
            </ul>
            {netBackups.length > 0 && (
              <div className="net-backups">
                <span>已备份：</span>
                {netBackups.map((b) => (
                  <button
                    key={b}
                    className="link-btn"
                    title={b}
                    onClick={() =>
                      void revealPath(b).catch((e) =>
                        onToast({ kind: "err", text: String(e) })
                      )
                    }
                  >
                    {baseName(b)}
                  </button>
                ))}
              </div>
            )}
          </>
        )}

        {err && <p className="form-err">{err}</p>}
        <div className="modal-actions">
          <button className="btn ghost" onClick={onClose}>
            取消
          </button>
          <button
            className="btn primary"
            disabled={busy}
            onClick={async () => {
              setBusy(true);
              setErr("");
              try {
                await onSave(snapshot());
              } catch (e) {
                setErr(String(e));
                setBusy(false);
              }
            }}
          >
            {busy ? "保存中…" : "保存"}
          </button>
        </div>
      </div>
    </div>
  );
}

function LogBadge({ log }: { log: CheckinLog }) {
  // 同 ResultBadge：`already` 必须优先于 `success`（已签时二者都为 true）
  if (log.already) return <span className="badge badge-already">今日已签</span>;
  if (log.success) return <span className="badge badge-ok">成功</span>;
  if (log.inactive) return <span className="badge badge-idle">活动未开</span>;
  return <span className="badge badge-err">失败</span>;
}

/** 一批签到结果的互斥计数（成功 / 已签 / 失败），避免「已签」被重复算成「成功」 */
function tally(items: { success: boolean; already: boolean; inactive: boolean }[]) {
  return {
    ok: items.filter((l) => l.success && !l.already).length,
    already: items.filter((l) => l.already).length,
    fail: items.filter((l) => !l.success && !l.already && !l.inactive).length,
  };
}

/// 账号在日志/筛选中的展示名：名称 + 手机号（手机号缺失时省略）
function accountLabel(name: string, phone?: string | null): string {
  return phone ? `${name}（${phone}）` : name;
}

/**
 * 签到日志：只按时间倒序列出，不做账号筛选 / 分组统计。
 *
 * 入口在每个账号条目上 —— 从条目进来看到的就是该账号的记录，
 * 因此这里不再提供「切换账号」下拉。
 */
export function LogsModal({
  accounts,
  initialAccountId,
  askConfirm,
  onClose,
  onToast,
}: {
  accounts: Account[];
  initialAccountId?: string;
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onClose: () => void;
  onToast: (t: Toast) => void;
}) {
  const [logs, setLogs] = useState<CheckinLog[]>([]);
  const [loading, setLoading] = useState(true);
  // 从账号条目进入时限定该账号；弹窗生命周期内不会变
  const accountId = initialAccountId;
  const account = accounts.find((a) => a.id === accountId);
  // 固定露出 3.5 条日志：容器高度按首条实测高度算（字号/内容变化都能自适应）
  const listRef = useRef<HTMLUListElement | null>(null);
  const [listMax, setListMax] = useState<number | null>(null);

  useEffect(() => {
    if (loading) return;
    const first = listRef.current?.querySelector<HTMLElement>(".log-item");
    if (first) {
      setListMax(first.offsetHeight * 3.5 + 8 /* gap */ * 3);
    }
  }, [loading, logs]);

  const refresh = useCallback(() => {
    setLoading(true);
    getCheckinLogs(300, accountId)
      .then(setLogs)
      .catch(() => setLogs([]))
      .finally(() => setLoading(false));
  }, [accountId]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return (
    <div className="modal-mask" onClick={onClose}>
      <div className="modal wide" onClick={(e) => e.stopPropagation()}>
        <h2>
          {account
            ? `签到日志 · ${accountLabel(account.name, account.phone)}`
            : "签到日志"}
        </h2>

        {loading ? (
          <p>加载中…</p>
        ) : logs.length === 0 ? (
          <p className="empty">暂无签到记录。</p>
        ) : (
          <ul className="log-list" ref={listRef} style={listMax ? { height: listMax } : undefined}>
            {logs.map((l) => (
              <li key={l.id} className="log-item">
                <div className="log-head">
                  <LogBadge log={l} />
                  <span className="log-time">{l.at}</span>
                </div>
                <div className="log-meta">
                  {l.balance != null && <span>剩余 {formatCredits(l.balance)}</span>}
                  {l.credit != null && <span>本次 +{l.credit}</span>}
                </div>
                {l.message && <div className="log-msg">{l.message}</div>}
              </li>
            ))}
          </ul>
        )}

        <div className="modal-actions">
          <button
            className="btn danger"
            disabled={logs.length === 0}
            onClick={async () => {
              const tip = account
                ? {
                    title: "清空日志",
                    body: `确认清空「${accountLabel(account.name, account.phone)}」的全部签到日志？`,
                    okText: "清空",
                    danger: true,
                  }
                : {
                    title: "清空日志",
                    body: "确认清空全部签到日志？",
                    okText: "清空",
                    danger: true,
                  };
              if (!(await askConfirm(tip))) return;
              try {
                await clearCheckinLogs(accountId);
                onToast({ kind: "ok", text: "日志已清空" });
                refresh();
              } catch (e) {
                onToast({ kind: "err", text: "清空失败：" + String(e) });
              }
            }}
          >
            清空日志
          </button>
          <button className="btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
