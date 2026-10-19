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

fn days_left(acc: &Value) -> Value {
    let exp = acc
        .get("manual")
        .and_then(|m| s(m, "expires_at"))
        .or_else(|| acc.get("auto").and_then(|m| s(m, "subscription_expires_at")));
    match exp.and_then(parse_time) {
        Some(d) => {
            let today = chrono::Local::now().date_naive();
            json!((d.date_naive() - today).num_days())
        }
        None => Value::Null,
    }
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

pub fn update_manual(store: &Store, key: &str, f: &Value) -> R<()> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let i = find_index(&data, key)?;
    let id = s(&accounts(&data)[i], "id").unwrap_or_default().to_string();
    {
        let acc = &mut accounts_mut(&mut data)[i];
        if let Some(l) = s(f, "label") {
            acc["label"] = json!(l.chars().take(60).collect::<String>());
        }
        if !acc.get("manual").map(|m| m.is_object()).unwrap_or(false) {
            acc["manual"] = json!({});
        }
        let m = &mut acc["manual"];
        for k in ["plan", "expires_at", "auto_renew", "cycle", "price", "note", "app_mode"] {
            let Some(v) = f.get(k) else { continue };
            let mut v = v.clone();
            if v.as_str() == Some("") {
                v = Value::Null;
            }
            if k == "expires_at" {
                if let Some(x) = v.as_str() {
                    if parse_time(x).is_none() {
                        return Err("到期日格式应为 YYYY-MM-DD".into());
                    }
                    v = json!(x.chars().take(10).collect::<String>());
                }
            }
            if k == "cycle" {
                if let Some(x) = v.as_str() {
                    if x != "monthly" && x != "yearly" {
                        return Err("cycle 只能是 monthly 或 yearly".into());
                    }
                }
            }
            m[k] = v;
        }
    }
    let mut sec = store.load_secret(&id);
    let mut touched = false;
    for k in ["session_key", "api_key", "admin_key"] {
        if let Some(v) = s(f, k).map(str::trim).filter(|x| !x.is_empty()) {
            sec[k] = json!(v);
            touched = true;
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
    write_json_atomic(&d.join("account.json"), &acc, true)?;
    let sp = store.secret_path(&id);
    if sp.exists() {
        std::fs::rename(&sp, d.join("secret.json")).map_err(|e| e.to_string())?;
    }
    accounts_mut(&mut data).remove(i);
    store.save(&mut data)
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
            let d = refresh_usage(store, s(body, "id"))?;
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
        ("POST", ["accounts", id, "switch"]) => {
            let r = switch_account(store, id)?;
            Ok(json!({
                "ok": true, "changed": r.changed, "account": r.id,
                "message": if r.changed { format!("已切换到 {}，正在运行的 claude 会话需要重启", r.label) } else { "已经是当前账号".into() },
                "backup": r.backup.map(|p| p.display().to_string())
            }))
        }
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
