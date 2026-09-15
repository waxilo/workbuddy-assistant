//! 积分台账与每日结算。
//!
//! # 为什么不用「定时抓一次余额、算涨跌」
//!
//! 最直觉的做法是每 12 点记一次剩余积分，用差值当「今天消耗」。它有两个致命问题：
//!
//! 1. **分不清消耗与新增**。同一天里先消耗 100 再签到得 100，余额回到原点是差值为 0，
//!    而那 100 的消耗凭空消失。采样越稀，这种抵消越严重。
//! 2. **多客户端一起消耗时无法归因**。这个应用只是其中一个消耗方（还可能被直连绕过），
//!    任何「按请求记账」的写法都会在并发下算错。
//!
//! # 所以改成读接口的累计量
//!
//! `get-user-resource` 每个资源包都带**累计**字段（实测确认真实存在）：
//! `CapacitySize`（累计授予）与 `CapacityUsed`（累计已用）。两者都是单调递增的计数器，
//! 于是：
//!
//! - 消耗 = Σ `CapacityUsed` 的增量
//! - 新增 = Σ `CapacitySize` 的增量
//!
//! 累计量的差值**天然覆盖多客户端**：谁消耗的都会让计数器上涨，不需要知道是谁、
//! 也不需要按请求归属，因此并发不会算错。这与「抓余额」的区别是本质的——
//! 计数器只增不减，消耗与新增各自留痕，不会互相抵消。
//!
//! # 台账为什么按「包」存
//!
//! 直接对「全部包的合计」取差值会踩两个坑：
//!
//! - **包会过期消失**。查询按 `PackageEndTime >= now` 过滤，包一过期就不再出现在响应里，
//!   合计值会**掉下来**，日报立刻变成负数消耗。
//! - **周期会重置**。同一 `PackageCode` 跨周期后 `CapacityUsed` 可能归零。
//!
//! 所以台账以 `PackageCode` 为键逐包记账，且每个包的数值**只取观测到的最大值**——
//! 包过期后条目留在台账里（不再更新），合计因此永不回退；只有观测到「同一包、
//! 周期起点变了、已用量真的变小了」时才把旧周期的量归档到 `rolled_used`，
//! 既不让合计倒退，也不把重置后的用量重复计入。

use crate::accounts::{set_private_permissions, Account};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 保留的日报条数上限（约一年多），超出后丢弃最旧的
const MAX_REPORTS: usize = 400;

/// 金额统一保留两位小数，与界面展示口径一致（接口给的是 `805.14000097` 这种精度）
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 一个资源包的观测值 —— 台账的输入。
///
/// 由 `checkin` 从 `get-user-resource` 的响应里解析出来；定义在这里是因为它就是
/// 台账的输入契约（`checkin` 负责解析，`ledger` 负责记账，两边不互相依赖内部结构）。
#[derive(Clone, Debug, PartialEq)]
pub struct PkgView {
    /// 资源包唯一标识（`PackageCode`，缺失时退化到 `ResourceId + CycleStartTime`）
    pub key: String,
    pub name: String,
    /// 累计授予（`CapacitySize`）
    pub size: f64,
    /// 累计已用（`CapacityUsed`）
    pub used: f64,
    /// 当前周期起点（`CycleStartTime`）：它变化意味着这个包翻了新周期
    pub cycle_start: String,
}

/// 台账里一个资源包的条目。数值只增不减（见模块头注释）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PkgEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub used: f64,
    #[serde(default)]
    pub cycle_start: String,
    #[serde(default)]
    pub last_seen: String,
}

