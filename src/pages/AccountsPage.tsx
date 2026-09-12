import type { Account } from "../types";
import { ResultBadge, formatCredits, maskToken } from "../common";

/**
 * 首页：账号列表 + 签到操作。
 *
 * 账号**没有**「添加 / 编辑」入口：条目只是给用户看的，凭证一律来自
 * 「登录新账号」（OAuth）或「导入本机账号」（本机登录文件），避免手工粘贴 token 出错。
 */
export function AccountsPage({
  accounts,
  loading,
  busyIds,
  onCheckinOne,
  onRemove,
  onOpenLogs,
  onLoginNew,
  onImportLocal,
}: {
  accounts: Account[];
  loading: boolean;
  busyIds: Set<string>;
  onCheckinOne: (id: string) => void;
  onRemove: (a: Account) => void;
  onOpenLogs: (accountId: string) => void;
  onLoginNew: () => void;
  onImportLocal: () => void;
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
          </div>
        </li>
      ))}
      {/* 空态引导也保留操作入口（列表非空时操作在页头，这里不重复） */}
      {accounts.length > 0 && (
        <li className="ac-more">
          <button className="btn ghost" onClick={onLoginNew}>
            + 登录新账号
          </button>
          <button className="btn ghost" onClick={onImportLocal}>
            + 导入本机账号
          </button>
        </li>
      )}
    </ul>
  );
}
