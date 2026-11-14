//! 用量获取、token 刷新、自动续期、本地日志统计。

use super::claude::*;
use super::store::*;
use chrono::{Datelike, Utc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

pub const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

pub(crate) fn api_base() -> String {
    std::env::var("CAM_API_BASE").unwrap_or_else(|_| "https://api.anthropic.com".into())
}
pub(crate) fn web_base() -> String {
    std::env::var("CAM_CLAUDE_WEB_BASE").unwrap_or_else(|_| "https://claude.ai".into())
}
pub fn token_urls() -> Vec<String> {
    std::env::var("CAM_TOKEN_URLS")
        .unwrap_or_else(|_| {
            "https://platform.claude.com/v1/oauth/token,https://console.anthropic.com/v1/oauth/token".into()
        })
        .split(',')
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// 读账号资料、读用量各最多等多久（秒；只读的小请求；续登录那一步不用这个数，照旧等足 20 秒）
pub const USAGE_TIMEOUT: u64 = 12;
/// 读资料等了这么久（秒）还没连上，就不接着读用量了
pub const USAGE_GIVE_UP: u64 = 5;

/// 一个等待时间：平时就是 secs 秒；测试里用环境变量（毫秒）改短，不用每个用例真等好几秒
pub(crate) fn wait_of(test_env: &str, secs: u64) -> Duration {
    std::env::var(test_env)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(secs))
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        .user_agent(concat!(
            "claude-account-manager/",
            env!("CARGO_PKG_VERSION")
        ))
        .try_proxy_from_env(true)
        .build()
}

/// 返回 (状态码, JSON)。网络错误时状态码为 0。
pub fn http_json(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (u16, Value) {
    http_json_timeout(method, url, headers, body, 20)
}

pub fn http_json_timeout(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
    timeout_secs: u64,
) -> (u16, Value) {
    http_json_in(
        method,
        url,
        headers,
        body,
        Duration::from_secs(timeout_secs),
    )
}

/// 同 http_json_timeout，时限可以不是整秒（剩多少时间就等多少）。
pub fn http_json_in(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
    timeout: Duration,
) -> (u16, Value) {
    let mut req = agent(timeout)
        .request(method, url)
        .set("Accept", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let res = match body {
        Some(b) => req.send_json(b.clone()),
        None => req.call(),
    };
    let parse = |resp: ureq::Response| -> (u16, Value) {
        let code = resp.status();
        let text = resp.into_string().unwrap_or_default();
        let v = if text.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&text).unwrap_or_else(
                |_| json!({"error": {"message": text.chars().take(200).collect::<String>()}}),
            )
        };
        (code, v)
    };
    match res {
        Ok(r) => parse(r),
        Err(ureq::Error::Status(code, r)) => {
            (code, error_body(&r.into_string().unwrap_or_default()))
        }
        Err(e) => (0, json!({"error": {"message": format!("网络错误：{}", e)}})),
    }
}

/// 出错（4xx / 5xx）时的响应体：接口自己答的是 JSON 对象，原样返回；不是的（代理、网关、验证页回的网页，
/// 或者不是对象的 JSON）带上 _not_json，让调用方分得清（见 token_answer_dead）。与 cam.py 的 http_json 一致。
pub fn error_body(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.is_object() => v,
        _ => json!({
            "error": {"message": text.chars().take(200).collect::<String>()},
            "_not_json": true
        }),
    }
}

/// GET 纯文本。返回 (状态, 文本)；状态 0 表示没连上。
pub fn http_text(url: &str, timeout_secs: u64) -> (u16, String) {
    http_text_in(url, Duration::from_secs(timeout_secs))
}

/// 同 http_text，时限可以不是整秒（剩多少时间就等多少）。
pub fn http_text_in(url: &str, timeout: Duration) -> (u16, String) {
    match agent(timeout).get(url).call() {
        Ok(r) => {
            let code = r.status();
            (code, r.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => (0, format!("网络错误：{}", e)),
    }
}

pub fn err_msg(code: u16, body: &Value) -> String {
    if let Some(e) = body.get("error") {
        if let Some(m) = s(e, "message").filter(|m| !m.is_empty()) {
            return format!("HTTP {}：{}", code, m);
        }
        if let Some(m) = e.as_str() {
            return format!("HTTP {}：{}", code, m);
        }
    }
    format!("HTTP {}", code)
}

fn oauth_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".into(), format!("Bearer {}", token)),
        ("anthropic-beta".into(), "oauth-2025-04-20".into()),
        ("anthropic-version".into(), "2023-06-01".into()),
    ]
}

/// GET 一个 JSON 接口，带上给的请求头，最多等 timeout
pub(crate) fn get_with_in(url: &str, h: &[(String, String)], timeout: Duration) -> (u16, Value) {
    let refs: Vec<(&str, &str)> = h.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    http_json_in("GET", url, &refs, None, timeout)
}

/// 带 OAuth access token 请求 api.anthropic.com 的只读接口。
pub fn get_oauth(token: &str, path: &str, timeout_secs: u64) -> (u16, Value) {
    let h = oauth_headers(token);
    let refs: Vec<(&str, &str)> = h.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    http_json_timeout(
        "GET",
        &format!("{}{}", api_base(), path),
        &refs,
        None,
        timeout_secs,
    )
}

/// token 接口报的这几种错说的是请求本身不对（client_id、grant_type、scope），和这个账号的登录无关
const OAUTH_REQUEST_ERRORS: [&str; 4] = [
    "invalid_client",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
];

/// token 接口这次的回答是不是在说“这个 refresh token 不能用了”。规则和 cam.py 的 _token_answer_dead 一致。
/// 只认它自己答的 400 / 401（JSON）。代理、网关、验证页回的 400 / 401 不是它答的；报的是请求本身不对
/// 也和这个账号的登录无关。这两种都不能说“登录已失效”，否则好好的账号被标成异常。
pub fn token_answer_dead(code: u16, body: &Value) -> bool {
    if !(code == 400 || code == 401) || !body.is_object() || truthy(body.get("_not_json")) {
        return false;
    }
    let e = match body.get("error") {
        Some(Value::Object(o)) => {
            let part = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("");
            format!("{} {}", part("type"), part("message"))
        }
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    !OAUTH_REQUEST_ERRORS.iter().any(|c| e.contains(c))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(x)) => !x.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub(crate) fn refresh_oauth(refresh_token: &str) -> R<Value> {
    let mut last = String::new();
    for url in token_urls() {
        // 在数据锁里调用，超时设短一些，别让其它操作等太久
        let (code, body) = http_json_timeout(
            "POST",
            &url,
            &[("Content-Type", "application/json")],
            Some(
                &json!({"grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": OAUTH_CLIENT_ID}),
            ),
            8,
        );
        if code == 200 && s(&body, "access_token").is_some_and(|t| !t.is_empty()) {
            return Ok(body);
        }
        last = err_msg(code, &body);
        if token_answer_dead(code, &body) {
            // refresh token 本身无效，换地址也没用
            return Err(format!(
                "登录已失效（{}），需要用这个账号重新登录 Claude Code 后再导入",
                last
            ));
        }
    }
    Err(format!("续登录失败（{}），稍后再试", last))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn find_acc<'a>(data: &'a Value, id: &str) -> R<&'a Value> {
    accounts(data)
        .iter()
        .find(|a| s(a, "id") == Some(id))
        .ok_or_else(|| "账号已被删除".to_string())
}

/// 不联网能拿到的 access token：当前账号读 Claude Code 正在用的；其它账号读保存的、且没过期的。
fn usable_token(store: &Store, data: &Value, id: &str) -> R<Option<(String, Value)>> {
    let saved = store
        .load_secret(id)
        .get("claudeAiOauth")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if active_code_id(data).as_deref() == Some(id) {
        let (creds, _) = read_live()?;
        let mut live: Option<(String, Value)> = None;
        if let Some(c) = creds {
            let ob = oauth_block(&c);
            if let (Some(t), Some(i)) = (
                s(&ob, "accessToken"),
                identify(data, &read_oauth_account()?, &c),
            ) {
                if s(&accounts(data)[i], "id") == Some(id) {
                    live = Some((t.to_string(), ob.clone()));
                }
            }
        }
        // 当前账号从不在这里刷新，交给 Claude Code；过期了就等它下次运行自己续，不白跑一趟拿 401
        let (t, ob) = match live {
            Some(x) => x,
            None => (
                s(&saved, "accessToken").ok_or("没有保存凭据")?.to_string(),
                saved,
            ),
        };
        if ob.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0) <= now_ms() {
            return Err("当前登录的 access token 过期了，等 Claude Code 自己续".into());
        }
        return Ok(Some((t, ob)));
    }
    let t = s(&saved, "accessToken").ok_or("没有保存凭据")?.to_string();
    let exp = saved.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(if exp > now_ms() + 120_000 {
        Some((t, saved))
    } else {
        None
    })
}

