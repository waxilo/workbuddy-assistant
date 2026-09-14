//! 签到结果通知：把结果推送到用户自配的 webhook 地址。
//!
//! 对齐参考脚本 `workbuddy-checkin/workbuddy_checkin.py` 的 `notify_webhook`：
//!   * `GET {webhook}?message=<消息内容>`（query 参数，需 URL 编码）
//!   * **必须带浏览器 User-Agent**——notify-hub 前置的 Cloudflare 会按 UA 拦截
//!     裸 `Python-urllib` / 空 UA，直接返回 `403 error code: 1010`
//!   * 网络抖动自动重试，共 3 次（1.5s / 3s 退避）
//!
//! 站点实测（2026-09-12，notify-hub）：
//!   `?message=…`      → `HTTP 201 {"ok":true,"id":N,"delivered":true}` ✅
//!   `?title=…&body=…` → `HTTP 201 {"ok":true,"id":N,"empty":true}`  ❌ 内容为空
//!   `?content=…`      → `HTTP 201 {"ok":true,"id":N,"empty":true}`  ❌ 内容为空
//! 所以只发 `message`，不要自作聪明换参数名。
//!
//! 通知永远只是「尽力而为」：失败只返回错误字符串，绝不影响签到主流程。

use crate::accounts::Account;
use std::time::Duration;

/// 普通浏览器 UA。Cloudflare 按 UA 拦截脚本类客户端，缺了它必然 403。
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                          AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_ATTEMPTS: u32 = 3;

fn excerpt(s: &str) -> String {
    let t = s.trim();
    let cut: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        format!("{cut}…")
    } else {
        cut
    }
}

/// 把消息拼进 webhook 的 query（`?message=…`），负责百分号编码。
///
/// 单独抽出来是为了可单测：中文、空格、`&`/`#` 都必须被正确转义，
/// 否则签到结果里的失败原因会截断 URL。
pub fn build_url(webhook: &str, message: &str) -> Result<String, String> {
    let raw = webhook.trim();
    if raw.is_empty() {
        return Err("未配置通知 webhook".to_string());
    }
    let mut url = reqwest::Url::parse(raw).map_err(|e| format!("webhook 地址无效：{e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("webhook 仅支持 http(s) 地址".to_string());
    }
    url.query_pairs_mut().append_pair("message", message);
    Ok(url.to_string())
}

/// 发送一条通知。返回可读的成功描述（含响应体片段）或最终失败原因。
pub async fn send(webhook: &str, message: &str) -> Result<String, String> {
    let url = build_url(webhook, message)?;
    let client = reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("初始化 HTTP 客户端失败：{e}"))?;

    let mut last_err = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    let b = excerpt(&body);
                    return Ok(if b.is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        format!("HTTP {} {}", status.as_u16(), b)
                    });
                }
                last_err = format!("HTTP {} {}", status.as_u16(), excerpt(&body));
            }
            Err(e) => last_err = e.to_string(),
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(1500 * attempt as u64)).await;
        }
    }
    Err(format!("已重试 {MAX_ATTEMPTS} 次仍失败：{last_err}"))
}

