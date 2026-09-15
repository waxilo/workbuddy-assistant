use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 单次签到结果（用于前端展示与落库）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CheckinRecord {
    pub success: bool,
    pub already: bool,
    pub inactive: bool,
    pub message: String,
    /// 本次获得积分（today_credit）
    pub credit: Option<i64>,
    /// 账号剩余积分（get-user-resource 汇总，可能是小数；取不到为 None）
    #[serde(default)]
    pub balance: Option<f64>,
    pub streak: Option<i64>,
    pub host: Option<String>,
    pub at: String,
    pub code: Option<i64>,
}

/// 积分快照（持久化到 accounts.json）：路由与展示共用的「真实积分画像」。
///
/// - `credits`：剩余积分（get-user-resource 汇总，取不到为 None）
/// - `earliest_expiry_ms`：还有余量的资源包里最早的重置/过期时间（毫秒，
///   驱动「按积分过期时间优先路由」）；无余量/未知为 None
/// - `fetched_at`：本次拉取时刻（本地时间串 `%Y-%m-%d %H:%M:%S`），用于判断快照是否过期
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CreditSnapshot {
    #[serde(default)]
    pub credits: Option<f64>,
    #[serde(default)]
    pub earliest_expiry_ms: Option<i64>,
    #[serde(default)]
    pub fetched_at: Option<String>,
}

/// 一个签到账号
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    pub id: String,
    pub name: String,
    /// 手机号（展示用标识，手动录入，可空）
    #[serde(default)]
    pub phone: Option<String>,
    pub token: String,
    /// 续签用的 refresh token（导入本机账号 / 无感登录时一并带上；老账号为 None）
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// access token 过期时间（毫秒时间戳）；None 表示未知
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last: Option<CheckinRecord>,
    /// 积分快照（持久化）：剩余积分 + 最早过期时间 + 拉取时刻。
    /// 智能接管路由与账号列表展示都读它；刷新命令会重拉并落盘。
    #[serde(default)]
    pub credit_snapshot: Option<CreditSnapshot>,
    /// 服务端「今日是否已签到」的真实状态（只读查询，持久化）。
    /// None = 尚未查询/查询失败（前端按「未知 / 待签到」处理，
    /// 绝不再把昨天的本地缓存或一次本地报错当成确定状态）。
    #[serde(default)]
    pub checked_today: Option<bool>,
}

