//! 跑 tests/shared 里的数据。scripts/test_cam.py 的 SharedFixtureTest 跑的是同一份，两边规则必须一致。

use cam_engine::alerts::due_alerts;
use cam_engine::api::{account_health, apply_manual, days_left_at};
use cam_engine::claude::{identify, identity_agrees};
use cam_engine::desktop_app::{login_state, version_from_path};
use cam_engine::health::{
    addr_special, auth_item, der_issuer, exit_note, net_fingerprint_of, note_risk_in, parse_hosts,
    parse_status_summary, plain_http_verdict, proxy_endpoints, proxy_vpn_items, region_check_fresh,
    region_fail_item, region_retry_wait, risk_items, risk_log_view, risk_points, risk_rules,
    risk_score, status_relevant, status_risk, strip_ago, stuck_stage, tls_issuer_item,
    RISK_LOG_SHOWN,
};
use cam_engine::health::{
    parse_airport_json, parse_netsh_wlan, parse_nmcli_wifi, region_view, wifi_security_class,
};
use cam_engine::health::{parse_windows_env, parse_windows_processes};
use cam_engine::health::{progress_open, service_steps, Progress};
use cam_engine::local_report::{report_event, usage_report};
use cam_engine::mailbox::detect;
use cam_engine::pace::{note_usage_samples, pace_series, usage_pace};
use cam_engine::rules::{match_time_rule, validate};
use cam_engine::sessions::{
    context_health, context_window, project_dir_name, session_phrase, session_state,
    transcript_composition, transcript_meta,
};
use cam_engine::store::{accounts, find_index, parse_day, s};
use cam_engine::usage::note_usage_day;
use cam_engine::usage::{login_days, login_expiry_ms, renewed_login_expiry};
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
fn pace_cases() {
    let fx = fixture("pace.json");
    for c in cases(&fx, "samples") {
        let accs = c["accounts"].as_array().unwrap();
        let got = note_usage_samples(&c["samples"], accs, c["now"].as_i64().unwrap());
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "pace") {
        let got = usage_pace(Some(&c["points"]), &c["usage"], c["now"].as_i64().unwrap());
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "series") {
        let g = &c["gen"];
        let (t0, dt, n) = (
            g["t0"].as_i64().unwrap(),
            g["dt"].as_i64().unwrap(),
            g["count"].as_i64().unwrap(),
        );
        let pts: Vec<Value> = (0..n)
            .map(|i| {
                let p7 = if i == g["p7_null_at"].as_i64().unwrap() {
                    Value::Null
                } else {
                    json!(i as f64)
                };
                json!([t0 + i * dt, 1.0, p7, null, null])
            })
            .collect();
        let got = pace_series(Some(&json!(pts)), &c["pace"], c["now"].as_i64().unwrap());
        let got = got.as_array().unwrap();
        let summary = json!({"len": got.len(), "first": got.first(), "last": got.last()});
        assert_eq!(summary, c["expect"], "{}", c["name"]);
    }
}

#[test]
fn local_usage_report_cases() {
    let fx = fixture("local_usage.json");
    let at = |v: &Value| {
        chrono::NaiveDateTime::parse_from_str(v.as_str().unwrap(), "%Y-%m-%dT%H:%M:%S").unwrap()
    };
    let now = at(&fx["now"]);
    for c in cases(&fx, "cases") {
        let events: Vec<_> = fx["events"][c["events"].as_str().unwrap()]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                report_event(
                    at(&e["at"]),
                    e["model"].as_str().unwrap(),
                    &e["usage"],
                    e["session"].as_str().unwrap(),
                    e["owner"].as_str(),
                )
            })
            .collect();
        let got = usage_report(&events, c["range"].as_str().unwrap(), now).unwrap();
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
    assert!(usage_report(&[], "90", now).is_none());
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
            .map(|(pid, exe, name, cmd, started)| json!([pid, exe, name, cmd, started]))
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