/// 单个账号的台账。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AcctLedger {
    #[serde(default)]
    pub pkgs: BTreeMap<String, PkgEntry>,
    /// 已翻过周期的包的「旧周期已用量」归档：保住合计不倒退
    #[serde(default)]
    pub rolled_used: f64,
    #[serde(default)]
    pub last_seen: String,
    /// 上次结算时点的累计（日报基线）
    #[serde(default)]
    pub base_granted: f64,
    #[serde(default)]
    pub base_used: f64,
    /// 是否已经采过样。首次采样要把基线对齐到当前值，否则「开始统计」那一刻
    /// 会把账号里已有的全部历史积分当成一天的新增/消耗报出来。
    #[serde(default)]
    pub seeded: bool,
}

impl AcctLedger {
    /// 累计授予（台账口径，只增）
    pub fn granted(&self) -> f64 {
        self.pkgs.values().map(|p| p.size).sum()
    }
    /// 累计已用（含已归档的旧周期用量，只增）
    pub fn used(&self) -> f64 {
        self.rolled_used + self.pkgs.values().map(|p| p.used).sum::<f64>()
    }
}

/// 全账号台账 + 结算基线
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Ledger {
    #[serde(default)]
    pub accts: BTreeMap<String, AcctLedger>,
    /// 上次结算时点（本地时间串）：下一条日报的窗口起点
    #[serde(default)]
    pub baseline_at: Option<String>,
    /// 累计采样次数（透明度：>1 说明窗口内不止 12 点那一刻采过）
    #[serde(default)]
    pub samples: u64,
}

/// 把一次观测合并进某个账号的台账。
///
/// 规则（每条都对应模块头里说的一个坑）：
/// - 新包 → 直接入账（它的 `size` 就是这段时间的新增）
/// - 已存在 → `size`/`used` 取观测最大值，包过期消失时条目留在台账里，合计不会倒退
/// - 同一包换了周期 **且** `used` 真的回退了 → 旧周期的用量归档进 `rolled_used`
pub fn merge_account(led: &mut AcctLedger, views: &[PkgView], at: &str) {
    for v in views {
        match led.pkgs.get_mut(&v.key) {
            None => {
                led.pkgs.insert(
                    v.key.clone(),
                    PkgEntry {
                        name: v.name.clone(),
                        size: v.size,
                        used: v.used,
                        cycle_start: v.cycle_start.clone(),
                        last_seen: at.to_string(),
                    },
                );
            }
            Some(e) => {
                if e.cycle_start != v.cycle_start && v.used < e.used {
                    led.rolled_used += e.used;
                    e.used = v.used;
                } else if v.used > e.used {
                    e.used = v.used;
                }
                if v.size > e.size {
                    e.size = v.size;
                }
                if v.name != e.name {
                    e.name = v.name.clone();
                }
                e.cycle_start = v.cycle_start.clone();
                e.last_seen = at.to_string();
            }
        }
    }
    if !led.seeded {
        // 首次采样：把基线对齐到刚刚入账的值，日报从「这两个时点之间」算起。
        // 不这么做的话，「开始统计」那一刻会把账号里已有的全部历史积分
        // 报成一天的新增与消耗。
        led.base_granted = led.granted();
        led.base_used = led.used();
        led.seeded = true;
    }
    led.last_seen = at.to_string();
}

/// 日报里的一行（一个账号）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreditReportAccount {
    pub account_id: String,
    pub name: String,
    #[serde(default)]
    pub phone: Option<String>,
    /// 本窗口消耗（Σ CapacityUsed 增量）
    pub consumed: f64,
    /// 本窗口新增（Σ CapacitySize 增量）
    pub gained: f64,
    /// 结算时点的剩余积分（口径与账号列表「剩余积分」一致）
    #[serde(default)]
    pub balance: Option<f64>,
    /// 结算时点仍在计量的资源包个数（便于判断「没数据」还是「真的 0」）
    #[serde(default)]
    pub packages: usize,
}

