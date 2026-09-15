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
//! 所以台账以 `ResourceId` 为键逐包记账（**注意不是 `PackageCode`**，见 `PkgView::key`），
//! 且每个包的数值**只取观测到的最大值**——包过期后条目留在台账里（不再更新），
//! 合计因此永不回退；只有观测到「同一包、周期起点变了、已用量真的变小了」时
//! 才把旧周期的量归档到 `rolled_used`，既不让合计倒退，也不把重置后的用量重复计入。
//!
//! # 「按天」与「每小时」是同一份数据的两种聚合
//!
//! 每个账号除了累计值，还维护一张**按小时的增量桶** `hours`，由每次采样时
//! 「与上次采样相比涨了多少」累加而成。于是：
//!
//! - 小时桶 → 某天的合计 = Σ 该天 24 个桶
//! - 相邻两天可直接相加（桶是「时刻 → 增量」的唯一归属，不重不漏）
//!
//! 关键设计：**增量在采样那一刻就落进「采样时刻所属的那个小时」，之后不再移动**。
//! 这样「第 24 格把余额清零推进新一天」这件事根本不会发生 —— 不需要「封口」这一步，
//! 也就没有「封口时机错了就丢数据」的风险。日期只是查询时的一个过滤条件。
//!
//! 代价是精度受采样密度限制（见 [`CreditReport::hours`] 的说明）。

use crate::accounts::{set_private_permissions, Account};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 保留的日报条数上限（约一年多），超出后丢弃最旧的
const MAX_REPORTS: usize = 400;

/// 保留的**手动**快照上限。
///
/// 手动快照有两种命运（见 [`SnapshotKind`]）：有系统快照做锚点时只剩一格，
/// 没有锚点时可以累积。累积那一支必须有上限，否则连点几十次就是一个只增不减的列表。
const MAX_MANUAL_SNAPSHOTS: usize = 20;

/// 小时桶保留天数。60 天 = 1440 个 (日期, 小时) 组合 / 账号，
/// JSON 体积可忽略，但足够回看两个月里的任意一天。
const KEEP_DAYS: i64 = 60;

/// 日期 → 24 个增量桶（下标 = 小时）。只保留 `KEEP_DAYS` 天。
pub type HourBuckets = BTreeMap<String, [f64; 24]>;

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
    /// 已翻过周期的包的「旧周期授予量」归档：与 `pkgs` 里各包的 `size` 一起构成累计授予。
    /// 周期重置时授予量不该重复计入「新增」，但**累计值的单调性**仍要保住 ——
    /// 否则「授予量 × 已用比例」这类会回退的包，会在基线里留下一个填不平的坑（见 `merge_account`）。
    #[serde(default)]
    pub rolled_granted: f64,
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
    /// 按小时的增量桶（消耗），键 = 日期 `YYYY-MM-DD`
    #[serde(default)]
    pub hours_used: HourBuckets,
    /// 按小时的增量桶（授予）
    #[serde(default)]
    pub hours_granted: HourBuckets,
    /// 上次采样时刻（本地时间串）。小时桶靠它算「这次比上次涨了多少」，
    /// 因此**必须与 `last_seen`（只用于展示）分开**：任何写 `last_seen` 的地方
    /// 都不能顺手改它，否则会把一段真实增量抹成 0。
    #[serde(default)]
    pub last_sample_at: String,
}

impl AcctLedger {
    /// 累计授予（台账口径，只增）
    pub fn granted(&self) -> f64 {
        self.rolled_granted + self.pkgs.values().map(|p| p.size).sum::<f64>()
    }
    /// 累计已用（含已归档的旧周期用量，只增）
    pub fn used(&self) -> f64 {
        self.rolled_used + self.pkgs.values().map(|p| p.used).sum::<f64>()
    }

    /// 删掉 `KEEP_DAYS` 天以前的桶，避免文件无限增长。
    fn prune_hours(&mut self, today: chrono::NaiveDate) {
        let cutoff = (today - chrono::Duration::days(KEEP_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        // 日期是 `YYYY-MM-DD`，字典序即时间序 —— 直接比字符串即可
        self.hours_used.retain(|d, _| d.as_str() >= cutoff.as_str());
        self.hours_granted.retain(|d, _| d.as_str() >= cutoff.as_str());
    }
}

/// 全账号台账。
///
/// 自然日口径下**不再需要结算基线**：某天的值就是那天 24 个小时桶之和，
/// 与「什么时候点的结算」无关。因此连点两次结算得到的是同一条日报的刷新，
/// 而不是一条接近全 0 的新日报。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Ledger {
    #[serde(default)]
    pub accts: BTreeMap<String, AcctLedger>,
}

/// 把一次观测合并进某个账号的台账，并把「与上次采样相比的增量」累加进对应的小时桶。
///
/// `at` 是本次采样时刻（本地 `YYYY-MM-DD HH:MM:SS`）。
///
/// 规则（每条都对应模块头里说的一个坑）：
/// - 新包 → 直接入账（它的 `size` 就是这段时间的新增）
/// - 已存在 → `size`/`used` 取观测最大值，包过期消失时条目留在台账里，合计不会倒退
/// - 同一包换了周期且 `used` 真的回退了 → 增量不能当负数记，改为把旧周期用量归档进
///   `rolled_used` 并重设该包基线，使 `used()` 保持不降
/// - 周期重置时授予量**不重复计入新增**（因此新周期授予量不作为增量记账），
///   把差额归档进 `rolled_granted` 保住单调性
/// - 小时桶只记**正增量**：负增量（周期重置、包过期）不是「消耗」，不能记进某一天
pub fn merge_account(led: &mut AcctLedger, views: &[PkgView], at: &str) {
    // 采样前先记下累计值：本次增量 = 合并之后 − 合并之前
    let before = (led.granted(), led.used());
    // 「本次是否有任何一个包被更新」。`views` 为空（账号一个包都没有 / 接口失败）
    // 时**不能**记增量：那不是「没有变化」，只是「这一轮没读到」。
    let mut touched = false;

    for v in views {
        touched = true;
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
                    // 翻了周期且已用量回退：把旧的观测值整体归档，然后**按最大值**并入新周期的观测。
                    //
                    // 这里不能「用新值覆盖旧值」：`size` 比 `used` 更常回退
                    // （「授予量 × 已用比例」型的包每月都会把授予量重新算一遍，可以缩水），
                    // 一旦 `granted()` 掉下去，而增量按 `max(0)` 记账，那个缺口就**永远补不回来** ——
                    // 之后每次给这个包授予积分都会被缺口先吃掉，表现为「新增积分迟迟不显示」。
                    // 取最大值则两个不变量同时成立：合计单调不降，且**任何真实增量都不被吞掉**
                    //（`size` 因取 max 而不上浮，新增就完整地表现成增量）。
                    led.rolled_used += e.used;
                    led.rolled_granted += e.size;
                    e.used = v.used;
                    e.size = v.size;
                } else {
                    if v.used > e.used {
                        e.used = v.used;
                    }
                    if v.size > e.size {
                        e.size = v.size;
                    }
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
        // 首采样只建基线，不产生任何桶（历史量不属于「今天」）
        led.last_sample_at = at.to_string();
    } else if !touched {
        // 一个包都没读到（接口失败 / 账号空空）：累计值仍是「上次观测的最大值」，
        // 不会被累加，所以既没有增量可记，也**不能推进采样时刻** ——
        // 否则下一次真正读到时会凭空多出一段横跨整个空档的增量，
        // 全部落进一个尴尬的小时里，看起来就像那个小时突然花掉一大笔。
    } else if let Some((date, hour)) = parse_at(at) {
        let (granted, used) = (led.granted(), led.used());
        // `max(0)` 是防御：合并规则已保证累计不降，负增量只可能来自手改的 json
        let d_used = (used - before.1).max(0.0);
        let d_granted = (granted - before.0).max(0.0);
        // 归入「本次采样时刻」所属的小时。跨过整点的那段增量会整块落进后一个小时 ——
        // 采样越密越准，这正是「应用运行期间逐小时」的含义。
        if d_used > 0.0 {
            led.hours_used.entry(date.clone()).or_insert([0.0; 24])[hour] += d_used;
        }
        if d_granted > 0.0 {
            led.hours_granted.entry(date.clone()).or_insert([0.0; 24])[hour] += d_granted;
        }
        led.prune_hours(
            chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .unwrap_or_else(|_| chrono::Local::now().date_naive()),
        );
        led.last_sample_at = at.to_string();
    }

    // 注意：`last_sample_at` 只在上面各分支里推进，**不在这里兜底** ——
    // 「空视图不推进」正是靠它不被无条件覆盖。（`last_seen` 只用于展示，随便更新。）
    led.last_seen = at.to_string();
}

/// 从 `YYYY-MM-DD HH:MM:SS` 里取出（日期, 小时）。格式不对则返回 None（不记账，
/// 但不影响累计值 —— 宁可少一格明细，也不能写进错误的日期）。
fn parse_at(at: &str) -> Option<(String, usize)> {
    let (d, rest) = at.split_once(' ')?;
    let hour: usize = rest.get(0..2)?.parse().ok()?;
    if hour > 23 {
        return None;
    }
    // 顺便校验日期段，避免把畸形串写进 hours 的键
    chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
    Some((d.to_string(), hour))
}

/// 日报里的一行（一个账号）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CreditReportAccount {
    pub account_id: String,
    pub name: String,
    #[serde(default)]
    pub phone: Option<String>,
    /// 当天消耗（Σ 当天小时桶 · 消耗）
    pub consumed: f64,
    /// 当天新增（Σ 当天小时桶 · 授予）
    pub gained: f64,
    /// 结算时点的剩余积分（口径与账号列表「剩余积分」一致）
    #[serde(default)]
    pub balance: Option<f64>,
    /// 结算时点仍在计量的资源包个数（便于判断「没数据」还是「真的 0」）
    #[serde(default)]
    pub packages: usize,
    /// 该账号在这一天的 24 个消耗桶（下标 = 小时，未采样的小时为 0）
    #[serde(default)]
    pub hours_consumed: [f64; 24],
    /// 该账号在这一天的 24 个新增桶
    #[serde(default)]
    pub hours_gained: [f64; 24],
}

