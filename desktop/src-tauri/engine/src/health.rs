//! 服务检查、出口地区与使用环境风险检测：DNS、出口地区、api.anthropic.com、claude.ai、代理、VPN，
//! 以及出口变化、时区、时钟偏差、官方状态页、账号多地区。与 cam.py 的同名函数一致。
//! 只检测和提示，不干预任何操作。

use super::claude::{oauth_block, read_live};
use super::store::*;
use super::usage::{api_base, get_oauth, get_with, http_json_timeout, http_text, web_base};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::process::Command;
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

/// 出口地区变了以后，提醒保留多久（小时）
pub const REGION_CHANGE_HOURS: i64 = 24;
/// 同一账号几天内从多个地区使用过就提醒
pub const ACCOUNT_REGION_DAYS: i64 = 7;
/// 每个账号最多记几个地区
pub const ACCOUNT_REGION_KEEP: usize = 8;
/// 本机时钟与服务器相差超过这么多秒就提醒
pub const CLOCK_SKEW_WARN: i64 = 120;
/// 切换账号时，多久以内的出口地区检测结果算有效（秒）
pub const REGION_FRESH_SECS: i64 = 3600;
/// 默认路由走这些接口就当作 VPN / 隧道（macOS utun、Linux tun/wg、各家 VPN 客户端）
const TUNNEL_PREFIXES: &[&str] = &[
    "utun", "tun", "tap", "wg", "ppp", "ipsec", "tailscale", "zt", "nebula", "proton", "nordlynx", "mullvad", "clash",
    "mihomo", "sing", "hysteria", "surge",
];
const TUNNEL_WORDS: &[&str] = &[
    "tap", "tun", "wintun", "wireguard", "vpn", "tunnel", "nordlynx", "clash", "mihomo", "sing-box", "surge",
    "shadowsocks", "v2ray", "hysteria", "tailscale", "zerotier", "anyconnect", "fortinet", "globalprotect",
    "pulse secure", "openvpn", "expressvpn", "mullvad", "proton",
];

fn status_url() -> String {
    std::env::var("CAM_STATUS_URL").unwrap_or_else(|_| "https://status.anthropic.com/api/v2/status.json".into())
}

/// 出口 IP 的运营商、城市（Cloudflare 的另一个接口，用来交叉核对和判断是不是机房出口）
fn cf_meta_url() -> String {
    std::env::var("CAM_CF_META_URL").unwrap_or_else(|_| "https://speed.cloudflare.com/meta".into())
}

/// Cloudflare 的 meta 接口拒绝时退到这里（同样返回运营商、城市、国家）
fn ipinfo_url() -> String {
    std::env::var("CAM_IPINFO_URL").unwrap_or_else(|_| "https://ipinfo.io/json".into())
}

/// 出口 IP 的运营商名里有这些就当作云服务商 / 机房（不是家庭或公司宽带）
const HOSTING_WORDS: &[&str] = &[
    "hosting", "cloud", "data center", "datacenter", "server", "vps", "dedicated", "colocation", "amazon", "aws",
    "google llc", "google cloud", "microsoft corporation", "azure", "digitalocean", "linode", "akamai", "vultr", "choopa",
    "ovh", "hetzner", "alibaba", "aliyun", "tencent", "huawei", "oracle", "m247", "datacamp", "leaseweb", "contabo",
    "scaleway", "upcloud", "hostinger", "psychz", "quadranet", "colocrossing", "zenlayer", "gcore", "g-core",
    "cloudflare", "packethub", "clouvider", "performive", "bandwagon", "racknerd", "ucloud", "kamatera", "ionos",
    "fly.io", "oneprovider", "hostpapa",
];

