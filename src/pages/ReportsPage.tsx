import { useCallback, useEffect, useState } from "react";
import type {
  CreditReport,
  CreditReportAccount,
  CreditSnapshot,
  SnapshotDiff,
} from "../types";
import {
  clearCreditReports,
  clearCreditSnapshots,
  creditReports,
  creditSnapshotDiffs,
  creditSnapshots,
  settleCreditReport,
} from "../api";
import { AccountCell, EmptyState, formatCredits } from "../common";
import type { ConfirmReq, Toast } from "../common";
import { Dialog } from "../components/Dialog";
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

/** 把秒数写成「2 小时 15 分」这类人话，用于两条快照之间的时间跨度 */
function spanText(sec: number): string {
  if (sec <= 0) return "同一时刻";
  const d = Math.floor(sec / 86400);
  const h = Math.floor((sec % 86400) / 3600);
  const m = Math.floor((sec % 3600) / 60);
  if (d > 0) return `${d} 天 ${h} 小时`;
  if (h > 0) return m > 0 ? `${h} 小时 ${m} 分` : `${h} 小时`;
  if (m > 0) return `${m} 分`;
  return "不到 1 分";
}

/**
 * 「积分日报」页：列表里**只有完整自然日**（00:00–24:00），每天一条。
 *
 * 后端在每次采样时就把「与上次采样相比的增量」记进**采样时刻所属的小时**，
 * 所以「按天」与「按小时」是同一份数据的两种聚合 —— 小时之和恒等于当天合计。
 * 这一层只做展示，不做任何口径计算。
 *
 * 列表里的日报由**次日结算**产生（结算时刻固定 24:00，即次日首次运行时把昨天封口），
 * 因此每条都是完整一天、任意两条都能直接相加。
 *
 * 快照是另一个东西：**某一刻的读数**，单独一块区域展示，**不进日报合计**。
 * 它回答的是「我刚点的这一枪，比上回多了多少」—— 日报答不了（日报只按天给）。
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
  /** 临时快照（「当前累计」的结果）。非 null 时弹出查看 */
  const [snapshot, setSnapshot] = useState<CreditReport | null>(null);
  /** 落盘的快照列表（含系统锚点）与它们两两之间的增量 */
  const [snaps, setSnaps] = useState<CreditSnapshot[]>([]);
  const [diffs, setDiffs] = useState<Map<number, SnapshotDiff>>(new Map());

  const refresh = useCallback(() => {
    setLoading(true);
    creditReports()
      .then(setReports)
      .catch(() => setReports([]))
      .finally(() => setLoading(false));
  }, []);

  /** 快照区单独刷新：它和日报是两份文件，互不影响 */
  const refreshSnaps = useCallback(() => {
    creditSnapshots()
      .then(setSnaps)
      .catch(() => setSnaps([]));
    creditSnapshotDiffs()
      .then((pairs) => setDiffs(new Map(pairs)))
      .catch(() => setDiffs(new Map()));
  }, []);

  useEffect(() => {
    refresh();
    refreshSnaps();
  }, [refresh, refreshSnaps]);

  const latest = reports[0] ?? null;
  // 「累计」= 已保留的全部日报之和。只含完整自然日，所以直接相加就是总量
  const lifetime = reports.reduce(
    (acc, r) => ({
      consumed: acc.consumed + r.total_consumed,
      gained: acc.gained + r.total_gained,
    }),
    { consumed: 0, gained: 0 }
  );

  /**
   * 取一次当前累计读数并弹窗展示。
   *
   * 后端顺带把它落成一条 `manual` 快照，所以这里也要刷新快照区 ——
   * 用户点完按钮会立刻在下方看到新的一行。
   */
  const doSnapshot = async () => {
    setBusy(true);
    try {
      setSnapshot(await settleCreditReport());
      refreshSnaps();
    } catch (e) {
      onToast({ kind: "err", text: "读取当前累计失败：" + String(e) });
    } finally {
      setBusy(false);
    }
  };

  const doClear = async () => {
    const ok = await askConfirm({
      title: "清空积分日报",
      body: "确认清空全部日报历史？积分台账、已记录的每小时数据与快照都会保留，后续统计不受影响。",
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

  const doClearSnaps = async () => {
    const ok = await askConfirm({
      title: "清空快照",
      body: "确认清空全部快照？日报历史与积分台账会保留。清空后就没有可比对的历史读数了。",
      okText: "清空",
      danger: true,
    });
    if (!ok) return;
    try {
      await clearCreditSnapshots();
      setSnaps([]);
      setDiffs(new Map());
      onToast({ kind: "ok", text: "快照已清空" });
    } catch (e) {
      onToast({ kind: "err", text: "清空失败：" + String(e) });
    }
  };

  return (
    <section className="panel-page">
      <p className="set-intro">
        <IconInfo size={14} />
        <span>
          按自然日统计（00:00–24:00）。结算时刻固定 24:00，即次日首次打开应用时把昨天
          整天算完 —— 所以列表里每条都是<b>完整一天</b>，任意两条都能直接相加。展开任意一天
          可以看到<b>每小时</b>的消耗与新增。消耗与新增都按资源包的累计量取差值，所以多个
          客户端同时消耗也都能统计到；应用没运行的时段没有采样，那一格就是空的。
        </span>
      </p>

      <div className="card logs-toolbar">
        <span className="count">
          {loading ? "加载中…" : `共 ${reports.length} 天`}
        </span>
        <span className="spacer" />
        <button
          className="btn small"
          disabled={busy}
          title="读取「今天 00:00 到现在」的累计，弹窗查看并记成一条快照（不进日报合计）"
          onClick={() => void doSnapshot()}
        >
          <IconRefresh size={15} className={busy ? "spin" : undefined} />
          {busy ? "读取中…" : "当前累计"}
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
          hint="每条日报都在次日首次打开应用时生成（结算时刻固定 24:00）。想现在就看当前累计，点上方「当前累计」。"
        />
      ) : (
        <>
          <div className="summary">
            <div className="sum-card card hoverable">
              <span className="sum-label">
                最近一天消耗{latest ? `（${latest.date}）` : ""}
              </span>
              <span className="sum-num rp-consumed">
                {formatCredits(latest?.total_consumed ?? null)}
              </span>
            </div>
            <div className="sum-card card hoverable">
              <span className="sum-label">
                最近一天新增{latest ? `（${latest.date}）` : ""}
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

      <SnapshotSection
        snaps={snaps}
        diffs={diffs}
        onClear={() => void doClearSnaps()}
      />

      {snapshot && (
        <SnapshotDialog
          rep={snapshot}
          snaps={snaps}
          onClose={() => setSnapshot(null)}
        />
      )}
    </section>
  );
}

/**
 * 快照区：某一刻的读数 + 与**上一条**相比的增量。
 *
 * 独立于日报列表，原因有两层：
 * 1. **口径不同**。日报是「一天一条的聚合」，快照是「某一刻的累计读数」，
 *    混进同一个列表会让同日出现两个值（此刻 vs 全天），相加还会重复计数；
 * 2. **留存策略不同**。日报按天累积（可留几百天），快照靠系统锚点压制数量。
 *
 * 这里**不显示合计**：快照是累计量，把几条加起来毫无意义（会重复计同一段时间）。
 * 真正有信息量的是每行右侧那个「较上一条 +N」。
 */
function SnapshotSection({
  snaps,
  diffs,
  onClear,
}: {
  snaps: CreditSnapshot[];
  diffs: Map<number, SnapshotDiff>;
  onClear: () => void;
}) {
  const hasSystem = snaps.some((s) => s.kind === "system");
  return (
    <div className="card snap-block">
      <div className="snap-head">
        <span className="snap-title">快照</span>
        <span className="snap-sub">
          {snaps.length === 0 ? "还没有快照" : `共 ${snaps.length} 条`}
        </span>
        <span className="spacer" />
        <button
          className="btn small danger"
          disabled={snaps.length === 0}
          onClick={onClear}
        >
          <IconTrash size={15} />
          清空快照
        </button>
      </div>

      {snaps.length === 0 ? (
        <p className="snap-note">
          <IconInfo size={13} />
          <span>
            点上方「当前累计」会记下一条快照。攒够两条后，这里就能看到
            <b>这一段时间到底消耗了多少</b> —— 日报按天给，快照能按你点的那一下给。
          </span>
        </p>
      ) : (
        <>
          <div className="snap-list">
            {snaps.map((s, i) => {
              const d = diffs.get(i);
              return (
                <div className="snap-row" key={`${s.at}-${i}`}>
                  <span className={"snap-tag " + s.kind}>
                    {s.kind === "system" ? "系统" : "手动"}
                  </span>
                  <span className="snap-at">{s.at}</span>
                  <span className="snap-bal">
                    剩余 {s.balance == null ? "—" : formatCredits(s.balance)}
                  </span>
                  <span className="snap-pts">
                    累计消耗 {formatCredits(s.consumed)} · 新增{" "}
                    {formatCredits(s.gained)}
                  </span>
                  <span className="snap-delta">
                    {d ? (
                      <>
                        <span className="sd-span">
                          较上一条 {spanText(d.span_seconds)}
                        </span>
                        <span className="sd-num rp-consumed">
                          耗 {formatCredits(d.consumed)}
                        </span>
                        <span className="sd-num rp-gained">
                          增 +{formatCredits(d.gained)}
                        </span>
                      </>
                    ) : (
                      <span className="sd-first">首个样本</span>
                    )}
                  </span>
                </div>
              );
            })}
          </div>
          <p className="snap-note">
            <IconInfo size={13} />
            <span>
              快照记的是<b>累计读数</b>，两条相减才是这段时间的真实增量 ——
              所以它<b>不参与</b>上面的日报合计。
              {hasSystem
                ? " 有系统快照（次日封口）后，手动快照只保留最新一条。"
                : " 目前没有系统快照，手动快照会累积，方便你多打几个点对比。"}
            </span>
          </p>
        </>
      )}
    </div>
  );
}

/**
 * 「当前累计」快照弹窗：展示**今天 00:00 到此刻**的数字。
 *
 * 复用 [`ReportDetail`] 的逐小时视图，因此快照也能看到今天各小时的分布。
 * 后端已把这次读数落成一条 `manual` 快照，关掉弹窗不会丢（在页面下方可对比）。
 */
function SnapshotDialog({
  rep,
  snaps,
  onClose,
}: {
  rep: CreditReport;
  snaps: CreditSnapshot[];
  onClose: () => void;
}) {
  // 找上一条手动快照作参照，直接在这里回答「比上次多用了多少」
  const prev = snaps.find(
    (s) => s.kind === "manual" && s.at !== rep.generated_at
  );
  return (
    <Dialog className="wide" label="当前累计快照" onClose={onClose}>
      <h3 className="modal-title">当前累计（今天 00:00 至此刻）</h3>
      <p className="snap-note">
        <IconInfo size={13} />
        <span>
          这次读数已记成一条<b>手动快照</b>（页面下方可对比）。它<b>不会</b>
          写进上面的日报列表 —— 那里每条都是由次日结算产生的完整自然日。
        </span>
      </p>

      <div className="summary snap-summary">
        <div className="sum-card card">
          <span className="sum-label">消耗</span>
          <span className="sum-num rp-consumed">
            {formatCredits(rep.total_consumed)}
          </span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">新增</span>
          <span className="sum-num ok">
            {formatCredits(rep.total_gained)}
          </span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">剩余</span>
          <span className="sum-num">
            {rep.total_balance == null
              ? "—"
              : formatCredits(rep.total_balance)}
          </span>
        </div>
        <div className="sum-card card">
          <span className="sum-label">账号</span>
          <span className="sum-num">{rep.accounts.length}</span>
        </div>
      </div>

      {prev && (
        <p className="snap-when">
          比上一条快照（{prev.at}）多耗{" "}
          <b className="rp-consumed">
            {formatCredits(rep.total_consumed - prev.consumed)}
          </b>
          、多增{" "}
          <b className="rp-gained">
            +{formatCredits(rep.total_gained - prev.gained)}
          </b>
        </p>
      )}

      <p className="snap-when">采样时刻 {rep.generated_at}</p>

      <ReportDetail rep={rep} />

      <div className="modal-actions">
        <button className="btn" onClick={onClose}>
          关闭
        </button>
      </div>
    </Dialog>
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
