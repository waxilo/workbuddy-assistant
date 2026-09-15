import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  Account,
  ImportItem,
  ImportReport,
  LocalAccount,
  OAuthPoll,
} from "../types";
import {
  discoverLocalAccounts,
  oauthPoll,
  oauthStart,
  openExternal,
} from "../api";
import { baseName, maskPhone, maskToken } from "../common";
import type { Toast } from "../common";
import { Dialog } from "./Dialog";

/**
 * 账号导入的两条通道（都保留弹窗形态——它们是「做完即走」的任务流）：
 * - 导入本机账号：读 WorkBuddy 写在本机的 auth/*.info；
 * - 登录新账号：官方 OAuth state 轮询，在系统浏览器完成登录。
 */

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
    <Dialog label="导入本机账号" className="wide" onClose={onClose}>
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
                      {d.phone && <span className="ac-phone">{maskPhone(d.phone)}</span>}
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
    </Dialog>
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
    <Dialog label="登录新账号" className="wide" onClose={onClose}>
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
    </Dialog>
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
            {result.phone && <span className="ac-phone">{maskPhone(result.phone)}</span>}
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
