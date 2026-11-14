//! 后台每轮刷新用量之后的提醒：额度快满（5 小时、7 天）、用满后恢复、快到期 / 过了到期日。
//! 桌面版、cam serve、菜单栏插件同一套规则，与 cam.py 的 due_alerts 一致（tests/shared/alerts.json）。

use super::store::*;
use serde_json::{json, Map, Value};

/// 到期日前几天提醒
pub const ALERT_SOON_DAYS: i64 = 3;
/// 某个额度窗口用到这么多算用满了，过了重置时间提醒一声“恢复了”
pub const EXHAUSTED_AT: f64 = 99.5;
const WINDOWS: [(&str, &str); 2] = [("five_hour", "5 小时"), ("seven_day", "7 天")];

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(x)) => !x.is_empty(),
        Some(Value::Array(x)) => !x.is_empty(),
        Some(Value::Object(x)) => !x.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
    }
}

/// 拼进提醒记录的重置时间：和 cam.py 的 f"{resets}" 一样，没有时写 None
fn resets_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => "None".into(),
        Some(other) => other.to_string(),
    }
}

fn name_of(a: &Value) -> String {
    let nonempty = |k: &str| s(a, k).filter(|x| !x.is_empty());
    nonempty("label")
        .or_else(|| nonempty("email"))
        .or_else(|| s(a, "id"))
        .unwrap_or("")
        .to_string()
}

/// 要弹的通知 [(标题, 正文)]。st 记着提醒过的（warned）和用满了等恢复的（exhausted），调用方保存到下一轮。
/// - 当前 Claude Code 号和有额度的网页号：5 小时、7 天额度超过阈值（默认 90%）各提醒一次，每个额度窗口一次
/// - 有额度的号（Claude Code、网页、桌面版）某个窗口用满了先记下来，过了它的重置时间提醒一次“恢复了”
/// - 手动填了到期日、没勾自动续期的号：还剩 ALERT_SOON_DAYS 天以内提醒一次，过了到期日再提醒一次
pub fn due_alerts(
    data: &Value,
    st: &mut Value,
    now: i64,
    today: chrono::NaiveDate,
) -> Vec<(String, String)> {
    let limit = super::rules::alert_threshold(data) as f64;
    let act = data.get("active").and_then(|a| s(a, "claude_code"));
    let accs = accounts(data);
    let mut warned: Vec<String> = st
        .get("warned")
        .and_then(|w| w.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut exhausted: Map<String, Value> = st
        .get("exhausted")
        .and_then(|e| e.as_object())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in accs {
        let id = s(a, "id").unwrap_or_default();
        let kind = s(a, "kind").unwrap_or_default();
        let usage = a.get("usage").cloned().unwrap_or(Value::Null);
        for (key, wname) in WINDOWS {
            let w = usage.get(key).cloned().unwrap_or(Value::Null);
            let Some(pct) = w.get("utilization").and_then(|v| v.as_f64()) else {
                continue;
            };
            let resets = w.get("resets_at");
            let rt = resets_text(resets);
            if (Some(id) == act || kind == "claude_web") && pct >= limit {
                let k = if key == "five_hour" {
                    format!("{}|{}", id, rt)
                } else {
                    format!("{}|7d|{}", id, rt)
                };
                if !warned.contains(&k) {
                    warned.push(k);
                    out.push((
                        format!("{} 的 {}额度已用 {:.0}%", name_of(a), wname, pct),
                        super::usage::fmt_reset(resets.and_then(|v| v.as_str()).unwrap_or("")),
                    ));
                }
            }
            let t = resets.and_then(|v| v.as_str()).and_then(parse_time);
            if pct >= EXHAUSTED_AT
                && t.is_some_and(|t| t.timestamp() > now)
                && matches!(kind, "claude_code" | "claude_web" | "claude_desktop")
            {
                exhausted
                    .entry(format!("{}|{}|{}", id, key, rt))
                    .or_insert_with(|| {
                        json!({"id": id, "window": key, "resets_at": resets.cloned().unwrap_or(Value::Null)})
                    });
            }
        }
    }
    // 过了重置时间的：提醒恢复了，记录去掉（剩下的保持原来的顺序）
    let mut keep = Map::new();
    for (k, e) in exhausted {
        let t = s(&e, "resets_at").and_then(parse_time);
        if t.is_some_and(|t| t.timestamp() > now) {
            keep.insert(k, e);
            continue;
        }
        let acc = accs.iter().find(|a| s(a, "id") == s(&e, "id"));
        if let (Some(acc), Some(_)) = (acc, t) {
            let wname = WINDOWS
                .iter()
                .find(|(k, _)| Some(*k) == s(&e, "window"))
                .map(|(_, n)| *n)
                .unwrap_or("");
            out.push((
                format!("{} 的 {}额度恢复了", name_of(acc), wname),
                format!("{}额度已经重置，这个号可以接着用了", wname),
            ));
        }
    }
    for a in accs {
        let m = a.get("manual").cloned().unwrap_or(Value::Null);
        let Some(exp) = s(&m, "expires_at").filter(|x| !x.is_empty()) else {
            continue;
        };
        if truthy(m.get("auto_renew")) {
            continue;
        }
        let Some(n) = super::api::days_left_at(a, today).as_i64() else {
            continue;
        };
        let stage = if n < 0 {
            "over"
        } else if n <= ALERT_SOON_DAYS {
            "soon"
        } else {
            continue;
        };
        let k = format!("{}|exp|{}|{}", s(a, "id").unwrap_or_default(), exp, stage);
        if warned.contains(&k) {
            continue;
        }
        warned.push(k);
        let name = name_of(a);
        out.push(if stage == "over" {
            (
                format!("{} 已经过了到期日（{}）", name, exp),
                "续费了的话，在“编辑”里把到期日改一下".to_string(),
            )
        } else {
            (
                if n == 0 {
                    format!("{} 今天到期", name)
                } else {
                    format!("{} 还有 {} 天到期（{}）", name, n, exp)
                },
                "要接着用就记得续费；续了在“编辑”里改到期日，或者勾上“自动续期”".to_string(),
            )
        });
    }
    let start = warned.len().saturating_sub(80);
    st["warned"] = json!(warned[start..]);
    let skip = keep.len().saturating_sub(50);
    st["exhausted"] = Value::Object(keep.into_iter().skip(skip).collect());
    out
}

/// 按现在的时间和本地日期算一轮（桌面版的后台用）
pub fn due_alerts_now(data: &Value, st: &mut Value) -> Vec<(String, String)> {
    due_alerts(
        data,
        st,
        chrono::Utc::now().timestamp(),
        chrono::Local::now().date_naive(),
    )
}
