import { useCallback, useEffect, useState } from "react";
import type { CreditReport, CreditReportAccount } from "../types";
import { clearCreditReports, creditReports, settleCreditReport } from "../api";
import { AccountCell, EmptyState, formatCredits } from "../common";
import type { ConfirmReq, Toast } from "../common";
import {
  IconActivity,
  IconInfo,
  IconRefresh,
  IconTrash,
} from "../components/Icons";

/** 一天的 24 个小时标签，按 0 点补零，保证条形图与刻度对齐 */
const HOURS = Array.from({ length: 24 }, (_, h) => String(h).padStart(2, "0"));

/** 参与小时条形的最大项（消耗与新增取大者），全为 0 时返回 0 */
function peakOf(values: number[]): number {
  return values.reduce((m, v) => (v > m ? v : m), 0);
}

/**
 * 「积分日报」页：**按自然日**结算，每天一条。
 *
 * 后端在每次采样时就把「与上次采样相比的增量」记进**采样时刻所属的小时**，
 * 所以「按天」与「按小时」是同一份数据的两种聚合 —— 小时之和恒等于当天合计。
 * 这一层只做展示，不做任何口径计算。
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
      // 结算会重新聚合当天并覆盖同一天的那条，所以直接重拉列表最稳
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
      body: "确认清空全部日报历史？积分台账与已记录的每小时数据会保留，后续统计不受影响。",
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
        按自然日统计（00:00–24:00），每天 12:00 出当天的数字，次日自动补齐全天。
        展开任意一天可以看到**每小时**的消耗与新增。消耗与新增都按资源包的累计量取差值，
        所以多个客户端同时消耗也都能统计到；应用没运行的时段没有采样，那一格就是空的。
      </p>

      <div className="card logs-toolbar">
        <span className="count">
          {loading ? "加载中…" : `共 ${reports.length} 天`}
        </span>
        <span className="spacer" />
        <button
          className="btn small"
          disabled={busy}
          title="把今天 00:00 到现在重新结算一次（同一天会覆盖更新）"
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
          hint="到点会自动结算；想现在就看看，点上方「立即结算」。首次结算只建立基线，消耗与新增从那时起按小时累计。"
        />
      ) : (
        <>
          <div className="summary">
            <div className="sum-card card hoverable">
              <span className="sum-label">
                今天消耗{latest?.sealed ? "（已封口）" : "（至今）"}
              </span>
              <span className="sum-num rp-consumed">
                {formatCredits(latest?.total_consumed ?? null)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">
                今天新增{latest?.sealed ? "（已封口）" : "（至今）"}
              </span>
              <span className="sum-num ok">
                {formatCredits(latest?.total_gained ?? null)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">累计消耗（{reports.length} 天）</span>
              <span className="sum-num rp-consumed">
                {formatCredits(lifetime.consumed)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">累计新增（{reports.length} 天）</span>
              <span className="sum-num ok">{formatCredits(lifetime.gained)}</span>
            </div>
          </div>

          <div className="report-list">
            {reports.map((r) => {
              const open = openDate === r.date;
              return (
                <article className="card report" key={r.date}>
                  <button
                    className="report-head"
                    aria-expanded={open}
                    onClick={() => setOpenDate(open ? null : r.date)}
                  >
                    <span className="report-date">{r.date}</span>
                    <span className="report-window">
                      {r.sealed ? "全天" : "至今"}
                      {r.granularity === 1 && " · 小时明细不可用"}
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

                  {open && <ReportDetail rep={r} />}
                </article>
              );
            })}
          </div>
        </>
      )}
    </section>
  );
}

/** 展开内容：每小时条形图 + 每账号明细（可再展开看该账号的逐小时） */
function ReportDetail({ rep }: { rep: CreditReport }) {
  /** 展开逐小时明细的账号 id（"" 表示都没展开） */
  const [openAcct, setOpenAcct] = useState<string | null>(null);
  const hasHourly = rep.hours.length > 0;
  const peak = peakOf(rep.hours.flatMap((h) => [h.consumed, h.gained]));

  return (
    <>
      {hasHourly && (
        <div className="hour-block">
          <div className="hour-head">
            <span className="hour-title">每小时</span>
            <span className="hour-legend">
              <i className="lg-dot rp-consumed-bg" />
              消耗
              <i className="lg-dot rp-gained-bg" />
              新增
            </span>
            <span className="hour-peak">峰值 {formatCredits(peak)}</span>
          </div>
          <HourChart hours={rep.hours} peak={peak} />
          <div className="hour-axis">
            {HOURS.map((h, i) => (
              // 每 3 小时标一次，避免 24 个刻度挤在一起
              <span key={h} className="hour-tick">
                {i % 3 === 0 ? h : ""}
              </span>
            ))}
          </div>
          <p className="hour-hint">
            只统计应用运行期间；没有采样的时段为空。「每小时」与上面合计同源，
            24 格相加等于当天合计。
          </p>
        </div>
      )}

      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <tr>
              <th>账号</th>
              <th className="num">消耗</th>
              <th className="num">新增</th>
              <th className="num">剩余</th>
              <th className="num">资源包</th>
              <th className="num">小时</th>
            </tr>
          </thead>
          <tbody>
            {rep.accounts.map((a) => {
              const open = openAcct === a.account_id;
              const canOpen = !rep.granularity && hasHourly;
              return (
                <>
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
                    <td className="num">
                      {canOpen ? (
                        <button
                          className="btn small hour-toggle"
                          aria-expanded={open}
                          onClick={() =>
                            setOpenAcct(open ? null : a.account_id)
                          }
                        >
                          {open ? "收起" : "展开"}
                        </button>
                      ) : (
                        <span className="muted">—</span>
                      )}
                    </td>
                  </tr>
                  {open && (
                    <tr key={`${a.account_id}-hours`} className="hour-row">
                      <td colSpan={6}>
                        <AcctHours acct={a} />
                      </td>
                    </tr>
                  )}
                </>
              );
            })}
          </tbody>
        </table>
      </div>
    </>
  );
}

