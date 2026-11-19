//! 7 天额度的节奏：每次刷新用量记一个采样点（usage-samples.json，不放进 accounts.json），
//! 算窗口过去了多少、照匀速应该用到多少、按速度往后推什么时候用完。只用来显示和提醒，不会因此做任何操作。
//! 与 cam.py 的 usage_sample / note_usage_samples / usage_pace / pace_series 一致（tests/shared/pace.json）。

use super::store::*;
use serde_json::{json, Map, Value};

pub const SAMPLES_FILE: &str = "usage-samples.json";
/// 只留最近 8 天（比一个 7 天窗口多一天）
pub const SAMPLES_KEEP_SECS: i64 = 8 * 86400;
/// 每个号最多这么多个点
pub const SAMPLES_MAX: usize = 1200;
pub const SAMPLE_KINDS: [&str; 3] = ["claude_code", "claude_web", "claude_desktop"];
pub const WEEK_SECS: i64 = 7 * 86400;
/// 按最近一天的速度往后推
pub const PACE_RECENT_SECS: i64 = 24 * 3600;
/// 最近一天的点不到 3 小时：不够算速度，改按窗口开始以来的平均速度
pub const PACE_MIN_SPAN: i64 = 3 * 3600;
/// 窗口开始还不到 6 小时：平均速度也不准，不推算
pub const PACE_MIN_ELAPSED: i64 = 6 * 3600;
/// 给界面画线的点最多这么多
pub const PACE_SERIES_POINTS: usize = 240;
/// 用到这么多算用满了（和 alerts::EXHAUSTED_AT 一样）
const FULL_AT: f64 = super::alerts::EXHAUSTED_AT;

/// 百分比保留一位小数，四舍五入
fn pct1(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| if x.is_boolean() { None } else { x.as_f64() })
        .map(|p| (p * 10.0 + 0.5).floor() / 10.0)
}

fn epoch(v: Option<&Value>) -> Option<i64> {
    v.and_then(|x| x.as_str())
        .and_then(parse_time)
        .map(|t| t.timestamp())
}

fn sample_ok(p: &Value) -> bool {
    p.as_array()
        .is_some_and(|a| a.len() == 5 && a[0].as_i64().is_some() && !a[0].is_f64())
}

fn window(u: &Value, key: &str) -> Value {
    u.get(key)
        .filter(|w| w.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}))
}

/// 这个号刚取到的用量变成一个点 [取到的时间, 5 小时 %, 7 天 %, 5 小时重置, 7 天重置]（时间都是 Unix 秒）。
/// 不是有额度的号、这次有错误（数字是上一次的）、两项都没有数字时返回 None。
pub fn usage_sample(acc: &Value) -> Option<Value> {
    if !SAMPLE_KINDS.contains(&s(acc, "kind")?) {
        return None;
    }
    let u = acc.get("usage").filter(|u| u.is_object())?;
    let failed = match u.get("error") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(e)) => !e.is_empty(),
        Some(_) => true,
    };
    if failed {
        return None;
    }
    let t = epoch(u.get("fetched_at"))?;
    let (w5, w7) = (window(u, "five_hour"), window(u, "seven_day"));
    let (p5, p7) = (pct1(w5.get("utilization")), pct1(w7.get("utilization")));
    if p5.is_none() && p7.is_none() {
        return None;
    }
    Some(json!([
        t,
        p5,
        p7,
        epoch(w5.get("resets_at")),
        epoch(w7.get("resets_at"))
    ]))
}

fn same_sample(a: &Value, b: &Value) -> bool {
    (1..5).all(|i| a[i].as_f64() == b[i].as_f64())
}