pub fn region_advice(status: &str) -> &'static str {
    match status {
        "unsupported" => "按 Anthropic 政策这个地区不提供服务，继续使用可能导致账号被限制或封禁。建议换到支持地区的网络出口后再使用。",
        "unknown" => "连不上 Cloudflare，判断不了出口地区。检查网络或代理后重新检测。",
        "supported" => "出口 IP 这一项没问题。Anthropic 结合 IP 和其他信号综合判断所在地，下面的代理 / VPN、时区、出口变化等就是这些信号里本机能看到的部分。",
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

pub fn fmt_ago(s: &str) -> String {
    let Some(d) = parse_time(s) else { return String::new() };
    let secs = chrono::Utc::now().signed_duration_since(d).num_seconds().max(0);
    if secs < 60 {
        return "刚刚".into();
    }
    let (h, m) = (secs / 3600, (secs / 60) % 60);
    if h >= 24 {
        format!("{} 天前", h / 24)
    } else if h > 0 {
        format!("{} 小时前", h)
    } else {
        format!("{} 分钟前", m)
    }
}

/// 从 Cloudflare 看出口地区（claude.ai 在它后面，它看到的就是 claude.ai 看到的）。
/// 返回 {loc, ip, colo, ms, warp, skew} 或 None：warp 是请求经过了 Cloudflare WARP / Zero Trust，
/// skew 是本机时钟比服务器快多少秒（没有 ts 字段时为 null）。
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let skew = get("ts").and_then(|t| t.parse::<f64>().ok()).map(|t| (now - t).round() as i64);
    let warp = matches!(get("warp").as_deref(), Some("on") | Some("plus")) || get("gateway").as_deref() == Some("on");
    let mut out = json!({
        "loc": get("loc").map(|l| l.to_uppercase()), "ip": get("ip"), "colo": get("colo"),
        "ms": t0.elapsed().as_millis() as u64, "warp": warp, "skew": skew,
        "asn": null, "org": null, "city": null, "meta_loc": null
    });
    if let Some(meta) = fetch_exit_meta() {
        for k in ["asn", "org", "city", "meta_loc"] {
            out[k] = meta.get(k).cloned().unwrap_or(Value::Null);
        }
    }
    Some(out)
}

/// 出口 IP 的 ASN、运营商、城市和国家：先问 Cloudflare 的 meta 接口，拒绝时退到 ipinfo.io。查不到返回 None。
pub fn fetch_exit_meta() -> Option<Value> {
    for url in [cf_meta_url(), ipinfo_url()] {
        let (st, txt) = http_text(&url, 6);
        if st != 200 {
            continue;
        }
        if let Some(m) = serde_json::from_str::<Value>(&txt).ok().and_then(|j| parse_exit_meta(&j)) {
            return Some(m);
        }
    }
    None
}

/// 统一两种格式：Cloudflare {asn, asOrganization, city, region, country} 和 ipinfo {org: "AS7922 Comcast…", city, region, country}。
pub fn parse_exit_meta(j: &Value) -> Option<Value> {
    j.as_object()?;
    let (mut asn, mut org) = (j.get("asn").and_then(|v| v.as_u64()), s(j, "asOrganization").map(String::from));
    if org.is_none() {
        if let Some(o) = s(j, "org") {
            let (head, rest) = o.split_once(' ').unwrap_or((o, ""));
            if let Some(n) = head.strip_prefix("AS").and_then(|n| n.parse::<u64>().ok()) {
                asn = asn.or(Some(n));
                org = Some(rest.trim().to_string()).filter(|r| !r.is_empty());
            } else {
                org = Some(o.to_string());
            }
        }
    }
    let city = [s(j, "city"), s(j, "region")].iter().flatten().cloned().collect::<Vec<_>>().join(", ");
    let loc = s(j, "country").map(|c| c.trim().to_uppercase()).filter(|c| !c.is_empty());
    if org.is_none() && loc.is_none() {
        return None;
    }
    Some(json!({
        "asn": asn, "org": org, "city": if city.is_empty() { None } else { Some(city) }, "meta_loc": loc
    }))
}

/// 运营商名像云服务商 / 机房时为 true（这类出口通常被当作 VPN / 代理流量）。
pub fn hosting_exit(org: Option<&str>) -> bool {
    let low = org.unwrap_or("").to_lowercase();
    !low.is_empty() && HOSTING_WORDS.iter().any(|w| low.contains(w))
}

/// 本机系统语言 / 区域 (名称, 国家代码) 或 None。测试用 CAM_TEST_LOCALE="zh_CN"（空字符串表示查不到）。
pub fn system_locale() -> Option<(String, Option<String>)> {
    let name = if let Ok(t) = std::env::var("CAM_TEST_LOCALE") {
        t
    } else if cfg!(target_os = "macos") {
        run_text("defaults", &["read", "-g", "AppleLocale"]).trim().to_string()
    } else if cfg!(windows) {
        run_text("powershell", &["-NoProfile", "-Command", "(Get-Culture).Name"]).trim().to_string()
    } else {
        ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_default()
    };
    let name = name.split('.').next().unwrap_or("").split('@').next().unwrap_or("").trim().to_string();
    if name.is_empty() || name.eq_ignore_ascii_case("C") || name.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    let cc = name
        .replace('-', "_")
        .split('_')
        .skip(1)
        .find(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|p| p.to_uppercase());
    Some((name, cc))
}

