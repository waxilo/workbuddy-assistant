import { useCallback, useEffect, useState } from "react";
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
  refreshCredits,
  getSettings,
  saveSettings as saveSettingsApi,
  appVersion,
} from "./api";
import { checkAndInstall, type UpdateProgress } from "./updater";
import { tally, type ConfirmReq, type Toast } from "./common";
import { AccountsPage } from "./pages/AccountsPage";
import { TakeoverPage } from "./pages/TakeoverPage";
import { NetfixPage } from "./pages/NetfixPage";
import { LogsPage } from "./pages/LogsPage";
import { SettingsPage } from "./pages/SettingsPage";
import { LocalAccountsModal, OAuthModal } from "./components/ImportModals";
import { ConfirmDialog } from "./components/ConfirmDialog";

/**
 * 应用外壳：左侧导航栏 + 右侧内容区。
 *
 * 页面（tab）承载常驻功能：账号签到 / 无感接管 / 网络急救 / 签到日志 / 设置；
 * 弹窗只留给「做完即走」的任务流（登录新账号、导入本机账号、危险操作确认）。
 */
type Page = "accounts" | "takeover" | "netfix" | "logs" | "settings";

type Modal = { type: "local" } | { type: "oauth" } | null;

const NAV: { key: Page; label: string; icon: string }[] = [
  { key: "accounts", label: "账号签到", icon: "✓" },
  { key: "takeover", label: "无感接管", icon: "⇄" },
  { key: "netfix", label: "网络急救", icon: "✚" },
  { key: "logs", label: "签到日志", icon: "☰" },
  { key: "settings", label: "设置", icon: "⚙" },
];

const PAGE_TITLES: Record<Page, string> = {
  accounts: "账号签到",
  takeover: "无感接管（WorkBuddy 专用）",
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
        body: `确认删除「${a.name}${a.phone ? `（${a.phone}）` : ""}」？该账号的签到日志也会一并删除。`,
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

  // 检查更新：结果只走一个通用 toast（顶部）。
  // update-bar（底部胶囊）只留给「下载中/安装中」这类过程态，
  // 避免「已是最新版本」同时弹顶部 toast + 底部胶囊两条通知。
  const onUpdate = useCallback(async () => {
    await checkAndInstall((p) => {
      if (p.status === "no-update" || p.status === "error") {
        setUpdate(null); // 收起「正在检查更新…」的过程条
        showToast({ kind: p.status === "error" ? "err" : "info", text: p.message });
        return;
      }
      setUpdate(p);
    });
  }, [showToast]);

  const saveSettings = useCallback(
    async (s: Settings) => {
      const saved = await saveSettingsApi(s);
      setSettings(saved);
      showToast({ kind: "ok", text: "设置已保存" });
    },
    [showToast]
  );

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="logo">✓</span>
          <div>
            <h1>WorkBuddy 助手</h1>
            <p className="sub">v{version}</p>
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
              <span className="nav-icon">{n.icon}</span>
              {n.label}
              {n.key === "takeover" && settings?.proxy_enabled && (
                <span className="nav-dot" title="接管生效中" />
              )}
            </button>
          ))}
        </nav>

        <div className="sidebar-foot">
          <button className="btn ghost" onClick={onUpdate}>
            检查更新
          </button>
          <span className="count">共 {accounts.length} 个账号</span>
        </div>
      </aside>

      <div className="main">
        <header className="pagebar">
          <h2>{PAGE_TITLES[page]}</h2>
          <span className="spacer" />
          {page === "accounts" && (
            <>
              {settings?.schedule_enabled && (
                <span className="tag" title="应用保持运行时才会触发；可在「设置」里修改">
                  每日 {settings.schedule_time} 自动签到
                </span>
              )}
              {settings?.stagger_checkin && settings.stagger_max_seconds > 0 && (
                <span
                  className="tag"
                  title="批量签到时账号之间随机间隔，降低同 IP 触发风控的概率"
                >
                  风控间隔 ≤{settings.stagger_max_seconds}s
                </span>
              )}
              <button
                className="btn ghost"
                disabled={accounts.length === 0}
                title="把全部账号导出为 JSON（含登录凭证），可在其他机器上导入"
                onClick={() => void runExport()}
              >
                导出
              </button>
              <button
                className="btn ghost"
                title="从导出的 JSON 文件导入账号（按手机号/token 合并）"
                onClick={() => void runImportFile()}
              >
                导入
              </button>
              <button
                className="btn ghost"
                disabled={busyRefresh || accounts.length === 0}
                title="查询全部账号的最新剩余积分，不触发签到"
                onClick={runRefresh}
              >
                {busyRefresh ? "刷新中…" : "刷新"}
              </button>
              <button
                className="btn primary"
                disabled={busyAll || accounts.length === 0}
                onClick={runCheckinAll}
              >
                {busyAll ? "签到中…" : "全部签到"}
              </button>
            </>
          )}
          {page === "logs" && (
            <button className="btn ghost" onClick={() => setModal({ type: "oauth" })}>
              登录新账号
            </button>
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
              onLoginNew={() => setModal({ type: "oauth" })}
              onImportLocal={() => setModal({ type: "local" })}
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

      {confirmReq && (
        <ConfirmDialog req={confirmReq} onDone={resolveConfirm} />
      )}
    </div>
  );
}
