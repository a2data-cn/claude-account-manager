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
/// IP 类型（家庭宽带 / 机房 / 代理 / 手机网络）：有 ipapi.is 的 key 用它的数据库；没有就用 ip-api.com 的免费接口（只有 http）
fn ipapi_is_url() -> String {
    std::env::var("CAM_IPAPI_IS_URL").unwrap_or_else(|_| "https://api.ipapi.is/".into())
}
/// 外部 IP 查询的三种设置。规则和 cam.py 的 IP_LOOKUP_MODES 一致
pub const IP_LOOKUP_MODES: &[(&str, &str)] = &[
    ("auto", "自动：ipapi.is → ipinfo.io → Cloudflare（都可以商用）"),
    ("ip-api", "自动，并优先用 ip-api.com（带机房 / 代理标记；仅限非商业用途，只有 http）"),
    ("off", "关闭：只查 claude.ai 自己的出口信息和 Anthropic 状态页，不查运营商和 IP 类型"),
];
/// 界面上的短标题和一句话说明，和 cam.py 的 IP_LOOKUP_TITLES 一致
pub const IP_LOOKUP_TITLES: &[(&str, &str, &str)] = &[
    ("auto", "自动（推荐）", "ipapi.is → ipinfo.io → Cloudflare，都可以商用"),
    ("ip-api", "加上 ip-api.com", "多一个带机房 / 代理标记的来源；仅限非商业用途，只有 http"),
    ("off", "关闭", "只看出口国家和官方状态，不查运营商和 IP 类型"),
];
/// ipapi.is 的 key、ipinfo 的 token 存在 secrets 目录，不进 accounts.json
pub const LOOKUP_SECRET_ID: &str = "_ip-lookup";
const META_FIELDS: &[&str] = &["asn", "org", "city", "meta_loc", "ip_type", "reg_loc", "meta_source", "meta_at"];
const STACK_FIELDS: &[&str] = &["v4_ip", "v4_loc", "v6_ip", "v6_loc"];

/// 出口 IP 没变时，运营商 / 类型 / 注册国多久重查一次（秒）
fn meta_ttl() -> i64 {
    std::env::var("CAM_META_TTL").ok().and_then(|v| v.parse().ok()).unwrap_or(86400)
}

#[derive(Clone, Debug)]
pub struct LookupConfig {
    pub mode: String,
    pub ipapi_key: String,
    pub ipinfo_token: String,
}

/// 外部 IP 查询的设置。环境变量 CAM_IP_LOOKUP / CAM_IPAPI_IS_KEY / CAM_IPINFO_TOKEN 优先于保存的设置。
pub fn ip_lookup_config(store: Option<&Store>) -> LookupConfig {
    let (mut mode, mut key, mut tok) = ("auto".to_string(), String::new(), String::new());
    if let Some(st) = store {
        if let Ok(d) = st.load() {
            if let Some(m) = d.get("prefs").and_then(|p| s(p, "ip_lookup")) {
                mode = m.to_string();
            }
        }
        let sec = st.load_secret(LOOKUP_SECRET_ID);
        key = s(&sec, "ipapi_key").unwrap_or("").to_string();
        tok = s(&sec, "ipinfo_token").unwrap_or("").to_string();
    }
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(m) = env("CAM_IP_LOOKUP") {
        mode = m;
    }
    if !IP_LOOKUP_MODES.iter().any(|(m, _)| *m == mode) {
        mode = "auto".into();
    }
    LookupConfig { mode, ipapi_key: env("CAM_IPAPI_IS_KEY").unwrap_or(key), ipinfo_token: env("CAM_IPINFO_TOKEN").unwrap_or(tok) }
}

fn with_param(url: &str, name: &str, value: &str) -> String {
    if value.is_empty() {
        return url.to_string();
    }
    let enc: String = value
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{:02X}", b) })
        .collect();
    format!("{}{}{}={}", url, if url.contains('?') { "&" } else { "?" }, name, enc)
}

/// 按顺序要问的运营商数据源 [(名字, 地址)]；off 时为空。
pub fn meta_sources(cfg: &LookupConfig) -> Vec<(&'static str, String)> {
    if cfg.mode == "off" {
        return Vec::new();
    }
    let mut out = Vec::new();
    if cfg.mode == "ip-api" {
        out.push(("ip-api.com", ipapi_url()));
    }
    out.push(("ipapi.is", with_param(&ipapi_is_url(), "key", &cfg.ipapi_key)));
    out.push(("ipinfo.io", with_param(&ipinfo_url(), "token", &cfg.ipinfo_token)));
    out.push(("Cloudflare", cf_meta_url()));
    out
}
fn ipapi_url() -> String {
    std::env::var("CAM_IPAPI_URL").unwrap_or_else(|_| {
        "http://ip-api.com/json/?fields=status,countryCode,city,regionName,isp,org,as,mobile,proxy,hosting,query".into()
    })
}
/// IP 的注册国（原生 / 广播 IP）；空字符串表示不查
fn rdap_url() -> String {
    std::env::var("CAM_RDAP_URL").unwrap_or_else(|_| "https://rdap.org/ip/".into())
}
/// 只走 IPv4 / 只走 IPv6 的出口；空字符串表示不查
fn cf_trace4_url() -> String {
    std::env::var("CAM_CF_TRACE4_URL").unwrap_or_else(|_| "https://1.1.1.1/cdn-cgi/trace".into())
}
fn cf_trace6_url() -> String {
    std::env::var("CAM_CF_TRACE6_URL").unwrap_or_else(|_| "https://[2606:4700:4700::1111]/cdn-cgi/trace".into())
}

pub fn ip_type_name(t: &str) -> &'static str {
    match t {
        "residential" => "家庭 / 公司宽带",
        "hosting" => "机房 / 云服务器",
        "proxy" => "代理 / VPN 出口",
        "tor" => "Tor 出口节点",
        "mobile" => "手机网络",
        _ => "未知",
    }
}

