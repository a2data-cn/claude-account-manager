//! 账号增删改与统一的 API 路由。HTTP 服务器和桌面窗口都走这里，保证行为一致。

use super::browser;
use super::claude::*;
use super::rules;
use super::store::*;
use super::usage::*;
use serde_json::{json, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn mask(k: Option<&str>) -> Value {
    match k {
        None => Value::Null,
        Some(k) if k.chars().count() > 16 => {
            let head: String = k.chars().take(10).collect();
            let tail: String = k.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
            json!(format!("{}…{}", head, tail))
        }
        Some(_) => json!("…"),
    }
}

/// 手动填的到期日优先；没填时用接口返回的订阅到期时间（换算成本地日期）。与 cam.py 一致。
pub fn days_left_at(acc: &Value, today: chrono::NaiveDate) -> Value {
    let day = match acc.get("manual").and_then(|m| s(m, "expires_at")).filter(|m| !m.is_empty()) {
        Some(m) => parse_day(m),
        None => acc
            .get("auto")
            .and_then(|a| s(a, "subscription_expires_at"))
            .and_then(parse_time)
            .map(|t| t.with_timezone(&chrono::Local).date_naive()),
    };
    match day {
        Some(d) => json!((d - today).num_days()),
        None => Value::Null,
    }
}

fn days_left(acc: &Value) -> Value {
    days_left_at(acc, chrono::Local::now().date_naive())
}

const PUBLIC_FIELDS: [&str; 12] = [
    "id",
    "kind",
    "label",
    "email",
    "plan",
    "org_name",
    "rate_limit_tier",
    "manual",
    "usage",
    "created_at",
    "last_switched_at",
    "auto",
];

pub fn public_state(store: &Store, data: &Value) -> Value {
    let active = active_code_id(data);
    let accs: Vec<Value> = accounts(data)
        .iter()
        .map(|a| {
            let mut p = serde_json::Map::new();
            for k in PUBLIC_FIELDS {
                p.insert(k.into(), a.get(k).cloned().unwrap_or(Value::Null));
            }
            let id = s(a, "id").unwrap_or_default();
            p.insert("is_active".into(), json!(active.as_deref() == Some(id)));
            p.insert("days_left".into(), days_left(a));
            let plan = s(a, "plan").or_else(|| a.get("manual").and_then(|m| s(m, "plan")));
            p.insert("plan_display".into(), json!(plan));
            match s(a, "kind") {
                Some("api") => {
                    let sec = store.load_secret(id);
                    p.insert("api_key_hint".into(), mask(s(&sec, "api_key")));
                    p.insert("has_admin_key".into(), json!(s(&sec, "admin_key").is_some()));
                }
                Some("claude_web") => {
                    let sec = store.load_secret(id);
                    p.insert("has_session_key".into(), json!(s(&sec, "session_key").is_some()));
                }
                _ => {}
            }
            Value::Object(p)
        })
        .collect();
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let platform = match std::env::consts::OS {
        "macos" => "Darwin",
        "windows" => "Windows",
        "linux" => "Linux",
        o => o,
    };
    json!({
        "version": VERSION, "backend": "desktop", "host": host, "platform": platform,
        "active": data.get("active").cloned().unwrap_or(json!({})), "accounts": accs,
        "warning": data.get("live_mismatch").or_else(|| data.get("live_unverified"))
            .and_then(|m| m.get("message")).cloned().unwrap_or(Value::Null),
        "pending_switch": pending_switch_view(data),
        "region": super::health::region_view(data),
        "risks": super::health::risk_items(data, true),
        "risk_score": super::health::risk_score(&super::health::risk_items(data, true)),
        "generated_at": now_iso()
    })
}

pub fn add_account(store: &Store, b: &Value) -> R<()> {
    let kind = s(b, "kind").unwrap_or("");
    if kind != "claude_web" && kind != "api" {
        return Err("kind 只能是 claude_web 或 api（Claude Code 账号请用导入）".into());
    }
    let api_key = s(b, "api_key").map(str::trim).filter(|x| !x.is_empty());
    let admin_key = s(b, "admin_key").map(str::trim).filter(|x| !x.is_empty());
    let session_key = s(b, "session_key").map(str::trim).filter(|x| !x.is_empty());
    if kind == "api" && api_key.is_none() && admin_key.is_none() {
        return Err("API 账号至少需要 API Key 或 Admin Key".into());
    }
    let mut label: String = s(b, "label").unwrap_or("").trim().chars().take(60).collect();
    if label.is_empty() {
        label = if kind == "api" {
            "API".into()
        } else {
            "网页账号".into()
        };
    }
    let _g = store.lock()?;
    let mut data = store.load()?;
    let id = new_id();
    let manual = if kind == "claude_web" {
        json!({"app_mode": b.get("app_mode").and_then(|v| v.as_bool()).unwrap_or(false)})
    } else {
        json!({})
    };
    let mut sec = serde_json::Map::new();
    for (k, v) in [
        ("session_key", session_key),
        ("api_key", api_key),
        ("admin_key", admin_key),
    ] {
        if let Some(v) = v {
            sec.insert(k.into(), json!(v));
        }
    }
    if !sec.is_empty() {
        store.save_secret(&id, &Value::Object(sec))?;
    }
    accounts_mut(&mut data).push(json!({
        "id": id, "kind": kind, "label": label, "email": s(b, "email"),
        "plan": if kind == "api" { json!("api") } else { Value::Null },
        "manual": manual, "usage": null, "created_at": now_iso()
    }));
    store.save(&mut data)
}

const MANUAL_TEXT: [&str; 3] = ["plan", "price", "note"];
const MANUAL_BOOL: [&str; 2] = ["auto_renew", "app_mode"];

/// 把要修改的字段校验后写进 acc（名称和 manual.*），出错时返回 Err 且不改 acc。
/// 规则和 cam.py 的 apply_manual 一致（tests/shared/manual.json）：
/// 文字字段只收字符串，空字符串表示清除；开关只收 true/false；到期日只收 YYYY-MM-DD。
/// 只有到期日真的变了才清掉 renew_day，所以界面整份提交表单时不会把月底扣费日弄丢。
pub fn apply_manual(acc: &mut Value, f: &Value) -> R<()> {
    let f = f.as_object().ok_or("请求体应为 JSON 对象")?;
    let mut m = acc.get("manual").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let mut label = acc.get("label").cloned().unwrap_or(Value::Null);
    match f.get("label") {
        None | Some(Value::Null) => {}
        Some(Value::String(l)) if l.trim().is_empty() => {}
        Some(Value::String(l)) => label = json!(l.trim().chars().take(60).collect::<String>()),
        Some(_) => return Err("名称应为文字".into()),
    }
    for k in MANUAL_TEXT.iter().chain(MANUAL_BOOL.iter()).chain(["expires_at", "cycle"].iter()) {
        let Some(v) = f.get(*k) else { continue };
        let mut v = if v.as_str() == Some("") { Value::Null } else { v.clone() };
        if MANUAL_BOOL.contains(k) {
            if !v.is_null() && !v.is_boolean() {
                return Err(format!("{} 应为 true 或 false", k));
            }
        } else if !v.is_null() && !v.is_string() {
            return Err(if *k == "expires_at" {
                "到期日格式应为 YYYY-MM-DD".into()
            } else {
                format!("{} 应为文字", k)
            });
        }
        if *k == "expires_at" {
            if let Some(x) = v.as_str() {
                if parse_day(x).is_none() {
                    return Err("到期日格式应为 YYYY-MM-DD".into());
                }
                v = json!(&x[..10]);
            }
            if m.get("expires_at").unwrap_or(&Value::Null) != &v {
                m.remove("renew_day"); // 换了到期日，扣费日按新日期重新算
            }
        }
        if *k == "cycle" && !matches!(v.as_str(), None | Some("monthly") | Some("yearly")) {
            return Err("cycle 只能是 monthly 或 yearly".into());
        }
        m.insert(k.to_string(), v);
    }
    acc["label"] = label;
    acc["manual"] = Value::Object(m);
    Ok(())
}

pub fn update_manual(store: &Store, key: &str, f: &Value) -> R<()> {
    if !f.is_object() {
        return Err("请求体应为 JSON 对象".into());
    }
    let _g = store.lock()?;
    let mut data = store.load()?;
    let i = find_index(&data, key)?;
    let id = s(&accounts(&data)[i], "id").unwrap_or_default().to_string();
    apply_manual(&mut accounts_mut(&mut data)[i], f)?;
    let mut sec = store.load_secret(&id);
    let mut touched = false;
    for k in ["session_key", "api_key", "admin_key"] {
        match f.get(k) {
            None | Some(Value::Null) => {}
            Some(Value::String(v)) if v.trim().is_empty() => {}
            Some(Value::String(v)) => {
                sec[k] = json!(v.trim());
                touched = true;
            }
            Some(_) => return Err(format!("{} 应为文字", k)),
        }
    }
    if touched {
        store.save_secret(&id, &sec)?;
    }
    roll_renewals(&mut data);
    store.save(&mut data)
}

pub fn remove_account(store: &Store, key: &str) -> R<()> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let i = find_index(&data, key)?;
    let acc = accounts(&data)[i].clone();
    let id = s(&acc, "id").unwrap_or_default().to_string();
    if active_code_id(&data).as_deref() == Some(id.as_str()) {
        return Err("这是 Claude Code 当前正在用的账号，请先切换到其它账号再删除".into());
    }
    let d = store.new_backup_dir(&format!("removed-{}", id))?;
    let mine = |r: &Value| s(r, "account") == Some(id.as_str());
    let rules: Vec<Value> = data
        .get("rules")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter(|r| mine(r)).cloned().collect())
        .unwrap_or_default();
    let mut saved = acc.clone();
    saved["rules"] = json!(rules);
    write_json_atomic(&d.join("account.json"), &saved, true)?;
    let sp = store.secret_path(&id);
    if sp.exists() {
        std::fs::rename(&sp, d.join("secret.json")).map_err(|e| e.to_string())?;
    }
    let prof = store.profiles_dir().join(&id);
    if prof.is_dir() {
        // 网页账号的浏览器 Profile（含登录 Cookie）也一起移走
        std::fs::rename(&prof, d.join("browser-profile")).map_err(|e| e.to_string())?;
    }
    accounts_mut(&mut data).remove(i);
    if let Some(a) = data.get_mut("rules").and_then(|v| v.as_array_mut()) {
        a.retain(|r| !mine(r));
    }
    store.save(&mut data)
}

