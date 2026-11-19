//! 管理面板“本机 Claude Code 消耗”的汇总：按小时 / 天 / 周 / 月的柱子、按账号和模型的明细、星期 × 钟点的热力图。
//! 与 cam.py 的 usage_report / local_report 一致（tests/shared/local_usage.json）。

use super::usage::{local_events_with_sessions, owner_finder, round4, token_cost, Tokens};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// 时间范围：今天、7 天、30 天、全部
pub const LOCAL_RANGES: [&str; 4] = ["today", "7", "30", "all"];
/// 热力图看最近几天：4 周，每个星期几正好 4 天
pub const HEAT_DAYS: i64 = 28;
/// “全部”最多画多少根柱子：按天超过了就按周，按周还超过就按月
pub const MAX_BARS: i64 = 60;

/// 一条已经去过重的消息：本地时间、模型、用量、会话 id、当时登录的账号（None = 未归属）
pub struct ReportEvent {
    at: NaiveDateTime,
    model: Arc<str>,
    tokens: Tokens,
    session: Arc<str>,
    owner: Option<String>,
}

/// 测试数据用：从 usage 的 JSON 拼一条消息
pub fn report_event(
    at: NaiveDateTime,
    model: &str,
    usage: &Value,
    session: &str,
    owner: Option<&str>,
) -> ReportEvent {
    ReportEvent {
        at,
        model: Arc::from(model),
        tokens: Tokens::of(usage),
        session: Arc::from(session),
        owner: owner.map(String::from),
    }
}

/// [input, output, cache_write, cache_read, messages] 和金额
#[derive(Default, Clone, Copy)]
struct Rec([i64; 5], f64);

impl Rec {
    fn add(&mut self, t: &Tokens, cost: f64) {
        self.0[0] += t.input;
        self.0[1] += t.output;
        self.0[2] += t.cache_write;
        self.0[3] += t.cache_read;
        self.0[4] += 1;
        self.1 += cost;
    }
    fn json(&self) -> serde_json::Map<String, Value> {
        let v = json!({"input": self.0[0], "output": self.0[1], "cache_write": self.0[2],
                       "cache_read": self.0[3], "messages": self.0[4], "cost_usd": round4(self.1)});
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }
}

fn with(mut base: serde_json::Map<String, Value>, rec: &Rec) -> Value {
    base.extend(rec.json());
    Value::Object(base)
}

fn monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

/// 柱子的单位
#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Hour,
    Day,
    Week,
    Month,
}

impl Unit {
    fn name(self) -> &'static str {
        match self {
            Unit::Hour => "hour",
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
        }
    }
    fn key(self, t: &NaiveDateTime) -> String {
        match self {
            Unit::Hour => t.format("%Y-%m-%dT%H").to_string(),
            Unit::Day => t.date().format("%Y-%m-%d").to_string(),
            Unit::Week => monday(t.date()).format("%Y-%m-%d").to_string(),
            Unit::Month => t.format("%Y-%m").to_string(),
        }
    }
}

/// 柱子怎么分：(单位, 第一根柱子的起点, 所有柱子的键)。与 cam.py 的 _report_buckets 一致。
fn buckets(
    rng: &str,
    today: NaiveDate,
    first_day: Option<NaiveDate>,
) -> (Unit, NaiveDate, Vec<String>) {
    if rng == "today" {
        let keys = (0..24)
            .map(|h| format!("{}T{:02}", today.format("%Y-%m-%d"), h))
            .collect();
        return (Unit::Hour, today, keys);
    }
    let (mut start, unit) = match rng {
        "7" => (today - Duration::days(6), Unit::Day),
        "30" => (today - Duration::days(29), Unit::Day),
        _ => {
            let start = first_day.unwrap_or(today).min(today);
            let unit = if (today - start).num_days() < MAX_BARS {
                Unit::Day
            } else if (monday(today) - monday(start)).num_days() / 7 < MAX_BARS {
                Unit::Week
            } else {
                Unit::Month
            };
            (start, unit)
        }
    };
    let mut keys = Vec::new();
    match unit {
        Unit::Day | Unit::Hour => {
            let mut d = start;
            while d <= today {
                keys.push(d.format("%Y-%m-%d").to_string());
                d += Duration::days(1);
            }
        }
        Unit::Week => {
            start = monday(start);
            let mut d = start;
            while d <= monday(today) {
                keys.push(d.format("%Y-%m-%d").to_string());
                d += Duration::days(7);
            }
        }
        Unit::Month => {
            start = start.with_day(1).unwrap_or(start);
            let (mut y, mut m) = (start.year(), start.month());
            while (y, m) <= (today.year(), today.month()) {
                keys.push(format!("{:04}-{:02}", y, m));
                (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            }
        }
    }
    (unit, start, keys)
}

/// 次数最多的那个；一样多时取名字排在前面的
fn most(counts: &BTreeMap<String, i64>) -> Option<&str> {
    let mut best: Option<(&str, i64)> = None;
    for (k, &n) in counts {
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((k, n));
        }
    }
    best.map(|(k, _)| k)
}

