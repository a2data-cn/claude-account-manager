//! 服务检查与出口地区检测：DNS、出口地区、api.anthropic.com、claude.ai、代理。与 cam.py 的同名函数一致。
//! 只检测和提示，不干预任何操作。

use super::claude::{oauth_block, read_live};
use super::store::*;
use super::usage::{api_base, get_oauth, get_with, http_json_timeout, http_text, web_base};
use serde_json::{json, Value};
use std::net::ToSocketAddrs;
use std::time::Instant;

/// Anthropic 官方公布的支持国家/地区（https://www.anthropic.com/supported-countries，2026-10 抓取），
/// ISO 3166-1 alpha-2。名单会变，变了改这里。
pub const SUPPORTED_REGIONS: &[&str] = &[
    "AL", "DZ", "AD", "AO", "AG", "AR", "AM", "AU", "AT", "AZ", "BS", "BH", "BD", "BB", "BE", "BZ", "BJ", "BT", "BO",
    "BA", "BW", "BR", "BN", "BG", "BF", "BI", "CV", "KH", "CM", "CA", "CF", "TD", "CL", "CO", "KM", "CG", "CD", "CR",
    "CI", "HR", "CY", "CZ", "DK", "DJ", "DM", "DO", "EC", "EG", "SV", "GQ", "ER", "EE", "SZ", "ET", "FJ", "FI", "FR",
    "GA", "GM", "GE", "DE", "GH", "GR", "GD", "GT", "GN", "GW", "GY", "HT", "HN", "HU", "IS", "IN", "ID", "IQ", "IE",
    "IL", "IT", "JM", "JP", "JO", "KZ", "KE", "KI", "KW", "KG", "LA", "LV", "LB", "LS", "LR", "LY", "LI", "LT", "LU",
    "MG", "MW", "MY", "MV", "ML", "MT", "MH", "MR", "MU", "MX", "FM", "MD", "MC", "MN", "ME", "MA", "MZ", "NA", "NR",
    "NP", "NL", "NZ", "NI", "NE", "NG", "MK", "NO", "OM", "PK", "PW", "PS", "PA", "PG", "PY", "PE", "PH", "PL", "PT",
    "QA", "RO", "RW", "KN", "LC", "VC", "WS", "SM", "ST", "SA", "SN", "RS", "SC", "SL", "SG", "SK", "SI", "SO", "SB",
    "ZA", "KR", "SS", "ES", "LK", "SD", "SR", "SE", "CH", "TW", "TJ", "TZ", "TH", "TL", "TG", "TO", "TT", "TN", "TR",
    "TM", "TV", "UG", "UA", "AE", "GB", "US", "UY", "UZ", "VU", "VA", "VN", "ZM", "ZW",
];
/// 官方名单写的是国家；这些属地按所属国家看待（美国、英国、法国、荷兰、丹麦、芬兰、挪威、新西兰、澳大利亚）
pub const REGION_DEPENDENCIES: &[&str] = &[
    "PR", "GU", "VI", "AS", "MP", "GI", "IM", "JE", "GG", "BM", "KY", "FK", "VG", "RE", "GP", "MQ", "GF", "YT", "NC",
    "PF", "PM", "WF", "BL", "MF", "AW", "CW", "SX", "BQ", "GL", "FO", "AX", "SJ", "CK", "NU", "TK", "CX", "CC", "NF",
];
/// 给提示用的中文名，只收常见的
pub const REGION_NAMES: &[(&str, &str)] = &[
    ("CN", "中国大陆"), ("HK", "香港"), ("MO", "澳门"), ("RU", "俄罗斯"), ("IR", "伊朗"), ("KP", "朝鲜"), ("SY", "叙利亚"),
    ("CU", "古巴"), ("BY", "白俄罗斯"), ("VE", "委内瑞拉"), ("AF", "阿富汗"), ("MM", "缅甸"), ("YE", "也门"),
    ("T1", "Tor 出口"), ("US", "美国"), ("JP", "日本"), ("SG", "新加坡"), ("TW", "台湾"), ("KR", "韩国"), ("GB", "英国"),
    ("DE", "德国"), ("FR", "法国"), ("NL", "荷兰"), ("CA", "加拿大"), ("AU", "澳大利亚"), ("MY", "马来西亚"), ("TH", "泰国"),
    ("VN", "越南"), ("ID", "印尼"), ("PH", "菲律宾"), ("IN", "印度"), ("AE", "阿联酋"), ("TR", "土耳其"),
];

