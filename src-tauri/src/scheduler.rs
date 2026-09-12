//! 定时自动签到：应用常驻时，每天在用户设定的时刻执行一次「全部签到」并推送通知。
//!
//! 触发模型：**到点即触发 + 补跑窗口**，而不是「当前分钟恰好等于设定值」。
//! 后者有两个必踩的坑：
//!   1. 轮询粒度做不到精确命中某一分钟（30s 一跳也可能整分钟落在两跳之间）；
//!   2. 应用常常是在预定时刻之后才被打开的（比如 09:10 才开机）。
//! 所以判定条件是「今天还没跑过 && 已过设定时刻但不超过 30 分钟」。
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

use crate::accounts::{self, Settings};
use crate::commands;
use crate::notify;
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

#[derive(Serialize, Deserialize, Default)]
struct ScheduleState {
    /// 最后一次定时执行的日期（`YYYY-MM-DD`）
    #[serde(default)]
    last_run_date: Option<String>,
    /// 最后一次自动续签扫描的时刻（毫秒时间戳）
    #[serde(default)]
    last_refresh_scan_ms: Option<i64>,
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

            if !settings.schedule_enabled {
                continue;
            }
            let mut state = load_state(&dir);
            let now = chrono::Local::now().naive_local();
            if !due_at(now, &settings.schedule_time, state.last_run_date.as_deref()) {
                continue;
            }

            // 先落盘「今天已跑」再执行：万一执行中崩溃，也不会在补跑窗口里反复重试
            state.last_run_date = Some(now.date().format("%Y-%m-%d").to_string());
            save_state(&dir, &state);
            log_event(
                &dir,
                &format!(
                    "触发定时签到（设定 {}，实际 {}）",
                    settings.schedule_time,
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
    match commands::checkin_all_inner(app).await {
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
        save_state(
            &dir,
            &ScheduleState {
                last_run_date: Some("2026-09-12".into()),
                last_refresh_scan_ms: Some(1_700_000_000_000),
            },
        );
        let loaded = load_state(&dir);
        assert_eq!(loaded.last_run_date.as_deref(), Some("2026-09-12"));
        assert_eq!(loaded.last_refresh_scan_ms, Some(1_700_000_000_000));
        let _ = fs::remove_dir_all(&dir);
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
