//! 切号规则：按时间段自动切换、额度提醒阈值。与 cam.py 的同名函数逻辑一致。
//! 目录规则只在脚本版 `cam claude` 里生效，这里只负责保存。

use super::claude::{switch_account, SwitchResult};
use super::store::*;
use chrono::{Datelike, Timelike};
use serde_json::{json, Value};

pub const DEFAULT_ALERT: i64 = 90;

fn parse_hhmm(v: &Value) -> R<u32> {
    let txt = v.as_str().unwrap_or("");
    let err = || format!("时间格式应为 HH:MM：{}", txt);
    let (h, m) = txt.split_once(':').ok_or_else(err)?;
    let h: u32 = h.trim().parse().map_err(|_| err())?;
    let m: u32 = m.trim().parse().map_err(|_| err())?;
    if h <= 24 && m < 60 && h * 60 + m <= 1440 {
        Ok(h * 60 + m)
    } else {
        Err(err())
    }
}

fn parse_days(v: &Value) -> R<Vec<u32>> {
    let mut days: Vec<u32> = Vec::new();
    let bad = || "星期需在 1–7 之间，1 是周一".to_string();
    match v {
        Value::Array(a) => {
            for x in a {
                days.push(x.as_u64().ok_or_else(bad)? as u32);
            }
        }
        Value::Null => days.extend(1..=7),
        other => {
            let txt = other.as_str().map(String::from).unwrap_or_else(|| other.to_string());
            for part in txt.replace('，', ",").split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                if let Some((a, b)) = part.split_once('-') {
                    let a: u32 = a.trim().parse().map_err(|_| bad())?;
                    let b: u32 = b.trim().parse().map_err(|_| bad())?;
                    days.extend(a..=b);
                } else {
                    days.push(part.parse().map_err(|_| bad())?);
                }
            }
        }
    }
    days.sort_unstable();
    days.dedup();
    if days.is_empty() || days.iter().any(|d| *d < 1 || *d > 7) {
        return Err(bad());
    }
    Ok(days)
}

fn validate(data: &Value, rules: &Value) -> R<Vec<Value>> {
    let ids: Vec<&str> = accounts(data)
        .iter()
        .filter(|a| s(a, "kind") == Some("claude_code"))
        .filter_map(|a| s(a, "id"))
        .collect();
    let mut out = Vec::new();
    for r in rules.as_array().into_iter().flatten() {
        let acc = s(r, "account")
            .filter(|a| ids.contains(a))
            .ok_or("规则里的账号不存在或不是 Claude Code 账号")?;
        let id = s(r, "id").map(String::from).unwrap_or_else(new_id);
        match s(r, "type") {
            Some("dir") => {
                let path = s(r, "path")
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or("目录规则需要填写目录")?;
                out.push(json!({"id": id, "type": "dir", "path": path, "account": acc}));
            }
            Some("time") => {
                let st = parse_hhmm(r.get("start").unwrap_or(&Value::Null))?;
                let en = parse_hhmm(r.get("end").unwrap_or(&Value::Null))?;
                if st == en {
                    return Err("开始和结束时间不能相同".into());
                }
                let days = parse_days(r.get("days").unwrap_or(&Value::Null))?;
                out.push(json!({
                    "id": id, "type": "time", "days": days,
                    "start": format!("{:02}:{:02}", st / 60, st % 60),
                    "end": format!("{:02}:{:02}", en / 60, en % 60),
                    "account": acc
                }));
            }
            _ => return Err("规则类型只能是 dir 或 time".into()),
        }
    }
    Ok(out)
}

pub fn alert_threshold(data: &Value) -> i64 {
    data.get("prefs")
        .and_then(|p| p.get("alert_threshold"))
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_ALERT)
}

pub fn rules_view(data: &Value) -> Value {
    json!({"rules": data.get("rules").cloned().unwrap_or(json!([])), "alert_threshold": alert_threshold(data)})
}

