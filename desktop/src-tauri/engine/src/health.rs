//! 服务检查、出口地区与使用环境风险检测：DNS、出口地区、api.anthropic.com、claude.ai、续登录的地址、HTTPS 证书、代理、VPN，
//! 以及出口变化、时区、时钟偏差、官方状态页、账号多地区。与 cam.py 的同名函数一致。
//! 只检测和提示，不干预任何操作。

use super::claude::{oauth_block, read_live};
use super::store::*;
use super::usage::{
    api_base, get_oauth, get_with_in, http_json_timeout, http_text, http_text_in, token_urls,
    wait_of, web_base,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Anthropic 官方公布的支持国家/地区（https://www.anthropic.com/supported-countries，2026-10 抓取），
/// ISO 3166-1 alpha-2。名单会变，变了改这里。
pub const SUPPORTED_REGIONS: &[&str] = &[
    "AL", "DZ", "AD", "AO", "AG", "AR", "AM", "AU", "AT", "AZ", "BS", "BH", "BD", "BB", "BE", "BZ",
    "BJ", "BT", "BO", "BA", "BW", "BR", "BN", "BG", "BF", "BI", "CV", "KH", "CM", "CA", "CF", "TD",
    "CL", "CO", "KM", "CG", "CD", "CR", "CI", "HR", "CY", "CZ", "DK", "DJ", "DM", "DO", "EC", "EG",
    "SV", "GQ", "ER", "EE", "SZ", "ET", "FJ", "FI", "FR", "GA", "GM", "GE", "DE", "GH", "GR", "GD",
    "GT", "GN", "GW", "GY", "HT", "HN", "HU", "IS", "IN", "ID", "IQ", "IE", "IL", "IT", "JM", "JP",
    "JO", "KZ", "KE", "KI", "KW", "KG", "LA", "LV", "LB", "LS", "LR", "LY", "LI", "LT", "LU", "MG",
    "MW", "MY", "MV", "ML", "MT", "MH", "MR", "MU", "MX", "FM", "MD", "MC", "MN", "ME", "MA", "MZ",
    "NA", "NR", "NP", "NL", "NZ", "NI", "NE", "NG", "MK", "NO", "OM", "PK", "PW", "PS", "PA", "PG",
    "PY", "PE", "PH", "PL", "PT", "QA", "RO", "RW", "KN", "LC", "VC", "WS", "SM", "ST", "SA", "SN",
    "RS", "SC", "SL", "SG", "SK", "SI", "SO", "SB", "ZA", "KR", "SS", "ES", "LK", "SD", "SR", "SE",
    "CH", "TW", "TJ", "TZ", "TH", "TL", "TG", "TO", "TT", "TN", "TR", "TM", "TV", "UG", "UA", "AE",
    "GB", "US", "UY", "UZ", "VU", "VA", "VN", "ZM", "ZW",
];
/// 官方名单写的是国家；这些属地按所属国家看待（美国、英国、法国、荷兰、丹麦、芬兰、挪威、新西兰、澳大利亚）
pub const REGION_DEPENDENCIES: &[&str] = &[
    "PR", "GU", "VI", "AS", "MP", "GI", "IM", "JE", "GG", "BM", "KY", "FK", "VG", "RE", "GP", "MQ",
    "GF", "YT", "NC", "PF", "PM", "WF", "BL", "MF", "AW", "CW", "SX", "BQ", "GL", "FO", "AX", "SJ",
    "CK", "NU", "TK", "CX", "CC", "NF",
];
/// 给提示用的中文名，只收常见的
pub const REGION_NAMES: &[(&str, &str)] = &[
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
    ("T1", "Tor 出口"),
    ("US", "美国"),
    ("JP", "日本"),
    ("SG", "新加坡"),
    ("TW", "台湾"),
    ("KR", "韩国"),
    ("GB", "英国"),
    ("DE", "德国"),
    ("FR", "法国"),
    ("NL", "荷兰"),
    ("CA", "加拿大"),
    ("AU", "澳大利亚"),
    ("MY", "马来西亚"),
    ("TH", "泰国"),
    ("VN", "越南"),
    ("ID", "印尼"),
    ("PH", "菲律宾"),
    ("IN", "印度"),
    ("AE", "阿联酋"),
    ("TR", "土耳其"),
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
/// claude.ai 的 trace 这么久（秒）没回音，就同时问 Cloudflare 自己的，谁先答用谁的
pub const TRACE_HEDGE_SECS: u64 = 2;
/// 运营商 / 注册国 / IPv4 / IPv6 这几项第三方查询加起来最多等多久（秒），到点没答的当作查不到
pub const META_BUDGET_SECS: u64 = 8;
/// 其中每个运营商数据源最多等多久（秒）
pub const META_SOURCE_TIMEOUT: u64 = 4;
/// 一次完整的环境检测最多等多久（秒：出口 6 秒 + 第三方 + 一点余量）
pub const REGION_CHECK_BUDGET: u64 = 6 + META_BUDGET_SECS + 2;
/// 环境检测没连上时，最快多久（秒）以后重试；连续失败每次翻一倍
pub const REGION_RETRY_SECS: i64 = 60;
/// 发现本机换了网络就补测一次，最多这么久（秒）一次
pub const NET_RECHECK_SECS: i64 = 30;
/// 服务检测里解析一个域名最多等多久（秒）
pub const SERVICE_DNS_SECS: u64 = 5;
/// 服务检测里连 api.anthropic.com、claude.ai 各最多等多久（秒）
pub const SERVICE_PROBE_SECS: u64 = 10;
/// 服务检测里看本机设的代理在不在运行：连它的端口最多等多久（秒）。Windows 连一个没人听的本机端口要重试两秒左右才报“拒绝”
pub const PROXY_PROBE_SECS: u64 = 3;
/// 服务检测里看 api.anthropic.com 的证书是谁签的：每次连接最多等多久（秒）
pub const TLS_PROBE_SECS: u64 = 6;
/// 常见的公共证书机构（证书颁发者的组织名、通用名里有这些字）：api.anthropic.com、claude.ai 正常拿到的证书都是它们签的。
/// 不是它们签的，多半是网络里有设备在解密 HTTPS。与 cam.py 的 PUBLIC_CA_WORDS 一致（tests/shared/service.json）
const PUBLIC_CA_WORDS: &[&str] = &[
    "google trust services",
    "let's encrypt",
    "isrg",
    "digicert",
    "amazon",
    "sectigo",
    "comodo",
    "usertrust",
    "globalsign",
    "cloudflare",
    "microsoft",
    "entrust",
    "godaddy",
    "starfield",
    "zerossl",
    "ssl.com",
    "ssl corporation",
    "certainly",
    "identrust",
    "baltimore",
    "geotrust",
    "thawte",
    "rapidssl",
    "buypass",
    "harica",
    "actalis",
];
const TLS_INTERCEPT_ADVICE: &str = "多半是公司的安全网关、杀毒软件的“HTTPS 扫描 / 网页防护”，或者开着抓包工具。Claude Code 不认这个证书时会报 \
                                    SELF_SIGNED_CERT_IN_CHAIN 或 unable to get local issuer certificate：向网络管理员要这个根证书\
                                    （杀毒软件的话在它的设置里关掉 HTTPS 扫描），再把环境变量 NODE_EXTRA_CA_CERTS 设成证书文件的路径。";
const AUTH_PARTIAL_ADVICE: &str = "Claude Code 续登录用的正好是连不上的那个地址时就续不上，表现为突然要重新登录、cam 显示“需要登录”。\
                                   检查网络或代理有没有拦这个域名（公司网络、代理的规则都可能拦）。";
const AUTH_DOWN_ADVICE: &str = "登录大约每 8 小时要续一次，这些地址连不上就续不上：Claude Code 会要求重新登录，cam 显示“需要登录”。\
                                先检查网络或代理设置。";
const PROXY_DOWN_ADVICE: &str = "代理软件多半没开、已经退出，或者端口改了。设着代理但它没在运行时，走它的请求（Claude Code、cam）都会失败。\
                                 打开代理软件，或者把这个代理设置去掉（环境变量在启动终端的配置文件里删，系统代理在系统的网络设置里关）。";
const RATE_LIMIT_ADVICE: &str =
    "Anthropic 那边在限流（短时间请求太多），不是网络或账号的问题；过几分钟再试。";
const OVERLOAD_ADVICE: &str =
    "Anthropic 那边负载太高，不是网络或账号的问题；稍后再试，可查看 status.claude.com。";
const STATUS_ADVICE: &str =
    "这是 Anthropic 那边的故障，不是本机或账号的问题；等恢复后再试，详情见 status.claude.com。";
/// 服务检测发现的这些问题记进 data["service_status"]，界面顶部的环境卡也列出来。与 cam.py 一致
const SERVICE_STATUS_KINDS: &[&str] = &[
    "api",
    "web",
    "dns",
    "tls",
    "auth",
    "proxy_down",
    "tls_intercept",
];
/// 状态页里和这里有关的组件：Claude Code、API、claude.ai，再加上续登录用的 Console（platform.claude.com）。按词比，见 status_relevant
const STATUS_COMPONENT_WORDS: &[&str] = &[
    "claude code",
    "api",
    "claude.ai",
    "console",
    "api.anthropic.com",
    "platform.claude.com",
];
/// 默认路由走这些接口就当作 VPN / 隧道（macOS utun、Linux tun/wg、各家 VPN 客户端）
const TUNNEL_PREFIXES: &[&str] = &[
    "utun",
    "tun",
    "tap",
    "wg",
    "ppp",
    "ipsec",
    "tailscale",
    "zt",
    "nebula",
    "proton",
    "nordlynx",
    "mullvad",
    "clash",
    "mihomo",
    "sing",
    "hysteria",
    "surge",
];
const TUNNEL_WORDS: &[&str] = &[
    "tap",
    "tun",
    "wintun",
    "wireguard",
    "vpn",
    "tunnel",
    "nordlynx",
    "clash",
    "mihomo",
    "sing-box",
    "surge",
    "shadowsocks",
    "v2ray",
    "hysteria",
    "tailscale",
    "zerotier",
    "anyconnect",
    "fortinet",
    "globalprotect",
    "pulse secure",
    "openvpn",
    "expressvpn",
    "mullvad",
    "proton",
];

fn status_url() -> String {
    std::env::var("CAM_STATUS_URL")
        .unwrap_or_else(|_| "https://status.anthropic.com/api/v2/status.json".into())
}

/// 官方状态页的摘要（各组件和没解决的事件）：先 status.claude.com，连不上再 status.anthropic.com
fn status_summary_urls() -> Vec<String> {
    std::env::var("CAM_STATUS_SUMMARY_URLS")
        .unwrap_or_else(|_| {
            "https://status.claude.com/api/v2/summary.json,https://status.anthropic.com/api/v2/summary.json"
                .into()
        })
        .split(',')
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
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
    (
        "auto",
        "自动：ipapi.is → ipinfo.io → Cloudflare（都可以商用）",
    ),
    (
        "ip-api",
        "自动，并优先用 ip-api.com（带机房 / 代理标记；仅限非商业用途，只有 http）",
    ),
    (
        "off",
        "关闭：只查 claude.ai 自己的出口信息和 Anthropic 状态页，不查运营商和 IP 类型",
    ),
];
/// 界面上的短标题和一句话说明，和 cam.py 的 IP_LOOKUP_TITLES 一致
pub const IP_LOOKUP_TITLES: &[(&str, &str, &str)] = &[
    (
        "auto",
        "自动（推荐）",
        "ipapi.is → ipinfo.io → Cloudflare，都可以商用",
    ),
    (
        "ip-api",
        "加上 ip-api.com",
        "多一个带机房 / 代理标记的来源；仅限非商业用途，只有 http",
    ),
    (
        "off",
        "关闭",
        "只看出口国家和官方状态，不查运营商和 IP 类型",
    ),
];
/// ipapi.is 的 key、ipinfo 的 token 存在 secrets 目录，不进 accounts.json
pub const LOOKUP_SECRET_ID: &str = "_ip-lookup";
const META_FIELDS: &[&str] = &[
    "asn",
    "org",
    "city",
    "meta_loc",
    "ip_type",
    "reg_loc",
    "meta_source",
    "meta_at",
];
const STACK_FIELDS: &[&str] = &["v4_ip", "v4_loc", "v6_ip", "v6_loc"];

/// 出口 IP 没变时，运营商 / 类型 / 注册国多久重查一次（秒）
fn meta_ttl() -> i64 {
    std::env::var("CAM_META_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(86400)
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
    LookupConfig {
        mode,
        ipapi_key: env("CAM_IPAPI_IS_KEY").unwrap_or(key),
        ipinfo_token: env("CAM_IPINFO_TOKEN").unwrap_or(tok),
    }
}

fn with_param(url: &str, name: &str, value: &str) -> String {
    if value.is_empty() {
        return url.to_string();
    }
    let enc: String = value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect();
    format!(
        "{}{}{}={}",
        url,
        if url.contains('?') { "&" } else { "?" },
        name,
        enc
    )
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
    out.push((
        "ipapi.is",
        with_param(&ipapi_is_url(), "key", &cfg.ipapi_key),
    ));
    out.push((
        "ipinfo.io",
        with_param(&ipinfo_url(), "token", &cfg.ipinfo_token),
    ));
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
    std::env::var("CAM_CF_TRACE6_URL")
        .unwrap_or_else(|_| "https://[2606:4700:4700::1111]/cdn-cgi/trace".into())
}
/// claude.ai 的 trace 没回音时改问 Cloudflare 自己的（同一个出口，看到的一样）；空字符串表示不问
fn cf_trace_url() -> String {
    std::env::var("CAM_CF_TRACE_URL")
        .unwrap_or_else(|_| "https://www.cloudflare.com/cdn-cgi/trace".into())
}
// 下面三个时限平时就是上面的常数；测试里用环境变量（毫秒）改短
fn trace_hedge() -> Duration {
    wait_of("CAM_TEST_TRACE_HEDGE_MS", TRACE_HEDGE_SECS)
}
fn meta_budget() -> Duration {
    wait_of("CAM_TEST_META_BUDGET_MS", META_BUDGET_SECS)
}
fn meta_source_timeout() -> Duration {
    wait_of("CAM_TEST_META_SOURCE_MS", META_SOURCE_TIMEOUT)
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
    ("8.8.8.8", "Google", ""),
    ("8.8.4.4", "Google", ""),
    ("2001:4860:4860::8888", "Google", ""),
    ("2001:4860:4860::8844", "Google", ""),
    ("1.1.1.1", "Cloudflare", ""),
    ("1.0.0.1", "Cloudflare", ""),
    ("2606:4700:4700::1111", "Cloudflare", ""),
    ("2606:4700:4700::1001", "Cloudflare", ""),
    ("9.9.9.9", "Quad9", ""),
    ("149.112.112.112", "Quad9", ""),
    ("208.67.222.222", "OpenDNS", ""),
    ("208.67.220.220", "OpenDNS", ""),
    ("223.5.5.5", "阿里 DNS", "CN"),
    ("223.6.6.6", "阿里 DNS", "CN"),
    ("2400:3200::1", "阿里 DNS", "CN"),
    ("2400:3200:baba::1", "阿里 DNS", "CN"),
    ("119.29.29.29", "腾讯 DNSPod", "CN"),
    ("182.254.116.116", "腾讯 DNSPod", "CN"),
    ("114.114.114.114", "114DNS", "CN"),
    ("114.114.115.115", "114DNS", "CN"),
    ("180.76.76.76", "百度 DNS", "CN"),
    ("1.2.4.8", "CNNIC", "CN"),
    ("210.2.4.8", "CNNIC", "CN"),
    ("168.95.1.1", "中华电信 HiNet", "TW"),
    ("168.95.192.1", "中华电信 HiNet", "TW"),
    ("168.126.63.1", "KT", "KR"),
    ("168.126.63.2", "KT", "KR"),
    ("77.88.8.8", "Yandex", "RU"),
    ("77.88.8.1", "Yandex", "RU"),
];

/// 出口 IP 的运营商名里有这些就当作云服务商 / 机房（不是家庭或公司宽带）
const HOSTING_WORDS: &[&str] = &[
    "hosting",
    "cloud",
    "data center",
    "datacenter",
    "server",
    "vps",
    "dedicated",
    "colocation",
    "amazon",
    "aws",
    "google llc",
    "google cloud",
    "microsoft corporation",
    "azure",
    "digitalocean",
    "linode",
    "akamai",
    "vultr",
    "choopa",
    "ovh",
    "hetzner",
    "alibaba",
    "aliyun",
    "tencent",
    "huawei",
    "oracle",
    "m247",
    "datacamp",
    "leaseweb",
    "contabo",
    "scaleway",
    "upcloud",
    "hostinger",
    "psychz",
    "quadranet",
    "colocrossing",
    "zenlayer",
    "gcore",
    "g-core",
    "cloudflare",
    "packethub",
    "clouvider",
    "performive",
    "bandwagon",
    "racknerd",
    "ucloud",
    "kamatera",
    "ionos",
    "fly.io",
    "oneprovider",
    "hostpapa",
];

pub fn region_advice(status: &str) -> &'static str {
    match status {
        "unsupported" => "按 Anthropic 政策这个地区不提供服务，继续使用可能导致账号被限制或封禁。Anthropic 按使用者实际所在地判断，不按出口 IP：人在支持地区的话，多半是代理 / VPN 把出口带到了这里，查一下它的设置。",
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

/// 地区的中文名（界面上单独显示用），名单里没有的返回 None
pub fn region_name(loc: Option<&str>) -> Option<&'static str> {
    loc.and_then(|l| REGION_NAMES.iter().find(|(c, _)| *c == l).map(|(_, n)| *n))
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
        if status == "supported" {
            "在 Anthropic 支持名单内"
        } else {
            "不在 Anthropic 支持名单内"
        }
    )
}

pub fn fmt_ago(s: &str) -> String {
    let Some(d) = parse_time(s) else {
        return String::new();
    };
    let secs = chrono::Utc::now()
        .signed_duration_since(d)
        .num_seconds()
        .max(0);
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
    let skew = get("ts")
        .and_then(|t| t.parse::<f64>().ok())
        .map(|t| (now - t).round() as i64);
    let warp = matches!(get("warp").as_deref(), Some("on") | Some("plus"))
        || get("gateway").as_deref() == Some("on");
    json!({
        "loc": get("loc").map(|l| l.to_uppercase()), "ip": get("ip"), "colo": get("colo"),
        "ms": t0.elapsed().as_millis() as u64, "warp": warp, "skew": skew
    })
}

/// 在另一个线程里做一件事，结果用 wait_job 收。线程不 join（不用 thread::scope）：
/// 卡在连不上的网络里的那件（域名解析没有超时可设）不拖着调用方。对应 cam.py 的 run_parallel。
fn spawn_job<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f()); // 调用方等不及先走了也没关系
    });
    rx
}

