import { useCallback, useEffect, useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import type { Account, ImportItem, ImportReport, Settings } from "./types";
import {
  listAccounts,
  importAccounts,
  importAccountsFile,
  exportAccounts,
  removeAccount,
  checkinOne,
  checkinAll,
  refreshAll,
  getSettings,
  saveSettings as saveSettingsApi,
  appVersion,
} from "./api";
import { checkAndInstall } from "./updater";
import { accountLabel, tally, type ConfirmReq, type Toast } from "./common";
import { AccountsPage } from "./pages/AccountsPage";
import { TakeoverPage } from "./pages/TakeoverPage";
import { NetfixPage } from "./pages/NetfixPage";
import { LogsPage } from "./pages/LogsPage";
import { SettingsPage } from "./pages/SettingsPage";
import {
  IconCheck,
  IconSwap,
  IconActivity,
  IconList,
  IconGear,
  IconUserPlus,
  IconDownload,
  IconUpload,
  IconRefresh,
} from "./components/Icons";
import { LocalAccountsModal, OAuthModal } from "./components/ImportModals";
import { ConfirmDialog } from "./components/ConfirmDialog";

/**
 * 应用外壳：左侧导航栏 + 右侧内容区。
 *
 * 页面（tab）承载常驻功能：账号签到 / 智能接管 / 网络急救 / 签到日志 / 设置；
 * 弹窗只留给「做完即走」的任务流（登录新账号、导入本机账号、危险操作确认）。
 */
type Page = "accounts" | "takeover" | "netfix" | "logs" | "settings";

type Modal = { type: "local" } | { type: "oauth" } | null;

const NAV: { key: Page; label: string }[] = [
  { key: "accounts", label: "账号签到" },
  { key: "takeover", label: "智能接管" },
  { key: "netfix", label: "网络急救" },
  { key: "logs", label: "签到日志" },
  { key: "settings", label: "设置" },
];

const PAGE_ICON: Record<Page, ReactNode> = {
  accounts: <IconCheck />,
  takeover: <IconSwap />,
  netfix: <IconActivity />,
  logs: <IconList />,
  settings: <IconGear />,
};

const PAGE_TITLES: Record<Page, string> = {
  accounts: "账号签到",
  takeover: "智能接管",
  netfix: "网络急救",
  logs: "签到日志",
  settings: "设置",
};

export default function App() {
  const [page, setPage] = useState<Page>("accounts");
  /** 日志页的初始账号筛选（从账号条目点「日志」跳转时带上） */
  const [logsInitialId, setLogsInitial] = useState<string | null>(null);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [version, setVersion] = useState("");
  const [loading, setLoading] = useState(true);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [busyAll, setBusyAll] = useState(false);
  const [busyRefresh, setBusyRefresh] = useState(false);
  const [modal, setModal] = useState<Modal>(null);
  const [toast, setToast] = useState<Toast>(null);
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

  const reloadSettings = useCallback(async () => {
    setSettings(await getSettings());
  }, []);

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

  // 一键刷新：重拉并持久化积分快照（含最早过期时间）+ 真实签到状态 + 积分余量，
  // 不触发签到（已签账号再打签到接口只会拿到 400）；后端返回最新账号列表一次到位。
  const runRefresh = useCallback(async () => {
    setBusyRefresh(true);
    try {
      // 一键刷新：积分快照（含最早过期时间）+ 真实签到状态 + 积分余量，全部重拉并持久化
      const updated = await refreshAll();
      setAccounts(updated);
      const got = updated.filter((a) => a.credit_snapshot?.credits != null).length;
      const st = updated.filter((a) => a.checked_today === true).length;
      showToast({
        kind: "ok",
        text: `已刷新 ${updated.length} 个账号（${got} 个取到积分，当前 ${st} 个今日已签）`,
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
        showToast({ kind: "ok", text: `已删除 ${accountLabel(a.name, a.phone)}` });
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
      if (report.added > 0 || report.updated > 0) {
        await load();
        // 导入新账号后补查真实状态（只读查询，持久化），列表直接反映服务端真相
        void refreshAll();
      }
      return report;
    },
    [load]
  );

  // 导出账号：弹系统保存框选路径，生成可迁移的 JSON（含凭证，提示妥善保管）
  const runExport = useCallback(async () => {
    try {
      const path = await save({
        title: "导出账号",
        defaultPath: `workbuddy-accounts-${new Date().toISOString().slice(0, 10)}.json`,
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (!path) return;
      await exportAccounts(path);
      showToast({
        kind: "ok",
        text: `已导出到 ${path}（文件含登录凭证，请妥善保管）`,
      });
    } catch (e) {
      showToast({ kind: "err", text: "导出失败：" + String(e) });
    }
  }, [showToast]);

  // 导入账号：选导出文件，按手机号/token 合并，跨机器迁移零重复
  const runImportFile = useCallback(async () => {
    try {
      const picked = await open({
        title: "导入账号",
        multiple: false,
        directory: false,
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (typeof picked !== "string") return;
      const report = await importAccountsFile(picked);
      await load();
      showToast({
        kind: "ok",
        text: `导入完成：新增 ${report.added}，更新 ${report.updated}`,
      });
    } catch (e) {
      showToast({ kind: "err", text: "导入失败：" + String(e) });
    }
  }, [load, showToast]);

  // 检查更新：所有通知统一走顶部 toast（底部通知条已移除）。
  // 下载中的进度事件只提示一次，避免刷屏。
  const onUpdate = useCallback(async () => {
    let downloadShown = false;
    await checkAndInstall((p) => {
      if (p.status === "checking") return;
      if (p.status === "downloading") {
        if (!downloadShown) {
          downloadShown = true;
          showToast({ kind: "info", text: p.message });
        }
        return;
      }
      showToast({
        kind:
          p.status === "error"
            ? "err"
            : p.status === "no-update"
            ? "info"
            : "ok",
        text: p.message,
      });
    });
  }, [showToast]);

  const saveSettings = useCallback(async (s: Settings) => {
    // 设置页已改为「改动自动保存」，这里只负责落盘并刷新内存中的 settings，
    // 不再弹成功 toast（每次改动都弹会刷屏）。失败提示由设置页兜底。
    const saved = await saveSettingsApi(s);
    setSettings(saved);
  }, []);

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="logo">
            <IconCheck size={18} />
          </span>
          <div className="brand-text">
            <span className="brand-name">WorkBuddy 助手</span>
            <span className="brand-ver">v{version}</span>
          </div>
        </div>

        <nav className="nav">
          {NAV.map((n) => (
            <button
              key={n.key}
              className={`nav-item ${page === n.key ? "active" : ""}`}
              onClick={() => {
                setPage(n.key);
                // 从导航进入日志页时不带账号预设（只有从账号条目跳转才带）
                if (n.key === "logs") setLogsInitial(null);
              }}
            >
              <span className="nav-icon">{PAGE_ICON[n.key]}</span>
              {n.label}
              {n.key === "takeover" && settings?.proxy_enabled && (
                <span className="nav-dot" title="接管生效中" />
              )}
            </button>
          ))}
        </nav>

        <div className="sidebar-foot">
          <button className="btn ghost block" onClick={onUpdate}>
            检查更新
          </button>
          <div className="side-count">共 {accounts.length} 个账号</div>
        </div>
      </aside>

      <div className="main">
        <header className="pagebar">
          <div className="pagebar-left">
            <span className="pagebar-icon">{PAGE_ICON[page]}</span>
            <h2>{PAGE_TITLES[page]}</h2>
          </div>
          <span className="spacer" />
          {page === "accounts" && (
            <>
              <div className="status-group">
                {settings?.schedule_enabled && (
                  <span
                    className="status-pill"
                    title="应用保持运行时才会触发；可在「设置」里修改"
                  >
                    <i className="dot" />
                    每日 {settings.schedule_time} 自动签到
                  </span>
                )}
                {settings?.stagger_checkin && settings.stagger_max_seconds > 0 && (
                  <span
                    className="status-pill warn"
                    title="批量签到时账号之间随机间隔，降低同 IP 触发风控的概率"
                  >
                    <i className="dot" />
                    风控间隔 ≤{settings.stagger_max_seconds}s
                  </span>
                )}
              </div>
              <div className="action-group">
                <button
                  className="btn ghost"
                  title="用系统浏览器扫码登录新账号"
                  onClick={() => setModal({ type: "oauth" })}
                >
                  <IconUserPlus size={15} />
                  登录新账号
                </button>
                <button
                  className="btn ghost"
                  title="读取本机 WorkBuddy 登录信息自动添加账号"
                  onClick={() => setModal({ type: "local" })}
                >
                  导入本机账号
                </button>
                <button
                  className="btn ghost"
                  disabled={accounts.length === 0}
                  title="把全部账号导出为 JSON（含登录凭证），可在其他机器上导入"
                  onClick={() => void runExport()}
                >
                  <IconDownload size={15} />
                  导出
                </button>
                <button
                  className="btn ghost"
                  title="从导出的 JSON 文件导入账号（按手机号/token 合并）"
                  onClick={() => void runImportFile()}
                >
                  <IconUpload size={15} />
                  导入
                </button>
                <button
                  className="btn ghost"
                  disabled={busyRefresh || accounts.length === 0}
                  title="重拉全部账号的积分快照 / 签到状态 / 积分余量并持久化"
                  onClick={runRefresh}
                >
                  {busyRefresh ? (
                    <>
                      <IconRefresh size={15} className="spin" />
                      刷新中
                    </>
                  ) : (
                    <>
                      <IconRefresh size={15} />
                      刷新
                    </>
                  )}
                </button>
                <button
                  className="btn primary"
                  disabled={busyAll || accounts.length === 0}
                  onClick={runCheckinAll}
                >
                  {busyAll ? "签到中…" : "全部签到"}
                </button>
              </div>
            </>
          )}
          {page === "logs" && (
            <span className="status-pill">
              <IconList size={14} />
              按账号筛选查看签到记录
            </span>
          )}
        </header>

        <main className={"content" + (page === "takeover" ? " content-fill" : "")}>
          {page === "accounts" && (
              <AccountsPage
              accounts={accounts}
              loading={loading}
              busyIds={busyIds}
              onCheckinOne={(id) => void runCheckinOne(id)}
              onRemove={(a) => void removeOne(a)}
              onOpenLogs={(id) => {
                // 从账号条目进日志页：锁定该账号
                setLogsInitial(id);
                setPage("logs");
              }}
            />
          )}
          {page === "takeover" && settings && (
            <TakeoverPage
              settings={settings}
              accounts={accounts}
              askConfirm={askConfirm}
              onSettings={setSettings}
              onToast={showToast}
            />
          )}
          {page === "netfix" && (
            <NetfixPage
              askConfirm={askConfirm}
              onReloadSettings={reloadSettings}
              onToast={showToast}
            />
          )}
          {page === "logs" && (
            <LogsPage
              accounts={accounts}
              initialAccountId={logsInitialId ?? undefined}
              askConfirm={askConfirm}
              onToast={showToast}
            />
          )}
          {page === "settings" && settings && (
            <SettingsPage
              settings={settings}
              onSave={async (s) => {
                try {
                  await saveSettings(s);
                } catch (e) {
                  showToast({ kind: "err", text: "保存失败：" + String(e) });
                }
              }}
              onToast={showToast}
            />
          )}
        </main>
      </div>

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

      {confirmReq && (
        <ConfirmDialog req={confirmReq} onDone={resolveConfirm} />
      )}
    </div>
  );
}