/// Anthropic 官方状态页（status.anthropic.com）的总体状态：{indicator, description} 或 None（连不上）。
pub fn fetch_status_page() -> Option<Value> {
    let (st, txt) = http_text(&status_url(), 6);
    if st != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&txt).ok()?;
    let status = v.get("status")?;
    let ind = s(status, "indicator")?;
    Some(json!({"indicator": ind, "description": s(status, "description").unwrap_or("")}))
}

fn run_text(cmd: &str, args: &[&str]) -> String {
    let mut c = Command::new(cmd);
    c.args(args);
    #[cfg(windows)]
    {
        // 桌面版是 GUI 程序，不加这个会闪一下控制台窗口
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// 本机时区 (时区名, 国家代码列表) 或 None。国家代码来自 tzdata 的 zone1970.tab，查不到时为空列表。
/// 测试用 CAM_TEST_TZ="Asia/Hong_Kong=HK"（空字符串表示查不到）。
pub fn system_timezone() -> Option<(String, Vec<String>)> {
    if let Ok(t) = std::env::var("CAM_TEST_TZ") {
        if t.is_empty() {
            return None;
        }
        let (zone, cc) = t.split_once('=').unwrap_or((t.as_str(), ""));
        return Some((zone.to_string(), cc.split(',').filter(|c| !c.is_empty()).map(String::from).collect()));
    }
    let mut zone = std::env::var("TZ").unwrap_or_default().trim_start_matches(':').to_string();
    if zone.is_empty() && !cfg!(windows) {
        if let Ok(link) = std::fs::read_link("/etc/localtime") {
            if let Some((_, z)) = link.to_string_lossy().split_once("zoneinfo/") {
                zone = z.to_string();
            }
        }
    }
    if zone.is_empty() && cfg!(windows) {
        zone = run_text("tzutil", &["/g"]).trim().to_string();
    }
    if zone.is_empty() {
        return None;
    }
    let mut countries: Vec<String> = Vec::new();
    for tab in [
        "/usr/share/zoneinfo/zone1970.tab",
        "/var/db/timezone/zoneinfo/zone1970.tab",
        "/usr/share/zoneinfo/zone.tab",
        "/var/db/timezone/zoneinfo/zone.tab",
    ] {
        let Ok(txt) = std::fs::read_to_string(tab) else { continue };
        for line in txt.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if !line.starts_with('#') && parts.len() >= 3 && parts[2] == zone {
                countries = parts[0].split(',').filter(|c| !c.is_empty()).map(String::from).collect();
                break;
            }
        }
        if !countries.is_empty() {
            break;
        }
    }
    Some((zone, countries))
}

/// 系统级代理设置（不是环境变量）：["HTTPS 127.0.0.1:7890", "PAC http://…"] 之类，没有时为空。
pub fn system_proxy() -> Vec<String> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        let txt = run_text("scutil", &["--proxy"]);
        let kv: HashMap<String, String> = txt
            .lines()
            .filter_map(|l| l.trim().split_once(" : "))
            .map(|(k, v)| (k.to_string(), v.trim().to_string()))
            .collect();
        let on = |k: &str| kv.get(k).map(|v| v == "1").unwrap_or(false);
        for (name, key) in [("HTTPS", "HTTPS"), ("HTTP", "HTTP"), ("SOCKS", "SOCKS")] {
            if on(&format!("{}Enable", key)) {
                if let Some(h) = kv.get(&format!("{}Proxy", key)) {
                    out.push(format!("{} {}:{}", name, h, kv.get(&format!("{}Port", key)).cloned().unwrap_or_default()));
                }
            }
        }
        if on("ProxyAutoConfigEnable") {
            out.push(format!("PAC {}", kv.get("ProxyAutoConfigURLString").cloned().unwrap_or_default()));
        }
    } else if cfg!(windows) {
        let txt = run_text("reg", &["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings"]);
        let mut kv: HashMap<String, String> = HashMap::new();
        for line in txt.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[1].starts_with("REG_") {
                kv.insert(parts[0].to_string(), parts[2..].join(" "));
            }
        }
        let enabled = kv.get("ProxyEnable").map(|v| v.to_lowercase() == "0x1" || v == "1").unwrap_or(false);
        if enabled {
            if let Some(p) = kv.get("ProxyServer") {
                out.push(format!("系统代理 {}", p));
            }
        }
        if let Some(u) = kv.get("AutoConfigURL").filter(|u| !u.is_empty()) {
            out.push(format!("PAC {}", u));
        }
    } else {
        let g = |args: &[&str]| run_text("gsettings", args).trim().trim_matches('\'').to_string();
        let mode = g(&["get", "org.gnome.system.proxy", "mode"]);
        if mode == "manual" {
            let host = g(&["get", "org.gnome.system.proxy.https", "host"]);
            let port = g(&["get", "org.gnome.system.proxy.https", "port"]);
            out.push(if host.is_empty() { "系统代理（手动）".into() } else { format!("HTTPS {}:{}", host, port) });
        } else if mode == "auto" {
            out.push(format!("PAC {}", g(&["get", "org.gnome.system.proxy", "autoconfig-url"])));
        }
    }
    out
}

