//! 跑 tests/shared 里的数据。scripts/test_cam.py 的 SharedFixtureTest 跑的是同一份，两边规则必须一致。

use cam_engine::claude::{identify, identity_agrees};
use cam_engine::rules::{match_time_rule, validate};
use cam_engine::store::{accounts, find_index, s};
use cam_engine::usage::roll_renewals_at;
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
fn renewal_cases() {
    for c in cases(&fixture("renewals.json"), "cases") {
        let mut data = json!({"accounts": [{"id": "x", "manual": c["manual"]}]});
        let today = chrono::NaiveDate::parse_from_str(c["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        roll_renewals_at(&mut data, today);
        assert_eq!(data["accounts"][0]["manual"], c["expect"], "{}", c["name"]);
    }
}