/// 面板 / 桌面版的切换。有会话在跑且没指定 mode：让前端先问用户；mode=wait：登记等待；mode=now：直接切。
/// 与 cam.py 的 api_switch 一致。
pub fn api_switch(store: &Store, key: &str, mode: Option<&str>) -> R<Value> {
    let data = store.load()?;
    let target = accounts(&data)[find_index(&data, key)?].clone();
    let tid = s(&target, "id").unwrap_or_default().to_string();
    let label = s(&target, "label").unwrap_or("").to_string();
    // 只提醒，不拦：把 warn / bad 的风险点随结果带给前端
    let risks: Vec<Value> = super::health::risk_items(&data, true)
        .into_iter()
        .filter(|r| matches!(s(r, "level"), Some("warn") | Some("bad")))
        .collect();
    let procs = if active_code_id(&data).as_deref() != Some(tid.as_str()) {
        running_claude_processes()
    } else {
        Vec::new()
    };
    if !procs.is_empty() && mode != Some("now") {
        if mode != Some("wait") {
            return Ok(json!({
                "ok": false, "choice_required": true, "running_claude": procs.len(), "account": tid, "label": label,
                "risks": risks,
                "message": format!("{}。等它们结束后再切换不会打断正在跑的任务；现在就切的话它们需要重开。", running_summary(&procs))
            }));
        }
        let _g = store.lock()?;
        let mut data = store.load()?;
        defer_switch(&mut data, &tid, "manual", None);
        store.save(&mut data)?;
        return Ok(json!({
            "ok": true, "changed": false, "deferred": true, "running_claude": procs.len(), "account": tid,
            "risks": risks,
            "message": format!("有 {} 个 claude 会话在运行，等它们退出后会自动切换到 {}", procs.len(), label)
        }));
    }
    let r = switch_account(store, key)?;
    let procs = if r.changed { running_claude_processes() } else { Vec::new() };
    let message = if r.changed {
        format!(
            "已切换到 {}。{}",
            r.label,
            running_warning(&procs).unwrap_or_else(|| "正在运行的 claude 会话需要重启".into())
        )
    } else {
        "已经是当前账号".into()
    };
    Ok(json!({
        "ok": true, "changed": r.changed, "account": r.id, "running_claude": procs.len(),
        "message": message, "backup": r.backup.map(|p| p.display().to_string()), "risks": risks
    }))
}