/// 公共 DNS → (名称, 国家)。Google / Cloudflare / Quad9 这类任播的全球都有节点，国家记空，不拿来判断位置
const KNOWN_DNS: &[(&str, &str, &str)] = &[
    ("8.8.8.8", "Google", ""), ("8.8.4.4", "Google", ""), ("2001:4860:4860::8888", "Google", ""),
    ("2001:4860:4860::8844", "Google", ""), ("1.1.1.1", "Cloudflare", ""), ("1.0.0.1", "Cloudflare", ""),
    ("2606:4700:4700::1111", "Cloudflare", ""), ("2606:4700:4700::1001", "Cloudflare", ""), ("9.9.9.9", "Quad9", ""),
    ("149.112.112.112", "Quad9", ""), ("208.67.222.222", "OpenDNS", ""), ("208.67.220.220", "OpenDNS", ""),
    ("223.5.5.5", "阿里 DNS", "CN"), ("223.6.6.6", "阿里 DNS", "CN"), ("2400:3200::1", "阿里 DNS", "CN"),
    ("2400:3200:baba::1", "阿里 DNS", "CN"), ("119.29.29.29", "腾讯 DNSPod", "CN"), ("182.254.116.116", "腾讯 DNSPod", "CN"),
    ("114.114.114.114", "114DNS", "CN"), ("114.114.115.115", "114DNS", "CN"), ("180.76.76.76", "百度 DNS", "CN"),
    ("1.2.4.8", "CNNIC", "CN"), ("210.2.4.8", "CNNIC", "CN"), ("168.95.1.1", "中华电信 HiNet", "TW"),
    ("168.95.192.1", "中华电信 HiNet", "TW"), ("168.126.63.1", "KT", "KR"), ("168.126.63.2", "KT", "KR"),
    ("77.88.8.8", "Yandex", "RU"), ("77.88.8.1", "Yandex", "RU"),
];

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
pub fn parse_trace(txt: &str, t0: Instant) -> Value {
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
    json!({
        "loc": get("loc").map(|l| l.to_uppercase()), "ip": get("ip"), "colo": get("colo"),
        "ms": t0.elapsed().as_millis() as u64, "warp": warp, "skew": skew
    })
}

/// 从 Cloudflare 看出口地区（claude.ai 在它后面，它看到的就是 claude.ai 看到的），再并行查运营商 / 类型 / 注册国 /
/// IPv4 / IPv6 出口。出口 IP 和数据源都没变、且 meta_ttl 内查过时，运营商那几项直接沿用 prev，不再问第三方。
/// 设置为 off 时只查 claude.ai。与 cam.py 的 detect_region 一致。
pub fn detect_region_with(cfg: &LookupConfig, prev: &Value) -> Option<Value> {
    let off = cfg.mode == "off";
    let t0 = Instant::now();
    let (mut st, mut txt) = http_text(&format!("{}/cdn-cgi/trace", web_base()), 6);
    if (st != 200 || !txt.contains("loc=")) && !off {
        (st, txt) = http_text("https://www.cloudflare.com/cdn-cgi/trace", 6);
    }
    if st != 200 || !txt.contains("loc=") {
        return None;
    }
    let mut out = parse_trace(&txt, t0);
    for k in META_FIELDS.iter().chain(STACK_FIELDS) {
        out[*k] = Value::Null;
    }
    out["lookup_mode"] = json!(cfg.mode);
    if off {
        return Some(out);
    }
    let ip = s(&out, "ip").unwrap_or("").to_string();
    let fresh = s(prev, "meta_at")
        .and_then(parse_time)
        .map(|t| chrono::Utc::now().signed_duration_since(t).num_seconds() < meta_ttl())
        .unwrap_or(false);
    let reuse = s(prev, "ip") == Some(ip.as_str()) && s(prev, "lookup_mode") == Some(cfg.mode.as_str()) && fresh;
    let need_rdap = !reuse && cfg.ipapi_key.is_empty(); // 带 key 的 ipapi.is 自己会给注册国
    let (v4, v6, meta, rdap) = std::thread::scope(|sc| {
        let h4 = sc.spawn(|| trace_only(&cf_trace4_url()));
        let h6 = sc.spawn(|| trace_only(&cf_trace6_url()));
        let hm = (!reuse).then(|| sc.spawn(|| fetch_exit_meta(cfg)));
        let hr = need_rdap.then(|| sc.spawn(|| fetch_ip_registration(&ip)));
        (
            h4.join().ok().flatten(),
            h6.join().ok().flatten(),
            hm.and_then(|h| h.join().ok()).flatten(),
            hr.and_then(|h| h.join().ok()).flatten(),
        )
    });
    if reuse {
        for k in META_FIELDS {
            out[*k] = prev.get(*k).cloned().unwrap_or(Value::Null);
        }
    } else {
        if let Some(m) = meta {
            for k in META_FIELDS {
                if let Some(v) = m.get(*k) {
                    out[*k] = v.clone();
                }
            }
        }
        if s(&out, "reg_loc").is_none() {
            out["reg_loc"] = json!(if need_rdap { rdap } else { fetch_ip_registration(&ip) });
        }
        if s(&out, "org").is_some() || s(&out, "meta_loc").is_some() {
            out["meta_at"] = json!(now_iso()); // 查到了才缓存；全失败的话下次重查
        }
    }
    if let Some((ip4, loc)) = v4 {
        out["v4_ip"] = json!(ip4);
        out["v4_loc"] = json!(loc);
    }
    if let Some((ip6, loc)) = v6 {
        out["v6_ip"] = json!(ip6);
        out["v6_loc"] = json!(loc);
    }
    Some(out)
}

/// 用环境变量和默认设置检测一次（不读保存的设置）。
pub fn detect_region() -> Option<Value> {
    detect_region_with(&ip_lookup_config(None), &Value::Null)
}

/// 出口 IP 的 ASN、运营商、城市、国家和类型，按 meta_sources 的顺序问，第一个查到的为准。查不到返回 None。
pub fn fetch_exit_meta(cfg: &LookupConfig) -> Option<Value> {
    for (name, url) in meta_sources(cfg) {
        if url.is_empty() {
            continue;
        }
        let (st, txt) = http_text(&url, 5);
        if st != 200 {
            continue;
        }
        if let Some(mut m) = serde_json::from_str::<Value>(&txt).ok().and_then(|j| parse_exit_meta(&j)) {
            m["meta_source"] = json!(name);
            return Some(m);
        }
    }
    None
}

