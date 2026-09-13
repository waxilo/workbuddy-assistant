use crate::accounts::{self, Account, Settings};
use crate::auth_file::{self, LocalAccount};
use crate::checkin;
use crate::logs::{self, CheckinLog};
use crate::notify;
use crate::oauth;
use crate::refresh;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri::Manager;
use tauri_plugin_autostart::ManagerExt;

/// 应用数据目录。取不到时返回 Err（而不是 panic）——后台调度线程也走这里，
/// 一旦 panic 在 release（`panic = "abort"`）下会直接把整个应用带崩。
pub(crate) fn try_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("无法获取应用数据目录（请检查系统权限）：{e}"))
}

fn data_dir(app: &AppHandle) -> PathBuf {
    try_data_dir(app).expect("无法获取应用数据目录（请检查系统权限）")
}

#[tauri::command]
pub fn list_accounts(app: AppHandle) -> Result<Vec<Account>, String> {
    Ok(accounts::load_accounts(&data_dir(&app)))
}

/// 账号所属域：优先 token 的 iss，退回国内容官方域。
/// 续签接口是 `/v2/plugin/...`，必须打到账号自己的域，不能打到自定义 base_url。
pub(crate) fn account_host(account: &Account) -> String {
    let iss = checkin::issuer_host(&account.token);
    oauth::normalize_host(iss.as_deref())
}

/// 单个账号的自动续签结果（供调度线程写日志 / 发事件）
#[derive(serde::Serialize, Clone, Debug)]
pub struct AutoRefreshReport {
    /// 实际完成续签的账号名
    pub refreshed: Vec<String>,
    /// 需要续签但失败的（账号名 + 原因）
    pub failed: Vec<String>,
}

/// 阈值内自动续签：仅在「有 refresh token 且已过期/剩余有效期不足阈值（48h）」时打一次接口。
///
/// 返回：`Ok(true)` 已续签；`Ok(false)` 不需要续（无 refresh token 或有效期还早）；
/// `Err` 需要续但失败了。调用方据此决定「记日志 / 继续用旧 token」。
pub(crate) async fn ensure_fresh_token(account: &mut Account) -> Result<bool, String> {
    let Some(rt) = account.refresh_token.clone() else {
        return Ok(false);
    };
    let now = chrono::Utc::now().timestamp_millis();
    if !refresh::should_refresh(account.expires_at, now) {
        return Ok(false);
    }
    let host = account_host(account);
    let r = refresh::refresh(&host, &account.token, &rt).await?;
    account.token = r.token;
    if let Some(next_rt) = r.refresh_token {
        account.refresh_token = Some(next_rt);
    }
    account.expires_at = r.expires_at.or(account.expires_at);
    Ok(true)
}

/// 自动续签全部账号（调度线程 / 启动自检调用）。
///
/// 只在确实有账号被续签时才落盘；单个账号失败不中断其它账号，也不让整体返回 Err
/// （续签失败是常态化的旁路事件，不该被当成「签到异常」）。
pub async fn auto_refresh_all(app: &AppHandle) -> Result<AutoRefreshReport, String> {
    let dir = data_dir(app);
    let mut accounts = accounts::load_accounts(&dir);
    let mut refreshed = Vec::new();
    let mut failed = Vec::new();
    let mut changed = false;
    for acct in accounts.iter_mut() {
        match ensure_fresh_token(acct).await {
            Ok(true) => {
                refreshed.push(acct.name.clone());
                changed = true;
            }
            Ok(false) => {}
            Err(e) => failed.push(format!("{}：{e}", acct.name)),
        }
    }
    if changed {
        accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    }
    Ok(AutoRefreshReport { refreshed, failed })
}