pub fn region_advice(status: &str) -> &'static str {
    match status {
        "unsupported" => "按 Anthropic 政策这个地区不提供服务，继续使用可能导致账号被限制或封禁。建议换到支持地区的网络出口后再使用。",
        "unknown" => "连不上 Cloudflare，判断不了出口地区。检查网络或代理后重新检测。",
        _ => "",
    }
}

pub fn region_label(loc: Option<&str>) -> String {
    match loc {
        Some(l) => match REGION_NAMES.iter().find(|(c, _)| *c == l) {
            Some((_, n)) => format!("{}（{}）", l, n),
            None => l.to_string(),
        },
        None => "未知".into(),
    }
}

/// supported / unsupported / unknown
pub fn region_status(loc: Option<&str>) -> &'static str {
    match loc {
        Some(l) if !l.is_empty() => {
            if SUPPORTED_REGIONS.contains(&l) || REGION_DEPENDENCIES.contains(&l) {
                "supported"
            } else {
                "unsupported"
            }
        }
        _ => "unknown",
    }
}

pub fn region_message(loc: Option<&str>, status: &str) -> String {
    if status == "unknown" {
        return "出口地区：查不到".into();
    }
    format!(
        "出口地区 {} {}",
        region_label(loc),
        if status == "supported" { "在 Anthropic 支持名单内" } else { "不在 Anthropic 支持名单内" }
    )
}

