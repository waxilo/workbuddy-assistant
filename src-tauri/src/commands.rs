use crate::accounts::{self, Account, Settings};
use crate::auth_file::{self, LocalAccount};
use crate::checkin;
use crate::ledger;
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

/// 这个账号接下来会真的发出续签请求吗？
///
/// 判定条件与 `ensure_fresh_token` 开头的守卫一致，并且共用同一个 `refresh::should_refresh`，
/// 所以阈值只有一处定义、不会各写一套。之所以要提前问一次，是为了让批量循环**只为真正
/// 会发生的请求**留间隔——续签本就是少数事件，若「全都不需要续签」的空转也被逐个账号拖住，
/// 一次后台自检就要凭空多花十几秒。
fn will_refresh(account: &Account) -> bool {
    account.refresh_token.is_some()
        && refresh::should_refresh(account.expires_at, chrono::Utc::now().timestamp_millis())
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
    // 上一个账号是否真的产生过出站请求：用来决定本账号前要不要让一拍。
    // 两个条件同时成立才等（上次发过 + 这次也要发），缺一个等待就纯属拖延
    let mut sent = false;
    for acct in accounts.iter_mut() {
        let due = will_refresh(acct);
        if sent && due {
            // 自动续签是后台静默循环，用户完全看不见它连发——这条路径反而更该有节奏
            crate::http::account_gap().await;
        }
        match ensure_fresh_token(acct).await {
            Ok(true) => {
                sent = true;
                refreshed.push(acct.name.clone());
                changed = true;
            }
            Ok(false) => {}
            Err(e) => {
                // 走到这里说明请求已经发出去了（只是失败），流量一样算数
                sent = true;
                failed.push(format!("{}：{e}", acct.name));
            }
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
                // 导入 = 重新同步：丢弃本地缓存的签到结果，避免陈旧的「今天失败」记录
                // 在状态列直接显示「签到失败」（真实状态由导入后的 refreshAll 以服务端为准重写）
                a.last = None;
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
                    credit_snapshot: None,
                    checked_today: None,
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
            credit_snapshot: None,
            checked_today: None,
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
    // 手动「全部签到」走短间隔档（manual_stagger_*）：同样打散顺序与节奏，只是等待短得多
    let results = checkin_all_inner(&app, false).await?;
    // 手动批量签到是否推送由设置决定（默认关，免得连点几下就把通知刷屏）
    if settings.notify_enabled && settings.notify_on_manual {
        let _ = notify::send(&settings.notify_webhook, &notify::summary_message(&results)).await;
    }
    Ok(results)
}

/// 一键刷新全部账号的三类数据，并持久化到 accounts.json：
///
/// 1. **积分快照**：剩余积分 + 最早过期时间（取自 `get-user-resource`，驱动智能接管路由）；
/// 2. **积分余量**：用快照里的剩余积分回填 `last.balance`（账号列表「剩余积分」列展示）；
/// 3. **签到状态**：只读查询「今日是否已签到」（不打签到接口、零副作用），写入 `checked_today`。
///
/// 顺带把同一份响应里的**逐包明细**喂进积分台账（见 [`crate::ledger`]）——不额外打接口，
/// 但让日报窗口里的采样更密：消耗与新增读的都是累计量，采样越密越不会漏。
///
/// 不打签到接口——已签到的账号再打只会拿到 400「已签到」，看最新状态没必要绕这一圈。
/// 逐账号查询、单个失败不改原值（界面保留旧数），最后整体保存一次。
#[tauri::command]
pub async fn refresh_all(app: AppHandle) -> Result<Vec<Account>, String> {
    let dir = data_dir(&app);
    let mut accounts = accounts::load_accounts(&dir);
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let settings = accounts::load_settings(&dir);
    // 走统一的账号接口客户端：这条路径过去是裸 `reqwest::Client::new()`，
    // 于是同一个 `get-user-resource` 接口在「刷新」里不带 UA / x-client-platform、
    // 在「签到」里带——同一个程序对同一个接口发出两套头，比固定身份更显眼
    let client = crate::http::api_client();
    let mut led = ledger::load_ledger(&dir);
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    for i in 0..accounts.len() {
        // 账号之间留抖动：每账号三四个请求，N 个账号零间隔打出去就是脚本形态。
        // 首个账号不等（它前面本就没有请求，也让首条结果尽快回到界面）
        if i > 0 {
            crate::http::account_gap().await;
        }
        // 凭证临期的先续签，避免拿着过期 token 把「没积分」误判成「查不到」
        let _ = ensure_fresh_token(&mut accounts[i]).await;
        let host = account_host(&accounts[i]);
        // 1) 资源视图：剩余积分 + 最早过期时间 + 逐包明细（明细只进台账，不落 accounts.json）
        let view = checkin::fetch_resource_view(&client, &host, &accounts[i].token).await;
        if !view.packages.is_empty() {
            ledger::merge_account(
                led.accts.entry(accounts[i].id.clone()).or_default(),
                &view.packages,
                &now,
            );
        }
        // 2) 积分余量（UI「剩余积分」列）：用视图里的 credits 回填
        let balance_from_snap = view.credits;
        accounts[i].credit_snapshot = Some(accounts::CreditSnapshot {
            credits: view.credits,
            earliest_expiry_ms: view.earliest_expiry_ms,
            fetched_at: Some(now.clone()),
        });
        if let Some(b) = balance_from_snap {
            let rec = accounts[i].last.get_or_insert_with(|| accounts::CheckinRecord {
                success: false,
                already: false,
                inactive: false,
                message: String::new(),
                credit: None,
                balance: None,
                streak: None,
                host: None,
                at: String::new(),
                code: None,
            });
            rec.balance = Some(b);
        }
        // 3) 签到状态：只读查询今日是否已签（持久化 checked_today）
        let checked = checkin::query_checked_today(&accounts[i], &settings.default_base_url).await;
        accounts[i].checked_today = checked;
    }
    // 台账落盘失败不影响刷新结果（它只是统计，丢了下次采样会重新累积）
    let _ = ledger::save_ledger(&dir, &led);
    let cloned = accounts.clone();
    accounts::save_accounts(&dir, &accounts).map_err(|e| e.to_string())?;
    Ok(cloned)
}

/// 「全部签到」的实际实现：逐账号签到、逐条落库，最后整体保存。
///
/// 抽成独立函数是因为定时调度（`scheduler`）与手动命令共用同一套逻辑——
/// 后台线程走不了 Tauri 的 invoke，只能直接调它。
///
/// `scheduled` 决定用哪一档「风控节奏」（见 [`gap_seconds`]）：
/// - `true`：定时/自动触发，两次请求之间随机歇 `stagger_max_seconds` 以内（默认 45s 档）；
/// - `false`：用户主动点（含启动即签到），走 `manual_stagger_max_seconds`（默认 8s 档）——
///   一样要等，只是等得短，避免手动路径成为全程唯一的瞬时连发入口。
///
/// 访问顺序由 [`visit_order`] 决定：默认打乱。**落盘与返回值仍按原顺序**，
/// 所以列表不会因为打乱而跳来跳去。
pub(crate) async fn checkin_all_inner(
    app: &AppHandle,
    scheduled: bool,
) -> Result<Vec<Account>, String> {
    let dir = data_dir(app);
    let mut accounts = accounts::load_accounts(&dir);
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    let settings = accounts::load_settings(&dir);
    let order = visit_order(accounts.len(), settings.shuffle_checkin_order);
    for (step, &i) in order.iter().enumerate() {
        if let Some(secs) = gap_seconds(&settings, scheduled, step) {
            tokio::time::sleep(std::time::Duration::from_secs(secs as u64)).await;
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
    settings.report_time = accounts::normalize_time(&settings.report_time)
        .ok_or_else(|| "日报结算时刻格式应为 HH:MM（例如 12:00）".to_string())?;
    settings.notify_webhook = settings.notify_webhook.trim().to_string();
    // 风控间隔上限：钳制到合理区间，防手滑填 0（退化成无间隔）或填超大值
    settings.stagger_max_seconds = settings.stagger_max_seconds.clamp(2, 600);
    settings.manual_stagger_max_seconds = settings.manual_stagger_max_seconds.clamp(2, 600);
    // 定时签到的随机窗口：0 = 关闭随机，上限 12 小时（再宽就会把签到推到半夜）
    settings.schedule_window_minutes = settings.schedule_window_minutes.min(720);
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

/// 结算一次积分日报：拉全部账号的资源视图 → 并进台账 → 聚合出**当天**的日报 → 落盘。
///
/// 定时调度（`scheduler`）与手动命令**共用这一条路径**；两份实现必然漂移。
///
/// 口径是**自然日 00:00–24:00**：某天的值 = 那天 24 个小时桶之和，而桶在每次采样时
/// 就已按「采样时刻所属的小时」归位。因此这里**不做任何封口动作**，也不再有结算基线 ——
/// 当天 12:00 结算得到的是「到今天此刻」，次日结算同一天时自动补齐成全天。
///
/// 唯一需要照顾历史的是 `granularity`：自然日口径上线时，当天在旧口径下已经
/// 累计过一段（旧口径把增量记在“结算基线”里，没有小时桶）。这一段的归属无法还原，
/// 所以**归到这一天**并在日报上标注 `granularity = 1`，避免它凭空消失 ——
/// 宁可让当天数字偏大且被如实标注，也不要让用户看到自己刚花掉的积分不见了。
pub(crate) async fn settle_report_inner(
    app: &AppHandle,
) -> Result<ledger::CreditReport, String> {
    let dir = data_dir(app);
    let accounts = accounts::load_accounts(&dir);
    let mut led = ledger::load_ledger(&dir);
    let now = chrono::Local::now();
    let now_s = now.format("%Y-%m-%d %H:%M:%S").to_string();
    let date = now.format("%Y-%m-%d").to_string();

    // 逐账号拉资源视图，把逐包明细并进台账（顺带写小时桶），同时记下结算时点的余额
    let client = crate::http::api_client();
    let mut balances: std::collections::BTreeMap<String, Option<f64>> =
        std::collections::BTreeMap::new();
    for (i, a) in accounts.iter().enumerate() {
        if i > 0 {
            crate::http::account_gap().await;
        }
        let host = account_host(a);
        let view = checkin::fetch_resource_view(&client, &host, &a.token).await;
        balances.insert(a.id.clone(), view.credits);
        if !view.packages.is_empty() {
            ledger::merge_account(
                led.accts.entry(a.id.clone()).or_default(),
                &view.packages,
                &now_s,
            );
        }
    }

    // 把「旧口径下今天已经累计、但还没有小时桶」的那部分补进当天的 00 点桶。
    // 只对还没有任何当天桶的账号做，且同一台机器只会发生一次（之后每天都有桶）。
    let mut reconciled = false;
    for a in &accounts {
        let e = led.accts.entry(a.id.clone()).or_default();
        let (d_used, d_granted) = ledger::reconcile_day_baseline(e, &date);
        if d_used > 0.0 || d_granted > 0.0 {
            reconciled = true;
        }
    }
    // 长期没采样时文件里可能还留着几个月前的桶，顺带清掉（`merge_account` 每次
    // 写入也会剪，这里兜「刚打开应用、本次还没写任何桶」的情况）
    ledger::prune_buckets(&mut led.accts, now.date_naive());

    // 当天还没走完：`sealed=false`，界面据此显示「至今」
    let rep = ledger::build_report(
        &led.accts,
        &accounts,
        &balances,
        &date,
        &now_s,
        false,
        if reconciled { 1 } else { 0 },
    );

    // 先落盘日报，成功后再保存台账（含刚写好的小时桶与已推进的旧口径基线）。
    // 写盘失败就直接返回错误，内存里的桶不落盘，下次结算还能把这段窗口补回来
    ledger::upsert_report(&dir, rep.clone()).map_err(|e| e.to_string())?;
    ledger::save_ledger(&dir, &led).map_err(|e| e.to_string())?;
    Ok(rep)
}

/// 次日把昨天封口：自然日已走完，日报补成完整一天。
///
/// 之所以要有这一步，是因为当天 12:00 结算时窗口还没结束（`window_to` = 结算时刻），
/// 而 12:00–24:00 之间产生的消耗要等次日才能体现在数字里。
/// **不需要额外采样**：桶早已在各自的采样时刻归位，这里只是把当天重新聚合一遍并标记封闭。
/// 因此即便用户次日没打开应用（[`maybe_seal_report`] 没跑到），敞口最多是
/// 「该条日报少了 12:00→24:00 那段」，而**不会丢数据** —— 那些桶仍在台账里。
pub fn seal_reports(
    dir: &std::path::Path,
    accounts: &[accounts::Account],
    today: &str,
) -> Vec<ledger::CreditReport> {
    let led = ledger::load_ledger(dir);
    let mut sealed_now = Vec::new();
    let mut all = ledger::load_reports(dir);
    for rep in all.iter_mut() {
        if rep.sealed || rep.date.as_str() >= today {
            continue;
        }
        let bal = rep
            .accounts
            .iter()
            .filter_map(|r| r.balance)
            .collect::<Vec<f64>>();
        // 重新聚合：桶是唯一事实来源，`build_report` 会把 12:00 之后的部分补进来
        let fresh = ledger::build_report(
            &led.accts,
            accounts,
            &std::collections::BTreeMap::new(),
            &rep.date,
            &rep.generated_at,
            true,
            rep.granularity,
        );
        // 余额是「结算那一刻」的快照，重新聚合取不到，沿用原来的值
        let mut merged = fresh;
        for r in merged.accounts.iter_mut() {
            if let Some(old) = rep.accounts.iter().find(|o| o.account_id == r.account_id) {
                r.balance = old.balance;
            }
        }
        merged.total_balance = if bal.is_empty() {
            None
        } else {
            Some(ledger::round2_public(bal.iter().sum()))
        };
        *rep = merged.clone();
        sealed_now.push(merged);
    }
    if !sealed_now.is_empty() {
        // 顺带把超期的桶清掉（日报列表比桶保留期长）
        let _ = ledger::save_reports_public(dir, &all);
    }
    sealed_now
}

/// 查询积分日报（新的在前）
#[tauri::command]
pub fn credit_reports(app: AppHandle) -> Result<Vec<ledger::CreditReport>, String> {
    Ok(ledger::load_reports(&data_dir(&app)))
}

/// 清空日报历史（不影响积分台账与结算基线）
#[tauri::command]
pub fn credit_reports_clear(app: AppHandle) -> Result<(), String> {
    ledger::clear_reports(&data_dir(&app)).map_err(|e| e.to_string())
}

/// 手动结算一次积分日报。
///
/// 会把基线推进到当前时刻，所以连点两次时第二条必然接近全 0 —— 这是对的
/// （「自上次结算以来」本来就什么都没发生），不是 bug。
#[tauri::command]
pub async fn credit_report_settle(app: AppHandle) -> Result<ledger::CreditReport, String> {
    settle_report_inner(&app).await
}

/// 当前应用版本（用于“关于/更新”展示）
#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 批量签到的访问顺序：返回「第 n 个被签到的账号」在列表里的下标。
///
/// `shuffle = false` 时就是顺序访问。打乱只作用于**请求次序**——调用方按它取账号，
/// 但结果照旧写回原下标、按原顺序落盘与返回，所以界面列表不会跟着跳。
fn visit_order(n: usize, shuffle: bool) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if shuffle {
        crate::rng::shuffle(&mut order);
    }
    order
}

/// 批量签到的「下一个账号之前等几秒」。返回 None = 不等。
///
/// - 第一个账号前永远不等（上来先等一段反而更像排队脚本）；
/// - 定时/自动触发走 `stagger_checkin` + `stagger_max_seconds`（默认 2..=45s）；
/// - 交互触发走 `manual_stagger` + `manual_stagger_max_seconds`（默认 2..=8s）。
///
/// 两档共用 [`stagger_seconds`]，只是上限不同：打散节奏靠的是「随机且非零」，
/// 不是某个特定秒数。
fn gap_seconds(settings: &Settings, scheduled: bool, step: usize) -> Option<u32> {
    if step == 0 {
        return None;
    }
    let (enabled, max) = if scheduled {
        (settings.stagger_checkin, settings.stagger_max_seconds)
    } else {
        (settings.manual_stagger, settings.manual_stagger_max_seconds)
    };
    stagger_seconds(enabled, max)
}

/// 返回某账号签到前应等待的秒数：未开启、或上限 < 2 时返回 None（不等待），
/// 否则在 2..=max（秒）内取值。
fn stagger_seconds(enabled: bool, max: u32) -> Option<u32> {
    if !enabled || max < 2 {
        return None;
    }
    Some(crate::rng::range(2, max as u64) as u32)
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

    #[test]
    fn visit_order_covers_everyone_exactly_once() {
        // 关掉打乱时必须原样顺序访问（老行为不能变）
        assert_eq!(visit_order(4, false), vec![0, 1, 2, 3]);
        assert!(visit_order(0, true).is_empty());
        assert_eq!(visit_order(1, true), vec![0]);

        // 打乱后仍是 0..n 的一个排列：不重不漏
        let mut changed = false;
        for _ in 0..200 {
            let order = visit_order(8, true);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..8).collect::<Vec<usize>>(), "下标被弄丢了：{order:?}");
            if order != (0..8).collect::<Vec<usize>>() {
                changed = true;
            }
        }
        assert!(changed, "200 次都没打乱顺序，说明打乱没生效");
    }

    #[test]
    fn gap_seconds_uses_the_right_dial_and_never_waits_for_the_first() {
        let mut s = Settings::default();
        // 两档上限不同：自动 45s、手动 8s（默认值）
        s.stagger_checkin = true;
        s.stagger_max_seconds = 45;
        s.manual_stagger = true;
        s.manual_stagger_max_seconds = 8;

        assert_eq!(gap_seconds(&s, true, 0), None, "第一个账号前不应等待");
        assert_eq!(gap_seconds(&s, false, 0), None, "第一个账号前不应等待");
        for _ in 0..50 {
            let auto = gap_seconds(&s, true, 1).unwrap();
            assert!((2..=45).contains(&auto), "自动档越界：{auto}");
            let manual = gap_seconds(&s, false, 1).unwrap();
            assert!((2..=8).contains(&manual), "手动档越界：{manual}");
        }

        // 各自关掉后互不影响
        s.manual_stagger = false;
        assert_eq!(gap_seconds(&s, false, 1), None);
        assert!(gap_seconds(&s, true, 1).is_some());
        s.stagger_checkin = false;
        assert_eq!(gap_seconds(&s, true, 1), None);
    }

    #[test]
    fn normalize_settings_clamps_the_guard_dials() {
        // 手滑填 0 或天文数字都不该生效（0 会让间隔退化成「无间隔」）
        let mut s = Settings::default();
        s.stagger_max_seconds = 0;
        s.manual_stagger_max_seconds = 9_999;
        s.schedule_window_minutes = 100_000;
        let n = normalize_settings(s).unwrap();
        assert_eq!(n.stagger_max_seconds, 2);
        assert_eq!(n.manual_stagger_max_seconds, 600);
        assert_eq!(n.schedule_window_minutes, 720);

        // 窗口 0 是**合法值**（= 精确到设定时刻，回到老行为），不能被当成非法输入拒掉
        let mut s = Settings::default();
        s.schedule_window_minutes = 0;
        assert_eq!(normalize_settings(s).unwrap().schedule_window_minutes, 0);
    }
}
