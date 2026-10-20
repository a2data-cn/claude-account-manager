//! 服务检查：DNS、出口地区、api.anthropic.com、claude.ai、代理。与 cam.py 的 service_check 一致。

use super::claude::{oauth_block, read_live};
use super::store::*;
use super::usage::{api_base, get_oauth, get_with, http_json_timeout, http_text, web_base};
use serde_json::Value;
use std::net::ToSocketAddrs;
use std::time::Instant;

/// Anthropic 公开的“不提供服务的地区”里常见的几个（名单会变，只用来提示，实际以接口实测为准）
const UNSUPPORTED_REGIONS: &[(&str, &str)] = &[
    ("CN", "中国大陆"),
    ("HK", "香港"),
    ("MO", "澳门"),
    ("RU", "俄罗斯"),
    ("IR", "伊朗"),
    ("KP", "朝鲜"),
    ("SY", "叙利亚"),
    ("CU", "古巴"),
    ("BY", "白俄罗斯"),
    ("VE", "委内瑞拉"),
    ("AF", "阿富汗"),
    ("MM", "缅甸"),
    ("YE", "也门"),
];

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

/// 返回 [(级别, 文字)]，级别为 ok / bad / warn / info。不包含任何密钥。
pub fn service_check(_store: &Store) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
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
                out.push((
                    "ok".into(),
                    format!("DNS：{} → {}", host, ips.iter().take(3).cloned().collect::<Vec<_>>().join(", ")),
                ));
            }
            Err(e) => out.push((
                "bad".into(),
                format!(
                    "DNS：{} 解析失败（{}）{}",
                    host,
                    e,
                    if proxies.is_empty() { "" } else { "；设了代理的话由代理解析，可忽略" }
                ),
            )),
        }
    }
    // 出口地区：claude.ai 在 Cloudflare 后面，/cdn-cgi/trace 会返回它看到的访问者信息
    let t0 = Instant::now();
    let (mut st, mut txt) = http_text(&format!("{}/cdn-cgi/trace", web_base()), 8);
    if st != 200 || !txt.contains("loc=") {
        (st, txt) = http_text("https://www.cloudflare.com/cdn-cgi/trace", 8);
    }
    if st == 200 && txt.contains("loc=") {
        let get = |k: &str| {
            txt.lines()
                .find_map(|l| l.strip_prefix(&format!("{}=", k)))
                .unwrap_or("?")
                .trim()
                .to_string()
        };
        let (loc, ip, colo) = (get("loc"), get("ip"), get("colo"));
        match UNSUPPORTED_REGIONS.iter().find(|(c, _)| *c == loc) {
            Some((_, name)) => out.push((
                "warn".into(),
                format!("出口地区：{}（{}，IP {}）—— Anthropic 公开名单里不支持的地区，以下面的实测为准", loc, name, ip),
            )),
            None => out.push((
                "ok".into(),
                format!("出口地区：{}（IP {}，Cloudflare 节点 {}，{} ms）", loc, ip, colo, t0.elapsed().as_millis()),
            )),
        }
    } else {
        out.push(("info".into(), "出口地区：查不到（连不上 Cloudflare 的 trace 接口）".into()));
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
    let (level, text) = if st == 0 {
        ("bad", format!("{}：连不上（{}）", api_host, msg))
    } else if st == 403 || region_denied(&msg) {
        ("bad", format!("{}：拒绝访问（HTTP {}：{}），通常是出口地区不支持，换个出口再试", api_host, st, or_dash(&msg)))
    } else if at.is_some() && st == 200 {
        ("ok", format!("{}：服务正常，当前登录可用（{} ms）", api_host, ms))
    } else if at.is_some() && st == 401 {
        ("warn", format!("{}：服务可达，但当前登录已过期或无效，Claude Code 下次运行时会自动续（{} ms）", api_host, ms))
    } else if st == 401 {
        ("ok", format!("{}：服务可达（未登录，{} ms）", api_host, ms))
    } else if st >= 500 {
        ("bad", format!("{}：服务端错误（HTTP {}），可能是 Anthropic 故障，稍后再试", api_host, st))
    } else {
        ("warn", format!("{}：HTTP {}（{}，{} ms）", api_host, st, or_dash(&msg), ms))
    };
    out.push((level.into(), text));
    // claude.ai：不带登录，看可达性和有没有被网络管控拦截
    let t0 = Instant::now();
    let (code, body) = get_with(
        &format!("{}/api/organizations", web_base()),
        &[("Cookie".to_string(), "sessionKey=".to_string())],
    );
    let ms = t0.elapsed().as_millis();
    let msg = err_message(&body);
    let (level, text) = if code == 0 {
        ("bad", format!("{}：连不上（{}）", web_host, msg))
    } else if is_cloudflare(&msg) {
        ("warn", format!("{}：返回人机验证页（通常是网络被公司管控拦截），网页账号的额度会拿不到（{} ms）", web_host, ms))
    } else if region_denied(&msg) {
        ("bad", format!("{}：拒绝访问（HTTP {}：{}），通常是出口地区不支持", web_host, code, msg))
    } else if code == 200 || code == 401 || code == 403 {
        ("ok", format!("{}：可达（{} ms）", web_host, ms))
    } else if code >= 500 {
        ("bad", format!("{}：服务端错误（HTTP {}），稍后再试", web_host, code))
    } else {
        ("warn", format!("{}：HTTP {}（{}，{} ms）", web_host, code, or_dash(&msg), ms))
    };
    out.push((level.into(), text));
    out.push((
        "info".into(),
        format!(
            "代理：{}",
            if proxies.is_empty() {
                "无（直连）".to_string()
            } else {
                proxies.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("；")
            }
        ),
    ));
    out
}