/// 某一天里 24 个小时的合计（跨全部账号）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HourTotal {
    /// 小时（0–23）
    pub hour: u8,
    pub consumed: f64,
    pub gained: f64,
}

/// 一条日报（每天一条，口径 = 自然日 00:00–24:00）
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CreditReport {
    /// 结算日 `YYYY-MM-DD`（列表按它倒序）
    pub date: String,
    /// 该条日报的生成时刻（当天 12:00 或手动结算；即「今天的分界点」）
    pub generated_at: String,
    /// 该条日报覆盖的窗口起点。新口径下固定为当天 `00:00:00`
    #[serde(default)]
    pub window_from: String,
    /// 窗口终点。当天为 `generated_at`（还没走完），封口后为次日 `00:00:00`
    #[serde(default)]
    pub window_to: String,
    /// 窗口是否已封闭（自然日已走完）
    #[serde(default)]
    pub sealed: bool,
    /// `0` = 这一天的数据实测逐小时可用；`1` = 自然日口径上线时对当天做的补算，
    /// 逐小时明细缺失（界面据此如实说明，不假装有小时数据）。
    #[serde(default)]
    pub granularity: u8,
    pub accounts: Vec<CreditReportAccount>,
    pub total_consumed: f64,
    pub total_gained: f64,
    /// 结算时点全部账号的剩余积分合计（全部取不到时为 None）
    #[serde(default)]
    pub total_balance: Option<f64>,
    /// 当天每小时合计（只列有数据的时点 —— 全列 24 行只会把界面灌满 0）
    #[serde(default)]
    pub hours: Vec<HourTotal>,
}

/// 快照的来源。决定它在列表里的地位，以及被谁覆盖。
///
/// - `System`：次日封口产生的**完整自然日**。日报列表的「合计」只算这些，任意两条可直接相加。
/// - `Manual`：用户点「当前累计」打的一次性读数。它是**临时样本**，用于和上一条对比看增量，
///   **不进合计** —— 同一天既有半截又有全天混进合计会重复计数。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotKind {
    System,
    Manual,
}

/// 一条快照：某个时刻的读数。与 [`CreditReport`] 的区别是**它不按天聚合**，
/// 而是「这一刻的累计消耗/新增/剩余」，因此两条快照相减就是这段时间的真实增量。
///
/// 刻意**不实现 `Default`**：`kind` 没有中立取值，凭空造一条 `System` 空快照
/// 会被 `push_snapshot` 当成真锚点，把用户手动积累的样本清掉。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    /// 打这一枪的时刻 `YYYY-MM-DD HH:MM:SS`
    pub at: String,
    /// 它属于哪一天（`YYYY-MM-DD`）—— 出于展示与排序的需要，不参与口径计算
    #[serde(default)]
    pub date: String,
    pub kind: SnapshotKind,
    /// 读数：从台账里对齐到的「此刻累计」
    ///
    /// 注意这不是余额而是**累计消耗/新增**，两条相减才有意义
    /// （余额相减会被「先消耗后签到」抵消掉）。
    pub consumed: f64,
    pub gained: f64,
    /// 截至此刻全部账号的剩余积分合计（全取不到时为 None）
    #[serde(default)]
    pub balance: Option<f64>,
    /// 参与统计的账号数
    #[serde(default)]
    pub accounts: usize,
}

/// 两条快照之间的增量（`newer - older`）。界面用它回答「这段时间到底用了多少」。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SnapshotDiff {
    /// 参照的那条（较早）
    pub from_at: String,
    /// 当前这条（较晚）
    pub to_at: String,
    /// 两条之间的时间跨度，按 `HH:MM:SS` 展开成总秒数（跨天也会正确累加）
    pub span_seconds: i64,
    pub consumed: f64,
    pub gained: f64,
}

impl Snapshot {
    /// 与更早的一条快照求差。`self` 是较晚的那条。
    ///
    /// 只做减法，不做任何「修正」：两边都是累计量，差值天然覆盖多客户端同时消耗。
    pub fn diff_from(&self, older: &Snapshot) -> SnapshotDiff {
        SnapshotDiff {
            from_at: older.at.clone(),
            to_at: self.at.clone(),
            span_seconds: seconds_between(&older.at, &self.at),
            consumed: round2(self.consumed - older.consumed),
            gained: round2(self.gained - older.gained),
        }
    }
}

/// 解析 `YYYY-MM-DD HH:MM:SS`，算出两个时刻相差多少秒（解析失败返回 0）。
///
/// 用 `NaiveDateTime` 而不是 `Local`：快照的字符串没有时区信息，
/// 而同一个进程写出的两条时间戳用同一种解释即可，不需要引入时区推断。
fn seconds_between(from: &str, to: &str) -> i64 {
    let parse = |s: &str| {
        chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()
    };
    match (parse(from), parse(to)) {
        (Some(a), Some(b)) => (b - a).num_seconds(),
        // 任一条读不出来就返回 0：界面上显示「跨度未知」比显示一个算错的数字好
        _ => 0,
    }
}

/// 从一个账号的桶里取出某天的值（缺失返回全 0）
fn day_buckets(buckets: &HourBuckets, date: &str) -> [f64; 24] {
    buckets.get(date).copied().unwrap_or([0.0; 24])
}

/// 汇总某一天的 24 个小时（跨全部账号），只保留有数据的时点。
fn hour_totals(accts: &BTreeMap<String, AcctLedger>, date: &str) -> Vec<HourTotal> {
    let mut sum_used = [0.0f64; 24];
    let mut sum_granted = [0.0f64; 24];
    for a in accts.values() {
        let hu = day_buckets(&a.hours_used, date);
        let hg = day_buckets(&a.hours_granted, date);
        for h in 0..24 {
            sum_used[h] += hu[h];
            sum_granted[h] += hg[h];
        }
    }
    (0..24)
        .filter(|&h| sum_used[h] > 0.0 || sum_granted[h] > 0.0)
        .map(|h| HourTotal {
            hour: h as u8,
            consumed: round2(sum_used[h]),
            gained: round2(sum_granted[h]),
        })
        .collect()
}

