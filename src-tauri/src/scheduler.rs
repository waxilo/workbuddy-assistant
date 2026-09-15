//! 定时自动签到：应用常驻时，每天在用户设定的时刻执行一次「全部签到」并推送通知。
//!
//! 触发模型：**到点即触发 + 补跑窗口**，而不是「当前分钟恰好等于设定值」。
//! 后者有两个必踩的坑：
//!   1. 轮询粒度做不到精确命中某一分钟（30s 一跳也可能整分钟落在两跳之间）；
//!   2. 应用常常是在预定时刻之后才被打开的（比如 09:10 才开机）。
//! 所以判定条件是「今天还没跑过 && 已过设定时刻但不超过 30 分钟」。
//!
//! 设定时刻是**窗口起点**：`schedule_window_minutes > 0` 时，当天实际触发时刻会在
//! `[设定时刻, +窗口]` 内随机挑一分钟，挑完落盘、当天不再变（见 [`target_time`]）。
//! 固定在同一分钟触发是脚本最好认的特征，窗口把它抹掉；`window = 0` 即回到
//! 「精确到分钟」的老行为。
//!
//! 跨启动去重靠 `schedule_state.json`（只记最后一次执行的日期），
//! 这样重启应用不会在补跑窗口内重复签一遍。
//!
//! 自动续签：常驻期间每 12 小时扫一遍账号，**剩余有效期不足 48 小时就静默续一次**；
//! 启动后也会立刻扫一次（久未开应用的情况靠它兜住）。
//! 与定时签到开关无关——续签是保命操作，不该被「没设定时签到」连带关掉。
//! 扫描间隔落盘在 `schedule_state.json`，避免每次启动/每跳都打接口。
//!
//! 通知只在 `notify_enabled && notify_on_schedule` 时发送，且**失败不影响签到**。
//!
//! 除定时签到外，本模块还管两件事，各自有独立开关与时刻、互不影响：
//!
//! - 自动续签（见 [`maybe_auto_refresh`]）：剩余有效期不足 48 小时就静默续一次。
//! - **每日积分日报**（见 [`maybe_settle_report`]）：到点结算「上次结算以来」的
//!   消耗与新增，并按 `notify_on_report` 推送。它**没有随机时间窗** —— 触发时刻
//!   就是统计窗口的边界，抖动会让相邻两天的日报无法直接相加。统计口径见
//!   [`crate::ledger`] 的模块说明。

use crate::accounts::{self, Settings};
use crate::commands;
use crate::ledger;
use crate::notify;
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 轮询间隔：30s 足够（补跑窗口有 30 分钟余量），开销可忽略
const TICK: Duration = Duration::from_secs(30);
/// 补跑窗口：错过设定时刻后仍愿意补签的时长
const CATCH_UP_MINUTES: i64 = 30;
/// 自动续签的扫描间隔：token 有效期 60 天，12 小时一跳足够从容
const REFRESH_SCAN_MS: i64 = 12 * 60 * 60 * 1000;
/// 定时任务进度事件，前端据此刷新列表并提示
pub const EVENT: &str = "checkin-scheduled";
/// 自动续签事件（payload：续签成功的账号数），前端据此刷新列表
pub const REFRESH_EVENT: &str = "auto-refreshed";
/// 积分日报结算完成事件，前端据此刷新日报列表
pub const REPORT_EVENT: &str = "credit-report-settled";

/// 当天随机挑定的定时签到时刻。
///
/// 必须落盘：否则每 30s 一跳都会重摇一次（「等时钟走过刚摇出来的那一分钟」这种判定
/// 在每跳都换目标时形同虚设），重启也会把当天已经用过的时刻换掉。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct DayTarget {
    /// 归属日期（`YYYY-MM-DD`）
    date: String,
    /// 当天实际触发时刻（`HH:MM`）
    at: String,
}