pub fn save_rules(store: &Store, body: &Value) -> R<Value> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    if let Some(r) = body.get("rules") {
        let v = validate(&data, r)?;
        data["rules"] = Value::Array(v);
        if !data.get("rule_state").map(|x| x.is_object()).unwrap_or(false) {
            data["rule_state"] = json!({});
        }
        data["rule_state"]["time_rule"] = Value::Null;
    }
    if let Some(t) = body.get("alert_threshold") {
        let t = t
            .as_i64()
            .or_else(|| t.as_str().and_then(|x| x.parse().ok()))
            .filter(|t| (1..=100).contains(t));
        let t = t.ok_or("提醒阈值应为 1–100 的整数")?;
        if !data.get("prefs").map(|x| x.is_object()).unwrap_or(false) {
            data["prefs"] = json!({});
        }
        data["prefs"]["alert_threshold"] = json!(t);
    }
    store.save(&mut data)?;
    Ok(rules_view(&data))
}

pub fn match_time_rule(data: &Value, now: chrono::NaiveDateTime) -> Option<Value> {
    let wd = now.weekday().number_from_monday();
    let prev = if wd == 1 { 7 } else { wd - 1 };
    let mins = now.hour() * 60 + now.minute();
    for r in data.get("rules").and_then(|v| v.as_array()).into_iter().flatten() {
        if s(r, "type") != Some("time") {
            continue;
        }
        let (Ok(st), Ok(en)) = (
            parse_hhmm(r.get("start").unwrap_or(&Value::Null)),
            parse_hhmm(r.get("end").unwrap_or(&Value::Null)),
        ) else {
            continue;
        };
        let days: Vec<u32> = r
            .get("days")
            .and_then(|d| d.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as u32)).collect())
            .unwrap_or_default();
        let hit = if st < en {
            days.contains(&wd) && mins >= st && mins < en
        } else {
            // 跨午夜：按开始那天算星期
            (days.contains(&wd) && mins >= st) || (days.contains(&prev) && mins < en)
        };
        if hit {
            return Some(r.clone());
        }
    }
    None
}

/// 进入新的时间段时切换；时间段内手动换号不会被改回。
pub fn apply_time_rules(store: &Store) -> R<Option<SwitchResult>> {
    let (rule, active) = {
        let _g = store.lock()?;
        let mut data = store.load()?;
        let rule = match_time_rule(&data, chrono::Local::now().naive_local());
        let rid = rule.as_ref().and_then(|r| s(r, "id")).map(String::from);
        let last = data.get("rule_state").and_then(|x| s(x, "time_rule")).map(String::from);
        if last == rid {
            return Ok(None);
        }
        if !data.get("rule_state").map(|x| x.is_object()).unwrap_or(false) {
            data["rule_state"] = json!({});
        }
        data["rule_state"]["time_rule"] = json!(rid);
        store.save(&mut data)?;
        (rule, active_code_id(&data))
    };
    let Some(rule) = rule else { return Ok(None) };
    let target = s(&rule, "account").unwrap_or_default().to_string();
    if active.as_deref() == Some(target.as_str()) {
        return Ok(None);
    }
    switch_account(store, &target).map(Some)
}

/// 返回余量最多的其它 Claude Code 账号 (已用百分比, 名称)。
pub fn best_alternative(data: &Value, exclude: &str) -> Option<(f64, String)> {
    accounts(data)
        .iter()
        .filter(|a| s(a, "kind") == Some("claude_code") && s(a, "id") != Some(exclude))
        .filter(|a| {
            a.get("usage")
                .map(|u| u.get("error").map(|e| e.is_null()).unwrap_or(true))
                .unwrap_or(false)
        })
        .filter_map(|a| {
            let u = a.get("usage")?.get("five_hour")?.get("utilization")?.as_f64()?;
            Some((u, s(a, "label").or(s(a, "email")).unwrap_or("未命名").to_string()))
        })
        .min_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal))
}
