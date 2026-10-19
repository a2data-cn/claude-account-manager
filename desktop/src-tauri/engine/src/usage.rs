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

fn api_base() -> String {
    std::env::var("CAM_API_BASE").unwrap_or_else(|_| "https://api.anthropic.com".into())
}
fn web_base() -> String {
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
        .user_agent(concat!("claude-account-manager/", env!("CARGO_PKG_VERSION")))
        .try_proxy_from_env(true)
        .build()
}

/// 返回 (状态码, JSON)。网络错误时状态码为 0。
pub fn http_json(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&Value>) -> (u16, Value) {
    http_json_timeout(method, url, headers, body, 20)
}

pub fn http_json_timeout(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
    timeout_secs: u64,
) -> (u16, Value) {
    let mut req = agent(timeout_secs).request(method, url).set("Accept", "application/json");
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
            serde_json::from_str(&text)
                .unwrap_or_else(|_| json!({"error": {"message": text.chars().take(200).collect::<String>()}}))
        };
        (code, v)
    };
    match res {
        Ok(r) => parse(r),
        Err(ureq::Error::Status(_, r)) => parse(r),
        Err(e) => (0, json!({"error": {"message": format!("网络错误：{}", e)}})),
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

fn get_with(url: &str, h: &[(String, String)]) -> (u16, Value) {
    let refs: Vec<(&str, &str)> = h.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    http_json("GET", url, &refs, None)
}

/// 带 OAuth access token 请求 api.anthropic.com 的只读接口。
pub fn get_oauth(token: &str, path: &str, timeout_secs: u64) -> (u16, Value) {
    let h = oauth_headers(token);
    let refs: Vec<(&str, &str)> = h.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    http_json_timeout("GET", &format!("{}{}", api_base(), path), &refs, None, timeout_secs)
}

fn refresh_oauth(refresh_token: &str) -> R<Value> {
    let mut last = String::new();
    for url in token_urls() {
        // 在数据锁里调用，超时设短一些，别让其它操作等太久
        let (code, body) = http_json_timeout(
            "POST",
            &url,
            &[("Content-Type", "application/json")],
            Some(&json!({"grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": OAUTH_CLIENT_ID})),
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
    let saved = store.load_secret(id).get("claudeAiOauth").cloned().unwrap_or_else(|| json!({}));
    if active_code_id(data).as_deref() == Some(id) {
        let (creds, _) = read_live();
        if let Some(c) = creds {
            let ob = oauth_block(&c);
            if let (Some(t), Some(i)) = (s(&ob, "accessToken"), identify(data, &read_oauth_account(), &c)) {
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
    Ok(if exp > now_ms() + 120_000 { Some((t, saved)) } else { None })
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
    let mut secret = store.load_secret(id);
    let mut ob = secret.get("claudeAiOauth").cloned().unwrap_or_else(|| json!({}));
    let rt = s(&ob, "refreshToken")
        .ok_or("登录已过期且没有 refresh token")?
        .to_string();
    let tok = refresh_oauth(&rt)?;
    ob["accessToken"] = tok["access_token"].clone();
    if let Some(r) = s(&tok, "refresh_token") {
        ob["refreshToken"] = json!(r);
    }
    let secs = tok.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(28800);
    ob["expiresAt"] = json!(now_ms() + secs * 1000);
    secret["claudeAiOauth"] = ob.clone();
    if secret.get("raw").map(|r| r.is_object()).unwrap_or(false) {
        secret["raw"]["claudeAiOauth"] = ob.clone();
    }
    secret["saved_at"] = json!(now_iso());
    store.save_secret(id, &secret)?;
    if let Some(r) = s(&ob, "refreshToken") {
        let fp = token_fp(r);
        if let Some(a) = accounts_mut(&mut data).iter_mut().find(|a| s(a, "id") == Some(id)) {
            a["token_fp"] = json!(fp);
        }
        store.save(&mut data)?;
    }
    Ok((s(&ob, "accessToken").unwrap_or_default().to_string(), ob))
}

fn pick_window(body: &Value, key: &str) -> Value {
    match body.get(key) {
        Some(w) if w.is_object() => match w.get("utilization").and_then(|u| u.as_f64()) {
            Some(u) => json!({"utilization": u, "resets_at": w.get("resets_at").cloned().unwrap_or(Value::Null)}),
            None => Value::Null,
        },
        _ => Value::Null,
    }
}

const WINDOWS: [&str; 4] = ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"];

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
    let mut usage = json!({"source": "oauth"});
    for k in WINDOWS {
        usage[k] = pick_window(&body, k);
    }
    if let Some(extra) = body.get("extra_usage") {
        if extra.get("is_enabled").and_then(|v| v.as_bool()) == Some(true) {
            usage["extra_usage"] = json!({
                "monthly_limit": extra.get("monthly_limit"), "used_credits": extra.get("used_credits"),
                "utilization": extra.get("utilization")
            });
        }
    }
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
        for k in ["subscription_status", "subscription_expires_at", "billing_type"] {
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
    let uuid = s(&org, "uuid").ok_or("组织信息缺少 uuid")?;
    let (code, body) = get_with(&format!("{}/api/organizations/{}/usage", web_base(), uuid), &h);
    if code != 200 {
        return Err(err_msg(code, &body));
    }
    let mut usage = json!({"source": "claude.ai"});
    for k in WINDOWS {
        usage[k] = pick_window(&body, k);
    }
    Ok((usage, meta))
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
            for b in body.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
                for r in b.get("results").and_then(|d| d.as_array()).into_iter().flatten() {
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
        store.save(&mut d)?;
        d
    };
    let targets: Vec<Value> = accounts(&data)
        .iter()
        .filter(|a| only.map(|o| s(a, "id") == Some(o)).unwrap_or(true))
        .cloned()
        .collect();
    let results: Vec<(String, Value, Map<String, Value>)> = std::thread::scope(|sc| {
        let handles: Vec<_> = targets
            .iter()
            .map(|acc| {
                sc.spawn(move || {
                    let r = match s(acc, "kind") {
                        Some("claude_code") => fetch_code(store, acc, allow_token_refresh),
                        Some("claude_web") => fetch_web(store, acc),
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
                    u["fetched_at"] = json!(now_iso());
                    (s(acc, "id").unwrap_or_default().to_string(), u, meta)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
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

/// “3小时20分后重置”，与 cam.py 的 fmt_reset 一致。
pub fn fmt_reset(resets_at: &str) -> String {
    let Some(t) = parse_time(resets_at) else { return String::new() };
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 {
        return "已重置".into();
    }
    let (h, m) = (secs / 3600, secs / 60 % 60);
    if h >= 24 {
        format!("{}天{}小时后重置", h / 24, h % 24)
    } else if h > 0 {
        format!("{}小时{}分后重置", h, m)
    } else {
        format!("{}分后重置", m)
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
        let Some(mut d) = s(m, "expires_at")
            .and_then(|e| e.get(..10))
            .and_then(|e| chrono::NaiveDate::parse_from_str(e, "%Y-%m-%d").ok())
        else {
            continue;
        };
        let step = if s(m, "cycle") == Some("yearly") { 12 } else { 1 };
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
            Ok(t) if t.is_file() && p.extension().map(|x| x == "jsonl").unwrap_or(false) => out.push(p),
            _ => {}
        }
    }
}

pub fn local_usage(data: &Value, days: i64) -> Value {
    let since = Utc::now() - chrono::Duration::days(days);
    let mut log: Vec<(chrono::DateTime<Utc>, String)> = data
        .get("switch_log")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .filter_map(|e| Some((parse_time(s(e, "at")?)?, s(e, "id")?.to_string())))
        .collect();
    log.sort_by_key(|a| a.0);

    let mut files = Vec::new();
    walk_jsonl(&claude_config_dir().join("projects"), &mut files, 0);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    // (日期, 账号) -> [input, output, cache_write, cache_read, messages]
    let mut daily: std::collections::BTreeMap<(String, String), [i64; 5]> = Default::default();
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
            let Ok(ev) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(msg) = ev.get("message") else { continue };
            let Some(u) = msg.get("usage").filter(|u| u.is_object()) else {
                continue;
            };
            let Some(ts) = s(&ev, "timestamp").and_then(parse_time) else {
                continue;
            };
            if ts < since {
                continue;
            }
            let mid = s(msg, "id").unwrap_or("").to_string();
            let rid = s(&ev, "requestId").unwrap_or("").to_string();
            if !(mid.is_empty() && rid.is_empty()) && !seen.insert((mid, rid)) {
                continue;
            }
            let mut owner = "unattributed".to_string();
            for (at, id) in &log {
                if *at <= ts {
                    owner = id.clone();
                } else {
                    break;
                }
            }
            let day = ts.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string();
            let n = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            let rec = daily.entry((day, owner)).or_insert([0; 5]);
            rec[0] += n("input_tokens");
            rec[1] += n("output_tokens");
            rec[2] += n("cache_creation_input_tokens");
            rec[3] += n("cache_read_input_tokens");
            rec[4] += 1;
        }
    }
    let rows: Vec<Value> = daily
        .into_iter()
        .map(|((d, a), r)| {
            json!({"date": d, "account_id": a, "input": r[0], "output": r[1], "cache_write": r[2],
                   "cache_read": r[3], "messages": r[4]})
        })
        .collect();
    json!({"days": days, "rows": rows})
}