/// 等 spawn_job 的那件事到 end 为止：到点还没做完（或者它自己崩了）是 None，不接着等；已经做完的过了点也收得到。
/// 同时等几件、时限又不一样的，先等时限最长的那件（它等完时别的多半也做完了），和 run_parallel 等到的一样多。
fn wait_job<T>(rx: &mpsc::Receiver<T>, end: Instant) -> Option<T> {
    rx.recv_timeout(end.saturating_duration_since(Instant::now()))
        .ok()
}

/// 出口信息（parse_trace 的结果）或 None。先问 claude.ai；它答不上来、或者 TRACE_HEDGE_SECS 秒还没回音，
/// 就同时问 Cloudflare 自己的 trace（fallback=false 时不问），谁先给出结果用谁的。总共最多等 timeout。
/// 与 cam.py 的 exit_trace 一致。
pub fn exit_trace(timeout: Duration, fallback: bool) -> Option<Value> {
    let (tx, rx) = mpsc::channel::<Option<Value>>();
    let ask = |url: String, wait: Duration| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let (st, txt) = http_text_in(&url, wait);
            let got = (st == 200 && txt.contains("loc=")).then(|| parse_trace(&txt, t0));
            let _ = tx.send(got);
        });
    };
    let start = Instant::now();
    ask(format!("{}/cdn-cgi/trace", web_base()), timeout);
    let spare_url = cf_trace_url();
    let (mut pending, mut spare) = (1, fallback && !spare_url.is_empty());
    loop {
        let end = start
            + if spare {
                trace_hedge().min(timeout)
            } else {
                timeout
            };
        match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok(got) => {
                pending -= 1;
                if got.is_some() {
                    return got;
                }
            }
            Err(_) if !spare => return None,
            Err(_) => {}
        }
        if spare {
            spare = false;
            let left = (start + timeout).saturating_duration_since(Instant::now());
            if left > Duration::from_millis(200) {
                ask(spare_url.clone(), left);
                pending += 1;
            }
        }
        if pending == 0 {
            return None;
        }
    }
}