#[derive(Serialize, Deserialize, Default)]
struct ScheduleState {
    /// 最后一次定时执行的日期（`YYYY-MM-DD`）
    #[serde(default)]
    last_run_date: Option<String>,
    /// 最后一次自动续签扫描的时刻（毫秒时间戳）
    #[serde(default)]
    last_refresh_scan_ms: Option<i64>,
    /// 今天随机挑定的触发时刻（跨天自动重挑；见 [`target_time`]）
    #[serde(default)]
    today_target: Option<DayTarget>,
    /// 最后一次给日报做每日结算的日期（`YYYY-MM-DD`）
    ///
    /// 结算时刻固定 24:00（见 [`accounts::REPORT_TIME`]），而 24:00 属于次日，
    /// 因此这一字段实际记录的是「今天是否已经把过去的自然日算完了」——
    /// 每天一次、不看具体时分、不依赖网络。
    #[serde(default)]
    last_seal_date: Option<String>,
}

fn state_file(dir: &Path) -> PathBuf {
    dir.join("schedule_state.json")
}

fn load_state(dir: &Path) -> ScheduleState {
    fs::read_to_string(state_file(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(dir: &Path, st: &ScheduleState) {
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let target = state_file(dir);
    let tmp = dir.join("schedule_state.json.tmp");
    let Ok(json) = serde_json::to_string_pretty(st) else {
        return;
    };
    if fs::write(&tmp, json).is_ok() && fs::rename(&tmp, &target).is_ok() {
        accounts::set_private_permissions(&target);
    }
}

/// 今天该在几点几分触发。
///
/// - 已经为**同一个日期**挑过、且旧目标仍落在当前窗口内 → 原样复用；
/// - 否则在 `[base, base + window]` 内随机取一分钟；
/// - `window == 0`（或 base 非法）→ 直接用 base，等于关掉随机、保留「精确到分钟」的老行为。
///
/// 窗口会被截断在当天 23:59 之内，绝不跨天——跨天会让 [`due_at`] 的「补跑窗口」
/// 判定失去意义（今天的任务跑到明天零点后触发）。
pub(crate) fn target_time(
    date: &str,
    base: &str,
    window_minutes: u32,
    existing: Option<&DayTarget>,
) -> DayTarget {
    if let Some(t) = existing.filter(|t| t.date == date && in_window(t, base, window_minutes)) {
        return t.clone();
    }
    let at = chrono::NaiveTime::parse_from_str(base.trim(), "%H:%M")
        .ok()
        .filter(|_| window_minutes > 0)
        .map(|t| {
            let base_min = t.hour() * 60 + t.minute();
            // 上限 23:59，保证不跨天
            let span = window_minutes.min(24 * 60 - 1 - base_min);
            let total = base_min + crate::rng::range(0, span as u64) as u32;
            format!("{:02}:{:02}", total / 60, total % 60)
        })
        .unwrap_or_else(|| base.trim().to_string());
    DayTarget {
        date: date.to_string(),
        at,
    }
}

/// 旧目标是否仍落在 `[base, base + window]` 内。
///
/// 用它判断「设置改过了要不要重挑」：只按日期复用的话，用户把签到时刻从 09:00
/// 改到 11:00 之后，那个 10:12 的旧目标既不在新窗口里、又已经过期（按补跑窗口
/// 立刻触发一次），等于改了设置反而提前签了。
fn in_window(target: &DayTarget, base: &str, window_minutes: u32) -> bool {
    let (Ok(at), Ok(b)) = (
        chrono::NaiveTime::parse_from_str(target.at.trim(), "%H:%M"),
        chrono::NaiveTime::parse_from_str(base.trim(), "%H:%M"),
    ) else {
        // 两边都解析不出来时退化成字面比较：不相等就重挑
        return target.at.trim() == base.trim();
    };
    let mins = |t: chrono::NaiveTime| t.hour() as i64 * 60 + t.minute() as i64;
    let (at_m, base_m) = (mins(at), mins(b));
    at_m >= base_m && at_m <= base_m + window_minutes as i64
}

/// 距上次自动续签扫描是否已满一个间隔（从未扫过 → 立刻扫）。
fn refresh_scan_due(last_scan_ms: Option<i64>, now_ms: i64) -> bool {
    match last_scan_ms {
        Some(last) => now_ms - last >= REFRESH_SCAN_MS,
        None => true,
    }
}

/// 今天是否该跑了：设定时刻已过（但不超过补跑窗口），且今天还没跑过。
///
/// 抽成纯函数以便单测——时间判断最容易在边界上出错（跨天、未来时刻、重复跑）。
///
/// **只服务定时签到。** 日报不走这里：它的结算时刻固定 24:00，而 `NaiveTime`
/// 表示不了 24:00（合法范围到 `23:59:59`），传进来必然解析失败而永不触发；
/// 日报改用「次日首次运行结算昨天」的判定，见 [`maybe_settle_report`]。
pub(crate) fn due_at(
    now: chrono::NaiveDateTime,
    time: &str,
    last_run_date: Option<&str>,
) -> bool {
    let Ok(t) = chrono::NaiveTime::parse_from_str(time.trim(), "%H:%M") else {
        return false; // 时刻非法时宁可不跑
    };
    let today = now.date();
    if let Some(d) = last_run_date {
        if d == today.format("%Y-%m-%d").to_string() {
            return false;
        }
    }
    let mins = (now - today.and_time(t)).num_minutes();
    (0..=CATCH_UP_MINUTES).contains(&mins)
}

/// 启动后台调度线程。线程与进程同生命周期，无需 join。
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        // 启动先扫一次续签：等 app 初始化完成（数据目录就绪）再动手
        std::thread::sleep(Duration::from_secs(5));
        if let Ok(dir) = commands::try_data_dir(&app) {
            maybe_auto_refresh(&app, &dir);
        }

        loop {
            std::thread::sleep(TICK);

            // 每跳都重读配置：用户在设置里改了时刻/开关即刻生效，不需要重启应用
            let Ok(dir) = commands::try_data_dir(&app) else {
                continue;
            };
            let settings = accounts::load_settings(&dir);

            // 自动续签与「定时签到」开关无关，单独判定
            maybe_auto_refresh(&app, &dir);

            // 积分日报同理：有自己的开关与时刻，不受「定时签到」影响。
            // 必须放在下面那个 continue 之前 —— 否则用户一关定时签到，日报也顺带没了
            maybe_settle_report(&app, &dir, &settings);

            if !settings.schedule_enabled {
                continue;
            }
            let mut state = load_state(&dir);
            let now = chrono::Local::now().naive_local();
            let today = now.date().format("%Y-%m-%d").to_string();
            // 当天目标时刻：第一次算出来后立刻落盘，之后每一跳都复用同一个值，
            // 否则「随机窗口」每跳重摇一次，等于没加（详见 DayTarget）
            let target = target_time(
                &today,
                &settings.schedule_time,
                settings.schedule_window_minutes,
                state.today_target.as_ref(),
            );
            if state.today_target.as_ref() != Some(&target) {
                state.today_target = Some(target.clone());
                save_state(&dir, &state);
            }
            if !due_at(now, &target.at, state.last_run_date.as_deref()) {
                continue;
            }

            // 先落盘「今天已跑」再执行：万一执行中崩溃，也不会在补跑窗口里反复重试
            state.last_run_date = Some(today.clone());
            save_state(&dir, &state);
            log_event(
                &dir,
                &format!(
                    "触发定时签到（设定 {}，今日随机目标 {}，实际 {}）",
                    settings.schedule_time,
                    target.at,
                    now.format("%H:%M")
                ),
            );
            tauri::async_runtime::block_on(run_once(&app, &settings, &dir));
        }
    });
}