/// 生成一条日报。`date` 的 24 个小时桶已经在采样时归位，这里只做聚合 ——
/// 因此**不需要任何「封口」动作**，也就不存在「封口时机没对上导致丢数据」的风险。
///
/// `sealed` 表示这个自然日是否已经走完（当天 12:00 结算时为 false）。
pub fn build_report(
    accts: &BTreeMap<String, AcctLedger>,
    accounts: &[Account],
    balances: &BTreeMap<String, Option<f64>>,
    date: &str,
    generated_at: &str,
    sealed: bool,
    granularity: u8,
) -> CreditReport {
    let mut rows = Vec::with_capacity(accounts.len());
    let (mut total_consumed, mut total_gained) = (0.0f64, 0.0f64);
    let mut total_balance = 0.0f64;
    let mut any_balance = false;

    for a in accounts {
        let entry = accts.get(&a.id);
        let empty = [0.0f64; 24];
        let hours_consumed = entry.map_or(empty, |e| day_buckets(&e.hours_used, date));
        let hours_gained = entry.map_or(empty, |e| day_buckets(&e.hours_granted, date));
        let consumed = hours_consumed.iter().sum::<f64>();
        let gained = hours_gained.iter().sum::<f64>();
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
            hours_consumed: hours_consumed.map(round2),
            hours_gained: hours_gained.map(round2),
        });
    }

    // 明细按消耗从多到少排：看日报的第一诉求是「谁在花」，而不是「谁在最前面」
    rows.sort_by(|a, b| {
        b.consumed
            .partial_cmp(&a.consumed)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.gained.partial_cmp(&a.gained).unwrap_or(std::cmp::Ordering::Equal))
    });

    CreditReport {
        date: date.to_string(),
        generated_at: generated_at.to_string(),
        window_from: format!("{date} 00:00:00"),
        window_to: if sealed {
            // 次日 00:00:00 —— 用日期加法而不是「加 24 小时」，避免夏令时把边界挪掉一小时
            chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .map(|d| format!("{} 00:00:00", d + chrono::Duration::days(1)))
                .unwrap_or_else(|_| generated_at.to_string())
        } else {
            generated_at.to_string()
        },
        sealed,
        granularity,
        accounts: rows,
        total_consumed: round2(total_consumed),
        total_gained: round2(total_gained),
        total_balance: any_balance.then(|| round2(total_balance)),
        hours: hour_totals(accts, date),
    }
}

/// 把台账里比 `KEEP_DAYS` 旧的桶清掉。
///
/// `merge_account` 每次写入时已经会顺手剪一次，这里是给「长期没采样、刚打开应用」
/// 的情况补一刀：否则首次采样前文件里可能还躺着几个月前的桶。
pub fn prune_buckets(accts: &mut BTreeMap<String, AcctLedger>, today: chrono::NaiveDate) {
    for a in accts.values_mut() {
        a.prune_hours(today);
    }
}

/// 供 `commands` 复用（`round2` 是私有助手）
pub fn round2_public(v: f64) -> f64 {
    round2(v)
}

/// 供 `commands` 复用（`save_reports` 是私有助手）
pub fn save_reports_public(dir: &Path, reports: &[CreditReport]) -> std::io::Result<()> {
    save_reports(dir, reports)
}

/// 旧口径 → 自然日口径的**一次性**迁移：把「旧口径下今天已累计、但还没有小时桶」
/// 的那部分补进当天的 00 点桶。
///
/// 背景：自然日口径上线前，增量被记在「结算基线」里而不是小时桶里。升级那一刻，
/// 当天已经发生的消耗/新增既不在任何桶里，也不该被丢掉 —— 所以就地问一次
/// `(当前累计 − 旧基线)`，作为当天 00 点的量补上，返回补了多少供调用方标注
/// `granularity = 1`。
///
/// **幂等**：补完立刻把旧基线推到当前值，因此同一天重复调用不会重复补；
/// 一旦这天有了任何桶（说明已经在新口径下正常记账）就直接跳过。
pub fn reconcile_day_baseline(e: &mut AcctLedger, date: &str) -> (f64, f64) {
    if !e.seeded {
        return (0.0, 0.0);
    }
    let has_today = e.hours_used.contains_key(date) || e.hours_granted.contains_key(date);
    if has_today {
        // 已有当天的桶：说明新口径已经在为这天记账，基线也早就对齐了
        e.base_granted = e.granted();
        e.base_used = e.used();
        return (0.0, 0.0);
    }
    let d_used = (e.used() - e.base_used).max(0.0);
    let d_granted = (e.granted() - e.base_granted).max(0.0);
    if d_used > 0.0 {
        e.hours_used.entry(date.to_string()).or_insert([0.0; 24])[0] += d_used;
    }
    if d_granted > 0.0 {
        e.hours_granted.entry(date.to_string()).or_insert([0.0; 24])[0] += d_granted;
    }
    e.base_granted = e.granted();
    e.base_used = e.used();
    (d_used, d_granted)
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

/// 日报文件的包装：带 schema 版本。
///
/// 旧格式（无 `v` 字段）是「滑动窗口 + 结算基线」那一版，字段语义已变，不能混读 ——
/// 所以加载时**直接丢弃**旧文件（`decode_reports` 里 `v` 缺失即返回空）。
/// 丢掉的只是展示历史，不影响台账累计值（`credit_ledger.json` 另存），
/// 而且刚开始统计时本来也没几条。
#[derive(Serialize, Deserialize)]
struct ReportsFile {
    v: u32,
    reports: Vec<CreditReport>,
}

/// 日报文件的 schema 版本（本版为 2）
const REPORTS_SCHEMA: u32 = 2;

/// 读日报：**新的在前**（界面直接顺序渲染）
pub fn load_reports(dir: &Path) -> Vec<CreditReport> {
    decode_reports(&fs::read_to_string(reports_file(dir)).unwrap_or_default())
}

/// 解析日报文件。抽出来是为了能单测「旧格式被安全丢弃」这件事 ——
/// 这类「读旧文件读到错数据」的 bug 只在真实升级路径上出现，手点很难覆盖。
fn decode_reports(raw: &str) -> Vec<CreditReport> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let Ok(f) = serde_json::from_str::<ReportsFile>(raw) else {
        return Vec::new();
    };
    if f.v != REPORTS_SCHEMA {
        return Vec::new();
    }
    f.reports
}

fn save_reports(dir: &Path, reports: &[CreditReport]) -> std::io::Result<()> {
    let f = ReportsFile {
        v: REPORTS_SCHEMA,
        reports: reports.to_vec(),
    };
    write_atomic(&reports_file(dir), &serde_json::to_string_pretty(&f)?)
}

/// 按 `date` 写入一条日报：同一天**覆盖**，不同天才新增；并维持排序与条数上限。
///
/// 同一天覆盖是必须的：封口会把某天从「已有数据」重算成「完整一天」，
/// 若追加而不是覆盖，列表里就会出现同一个日期的两条记录。
///
/// **生产代码已不再走这条路径**：日报列表现在只由
/// [`crate::commands::seal_reports`] 通过「读全量 → 就地改 → [`save_reports_public`]」
/// 生成，而「当前累计」是一次性快照、根本不落盘。这里保留下来是因为
/// 「同日覆盖 + 条数上限」这两个不变量需要单测固定住（见
/// `upsert_replaces_the_same_day_and_keeps_newest_first`），
/// 加 `#[cfg(test)]` 是为了不留一条永远不执行的生产代码路径。
#[cfg(test)]
pub fn upsert_report(dir: &Path, rep: CreditReport) -> std::io::Result<()> {
    let mut all = load_reports(dir);
    match all.iter().position(|r| r.date == rep.date) {
        Some(i) => all[i] = rep,
        None => all.insert(0, rep),
    }
    all.sort_by(|a, b| b.date.cmp(&a.date)); // 新的在前
    all.truncate(MAX_REPORTS);
    save_reports(dir, &all)
}

