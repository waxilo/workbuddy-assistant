import { useEffect, useState } from "react";
import type { Settings } from "../types";
import { setAutostart, getAutostart, testNotify } from "../api";
import type { Toast } from "../common";

/**
 * 「设置」页：定时签到 / 通知 / 风控 / 自启动等常规配置。
 *
 * 网络急救与无感接管都已是独立页面，这里不再混排；
 * 接管相关字段（proxy_* / preferred_account_id）本页没有编辑权，保存时原样透传。
 */
export function SettingsPage({
  settings,
  onSave,
  onToast,
}: {
  settings: Settings;
  onSave: (s: Settings) => Promise<void>;
  onToast: (t: Toast) => void;
}) {
  const [auto, setAuto] = useState(settings.auto_checkin_on_start);
  const [schedOn, setSchedOn] = useState(settings.schedule_enabled);
  const [schedTime, setSchedTime] = useState(settings.schedule_time);
  const [notifyOn, setNotifyOn] = useState(settings.notify_enabled);
  const [webhook, setWebhook] = useState(settings.notify_webhook);
  const [notifySched, setNotifySched] = useState(settings.notify_on_schedule);
  const [notifyManual, setNotifyManual] = useState(settings.notify_on_manual);
  // 多账号风控：批量签到时账号之间随机歇几秒，避免同 IP 瞬时连发
  const [staggerOn, setStaggerOn] = useState(settings.stagger_checkin);
  const [staggerMax, setStaggerMax] = useState(String(settings.stagger_max_seconds));
  const [testing, setTesting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  // 开机自启动是「操作系统状态」，不属于 settings.json，改一次立即生效
  const [autostart, setAutostartOn] = useState<boolean | null>(null);
  const [autoBusy, setAutoBusy] = useState(false);

  useEffect(() => {
    getAutostart()
      .then(setAutostartOn)
      .catch(() => setAutostartOn(null));
  }, []);

  const toggleAutostart = async (next: boolean) => {
    setAutoBusy(true);
    setAutostartOn(next); // 乐观更新，失败再回滚
    try {
      setAutostartOn(await setAutostart(next));
      onToast({ kind: "ok", text: next ? "已设置开机自启动" : "已关闭开机自启动" });
    } catch (e) {
      setAutostartOn(!next);
      onToast({ kind: "err", text: String(e) });
    } finally {
      setAutoBusy(false);
    }
  };

  // 默认 Base URL 是内置常量，界面不提供修改入口（改错会让签到打到错误的域）；
  // 接管相关字段本页没有编辑权，原样透传（它们归「无感接管」页管）
  const snapshot = (): Settings => ({
    ...settings,
    auto_checkin_on_start: auto,
    schedule_enabled: schedOn,
    // <input type="time"> 已产出 HH:MM；后端还会再规范化一次
    schedule_time: schedTime.trim(),
    notify_enabled: notifyOn,
    notify_webhook: webhook.trim(),
    notify_on_schedule: notifySched,
    notify_on_manual: notifyManual,
    stagger_checkin: staggerOn,
    stagger_max_seconds: Number(staggerMax) || 45,
  });

  const doTest = async () => {
    setTesting(true);
    try {
      const res = await testNotify(webhook.trim());
      onToast({ kind: "ok", text: "推送成功：" + res });
    } catch (e) {
      onToast({ kind: "err", text: "推送失败：" + String(e) });
    } finally {
      setTesting(false);
    }
  };

  return (
    <section className="panel-page">
      <label className="checkbox">
        <input
          type="checkbox"
          checked={auto}
          onChange={(e) => setAuto(e.target.checked)}
        />
        启动应用时自动签到全部账号
      </label>

      <h3 className="sec">定时自动签到</h3>
      <label className="checkbox">
        <input
          type="checkbox"
          checked={schedOn}
          onChange={(e) => setSchedOn(e.target.checked)}
        />
        每天定时自动签到全部账号
      </label>
      {schedOn && (
        <>
          <div className="opt-col">
            <label>
              签到时刻（24 小时制）
              <input
                type="time"
                value={schedTime}
                onChange={(e) => setSchedTime(e.target.value)}
              />
            </label>
          </div>
          <p className="hint">
            ⚠️ 定时任务只在<strong>应用运行期间</strong>触发（桌面端退出后没有后台进程可代为执行）。
            错过时刻后 30 分钟内打开应用会自动补签一次。
          </p>
        </>
      )}
      <label className="checkbox">
        <input
          type="checkbox"
          checked={autostart === true}
          disabled={autoBusy || autostart === null}
          onChange={(e) => void toggleAutostart(e.target.checked)}
        />
        开机自启动（登录时自动运行本应用）
      </label>
      {autostart === null ? (
        <p className="hint">未能读取开机自启动状态（该平台可能不支持）。</p>
      ) : (
        schedOn &&
        !autostart && (
          <p className="hint">
            建议同时开启「开机自启动」，否则应用不运行时定时签到不会发生。
          </p>
        )
      )}

      <h3 className="sec">多账号风控</h3>
      <label className="checkbox">
        <input
          type="checkbox"
          checked={staggerOn}
          onChange={(e) => setStaggerOn(e.target.checked)}
        />
        批量签到时账号之间加入随机间隔
      </label>
      {staggerOn && (
        <div className="opt-col">
          <label>
            间隔上限（秒）
            <input
              type="number"
              value={staggerMax}
              min={2}
              max={600}
              onChange={(e) => setStaggerMax(e.target.value)}
            />
          </label>
        </div>
      )}
      <p className="hint">
        多个账号在同一台电脑上签到属于同一 IP 的批量请求。开启后，每次批量签到会在账号之间随机等待
        2～上限秒再发下一个，打散请求节奏、降低触发风控的概率；单账号签到不受影响。
      </p>

      <h3 className="sec">签到通知</h3>
      <label className="checkbox">
        <input
          type="checkbox"
          checked={notifyOn}
          onChange={(e) => setNotifyOn(e.target.checked)}
        />
        开启 webhook 通知
      </label>
      {notifyOn && (
        <>
          <label className="wide">
            Webhook 地址
            <input
              value={webhook}
              placeholder="https://…/hook/&lt;key&gt;"
              onChange={(e) => setWebhook(e.target.value)}
            />
          </label>
          <div className="opt-row">
            <button
              className="btn small"
              disabled={testing || !webhook.trim()}
              onClick={() => void doTest()}
            >
              {testing ? "发送中…" : "测试推送"}
            </button>
            <span className="hint inline">点一下会给这个地址发一条测试消息</span>
          </div>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={notifySched}
              onChange={(e) => setNotifySched(e.target.checked)}
            />
            定时签到后推送
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={notifyManual}
              onChange={(e) => setNotifyManual(e.target.checked)}
            />
            手动「全部签到」后推送
          </label>
        </>
      )}

      {err && <p className="form-err">{err}</p>}
      <div className="page-actions">
        <button
          className="btn primary"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            setErr("");
            try {
              await onSave(snapshot());
            } catch (e) {
              setErr(String(e));
              setBusy(false);
            }
          }}
        >
          {busy ? "保存中…" : "保存"}
        </button>
      </div>
    </section>
  );
}