/// 从 Cloudflare 看出口地区（claude.ai 在它后面，它看到的就是 claude.ai 看到的），再并行查运营商 / 类型 / 注册国 /
/// IPv4 / IPv6 出口。出口 IP 和数据源都没变、且 meta_ttl 内查过时，运营商那几项直接沿用 prev，不再问第三方。
/// 设置为 off 时只查 claude.ai。返回 None 是 claude.ai 和 Cloudflare 都没回音。
/// 最多等 6 + META_BUDGET_SECS 秒：第三方那几项到时限还没答的当作查不到，不拖着出口地区。
/// 与 cam.py 的 detect_region 一致。
pub fn detect_region_with(cfg: &LookupConfig, prev: &Value) -> Option<Value> {
    let off = cfg.mode == "off";
    let mut out = exit_trace(Duration::from_secs(6), !off)?;
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
    let reuse = !ip.is_empty()
        && s(prev, "ip") == Some(ip.as_str())
        && s(prev, "lookup_mode") == Some(cfg.mode.as_str())
        && fresh;
    let need_rdap = !reuse && cfg.ipapi_key.is_empty(); // 带 key 的 ipapi.is 自己会给注册国
    let budget = meta_budget();
    let t0 = Instant::now();
    let j4 = spawn_job(|| trace_only(&cf_trace4_url()));
    let j6 = spawn_job(|| trace_only(&cf_trace6_url()));
    let jm = (!reuse).then(|| {
        let cfg = cfg.clone();
        spawn_job(move || fetch_exit_meta(&cfg))
    });
    let jr = need_rdap.then(|| {
        let ip = ip.clone();
        spawn_job(move || fetch_ip_registration(&ip, Duration::from_secs(5)))
    });
    let end = t0 + budget;
    let v4 = wait_job(&j4, end).flatten();
    let v6 = wait_job(&j6, end).flatten();
    let meta = jm.and_then(|j| wait_job(&j, end)).flatten();
    let rdap = jr.and_then(|j| wait_job(&j, end)).flatten();
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
            out["reg_loc"] = json!(if need_rdap {
                rdap
            } else {
                // 带了 key 但 ipapi.is 这次没给注册国：时限里还有剩才补问 RDAP
                let left = budget.saturating_sub(t0.elapsed());
                if left >= Duration::from_secs(1) {
                    fetch_ip_registration(&ip, left.min(Duration::from_secs(5)))
                } else {
                    None
                }
            });
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
/// 每个数据源最多等 META_SOURCE_TIMEOUT 秒，加起来不超过 META_BUDGET_SECS 秒：前面的卡住了，后面的就少等或不问。
pub fn fetch_exit_meta(cfg: &LookupConfig) -> Option<Value> {
    let timeout = meta_source_timeout();
    let end = Instant::now() + meta_budget();
    for (name, url) in meta_sources(cfg) {
        if url.is_empty() {
            continue;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left < timeout.min(Duration::from_millis(500)) {
            break;
        }
        let (st, txt) = http_text_in(&url, timeout.min(left));
        if st != 200 {
            continue;
        }
        if let Some(mut m) = serde_json::from_str::<Value>(&txt)
            .ok()
            .and_then(|j| parse_exit_meta(&j))
        {
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
    let (mut asn, mut org, ip_type, reg_loc): (
        Option<u64>,
        Option<String>,
        Option<&str>,
        Option<String>,
    );
    let loc_src = j.get("location").filter(|l| l.is_object()).unwrap_or(j);
    if let Some(a) = j.get("asn").filter(|a| a.is_object()) {
        // ipapi.is
        asn = a.get("asn").and_then(|v| v.as_u64());
        org = s(a, "org")
            .or_else(|| j.get("company").and_then(|c| s(c, "name")))
            .map(String::from);
        reg_loc = s(a, "country")
            .map(|c| c.trim().to_uppercase())
            .filter(|c| !c.is_empty());
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
        org = s(j, "asOrganization")
            .or_else(|| s(j, "isp"))
            .map(String::from);
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
    let region = s(loc_src, "region")
        .or_else(|| s(loc_src, "regionName"))
        .or_else(|| s(loc_src, "state"));
    let city = [s(loc_src, "city"), region]
        .iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let country = s(loc_src, "country_code")
        .or_else(|| s(j, "countryCode"))
        .or_else(|| s(loc_src, "country"))
        .unwrap_or("");
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
pub fn fetch_ip_registration(ip: &str, timeout: Duration) -> Option<String> {
    let base = rdap_url();
    if base.is_empty() || ip.is_empty() {
        return None;
    }
    let (st, txt) = http_text_in(&format!("{}{}", base, ip), timeout);
    if st != 200 {
        return None;
    }
    let j: Value = serde_json::from_str(&txt).ok()?;
    let c = s(&j, "country")?.trim().to_uppercase();
    if c.len() == 2 {
        Some(c)
    } else {
        None
    }
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
            a.is_loopback()
                || (a.segments()[0] & 0xfe00) == 0xfc00
                || (a.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// 本机配置的 DNS 服务器 [{ip, name, country, private}]。只读本机配置，不往外发任何探测。
/// 测试用 CAM_TEST_DNS="8.8.8.8,223.5.5.5"（空字符串表示查不到）。
pub fn system_dns_servers() -> Vec<Value> {
    let mut ips: Vec<String> = Vec::new();
    if let Ok(t) = std::env::var("CAM_TEST_DNS") {
        ips = t
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
    } else if cfg!(windows) {
        ips = windows_env()["dns"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
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

const PROXY_ENV_KEYS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

/// 代理地址里的账号密码不露出来：http://user:pass@host:port → http://***@host:port。与 cam.py 的 mask_proxy 一致。
pub fn mask_proxy(v: &str) -> String {
    let (scheme, rest) = match v.find("://") {
        Some(i) => (&v[..i + 3], &v[i + 3..]),
        None => ("", v),
    };
    let auth = &rest[..rest.find('/').unwrap_or(rest.len())];
    match auth.rfind('@') {
        Some(at) => format!("{}***@{}", scheme, &auth[at + 1..]),
        None => v.to_string(),
    }
}

/// 当前生效的代理：环境变量 + 系统设置。与 cam.py 的 proxy_parts 一致，账号密码遮掉。
pub fn proxy_parts() -> Vec<String> {
    let mut parts: Vec<String> = PROXY_ENV_KEYS
        .iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| format!("{}={}", k, mask_proxy(&v)))
        })
        .collect();
    parts.extend(
        system_proxy()
            .into_iter()
            .map(|p| format!("系统设置 {}", mask_proxy(&p))),
    );
    parts
}

/// 代理、VPN 两行（服务检测和顶部环境卡共用，扣分一致）。env 是 local_env() 的结果。
pub fn proxy_vpn_items(env: &Value) -> Vec<Value> {
    let parts: Vec<String> = env
        .get("proxy")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut out = vec![if parts.is_empty() {
        json!({"kind": "proxy", "level": "ok", "text": "代理：未配置显式代理", "advice": "没有代理环境变量或系统代理设置；TUN、透明代理仍可能接管流量，不能据此判断为直连"})
    } else {
        json!({"kind": "proxy", "level": "info", "text": format!("代理：{}", parts.join("；")),
               "advice": "请求经过代理，上面的出口地区是代理的出口，不是本机网络的出口"})
    }];
    let tun = env
        .get("tunnel")
        .and_then(|t| t.as_array())
        .filter(|a| a.len() == 2);
    out.push(match tun {
        Some(t) => json!({"kind": "vpn", "level": "info",
                          "text": format!("VPN：默认路由走 {}（{}）", t[0].as_str().unwrap_or("?"), t[1].as_str().unwrap_or("")),
                          "advice": "出口地区是 VPN 的出口，不代表本机所在地；Anthropic 按使用者实际所在地判断是否支持。"}),
        None => json!({"kind": "vpn", "level": "ok", "text": "VPN / TUN：默认路由未检出隧道",
                       "advice": "只检查默认路由；TUN 可以通过更具体的路由接管流量，未检出不代表 TUN 未开启。透明代理和路由器上的 VPN 也可能无法识别"}),
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
        run_text("defaults", &["read", "-g", "AppleLocale"])
            .trim()
            .to_string()
    } else if cfg!(windows) {
        windows_env()["culture"].as_str().unwrap_or("").to_string()
    } else {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default()
    };
    let name = name
        .split('.')
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
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

/// 有关的组件出了这些状态就提醒：部分 / 严重故障是“要处理”，性能下降、维护中是“留意”
fn component_level(status: &str) -> Option<&'static str> {
    match status {
        "partial_outage" | "major_outage" => Some("bad"),
        "degraded_performance" | "under_maintenance" => Some("warn"),
        _ => None,
    }
}

fn component_status_name(status: &str) -> &str {
    match status {
        "degraded_performance" => "性能下降",
        "partial_outage" => "部分故障",
        "major_outage" => "严重故障",
        "under_maintenance" => "维护中",
        other => other,
    }
}

fn incident_status_name(status: &str) -> &str {
    match status {
        "investigating" => "调查中",
        "identified" => "已找到原因",
        "monitoring" => "已修复，观察中",
        "scheduled" => "已安排",
        "in_progress" => "进行中",
        "verifying" => "验证中",
        other => other,
    }
}

/// 状态页上的组件 / 事件名和这里有关（Claude Code、API、claude.ai、Console）。按词比，“rapid” 里的 api 不算。
/// 与 cam.py 的 status_relevant 一致（tests/shared/status_page.json）。
pub fn status_relevant(name: &str) -> bool {
    let low: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let words: Vec<&str> = low
        .split_whitespace()
        .map(|w| w.trim_matches('.'))
        .filter(|w| !w.is_empty())
        .collect();
    let text = format!(" {} ", words.join(" "));
    STATUS_COMPONENT_WORDS
        .iter()
        .any(|w| text.contains(&format!(" {} ", w)))
}

/// Statuspage 的 summary.json → {indicator, description, incidents: [{name, status}], components: [{name, status}]}。
/// 只留和这里有关（status_relevant）的：不正常的组件，没解决的事件（名字有关，或者影响到的组件里有有关的）。
/// indicator / description 是整个状态页的。格式不对返回 None。与 cam.py 的 parse_status_summary 一致（tests/shared/status_page.json）。
pub fn parse_status_summary(j: &Value) -> Option<Value> {
    let st = j.get("status").filter(|v| v.is_object())?;
    let ind = s(st, "indicator").filter(|i| !i.is_empty())?;
    let incidents: Vec<Value> = j
        .get("incidents")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|i| i.is_object())
                .filter(|i| !matches!(s(i, "status"), Some("resolved" | "postmortem")))
                .filter(|i| {
                    status_relevant(s(i, "name").unwrap_or(""))
                        || i.get("components")
                            .and_then(|c| c.as_array())
                            .is_some_and(|cs| {
                                cs.iter()
                                    .any(|c| status_relevant(s(c, "name").unwrap_or("")))
                            })
                })
                .map(|i| {
                    json!({"name": s(i, "name").filter(|n| !n.is_empty()).unwrap_or("?"),
                           "status": s(i, "status").unwrap_or("")})
                })
                .collect()
        })
        .unwrap_or_default();
    let components: Vec<Value> = j
        .get("components")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let (name, st) = (
                        s(c, "name").filter(|n| !n.is_empty())?,
                        s(c, "status").filter(|x| !x.is_empty())?,
                    );
                    (st != "operational" && status_relevant(name))
                        .then(|| json!({"name": name, "status": st}))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(
        json!({"indicator": ind, "description": s(st, "description").unwrap_or(""),
                "incidents": incidents, "components": components}),
    )
}

/// 官方状态页这一条提醒：(级别, 文字) 或 None。sp 是 data["status_page"]。
/// 有 components（来自 summary.json）：只看和这里有关的组件和事件，别的组件出故障不提醒；有关的组件部分 / 严重故障是 bad，
/// 性能下降、维护中，或者只有没解决的事件是 warn。没有 components（status.json 的总体状态，或者旧版本记的）：按总体 indicator 算。
/// 与 cam.py 的 status_risk 一致（tests/shared/status_page.json）。
pub fn status_risk(sp: &Value) -> Option<(&'static str, String)> {
    if !sp.is_object() {
        return None;
    }
    if sp.get("components").is_none() {
        let ind = s(sp, "indicator").filter(|i| !i.is_empty() && *i != "none")?;
        let desc = s(sp, "description")
            .filter(|d| !d.is_empty())
            .unwrap_or(ind);
        let level = if ind == "major" || ind == "critical" {
            "bad"
        } else {
            "warn"
        };
        return Some((level, format!("Anthropic 官方状态页：{}（{}）", desc, ind)));
    }
    let comps: Vec<&Value> = sp["components"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|c| s(c, "status").and_then(component_level).is_some())
                .collect()
        })
        .unwrap_or_default();
    let incs: Vec<Value> = sp
        .get("incidents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if comps.is_empty() && incs.is_empty() {
        return None;
    }
    let level = if comps
        .iter()
        .any(|c| s(c, "status").and_then(component_level) == Some("bad"))
    {
        "bad"
    } else {
        "warn"
    };
    let mut parts: Vec<String> = comps
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                s(c, "name").unwrap_or(""),
                component_status_name(s(c, "status").unwrap_or(""))
            )
        })
        .collect();
    if let Some(i) = incs.first() {
        parts.push(format!(
            "{}（{}）",
            s(i, "name").unwrap_or(""),
            incident_status_name(s(i, "status").unwrap_or(""))
        ));
    }
    Some((level, format!("Anthropic 官方状态页：{}", parts.join("；"))))
}

/// 官方状态页，按 data["status_page"] 存的样子：先要 summary.json（只留和这里有关的组件和事件，见 parse_status_summary），
/// 连不上再要 status.json 的总体状态 {indicator, description}（这种没有 components，status_risk 按总体算）。
/// 都连不上返回 None。总共最多等 6 秒。与 cam.py 的 fetch_status_page 一致。
pub fn fetch_status_page() -> Option<Value> {
    let end = Instant::now() + Duration::from_secs(6);
    for url in status_summary_urls() {
        let left = end.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(500) {
            break;
        }
        let (st, txt) = http_text_in(&url, left);
        if st != 200 {
            continue;
        }
        let parsed = serde_json::from_str::<Value>(&txt)
            .ok()
            .and_then(|j| parse_status_summary(&j));
        if let Some(mut v) = parsed {
            v["page"] = json!(host_of(&url));
            return Some(v);
        }
    }
    let left = end.saturating_duration_since(Instant::now());
    if left < Duration::from_millis(500) {
        return None;
    }
    let (st, txt) = http_text_in(&status_url(), left);
    if st != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&txt).ok()?;
    let status = v.get("status")?;
    let ind = s(status, "indicator")?;
    Some(json!({"indicator": ind, "description": s(status, "description").unwrap_or("")}))
}

// ---- Windows 上查进程、查系统设置都要起一次 PowerShell（每次大约半秒 CPU）：几处要用的合并成一次，结果几秒内共用。
// 与 cam.py 的 windows_env / windows_processes 一致（解析规则见 tests/shared/windows_env.json）。

const WIN_ENV_PS: &str = "$ErrorActionPreference='SilentlyContinue'; \
    $dns = @(Get-DnsClientServerAddress | ForEach-Object { $_.ServerAddresses }); \
    $r = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 | Sort-Object RouteMetric, InterfaceMetric \
    | Select-Object -First 1; $a = $null; if ($r) { $a = Get-NetAdapter -InterfaceIndex $r.InterfaceIndex }; \
    [pscustomobject]@{ dns = $dns; culture = (Get-Culture).Name; route = \"$($r.InterfaceAlias)\"; \
    route_desc = \"$($a.InterfaceDescription)\" } | ConvertTo-Json -Compress";
const WIN_PROCS_PS: &str =
    "$ErrorActionPreference='Stop'; Get-CimInstance Win32_Process | ForEach-Object { \
    \"$($_.ProcessId)`t$($_.ExecutablePath)`t$($_.Name)`t$($_.CommandLine)\" }";

/// WIN_ENV_PS 的输出（ConvertTo-Json）→ {dns: [...], culture, route, route_desc}；前后有别的输出也认，解析不了的项是空的
pub fn parse_windows_env(text: &str) -> Value {
    let v = match (text.find('{'), text.rfind('}')) {
        (Some(i), Some(j)) if i < j => serde_json::from_str::<Value>(&text[i..=j]).ok(),
        _ => None,
    }
    .filter(|v| v.is_object())
    .unwrap_or_else(|| json!({}));
    let dns: Vec<String> = match v.get("dns") {
        Some(Value::String(x)) => vec![x.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|d| d.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .map(|d| d.trim().to_string())
    .filter(|d| !d.is_empty())
    .collect();
    let txt = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    json!({"dns": dns, "culture": txt("culture"), "route": txt("route"), "route_desc": txt("route_desc")})
}

/// WIN_PROCS_PS 的输出 → [(pid, 程序路径（可能是空的）, 名字, 命令行)]
pub fn parse_windows_processes(text: &str) -> Vec<(u32, String, String, String)> {
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.trim_end_matches('\r').splitn(4, '\t').collect();
            let [pid, exe, name, cmd] = parts[..] else {
                return None;
            };
            let pid = pid.trim();
            if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((
                pid.parse().ok()?,
                exe.trim().to_string(),
                name.trim().to_string(),
                cmd.trim().to_string(),
            ))
        })
        .collect()
}

fn powershell(script: &str) -> Command {
    let mut c = Command::new("powershell");
    c.args(["-NoProfile", "-Command", script]);
    #[cfg(windows)]
    {
        // 桌面版是 GUI 程序，不加这个会闪一下控制台窗口
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// 一次环境检测里 DNS、系统语言、默认路由三处共用这一次查询，5 秒内的直接用
fn windows_env() -> Value {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(Instant, Value)>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, v)) = cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if at.elapsed() < Duration::from_secs(5) {
            return v.clone();
        }
    }
    let v = parse_windows_env(&run_with_timeout(
        powershell(WIN_ENV_PS),
        Duration::from_secs(20),
    ));
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
    v
}

/// Windows 上的全部进程：Claude Code 会话、桌面版共用一次 PowerShell 查询，max_age 内查过的直接用（零是现查）。
/// 查不出来是 None。
pub fn win_processes(max_age: Duration) -> Option<Vec<(u32, String, String, String)>> {
    type Rows = Vec<(u32, String, String, String)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(Instant, Rows)>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, rows)) = g.as_ref() {
        if !max_age.is_zero() && at.elapsed() < max_age {
            return Some(rows.clone());
        }
    }
    let (Some(true), out) = run_command(powershell(WIN_PROCS_PS), Duration::from_secs(20)) else {
        return None;
    };
    let rows = parse_windows_processes(&out);
    *g = Some((Instant::now(), rows.clone()));
    Some(rows)
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
pub(crate) fn run_with_timeout(c: Command, timeout: std::time::Duration) -> String {
    run_command(c, timeout).1
}

/// 同 run_with_timeout，另外说明命令跑完没有：Some(退出码是不是 0)；没起来、超时被结束的是 None。
/// 要分清“命令说没有”和“命令没跑成”的地方用这个（查 claude 进程）。
pub(crate) fn run_command(mut c: Command, timeout: std::time::Duration) -> (Option<bool>, String) {
    use std::io::Read;
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = c.spawn() else {
        return (None, String::new());
    };
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + timeout;
    let done = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status.success()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(30))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
    (done, out)
}

