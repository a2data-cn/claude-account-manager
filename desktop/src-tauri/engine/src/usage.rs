//! 用量获取、token 刷新、自动续期、本地日志统计。

use super::claude::*;
use super::store::*;
use chrono::{Datelike, Utc};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

pub(crate) fn api_base() -> String {
    std::env::var("CAM_API_BASE").unwrap_or_else(|_| "https://api.anthropic.com".into())
}
pub(crate) fn web_base() -> String {
    std::env::var("CAM_CLAUDE_WEB_BASE").unwrap_or_else(|_| "https://claude.ai".into())
}
fn token_urls() -> Vec<String> {
    std::env::var("CAM_TOKEN_URLS")
        .unwrap_or_else(|_| {
            "https://platform.claude.com/v1/oauth/token,https://console.anthropic.com/v1/oauth/token".into()
        })
        .split(',')
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout_secs))
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
    let mut req = agent(timeout_secs)
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
        Err(ureq::Error::Status(_, r)) => parse(r),
        Err(e) => (0, json!({"error": {"message": format!("网络错误：{}", e)}})),
    }
}

/// GET 纯文本。返回 (状态, 文本)；状态 0 表示没连上。
pub fn http_text(url: &str, timeout_secs: u64) -> (u16, String) {
    match agent(timeout_secs).get(url).call() {
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
        if let Some(m) = e.get("message").and_then(|m| m.as_str()) {
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

pub(crate) fn get_with(url: &str, h: &[(String, String)]) -> (u16, Value) {
    let refs: Vec<(&str, &str)> = h.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    http_json("GET", url, &refs, None)
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
        if code == 200 && s(&body, "access_token").is_some() {
            return Ok(body);
        }
        last = err_msg(code, &body);
        if code == 400 || code == 401 {
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
        if let Some(c) = creds {
            let ob = oauth_block(&c);
            if let (Some(t), Some(i)) = (
                s(&ob, "accessToken"),
                identify(data, &read_oauth_account()?, &c),
            ) {
                if s(&accounts(data)[i], "id") == Some(id) {
                    return Ok(Some((t.to_string(), ob.clone())));
                }
            }
        }
        // 当前账号从不在这里刷新，交给 Claude Code
        let t = s(&saved, "accessToken").ok_or("没有保存凭据")?.to_string();
        return Ok(Some((t, saved)));
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
pub fn ensure_fresh_token(store: &Store, id: &str, allow_refresh: bool) -> R<(String, Value)> {
    let data = store.load()?;
    find_acc(&data, id)?;
    if let Some(t) = usable_token(store, &data, id)? {
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
    if let Some(t) = usable_token(store, &data, id)? {
        return Ok(t);
    }
    let ob = renew_saved_login(store, &mut data, id)?;
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

/// 一条消息按 API 价折算多少美元；不认识的模型返回 None。与 cam.py 的 message_cost 一致。
pub fn message_cost(model: &str, u: &Value) -> Option<f64> {
    let m = model.to_lowercase();
    let m = m.find("claude-").map(|i| &m[i + 7..]).unwrap_or(&m);
    let &(key, mut inp, mut out, mut cr) = PRICES.iter().find(|r| m.starts_with(r.0))?;
    let (w5, w1) = match u
        .get("cache_creation")
        .filter(|c| c.as_object().is_some_and(|o| !o.is_empty()))
    {
        Some(cc) => (
            count(cc, "ephemeral_5m_input_tokens"),
            count(cc, "ephemeral_1h_input_tokens"),
        ),
        None => (count(u, "cache_creation_input_tokens"), 0),
    };
    // Haiku 5.5 的提示超过 10 万 token 时整条按 5 倍价
    if key == "haiku-5-5"
        && count(u, "input_tokens") + count(u, "cache_read_input_tokens") + w5 + w1 > 100_000
    {
        (inp, out, cr) = (inp * 5.0, out * 5.0, cr * 5.0);
    }
    let n = |k: &str| count(u, k) as f64;
    Some(
        (n("input_tokens") * inp
            + n("output_tokens") * out
            + n("cache_read_input_tokens") * cr
            + w5 as f64 * inp * 1.25
            + w1 as f64 * inp * 2.0)
            / 1e6,
    )
}

/// 返回 (usage, 要合并进账号的元数据)
fn fetch_code(store: &Store, acc: &Value, allow_refresh: bool) -> R<(Value, Map<String, Value>)> {
    let (token, ob) = ensure_fresh_token(store, s(acc, "id").unwrap_or_default(), allow_refresh)?;
    let h = oauth_headers(&token);
    // 先拿账号资料：既用来补邮箱、套餐，也用来核对这份凭据是不是这个账号的
    let (c2, prof) = get_with(&format!("{}/api/oauth/profile", api_base()), &h);
    if c2 == 200 {
        let a = prof.get("account").cloned().unwrap_or_default();
        if !identity_agrees(acc, s(&a, "uuid"), s(&a, "email")) {
            return Err(format!(
                "这个账号保存的登录实际属于 {}，可能被旧的 claude 会话覆盖了。请用这个账号重新登录 Claude Code 后再导入",
                s(&a, "email").or(s(&a, "uuid")).unwrap_or("其它账号")
            ));
        }
    }
    let (code, body) = get_with(&format!("{}/api/oauth/usage", api_base()), &h);
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

fn fetch_web(store: &Store, acc: &Value) -> R<(Value, Map<String, Value>)> {
    let id = s(acc, "id").unwrap_or_default();
    let sec = store.load_secret(id);
    let sk = s(&sec, "session_key").ok_or("未填写 sessionKey，只能显示手动信息")?;
    let h = vec![("Cookie".to_string(), format!("sessionKey={}", sk))];
    let (code, orgs) = get_with(&format!("{}/api/organizations", web_base()), &h);
    let org = orgs.as_array().and_then(|a| a.first()).cloned();
    let org = match (code, org) {
        (200, Some(o)) => o,
        _ => {
            return Err(format!(
                "{}（claude.ai 可能拦截了非浏览器请求，或 sessionKey 已过期）",
                err_msg(code, &orgs)
            ))
        }
    };
    let mut meta = Map::new();
    if let Some(n) = s(&org, "name") {
        meta.insert("org_name".into(), json!(n));
    }
    if let Some(caps) = org.get("capabilities").and_then(|c| c.as_array()) {
        for p in ["claude_max", "claude_pro"] {
            if caps.iter().any(|c| c.as_str() == Some(p)) {
                meta.insert("plan".into(), json!(p.trim_start_matches("claude_")));
                break;
            }
        }
    }
    if let Some(v) = s(&org, "rate_limit_tier") {
        meta.insert("rate_limit_tier".into(), json!(v));
    }
    let uuid = s(&org, "uuid").ok_or("组织信息缺少 uuid")?;
    let (code, body) = get_with(
        &format!("{}/api/organizations/{}/usage", web_base(), uuid),
        &h,
    );
    if code != 200 {
        return Err(err_msg(code, &body));
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
                    // Desktop 的 fetched_at 是 Claude 缓存/采样时间，读取动作不能把旧数据伪装成实时。
                    if s(acc, "kind") != Some("claude_desktop") {
                        u["fetched_at"] = json!(now_iso());
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
    let _g = store.lock()?;
    let mut fresh = store.load()?;
    let by_id: HashMap<String, (Value, Map<String, Value>)> =
        results.into_iter().map(|(id, u, m)| (id, (u, m))).collect();
    for a in accounts_mut(&mut fresh).iter_mut() {
        let id = s(a, "id").unwrap_or_default().to_string();
        if let Some((u, meta)) = by_id.get(&id) {
            a["usage"] = u.clone();
            for (k, v) in meta {
                a[k.as_str()] = v.clone();
            }
        }
    }
    roll_renewals(&mut fresh);
    store.save(&mut fresh)?;
    Ok(fresh)
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
        let m = &mut a["manual"];
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

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 8 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_jsonl(&p, out, depth + 1),
            Ok(t) if t.is_file() && p.extension().map(|x| x == "jsonl").unwrap_or(false) => {
                out.push(p)
            }
            _ => {}
        }
    }
}

/// 本机 Claude Code 记录里每条带用量的消息：(时间, 模型, usage)。同一条消息只算一次。与 cam.py 的 local_events 一致。
fn local_events(since: chrono::DateTime<Utc>) -> Vec<(chrono::DateTime<Utc>, String, Value)> {
    let mut files = Vec::new();
    walk_jsonl(&claude_config_dir().join("projects"), &mut files, 0);
    let mut seen: HashSet<(Option<String>, Option<String>)> = HashSet::new();
    let mut out = Vec::new();
    for f in files {
        let recent = fs::metadata(&f)
            .and_then(|m| m.modified())
            .map(|t| chrono::DateTime::<Utc>::from(t) >= since)
            .unwrap_or(true);
        if !recent {
            continue;
        }
        let Ok(fh) = fs::File::open(&f) else { continue };
        for line in BufReader::new(fh).lines().map_while(Result::ok) {
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(mut ev) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(ts) = s(&ev, "timestamp").and_then(parse_time) else {
                continue;
            };
            let Some(msg) = ev
                .get_mut("message")
                .filter(|m| m.get("usage").is_some_and(Value::is_object))
            else {
                continue;
            };
            if ts < since {
                continue;
            }
            let key = (
                s(msg, "id").map(String::from),
                s(&ev, "requestId").map(String::from),
            );
            let msg = &mut ev["message"];
            if key != (None, None) && !seen.insert(key) {
                continue;
            }
            let model = s(msg, "model").unwrap_or_default().to_string();
            out.push((ts, model, msg["usage"].take()));
        }
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
    let add = |rec: &mut Rec, u: &Value, cost: f64| {
        rec.0[0] += count(u, "input_tokens");
        rec.0[1] += count(u, "output_tokens");
        rec.0[2] += count(u, "cache_creation_input_tokens");
        rec.0[3] += count(u, "cache_read_input_tokens");
        rec.0[4] += 1;
        rec.1 += cost;
    };
    for (ts, model, u) in local_events(since) {
        let owner = owner_of(ts).unwrap_or_else(|| "unattributed".to_string());
        let day = ts
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
        let cost = message_cost(&model, &u).unwrap_or(0.0);
        let name = if model.is_empty() {
            "unknown".to_string()
        } else {
            model
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
        let cost = message_cost(&model, &u).unwrap_or(0.0);
        let tokens = count(&u, "input_tokens")
            + count(&u, "output_tokens")
            + count(&u, "cache_creation_input_tokens");
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