/// 自动续签扫描：距上次扫描超过 `REFRESH_SCAN_MS` 才真跑。
///
/// 先落盘扫描时刻再执行——续签接口可能耗时/失败，先记账可避免同一时刻被反复触发。
fn maybe_auto_refresh(app: &AppHandle, dir: &Path) {
    let mut state = load_state(dir);
    let now_ms = chrono::Utc::now().timestamp_millis();
    if !refresh_scan_due(state.last_refresh_scan_ms, now_ms) {
        return;
    }
    state.last_refresh_scan_ms = Some(now_ms);
    save_state(dir, &state);

    match tauri::async_runtime::block_on(commands::auto_refresh_all(app)) {
        Ok(report) => {
            if !report.refreshed.is_empty() {
                log_event(dir, &format!("自动续签完成：{}", report.refreshed.join("、")));
                let _ = app.emit(
                    REFRESH_EVENT,
                    serde_json::json!({ "count": report.refreshed.len() }),
                );
            }
            for f in &report.failed {
                log_event(dir, &format!("自动续签失败：{f}"));
            }
        }
        Err(e) => log_event(dir, &format!("自动续签异常：{e}")),
    }
}

/// 每日积分日报：**次日结算「昨天」**，并按设置推送。
///
/// 结算时刻固定 24:00（[`accounts::REPORT_TIME`]，用户不可改）。24:00 不是一个
/// 能「到点触发」的时刻 —— 它已经是次日的 00:00，而且 [`due_at`] 用的
/// `NaiveTime` 根本表示不了 24:00。所以这里不走去点判定，而是换个说法：
///
/// **当天走完（= 到了次日）后的第一次运行，把昨天封口。**
///
/// 这样每条日报都是一个**完整自然日**（00:00–24:00），列表里任意两条都能直接相加，
/// 也不会出现「同一个日期先看到半天、次日又变成全天」的前后不一致。
///
/// 代价是推送时间变成「次日首次打开应用时」——这正是「结算时刻 24:00」的应有之义：
/// 全天数字只有当天结束后才算得出来。若用户想随时看当前累计，用界面上的「当前累计」，
/// 那是一次性快照、不落盘。
///
/// 封口不依赖任何网络请求（桶早在采样时就归位了，这里只是重新聚合），
/// 所以每天第一跳就能完成，不受账号在线状态影响。
fn maybe_settle_report(app: &AppHandle, dir: &Path, settings: &Settings) {
    if !settings.report_enabled {
        return;
    }
    let mut state = load_state(dir);
    let now = chrono::Local::now().naive_local();
    let today = now.date().format("%Y-%m-%d").to_string();

    // 每天只做一次。先落盘再推送：推送失败/中断都不该让同一天重复结算。
    if state.last_seal_date.as_deref() == Some(today.as_str()) {
        return;
    }

    let Ok(data_dir) = commands::try_data_dir(app) else {
        log_event(dir, "日报结算跳过：数据目录不可用");
        return;
    };
    let accounts = accounts::load_accounts(&data_dir);
    let sealed = commands::seal_reports(&data_dir, &accounts, &today);

    state.last_seal_date = Some(today.clone());
    save_state(dir, &state);

    if sealed.is_empty() {
        // 昨天没产生任何数据（或已封口过）——不打扰用户，也不推空日报
        return;
    }

    let total_days = sealed.len();
    let latest = sealed.first().cloned();
    log_event(
        &data_dir,
        &format!(
            "已结算 {} 天的日报（结算时刻 {REPORT_TIME}，每条均为完整自然日）",
            total_days,
            REPORT_TIME = accounts::REPORT_TIME,
        ),
    );
    let _ = app.emit(
        REPORT_EVENT,
        serde_json::json!({ "sealed": total_days }),
    );

    // 只推最新那条（通常是昨天）。补算了多天时，前面的天是历史欠账，
    // 一次性推出去会把通知刷屏，用户真正关心的是刚结束的那一天。
    if settings.notify_enabled && settings.notify_on_report {
        if let Some(rep) = latest {
            let outcome = match tauri::async_runtime::block_on(notify::send(
                &settings.notify_webhook,
                &ledger::report_message(&rep),
            )) {
                Ok(resp) => format!("日报通知已发送（{}）：{resp}", rep.date),
                Err(e) => format!("日报通知发送失败（{}）：{e}", rep.date),
            };
            log_event(dir, &outcome);
        }
    }
}