/// 接口名 / 描述看起来是 VPN 或隧道时返回说明，否则 None。
pub fn tunnel_kind(name: &str, desc: &str) -> Option<&'static str> {
    let low = name.to_lowercase();
    if TUNNEL_PREFIXES.iter().any(|p| low.starts_with(p)) {
        return Some("VPN / 隧道接口");
    }
    let d = desc.to_lowercase();
    if TUNNEL_WORDS.iter().any(|w| d.contains(w)) {
        return Some("VPN / 隧道适配器");
    }
    None
}

/// 默认路由走的接口像 VPN / 隧道时返回 (接口名, 说明)。只看默认路由，透明代理和路由器上的 VPN 看不出来。
/// 测试用 CAM_TEST_ROUTE_IF="utun4"（空字符串表示没有）。
pub fn default_route_tunnel() -> Option<(String, &'static str)> {
    if let Ok(t) = std::env::var("CAM_TEST_ROUTE_IF") {
        return if t.is_empty() { None } else { tunnel_kind(&t, "").map(|k| (t, k)) };
    }
    let (mut name, mut desc) = (String::new(), String::new());
    if cfg!(windows) {
        let ps = "$r = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 -ErrorAction SilentlyContinue | \
                  Sort-Object RouteMetric, InterfaceMetric | Select-Object -First 1; \
                  if ($r) { $a = Get-NetAdapter -InterfaceIndex $r.InterfaceIndex -ErrorAction SilentlyContinue; \
                  \"$($r.InterfaceAlias)`t$($a.InterfaceDescription)\" }";
        let txt = run_text("powershell", &["-NoProfile", "-Command", ps]);
        if let Some(l) = txt.lines().next() {
            let (n, d) = l.split_once('\t').unwrap_or((l, ""));
            name = n.trim().to_string();
            desc = d.trim().to_string();
        }
    } else if cfg!(target_os = "macos") {
        for line in run_text("route", &["-n", "get", "default"]).lines() {
            if let Some(v) = line.trim().strip_prefix("interface:") {
                name = v.trim().to_string();
            }
        }
    } else {
        let txt = run_text("ip", &["route", "show", "default"]);
        let words: Vec<&str> = txt.split_whitespace().collect();
        if let Some(i) = words.iter().position(|w| *w == "dev") {
            name = words.get(i + 1).map(|n| n.to_string()).unwrap_or_default();
        }
    }
    if name.is_empty() {
        return None;
    }
    tunnel_kind(&name, &desc).map(|k| (name, k))
}

/// 记一笔“这个账号在这个出口地区用过”，给风险提示用（每个账号只留最近几个地区）。
pub fn note_account_region(data: &mut Value, acc_id: Option<&str>, loc: Option<&str>) {
    let (Some(id), Some(loc)) = (acc_id, loc) else { return };
    for a in accounts_mut(data) {
        if s(a, "id") == Some(id) {
            let mut regs: Vec<Value> = a
                .get("regions")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| s(r, "loc") != Some(loc))
                .collect();
            regs.insert(0, json!({"loc": loc, "at": now_iso()}));
            regs.truncate(ACCOUNT_REGION_KEEP);
            a["regions"] = Value::Array(regs);
        }
    }
}

/// 后台最近一次检测到的出口地区；超过 max_age 秒或没检测过时为 None。
pub fn cached_region_loc(data: &Value, max_age: i64) -> Option<String> {
    let reg = data.get("region")?;
    let at = s(reg, "checked_at").and_then(parse_time)?;
    let loc = s(reg, "loc")?;
    if chrono::Utc::now().signed_duration_since(at).num_seconds() <= max_age {
        Some(loc.to_string())
    } else {
        None
    }
}