/// 把各个号刚取到的用量记进采样点，返回新的 {"version": 1, "accounts": {账号 id: [点, …]}}。
/// 点的时间是用量取到的时间，不比上一个点新的不记；和上一个点一样、上一个点也和再前一个一样时只把它的时间往后挪
/// （一段没变化只留头尾两个点）；只留最近 SAMPLES_KEEP_SECS、每个号最多 SAMPLES_MAX 个点；已经删掉的号不留。
pub fn note_usage_samples(samples: &Value, accounts: &[Value], now: i64) -> Value {
    let old = samples
        .get("accounts")
        .and_then(|a| a.as_object())
        .cloned()
        .unwrap_or_default();
    let mut out = Map::new();
    for a in accounts {
        let Some(id) = s(a, "id") else { continue };
        let mut pts: Vec<Value> = old
            .get(id)
            .and_then(|v| v.as_array())
            .map(|v| v.iter().filter(|p| sample_ok(p)).cloned().collect())
            .unwrap_or_default();
        if let Some(p) = usage_sample(a) {
            let t = p[0].as_i64().unwrap_or(0);
            let newer = pts.last().is_none_or(|l| t > l[0].as_i64().unwrap_or(0));
            if t <= now + 300 && newer {
                let n = pts.len();
                if n >= 2 && same_sample(&pts[n - 1], &p) && same_sample(&pts[n - 2], &p) {
                    pts[n - 1] = p;
                } else {
                    pts.push(p);
                }
            }
        }
        pts.retain(|q| q[0].as_i64().unwrap_or(0) >= now - SAMPLES_KEEP_SECS);
        let start = pts.len().saturating_sub(SAMPLES_MAX);
        if start < pts.len() {
            out.insert(id.to_string(), Value::Array(pts.split_off(start)));
        }
    }
    json!({"version": 1, "accounts": out})
}

/// 读采样点；没有这个文件或者读不出来时是空的（不建文件）。
pub fn load_usage_samples(store: &Store) -> Value {
    match read_json(&store.root.join(SAMPLES_FILE)) {
        Ok(Some(d)) if d.get("accounts").is_some_and(|a| a.is_object()) => d,
        _ => json!({"version": 1, "accounts": {}}),
    }
}

/// 有变化才写（一个点都没有、文件也不在时不建文件）。调用方持有数据锁。
pub fn save_usage_samples(store: &Store, samples: &Value) -> R<()> {
    let path = store.root.join(SAMPLES_FILE);
    let empty = samples["accounts"].as_object().is_none_or(|a| a.is_empty());
    if *samples == load_usage_samples(store) && (!empty || !path.exists()) {
        return Ok(());
    }
    write_json_compact(&path, samples)
}

/// 刷新用量写回之后记一轮（cam.py 的 refresh_usage 里同样的一步）
pub fn record(store: &Store, data: &Value) -> R<()> {
    let next = note_usage_samples(
        &load_usage_samples(store),
        accounts(data),
        chrono::Utc::now().timestamp(),
    );
    save_usage_samples(store, &next)
}

/// 7 天额度的节奏：窗口过去了多少（even：照匀速应该用到的百分比）、比匀速快没有（ahead），按速度往后推：
/// 重置前就用完（state "out"，out_at、early_secs）还是用不完（"ok"，left 重置时大约还剩百分之几）。
/// 速度先按最近一天的采样点（basis "recent"），不够就按窗口开始以来的平均（"window"），窗口刚开始不推（state null）；
/// 用满了是 "full"。没有 7 天额度或重置时间时返回 Null。时间都是 Unix 秒。
pub fn usage_pace(points: Option<&Value>, usage: &Value, now: i64) -> Value {
    let w = window(usage, "seven_day");
    let (Some(used), Some(reset)) = (pct1(w.get("utilization")), epoch(w.get("resets_at"))) else {
        return Value::Null;
    };
    let start = reset - WEEK_SECS;
    if !(start <= now && now < reset) {
        return Value::Null;
    }
    let used = used.clamp(0.0, 100.0);
    // 这个数字是什么时候取到的：速度按它算
    let at = epoch(usage.get("fetched_at"))
        .filter(|t| *t <= now && *t >= start)
        .unwrap_or(now);
    let even = ((now - start) as f64 / WEEK_SECS as f64 * 100.0 * 10.0 + 0.5).floor() / 10.0;
    let mut out = json!({
        "start": start, "resets_at": reset, "used": used, "even": even, "ahead": used > even,
        "basis": null, "per_day": null, "state": null, "out_at": null, "early_secs": null, "left": null
    });
    if used >= FULL_AT {
        out["state"] = json!("full");
        return out;
    }
    let pts: Vec<(i64, f64)> = points
        .and_then(|p| p.as_array())
        .map(|v| {
            v.iter()
                .filter(|p| sample_ok(p))
                .filter_map(|p| {
                    let t = p[0].as_i64()?;
                    let v = if p[2].is_boolean() {
                        None
                    } else {
                        p[2].as_f64()
                    }?;
                    (start <= t && t <= at).then_some((t, v))
                })
                .collect()
        })
        .unwrap_or_default();
    let base = pts.iter().find(|(t, _)| *t >= at - PACE_RECENT_SECS);
    let rate = match base {
        Some((bt, bv)) if at - bt >= PACE_MIN_SPAN && used >= *bv => {
            out["basis"] = json!("recent");
            (used - bv) / (at - bt) as f64
        }
        _ if at - start >= PACE_MIN_ELAPSED => {
            out["basis"] = json!("window");
            used / (at - start) as f64
        }
        _ => return out,
    };
    out["per_day"] = json!((rate * 86400.0 * 10.0 + 0.5).floor() / 10.0);
    if rate > 0.0 {
        let hit = (at as f64 + (100.0 - used) / rate).floor() as i64;
        if hit < reset {
            out["state"] = json!("out");
            out["out_at"] = json!(hit);
            out["early_secs"] = json!(reset - hit);
            return out;
        }
    }
    let left = (100.0 - used - rate * (reset - at) as f64 + 0.5).floor() as i64;
    out["state"] = json!("ok");
    out["left"] = json!(left.max(0));
    out
}