/// 一条日报（每天一条，窗口 = 上次结算时点 → 本次结算时点）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreditReport {
    /// 结算日 `YYYY-MM-DD`（窗口结束那天，作为列表标题）
    pub date: String,
    /// 实际结算时刻
    pub generated_at: String,
    /// 窗口起点（上次结算时刻；首次为结算时刻往前 24 小时）
    pub window_from: String,
    /// 窗口终点（本次结算时刻）
    pub window_to: String,
    pub accounts: Vec<CreditReportAccount>,
    pub total_consumed: f64,
    pub total_gained: f64,
    /// 结算时点全部账号的剩余积分合计（全部取不到时为 None）
    #[serde(default)]
    pub total_balance: Option<f64>,
    /// 窗口内的采样次数
    #[serde(default)]
    pub samples: u64,
}

/// 结算：把「当前累计 − 上次结算时的累计」写成一条日报，并把基线推进到当前时点。
///
/// 基线只在结算时推进，所以窗口内的高频采样（刷新 / 签到）只会让累计更准，
/// 不会把窗口切成碎片。
pub fn settle(
    led: &mut Ledger,
    date: &str,
    window_from: &str,
    window_to: &str,
    accounts: &[Account],
    balances: &BTreeMap<String, Option<f64>>,
    samples: u64,
) -> CreditReport {
    let mut rows = Vec::with_capacity(accounts.len());
    let (mut total_consumed, mut total_gained) = (0.0f64, 0.0f64);
    let mut total_balance = 0.0f64;
    let mut any_balance = false;

    for a in accounts {
        let entry = led.accts.get(&a.id);
        let (granted, used) = entry.map_or((0.0, 0.0), |e| (e.granted(), e.used()));
        let (base_granted, base_used) = entry.map_or((0.0, 0.0), |e| (e.base_granted, e.base_used));
        // 台账保证累计只增，这里再钳一次 0 纯粹是防御（例如用户手改过 json）
        let consumed = (used - base_used).max(0.0);
        let gained = (granted - base_granted).max(0.0);
        let balance = balances.get(&a.id).copied().flatten();
        if let Some(b) = balance {
            total_balance += b;
            any_balance = true;
        }
        total_consumed += consumed;
        total_gained += gained;
        rows.push(CreditReportAccount {
            account_id: a.id.clone(),
            name: a.name.clone(),
            phone: a.phone.clone(),
            consumed: round2(consumed),
            gained: round2(gained),
            balance: balance.map(round2),
            packages: entry.map_or(0, |e| e.pkgs.len()),
        });
    }

    // 推进基线：必须在算完所有差值之后，否则本轮就被抹成 0
    for a in accounts {
        let e = led.accts.entry(a.id.clone()).or_default();
        if !e.seeded {
            e.seeded = true;
        }
        e.base_granted = e.granted();
        e.base_used = e.used();
    }
    led.baseline_at = Some(window_to.to_string());

    CreditReport {
        date: date.to_string(),
        generated_at: window_to.to_string(),
        window_from: window_from.to_string(),
        window_to: window_to.to_string(),
        accounts: rows,
        total_consumed: round2(total_consumed),
        total_gained: round2(total_gained),
        total_balance: any_balance.then(|| round2(total_balance)),
        samples,
    }
}

// ── 落盘 ───────────────────────────────────────────────────────

pub fn ledger_file(dir: &Path) -> PathBuf {
    dir.join("credit_ledger.json")
}

pub fn reports_file(dir: &Path) -> PathBuf {
    dir.join("credit_reports.json")
}

/// 原子写：先写 `.tmp` 再 rename，避免中途崩溃留下半截 JSON
fn write_atomic(target: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, target)?;
    set_private_permissions(target);
    Ok(())
}

pub fn load_ledger(dir: &Path) -> Ledger {
    let f = ledger_file(dir);
    if !f.exists() {
        return Ledger::default();
    }
    serde_json::from_str(&fs::read_to_string(&f).unwrap_or_default()).unwrap_or_default()
}

pub fn save_ledger(dir: &Path, led: &Ledger) -> std::io::Result<()> {
    write_atomic(&ledger_file(dir), &serde_json::to_string_pretty(led)?)
}