/// 从 Cloudflare 看出口地区（claude.ai 在它后面，它看到的就是 claude.ai 看到的）。返回 {loc, ip, colo, ms} 或 None。
pub fn detect_region() -> Option<Value> {
    let t0 = Instant::now();
    let (mut st, mut txt) = http_text(&format!("{}/cdn-cgi/trace", web_base()), 6);
    if st != 200 || !txt.contains("loc=") {
        (st, txt) = http_text("https://www.cloudflare.com/cdn-cgi/trace", 6);
    }
    if st != 200 || !txt.contains("loc=") {
        return None;
    }
    let get = |k: &str| {
        txt.lines()
            .find_map(|l| l.strip_prefix(&format!("{}=", k)))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some(json!({
        "loc": get("loc").map(|l| l.to_uppercase()), "ip": get("ip"), "colo": get("colo"),
        "ms": t0.elapsed().as_millis() as u64
    }))
}

/// 界面常显用的出口地区状态（不联网，用后台上次检测的结果）。没检测过时 status 为 null。
pub fn region_view(data: &Value) -> Value {
    let c = data.get("region").cloned().unwrap_or(Value::Null);
    let Some(checked) = s(&c, "checked_at").map(String::from) else {
        return json!({"loc": null, "ip": null, "status": null, "checked_at": null,
                      "message": "还没有检测过出口地区", "advice": ""});
    };
    let loc = s(&c, "loc").map(String::from);
    let status = region_status(loc.as_deref());
    json!({
        "loc": loc, "ip": s(&c, "ip"), "status": status, "checked_at": checked,
        "message": region_message(loc.as_deref(), status), "advice": region_advice(status)
    })
}

/// 后台定时调用：检测一次出口地区记到 data["region"]，给界面常显。max_age 秒内查过就跳过。只记录，不干预任何操作。
pub fn check_region(store: &Store, max_age: i64) -> R<Value> {
    let data = store.load()?;
    if let Some(at) = data.get("region").and_then(|c| s(c, "checked_at")).and_then(parse_time) {
        if chrono::Utc::now().signed_duration_since(at).num_seconds() < max_age {
            return Ok(region_view(&data));
        }
    }
    let r = detect_region().unwrap_or_else(|| json!({}));
    let _g = store.lock()?;
    let mut data = store.load()?;
    data["region"] = json!({"loc": r.get("loc"), "ip": r.get("ip"), "colo": r.get("colo"), "checked_at": now_iso()});
    store.save(&mut data)?;
    Ok(region_view(&data))
}

fn region_denied(msg: &str) -> bool {
    let low = msg.to_lowercase();
    ["region", "country", "not available", "unsupported", "not allowed", "地区"]
        .iter()
        .any(|k| low.contains(k))
}

pub fn is_cloudflare(raw: &str) -> bool {
    let low = raw.to_lowercase();
    ["just a moment", "cf-chl", "cloudflare", "challenge-platform", "/cdn-cgi/"]
        .iter()
        .any(|k| low.contains(k))
}

fn host_of(url: &str) -> String {
    let rest = url.trim_start_matches("https://").trim_start_matches("http://");
    let hostport = rest.split('/').next().unwrap_or(rest);
    hostport.split(':').next().unwrap_or(hostport).to_string()
}

fn err_message(body: &Value) -> String {
    body.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string()
}

fn or_dash(msg: &str) -> &str {
    if msg.is_empty() {
        "-"
    } else {
        msg
    }
}

/// 返回 [{level, text, advice}]，级别为 ok / bad / warn / info，第一项是总体结论。不包含任何密钥。
pub fn service_check(_store: &Store) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut add = |level: &str, text: String, advice: &str| out.push(json!({"level": level, "text": text, "advice": advice}));
    let proxies: Vec<(String, String)> = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect();
    let api_host = host_of(&api_base());
    let web_host = host_of(&web_base());
    let mut hosts = vec![api_host.clone()];
    if web_host != api_host {
        hosts.push(web_host.clone());
    }
    for host in &hosts {
        match (host.as_str(), 443u16).to_socket_addrs() {
            Ok(addrs) => {
                let mut ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
                ips.sort();
                ips.dedup();
                add("ok", format!("DNS：{} → {}", host, ips.iter().take(3).cloned().collect::<Vec<_>>().join(", ")), "");
            }
            Err(e) => add(
                "bad",
                format!("DNS：{} 解析失败（{}）", host, e),
                if proxies.is_empty() { "检查本机 DNS 或代理设置" } else { "检查本机 DNS 或代理设置；设了代理的话通常由代理解析，可忽略" },
            ),
        }
    }
    // 出口地区：claude.ai 在 Cloudflare 后面，/cdn-cgi/trace 会返回它看到的访问者信息
    match detect_region() {
        Some(r) if s(&r, "loc").is_some() => {
            let loc = s(&r, "loc");
            let status = region_status(loc);
            add(
                if status == "supported" { "ok" } else { "warn" },
                format!(
                    "{}（IP {}，Cloudflare 节点 {}，{} ms）",
                    region_message(loc, status),
                    s(&r, "ip").unwrap_or("?"),
                    s(&r, "colo").unwrap_or("?"),
                    r.get("ms").and_then(|v| v.as_u64()).unwrap_or(0)
                ),
                region_advice(status),
            );
        }
        _ => add("warn", "出口地区：查不到（连不上 Cloudflare 的 trace 接口）".into(), region_advice("unknown")),
    }
    // api.anthropic.com：有登录就用当前登录实测
    let at = read_live()
        .ok()
        .and_then(|(c, _)| c)
        .and_then(|c| s(&oauth_block(&c), "accessToken").map(String::from));
    let t0 = Instant::now();
    let (st, body) = match &at {
        Some(t) => get_oauth(t, "/api/oauth/profile", 10),
        None => http_json_timeout("GET", &format!("{}/api/oauth/usage", api_base()), &[], None, 10),
    };
    let ms = t0.elapsed().as_millis();
    let msg = err_message(&body);
    let region_hint = "通常是出口地区不在 Anthropic 支持范围，换一个出口后重试";
    if st == 0 {
        add("bad", format!("{}：连不上（{}）", api_host, msg), "检查网络或代理；公司网络可能屏蔽了这个域名");
    } else if st == 403 || region_denied(&msg) {
        add("bad", format!("{}：拒绝访问（HTTP {}：{}）", api_host, st, or_dash(&msg)), region_hint);
    } else if at.is_some() && st == 200 {
        add("ok", format!("{}：服务正常，当前登录可用（{} ms）", api_host, ms), "");
    } else if at.is_some() && st == 401 {
        add("warn", format!("{}：服务可达，但当前登录已过期或无效（{} ms）", api_host, ms), "运行一次 claude 会自动续登录，不影响切换账号");
    } else if st == 401 {
        add("ok", format!("{}：服务可达（未登录，{} ms）", api_host, ms), "");
    } else if st >= 500 {
        add("bad", format!("{}：服务端错误（HTTP {}）", api_host, st), "可能是 Anthropic 故障，稍后再试，可查看 status.anthropic.com");
    } else {
        add("warn", format!("{}：HTTP {}（{}，{} ms）", api_host, st, or_dash(&msg), ms), "少见的返回，稍后重试；持续出现请把这一行发给作者");
    }
    // claude.ai：不带登录，看可达性和有没有被网络管控拦截
    let t0 = Instant::now();
    let (code, body) = get_with(
        &format!("{}/api/organizations", web_base()),
        &[("Cookie".to_string(), "sessionKey=".to_string())],
    );
    let ms = t0.elapsed().as_millis();
    let msg = err_message(&body);
    if code == 0 {
        add("bad", format!("{}：连不上（{}）", web_host, msg), "检查网络或代理");
    } else if is_cloudflare(&msg) {
        add(
            "warn",
            format!("{}：返回人机验证页（{} ms）", web_host, ms),
            "公司网络或代理触发了 Cloudflare 人机验证；只影响网页账号读取额度，不影响 Claude Code 切换",
        );
    } else if region_denied(&msg) {
        add("bad", format!("{}：拒绝访问（HTTP {}：{}）", web_host, code, msg), region_hint);
    } else if code == 200 || code == 401 || code == 403 {
        add("ok", format!("{}：可达（{} ms）", web_host, ms), "");
    } else if code >= 500 {
        add("bad", format!("{}：服务端错误（HTTP {}）", web_host, code), "稍后再试");
    } else {
        add("warn", format!("{}：HTTP {}（{}，{} ms）", web_host, code, or_dash(&msg), ms), "少见的返回，稍后重试");
    }
    add(
        "info",
        format!(
            "代理：{}",
            if proxies.is_empty() {
                "无（直连）".to_string()
            } else {
                proxies.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("；")
            }
        ),
        if proxies.is_empty() { "直连，出口地区就是本机网络的出口" } else { "请求经过代理，上面的出口地区是代理的出口" },
    );
    // 总体结论放最前面
    let first = |lv: &str| out.iter().find(|i| s(i, "level") == Some(lv)).cloned();
    let summary = if let Some(b) = first("bad") {
        json!({"level": "bad", "text": format!("总体：当前无法正常使用 —— {}", s(&b, "text").unwrap_or("")), "advice": b["advice"]})
    } else if let Some(w) = first("warn") {
        json!({"level": "warn", "text": format!("总体：可以使用，但有风险 —— {}", s(&w, "text").unwrap_or("")), "advice": w["advice"]})
    } else {
        json!({"level": "ok", "text": "总体：Claude 服务正常，出口地区在支持名单内", "advice": ""})
    };
    out.insert(0, summary);
    out
}