/// 本机时区 (时区名, 国家代码列表) 或 None。国家代码来自 tzdata 的 zone1970.tab，查不到时为空列表。
/// 测试用 CAM_TEST_TZ="Asia/Hong_Kong=HK"（空字符串表示查不到）。
pub fn system_timezone() -> Option<(String, Vec<String>)> {
    if let Ok(t) = std::env::var("CAM_TEST_TZ") {
        if t.is_empty() {
            return None;
        }
        let (zone, cc) = t.split_once('=').unwrap_or((t.as_str(), ""));
        return Some((
            zone.to_string(),
            cc.split(',')
                .filter(|c| !c.is_empty())
                .map(String::from)
                .collect(),
        ));
    }
    let mut zone = std::env::var("TZ")
        .unwrap_or_default()
        .trim_start_matches(':')
        .to_string();
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
        let Ok(txt) = std::fs::read_to_string(tab) else {
            continue;
        };
        for line in txt.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if !line.starts_with('#') && parts.len() >= 3 && parts[2] == zone {
                countries = parts[0]
                    .split(',')
                    .filter(|c| !c.is_empty())
                    .map(String::from)
                    .collect();
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
        return t
            .split(',')
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect();
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
                    out.push(format!(
                        "{} {}:{}",
                        name,
                        h,
                        kv.get(&format!("{}Port", key)).cloned().unwrap_or_default()
                    ));
                }
            }
        }
        if on("ProxyAutoConfigEnable") {
            out.push(format!(
                "PAC {}",
                kv.get("ProxyAutoConfigURLString")
                    .cloned()
                    .unwrap_or_default()
            ));
        }
    } else if cfg!(windows) {
        let txt = run_text(
            "reg",
            &[
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            ],
        );
        let mut kv: HashMap<String, String> = HashMap::new();
        for line in txt.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && parts[1].starts_with("REG_") {
                kv.insert(parts[0].to_string(), parts[2..].join(" "));
            }
        }
        let enabled = kv
            .get("ProxyEnable")
            .map(|v| v.to_lowercase() == "0x1" || v == "1")
            .unwrap_or(false);
        if enabled {
            if let Some(p) = kv.get("ProxyServer") {
                out.push(format!("系统代理 {}", p));
            }
        }
        if let Some(u) = kv.get("AutoConfigURL").filter(|u| !u.is_empty()) {
            out.push(format!("PAC {}", u));
        }
    } else {
        let g = |args: &[&str]| {
            run_text("gsettings", args)
                .trim()
                .trim_matches('\'')
                .to_string()
        };
        let mode = g(&["get", "org.gnome.system.proxy", "mode"]);
        if mode == "manual" {
            let host = g(&["get", "org.gnome.system.proxy.https", "host"]);
            let port = g(&["get", "org.gnome.system.proxy.https", "port"]);
            out.push(if host.is_empty() {
                "系统代理（手动）".into()
            } else {
                format!("HTTPS {}:{}", host, port)
            });
        } else if mode == "auto" {
            out.push(format!(
                "PAC {}",
                g(&["get", "org.gnome.system.proxy", "autoconfig-url"])
            ));
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
        return if t.is_empty() {
            None
        } else {
            tunnel_kind(&t, "").map(|k| (t, k))
        };
    }
    let (mut name, mut desc) = (String::new(), String::new());
    if cfg!(windows) {
        let w = windows_env();
        name = w["route"].as_str().unwrap_or("").to_string();
        desc = w["route_desc"].as_str().unwrap_or("").to_string();
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

/// 记一笔“这个账号在这个出口地区用过”，给风险提示和诊断导出用（每个账号只留最近几个地区）。
///
/// reg 是当时的出口检测结果，有的话把 IP、运营商、城市、IP 类型一起记下；同一地区只留一条，次数累加、首次时间不变。
/// 和 cam.py 的 note_account_region 一致（两个版本共用一份数据，不能互相覆盖掉对方记的）
pub fn note_account_region(
    data: &mut Value,
    acc_id: Option<&str>,
    loc: Option<&str>,
    reg: Option<&Value>,
) {
    let (Some(id), Some(loc)) = (acc_id, loc) else {
        return;
    };
    let now = now_iso();
    fn text(v: Option<&Value>) -> Option<&str> {
        v.and_then(|v| v.as_str()).filter(|x| !x.is_empty())
    }
    for a in accounts_mut(data) {
        if s(a, "id") != Some(id) {
            continue;
        }
        let all: Vec<Value> = a
            .get("regions")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let old = all
            .iter()
            .find(|r| s(r, "loc") == Some(loc))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut regs: Vec<Value> = all
            .into_iter()
            .filter(|r| s(r, "loc") != Some(loc))
            .collect();
        let first = s(&old, "first_at")
            .or_else(|| s(&old, "at"))
            .map(String::from)
            .unwrap_or_else(|| now.clone());
        let times = old.get("times").and_then(|t| t.as_i64()).unwrap_or(0) + 1;
        let mut entry = json!({"loc": loc, "at": now, "first_at": first, "times": times});
        for k in ["ip", "org", "city", "ip_type"] {
            if let Some(v) = text(reg.and_then(|r| r.get(k))).or_else(|| text(old.get(k))) {
                entry[k] = json!(v);
            }
        }
        regs.insert(0, entry);
        regs.truncate(ACCOUNT_REGION_KEEP);
        a["regions"] = Value::Array(regs);
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
/// failed_at / fail_count：最近一次检测没连上（这时其它字段还是上一次查到的，checked_at 是那一次的时间）。
pub fn region_view(data: &Value) -> Value {
    let c = data.get("region").cloned().unwrap_or(Value::Null);
    let Some(checked) = s(&c, "checked_at").map(String::from) else {
        return json!({"loc": null, "loc_name": null, "ip": null, "status": null, "checked_at": null,
                      "message": "还没有检测过出口地区", "advice": "", "warp": false, "prev_loc": null, "changed_at": null,
                      "org": null, "asn": null, "city": null, "ip_type": null, "reg_loc": null, "reg_loc_name": null,
                      "meta_source": null, "lookup_mode": null, "failed_at": null, "fail_count": 0});
    };
    let loc = s(&c, "loc").map(String::from);
    let status = region_status(loc.as_deref());
    let reg_loc = s(&c, "reg_loc").map(String::from);
    json!({
        "loc": loc, "loc_name": region_name(loc.as_deref()), "ip": s(&c, "ip"), "status": status, "checked_at": checked,
        "message": region_message(loc.as_deref(), status), "advice": region_advice(status),
        "warp": c.get("warp").and_then(|v| v.as_bool()).unwrap_or(false),
        "prev_loc": s(&c, "prev_loc"), "changed_at": s(&c, "changed_at"),
        "org": s(&c, "org"), "asn": c.get("asn").cloned().unwrap_or(Value::Null), "city": s(&c, "city"), "ip_type": s(&c, "ip_type"),
        "reg_loc": reg_loc, "reg_loc_name": region_name(s(&c, "reg_loc")),
        "meta_source": s(&c, "meta_source"), "lookup_mode": s(&c, "lookup_mode"),
        "failed_at": s(&c, "failed_at"), "fail_count": c.get("fail_count").and_then(|v| v.as_i64()).unwrap_or(0)
    })
}

/// 离 iso 那个时间过了几秒；没有、看不懂是 None
fn age_secs(iso: Option<&str>) -> Option<f64> {
    let t = iso.and_then(parse_time)?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(t)
            .num_milliseconds() as f64
            / 1000.0,
    )
}

/// 本机现在连的是哪张网（短哈希，只在本机比较用）：默认路由用的本机地址，IPv6 只取前 64 位（后半截隔一阵自己会换）。
/// 换 Wi-Fi、插拔网线、开关接管整机流量的 VPN 时会变；代理换节点在本机看不出来，那种靠定时检测。
/// 不发任何数据包（UDP 的 connect 只让系统选路）。没有网络时返回 ""。与 cam.py 的 net_fingerprint 一致。
pub fn net_fingerprint() -> String {
    use std::net::{IpAddr, UdpSocket};
    if let Ok(fake) = std::env::var("CAM_TEST_NET") {
        return fake;
    }
    let addrs: Vec<IpAddr> = [
        ("0.0.0.0:0", "1.1.1.1:53"),
        ("[::]:0", "[2606:4700:4700::1111]:53"),
    ]
    .iter()
    .filter_map(|(local, remote)| {
        let sock = UdpSocket::bind(local).ok()?;
        sock.connect(remote).ok()?;
        sock.local_addr().ok().map(|a| a.ip())
    })
    .collect();
    net_fingerprint_of(&addrs)
}

/// net_fingerprint 的计算部分：addrs 是默认路由用的本机地址（IPv4 在前、IPv6 在后）。
/// IPv6 只取前 64 位；链路本地地址（fe80::/10）哪张网都是它，不算。一个能用的都没有返回 ""。
/// 两个版本读写同一个 region.net，算出来的必须是同一个值：规则和 cam.py 的 net_fingerprint_of 一致
/// （tests/shared/region_check.json）。
pub fn net_fingerprint_of(addrs: &[std::net::IpAddr]) -> String {
    use sha2::{Digest, Sha256};
    use std::net::IpAddr;
    let parts: Vec<String> = addrs
        .iter()
        .filter_map(|a| match a {
            IpAddr::V4(ip) => Some(ip.to_string()),
            IpAddr::V6(ip) if ip.segments()[0] & 0xffc0 != 0xfe80 => {
                Some(hex::encode(&ip.octets()[..8]))
            }
            IpAddr::V6(_) => None,
        })
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    hex::encode(Sha256::digest(parts.join("|").as_bytes()))[..12].to_string()
}

/// 上次环境检测的结果现在还能直接用：一小时内查到的，之后没有失败过，本机也没换网络。与 cam.py 的 region_trusted 一致。
pub fn region_trusted(data: &Value) -> bool {
    let reg = data.get("region");
    if cached_region_loc(data, REGION_FRESH_SECS).is_none()
        || reg.and_then(|r| s(r, "failed_at")).is_some()
    {
        return false;
    }
    let fp = net_fingerprint();
    !(!fp.is_empty() && reg.and_then(|r| s(r, "net")).is_some_and(|n| n != fp))
}

/// check_region 这一次可以跳过：max_age 秒内查到过，本机没换网络；上次没连上的话只看离那次失败有没有到
/// REGION_RETRY_SECS 秒（max_age 更短就按它）。age / fage：离上次查到 / 上次没连上过了几秒，没有过是 None。
/// 规则和 cam.py 的 region_check_fresh 一致（tests/shared/region_check.json）。
pub fn region_check_fresh(age: Option<f64>, fage: Option<f64>, max_age: i64, moved: bool) -> bool {
    if let Some(f) = fage {
        return f < max_age.min(REGION_RETRY_SECS) as f64;
    }
    age.is_some_and(|a| a < max_age as f64) && !moved
}

/// 环境检测没连上之后隔几秒重试：从 REGION_RETRY_SECS 起每连着失败一次翻一倍（最多翻四次），
/// 最长不超过定时检测的间隔。规则和 cam.py 的 region_retry_wait 一致（tests/shared/region_check.json）。
pub fn region_retry_wait(fail_count: i64, refresh_min: i64) -> i64 {
    let n = fail_count.max(1);
    (REGION_RETRY_SECS << (n - 1).min(4)).min(REGION_RETRY_SECS.max(refresh_min.saturating_mul(60)))
}

/// 后台循环在两次定时检测之间记的状态（region_recheck_due 用）：上次看到本机连的是哪张网、什么时候看到的（Unix 秒）
#[derive(Clone, Debug, Default)]
pub struct RegionRecheck {
    pub net: String,
    pub net_at: f64,
}

impl RegionRecheck {
    /// 这一轮本来就要测，记下现在连的是哪张网
    pub fn note_net(&mut self) {
        let fp = net_fingerprint();
        if !fp.is_empty() {
            self.net = fp;
        }
    }
}

/// 两次定时检测之间要不要补测一次环境（后台循环每轮问一次，st 是它的状态）：
/// 本机换了网络（net_fingerprint 和上次不一样；NET_RECHECK_SECS 秒内最多补测一次），
/// 或者上次没连上、到了重试的时候（从 REGION_RETRY_SECS 起每失败一次翻一倍，最长不超过定时检测的间隔）。
/// 与 cam.py 的 region_recheck_due 一致。
pub fn region_recheck_due(data: &Value, st: &mut RegionRecheck, refresh_min: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    region_recheck_due_at(data, st, refresh_min, now)
}

/// 同 region_recheck_due，现在是几点（Unix 秒）由调用方给
pub fn region_recheck_due_at(
    data: &Value,
    st: &mut RegionRecheck,
    refresh_min: u64,
    now: f64,
) -> bool {
    let reg = data.get("region").cloned().unwrap_or(Value::Null);
    let fp = net_fingerprint();
    let seen = if st.net.is_empty() {
        s(&reg, "net").unwrap_or("").to_string()
    } else {
        st.net.clone()
    };
    if !fp.is_empty()
        && fp != seen
        && (seen.is_empty() || now - st.net_at >= NET_RECHECK_SECS as f64)
    {
        st.net = fp;
        st.net_at = now;
        if !seen.is_empty() {
            return true;
        }
    }
    if let Some(failed) = s(&reg, "failed_at") {
        let fails = reg.get("fail_count").and_then(|v| v.as_i64()).unwrap_or(1);
        let wait = region_retry_wait(fails, i64::try_from(refresh_min).unwrap_or(i64::MAX));
        return age_secs(Some(failed)).is_none_or(|age| age >= wait as f64);
    }
    false
}

/// 后台定时调用：检测出口地区（连带 WARP、时钟偏差）和 Anthropic 官方状态页，记到 data["region"] / data["status_page"]，
/// 并给当前账号记一笔地区。max_age 秒内查到过就跳过（本机换了网络的不跳）；上次没连上的话 REGION_RETRY_SECS 秒内不重试
/// （max_age 更短就按它）。
///
/// 这次没连上、以前查到过：出口那一份留着不动（checked_at 还是查到那次的时间），只记 failed_at / fail_count，
/// 界面据此说“最近一次没连上，先用上一次的结果”；连上了就清掉。从没查到过的还是记成查不到。
/// 本机设置（代理、VPN、时区、DNS）不用联网，每次都更新。最多等 REGION_CHECK_BUDGET 秒。只记录，不干预任何操作。
/// 与 cam.py 的 check_region 一致。
pub fn check_region(store: &Store, max_age: i64) -> R<Value> {
    let data = store.load()?;
    let old = data.get("region").cloned().unwrap_or_else(|| json!({}));
    let fp = net_fingerprint();
    // 本机换了网络：上次查到的是另一张网的，没到 max_age 也重测
    let moved = !fp.is_empty() && s(&old, "net").is_some_and(|n| n != fp);
    if region_check_fresh(
        age_secs(s(&old, "checked_at")),
        age_secs(s(&old, "failed_at")),
        max_age,
        moved,
    ) {
        return Ok(region_view(&data));
    }
    let cfg = ip_lookup_config(Some(store));
    // 出口、状态页、本机设置几件事并行，互不等待；各有各的时限，到点没做完的当作没查到
    let t0 = Instant::now();
    let within = |secs: u64| t0 + Duration::from_secs(secs.min(REGION_CHECK_BUDGET));
    let jr = {
        let (cfg, old) = (cfg.clone(), old.clone());
        spawn_job(move || detect_region_with(&cfg, &old))
    };
    let je = spawn_job(local_env);
    let js = spawn_job(fetch_status_page);
    let jn = spawn_job(net_fingerprint);
    let jp = {
        let data = data.clone();
        spawn_job(move || proxies_back(&data))
    };
    let r = wait_job(&jr, within(REGION_CHECK_BUDGET))
        .flatten()
        .unwrap_or_else(|| json!({}));
    let env = wait_job(&je, within(10));
    let sp = wait_job(&js, within(7)).flatten();
    let net = wait_job(&jn, within(2)).unwrap_or_default();
    let back = wait_job(&jp, within(PROXY_PROBE_SECS + 1)).unwrap_or_default();
    let _g = store.lock()?;
    let mut data = store.load()?;
    let old = data.get("region").cloned().unwrap_or_else(|| json!({}));
    let now = now_iso();
    let new_loc = s(&r, "loc").map(String::from);
    let mut reg = if new_loc.is_some() || s(&old, "loc").is_none() {
        let mut reg = json!({
            "loc": new_loc, "ip": r.get("ip"), "colo": r.get("colo"), "ms": r.get("ms"),
            "warp": r.get("warp").and_then(|v| v.as_bool()).unwrap_or(false), "skew": r.get("skew"),
            "lookup_mode": cfg.mode, "env": old.get("env"), "env_at": old.get("env_at"),
            "checked_at": now, "net": net
        });
        for k in META_FIELDS.iter().chain(STACK_FIELDS) {
            reg[*k] = r.get(*k).cloned().unwrap_or(Value::Null);
        }
        // 中间失败过一次也和最后一次知道的出口比，出口变化不漏报（与 cam.py 一致）
        let last = s(&old, "loc")
            .or_else(|| s(&old, "last_loc"))
            .map(String::from);
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
        reg
    } else {
        old.clone() // 这次没连上，手里有以前查到的：留着那一份
    };
    if new_loc.is_none() {
        let fails = old.get("fail_count").and_then(|v| v.as_i64()).unwrap_or(0);
        reg["failed_at"] = json!(now);
        reg["fail_count"] = json!(fails + 1);
    }
    if let Some(env) = env {
        reg["env"] = env;
        reg["env_at"] = json!(now);
    }
    data["region"] = reg;
    // 状态页这次没连上就留着上次的（它自己带时间，过一小时不再算数）
    if let Some(mut v) = sp {
        v["checked_at"] = json!(now);
        data["status_page"] = v;
    }
    if new_loc.is_some() {
        let act = active_code_id(&data);
        let reg_now = data.get("region").cloned();
        note_account_region(
            &mut data,
            act.as_deref(),
            new_loc.as_deref(),
            reg_now.as_ref(),
        );
    }
    // 连得上 claude.ai 了：只清掉它能证明已经好了的（DNS、证书、claude.ai 连不上）；代理又连得上了的也清掉
    let traced = new_loc.is_some();
    let mut drop_all = false;
    if let Some(items) = data
        .get_mut("service_status")
        .and_then(|ss| ss.get_mut("items"))
        .and_then(|i| i.as_array_mut())
    {
        items.retain(|i| {
            !(traced && resolved_by_trace(i)) && proxy_key(i).is_none_or(|k| !back.contains(&k))
        });
        drop_all = items.is_empty();
    }
    if drop_all {
        if let Some(o) = data.as_object_mut() {
            o.remove("service_status");
        }
    }
    store.save(&mut data)?;
    Ok(region_view(&data))
}

/// 最近一次环境检测没连上、手里还有以前查到的出口时的那一条提示；顶部环境卡（risk_items）和服务检测共用，两边扣分才一致。
/// 那份结果还新（一小时内）：只说一声，不扣分；已经旧了：出口现在是哪里说不准，按“查不到”提醒。
/// 上一次查到的出口不在支持名单时也只说一声：“不在名单”那条已经提醒并扣过分了，不再叠一条。没失败过返回 None。
/// 规则和 cam.py 的 region_fail_item 一致（tests/shared/region_check.json）。
pub fn region_fail_item(reg: &Value) -> Option<Value> {
    let (failed, loc, checked) = (s(reg, "failed_at")?, s(reg, "loc")?, s(reg, "checked_at")?);
    if age_secs(Some(checked)).is_some_and(|a| a <= REGION_FRESH_SECS as f64)
        || region_status(Some(loc)) == "unsupported"
    {
        return Some(json!({
            "kind": "env_failed", "level": "info",
            "text": format!("最近一次环境检测没连上（{}），先用上一次查到的结果（{}）", fmt_ago(failed), fmt_ago(checked)),
            "advice": ENV_FAILED_ADVICE
        }));
    }
    let n = reg.get("fail_count").and_then(|v| v.as_i64()).unwrap_or(1);
    let times = if n > 1 {
        format!("连续 {} 次检测都", n)
    } else {
        "最近一次检测".to_string()
    };
    Some(json!({
        "kind": "region_unknown", "level": "warn",
        "text": format!("出口地区现在查不到：{}没连上 claude.ai 和 Cloudflare，上一次查到是 {}（{}）", times, region_label(Some(loc)), fmt_ago(checked)),
        "advice": region_advice("unknown")
    }))
}

/// 服务检测记下的“代理连不上”那一条对应的 (主机, 端口)；别的条目返回 None。与 cam.py 的 proxy_key 一致。
fn proxy_key(item: &Value) -> Option<(String, u16)> {
    if s(item, "kind") != Some("proxy_down") {
        return None;
    }
    let host = s(item, "host").filter(|h| !h.is_empty())?;
    let port = u16::try_from(item.get("port")?.as_u64()?).ok()?;
    Some((host.to_string(), port))
}

/// 上次服务检测记下连不上的代理，现在又连得上了的。只连一下本机设的代理端口，不发别的。与 cam.py 的 proxies_back 一致。
fn proxies_back(data: &Value) -> Vec<(String, u16)> {
    let mut keys: Vec<(String, u16)> = data
        .get("service_status")
        .and_then(|ss| ss.get("items"))
        .and_then(|i| i.as_array())
        .map(|a| a.iter().filter_map(proxy_key).collect())
        .unwrap_or_default();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|(h, p)| port_open(h, *p, Duration::from_secs(PROXY_PROBE_SECS)).is_empty())
        .collect()
}

/// 后台连上了 claude.ai 的 trace，就能证明这条服务检测问题已经好了：DNS、证书、claude.ai 连不上。
/// 人机验证、服务端错误、api.anthropic.com 的问题证明不了，留到下次服务检测或一小时后过期。与 cam.py 一致。
fn resolved_by_trace(item: &Value) -> bool {
    match s(item, "kind") {
        Some("dns" | "tls") => true,
        Some("web") => s(item, "text")
            .map(|t| t.contains("连不上"))
            .unwrap_or(false),
        _ => false,
    }
}

/// 根据后台缓存和本机设置算出的风险提示（不联网）：[{kind, level, text, advice}]。
/// 只提示，不拦任何操作；界面把 warn / bad 的常显在顶部，info 只在服务检查里列出。
/// 上次环境检测超过一小时（后台没在跑时会这样）：从那次结果里算出来的每一条后面标上是多久以前测的，
/// 再加一条 env_stale 说明怎么重新检测；官方状态页的故障过了一小时不再算数。note_age=false 不标（调用方自己说了是多久前的）。
/// 最近一次检测没连上（failed_at）：出口还按上一次查到的算，另加一条 region_fail_item 说明。
/// 与 cam.py 的 risk_items 一致。
pub fn risk_items(data: &Value, include_region: bool, note_age: bool) -> Vec<Value> {
    let now = chrono::Utc::now();
    let reg = data.get("region").cloned().unwrap_or_else(|| json!({}));
    // 过了多少毫秒；时间看不懂是 None
    let age_ms = |iso: Option<&str>| {
        iso.and_then(parse_time)
            .map(|t| now.signed_duration_since(t).num_milliseconds())
    };
    let checked = s(&reg, "checked_at");
    let old = match checked {
        Some(at) if note_age && age_ms(checked).is_some_and(|a| a > REGION_FRESH_SECS * 1000) => {
            format!("（{}检测的）", fmt_ago(at))
        }
        _ => String::new(),
    };
    // 本机设置每次都更新，出口没连上那次也是
    let env_at = s(&reg, "env_at").or(checked);
    let env_old = match env_at {
        Some(at) if note_age && age_ms(env_at).is_some_and(|a| a > REGION_FRESH_SECS * 1000) => {
            format!("（{}检测的）", fmt_ago(at))
        }
        _ => String::new(),
    };
    let out: std::cell::RefCell<Vec<Value>> = std::cell::RefCell::new(Vec::new());
    // cached：这一条是从上次环境检测的结果里算出来的，结果旧了要标出来；自己带时间或不靠那次检测的传 false
    let push = |kind: &str, level: &str, text: String, advice: &str, cached: bool| {
        let text = if cached { text + &old } else { text };
        out.borrow_mut()
            .push(json!({"kind": kind, "level": level, "text": text, "advice": advice}))
    };
    let add =
        |kind: &str, level: &str, text: String, advice: &str| push(kind, level, text, advice, true);
    let add_plain = |kind: &str, level: &str, text: String, advice: &str| {
        push(kind, level, text, advice, false)
    };
    let loc: Option<String> = if s(&reg, "checked_at").is_some() {
        s(&reg, "loc").map(String::from)
    } else {
        None
    };
    let fail = region_fail_item(&reg);
    if include_region {
        if let Some(l) = loc.as_deref() {
            if region_status(Some(l)) == "unsupported" {
                add(
                    "region",
                    "warn",
                    region_message(Some(l), "unsupported"),
                    region_advice("unsupported"),
                );
            }
        } else if s(&reg, "checked_at").is_some() {
            add(
                "region_unknown",
                "warn",
                "出口地区查不到：上次检测连不上 claude.ai 和 Cloudflare".into(),
                region_advice("unknown"),
            );
        }
        if let Some(f) = &fail {
            out.borrow_mut().push(f.clone());
        }
        // 代理 / VPN：和服务检测用同一份缓存，扣分一致
        if s(&reg, "checked_at").is_some() {
            if let Some(e) = reg.get("env").filter(|e| e.get("proxy").is_some()) {
                for it in proxy_vpn_items(e) {
                    if s(&it, "level") != Some("ok") {
                        add_plain(
                            s(&it, "kind").unwrap_or(""),
                            s(&it, "level").unwrap_or("info"),
                            s(&it, "text").unwrap_or("").to_string() + &env_old,
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
                for it in ss
                    .get("items")
                    .and_then(|i| i.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    add_plain(
                        s(&it, "kind").unwrap_or(""),
                        s(&it, "level").unwrap_or("warn"),
                        s(&it, "text").unwrap_or("").to_string(),
                        s(&it, "advice").unwrap_or(""),
                    );
                }
            }
        }
    }
    if let (Some(prev), Some(l), Some(changed)) =
        (s(&reg, "prev_loc"), loc.as_deref(), s(&reg, "changed_at"))
    {
        if let Some(t) = parse_time(changed) {
            if now.signed_duration_since(t).num_seconds() < REGION_CHANGE_HOURS * 3600 {
                // 网络探测不等于账号登录；当前出口已支持时只保留历史，避免恢复后仍因旧出口扣分。
                let supported_change = region_status(Some(l)) == "supported";
                add_plain(
                    "region_change",
                    if supported_change { "info" } else { "warn" },
                    format!(
                        "出口地区变化：{} → {}（{}）",
                        region_label(Some(prev)),
                        region_label(Some(l)),
                        fmt_ago(changed)
                    ),
                    if supported_change {
                        "当前出口在支持名单内。换网络、出差或开关 TUN / 代理都可能造成变化；这是网络检测记录，不等于账号异地登录。保留 24 小时供排查，不额外扣分；账号使用地区另列提示。"
                    } else {
                        "网络检测出口发生变化，当前出口不在支持名单内。请结合当前出口地区和 Claude 服务实测判断；这条记录不等于账号异地登录，账号使用地区另列提示。"
                    },
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
    let asn = reg
        .get("asn")
        .filter(|v| !v.is_null())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
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
                format!(
                    "出口 IP 注册在 {}，定位在 {}（广播 IP）",
                    region_label(Some(r)),
                    region_label(Some(l))
                ),
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
        for d in e
            .get("dns")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let Some(c) = s(&d, "country") else { continue };
            if c == l {
                continue;
            }
            let text = format!(
                "系统 DNS 配置 {}（{}，{}）与出口地区 {} 不一致（仅配置）",
                s(&d, "ip").unwrap_or("?"),
                s(&d, "name").unwrap_or("公共 DNS"),
                region_label(Some(c)),
                region_label(Some(l))
            );
            add(
                "dns_resolver",
                "info",
                text,
                "TUN / 代理可能接管 DNS；这里只读取系统配置，未测实际解析出口，不能据此判断 DNS 泄漏或所在地，不扣分。\
                 解析是否成功请看 DNS 实测项；需要排查旧的手动配置时，可在系统网络设置中恢复自动获取。",
            );
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
    if let Some(skew) = reg
        .get("skew")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
    {
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
        // 旧版没单独记时间，和出口是同一次查的；一小时前的故障现在多半已经恢复，不再提
        let sp_fresh =
            age_ms(s(sp, "checked_at").or(checked)).is_some_and(|a| a <= REGION_FRESH_SECS * 1000);
        if let Some((level, text)) = status_risk(sp).filter(|_| sp_fresh) {
            add_plain("status", level, text, STATUS_ADVICE);
        }
    }
    let tz_cached = env.as_ref().and_then(|e| {
        s(e, "tz").map(|z| {
            (
                z.to_string(),
                e.get("tz_cc")
                    .and_then(|c| c.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            )
        })
    });
    if let (Some((zone, cc)), Some(l)) = (tz_cached, loc.as_deref()) {
        if !cc.is_empty() && !cc.iter().any(|c| c == l) {
            let home = cc[0].as_str();
            let text = format!(
                "系统时区 {} 指向 {}，与出口地区 {} 不一致",
                zone,
                region_label(Some(home)),
                region_label(Some(l))
            );
            if region_status(Some(home)) == "unsupported" {
                add(
                    "timezone",
                    "warn",
                    text,
                    "出口在支持地区，但本机设置显示人可能在不支持地区。Anthropic 按使用者实际所在地判断，不按出口 IP；\
                     人确实在不支持地区的话，风险与出口无关。",
                );
            } else {
                add(
                    "timezone",
                    "info",
                    text,
                    "出差或经过代理都会这样；人所在地在支持名单内就没问题。想让它和所在地一致：在系统设置的“日期与时间”里按所在城市选时区。",
                );
            }
        }
    }
    let lc_cached = env
        .as_ref()
        .and_then(|e| s(e, "locale").map(|n| (n.to_string(), s(e, "locale_cc").map(String::from))));
    if let (Some((name, Some(home))), Some(l)) = (lc_cached, loc.as_deref()) {
        if home != l {
            let text = format!(
                "系统语言 / 区域 {} 指向 {}，与出口地区 {} 不一致",
                name,
                region_label(Some(&home)),
                region_label(Some(l))
            );
            // 语言设置只代表语言习惯（新加坡、美国很多人也用 zh_CN），不代表所在地：只做参考，扣 5 分
            add("locale", "info", text, LOCALE_ADVICE);
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
                    .filter(|r| {
                        s(r, "at")
                            .and_then(parse_time)
                            .map(|t| t >= cutoff)
                            .unwrap_or(false)
                    })
                    .filter_map(|r| s(r, "loc").map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if locs.len() > 1 {
            add_plain(
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
    let n = accounts(data)
        .iter()
        .filter(|a| s(a, "kind") == Some("claude_code"))
        .count();
    if n >= 2 {
        add_plain(
            "accounts",
            "info",
            format!("本机保存了 {} 个 Claude Code 账号", n),
            "按 Anthropic 消费者条款，账号和登录信息不能给别人用或分享；一个人自己用多个号条款没有明确禁止。",
        );
    }
    // 没连上那一条已经说了结果是多久前的、怎么重新检测
    if let Some(at) = checked.filter(|_| !old.is_empty() && fail.is_none()) {
        add_plain(
            "env_stale",
            "info",
            format!(
                "环境检测结果是 {}的，出口、代理、时区这些可能已经变了",
                fmt_ago(at)
            ),
            ENV_STALE_ADVICE,
        );
    }
    out.into_inner()
}

/// 环境检测结果旧了（env_stale）时告诉用户怎么重新检测。与 cam.py 的 ENV_STALE_ADVICE 一致
const ENV_STALE_ADVICE: &str = "重新检测：运行 cam doctor，或在面板的“环境”里点“重新检测”。开着面板（cam serve）、桌面版或菜单栏小工具时会定时自动检测。";
/// 最近一次环境检测没连上（env_failed）时的说明。与 cam.py 的 ENV_FAILED_ADVICE 一致
const ENV_FAILED_ADVICE: &str = "多半是网络刚好断了一下，或者代理在换节点。开着面板（cam serve）、桌面版或菜单栏小工具时过一会儿会自动重试，连上就恢复；想马上重测：运行 cam doctor，或在面板的“环境”里点“重新检测”。";

const LOCALE_ADVICE: &str = "系统语言只代表语言习惯，不代表所在地，所以只作参考、扣 5 分。想让它和所在地一致：在系统设置的“语言与地区”里\
                             把“地区”改成你实际所在的国家（语言可以继续用中文）。";

/// 环境风险评分：满分 100，按 (kind, level) 扣分；没列出的 warn 扣 10、bad 扣 30。只是把提醒汇总成一个数，不拦任何操作。
const RISK_WEIGHTS: &[(&str, &str, i64)] = &[
    ("region", "warn", 60),
    ("region_unknown", "warn", 30),
    ("tls", "bad", 0),
    ("account_regions", "warn", 20),
    ("region_change", "warn", 15),
    ("isp", "warn", 20),
    ("region_mismatch", "warn", 15),
    ("timezone", "warn", 25),
    ("timezone", "info", 5),
    ("locale", "info", 5),
    ("clock", "warn", 10),
    ("warp", "info", 5),
    ("vpn", "info", 10),
    ("proxy", "info", 5),
    ("status", "bad", 30),
    ("status", "warn", 10),
    ("api", "bad", 40),
    ("api", "warn", 5),
    ("web", "bad", 10),
    ("web", "warn", 5),
    ("dns", "bad", 20),
    ("accounts", "info", 0),
    ("device", "info", 0),
    ("scope", "info", 0),
    ("tor_exit", "warn", 40),
    ("proxy_ip", "warn", 25),
    ("ipv6_mismatch", "warn", 15),
    ("dns_resolver", "warn", 15),
    ("dns_resolver", "info", 0),
    ("ip_origin", "info", 5),
    ("ipv6", "info", 0),
    ("dns_servers", "info", 0),
    ("auth", "bad", 20),
    ("auth", "warn", 5),
    ("proxy_down", "bad", 0),
    ("tls_intercept", "warn", 0),
];
/// (最低分, 档位, 说明)
const RISK_GRADES: &[(i64, &str, &str)] = &[
    (90, "low", "风险低，可以使用"),
    (70, "medium", "有风险，建议先处理提醒项再使用"),
    (0, "high", "风险高，不建议在当前环境使用；是否继续由你决定"),
];
const RISK_SCORE_ADVICE: &str =
    "评分只汇总本机能看到的信号，满分也不代表一定合规；低于 90 时逐条看提醒和建议。";

/// 把风险项汇总成 {score, grade, label, advice}。只用于提示。
pub fn risk_score(items: &[Value]) -> Value {
    let mut score: i64 = 100;
    for it in items {
        let (kind, level) = (s(it, "kind").unwrap_or(""), s(it, "level").unwrap_or(""));
        if kind == "summary" {
            continue;
        }
        score -= match RISK_WEIGHTS
            .iter()
            .find(|(k, l, _)| *k == kind && *l == level)
        {
            Some((_, _, w)) => *w,
            None => match level {
                "warn" => 10,
                "bad" => 30,
                _ => 0,
            },
        };
    }
    let score = score.max(0);
    let (_, grade, label) = RISK_GRADES
        .iter()
        .find(|(floor, _, _)| score >= *floor)
        .unwrap_or(&RISK_GRADES[2]);
    json!({"score": score, "grade": grade, "label": label, "advice": RISK_SCORE_ADVICE})
}

fn region_denied(msg: &str) -> bool {
    let low = msg.to_lowercase();
    [
        "region",
        "country",
        "not available",
        "unsupported",
        "not allowed",
        "地区",
    ]
    .iter()
    .any(|k| low.contains(k))
}

pub fn is_cloudflare(raw: &str) -> bool {
    let low = raw.to_lowercase();
    [
        "just a moment",
        "cf-chl",
        "cloudflare",
        "challenge-platform",
        "/cdn-cgi/",
    ]
    .iter()
    .any(|k| low.contains(k))
}

fn host_of(url: &str) -> String {
    let rest = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
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
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

fn or_dash(msg: &str) -> &str {
    if msg.is_empty() {
        "-"
    } else {
        msg
    }
}

/// 解析一个域名：排好序、去了重的地址，或者错误的文字
fn resolve_host(host: &str) -> Result<Vec<String>, String> {
    let addrs = (host, 443u16)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?;
    let mut ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

/// [(来源, 设置值)] → [(来源, 主机, 端口)]：本机设的代理要连的地址，服务检测拿来看它在不在运行。最多 4 个，同一个地址只留一次；
/// PAC 和没写端口的不看。来源是环境变量名或“系统设置”；设置值是环境变量的值（http://user:pass@127.0.0.1:7890），
/// 或 system_proxy() 的一项（“HTTPS 127.0.0.1:7890”“系统代理 http=127.0.0.1:7890;https=127.0.0.1:7890”，前面的说明去掉）。
/// 与 cam.py 的 proxy_endpoints 一致（tests/shared/service.json）。
pub fn proxy_endpoints(entries: &[(String, String)]) -> Vec<(String, String, u16)> {
    let mut out: Vec<(String, String, u16)> = Vec::new();
    let mut seen: Vec<(String, u16)> = Vec::new();
    for (src, v) in entries {
        let mut v = v.trim();
        if src == "系统设置" {
            if v.to_uppercase().starts_with("PAC") {
                continue;
            }
            v = v.rsplit(' ').next().unwrap_or("");
        }
        for part in v.split(';') {
            let mut part = part.trim();
            if part.split("://").next().unwrap_or("").contains('=') {
                // Windows 的 http=…;https=…
                part = part.split_once('=').map(|x| x.1).unwrap_or(part);
            }
            let part = part.split_once("://").map(|x| x.1).unwrap_or(part);
            let part = &part[part.rfind('@').map(|i| i + 1).unwrap_or(0)..]; // 账号密码、路径都不要
            let part = part.split('/').next().unwrap_or("");
            let (host, port) = match part.strip_prefix('[') {
                Some(rest) => {
                    let (h, r) = rest.split_once(']').unwrap_or((rest, ""));
                    (h, r.strip_prefix(':').unwrap_or(""))
                }
                None => part.rsplit_once(':').unwrap_or(("", part)),
            };
            let port = match port.parse::<u32>() {
                Ok(p) if port.bytes().all(|b| b.is_ascii_digit()) && p > 0 && p < 65536 => p as u16,
                _ => continue,
            };
            if host.is_empty() {
                continue;
            }
            let key = (host.to_lowercase(), port);
            if !seen.contains(&key) {
                seen.push(key);
                out.push((src.clone(), host.to_string(), port));
            }
        }
    }
    out.truncate(4);
    out
}

/// 连一下 host:port（只建 TCP 连接，马上断开，不发数据）：连上了返回空字符串，没连上返回原因。与 cam.py 的 port_open 一致。
pub fn port_open(host: &str, port: u16, timeout: Duration) -> String {
    let addrs: Vec<std::net::SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return e.to_string(),
    };
    let mut why = "解析不了这个地址".to_string();
    for a in addrs {
        match std::net::TcpStream::connect_timeout(&a, timeout) {
            Ok(_) => return String::new(),
            Err(e) => {
                why = match e.kind() {
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                        format!("超过 {} 秒没有回应", timeout.as_secs())
                    }
                    std::io::ErrorKind::ConnectionRefused => "连接被拒绝".to_string(),
                    _ => e.to_string(),
                }
            }
        }
    }
    why
}

/// 本机设的代理连不上的那一条；host / port 记着，后台检测时再连一下，连上了就清掉。与 cam.py 一致。
fn proxy_down_item(src: &str, host: &str, port: u16, why: &str) -> Value {
    json!({"kind": "proxy_down", "level": "bad", "text": format!("代理 {}:{}（{}）连不上：{}", host, port, src, why),
           "advice": PROXY_DOWN_ADVICE, "host": host, "port": port})
}

/// 续登录的地址（Claude Code 和 cam 续登录都用）连不连得上，汇总成一条 {kind: auth}。
/// results：(域名, HTTP 状态, 返回的文字, 毫秒)，状态 0 是没连上。这里不带登录、用的是 GET，回什么状态都算连得上（正常是 405）；
/// 回人机验证页、因为地区拒绝的算被拦住了。与 cam.py 的 auth_item 一致（tests/shared/service.json）。
pub fn auth_item(results: &[(String, u16, String, u128)]) -> Value {
    let (mut good, mut bad) = (Vec::new(), Vec::new());
    for (host, st, txt, ms) in results {
        if *st == 0 {
            bad.push(format!("{} 连不上", host));
        } else if (*st == 403 || *st == 503) && is_cloudflare(txt) {
            bad.push(format!("{} 返回人机验证页", host));
        } else if *st == 403 && region_denied(txt) {
            bad.push(format!("{} 拒绝访问（HTTP 403）", host));
        } else {
            good.push(format!("{}（{} ms）", host, ms));
        }
    }
    if bad.is_empty() {
        return json!({"kind": "auth", "level": "ok", "text": format!("续登录的地址连得上：{}", good.join("、")), "advice": ""});
    }
    if !good.is_empty() {
        return json!({"kind": "auth", "level": "warn",
                      "text": format!("续登录的地址有的连不上：{}；{}连得上", bad.join("、"), good.join("、")),
                      "advice": AUTH_PARTIAL_ADVICE});
    }
    json!({"kind": "auth", "level": "bad", "text": format!("续登录的地址都连不上：{}", bad.join("、")), "advice": AUTH_DOWN_ADVICE})
}

/// X.509 证书（DER）里颁发者的组织名和通用名 {"org", "cn"}（有哪个给哪个），解析不了返回 None。只读这两项，不做任何校验。
/// 与 cam.py 的 der_issuer 一致（tests/shared/service.json）。
pub fn der_issuer(der: &[u8]) -> Option<Value> {
    // (标签, 内容开始, 内容结束)
    fn tlv(der: &[u8], i: usize) -> Option<(u8, usize, usize)> {
        let tag = *der.get(i)?;
        let n = *der.get(i + 1)? as usize;
        let mut i = i + 2;
        let n = if n & 0x80 != 0 {
            let k = n & 0x7f;
            if k == 0 || k > 4 {
                return None;
            }
            let bytes = der.get(i..i + k)?;
            i += k;
            bytes.iter().fold(0usize, |a, b| (a << 8) | *b as usize)
        } else {
            n
        };
        let end = i.checked_add(n)?;
        (end <= der.len()).then_some((tag, i, end))
    }
    let (_, cert, _) = tlv(der, 0)?; // Certificate
    let (_, tbs, _) = tlv(der, cert)?; // tbsCertificate
    let (tag, _, mut e) = tlv(der, tbs)?;
    if tag == 0xa0 {
        e = tlv(der, e)?.2; // [0] 版本之后是序列号
    }
    e = tlv(der, e)?.2; // 签名算法
    let (tag, mut j, end) = tlv(der, e)?; // 颁发者
    if tag != 0x30 {
        return None;
    }
    let mut out = serde_json::Map::new();
    while j < end {
        let (_, mut k, set_end) = tlv(der, j)?;
        j = set_end;
        while k < set_end {
            let (_, a, attr_end) = tlv(der, k)?;
            k = attr_end;
            let (_, os, oe) = tlv(der, a)?;
            let (vt, vs, ve) = tlv(der, oe)?;
            let key = match &der[os..oe] {
                [0x55, 0x04, 0x0a] => "org", // 2.5.4.10
                [0x55, 0x04, 0x03] => "cn",  // 2.5.4.3
                _ => continue,
            };
            if !out.contains_key(key) {
                let raw = &der[vs..ve];
                let text = if vt == 0x1e {
                    let units: Vec<u16> = raw
                        .chunks(2)
                        .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
                        .collect();
                    String::from_utf16_lossy(&units)
                } else {
                    String::from_utf8_lossy(raw).into_owned()
                };
                out.insert(key.into(), json!(text));
            }
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn pct_decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%')
            .then(|| v.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(x) => {
                out.push(x);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// host 现在拿到的 HTTPS 证书是谁签的：(颁发者 {"org", "cn"}, 系统的证书库认不认它)；连不上、走的是 SOCKS 代理看不了、
/// 证书解析不了时返回 None。和 ureq 一样只看环境变量里的代理（http:// 的，用 CONNECT 打通道，不在代理那头解密）。
/// 先按系统的证书库校验着连；不行再不校验地连一次，只为看颁发者。只握手，不发任何请求。与 cam.py 的 peer_issuer 对应
/// （那边用的是 Python 的证书库，也看系统代理设置）。
pub fn peer_issuer(host: &str, port: u16, timeout: Duration) -> Option<(Value, bool)> {
    use std::io::{Read, Write};
    let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
    let open = || -> Option<std::net::TcpStream> {
        let (h, p, auth) = match &proxy {
            Some(px) => {
                let full = if px.contains("://") {
                    px.clone()
                } else {
                    format!("http://{}", px)
                };
                let u = url::Url::parse(&full).ok()?;
                if u.scheme() != "http" {
                    return None;
                }
                let auth = (!u.username().is_empty()).then(|| {
                    use base64::Engine;
                    let cred = format!(
                        "{}:{}",
                        pct_decode(u.username()),
                        pct_decode(u.password().unwrap_or(""))
                    );
                    base64::engine::general_purpose::STANDARD.encode(cred)
                });
                let h = u
                    .host_str()?
                    .trim_matches(|c| c == '[' || c == ']')
                    .to_string();
                (h, u.port().unwrap_or(80), auth)
            }
            None => (host.to_string(), port, None),
        };
        let addr = (h.as_str(), p).to_socket_addrs().ok()?.next()?;
        let mut st = std::net::TcpStream::connect_timeout(&addr, timeout).ok()?;
        st.set_read_timeout(Some(timeout)).ok()?;
        st.set_write_timeout(Some(timeout)).ok()?;
        if proxy.is_some() {
            let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
            if let Some(a) = auth {
                req += &format!("Proxy-Authorization: Basic {}\r\n", a);
            }
            req += "\r\n";
            st.write_all(req.as_bytes()).ok()?;
            let (mut buf, mut b) = (Vec::new(), [0u8; 1]);
            while !buf.ends_with(b"\r\n\r\n") {
                if buf.len() > 8192 || st.read(&mut b).ok()? == 0 {
                    return None;
                }
                buf.push(b[0]);
            }
            let head = String::from_utf8_lossy(&buf);
            if head.split_whitespace().nth(1) != Some("200") {
                return None;
            }
        }
        Some(st)
    };
    let fetch = |verify: bool| -> Option<Vec<u8>> {
        let mut b = native_tls::TlsConnector::builder();
        if !verify {
            b.danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true);
        }
        let tls = b.build().ok()?.connect(host, open()?).ok()?;
        tls.peer_certificate().ok()??.to_der().ok()
    };
    let (der, ok) = match fetch(true) {
        Some(d) => (d, true),
        None => (fetch(false)?, false),
    };
    der_issuer(&der).map(|i| (i, ok))
}

/// 证书颁发者 → 一条结果。公共证书机构签的：正常；不是的话网络里有设备在解密 HTTPS（公司安全网关、杀毒软件、抓包工具）。
/// verified：本机这一方认不认它。与 cam.py 的 tls_issuer_item 一致（tests/shared/service.json）。
pub fn tls_issuer_item(host: &str, issuer: &Value, verified: bool) -> Value {
    let org = s(issuer, "org").filter(|v| !v.is_empty());
    let cn = s(issuer, "cn").filter(|v| !v.is_empty());
    let name = org.or(cn).unwrap_or("?");
    let low = [org, cn]
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if PUBLIC_CA_WORDS.iter().any(|w| low.contains(w)) {
        return json!({"kind": "tls_chain", "level": "ok",
                      "text": format!("HTTPS 证书：{} 由 {} 签发（公共证书机构，没有被中间设备解密）", host, name), "advice": ""});
    }
    json!({"kind": "tls_intercept", "level": "warn",
           "text": format!("HTTPS 证书：{} 由 {} 签发，不是公共证书机构：网络里有设备在解密 HTTPS{}", host, name,
                           if verified { "" } else { "，本机不认它" }),
           "advice": TLS_INTERCEPT_ADVICE})
}

/// 检测 Claude 相关服务和使用环境：DNS、出口地区、api.anthropic.com、claude.ai、续登录的地址、HTTPS 证书是谁签的、
/// 代理（设了的话它在不在运行）、VPN，再加上 risk_items 的各项（出口变化、时钟、官方状态页、时区、账号地区）。
/// 返回 [{kind, level, text, advice}]，级别为 ok / bad / warn / info，第一项是总体结论。不包含任何密钥。
/// 会顺便刷新界面常显的缓存和账号的地区记录。
pub fn service_check(store: &Store) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut add = |kind: &str, level: &str, text: String, advice: &str| {
        out.push(json!({"kind": kind, "level": level, "text": text, "advice": advice}))
    };
    let proxies: Vec<(String, String)> = [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .iter()
    .filter_map(|k| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| (k.to_string(), v))
    })
    .collect();
    let api_host = host_of(&api_base());
    let web_host = host_of(&web_base());
    let mut hosts = vec![api_host.clone()];
    if web_host != api_host {
        hosts.push(web_host.clone());
    }
    // 有登录就用当前登录实测 api.anthropic.com；读登录可能要问钥匙串，留在主线程做
    let at = read_live()
        .ok()
        .and_then(|(c, _)| c)
        .and_then(|c| s(&oauth_block(&c), "accessToken").map(String::from));
    // 解析域名、出口地区、api.anthropic.com、claude.ai 这几件事同时做，各有各的时限：哪件卡住了都不拖着别的
    let t0 = Instant::now();
    let within = |secs: u64| t0 + Duration::from_secs(secs);
    let j_dns: Vec<_> = hosts
        .iter()
        .map(|h| {
            let h = h.clone();
            spawn_job(move || resolve_host(&h))
        })
        .collect();
    let j_region = {
        let store = Store {
            root: store.root.clone(),
        };
        // 顺便更新界面常显的缓存和账号的地区记录
        spawn_job(move || check_region(&store, 0).is_ok())
    };
    let j_api = {
        let at = at.clone();
        spawn_job(move || {
            let t0 = Instant::now();
            let (st, body) = match &at {
                Some(t) => get_oauth(t, "/api/oauth/profile", SERVICE_PROBE_SECS),
                None => http_json_timeout(
                    "GET",
                    &format!("{}/api/oauth/usage", api_base()),
                    &[],
                    None,
                    SERVICE_PROBE_SECS,
                ),
            };
            (st, body, t0.elapsed().as_millis())
        })
    };
    let j_web = spawn_job(|| {
        let t0 = Instant::now();
        let (code, body) = get_with_in(
            &format!("{}/api/organizations", web_base()),
            &[("Cookie".to_string(), "sessionKey=".to_string())],
            Duration::from_secs(SERVICE_PROBE_SECS),
        );
        (code, body, t0.elapsed().as_millis())
    });
    // 续登录的地址、证书、本机设的代理也同时测
    let auth_urls = token_urls();
    let j_auth: Vec<_> = auth_urls
        .iter()
        .map(|u| {
            let u = u.clone();
            spawn_job(move || {
                let t0 = Instant::now();
                let (st, txt) = http_text_in(&u, Duration::from_secs(SERVICE_PROBE_SECS));
                (st, txt, t0.elapsed().as_millis())
            })
        })
        .collect();
    let mut proxy_entries = proxies.clone();
    proxy_entries.extend(
        system_proxy()
            .into_iter()
            .map(|p| ("系统设置".to_string(), p)),
    );
    let proxy_eps = proxy_endpoints(&proxy_entries);
    let j_proxy: Vec<_> = proxy_eps
        .iter()
        .map(|(_, h, p)| {
            let (h, p) = (h.clone(), *p);
            spawn_job(move || port_open(&h, p, Duration::from_secs(PROXY_PROBE_SECS)))
        })
        .collect();
    let j_tls = {
        let h = api_host.clone();
        let https = api_base().starts_with("https://");
        spawn_job(move || {
            if https {
                peer_issuer(&h, 443, Duration::from_secs(TLS_PROBE_SECS))
            } else {
                None
            }
        })
    };
    // 先等时限最长的那件（见 wait_job）
    let _ = wait_job(&j_region, within(REGION_CHECK_BUDGET + 2));
    let tls = wait_job(&j_tls, within(TLS_PROBE_SECS * 2 + 1)).flatten();
    let api = wait_job(&j_api, within(SERVICE_PROBE_SECS + 1));
    let web = wait_job(&j_web, within(SERVICE_PROBE_SECS + 1));
    let auth: Vec<(String, u16, String, u128)> = auth_urls
        .iter()
        .zip(&j_auth)
        .map(|(u, j)| {
            let (st, txt, ms) =
                wait_job(j, within(SERVICE_PROBE_SECS + 1)).unwrap_or((0, String::new(), 0));
            (host_of(u), st, txt, ms)
        })
        .collect();
    let proxy_why: Vec<String> = j_proxy
        .iter()
        .map(|j| {
            wait_job(j, within(PROXY_PROBE_SECS + 1))
                .unwrap_or_else(|| format!("超过 {} 秒没有回应", PROXY_PROBE_SECS))
        })
        .collect();
    let dns_advice = if proxies.is_empty() {
        "检查本机 DNS 或代理设置"
    } else {
        "检查本机 DNS 或代理设置；设了代理的话通常由代理解析，可忽略"
    };
    for (host, job) in hosts.iter().zip(&j_dns) {
        match wait_job(job, within(SERVICE_DNS_SECS)) {
            Some(Ok(ips)) if !ips.is_empty() => add(
                "dns",
                "ok",
                format!(
                    "DNS：{} → {}",
                    host,
                    ips.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                ),
                "",
            ),
            Some(Err(e)) => add(
                "dns",
                "bad",
                format!("DNS：{} 解析失败（{}）", host, e),
                dns_advice,
            ),
            _ => add(
                "dns",
                "bad",
                format!("DNS：{} 解析超过 {} 秒没有回应", host, SERVICE_DNS_SECS),
                dns_advice,
            ),
        }
    }
    // 出口地区：claude.ai 在 Cloudflare 后面，/cdn-cgi/trace 会返回它看到的访问者信息
    let data = store.load().unwrap_or_else(|_| json!({}));
    let r = data.get("region").cloned().unwrap_or_else(|| json!({}));
    match s(&r, "loc") {
        Some(l) => {
            let status = region_status(Some(l));
            let kept = match (s(&r, "failed_at"), s(&r, "checked_at")) {
                (Some(_), Some(at)) => format!("；这次没连上，这是{}查到的", fmt_ago(at)),
                _ => String::new(),
            };
            add(
                "region",
                if status == "supported" { "ok" } else { "warn" },
                format!(
                    "{}（IP {}{}，Cloudflare 节点 {}，{} ms{}）",
                    region_message(Some(l), status),
                    s(&r, "ip").unwrap_or("?"),
                    s(&r, "city")
                        .map(|c| format!("，{}", c))
                        .unwrap_or_default(),
                    s(&r, "colo").unwrap_or("?"),
                    r.get("ms").and_then(|v| v.as_u64()).unwrap_or(0),
                    kept
                ),
                region_advice(status),
            );
            // 和顶部环境卡同一条，两边分数才一致
            if let Some(f) = region_fail_item(&r) {
                add(
                    s(&f, "kind").unwrap_or(""),
                    s(&f, "level").unwrap_or("info"),
                    s(&f, "text").unwrap_or("").to_string(),
                    s(&f, "advice").unwrap_or(""),
                );
            }
        }
        None => add(
            "region_unknown",
            "warn",
            "出口地区：查不到（连不上 claude.ai 和 Cloudflare 的 trace 接口）".into(),
            region_advice("unknown"),
        ),
    }
    let asn = r
        .get("asn")
        .filter(|v| !v.is_null())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".into());
    let src_note = s(&r, "meta_source")
        .map(|m| format!("（数据：{}）", m))
        .unwrap_or_default();
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
        (Some(reg_loc), Some(l)) if reg_loc == l => add(
            "ip_origin",
            "ok",
            format!(
                "IP 归属：注册在 {}，与定位一致（原生 IP）",
                region_label(Some(reg_loc))
            ),
            "",
        ),
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
        (Some(v4), Some(v6), _) if v4 == v6 => add(
            "ipv6",
            "ok",
            format!(
                "IPv6：可用，出口 {}，地区与 IPv4 一致",
                s(&r, "v6_ip").unwrap_or("?")
            ),
            "",
        ),
        (_, None, Some(_)) => add(
            "ipv6",
            "ok",
            "IPv6：不可用，只有 IPv4 出口".into(),
            "不会出现 IPv6 漏出真实出口的问题",
        ),
        _ => {}
    }
    if let Some(dns) = r
        .get("env")
        .and_then(|e| e.get("dns"))
        .and_then(|d| d.as_array())
        .filter(|d| !d.is_empty())
    {
        let parts: Vec<String> = dns
            .iter()
            .map(|d| {
                let who = if d.get("private").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "局域网 / 本机".to_string()
                } else {
                    s(d, "name").unwrap_or("公共 DNS").to_string()
                };
                let c = s(d, "country")
                    .map(|c| format!("，{}", region_label(Some(c))))
                    .unwrap_or_default();
                format!("{}（{}{}）", s(d, "ip").unwrap_or("?"), who, c)
            })
            .collect();
        add(
            "dns_servers",
            "info",
            format!("系统 DNS 配置：{}", parts.join("；")),
            "这是系统配置中的解析器，不一定是请求实际使用的 DNS；TUN / 代理可能接管解析。未做 DNS 出口或泄漏探测，\
             不能用解析器的登记地区推断所在地；解析是否成功请看 DNS 实测项。",
        );
    }
    // api.anthropic.com
    let no_answer =
        json!({"error": {"message": format!("超过 {} 秒没有回应", SERVICE_PROBE_SECS)}});
    let (st, body, ms) = api.unwrap_or_else(|| (0, no_answer.clone(), 0));
    let msg = err_message(&body);
    let region_hint = "通常是出口地区不在 Anthropic 支持范围。Anthropic 按使用者实际所在地判断；人在支持地区的话，查一下代理 / VPN 把出口带到了哪里";
    if st == 0 {
        add(
            "api",
            "bad",
            format!("{}：连不上（{}）", api_host, msg),
            "检查网络或代理；公司网络可能屏蔽了这个域名",
        );
    } else if st == 403 || region_denied(&msg) {
        add(
            "api",
            "bad",
            format!("{}：拒绝访问（HTTP {}：{}）", api_host, st, or_dash(&msg)),
            region_hint,
        );
    } else if at.is_some() && st == 200 {
        add(
            "api",
            "ok",
            format!("{}：服务正常，当前登录可用（{} ms）", api_host, ms),
            "",
        );
    } else if at.is_some() && st == 401 {
        add(
            "api",
            "warn",
            format!(
                "{}：服务可达，但当前登录已过期或无效（{} ms）",
                api_host, ms
            ),
            "运行一次 claude 会自动续登录，不影响切换账号",
        );
    } else if st == 401 {
        add(
            "api",
            "ok",
            format!("{}：服务可达（未登录，{} ms）", api_host, ms),
            "",
        );
    } else if st == 429 {
        add(
            "api",
            "warn",
            format!("{}：官方在限流（HTTP 429，{} ms）", api_host, ms),
            RATE_LIMIT_ADVICE,
        );
    } else if st == 529 {
        add(
            "api",
            "warn",
            format!("{}：官方服务过载（HTTP 529，{} ms）", api_host, ms),
            OVERLOAD_ADVICE,
        );
    } else if st >= 500 {
        add(
            "api",
            "bad",
            format!("{}：服务端错误（HTTP {}）", api_host, st),
            "可能是 Anthropic 故障，稍后再试，可查看 status.claude.com",
        );
    } else {
        add(
            "api",
            "warn",
            format!("{}：HTTP {}（{}，{} ms）", api_host, st, or_dash(&msg), ms),
            "少见的返回，稍后重试；持续出现请把这一行发给作者",
        );
    }
    // claude.ai：不带登录，看可达性和有没有被网络管控拦截
    let (code, body, ms) = web.unwrap_or_else(|| (0, no_answer.clone(), 0));
    let msg = err_message(&body);
    if code == 0 {
        add(
            "web",
            "bad",
            format!("{}：连不上（{}）", web_host, msg),
            "检查网络或代理",
        );
    } else if is_cloudflare(&msg) {
        add(
            "web",
            "warn",
            format!("{}：返回人机验证页（{} ms）", web_host, ms),
            "公司网络或代理触发了 Cloudflare 人机验证；只影响网页账号读取额度，不影响 Claude Code 切换",
        );
    } else if region_denied(&msg) {
        add(
            "web",
            "bad",
            format!("{}：拒绝访问（HTTP {}：{}）", web_host, code, msg),
            region_hint,
        );
    } else if code == 200 || code == 401 || code == 403 {
        add("web", "ok", format!("{}：可达（{} ms）", web_host, ms), "");
    } else if code == 429 {
        add(
            "web",
            "warn",
            format!("{}：官方在限流（HTTP 429，{} ms）", web_host, ms),
            RATE_LIMIT_ADVICE,
        );
    } else if code >= 500 {
        add(
            "web",
            "bad",
            format!("{}：服务端错误（HTTP {}）", web_host, code),
            "稍后再试",
        );
    } else {
        add(
            "web",
            "warn",
            format!(
                "{}：HTTP {}（{}，{} ms）",
                web_host,
                code,
                or_dash(&msg),
                ms
            ),
            "少见的返回，稍后重试",
        );
    }
    drop(add);
    // 续登录的地址：Claude Code 和 cam 续登录都用，连不上的话登录到期后续不上
    out.push(auth_item(&auth));
    // 证书是谁签的：不是公共证书机构的话，网络里有设备在解密 HTTPS
    if let Some((issuer, verified)) = &tls {
        out.push(tls_issuer_item(&api_host, issuer, *verified));
    }
    // 代理：环境变量 + 系统设置（设了的话看它在不在运行）；VPN：默认路由
    let env = r
        .get("env")
        .filter(|e| e.get("proxy").is_some())
        .cloned()
        .unwrap_or_else(local_env);
    out.extend(proxy_vpn_items(&env));
    let mut up = Vec::new();
    for ((src, h, p), why) in proxy_eps.iter().zip(&proxy_why) {
        if why.is_empty() {
            up.push(format!("{}:{}", h, p));
        } else {
            out.push(proxy_down_item(src, h, *p, why));
        }
    }
    if !up.is_empty() {
        out.push(json!({"kind": "proxy_up", "level": "ok", "text": format!("代理在运行：{}", up.join("、")), "advice": ""}));
    }
    out.extend(risk_items(&data, false, true));
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
    let head = format!(
        "总体：{}/100，{}",
        sc["score"],
        s(&sc, "label").unwrap_or("")
    );
    // 出口这次没测到，别说成“在支持名单内”
    let fine = if out.iter().any(|i| s(i, "kind") == Some("env_failed")) {
        "；Claude 服务连得上，这次没测到出口地区，先按上一次查到的算，其它没有发现风险点"
    } else {
        "；Claude 服务正常，出口地区在支持名单内，没有发现风险点"
    };
    let summary = match &worst {
        Some(w) => json!({"kind": "summary", "level": level, "score": sc["score"], "grade": grade,
                          "text": format!("{} —— {}", head, s(w, "text").unwrap_or("")), "advice": w["advice"]}),
        None => json!({"kind": "summary", "level": level, "score": sc["score"], "grade": grade,
                       "text": format!("{}{}", head, fine), "advice": RISK_SCORE_ADVICE}),
    };
    out.insert(0, summary);
    // 连接问题记下来，界面顶部的环境卡和这里的分数保持一致
    let bad: Vec<Value> = out
        .iter()
        .filter(|i| {
            s(i, "kind").is_some_and(|k| SERVICE_STATUS_KINDS.contains(&k))
                && matches!(s(i, "level"), Some("warn" | "bad"))
        })
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
        src(
            "claude.ai/cdn-cgi/trace",
            "出口国家、IP、时钟。访问 Claude 时它本来就看得到",
            "anthropic",
            false,
        ),
        src(
            "status.claude.com",
            "官方状态页（连不上时换 status.anthropic.com），不带任何本机信息",
            "anthropic",
            false,
        ),
    ];
    if cfg.mode == "off" {
        return out;
    }
    if cfg.mode == "ip-api" {
        out.push(src(
            "ip-api.com（http）",
            "运营商、城市、机房 / 代理标记。免费接口仅限非商业用途",
            "third",
            true,
        ));
    }
    let ipapi = if cfg.ipapi_key.is_empty() {
        "运营商、城市（没配 key：每天 30 次，不带类型标记）"
    } else {
        "运营商、城市、机房 / VPN / Tor 标记"
    };
    out.push(src("api.ipapi.is", ipapi, "third", false));
    out.push(src(
        "ipinfo.io",
        "上一家查不到时：运营商、城市",
        "third",
        false,
    ));
    out.push(src(
        "speed.cloudflare.com",
        "再查不到时：运营商、城市",
        "third",
        false,
    ));
    out.push(src("rdap.org", "出口 IP 的注册国", "third", false));
    out.push(src(
        "1.1.1.1 / Cloudflare IPv6",
        "分别测 IPv4、IPv6 出口",
        "third",
        false,
    ));
    out.push(src(
        "www.cloudflare.com",
        "claude.ai 连不上或两秒没回音时，代替它看出口国家",
        "third",
        false,
    ));
    out
}

pub fn privacy_view(store: &Store) -> R<Value> {
    let cfg = ip_lookup_config(Some(store));
    let data = store.load()?;
    let n = accounts(&data)
        .iter()
        .filter(|a| {
            a.get("regions")
                .and_then(|r| r.as_array())
                .map(|r| !r.is_empty())
                .unwrap_or(false)
        })
        .count();
    let modes: Vec<Value> = IP_LOOKUP_MODES
        .iter()
        .map(|(v, l)| {
            let (_, title, desc) = IP_LOOKUP_TITLES
                .iter()
                .find(|(k, _, _)| k == v)
                .copied()
                .unwrap_or((v, v, ""));
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
            let cur = data
                .get("prefs")
                .and_then(|p| s(p, "ip_lookup"))
                .unwrap_or("auto")
                .to_string();
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

#[cfg(test)]
mod proxy_mask_tests {
    use super::mask_proxy;

    #[test]
    fn hides_credentials_only() {
        assert_eq!(
            mask_proxy("http://alice:S3cretPass@proxy.local:8080"),
            "http://***@proxy.local:8080"
        );
        assert_eq!(mask_proxy("user:pw@10.0.0.1:3128"), "***@10.0.0.1:3128");
        assert_eq!(mask_proxy("http://127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(mask_proxy("HTTPS 127.0.0.1:7890"), "HTTPS 127.0.0.1:7890");
        assert_eq!(mask_proxy("http://u:p@host:port"), "http://***@host:port"); // 端口不是数字也照样遮
    }
}

#[cfg(test)]
mod job_tests {
    use super::{spawn_job, wait_job};
    use std::time::{Duration, Instant};

    #[test]
    fn waits_no_longer_than_each_limit() {
        // 对应 test_cam.py 的 test_run_parallel_budget_and_limits
        fn ms(n: u64) -> Duration {
            Duration::from_millis(n)
        }
        let stuck = || spawn_job(|| std::thread::sleep(ms(3000)));
        assert_eq!(
            wait_job(&spawn_job(|| 1), Instant::now() + ms(2000)),
            Some(1)
        );
        // 到了时限还没做完的那件是 None，不接着等
        let t0 = Instant::now();
        assert_eq!(wait_job(&stuck(), t0 + ms(200)), None);
        assert!(t0.elapsed() < ms(2000));
        // 时限不一样的几件，先等时限长的：慢一点但在自己时限内的照样等到；
        // 时限短的那件早做完了，过了点也收得到；卡住的那件不再多等
        let t0 = Instant::now();
        let slowish = spawn_job(|| {
            std::thread::sleep(ms(300));
            "done"
        });
        let (fast, hung) = (spawn_job(|| 1), stuck());
        assert_eq!(wait_job(&slowish, t0 + ms(5000)), Some("done"));
        assert_eq!(wait_job(&fast, t0 + ms(100)), Some(1));
        assert_eq!(wait_job(&hung, t0 + ms(100)), None);
        assert!(t0.elapsed() < ms(2000));
        // 自己崩了的那件也是 None
        let boom = spawn_job(|| -> i32 { panic!("boom") });
        assert_eq!(wait_job(&boom, Instant::now() + ms(2000)), None);
    }
}

#[cfg(test)]
mod region_note_tests {
    use super::note_account_region;
    use serde_json::json;

    #[test]
    fn merges_same_region_and_keeps_details() {
        // 同一地区只留一条：次数累加、首次时间不变；IP 这些有新的用新的，没有就留旧的
        let mut data = json!({"accounts": [{"id": "a", "regions": [
            {"loc": "US", "at": "2026-01-01T00:00:00Z", "first_at": "2025-12-01T00:00:00Z", "times": 7, "ip": "1.2.3.4", "org": "Old ISP"}]}]});
        note_account_region(
            &mut data,
            Some("a"),
            Some("US"),
            Some(&json!({"ip": "5.6.7.8", "city": "Seattle"})),
        );
        let r = &data["accounts"][0]["regions"];
        assert_eq!(r.as_array().unwrap().len(), 1);
        assert_eq!(r[0]["times"], 8);
        assert_eq!(r[0]["first_at"], "2025-12-01T00:00:00Z");
        assert_eq!(r[0]["ip"], "5.6.7.8");
        assert_eq!(r[0]["org"], "Old ISP");
        assert_eq!(r[0]["city"], "Seattle");
        note_account_region(&mut data, Some("a"), Some("SG"), None);
        let r = &data["accounts"][0]["regions"];
        assert_eq!(
            (
                r[0]["loc"].as_str(),
                r[0]["times"].as_i64(),
                r[1]["loc"].as_str()
            ),
            (Some("SG"), Some(1), Some("US"))
        );
    }
}