/// 读日报：**新的在前**（界面直接顺序渲染）
pub fn load_reports(dir: &Path) -> Vec<CreditReport> {
    let f = reports_file(dir);
    if !f.exists() {
        return Vec::new();
    }
    serde_json::from_str(&fs::read_to_string(&f).unwrap_or_default()).unwrap_or_default()
}

fn save_reports(dir: &Path, reports: &[CreditReport]) -> std::io::Result<()> {
    write_atomic(&reports_file(dir), &serde_json::to_string_pretty(reports)?)
}

/// 追加一条日报（新的在前）并裁剪长度
pub fn append_report(dir: &Path, rep: CreditReport) -> std::io::Result<()> {
    let mut all = load_reports(dir);
    all.insert(0, rep);
    all.truncate(MAX_REPORTS);
    save_reports(dir, &all)
}

/// 清空全部日报（不影响台账与基线）
pub fn clear_reports(dir: &Path) -> std::io::Result<()> {
    save_reports(dir, &[])
}

/// 日报的推送文案（与 `notify::send` 的纯文本约定一致）
pub fn report_message(rep: &CreditReport) -> String {
    let mut s = format!(
        "积分日报 {}｜消耗 {:.2}｜新增 {:.2}",
        rep.date, rep.total_consumed, rep.total_gained
    );
    if let Some(b) = rep.total_balance {
        s.push_str(&format!("｜剩余 {b:.2}"));
    }
    s.push_str(&format!(
        "\n窗口 {} → {}",
        &rep.window_from[..16.min(rep.window_from.len())],
        &rep.window_to[..16.min(rep.window_to.len())]
    ));
    // 明细只列有变化的账号，避免推送被一串 0 刷屏
    let mut lines: Vec<String> = rep
        .accounts
        .iter()
        .filter(|a| a.consumed > 0.0 || a.gained > 0.0)
        .map(|a| format!("· {} 耗 {:.2} / 增 {:.2}", a.name, a.consumed, a.gained))
        .collect();
    if lines.is_empty() {
        lines.push("· 窗口内没有账号产生消耗或新增".to_string());
    }
    s.push('\n');
    s.push_str(&lines.join("\n"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Account, CheckinRecord};

    fn pkg(key: &str, size: f64, used: f64, cycle: &str) -> PkgView {
        PkgView {
            key: key.into(),
            name: format!("包{key}"),
            size,
            used,
            cycle_start: cycle.into(),
        }
    }

    fn account(id: &str, name: &str) -> Account {
        Account {
            id: id.into(),
            name: name.into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            base_url: None,
            created_at: String::new(),
            last: None::<CheckinRecord>,
            credit_snapshot: None,
            checked_today: None,
        }
    }

    #[test]
    fn merge_keeps_maxima_so_an_expiring_package_never_rewinds_the_total() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("a", 100.0, 40.0, "d1")], "t1");
        assert_eq!((led.granted(), led.used()), (100.0, 40.0));

        // 又消耗了一些
        merge_account(&mut led, &[pkg("a", 100.0, 70.0, "d1")], "t2");
        assert_eq!(led.used(), 70.0);

        // 包过期、从响应里消失 —— 合计必须保持 70，不能掉回 0
        merge_account(&mut led, &[], "t3");
        assert_eq!((led.granted(), led.used()), (100.0, 70.0));
    }

    #[test]
    fn cycle_rollover_archives_the_old_cycle_instead_of_rewinding() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("a", 500.0, 320.0, "9月")], "t1");
        // 翻到新周期：同一个月度包，已用量归零
        merge_account(&mut led, &[pkg("a", 500.0, 0.0, "10月")], "t2");
        assert_eq!(led.used(), 320.0, "归零应被归档，合计不得倒退");
        // 新周期里再消耗 80 → 合计 400
        merge_account(&mut led, &[pkg("a", 500.0, 80.0, "10月")], "t3");
        assert_eq!(led.used(), 400.0);
        // 授予量只增：周期重置不重复计入「新增」
        assert_eq!(led.granted(), 500.0);
    }

    #[test]
    fn first_sample_seeds_the_baseline_so_history_is_not_reported_as_one_day() {
        // 账号里本来就躺着 100 授予 / 90 已用（历史用量）。第一次采样只负责建立基线，
        // 不该把这 90 当成「窗口内消耗」报出来。
        let mut led = Ledger::default();
        merge_account(
            led.accts.entry("a1".into()).or_default(),
            &[pkg("a", 100.0, 90.0, "d1")],
            "t1",
        );
        assert!(led.accts["a1"].seeded);
        assert_eq!(
            (led.accts["a1"].base_granted, led.accts["a1"].base_used),
            (100.0, 90.0)
        );

        let a1 = account("a1", "甲");
        let rep = settle(
            &mut led,
            "2026-09-15",
            "2026-09-14 12:00:00",
            "2026-09-15 12:00:00",
            std::slice::from_ref(&a1),
            &BTreeMap::new(),
            1,
        );
        assert_eq!(
            (rep.total_consumed, rep.total_gained),
            (0.0, 0.0),
            "首次结算不该把历史用量算进来"
        );

        // 此后真的又消耗了 10 → 只报这 10
        merge_account(
            led.accts.entry("a1".into()).or_default(),
            &[pkg("a", 100.0, 100.0, "d1")],
            "t2",
        );
        let rep2 = settle(
            &mut led,
            "2026-09-16",
            "2026-09-15 12:00:00",
            "2026-09-16 12:00:00",
            &[a1],
            &BTreeMap::new(),
            2,
        );
        assert_eq!((rep2.total_consumed, rep2.total_gained), (10.0, 0.0));
    }

    #[test]
    fn settle_reports_per_account_and_total_deltas_then_advances_the_baseline() {
        let mut led = Ledger::default();
        let a1 = account("a1", "甲");
        let a2 = account("a2", "乙");

        // 第一次采样：建立基线（不计入任何窗口）
        merge_account(led.accts.entry("a1".into()).or_default(), &[pkg("p1", 100.0, 10.0, "d1")], "t0");
        merge_account(led.accts.entry("a2".into()).or_default(), &[pkg("p2", 200.0, 5.0, "d1")], "t0");

        // 窗口内：甲消耗 30、签到又得一个新包 100；乙消耗 50
        merge_account(led.accts.entry("a1".into()).or_default(), &[pkg("p1", 100.0, 40.0, "d1")], "t1");
        merge_account(led.accts.entry("a1".into()).or_default(), &[pkg("p1", 100.0, 40.0, "d1"), pkg("p1b", 100.0, 0.0, "d2")], "t1");
        merge_account(led.accts.entry("a2".into()).or_default(), &[pkg("p2", 200.0, 55.0, "d1")], "t1");

        let mut balances = BTreeMap::new();
        balances.insert("a1".to_string(), Some(170.0));
        balances.insert("a2".to_string(), Some(145.0));
        let rep = settle(
            &mut led,
            "2026-09-15",
            "2026-09-14 12:00:00",
            "2026-09-15 12:00:00",
            &[a1, a2],
            &balances,
            4,
        );

        let row = |id: &str| rep.accounts.iter().find(|r| r.account_id == id).unwrap().clone();
        assert_eq!((row("a1").consumed, row("a1").gained), (30.0, 100.0));
        assert_eq!((row("a2").consumed, row("a2").gained), (50.0, 0.0));
        assert_eq!((rep.total_consumed, rep.total_gained), (80.0, 100.0));
        assert_eq!(rep.total_balance, Some(315.0));
        assert_eq!(row("a1").packages, 2);

        // 基线已推进：立刻再结算一次必须是全 0（否则会重复计数）
        let again = settle(
            &mut led,
            "2026-09-15",
            "2026-09-15 12:00:00",
            "2026-09-15 12:00:01",
            &[account("a1", "甲"), account("a2", "乙")],
            &BTreeMap::new(),
            5,
        );
        assert_eq!((again.total_consumed, again.total_gained), (0.0, 0.0));
        assert_eq!(led.baseline_at.as_deref(), Some("2026-09-15 12:00:01"));
    }

    #[test]
    fn no_balance_reading_yields_none_instead_of_a_misleading_zero() {
        let mut led = Ledger::default();
        let a1 = account("a1", "甲");
        merge_account(led.accts.entry("a1".into()).or_default(), &[pkg("p", 10.0, 0.0, "d")], "t");
        let rep = settle(&mut led, "d", "f", "t", &[a1], &BTreeMap::new(), 1);
        assert_eq!(rep.total_balance, None);
        assert_eq!(rep.accounts[0].balance, None);
    }

    #[test]
    fn reports_are_newest_first_and_capped() {
        let dir = std::env::temp_dir().join(format!("wba-ledger-{}", uuid::Uuid::new_v4()));
        let mk = |d: &str| CreditReport {
            date: d.into(),
            generated_at: d.into(),
            window_from: String::new(),
            window_to: String::new(),
            accounts: vec![],
            total_consumed: 0.0,
            total_gained: 0.0,
            total_balance: None,
            samples: 0,
        };
        append_report(&dir, mk("2026-09-14")).unwrap();
        append_report(&dir, mk("2026-09-15")).unwrap();
        let all = load_reports(&dir);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].date, "2026-09-15", "最新的必须排在最前");

        for i in 0..MAX_REPORTS + 5 {
            append_report(&dir, mk(&format!("2026-01-{i:02}"))).unwrap();
        }
        assert_eq!(load_reports(&dir).len(), MAX_REPORTS);

        clear_reports(&dir).unwrap();
        assert!(load_reports(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ledger_round_trips_and_tolerates_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("wba-ledger-{}", uuid::Uuid::new_v4()));
        assert!(load_ledger(&dir).accts.is_empty(), "文件不存在时应是空台账");
        let mut led = Ledger::default();
        merge_account(led.accts.entry("a1".into()).or_default(), &[pkg("p", 5.0, 1.0, "d")], "t");
        led.samples = 7;
        save_ledger(&dir, &led).unwrap();
        let back = load_ledger(&dir);
        assert_eq!(back.samples, 7);
        assert_eq!(back.accts["a1"].used(), 1.0);
        assert!(back.accts["a1"].seeded);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn message_lists_only_accounts_with_movement() {
        let rep = CreditReport {
            date: "2026-09-15".into(),
            generated_at: "2026-09-15 12:00:00".into(),
            window_from: "2026-09-14 12:00:00".into(),
            window_to: "2026-09-15 12:00:00".into(),
            accounts: vec![
                CreditReportAccount {
                    account_id: "a".into(),
                    name: "甲".into(),
                    phone: None,
                    consumed: 12.5,
                    gained: 100.0,
                    balance: Some(1.0),
                    packages: 1,
                },
                CreditReportAccount {
                    account_id: "b".into(),
                    name: "乙".into(),
                    phone: None,
                    consumed: 0.0,
                    gained: 0.0,
                    balance: Some(1.0),
                    packages: 1,
                },
            ],
            total_consumed: 12.5,
            total_gained: 100.0,
            total_balance: Some(2.0),
            samples: 2,
        };
        let m = report_message(&rep);
        assert!(m.contains("消耗 12.50"));
        assert!(m.contains("新增 100.00"));
        assert!(m.contains("甲 耗 12.50 / 增 100.00"));
        assert!(!m.contains("乙"), "没有变化的账号不该出现在明细里：{m}");
    }
}
