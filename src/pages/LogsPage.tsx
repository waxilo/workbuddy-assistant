import { useCallback, useEffect, useState } from "react";
import type { Account, CheckinLog } from "../types";
import { clearCheckinLogs, getCheckinLogs } from "../api";
import { LogBadge, accountLabel, formatCredits } from "../common";
import type { ConfirmReq, Toast } from "../common";

/**
 * 「签到日志」页：按时间倒序列出全部记录，可按账号筛选。
 *
 * 从账号条目跳进来时默认锁定该账号；作为整页后也可以随时切换查看范围。
 */
export function LogsPage({
  accounts,
  initialAccountId,
  askConfirm,
  onToast,
}: {
  accounts: Account[];
  initialAccountId?: string;
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onToast: (t: Toast) => void;
}) {
  const [logs, setLogs] = useState<CheckinLog[]>([]);
  const [loading, setLoading] = useState(true);
  // 空串 = 全部账号；从账号条目进入时初始为该账号
  const [accountId, setAccountId] = useState(initialAccountId ?? "");

  const refresh = useCallback(() => {
    setLoading(true);
    getCheckinLogs(300, accountId || undefined)
      .then(setLogs)
      .catch(() => setLogs([]))
      .finally(() => setLoading(false));
  }, [accountId]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return (
    <section className="panel-page">
      <div className="logs-toolbar">
        <label className="filter">
          账号
          <select value={accountId} onChange={(e) => setAccountId(e.target.value)}>
            <option value="">全部</option>
            {accounts.map((a) => (
              <option key={a.id} value={a.id}>
                {accountLabel(a.name, a.phone)}
              </option>
            ))}
          </select>
        </label>
        <span className="spacer" />
        <button
          className="btn small danger"
          disabled={logs.length === 0}
          onClick={async () => {
            const acc = accounts.find((a) => a.id === accountId);
            const tip = acc
              ? {
                  title: "清空日志",
                  body: `确认清空「${accountLabel(acc.name, acc.phone)}」的全部签到日志？`,
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
              await clearCheckinLogs(accountId || undefined);
              onToast({ kind: "ok", text: "日志已清空" });
              refresh();
            } catch (e) {
              onToast({ kind: "err", text: "清空失败：" + String(e) });
            }
          }}
        >
          清空日志
        </button>
      </div>

      {loading ? (
        <p>加载中…</p>
      ) : logs.length === 0 ? (
        <p className="empty">暂无签到记录。</p>
      ) : (
        <ul className="log-list page-list">
          {logs.map((l) => (
            <li key={l.id} className="log-item">
              <div className="log-head">
                <LogBadge log={l} />
                <span className="log-name">
                  {accountLabel(l.account_name, l.account_phone)}
                </span>
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
    </section>
  );
}
