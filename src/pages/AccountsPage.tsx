import type { Account } from "../types";
import {
  formatCredits,
  maskPhone,
  relativeTime,
  signState,
  expiryInfo,
  type SignState,
} from "../common";
import { IconFile, IconTrash, IconUser, IconRefresh } from "../components/Icons";

const STATUS_LABEL: Record<SignState, string> = {
  signing: "签到中",
  done: "今日已签到",
  pending: "待签到",
  fail: "签到失败",
  inactive: "活动未开",
};

function StatusDot({ state }: { state: SignState }) {
  return (
    <span className={`status-dot ${state}`}>
      <i className="dot" />
      <span>{STATUS_LABEL[state]}</span>
    </span>
  );
}

/**
 * 首页：账号列表 + 签到操作。
 *
 * 账号**没有**「添加 / 编辑」入口：条目只是给用户看的，凭证一律来自
 * 页头「登录新账号」（OAuth）或「导入本机账号」（本机登录文件），避免手工粘贴 token 出错。
 */
export function AccountsPage({
  accounts,
  loading,
  busyIds,
  onCheckinOne,
  onRemove,
  onOpenLogs,
}: {
  accounts: Account[];
  loading: boolean;
  busyIds: Set<string>;
  onCheckinOne: (id: string) => void;
  onRemove: (a: Account) => void;
  onOpenLogs: (accountId: string) => void;
}) {
  if (loading) return <p className="empty">加载中…</p>;
  if (accounts.length === 0)
    return (
      <div className="empty">
        <p>还没有账号。</p>
        <p>
          点「登录新账号」用系统浏览器扫码登录，或点「导入本机账号」直接读取 WorkBuddy
          写在本机的登录信息（自动带上昵称与手机号）。
        </p>
      </div>
    );

  const total = accounts.length;
  const done = accounts.filter((a) => signState(a, false) === "done").length;
  const pending = total - done;
  const rate = total > 0 ? Math.round((done / total) * 100) : 0;

  return (
    <div className="ac-page">
      <div className="summary">
        <div className="sum-card">
          <span className="sum-label">账号总数</span>
          <span className="sum-num">{total}</span>
        </div>
        <div className="sum-card">
          <span className="sum-label">今日已签到</span>
          <span className="sum-num ok">{done}</span>
        </div>
        <div className="sum-card">
          <span className="sum-label">待签到</span>
          <span className="sum-num">{pending}</span>
        </div>
        <div className="sum-card">
          <span className="sum-label">签到成功率</span>
          <span className="sum-num">{rate}%</span>
        </div>
      </div>

      <div className="table-wrap">
        <table className="account-table">
          <thead>
            <tr>
              <th>账号</th>
              <th>剩余积分</th>
              <th>积分到期</th>
              <th>最近签到</th>
              <th>状态</th>
              <th className="col-actions">操作</th>
            </tr>
          </thead>
          <tbody>
            {accounts.map((a) => {
              const busy = busyIds.has(a.id);
              const st = signState(a, busy);
              const low = a.last?.balance != null && a.last.balance < 100;
              const e = expiryInfo(a.credit_snapshot?.earliest_expiry_ms);
              const initial = /^\d/.test(a.name) ? null : a.name.slice(0, 1);
              return (
                <tr key={a.id} className="account-row">
                  <td className="ac-cell-name">
                    <span className="ac-avatar">
                      {initial ? initial : <IconUser size={16} />}
                    </span>
                    <div className="ac-id">
                      <span className="ac-name">{maskPhone(a.name)}</span>
                      {a.phone && <span className="ac-phone">{maskPhone(a.phone)}</span>}
                    </div>
                  </td>
                  <td className="ac-cell-balance">
                    {a.last?.balance != null ? (
                      <span className={"ac-balance" + (low ? " low" : "")}>
                        {formatCredits(a.last.balance)}
                      </span>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td className="ac-cell-expiry">
                    {e.text === "—" ? (
                      <span className="muted">—</span>
                    ) : (
                      <span className={"ac-expiry" + (e.expired ? " expired" : "")}>
                        {e.text}
                      </span>
                    )}
                  </td>
                  <td className="ac-cell-last">
                    {a.last?.at ? (
                      <div className="ac-last">
                        <span className="ac-last-main">{relativeTime(a.last.at)}</span>
                        <span className="ac-last-sub" title={a.last.at}>
                          {a.last.at.slice(11, 16)}
                        </span>
                      </div>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td className="ac-cell-status">
                    <StatusDot state={st} />
                  </td>
                  <td className="ac-cell-actions">
                    <button
                      className="btn btn-sm btn-primary"
                      disabled={busy}
                      onClick={() => onCheckinOne(a.id)}
                    >
                      {busy ? (
                        <>
                          <IconRefresh size={14} className="spin" /> 签到中
                        </>
                      ) : (
                        "签到"
                      )}
                    </button>
                    <button
                      className="icon-btn"
                      title="查看签到日志"
                      onClick={() => onOpenLogs(a.id)}
                    >
                      <IconFile size={16} />
                    </button>
                    <button
                      className="icon-btn icon-danger"
                      title="删除账号"
                      onClick={() => onRemove(a)}
                    >
                      <IconTrash size={16} />
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
