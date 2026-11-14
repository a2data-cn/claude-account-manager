//! 跑 tests/shared 里的数据。scripts/test_cam.py 的 SharedFixtureTest 跑的是同一份，两边规则必须一致。

use cam_engine::alerts::due_alerts;
use cam_engine::api::{account_health, apply_manual, days_left_at};
use cam_engine::claude::{identify, identity_agrees};
use cam_engine::desktop_app::{login_state, version_from_path};
use cam_engine::health::{
    auth_item, der_issuer, net_fingerprint_of, parse_status_summary, proxy_endpoints,
    proxy_vpn_items, region_check_fresh, region_fail_item, region_retry_wait, risk_items,
    risk_score, status_relevant, status_risk, tls_issuer_item,
};
use cam_engine::health::{parse_windows_env, parse_windows_processes};
use cam_engine::rules::{match_time_rule, validate};
use cam_engine::store::{accounts, find_index, parse_day, s};
use cam_engine::usage::note_usage_day;
use cam_engine::usage::{message_cost, parse_usage_body, roll_renewals_at};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/shared")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 {} 失败：{}", path.display(), e));
    serde_json::from_str(&text).unwrap()
}

fn cases<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    v[key].as_array().unwrap()
}

#[test]
fn alert_cases() {
    let fx = fixture("alerts.json");
    for c in cases(&fx, "cases") {
        let mut st = c["st"].clone();
        let today =
            chrono::NaiveDate::parse_from_str(c["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        let out = due_alerts(&c["data"], &mut st, c["now"].as_i64().unwrap(), today);
        let titles: Vec<&str> = out.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(json!(titles), c["expect_titles"], "{}", c["name"]);
        let bodies: Vec<Value> = out
            .iter()
            .zip(c["expect_bodies"].as_array().unwrap())
            .map(|((_, b), e)| if e.is_null() { Value::Null } else { json!(b) })
            .collect();
        assert_eq!(json!(bodies), c["expect_bodies"], "{}", c["name"]);
        assert_eq!(st, c["expect_st"], "{}", c["name"]);
    }
}

#[test]
fn usage_day_cases() {
    let fx = fixture("usage_days.json");
    for c in cases(&fx, "cases") {
        let mut a = c["account"].clone();
        let today =
            chrono::NaiveDate::parse_from_str(c["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        note_usage_day(&mut a, today);
        assert_eq!(
            a.get("usage_days").cloned().unwrap_or(Value::Null),
            c["expect"],
            "{}",
            c["name"]
        );
    }
}

#[test]
fn windows_parsing_cases() {
    let fx = fixture("windows_env.json");
    for c in cases(&fx, "env") {
        assert_eq!(
            parse_windows_env(c["output"].as_str().unwrap()),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "processes") {
        let got: Vec<Value> = parse_windows_processes(c["output"].as_str().unwrap())
            .into_iter()
            .map(|(pid, exe, name, cmd)| json!([pid, exe, name, cmd]))
            .collect();
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
}

#[test]
fn desktop_status_cases() {
    let fx = fixture("desktop_status.json");
    for c in cases(&fx, "version_from_path") {
        let got = version_from_path(c["path"].as_str().unwrap());
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "login_state") {
        let cookies: Option<Vec<i64>> = c["cookies"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_i64().unwrap()).collect());
        let got = login_state(
            c["dir_exists"].as_bool().unwrap(),
            c["token_cache"].as_bool().unwrap(),
            c["known_account"].as_bool().unwrap(),
            cookies.as_deref(),
            c["cookie_files"].as_bool().unwrap(),
            c["now"].as_i64().unwrap(),
        );
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
}

#[test]
fn status_page_cases() {
    let fx = fixture("status_page.json");
    for c in cases(&fx, "relevant") {
        let got = status_relevant(c["name"].as_str().unwrap_or(""));
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "parse") {
        assert_eq!(
            parse_status_summary(&c["summary"]).unwrap_or(Value::Null),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "risk") {
        let got = status_risk(&c["status_page"])
            .map(|(level, text)| json!([level, text]))
            .unwrap_or(Value::Null);
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
}

#[test]
fn service_cases() {
    let fx = fixture("service.json");
    let text = |v: &Value| v.as_str().unwrap().to_string();
    for c in cases(&fx, "proxy_endpoints") {
        let entries: Vec<(String, String)> = c["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (text(&e[0]), text(&e[1])))
            .collect();
        let got: Vec<Value> = proxy_endpoints(&entries)
            .into_iter()
            .map(|(src, host, port)| json!([src, host, port]))
            .collect();
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "auth_item") {
        let results: Vec<(String, u16, String, u128)> = c["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    text(&r[0]),
                    r[1].as_u64().unwrap() as u16,
                    text(&r[2]),
                    r[3].as_u64().unwrap_or(0) as u128,
                )
            })
            .collect();
        let it = auth_item(&results);
        assert_eq!(
            json!({"level": it["level"], "text": it["text"]}),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "der_issuer") {
        let hex = c["der_hex"].as_str().unwrap();
        let der: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(
            der_issuer(&der).unwrap_or(Value::Null),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "tls_issuer_item") {
        let it = tls_issuer_item(
            c["host"].as_str().unwrap(),
            &c["issuer"],
            c["verified"].as_bool().unwrap(),
        );
        assert_eq!(
            json!({"kind": it["kind"], "level": it["level"], "text": it["text"]}),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "risk_score") {
        let items: Vec<Value> = c["items"].as_array().unwrap().clone();
        assert_eq!(risk_score(&items)["score"], c["expect"], "{}", c["name"]);
    }
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
        let got =
            identify(&data, &c["oauth_account"], &creds).map(|i| accounts(&data)[i]["id"].clone());
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
        let got = find_index(&data, c["key"].as_str().unwrap())
            .ok()
            .map(|i| accounts(&data)[i]["id"].clone());
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
        let at = chrono::NaiveDateTime::parse_from_str(c["at"].as_str().unwrap(), "%Y-%m-%dT%H:%M")
            .unwrap();
        let got = match_time_rule(&data, at).and_then(|r| s(&r, "id").map(|x| json!(x)));
        assert_eq!(got.unwrap_or(Value::Null), c["expect"], "{}", c["name"]);
    }
}

#[test]
fn health_cases() {
    for c in cases(&fixture("health.json"), "cases") {
        let h = account_health(
            &c["account"],
            c["has_creds"].as_bool().unwrap(),
            c["days_left"].as_i64(),
        );
        let kinds: Vec<Value> = h["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["kind"].clone())
            .collect();
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
        let today =
            chrono::NaiveDate::parse_from_str(c["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        roll_renewals_at(&mut data, today);
        assert_eq!(data["accounts"][0]["manual"], c["expect"], "{}", c["name"]);
    }
}

#[test]
fn manual_cases() {
    let fx = fixture("manual.json");
    for c in cases(&fx, "parse_day") {
        let got = parse_day(c["input"].as_str().unwrap())
            .map(|d| json!(d.format("%Y-%m-%d").to_string()));
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
        let today =
            chrono::NaiveDate::parse_from_str(c["today"].as_str().unwrap(), "%Y-%m-%d").unwrap();
        assert_eq!(
            days_left_at(&c["account"], today),
            c["expect"],
            "{}",
            c["name"]
        );
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
            Some(want) => assert!(
                (got.unwrap() - want).abs() < 1e-9,
                "{}：{:?} != {}",
                c["name"],
                got,
                want
            ),
        }
    }
}

/// 环境检测没连上之后的规则：这一次能不能跳过、隔多久重试、界面上提示哪一条。
#[test]
fn region_check_cases() {
    let fx = fixture("region_check.json");
    for c in cases(&fx, "net_fingerprint") {
        let addrs: Vec<std::net::IpAddr> = c["addrs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(
            json!(net_fingerprint_of(&addrs)),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "fresh") {
        let got = region_check_fresh(
            c["age"].as_f64(),
            c["fage"].as_f64(),
            c["max_age"].as_i64().unwrap(),
            c["moved"].as_bool().unwrap(),
        );
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "retry_wait") {
        // 没记次数（null）按一次算，和 cam.py 的 int(fail_count or 1) 一样
        let got = region_retry_wait(
            c["fail_count"].as_i64().unwrap_or(1),
            c["refresh_min"].as_i64().unwrap(),
        );
        assert_eq!(json!(got), c["expect"], "{}", c);
    }
    let ago = |secs: i64| {
        (chrono::Utc::now() - chrono::Duration::seconds(secs))
            .format("%Y-%m-%dT%H:%M:%S+00:00")
            .to_string()
    };
    for c in cases(&fx, "fail_item") {
        let r = &c["region"];
        let mut reg = json!({"loc": r["loc"], "fail_count": r["fail_count"]});
        for (key, field) in [
            ("checked_secs_ago", "checked_at"),
            ("failed_secs_ago", "failed_at"),
        ] {
            if let Some(secs) = r[key].as_i64() {
                reg[field] = json!(ago(secs));
            }
        }
        let got = region_fail_item(&reg);
        let Some(want) = c["expect"].as_object() else {
            assert_eq!(got, None, "{}", c["name"]);
            continue;
        };
        let got = got.unwrap_or_else(|| panic!("{}：应该有这一条", c["name"]));
        assert_eq!(
            (&got["kind"], &got["level"]),
            (&want["kind"], &want["level"]),
            "{}",
            c["name"]
        );
        let text = got["text"].as_str().unwrap();
        for part in want["contains"].as_array().unwrap() {
            assert!(
                text.contains(part.as_str().unwrap()),
                "{}：{} 里没有 {}",
                c["name"],
                text,
                part
            );
        }
        let mut keys: Vec<&str> = got
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["advice", "kind", "level", "text"], "{}", c["name"]);
    }
}

/// 静态配置与实测异常分开；网络出口变化不重复当成账号异地登录。
#[test]
fn environment_risk_cases() {
    let now = chrono::Utc::now();
    let raw = fixture("environment_risk.json").to_string();
    let fx: Value = serde_json::from_str(
        &raw.replace(
            "$past_day",
            &(now - chrono::Duration::hours(25)).to_rfc3339(),
        )
        .replace("$now", &now.to_rfc3339()),
    )
    .unwrap();
    for c in cases(&fx, "cases") {
        let before = c["data"].clone();
        let items = risk_items(&c["data"], true, false);
        let levels: serde_json::Map<String, Value> = items
            .iter()
            .map(|i| (i["kind"].as_str().unwrap().to_owned(), i["level"].clone()))
            .collect();
        assert_eq!(json!(levels), c["expect"]["levels"], "{}", c["name"]);
        assert_eq!(
            risk_score(&items)["score"],
            c["expect"]["score"],
            "{}",
            c["name"]
        );
        assert_eq!(c["data"], before, "{}：保留原始历史", c["name"]);
        if let Some(parts) = c["expect"]["contains"].as_object() {
            for (kind, part) in parts {
                let item = items.iter().find(|i| i["kind"] == kind.as_str()).unwrap();
                let text = format!(
                    "{}{}",
                    item["text"].as_str().unwrap(),
                    item["advice"].as_str().unwrap()
                );
                assert!(
                    text.contains(part.as_str().unwrap()),
                    "{}: {}",
                    c["name"],
                    text
                );
            }
        }
    }
    for c in cases(&fx, "proxy_display") {
        let items = proxy_vpn_items(&c["env"]);
        for field in ["text", "advice"] {
            for (kind, part) in c[format!("expect_{}", field)].as_object().unwrap() {
                let item = items.iter().find(|i| i["kind"] == kind.as_str()).unwrap();
                assert!(
                    item[field]
                        .as_str()
                        .unwrap()
                        .contains(part.as_str().unwrap()),
                    "{}",
                    c["name"]
                );
            }
        }
    }
}