/// 界面画节奏线用的点 [[时间, 7 天 %], …]：只要这个 7 天窗口里的，最多 PACE_SERIES_POINTS 个（均匀抽，留最后一个）。
pub fn pace_series(points: Option<&Value>, pace: &Value, now: i64) -> Value {
    let Some(start) = pace.get("start").and_then(|v| v.as_i64()) else {
        return json!([]);
    };
    let pts: Vec<Value> = points
        .and_then(|p| p.as_array())
        .map(|v| {
            v.iter()
                .filter(|p| sample_ok(p) && !p[2].is_null())
                .filter(|p| {
                    let t = p[0].as_i64().unwrap_or(0);
                    start <= t && t <= now
                })
                .map(|p| json!([p[0], p[2]]))
                .collect()
        })
        .unwrap_or_default();
    if pts.len() <= PACE_SERIES_POINTS {
        return Value::Array(pts);
    }
    let step = pts.len().div_ceil(PACE_SERIES_POINTS);
    let mut kept: Vec<Value> = pts.iter().step_by(step).cloned().collect();
    if !(pts.len() - 1).is_multiple_of(step) {
        kept.push(pts[pts.len() - 1].clone());
    }
    Value::Array(kept)
}

/// GET /api/usage-samples：每个有 7 天额度的号 {"pace", "points"}，给界面画节奏线。只读。
pub fn usage_samples_view(store: &Store, data: &Value, only: Option<&str>, now: i64) -> Value {
    let samples = load_usage_samples(store);
    let mut out = Map::new();
    for a in accounts(data) {
        let id = s(a, "id").unwrap_or_default();
        if !SAMPLE_KINDS.contains(&s(a, "kind").unwrap_or_default())
            || only.is_some_and(|o| o != id)
        {
            continue;
        }
        let pts = samples["accounts"].get(id);
        let pace = usage_pace(pts, &shown_usage(a), now);
        if !pace.is_null() {
            let series = pace_series(pts, &pace, now);
            out.insert(id.to_string(), json!({"pace": pace, "points": series}));
        }
    }
    json!({"now": now, "accounts": out})
}

/// 界面上显示的用量：桌面号过了重置时间的窗口不显示，旧版本误写的不显示（和 public_state 一致）
pub fn shown_usage(a: &Value) -> Value {
    if s(a, "kind") != Some("claude_desktop") {
        return a.get("usage").cloned().unwrap_or(Value::Null);
    }
    if !matches!(
        a.get("usage").and_then(|u| s(u, "source")),
        Some("desktop_cache" | "desktop_history")
    ) {
        return Value::Null;
    }
    super::desktop_usage::desktop_usage_view(&a["usage"])
}
