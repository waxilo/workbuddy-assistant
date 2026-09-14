//! 本地反代（默认 `127.0.0.1:8787`）：**WorkBuddy 专用**的智能接管通道。
//!
//! 开关只有一个：开启 = 监听本机端口 + 把 WorkBuddy 的端点指向这里；
//! 关闭 = 停止监听 + 摘掉端点。装卸与安全由 `stealth` 模块负责。
//!
//! 因为只服务本机的 WorkBuddy（监听 `127.0.0.1`，来源只可能是本机进程），
//! **没有鉴权 Key**——被接管的 WorkBuddy 也不会带任何额外请求头。
//!
//! 路由逻辑：
//!
//! 1. **会话粘滞**：带 `x-conversation-id` 的请求复用上次选中的账号 —— 一次对话中途
//!    换账号会丢上下文，必须粘住。新会话（粘滞过期或首次）才重新选。
//! 2. **选账号**：谁的「还有余量的资源包」最早过期就用谁——把快过期的积分先消耗掉；
//!    查不到过期时间的账号排最后，剩余积分为 0 的账号直接跳过（除非全员为 0）。
//! 3. **限流无感切换**：免费模型（从网关 `/v2/enterprises/personal/models` 动态拉取
//!    积分倍率，倍率为 0 即免费；1h 缓存，失败兜底 hy3）触发限流（429）时，把该账号
//!    打入 10 分钟冷却、解绑会话粘滞，换下一个账号重发同一请求（上限 2 次切换）；
//!    冷却中的账号路由优先跳过。付费模型的 429 原样透传。
//! 4. **续签兜底**：选中的账号若凭证临近过期（<48h）会先自动续签。
//! 5. **转发**：路径与查询串原样保留，替换 `Authorization` 为选中账号的 token，
//!    去掉逐跳头（Host / Content-Length 等）后透传其余请求头。
//!
//! **响应一律用 chunked 流式下发。** 对话是 SSE（`text/event-stream`），实测若缓冲成
//! 一次性 body，CLI 会报 `Empty stream` 并拿不到任何输出。
//!
//! 实现：`std::net::TcpListener` 手写 HTTP/1.1 解析（本机自用足够），
//! 上游请求用现有 async reqwest + `block_on`。监督线程每 150ms 轮询一次设置，
//! 关闭开关或改端口即自动解绑/重绑，无需重启应用。

use crate::accounts::{self, CreditSnapshot};
use crate::checkin::fetch_credit_snapshot;
use crate::commands;
use crate::stealth;
use chrono;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// WorkBuddy 对话请求带的会话标识，用作粘滞键
pub const CONV_HEADER: &str = "X-Conversation-Id";
/// 积分快照缓存时长：路由决策不必每次都打资源接口
const SNAPSHOT_TTL: Duration = Duration::from_secs(600);
/// 同一会话多久没新请求就释放粘滞（换回按积分重新选）
const STICKY_TTL: Duration = Duration::from_secs(30 * 60);
/// accept 空轮询间隔（非阻塞监听）
const ACCEPT_POLL: Duration = Duration::from_millis(150);
/// 上游连接超时（建连阶段）
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 上游单次读空闲超时：SSE 长对话会持续数分钟，**不能用总超时**——
/// reqwest 的 `timeout()` 覆盖整个响应体读取，会掐断活着的流；
/// `read_timeout()` 只管「多久没收到新数据」，才是流的正确保护方式
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 请求头 / 请求体上限
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .read_timeout(UPSTREAM_IDLE_TIMEOUT)
        // 跟随重定向会把「路由拼错」的 302 变成一页 HTML（CLI 端只见空流，无从诊断）。
        // 不跟：错误的 302 原样回到 CLI 和路由日志，一眼可见。
        .redirect(reqwest::redirect::Policy::none())
        // 官方域直连可达；若继承 shell 的 HTTP_PROXY 会把上游请求发去无关代理
        .no_proxy()
        .user_agent(concat!("WorkBuddyAssistant/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("构建 HTTP 客户端失败")
});

/// 一个账号的积分画像（缓存值）
#[derive(Clone, Copy, Default, Debug)]
struct CreditInfo {
    /// 还有余量的资源包里最早的重置时间（毫秒）；未知为 None
    expiry_ms: Option<i64>,
    /// 剩余积分；未知为 None
    credits: Option<f64>,
}

/// 会话粘滞：`x-conversation-id` → (最后命中时刻, 账号 id)
fn sticky() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    static STICKY: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();
    STICKY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 账号触发限流（429）后的冷却时长：期间路由优先跳过它
const COOLDOWN_TTL: Duration = Duration::from_secs(10 * 60);
/// 同一次客户端请求里，最多换几个账号重试（首次 + 2 次切换）
const FAILOVER_MAX_TRIES: usize = 3;

/// 限流冷却表：账号 id → 进入冷却的时刻
fn cooldown() -> &'static Mutex<HashMap<String, Instant>> {
    static COOLDOWN: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    COOLDOWN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 账号是否仍在冷却期内
fn cooling(id: &str) -> bool {
    cooldown()
        .lock()
        .ok()
        .and_then(|m| m.get(id).map(|t| t.elapsed() < COOLDOWN_TTL))
        .unwrap_or(false)
}

/// 候选软过滤（纯函数，便于单测）：优先剔除冷却中的账号；
/// 若剔完为空（全员都在冷却）则原样返回——让上游裁决也比代理直接 503 有信息量。
fn available_candidates<T: Clone>(candidates: &[T], is_cooling: impl Fn(&T) -> bool) -> Vec<T> {
    let usable: Vec<T> = candidates.iter().filter(|a| !is_cooling(a)).cloned().collect();
    if usable.is_empty() {
        candidates.to_vec()
    } else {
        usable
    }
}

