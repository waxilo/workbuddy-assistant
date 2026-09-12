//! 无感接管的**保险丝**：把「自定义端点」写进 WorkBuddy 全局配置，并保证它一定能被取下来。
//!
//! # 为什么必须独立成模块
//!
//! 无感接管 = 让 WorkBuddy 桌面端的所有对话请求自动走本应用的反代。实现上只能是
//! 往 `~/.workbuddy/settings.json` 的 `env.CODEBUDDY_BASE_URL` 写值 —— 而这**正是**
//! 历史上把整个 WorkBuddy 搞到 502 断网的那个动作（`netfix.rs` 的清理目标）。
//!
//! 所以「能装」不是本事，「保证一定能卸」才是。本模块用**租约 + 心跳**把这件事钉死：
//!
//! - 装之前先把原值存进租约，卸载时还原（不新增也不吞掉用户原有的值）；
//! - 反代活着就持续心跳；心跳停了（应用崩了 / 被 kill -9 / 端口没了）租约即过期；
//! - 应用启动时先 `sweep`：租约过期 = 僵尸，直接卸掉，绝不让上次崩溃留下断网残留。
//!
//! # 与 netfix 的关系
//!
//! `netfix` 扫到 `env.CODEBUDDY_BASE_URL` 时会来问本模块「这是自己人吗」：
//! 租约新鲜 → 正常工作中，不算问题；租约过期 → 僵尸残留，照常判为会断网。
//! 这样「一键恢复」清掉的是真残留，不会把正在工作的接管误伤掉。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 心跳间隔：反代监督线程按此频率续租
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// 超过这么久没有心跳，就认定接管方已经不在了
pub const LEASE_TTL: Duration = Duration::from_secs(30);

/// 写进 WorkBuddy 配置的变量名（与 netfix 的 `ENV_KEYS[0]` 一致）
pub const ENV_KEY: &str = "CODEBUDDY_BASE_URL";

const LEASE_FILE: &str = "stealth.json";

/// 接管事件日志（追加式 JSONL）。WorkBuddy 的长驻 CLI host 会把 settings.env
/// 注入 process.env，但删除配置键时不会清掉旧值。日志记录 install / proxy_request /
/// uninstall / restart，用于判断端点已摘除后是否仍存在缓存旧地址的 CLI host。
const JOURNAL_FILE: &str = "takeover-journal.jsonl";
/// 只留最近这么多条：诊断只需要时间线，不需要考古
const JOURNAL_MAX: usize = 200;

/// 一条接管事件
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JournalEvent {
    pub at_ms: i64,
    /// 本地时间（展示用）
    pub at: String,
    /// install / uninstall / restart_workbuddy
    pub event: String,
    #[serde(default)]
    pub detail: String,
}

pub fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

pub fn journal_read(data_dir: &Path) -> Vec<JournalEvent> {
    let Ok(text) = fs::read_to_string(journal_path(data_dir)) else {
        return Vec::new();
    };
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// 追加一条事件。失败一律静默：日志是诊断辅助，绝不能反过来影响接管本身。
pub fn journal_append(data_dir: &Path, event: &str, detail: &str) {
    let e = JournalEvent {
        at_ms: now_ms(),
        at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        event: event.to_string(),
        detail: detail.to_string(),
    };
    let mut all = journal_read(data_dir);
    all.push(e);
    if all.len() > JOURNAL_MAX {
        let drop = all.len() - JOURNAL_MAX;
        all.drain(..drop);
    }
    let body: String = all
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .map(|l| format!("{l}\n"))
        .collect();
    let target = journal_path(data_dir);
    if fs::create_dir_all(data_dir).is_ok() {
        let tmp = data_dir.join(format!("{JOURNAL_FILE}.tmp"));
        if fs::write(&tmp, body).is_ok() {
            let _ = fs::rename(&tmp, &target);
        }
    }
}

/// 接管租约。落在**本应用**的数据目录里，不进 WorkBuddy 的配置。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Lease {
    /// 写进配置的端点值，如 `http://127.0.0.1:8787`
    pub url: String,
    pub port: u16,
    /// 装载方进程号（仅用于展示与自查，不作为存活判据——PID 会被复用）
    pub pid: u32,
    /// 被覆盖前的原值；原本没有就是 None，卸载时把键整个删掉
    pub previous: Option<String>,
    pub installed_at_ms: i64,
    /// 最近一次心跳（毫秒）。存活判据只看这个。
    pub heartbeat_ms: i64,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn lease_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LEASE_FILE)
}