/**
 * 单账号的逐小时明细。
 *
 * 用「小时 : 消耗 / 新增」的文字列表而非第二张条形图 —— 这里要看的是**精确数值**
 * （例如「21 点到底花了多少」），条形图只适合看形状。
 */
function AcctHours({ acct }: { acct: CreditReportAccount }) {
  const rows = Array.from({ length: 24 }, (_, h) => ({
    hour: h,
    consumed: acct.hours_consumed[h] ?? 0,
    gained: acct.hours_gained[h] ?? 0,
  })).filter((r) => r.consumed > 0 || r.gained > 0);

  if (rows.length === 0) {
    return <p className="hour-hint">这一天该账号没有按小时记录到变化。</p>;
  }
  return (
    <ul className="acct-hours">
      {rows.map((r) => (
        <li key={r.hour}>
          <span className="ah-hour">{HOURS[r.hour]}:00</span>
          <span className="ah-consumed rp-consumed">
            {r.consumed > 0 ? formatCredits(r.consumed) : "—"}
          </span>
          <span className="ah-gained rp-gained">
            {r.gained > 0 ? "+" + formatCredits(r.gained) : "—"}
          </span>
        </li>
      ))}
    </ul>
  );
}

/**
 * 24 小时的双色柱状图。
 *
 * 用纯 CSS 柱（高度 = 值 / 峰值）而不是引图表库：一屏 24 根柱子、
 * 数据只来自本地 IPC，为它引入一个图表依赖不划算。
 * 每格用两个并排的细柱表示消耗与新增，各自独立按峰值归一。
 */
function HourChart({
  hours,
  peak,
}: {
  hours: CreditReport["hours"];
  peak: number;
}) {
  const byHour = new Map(hours.map((h) => [h.hour, h]));
  return (
    <div className="hour-chart" role="img" aria-label="每小时消耗与新增">
      {HOURS.map((label, h) => {
        const item = byHour.get(h);
        const c = item?.consumed ?? 0;
        const g = item?.gained ?? 0;
        const pct = (v: number) => (peak > 0 ? (v / peak) * 100 : 0);
        return (
          <div
            className="hour-col"
            key={label}
            title={
              item
                ? `${label}:00　消耗 ${c}　新增 ${g}`
                : `${label}:00　无记录`
            }
          >
            <div className="hour-bars">
              <i
                className="bar bar-consumed"
                style={{ height: `${pct(c)}%` }}
              />
              <i className="bar bar-gained" style={{ height: `${pct(g)}%` }} />
            </div>
          </div>
        );
      })}
    </div>
  );
}
