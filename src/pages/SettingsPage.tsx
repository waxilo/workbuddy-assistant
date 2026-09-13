import { useEffect, useState } from "react";
import type { Settings } from "../types";
import { setAutostart, getAutostart, testNotify } from "../api";
import type { Toast } from "../common";

/** 开关控件：复用全局 .switch 样式（与智能接管页一致） */
function Toggle({
  checked,
  disabled,
  onChange,
  title,
}: {
  checked: boolean;
  disabled?: boolean;
  onChange: (v: boolean) => void;
  title?: string;
}) {
  return (
    <label className="switch" title={title}>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span className="track">
        <span className="thumb" />
      </span>
    </label>
  );
}

/** 一行设置：左侧标题 + 说明，右侧控件 */
function Row({
  title,
  desc,
  ctrl,
  sub,
  bare,
}: {
  title: string;
  desc?: string;
  ctrl: React.ReactNode;
  /** 嵌套行：带浅色背景，视觉上从属于上一项开关 */
  sub?: boolean;
  /** 展开区内部的行：不显示分隔线 */
  bare?: boolean;
}) {
  return (
    <div className={`set-row${sub ? " sub" : ""}${bare ? " in-expand" : ""}`}>
      <div className="set-row-main">
        <div className="set-row-title">{title}</div>
        {desc && <div className="set-row-desc">{desc}</div>}
      </div>
      <div className="set-row-ctrl">{ctrl}</div>
    </div>
  );
}

/**
 * 「设置」页：定时签到 / 通知 / 风控 / 自启动等常规配置。
 *
 * 卡片式分组布局：每个主题一张卡，每条设置左右分栏（左侧标题 + 说明，右侧控件），
 * 开关统一用 .switch 切换。网络急救与智能接管已独立成页，本页不再混排；
 * 接管相关字段（proxy_* / billing_account_ids）本页没有编辑权，保存时原样透传。
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
  // 接管相关字段本页没有编辑权，原样透传（它们归「智能接管」页管）
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

  const doSave = async () => {
    setBusy(true);
    setErr("");
    try {
      await onSave(snapshot());
    } catch (e) {
      setErr(String(e));
      setBusy(false);
    }
  };

  return (
    <section className="panel-page set-page">
      <header className="set-head">
        <h2>设置</h2>
        <p>定时签到、多账号风控与通知等常规配置。智能接管相关配置请在「智能接管」页调整。</p>
      </header>

      {/* 签到自动化 */}
      <div className="set-card">
        <div className="set-card-head">
          <div>
            <div className="set-card-title">签到自动化</div>
            <div className="set-card-sub">控制账号在何时自动完成签到</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="启动应用时自动签到"
            desc="打开应用时自动为全部账号签到一次"
            ctrl={<Toggle checked={auto} onChange={setAuto} title="启动即签到" />}
          />
          <Row
            title="每天定时签到"
            desc="在指定时刻为全部账号自动签到"
            ctrl={<Toggle checked={schedOn} onChange={setSchedOn} />}
          />
          {schedOn && (
            <Row
              sub
              title="签到时刻"
              desc="24 小时制。应用运行期间触发；错过时刻后 30 分钟内打开会自动补签。"
              ctrl={
                <input
                  type="time"
                  value={schedTime}
                  onChange={(e) => setSchedTime(e.target.value)}
                />
              }
            />
          )}
          <Row
            title="开机自启动"
            desc="登录系统时自动运行本应用（操作系统级设置）"
            ctrl={
              <Toggle
                checked={autostart === true}
                disabled={autoBusy || autostart === null}
                onChange={(v) => void toggleAutostart(v)}
              />
            }
          />
          {autostart === null ? (
            <p className="hint set-foot">未能读取开机自启动状态（该平台可能不支持）。</p>
          ) : (
            schedOn &&
            !autostart && (
              <p className="hint set-foot warn">
                建议同时开启「开机自启动」，否则应用不运行时定时签到不会发生。
              </p>
            )
          )}
        </div>
      </div>

      {/* 多账号风控 */}
      <div className="set-card">
        <div className="set-card-head">
          <div>
            <div className="set-card-title">多账号风控</div>
            <div className="set-card-sub">批量签到时打散请求节奏，降低触发风控的概率</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="账号间随机间隔"
            desc="批量签到时，每个账号之间随机等待一段时间再发下一个，避免同 IP 瞬时连发。"
            ctrl={<Toggle checked={staggerOn} onChange={setStaggerOn} />}
          />
          {staggerOn && (
            <Row
              sub
              title="间隔上限"
              desc="每个账号之间的随机等待不超过该秒数（2～600）。单账号签到不受影响。"
              ctrl={
                <>
                  <input
                    type="number"
                    value={staggerMax}
                    min={2}
                    max={600}
                    onChange={(e) => setStaggerMax(e.target.value)}
                  />
                  <span className="set-suffix">秒</span>
                </>
              }
            />
          )}
        </div>
      </div>

      {/* 签到通知 */}
      <div className="set-card">
        <div className="set-card-head">
          <div>
            <div className="set-card-title">签到通知</div>
            <div className="set-card-sub">签到结果通过 webhook 推送到你的渠道</div>
          </div>
        </div>
        <div className="set-group">
          <Row
            title="开启 webhook 通知"
            desc="关闭后不再推送任何签到通知"
            ctrl={<Toggle checked={notifyOn} onChange={setNotifyOn} />}
          />
          {notifyOn && (
            <div className="set-expand">
              <label className="set-field">
                推送地址（Webhook）
                <input
                  value={webhook}
                  placeholder="https://…/hook/<key>"
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
              <Row
                bare
                title="定时签到后推送"
                ctrl={<Toggle checked={notifySched} onChange={setNotifySched} />}
              />
              <Row
                bare
                title="手动「全部签到」后推送"
                ctrl={<Toggle checked={notifyManual} onChange={setNotifyManual} />}
              />
            </div>
          )}
        </div>
      </div>

      {err && <p className="form-err">{err}</p>}
      <div className="page-actions">
        <button className="btn primary" disabled={busy} onClick={() => void doSave()}>
          {busy ? "保存中…" : "保存"}
        </button>
      </div>
    </section>
  );
}