/// 界面常显用的出口地区状态（不联网，用后台上次检测的结果）。没检测过时 status 为 null。
pub fn region_view(data: &Value) -> Value {
    let c = data.get("region").cloned().unwrap_or(Value::Null);
    let Some(checked) = s(&c, "checked_at").map(String::from) else {
        return json!({"loc": null, "ip": null, "status": null, "checked_at": null,
                      "message": "还没有检测过出口地区", "advice": "", "warp": false, "prev_loc": null, "changed_at": null,
                      "org": null, "city": null});
    };
    let loc = s(&c, "loc").map(String::from);
    let status = region_status(loc.as_deref());
    json!({
        "loc": loc, "ip": s(&c, "ip"), "status": status, "checked_at": checked,
        "message": region_message(loc.as_deref(), status), "advice": region_advice(status),
        "warp": c.get("warp").and_then(|v| v.as_bool()).unwrap_or(false),
        "prev_loc": s(&c, "prev_loc"), "changed_at": s(&c, "changed_at"),
        "org": s(&c, "org"), "city": s(&c, "city")
    })
}

/// 后台定时调用：检测出口地区（连带 WARP、时钟偏差）和 Anthropic 官方状态页，记到 data["region"] / data["status_page"]，
/// 并给当前账号记一笔地区。max_age 秒内查过就跳过。只记录，不干预任何操作。
pub fn check_region(store: &Store, max_age: i64) -> R<Value> {
    let data = store.load()?;
    if let Some(at) = data.get("region").and_then(|c| s(c, "checked_at")).and_then(parse_time) {
        if chrono::Utc::now().signed_duration_since(at).num_seconds() < max_age {
            return Ok(region_view(&data));
        }
    }
    let r = detect_region().unwrap_or_else(|| json!({}));
    let sp = fetch_status_page();
    let _g = store.lock()?;
    let mut data = store.load()?;
    let old = data.get("region").cloned().unwrap_or_else(|| json!({}));
    let now = now_iso();
    let new_loc = s(&r, "loc").map(String::from);
    let mut reg = json!({
        "loc": new_loc, "ip": r.get("ip"), "colo": r.get("colo"), "ms": r.get("ms"),
        "warp": r.get("warp").and_then(|v| v.as_bool()).unwrap_or(false), "skew": r.get("skew"),
        "asn": r.get("asn"), "org": r.get("org"), "city": r.get("city"), "meta_loc": r.get("meta_loc"), "checked_at": now
    });
    match (s(&old, "loc"), new_loc.as_deref()) {
        (Some(o), Some(n)) if o != n => {
            reg["prev_loc"] = json!(o);
            reg["changed_at"] = json!(now);
        }
        _ => {
            if let Some(p) = s(&old, "prev_loc") {
                reg["prev_loc"] = json!(p);
                reg["changed_at"] = old.get("changed_at").cloned().unwrap_or(Value::Null);
            }
        }
    }
    data["region"] = reg;
    match sp {
        Some(mut v) => {
            v["checked_at"] = json!(now);
            data["status_page"] = v;
        }
        None => {
            if let Some(o) = data.as_object_mut() {
                o.remove("status_page");
            }
        }
    }
    let act = active_code_id(&data);
    note_account_region(&mut data, act.as_deref(), new_loc.as_deref());
    store.save(&mut data)?;
    Ok(region_view(&data))
}