/// 全局设置
///
/// 新增字段一律带 `#[serde(default)]`，这样老版本写下的 settings.json
/// 仍能反序列化（不会整个文件被判为非法而回退成默认值）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default = "default_base_url")]
    pub default_base_url: String,
    #[serde(default)]
    pub auto_checkin_on_start: bool,
    /// 是否开启「每天定时自动签到」
    #[serde(default)]
    pub schedule_enabled: bool,
    /// 定时签到时刻，24 小时制 `HH:MM`（应用需保持运行才会触发）
    #[serde(default = "default_schedule_time")]
    pub schedule_time: String,
    /// 定时签到的随机时间窗（分钟）：当天实际触发时刻 = `schedule_time` + `[0, 窗口]` 内随机。
    ///
    /// 「每天同一分钟触发」是脚本最好认的特征，给一个窗口就能让每天都不一样；
    /// 当天挑定的时刻会写进 `schedule_state.json` 复用，重启不会重新摇。0 = 关闭随机。
    #[serde(default = "default_schedule_window")]
    pub schedule_window_minutes: u32,
    /// 通知总开关（关掉后定时与手动都不推送）
    #[serde(default)]
    pub notify_enabled: bool,
    /// 通知 webhook 地址，形如 `https://…/hook/<key>`
    #[serde(default)]
    pub notify_webhook: String,
    /// 定时签到结束后推送
    #[serde(default = "default_true")]
    pub notify_on_schedule: bool,
    /// 手动「全部签到」结束后推送（默认关，避免连点造成刷屏）
    #[serde(default)]
    pub notify_on_manual: bool,
    /// 智能接管总开关（127.0.0.1 反代 + 把 WorkBuddy 端点指向它，按积分过期时间优先路由）。
    /// 这是 WorkBuddy 专用通道：只监听本机、无鉴权 Key、无独立「仅反代」模式。
    #[serde(default)]
    pub proxy_enabled: bool,
    /// 反代监听端口（默认 8787，避开 Clash 的 7897）
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    /// 扣费备选账号（多选）：反代只在这批账号里选号扣费，**未选中的账号不允许扣费**。
    /// 空 = 全部账号都可作为备选（智能轮换）。
    #[serde(default)]
    pub billing_account_ids: Vec<String>,
    /// 限流无感切换生效的模型（多选）：这些模型触发 429 时自动换备用账号重发同一请求。
    /// 0 积分（免费）模型恒生效、无需勾选；这里只存用户额外勾选的付费模型。
    /// 空 = 仅 0 积分免费模型享受限流切换。
    #[serde(default)]
    pub rate_limit_models: Vec<String>,
    /// 多账号风控预防：批量签到时在账号之间加入随机间隔，避免同一 IP 瞬时连发多账号请求。
    #[serde(default = "default_true")]
    pub stagger_checkin: bool,
    /// 随机间隔上限（秒）；实际间隔在 2..=max 之间取值
    #[serde(default = "default_stagger_max")]
    pub stagger_max_seconds: u32,
    /// 批量签到时随机打乱账号顺序。
    ///
    /// 顺序本身也是特征：每次都按账号列表的固定先后连发，等于把「同一批账号」
    /// 直接写进请求序列里。打乱只影响**发请求的次序**，列表展示与落盘顺序不变。
    #[serde(default = "default_true")]
    pub shuffle_checkin_order: bool,
    /// 手动「全部签到」也加账号间隔（默认开）。
    ///
    /// 手动路径曾为了「点完就想看结果」跳过等待，于是它成了全程唯一的瞬时连发入口——
    /// 风控看到的恰好就是那一串几秒内完成的签到。
    #[serde(default = "default_true")]
    pub manual_stagger: bool,
    /// 手动「全部签到」的间隔上限（秒）；实际在 2..=max 之间取值。
    /// 比自动签到小得多：手动场景还得让人等得下去。
    #[serde(default = "default_manual_stagger_max")]
    pub manual_stagger_max_seconds: u32,
    /// 限流（429）时是否在同一会话内换备用账号。
    ///
    /// true = 无感续跑，但同一个会话会出现「中途换凭证」——正常用户的一个会话
    /// 自始至终只有一个账号，这在风控眼里是极高异常值。false = 防御优先：
    /// 429 原样透传、不记冷却，该会话不会被切到别的账号上。
    #[serde(default = "default_true")]
    pub failover_on_rate_limit: bool,
    /// 每日积分日报：应用常驻时每天在 `report_time` 结算一次
    /// （统计「上次结算 → 本次结算」的消耗与新增，见 `ledger`）。
    #[serde(default = "default_true")]
    pub report_enabled: bool,
    /// 日报结算时刻，24 小时制 `HH:MM`（默认 12:00）
    #[serde(default = "default_report_time")]
    pub report_time: String,
    /// 日报是否推送到 webhook（复用通知总开关 `notify_enabled` 与 `notify_webhook`）
    #[serde(default = "default_true")]
    pub notify_on_report: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_base_url: default_base_url(),
            auto_checkin_on_start: false,
            schedule_enabled: false,
            schedule_time: default_schedule_time(),
            schedule_window_minutes: default_schedule_window(),
            notify_enabled: false,
            notify_webhook: String::new(),
            notify_on_schedule: true,
            notify_on_manual: false,
            proxy_enabled: false,
            proxy_port: default_proxy_port(),
            billing_account_ids: Vec::new(),
            rate_limit_models: Vec::new(),
            stagger_checkin: true,
            stagger_max_seconds: default_stagger_max(),
            shuffle_checkin_order: true,
            manual_stagger: true,
            manual_stagger_max_seconds: default_manual_stagger_max(),
            failover_on_rate_limit: true,
            report_enabled: true,
            report_time: default_report_time(),
            notify_on_report: true,
        }
    }
}

/// 日报结算时刻：默认 12:00。
///
/// 选中午而不是零点：签到在上午跑完，12 点结算刚好把「上午签到拿到的」与
/// 「白天用掉的」放在两个窗口里看，不会混在同一条日报里。
fn default_report_time() -> String {
    "12:00".to_string()
}

/// 风控随机间隔默认上限：45 秒足够打散节奏，又不至于让「全部签到」等太久
fn default_stagger_max() -> u32 {
    45
}

/// 手动「全部签到」的间隔上限：8 秒。用户是主动点的，等待要明显短于自动签到
fn default_manual_stagger_max() -> u32 {
    8
}

/// 定时签到的随机窗口：90 分钟。既能消掉「每天同一分钟」，又不会把签到推到太晚
fn default_schedule_window() -> u32 {
    90
}

/// 规范化时刻字符串：接受 `9:7` / `09:07` / 首尾空格，统一输出 `HH:MM`。
/// 非法输入返回 None（调用方保留原值，避免把用户输入悄悄改掉）。
pub fn normalize_time(input: &str) -> Option<String> {
    let t = input.trim();
    let (h, m) = t.split_once(':')?;
    // 只允许纯数字，排除 `9:7:0`、`09am` 之类
    if h.is_empty() || m.is_empty() || !h.chars().all(|c| c.is_ascii_digit()) || !m.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(format!("{h:02}:{m:02}"))
}

