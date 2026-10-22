//! 跑 tests/shared 里的数据。scripts/test_cam.py 的 SharedFixtureTest 跑的是同一份，两边规则必须一致。

use cam_engine::api::{account_health, apply_manual, days_left_at};
use cam_engine::claude::{identify, identity_agrees};
use cam_engine::rules::{match_time_rule, validate};
use cam_engine::store::{accounts, find_index, parse_day, s};
use cam_engine::usage::{message_cost, parse_usage_body, roll_renewals_at};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/shared").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 {} 失败：{}", path.display(), e));
    serde_json::from_str(&text).unwrap()
}

fn cases<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    v[key].as_array().unwrap()
}

#[test]
fn identify_cases() {
    let fx = fixture("identify.json");
    let data = json!({"accounts": fx["accounts"]});
    for c in cases(&fx, "identify") {
        let creds = match c["refresh_token"].as_str() {
            Some(rt) => json!({"claudeAiOauth": {"refreshToken": rt}}),
            None => json!({}),
        };
        let got = identify(&data, &c["oauth_account"], &creds).map(|i| accounts(&data)[i]["id"].clone());
        assert_eq!(got.unwrap_or(Value::Null), c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "agrees") {
        let got = identity_agrees(&c["account"], c["uuid"].as_str(), c["email"].as_str());
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
}

#[test]
fn find_account_cases() {
    let fx = fixture("find_account.json");
    let data = json!({"accounts": fx["accounts"]});
    for c in cases(&fx, "cases") {
        let got = find_index(&data, c["key"].as_str().unwrap()).ok().map(|i| accounts(&data)[i]["id"].clone());
        assert_eq!(got.unwrap_or(Value::Null), c["expect"], "{}", c["name"]);
    }
}

#[test]
fn rule_cases() {
    let fx = fixture("rules.json");
    let mut data = json!({"accounts": fx["accounts"]});
    for c in cases(&fx, "validate") {
        let got = validate(&data, &c["input"]).ok().map(Value::Array);
        assert_eq!(got.unwrap_or(Value::Null), c["expect"], "{}", c["name"]);
    }
    data["rules"] = fx["match_rules"].clone();
    for c in cases(&fx, "match") {
        let at = chrono::NaiveDateTime::parse_from_str(c["at"].as_str().unwrap(), "%Y-%m-%dT%H:%M").unwrap();
        let got = match_time_rule(&data, at).and_then(|r| s(&r, "id").map(|x| json!(x)));
        assert_eq!(got.unwrap_or(Value::Null), c["expect"], "{}", c["name"]);
    }
}

#[test]
fn health_cases() {
    for c in cases(&fixture("health.json"), "cases") {
        let h = account_health(&c["account"], c["has_creds"].as_bool().unwrap(), c["days_left"].as_i64());
        let kinds: Vec<Value> = h["issues"].as_array().unwrap().iter().map(|i| i["kind"].clone()).collect();
        let got = json!({"level": h["level"], "label": h["label"], "kinds": kinds});
        assert_eq!(got, c["expect"], "{}", c["name"]);
        for i in h["issues"].as_array().unwrap() {
            assert_eq!(i.as_object().unwrap().len(), 5, "{}", c["name"]);
        }
    }
}

#[test]
fn renewal_cases() {
    for c in cases(&fixture("renewals.json"), "cases") {
        let mut data = json!({"accounts": [{"id": "x", "manual": c["manual"]}]});
        let today = chrono::NaiveDate::parse_from_str(c["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        roll_renewals_at(&mut data, today);
        assert_eq!(data["accounts"][0]["manual"], c["expect"], "{}", c["name"]);
    }
}

#[test]
fn manual_cases() {
    let fx = fixture("manual.json");
    for c in cases(&fx, "parse_day") {
        let got = parse_day(c["input"].as_str().unwrap()).map(|d| json!(d.format("%Y-%m-%d").to_string()));
        assert_eq!(got.unwrap_or(Value::Null), c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "apply") {
        let mut acc = fx["account"].clone();
        match apply_manual(&mut acc, &c["fields"]) {
            Ok(()) => assert_eq!(acc, c["expect"], "{}", c["name"]),
            Err(_) => {
                assert_eq!(c["expect"], Value::Null, "{}", c["name"]);
                assert_eq!(acc, fx["account"], "{}：出错时不应改动账号", c["name"]);
            }
        }
    }
    for c in cases(&fx, "days_left") {
        let today = chrono::NaiveDate::parse_from_str(c["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        assert_eq!(days_left_at(&c["account"], today), c["expect"], "{}", c["name"]);
    }
}

#[test]
fn usage_cases() {
    let fx = fixture("usage.json");
    for c in cases(&fx, "parse") {
        let got = parse_usage_body(&c["body"], c["source"].as_str().unwrap());
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "cost") {
        let got = message_cost(c["model"].as_str().unwrap(), &c["usage"]);
        match c["expect"].as_f64() {
            None => assert_eq!(got, None, "{}", c["name"]),
            Some(want) => assert!((got.unwrap() - want).abs() < 1e-9, "{}：{:?} != {}", c["name"], got, want),
        }
    }
}
