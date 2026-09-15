import { useCallback, useEffect, useState } from "react";
import type { CreditReport } from "../types";
import { clearCreditReports, creditReports, settleCreditReport } from "../api";
import { AccountCell, EmptyState, formatCredits } from "../common";
import type { ConfirmReq, Toast } from "../common";
import {
  IconActivity,
  IconInfo,
  IconRefresh,
  IconTrash,
} from "../components/Icons";

/**
 * 「积分日报」页：应用常驻时每天（默认 12:00）自动结算一条，
 * 记录窗口内的**消耗**（花掉的）、**新增**（拿到的）以及每个账号的明细。
 *
 * 口径不在这一层：消耗 / 新增都取自资源包的**累计**字段增量（见后端 `ledger` 模块），
 * 所以多个客户端同时消耗也都能算进来 —— 不需要按请求归因，并发也不会算错。
 * 这里只负责把后端算好的结果排开放。
 */
export function ReportsPage({
  askConfirm,
  onToast,
}: {
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  onToast: (t: Toast) => void;
}) {
  const [reports, setReports] = useState<CreditReport[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  /** 展开的日报日期；一次只展开一条，避免长列表被撑得找不到北 */
  const [openDate, setOpenDate] = useState<string | null>(null);

  const refresh = useCallback(() => {
    setLoading(true);
    creditReports()
      .then(setReports)
      .catch(() => setReports([]))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const latest = reports[0] ?? null;
  // 「累计」= 已保留的全部日报之和。注意这是「自开始统计以来」，不是账号的生命周期总量
  const lifetime = reports.reduce(
    (acc, r) => ({
      consumed: acc.consumed + r.total_consumed,
      gained: acc.gained + r.total_gained,
    }),
    { consumed: 0, gained: 0 }
  );

  const doSettle = async () => {
    setBusy(true);
    try {
      const rep = await settleCreditReport();
      // 结算会推进基线，所以直接重拉列表最稳（顺序与截断都由后端决定）
      setReports(await creditReports());
      setOpenDate(rep.date);
      onToast({
        kind: "ok",
        text: `已结算 ${rep.date}：消耗 ${formatCredits(rep.total_consumed)}、新增 ${formatCredits(rep.total_gained)}`,
      });
    } catch (e) {
      onToast({ kind: "err", text: "结算失败：" + String(e) });
    } finally {
      setBusy(false);
    }
  };

  const doClear = async () => {
    const ok = await askConfirm({
      title: "清空积分日报",
      body: "确认清空全部日报历史？积分台账与结算基线会保留，不影响后续统计。",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearCreditReports();
      setReports([]);
      onToast({ kind: "ok", text: "日报已清空" });
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
    }
  };

  return (
    <section className="panel-page">
      <p className="set-intro">
        <IconInfo size={14} />
        每天 12:00 自动结算一次「上次结算到现在」的消耗与新增；应用未运行时不会结算，
        下一次会把这段空档一起算进来。消耗与新增都按资源包的累计量取差值，
        所以多个客户端同时消耗也都能统计到。
      </p>

      <div className="card logs-toolbar">
        <span className="count">
          {loading ? "加载中…" : `共 ${reports.length} 条日报`}
        </span>
        <span className="spacer" />
        <button
          className="btn small"
          disabled={busy}
          title="把「上次结算到现在」的消耗与新增立刻结算成一条日报"
          onClick={() => void doSettle()}
        >
          <IconRefresh size={15} className={busy ? "spin" : undefined} />
          {busy ? "结算中…" : "立即结算"}
        </button>
        <button
          className="btn small danger"
          disabled={busy || reports.length === 0}
          onClick={() => void doClear()}
        >
          <IconTrash size={15} />
          清空日报
        </button>
      </div>

      {loading ? (
        <p className="empty">加载中…</p>
      ) : reports.length === 0 ? (
        <EmptyState
          icon={<IconActivity size={26} />}
          title="还没有日报"
          hint="到点会自动结算一条；想现在就看看，点上方「立即结算」。首次结算只建立基线，消耗与新增会从那时开始累计。"
        />
      ) : (
        <>
          <div className="summary">
            <div className="sum-card card hoverable">
              <span className="sum-label">
                最近消耗{latest ? `（${latest.date}）` : ""}
              </span>
              <span className="sum-num rp-consumed">
                {formatCredits(latest?.total_consumed ?? null)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">
                最近新增{latest ? `（${latest.date}）` : ""}
              </span>
              <span className="sum-num ok">
                {formatCredits(latest?.total_gained ?? null)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">累计消耗（{reports.length} 条）</span>
              <span className="sum-num rp-consumed">
                {formatCredits(lifetime.consumed)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">累计新增（{reports.length} 条）</span>
              <span className="sum-num ok">{formatCredits(lifetime.gained)}</span>
            </div>
          </div>

          <div className="report-list">
            {reports.map((r) => {
              const open = openDate === r.date;
              return (
                <article className="card report" key={`${r.date}-${r.generated_at}`}>
                  <button
                    className="report-head"
                    aria-expanded={open}
                    onClick={() => setOpenDate(open ? null : r.date)}
                  >
                    <span className="report-date">{r.date}</span>
                    <span className="report-window">
                      {r.window_from.slice(5, 16)} → {r.window_to.slice(5, 16)}
                    </span>
                    <span className="spacer" />
                    <span className="report-metric">
                      <span className="rm-label">消耗</span>
                      <span className="rm-num rp-consumed">
                        {formatCredits(r.total_consumed)}
                      </span>
                    </span>
                    <span className="report-metric">
                      <span className="rm-label">新增</span>
                      <span className="rm-num ok">
                        {formatCredits(r.total_gained)}
                      </span>
                    </span>
                    <span className="report-metric">
                      <span className="rm-label">剩余</span>
                      <span className="rm-num">
                        {r.total_balance == null
                          ? "—"
                          : formatCredits(r.total_balance)}
                      </span>
                    </span>
                    <span
                      className={"report-caret" + (open ? " open" : "")}
                      aria-hidden="true"
                    />
                  </button>

                  {open && (
                    <div className="table-wrap">
                      <table className="data-table">
                        <thead>
                          <tr>
                            <th>账号</th>
                            <th className="num">消耗</th>
                            <th className="num">新增</th>
                            <th className="num">剩余</th>
                            <th className="num">资源包</th>
                          </tr>
                        </thead>
                        <tbody>
                          {r.accounts.map((a) => (
                            <tr key={a.account_id}>
                              <td>
                                <AccountCell name={a.name} phone={a.phone} />
                              </td>
                              <td className="num">
                                {a.consumed > 0 ? (
                                  <span className="rp-consumed">
                                    {formatCredits(a.consumed)}
                                  </span>
                                ) : (
                                  <span className="muted">—</span>
                                )}
                              </td>
                              <td className="num">
                                {a.gained > 0 ? (
                                  <span className="rp-gained">
                                    +{formatCredits(a.gained)}
                                  </span>
                                ) : (
                                  <span className="muted">—</span>
                                )}
                              </td>
                              <td className="num num-muted">
                                {a.balance == null ? "—" : formatCredits(a.balance)}
                              </td>
                              <td className="num num-muted">{a.packages}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </article>
              );
            })}
          </div>
        </>
      )}
    </section>
  );
}