/// 统一路由：返回 (HTTP 状态码, JSON)。
pub fn route(store: &Store, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    let parts: &[&str] = if parts.first() == Some(&"api") {
        &parts[1..]
    } else {
        &parts[..]
    };
    let res: R<Value> = (|| match (method, parts) {
        ("GET", ["state"]) => Ok(public_state(store, &store.load()?)),
        ("POST", ["refresh"]) => {
            // 来自用户点击“刷新”，允许替过期的账号续登录
            let d = refresh_usage(store, s(body, "id"), true)?;
            Ok(public_state(store, &d))
        }
        ("GET", ["local-usage"]) => {
            let days = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("days="))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(30)
                .clamp(1, 365);
            Ok(local_usage(&store.load()?, days))
        }
        ("GET", ["rules"]) => Ok(rules::rules_view(&store.load()?)),
        ("PUT", ["rules"]) => rules::save_rules(store, body),
        ("POST", ["accounts"]) => {
            if s(body, "kind") == Some("claude_code") {
                import_current(store, s(body, "label"))?;
            } else {
                add_account(store, body)?;
            }
            Ok(public_state(store, &store.load()?))
        }
        ("POST", ["accounts", id, "switch"]) => api_switch(store, id, s(body, "mode")),
        ("DELETE", ["pending-switch"]) => {
            cancel_pending_switch(store)?;
            Ok(public_state(store, &store.load()?))
        }
        ("GET", ["service-check"]) => Ok(json!({"items": super::health::service_check(store)})),
        ("POST", ["accounts", id, "open"]) => {
            let d = store.load()?;
            let i = find_index(&d, id)?;
            browser::open_web_account(store, &accounts(&d)[i])?;
            Ok(json!({"ok": true, "message": "已在电脑上打开浏览器窗口"}))
        }
        ("PATCH", ["accounts", id]) => {
            update_manual(store, id, body)?;
            Ok(public_state(store, &store.load()?))
        }
        ("DELETE", ["accounts", id]) => {
            remove_account(store, id)?;
            Ok(public_state(store, &store.load()?))
        }
        _ => Err("__404__".into()),
    })();
    match res {
        Ok(v) => (200, v),
        Err(e) if e == "__404__" => (404, json!({"error": "not found"})),
        Err(e) => (400, json!({"error": e})),
    }
}