pub fn load_lease(data_dir: &Path) -> Option<Lease> {
    let text = fs::read_to_string(lease_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_lease(data_dir: &Path, lease: &Lease) -> std::io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let target = lease_path(data_dir);
    let tmp = data_dir.join(format!("{LEASE_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(lease)?)?;
    fs::rename(&tmp, &target)?;
    crate::accounts::set_private_permissions(&target);
    Ok(())
}

/// 租约是否还活着
pub fn is_alive(lease: &Lease) -> bool {
    now_ms().saturating_sub(lease.heartbeat_ms) < LEASE_TTL.as_millis() as i64
}

/// 续租。没有租约就什么也不做（避免凭空造出一个租约）。
/// 端口变了说明配置被改过，也不续 —— 让 `sweep` 去收拾。
pub fn heartbeat(data_dir: &Path, port: u16) {
    if let Some(mut lease) = load_lease(data_dir) {
        if lease.port == port {
            lease.heartbeat_ms = now_ms();
            let _ = save_lease(data_dir, &lease);
        }
    }
}

/// 这个值是不是**当前这个端口**的接管端点
fn url_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// 装卸的目标配置文件。只动 `.workbuddy`，不碰 `.codebuddy`（那是旧版目录）。
fn target_config(home: &Path) -> PathBuf {
    home.join(".workbuddy").join("settings.json")
}

/// 把一个值写进 `settings.json` 的 `env.CODEBUDDY_BASE_URL`。
/// `value` 为 None 表示删除该键（`env` 空了就连 `env` 一起删）。
///
/// 用 `Value` 精确改而不是整份重写：那份文件里有 `sandbox` / `claw` / `enabledPlugins`
/// 等我们不认识的键，绝不能顺手清掉。
fn set_endpoint(path: &Path, value: Option<&str>) -> Result<(), String> {
    let text = if path.exists() {
        fs::read_to_string(path).map_err(|e| format!("读取配置失败：{e}"))?
    } else {
        // 文件不存在时从空对象开始——只在用户机器上确实没有该文件的极端情况下发生
        "{}".to_string()
    };
    let mut v: Value = if text.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&text).map_err(|e| format!("配置不是合法 JSON，未改动：{e}"))?
    };
    let Some(obj) = v.as_object_mut() else {
        return Err("配置顶层不是对象，未改动。".into());
    };

    match value {
        Some(val) => {
            let env = obj
                .entry("env")
                .or_insert_with(|| Value::Object(Default::default()));
            let env = env
                .as_object_mut()
                .ok_or_else(|| "配置里的 env 不是对象，未改动。".to_string())?;
            env.insert(ENV_KEY.to_string(), Value::String(val.to_string()));
        }
        None => {
            let empty = match obj.get_mut("env").and_then(Value::as_object_mut) {
                Some(env) => {
                    env.remove(ENV_KEY);
                    env.is_empty()
                }
                None => false,
            };
            if empty {
                obj.remove("env");
            }
        }
    }

    let out = serde_json::to_string_pretty(&v).map_err(|e| format!("序列化失败：{e}"))?;
    crate::netfix::write_atomic(path, &format!("{out}\n")).map_err(|e| format!("写入失败：{e}"))
}

/// 读取配置里当前的端点值
pub fn current_endpoint(home: &Path) -> Option<String> {
    let text = fs::read_to_string(target_config(home)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("env")
        .and_then(Value::as_object)
        .and_then(|e| e.get(ENV_KEY))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// **装**：把端点写进 WorkBuddy 配置，并落下租约。
///
/// 幂等：已装着且目标端口一致 → 只续一次心跳，不重复写文件。
pub fn install(home: &Path, data_dir: &Path, port: u16) -> Result<(), String> {
    let url = url_for_port(port);

    if let Some(mut lease) = load_lease(data_dir) {
        if lease.port == port && lease.url == url {
            lease.heartbeat_ms = now_ms();
            let _ = save_lease(data_dir, &lease);
            // 值可能被别的程序改过，这里保证它仍是我们期望的那个
            if current_endpoint(home).as_deref() != Some(url.as_str()) {
                set_endpoint(target_config(home).as_path(), Some(&url))?;
            }
            return Ok(());
        }
        // 端口变了：先按旧租约卸干净，再装新的
        let _ = uninstall(home, data_dir);
    }

    let previous = current_endpoint(home);
    let lease = Lease {
        url: url.clone(),
        port,
        pid: std::process::id(),
        previous,
        installed_at_ms: now_ms(),
        heartbeat_ms: now_ms(),
    };
    // 先落租约再改配置：中途崩了也留有记录，sweep 能收尾
    save_lease(data_dir, &lease).map_err(|e| format!("写入租约失败：{e}"))?;
    set_endpoint(target_config(home).as_path(), Some(&url))?;
    journal_append(data_dir, "install", &format!("端点写入 env.{ENV_KEY}={url}"));
    Ok(())
}

/// **卸**：把端点从配置里摘掉（还原成装载前的值），删掉租约。
///
/// 安全起见：只有当配置里的值**仍是我们写进去的那个**才动手；
/// 被别人改过就不碰，避免把用户的配置改坏。
pub fn uninstall(home: &Path, data_dir: &Path) -> Result<(), String> {
    let lease = load_lease(data_dir);
    let path = target_config(home);
    if !path.exists() {
        let _ = fs::remove_file(lease_path(data_dir));
        return Ok(());
    }

    let current = current_endpoint(home);
    let mut changed = false;
    match (&lease, &current) {
        (Some(lease), Some(cur)) if cur == &lease.url => {
            // 正常路径：还原成装载前的值
            match lease.previous.as_deref() {
                Some(prev) => set_endpoint(&path, Some(prev))?,
                None => set_endpoint(&path, None)?,
            }
            changed = true;
        }
        // 没租约、或值已被改动：只在我们能确认端口归属时才清
        (_, Some(cur)) if *cur == url_for_port(crate::accounts::load_settings(data_dir).proxy_port) => {
            set_endpoint(&path, None)?;
            changed = true;
        }
        _ => {}
    }
    if changed {
        journal_append(data_dir, "uninstall", "端点已从 WorkBuddy 配置摘除");
    }
    let _ = fs::remove_file(lease_path(data_dir));
    Ok(())
}

/// **清扫僵尸**：启动时调用。
///
/// 上一轮可能是崩溃退出的（`kill -9`、系统重启、端口被抢），此时配置里还留着
/// 指向本机的端点而反代已经不在 —— 这就是断网现场。一律卸掉。
pub fn sweep(home: &Path, data_dir: &Path) {
    let Some(lease) = load_lease(data_dir) else {
        // 没有租约但配置里却指向我们的端口 → 更危险的孤儿残留
        let port = crate::accounts::load_settings(data_dir).proxy_port;
        if current_endpoint(home).as_deref() == Some(url_for_port(port).as_str()) {
            let _ = set_endpoint(&target_config(home), None);
        }
        return;
    };
    if !is_alive(&lease) {
        let _ = uninstall(home, data_dir);
    }
}

/// 给前端展示的接管状态
#[derive(Serialize, Clone, Debug)]
pub struct StealthStatus {
    /// 设置里是否开启
    pub enabled: bool,
    /// 端点是否真的写进 WorkBuddy 配置了
    pub installed: bool,
    /// 租约是否新鲜（心跳还在跳）
    pub alive: bool,
    pub port: u16,
    pub url: String,
    /// 人话说明当前状态与下一步该做什么
    pub note: String,
}

/// 综合「设置 + 租约 + 配置实际值」给出状态。只读，不改动任何东西。
pub fn status(home: &Path, data_dir: &Path) -> StealthStatus {
    let settings = crate::accounts::load_settings(data_dir);
    let port = settings.proxy_port;
    let url = url_for_port(port);
    let lease = load_lease(data_dir);
    let alive = lease.as_ref().is_some_and(is_alive);
    let installed = current_endpoint(home).as_deref() == Some(url.as_str());

    let note = match (settings.proxy_enabled, installed, alive) {
        (false, _, _) => "未开启。开启后 WorkBuddy 的对话请求会自动走本应用反代，按最旧积分优先选账号。".into(),
        (true, true, true) => "接管生效中。模型请求经本机代理分流；启停或换端口会自动安全重启长驻 CLI host。".into(),
        (true, true, false) => "配置里有端点，但心跳已停 —— 接管进程可能已退出，请点「停止接管」清理。".into(),
        (true, false, _) => "正在装载：稍等几秒后刷新；若一直卡在这里，检查 ~/.workbuddy/settings.json 是否可写。".into(),
    };

    StealthStatus {
        enabled: settings.proxy_enabled,
        installed,
        alive,
        port,
        url,
        note,
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------

fn home_dir() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "无法定位家目录，无法读写 WorkBuddy 全局配置。".to_string())
}

/// 查询接管状态（只读）
#[tauri::command]
pub fn stealth_status(app: tauri::AppHandle) -> Result<StealthStatus, String> {
    let dir = crate::commands::try_data_dir(&app)?;
    Ok(status(&home_dir()?, &dir))
}

/// 安全停止接管：摘掉端点后重启 WorkBuddy，清除长驻 CLI host 缓存，再停止监听。
#[tauri::command]
pub fn stealth_stop(app: tauri::AppHandle) -> Result<StealthStatus, String> {
    let dir = crate::commands::try_data_dir(&app)?;
    let home = home_dir()?;
    let mut settings = crate::accounts::load_settings(&dir);
    settings.proxy_enabled = false;
    crate::commands::apply_settings_inner(&app, settings)?;
    Ok(status(&home, &dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn sandbox() -> (PathBuf, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "wb-stealth-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let home = base.join("home");
        let data = base.join("data");
        fs::create_dir_all(home.join(".workbuddy")).unwrap();
        fs::create_dir_all(&data).unwrap();
        (home, data)
    }

    fn write_cfg(home: &Path, json: &str) {
        fs::write(target_config(home), json).unwrap();
    }

    #[test]
    fn install_writes_endpoint_and_uninstall_restores_previous() {
        let (home, data) = sandbox();
        write_cfg(&home, r#"{"sandbox":{"a":1},"env":{"OTHER":"keep"}}"#);

        install(&home, &data, 8787).unwrap();
        assert_eq!(current_endpoint(&home).as_deref(), Some("http://127.0.0.1:8787"));
        let text = fs::read_to_string(target_config(&home)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["sandbox"]["a"], Value::from(1), "不认识的键必须原样保留");
        assert_eq!(v["env"]["OTHER"], Value::from("keep"));

        uninstall(&home, &data).unwrap();
        assert_eq!(current_endpoint(&home), None, "端点必须被摘掉");
        let v2: Value = serde_json::from_str(&fs::read_to_string(target_config(&home)).unwrap()).unwrap();
        assert!(v2["env"].get("CODEBUDDY_BASE_URL").is_none());
        // env 里还有用户的 OTHER，所以它不能整个消失——只能摘掉我们那一键
        assert_eq!(v2["env"]["OTHER"], Value::from("keep"));
        assert!(!lease_path(&data).exists(), "租约要一并删除");

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn uninstall_restores_an_existing_user_value() {
        let (home, data) = sandbox();
        write_cfg(&home, r#"{"env":{"CODEBUDDY_BASE_URL":"https://my.own.gateway"}}"#);

        install(&home, &data, 8787).unwrap();
        uninstall(&home, &data).unwrap();
        assert_eq!(
            current_endpoint(&home).as_deref(),
            Some("https://my.own.gateway"),
            "卸载必须还原成用户原本的值，而不是删掉"
        );

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn sweep_clears_stale_lease_left_by_a_crash() {
        let (home, data) = sandbox();
        install(&home, &data, 8787).unwrap();
        assert_eq!(current_endpoint(&home).as_deref(), Some("http://127.0.0.1:8787"));

        // 模拟崩溃：把心跳拨回很久以前，就像应用被 kill -9 后再也没起来
        let mut lease = load_lease(&data).unwrap();
        lease.heartbeat_ms = now_ms() - LEASE_TTL.as_millis() as i64 - 1_000;
        save_lease(&data, &lease).unwrap();
        assert!(!is_alive(&lease), "过期租约必须判定为不存活");

        sweep(&home, &data);
        assert_eq!(current_endpoint(&home), None, "僵尸端点必须被清掉");
        assert!(!lease_path(&data).exists());

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn sweep_keeps_a_live_lease() {
        let (home, data) = sandbox();
        install(&home, &data, 8787).unwrap();
        sweep(&home, &data);
        assert_eq!(
            current_endpoint(&home).as_deref(),
            Some("http://127.0.0.1:8787"),
            "心跳新鲜的接管不能被清扫掉"
        );
        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn sweep_clears_orphan_endpoint_without_lease() {
        let (home, data) = sandbox();
        // 租约文件丢了（例如数据目录被清理过），但配置里还留着端点
        write_cfg(&home, r#"{"env":{"CODEBUDDY_BASE_URL":"http://127.0.0.1:8787"}}"#);
        // 默认端口正是 8787
        assert_eq!(crate::accounts::load_settings(&data).proxy_port, 8787);

        sweep(&home, &data);
        assert_eq!(current_endpoint(&home), None, "无租约的孤儿端点要清掉");

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn install_is_idempotent_and_retargets_on_port_change() {
        let (home, data) = sandbox();
        install(&home, &data, 8787).unwrap();
        install(&home, &data, 8787).unwrap();
        assert_eq!(current_endpoint(&home).as_deref(), Some("http://127.0.0.1:8787"));

        install(&home, &data, 8899).unwrap();
        assert_eq!(current_endpoint(&home).as_deref(), Some("http://127.0.0.1:8899"));
        assert_eq!(load_lease(&data).unwrap().port, 8899);

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn installing_twice_does_not_lose_the_original_value() {
        // 连续换端口时，previous 必须是「最初的」那个值，而不是上一次我们写进去的
        let (home, data) = sandbox();
        write_cfg(&home, r#"{"env":{"CODEBUDDY_BASE_URL":"https://original"}}"#);
        install(&home, &data, 8787).unwrap();
        install(&home, &data, 8899).unwrap();
        uninstall(&home, &data).unwrap();
        assert_eq!(current_endpoint(&home).as_deref(), Some("https://original"));

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn refuses_to_touch_unparsable_config() {
        let (home, data) = sandbox();
        write_cfg(&home, "{not json");
        assert!(install(&home, &data, 8787).is_err());
        assert_eq!(fs::read_to_string(target_config(&home)).unwrap(), "{not json");

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn journal_records_real_changes_only() {
        let (home, data) = sandbox();
        // 没装过就卸载：不该留下事件（噪声会让诊断误判）
        uninstall(&home, &data).unwrap();
        assert!(journal_read(&data).is_empty());

        install(&home, &data, 8787).unwrap();
        uninstall(&home, &data).unwrap();
        let events = journal_read(&data);
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            vec!["install", "uninstall"],
            "只记真实发生的变更：{:?}",
            events
        );
        assert!(events[0].detail.contains("8787"));

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }

    #[test]
    fn journal_caps_length_and_keeps_latest() {
        let (home, data) = sandbox();
        for i in 0..(JOURNAL_MAX + 10) {
            journal_append(&data, "install", &format!("e{i}"));
        }
        let all = journal_read(&data);
        assert_eq!(all.len(), JOURNAL_MAX);
        assert_eq!(all.last().unwrap().detail, format!("e{}", JOURNAL_MAX + 9));

        let _ = fs::remove_dir_all(home.parent().unwrap());
    }
}
