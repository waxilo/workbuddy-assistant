import { useEffect, useState } from "react";
import type { NetReport, NetStep } from "../types";
import { netDiagnose, netRestore, revealPath } from "../api";
import { baseName } from "../common";
import type { ConfirmReq, Toast } from "../common";

/**
 * 「网络急救」页：从设置里独立出来——它排查的是 WorkBuddy 全局网络配置，
 * 与签到设置无关；而且出问题时用户需要**直接找到它**，不该藏在设置深处。
 */
export function NetfixPage({
  askConfirm,
  onReloadSettings,
  onToast,
}: {
  askConfirm: (opts: Omit<ConfirmReq, "resolve">) => Promise<boolean>;
  /** 恢复/关闭反代后让外层状态跟上（后端 stealth_stop/netRestore 都会落盘） */
  onReloadSettings: () => Promise<void>;
  onToast: (t: Toast) => void;
}) {
  const [netReport, setNetReport] = useState<NetReport | null>(null);
  const [netSteps, setNetSteps] = useState<NetStep[] | null>(null);
  const [netBackups, setNetBackups] = useState<string[]>([]);
  const [netBusy, setNetBusy] = useState<"diag" | "restore" | null>(null);

  const doDiagnose = async () => {
    setNetBusy("diag");
    try {
      setNetReport(await netDiagnose());
      // 上一次的恢复轨迹已经过时了，清掉免得和新的诊断结果混在一起
      setNetSteps(null);
      setNetBackups([]);
    } catch (e) {
      onToast({ kind: "err", text: "诊断失败：" + String(e) });
    } finally {
      setNetBusy(null);
    }
  };

  const doRestore = async () => {
    const ok = await askConfirm({
      title: "一键恢复网络配置",
      body:
        "会依次：安全关闭无感接管并重启 WorkBuddy/CLI host、备份并清除全局调试端点、" +
        "取消 launchd 全局环境变量，最后复检。被改动的文件都会先备份。继续？",
      okText: "恢复",
      danger: true,
    });
    if (!ok) return;
    setNetBusy("restore");
    try {
      const rep = await netRestore();
      // 接管可能是这次恢复关掉的（后端已落盘 proxy_enabled=false），外层状态要跟上
      await onReloadSettings();
      setNetReport(rep.report);
      setNetSteps(rep.steps);
      setNetBackups(rep.backups);
      const failed = rep.steps.filter((s) => !s.ok).length;
      onToast(
        failed
          ? { kind: "err", text: `恢复完成，但有 ${failed} 步失败，见下方详情` }
          : { kind: "ok", text: "已恢复：残留配置已清除" }
      );
    } catch (e) {
      onToast({ kind: "err", text: "恢复失败：" + String(e) });
    } finally {
      setNetBusy(null);
    }
  };

  useEffect(() => {
    // 进入页面就扫一遍（只读）：有问题不用先点「诊断」
    void doDiagnose();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <section className="panel-page">
      <p className="hint">
        调试「自定义服务端点」时，如果把地址写进了 WorkBuddy 的
        <strong>全局</strong>配置（<code>~/.workbuddy/settings.json</code> 的 <code>env</code>，
        或 launchd 全局环境变量），受影响的会是<strong>整个 WorkBuddy</strong>（含正在运行的桌面端），
        典型表现是「502 连接被拒绝」。这里可以扫出这些残留并一键清掉。
      </p>
      <div className="opt-row">
        <button
          className="btn small"
          disabled={netBusy !== null}
          onClick={() => void doDiagnose()}
        >
          {netBusy === "diag" ? "扫描中…" : "重新诊断"}
        </button>
        <button
          className="btn small danger"
          disabled={netBusy !== null}
          onClick={() => void doRestore()}
        >
          {netBusy === "restore" ? "恢复中…" : "一键恢复（含关闭反代）"}
        </button>
      </div>

      {netReport &&
        (netReport.healthy && netReport.issues.length === 0 ? (
          <p className="net-ok">✓ 未发现残留端点配置，WorkBuddy 的网络链路是干净的。</p>
        ) : (
          <>
            <ul className="net-list">
              {netReport.issues.map((it) => (
                <li key={it.id} className={`net-item ${it.level}`}>
                  <div className="net-head">
                    <span
                      className={`badge ${
                        it.level === "block"
                          ? "badge-err"
                          : it.level === "ok"
                          ? "badge-ok"
                          : "badge-already"
                      }`}
                    >
                      {it.level === "block"
                        ? "会断网"
                        : it.level === "ok"
                        ? "正常"
                        : "残留"}
                    </span>
                    <span className="net-scope">{it.scope}</span>
                    <span className="net-target">{it.target}</span>
                  </div>
                  <div className="net-value">{it.value}</div>
                  <div className="net-note">
                    {it.note}
                    {!it.fixable && "（此项只报告，需要你手动处理）"}
                  </div>
                </li>
              ))}
            </ul>
            {netReport.issues.some((i) => i.fixable && i.level !== "ok") && (
              <p className="hint">
                「一键恢复」会清除上表中标记为可自动处理的项；改动前一律先备份。
              </p>
            )}
          </>
        ))}

      {netSteps && (
        <>
          <ul className="net-steps">
            {netSteps.map((s, i) => (
              <li key={`${i}-${s.action}`} className={`net-step ${s.ok ? "ok" : "bad"}`}>
                <span className="mark">{s.ok ? "✓" : "✕"}</span>
                <span>
                  <strong>{s.action}</strong>：{s.detail}
                </span>
              </li>
            ))}
          </ul>
          {netBackups.length > 0 && (
            <div className="net-backups">
              <span>已备份：</span>
              {netBackups.map((b) => (
                <button
                  key={b}
                  className="link-btn"
                  title={b}
                  onClick={() =>
                    void revealPath(b).catch((e) =>
                      onToast({ kind: "err", text: String(e) })
                    )
                  }
                >
                  {baseName(b)}
                </button>
              ))}
            </div>
          )}
        </>
      )}
    </section>
  );
}