/// 调度日志（`scheduler.log`）：只记「触发 / 通知结果 / 异常」这类有排查价值的事件。
///
/// 定时任务最大的失败模式是**静默不生效**（应用没开、时刻写错、webhook 被墙），
/// 所以留一份可查的记录；逐 tick 的判定不写进来，避免日志噪音与无限增长。
fn log_event(dir: &Path, msg: &str) {
    const MAX_LINES: usize = 200;

    let target = dir.join("scheduler.log");
    let old = fs::read_to_string(&target).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().collect();
    if lines.len() >= MAX_LINES {
        lines.drain(0..lines.len() - MAX_LINES + 1);
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(&format!(
        "[{}] {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        msg
    ));

    if fs::create_dir_all(dir).is_err() {
        return;
    }
    if fs::write(&target, body).is_ok() {
        accounts::set_private_permissions(&target);
    }
}

async fn run_once(app: &AppHandle, settings: &Settings, dir: &Path) {
    let _ = app.emit(EVENT, serde_json::json!({ "stage": "start" }));

    let notify_on = settings.notify_enabled && settings.notify_on_schedule;
    match commands::checkin_all_inner(app, true).await {
        Ok(accounts) => {
            // 与 notify 一致：「已签」与「成功」互斥计数（已签的响应 success 也是 true）
            let already = accounts.iter().filter(|a| matches!(&a.last, Some(r) if r.already)).count();
            let ok = accounts
                .iter()
                .filter(|a| matches!(&a.last, Some(r) if r.success && !r.already))
                .count();
            let msg = format!("签到完成：{} 个账号，成功 {ok} / 已签 {already}", accounts.len());
            log_event(dir, &msg);
            let _ = app.emit(
                EVENT,
                serde_json::json!({ "stage": "done", "count": accounts.len() }),
            );
            if notify_on {
                // 通知失败只记日志，绝不影响签到结果
                let outcome = match notify::send(&settings.notify_webhook, &notify::summary_message(&accounts)).await {
                    Ok(resp) => format!("通知已发送：{resp}"),
                    Err(e) => format!("通知发送失败：{e}"),
                };
                log_event(dir, &outcome);
            }
        }
        Err(e) => {
            log_event(dir, &format!("签到异常：{e}"));
            let _ = app.emit(EVENT, serde_json::json!({ "stage": "error", "message": e }));
            if notify_on {
                let _ = notify::send(
                    &settings.notify_webhook,
                    &format!("WorkBuddy 定时签到异常：{e}"),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(h: u32, m: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 9, 12)
            .unwrap()
            .and_hms_opt(h, m, 0)
            .unwrap()
    }

    #[test]
    fn fires_at_the_configured_minute() {
        assert!(due_at(at(9, 7), "09:07", None));
        // 轮询粒度落在同一分钟内的任意秒都算数
        let t = NaiveDate::from_ymd_opt(2026, 9, 12)
            .unwrap()
            .and_hms_opt(9, 7, 45)
            .unwrap();
        assert!(due_at(t, "09:07", None));
    }

    #[test]
    fn catches_up_when_app_starts_late() {
        // 09:30 才打开应用，仍在 30 分钟补跑窗口内
        assert!(due_at(at(9, 30), "09:07", None));
        // 超出窗口就不再补，避免晚上开应用时突然签一次
        assert!(!due_at(at(9, 38), "09:07", None));
        assert!(!due_at(at(20, 0), "09:07", None));
    }

    #[test]
    fn does_not_fire_before_the_time_or_twice_a_day() {
        assert!(!due_at(at(9, 6), "09:07", None));
        // 同一天已跑过 → 不再补
        assert!(!due_at(at(9, 20), "09:07", Some("2026-09-12")));
        // 昨天的记录不影响今天
        assert!(due_at(at(9, 7), "09:07", Some("2026-09-11")));
    }

    #[test]
    fn tolerates_loose_or_invalid_time() {
        assert!(due_at(at(9, 7), " 09:07 ", None));
        for bad in ["", "9", "24:00", "aa:bb", "09:07:00"] {
            assert!(!due_at(at(9, 7), bad, None), "{bad:?} 不应触发");
        }
    }

    #[test]
    fn scans_refresh_at_most_once_per_interval() {
        let now = 1_700_000_000_000;
        assert!(refresh_scan_due(None, now), "从未扫过应立刻扫");
        assert!(!refresh_scan_due(Some(now), now), "刚扫过不应立刻再扫");
        assert!(!refresh_scan_due(Some(now - REFRESH_SCAN_MS + 1), now));
        assert!(refresh_scan_due(Some(now - REFRESH_SCAN_MS - 1), now));
        // 时钟回拨 / 文件被手改成未来时间：视为「刚扫过」，宁可不扫也不刷接口
        assert!(!refresh_scan_due(Some(now + 1000), now));
    }

    #[test]
    fn state_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("wba-sched-{}", uuid::Uuid::new_v4()));
        assert!(load_state(&dir).last_run_date.is_none());
        assert!(load_state(&dir).today_target.is_none());
        save_state(
            &dir,
            &ScheduleState {
                last_run_date: Some("2026-09-12".into()),
                last_refresh_scan_ms: Some(1_700_000_000_000),
                today_target: Some(DayTarget {
                    date: "2026-09-12".into(),
                    at: "10:12".into(),
                }),
                last_seal_date: Some("2026-09-13".into()),
            },
        );
        let loaded = load_state(&dir);
        assert_eq!(loaded.last_run_date.as_deref(), Some("2026-09-12"));
        assert_eq!(loaded.last_refresh_scan_ms, Some(1_700_000_000_000));
        // 当天目标必须扛得住重启，否则「随机窗口」会被重启重新摇一次
        assert_eq!(
            loaded.today_target.as_ref().map(|t| t.at.as_str()),
            Some("10:12")
        );
        // 日报的日期独立于签到：两个开关互不影响，去重也必须独立
        assert_eq!(loaded.last_seal_date.as_deref(), Some("2026-09-13"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_time_without_window_stays_on_the_exact_minute() {
        // 窗口 0 = 关掉随机，保持「精确到分钟」的老行为
        assert_eq!(target_time("2026-09-12", "09:07", 0, None).at, "09:07");
        // 非法时刻原样留着（due_at 会拒绝触发），不在这里瞎猜一个点
        assert_eq!(target_time("2026-09-12", "aa:bb", 90, None).at, "aa:bb");
    }

    #[test]
    fn target_time_lands_inside_the_window_and_is_stable_for_the_day() {
        let first = target_time("2026-09-12", "08:00", 90, None);
        assert!(in_window(&first, "08:00", 90), "落在窗口外：{}", first.at);
        for _ in 0..100 {
            assert_eq!(
                target_time("2026-09-12", "08:00", 90, Some(&first)).at,
                first.at,
                "同一天重复调用不该重摇"
            );
        }
        // 换一天就要重挑（值可以碰巧相同，但归属日期必须更新）
        let next = target_time("2026-09-13", "08:00", 90, Some(&first));
        assert_eq!(next.date, "2026-09-13");
        assert!(in_window(&next, "08:00", 90));
    }

    #[test]
    fn target_time_never_crosses_midnight() {
        let lo = chrono::NaiveTime::from_hms_opt(23, 30, 0).unwrap();
        let hi = chrono::NaiveTime::from_hms_opt(23, 59, 0).unwrap();
        for _ in 0..200 {
            let t = target_time("2026-09-12", "23:30", 90, None);
            let at = chrono::NaiveTime::parse_from_str(&t.at, "%H:%M").unwrap();
            assert!(at >= lo && at <= hi, "窗口被截断失败：{}", t.at);
        }
    }

    #[test]
    fn target_time_repicks_when_the_setting_moves_later() {
        // 旧目标 09:40 对应的设置是 09:00 + 90min；用户把时刻改到 11:00 后
        // 旧目标既不在新窗口里、按补跑窗口又会立刻触发一次 → 必须重挑
        let stale = DayTarget {
            date: "2026-09-12".into(),
            at: "09:40".into(),
        };
        let t = target_time("2026-09-12", "11:00", 90, Some(&stale));
        assert!(in_window(&t, "11:00", 90), "旧目标不该被复用：{}", t.at);

        // 反向：设置改早时旧目标仍在新窗口内，继续用，不必重摇
        let keep = DayTarget {
            date: "2026-09-12".into(),
            at: "10:12".into(),
        };
        assert_eq!(
            target_time("2026-09-12", "09:00", 120, Some(&keep)).at,
            "10:12"
        );
    }

    #[test]
    fn log_event_appends_and_caps_length() {
        let dir = std::env::temp_dir().join(format!("wba-log-{}", uuid::Uuid::new_v4()));
        for i in 0..5 {
            log_event(&dir, &format!("事件 {i}"));
        }
        let body = fs::read_to_string(dir.join("scheduler.log")).unwrap();
        assert_eq!(body.lines().count(), 5);
        assert!(body.contains("事件 4"));
        assert!(body.contains("事件 0"), "未超上限时不应丢历史：{body}");

        // 超过上限后只保留最后 MAX_LINES 行（避免日志无限增长）
        for i in 0..205 {
            log_event(&dir, &format!("填充 {i}"));
        }
        let body = fs::read_to_string(dir.join("scheduler.log")).unwrap();
        let n = body.lines().count();
        assert!((200..=201).contains(&n), "应被裁剪到 ~200 行，实际 {n}");
        assert!(!body.contains("事件 0"), "最旧的记录应被裁掉");
        assert!(body.contains("填充 204"), "最新记录应保留");
        let _ = fs::remove_dir_all(&dir);
    }
}