fn default_base_url() -> String {
    "https://copilot.tencent.com".to_string()
}

/// 默认 09:07 —— 与参考脚本 `workbuddy_checkin.py` 的 launchd 定时保持一致
fn default_schedule_time() -> String {
    "09:07".to_string()
}

fn default_true() -> bool {
    true
}

/// 反代默认端口：8787（避开 Clash 的 7897 等常用端口）
fn default_proxy_port() -> u16 {
    8787
}

pub fn accounts_file(dir: &Path) -> PathBuf {
    dir.join("accounts.json")
}

pub fn settings_file(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

pub fn load_accounts(dir: &Path) -> Vec<Account> {
    let f = accounts_file(dir);
    if !f.exists() {
        return Vec::new();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    serde_json::from_str(&s).unwrap_or_default()
}

pub fn save_accounts(dir: &Path, accounts: &[Account]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let target = accounts_file(dir);
    let tmp = dir.join("accounts.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(accounts)?)?;
    fs::rename(&tmp, &target)?;
    set_private_permissions(&target);
    Ok(())
}

pub fn load_settings(dir: &Path) -> Settings {
    let f = settings_file(dir);
    if !f.exists() {
        return Settings::default();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    serde_json::from_str(&s).unwrap_or_default()
}

pub fn save_settings(dir: &Path, settings: &Settings) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let target = settings_file(dir);
    let tmp = dir.join("settings.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(settings)?)?;
    fs::rename(&tmp, &target)?;
    set_private_permissions(&target);
    Ok(())
}

/// 把账号/设置文件权限设为 0600（仅当前用户可读写）。
/// Windows 下忽略（ACL 模型不同，文件仍在用户专属 AppData 内）。
pub(crate) fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_loose_time_input_to_hh_mm() {
        assert_eq!(normalize_time("9:7").as_deref(), Some("09:07"));
        assert_eq!(normalize_time(" 09:07 ").as_deref(), Some("09:07"));
        assert_eq!(normalize_time("23:59").as_deref(), Some("23:59"));
        assert_eq!(normalize_time("00:00").as_deref(), Some("00:00"));
    }

    #[test]
    fn rejects_invalid_time_input() {
        for bad in ["", "9", "9:", ":7", "24:00", "09:60", "09:07:00", "09am", "aa:bb", "-1:5"] {
            assert_eq!(normalize_time(bad), None, "{bad:?} 不应通过");
        }
    }

    #[test]
    fn old_settings_file_still_deserializes_with_new_fields_defaulted() {
        // 老版本只写了这两个字段，新增的定时/通知字段必须走默认值而不是让整份配置报废
        let old = r#"{"default_base_url":"https://x","auto_checkin_on_start":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.default_base_url, "https://x");
        assert!(s.auto_checkin_on_start);
        assert!(!s.schedule_enabled);
        assert_eq!(s.schedule_time, "09:07");
        assert!(!s.notify_enabled);
        assert!(s.notify_webhook.is_empty());
        // 定时推送默认开、手动推送默认关
        assert!(s.notify_on_schedule);
        assert!(!s.notify_on_manual);
        // 优先扣费账号：老配置没有 → 空（= 全部账号都可作为备选）
        assert!(s.billing_account_ids.is_empty());
        // 风控相关字段同样必须落到默认值，否则升级后老用户会突然丢掉整套打散策略
        assert_eq!(s.schedule_window_minutes, 90);
        assert!(s.stagger_checkin);
        assert_eq!(s.stagger_max_seconds, 45);
        assert!(s.shuffle_checkin_order);
        assert!(s.manual_stagger);
        assert_eq!(s.manual_stagger_max_seconds, 8);
        // 限流换号默认保持开启 = 与升级前的行为一致（关掉才是主动选的防御姿态）
        assert!(s.failover_on_rate_limit);
        // 积分日报：默认开启、12:00 结算、跟着推送一起发（老配置没有这些字段）
        assert!(s.report_enabled);
        assert_eq!(s.report_time, "12:00");
        assert!(s.notify_on_report);
    }

    #[test]
    fn legacy_preferred_account_field_is_ignored() {
        // 旧版单选字段不再使用：读到也不影响新逻辑（多选列表仍为空 = 全部可用）
        let s: Settings = serde_json::from_str(r#"{"preferred_account_id":"abc"}"#).unwrap();
        assert!(s.billing_account_ids.is_empty());
    }
}