/// 落盘日报列表前的**统一整理**：按日期降序 + 截断到上限。
///
/// 封口路径是「读全量 → 就地改 → 整体写回」，不经过写入单条的函数，
/// 所以必须在这里再收一次口，否则日报会无限增长（曾经就漏了这一步）。
pub fn normalize_reports(reports: &mut Vec<CreditReport>) {
    reports.sort_by(|a, b| b.date.cmp(&a.date));
    reports.truncate(MAX_REPORTS);
}

/// 清空全部日报（不影响台账与基线）
pub fn clear_reports(dir: &Path) -> std::io::Result<()> {
    save_reports(dir, &[])
}

// ── 快照存储 ─────────────────────────────────────────────────────
//
// 快照与日报是**两份数据**：日报是「一天一条的聚合」，快照是「某一刻的读数」。
// 分开存是因为写入规则完全不同 —— 日报按 date 覆盖，快照要按 kind 决定覆盖还是累积。

/// 快照文件的包装（带 schema 版本，风格与 [ReportsFile] 一致）
#[derive(Serialize, Deserialize)]
struct SnapshotsFile {
    v: u32,
    snapshots: Vec<Snapshot>,
}

/// 快照文件 schema 版本
const SNAPSHOTS_SCHEMA: u32 = 1;

pub fn snapshots_file(dir: &Path) -> PathBuf {
    dir.join("credit_snapshots.json")
}

/// 读快照：**新的在前**（界面直接顺序渲染）
pub fn load_snapshots(dir: &Path) -> Vec<Snapshot> {
    decode_snapshots(&fs::read_to_string(snapshots_file(dir)).unwrap_or_default())
}

/// 解析快照文件。版本对不上即丢弃（与日报同一套策略：宁可少几条展示，也不混读旧语义）。
fn decode_snapshots(raw: &str) -> Vec<Snapshot> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let Ok(f) = serde_json::from_str::<SnapshotsFile>(raw) else {
        return Vec::new();
    };
    if f.v != SNAPSHOTS_SCHEMA {
        return Vec::new();
    }
    f.snapshots
}

pub fn save_snapshots(dir: &Path, snaps: &[Snapshot]) -> std::io::Result<()> {
    let f = SnapshotsFile {
        v: SNAPSHOTS_SCHEMA,
        snapshots: snaps.to_vec(),
    };
    write_atomic(&snapshots_file(dir), &serde_json::to_string_pretty(&f)?)
}

/// 把一条新快照并入列表 —— **这里是四条规则唯一落地的地方**。
///
/// 规则（用户拍板，`kind` 决定命运）：
///
/// | 已有 | 新来 | 结果 |
/// |---|---|---|
/// | 无 | 手动 | 存下，等对比 |
/// | 手动（无系统快照） | 手动 | **并存** —— 自由样本，可两两对比 |
/// | 系统 + 手动 | 手动 | **覆盖那条手动** —— 有锚点后手动只剩一格 |
/// | 系统 + 手动 | 系统 | **覆盖那条手动**，系统也只留最新 —— 手动作废 |
///
/// 归纳成一句话：**只剩最新一条系统快照 + 手动快照若干（有系统快照时压缩成一条）**。
///
/// 为什么「有系统快照就压缩手动」：系统快照是完整自然日，是唯一能进合计的口径。
/// 手动快照在它出现之后就只是「日内临时读数」，留多条没有意义，还会让界面上
/// 同一天的样本越堆越多；而在没有系统快照时（刚装上、或还没到次日），
/// 手动快照就是唯一的对比依据，必须允许累积。
pub fn push_snapshot(snaps: &mut Vec<Snapshot>, snap: Snapshot) {
    match snap.kind {
        SnapshotKind::System => {
            // 系统快照：先清掉全部手动样本（它们已经被这一枪「覆盖」掉了），
            // 再清掉旧系统快照 —— 系统快照本身也只留最新一条，否则同一天封口两次就会两条。
            snaps.retain(|s| s.kind != SnapshotKind::Manual);
            snaps.retain(|s| s.kind != SnapshotKind::System);
            snaps.push(snap);
        }
        SnapshotKind::Manual => {
            let has_system = snaps.iter().any(|s| s.kind == SnapshotKind::System);
            if has_system {
                // 有锚点：手动只剩一格 —— 覆盖掉之前那条手动
                snaps.retain(|s| s.kind != SnapshotKind::Manual);
                snaps.push(snap);
            } else {
                // 没锚点：累积，供两两对比。上限兜住「连点几十次」的极端情况。
                snaps.insert(0, snap);
                // 保留最新的 MAX_MANUAL_SNAPSHOTS 条（列表是新的在前）
                if snaps.len() > MAX_MANUAL_SNAPSHOTS {
                    snaps.truncate(MAX_MANUAL_SNAPSHOTS);
                }
            }
        }
    }
    snaps.sort_by(|a, b| b.at.cmp(&a.at)); // 新的在前
}