/// 把一批账号的签到结果汇总成一条人类可读的通知正文。
///
/// 全成功时只报数量；有失败时附上前 5 条失败明细（账号名 + 手机号 + 原因），
/// 因为推送里最有价值的信息就是「哪个账号为什么没签到」。
pub fn summary_message(accounts: &[Account]) -> String {
    let total = accounts.len();
    // 「已签」也算 success=true（幂等成功），但计数时必须与「成功」互斥，
    // 否则一个账号会被同时算进两栏（实测：1 个账号显示「成功 1 / 已签 1」）。
    let already = accounts.iter().filter(|a| matches!(&a.last, Some(r) if r.already)).count();
    let ok = accounts
        .iter()
        .filter(|a| matches!(&a.last, Some(r) if r.success && !r.already))
        .count();
    let failed: Vec<&Account> = accounts
        .iter()
        .filter(|a| match &a.last {
            Some(r) => !r.success && !r.already && !r.inactive,
            None => true,
        })
        .collect();

    let mut s = format!(
        "WorkBuddy 签到完成：成功 {ok} / 已签 {already} / 失败 {}（共 {total} 个账号）",
        failed.len()
    );
    if !failed.is_empty() {
        s.push_str("\n失败明细：");
        for a in failed.iter().take(5) {
            let who = match a.phone.as_deref().filter(|p| !p.is_empty()) {
                Some(p) => format!("{}（{}）", a.name, p),
                None => a.name.clone(),
            };
            let why = a
                .last
                .as_ref()
                .map(|r| r.message.clone())
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| "未执行".to_string());
            s.push_str(&format!("\n· {who}：{why}"));
        }
        if failed.len() > 5 {
            s.push_str(&format!("\n…另有 {} 个失败账号", failed.len() - 5));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::CheckinRecord;

    fn acct(name: &str, phone: Option<&str>, rec: Option<CheckinRecord>) -> Account {
        Account {
            id: name.to_string(),
            name: name.to_string(),
            phone: phone.map(|p| p.to_string()),
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            base_url: None,
            created_at: String::new(),
            credit_snapshot: None,
            checked_today: None,
            last: rec,
        }
    }

    fn rec(success: bool, already: bool, inactive: bool, msg: &str) -> CheckinRecord {
        CheckinRecord {
            success,
            already,
            inactive,
            message: msg.to_string(),
            credit: None,
            balance: None,
            streak: None,
            host: None,
            at: String::new(),
            code: None,
        }
    }

    #[test]
    fn builds_query_with_message_param_only() {
        let u = build_url("https://hub.example/hook/abc", "签到成功").unwrap();
        // 只应出现 message，不要 title/body（站点不认，会存成空内容）
        assert!(u.contains("message="), "{u}");
        assert!(!u.contains("title="), "{u}");
        assert!(!u.contains("body="), "{u}");
        assert!(u.starts_with("https://hub.example/hook/abc?"), "{u}");
    }

    #[test]
    fn percent_encodes_tricky_characters() {
        // 中文 + 空格 + & + # 都必须被转义，否则 URL 会在这些字符处被截断
        let u = build_url("https://h/x", "失败 1&2 #tag").unwrap();
        assert!(!u.contains(' '), "{u}");
        // `Url::query_pairs_mut` 用表单编码，空格写成 `+`（与 Python 的 urlencode 一致）
        assert!(u.contains("+") || u.contains("%20"), "{u}");
        assert!(u.contains("%26"), "& 应编码：{u}");
        assert!(u.contains("%23"), "# 应编码：{u}");
        // 解码回来应与原文一致
        let parsed = reqwest::Url::parse(&u).unwrap();
        let got: Vec<_> = parsed.query_pairs().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "message");
        assert_eq!(got[0].1, "失败 1&2 #tag");
    }

    #[test]
    fn keeps_existing_query_params() {
        let u = build_url("https://h/x?k=v", "m").unwrap();
        let parsed = reqwest::Url::parse(&u).unwrap();
        let pairs: Vec<_> = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(pairs, vec![("k".into(), "v".into()), ("message".into(), "m".into())]);
    }

    #[test]
    fn rejects_empty_and_non_http_webhook() {
        assert!(build_url("", "m").unwrap_err().contains("未配置"));
        assert!(build_url("   ", "m").unwrap_err().contains("未配置"));
        assert!(build_url("ftp://h/x", "m").unwrap_err().contains("http"));
        assert!(build_url("not a url", "m").unwrap_err().contains("无效"));
    }

    #[test]
    fn summary_reports_counts_when_all_good() {
        let accounts = vec![
            acct("a", Some("138"), Some(rec(true, false, false, "签到成功"))),
            acct("b", None, Some(rec(false, true, false, "今天已签到"))),
        ];
        let m = summary_message(&accounts);
        assert!(m.contains("成功 1"), "{m}");
        assert!(m.contains("已签 1"), "{m}");
        assert!(m.contains("失败 0"), "{m}");
        // 全成功/已签不该堆失败明细
        assert!(!m.contains("失败明细"), "{m}");
    }

    #[test]
    fn already_claimed_is_counted_once_not_as_success_too() {
        // 真实接口对「今天已签到」返回 HTTP 400 + code 10001，
        // 此时 success 与 already 会同时为 true —— 计数必须互斥，
        // 否则 1 个账号会显示成「成功 1 / 已签 1」。
        let accounts = vec![acct(
            "waxiloao",
            Some("190****9775"),
            Some(rec(true, true, false, "今天已签到，请明天再来")),
        )];
        let m = summary_message(&accounts);
        assert!(m.contains("成功 0 / 已签 1"), "{m}");
        assert!(m.contains("失败 0"), "{m}");
    }

    #[test]
    fn summary_lists_failures_with_phone_and_reason() {
        let accounts = vec![
            acct("主号", Some("190****9775"), Some(rec(true, false, false, "签到成功"))),
            acct("小号", Some("138****0000"), Some(rec(false, false, false, "HTTP 401 (token expired)"))),
            acct("无记录", None, None),
        ];
        let m = summary_message(&accounts);
        assert!(m.contains("成功 1"), "{m}");
        assert!(m.contains("失败 2"), "{m}");
        assert!(m.contains("失败明细"), "{m}");
        assert!(m.contains("小号（138****0000）：HTTP 401 (token expired)"), "{m}");
        // last 为 None 的账号算失败，原因写「未执行」
        assert!(m.contains("无记录：未执行"), "{m}");
    }

    /// 本机冒烟：真的往 webhook 发一条（会收到真实推送）。
    /// 运行：`cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn smoke_real_webhook_delivery() {
        let hook = "https://notify-hub-worker.sloan.dpdns.org/hook/z9sm8jfJNpWwfWsfGW1xlRiFV8t-t6WD";
        let out = send(hook, "【测试】WorkBuddy 助手 通知链路自检")
            .await
            .unwrap_or_else(|e| panic!("webhook 发送失败: {e}"));
        // 站点成功响应形如 {"ok":true,"id":N,"delivered":true}
        assert!(out.contains("\"ok\":true"), "响应异常：{out}");
        assert!(out.contains("\"delivered\":true"), "未投递：{out}");
        println!("webhook ok → {out}");
    }
}