fn parse_as_field(v: &str, asn: &mut Option<u64>, org: &mut Option<String>) {
    let (head, rest) = v.split_once(' ').unwrap_or((v, ""));
    if let Some(n) = head.strip_prefix("AS").and_then(|n| n.parse::<u64>().ok()) {
        *asn = asn.or(Some(n));
        if org.is_none() {
            *org = Some(rest.trim().to_string()).filter(|r| !r.is_empty());
        }
    } else if org.is_none() {
        *org = Some(v.to_string());
    }
}

/// 统一几种格式：ipapi.is {asn:{asn,org,country}, company:{type}, is_datacenter, is_vpn, is_proxy, is_tor, is_mobile, location:{…}}、
/// ip-api.com {as, isp, org, countryCode, city, regionName, hosting, proxy, mobile}、
/// Cloudflare {asn, asOrganization, city, region, country}、ipinfo {org: "AS7922 Comcast…", city, region, country}。
/// 返回 {asn, org, city, meta_loc, ip_type, reg_loc}；ip_type 为 residential / hosting / proxy / tor / mobile / null（这个来源不带标记）。
pub fn parse_exit_meta(j: &Value) -> Option<Value> {
    j.as_object()?;
    if s(j, "status") == Some("fail") {
        return None;
    }
    let truthy = |k: &str| j.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let (mut asn, mut org, ip_type, reg_loc): (Option<u64>, Option<String>, Option<&str>, Option<String>);
    let loc_src = j.get("location").filter(|l| l.is_object()).unwrap_or(j);
    if let Some(a) = j.get("asn").filter(|a| a.is_object()) {
        // ipapi.is
        asn = a.get("asn").and_then(|v| v.as_u64());
        org = s(a, "org").or_else(|| j.get("company").and_then(|c| s(c, "name"))).map(String::from);
        reg_loc = s(a, "country").map(|c| c.trim().to_uppercase()).filter(|c| !c.is_empty());
        let ctype = j.get("company").and_then(|c| s(c, "type"));
        ip_type = if truthy("is_tor") {
            Some("tor")
        } else if truthy("is_vpn") || truthy("is_proxy") {
            Some("proxy")
        } else if truthy("is_datacenter") || ctype == Some("hosting") {
            Some("hosting")
        } else if truthy("is_mobile") {
            Some("mobile")
        } else if ctype.is_some() {
            Some("residential")
        } else {
            None
        };
    } else {
        asn = j.get("asn").and_then(|v| v.as_u64());
        org = s(j, "asOrganization").or_else(|| s(j, "isp")).map(String::from);
        if let Some(v) = s(j, "as").or_else(|| s(j, "asn")) {
            parse_as_field(v, &mut asn, &mut org);
        } else if org.is_none() {
            if let Some(v) = s(j, "org") {
                parse_as_field(v, &mut asn, &mut org);
            }
        }
        if org.is_none() {
            org = s(j, "company").map(String::from); // ipapi.is 不带 key 的简版
        }
        ip_type = if j.get("hosting").is_some() || j.get("proxy").is_some() {
            // ip-api.com
            Some(if truthy("proxy") {
                "proxy"
            } else if truthy("hosting") {
                "hosting"
            } else if truthy("mobile") {
                "mobile"
            } else {
                "residential"
            })
        } else {
            None
        };
        reg_loc = None;
    }
    let region = s(loc_src, "region").or_else(|| s(loc_src, "regionName")).or_else(|| s(loc_src, "state"));
    let city = [s(loc_src, "city"), region].iter().flatten().cloned().collect::<Vec<_>>().join(", ");
    let country = s(loc_src, "country_code").or_else(|| s(j, "countryCode")).or_else(|| s(loc_src, "country")).unwrap_or("");
    let loc = Some(country.trim().to_uppercase()).filter(|c| c.len() == 2);
    if org.is_none() && loc.is_none() {
        return None;
    }
    Some(json!({
        "asn": asn, "org": org, "city": if city.is_empty() { None } else { Some(city) }, "meta_loc": loc,
        "ip_type": ip_type, "reg_loc": reg_loc
    }))
}

/// IP 在 RIR 登记的国家（RDAP）。查不到返回 None（ARIN 的分配记录常常没写国家）。
pub fn fetch_ip_registration(ip: &str) -> Option<String> {
    let base = rdap_url();
    if base.is_empty() || ip.is_empty() {
        return None;
    }
    let (st, txt) = http_text(&format!("{}{}", base, ip), 6);
    if st != 200 {
        return None;
    }
    let j: Value = serde_json::from_str(&txt).ok()?;
    let c = s(&j, "country")?.trim().to_uppercase();
    if c.len() == 2 { Some(c) } else { None }
}