/// 登录本身的有效期：认不认这个到期时间、还剩几天、续期后取哪个（和 cam.py、Claude Code 一致）
#[test]
fn login_expiry_cases() {
    let fx = fixture("login_expiry.json");
    let now = fx["now_ms"].as_i64().unwrap();
    for c in cases(&fx, "expiry") {
        assert_eq!(
            json!(login_expiry_ms(&c["ob"], now)),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "days") {
        assert_eq!(
            json!(login_days(c["exp_ms"].as_i64().unwrap(), now)),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "renew") {
        assert_eq!(
            json!(renewed_login_expiry(&c["old"], &c["tok"], now)),
            c["expect"],
            "{}",
            c["name"]
        );
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

/// 域名有没有被动过手脚：hosts 文件里指到 Claude / Anthropic 域名的条目、解析出来的地址是不是内网 / 保留地址。
#[test]
fn hosts_cases() {
    let raw = fixture("hosts.json").to_string();
    let fx: Value =
        serde_json::from_str(&raw.replace("$now", &chrono::Utc::now().to_rfc3339())).unwrap();
    for c in cases(&fx, "parse_hosts") {
        assert_eq!(
            json!(parse_hosts(c["text"].as_str().unwrap())),
            c["expect"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "addr_special") {
        assert_eq!(
            json!(addr_special(c["ip"].as_str().unwrap())),
            c["expect"],
            "{}",
            c["ip"]
        );
    }
    for c in cases(&fx, "risk") {
        let items = risk_items(&c["data"], true, false);
        let kind = c["kind"].as_str().unwrap_or("hosts");
        let got = items.iter().find(|i| i["kind"] == kind);
        if c["expect"].is_null() {
            assert!(got.is_none(), "{}", c["name"]);
            continue;
        }
        let (got, want) = (got.unwrap(), &c["expect"]);
        assert_eq!(got["level"], want["level"], "{}", c["name"]);
        assert_eq!(json!(risk_points(got)), want["points"], "{}", c["name"]);
        assert_eq!(got["text"], want["text"], "{}", c["name"]);
        assert!(
            got["advice"]
                .as_str()
                .unwrap()
                .contains(want["advice_has"].as_str().unwrap()),
            "{}",
            c["name"]
        );
        assert_eq!(risk_score(&items)["score"], want["score"], "{}", c["name"]);
    }
}

/// 检测进度（界面的进度卡）：“重新检测”有哪几步、一份进度按顺序操作后是什么样、认哪些 run。
#[test]
fn progress_cases() {
    let fx = fixture("progress.json");
    for c in cases(&fx, "steps") {
        let got: Vec<Value> = service_steps(
            c["lookup_off"].as_bool().unwrap(),
            c["has_proxy"].as_bool().unwrap(),
        )
        .into_iter()
        .map(|(k, label)| json!([k, label]))
        .collect();
        assert_eq!(
            json!(got),
            c["want"],
            "steps {} {}",
            c["lookup_off"],
            c["has_proxy"]
        );
    }
    let text = |v: &Value| v.as_str().unwrap().to_string();
    for c in cases(&fx, "machine") {
        let p = Progress::new();
        for op in c["ops"].as_array().unwrap() {
            match op[0].as_str().unwrap() {
                "add" => p.add(&text(&op[1]), &text(&op[2]), op[3].as_i64().unwrap()),
                "start" => p.start(&text(&op[1])),
                "finish" => p.finish(&text(&op[1]), op[2].as_bool().unwrap(), &text(&op[3])),
                "skip" => p.skip(&text(&op[1]), &text(&op[2])),
                "expire" => {
                    let keys: Vec<String> = op[1].as_array().unwrap().iter().map(text).collect();
                    p.expire(&keys.iter().map(String::as_str).collect::<Vec<_>>())
                }
                "close" => p.close(),
                other => panic!("没见过的操作 {other}"),
            }
        }
        let v = p.view();
        let got: Vec<Value> = v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| json!({"key": s["key"], "label": s["label"], "state": s["state"], "note": s["note"]}))
            .collect();
        assert_eq!(json!(got), c["want"], "{}", c["name"]);
        assert_eq!(v["done"], c["done"], "{}", c["name"]);
        // 做完的、没等到的步骤带用时，其它的没有
        for s in v["steps"].as_array().unwrap() {
            let timed = matches!(s["state"].as_str(), Some("ok" | "fail" | "timeout"));
            assert_eq!(s["ms"].is_u64(), timed, "{}：{}", c["name"], s);
        }
    }
    for c in cases(&fx, "runs") {
        assert_eq!(
            progress_open(c["run"].as_str()).live(),
            c["ok"].as_bool().unwrap(),
            "run {}",
            c["run"]
        );
    }
    // 没人看进度时的空壳：什么都不记，view 是空的
    let none = Progress::none();
    none.add("a", "甲", 0);
    none.start("a");
    assert_eq!(none.view(), json!({"steps": [], "done": false, "ms": 0}));
}

/// 连着的 Wi-Fi 加没加密：三个系统各自的输出怎么读、加密方式怎么归类、开放网络和 WEP 在环境里怎么提醒。
#[test]
fn wifi_cases() {
    let fx = fixture("wifi.json");
    for c in cases(&fx, "class") {
        assert_eq!(
            json!(wifi_security_class(c[0].as_str().unwrap())),
            c[1],
            "{}",
            c[0]
        );
    }
    type Parser = fn(&str) -> Option<Value>;
    let parsers: [(&str, Parser); 3] = [
        ("nmcli", parse_nmcli_wifi),
        ("netsh", parse_netsh_wlan),
        ("airport", parse_airport_json),
    ];
    for (key, parse) in parsers {
        for c in cases(&fx, key) {
            assert_eq!(
                json!(parse(c["text"].as_str().unwrap())),
                c["expect"],
                "{}：{}",
                key,
                c["name"]
            );
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    for c in cases(&fx, "risk") {
        let data = json!({"region": {"loc": "US", "checked_at": now,
                                     "env": {"proxy": [], "tunnel": null, "wifi": c["wifi"]}}});
        let items = risk_items(&data, true, false);
        let got = items.iter().find(|i| i["kind"] == "wifi");
        assert_eq!(region_view(&data)["wifi"], c["wifi"], "{}", c["name"]);
        if c["expect"].is_null() {
            assert!(got.is_none(), "{}", c["name"]);
            continue;
        }
        let (got, want) = (got.unwrap(), &c["expect"]);
        assert_eq!(got["level"], "warn", "{}", c["name"]);
        assert_eq!(got["text"], want["text"], "{}", c["name"]);
        assert_eq!(json!(risk_points(got)), want["points"], "{}", c["name"]);
        assert!(
            got["advice"]
                .as_str()
                .unwrap()
                .contains(want["advice_has"].as_str().unwrap()),
            "{}",
            c["name"]
        );
        assert_eq!(risk_score(&items)["score"], want["score"], "{}", c["name"]);
    }
}

/// 明文网页有没有被中途改动：拿到的是不是空的 204。
#[test]
fn plain_http_cases() {
    let fx = fixture("service.json");
    for c in cases(&fx, "plain_http") {
        let got = plain_http_verdict(
            c["status"].as_u64().unwrap() as u16,
            c["location"].as_str().unwrap(),
            c["body"].as_str().unwrap(),
        );
        assert_eq!(got, c["expect"], "{}", c["name"]);
    }
}

/// 连不上时卡在哪一层：走代理、解析、TCP、TLS、等回应。
#[test]
fn stuck_stage_cases() {
    let fx = fixture("service.json");
    for c in cases(&fx, "stuck_stage") {
        let got = stuck_stage(
            c["host"].as_str().unwrap(),
            c["proxied"].as_bool().unwrap(),
            c["dns_ok"].as_bool().unwrap(),
            c["tcp_why"].as_str(),
            c["tls_ok"].as_bool(),
        );
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
}

/// 给人看的扣分规则和实际扣的分是同一张表；关注项（Anthropic 那边的状况）、说明项不扣分。
#[test]
fn risk_rules_cases() {
    let fx = fixture("risk_rules.json");
    let got = risk_rules();
    let want: Vec<Value> = cases(&fx, "rules")
        .iter()
        .map(|r| json!({"points": r["points"], "text": r["text"]}))
        .collect();
    assert_eq!(got["rules"], json!(want));
    assert_eq!(got["notices"], fx["notices"]);
    for r in cases(&fx, "rules") {
        assert_eq!(json!(risk_points(r)), r["points"], "{}", r);
        assert!(r["points"].as_i64().unwrap() > 0, "{}", r);
    }
    for r in cases(&fx, "no_points") {
        assert_eq!(risk_points(r), 0, "{}", r);
    }
}

/// 环境评分：每条扣几分、分数变了记一笔、界面上的“分数变化”。
#[test]
fn risk_log_cases() {
    let raw = fixture("risk_log.json").to_string();
    let fx: Value =
        serde_json::from_str(&raw.replace("$now", &chrono::Utc::now().to_rfc3339())).unwrap();
    for c in cases(&fx, "points") {
        assert_eq!(json!(risk_points(c)), c["points"], "{}", c);
    }
    for c in cases(&fx, "strip_ago") {
        assert_eq!(json!(strip_ago(c["text"].as_str().unwrap())), c["expect"]);
    }
    for c in cases(&fx, "exit_note") {
        assert_eq!(
            json!(exit_note(&c["prev"], &c["cur"])),
            c["expect"],
            "{}",
            c
        );
    }
    let pairs = |items: &Value| -> Vec<Value> {
        items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| json!([i["kind"], i["points"]]))
            .collect()
    };
    for seq in cases(&fx, "sequences") {
        let mut data = json!({});
        for (n, step) in seq["steps"].as_array().unwrap().iter().enumerate() {
            let name = format!("{}（第 {} 步）", seq["name"], n);
            for (k, v) in step["set"].as_object().unwrap() {
                data[k] = v.clone();
            }
            if step["note"].as_bool().unwrap_or(true) {
                let at = format!("2026-01-01T00:00:{:02}+00:00", n);
                assert_eq!(
                    json!(note_risk_in(&mut data, Some(&at))),
                    step["expect_noted"],
                    "{}",
                    name
                );
            }
            let log = data["risk_log"].as_array().cloned().unwrap_or_default();
            let got: Vec<Value> = log
                .iter()
                .map(|e| {
                    let kinds: Vec<&Value> = e["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|i| &i["kind"])
                        .collect();
                    json!({"score": e["score"], "kinds": kinds})
                })
                .collect();
            assert_eq!(json!(got), step["expect_log"], "{}", name);
            let text: String = log
                .iter()
                .flat_map(|e| e["items"].as_array().unwrap().iter())
                .map(|i| i["text"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join(" ");
            for part in step["text_excludes"].as_array().into_iter().flatten() {
                assert!(!text.contains(part.as_str().unwrap()), "{}：{}", name, text);
            }
            for part in step["text_includes"].as_array().into_iter().flatten() {
                assert!(text.contains(part.as_str().unwrap()), "{}：{}", name, text);
            }
            if !step["expect_view"].is_null() {
                let v = risk_log_view(&data, RISK_LOG_SHOWN);
                let changes: Vec<Value> = v["changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| {
                        json!({"at": !c["at"].is_null(), "prev": c["prev"], "score": c["score"],
                               "grade": c["grade"], "added": pairs(&c["added"]),
                               "removed": pairs(&c["removed"]), "exit_note": c["exit_note"]})
                    })
                    .collect();
                assert_eq!(
                    json!({"since": !v["since"].is_null(), "changes": changes}),
                    step["expect_view"],
                    "{}",
                    name
                );
            }
        }
    }
}

#[test]
fn session_cases() {
    // 会话页的规则：模型的上下文窗口、上下文健不健康、记录目录名、会话状态、红绿灯的说法、记录的摘要和组成（估的）
    let fx = fixture("sessions.json");
    for c in cases(&fx, "windows") {
        let (w, exact) = context_window(c["model"].as_str());
        assert_eq!(json!([w, exact]), c["expect"], "{}", c["model"]);
    }
    for c in cases(&fx, "health") {
        let got = context_health(c["used"].as_u64(), c["window"].as_u64().unwrap());
        assert_eq!(json!(got), c["expect"], "{c}");
    }
    for c in cases(&fx, "project_dir") {
        assert_eq!(
            json!(project_dir_name(c["cwd"].as_str().unwrap())),
            c["expect"],
            "{}",
            c["cwd"]
        );
    }
    for c in cases(&fx, "states") {
        let rec = if c["rec"].is_null() {
            None
        } else {
            Some(&c["rec"])
        };
        let got = session_state(
            rec,
            c["waiting"].as_bool().unwrap(),
            c["written_ago"].as_f64(),
            c["cpu"].as_f64(),
            c["now"].as_f64().unwrap(),
        );
        assert_eq!(json!(got), c["expect"], "{}", c["name"]);
    }
    for c in cases(&fx, "phrases") {
        let got = session_phrase(
            c["n"].as_u64().unwrap() as usize,
            c["busy"].as_bool().unwrap(),
            c["answering"].as_u64().map(|x| x as usize),
            c["waiting"].as_u64().unwrap() as usize,
        );
        assert_eq!(json!(got), c["expect"], "{c}");
    }
    let dir = std::env::temp_dir().join(format!("cam-sessions-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for c in cases(&fx, "transcripts") {
        let name = c["name"].as_str().unwrap();
        let path = dir.join(format!("{}.jsonl", name.replace(' ', "_")));
        let text: String = c["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| match l {
                Value::String(raw) => raw.clone() + "\n",
                other => other.to_string() + "\n",
            })
            .collect();
        std::fs::write(&path, text).unwrap();
        let m = transcript_meta(&path).unwrap();
        let got = json!({"turns": m.turns, "first_ts": m.first_ts, "cwd": m.cwd, "model": m.model,
            "context_used": m.context_used, "cache_hit": m.cache_hit, "turns_at_boundary": m.turns_at_boundary,
            "compacted": m.boundary.is_some()});
        assert_eq!(got, c["meta"], "{name}");
        let comp = transcript_composition(&path, &m, c["top"].as_u64().unwrap() as usize).unwrap();
        let items: Vec<Value> = comp["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                json!([
                    i["cat"],
                    i["key"],
                    i["name"],
                    i["detail"],
                    i["count"],
                    i["tokens"]
                ])
            })
            .collect();
        let got = json!({"total": comp["total"], "exact": comp["exact"], "scaled": comp["scaled"],
            "categories": comp["categories"], "item_count": comp["item_count"], "items": items});
        assert_eq!(got, c["composition"], "{name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn security_cases() {
    use cam_engine::health::{hook_name, mcp_servers, remote_apps, scan_keys, settings_security};
    let fx = fixture("security.json");
    let head =
        |v: &Value, n: usize| -> String { v.as_str().unwrap_or("").chars().take(n).collect() };
    for c in cases(&fx, "scan_keys") {
        assert_eq!(
            scan_keys(c["text"].as_str().unwrap()),
            c["want"],
            "{}",
            c["name"]
        );
    }
    for c in cases(&fx, "hook_name") {
        assert_eq!(json!(hook_name(c[0].as_str().unwrap())), c[1], "{}", c[0]);
    }
    for c in cases(&fx, "settings") {
        let docs: Vec<(String, Value)> = c["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d[0].as_str().unwrap().to_string(), d[1].clone()))
            .collect();
        let (bypass, hooks) = settings_security(&docs);
        assert_eq!(json!(bypass), c["bypass"], "{}", c["name"]);
        assert_eq!(json!(hooks), c["hooks"], "{}", c["name"]);
    }
    for c in cases(&fx, "mcp") {
        let (names, local) = mcp_servers(&c["doc"]);
        assert_eq!(json!(names), c["names"], "{}", c["name"]);
        assert_eq!(json!(local), c["local"], "{}", c["name"]);
    }
    for c in cases(&fx, "remote_apps") {
        let names: Vec<String> = c["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_string())
            .collect();
        assert_eq!(json!(remote_apps(&names)), c["want"], "{}", c["name"]);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let kinds = [
        "history_key",
        "perm_bypass",
        "panel_lan",
        "proxy_lan",
        "config_perm",
        "auto_run",
        "remote_app",
        "wifi",
    ];
    for c in cases(&fx, "risk") {
        let data = json!({"region": {"loc": "US", "checked_at": now, "env": c["env"]}});
        let items = risk_items(&data, true, false);
        let got: Vec<Value> = items
            .iter()
            .filter(|i| kinds.contains(&i["kind"].as_str().unwrap_or("")))
            .map(|i| {
                json!({"kind": i["kind"], "level": i["level"], "text": i["text"],
                       "points": risk_points(i), "advice_head": head(&i["advice"], 12)})
            })
            .collect();
        assert_eq!(json!(got), c["want"], "{}", c["name"]);
        assert_eq!(risk_score(&items)["score"], c["score"], "{}", c["name"]);
    }
    let c = &fx["config_perm"];
    let data = json!({"region": {"loc": "US", "checked_at": now, "env": c["env"]}});
    let items = risk_items(&data, true, false);
    let got = items.iter().find(|i| i["kind"] == "config_perm").unwrap();
    assert_eq!(got["level"], c["level"]);
    assert_eq!(json!(risk_points(got)), c["points"]);
    assert!(got["text"]
        .as_str()
        .unwrap()
        .contains(c["text_has"].as_str().unwrap()));
    assert!(got["advice"]
        .as_str()
        .unwrap()
        .contains(c["advice_has"].as_str().unwrap()));
    for c in cases(&fx, "domains") {
        let mut reg =
            json!({"loc": c["loc"], "checked_at": now, "env": {"proxy": [], "tunnel": null}});
        if !c["domains"].is_null() {
            reg["domains"] = c["domains"].clone();
        }
        let data = json!({ "region": reg });
        let items = risk_items(&data, true, false);
        let got: Vec<Value> = items
            .iter()
            .filter(|i| i["kind"] == "domain_region")
            .map(|i| {
                json!({"level": i["level"], "text": i["text"], "points": risk_points(i),
                       "advice_head": head(&i["advice"], 14)})
            })
            .collect();
        assert_eq!(json!(got), c["want"], "{}", c["name"]);
        assert_eq!(region_view(&data)["domains"], c["view"], "{}", c["name"]);
        assert_eq!(risk_score(&items)["score"], c["score"], "{}", c["name"]);
    }
}

#[test]
fn regions_cases() {
    use cam_engine::health::{region_label, region_status, regions_info};
    let fx = fixture("regions.json");
    let info = regions_info();
    for (k, v) in info.as_object().unwrap() {
        assert_eq!(v, &fx[k], "{}", k);
    }
    assert_eq!(info["supported"].as_array().unwrap().len(), 185);
    for c in cases(&fx, "status") {
        assert_eq!(json!(region_status(c[0].as_str())), c[1], "{}", c[0]);
    }
    for c in cases(&fx, "labels") {
        assert_eq!(json!(region_label(c[0].as_str())), c[1], "{}", c[0]);
    }
}

#[test]
fn mail_detect_cases() {
    let fx = fixture("mail_detect.json");
    for c in fx["cases"].as_array().unwrap() {
        let email = c["email"].as_str().unwrap();
        let got = detect(email);
        if c["expect"].is_null() {
            assert!(got.is_err(), "{email}");
            continue;
        }
        let got = got.unwrap_or_else(|e| panic!("{email}: {e}"));
        for (key, want) in c["expect"].as_object().unwrap() {
            assert_eq!(&got[key], want, "{email} {key}");
        }
        assert_eq!(got["imap_port"], 993, "{email}");
        assert!(!got["note"].as_str().unwrap().is_empty(), "{email}");
    }
}