/// 一条导入项：来自「导入本机账号」或「登录新账号」。
#[derive(serde::Deserialize)]
pub struct ImportItem {
    pub token: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// 导入结果：新增 / 更新各多少个（前端据此提示）
#[derive(serde::Serialize)]
pub struct ImportReport {
    pub added: usize,
    pub updated: usize,
}

/// 批量导入账号：已存在的账号**合并补全**而不是跳过。
///
/// 识别规则：手机号相同或 token 相同即视为同一账号（token 会轮换，手机号更稳定）。
/// 合并时以新凭证为准更新 token；refresh_token / expires_at 用新值覆盖（本机登录文件
/// 是权威来源）；昵称仅在原名为空时补。这样早期导入、缺续签字段的账号重新导入一次
/// 即可获得自动续签能力，也不会产生重复条目。
#[tauri::command]
pub fn import_accounts(app: AppHandle, items: Vec<ImportItem>) -> Result<ImportReport, String> {
    let dir = data_dir(&app);
    let mut accounts = accounts::load_accounts(&dir);
    let report = merge_import(&mut accounts, items);
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(report)
}

/// 合并逻辑本体（纯函数，便于单测）：见 `import_accounts`。
pub(crate) fn merge_import(accounts: &mut Vec<Account>, items: Vec<ImportItem>) -> ImportReport {
    let mut added = 0;
    let mut updated = 0;
    for it in items {
        let token = it.token.trim().to_string();
        if token.is_empty() {
            continue;
        }
        let phone = it
            .phone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let found = accounts.iter_mut().find(|a| {
            a.token == token || phone.is_some() && a.phone.as_deref() == phone.as_deref()
        });
        match found {
            Some(a) => {
                a.token = token;
                if let Some(rt) = it.refresh_token.filter(|s| !s.trim().is_empty()) {
                    a.refresh_token = Some(rt);
                }
                if it.expires_at.is_some() {
                    a.expires_at = it.expires_at;
                }
                if phone.is_some() {
                    a.phone = phone;
                }
                if a.name.trim().is_empty() {
                    if let Some(n) = it.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        a.name = n.to_string();
                    }
                }
                if a.base_url.is_none() {
                    a.base_url = it.host.clone().filter(|s| !s.trim().is_empty());
                }
                updated += 1;
            }
            None => {
                let name = it
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!("账号-{}", &token.chars().take(6).collect::<String>())
                    });
                accounts.push(Account {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    phone,
                    token,
                    refresh_token: it.refresh_token.filter(|s| !s.trim().is_empty()),
                    expires_at: it.expires_at,
                    base_url: it.host.filter(|s| !s.trim().is_empty()),
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    last: None,
                });
                added += 1;
            }
        }
    }
    ImportReport { added, updated }
}

// ── 账号导出 / 导入（跨机器迁移）────────────────────────────────────────────

/// 导出文件里的一条账号（只带迁移必需字段，不带签到状态）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct ExportAccount {
    token: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    /// 兼容两代字段名：导出写 base_url，导入同时认 base_url / host
    #[serde(default, alias = "host")]
    base_url: Option<String>,
}

/// 导出文件结构：带 kind 标记，导入时据此识别（也容忍裸数组）。
#[derive(serde::Serialize)]
struct ExportWrapper<'a> {
    app: &'a str,
    kind: &'a str,
    version: u32,
    exported_at: &'a str,
    accounts: Vec<ExportAccount>,
}

/// 导出 JSON 本体（纯函数，便于单测）。
pub(crate) fn build_export_json(accounts: &[Account], exported_at: &str) -> String {
    let items = accounts
        .iter()
        .map(|a| ExportAccount {
            token: a.token.clone(),
            name: Some(a.name.clone()),
            phone: a.phone.clone(),
            refresh_token: a.refresh_token.clone(),
            expires_at: a.expires_at,
            base_url: a.base_url.clone(),
        })
        .collect();
    serde_json::to_string_pretty(&ExportWrapper {
        app: "workbuddy-assistant",
        kind: "account-export",
        version: 1,
        exported_at,
        accounts: items,
    })
    .expect("导出 JSON 序列化不应失败")
}

/// 解析导出文件（纯函数，便于单测）。容忍三种形态：
/// 1. 本应用导出的 `{kind:"account-export", accounts:[…]}`
/// 2. 裸数组 `[… ]`（每条一个账号）
/// 3. 单个账号对象
/// 没有 token 或 token 为空的条目直接跳过。
pub(crate) fn parse_accounts_export(text: &str) -> Result<Vec<ImportItem>, String> {
    let v: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("不是有效的 JSON 文件：{e}"))?;
    let arr = match &v {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => match o.get("accounts") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => vec![v],
        },
        _ => return Err("文件里没有账号数据".into()),
    };
    Ok(arr.iter().filter_map(value_to_import_item).collect())
}