/// 只用某一个协议（IPv4 或 IPv6 的 IP 直连）访问 Cloudflare trace，返回 (ip, loc) 或 None（该协议不可用）。
pub fn trace_only(url: &str) -> Option<(Option<String>, Option<String>)> {
    if url.is_empty() {
        return None;
    }
    let (st, txt) = http_text(url, 4);
    if st != 200 || !txt.contains("loc=") {
        return None;
    }
    let get = |k: &str| {
        txt.lines()
            .find_map(|l| l.strip_prefix(&format!("{}=", k)))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some((get("ip"), get("loc").map(|l| l.to_uppercase())))
}

pub fn is_private_ip(ip: &str) -> bool {
    let ip = ip.split('%').next().unwrap_or(ip);
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => a.is_private() || a.is_loopback() || a.is_link_local(),
        Ok(std::net::IpAddr::V6(a)) => {
            a.is_loopback() || (a.segments()[0] & 0xfe00) == 0xfc00 || (a.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// 本机配置的 DNS 服务器 [{ip, name, country, private}]。只读本机配置，不往外发任何探测。
/// 测试用 CAM_TEST_DNS="8.8.8.8,223.5.5.5"（空字符串表示查不到）。
pub fn system_dns_servers() -> Vec<Value> {
    let mut ips: Vec<String> = Vec::new();
    if let Ok(t) = std::env::var("CAM_TEST_DNS") {
        ips = t.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    } else if cfg!(windows) {
        let out = run_text(
            "powershell",
            &["-NoProfile", "-Command", "Get-DnsClientServerAddress -ErrorAction SilentlyContinue | ForEach-Object { $_.ServerAddresses }"],
        );
        ips = out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    } else if cfg!(target_os = "macos") {
        for line in run_text("scutil", &["--dns"]).lines() {
            let line = line.trim();
            if line.starts_with("nameserver[") {
                if let Some((_, v)) = line.split_once(':') {
                    ips.push(v.trim().to_string());
                }
            }
        }
    } else {
        if let Ok(txt) = std::fs::read_to_string("/etc/resolv.conf") {
            for line in txt.lines() {
                let mut it = line.split_whitespace();
                if it.next() == Some("nameserver") {
                    if let Some(ip) = it.next() {
                        ips.push(ip.to_string());
                    }
                }
            }
        }
        if ips.iter().any(|ip| ip.starts_with("127.0.0.53")) {
            // systemd-resolved 的本机代理，真正的上游看 resolvectl
            for line in run_text("resolvectl", &["status"]).lines() {
                if line.contains("DNS Servers:") || line.contains("Current DNS Server:") {
                    if let Some((_, v)) = line.split_once(':') {
                        ips.extend(v.split_whitespace().map(String::from));
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in ips {
        let ip = raw.trim().split('%').next().unwrap_or("").to_string();
        if ip.is_empty() || ip.starts_with("127.0.0.53") || !seen.insert(ip.clone()) {
            continue;
        }
        let known = KNOWN_DNS.iter().find(|(k, _, _)| *k == ip);
        out.push(json!({
            "ip": ip, "name": known.map(|(_, n, _)| *n), "country": known.map(|(_, _, c)| *c).filter(|c| !c.is_empty()),
            "private": is_private_ip(&ip)
        }));
        if out.len() >= 6 {
            break;
        }
    }
    out
}

/// 本机的区域设置快照，和出口一起缓存，免得界面每次刷新都去跑系统命令。
pub fn local_env() -> Value {
    let tz = system_timezone();
    let lc = system_locale();
    let tun = default_route_tunnel();
    json!({
        "tz": tz.as_ref().map(|t| t.0.clone()), "tz_cc": tz.as_ref().map(|t| t.1.clone()).unwrap_or_default(),
        "locale": lc.as_ref().map(|l| l.0.clone()), "locale_cc": lc.as_ref().and_then(|l| l.1.clone()),
        "dns": system_dns_servers(), "proxy": proxy_parts(),
        "tunnel": tun.map(|(n, k)| json!([n, k]))
    })
}

const PROXY_ENV_KEYS: &[&str] = &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"];

/// 当前生效的代理：环境变量 + 系统设置。与 cam.py 的 proxy_parts 一致。
pub fn proxy_parts() -> Vec<String> {
    let mut parts: Vec<String> = PROXY_ENV_KEYS
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| format!("{}={}", k, v)))
        .collect();
    parts.extend(system_proxy().into_iter().map(|p| format!("系统设置 {}", p)));
    parts
}

/// 代理、VPN 两行（服务检测和顶部环境卡共用，扣分一致）。env 是 local_env() 的结果。
pub fn proxy_vpn_items(env: &Value) -> Vec<Value> {
    let parts: Vec<String> = env
        .get("proxy")
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mut out = vec![if parts.is_empty() {
        json!({"kind": "proxy", "level": "ok", "text": "代理：无（直连）", "advice": "直连，出口地区就是本机网络的出口"})
    } else {
        json!({"kind": "proxy", "level": "info", "text": format!("代理：{}", parts.join("；")),
               "advice": "请求经过代理，上面的出口地区是代理的出口，不是本机网络的出口"})
    }];
    let tun = env.get("tunnel").and_then(|t| t.as_array()).filter(|a| a.len() == 2);
    out.push(match tun {
        Some(t) => json!({"kind": "vpn", "level": "info",
                          "text": format!("VPN：默认路由走 {}（{}）", t[0].as_str().unwrap_or("?"), t[1].as_str().unwrap_or("")),
                          "advice": "出口地区是 VPN 的出口，不代表本机所在地；Anthropic 按使用者实际所在地判断是否支持。"}),
        None => json!({"kind": "vpn", "level": "ok", "text": "VPN：默认路由没有经过隧道接口",
                       "advice": "只看本机默认路由；透明代理和路由器上的 VPN 看不出来"}),
    });
    out
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
    run_with_timeout(c, std::time::Duration::from_secs(10))
}

/// 跑系统命令，最多等 timeout；超时就结束它，返回已经读到的输出。卡住的命令不会拖住整轮检测。
fn run_with_timeout(mut c: Command, timeout: std::time::Duration) -> String {
    use std::io::Read;
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let Ok(mut child) = c.spawn() else { return String::new() };
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned()
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
    // 测试用 CAM_TEST_PROXY="HTTPS 127.0.0.1:7890"（空字符串表示没有）
    if let Ok(t) = std::env::var("CAM_TEST_PROXY") {
        return t.split(',').filter(|x| !x.is_empty()).map(String::from).collect();
    }
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
                      "org": null, "city": null, "ip_type": null, "meta_source": null, "lookup_mode": null});
    };
    let loc = s(&c, "loc").map(String::from);
    let status = region_status(loc.as_deref());
    json!({
        "loc": loc, "ip": s(&c, "ip"), "status": status, "checked_at": checked,
        "message": region_message(loc.as_deref(), status), "advice": region_advice(status),
        "warp": c.get("warp").and_then(|v| v.as_bool()).unwrap_or(false),
        "prev_loc": s(&c, "prev_loc"), "changed_at": s(&c, "changed_at"),
        "org": s(&c, "org"), "city": s(&c, "city"), "ip_type": s(&c, "ip_type"),
        "meta_source": s(&c, "meta_source"), "lookup_mode": s(&c, "lookup_mode")
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
    let cfg = ip_lookup_config(Some(store));
    let prev = data.get("region").cloned().unwrap_or(Value::Null);
    // 出口、状态页、本机设置三件事并行，互不等待
    let (r, sp, env) = std::thread::scope(|sc| {
        let hr = sc.spawn(|| detect_region_with(&cfg, &prev));
        let hs = sc.spawn(fetch_status_page);
        let he = sc.spawn(local_env);
        (
            hr.join().ok().flatten().unwrap_or_else(|| json!({})),
            hs.join().ok().flatten(),
            he.join().unwrap_or(Value::Null),
        )
    });
    let _g = store.lock()?;
    let mut data = store.load()?;
    let old = data.get("region").cloned().unwrap_or_else(|| json!({}));
    let now = now_iso();
    let new_loc = s(&r, "loc").map(String::from);
    let mut reg = json!({
        "loc": new_loc, "ip": r.get("ip"), "colo": r.get("colo"), "ms": r.get("ms"),
        "warp": r.get("warp").and_then(|v| v.as_bool()).unwrap_or(false), "skew": r.get("skew"),
        "lookup_mode": cfg.mode, "env": env, "checked_at": now
    });
    for k in META_FIELDS.iter().chain(STACK_FIELDS) {
        reg[*k] = r.get(*k).cloned().unwrap_or(Value::Null);
    }
    // 中间失败过一次也和最后一次知道的出口比，出口变化不漏报（与 cam.py 一致）
    let last = s(&old, "loc").or_else(|| s(&old, "last_loc")).map(String::from);
    reg["last_loc"] = json!(new_loc.clone().or_else(|| last.clone()));
    match (last.as_deref(), new_loc.as_deref()) {
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
    if new_loc.is_some() {
        // 连得上 claude.ai 了：只清掉它能证明已经好了的（DNS、证书、claude.ai 连不上）
        let mut drop_all = false;
        if let Some(items) = data.get_mut("service_status").and_then(|ss| ss.get_mut("items")).and_then(|i| i.as_array_mut()) {
            items.retain(|i| !resolved_by_trace(i));
            drop_all = items.is_empty();
        }
        if drop_all {
            if let Some(o) = data.as_object_mut() {
                o.remove("service_status");
            }
        }
    }
    store.save(&mut data)?;
    Ok(region_view(&data))
}

/// 后台连上了 claude.ai 的 trace，就能证明这条服务检测问题已经好了：DNS、证书、claude.ai 连不上。
/// 人机验证、服务端错误、api.anthropic.com 的问题证明不了，留到下次服务检测或一小时后过期。与 cam.py 一致。
fn resolved_by_trace(item: &Value) -> bool {
    match s(item, "kind") {
        Some("dns" | "tls") => true,
        Some("web") => s(item, "text").map(|t| t.contains("连不上")).unwrap_or(false),
        _ => false,
    }
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
        } else if s(&reg, "checked_at").is_some() {
            add(
                "region_unknown",
                "warn",
                "出口地区查不到：上次检测连不上 claude.ai 和 Cloudflare".into(),
                region_advice("unknown"),
            );
        }
        // 代理 / VPN：和服务检测用同一份缓存，扣分一致
        if s(&reg, "checked_at").is_some() {
            if let Some(e) = reg.get("env").filter(|e| e.get("proxy").is_some()) {
                for it in proxy_vpn_items(e) {
                    if s(&it, "level") != Some("ok") {
                        add(
                            s(&it, "kind").unwrap_or(""),
                            s(&it, "level").unwrap_or("info"),
                            s(&it, "text").unwrap_or("").to_string(),
                            s(&it, "advice").unwrap_or(""),
                        );
                    }
                }
            }
        }
        // 最近一次服务检测发现的连接问题（后台检测到网络恢复后会清掉）
        if let Some(ss) = data.get("service_status") {
            let fresh = s(ss, "checked_at")
                .and_then(parse_time)
                .map(|t| now.signed_duration_since(t).num_seconds() < 3600)
                .unwrap_or(false);
            if fresh {
                for it in ss.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default() {
                    add(
                        s(&it, "kind").unwrap_or(""),
                        s(&it, "level").unwrap_or("warn"),
                        s(&it, "text").unwrap_or("").to_string(),
                        s(&it, "advice").unwrap_or(""),
                    );
                }
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
    let org = s(&reg, "org").unwrap_or("?");
    let asn = reg.get("asn").filter(|v| !v.is_null()).map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    match s(&reg, "ip_type") {
        Some("tor") => add(
            "tor_exit",
            "warn",
            format!("出口 IP 是 Tor 出口节点（{}）", org),
            "Tor 出口几乎一定会被当作高风险流量，账号很容易被限制；人所在地仍按实际位置判断。",
        ),
        Some("proxy") => add(
            "proxy_ip",
            "warn",
            format!("出口 IP 被数据库标记为代理 / VPN 出口（{}，AS{}）", org, asn),
            "服务端的 IP 库同样能查到这个标记，这是最常见的风控信号之一；人所在地仍按实际位置判断。",
        ),
        t if t == Some("hosting") || (t.is_none() && hosting_exit(s(&reg, "org"))) => add(
            "isp",
            "warn",
            format!("出口 IP 属于 {}（AS{}），是机房 / 云服务器，不是家庭或公司宽带", org, asn),
            "这类出口通常被当作 VPN / 代理流量看待，是常见的风控信号之一；人所在地仍按实际位置判断。",
        ),
        _ => {}
    }
    if let (Some(r), Some(l)) = (s(&reg, "reg_loc"), loc.as_deref()) {
        if r != l {
            add(
                "ip_origin",
                "info",
                format!("出口 IP 注册在 {}，定位在 {}（广播 IP）", region_label(Some(r)), region_label(Some(l))),
                "不同地理库可能把它判到不同国家，服务端看到的国家不一定和这里一致。",
            );
        }
    }
    if let (Some(v4), Some(v6)) = (s(&reg, "v4_loc"), s(&reg, "v6_loc")) {
        if v4 != v6 {
            add(
                "ipv6_mismatch",
                "warn",
                format!(
                    "IPv4 出口在 {}，IPv6 出口在 {}（{}），两个协议走了不同的出口",
                    region_label(Some(v4)),
                    region_label(Some(v6)),
                    s(&reg, "v6_ip").unwrap_or("?")
                ),
                "VPN / 代理通常只接管 IPv4，IPv6 直接从本机网络出去，服务端通过 IPv6 看到的就是本机网络的真实出口。\
                 如果 IPv6 这边在不支持地区，政策按实际所在地判断，风险与 IPv4 出口无关。",
            );
        }
    }
    // 旧版缓存没有这一段，就当作不知道；这里不现场跑系统命令
    let env: Option<Value> = reg.get("env").filter(|e| e.is_object()).cloned();
    if let (Some(e), Some(l)) = (&env, loc.as_deref()) {
        for d in e.get("dns").and_then(|d| d.as_array()).cloned().unwrap_or_default() {
            let Some(c) = s(&d, "country") else { continue };
            if c == l {
                continue;
            }
            let text = format!(
                "DNS 解析器 {}（{}，{}）与出口地区 {} 不一致",
                s(&d, "ip").unwrap_or("?"),
                s(&d, "name").unwrap_or("公共 DNS"),
                region_label(Some(c)),
                region_label(Some(l))
            );
            if region_status(Some(c)) == "unsupported" {
                add(
                    "dns_resolver",
                    "warn",
                    text,
                    "本机在用不支持地区的公共 DNS，说明人或网络可能在不支持地区；服务端也可能据此看出实际网络。政策按实际所在地判断。\
                     如果人确实已经在支持地区，多半是以前手动填的 DNS 没改回来：系统网络设置里把 DNS 改回自动获取（或 1.1.1.1 / 8.8.8.8 这类任播 DNS）。",
                );
            } else {
                add("dns_resolver", "info", text, "出差或代理分流都会这样；人所在地在支持名单内就没问题。");
            }
            break;
        }
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
    let tz_cached = env.as_ref().and_then(|e| {
        s(e, "tz").map(|z| {
            (
                z.to_string(),
                e.get("tz_cc")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
        })
    });
    if let (Some((zone, cc)), Some(l)) = (tz_cached, loc.as_deref()) {
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
    let lc_cached = env
        .as_ref()
        .and_then(|e| s(e, "locale").map(|n| (n.to_string(), s(e, "locale_cc").map(String::from))));
    if let (Some((name, Some(home))), Some(l)) = (lc_cached, loc.as_deref()) {
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
            "按 Anthropic 消费者条款，账号和登录信息不能给别人用或分享；一个人自己用多个号条款没有明确禁止。",
        );
    }
    out
}

/// 环境风险评分：满分 100，按 (kind, level) 扣分；没列出的 warn 扣 10、bad 扣 30。只是把提醒汇总成一个数，不拦任何操作。
const RISK_WEIGHTS: &[(&str, &str, i64)] = &[
    ("region", "warn", 60), ("region_unknown", "warn", 30), ("tls", "bad", 0), ("account_regions", "warn", 20), ("region_change", "warn", 15), ("isp", "warn", 20),
    ("region_mismatch", "warn", 15), ("timezone", "warn", 25), ("timezone", "info", 5), ("locale", "warn", 15),
    ("locale", "info", 3), ("clock", "warn", 10), ("warp", "info", 5), ("vpn", "info", 10), ("proxy", "info", 5),
    ("status", "bad", 30), ("status", "warn", 10), ("api", "bad", 40), ("api", "warn", 5), ("web", "bad", 10),
    ("web", "warn", 5), ("dns", "bad", 20), ("accounts", "info", 0), ("device", "info", 0), ("scope", "info", 0),
    ("tor_exit", "warn", 40), ("proxy_ip", "warn", 25), ("ipv6_mismatch", "warn", 15), ("dns_resolver", "warn", 15),
    ("dns_resolver", "info", 3), ("ip_origin", "info", 5), ("ipv6", "info", 0), ("dns_servers", "info", 0),
];
/// (最低分, 档位, 说明)
const RISK_GRADES: &[(i64, &str, &str)] = &[
    (90, "low", "风险低，可以使用"),
    (70, "medium", "有风险，建议先处理提醒项再使用"),
    (0, "high", "风险高，不建议在当前环境使用；是否继续由你决定"),
];
const RISK_SCORE_ADVICE: &str = "评分只汇总本机能看到的信号，满分也不代表一定合规；低于 90 时逐条看提醒和建议。";

/// 把风险项汇总成 {score, grade, label, advice}。只用于提示。
pub fn risk_score(items: &[Value]) -> Value {
    let mut score: i64 = 100;
    for it in items {
        let (kind, level) = (s(it, "kind").unwrap_or(""), s(it, "level").unwrap_or(""));
        if kind == "summary" {
            continue;
        }
        score -= match RISK_WEIGHTS.iter().find(|(k, l, _)| *k == kind && *l == level) {
            Some((_, _, w)) => *w,
            None => match level {
                "warn" => 10,
                "bad" => 30,
                _ => 0,
            },
        };
    }
    let score = score.max(0);
    let (_, grade, label) = RISK_GRADES.iter().find(|(floor, _, _)| score >= *floor).unwrap_or(&RISK_GRADES[2]);
    json!({"score": score, "grade": grade, "label": label, "advice": RISK_SCORE_ADVICE})
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
        None => add(
            "region_unknown",
            "warn",
            "出口地区：查不到（连不上 claude.ai 和 Cloudflare 的 trace 接口）".into(),
            region_advice("unknown"),
        ),
    }
    let asn = r.get("asn").filter(|v| !v.is_null()).map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    let src_note = s(&r, "meta_source").map(|m| format!("（数据：{}）", m)).unwrap_or_default();
    let off = s(&r, "lookup_mode") == Some("off");
    match (s(&r, "org"), s(&r, "ip_type")) {
        _ if off => add(
            "isp",
            "info",
            "网络服务商 / IP 类型：外部 IP 查询已关闭".into(),
            "出口地区照常检测；想看运营商、IP 类型、IPv6 和注册国，在设置里把“外部 IP 查询”改回自动（cam risk --ip-lookup auto）",
        ),
        (Some(org), Some(t)) if t == "residential" || t == "mobile" => add(
            "isp",
            "info",
            format!("网络服务商：{}（AS{}），IP 类型：{}{}", org, asn, ip_type_name(t), src_note),
            if t == "residential" { "数据库判定为普通宽带或运营商网络，不是机房 / 代理出口" } else { "手机网络的出口 IP 会变，地区判定也可能跳动" },
        ),
        (Some(org), None) if !hosting_exit(Some(org)) => add(
            "isp",
            "info",
            format!("网络服务商：{}（AS{}），IP 类型：数据源没给标记{}", org, asn, src_note),
            "按运营商名看不像机房。想要数据库判定：到 ipapi.is 免费注册一个 key（每天 1000 次，可商用），cam risk --ipapi-key 填进去",
        ),
        (Some(_), _) => {} // 机房 / 代理 / Tor 出口由 risk_items 以 warn 给出
        (None, _) => add("isp", "info", "网络服务商：查不到（IP 信息接口都连不上）".into(), "判断不了出口是不是机房 / 代理；不影响其它检查"),
    }
    match (s(&r, "reg_loc"), s(&r, "loc")) {
        _ if off => {}
        (Some(reg_loc), Some(l)) if reg_loc == l => {
            add("ip_origin", "ok", format!("IP 归属：注册在 {}，与定位一致（原生 IP）", region_label(Some(reg_loc))), "")
        }
        (None, Some(_)) => add(
            "ip_origin_unknown",
            "info",
            "IP 归属：注册国查不到（RDAP 记录没写国家）".into(),
            "美国 ARIN 的记录常常不写，不影响其它检查，不扣分",
        ),
        _ => {} // 不一致由 risk_items 给出
    }
    match (s(&r, "v4_loc"), s(&r, "v6_loc"), s(&r, "loc")) {
        _ if off => {}
        (Some(v4), Some(v6), _) if v4 == v6 => add("ipv6", "ok", format!("IPv6：可用，出口 {}，地区与 IPv4 一致", s(&r, "v6_ip").unwrap_or("?")), ""),
        (_, None, Some(_)) => add("ipv6", "ok", "IPv6：不可用，只有 IPv4 出口".into(), "不会出现 IPv6 漏出真实出口的问题"),
        _ => {}
    }
    if let Some(dns) = r.get("env").and_then(|e| e.get("dns")).and_then(|d| d.as_array()).filter(|d| !d.is_empty()) {
        let parts: Vec<String> = dns
            .iter()
            .map(|d| {
                let who = if d.get("private").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "局域网 / 本机".to_string()
                } else {
                    s(d, "name").unwrap_or("公共 DNS").to_string()
                };
                let c = s(d, "country").map(|c| format!("，{}", region_label(Some(c)))).unwrap_or_default();
                format!("{}（{}{}）", s(d, "ip").unwrap_or("?"), who, c)
            })
            .collect();
        add(
            "dns_servers",
            "info",
            format!("DNS 解析器：{}", parts.join("；")),
            "局域网 DNS 最终走路由器配置的上游；Google / Cloudflare 这类任播 DNS 全球有节点，不代表位置；某国公共 DNS 说明本机网络大概率在那个国家。\
             人在支持地区的话，用当地网络自动获取的 DNS 或任播 DNS 即可；出差前手动填过国内 DNS 的，到了支持地区记得在系统网络设置里改回自动获取。只读本机配置，没有做泄漏探测",
        );
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
    drop(add);
    let env = r.get("env").filter(|e| e.get("proxy").is_some()).cloned().unwrap_or_else(local_env);
    out.extend(proxy_vpn_items(&env));
    out.extend(risk_items(&data, false));
    out.push(json!({
        "kind": "device", "level": "info",
        "text": format!("设备：{} {}，{}", std::env::consts::OS, os_release(), std::env::consts::ARCH),
        "advice": "设备类型、系统、时区、语言这些设备环境信息 Anthropic 会收集；这里只是列出来，本工具不会改动或伪装任何设备标识。"
    }));
    out.push(json!({
        "kind": "scope", "level": "info",
        "text": "检测范围：本机能看到的信号（出口 IP、IP 类型、IPv4 / IPv6、注册国、运营商、代理 / VPN、WARP、DNS 配置、时钟、时区、系统语言、账号使用地区）",
        "advice": "按 Anthropic 官方说明，所在地是结合 IP 和其他信号综合判断的；账号侧本机看不到的信号不在检测范围内，\
                   所以“没有发现风险点”不等于一定合规，只表示本机能查到的都正常。"
    }));
    // 总体结论放最前面：评分 + 档位 + 最严重的一条
    let sc = risk_score(&out);
    let first = |lv: &str| out.iter().find(|i| s(i, "level") == Some(lv)).cloned();
    let (first_bad, first_warn) = (first("bad"), first("warn"));
    let worst = first_bad.clone().or_else(|| first_warn.clone());
    let grade = s(&sc, "grade").unwrap_or("");
    let level = if first_bad.is_some() || grade == "high" {
        "bad"
    } else if first_warn.is_some() || grade == "medium" {
        "warn"
    } else {
        "ok"
    };
    let head = format!("总体：{}/100，{}", sc["score"], s(&sc, "label").unwrap_or(""));
    let summary = match &worst {
        Some(w) => json!({"kind": "summary", "level": level, "score": sc["score"], "grade": grade,
                          "text": format!("{} —— {}", head, s(w, "text").unwrap_or("")), "advice": w["advice"]}),
        None => json!({"kind": "summary", "level": level, "score": sc["score"], "grade": grade,
                       "text": format!("{}；Claude 服务正常，出口地区在支持名单内，没有发现风险点", head), "advice": RISK_SCORE_ADVICE}),
    };
    out.insert(0, summary);
    // 连接问题记下来，界面顶部的环境卡和这里的分数保持一致
    let bad: Vec<Value> = out
        .iter()
        .filter(|i| matches!(s(i, "kind"), Some("api" | "web" | "dns" | "tls")) && matches!(s(i, "level"), Some("warn" | "bad")))
        .cloned()
        .collect();
    if let Ok(_g) = store.lock() {
        if let Ok(mut d) = store.load() {
            if bad.is_empty() {
                if let Some(o) = d.as_object_mut() {
                    o.remove("service_status");
                }
            } else {
                d["service_status"] = json!({"checked_at": now_iso(), "items": bad});
            }
            let _ = store.save(&mut d);
        }
    }
    out
}

/// 当前设置下会访问哪些外部服务、各发了什么。只发出口 IP，不发任何账号信息。与 cam.py 一致。
pub fn privacy_sources(cfg: &LookupConfig) -> Vec<Value> {
    let src = |name: &str, what: &str, group: &str, warn: bool| json!({"name": name, "what": what, "group": group, "warn": warn});
    let mut out = vec![
        src("claude.ai/cdn-cgi/trace", "出口国家、IP、时钟。访问 Claude 时它本来就看得到", "anthropic", false),
        src("status.anthropic.com", "官方状态页，不带任何本机信息", "anthropic", false),
    ];
    if cfg.mode == "off" {
        return out;
    }
    if cfg.mode == "ip-api" {
        out.push(src("ip-api.com（http）", "运营商、城市、机房 / 代理标记。免费接口仅限非商业用途", "third", true));
    }
    let ipapi = if cfg.ipapi_key.is_empty() { "运营商、城市（没配 key：每天 30 次，不带类型标记）" } else { "运营商、城市、机房 / VPN / Tor 标记" };
    out.push(src("api.ipapi.is", ipapi, "third", false));
    out.push(src("ipinfo.io", "上一家查不到时：运营商、城市", "third", false));
    out.push(src("speed.cloudflare.com", "再查不到时：运营商、城市", "third", false));
    out.push(src("rdap.org", "出口 IP 的注册国", "third", false));
    out.push(src("1.1.1.1 / Cloudflare IPv6", "分别测 IPv4、IPv6 出口", "third", false));
    out.push(src("www.cloudflare.com", "claude.ai 连不上时，代替它看出口国家", "third", false));
    out
}

pub fn privacy_view(store: &Store) -> R<Value> {
    let cfg = ip_lookup_config(Some(store));
    let data = store.load()?;
    let n = accounts(&data)
        .iter()
        .filter(|a| a.get("regions").and_then(|r| r.as_array()).map(|r| !r.is_empty()).unwrap_or(false))
        .count();
    let modes: Vec<Value> = IP_LOOKUP_MODES
        .iter()
        .map(|(v, l)| {
            let (_, title, desc) = IP_LOOKUP_TITLES.iter().find(|(k, _, _)| k == v).copied().unwrap_or((v, v, ""));
            json!({"value": v, "label": l, "title": title, "desc": desc})
        })
        .collect();
    let hours = (meta_ttl() as f64 / 3600.0 * 10.0).round() / 10.0;
    Ok(json!({
        "ip_lookup": cfg.mode, "modes": modes,
        "env_override": std::env::var("CAM_IP_LOOKUP").map(|v| !v.is_empty()).unwrap_or(false),
        "has_ipapi_key": !cfg.ipapi_key.is_empty(), "has_ipinfo_token": !cfg.ipinfo_token.is_empty(),
        "meta_ttl_hours": hours, "sources": privacy_sources(&cfg),
        "history": {"accounts": n, "checked_at": data.get("region").and_then(|r| s(r, "checked_at"))}
    }))
}

fn drop_meta(data: &mut Value) {
    if let Some(reg) = data.get_mut("region").and_then(|r| r.as_object_mut()) {
        for k in META_FIELDS.iter().chain(STACK_FIELDS) {
            reg.remove(*k);
        }
    }
}

/// 改外部 IP 查询的设置：{ip_lookup: auto / ip-api / off, ipapi_key, ipinfo_token}，key 传空字符串表示删除。
/// 改了数据源或 key，缓存的运营商信息作废，下次检测重新查。与 cam.py 的 save_privacy 一致。
pub fn save_privacy(store: &Store, body: &Value) -> R<Value> {
    let mode = s(body, "ip_lookup");
    if let Some(m) = mode {
        if !IP_LOOKUP_MODES.iter().any(|(v, _)| *v == m) {
            return Err("外部 IP 查询应为 auto / ip-api / off".into());
        }
    }
    {
        let _g = store.lock()?;
        let mut data = store.load()?;
        let mut changed = false;
        if let Some(m) = mode {
            let cur = data.get("prefs").and_then(|p| s(p, "ip_lookup")).unwrap_or("auto").to_string();
            if m != cur {
                if !data.get("prefs").map(|p| p.is_object()).unwrap_or(false) {
                    data["prefs"] = json!({});
                }
                data["prefs"]["ip_lookup"] = json!(m);
                changed = true;
            }
        }
        let mut sec = store.load_secret(LOOKUP_SECRET_ID);
        if !sec.is_object() {
            sec = json!({});
        }
        let mut sec_changed = false;
        for k in ["ipapi_key", "ipinfo_token"] {
            if let Some(v) = body.get(k).and_then(|v| v.as_str()) {
                let v = v.trim();
                if v != s(&sec, k).unwrap_or("") {
                    if v.is_empty() {
                        sec.as_object_mut().map(|o| o.remove(k));
                    } else {
                        sec[k] = json!(v);
                    }
                    sec_changed = true;
                }
            }
        }
        if sec_changed {
            store.save_secret(LOOKUP_SECRET_ID, &sec)?;
        }
        if changed || sec_changed {
            drop_meta(&mut data);
            store.save(&mut data)?;
        }
    }
    privacy_view(store)
}

/// 清掉缓存的出口信息和每个账号的使用地区记录。
pub fn clear_region_history(store: &Store) -> R<Value> {
    {
        let _g = store.lock()?;
        let mut data = store.load()?;
        if let Some(o) = data.as_object_mut() {
            o.remove("region");
            o.remove("status_page");
        }
        for a in accounts_mut(&mut data) {
            if let Some(o) = a.as_object_mut() {
                o.remove("regions");
            }
        }
        store.save(&mut data)?;
    }
    privacy_view(store)
}