/// 纯计算：面板要的柱子、合计、明细、热力图。range 不认识时返回 None。与 cam.py 的 usage_report 一致。
pub fn usage_report(events: &[ReportEvent], rng: &str, now_local: NaiveDateTime) -> Option<Value> {
    if !LOCAL_RANGES.contains(&rng) {
        return None;
    }
    let today = now_local.date();
    let first = if rng == "all" {
        events.iter().map(|e| e.at.date()).min()
    } else {
        None
    };
    let (unit, start, keys) = buckets(rng, today, first);
    let want: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let heat_start = today - Duration::days(HEAT_DAYS - 1);
    let mut heat = [[0i64; 24]; 7];
    let mut series: BTreeMap<(String, String), Rec> = BTreeMap::new();
    let mut mseries: BTreeMap<(String, String), Rec> = BTreeMap::new();
    let mut accs: BTreeMap<String, Rec> = BTreeMap::new();
    let mut models: BTreeMap<String, Rec> = BTreeMap::new();
    let mut total = Rec::default();
    let mut sessions: BTreeSet<&str> = BTreeSet::new();
    let mut acc_sess: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    let mut model_sess: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    let mut acc_models: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut model_accs: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    for e in events {
        let d = e.at.date();
        if heat_start <= d && d <= today {
            heat[e.at.weekday().num_days_from_monday() as usize][e.at.hour() as usize] += 1;
        }
        let k = unit.key(&e.at);
        if !want.contains(k.as_str()) {
            continue;
        }
        let cost = token_cost(&e.model, &e.tokens).unwrap_or(0.0);
        let aid = e.owner.clone().unwrap_or_else(|| "unattributed".into());
        let m = if e.model.is_empty() {
            "unknown".to_string()
        } else {
            e.model.to_string()
        };
        series
            .entry((k.clone(), aid.clone()))
            .or_default()
            .add(&e.tokens, cost);
        mseries
            .entry((k, m.clone()))
            .or_default()
            .add(&e.tokens, cost);
        accs.entry(aid.clone()).or_default().add(&e.tokens, cost);
        models.entry(m.clone()).or_default().add(&e.tokens, cost);
        total.add(&e.tokens, cost);
        sessions.insert(&e.session);
        acc_sess.entry(aid.clone()).or_default().insert(&e.session);
        model_sess.entry(m.clone()).or_default().insert(&e.session);
        *acc_models
            .entry(aid.clone())
            .or_default()
            .entry(m.clone())
            .or_default() += 1;
        *model_accs.entry(m).or_default().entry(aid).or_default() += 1;
    }
    let obj = |v: Value| match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    let read = total.0[0] + total.0[2] + total.0[3];
    let cache_hit = if read > 0 {
        json!(round4(total.0[3] as f64 / read as f64))
    } else {
        Value::Null
    };
    let mut totals = total.json();
    totals.insert("sessions".into(), json!(sessions.len()));
    totals.insert("cache_hit".into(), cache_hit);
    Some(json!({
        "range": rng,
        "unit": unit.name(),
        "start": start.format("%Y-%m-%d").to_string(),
        "buckets": keys,
        "series": series.iter().map(|((k, a), r)| with(obj(json!({"bucket": k, "account_id": a})), r)).collect::<Vec<_>>(),
        "model_series": mseries.iter().map(|((k, m), r)| with(obj(json!({"bucket": k, "model": m})), r)).collect::<Vec<_>>(),
        "totals": totals,
        "accounts": accs.iter().map(|(a, r)| {
            let mut v = with(obj(json!({"account_id": a})), r);
            v["sessions"] = json!(acc_sess[a].len());
            v["top_model"] = json!(most(&acc_models[a]));
            v
        }).collect::<Vec<_>>(),
        "model_totals": models.iter().map(|(m, r)| {
            let mut v = with(obj(json!({"model": m})), r);
            v["sessions"] = json!(model_sess[m].len());
            v["top_account"] = json!(most(&model_accs[m]));
            v
        }).collect::<Vec<_>>(),
        "heatmap": {"metric": "messages", "days": HEAT_DAYS, "start": heat_start.format("%Y-%m-%d").to_string(),
                    "cells": heat.iter().map(|r| r.to_vec()).collect::<Vec<_>>()},
    }))
}

/// 读本机记录、按切换记录归到账号，交给 usage_report。“全部”读能找到的所有记录（Claude Code 默认只留 30 天）。
/// 与 cam.py 的 local_report 一致。
pub fn local_report(data: &Value, rng: &str) -> Result<Value, String> {
    if !LOCAL_RANGES.contains(&rng) {
        return Err("时间范围只能是 today、7、30 或 all".into());
    }
    let now = Utc::now();
    let since = if rng == "all" {
        chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap_or(now)
    } else {
        now - Duration::days(HEAT_DAYS.max(30) + 2)
    };
    let owner_of = owner_finder(data);
    let events: Vec<ReportEvent> = local_events_with_sessions(since)
        .into_iter()
        .map(|(ts, model, tokens, session)| ReportEvent {
            at: ts.with_timezone(&chrono::Local).naive_local(),
            model,
            tokens,
            session,
            owner: owner_of(ts),
        })
        .collect();
    usage_report(
        &events,
        rng,
        now.with_timezone(&chrono::Local).naive_local(),
    )
    .ok_or_else(|| "时间范围只能是 today、7、30 或 all".into())
}