/// 根据后台缓存和本机设置算出的风险提示（不联网）：[{kind, level, text, advice}]。
/// 只提示，不拦任何操作；界面把 warn / bad 的常显在顶部，info 只在服务检查里列出。
pub fn risk_items(data: &Value, include_region: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut add = |kind: &str, level: &str, text: String, advice: &str| {
        out.push(json!({"kind": kind, "level": level, "text": text, "advice": advice}))
    };
    let now = chrono::Utc::now();
    let reg = data.get("region").cloned().unwrap_or_else(|| json!({}));
    let loc: Option<String> = if s(&reg, "checked_at").is_some() { s(&reg, "loc").map(String::from) } else { None };
    if include_region {
        if let Some(l) = loc.as_deref() {
            if region_status(Some(l)) == "unsupported" {
                add("region", "warn", region_message(Some(l), "unsupported"), region_advice("unsupported"));
            }
        }
    }
    if let (Some(prev), Some(l), Some(changed)) = (s(&reg, "prev_loc"), loc.as_deref(), s(&reg, "changed_at")) {
        if let Some(t) = parse_time(changed) {
            if now.signed_duration_since(t).num_seconds() < REGION_CHANGE_HOURS * 3600 {
                add(
                    "region_change",
                    "warn",
                    format!("出口地区变化：{} → {}（{}）", region_label(Some(prev)), region_label(Some(l)), fmt_ago(changed)),
                    "网络环境变了（出差、换网络、开关 VPN 或代理都会这样）。Anthropic 按使用者实际所在地判断是否支持，\
                     同一账号短时间内从多个地区使用也容易触发风控。确认现在人所在的地区在支持名单内再继续。",
                );
            }
        }
    }
    if let (Some(l), Some(m)) = (loc.as_deref(), s(&reg, "meta_loc")) {
        if m != l {
            add(
                "region_mismatch",
                "warn",
                format!("出口地区判定不一致：trace 接口看到 {}，meta 接口看到 {}", region_label(Some(l)), region_label(Some(m))),
                "多半是 IPv4 和 IPv6 走了不同出口，或代理只分流了一部分流量。这种情况下服务端看到的地区可能和你以为的不一样，\
                 先统一出口再判断。",
            );
        }
    }
    if hosting_exit(s(&reg, "org")) {
        add(
            "isp",
            "warn",
            format!(
                "出口 IP 属于 {}（AS{}），像云服务商 / 机房，不是家庭或公司宽带",
                s(&reg, "org").unwrap_or(""),
                reg.get("asn").map(|v| v.to_string()).unwrap_or_else(|| "?".into())
            ),
            "这类出口通常被当作 VPN / 代理流量看待，是常见的风控信号之一；人所在地仍按实际位置判断。",
        );
    }
    if reg.get("warp").and_then(|v| v.as_bool()).unwrap_or(false) {
        add(
            "warp",
            "info",
            "出口经过 Cloudflare WARP / Zero Trust".into(),
            "上面的出口地区是 WARP 的出口，不是本机网络的真实出口；Anthropic 按使用者所在地判断。",
        );
    }
    if let Some(skew) = reg.get("skew").and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))) {
        if skew.abs() > CLOCK_SKEW_WARN {
            add(
                "clock",
                "warn",
                format!("本机时钟与服务器相差 {:+} 秒", skew),
                "时钟偏差大会让登录续期和 HTTPS 校验失败，也是常见的风控信号。打开系统的自动校时。",
            );
        }
    }
    if let Some(sp) = data.get("status_page") {
        if let Some(ind) = s(sp, "indicator").filter(|i| *i != "none") {
            let level = if ind == "major" || ind == "critical" { "bad" } else { "warn" };
            let desc = s(sp, "description").unwrap_or(ind);
            add(
                "status",
                level,
                format!("Anthropic 官方状态页：{}（{}）", desc, ind),
                "这是 Anthropic 侧的故障，不是本机或账号的问题；等恢复后再试，详情见 status.anthropic.com。",
            );
        }
    }
    if let (Some((zone, cc)), Some(l)) = (system_timezone(), loc.as_deref()) {
        if !cc.is_empty() && !cc.iter().any(|c| c == l) {
            let home = cc[0].as_str();
            let text = format!("系统时区 {} 指向 {}，与出口地区 {} 不一致", zone, region_label(Some(home)), region_label(Some(l)));
            if region_status(Some(home)) == "unsupported" {
                add(
                    "timezone",
                    "warn",
                    text,
                    "出口在支持地区，但本机设置显示人可能在不支持地区。Anthropic 按使用者实际所在地判断，不按出口 IP；\
                     人确实在不支持地区的话，风险与出口无关。",
                );
            } else {
                add("timezone", "info", text, "出差或经过代理都会这样；人所在地在支持名单内就没问题。");
            }
        }
    }
    if let (Some((name, Some(home))), Some(l)) = (system_locale(), loc.as_deref()) {
        if home != l {
            let text = format!("系统语言 / 区域 {} 指向 {}，与出口地区 {} 不一致", name, region_label(Some(&home)), region_label(Some(l)));
            if region_status(Some(&home)) == "unsupported" {
                add(
                    "locale",
                    "warn",
                    text,
                    "系统语言属于 Anthropic 会收集的设备环境信息。出口在支持地区但系统区域指向不支持地区，\
                     说明人可能在不支持地区；政策按实际所在地判断。",
                );
            } else {
                add("locale", "info", text, "用外语系统或出差都会这样；人所在地在支持名单内就没问题。");
            }
        }
    }
    let act = active_code_id(data);
    if let Some(acc) = accounts(data).iter().find(|a| s(a, "id") == act.as_deref()) {
        let cutoff = now - chrono::Duration::days(ACCOUNT_REGION_DAYS);
        let locs: Vec<String> = acc
            .get("regions")
            .and_then(|r| r.as_array())
            .map(|rs| {
                rs.iter()
                    .filter(|r| s(r, "at").and_then(parse_time).map(|t| t >= cutoff).unwrap_or(false))
                    .filter_map(|r| s(r, "loc").map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if locs.len() > 1 {
            add(
                "account_regions",
                "warn",
                format!(
                    "当前账号“{}”{} 天内从 {} 个地区使用过：{}",
                    s(acc, "label").unwrap_or(""),
                    ACCOUNT_REGION_DAYS,
                    locs.len(),
                    locs.iter().map(|l| region_label(Some(l))).collect::<Vec<_>>().join("、")
                ),
                "同一账号短时间在多个地区使用（出差、VPN、多人共用）是常见的风控触发点，可能被要求重新验证或限制使用。",
            );
        }
    }
    let n = accounts(data).iter().filter(|a| s(a, "kind") == Some("claude_code")).count();
    if n >= 2 {
        add(
            "accounts",
            "info",
            format!("本机保存了 {} 个 Claude Code 账号", n),
            "Anthropic 账号仅限本人使用，多人共用同一账号违反使用条款。",
        );
    }
    out
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

/// 系统版本号，查不到时为空
fn os_release() -> String {
    if cfg!(target_os = "macos") {
        run_text("sw_vers", &["-productVersion"]).trim().to_string()
    } else if cfg!(windows) {
        run_text("cmd", &["/c", "ver"]).trim().to_string()
    } else {
        std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default()
    }
}

fn or_dash(msg: &str) -> &str {
    if msg.is_empty() {
        "-"
    } else {
        msg
    }
}

/// 检测 Claude 相关服务和使用环境：DNS、出口地区、api.anthropic.com、claude.ai、代理、VPN，
/// 再加上 risk_items 的各项（出口变化、时钟、官方状态页、时区、账号地区）。
/// 返回 [{kind, level, text, advice}]，级别为 ok / bad / warn / info，第一项是总体结论。不包含任何密钥。
/// 会顺便刷新界面常显的缓存和账号的地区记录。
pub fn service_check(store: &Store) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut add = |kind: &str, level: &str, text: String, advice: &str| {
        out.push(json!({"kind": kind, "level": level, "text": text, "advice": advice}))
    };
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
                add("dns", "ok", format!("DNS：{} → {}", host, ips.iter().take(3).cloned().collect::<Vec<_>>().join(", ")), "");
            }
            Err(e) => add(
                "dns",
                "bad",
                format!("DNS：{} 解析失败（{}）", host, e),
                if proxies.is_empty() { "检查本机 DNS 或代理设置" } else { "检查本机 DNS 或代理设置；设了代理的话通常由代理解析，可忽略" },
            ),
        }
    }
    // 出口地区：claude.ai 在 Cloudflare 后面，/cdn-cgi/trace 会返回它看到的访问者信息。
    // 顺便更新界面常显的缓存和账号的地区记录。
    let _ = check_region(store, 0);
    let data = store.load().unwrap_or_else(|_| json!({}));
    let r = data.get("region").cloned().unwrap_or_else(|| json!({}));
    match s(&r, "loc") {
        Some(l) => {
            let status = region_status(Some(l));
            add(
                "region",
                if status == "supported" { "ok" } else { "warn" },
                format!(
                    "{}（IP {}{}，Cloudflare 节点 {}，{} ms）",
                    region_message(Some(l), status),
                    s(&r, "ip").unwrap_or("?"),
                    s(&r, "city").map(|c| format!("，{}", c)).unwrap_or_default(),
                    s(&r, "colo").unwrap_or("?"),
                    r.get("ms").and_then(|v| v.as_u64()).unwrap_or(0)
                ),
                region_advice(status),
            );
        }
        None => add("region", "warn", "出口地区：查不到（连不上 Cloudflare 的 trace 接口）".into(), region_advice("unknown")),
    }
    match s(&r, "org") {
        Some(org) if !hosting_exit(Some(org)) => add(
            "isp",
            "info",
            format!("网络服务商：{}（AS{}）", org, r.get("asn").map(|v| v.to_string()).unwrap_or_else(|| "?".into())),
            "看起来是普通宽带或运营商网络，不是机房出口",
        ),
        Some(_) => {} // 机房出口由 risk_items 以 warn 给出
        None => add("isp", "info", "网络服务商：查不到（连不上 Cloudflare 的 meta 接口）".into(), "判断不了出口是不是机房；不影响其它检查"),
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
        add("api", "bad", format!("{}：连不上（{}）", api_host, msg), "检查网络或代理；公司网络可能屏蔽了这个域名");
    } else if st == 403 || region_denied(&msg) {
        add("api", "bad", format!("{}：拒绝访问（HTTP {}：{}）", api_host, st, or_dash(&msg)), region_hint);
    } else if at.is_some() && st == 200 {
        add("api", "ok", format!("{}：服务正常，当前登录可用（{} ms）", api_host, ms), "");
    } else if at.is_some() && st == 401 {
        add("api", "warn", format!("{}：服务可达，但当前登录已过期或无效（{} ms）", api_host, ms), "运行一次 claude 会自动续登录，不影响切换账号");
    } else if st == 401 {
        add("api", "ok", format!("{}：服务可达（未登录，{} ms）", api_host, ms), "");
    } else if st >= 500 {
        add("api", "bad", format!("{}：服务端错误（HTTP {}）", api_host, st), "可能是 Anthropic 故障，稍后再试，可查看 status.anthropic.com");
    } else {
        add("api", "warn", format!("{}：HTTP {}（{}，{} ms）", api_host, st, or_dash(&msg), ms), "少见的返回，稍后重试；持续出现请把这一行发给作者");
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
        add("web", "bad", format!("{}：连不上（{}）", web_host, msg), "检查网络或代理");
    } else if is_cloudflare(&msg) {
        add(
            "web",
            "warn",
            format!("{}：返回人机验证页（{} ms）", web_host, ms),
            "公司网络或代理触发了 Cloudflare 人机验证；只影响网页账号读取额度，不影响 Claude Code 切换",
        );
    } else if region_denied(&msg) {
        add("web", "bad", format!("{}：拒绝访问（HTTP {}：{}）", web_host, code, msg), region_hint);
    } else if code == 200 || code == 401 || code == 403 {
        add("web", "ok", format!("{}：可达（{} ms）", web_host, ms), "");
    } else if code >= 500 {
        add("web", "bad", format!("{}：服务端错误（HTTP {}）", web_host, code), "稍后再试");
    } else {
        add("web", "warn", format!("{}：HTTP {}（{}，{} ms）", web_host, code, or_dash(&msg), ms), "少见的返回，稍后重试");
    }
    // 代理：环境变量 + 系统设置；VPN：默认路由
    let mut parts: Vec<String> = proxies.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    parts.extend(system_proxy().into_iter().map(|p| format!("系统设置 {}", p)));
    add(
        "proxy",
        "info",
        format!("代理：{}", if parts.is_empty() { "无（直连）".to_string() } else { parts.join("；") }),
        if parts.is_empty() { "直连，出口地区就是本机网络的出口" } else { "请求经过代理，上面的出口地区是代理的出口，不是本机网络的出口" },
    );
    match default_route_tunnel() {
        Some((name, kind)) => add(
            "vpn",
            "info",
            format!("VPN：默认路由走 {}（{}）", name, kind),
            "出口地区是 VPN 的出口，不代表本机所在地；Anthropic 按使用者实际所在地判断是否支持。",
        ),
        None => add("vpn", "ok", "VPN：默认路由没有经过隧道接口".into(), "只看本机默认路由；透明代理和路由器上的 VPN 看不出来"),
    }
    drop(add);
    out.extend(risk_items(&data, false));
    out.push(json!({
        "kind": "device", "level": "info",
        "text": format!("设备：{} {}，{}", std::env::consts::OS, os_release(), std::env::consts::ARCH),
        "advice": "设备类型、系统、时区、语言这些设备环境信息 Anthropic 会收集；这里只是列出来，本工具不会改动或伪装任何设备标识。"
    }));
    out.push(json!({
        "kind": "scope", "level": "info",
        "text": "检测范围：本机能看到的信号（出口 IP 和运营商、代理 / VPN、WARP、时钟、时区、系统语言、账号使用地区）",
        "advice": "按 Anthropic 官方说明，所在地是结合 IP 和其他信号综合判断的；账号侧本机看不到的信号不在检测范围内，\
                   所以“没有发现风险点”不等于一定合规，只表示本机能查到的都正常。"
    }));
    // 总体结论放最前面
    let first = |lv: &str| out.iter().find(|i| s(i, "level") == Some(lv)).cloned();
    let summary = if let Some(b) = first("bad") {
        json!({"kind": "summary", "level": "bad", "text": format!("总体：当前无法正常使用 —— {}", s(&b, "text").unwrap_or("")), "advice": b["advice"]})
    } else if let Some(w) = first("warn") {
        json!({"kind": "summary", "level": "warn", "text": format!("总体：可以使用，但有风险 —— {}", s(&w, "text").unwrap_or("")), "advice": w["advice"]})
    } else {
        json!({"kind": "summary", "level": "ok", "text": "总体：Claude 服务正常，出口地区在支持名单内，没有发现风险点", "advice": ""})
    };
    out.insert(0, summary);
    out
}