/// 把每条快照与它**前面那一条**配对求差，返回 `(下标, 增量)`。
///
/// 下标指的是「较晚那条」在 `snaps` 里的位置，前端按它把增量贴到对应行上；
/// 最老的一条没有前辈，不出现在结果里（界面显示「首个样本」而不是一堆 0）。
///
/// 这里直接用「后一位」而不是比较时间戳：`snaps` 保证新的在前，位置关系就是
/// 时间关系，且同一秒内连点两次也不会互相干扰 —— 时间戳会撞，下标不会。
pub fn snapshot_diffs(snaps: &[Snapshot]) -> Vec<(usize, SnapshotDiff)> {
    snaps
        .iter()
        .enumerate()
        .filter_map(|(i, s)| snaps.get(i + 1).map(|older| (i, s.diff_from(older))))
        .collect()
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
    // 当天还没走完时要讲清楚：这条是「到此刻为止」，不是全天
    s.push_str(if rep.sealed {
        "\n统计范围：全天 00:00–24:00"
    } else {
        "\n统计范围：今天 00:00 至此刻（当天尚未结束）"
    });
    // 明细只列有变化的账号，避免推送被一串 0 刷屏
    let mut lines: Vec<String> = rep
        .accounts
        .iter()
        .filter(|a| a.consumed > 0.0 || a.gained > 0.0)
        .map(|a| format!("· {} 耗 {:.2} / 增 {:.2}", a.name, a.consumed, a.gained))
        .collect();
    if lines.is_empty() {
        lines.push("· 今天还没有账号产生消耗或新增".to_string());
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

    /// 采样并顺手返回该账号的桶（绝大多数用例只关心桶，不关心累计值）
    fn sample(led: &mut Ledger, id: &str, views: &[PkgView], at: &str) {
        merge_account(led.accts.entry(id.into()).or_default(), views, at);
    }

    /// 只给某个账号建一次基线（首采样不产生桶）
    fn seed(led: &mut Ledger, id: &str, views: &[PkgView], at: &str) {
        sample(led, id, views, at);
    }

    // ── 累计值不倒退（原有不变量，必须继续成立）─────────────────────

    #[test]
    fn merge_keeps_maxima_so_an_expiring_package_never_rewinds_the_total() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("a", 100.0, 40.0, "d1")], "2026-09-15 10:00:00");
        assert_eq!((led.granted(), led.used()), (100.0, 40.0));

        merge_account(&mut led, &[pkg("a", 100.0, 70.0, "d1")], "2026-09-15 11:00:00");
        assert_eq!(led.used(), 70.0);

        // 包过期、从响应里消失 —— 合计必须保持 70，不能掉回 0
        merge_account(&mut led, &[], "2026-09-15 12:00:00");
        assert_eq!((led.granted(), led.used()), (100.0, 70.0));
    }

    #[test]
    fn cycle_rollover_archives_the_old_cycle_instead_of_rewinding() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("a", 500.0, 320.0, "9月")], "2026-09-30 10:00:00");
        // 翻到新周期：同一个月度包，已用量归零
        merge_account(&mut led, &[pkg("a", 500.0, 0.0, "10月")], "2026-10-01 10:00:00");
        assert_eq!(led.used(), 320.0, "归零应被归档，合计不得倒退");
        // 新周期里再消耗 80 → 合计 400
        merge_account(&mut led, &[pkg("a", 500.0, 80.0, "10月")], "2026-10-01 11:00:00");
        assert_eq!(led.used(), 400.0);
        // 授予量：归档的 500 + 新周期的 500。**周期重置后授予量确实又发了一次**，
        // 所以这里不是 500 而是 1000 —— 它是「累计授予」这一计数器本身的语义。
        // 「不重复计入新增」靠的是增量记账（旧周期没产生 500 的授予增量），
        // 而不是靠把累计值压在 500。
        assert_eq!(led.granted(), 1000.0);
    }

    /// 回归：授予量会随周期回退的包（「授予量 × 已用比例」型）。
    ///
    /// 关键在于 `granted()` 必须**单调不降**：一旦它掉下去，而增量又按 `max(0)` 记账，
    /// 那个缺口永远补不回来 —— 之后每次给这个包授予积分都会被缺口先吃掉一部分，
    /// 表现为「新增积分要等好久才显示出来」。
    /// 这条断言就是钉住这个坑。
    #[test]
    fn rollover_that_also_shrinks_the_grant_keeps_granted_monotonic() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("m", 100.0, 90.0, "9月")], "2026-09-30 10:00:00");
        assert_eq!(led.granted(), 100.0);

        // 新周期：授予量缩到 50、已用归零 → 累计 = 归档 100 + 新周期 50
        merge_account(&mut led, &[pkg("m", 50.0, 0.0, "10月")], "2026-10-01 10:00:00");
        assert_eq!(led.granted(), 150.0, "授予量不得因周期重置而回退");
        assert_eq!(led.used(), 90.0);

        // 同一个周期内，观测值从 50 涨回 100：这就是真实发生的「又授予 50」。
        // **必须报成 +50 的新增** —— 若此时还拿归档的 100 去取 max，
        // 新增会被永久吞掉，用户看到的就是「发了积分但日报不动」。
        merge_account(&mut led, &[pkg("m", 100.0, 0.0, "10月")], "2026-10-01 11:00:00");
        assert_eq!(led.granted(), 200.0, "新周期内涨的量必须算进累计，不能被归档值盖住");
    }

    /// 与上一条相反的情形：新周期的授予量**更高**时，高出来的部分必须是「新增」。
    ///
    /// 这条是防「归档值赖着不走」：若翻周期时把旧观测留在 `e.size` 里取 max，
    /// 它会在新周期的每次采样里都盖住真实值，等于**永久吞掉该包后续的全部新增**
    /// ——用户在界面上会看到「签到发了积分但日报一直是 0」。
    /// 归档 + 用新观测替换，才能让两个不变量同时成立。
    #[test]
    fn rollover_that_raises_the_grant_reports_the_increase_as_gained() {
        let mut led = AcctLedger::default();
        merge_account(&mut led, &[pkg("m", 100.0, 90.0, "9月")], "2026-09-30 10:00:00");

        // 新周期授予量升到 120：累计 = 归档 100 + 新周期 120 = 220
        merge_account(&mut led, &[pkg("m", 120.0, 0.0, "10月")], "2026-10-01 10:00:00");
        assert_eq!(led.granted(), 220.0);
        assert_eq!(led.used(), 90.0, "已用量归零要归档，不能倒退");
    }

    // ── 小时桶 ────────────────────────────────────────────────

    #[test]
    fn first_sample_only_seeds_the_baseline_and_writes_no_bucket() {
        // 账号里本来就躺着 100 授予 / 90 已用（历史用量）。
        // 第一次采样只建立基线：不产生任何小时桶，否则历史量会被算成「今天的消耗」。
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("a", 100.0, 90.0, "d1")], "2026-09-15 09:30:00");
        let a = &led.accts["a1"];
        assert!(a.seeded);
        assert!(a.hours_used.is_empty(), "首采样不该写桶");
        assert!(a.hours_granted.is_empty());

        // 此后真的又消耗 10 → 只报这 10
        sample(&mut led, "a1", &[pkg("a", 100.0, 100.0, "d1")], "2026-09-15 09:45:00");
        assert_eq!(day_buckets(&led.accts["a1"].hours_used, "2026-09-15")[9], 10.0);
    }

    #[test]
    fn increments_land_in_the_hour_of_the_sample_that_observed_them() {
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("p", 1000.0, 0.0, "d")], "2026-09-15 08:00:00");

        // 08 点这一段 +30
        sample(&mut led, "a1", &[pkg("p", 1000.0, 30.0, "d")], "2026-09-15 08:59:00");
        // 跨过整点后的一次采样：整段 +20 归入 09 点
        sample(&mut led, "a1", &[pkg("p", 1000.0, 50.0, "d")], "2026-09-15 09:01:00");
        // 11 点（10 点整点之前那段没有采样，不产生桶）
        sample(&mut led, "a1", &[pkg("p", 1000.0, 80.0, "d")], "2026-09-15 11:30:00");

        let hu = day_buckets(&led.accts["a1"].hours_used, "2026-09-15");
        assert_eq!(hu[8], 30.0);
        assert_eq!(hu[9], 20.0);
        assert_eq!(hu[10], 0.0, "没有采样的时段不该凭空有值");
        assert_eq!(hu[11], 30.0);
        // 桶之和 = 当天累计增量
        assert_eq!(hu.iter().sum::<f64>(), 80.0);
    }

    #[test]
    fn a_sample_landing_in_the_next_day_opens_that_day_bucket() {
        // 跨天采样：增量整块记进「采样时刻」那一天，前一天的桶不受影响 ——
        // 这正是「不需要封口动作」的原因。
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("p", 100.0, 0.0, "d")], "2026-09-15 23:00:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 70.0, "d")], "2026-09-15 23:50:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 90.0, "d")], "2026-09-16 00:10:00");

        let a = &led.accts["a1"];
        assert_eq!(day_buckets(&a.hours_used, "2026-09-15")[23], 70.0);
        assert_eq!(day_buckets(&a.hours_used, "2026-09-16")[0], 20.0);
        // 两天相加 = 累计增量，不重不漏
        let total: f64 = a.hours_used.values().flatten().sum();
        assert_eq!(total, 90.0);
    }

    #[test]
    fn an_empty_view_does_not_create_a_delta_or_move_the_sample_clock() {
        // 接口失败 / 账号一个包都没有时 `views` 为空。这**不是**「没有变化」，
        // 只是这一轮没读到 —— 累计值仍是上次的最大值，所以不该产生增量，
        // 采样时刻也不能动，否则下次真读到时会凭空多出一段横跨空档的增量。
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("p", 100.0, 0.0, "d")], "2026-09-15 09:00:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 40.0, "d")], "2026-09-15 10:00:00");

        sample(&mut led, "a1", &[], "2026-09-15 11:00:00");
        let a = &led.accts["a1"];
        assert!(a.hours_used.is_empty() || day_buckets(&a.hours_used, "2026-09-15")[11] == 0.0);
        assert_eq!(
            a.last_sample_at, "2026-09-15 10:00:00",
            "空视图不该推进采样时刻"
        );
    }

    #[test]
    fn negative_movement_is_never_recorded_as_a_bucket() {
        // 包过期消失 / 周期重置都不是「消耗」，不能写成负数把某天的量冲掉
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("p", 500.0, 300.0, "9月")], "2026-09-30 10:00:00");
        sample(&mut led, "a1", &[pkg("p", 500.0, 0.0, "10月")], "2026-10-01 09:00:00");
        assert!(
            day_buckets(&led.accts["a1"].hours_used, "2026-10-01").iter().all(|v| *v == 0.0),
            "周期重置不产生桶"
        );
        assert_eq!(led.accts["a1"].used(), 300.0, "但累计值保住了");
    }

    #[test]
    fn a_malformed_timestamp_is_skipped_without_corrupting_the_bucket_map() {
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("p", 100.0, 0.0, "d")], "2026-09-15 10:00:00");
        sample(&mut led, "a1", &[pkg("p", 100.0, 40.0, "d")], "坏时刻");
        let a = &led.accts["a1"];
        assert!(a.hours_used.is_empty(), "畸形时刻不记账，也不能写进错误的日期");
        assert_eq!(a.used(), 40.0, "累计值照常更新");
    }

    #[test]
    fn buckets_are_pruned_but_recent_days_stay() {
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("p", 1000.0, 0.0, "d")], "2026-01-01 00:00:00");
        // 造两个桶：一个远超出保留期、一个就在「今天」。
        // 注意第一次 sample 时 2026-01-02 就已经超期了（远离 03-20），会被当场剪掉 ——
        // 剪枝是在**每次写入时**做的，不是只在读的时候。
        sample(&mut led, "a1", &[pkg("p", 1000.0, 10.0, "d")], "2026-01-02 05:00:00");
        sample(&mut led, "a1", &[pkg("p", 1000.0, 20.0, "d")], "2026-03-20 05:00:00");
        assert_eq!(led.accts["a1"].hours_used.len(), 1, "超期桶当场被剪掉");
        assert!(led.accts["a1"].hours_used.contains_key("2026-03-20"));

        // 保留期**之内**的旧天不该被剪掉
        let mut led2 = Ledger::default();
        seed(&mut led2, "a1", &[pkg("p", 1000.0, 0.0, "d")], "2026-03-19 00:00:00");
        sample(&mut led2, "a1", &[pkg("p", 1000.0, 10.0, "d")], "2026-03-19 05:00:00");
        sample(&mut led2, "a1", &[pkg("p", 1000.0, 20.0, "d")], "2026-03-20 05:00:00");
        assert_eq!(led2.accts["a1"].hours_used.len(), 2, "昨天的桶必须留着");

        // 显式剪枝到某天：比保留期旧的清掉，近期的留下
        let today = chrono::NaiveDate::parse_from_str("2026-06-01", "%Y-%m-%d").unwrap();
        prune_buckets(&mut led2.accts, today);
        assert!(led2.accts["a1"].hours_used.is_empty(), "3 月的桶在 6 月该清掉");
    }

    // ── 按天聚合 ──────────────────────────────────────────────

    #[test]
    fn report_totals_are_the_sum_of_that_days_buckets() {
        let mut led = Ledger::default();
        let a1 = account("a1", "甲");
        let a2 = account("a2", "乙");
        seed(&mut led, "a1", &[pkg("p1", 100.0, 0.0, "d")], "2026-09-15 08:00:00");
        seed(&mut led, "a2", &[pkg("p2", 200.0, 0.0, "d")], "2026-09-15 08:00:00");

        // 甲：消耗 30 又拿到新包 100；乙：消耗 50
        sample(&mut led, "a1", &[pkg("p1", 100.0, 30.0, "d")], "2026-09-15 09:00:00");
        sample(
            &mut led,
            "a1",
            &[pkg("p1", 100.0, 30.0, "d"), pkg("p1b", 100.0, 0.0, "d2")],
            "2026-09-15 10:00:00",
        );
        sample(&mut led, "a2", &[pkg("p2", 200.0, 50.0, "d")], "2026-09-15 11:00:00");

        let mut balances = BTreeMap::new();
        balances.insert("a1".to_string(), Some(170.0));
        balances.insert("a2".to_string(), Some(145.0));

        let rep = build_report(
            &led.accts,
            &[a1, a2],
            &balances,
            "2026-09-15",
            "2026-09-15 12:00:00",
            false,
            0,
        );

        let row = |id: &str| rep.accounts.iter().find(|r| r.account_id == id).unwrap().clone();
        assert_eq!((row("a1").consumed, row("a1").gained), (30.0, 100.0));
        assert_eq!((row("a2").consumed, row("a2").gained), (50.0, 0.0));
        assert_eq!((rep.total_consumed, rep.total_gained), (80.0, 100.0));
        assert_eq!(rep.total_balance, Some(315.0));
        assert_eq!(row("a1").packages, 2);
        // 窗口按自然日固定
        assert_eq!(rep.window_from, "2026-09-15 00:00:00");
        assert!(!rep.sealed);
        assert_eq!(rep.window_to, "2026-09-15 12:00:00", "当天未封闭，终点=结算时刻");
    }

    #[test]
    fn a_sealed_day_spans_the_full_calendar_day() {
        let led = Ledger::default();
        let rep = build_report(
            &led.accts,
            &[account("a1", "甲")],
            &BTreeMap::new(),
            "2026-09-15",
            "2026-09-16 00:05:00",
            true,
            0,
        );
        assert_eq!(rep.window_from, "2026-09-15 00:00:00");
        assert_eq!(rep.window_to, "2026-09-16 00:00:00");
        assert!(rep.sealed);
    }

    #[test]
    fn hour_totals_aggregate_accounts_and_skip_empty_hours() {
        let mut led = Ledger::default();
        seed(&mut led, "a1", &[pkg("p1", 1000.0, 0.0, "d")], "2026-09-15 08:00:00");
        seed(&mut led, "a2", &[pkg("p2", 1000.0, 0.0, "d")], "2026-09-15 08:00:00");
        sample(&mut led, "a1", &[pkg("p1", 1000.0, 12.0, "d")], "2026-09-15 09:00:00");
        sample(&mut led, "a2", &[pkg("p2", 1000.0, 8.0, "d")], "2026-09-15 09:30:00");
        sample(&mut led, "a1", &[pkg("p1", 1000.0, 20.0, "d")], "2026-09-15 14:00:00");

        let rep = build_report(
            &led.accts,
            &[account("a1", "甲"), account("a2", "乙")],
            &BTreeMap::new(),
            "2026-09-15",
            "2026-09-15 15:00:00",
            false,
            0,
        );

        let hours: Vec<(u8, f64)> = rep.hours.iter().map(|h| (h.hour, h.consumed)).collect();
        assert_eq!(
            hours,
            vec![(9, 20.0), (14, 8.0)],
            "两个账号 9 点合起来 20，14 点 8；空小时不入列表"
        );
        // 小时合计必须与当天合计一致（同一份桶的两种聚合）
        let sum: f64 = rep.hours.iter().map(|h| h.consumed).sum();
        assert_eq!(round2(sum), rep.total_consumed);
    }

    #[test]
    fn accounts_are_sorted_by_consumption_desc() {
        let mut led = Ledger::default();
        seed(&mut led, "small", &[pkg("p", 100.0, 0.0, "d")], "2026-09-15 08:00:00");
        seed(&mut led, "big", &[pkg("q", 900.0, 0.0, "d")], "2026-09-15 08:00:00");
        sample(&mut led, "small", &[pkg("p", 100.0, 5.0, "d")], "2026-09-15 09:00:00");
        sample(&mut led, "big", &[pkg("q", 900.0, 500.0, "d")], "2026-09-15 09:00:00");

        let rep = build_report(
            &led.accts,
            &[account("small", "小"), account("big", "大")],
            &BTreeMap::new(),
            "2026-09-15",
            "2026-09-15 12:00:00",
            false,
            0,
        );
        assert_eq!(rep.accounts[0].account_id, "big", "消耗多的排前面");
    }

    #[test]
    fn no_balance_reading_yields_none_instead_of_a_misleading_zero() {
        let led = Ledger::default();
        let rep = build_report(
            &led.accts,
            &[account("a1", "甲")],
            &BTreeMap::new(),
            "d",
            "t",
            false,
            0,
        );
        assert_eq!(rep.total_balance, None);
        assert_eq!(rep.accounts[0].balance, None);
    }

    // ── 落盘 ─────────────────────────────────────────────────

    #[test]
    fn upsert_replaces_the_same_day_and_keeps_newest_first() {
        let dir = std::env::temp_dir().join(format!("wba-ledger-{}", uuid::Uuid::new_v4()));
        let mk = |d: &str, consumed: f64| CreditReport {
            date: d.into(),
            total_consumed: consumed,
            ..Default::default()
        };

        upsert_report(&dir, mk("2026-09-15", 10.0)).unwrap();
        upsert_report(&dir, mk("2026-09-14", 5.0)).unwrap();
        let all = load_reports(&dir);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].date, "2026-09-15", "最新的必须排在最前");

        // 同一天再写一次：覆盖而不是追加（结算只是重新聚合当天）
        upsert_report(&dir, mk("2026-09-15", 42.0)).unwrap();
        let all = load_reports(&dir);
        assert_eq!(all.len(), 2, "同一天不该出现两条");
        assert_eq!(all[0].total_consumed, 42.0);
        assert_eq!(all[0].date, "2026-09-15");

        for i in 0..MAX_REPORTS + 5 {
            upsert_report(&dir, mk(&format!("2026-01-{i:02}"), 1.0)).unwrap();
        }
        assert_eq!(load_reports(&dir).len(), MAX_REPORTS);

        clear_reports(&dir).unwrap();
        assert!(load_reports(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 旧格式（无 schema 版本）必须被安全丢弃：它的 `total_consumed` 是「上次结算以来」的
    /// 滑动窗口口径，混进新的自然日列表会让人以为某天消耗异常大。
    #[test]
    fn reports_written_in_the_old_schema_are_discarded_on_load() {
        let legacy = r#"[
          {"date":"2026-09-14","generated_at":"2026-09-14 12:00:00",
           "window_from":"2026-09-13 12:00:00","window_to":"2026-09-14 12:00:00",
           "accounts":[],"total_consumed":999.0,"total_gained":0.0,"samples":3}
        ]"#;
        assert!(decode_reports(legacy).is_empty(), "旧格式应被丢弃");
        assert!(decode_reports("").is_empty());
        assert!(decode_reports("{ 不是 json").is_empty());

        // 新格式则正常读回
        let rep = CreditReport {
            date: "2026-09-15".into(),
            total_consumed: 7.0,
            ..Default::default()
        };
        let fresh = serde_json::to_string(&ReportsFile {
            v: REPORTS_SCHEMA,
            reports: vec![rep],
        })
        .unwrap();
        assert_eq!(decode_reports(&fresh).len(), 1);
    }

    #[test]
    fn ledger_round_trips_and_tolerates_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("wba-ledger-{}", uuid::Uuid::new_v4()));
        assert!(load_ledger(&dir).accts.is_empty(), "文件不存在时应是空台账");
        let mut led = Ledger::default();
        merge_account(led.accts.entry("a1".into()).or_default(), &[pkg("p", 5.0, 1.0, "d")], "t");
        save_ledger(&dir, &led).unwrap();
        let back = load_ledger(&dir);
        assert_eq!(back.accts["a1"].used(), 1.0);
        assert!(back.accts["a1"].seeded);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn message_lists_only_accounts_with_movement() {
        let rep = CreditReport {
            date: "2026-09-15".into(),
            generated_at: "2026-09-15 12:00:00".into(),
            window_from: "2026-09-15 00:00:00".into(),
            window_to: "2026-09-15 12:00:00".into(),
            accounts: vec![
                CreditReportAccount {
                    account_id: "a".into(),
                    name: "甲".into(),
                    consumed: 12.5,
                    gained: 100.0,
                    balance: Some(1.0),
                    packages: 1,
                    ..Default::default()
                },
                CreditReportAccount {
                    account_id: "b".into(),
                    name: "乙".into(),
                    consumed: 0.0,
                    gained: 0.0,
                    balance: Some(1.0),
                    packages: 1,
                    ..Default::default()
                },
            ],
            total_consumed: 12.5,
            total_gained: 100.0,
            total_balance: Some(2.0),
            ..Default::default()
        };
        let m = report_message(&rep);
        assert!(m.contains("消耗 12.50"));
        assert!(m.contains("新增 100.00"));
        assert!(m.contains("甲 耗 12.50 / 增 100.00"));
        assert!(m.contains("今天 00:00 至此刻"), "当天未结束时文案要说清范围");
        assert!(!m.contains("乙"), "没有变化的账号不该出现在明细里：{m}");

        // 封口后的文案换成全天
        let sealed = CreditReport {
            sealed: true,
            ..rep.clone()
        };
        assert!(report_message(&sealed).contains("全天 00:00–24:00"));
    }


    // ── 快照写入规则（用户拍板的四条，逐条钉死） ──────────────────

    fn snap(at: &str, kind: SnapshotKind, consumed: f64, gained: f64) -> Snapshot {
        Snapshot {
            at: at.into(),
            date: at[..10].to_string(),
            kind,
            consumed,
            gained,
            balance: Some(1000.0),
            accounts: 2,
        }
    }

    fn manual(at: &str, c: f64, g: f64) -> Snapshot {
        snap(at, SnapshotKind::Manual, c, g)
    }
    fn system(at: &str, c: f64, g: f64) -> Snapshot {
        snap(at, SnapshotKind::System, c, g)
    }

    /// 规则 1：没有任何快照时手动打一条 —— 存下（等对比），不结算
    #[test]
    fn first_manual_snapshot_is_kept_as_a_baseline() {
        let mut v = Vec::new();
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 100.0, 50.0));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, SnapshotKind::Manual);
        // 它是第一条 ⇒ 没有参照物，不出现在差值表里
        assert!(snapshot_diffs(&v).is_empty());
    }

    /// 规则 1'：**没有系统快照**时，手动快照可以累积并存（自由样本，可两两对比）
    #[test]
    fn manual_snapshots_accumulate_while_no_system_snapshot_exists() {
        let mut v = Vec::new();
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 100.0, 50.0));
        push_snapshot(&mut v, manual("2026-09-15 12:00:00", 130.0, 50.0));
        push_snapshot(&mut v, manual("2026-09-15 15:00:00", 150.0, 60.0));

        assert_eq!(v.len(), 3, "没有锚点时必须并存，否则无法对比：{v:?}");
        assert!(v.iter().all(|s| s.kind == SnapshotKind::Manual));
        // 新的在前
        assert_eq!(v[0].at, "2026-09-15 15:00:00");
        // 三条 ⇒ 两对差值；最新那条的参照是次新的那条
        let diffs = snapshot_diffs(&v);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].0, 0, "第一对贴在最新那条上");
        assert_eq!(diffs[0].1.from_at, "2026-09-15 12:00:00");
    }

    /// 规则 2：存在快照时手动打一条 —— 能和已有的那条对比（差值 = 真实增量）
    #[test]
    fn manual_snapshot_can_be_compared_with_the_previous_one() {
        let mut v = Vec::new();
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 100.0, 50.0));
        push_snapshot(&mut v, manual("2026-09-15 14:30:00", 142.3, 50.0));

        let diffs = snapshot_diffs(&v);
        assert_eq!(diffs.len(), 1, "两条 ⇒ 一对差值");
        let (idx, d) = &diffs[0];
        assert_eq!(*idx, 0, "贴在较晚的那条上");

        assert_eq!(d.from_at, "2026-09-15 10:00:00");
        assert_eq!(d.to_at, "2026-09-15 14:30:00");
        assert_eq!(d.span_seconds, 4 * 3600 + 30 * 60, "跨度应为 4.5 小时");
        assert_eq!(d.consumed, 42.3, "增量必须是两条读数之差");
        assert_eq!(d.gained, 0.0);
    }

    /// 规则 3：**存在系统快照**时再手动打一条 —— 覆盖上一条手动（手动只剩一格）
    #[test]
    fn manual_snapshot_replaces_the_previous_manual_once_a_system_snapshot_exists() {
        let mut v = Vec::new();
        push_snapshot(&mut v, system("2026-09-15 00:00:00", 100.0, 50.0));
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 130.0, 50.0));
        assert_eq!(v.len(), 2);

        push_snapshot(&mut v, manual("2026-09-15 12:00:00", 160.0, 50.0));
        let manuals: Vec<_> = v.iter().filter(|s| s.kind == SnapshotKind::Manual).collect();
        assert_eq!(manuals.len(), 1, "有锚点后手动只剩一格：{v:?}");
        assert_eq!(manuals[0].at, "2026-09-15 12:00:00");
        // 系统快照不受影响
        assert!(v.iter().any(|s| s.kind == SnapshotKind::System));
    }

    /// 规则 4：**存在系统快照**时又来一条系统快照 —— 覆盖上一条手动，系统也只留最新
    #[test]
    fn system_snapshot_clears_the_pending_manual_and_replaces_the_old_system() {
        let mut v = Vec::new();
        push_snapshot(&mut v, system("2026-09-15 00:00:00", 100.0, 50.0));
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 130.0, 50.0));
        assert_eq!(v.len(), 2);

        push_snapshot(&mut v, system("2026-09-16 00:00:00", 200.0, 80.0));
        assert_eq!(v.len(), 1, "手动作废、旧系统被替换：{v:?}");
        assert_eq!(v[0].kind, SnapshotKind::System);
        assert_eq!(v[0].at, "2026-09-16 00:00:00");
    }

    /// 系统快照本身也去重：同一天封口两次不该留下两条系统快照
    #[test]
    fn system_snapshots_never_pile_up() {
        let mut v = Vec::new();
        push_snapshot(&mut v, system("2026-09-15 00:00:00", 100.0, 50.0));
        push_snapshot(&mut v, system("2026-09-16 00:00:00", 200.0, 80.0));
        push_snapshot(&mut v, system("2026-09-17 00:00:00", 260.0, 80.0));
        assert_eq!(v.len(), 1, "系统快照只留最新一条：{v:?}");
        assert_eq!(v[0].at, "2026-09-17 00:00:00");
    }

    /// 无锚点的手动快照累积也有上限（防止连点几十次堆出一个只增不减的列表）
    #[test]
    fn accumulating_manual_snapshots_are_capped() {
        let mut v = Vec::new();
        for i in 0..(MAX_MANUAL_SNAPSHOTS + 5) {
            push_snapshot(&mut v, manual(&format!("2026-09-15 10:{i:02}:00"), i as f64, 0.0));
        }
        assert_eq!(v.len(), MAX_MANUAL_SNAPSHOTS);
    }

    /// 生命周期全流程：装好 → 手动×2 → 系统 → 手动 → 系统
    #[test]
    fn snapshot_lifecycle_follows_the_four_rules() {        let mut v = Vec::new();
        // 1) 没有任何快照，手动一条 —— 存下
        push_snapshot(&mut v, manual("2026-09-15 09:00:00", 10.0, 0.0));
        assert_eq!(v.len(), 1);
        // 1') 仍无系统快照，再手动 —— 并存
        push_snapshot(&mut v, manual("2026-09-15 11:00:00", 25.0, 0.0));
        assert_eq!(v.len(), 2);
        // 4) 系统快照到来 —— 手动全清，只留这条系统
        push_snapshot(&mut v, system("2026-09-16 00:00:00", 40.0, 100.0));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, SnapshotKind::System);
        // 3) 有锚点后手动 —— 只有一格
        push_snapshot(&mut v, manual("2026-09-16 10:00:00", 55.0, 100.0));
        push_snapshot(&mut v, manual("2026-09-16 14:00:00", 70.0, 100.0));
        let m: Vec<_> = v.iter().filter(|s| s.kind == SnapshotKind::Manual).collect();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].at, "2026-09-16 14:00:00");
        // 4) 又一个系统快照 —— 手动作废
        push_snapshot(&mut v, system("2026-09-17 00:00:00", 90.0, 120.0));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, SnapshotKind::System);
    }

    /// 锚点出现的**那一刻**要有清理作用，而不是等下一次手动：
    /// 没有锚点时攒了 3 条手动，系统快照一来必须全部让位，否则列表里会同时
    /// 存在「锚点」和「锚点之前的自由样本」，差值语义混乱。
    #[test]
    fn an_arriving_system_snapshot_purges_every_pre_anchor_manual() {
        let mut v = Vec::new();
        push_snapshot(&mut v, manual("2026-09-15 09:00:00", 10.0, 0.0));
        push_snapshot(&mut v, manual("2026-09-15 11:00:00", 25.0, 0.0));
        push_snapshot(&mut v, manual("2026-09-15 13:00:00", 33.0, 0.0));
        assert_eq!(v.len(), 3);

        push_snapshot(&mut v, system("2026-09-16 00:00:00", 40.0, 100.0));
        assert_eq!(v.len(), 1, "锚点到来必须清掉全部锚点前的手动：{v:?}");
        assert_eq!(v[0].kind, SnapshotKind::System);
        // 只剩一条 ⇒ 没有可比对象
        assert!(snapshot_diffs(&v).is_empty());
    }

    /// 快照文件往返 + 旧/坏文件被安全丢弃（与日报同一套 schema 策略）
    #[test]
    fn snapshots_round_trip_and_survive_a_broken_file() {
        let dir = std::env::temp_dir().join(format!("wb-snap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let mut v = Vec::new();
        push_snapshot(&mut v, system("2026-09-15 00:00:00", 100.0, 50.0));
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 130.0, 50.0));
        save_snapshots(&dir, &v).unwrap();

        let back = load_snapshots(&dir);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].at, "2026-09-15 10:00:00");
        assert_eq!(back[0].kind, SnapshotKind::Manual);
        assert_eq!(back[1].kind, SnapshotKind::System);

        // 坏 JSON / 空文件都不该 panic，返回空即可
        assert!(decode_snapshots("{not json").is_empty());
        assert!(decode_snapshots("").is_empty());
        // 版本对不上丢弃
        assert!(decode_snapshots(r#"{"v":99,"snapshots":[]}"#).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 时间戳解析不出来时跨度返回 0（宁可显示「未知」，也不要一个算错的数字）
    #[test]
    fn an_unparsable_timestamp_yields_a_zero_span() {
        assert_eq!(seconds_between("坏掉的时间", "2026-09-15 10:00:00"), 0);
        assert_eq!(seconds_between("2026-09-15 10:00:00", ""), 0);
        // 跨天要正确累加
        assert_eq!(
            seconds_between("2026-09-15 23:30:00", "2026-09-16 00:30:00"),
            3600
        );
    }

    /// 同一秒内连点两次：不崩、不错位，第二条与第一条的跨度是 0。
    ///
    /// 这是真实可达的（用户连按「当前累计」）——时间戳会撞，所以 `snapshot_diffs`
    /// 用**下标**而不是时间戳来配对。跨度 0 由界面显示成「同一时刻」，
    /// 好过让两条互相覆盖或让某一条永远找不到参照。
    #[test]
    fn two_snapshots_in_the_same_second_still_pair_up_by_position() {
        let mut v = Vec::new();
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 100.0, 0.0));
        push_snapshot(&mut v, manual("2026-09-15 10:00:00", 118.0, 0.0));
        assert_eq!(v.len(), 2, "碰时间戳不该让任何一条被吞掉：{v:?}");

        let diffs = snapshot_diffs(&v);
        assert_eq!(diffs.len(), 1, "两条 ⇒ 一对差值");
        let (idx, d) = &diffs[0];
        assert_eq!(*idx, 0, "增量贴在较晚那条上（下标 0，因为它插在最前）");
        assert_eq!(d.span_seconds, 0, "同秒 ⇒ 跨度 0");
        assert_eq!(d.consumed, 18.0, "读数差仍然是真实的");
    }
}