/// 返回 (access token, oauth 块)。
/// 当前生效账号只读不刷新，由 Claude Code 自己刷新。其它账号的 token 过期时，在数据锁里重新确认
/// “它仍然不是当前账号”后再换新，和切换操作互斥：不会出现刚切过去、Claude Code 和这里拿着同一个
/// refresh token 各刷一次、把账号弄成需要重新登录的情况。
/// allow_refresh=false 时（后台定时任务）不换 token，只用还没过期的。
/// rejected：服务端刚对这个 access token 答了 401（本地看还没过期）。不是当前登录的话当它过期了，续一次。
///
/// 换之前先看暂存的登录（adopt_pending_for）：这个号更新的一份登录可能还暂存着（Claude Code 换过 token，
/// 当时核对不了）。它保存的那份 refresh token 这时已经被顶掉，拿去续会被拒绝，账号就被错标成“登录已失效”，
/// 而切换到它（那时会核对暂存）又是好的。所以核对成了就用暂存的那份；核对不成就不续，报 pending_login。
/// 与 cam.py 的 ensure_fresh_token 一致。
pub fn ensure_fresh_token(
    store: &Store,
    id: &str,
    allow_refresh: bool,
    rejected: Option<&str>,
) -> R<(String, Value)> {
    let usable = |data: &Value| -> R<Option<(String, Value)>> {
        let got = usable_token(store, data, id)?;
        Ok(got.filter(|(t, _)| {
            !(rejected.is_some_and(|r| !r.is_empty() && r == t)
                && active_code_id(data).as_deref() != Some(id))
        }))
    };
    let saved_rt = || -> Option<String> {
        store
            .load_secret(id)
            .get("claudeAiOauth")
            .and_then(|o| s(o, "refreshToken"))
            .map(String::from)
    };
    let data = store.load()?;
    find_acc(&data, id)?;
    if let Some(t) = usable(&data)? {
        return Ok(t);
    }
    if !allow_refresh {
        return Err(
            "登录已过期。后台不会替它续登录（避免和 Claude Code 抢 refresh token），手动刷新一次或切换到这个账号即可"
                .into(),
        );
    }
    let _g = store.lock()?;
    let mut data = store.load()?;
    find_acc(&data, id)?;
    // 等锁期间可能已切成当前账号，或已被别的进程刷新
    if let Some(t) = usable(&data)? {
        return Ok(t);
    }
    let held = adopt_pending_for(store, &mut data, id, false, true)?;
    store.save(&mut data)?;
    if !held.is_empty() {
        return Err(pending_held_error(&held, true));
    }
    let mut retry = true;
    let ob = loop {
        if let Some(t) = usable(&data)? {
            return Ok(t);
        }
        let before = saved_rt();
        match renew_saved_login(store, &mut data, id) {
            Ok(ob) => break ob,
            Err(e) => {
                if !(retry && e.contains("登录已失效")) {
                    return Err(e);
                }
                retry = false;
                // 被拒绝的也许只是旧的那份：还有登录暂存着没核对的话（当时猜的是别的账号），看看是不是这个号更新的登录
                let held = adopt_pending_for(store, &mut data, id, false, false)?;
                store.save(&mut data)?;
                if !held.is_empty() {
                    return Err(pending_held_error(&held, false));
                }
                if saved_rt() == before {
                    return Err(e); // 没有暂存，或者暂存的是别人的：这个号的登录确实失效了
                }
            }
        }
    };
    store.save(&mut data)?;
    Ok((s(&ob, "accessToken").unwrap_or_default().to_string(), ob))
}

/// 用账号 id 自己保存的 refresh token 换一个新的 access token，存回它名下，返回新的 oauth 块。
/// 调用方持有数据锁，并且已经确认它不是当前生效的登录（没有 Claude Code 跟它抢这个 refresh token），
/// 还要负责保存 data（token_fp 改了）。refresh token 已作废或连不上时返回 Err（见 refresh_oauth）。与 cam.py 一致。
pub(crate) fn renew_saved_login(store: &Store, data: &mut Value, id: &str) -> R<Value> {
    let mut secret = store.load_secret(id);
    let mut ob = secret
        .get("claudeAiOauth")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let rt = s(&ob, "refreshToken")
        .ok_or("登录已过期且没有 refresh token")?
        .to_string();
    let tok = refresh_oauth(&rt)?;
    ob["accessToken"] = tok["access_token"].clone();
    if let Some(r) = s(&tok, "refresh_token") {
        ob["refreshToken"] = json!(r);
    }
    let secs = tok
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(28800);
    ob["expiresAt"] = json!(now_ms() + secs * 1000);
    secret["claudeAiOauth"] = ob.clone();
    if secret.get("raw").map(|r| r.is_object()).unwrap_or(false) {
        secret["raw"]["claudeAiOauth"] = ob.clone();
    }
    secret["saved_at"] = json!(now_iso());
    store.save_secret(id, &secret)?;
    let fp = token_fp(s(&ob, "refreshToken").unwrap_or_default());
    if let Some(a) = accounts_mut(data)
        .iter_mut()
        .find(|a| s(a, "id") == Some(id))
    {
        a["token_fp"] = json!(fp);
    }
    Ok(ob)
}

fn pick_window(body: &Value, key: &str) -> Value {
    match body.get(key) {
        Some(w) if w.is_object() => match w.get("utilization").and_then(|u| u.as_f64()) {
            Some(u) => {
                json!({"utilization": u, "resets_at": w.get("resets_at").cloned().unwrap_or(Value::Null)})
            }
            None => Value::Null,
        },
        _ => Value::Null,
    }
}

const WINDOWS: [&str; 4] = [
    "five_hour",
    "seven_day",
    "seven_day_opus",
    "seven_day_sonnet",
];

/// 单独计算的周额度，例如“7 天 · Fable”。接口在 limits 里按模型（或产品）给出。与 cam.py 的 scoped_windows 一致。
fn scoped_windows(body: &Value) -> Vec<Value> {
    fn name_of<'a>(scope: &'a Value, k: &str) -> Option<&'a str> {
        scope
            .get(k)
            .and_then(|v| s(v, "display_name"))
            .filter(|n| !n.is_empty())
    }
    body.get("limits")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .filter(|l| s(l, "kind") == Some("weekly_scoped"))
        .filter_map(|l| {
            let scope = l.get("scope").cloned().unwrap_or(Value::Null);
            let name = name_of(&scope, "model").or_else(|| name_of(&scope, "surface"))?;
            let pct = l.get("percent").filter(|v| v.is_number())?.as_f64()?;
            Some(json!({"name": name, "utilization": pct, "resets_at": l.get("resets_at").cloned().unwrap_or(Value::Null)}))
        })
        .collect()
}

/// 7 天额度花在了哪里：Claude Code / 网页聊天 / Cowork…，只留占比大于 0 的。与 cam.py 的 week_breakdown 一致。
fn week_breakdown(body: &Value) -> Vec<Value> {
    body.get("seven_day_breakdown")
        .and_then(|b| b.get("rows"))
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let pct = r.get("percent").filter(|v| v.is_number())?.as_f64().filter(|p| *p > 0.0)?;
            let key = r.get("key").cloned().unwrap_or(Value::Null);
            let name = r.get("display_name").filter(|n| n.as_str().map(|x| !x.is_empty()).unwrap_or(false));
            Some(json!({"key": key, "name": name.cloned().unwrap_or_else(|| key.clone()), "percent": pct}))
        })
        .collect()
}

/// 把 /api/oauth/usage（或 claude.ai 的 usage）响应整理成存下来的格式。与 cam.py 的 parse_usage_body 一致。
pub fn parse_usage_body(body: &Value, source: &str) -> Value {
    let mut usage = json!({"source": source});
    for k in WINDOWS {
        usage[k] = pick_window(body, k);
    }
    let models = scoped_windows(body);
    if !models.is_empty() {
        // 新接口里 Opus / Sonnet 的周额度也在 limits 里，别显示两遍
        let names: HashSet<String> = models
            .iter()
            .filter_map(|m| s(m, "name"))
            .map(str::to_lowercase)
            .collect();
        for (k, n) in [("seven_day_opus", "opus"), ("seven_day_sonnet", "sonnet")] {
            if names.contains(n) {
                usage[k] = Value::Null;
            }
        }
        usage["models"] = json!(models);
    }
    let parts = week_breakdown(body);
    if !parts.is_empty() {
        usage["breakdown"] = json!(parts);
    }
    if let Some(extra) = body.get("extra_usage") {
        if extra.get("is_enabled").and_then(|v| v.as_bool()) == Some(true) {
            usage["extra_usage"] = json!({
                "monthly_limit": extra.get("monthly_limit"), "used_credits": extra.get("used_credits"),
                "utilization": extra.get("utilization")
            });
        }
    }
    usage
}