fn value_to_import_item(v: &serde_json::Value) -> Option<ImportItem> {
    let token = v.get("token")?.as_str()?.trim().to_string();
    if token.is_empty() {
        return None;
    }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some(ImportItem {
        host: s("base_url").or_else(|| s("host")),
        name: s("name"),
        phone: s("phone"),
        refresh_token: s("refresh_token"),
        expires_at: v.get("expires_at").and_then(|x| x.as_i64()),
        token,
    })
}

/// 导出全部账号到用户选择的 JSON 文件。文件含登录凭证，落盘后权限收紧为 0600。
#[tauri::command]
pub fn export_accounts(app: AppHandle, path: String) -> Result<String, String> {
    let accounts = accounts::load_accounts(&data_dir(&app));
    if accounts.is_empty() {
        return Err("还没有账号可导出".into());
    }
    let json = build_export_json(&accounts, &chrono::Local::now().to_rfc3339());
    let p = PathBuf::from(&path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    std::fs::write(&p, json).map_err(|e| format!("写入失败：{e}"))?;
    accounts::set_private_permissions(&p);
    Ok(path)
}

/// 从导出文件导入账号：复用 merge_import（按手机号/token 合并，不产生重复）。
#[tauri::command]
pub fn import_accounts_file(app: AppHandle, path: String) -> Result<ImportReport, String> {
    let text = std::fs::read_to_string(&path).map_err(|e| format!("读取失败：{e}"))?;
    let items = parse_accounts_export(&text)?;
    if items.is_empty() {
        return Err("文件里没有可导入的账号（缺少 token 字段？）".into());
    }
    let dir = data_dir(&app);
    let mut accounts = accounts::load_accounts(&dir);
    let report = merge_import(&mut accounts, items);
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod import_tests {
    use super::*;

    fn acct(name: &str, phone: Option<&str>, token: &str) -> Account {
        Account {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            phone: phone.map(str::to_string),
            token: token.into(),
            refresh_token: None,
            expires_at: None,
            base_url: None,
            created_at: String::new(),
            last: None,
        }
    }

    fn item(token: &str, phone: Option<&str>) -> ImportItem {
        ImportItem {
            token: token.into(),
            host: None,
            name: None,
            phone: phone.map(str::to_string),
            refresh_token: Some("rt-new".into()),
            expires_at: Some(123456),
        }
    }

    #[test]
    fn export_json_roundtrips_through_parser() {
        let accs = vec![
            acct("waxiloao", Some("19098779775"), "tok-1"),
            Account {
                refresh_token: Some("rt-2".into()),
                expires_at: Some(1777777777000),
                base_url: Some("https://workbuddy.ai".into()),
                ..acct("二号", None, "tok-2")
            },
        ];
        let json = build_export_json(&accs, "2026-09-13T00:00:00+08:00");
        let items = parse_accounts_export(&json).expect("自己的导出必须能解析");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].token, "tok-1");
        assert_eq!(items[0].phone.as_deref(), Some("19098779775"));
        // base_url 应映射为 host
        assert_eq!(items[1].host.as_deref(), Some("https://workbuddy.ai"));
        assert_eq!(items[1].refresh_token.as_deref(), Some("rt-2"));
        assert_eq!(items[1].expires_at, Some(1777777777000));
        // 签到状态不进导出文件
        assert!(!json.contains("last"));
    }

    #[test]
    fn parser_accepts_bare_array_and_single_object() {
        let items =
            parse_accounts_export(r#"[{"token":" t1 "},{"token":"t2","host":"https://h"}]"#)
                .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].token, "t1");
        assert_eq!(items[1].host.as_deref(), Some("https://h"));

        let one = parse_accounts_export(r#"{"token":"only"}"#).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn parser_skips_entries_without_token_and_rejects_garbage() {
        let items = parse_accounts_export(r#"[{"name":"没有token"},{"token":""},{"token":"ok"}]"#)
            .unwrap();
        assert_eq!(items.len(), 1, "无 token / 空 token 的条目应被跳过");
        assert!(parse_accounts_export("不是json").is_err());
        assert!(parse_accounts_export("42").is_err());
    }

    #[test]
    fn merges_existing_account_by_phone_and_fills_credentials() {
        let mut accs = vec![acct("waxiloao", Some("19098779775"), "old-token")];
        let r = merge_import(&mut accs, vec![item("new-token", Some("19098779775"))]);
        assert_eq!((r.added, r.updated), (0, 1), "同手机号应识别为同一账号");
        assert_eq!(accs.len(), 1, "不应产生重复条目");
        let a = &accs[0];
        assert_eq!(a.token, "new-token");
        assert_eq!(a.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(a.expires_at, Some(123456));
        assert_eq!(a.name, "waxiloao", "已有名字不应被覆盖");
    }

    #[test]
    fn adds_new_account_when_phone_and_token_unknown() {
        let mut accs = vec![acct("a", Some("111"), "t1")];
        let r = merge_import(&mut accs, vec![item("t2", Some("222"))]);
        assert_eq!((r.added, r.updated), (1, 0));
        assert_eq!(accs.len(), 2);
        assert_eq!(accs[1].name, "账号-t2", "无名导入项用默认名");
    }

    #[test]
    fn empty_tokens_are_ignored() {
        let mut accs = vec![acct("a", Some("111"), "t1")];
        let r = merge_import(&mut accs, vec![item("  ", Some("111"))]);
        assert_eq!((r.added, r.updated), (0, 0));
        assert_eq!(accs.len(), 1);
    }
}

#[tauri::command]
pub fn remove_account(app: AppHandle, id: String) -> Result<(), String> {
    let dir = data_dir(&app);
    let mut accounts = accounts::load_accounts(&dir);
    let before = accounts.len();
    accounts.retain(|a| a.id != id);
    if accounts.len() == before {
        return Err("账号不存在".into());
    }
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    // 一并清理该账号的签到日志，避免留下无归属的孤儿记录
    let _ = logs::clear_logs(&dir, Some(&id));
    Ok(())
}

#[tauri::command]
pub async fn checkin_one(app: AppHandle, id: String) -> Result<Account, String> {
    let dir = data_dir(&app);
    let mut accounts = accounts::load_accounts(&dir);
    let idx = accounts
        .iter()
        .position(|a| a.id == id)
        .ok_or("账号不存在")?;
    let settings = accounts::load_settings(&dir);
    // 续签失败不阻断：仍用旧 token 试一次，由签到结果给出明确提示
    let _ = ensure_fresh_token(&mut accounts[idx]).await;
    let rec = checkin::do_checkin(&accounts[idx], &settings.default_base_url).await;
    accounts[idx].last = Some(rec.clone());
    let _ = logs::append_log(&dir, logs::log_from_record(&accounts[idx], &rec));
    let cloned = accounts[idx].clone();
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(cloned)
}

#[tauri::command]
pub async fn checkin_all(app: AppHandle) -> Result<Vec<Account>, String> {
    let dir = data_dir(&app);
    let settings = accounts::load_settings(&dir);
    // 手动「全部签到」不启用风控间隔：用户主动触发，期望尽快完成
    let results = checkin_all_inner(&app, false).await?;
    // 手动批量签到是否推送由设置决定（默认关，免得连点几下就把通知刷屏）
    if settings.notify_enabled && settings.notify_on_manual {
        let _ = notify::send(&settings.notify_webhook, &notify::summary_message(&results)).await;
    }
    Ok(results)
}

/// 一键刷新积分：**不打签到接口**，只查每个账号的最新剩余积分并回填到列表。
///
/// 与签到解耦——已签到的账号再打签到接口只会拿到 400「已签到」，
/// 想看最新余额没必要绕这一圈。逐账号查询、失败的不改原值（界面保留旧数）。
#[tauri::command]
pub async fn refresh_all_credits(app: AppHandle) -> Result<Vec<Account>, String> {
    let dir = data_dir(&app);
    let mut accounts = accounts::load_accounts(&dir);
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let client = reqwest::Client::new();
    for i in 0..accounts.len() {
        // 凭证临期的先续签，避免拿着过期 token 把「没积分」误判成「查不到」
        let _ = ensure_fresh_token(&mut accounts[i]).await;
        let host = account_host(&accounts[i]);
        let snap = checkin::fetch_credit_snapshot(&client, &host, &accounts[i].token).await;
        if let (Some(b), Some(rec)) = (snap.credits, accounts[i].last.as_mut()) {
            rec.balance = Some(b);
        }
    }
    let cloned = accounts.clone();
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(cloned)
}

/// 「全部签到」的实际实现：逐账号签到、逐条落库，最后整体保存。
///
/// 抽成独立函数是因为定时调度（`scheduler`）与手动命令共用同一套逻辑——
/// 后台线程走不了 Tauri 的 invoke，只能直接调它。
///
/// `stagger` 控制是否启用「多账号风控间隔」：
/// - 定时自动签到传 `true`（同一 IP 瞬时连发多账号请求容易被风控，需要打散）；
/// - 页面手动「全部签到」传 `false`（用户主动点、期望尽快出结果，不故意等待）。
pub(crate) async fn checkin_all_inner(
    app: &AppHandle,
    stagger: bool,
) -> Result<Vec<Account>, String> {
    let dir = data_dir(app);
    let mut accounts = accounts::load_accounts(&dir);
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let settings = accounts::load_settings(&dir);
    for i in 0..accounts.len() {
        // 风控预防：从第二个账号起随机歇几秒再签，避免同一 IP 瞬时连发多账号请求。
        // 仅自动签到启用（手动「全部签到」跳过，避免用户等待）。
        if stagger && i > 0 {
            if let Some(secs) = stagger_seconds(settings.stagger_checkin, settings.stagger_max_seconds)
            {
                tokio::time::sleep(std::time::Duration::from_secs(secs as u64)).await;
            }
        }
        let _ = ensure_fresh_token(&mut accounts[i]).await;
        let rec = checkin::do_checkin(&accounts[i], &settings.default_base_url).await;
        accounts[i].last = Some(rec.clone());
        let _ = logs::append_log(&dir, logs::log_from_record(&accounts[i], &rec));
    }
    let cloned = accounts.clone();
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(cloned)
}

/// 首选通道：直接读本机 WorkBuddy 写在磁盘上的登录信息文件（auth/*.info）。
///
/// 不需要应用处于运行状态、不需要调试端口，且一次就能拿到 token + 昵称 + 手机号。
#[tauri::command]
pub fn discover_local_accounts() -> Result<Vec<LocalAccount>, String> {
    Ok(auth_file::discover_local_accounts())
}

/// 「无感登录」第一步：申请 state + 授权链接（不重启应用、不打断当前 WorkBuddy）。
///
/// `host` 省略时默认国内版 `https://www.workbuddy.cn`；国际版传 `https://www.workbuddy.ai`。
#[tauri::command]
pub async fn oauth_start(host: Option<String>) -> Result<oauth::OAuthStart, String> {
    oauth::start(host).await
}

/// 「无感登录」第二步：轮询一次授权结果。
///
/// 返回 `done=false` 表示用户还没完成授权（继续轮询即可，**不是错误**）；
/// `done=true` 且带 `token` 表示授权完成，`nickname` / `phone` / `uid` 一并带回。
#[tauri::command]
pub async fn oauth_poll(login_id: String) -> Result<oauth::OAuthPoll, String> {
    oauth::poll(&login_id).await
}

/// 在系统默认浏览器打开链接（用于打开无感登录的授权页）
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    oauth::open_in_browser(&url)
}

#[tauri::command]
pub fn get_settings(app: AppHandle) -> Result<Settings, String> {
    Ok(accounts::load_settings(&data_dir(&app)))
}

const WORKBUDDY_MAIN_PATTERN: &str = "^/Applications/WorkBuddy.app/Contents/MacOS/Electron$";
const WORKBUDDY_CORE_PATTERN: &str = "^/Applications/WorkBuddy.app/Contents/MacOS/Electron($| )";

/// 长驻 CLI host（对话真正跑在它里面）：argv 里带着 WorkBuddy 内置 CLI 的路径。
///
/// # 为什么必须单独杀它
///
/// 它是 Electron 桌面端 spawn 的独立 node 进程，**桌面端退出后会被孤儿化并继续存活**，
/// 而它进程环境里的 `CODEBUDDY_BASE_URL` 是 spawn 那一刻定死的：
/// - 接管开启期间启动的 host，在关闭接管后仍把请求发向已死的本地端口 →「服务异常」；
/// - 接管关闭期间启动的 host，在开启接管并重启桌面端后依然直连上游 →「感觉没走代理」。
/// 两者都只有把 host 进程杀掉、让桌面端重新 spawn 才能纠正。
const WORKBUDDY_CLI_HOST_PATTERN: &str =
    "/Applications/WorkBuddy\\.app/Contents/Resources/app\\.asar\\.unpacked/cli/";

fn process_pids(pattern: &str) -> Vec<u32> {
    std::process::Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn process_matches(pattern: &str) -> bool {
    !process_pids(pattern).is_empty()
}

/// 等到匹配进程全部消失；超时返回 false（剩余进程数用于报错信息）
fn wait_processes_gone(pattern: &str, deadline: std::time::Instant) -> usize {
    loop {
        let left = process_pids(pattern).len();
        if left == 0 || std::time::Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn kill_processes(pattern: &str) -> usize {
    let pids = process_pids(pattern);
    let n = pids.len();
    if n == 0 {
        return 0;
    }
    let args: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    // TERM 先礼后兵：TERM 等 3 秒，还在就 KILL
    let _ = std::process::Command::new("kill").args(&args).output();
    if wait_processes_gone(pattern, std::time::Instant::now() + std::time::Duration::from_secs(3)) == 0 {
        return n;
    }
    let _ = std::process::Command::new("kill")
        .args(["-9"])
        .args(&args)
        .output();
    let _ = wait_processes_gone(pattern, std::time::Instant::now() + std::time::Duration::from_secs(3));
    n
}

fn quit_workbuddy_and_wait() -> Result<usize, String> {
    let quit = std::process::Command::new("osascript")
        .args(["-e", "tell application \"WorkBuddy\" to quit"])
        .output()
        .map_err(|e| format!("执行 AppleScript 失败：{e}"))?;
    if !quit.status.success() {
        return Err(format!(
            "WorkBuddy 未能正常退出：{}",
            String::from_utf8_lossy(&quit.stderr).trim()
        ));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let left = wait_processes_gone(WORKBUDDY_CORE_PATTERN, deadline);
    if left > 0 {
        return Err("等待 WorkBuddy 退出超时；接管状态未改变。".into());
    }
    // 桌面端已退出，但长驻 CLI host 会被孤儿化继续存活——必须显式收割，
    // 否则它带着旧的环境变量继续服务对话，接管开关对它永远不生效。
    let hosts_killed = kill_processes(WORKBUDDY_CLI_HOST_PATTERN);
    Ok(hosts_killed)
}

fn open_workbuddy() -> Result<(), String> {
    let status = std::process::Command::new("open")
        .args(["-a", "WorkBuddy"])
        .status()
        .map_err(|e| format!("启动 WorkBuddy 失败：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("启动 WorkBuddy 失败（状态 {status}）"))
    }
}

pub(crate) fn restart_workbuddy_process(app: &AppHandle) -> Result<(), String> {
    let hosts_killed = quit_workbuddy_and_wait()?;
    open_workbuddy()?;
    if let Ok(dir) = try_data_dir(app) {
        crate::stealth::journal_append(
            &dir,
            "restart_workbuddy",
            &format!(
                "WorkBuddy 已重启；长驻 CLI host 终止 {hosts_killed} 个（重生后按当前接管状态取端点）"
            ),
        );
    }
    Ok(())
}

fn normalize_settings(mut settings: Settings) -> Result<Settings, String> {
    settings.schedule_time = accounts::normalize_time(&settings.schedule_time)
        .ok_or_else(|| "定时签到时刻格式应为 HH:MM（例如 09:07）".to_string())?;
    settings.notify_webhook = settings.notify_webhook.trim().to_string();
    // 风控间隔上限：钳制到合理区间，防手滑填 0（退化成无间隔）或填超大值
    settings.stagger_max_seconds = settings.stagger_max_seconds.clamp(2, 600);
    // 扣费备选账号：去空格、去空项、去重，保持原有顺序（多选池；空 = 全部可用）
    let mut seen = std::collections::HashSet::new();
    settings.billing_account_ids = settings
        .billing_account_ids
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect();
    if settings.notify_enabled && settings.notify_webhook.is_empty() {
        return Err("已开启签到通知，请填写 webhook 地址".into());
    }
    if settings.proxy_enabled && settings.proxy_port == 0 {
        return Err("反代端口不能为 0".into());
    }
    Ok(settings)
}

fn wait_for_takeover(home: &std::path::Path, dir: &std::path::Path, port: u16) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let status = crate::stealth::status(home, dir);
        if status.installed && status.alive && status.port == port {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub(crate) fn apply_settings_inner(app: &AppHandle, settings: Settings) -> Result<Settings, String> {
    let dir = data_dir(app);
    let home = dirs::home_dir().ok_or_else(|| "无法定位家目录".to_string())?;
    let old = accounts::load_settings(&dir);
    let next = normalize_settings(settings)?;
    let topology_changed = old.proxy_enabled != next.proxy_enabled
        || (old.proxy_enabled && next.proxy_enabled && old.proxy_port != next.proxy_port);
    if !topology_changed {
        accounts::save_settings(&dir, &next).map_err(|e| e.to_string())?;
        return Ok(next);
    }

    let was_running = process_matches(WORKBUDDY_MAIN_PATTERN);
    match (old.proxy_enabled, next.proxy_enabled) {
        (false, true) => {
            accounts::save_settings(&dir, &next).map_err(|e| e.to_string())?;
            if !wait_for_takeover(&home, &dir, next.proxy_port) {
                let _ = accounts::save_settings(&dir, &old);
                let _ = crate::stealth::uninstall(&home, &dir);
                return Err(format!(
                    "无法监听 127.0.0.1:{}，接管未开启。请检查端口是否被占用。",
                    next.proxy_port
                ));
            }
            if was_running {
                restart_workbuddy_process(app)?;
            }
        }
        (true, false) => {
            let note = was_running.then_some("已重启 WorkBuddy 清除长驻 CLI host 环境");
            crate::stealth::uninstall_with_note(&home, &dir, note)?;
            if was_running {
                if let Err(e) = quit_workbuddy_and_wait() {
                    let _ = crate::stealth::install(&home, &dir, old.proxy_port);
                    return Err(e);
                }
            }
            if let Err(e) = accounts::save_settings(&dir, &next) {
                let _ = crate::stealth::install(&home, &dir, old.proxy_port);
                if was_running {
                    let _ = open_workbuddy();
                }
                return Err(e.to_string());
            }
            if was_running {
                open_workbuddy()?;
            }
        }
        (true, true) => {
            let note = was_running.then_some("已重启 WorkBuddy 切换端口并清除长驻 CLI host 环境");
            crate::stealth::uninstall_with_note(&home, &dir, note)?;
            if was_running {
                if let Err(e) = quit_workbuddy_and_wait() {
                    let _ = crate::stealth::install(&home, &dir, old.proxy_port);
                    return Err(e);
                }
            }
            accounts::save_settings(&dir, &next).map_err(|e| e.to_string())?;
            if !wait_for_takeover(&home, &dir, next.proxy_port) {
                let _ = accounts::save_settings(&dir, &old);
                let _ = wait_for_takeover(&home, &dir, old.proxy_port);
                if was_running {
                    let _ = open_workbuddy();
                }
                return Err(format!("无法切换到端口 {}，已回滚原端口。", next.proxy_port));
            }
            if was_running {
                open_workbuddy()?;
            }
        }
        (false, false) => unreachable!(),
    }
    Ok(next)
}

#[tauri::command]
pub fn apply_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    apply_settings_inner(&app, settings)
}

#[tauri::command]
pub fn save_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    let dir = data_dir(&app);
    let current = accounts::load_settings(&dir);
    let settings = normalize_settings(settings)?;
    if current.proxy_enabled != settings.proxy_enabled
        || (current.proxy_enabled && current.proxy_port != settings.proxy_port)
    {
        return Err("接管启停或换端口必须使用安全切换流程。".into());
    }
    accounts::save_settings(&dir, &settings).map_err(|e| e.to_string())?;
    Ok(settings)
}

/// 发送一条测试通知，直接返回推送服务的原始响应（便于用户自查配置）
#[tauri::command]
pub async fn test_notify(webhook: String) -> Result<String, String> {
    notify::send(&webhook, "【测试】WorkBuddy 助手 · 通知配置正常").await
}

/// 是否已注册开机自启动。
///
/// 以操作系统为准（登录项 / LaunchAgent），不写进 Settings——
/// 用户可能在系统设置里手动关掉，我们不能拿一份本地缓存自欺欺人。
#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|e| format!("读取开机自启动状态失败：{e}"))
}

/// 开启/关闭开机自启动，返回落定后的真实状态
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let autolaunch = app.autolaunch();
    let res = if enabled {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
    res.map_err(|e| format!("设置开机自启动失败：{e}"))?;
    autolaunch
        .is_enabled()
        .map_err(|e| format!("读取开机自启动状态失败：{e}"))
}

/// 查询签到日志：默认倒序（最新在前），最多 200 条；可按账号筛选
#[tauri::command]
pub fn get_checkin_logs(
    app: AppHandle,
    limit: Option<usize>,
    account_id: Option<String>,
) -> Result<Vec<CheckinLog>, String> {
    let mut logs = logs::load_logs(&data_dir(&app));
    if let Some(id) = account_id {
        logs.retain(|l| l.account_id == id);
    }
    logs.reverse();
    if let Some(n) = limit {
        logs.truncate(n);
    }
    Ok(logs)
}

/// 清空签到日志：`account_id` 为空则清空全部，否则只清该账号的日志
#[tauri::command]
pub fn clear_checkin_logs(app: AppHandle, account_id: Option<String>) -> Result<(), String> {
    logs::clear_logs(&data_dir(&app), account_id.as_deref()).map_err(|e| e.to_string())
}

/// 当前应用版本（用于“关于/更新”展示）
#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 批量签到的风控间隔：返回某账号签到前应等待的秒数。
///
/// 未开启、或上限 < 2 时返回 None（不等待）。否则在 2..=max（秒）内取值——
/// 用纳秒级时钟做轻量打散即可，这里不需要密码学强度，只要别让多账号
/// 请求以固定节奏连发。
fn stagger_seconds(enabled: bool, max: u32) -> Option<u32> {
    if !enabled || max < 2 {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs().wrapping_mul(2654435761))
        .unwrap_or(0);
    Some(2 + (nanos % (max as u64 - 1)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_host_pattern_matches_host_and_not_ourselves() {
        let re = regex::Regex::new(WORKBUDDY_CLI_HOST_PATTERN).unwrap();
        // 长驻 CLI host 的典型 argv：node + WorkBuddy 内置 CLI 路径
        assert!(re.is_match(
            "/usr/local/bin/node /Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy host --session=x"
        ));
        assert!(re.is_match(
            "/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy"
        ));
        // 自家应用（WorkBuddyAssistant）绝不能被误杀
        assert!(!re.is_match(
            "/Users/waxilo/Desktop/Code/WorkBuddyAssistant/src-tauri/target/debug/workbuddy-assistant"
        ));
        assert!(!re.is_match(
            "/Applications/WorkBuddy.app/Contents/MacOS/Electron --type=renderer"
        ));
    }

    #[test]
    fn stagger_seconds_bounds_and_disabled() {
        assert_eq!(stagger_seconds(false, 45), None);
        assert_eq!(stagger_seconds(true, 0), None);
        assert_eq!(stagger_seconds(true, 2), Some(2));
        for _ in 0..50 {
            let s = stagger_seconds(true, 45).unwrap();
            assert!((2..=45).contains(&s), "间隔越界：{s}");
        }
    }
}