/// 路由排序键：最早过期者优先 → 查不到过期时间的靠后 → 剩余积分多者略优先。
fn score(info: CreditInfo) -> (i64, i64) {
    (
        info.expiry_ms.unwrap_or(i64::MAX),
        -(info.credits.unwrap_or(0.0) * 100.0) as i64,
    )
}

/// 从候选里选出该用的账号下标。`infos` 与 `ids` 一一对应。
fn pick_index(ids: &[String], infos: &[CreditInfo]) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_score: Option<(i64, i64)> = None;
    for (i, info) in infos.iter().enumerate() {
        // 剩余积分为 0 的账号直接跳过（除非全员为 0 / 全未知）
        if info.credits == Some(0.0) {
            continue;
        }
        let s = score(*info);
        if best_score.map_or(true, |cur| s < cur) {
            best_score = Some(s);
            best = Some(i);
        }
    }
    // 全员为 0 时退化为取第一个（让上游自己报错，比代理直接 503 更有信息量）
    best.or(if ids.is_empty() { None } else { Some(0) })
}

/// 监督线程：按设置启停 / 换端口重绑，并负责接管端点的装卸与心跳。
///
/// 必须先成功监听，再安装端点。否则端口被占时会把 WorkBuddy 指向无人监听的地址。
pub fn spawn(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        if let (Ok(dir), Some(home)) = (commands::try_data_dir(&app), dirs::home_dir()) {
            stealth::sweep(&home, &dir);
        }

        let mut installed_port: Option<u16> = None;
        let mut disabled_cleaned = false;
        loop {
            let Ok(dir) = commands::try_data_dir(&app) else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            let settings = accounts::load_settings(&dir);
            if !settings.proxy_enabled {
                if !disabled_cleaned || installed_port.take().is_some() {
                    if let Some(home) = dirs::home_dir() {
                        if let Err(e) = stealth::uninstall(&home, &dir) {
                            eprintln!("[proxy] 摘除接管端点失败：{e}");
                        }
                    }
                    disabled_cleaned = true;
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            disabled_cleaned = false;
            let port = settings.proxy_port;
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => {
                    let Some(home) = dirs::home_dir() else {
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    };
                    if let Err(e) = stealth::install(&home, &dir, port) {
                        eprintln!("[proxy] 安装接管端点失败：{e}");
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    installed_port = Some(port);
                    let _ = listener.set_nonblocking(true);
                    let mut last_beat = Instant::now();
                    loop {
                        let current = accounts::load_settings(&dir);
                        if !current.proxy_enabled || current.proxy_port != port {
                            break;
                        }
                        if last_beat.elapsed() >= stealth::HEARTBEAT_INTERVAL {
                            stealth::heartbeat(&dir, port);
                            last_beat = Instant::now();
                        }
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let app2 = app.clone();
                                std::thread::spawn(move || handle_conn(stream, app2));
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(ACCEPT_POLL);
                            }
                            Err(_) => std::thread::sleep(ACCEPT_POLL),
                        }
                    }
                }
                Err(e) => {
                    if installed_port.take().is_some() {
                        if let Some(home) = dirs::home_dir() {
                            let _ = stealth::uninstall(&home, &dir);
                        }
                    }
                    eprintln!("[proxy] 无法监听 127.0.0.1:{port}：{e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// HTTP 解析（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// 解析出的请求头部分
#[derive(Debug, PartialEq)]
struct Request {
    method: String,
    /// 含查询串的路径，如 `/v2/xxx?a=1`
    target: String,
    /// 全部请求头（名字保留原样，值 trim 过）
    headers: Vec<(String, String)>,
    /// 正文字节数（按 Content-Length）
    body_len: usize,
    /// 请求头（含 `\r\n\r\n`）之后的起始偏移
    head_end: usize,
}

/// 从缓冲里解析请求行 + 请求头。返回 None 表示数据不完整或非法。
fn parse_request(buf: &[u8]) -> Option<Request> {
    let end = find_subslice(buf, b"\r\n\r\n")?;
    if end + 4 > MAX_HEAD + 4 {
        return None;
    }
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?.to_string();
    if parts.next().is_none() {
        return None;
    }

    let mut headers = Vec::new();
    let mut body_len = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        if name.eq_ignore_ascii_case("content-length") {
            body_len = value.parse().unwrap_or(0);
        }
        headers.push((name.trim().to_string(), value));
    }
    Some(Request {
        method,
        target,
        headers,
        body_len,
        head_end: end + 4,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn header_value<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// 不该透传给上游的请求头
fn hop_by_hop(name: &str) -> bool {
    [
        "host",
        "authorization",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "accept-encoding",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

// ---------------------------------------------------------------------------
// 连接处理
// ---------------------------------------------------------------------------

fn handle_conn(mut stream: TcpStream, app: tauri::AppHandle) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    // SSE 长对话可能持续数分钟，写超时要给得足够宽
    let _ = stream.set_write_timeout(Some(Duration::from_secs(600)));

    // 1. 读完请求头（+ body）
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 8192];
    let req = loop {
        match stream.read(&mut tmp) {
            Ok(0) => break None, // 对端关闭
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(req) = parse_request(&buf) {
                    if buf.len() >= req.head_end() + req.body_len {
                        break Some(req);
                    }
                }
                if buf.len() > MAX_HEAD + MAX_BODY {
                    break None;
                }
            }
            Err(_) => break None,
        }
    };
    let Some(req) = req else {
        respond(&mut stream, 400, "text/plain", b"bad request", &[]);
        return;
    };

    let Ok(dir) = commands::try_data_dir(&app) else {
        respond(&mut stream, 500, "text/plain", b"internal error", &[]);
        return;
    };

    // 2. 选账号：同一会话粘住同一个账号，新会话才按积分重新选
    //
    // 无鉴权：监听 127.0.0.1，来源只可能是本机进程（WorkBuddy 或调试用的 curl）。
    let body_start = req.head_end;
    let body = buf.get(body_start..body_start + req.body_len).unwrap_or(&[]);
    let conv = header_value(&req, CONV_HEADER)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let settings = accounts::load_settings(&dir);
    let host = settings.default_base_url;
    let bare = normalize_target(&req.target);
    let path = upstream_path(bare).to_string();
    let is_chat = bare == "/chat/completions";
    let model = if is_chat { body_model(body) } else { None };
    let url = upstream_url(&host, &path);

    // 3. 选账号并透传；免费模型（Hy3）限流（429）发生在流式输出开始前，响应头还没写给
    //    下游，正好有重试窗口：把限流账号打入冷却、解绑会话粘滞，换下一个账号重发同一
    //    请求，对 CLI 完全无感。付费模型的 429 与积分余额相关，原样透传不重试。
    //    重试次数有上限，用尽后 429 原样透传。
    let mut ban: Vec<String> = Vec::new();
    loop {
        let Some(account) =
            tauri::async_runtime::block_on(choose_account(&dir, conv.as_deref(), &ban))
        else {
            respond(&mut stream, 503, "text/plain", b"no account available", &[]);
            return;
        };
        if ban.is_empty() && is_chat {
            // 内部证据事件：仅供网络救急判定「端点被用过」，时间线展示层会过滤。
            stealth::journal_append(
                &dir,
                "proxy_request",
                &format!(
                    "长驻 CLI host 已使用接管端点（扣费账号：{}，模型：{}）",
                    account.name,
                    model.as_deref().unwrap_or("未知")
                ),
            );
        }

        // 透传
        let upstream = tauri::async_runtime::block_on(async {
            let mut r = CLIENT
                .request(
                    reqwest::Method::from_bytes(req.method.as_bytes())
                        .unwrap_or(reqwest::Method::GET),
                    &url,
                )
                .bearer_auth(&account.token);
            for (k, v) in &req.headers {
                if !hop_by_hop(k) {
                    r = r.header(k.as_str(), v.as_str());
                }
            }
            if !body.is_empty() {
                r = r.body(body.to_vec());
            }
            r.send().await
        });

        // 免费模型集（动态拉取、1h 缓存；拿不到用内置兜底）
        let free_set = if is_chat {
            ensure_free_models(&host, &account.token)
        } else {
            HashSet::new()
        };

        match upstream {
            Ok(resp) if is_chat && is_free_model(model.as_deref(), &free_set) && resp.status() == 429 => {
                // 限流账号冷却 + 会话解绑：同一会话的下一次请求也会自动绕开它
                if let Ok(mut m) = cooldown().lock() {
                    m.insert(account.id.clone(), Instant::now());
                }
                if let Some(c) = &conv {
                    if let Ok(mut s) = sticky().lock() {
                        s.remove(c);
                    }
                }
                let m = model.as_deref().unwrap_or("未知");
                if ban.len() + 1 < FAILOVER_MAX_TRIES {
                    ban.push(account.id.clone());
                    stealth::journal_append(
                        &dir,
                        "failover",
                        &format!(
                            "账号「{}」的 {m} 请求触发限流（429），已无感切换备用账号继续服务",
                            account.name
                        ),
                    );
                    continue;
                }
                stealth::journal_append(
                    &dir,
                    "failover",
                    &format!(
                        "账号「{}」的 {m} 请求触发限流（429），已无更多备用账号，限流响应原样透传",
                        account.name
                    ),
                );
                stream_response(&mut stream, resp, &dir, &account, &host, &path);
                return;
            }
            Ok(resp) => {
                stream_response(&mut stream, resp, &dir, &account, &host, &path);
                return;
            }
            Err(e) => {
                let msg = format!("upstream error: {e}");
                stealth::journal_append(&dir, "proxy_upstream_error", &msg);
                respond(&mut stream, 502, "text/plain", msg.as_bytes(), &[]);
                return;
            }
        }
    }
}

fn upstream_url(host: &str, path: &str) -> String {
    format!("{}{}", host.trim_end_matches('/'), path)
}

/// 请求目标可能是相对路径 `/chat/completions`，也可能是代理风格的绝对 URL。
/// 上游只认相对路径，这里统一剥掉协议与主机部分。
fn normalize_target(target: &str) -> &str {
    match target.find("://") {
        Some(i) => {
            let rest = &target[i + 3..];
            match rest.find('/') {
                Some(p) => &rest[p..],
                None => "/",
            }
        }
        None => target,
    }
}

/// 补上 CLI 在端点覆盖模式下丢掉的 `/v2` 前缀。
///
/// # 为什么必须由代理来补
///
/// CLI 直连官方网关时，请求的是 `https://copilot.tencent.com/v2/chat/completions`
/// （`/v2` 由 CLI 自己拼上）；而一旦设置 `CODEBUDDY_BASE_URL`，它请求的路径就变成
/// 裸的 `/chat/completions`。网关上这个裸路径**不存在**——APISIX 会 302 跳到官网，
/// CLI 收到一页 HTML、解析出 0 个 SSE 数据事件，报
/// `Empty stream: upstream gateway sent only placeholder chunks (chunks=0, bytes=0)`。
/// 所以转发前必须改写成网关真实路由 `/v2/chat/completions`。
fn upstream_path(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = path.strip_prefix("/chat/completions") {
        // 精确匹配裸路径（可带查询串），避免误伤 /chat/completions-foo 之类的路径
        if rest.is_empty() || rest.starts_with('?') {
            return format!("/v2/chat/completions{rest}").into();
        }
    }
    path.into()
}

/// 从对话请求体里取 `model` 字段（解析失败返回 None，不阻断转发）。
fn body_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str().map(str::to_string)))
}

/// 免费模型判定：不再写死模型名，从网关 `GET /v2/enterprises/personal/models`
/// 动态拉取每个模型的积分倍率（`credits` 字段，如 "x0.00 credits"/"x0.05"），
/// 倍率为 0 即免费。缓存 1 小时；拉取失败退回内置兜底（官方目录里 hy3 为
/// x0.00，而 hy3-x 是 x0.05 **不免费**——所以绝不能用 `hy3` 前缀匹配）。
const FREE_MODELS_TTL: Duration = Duration::from_secs(3600);
const FALLBACK_FREE_MODELS: [&str; 1] = ["hy3"];

/// 解析倍率字符串："x0.00 credits" / "x0.05" / "x0.79 credits" → 数字
fn parse_multiplier(s: &str) -> Option<f64> {
    s.trim()
        .trim_start_matches('x')
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// 从 models 接口响应里提取免费模型 id 集（纯函数，便于单测）
fn free_ids_from_value(v: &serde_json::Value) -> Option<HashSet<String>> {
    let arr = v.get("data")?.get("models")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_string();
                let mult = m.get("credits")?.as_str().and_then(parse_multiplier)?;
                (mult == 0.0).then_some(id)
            })
            .collect(),
    )
}

/// 免费模型集缓存：(拉取成功时刻, 模型 id 集)
fn free_models_cache() -> &'static Mutex<Option<(Instant, HashSet<String>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, HashSet<String>)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 拉取免费模型集（网络）。失败返回 None，调用方用兜底。
async fn fetch_free_models(host: &str, token: &str) -> Option<HashSet<String>> {
    let url = format!(
        "{}/v2/enterprises/personal/models",
        host.trim_end_matches('/')
    );
    let resp = CLIENT.get(&url).bearer_auth(token).send().await.ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    free_ids_from_value(&v)
}

/// 惰性获取免费模型集：缓存有效直接用；过期则用当前账号 token 拉一次；
/// 拉取失败用内置兜底（并保留旧缓存，避免每次请求都重试打接口）。
fn ensure_free_models(host: &str, token: &str) -> HashSet<String> {
    if let Ok(guard) = free_models_cache().lock() {
        if let Some((at, set)) = guard.as_ref() {
            if at.elapsed() < FREE_MODELS_TTL {
                return set.clone();
            }
        }
    }
    let fresh = tauri::async_runtime::block_on(fetch_free_models(host, token));
    match fresh {
        Some(set) if !set.is_empty() => {
            if let Ok(mut g) = free_models_cache().lock() {
                *g = Some((Instant::now(), set.clone()));
            }
            set
        }
        _ => FALLBACK_FREE_MODELS.iter().map(|s| s.to_string()).collect(),
    }
}

/// 免费判定：精确匹配动态集合
fn is_free_model(model: Option<&str>, free: &HashSet<String>) -> bool {
    model.is_some_and(|m| free.contains(m))
}

/// 「限流切换」支持的模型（供 UI 弹窗展示与手动刷新）
#[derive(serde::Serialize)]
pub struct FreeModelsReport {
    pub models: Vec<String>,
    /// "fetched" = 刚从网关拉取；"cache" = 1 小时缓存内；"fallback" = 拉取失败用内置兜底
    pub source: String,
}

fn sorted_models(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

/// 免费模型列表（限流切换的生效范围）：优先读缓存；`refresh=true` 或缓存过期时
/// 用任一账号的 token 从网关重新拉取（倍率 x0.00 的模型）。UI 弹窗展示 + 手动刷新。
#[tauri::command]
pub async fn free_models(
    app: tauri::AppHandle,
    refresh: Option<bool>,
) -> Result<FreeModelsReport, String> {
    let dir = crate::commands::try_data_dir(&app)?;
    if !refresh.unwrap_or(false) {
        if let Ok(guard) = free_models_cache().lock() {
            if let Some((at, set)) = guard.as_ref() {
                if at.elapsed() < FREE_MODELS_TTL {
                    return Ok(FreeModelsReport {
                        models: sorted_models(set),
                        source: "cache".into(),
                    });
                }
            }
        }
    }
    let settings = accounts::load_settings(&dir);
    let mut account = accounts::load_accounts(&dir)
        .into_iter()
        .find(|a| !a.token.is_empty())
        .ok_or_else(|| "暂无账号，无法拉取模型列表".to_string())?;
    // token 临近过期就先续签（不落盘也无妨：落盘版只在路由时做，这里仅求拉取成功）
    let _ = commands::ensure_fresh_token(&mut account).await;
    match fetch_free_models(&settings.default_base_url, &account.token).await {
        Some(set) if !set.is_empty() => {
            if let Ok(mut g) = free_models_cache().lock() {
                *g = Some((Instant::now(), set.clone()));
            }
            Ok(FreeModelsReport {
                models: sorted_models(&set),
                source: "fetched".into(),
            })
        }
        _ => Ok(FreeModelsReport {
            models: FALLBACK_FREE_MODELS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            source: "fallback".into(),
        }),
    }
}

/// 不该回给客户端的响应头（逐跳的，或 reqwest 已代劳解压后失效的）
fn response_hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "content-length",
        "transfer-encoding",
        "content-encoding",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// 写响应头。**一律用 chunked** —— 下游（CLI / 桌面端）按 SSE 解析，
/// 缓冲成一次性 body 会让它报 `Empty stream` 并丢掉全部输出。
fn write_head(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {ctype}\r\n\
         Transfer-Encoding: chunked\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n"
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// 写一个 chunk 并**立刻 flush**：SSE 的实时性全靠这个
fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    stream.write_all(format!("{:X}\r\n", data.len()).as_bytes())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

/// 终止 chunked 流
fn write_chunk_end(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

/// 边收边转：上游出一个 chunk 就往下游写一个。
///
/// 中途出错只能断开 —— chunked 没有「出错补报」机制，但对端看到流被截断
/// 至少比拿到一个空响应要好。
fn stream_response(
    stream: &mut TcpStream,
    mut resp: reqwest::Response,
    dir: &std::path::Path,
    account: &accounts::Account,
    host: &str,
    path: &str,
) {
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let mut headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| !response_hop_by_hop(k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    // 只回 ASCII：账号名可能是中文，直接放进响应头会破坏报文
    headers.push(("X-Proxy-Account-Id".into(), account.id.clone()));
    headers.push(("X-Proxy-Host".into(), host.to_string()));

    if write_head(stream, status, &ctype, &headers).is_err() {
        return;
    }

    let mut bytes = 0usize;
    let mut read_error: Option<String> = None;
    tauri::async_runtime::block_on(async {
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if write_chunk(stream, &chunk).is_err() {
                        break; // 下游断了，不是上游的错
                    }
                    bytes += chunk.len();
                }
                Ok(None) => break,
                // 上游读失败绝不能静默：吞掉的话 CLI 只会看到一个「干净」的空流，
                // 报 Empty stream 却查不到原因。这里留痕到接管日志。
                Err(e) => {
                    read_error = Some(format!("上游读流失败（已转发 {bytes} 字节）：{e}"));
                    break;
                }
            }
        }
    });
    if let Some(msg) = read_error {
        stealth::journal_append(dir, "proxy_stream_error", &format!("[{path}] {msg}"));
    }
    let _ = write_chunk_end(stream);
}

impl Request {
    /// 头结束（含 `\r\n\r\n`）之后的起始偏移
    fn head_end(&self) -> usize {
        self.head_end
    }
}

/// 粘滞是否命中：命中返回账号 id，顺手清掉过期项。
fn sticky_hit(conv: &str) -> Option<String> {
    let mut map = sticky().lock().ok()?;
    map.retain(|_, (at, _)| at.elapsed() < STICKY_TTL);
    let (at, id) = map.get_mut(conv)?;
    *at = Instant::now();
    Some(id.clone())
}

/// 写入/刷新会话粘滞。返回 true 表示该会话**换到了新账号**（首次上代理或被切换），
/// 调用方据此写「开始使用账号」事件；同一会话的后续请求返回 false，不刷屏。
fn sticky_put(conv: &str, account_id: String) -> bool {
    if let Ok(mut map) = sticky().lock() {
        let changed = map
            .get(conv)
            .map(|(_, id)| id != &account_id)
            .unwrap_or(true);
        map.insert(conv.to_string(), (Instant::now(), account_id));
        return changed;
    }
    false
}

/// 持久化快照是否过期（决定新会话要不要重新打资源接口）。
fn snapshot_stale(snap: Option<&CreditSnapshot>) -> bool {
    let Some(snap) = snap else { return true };
    let Some(at) = &snap.fetched_at else { return true };
    match chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|t| t.and_local_timezone(chrono::Local).single())
    {
        Some(t) => {
            chrono::Local::now().timestamp() - t.timestamp() >= SNAPSHOT_TTL.as_secs() as i64
        }
        None => true,
    }
}

/// 扣费候选集过滤（纯函数，便于单测）：设置里勾了谁，谁才有资格被扣费。
///
/// - 勾选列表为空 = 不限制，全部账号都可作为备选；
/// - 勾选的 id 在账号列表里一个都找不到（比如账号已删光）→ 退回全部，
///   宁可多扣也不能让接管直接瘫掉；用户在界面上能看到「备选为空」的提示。
fn billing_candidates(accounts: &[crate::accounts::Account], selected: &[String]) -> Vec<crate::accounts::Account> {
    if selected.is_empty() {
        return accounts.to_vec();
    }
    let picked: Vec<_> = accounts
        .iter()
        .filter(|a| selected.iter().any(|s| s == &a.id))
        .cloned()
        .collect();
    if picked.is_empty() {
        accounts.to_vec()
    } else {
        picked
    }
}

/// 选出一个该用的账号。
///
/// 候选集 = 设置里勾选的扣费账号（未勾选的不允许扣费；全不勾 = 全部可用），再做两层过滤：
/// - **禁用（严格）**：`ban` 里的账号是本轮请求已试败的限流账号，直接剔除；剔完为空返回 None；
/// - **冷却（软）**：近 10 分钟触发过限流的账号优先跳过，全员冷却则照常用。
///
/// 候选集内的优先级从高到低：
/// 1. **会话粘滞**——一次对话中途换账号会丢上下文；粘滞账号若已被移出候选集/在冷却则视为未命中；
/// 2. **智能轮换**——快照过期就重新拉，按「最旧积分」挑。
///
/// 会话首次落到某个账号（或被切换到新账号）时写一条 `route_start` 事件。
/// 若选中的账号触发了续签，会就地保存账号列表。
async fn choose_account(
    dir: &PathBuf,
    conv: Option<&str>,
    ban: &[String],
) -> Option<crate::accounts::Account> {
    let settings = accounts::load_settings(dir);
    let mut all = accounts::load_accounts(dir);
    if all.is_empty() {
        return None;
    }
    let candidates = billing_candidates(&all, &settings.billing_account_ids);
    // 限流重试时已试败的账号严格剔除：再试一次只会再吃一个 429
    let accounts: Vec<_> = candidates
        .into_iter()
        .filter(|a| !ban.iter().any(|b| b == &a.id))
        .collect();
    if accounts.is_empty() {
        return None;
    }
    let usable = available_candidates(&accounts, |a| cooling(&a.id));

    // 1) 已在进行的会话：继续用同一个账号（除非它已被移出可用集）
    if let Some(conv) = conv {
        if let Some(id) = sticky_hit(conv) {
            if let Some(a) = usable.iter().find(|a| a.id == id) {
                return Some(a.clone());
            }
        }
    }

    // 2) 新会话：快照过期就重新拉，然后按最旧积分挑
    // 2) 新会话：快照缺失/过期就从接口重拉并落盘，否则直接用持久化的积分快照
    let ids: Vec<String> = usable.iter().map(|a| a.id.clone()).collect();
    let mut infos: Vec<CreditInfo> = Vec::with_capacity(usable.len());
    let mut need_persist = false;
    for acct in &usable {
        let mut info = acct
            .credit_snapshot
            .as_ref()
            .map(|s| CreditInfo {
                expiry_ms: s.earliest_expiry_ms,
                credits: s.credits,
            })
            .unwrap_or_default();
        if snapshot_stale(acct.credit_snapshot.as_ref()) {
            let host = commands::account_host(acct);
            let snap = fetch_credit_snapshot(&CLIENT, &host, &acct.token).await;
            info = CreditInfo {
                expiry_ms: snap.earliest_expiry_ms,
                credits: snap.credits,
            };
            // 回写持久化快照（含 fetched_at），下次新会话直接读、不必再打接口
            if let Some(a) = all.iter_mut().find(|a| a.id.as_str() == acct.id.as_str()) {
                a.credit_snapshot = Some(snap);
            }
            need_persist = true;
        }
        infos.push(info);
    }
    if need_persist {
        let _ = accounts::save_accounts(dir, &all);
    }

    let idx = pick_index(&ids, &infos)?;
    let mut account = usable[idx].clone();
    // 选中的账号若凭证临近过期，先续签（失败不阻断，仍用旧 token 试）
    if commands::ensure_fresh_token(&mut account).await.unwrap_or(false) {
        // 回填**全量**账号列表落盘：只存候选子集会把未勾选的账号从磁盘上删掉
        let mut merged = all;
        if let Some(a) = merged.iter_mut().find(|a| a.id == account.id) {
            *a = account.clone();
        }
        let _ = accounts::save_accounts(dir, &merged);
    }
    if let Some(conv) = conv {
        if sticky_put(conv, account.id.clone()) {
            // 该会话第一次走上代理，或被切到了新账号 —— 记一条「开始使用」事件
            stealth::journal_append(
                dir,
                "route_start",
                &format!(
                    "开始使用账号「{}」服务会话 {}（当前扣费备选 {} 个）",
                    account.name,
                    &conv.chars().take(8).collect::<String>(),
                    usable.len()
                ),
            );
        }
    }
    Some(account)
}

/// 写一个最简 HTTP 响应并关闭连接。
fn respond(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_head_and_body_length() {
        let raw = b"POST /v2/billing/meter/daily-checkin?a=1 HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\nX-Request-Id: r1\r\n\r\n{}";
        let req = parse_request(raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/v2/billing/meter/daily-checkin?a=1");
        assert_eq!(req.body_len, 2);
        assert_eq!(req.head_end, raw.len() - 2);
        assert_eq!(header_value(&req, "x-request-id"), Some("r1"));
        assert_eq!(header_value(&req, "X-REQUEST-ID"), Some("r1"));
    }

    #[test]
    fn rejects_incomplete_or_garbage_input() {
        assert_eq!(parse_request(b"GET /x HTTP/1.1\r\n"), None, "头未读完");
        assert_eq!(parse_request(b"garbage"), None);
        // 请求行必须有三段
        assert_eq!(parse_request(b"GET /x\r\n\r\n"), None);
    }

    #[test]
    fn hop_by_hop_filters_credentials_and_length() {
        assert!(hop_by_hop("Authorization"));
        assert!(hop_by_hop("content-length"));
        assert!(!hop_by_hop("Content-Type"));
        assert!(!hop_by_hop("Accept"));
    }

    #[test]
    fn model_requests_use_the_configured_gateway() {
        assert_eq!(
            upstream_url("https://copilot.tencent.com/", "/chat/completions"),
            "https://copilot.tencent.com/chat/completions"
        );
    }

    #[test]
    fn normalizes_absolute_and_relative_targets() {
        // WorkBuddy 实测发的是相对路径
        assert_eq!(normalize_target("/chat/completions"), "/chat/completions");
        // 代理风格（绝对 URL）要把协议与主机剥掉，只留路径 + 查询串
        assert_eq!(
            normalize_target("http://copilot.tencent.com/v2/x?a=1"),
            "/v2/x?a=1"
        );
        assert_eq!(normalize_target("https://host"), "/");
    }

    #[test]
    fn rewrites_bare_chat_path_to_v2() {
        // CLI 在端点覆盖模式下发的裸路径：必须补 /v2，否则网关 302 → CLI 报 Empty stream
        assert_eq!(upstream_path("/chat/completions"), "/v2/chat/completions");
        assert_eq!(
            upstream_path("/chat/completions?a=b"),
            "/v2/chat/completions?a=b"
        );
    }

    #[test]
    fn billing_candidates_restricts_to_selected_accounts() {
        let mk = |id: &str| crate::accounts::Account {
            id: id.into(),
            name: id.into(),
            phone: None,
            token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            base_url: None,
            created_at: String::new(),
            credit_snapshot: None,
            checked_today: None,
            last: None,
        };
        let all = vec![mk("a"), mk("b"), mk("c")];
        // 未勾选 = 全部可用
        assert_eq!(billing_candidates(&all, &[]).len(), 3);
        // 勾了 b → 只有 b 有资格被扣费
        let picked = billing_candidates(&all, &["b".to_string()]);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, "b");
        // 勾选的账号全部不存在（如已被删除）→ 退回全部，接管不瘫
        assert_eq!(billing_candidates(&all, &["zzz".to_string()]).len(), 3);
    }

    #[test]
    fn available_candidates_skips_cooling_unless_all_cooling() {
        let mk = |id: &str| crate::accounts::Account {
            id: id.into(),
            name: id.into(),
            phone: None,
            token: "tok".into(),
            refresh_token: None,
            expires_at: None,
            base_url: None,
            created_at: String::new(),
            credit_snapshot: None,
            checked_today: None,
            last: None,
        };
        let all = vec![mk("a"), mk("b"), mk("c")];
        let ids = |v: &[crate::accounts::Account]| -> Vec<String> {
            v.iter().map(|a| a.id.clone()).collect()
        };
        // 无人冷却 → 原样返回
        assert_eq!(ids(&available_candidates(&all, |a| a.id == "x")).len(), 3);
        // b 在冷却 → 跳过 b
        let got = ids(&available_candidates(&all, |a| a.id == "b"));
        assert_eq!(got, vec!["a".to_string(), "c".to_string()]);
        // 全员冷却 → 软过滤退回全部：让上游裁决也比代理直接 503 有信息量
        assert_eq!(ids(&available_candidates(&all, |_| true)).len(), 3);
    }

    #[test]
    fn failover_only_for_free_models() {
        // 请求体缺 model / 非法 JSON → 视为未知，不触发切换
        assert_eq!(body_model(b"{}"), None);
        assert_eq!(body_model(b"not json"), None);
        assert_eq!(body_model(br#"{"model":"hy3"}"#).as_deref(), Some("hy3"));

        // 倍率解析：格式不统一（带/不带 "credits" 后缀）都要兼容
        assert_eq!(parse_multiplier("x0.00 credits"), Some(0.0));
        assert_eq!(parse_multiplier("x0.05"), Some(0.05));
        assert_eq!(parse_multiplier(" x0.79 credits "), Some(0.79));
        assert_eq!(parse_multiplier("credits"), None);

        // 从接口响应提取免费模型集：倍率 0 才算，缺 credits 字段的不算
        let sample = serde_json::json!({"data":{"models":[
            {"id":"hy3","credits":"x0.00 credits"},
            {"id":"hy3-x","credits":"x0.05"},
            {"id":"auto"},
            {"id":"glm-5.1","credits":"x0.79 credits"}
        ]}});
        let set = free_ids_from_value(&sample).unwrap();
        assert!(set.contains("hy3"));
        assert!(!set.contains("hy3-x"), "hy3-x 倍率 x0.05，不是免费模型");
        assert!(!set.contains("auto"));
        assert!(!set.contains("glm-5.1"));

        // 判定走精确匹配
        assert!(is_free_model(Some("hy3"), &set));
        assert!(!is_free_model(Some("hy3-x"), &set));
        assert!(!is_free_model(None, &set));
    }

    #[test]
    fn leaves_non_chat_paths_untouched() {
        // 已带 /v2 的、以及其它任何路径都不动
        assert_eq!(upstream_path("/v2/chat/completions"), "/v2/chat/completions");
        assert_eq!(upstream_path("/v1/models"), "/v1/models");
        assert_eq!(upstream_path("/"), "/");
        // 前缀相同但不是同一个路径，不能误伤
        assert_eq!(
            upstream_path("/chat/completions-extra"),
            "/chat/completions-extra"
        );
    }

    #[test]
    fn strips_hop_and_decompressed_headers_from_responses() {
        assert!(response_hop_by_hop("Content-Length"));
        assert!(response_hop_by_hop("transfer-encoding"));
        // reqwest 已代劳解压，这个头留着会让对端以为内容还是 gzip
        assert!(response_hop_by_hop("content-encoding"));
        assert!(!response_hop_by_hop("Content-Type"));
        assert!(!response_hop_by_hop("X-Request-Id"));
    }

    #[test]
    fn sticky_session_reuses_the_same_account() {
        // 一次对话中途换账号会丢上下文，必须粘住
        let conv = "conv-abc";
        assert!(sticky_hit(conv).is_none(), "首次访问不该命中");
        sticky_put(conv, "acct-1".into());
        assert_eq!(sticky_hit(conv).as_deref(), Some("acct-1"));
        sticky_put(conv, "acct-2".into());
        assert_eq!(
            sticky_hit(conv).as_deref(),
            Some("acct-2"),
            "同一会话被改写后应跟随最新值"
        );
        // 别的会话互不干扰
        assert!(sticky_hit("conv-other").is_none());
    }

    #[test]
    fn sticky_entry_expires_after_ttl() {
        let conv = "conv-expire";
        sticky_put(conv, "acct-1".into());
        // 把最后命中时刻拨回 TTL 之前
        if let Ok(mut m) = sticky().lock() {
            if let Some((at, _)) = m.get_mut(conv) {
                *at = Instant::now() - STICKY_TTL - Duration::from_secs(1);
            }
        }
        assert!(
            sticky_hit(conv).is_none(),
            "超过 TTL 的粘滞必须释放，好让新会话重新按积分选号"
        );
    }

    /// 流式最容易写错的就是分块长度帧与终止帧，这里用一对真实 socket 端到端校验字节。
    #[test]
    fn chunked_encoding_frames_and_terminates_correctly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut server = listener.accept().unwrap().0;

        write_head(
            &mut server,
            200,
            "text/event-stream",
            &[("X-Proxy-Account-Id".to_string(), "a1".to_string())],
        )
        .unwrap();
        // SSE 的一个事件帧，长度 13 → 十六进制 D
        write_chunk(&mut server, b"data: hello\n\n").unwrap();
        write_chunk(&mut server, b"data: [DONE]\n\n").unwrap();
        write_chunk_end(&mut server).unwrap();
        drop(server); // 关掉写端，让客户端 read 到 EOF

        let mut out = String::new();
        let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = client.read_to_string(&mut out);

        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "状态行：{out:?}");
        assert!(out.contains("Transfer-Encoding: chunked"));
        assert!(out.contains("Content-Type: text/event-stream"));
        assert!(out.contains("X-Proxy-Account-Id: a1"), "自定义头要带上");
        // 长度必须是十六进制且不含前导 0x
        assert!(out.contains("\r\nD\r\ndata: hello\n\n\r\n"), "分块长度帧：{out:?}");
        assert!(out.ends_with("0\r\n\r\n"), "必须以终止帧收尾：{out:?}");
        // SSE 内容必须原样透传，不能被改写或缓冲
        assert!(out.contains("data: [DONE]"));
    }

    /// 端到端：起一个本地 SSE 上游 → 用真实 reqwest 请求 → 走 `stream_response` 写到下游。
    ///
    /// 这是「边收边转」那条胶水的唯一自动化覆盖点：上游分块、我们解块再重新分块，
    /// 哪一步写错都会在这里露出来。不碰任何全局配置，也不消耗真实配额。
    #[test]
    fn streams_an_sse_upstream_end_to_end() {
        // 1) 本地 mock 上游：一次性返回一个 SSE 流（chunked）
        let up = TcpListener::bind("127.0.0.1:0").unwrap();
        let up_port = up.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = up.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf); // 读掉请求头，读多少算多少
                let body = "data: {\"a\":1}\n\ndata: [DONE]\n\n";
                let head = "HTTP/1.1 200 OK\r\n\
                            Content-Type: text/event-stream\r\n\
                            Transfer-Encoding: chunked\r\n\r\n";
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(format!("{:X}\r\n{body}\r\n", body.len()).as_bytes());
                let _ = s.write_all(b"0\r\n\r\n");
                let _ = s.flush();
            }
        });

        let resp = tauri::async_runtime::block_on(async {
            CLIENT
                .get(format!("http://127.0.0.1:{up_port}/chat/completions"))
                .send()
                .await
        })
        .expect("请求 mock 上游失败");

        // 2) 下游：一对真实 socket
        let down = TcpListener::bind("127.0.0.1:0").unwrap();
        let down_port = down.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", down_port)).unwrap();
        let mut server = down.accept().unwrap().0;

        let acct = accounts::Account {
            id: "acct-e2e".into(),
            name: "端到端".into(),
            phone: None,
            token: "t".into(),
            refresh_token: None,
            expires_at: None,
            base_url: None,
            created_at: String::new(),
            credit_snapshot: None,
            checked_today: None,
            last: None,
        };
        stream_response(
            &mut server,
            resp,
            std::path::Path::new("/tmp"),
            &acct,
            "mock.host",
            "/chat/completions",
        );
        drop(server); // 关写端，让客户端读到 EOF

        let mut out = String::new();
        let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = client.read_to_string(&mut out);

        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "状态行：{out:?}");
        assert!(out.contains("Transfer-Encoding: chunked"), "必须流式下发");
        assert!(
            out.contains("Content-Type: text/event-stream"),
            "内容类型要透传：{out:?}"
        );
        assert!(out.contains("X-Proxy-Account-Id: acct-e2e"), "要带上选中账号");
        // SSE 数据必须原样到达下游，不能被吞掉或改写成一次性 body
        assert!(out.contains("data: {\"a\":1}"), "SSE 帧要透传：{out:?}");
        assert!(out.contains("data: [DONE]"));
        assert!(out.ends_with("0\r\n\r\n"), "必须以终止帧收尾：{out:?}");
    }

    #[test]
    fn routing_prefers_earliest_expiry_then_most_credits() {
        let ids = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let infos = vec![
            CreditInfo { expiry_ms: Some(2000), credits: Some(100.0) },
            CreditInfo { expiry_ms: Some(1000), credits: Some(10.0) }, // 最早过期 → 胜出
            CreditInfo { expiry_ms: None, credits: Some(99999.0) },    // 未知 → 靠后
            CreditInfo { expiry_ms: Some(500), credits: Some(0.0) },   // 积分为 0 → 跳过
        ];
        assert_eq!(pick_index(&ids, &infos), Some(1));

        // 过期时间相同 → 剩余积分多者优先
        let ids2 = vec!["a".into(), "b".into()];
        let infos2 = vec![
            CreditInfo { expiry_ms: Some(1000), credits: Some(10.0) },
            CreditInfo { expiry_ms: Some(1000), credits: Some(500.0) },
        ];
        assert_eq!(pick_index(&ids2, &infos2), Some(1));
    }

    #[test]
    fn routing_falls_back_to_first_when_everyone_is_empty() {
        // 未知积分（可能还有余量）应优先于已知为 0 的账号
        let ids = vec!["a".into(), "b".into()];
        let infos = vec![
            CreditInfo { expiry_ms: Some(100), credits: Some(0.0) },
            CreditInfo::default(),
        ];
        assert_eq!(pick_index(&ids, &infos), Some(1));
        // 全员已知为 0 → 谁都不入选，退化为第一个（让上游报错，比代理 503 更有信息量）
        let ids2 = vec!["a".into(), "b".into()];
        let infos2 = vec![
            CreditInfo { expiry_ms: Some(100), credits: Some(0.0) },
            CreditInfo { expiry_ms: Some(200), credits: Some(0.0) },
        ];
        assert_eq!(pick_index(&ids2, &infos2), Some(0));
        assert_eq!(pick_index(&[], &[]), None, "没有账号就没有下标");
    }
}