/// 按 API 标价折算的价格（美元 / 百万 token）：输入、输出、缓存读取。缓存写入是输入价的 1.25 倍（5 分钟）
/// 或 2 倍（1 小时）。订阅账号不按这个收费，这里只用来比较用量大小、推算额度窗口。按前缀匹配，长的在前。
/// 与 cam.py 的 PRICES 一致。
const PRICES: [(&str, f64, f64, f64); 15] = [
    ("fable-5-1", 10.0, 50.0, 0.25),
    ("mythos-5-1", 10.0, 50.0, 0.25),
    ("fable-5", 10.0, 50.0, 1.0),
    ("mythos-5", 10.0, 50.0, 1.0),
    ("opus-5-5", 4.0, 20.0, 0.20),
    ("opus-5", 5.0, 25.0, 0.50),
    ("opus-4-8", 5.0, 25.0, 0.50),
    ("opus-4-7", 5.0, 25.0, 0.50),
    ("opus-4-6", 5.0, 25.0, 0.50),
    ("opus-4-5", 5.0, 25.0, 0.50),
    ("opus-4", 15.0, 75.0, 1.50),
    ("sonnet-5", 2.0, 10.0, 0.20),
    ("sonnet-4", 3.0, 15.0, 0.30),
    ("haiku-5-5", 0.10, 0.50, 0.01),
    ("haiku-4-5", 1.0, 5.0, 0.10),
];

/// usage 里的整数字段，没有或不是数字时为 0（小数截掉，和 cam.py 的 int() 一样）。
fn count(u: &Value, k: &str) -> i64 {
    u.get(k)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

/// 一条消息记账用得上的几个数。本机记录里的消息要留在内存里（见 local_events），几十万条，整个 usage 留着太占地方。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Tokens {
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    /// 写缓存的部分里 5 分钟的和 1 小时的（价钱不一样）；usage 没分开给时都算 5 分钟的
    write_5m: i64,
    write_1h: i64,
}

impl Tokens {
    pub(crate) fn of(u: &Value) -> Tokens {
        let cache_write = count(u, "cache_creation_input_tokens");
        let (write_5m, write_1h) = match u
            .get("cache_creation")
            .filter(|c| c.as_object().is_some_and(|o| !o.is_empty()))
        {
            Some(cc) => (
                count(cc, "ephemeral_5m_input_tokens"),
                count(cc, "ephemeral_1h_input_tokens"),
            ),
            None => (cache_write, 0),
        };
        Tokens {
            input: count(u, "input_tokens"),
            output: count(u, "output_tokens"),
            cache_write,
            cache_read: count(u, "cache_read_input_tokens"),
            write_5m,
            write_1h,
        }
    }
}

/// 一条消息按 API 价折算多少美元；不认识的模型返回 None。与 cam.py 的 message_cost 一致。
pub fn message_cost(model: &str, u: &Value) -> Option<f64> {
    token_cost(model, &Tokens::of(u))
}

fn token_cost(model: &str, t: &Tokens) -> Option<f64> {
    let m = model.to_lowercase();
    let m = m.find("claude-").map(|i| &m[i + 7..]).unwrap_or(&m);
    let &(key, mut inp, mut out, mut cr) = PRICES.iter().find(|r| m.starts_with(r.0))?;
    // Haiku 5.5 的提示超过 10 万 token 时整条按 5 倍价
    if key == "haiku-5-5" && t.input + t.cache_read + t.write_5m + t.write_1h > 100_000 {
        (inp, out, cr) = (inp * 5.0, out * 5.0, cr * 5.0);
    }
    Some(
        (t.input as f64 * inp
            + t.output as f64 * out
            + t.cache_read as f64 * cr
            + t.write_5m as f64 * inp * 1.25
            + t.write_1h as f64 * inp * 2.0)
            / 1e6,
    )
}

/// 返回 (usage, 要合并进账号的元数据)
fn fetch_code(store: &Store, acc: &Value, allow_refresh: bool) -> R<(Value, Map<String, Value>)> {
    let id = s(acc, "id").unwrap_or_default();
    let (mut token, mut ob) = ensure_fresh_token(store, id, allow_refresh, None)?;
    let timeout = wait_of("CAM_TEST_USAGE_TIMEOUT_MS", USAGE_TIMEOUT);
    let profile = |token: &str| {
        get_with_in(
            &format!("{}/api/oauth/profile", api_base()),
            &oauth_headers(token),
            timeout,
        )
    };
    // 先拿账号资料：既用来补邮箱、套餐，也用来核对这份凭据是不是这个账号的
    let t0 = Instant::now();
    let (mut c2, mut prof) = profile(&token);
    if c2 == 0 && t0.elapsed() >= wait_of("CAM_TEST_USAGE_GIVE_UP_MS", USAGE_GIVE_UP) {
        // 等了好几秒都没连上（断网、代理节点挂了）：接着读用量也是白等同样久。一下就失败的（连接被重置）才接着试
        return Err(err_msg(c2, &prof));
    }
    if c2 == 401 && allow_refresh && !truthy(prof.get("_not_json")) {
        // 本地看还没过期，服务端却不认：这份登录在别处被续过（旧的 access token 跟着作废），或者本机的钟慢了。
        // 不是当前登录的话续一次再试；当前登录原样返回，留给 Claude Code 自己续
        let (again, renewed) = ensure_fresh_token(store, id, true, Some(&token))?;
        ob = renewed;
        if again != token {
            token = again;
            (c2, prof) = profile(&token);
        }
    }
    let h = oauth_headers(&token);
    if c2 == 200 {
        let a = prof.get("account").cloned().unwrap_or_default();
        if !identity_agrees(acc, s(&a, "uuid"), s(&a, "email")) {
            return Err(format!(
                "这个账号保存的登录实际属于 {}，可能被旧的 claude 会话覆盖了。请用这个账号重新登录 Claude Code 后再导入",
                s(&a, "email").or(s(&a, "uuid")).unwrap_or("其它账号")
            ));
        }
    }
    let (code, body) = get_with_in(&format!("{}/api/oauth/usage", api_base()), &h, timeout);
    if code != 200 {
        return Err(err_msg(code, &body));
    }
    let usage = parse_usage_body(&body, "oauth");
    let mut meta = Map::new();
    if c2 == 200 {
        let a = prof.get("account").cloned().unwrap_or_default();
        let o = prof.get("organization").cloned().unwrap_or_default();
        if let (Some(v), None) = (s(&a, "uuid"), s(acc, "account_uuid")) {
            meta.insert("account_uuid".into(), json!(v));
        }
        if let Some(v) = s(&a, "email") {
            meta.insert("email".into(), json!(v));
        }
        if let Some(v) = s(&o, "name") {
            meta.insert("org_name".into(), json!(v));
        }
        let plan = if a.get("has_claude_max").and_then(|v| v.as_bool()) == Some(true) {
            Some("max".to_string())
        } else if a.get("has_claude_pro").and_then(|v| v.as_bool()) == Some(true) {
            Some("pro".to_string())
        } else {
            match s(&o, "organization_type") {
                Some("claude_team") => Some("team".into()),
                Some("claude_enterprise") => Some("enterprise".into()),
                _ => None,
            }
        };
        if let Some(p) = plan {
            meta.insert("plan".into(), json!(p));
        }
        if let Some(v) = s(&o, "rate_limit_tier") {
            meta.insert("rate_limit_tier".into(), json!(v));
        }
        let mut auto = Map::new();
        for k in [
            "subscription_status",
            "subscription_expires_at",
            "billing_type",
        ] {
            if let Some(v) = s(&o, k) {
                auto.insert(k.into(), json!(v));
            }
        }
        if !auto.is_empty() {
            meta.insert("auto".into(), Value::Object(auto));
        }
    } else if let Some(p) = s(&ob, "subscriptionType") {
        if s(acc, "plan").is_none() {
            meta.insert("plan".into(), json!(p));
        }
    }
    Ok((usage, meta))
}

/// 浏览器样式的请求头：带上这些后 claude.ai 更不容易把请求当成机器人。
/// 这仍然是用用户自己的登录态做只读查询，不绕过任何验证。和 cam.py 的 WEB_UA / web_get 一致
const WEB_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36";

