import type { Account } from "../types";
import { ResultBadge, formatCredits, maskPhone } from "../common";

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

  return (
    <table className="account-table">
      <thead>
        <tr>
          <th>账号</th>
          <th>剩余积分</th>
          <th>最近签到</th>
          <th>操作</th>
        </tr>
      </thead>
      <tbody>
        {accounts.map((a) => (
          <tr key={a.id} className="account-row">
            <td className="ac-cell-name">
              <div className="ac-title">
                <span className="ac-name">{maskPhone(a.name)}</span>
                {a.phone && <span className="ac-phone">{maskPhone(a.phone)}</span>}
              </div>
              <ResultBadge last={a.last} />
            </td>
            <td className="ac-cell-balance">
              {a.last?.balance != null ? (
                <span className="ac-balance">{formatCredits(a.last.balance)}</span>
              ) : (
                <span className="muted">—</span>
              )}
            </td>
            <td className="ac-cell-last">
              {a.last?.at && <span>{a.last.at}</span>}
              {a.last?.streak != null && (
                <span className="muted">连续 {a.last.streak} 天</span>
              )}
              {a.last?.credit != null && (
                <span className="ac-credit">+{a.last.credit}</span>
              )}
              {/* 「今日已签」的文案与徽标重复，不再展示；失败原因仍要显示 */}
              {a.last?.message && !a.last.already && (
                <span className="msg">{a.last.message}</span>
              )}
            </td>
            <td className="ac-cell-actions">
              <button
                className="btn small"
                disabled={busyIds.has(a.id)}
                onClick={() => onCheckinOne(a.id)}
              >
                {busyIds.has(a.id) ? "…" : "签到"}
              </button>
              <button
                className="btn small ghost"
                onClick={() => onOpenLogs(a.id)}
              >
                日志
              </button>
              {/* 续签不提供手动按钮：签到前与后台扫描会自动完成（剩余有效期不足 48h） */}
              <button className="btn small danger" onClick={() => onRemove(a)}>
                删除
              </button>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
