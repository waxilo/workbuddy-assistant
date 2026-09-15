import type { Account } from "../types";
import {
  AccountCell,
  EmptyState,
  StatusDot,
  formatCredits,
  relativeTime,
  signState,
  expiryInfo,
  accountCredits,
  totalCredits,
  type SignState,
} from "../common";
import { IconFile, IconTrash, IconUser, IconRefresh } from "../components/Icons";

/** 账号签到状态 → 状态圆点文案（圆点本身的视觉由 common 的 StatusDot 统一提供） */
const STATUS_LABEL: Record<SignState, string> = {
  signing: "签到中",
  done: "今日已签到",
  pending: "待签到",
  fail: "签到失败",
  inactive: "活动未开",
};

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
  if (loading)
    return (
      <section className="panel-page">
        <p className="empty">加载中…</p>
      </section>
    );
  if (accounts.length === 0)
    return (
      <section className="panel-page">
        <EmptyState
          icon={<IconUser size={26} />}
          title="还没有账号"
          hint="点页头「登录新账号」用系统浏览器扫码登录，或点「导入本机账号」直接读取 WorkBuddy 写在本机的登录信息（自动带上昵称与手机号）。"
        />
      </section>
    );

  const total = accounts.length;
  const done = accounts.filter((a) => signState(a, false) === "done").length;
  const pending = total - done;
  const credits = totalCredits(accounts);

  return (
    <section className="panel-page">
      <div className="summary">
        <div className="sum-card card">
          <span className="sum-label">账号总数</span>
          <span className="sum-num">{total}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">今日已签到</span>
          <span className="sum-num ok">{done}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">待签到</span>
          <span className="sum-num">{pending}</span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">总积分</span>
          <span className="sum-num">{formatCredits(credits)}</span>
        </div>
      </div>

      <div className="table-wrap card">
        <table className="data-table">
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
              const bal = accountCredits(a);
              const low = bal != null && bal < 100;
              const e = expiryInfo(a.credit_snapshot?.earliest_expiry_ms);
              return (
                <tr key={a.id}>
                  <td>
                    <AccountCell name={a.name} phone={a.phone} />
                  </td>
                  <td className="num">
                    {bal != null ? (
                      <span className={"ac-balance" + (low ? " low" : "")}>
                        {formatCredits(bal)}
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
                  <td>
                    <StatusDot tone={st} label={STATUS_LABEL[st]} />
                  </td>
                  <td className="ac-cell-actions">
                    <button
                      className="btn small primary"
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
    </section>
  );
}