/// 向 claude.ai 发起只读请求。返回 (状态码, 解析后的 JSON 或 Null, 原始文本)；状态 0 表示没连上，原始文本是错误说明
fn web_get(path: &str, sk: &str) -> (u16, Value, String) {
    let base = web_base();
    let req = agent(Duration::from_secs(20))
        .get(&format!("{}{}", base, path))
        .set("User-Agent", WEB_UA)
        .set("Accept", "application/json, text/plain, */*")
        .set("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .set("Referer", &format!("{}/", base))
        .set("Origin", &base)
        .set("Cookie", &format!("sessionKey={}", sk));
    let (code, raw) = match req.call() {
        Ok(r) => {
            let code = r.status();
            (code, r.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => return (0, Value::Null, format!("网络错误：{}", e)),
    };
    let body = serde_json::from_str(&raw).unwrap_or(Value::Null);
    (code, body, raw)
}

/// 把 claude.ai 的失败翻译成一句人话（和 cam.py 的 web_error 一致；健康分类靠这些字）
pub fn web_error(code: u16, body: &Value, raw: &str) -> String {
    if super::health::is_cloudflare(raw) {
        return "claude.ai 返回了人机验证页（通常是网络被公司管控拦截，和账号无关）。可换个网络，或只手动记录到期日。".into();
    }
    if code == 401 || code == 403 {
        return "sessionKey 已过期或无效，请在浏览器里重新复制一次（见 cam set -h）。".into();
    }
    if code == 0 {
        return s(body, "message")
            .map(String::from)
            .unwrap_or_else(|| raw.to_string());
    }
    if body.as_object().is_some_and(|o| !o.is_empty()) {
        err_msg(code, body)
    } else {
        let head: String = raw.chars().take(120).collect();
        err_msg(code, &json!({"error": {"message": head}}))
    }
}

/// 组织的 capabilities 里的订阅：claude_max / claude_pro / claude_team / claude_enterprise → max / pro / team / enterprise
pub fn plan_from_caps(caps: Option<&Value>) -> Option<&'static str> {
    let caps = caps.and_then(|c| c.as_array())?;
    [
        "claude_max",
        "claude_pro",
        "claude_team",
        "claude_enterprise",
    ]
    .into_iter()
    .find(|p| caps.iter().any(|c| c.as_str() == Some(p)))
    .map(|p| p.trim_start_matches("claude_"))
}

fn fetch_web(store: &Store, acc: &Value) -> R<(Value, Map<String, Value>)> {
    let id = s(acc, "id").unwrap_or_default();
    let sec = store.load_secret(id);
    let sk = s(&sec, "session_key")
        .ok_or("未填写 sessionKey。在浏览器登录 claude.ai 后复制 sessionKey 填到“编辑”里；或只手动记录到期日。")?;
    let (code, orgs, raw) = web_get("/api/organizations", sk);
    let list = match orgs.as_array() {
        Some(a) if code == 200 && !a.is_empty() => a.clone(),
        _ => return Err(web_error(code, &orgs, &raw)),
    };
    // 优先选个人订阅那个组织（有 claude_pro / claude_max 能力的）
    let org = list
        .iter()
        .find(|o| plan_from_caps(o.get("capabilities")).is_some())
        .unwrap_or(&list[0])
        .clone();
    let mut meta = Map::new();
    if let Some(n) = s(&org, "name") {
        meta.insert("org_name".into(), json!(n));
    }
    if let Some(p) = plan_from_caps(org.get("capabilities")) {
        meta.insert("plan".into(), json!(p));
    }
    if let Some(v) = s(&org, "rate_limit_tier") {
        meta.insert("rate_limit_tier".into(), json!(v));
    }
    // 账号邮箱，失败不影响用量
    let (code_a, acct, _) = web_get("/api/account", sk);
    if code_a == 200 {
        let email = s(&acct, "email_address")
            .or_else(|| acct.get("account").and_then(|a| s(a, "email_address")));
        if let Some(e) = email {
            meta.insert("email".into(), json!(e));
        }
    }
    let uuid = s(&org, "uuid").ok_or("组织信息缺少 uuid")?;
    let (code, body, raw) = web_get(&format!("/api/organizations/{}/usage", uuid), sk);
    if code != 200 || !body.is_object() {
        return Err(web_error(code, &body, &raw));
    }
    Ok((parse_usage_body(&body, "claude.ai"), meta))
}

fn fetch_api(store: &Store, acc: &Value) -> R<(Value, Map<String, Value>)> {
    let sec = store.load_secret(s(acc, "id").unwrap_or_default());
    let mut usage = json!({"source": "api"});
    let key = s(&sec, "api_key");
    let admin = s(&sec, "admin_key");
    if key.is_none() && admin.is_none() {
        return Err("没有填写 API Key".into());
    }
    if let Some(k) = key {
        let (code, body) = http_json(
            "GET",
            &format!("{}/v1/models?limit=1", api_base()),
            &[("x-api-key", k), ("anthropic-version", "2023-06-01")],
            None,
        );
        usage["key_valid"] = json!(code == 200);
        if code != 200 {
            usage["key_error"] = json!(err_msg(code, &body));
        }
    }
    if let Some(a) = admin {
        let n = Utc::now();
        let start = format!("{:04}-{:02}-01T00:00:00Z", n.year(), n.month());
        let mut total = 0.0f64;
        let mut page: Option<String> = None;
        for _ in 0..20 {
            let mut url = format!(
                "{}/v1/organizations/cost_report?starting_at={}&limit=31",
                api_base(),
                start
            );
            if let Some(p) = &page {
                url.push_str("&page=");
                url.push_str(p);
            }
            let (code, body) = http_json(
                "GET",
                &url,
                &[("x-api-key", a), ("anthropic-version", "2023-06-01")],
                None,
            );
            if code != 200 {
                return Err(err_msg(code, &body));
            }
            for b in body
                .get("data")
                .and_then(|d| d.as_array())
                .into_iter()
                .flatten()
            {
                for r in b
                    .get("results")
                    .and_then(|d| d.as_array())
                    .into_iter()
                    .flatten()
                {
                    let amt = match r.get("amount") {
                        Some(Value::String(x)) => x.parse::<f64>().unwrap_or(0.0),
                        Some(Value::Number(x)) => x.as_f64().unwrap_or(0.0),
                        _ => 0.0,
                    };
                    total += amt; // 单位：美分
                }
            }
            if body.get("has_more").and_then(|v| v.as_bool()) != Some(true) {
                break;
            }
            page = s(&body, "next_page").map(String::from);
            if page.is_none() {
                break;
            }
        }
        usage["cost_month_usd"] = json!((total / 100.0 * 100.0).round() / 100.0);
    }
    Ok((usage, Map::new()))
}

/// 刷新用量。网络请求在锁外并发进行，最后合并写回；只有换 token 那一步在锁内（见 ensure_fresh_token）。
/// 后台定时任务传 allow_token_refresh=false：不常用的账号 token 过期后就不再替它续，
/// 少一些和 Claude Code 抢 refresh token 的机会，也少一些多账号同时在线的请求。
pub fn refresh_usage(store: &Store, only: Option<&str>, allow_token_refresh: bool) -> R<Value> {
    let data = {
        let _g = store.lock()?;
        let mut d = store.load()?;
        // 回写 Claude Code 刚刷新过的 token；新登录的账号自动导入
        snapshot_live(store, &mut d, None, true)?;
        // 清除旧版把 Desktop 当 API 账号刷出的错误，保留真实的客户端额度快照。
        for a in accounts_mut(&mut d) {
            if s(a, "kind") == Some("claude_desktop")
                && !matches!(
                    a.get("usage").and_then(|u| s(u, "source")),
                    Some("desktop_cache" | "desktop_history")
                )
            {
                a["usage"] = Value::Null;
            }
        }
        store.save(&mut d)?;
        d
    };
    let targets: Vec<Value> = accounts(&data)
        .iter()
        .filter(|a| only.map(|o| s(a, "id") == Some(o)).unwrap_or(true))
        .filter(|a| {
            matches!(
                s(a, "kind"),
                Some("claude_code" | "claude_web" | "claude_desktop" | "api")
            )
        })
        .cloned()
        .collect();
    let mut results: Vec<(String, Value, Map<String, Value>)> = std::thread::scope(|sc| {
        let handles: Vec<_> = targets
            .iter()
            .map(|acc| {
                sc.spawn(move || {
                    let r = match s(acc, "kind") {
                        Some("claude_code") => fetch_code(store, acc, allow_token_refresh),
                        Some("claude_web") => fetch_web(store, acc),
                        Some("claude_desktop") => {
                            super::desktop_usage::fetch_desktop_usage(store, acc)
                        }
                        _ => fetch_api(store, acc),
                    };
                    let (mut u, meta) = match r {
                        Ok((mut u, m)) => {
                            u["error"] = Value::Null;
                            (u, m)
                        }
                        Err(e) => {
                            let mut u = acc
                                .get("usage")
                                .cloned()
                                .filter(|v| v.is_object())
                                .unwrap_or_else(|| json!({}));
                            u["error"] = json!(e);
                            (u, Map::new())
                        }
                    };
                    // 桌面版的两个时间由 fetch_desktop_usage 自己记，意思一样
                    if s(acc, "kind") != Some("claude_desktop") {
                        let now = now_iso();
                        // 这些数字是什么时候的：没取到就还是上次那份的时间，不把旧数字说成刚更新的
                        if u["error"].is_null() {
                            u["fetched_at"] = json!(now);
                        }
                        u["checked_at"] = json!(now); // 什么时候查的
                    }
                    (s(acc, "id").unwrap_or_default().to_string(), u, meta)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    // 本机记录里每个 Claude Code 号当前窗口用了多少，和刚拉到的百分比对照着看；统计不影响用量本身
    let is_code = |id: &str| {
        targets
            .iter()
            .any(|a| s(a, "id") == Some(id) && s(a, "kind") == Some("claude_code"))
    };
    let fetched: HashMap<String, Value> = results
        .iter()
        .filter(|(id, u, _)| u["error"].is_null() && is_code(id))
        .map(|(id, u, _)| (id.clone(), u.clone()))
        .collect();
    let local = local_window_usage(&data, &fetched, Utc::now());
    for (id, u, _) in results.iter_mut() {
        if fetched.contains_key(id) {
            u["local"] = local.get(id).cloned().unwrap_or(Value::Null);
        }
    }
    // 有账号这次新出了问题时要记下几个 claude 会话在跑：数进程慢（还可能卡住），放在拿锁之前；都没报错就不用数
    let sessions = if results.iter().any(|(_, u, _)| s(u, "error").is_some()) {
        sessions_view().0
    } else {
        0
    };
    let _g = store.lock()?;
    let mut fresh = store.load()?;
    let mut by_id: HashMap<String, (Value, Map<String, Value>)> =
        results.into_iter().map(|(id, u, m)| (id, (u, m))).collect();
    // 先于记现场：查出过“登录已失效”的号，后台这轮的“登录过期了”盖不掉它，也就不会再记一次
    for a in accounts(&fresh) {
        if let Some((u, _)) = s(a, "id").and_then(|id| by_id.get_mut(id)) {
            keep_login_invalid(a, u);
        }
    }
    // 账号刚出问题的那一刻把现场记下来（和 cam.py 的 note_incident 一致）：先看哪些号这次新出了问题，现场只算一次
    let incidents: Vec<(String, &'static str, String)> = accounts(&fresh)
        .iter()
        .filter_map(|a| {
            let id = s(a, "id")?;
            let (u, _) = by_id.get(id)?;
            let kind = super::api::incident_kind(a, u)?;
            Some((
                id.to_string(),
                kind,
                s(u, "error").unwrap_or_default().to_string(),
            ))
        })
        .collect();
    let scene = (!incidents.is_empty()).then(|| super::api::incident_scene(&fresh, sessions));
    for a in accounts_mut(&mut fresh).iter_mut() {
        let id = s(a, "id").unwrap_or_default().to_string();
        if let Some((u, meta)) = by_id.get(&id) {
            if let (Some(sc), Some((_, kind, err))) =
                (&scene, incidents.iter().find(|(i, _, _)| *i == id))
            {
                super::api::push_incident(a, sc, kind, err);
            }
            a["usage"] = u.clone();
            for (k, v) in meta {
                a[k.as_str()] = v.clone();
            }
            note_usage_day(a, chrono::Local::now().date_naive());
        }
    }
    roll_renewals(&mut fresh);
    store.save(&mut fresh)?;
    Ok(fresh)
}

/// 每个号记最近几天的每天最高用量
pub const USAGE_DAYS_KEEP: usize = 30;

/// 把这个号刚取到的 5 小时 / 7 天已用百分比记进 usage_days[那天]（当天的最高值），只留最近 USAGE_DAYS_KEEP 天。
/// 这次没取到（有错误，数字还是上一次的）、或者数字不是今天取到的（桌面版的本地记录可能是几天前的）就不记。
/// 与 cam.py 的 note_usage_day 一致（tests/shared/usage_days.json）。
pub fn note_usage_day(acc: &mut Value, today: chrono::NaiveDate) {
    let u = acc.get("usage").cloned().unwrap_or(Value::Null);
    let failed = s(&u, "error").is_some_and(|e| !e.is_empty());
    let when = s(&u, "fetched_at")
        .and_then(parse_time)
        .map(|t| t.with_timezone(&chrono::Local).date_naive());
    if failed || when != Some(today) {
        return;
    }
    // 保留一位小数，四舍五入
    let vals: Vec<Option<f64>> = ["five_hour", "seven_day"]
        .iter()
        .map(|k| {
            u.get(*k)
                .and_then(|w| w.get("utilization"))
                .and_then(|v| v.as_f64())
                .map(|p| (p * 10.0 + 0.5).floor() / 10.0)
        })
        .collect();
    if vals.iter().all(|v| v.is_none()) {
        return;
    }
    let mut days: Map<String, Value> = acc
        .get("usage_days")
        .and_then(|d| d.as_object())
        .cloned()
        .unwrap_or_default();
    let key = today.format("%Y-%m-%d").to_string();
    let old: Vec<Option<f64>> = days
        .get(&key)
        .and_then(|d| d.as_array())
        .filter(|a| a.len() == 2)
        .map(|a| a.iter().map(|v| v.as_f64()).collect())
        .unwrap_or_else(|| vec![None, None]);
    let today_vals: Vec<Value> = old
        .iter()
        .zip(&vals)
        .map(|(o, v)| match (o, v) {
            (None, None) => Value::Null,
            _ => json!(o.unwrap_or(f64::MIN).max(v.unwrap_or(f64::MIN))),
        })
        .collect();
    days.insert(key, json!(today_vals));
    let mut keys: Vec<String> = days.keys().cloned().collect();
    keys.sort();
    let start = keys.len().saturating_sub(USAGE_DAYS_KEEP);
    acc["usage_days"] = Value::Object(
        keys[start..]
            .iter()
            .map(|k| (k.clone(), days[k].clone()))
            .collect(),
    );
}

/// “登录已失效”查出来以后不让后台那轮盖掉。规则和 cam.py 的 keep_login_invalid 一致。
///
/// 后台不替不常用的号续登录，登录过期的号只会得到“登录过期了”（stale，算正常）。上次手动刷新已经查出它的
/// refresh token 被服务端拒绝、之后保存的登录也没换过（error_fp 记着当时的 token 指纹）的话，结论还是失效：
/// 不然状态一会儿异常、一会儿正常，下次手动刷新又报一遍。重新登录、导入后指纹变了，就按新的结果来。
pub fn keep_login_invalid(acc: &Value, new_usage: &mut Value) {
    if s(acc, "kind") != Some("claude_code") || !new_usage.is_object() {
        return;
    }
    let filled = |v: &Value, key: &str| s(v, key).filter(|x| !x.is_empty()).map(String::from);
    let prev = acc.get("usage").cloned().unwrap_or(Value::Null);
    let (was, fp) = (filled(&prev, "error"), filled(&prev, "error_fp"));
    let acc_fp = filled(acc, "token_fp");
    let kind_of = |e: &Option<String>| e.as_deref().and_then(super::api::health_kind_of_error);
    let mut kind = kind_of(&filled(new_usage, "error"));
    if let Some(o) = new_usage.as_object_mut() {
        o.remove("error_fp");
    }
    if kind == Some("stale")
        && fp.is_some()
        && fp == acc_fp
        && kind_of(&was) == Some("login_invalid")
    {
        new_usage["error"] = json!(was);
        kind = Some("login_invalid");
    }
    if let (Some("login_invalid"), Some(f)) = (kind, acc_fp) {
        new_usage["error_fp"] = json!(f);
    }
}

/// “3小时20分后重置（17:39）”，与 cam.py 的 fmt_reset 一致。
pub fn fmt_reset(resets_at: &str) -> String {
    let Some(t) = parse_time(resets_at) else {
        return String::new();
    };
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 {
        return "已重置".into();
    }
    let (h, m) = (secs / 3600, secs / 60 % 60);
    let local = t.with_timezone(&chrono::Local);
    if h >= 24 {
        format!(
            "{}天{}小时后重置（{}月{}日 {}）",
            h / 24,
            h % 24,
            local.month(),
            local.day(),
            local.format("%H:%M")
        )
    } else if h > 0 {
        format!("{}小时{}分后重置（{}）", h, m, local.format("%H:%M"))
    } else {
        format!("{}分后重置（{}）", m, local.format("%H:%M"))
    }
}

/// 加 n 个月，日子取 day（没有就用原来的日子），超过当月天数时取月末。与 cam.py 的 add_months 一致。
fn add_months(d: chrono::NaiveDate, n: u32, day: u32) -> chrono::NaiveDate {
    let m0 = d.month0() + n;
    let (y, m) = (d.year() + (m0 / 12) as i32, m0 % 12 + 1);
    let last = (28..=31)
        .rev()
        .find(|dd| chrono::NaiveDate::from_ymd_opt(y, m, *dd).is_some())
        .unwrap_or(28);
    chrono::NaiveDate::from_ymd_opt(y, m, day.min(last)).unwrap_or(d)
}

/// 自动续期：到期日已过且标记自动续期时，按周期顺延。
pub fn roll_renewals(data: &mut Value) {
    roll_renewals_at(data, chrono::Local::now().date_naive());
}

pub fn roll_renewals_at(data: &mut Value, today: chrono::NaiveDate) {
    for a in accounts_mut(data).iter_mut() {
        // 手改过的 accounts.json 里混进非对象时跳过，不 panic
        let Some(m) = a.get_mut("manual").filter(|m| m.is_object()) else {
            continue;
        };
        if m.get("auto_renew").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let Some(mut d) = s(m, "expires_at").and_then(parse_day) else {
            continue;
        };
        let step = if s(m, "cycle") == Some("yearly") {
            12
        } else {
            1
        };
        // 记住原始扣费日，避免 1 月 31 日顺延到 2 月 28 日后一直停在 28 日
        let anchor = m
            .get("renew_day")
            .and_then(|v| v.as_u64())
            .filter(|d| (1..=31).contains(d))
            .map(|d| d as u32)
            .unwrap_or(d.day());
        let mut changed = false;
        let mut guard = 0;
        while d < today && guard < 1200 {
            d = add_months(d, step, anchor);
            changed = true;
            guard += 1;
        }
        if changed {
            m["expires_at"] = json!(d.format("%Y-%m-%d").to_string());
            m["renew_day"] = json!(anchor);
        }
    }
}

// ---------------------------------------------------------------- 本地 JSONL 统计

/// root 下所有 .jsonl（含子目录）。跟着符号链接走（有人把项目目录链接到别的盘），隐藏目录也算，和 cam.py 的两处遍历
/// （local_events、transcript_written_ago）同一个规则。最深 8 层。经过链接才走到的目录按真实位置认：在 root 里面的不进去
/// （那些文件直接走得到），在外面的每个只进一次，链接绕圈时不会把同一批文件数很多遍。
pub(crate) fn walk_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut walk = JsonlWalk {
        real_root: fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
        linked: HashSet::new(),
        out: Vec::new(),
    };
    walk.dir(root, 0, false);
    walk.out
}

struct JsonlWalk {
    real_root: PathBuf,
    /// 经过符号链接进去过的目录（真实路径）
    linked: HashSet<PathBuf>,
    out: Vec<PathBuf>,
}

impl JsonlWalk {
    /// via_link：dir 是不是经过符号链接才走到的
    fn dir(&mut self, dir: &Path, depth: u32, via_link: bool) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(t) = e.file_type() else { continue };
            // 普通文件和目录用 read_dir 给的类型，不多查一次；符号链接看它指的是什么（断掉的链接跳过）
            let (is_dir, is_file) = if t.is_symlink() {
                match fs::metadata(&p) {
                    Ok(m) => (m.is_dir(), m.is_file()),
                    Err(_) => continue,
                }
            } else {
                (t.is_dir(), t.is_file())
            };
            if is_dir {
                let via_link = via_link || t.is_symlink();
                if via_link {
                    let Ok(real) = fs::canonicalize(&p) else {
                        continue;
                    };
                    if real.starts_with(&self.real_root) || !self.linked.insert(real) {
                        continue;
                    }
                }
                self.dir(&p, depth + 1, via_link);
            } else if is_file && p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                self.out.push(p);
            }
        }
    }
}

/// 记录文件里一条带用量的消息
struct LocalEvent {
    ts: chrono::DateTime<Utc>,
    model: Arc<str>,
    tokens: Tokens,
    /// (消息 id, requestId)：同一条消息在几个文件里都有时只算一次
    key: (Option<Box<str>>, Option<Box<str>>),
}

/// 一个记录文件读到了哪、里面带用量的消息
#[derive(Default)]
struct FileEvents {
    /// 读到的字节位置（最后一个完整行的结尾）
    pos: u64,
    size: u64,
    mtime: Option<SystemTime>,
    events: Vec<LocalEvent>,
}

type LocalCache = HashMap<PathBuf, FileEvents>;

/// 按文件缓存在进程里：没变的文件不重读，只追加了的只读新加的部分。桌面版常驻，每次刷新用量、每次打开本机用量都要
/// 这些消息，一天的记录能有几百 MB，每次从头读要零点几秒到几秒。与 cam.py 的 _LOCAL_CACHE 一致。
fn local_cache() -> &'static Mutex<LocalCache> {
    static CACHE: OnceLock<Mutex<LocalCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 一行记录里带用量的消息；不是这种行返回 None。行里有不是 UTF-8 的字节也照样读（换成 U+FFFD）。
fn parse_usage_line(raw: &[u8], last_model: Option<&Arc<str>>) -> Option<LocalEvent> {
    let line = String::from_utf8_lossy(raw);
    if !line.contains("\"usage\"") {
        return None;
    }
    let ev = serde_json::from_str::<Value>(&line).ok()?;
    let ts = s(&ev, "timestamp").and_then(parse_time)?;
    let msg = ev
        .get("message")
        .filter(|m| m.get("usage").is_some_and(Value::is_object))?;
    let model = s(msg, "model").unwrap_or_default();
    Some(LocalEvent {
        ts,
        // 一个会话里前后两条消息多半是同一个模型：名字共用一份
        model: match last_model {
            Some(m) if **m == *model => m.clone(),
            _ => Arc::from(model),
        },
        tokens: Tokens::of(&msg["usage"]),
        key: (
            s(msg, "id").map(Box::from),
            s(&ev, "requestId").map(Box::from),
        ),
    })
}

/// 从 entry 记的位置接着读 path 到最后一个完整的行，带用量的消息追加进 entry；文件变小了（被重写）就从头读。
/// Claude Code 的记录是只追加的，正在写的最后一行可能还没写完：没换行符就留到下次（文件一分钟没动过的除外）。
/// 按字节一行一行读：某一行不是合法的 UTF-8 不影响后面的行。与 cam.py 的 _scan_jsonl 一致。
fn scan_jsonl(path: &Path, meta: &fs::Metadata, entry: &mut FileEvents) -> std::io::Result<()> {
    let (size, mtime) = (meta.len(), meta.modified().ok());
    if size < entry.pos {
        *entry = FileEvents::default();
    }
    if size == entry.size && mtime.is_some() && mtime == entry.mtime {
        return Ok(());
    }
    let stale = mtime
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|d| d.as_secs() > 60);
    let mut fh = BufReader::new(fs::File::open(path)?);
    fh.seek(SeekFrom::Start(entry.pos))?;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let n = fh.read_until(b'\n', &mut raw)?;
        if n == 0 || (raw.last() != Some(&b'\n') && !stale) {
            break;
        }
        entry.pos += n as u64;
        let last_model = entry.events.last().map(|e| &e.model);
        if let Some(ev) = parse_usage_line(&raw, last_model) {
            entry.events.push(ev);
        }
    }
    (entry.size, entry.mtime) = (size, mtime);
    Ok(())
}

/// 本机 Claude Code 记录里每条带用量的消息：(时间, 模型, 用量)。同一条消息只算一次。与 cam.py 的 local_events 一致。
fn local_events(since: chrono::DateTime<Utc>) -> Vec<(chrono::DateTime<Utc>, Arc<str>, Tokens)> {
    let mut cache = local_cache().lock().unwrap_or_else(|e| e.into_inner());
    local_events_in(&claude_config_dir().join("projects"), &mut cache, since)
}

fn local_events_in(
    root: &Path,
    cache: &mut LocalCache,
    since: chrono::DateTime<Utc>,
) -> Vec<(chrono::DateTime<Utc>, Arc<str>, Tokens)> {
    let files = walk_jsonl(root);
    {
        let live: HashSet<&PathBuf> = files.iter().collect();
        cache.retain(|p, _| live.contains(p)); // 文件没了的不留
    }
    let mut recent = Vec::new();
    for f in files {
        let Ok(meta) = fs::metadata(&f) else { continue };
        let old = meta
            .modified()
            .map(|t| chrono::DateTime::<Utc>::from(t) < since)
            .unwrap_or(false);
        if old {
            continue;
        }
        if scan_jsonl(&f, &meta, cache.entry(f.clone()).or_default()).is_ok() {
            recent.push(f);
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for ev in recent
        .iter()
        .filter_map(|f| cache.get(f))
        .flat_map(|e| &e.events)
    {
        if ev.ts < since || (ev.key != (None, None) && !seen.insert(&ev.key)) {
            continue;
        }
        out.push((ev.ts, ev.model.clone(), ev.tokens));
    }
    out
}

/// 按切换记录找某个时刻登录的是哪个号。从没切换过时都算当前账号；最早一次切换之前的算不出来，返回 None。
/// 与 cam.py 的 owner_finder 一致。
fn owner_finder(data: &Value) -> impl Fn(chrono::DateTime<Utc>) -> Option<String> {
    let mut log: Vec<(chrono::DateTime<Utc>, String)> = data
        .get("switch_log")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .filter_map(|e| Some((parse_time(s(e, "at")?)?, s(e, "id")?.to_string())))
        .collect();
    log.sort_by_key(|a| a.0);
    let active = data
        .get("active")
        .and_then(|a| s(a, "claude_code"))
        .map(String::from);
    move |ts| {
        if log.is_empty() {
            return active.clone();
        }
        log.iter()
            .take_while(|(at, _)| *at <= ts)
            .last()
            .map(|(_, id)| id.clone())
    }
}

/// 保留 4 位小数，和 cam.py 的 round(x, 4) 一样按精确值取舍。
fn round4(x: f64) -> f64 {
    format!("{:.4}", x).parse().unwrap_or(x)
}

pub fn local_usage(data: &Value, days: i64) -> Value {
    let since = Utc::now() - chrono::Duration::days(days);
    let owner_of = owner_finder(data);
    // (日期, 账号) -> ([input, output, cache_write, cache_read, messages], 金额)；按 (日期, 模型) 的一份给面板画模型分布
    type Rec = ([i64; 5], f64);
    let mut daily: std::collections::BTreeMap<(String, String), Rec> = Default::default();
    let mut by_model: std::collections::BTreeMap<(String, String), Rec> = Default::default();
    let add = |rec: &mut Rec, u: &Tokens, cost: f64| {
        rec.0[0] += u.input;
        rec.0[1] += u.output;
        rec.0[2] += u.cache_write;
        rec.0[3] += u.cache_read;
        rec.0[4] += 1;
        rec.1 += cost;
    };
    for (ts, model, u) in local_events(since) {
        let owner = owner_of(ts).unwrap_or_else(|| "unattributed".to_string());
        let day = ts
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
        let cost = token_cost(&model, &u).unwrap_or(0.0);
        let name = if model.is_empty() {
            "unknown".to_string()
        } else {
            model.to_string()
        };
        add(
            daily.entry((day.clone(), owner)).or_insert(([0; 5], 0.0)),
            &u,
            cost,
        );
        add(
            by_model.entry((day, name)).or_insert(([0; 5], 0.0)),
            &u,
            cost,
        );
    }
    let rows: Vec<Value> = daily
        .into_iter()
        .map(|((d, a), (r, cost))| {
            json!({"date": d, "account_id": a, "input": r[0], "output": r[1], "cache_write": r[2],
                   "cache_read": r[3], "messages": r[4], "cost_usd": round4(cost)})
        })
        .collect();
    let models: Vec<Value> = by_model
        .into_iter()
        .map(|((d, m), (r, cost))| {
            json!({"date": d, "model": m, "input": r[0], "output": r[1], "cache_write": r[2],
                   "cache_read": r[3], "messages": r[4], "cost_usd": round4(cost)})
        })
        .collect();
    json!({"days": days, "rows": rows, "models": models})
}

const WINDOW_SECS: [(&str, i64); 2] = [("five_hour", 5 * 3600), ("seven_day", 7 * 86400)];

/// 一个额度窗口：(名字, 按模型统计时的模型名, 起点)
type Span = (String, Option<String>, chrono::DateTime<Utc>);

/// 每个 Claude Code 账号当前额度窗口里，本机记录到的用量和按 API 价折算的金额。与 cam.py 的 local_window_usage 一致。
///
/// usages 是 {账号 id: 刚拉到的 usage}。窗口起点 = 重置时间 - 窗口长度（按模型的周额度也是 7 天）。
/// 界面拿它和接口给的百分比对照，推算整个窗口大约能用多少。返回 {账号 id: {"at", "windows"}}。
pub fn local_window_usage(
    data: &Value,
    usages: &HashMap<String, Value>,
    now: chrono::DateTime<Utc>,
) -> Map<String, Value> {
    let mut spans: HashMap<&str, Vec<Span>> = HashMap::new();
    for (id, u) in usages {
        for (key, secs) in WINDOW_SECS {
            if let Some(r) = u
                .get(key)
                .and_then(|w| s(w, "resets_at"))
                .and_then(parse_time)
            {
                spans.entry(id).or_default().push((
                    key.to_string(),
                    None,
                    r - chrono::Duration::seconds(secs),
                ));
            }
        }
        for m in u
            .get("models")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(name), Some(r)) = (s(m, "name"), s(m, "resets_at").and_then(parse_time))
            else {
                continue;
            };
            spans.entry(id).or_default().push((
                name.to_string(),
                Some(name.to_lowercase()),
                r - chrono::Duration::days(7),
            ));
        }
    }
    let Some(since) = spans.values().flatten().map(|(_, _, start)| *start).min() else {
        return Map::new();
    };
    let owner_of = owner_finder(data);
    // (账号 id, 窗口名) -> (tokens, 金额, 消息数)
    let mut sums: HashMap<(&str, &str), (i64, f64, i64)> = HashMap::new();
    for (ts, model, u) in local_events(since) {
        let Some(owner) = owner_of(ts) else { continue };
        let Some((&id, list)) = spans.get_key_value(owner.as_str()) else {
            continue;
        };
        if ts > now {
            continue;
        }
        let cost = token_cost(&model, &u).unwrap_or(0.0);
        let tokens = u.input + u.output + u.cache_write;
        let lower = model.to_lowercase();
        for (name, family, start) in list {
            if ts < *start || family.as_ref().is_some_and(|f| !lower.contains(f.as_str())) {
                continue;
            }
            let w = sums.entry((id, name.as_str())).or_insert((0, 0.0, 0));
            w.0 += tokens;
            w.1 += cost;
            w.2 += 1;
        }
    }
    let at = if now.timestamp_subsec_micros() == 0 {
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    } else {
        now.to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
    };
    let mut out: Map<String, Value> = spans
        .keys()
        .map(|id| (id.to_string(), json!({"at": at, "windows": {}})))
        .collect();
    for ((id, name), (tokens, cost, messages)) in sums {
        out[id]["windows"][name] =
            json!({"tokens": tokens, "cost_usd": round4(cost), "messages": messages});
    }
    out
}

#[cfg(test)]
mod local_events_tests {
    use super::*;

    /// 每个测试自己的临时目录，用完删掉
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            let dir = std::env::temp_dir().join(format!(
                "cam-engine-events-{}-{}",
                std::process::id(),
                new_id()
            ));
            fs::create_dir_all(dir.join("projects")).unwrap();
            Tmp(dir)
        }
        fn root(&self) -> PathBuf {
            self.0.join("projects")
        }
        /// projects 下的一个文件（上级目录没有就建）
        fn file(&self, rel: &str) -> PathBuf {
            let p = self.root().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 一行带用量的消息，output_tokens 用来认是哪一条
    fn line(id: &str, out: i64) -> String {
        json!({
            "timestamp": "2026-10-20T12:00:00Z", "requestId": format!("req-{id}"), "cwd": "/home/me/proj",
            "message": {"id": id, "model": "claude-fable-5", "usage": {"input_tokens": 1, "output_tokens": out}},
        })
        .to_string()
            + "\n"
    }
    fn since() -> chrono::DateTime<Utc> {
        parse_time("2020-01-01T00:00:00Z").unwrap()
    }
    /// 读到的每条消息的 output_tokens，排好序
    fn outputs(root: &Path, cache: &mut LocalCache) -> Vec<i64> {
        let mut v: Vec<i64> = local_events_in(root, cache, since())
            .iter()
            .map(|(_, _, t)| t.output)
            .collect();
        v.sort_unstable();
        v
    }
    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = fs::File::options().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }
    fn set_mtime(path: &Path, t: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn unchanged_file_is_not_read_again() {
        let tmp = Tmp::new();
        let f = tmp.file("proj/a.jsonl");
        fs::write(&f, line("m1", 11) + &line("m2", 12)).unwrap();
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12]);
        // 内容换了但大小和修改时间都没变：当没变，用的是上次读到的
        let before = mtime(&f);
        fs::write(&f, line("m1", 21) + &line("m2", 22)).unwrap();
        set_mtime(&f, before);
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12]);
        // 修改时间变了就接着读（没有新的行），不会把已有的再数一遍
        set_mtime(&f, before + Duration::from_secs(1));
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12]);
    }

    #[test]
    fn appended_lines_are_read_from_where_it_stopped() {
        let tmp = Tmp::new();
        let f = tmp.file("proj/a.jsonl");
        let head = line("m1", 11) + &line("m2", 12);
        fs::write(&f, &head).unwrap();
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12]);
        assert_eq!(cache[&f].pos, head.len() as u64);
        // 前面那段换成等长的乱码再追加一行：要是从头读，前两条就没了
        let third = line("m3", 13);
        fs::write(&f, "x".repeat(head.len()) + &third).unwrap();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12, 13]);
        assert_eq!(cache[&f].pos, (head.len() + third.len()) as u64);
    }

    #[test]
    fn unfinished_last_line_waits_for_its_newline() {
        let tmp = Tmp::new();
        let f = tmp.file("proj/a.jsonl");
        let (first, second) = (line("m1", 11), line("m2", 12));
        let (half, rest) = second.split_at(second.len() / 2);
        fs::write(&f, first.clone() + half).unwrap();
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11]);
        assert_eq!(cache[&f].pos, first.len() as u64);
        append(&f, rest);
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12]);
    }

    #[test]
    fn last_line_without_newline_is_read_once_the_file_has_gone_quiet() {
        let tmp = Tmp::new();
        let f = tmp.file("proj/a.jsonl");
        let text = line("m1", 11) + line("m2", 12).trim_end();
        fs::write(&f, &text).unwrap();
        set_mtime(&f, SystemTime::now() - Duration::from_secs(120));
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12]);
        assert_eq!(cache[&f].pos, text.len() as u64);
    }

    #[test]
    fn shrunk_file_is_read_from_the_start() {
        let tmp = Tmp::new();
        let f = tmp.file("proj/a.jsonl");
        fs::write(&f, line("m1", 11) + &line("m2", 12) + &line("m3", 13)).unwrap();
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12, 13]);
        // 被重写成更短的：之前读到的都不算了
        fs::write(&f, line("m9", 99)).unwrap();
        assert_eq!(outputs(&tmp.root(), &mut cache), [99]);
        assert_eq!(cache[&f].events.len(), 1);
    }

    #[test]
    fn line_with_bad_utf8_does_not_drop_the_lines_after_it() {
        let tmp = Tmp::new();
        let f = tmp.file("proj/a.jsonl");
        // 第二行的路径里有一个不是 UTF-8 的字节（文件名是别的编码）
        let mut bytes = line("m1", 11).into_bytes();
        let bad = line("m2", 12).replace("/home/me/proj", "/home/me/pr\u{1}j");
        bytes.extend(bad.bytes().map(|b| if b == 1 { 0xff } else { b }));
        bytes.extend(line("m3", 13).into_bytes());
        assert!(String::from_utf8(bytes.clone()).is_err());
        fs::write(&f, bytes).unwrap();
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12, 13]);
    }

    #[test]
    fn same_message_in_two_files_counts_once_and_removed_files_leave_the_cache() {
        let tmp = Tmp::new();
        let (a, b) = (tmp.file("proj/a.jsonl"), tmp.file("proj/sub/b.jsonl"));
        fs::write(&a, line("m1", 11) + &line("m2", 12)).unwrap();
        fs::write(&b, line("m2", 12) + &line("m3", 13)).unwrap(); // 续接的会话把 m2 又抄了一遍
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11, 12, 13]);
        assert_eq!(cache.len(), 2);
        fs::remove_file(&a).unwrap();
        assert_eq!(outputs(&tmp.root(), &mut cache), [12, 13]);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn old_files_and_old_messages_are_left_out() {
        let tmp = Tmp::new();
        let (old, new) = (tmp.file("proj/old.jsonl"), tmp.file("proj/new.jsonl"));
        fs::write(&old, line("m1", 11)).unwrap();
        set_mtime(
            &old,
            SystemTime::from(parse_time("2019-06-01T00:00:00Z").unwrap()),
        );
        let early = line("m2", 12).replace("2026-10-20T12:00:00Z", "2019-12-31T23:59:59Z");
        fs::write(&new, early + &line("m3", 13)).unwrap();
        let mut cache = LocalCache::new();
        // 文件在 since 之前就没再动过的不读；读了的文件里早于 since 的消息不给
        assert_eq!(outputs(&tmp.root(), &mut cache), [13]);
        assert!(!cache.contains_key(&old));
        assert_eq!(cache[&new].events.len(), 2);
    }

    #[test]
    fn rows_that_are_not_usage_messages_are_skipped() {
        let tmp = Tmp::new();
        let f = tmp.file("proj/a.jsonl");
        let no_time = line("m4", 14).replace("\"timestamp\"", "\"when\"");
        let text = [
            "{\"type\":\"summary\",\"summary\":\"no usage here\"}\n".to_string(),
            "{\"usage\": broken json\n".to_string(),
            "\n".to_string(),
            no_time,
            line("m1", 11),
        ]
        .concat();
        fs::write(&f, &text).unwrap();
        let mut cache = LocalCache::new();
        assert_eq!(outputs(&tmp.root(), &mut cache), [11]);
        assert_eq!(cache[&f].pos, text.len() as u64);
    }

    fn names(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = walk_jsonl(root)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn walk_includes_hidden_directories_and_stops_at_the_depth_cap() {
        let tmp = Tmp::new();
        fs::write(tmp.file("proj/a.jsonl"), "").unwrap();
        fs::write(tmp.file(".hidden/b.jsonl"), "").unwrap();
        fs::write(tmp.file("proj/notes.txt"), "").unwrap();
        fs::write(tmp.file("1/2/3/4/5/6/7/8/deep.jsonl"), "").unwrap(); // root 下第 8 层目录：还在范围内
        fs::write(tmp.file("1/2/3/4/5/6/7/8/9/deeper.jsonl"), "").unwrap();
        assert_eq!(names(&tmp.root()), ["a.jsonl", "b.jsonl", "deep.jsonl"]);
        assert!(walk_jsonl(&tmp.0.join("missing")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn walk_follows_symlinks_without_going_round_in_circles() {
        use std::os::unix::fs::symlink;
        let tmp = Tmp::new();
        let outside = tmp.0.join("other-disk");
        fs::create_dir_all(outside.join("sub")).unwrap();
        fs::write(tmp.file("proj/a.jsonl"), "").unwrap();
        fs::write(outside.join("c.jsonl"), "").unwrap();
        fs::write(outside.join("sub/d.jsonl"), "").unwrap();
        fs::write(tmp.0.join("e.jsonl"), "").unwrap();
        symlink(&outside, tmp.root().join("linked")).unwrap(); // 目录链接到 root 外面
        symlink(tmp.0.join("e.jsonl"), tmp.root().join("proj/e.jsonl")).unwrap(); // 文件链接
        symlink(tmp.root(), tmp.root().join("proj/back")).unwrap(); // 指回 root 自己
        symlink(&outside, outside.join("sub/again")).unwrap(); // 外面的目录里绕回自己
        symlink(outside.join("sub"), tmp.root().join("linked-sub")).unwrap(); // 已经进过的目录再链接一次
        symlink(&tmp.0, outside.join("up")).unwrap(); // 外面绕到 root 的上级：root 里的文件不再数一遍
        symlink(tmp.0.join("nowhere"), tmp.root().join("broken")).unwrap(); // 断掉的链接
        symlink(tmp.0.join("nowhere.jsonl"), tmp.root().join("gone.jsonl")).unwrap();
        // tmp.0 下的 e.jsonl 经过 up 这个链接也走得到，和文件链接各算一次（同一条消息按 id 只算一次）
        assert_eq!(
            names(&tmp.root()),
            ["a.jsonl", "c.jsonl", "d.jsonl", "e.jsonl", "e.jsonl"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn walk_reads_a_root_that_is_itself_a_symlink() {
        use std::os::unix::fs::symlink;
        let tmp = Tmp::new();
        fs::write(tmp.file("proj/a.jsonl"), "").unwrap();
        let link = tmp.0.join("projects-link");
        symlink(tmp.root(), &link).unwrap();
        assert_eq!(names(&link), ["a.jsonl"]);
    }
}
