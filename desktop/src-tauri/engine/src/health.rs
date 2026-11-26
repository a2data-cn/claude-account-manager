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
/// 官方名单写的是国家；这些属地按所属国家看待（美国、英国、法国、荷兰、丹麦、芬兰、挪威、新西兰、澳大利亚）。
/// 香港、澳门不是属地，不在名单里就是不支持
pub const REGION_DEPENDENCIES: &[&str] = &[
    "PR", "GU", "VI", "AS", "MP", "UM", "GI", "IM", "JE", "GG", "BM", "KY", "FK", "VG", "AI", "MS",
    "TC", "SH", "IO", "PN", "GS", "RE", "GP", "MQ", "GF", "YT", "NC", "PF", "PM", "WF", "BL", "MF",
    "TF", "AW", "CW", "SX", "BQ", "GL", "FO", "AX", "SJ", "BV", "CK", "NU", "TK", "CX", "CC", "NF",
    "HM",
];
/// 上面的名单是哪个月对着官方页面核对的
pub const REGIONS_CHECKED: &str = "2026-10";
pub const REGIONS_URL: &str = "https://www.anthropic.com/supported-countries";
/// 不在名单里的国家和地区（地区卡上列出来）
pub const REGIONS_UNSUPPORTED: &[&str] = &[
    "CN", "HK", "MO", "RU", "BY", "IR", "KP", "SY", "CU", "VE", "AF", "MM", "YE", "XK",
];
/// 地区卡上的几条说明（cam regions 也打印）。与 cam.py 的 REGION_NOTES 一致（tests/shared/regions.json）
pub const REGION_NOTES: &[&str] = &[
    "名单以 Anthropic 官方公布的为准，claude.ai、Claude Code 和 API 用同一份；没在名单里的就是不支持。",
    "支持国家的海外属地（波多黎各、关岛、直布罗陀等）按所属国家算；香港、澳门不在名单里，不支持。",
    "乌克兰在名单里，但被占领的克里米亚、顿涅茨克、卢甘斯克、赫尔松、扎波罗热地区不支持；IP 只能定位到国家，这里分不出来。",
    "2025 年 9 月 4 日起，被不支持地区的公司直接或间接控股超过 50% 的企业也不能用；这是账号层面的，本机查不出来。",
    "Anthropic 按使用者实际所在地判断，不只看出口 IP；这里只看本机能看到的出口，只提醒，不拦任何操作。",
];
const UA_REGION_NOTE: &str = "乌克兰被占领的克里米亚、顿涅茨克、卢甘斯克、赫尔松、扎波罗热地区不在支持范围内；IP 只能定位到国家，这里分不出来。";
const DOMAIN_REGION_ADVICE: &str = "Claude Code 的请求发往 api.anthropic.com，续登录走 platform.claude.com，Anthropic 在这几处看到的是各自的出口。\
    多半是代理按域名分流了，或者 IPv4 / IPv6 走了不同出口。几个出口都在支持地区就不影响使用；\
    同一账号的请求来自不同地区，是常见的风控信号之一。";
const DOMAIN_REGION_BAD_ADVICE: &str = "Claude Code 的请求发往 api.anthropic.com，续登录走 platform.claude.com，这里看到的出口不在支持名单内，\
    按 Anthropic 政策这个地区不提供服务。Anthropic 按使用者实际所在地判断：人在支持地区的话，\
    多半是代理 / VPN 只接管了一部分地址，查一下它的设置；人在不支持地区的话，风险与出口无关。";

/// 支持地区卡用的名单和说明（不联网），代码都是 ISO 3166-1 两位字母。与 cam.py 的 regions_info 一致（tests/shared/regions.json）
pub fn regions_info() -> Value {
    let mut sup: Vec<&str> = SUPPORTED_REGIONS.to_vec();
    sup.sort_unstable();
    let mut dep: Vec<&str> = REGION_DEPENDENCIES.to_vec();
    dep.sort_unstable();
    json!({"supported": sup, "dependencies": dep, "unsupported": REGIONS_UNSUPPORTED,
           "notes": REGION_NOTES, "checked": REGIONS_CHECKED, "url": REGIONS_URL})
}
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
    ("XK", "科索沃"),
    ("UA", "乌克兰"),
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
const RATE_LIMIT_ADVICE: &str = "Anthropic 那边在限流（短时间请求太多），不是网络或账号的问题，只提醒、不扣环境评分；过几分钟再试。";
const OVERLOAD_ADVICE: &str = "Anthropic 那边负载太高，不是网络或账号的问题，只提醒、不扣环境评分；稍后再试，可查看 status.claude.com。";
const SERVER_ERROR_ADVICE: &str = "多半是 Anthropic 那边的故障，不是本机环境的问题，只提醒、不扣环境评分；稍后再试，可查看 status.claude.com。";
/// 用这几条说明的检测项是关注项（Anthropic 那边的状况：限流、过载、服务端错误）：标出来但不扣环境评分，条目上带 notice。
const NOTICE_ADVICE: &[&str] = &[RATE_LIMIT_ADVICE, OVERLOAD_ADVICE, SERVER_ERROR_ADVICE];
/// 本机解析不了域名、但实际连得上时的说明。与 cam.py 的 DNS_VIA_PROXY_ADVICE 一致
const DNS_VIA_PROXY_ADVICE: &str =
    "请求走的是代理，域名由代理解析，本机解析不了不影响使用，不扣分。";
const STATUS_ADVICE: &str =
    "这是 Anthropic 那边的故障，不是本机或账号的问题，只提醒、不扣环境评分；等恢复后再试，详情见 status.claude.com。";
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

// ---------------------------------------------------------------- 检测进度

/// “重新检测”要走的步骤（界面上进度卡的每一行）：键 → 写给人看的名字，顺序就是显示的顺序。
/// 与 cam.py 的 SERVICE_STEPS 一致（tests/shared/progress.json）
pub const SERVICE_STEPS: &[(&str, &str)] = &[
    (
        "local",
        "本机设置和安全项：时区、语言、DNS、hosts、代理、Wi-Fi、命令历史、钩子",
    ),
    ("dns", "解析 Claude 的域名"),
    ("exit", "出口 IP 和地区"),
    ("domains", "Claude Code 的 API、续登录的地址看到的地区"),
    ("isp", "运营商、IP 类型、注册国"),
    ("stack", "IPv4 / IPv6 出口"),
    ("plain", "没加密的网页有没有被改动"),
    ("api", "连接 api.anthropic.com"),
    ("web", "连接 claude.ai"),
    ("auth", "续登录的地址"),
    ("tls", "HTTPS 证书是谁签的"),
    ("proxy", "本机设的代理在不在运行"),
    ("status", "Anthropic 官方状态页"),
    ("score", "汇总结果，算环境评分"),
];
/// “外部 IP 查询”关掉时不做的几步
const LOOKUP_STEPS: &[&str] = &["isp", "stack", "plain"];
/// check_region 里做的几步
const REGION_STEPS: &[&str] = &[
    "local", "exit", "domains", "isp", "stack", "plain", "status",
];
/// 留着最近几次检测的进度
const PROGRESS_KEEP: usize = 6;

/// 这次“重新检测”要走哪几步：外部 IP 查询关了就没有问第三方的那几步，没设代理就没有代理那一步。
pub fn service_steps(lookup_off: bool, has_proxy: bool) -> Vec<(&'static str, &'static str)> {
    SERVICE_STEPS
        .iter()
        .filter(|(k, _)| !(lookup_off && LOOKUP_STEPS.contains(k)) && (has_proxy || *k != "proxy"))
        .copied()
        .collect()
}

struct ProgStep {
    key: String,
    label: String,
    state: &'static str,
    ms: Option<u64>,
    note: String,
    rank: i64,
    t0: Option<Instant>,
    pending: i64,
    ok: bool,
}

struct ProgInner {
    t0: Instant,
    steps: Vec<ProgStep>,
    total: Option<u64>,
}

/// 一次检测的进度，给界面的进度卡用：每一步是在等（wait）、在做（run）、做完了（ok，带用了多少毫秒）、
/// 没连上 / 没查到（fail）、超过时限没等到（timeout），还是这次不用做（skip）。只有步骤名和状态，不带任何检测到的内容。
/// 一步里可以有几件事同时做（start 几次、finish 几次），全做完才算完，有一件没成就是 fail；
/// 已经有了结论的步骤，晚到的 finish 不再改它。没人看进度时用 Progress::none()，什么都不记。对应 cam.py 的 CheckProgress。
#[derive(Clone, Default)]
pub struct Progress(Option<std::sync::Arc<std::sync::Mutex<ProgInner>>>);

impl Progress {
    pub fn new() -> Self {
        Progress(Some(std::sync::Arc::new(std::sync::Mutex::new(
            ProgInner {
                t0: Instant::now(),
                steps: Vec::new(),
                total: None,
            },
        ))))
    }

    pub fn none() -> Self {
        Progress(None)
    }

    /// 真的在记（不是 none 那种空壳）
    pub fn live(&self) -> bool {
        self.0.is_some()
    }

    fn with<T>(&self, f: impl FnOnce(&mut ProgInner) -> T) -> Option<T> {
        let inner = self.0.as_ref()?;
        let mut g = inner.lock().unwrap_or_else(|e| e.into_inner());
        Some(f(&mut g))
    }

    /// 登记一步；已经有了就不动。rank 小的排前面，一样的按登记的先后。
    pub fn add(&self, key: &str, label: &str, rank: i64) {
        self.with(|p| {
            if !p.steps.iter().any(|s| s.key == key) {
                p.steps.push(ProgStep {
                    key: key.to_string(),
                    label: label.to_string(),
                    state: "wait",
                    ms: None,
                    note: String::new(),
                    rank,
                    t0: None,
                    pending: 0,
                    ok: true,
                });
            }
        });
    }

    pub fn start(&self, key: &str) {
        self.with(|p| {
            if let Some(s) = p.steps.iter_mut().find(|s| s.key == key) {
                if s.state == "wait" {
                    s.state = "run";
                    s.t0 = Some(Instant::now());
                }
                if s.state == "run" {
                    s.pending += 1;
                }
            }
        });
    }

    /// 这一步里的一件事做完了；ok 为假时 note 写一句是怎么没成的（“没连上”“解析失败”）。
    pub fn finish(&self, key: &str, ok: bool, note: &str) {
        self.with(|p| {
            if let Some(s) = p
                .steps
                .iter_mut()
                .find(|s| s.key == key && s.state == "run")
            {
                s.pending -= 1;
                if !ok {
                    s.ok = false;
                    if !note.is_empty() {
                        s.note = note.to_string();
                    }
                }
                if s.pending <= 0 {
                    s.state = if s.ok { "ok" } else { "fail" };
                    s.ms = s.t0.map(|t| t.elapsed().as_millis() as u64);
                }
            }
        });
    }

    /// 这一步这次不用做（还没开始的才算）。
    pub fn skip(&self, key: &str, note: &str) {
        self.with(|p| {
            if let Some(s) = p
                .steps
                .iter_mut()
                .find(|s| s.key == key && s.state == "wait")
            {
                s.state = "skip";
                s.note = note.to_string();
            }
        });
    }

    /// 到时限了：这几步里还在做的记成没等到。
    pub fn expire(&self, keys: &[&str]) {
        self.with(|p| {
            for s in p.steps.iter_mut() {
                if s.state == "run" && keys.contains(&s.key.as_str()) {
                    s.state = "timeout";
                    s.ms = s.t0.map(|t| t.elapsed().as_millis() as u64);
                }
            }
        });
    }

    /// 整次检测结束：还在做的记成没等到，没轮到的记成没做。
    pub fn close(&self) {
        self.with(|p| {
            for s in p.steps.iter_mut() {
                if s.state == "run" {
                    s.state = "timeout";
                    s.ms = s.t0.map(|t| t.elapsed().as_millis() as u64);
                } else if s.state == "wait" {
                    s.state = "skip";
                }
            }
            if p.total.is_none() {
                p.total = Some(p.t0.elapsed().as_millis() as u64);
            }
        });
    }

    /// {"steps": [{"key", "label", "state", "ms", "note"}], "done", "ms"}，ms 是到现在（或到结束）一共用了多久。
    pub fn view(&self) -> Value {
        self.with(|p| {
            let mut rows: Vec<&ProgStep> = p.steps.iter().collect();
            rows.sort_by_key(|s| s.rank); // 稳定排序：rank 一样的保持登记的先后
            let steps: Vec<Value> = rows
                .iter()
                .map(|s| json!({"key": s.key, "label": s.label, "state": s.state, "ms": s.ms, "note": s.note}))
                .collect();
            json!({
                "steps": steps,
                "done": p.total.is_some(),
                "ms": p.total.unwrap_or_else(|| p.t0.elapsed().as_millis() as u64)
            })
        })
        .unwrap_or_else(|| json!({"steps": [], "done": false, "ms": 0}))
    }
}

fn progress_runs() -> &'static std::sync::Mutex<Vec<(String, Progress)>> {
    static RUNS: std::sync::OnceLock<std::sync::Mutex<Vec<(String, Progress)>>> =
        std::sync::OnceLock::new();
    RUNS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// 给一次检测开一份进度记录。run 是界面自己生成的随机串（字母数字、_、-，4–40 位），没给或者不合规矩就不记。
pub fn progress_open(run: Option<&str>) -> Progress {
    let Some(run) = run.filter(|r| {
        (4..=40).contains(&r.len())
            && r.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }) else {
        return Progress::none();
    };
    let prog = Progress::new();
    let mut runs = progress_runs().lock().unwrap_or_else(|e| e.into_inner());
    runs.retain(|(r, _)| r != run);
    runs.push((run.to_string(), prog.clone()));
    while runs.len() > PROGRESS_KEEP {
        runs.remove(0);
    }
    prog
}

/// GET /api/progress?run=…：那次检测现在做到哪了。没有这次（还没开始、太久以前的）返回空的步骤表。
pub fn progress_view(run: &str) -> Value {
    let runs = progress_runs().lock().unwrap_or_else(|e| e.into_inner());
    runs.iter()
        .find(|(r, _)| r == run)
        .map(|(_, p)| p.clone())
        .unwrap_or_default()
        .view()
}

/// spawn_job 外面包一层记进度：现在就算开始，做完时按 good(结果) 记成做完还是没成（note 写怎么没成的）。
fn tracked<T: Send + 'static>(
    prog: &Progress,
    key: &'static str,
    note: &'static str,
    good: impl Fn(&T) -> bool + Send + 'static,
    f: impl FnOnce() -> T + Send + 'static,
) -> mpsc::Receiver<T> {
    prog.start(key);
    let prog = prog.clone();
    spawn_job(move || {
        let res = f();
        prog.finish(key, good(&res), note);
        res
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
    detect_region_prog(cfg, prev, &Progress::none())
}

/// detect_region_with 的本体；prog 给了就把 exit / isp / stack 这几步做到哪了记进去。
fn detect_region_prog(cfg: &LookupConfig, prev: &Value, prog: &Progress) -> Option<Value> {
    let off = cfg.mode == "off";
    prog.start("exit");
    let traced = exit_trace(Duration::from_secs(6), !off);
    prog.finish("exit", traced.is_some(), "没查到");
    let Some(mut out) = traced else {
        for k in ["domains", "isp", "stack"] {
            prog.skip(k, "出口没查到，这一步没做");
        }
        return None;
    };
    for k in META_FIELDS.iter().chain(STACK_FIELDS) {
        out[*k] = Value::Null;
    }
    out["lookup_mode"] = json!(cfg.mode);
    // Claude Code 用的 API、续登录的地址各自看到的出口地区：问的是 Anthropic 自己的地址，“外部 IP 查询”关掉也照问
    let doms = domain_trace_urls();
    if doms.is_empty() {
        prog.skip("domains", "没有别的地址要问");
    }
    let dom_jobs: Vec<(String, TraceJob)> = doms
        .into_iter()
        .map(|(h, u)| {
            let job = tracked(
                prog,
                "domains",
                "没查到",
                |v: &Option<(Option<String>, Option<String>)>| {
                    v.as_ref().is_some_and(|x| x.1.is_some())
                },
                move || trace_only(&u),
            );
            (h, job)
        })
        .collect();
    let domains = |end: Instant| -> Value {
        let got: Vec<Value> = dom_jobs
            .iter()
            .filter_map(|(h, j)| match wait_job(j, end).flatten() {
                Some((ip, Some(loc))) => Some(json!({"host": h, "ip": ip, "loc": loc})),
                _ => None,
            })
            .collect();
        json!(got)
    };
    if off {
        out["domains"] = domains(Instant::now() + meta_budget());
        prog.expire(&["domains"]);
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
    // 没有 IPv6 的网络很常见，IPv6 那一问答不上来不算这一步没成
    let j4 = tracked(
        prog,
        "stack",
        "没查到",
        |v: &Option<(Option<String>, Option<String>)>| v.is_some(),
        || trace_only(&cf_trace4_url()),
    );
    let j6 = tracked(
        prog,
        "stack",
        "",
        |_: &Option<(Option<String>, Option<String>)>| true,
        || trace_only(&cf_trace6_url()),
    );
    if reuse {
        prog.skip("isp", "出口 IP 没变，沿用上次查到的");
    }
    let jm = (!reuse).then(|| {
        let cfg = cfg.clone();
        tracked(
            prog,
            "isp",
            "没查到",
            |v: &Option<Value>| v.is_some(),
            move || fetch_exit_meta(&cfg),
        )
    });
    // 注册国常常查不到，不算没成
    let jr = need_rdap.then(|| {
        let ip = ip.clone();
        tracked(
            prog,
            "isp",
            "",
            |_: &Option<String>| true,
            move || fetch_ip_registration(&ip, Duration::from_secs(5)),
        )
    });
    let end = t0 + budget;
    let v4 = wait_job(&j4, end).flatten();
    let v6 = wait_job(&j6, end).flatten();
    let meta = jm.and_then(|j| wait_job(&j, end)).flatten();
    let rdap = jr.and_then(|j| wait_job(&j, end)).flatten();
    out["domains"] = domains(end);
    prog.expire(&["domains", "isp", "stack"]);
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
type TraceJob = mpsc::Receiver<Option<(Option<String>, Option<String>)>>;

/// 除了 claude.ai，还要问出口地区的 Anthropic 地址 [(主机, trace 地址)]：Claude Code 用的 API、续登录的地址。
/// 代理可能按域名分流，Anthropic 在这几处看到的出口不一定一样。和 claude.ai（或彼此）是同一个地址的不重复问。
/// 与 cam.py 的 domain_trace_urls 一致。
pub fn domain_trace_urls() -> Vec<(String, String)> {
    let netloc = |u: &url::Url| -> Option<String> {
        let h = u.host_str()?;
        Some(match u.port() {
            Some(p) => format!("{}:{}", h, p),
            None => h.to_string(),
        })
    };
    let mut seen: Vec<String> = url::Url::parse(&crate::usage::web_base())
        .ok()
        .and_then(|u| netloc(&u))
        .into_iter()
        .collect();
    let mut out = Vec::new();
    let token = crate::usage::token_urls()
        .into_iter()
        .next()
        .unwrap_or_default();
    for base in [crate::usage::api_base(), token] {
        let Ok(u) = url::Url::parse(&base) else {
            continue;
        };
        let Some(nl) = netloc(&u) else { continue };
        if (u.scheme() == "http" || u.scheme() == "https") && !seen.contains(&nl) {
            out.push((
                u.host_str().unwrap_or("").to_string(),
                format!("{}://{}/cdn-cgi/trace", u.scheme(), nl),
            ));
            seen.push(nl);
        }
    }
    out
}

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

// ---- 域名有没有被人动过手脚（木马、来路不明的“加速器 / 中转工具”常改 hosts 文件，把 Claude 的域名指到自己的服务器，
// 流量连同登录凭据就送过去了）。只读本机，不改任何东西。与 cam.py 一致（tests/shared/hosts.json）

/// 这几个域名和它们的子域名：Claude Code 官方文档“Network access requirements”里 Anthropic 自己的那几个
const CLAUDE_DOMAINS: &[&str] = &[
    "anthropic.com",
    "claude.ai",
    "claude.com",
    "claudeusercontent.com",
];
/// hosts 文件里最多记几条
const HOSTS_MAX: usize = 8;
const HOSTS_ADVICE: &str = "正常用不着这样设：发往这个域名的请求（带着登录凭据）会被送到那个地址。可能是第三方中转 / 加速工具写的，也可能是恶意软件改的。\
                            不是你自己设的就把这几行删掉（Windows：C:\\Windows\\System32\\drivers\\etc\\hosts；macOS / Linux：/etc/hosts，要管理员权限），\
                            再用杀毒软件查一遍。";
const DNS_SPECIAL_ADVICE: &str =
    "装了代理软件（TUN / fake-ip）、安全软件或者家长控制时会这样：是它们在替你解析，不扣分。\
                                  你没装这类软件的话，检查 hosts 文件和系统的 DNS 设置。";

/// 系统的 hosts 文件在哪。测试用 CAM_TEST_HOSTS 指过去（空字符串：当没有）。
fn hosts_file_path() -> Option<std::path::PathBuf> {
    if let Ok(t) = std::env::var("CAM_TEST_HOSTS") {
        return if t.is_empty() { None } else { Some(t.into()) };
    }
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        return Some(
            std::path::Path::new(&root)
                .join("System32")
                .join("drivers")
                .join("etc")
                .join("hosts"),
        );
    }
    Some("/etc/hosts".into())
}

/// hosts 文件里指到 Claude / Anthropic 域名（CLAUDE_DOMAINS 和它们的子域名）的条目 [{host, ip}]，最多 HOSTS_MAX 条。
/// 同一个域名只留第一条（系统用的就是第一条）；# 后面是注释。与 cam.py 的 parse_hosts 一致。
pub fn parse_hosts(text: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut parts = line.split_whitespace();
        let Some(ip) = parts.next() else { continue };
        for name in parts {
            let h = name.to_lowercase();
            let h = h.trim_end_matches('.').to_string();
            let ours = CLAUDE_DOMAINS
                .iter()
                .any(|d| h == *d || h.ends_with(&format!(".{}", d)));
            if ours && !seen.contains(&h) {
                seen.push(h.clone());
                out.push(json!({"host": h, "ip": ip}));
            }
        }
    }
    out.truncate(HOSTS_MAX);
    out
}

/// 这台电脑的 hosts 文件里把 Claude / Anthropic 的域名指到了哪（parse_hosts）；没有、读不了返回空的。
fn hosts_overrides() -> Vec<Value> {
    let Some(path) = hosts_file_path() else {
        return Vec::new();
    };
    match std::fs::read(&path) {
        Ok(raw) => {
            let raw = &raw[..raw.len().min(1_000_000)];
            parse_hosts(&String::from_utf8_lossy(raw))
        }
        Err(_) => Vec::new(),
    }
}

/// 解析出来的地址是不是内网 / 保留地址：回环、私有、链路本地、运营商内网（100.64.0.0/10）、测试用的 198.18.0.0/15、
/// 0 开头和 224 以上，IPv6 的 ::、::1、fc00::/7、fe80::/10（映射的 IPv4 按 IPv4 看）。Claude 的域名解析成这样，
/// 是解析被本机的软件接管了。与 cam.py 的 addr_special 一致。
pub fn addr_special(ip: &str) -> bool {
    let ip = ip.split('%').next().unwrap_or("");
    let v4 = |b: [u8; 4]| {
        matches!(b[0], 0 | 10 | 127)
            || b[0] >= 224
            || (b[0] == 100 && (64..=127).contains(&b[1]))
            || (b[0] == 169 && b[1] == 254)
            || (b[0] == 172 && (16..=31).contains(&b[1]))
            || (b[0] == 192 && b[1] == 168)
            || (b[0] == 198 && (b[1] == 18 || b[1] == 19))
    };
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => v4(a.octets()),
        Ok(std::net::IpAddr::V6(a)) => match a.to_ipv4_mapped() {
            Some(m) => v4(m.octets()),
            None => {
                let n = u128::from(a);
                n <= 1 || (n >> 121) == 0x7E || (n >> 118) == 0x3FA
            }
        },
        Err(_) => false,
    }
}

const CREDS_PERM_ADVICE: &str =
    "凭据文件里是登录用的令牌，别的用户读到就能用你的账号。改成只有自己能读：chmod 600 ";
const CREDS_PERM_ADVICE_TAIL: &str =
    "（Claude Code 自己写这个文件时就是 600；被改宽多半是复制、解压或者同步工具造成的）。";

/// Claude Code 的登录凭据文件（.credentials.json）这台电脑上别的用户读不读得到：读得到返回它的权限（"644" 这样），
/// 只有自己能读、没有这个文件（macOS 上登录存在钥匙串里）、Windows（权限不是这么管的）返回 None。只看，不改。
/// 与 cam.py 的 creds_file_mode 一致。
fn creds_file_mode() -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(crate::claude::credentials_file())
            .ok()?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Some(format!("{:o}", mode));
        }
    }
    None
}

/// 路径里的主目录写成 ~（给人看的）。
fn tilde_path(p: &std::path::Path) -> String {
    let full = p.display().to_string();
    match dirs::home_dir() {
        Some(h) => {
            let h = h.display().to_string();
            match full.strip_prefix(&h) {
                Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
                    format!("~{}", rest)
                }
                _ => full,
            }
        }
        None => full,
    }
}

// ---- 安全项（只看、不改、不显示内容）：命令历史里留着的密钥、Claude Code 设成不问就执行、会自动运行命令的钩子和 MCP 服务器、
// ~/.claude.json 的权限、正在运行的远程控制软件、对局域网开放的 cam 面板和本机代理。都在 local_env 里查，和出口一起缓存。
// 与 cam.py 的同名函数一致（tests/shared/security.json）

const KEY_KINDS: &[(&str, &str)] = &[
    ("api", "API Key"),
    ("admin", "管理 Key"),
    ("sid", "网页登录凭据"),
    ("oat", "登录凭据"),
    ("ort", "登录凭据"),
];
/// 每个历史文件只看最后这么多
const HISTORY_SCAN_BYTES: u64 = 4 * 1024 * 1024;
const HISTORY_FILES: &[&str] = &[
    ".bash_history",
    ".zsh_history",
    ".histfile",
    ".sh_history",
    ".local/share/fish/fish_history",
    ".local/share/powershell/PSReadLine/ConsoleHost_history.txt",
];
const HOOK_RUNNERS: &[&str] = &[
    "python",
    "python3",
    "py",
    "node",
    "bash",
    "sh",
    "zsh",
    "fish",
    "pwsh",
    "powershell",
    "cmd",
    "npx",
    "npm",
    "uvx",
    "uv",
    "bunx",
    "bun",
    "deno",
    "env",
    "sudo",
    "exec",
    "nohup",
    "run",
];
/// 远程控制软件：(进程名，小写、不带 .exe) → 给人看的名字
const REMOTE_APPS: &[(&str, &str)] = &[
    ("todesk", "ToDesk"),
    ("todesk_service", "ToDesk"),
    ("sunloginclient", "向日葵"),
    ("awesun", "向日葵"),
    ("teamviewer", "TeamViewer"),
    ("teamviewer_service", "TeamViewer"),
    ("teamviewerd", "TeamViewer"),
    ("anydesk", "AnyDesk"),
    ("rustdesk", "RustDesk"),
    ("quickassist", "Windows 快速助手"),
    ("remoting_host", "Chrome 远程桌面"),
    ("remoting_me2me_host", "Chrome 远程桌面"),
    ("srserver", "Splashtop"),
    ("parsecd", "Parsec"),
    ("winvnc", "VNC"),
    ("tvnserver", "VNC"),
    ("x11vnc", "VNC"),
    ("xrdp", "xrdp"),
];
/// 拿本机的局域网地址连面板、代理的端口，最多等多久
const LAN_PROBE_SECS: u64 = 1;
/// 钩子、MCP 服务器各列几个名字
const AUTO_RUN_SHOWN: usize = 6;
const ACTIVE_SESSIONS_TIP: &str = "到 claude.ai 的“设置 → 账号 → 活动会话”（claude.ai/settings/account）看看登录着的设备，有不认识的就结束它。";
const HISTORY_KEY_ADVICE: &str = "在命令行里直接写的密钥会被记进历史文件，拿到这台电脑、备份或同步盘的人都能看到。先关掉所有终端窗口（不然退出时又写回去），\
    再删掉带 sk-ant- 的那几行：macOS / Linux 运行 sed -i.bak '/sk-ant-/d' 文件路径（改前留一份 .bak，确认没问题再删掉它，\
    里面还有密钥）；Windows 在 PowerShell 里运行 $p = (Get-PSReadLineOption).HistorySavePath; \
    (Get-Content $p) -notmatch 'sk-ant-' | Set-Content $p。API Key 和管理 Key 长期有效，到 Console 的 API Keys 页面作废后新建一个；\
    登录凭据会自动过期换新，不放心就到 claude.ai 的“设置 → 账号 → 活动会话”（claude.ai/settings/account）看看登录着的设备，有不认识的就结束它，再重新登录。\
    cam 不会替你改这些文件。";
const PERM_BYPASS_ADVICE: &str = "这样 Claude Code 跑任何命令、改任何文件都不再问你。它读到的网页、文件、代码仓库里要是藏着恶意指令（提示注入），\
    可能直接删文件或把密钥发出去。只在虚拟机、容器这类隔离环境里这样用；平时去掉这一项，常用的命令在 permissions.allow 里单独放行。";
const PANEL_LAN_ADVICE: &str = "面板走的是没加密的 HTTP，地址里带着访问令牌，拿到令牌就能查看、切换你的账号。没加密的 Wi-Fi 上别人能截到；\
    有加密的网络上同一个网络的人也能打开面板的页面（没有令牌进不去）。不用手机看就关掉局域网访问\
    （cam serve 不加 --lan，桌面版在设置里关掉“允许局域网访问”）；在外面要用，换成 Tailscale 这类组网工具。";
const PROXY_LAN_ADVICE: &str = "代理软件里的“允许局域网连接（Allow LAN）”开着：同一个网络里的人能借你的代理上网，流量从你的出口出去。\
    家里、公司的网络一般不要紧，只提醒、不扣分；连公共 Wi-Fi 时最好关掉，要给手机用就给代理设上用户名和密码。";
const CONFIG_PERM_ADVICE: &str = "这个文件里有账号邮箱、组织、项目列表和 MCP 服务器的配置（可能带着密钥）。电脑只有你自己用就不要紧，只提醒、不扣分；\
    要收紧的话：chmod 600 ";
const AUTO_RUN_ADVICE: &str = "钩子会在 Claude Code 的特定时刻自动运行命令，MCP 服务器是 Claude Code 能调用的外部程序，都能读写你的文件。\
    看看有没有不是你自己加的：钩子在 Claude Code 的 settings.json 的 hooks 里（在 Claude Code 里输入 /hooks 也能看），\
    MCP 服务器用 claude mcp list 查、claude mcp remove 名字 删。只列出来，不扣分。";
const REMOTE_APP_ADVICE: &str = "远程控制软件开着时，别人拿到设备代码和密码就能连进来，直接用你登录着的 Claude 网页和 Claude Code。不用时退出，\
    别把代码和密码告诉别人；自称客服、要你装远程软件“帮你处理”的，多半是骗子。只列出来，不扣分。";

/// 一行里的 Claude 密钥（sk-ant-种类[0-3 位数字]-至少 16 个字母数字_-）的种类，从左往右、不重叠。与 cam.py 的 KEY_RE 一致。
fn key_kinds_in(line: &str) -> Vec<String> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = line[i..].find("sk-ant-") {
        let start = i + off;
        let mut j = start + 7;
        let k0 = j;
        while j < b.len() && b[j].is_ascii_lowercase() {
            j += 1;
        }
        let kind = &line[k0..j];
        let d0 = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        let ok_head = !kind.is_empty() && j - d0 <= 3 && j < b.len() && b[j] == b'-';
        if ok_head {
            let body0 = j + 1;
            let mut e = body0;
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_' || b[e] == b'-') {
                e += 1;
            }
            if e - body0 >= 16 {
                out.push(kind.to_string());
                i = e;
                continue;
            }
        }
        i = start + 1;
    }
    out
}

/// 一段文字里有几行带着 Claude 的密钥、都是哪几类：{count, kinds}（KEY_KINDS 里的说法，去重排好序）。只数不留。
pub fn scan_keys(text: &str) -> Value {
    let mut count = 0;
    let mut kinds: Vec<String> = Vec::new();
    for line in text.lines() {
        let found = key_kinds_in(line);
        if !found.is_empty() {
            count += 1;
            for k in found {
                let name = KEY_KINDS
                    .iter()
                    .find(|(c, _)| *c == k)
                    .map(|(_, n)| *n)
                    .unwrap_or("密钥")
                    .to_string();
                if !kinds.contains(&name) {
                    kinds.push(name);
                }
            }
        }
    }
    kinds.sort();
    json!({"count": count, "kinds": kinds})
}

/// 要看的命令历史文件：bash / zsh / fish / PowerShell 的，HISTFILE 指的，macOS 终端按窗口存的 ~/.zsh_sessions。
/// 测试用 CAM_TEST_HISTORY 直接给文件列表（用系统的路径分隔符隔开，空字符串：一个都不看）。
fn history_files() -> Vec<std::path::PathBuf> {
    if let Some(t) = std::env::var_os("CAM_TEST_HISTORY") {
        return std::env::split_paths(&t)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
    }
    let h = home_dir();
    let mut paths: Vec<std::path::PathBuf> = HISTORY_FILES
        .iter()
        .map(|n| n.split('/').fold(h.clone(), |p, part| p.join(part)))
        .collect();
    if let Some(f) = std::env::var_os("HISTFILE").filter(|f| !f.is_empty()) {
        let f = std::path::PathBuf::from(f);
        paths.push(match f.strip_prefix("~") {
            Ok(rest) => h.join(rest),
            Err(_) => f,
        });
    }
    if cfg!(windows) {
        if let Some(a) = std::env::var_os("APPDATA").filter(|a| !a.is_empty()) {
            paths.push(
                std::path::PathBuf::from(a)
                    .join("Microsoft")
                    .join("Windows")
                    .join("PowerShell")
                    .join("PSReadLine")
                    .join("ConsoleHost_history.txt"),
            );
        }
    }
    let mut sessions: Vec<String> = std::fs::read_dir(h.join(".zsh_sessions"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".history"))
                .collect()
        })
        .unwrap_or_default();
    sessions.sort();
    paths.extend(
        sessions
            .iter()
            .take(50)
            .map(|n| h.join(".zsh_sessions").join(n)),
    );
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for p in paths {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// 命令历史里留着 Claude 密钥的文件：[{path, count, kinds}]，没有是空的。每个文件只读最后 HISTORY_SCAN_BYTES。
fn history_keys() -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let mut out = Vec::new();
    for p in history_files() {
        let Ok(mut f) = std::fs::File::open(&p) else {
            continue;
        };
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut raw = Vec::new();
        if f.seek(SeekFrom::Start(size.saturating_sub(HISTORY_SCAN_BYTES)))
            .is_err()
            || f.read_to_end(&mut raw).is_err()
        {
            continue;
        }
        let r = scan_keys(&String::from_utf8_lossy(&raw));
        if r["count"].as_u64().unwrap_or(0) > 0 {
            out.push(json!({"path": tilde_path(&p), "count": r["count"], "kinds": r["kinds"]}));
        }
    }
    out
}

/// 钩子命令给人看的名字：跳过解释器（python3、node、bash…）、选项和 X=1 这样的变量，取第一个真正的程序 / 脚本的文件名。
/// 命令里别的东西（参数里可能有密钥）不显示。与 cam.py 的 hook_name 一致。
pub fn hook_name(cmd: &str) -> String {
    for tok in cmd.split_whitespace() {
        let t = tok.trim_matches(|c| c == '"' || c == '\'');
        let is_var = t.split_once('=').is_some_and(|(k, _)| {
            let mut ch = k.chars();
            ch.next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if t.is_empty() || t.starts_with('-') || is_var {
            continue;
        }
        let base = t.rsplit(['/', '\\']).next().unwrap_or("");
        let low = base.to_lowercase();
        let low = low.strip_suffix(".exe").unwrap_or(&low);
        let py_ver = low
            .strip_prefix("python3.")
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()));
        if HOOK_RUNNERS.contains(&low) || py_ver {
            continue;
        }
        let name: String = base.chars().take(40).collect();
        return if name.is_empty() { "?".into() } else { name };
    }
    "?".into()
}

/// 这条钩子是不是 cam 自己装的（红绿灯）。与 cam.py 的 HOOK_MARK 一致。
fn is_cam_hook(cmd: &str) -> bool {
    let c = cmd.strip_suffix('\n').unwrap_or(cmd);
    [
        "cam.py hook",
        "cam.py\" hook",
        "cam.py' hook",
        "cam_hook.py",
        "cam_hook.py\"",
        "cam_hook.py'",
    ]
    .iter()
    .any(|end| c.ends_with(end))
}

/// Claude Code 的几份设置（[(路径, 内容)]）里和安全有关的：(设成不问就执行的那份的路径, ["事件 → 钩子名"]（去重，
/// cam 自己装的红绿灯钩子不算）)。与 cam.py 的 settings_security 一致。
pub fn settings_security(docs: &[(String, Value)]) -> (Option<String>, Vec<String>) {
    let mut bypass = None;
    let mut hooks: Vec<String> = Vec::new();
    for (path, doc) in docs {
        let Some(obj) = doc.as_object() else { continue };
        if bypass.is_none()
            && obj
                .get("permissions")
                .and_then(|p| s(p, "defaultMode"))
                .is_some_and(|m| m == "bypassPermissions")
        {
            bypass = Some(path.clone());
        }
        let Some(hk) = obj.get("hooks").and_then(|h| h.as_object()) else {
            continue;
        };
        for (event, groups) in hk {
            for g in groups.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                for h in g
                    .get("hooks")
                    .and_then(|x| x.as_array())
                    .map(|a| a.as_slice())
                    .unwrap_or(&[])
                {
                    if !h.is_object() || s(h, "type").unwrap_or("command") != "command" {
                        continue;
                    }
                    let Some(cmd) = s(h, "command").filter(|c| !c.is_empty()) else {
                        continue;
                    };
                    let line = format!("{} → {}", event, hook_name(cmd));
                    if !is_cam_hook(cmd) && !hooks.contains(&line) {
                        hooks.push(line);
                    }
                }
            }
        }
    }
    (bypass, hooks)
}

/// ~/.claude.json 的内容里配的 MCP 服务器：([所有项目都能用的名字]（排好序）, 只在某个项目里用的个数)。
pub fn mcp_servers(d: &Value) -> (Vec<String>, usize) {
    let Some(obj) = d.as_object() else {
        return (Vec::new(), 0);
    };
    let mut names: Vec<String> = obj
        .get("mcpServers")
        .and_then(|m| m.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    let local = obj
        .get("projects")
        .and_then(|p| p.as_object())
        .map(|p| {
            p.values()
                .filter_map(|v| v.get("mcpServers").and_then(|m| m.as_object()))
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    (names, local)
}

/// 系统级的托管设置（企业管理员下发）。测试用 CAM_MANAGED_SETTINGS 指过去。与 cam.py 的 managed_settings_paths 一致。
fn managed_settings_paths() -> Vec<std::path::PathBuf> {
    if let Some(t) = std::env::var_os("CAM_MANAGED_SETTINGS") {
        return std::env::split_paths(&t)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
    }
    if cfg!(target_os = "macos") {
        return vec!["/Library/Application Support/ClaudeCode/managed-settings.json".into()];
    }
    if cfg!(windows) {
        let env_or = |k: &str, d: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| d.into())
        };
        return vec![
            env_or("ProgramFiles", r"C:\Program Files")
                .join("ClaudeCode")
                .join("managed-settings.json"),
            env_or("ProgramData", r"C:\ProgramData")
                .join("ClaudeCode")
                .join("managed-settings.json"),
        ];
    }
    vec!["/etc/claude-code/managed-settings.json".into()]
}

/// Claude Code 会自动运行的东西：(设成不问就执行的那份设置, {hooks: [前几个], hooks_n, mcp: [前几个], mcp_n, mcp_local}；都没有是 null)。
fn auto_run_info() -> (Option<String>, Value) {
    let dir = crate::claude::claude_config_dir();
    let mut docs = Vec::new();
    for p in [dir.join("settings.json"), dir.join("settings.local.json")]
        .into_iter()
        .chain(managed_settings_paths())
    {
        if let Ok(doc) = read_json(&p) {
            docs.push((tilde_path(&p), doc.unwrap_or(Value::Null)));
        }
    }
    let (bypass, hooks) = settings_security(&docs);
    let (names, local) = match read_json(&crate::claude::claude_json_file()) {
        Ok(d) => mcp_servers(&d.unwrap_or(Value::Null)),
        Err(_) => (Vec::new(), 0),
    };
    let info = if hooks.is_empty() && names.is_empty() && local == 0 {
        Value::Null
    } else {
        json!({"hooks": hooks.iter().take(AUTO_RUN_SHOWN).collect::<Vec<_>>(), "hooks_n": hooks.len(),
               "mcp": names.iter().take(AUTO_RUN_SHOWN).collect::<Vec<_>>(), "mcp_n": names.len(), "mcp_local": local})
    };
    (bypass, info)
}

/// ~/.claude.json 这台电脑上别的用户读不读得到：读得到返回它的权限（"644" 这样），否则 None（Windows 不看）。只看，不改。
fn config_file_mode() -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(crate::claude::claude_json_file())
            .ok()?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Some(format!("{:o}", mode));
        }
    }
    None
}

/// 进程名 → 正在运行的远程控制软件（REMOTE_APPS 里的说法，去重，按表里的顺序）。进程名不分大小写、可以带路径和 .exe；
/// Linux 的进程名最长 15 个字，被截短的也认。与 cam.py 的 remote_apps 一致。
pub fn remote_apps(names: &[String]) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    for n in names {
        let b = n
            .trim()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_lowercase();
        let b = b.strip_suffix(".exe").unwrap_or(&b);
        for (key, label) in REMOTE_APPS {
            if !b.is_empty() && (b == *key || (b.chars().count() == 15 && key.starts_with(b))) {
                seen.push(label);
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (_, label) in REMOTE_APPS {
        if seen.contains(label) && !out.iter().any(|o| o == label) {
            out.push(label.to_string());
        }
    }
    out
}

/// 本机在跑的进程名（远程控制软件那一项用），查不出来是空的。测试用 CAM_TEST_PROCS="ToDesk.exe,foo"。
fn process_names() -> Vec<String> {
    if let Ok(t) = std::env::var("CAM_TEST_PROCS") {
        return t
            .split(',')
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect();
    }
    if cfg!(windows) {
        return win_processes(Duration::from_secs(3))
            .map(|rows| rows.into_iter().map(|r| r.2).collect())
            .unwrap_or_default();
    }
    run_text("ps", &["-A", "-o", "comm="])
        .lines()
        .map(String::from)
        .collect()
}

/// 进程表里带 --lan 的 cam serve 听的端口（没写 --port 就是 8787）。与 cam.py 的 _is_cam_serve / serve_port_of 一致。
fn lan_serve_ports() -> Vec<u16> {
    let cmds: Vec<String> = if cfg!(windows) {
        win_processes(Duration::from_secs(3))
            .map(|rows| rows.into_iter().map(|r| r.3).collect())
            .unwrap_or_default()
    } else {
        run_text("ps", &["-A", "-o", "args="])
            .lines()
            .map(String::from)
            .collect()
    };
    cmds.iter()
        .filter(|c| is_cam_serve(c) && has_lan_flag(c))
        .map(|c| serve_port_of(c))
        .collect()
}

fn is_cam_serve(cmd: &str) -> bool {
    cmd.match_indices("cam.py").any(|(i, m)| {
        let rest = &cmd[i + m.len()..];
        let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
        let trimmed = rest.trim_start();
        trimmed.len() < rest.len()
            && trimmed
                .strip_prefix("serve")
                .is_some_and(|after| after.is_empty() || after.starts_with(char::is_whitespace))
    })
}

fn has_lan_flag(cmd: &str) -> bool {
    cmd.match_indices("--lan").any(|(i, m)| {
        let before = cmd[..i].chars().last();
        let after = cmd[i + m.len()..].chars().next();
        before.is_some_and(char::is_whitespace) && after.is_none_or(char::is_whitespace)
    })
}

fn serve_port_of(cmd: &str) -> u16 {
    for (i, m) in cmd.match_indices("--port") {
        let rest = &cmd[i + m.len()..];
        let digits = rest.trim_start_matches(|c: char| c == '=' || c.is_whitespace());
        if digits.len() == rest.len() {
            continue;
        }
        let n: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = n.parse::<u16>() {
            return p;
        }
    }
    8787
}

/// 默认路由用的本机 IPv4 地址（局域网地址），没有是 None。
fn lan_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("10.255.255.255:1").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// 对局域网开放的 cam 面板和本机代理：{panel: 端口或 null, proxy: ["127.0.0.1:7890", …]}。
/// 面板的候选：桌面版设置里开着内置面板和局域网访问的端口、带 --lan 的 cam serve；代理的候选：设在本机（127.0.0.1 / localhost）的。
/// 拿本机的局域网地址去连这些端口，连得上就是对局域网开着。测试用 CAM_TEST_LAN_IP（空：没有局域网地址），这时不看进程表。
/// 与 cam.py 的 lan_exposed 一致。
fn lan_exposed() -> Value {
    let test_ip = std::env::var("CAM_TEST_LAN_IP").ok();
    let ip = match &test_ip {
        Some(t) => Some(t.clone()).filter(|t| !t.is_empty()),
        None => lan_ip(),
    };
    let Some(ip) = ip else {
        return json!({"panel": null, "proxy": []});
    };
    let mut panel: Vec<u16> = Vec::new();
    let root = std::env::var_os("CAM_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude-accounts"));
    if let Ok(Some(d)) = read_json(&root.join("desktop.json")) {
        let on = |k: &str, dflt: bool| d.get(k).and_then(|v| v.as_bool()).unwrap_or(dflt);
        if on("server_enabled", false) && on("lan", true) {
            let port = d
                .get("port")
                .and_then(|v| v.as_u64())
                .filter(|p| *p > 0 && *p < 65536);
            panel.push(port.unwrap_or(8787) as u16);
        }
    }
    if test_ip.is_none() {
        panel.extend(lan_serve_ports());
    }
    let mut uniq: Vec<u16> = Vec::new();
    for p in panel {
        if !uniq.contains(&p) {
            uniq.push(p);
        }
    }
    let entries: Vec<(String, String)> = PROXY_ENV_KEYS
        .iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (k.to_string(), v))
        })
        .chain(
            system_proxy()
                .into_iter()
                .map(|p| ("系统设置".to_string(), p)),
        )
        .collect();
    let proxy: Vec<(String, u16)> = proxy_endpoints(&entries)
        .into_iter()
        .filter(|(_, h, _)| matches!(h.to_lowercase().as_str(), "127.0.0.1" | "localhost" | "::1"))
        .map(|(_, h, p)| (h, p))
        .collect();
    let probe = |port: u16| {
        let ip = ip.clone();
        spawn_job(move || port_open(&ip, port, Duration::from_secs(LAN_PROBE_SECS)))
    };
    let pj: Vec<(u16, mpsc::Receiver<String>)> = uniq.iter().map(|p| (*p, probe(*p))).collect();
    let xj: Vec<(String, mpsc::Receiver<String>)> = proxy
        .iter()
        .map(|(h, p)| (format!("{}:{}", h, p), probe(*p)))
        .collect();
    let end = Instant::now() + Duration::from_secs(LAN_PROBE_SECS + 1);
    let open = |rx: &mpsc::Receiver<String>| wait_job(rx, end).is_some_and(|w| w.is_empty());
    let panel = pj.iter().find(|(_, rx)| open(rx)).map(|(p, _)| *p);
    let proxy: Vec<String> = xj
        .iter()
        .filter(|(_, rx)| open(rx))
        .map(|(n, _)| n.clone())
        .collect();
    json!({"panel": panel, "proxy": proxy})
}

/// 本机的区域设置、代理和 VPN 快照，和出口一起缓存，免得界面每次刷新都去跑系统命令。
/// 顶部环境卡和服务检测都用这一份，两边的分数才一致。hosts：hosts 文件里把 Claude 的域名指到了哪（没有是空的）；
/// creds_mode：登录凭据文件别的用户也读得到时是它的权限，正常是 null。
/// 安全项：history_keys（命令历史里留着密钥的文件）、perm_bypass（设成不问就执行的那份设置）、auto_run（钩子和 MCP 服务器）、
/// config_mode（~/.claude.json 别人也读得到时的权限）、remote（在跑的远程控制软件）、lan（对局域网开放的面板和代理）。
pub fn local_env() -> Value {
    let tz = system_timezone();
    let lc = system_locale();
    let tun = default_route_tunnel();
    let (bypass, auto_run) = auto_run_info();
    json!({
        "tz": tz.as_ref().map(|t| t.0.clone()), "tz_cc": tz.as_ref().map(|t| t.1.clone()).unwrap_or_default(),
        "locale": lc.as_ref().map(|l| l.0.clone()), "locale_cc": lc.as_ref().and_then(|l| l.1.clone()),
        "dns": system_dns_servers(), "proxy": proxy_parts(),
        "tunnel": tun.map(|(n, k)| json!([n, k])),
        "hosts": hosts_overrides(),
        "creds_mode": creds_file_mode(),
        "wifi": wifi_info(),
        "history_keys": history_keys(), "perm_bypass": bypass, "auto_run": auto_run,
        "config_mode": config_file_mode(), "remote": remote_apps(&process_names()), "lan": lan_exposed()
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
    $w = ''; try { $w = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((netsh wlan show interfaces | Out-String))) } catch {}; \
    [pscustomobject]@{ dns = $dns; culture = (Get-Culture).Name; route = \"$($r.InterfaceAlias)\"; \
    route_desc = \"$($a.InterfaceDescription)\"; wlan = $w } | ConvertTo-Json -Compress";
// 每行：pid、程序路径、名字、启动时间（Unix 秒，拿不到是空的）、命令行；命令行放最后，里面有 Tab 也不怕
const WIN_PROCS_PS: &str =
    "$ErrorActionPreference='Stop'; Get-CimInstance Win32_Process | ForEach-Object { $t = ''; \
    try { if ($_.CreationDate) { $t = ([DateTimeOffset]$_.CreationDate).ToUnixTimeSeconds() } } catch {}; \
    \"$($_.ProcessId)`t$($_.ExecutablePath)`t$($_.Name)`t$t`t$($_.CommandLine)\" }";

/// Windows 进程表的一行：(pid, 程序路径（可能是空的）, 名字, 命令行, 启动时间（Unix 秒）)
pub type WinProc = (u32, String, String, String, Option<i64>);

/// WIN_ENV_PS 的输出（ConvertTo-Json）→ {dns: [...], culture, route, route_desc, wlan}；前后有别的输出也认，解析不了的项是空的。
/// wlan 是 netsh wlan show interfaces 的原文（脚本里转成了 base64，中文系统的代码页过管道不会乱）
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
    let wlan = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(txt("wlan"))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    json!({"dns": dns, "culture": txt("culture"), "route": txt("route"), "route_desc": txt("route_desc"), "wlan": wlan})
}

/// WIN_PROCS_PS 的输出 → [(pid, 程序路径（可能是空的）, 名字, 命令行, 启动时间)]
pub fn parse_windows_processes(text: &str) -> Vec<WinProc> {
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.trim_end_matches('\r').splitn(5, '\t').collect();
            let [pid, exe, name, started, cmd] = parts[..] else {
                return None;
            };
            let pid = pid.trim();
            if !digits(pid) {
                return None;
            }
            let started = started.trim();
            let started = if digits(started) {
                started.parse::<i64>().ok().filter(|t| *t > 0)
            } else {
                None
            };
            Some((
                pid.parse().ok()?,
                exe.trim().to_string(),
                name.trim().to_string(),
                cmd.trim().to_string(),
                started,
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
pub fn win_processes(max_age: Duration) -> Option<Vec<WinProc>> {
    type Rows = Vec<WinProc>;
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
                      "meta_source": null, "lookup_mode": null, "failed_at": null, "fail_count": 0,
                      "plain_http": null, "wifi": null, "domains": []});
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
        "failed_at": s(&c, "failed_at"), "fail_count": c.get("fail_count").and_then(|v| v.as_i64()).unwrap_or(0),
        // 明文网页有没有被中途改动：true 没被改，false 被改了，null 没测（关掉了外部查询、或者没连上）
        "plain_http": c.get("plain_http").and_then(|p| p.get("ok")).and_then(|v| v.as_bool()),
        // 连着的 Wi-Fi：{ssid, security: open / wep / wpa}；有线、没查到是 null
        "wifi": c.get("env").and_then(|e| e.get("wifi")).filter(|w| s(w, "security").is_some())
            .map(|w| json!({"ssid": w.get("ssid"), "security": w.get("security")})),
        // Claude Code 用的 API、续登录的地址各自看到的出口：[{host, ip, loc, status}]，没问到是 []
        "domains": region_domains(&c).iter().map(|d| {
            let mut d = d.clone();
            d["status"] = json!(region_status(s(&d, "loc")));
            d
        }).collect::<Vec<_>>()
    })
}

/// 上次检测记下的 Anthropic 几处地址看到的出口里问到了地区的那些（旧缓存没有这一段是空的）
fn region_domains(reg: &Value) -> Vec<Value> {
    reg.get("domains")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter(|d| s(d, "loc").is_some() && s(d, "host").is_some())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
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
    check_region_with(store, max_age, true, &Progress::none())
}

/// check_region 的本体。做完后评分变了就记一笔（note_risk）；note 为 false 不记：服务检测先调这里、自己还要接着测别的，
/// 等它全做完一起记，不然一次检测记成两笔。prog 给了就把 REGION_STEPS 这几步做到哪了记进去。
fn check_region_with(store: &Store, max_age: i64, note: bool, prog: &Progress) -> R<Value> {
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
        for k in REGION_STEPS {
            prog.skip(k, "刚测过，沿用上次的结果");
        }
        if note {
            note_risk(store); // 这次没重测，分数也可能变了（出口变化的观察期过了、切了账号）
        }
        return Ok(region_view(&data));
    }
    let cfg = ip_lookup_config(Some(store));
    // 出口、状态页、本机设置几件事并行，互不等待；各有各的时限，到点没做完的当作没查到
    let t0 = Instant::now();
    let within = |secs: u64| t0 + Duration::from_secs(secs.min(REGION_CHECK_BUDGET));
    let jr = {
        let (cfg, old, prog) = (cfg.clone(), old.clone(), prog.clone());
        spawn_job(move || detect_region_prog(&cfg, &old, &prog))
    };
    let je = tracked(prog, "local", "", |_: &Value| true, local_env);
    let js = tracked(
        prog,
        "status",
        "没查到",
        |v: &Option<Value>| v.is_some(),
        fetch_status_page,
    );
    let jn = spawn_job(net_fingerprint);
    let jp = {
        let data = data.clone();
        spawn_job(move || proxies_back(&data))
    };
    // 明文网页有没有被改：要访问一个 Anthropic 以外的地址，“外部 IP 查询”关掉时不做
    let jh = (cfg.mode != "off").then(|| {
        tracked(
            prog,
            "plain",
            "没连上",
            |v: &Option<Value>| v.is_some(),
            plain_http_check,
        )
    });
    let r = wait_job(&jr, within(REGION_CHECK_BUDGET))
        .flatten()
        .unwrap_or_else(|| json!({}));
    let env = wait_job(&je, within(10));
    let sp = wait_job(&js, within(7)).flatten();
    let net = wait_job(&jn, within(2)).unwrap_or_default();
    let back = wait_job(&jp, within(PROXY_PROBE_SECS + 1)).unwrap_or_default();
    let plain = jh
        .as_ref()
        .and_then(|j| wait_job(j, within(PLAIN_HTTP_SECS + 1)))
        .flatten();
    prog.expire(REGION_STEPS);
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
        reg["domains"] = r.get("domains").cloned().unwrap_or_else(|| json!([]));
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
    // 这次测到了：记下；没测、没连上就当不知道，不留上一次的
    match plain {
        Some(v) => reg["plain_http"] = v,
        None => {
            if let Some(o) = reg.as_object_mut() {
                o.remove("plain_http");
            }
        }
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
    if note {
        note_risk_in(&mut data, None); // 评分变了记一笔（锁还拿着，直接写进这一份）
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
/// 环境快照里的安全项变成提醒：命令历史里的密钥、不问就执行、对局域网开放的面板和代理、~/.claude.json 的权限、
/// 钩子和 MCP 服务器、远程控制软件。只说在哪、几处，不显示内容。与 cam.py 的 risk_items 里同一段一致（tests/shared/security.json）。
fn security_items(env: Option<&Value>, env_old: &str, add: &dyn Fn(&str, &str, String, &str)) {
    let Some(env) = env else { return };
    let hist: Vec<&Value> = env
        .get("history_keys")
        .and_then(|h| h.as_array())
        .map(|a| {
            a.iter()
                .filter(|h| h.get("count").and_then(|c| c.as_u64()).unwrap_or(0) > 0)
                .collect()
        })
        .unwrap_or_default();
    if !hist.is_empty() {
        let mut kinds: Vec<&str> = hist
            .iter()
            .flat_map(|h| {
                h.get("kinds")
                    .and_then(|k| k.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        let mut wh = hist
            .iter()
            .take(3)
            .map(|h| {
                format!(
                    "{}（{} 处）",
                    s(h, "path").unwrap_or("None"),
                    h.get("count").and_then(|c| c.as_u64()).unwrap_or(0)
                )
            })
            .collect::<Vec<_>>()
            .join("、");
        if hist.len() > 3 {
            wh += &format!("等 {} 个文件", hist.len());
        }
        let what = if kinds.is_empty() {
            String::new()
        } else {
            format!("（{}）", kinds.join("、"))
        };
        add(
            "history_key",
            "warn",
            format!("命令历史里留着 Claude 的密钥{}：{}{}", what, wh, env_old),
            HISTORY_KEY_ADVICE,
        );
    }
    if let Some(p) = s(env, "perm_bypass").filter(|p| !p.is_empty()) {
        add(
            "perm_bypass",
            "warn",
            format!(
                "Claude Code 设成了不问就执行：{} 里 permissions.defaultMode 是 bypassPermissions{}",
                p, env_old
            ),
            PERM_BYPASS_ADVICE,
        );
    }
    let lan = env.get("lan").filter(|l| l.is_object());
    if let Some(port) = lan.and_then(|l| l.get("panel")).and_then(|p| p.as_u64()) {
        let open_wifi = env
            .get("wifi")
            .and_then(|w| s(w, "security"))
            .is_some_and(|x| x == "open" || x == "wep");
        add(
            "panel_lan",
            if open_wifi { "warn" } else { "info" },
            format!(
                "cam 面板（{} 端口）开着局域网访问{}{}",
                port,
                if open_wifi {
                    "，连着的 Wi-Fi 又没加密"
                } else {
                    ""
                },
                env_old
            ),
            PANEL_LAN_ADVICE,
        );
    }
    let proxies: Vec<&str> = lan
        .and_then(|l| l.get("proxy"))
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    if !proxies.is_empty() {
        add(
            "proxy_lan",
            "info",
            format!(
                "本机代理 {} 对局域网开放：同一个网络里的人能借它上网{}",
                proxies.join("、"),
                env_old
            ),
            PROXY_LAN_ADVICE,
        );
    }
    if let Some(mode) = s(env, "config_mode").filter(|m| !m.is_empty()) {
        let path = tilde_path(&crate::claude::claude_json_file());
        add(
            "config_perm",
            "info",
            format!(
                "{} 的权限是 0{}，这台电脑上的其他用户也读得到{}",
                path, mode, env_old
            ),
            &format!("{}{}", CONFIG_PERM_ADVICE, path),
        );
    }
    if let Some(ar) = env.get("auto_run").filter(|a| a.is_object()) {
        let n = |k: &str| ar.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let names = |k: &str| -> Vec<&str> {
            ar.get(k)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                .unwrap_or_default()
        };
        let mut parts = Vec::new();
        for (cnt, list, what) in [
            (n("hooks_n"), names("hooks"), "个钩子"),
            (n("mcp_n"), names("mcp"), "个 MCP 服务器"),
        ] {
            if cnt > 0 {
                parts.push(format!(
                    "{} {}（{}{}）",
                    cnt,
                    what,
                    list.join("、"),
                    if cnt as usize > list.len() {
                        " 等"
                    } else {
                        ""
                    }
                ));
            }
        }
        if n("mcp_local") > 0 {
            parts.push(format!(
                "另有 {} 个只在某个项目里用的 MCP 服务器",
                n("mcp_local")
            ));
        }
        if !parts.is_empty() {
            add(
                "auto_run",
                "info",
                format!("Claude Code 会自动运行的：{}{}", parts.join("；"), env_old),
                AUTO_RUN_ADVICE,
            );
        }
    }
    let remote: Vec<&str> = env
        .get("remote")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    if !remote.is_empty() {
        add(
            "remote_app",
            "info",
            format!("正在运行远程控制软件：{}{}", remote.join("、"), env_old),
            REMOTE_APP_ADVICE,
        );
    }
}

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
    // 刚加的那一条是关注项
    let mark_notice = || {
        if let Some(last) = out.borrow_mut().last_mut() {
            last["notice"] = json!(true);
        }
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
                    if is_notice(&it) {
                        mark_notice();
                    }
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
    // Claude Code 用的 API、续登录的地址看到的出口和 claude.ai 看到的不一样
    if let Some(l) = loc.as_deref() {
        let diff: Vec<Value> = region_domains(&reg)
            .into_iter()
            .filter(|d| s(d, "loc") != Some(l))
            .collect();
        if !diff.is_empty() {
            let bad = region_status(Some(l)) == "supported"
                && diff
                    .iter()
                    .any(|d| region_status(s(d, "loc")) != "supported");
            let seen = diff
                .iter()
                .map(|d| {
                    format!(
                        "{} 看到的是 {}",
                        s(d, "host").unwrap_or(""),
                        region_label(s(d, "loc"))
                    )
                })
                .collect::<Vec<_>>()
                .join("，");
            add(
                "domain_region",
                if bad { "bad" } else { "warn" },
                format!(
                    "Anthropic 几处地址看到的出口地区不一样：claude.ai 看到的是 {}，{}{}",
                    region_label(Some(l)),
                    seen,
                    if bad { "，不在支持名单内" } else { "" }
                ),
                if bad {
                    DOMAIN_REGION_BAD_ADVICE
                } else {
                    DOMAIN_REGION_ADVICE
                },
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
    // hosts 文件把 Claude 的域名指到了别处：流量（带着登录凭据）被送去那个地址
    let hosts: Vec<Value> = env
        .as_ref()
        .and_then(|e| e.get("hosts"))
        .and_then(|h| h.as_array())
        .map(|a| {
            a.iter()
                .filter(|h| s(h, "host").is_some())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(first) = hosts.first() {
        let more = if hosts.len() > 1 {
            format!("（另有 {} 条）", hosts.len() - 1)
        } else {
            String::new()
        };
        add_plain(
            "hosts",
            "warn",
            format!(
                "hosts 文件把 {} 指到了 {}{}{}",
                s(first, "host").unwrap_or(""),
                s(first, "ip").unwrap_or(""),
                more,
                env_old
            ),
            HOSTS_ADVICE,
        );
    }
    // 连着的 Wi-Fi 没加密，或者是 WEP
    if let Some(wifi) = env.as_ref().and_then(|e| e.get("wifi")) {
        let sec = s(wifi, "security").unwrap_or("");
        if sec == "open" || sec == "wep" {
            let name = s(wifi, "ssid")
                .filter(|n| !n.is_empty())
                .map(|n| format!("“{}”", n))
                .unwrap_or_default();
            let text = if sec == "open" {
                format!("连着的 Wi-Fi{}没有加密（开放网络）", name)
            } else {
                format!(
                    "连着的 Wi-Fi{}用的是 WEP 加密：早就被破解了，和没加密差不多",
                    name
                )
            };
            add_plain("wifi", "warn", text + &env_old, WIFI_ADVICE);
        }
    }
    // 明文网页被中途改动了
    if reg
        .get("plain_http")
        .and_then(|p| p.get("ok"))
        .and_then(|v| v.as_bool())
        == Some(false)
    {
        let got = reg
            .get("plain_http")
            .and_then(|p| s(p, "got"))
            .filter(|g| !g.is_empty())
            .unwrap_or("拿到的不是原样");
        add(
            "http_tamper",
            "warn",
            format!("这张网在改动没加密的网页：{}", got),
            HTTP_TAMPER_ADVICE,
        );
    }
    // 登录凭据文件别的用户也读得到
    if let Some(mode) = env.as_ref().and_then(|e| s(e, "creds_mode")) {
        add_plain(
            "creds_perm",
            "warn",
            format!(
                "Claude Code 的登录凭据文件权限是 0{}，这台电脑上的其他用户也读得到{}",
                mode, env_old
            ),
            &format!(
                "{}{}{}",
                CREDS_PERM_ADVICE,
                tilde_path(&crate::claude::credentials_file()),
                CREDS_PERM_ADVICE_TAIL
            ),
        );
    }
    security_items(env.as_ref(), &env_old, &add_plain);
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
            mark_notice(); // 关注项：只提醒，不扣分
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
                &format!(
                    "同一账号短时间在多个地区使用（出差、VPN、多人共用）是常见的风控触发点，可能被要求重新验证或限制使用。不是你自己用的话，{}",
                    ACTIVE_SESSIONS_TIP
                ),
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
    // 官方状态页的公告不扣分：那是 Anthropic 那边的事，不是这台电脑环境的风险，只当关注项列出来
    ("status", "bad", 0),
    ("status", "warn", 0),
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
    ("dns_resolver", "info", 0),
    ("ip_origin", "info", 5),
    ("dns_servers", "info", 0),
    ("auth", "bad", 20),
    ("auth", "warn", 5),
    ("proxy_down", "bad", 0),
    ("tls_intercept", "warn", 0),
    ("hosts", "warn", 20),
    ("creds_perm", "warn", 15),
    ("http_tamper", "warn", 10),
    ("wifi", "warn", 5),
    ("domain_region", "bad", 60),
    ("domain_region", "warn", 15),
    ("history_key", "warn", 5),
    ("perm_bypass", "warn", 5),
    ("panel_lan", "warn", 5),
    ("panel_lan", "info", 0),
    ("proxy_lan", "info", 0),
    ("config_perm", "info", 0),
    ("auto_run", "info", 0),
    ("remote_app", "info", 0),
];
/// (最低分, 档位, 说明)
const RISK_GRADES: &[(i64, &str, &str)] = &[
    (90, "low", "风险低，可以使用"),
    (70, "medium", "有风险，建议先处理提醒项再使用"),
    (0, "high", "风险高，不建议在当前环境使用；是否继续由你决定"),
];
const RISK_SCORE_ADVICE: &str =
    "评分只汇总本机能看到的信号，满分也不代表一定合规；低于 90 时逐条看提醒和建议。";

/// 环境评分的变化最多记几次
const RISK_LOG_KEEP: usize = 30;
/// 界面上列最近几次
pub const RISK_LOG_SHOWN: usize = 8;

/// 条目是不是关注项（带 notice）：Anthropic 那边的状况，不是这台电脑环境的风险，照样标出来但不扣分。
fn is_notice(item: &Value) -> bool {
    item.get("notice").and_then(|v| v.as_bool()) == Some(true)
}

/// 这一条从环境评分里扣几分（不扣是 0）。关注项（官方状态页的公告、限流、过载、服务端错误）不扣分。
/// 与 cam.py 的 risk_points 一致（tests/shared/risk_log.json）。
pub fn risk_points(item: &Value) -> i64 {
    let (kind, level) = (
        s(item, "kind").unwrap_or(""),
        s(item, "level").unwrap_or(""),
    );
    if kind == "summary" || is_notice(item) {
        return 0;
    }
    match RISK_WEIGHTS
        .iter()
        .find(|(k, l, _)| *k == kind && *l == level)
    {
        Some((_, _, w)) => *w,
        None => match level {
            "warn" => 10,
            "bad" => 30,
            _ => 0,
        },
    }
}

/// 扣分规则写给人看的说法（面板评分卡里的“扣分规则”）：每条对应 RISK_WEIGHTS 里扣分的一项，分数从那里取，
/// 所以界面上列的和实际扣的不会对不上。与 cam.py 的 RISK_RULES / RISK_NOTICES 一致（tests/shared/risk_rules.json）
const RISK_RULES: &[(&str, &str, &str)] = &[
    ("region", "warn", "出口地区不在 Anthropic 支持名单"),
    (
        "domain_region",
        "bad",
        "Claude Code 用的 API 或续登录的地址看到的出口不在支持名单（claude.ai 看到的在）",
    ),
    ("tor_exit", "warn", "出口 IP 是 Tor 出口节点"),
    ("api", "bad", "api.anthropic.com 连不上，或者拒绝访问"),
    (
        "region_unknown",
        "warn",
        "出口地区查不到（连不上 claude.ai 和 Cloudflare）",
    ),
    ("proxy_ip", "warn", "出口 IP 被数据库标记为代理 / VPN 出口"),
    (
        "timezone",
        "warn",
        "系统时区指向不支持的地区，出口却在支持地区",
    ),
    ("isp", "warn", "出口 IP 是机房 / 云服务器"),
    ("account_regions", "warn", "当前账号 7 天内在多个地区用过"),
    ("auth", "bad", "续登录的地址都连不上"),
    ("dns", "bad", "Claude 的域名解析不了，也确实连不上"),
    (
        "hosts",
        "warn",
        "hosts 文件把 Claude / Anthropic 的域名指到了别处",
    ),
    (
        "creds_perm",
        "warn",
        "Claude Code 的登录凭据文件别的用户也读得到",
    ),
    ("ipv6_mismatch", "warn", "IPv4 和 IPv6 的出口不在同一个地区"),
    ("region_mismatch", "warn", "两个接口看到的出口地区不一致"),
    ("region_change", "warn", "24 小时内出口换到了不支持的地区"),
    (
        "domain_region",
        "warn",
        "Anthropic 几处地址（claude.ai、API、续登录）看到的出口地区不一样",
    ),
    ("clock", "warn", "本机时钟和服务器相差 2 分钟以上"),
    (
        "wifi",
        "warn",
        "连着的 Wi-Fi 没有加密（开放网络，包括要在网页里登录的），或者用的是 WEP",
    ),
    (
        "history_key",
        "warn",
        "命令历史里留着 Claude 的密钥（sk-ant- 开头）",
    ),
    (
        "perm_bypass",
        "warn",
        "Claude Code 设成了不问就执行（bypassPermissions）",
    ),
    (
        "panel_lan",
        "warn",
        "cam 面板开着局域网访问，连着的 Wi-Fi 又没加密",
    ),
    (
        "http_tamper",
        "warn",
        "这张网在改动没加密的网页（公共 Wi-Fi 的登录页，或者有人往里塞东西）",
    ),
    ("vpn", "info", "默认路由走 VPN / 隧道"),
    ("web", "bad", "claude.ai 连不上，或者拒绝访问"),
    ("proxy", "info", "设了代理（环境变量或系统代理）"),
    ("warp", "info", "出口经过 Cloudflare WARP / Zero Trust"),
    (
        "timezone",
        "info",
        "系统时区和出口地区不一致（两边都在支持地区）",
    ),
    ("locale", "info", "系统语言 / 区域和出口地区不一致"),
    (
        "ip_origin",
        "info",
        "出口 IP 的注册地和定位不一致（广播 IP）",
    ),
    ("auth", "warn", "续登录的地址有的连不上"),
    ("web", "warn", "claude.ai 返回人机验证页，或者少见的状态"),
    ("api", "warn", "api.anthropic.com 返回少见的状态"),
];
/// 只提醒、不扣分的（关注项和说明）：不是这台电脑环境的风险
const RISK_NOTICES: &[&str] = &[
    "Anthropic 官方状态页的公告",
    "官方限流（429）、服务过载（529）、服务端错误（5xx）",
    "当前登录已过期（账号的事，运行一次 claude 会自动续上）",
    "出口在支持地区之间变化、系统配的 DNS 和出口地区不一致",
    "本机解析不了域名但实际连得上（请求走代理）、域名解析成内网 / 保留地址（被本机的软件接管）",
    "HTTPS 被公司网关或安全软件解密、本机设的代理没开（连不上的那几项已经扣了）",
    "cam 面板开着局域网访问（Wi-Fi 有加密时）、本机代理对局域网开放、~/.claude.json 别的用户也读得到",
    "会自动运行命令的钩子和 MCP 服务器、正在运行的远程控制软件（只列出来，看看有没有不是你装的）",
];

/// 给人看的扣分规则：{rules: [{points, text}]（扣得多的在前，同分按表里的顺序）, notices: [只提醒不扣分的]}。
pub fn risk_rules() -> Value {
    let mut rules: Vec<(i64, &str)> = RISK_RULES
        .iter()
        .map(|(k, l, t)| (risk_points(&json!({"kind": k, "level": l})), *t))
        .collect();
    rules.sort_by_key(|r| -r.0); // 稳定排序
    let rules: Vec<Value> = rules
        .into_iter()
        .map(|(p, t)| json!({"points": p, "text": t}))
        .collect();
    json!({"rules": rules, "notices": RISK_NOTICES})
}

/// 把风险项汇总成 {score, grade, label, advice}。只用于提示。
pub fn risk_score(items: &[Value]) -> Value {
    let score = (100 - items.iter().map(risk_points).sum::<i64>()).max(0);
    let (_, grade, label) = RISK_GRADES
        .iter()
        .find(|(floor, _, _)| score >= *floor)
        .unwrap_or(&RISK_GRADES[2]);
    json!({"score": score, "grade": grade, "label": label, "advice": RISK_SCORE_ADVICE})
}

/// 去掉文字里的“（3 分钟前）”“（刚刚）”这类相对时间：记进分数变化里的说明过几天再看，这些就不对了。与 cam.py 的 strip_ago 一致。
pub fn strip_ago(text: &str) -> String {
    fn is_ago(inner: &str) -> bool {
        if inner == "刚刚" {
            return true;
        }
        let digits = inner.chars().take_while(|c| c.is_ascii_digit()).count();
        digits > 0 && matches!(&inner[digits..], " 分钟前" | " 小时前" | " 天前")
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(a) = rest.find('（') {
        let body = &rest[a + '（'.len_utf8()..];
        match body.find('）') {
            // 括号里还套着别的括号的不算（“（没有（3 分钟前）”只去掉里面那一对）
            Some(b) if is_ago(&body[..b]) => {
                out.push_str(&rest[..a]);
                rest = &body[b + '）'.len_utf8()..];
            }
            _ => {
                out.push_str(&rest[..a + '（'.len_utf8()]);
                rest = body;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 现在的环境评分和扣分的是哪几条：{score, grade, exit: {loc, ip, org}, items: [{kind, level, text, points}]}。还没检测过返回 None。
/// exit 是当时的出口；文字不带“多久前检测的”这类相对时间（strip_ago）。与 cam.py 的 risk_snapshot 一致。
pub fn risk_snapshot(data: &Value) -> Option<Value> {
    let reg = data.get("region")?;
    s(reg, "checked_at")?;
    let items = risk_items(data, true, false);
    let sc = risk_score(&items);
    let lost: Vec<Value> = items
        .iter()
        .filter(|i| risk_points(i) > 0)
        .map(|i| {
            json!({"kind": i.get("kind"), "level": i.get("level"),
                   "text": strip_ago(s(i, "text").unwrap_or("")), "points": risk_points(i)})
        })
        .collect();
    Some(json!({
        "score": sc["score"], "grade": sc["grade"],
        "exit": {"loc": reg.get("loc"), "ip": reg.get("ip"), "org": reg.get("org")},
        "items": lost
    }))
}

/// 两次之间出口换没换，写成一句（没换、或者有一边没记是空字符串）：分数变了多半是因为出口换了，一起说出来才看得懂。
/// 与 cam.py 的 exit_note 一致（tests/shared/risk_log.json）。
pub fn exit_note(prev: &Value, cur: &Value) -> String {
    let org = match s(cur, "org") {
        Some(o) if !o.is_empty() && s(prev, "org") != Some(o) => format!("（{}）", o),
        _ => String::new(),
    };
    let pair = |k: &str| match (s(prev, k), s(cur, k)) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() && a != b => Some((a, b)),
        _ => None,
    };
    if let Some((a, b)) = pair("loc") {
        return format!(
            "出口从 {} 换到了 {}{}",
            region_label(Some(a)),
            region_label(Some(b)),
            org
        );
    }
    if let Some((a, b)) = pair("ip") {
        return format!("出口 IP 换了：{} → {}{}", a, b, org);
    }
    String::new()
}

fn risk_item_keys(snap: &Value) -> Vec<(String, String)> {
    snap.get("items")
        .and_then(|i| i.as_array())
        .map(|a| {
            a.iter()
                .map(|i| {
                    (
                        s(i, "kind").unwrap_or("").to_string(),
                        s(i, "level").unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 两次的评分算不算一样：分数一样，扣分的也是同样的几条（按 kind + level 比，文字里的毫秒数、时间不算）。
fn risk_same(a: &Value, b: &Value) -> bool {
    let key = |v: &Value| {
        let mut k = risk_item_keys(v);
        k.sort();
        (v.get("score").and_then(|x| x.as_i64()), k)
    };
    key(a) == key(b)
}

fn risk_log(data: &Value) -> Vec<Value> {
    data.get("risk_log")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default()
}

/// 环境评分变了（分数变了，或者扣分的换了几条）就在 data["risk_log"] 最前面记一笔 {at, score, grade, exit, items}，最多留
/// RISK_LOG_KEEP 笔；第一笔是开始记的时候。记了返回 true，没变（或者还没检测过）返回 false。与 cam.py 的 note_risk_in 一致。
pub fn note_risk_in(data: &mut Value, at: Option<&str>) -> bool {
    let Some(mut snap) = risk_snapshot(data) else {
        return false;
    };
    let mut log = risk_log(data);
    if log.first().is_some_and(|last| risk_same(last, &snap)) {
        return false;
    }
    snap["at"] = json!(at.map(String::from).unwrap_or_else(now_iso));
    log.insert(0, snap);
    log.truncate(RISK_LOG_KEEP);
    data["risk_log"] = json!(log);
    true
}

/// 环境检测、服务检测每次做完都调一次：评分变了就记一笔，没变什么都不写。只记录，不干预任何操作。
pub fn note_risk(store: &Store) {
    // 先不加锁看一眼，绝大多数时候没变
    let changed = store
        .load()
        .map(|mut d| note_risk_in(&mut d, None))
        .unwrap_or(false);
    if !changed {
        return;
    }
    if let Ok(_g) = store.lock() {
        if let Ok(mut d) = store.load() {
            if note_risk_in(&mut d, None) {
                let _ = store.save(&mut d);
            }
        }
    }
}

/// new 比 old 多出来的扣分项、没有了的扣分项。同一种（kind + level）有几条就按条数比。
fn risk_diff(old: &Value, new: &Value) -> (Vec<Value>, Vec<Value>) {
    fn extra(a: &Value, b: &Value) -> Vec<Value> {
        let mut left = risk_item_keys(b);
        let mut out = Vec::new();
        for i in a
            .get("items")
            .and_then(|i| i.as_array())
            .into_iter()
            .flatten()
        {
            let k = (
                s(i, "kind").unwrap_or("").to_string(),
                s(i, "level").unwrap_or("").to_string(),
            );
            match left.iter().position(|x| *x == k) {
                Some(n) => {
                    left.remove(n);
                }
                None => out.push(json!({
                    "kind": i.get("kind"), "text": s(i, "text").unwrap_or(""),
                    "points": i.get("points").and_then(|p| p.as_i64()).unwrap_or(0)
                })),
            }
        }
        out
    }
    (extra(new, old), extra(old, new))
}

/// 界面上的“分数变化”：{since: 从什么时候开始记的, changes: [{at, score, grade, prev, added, removed, exit_note}]}，新的在前，最多 limit 次。
/// prev 是变之前的分数；added / removed 是和变之前比多出来、没有了的扣分项 [{kind, text, points}]；exit_note 是这两次之间出口换了的那一句（没换是空的）。
/// 现在的分数和最后记的那一笔不一样（后台还没来得及记）时，最前面补一条 at 为 null 的。与 cam.py 的 risk_log_view 一致。
pub fn risk_log_view(data: &Value, limit: usize) -> Value {
    let log: Vec<Value> = risk_log(data)
        .into_iter()
        .filter(|e| e.is_object())
        .collect();
    if log.is_empty() {
        return json!({"since": null, "changes": []});
    }
    let mut seq = Vec::new();
    if let Some(mut snap) = risk_snapshot(data) {
        if !risk_same(&log[0], &snap) {
            snap["at"] = Value::Null;
            seq.push(snap);
        }
    }
    seq.extend(log.iter().cloned());
    let changes: Vec<Value> = seq
        .windows(2)
        .take(limit)
        .map(|w| {
            let (new, old) = (&w[0], &w[1]);
            let (added, removed) = risk_diff(old, new);
            let note = exit_note(
                old.get("exit").unwrap_or(&Value::Null),
                new.get("exit").unwrap_or(&Value::Null),
            );
            json!({"at": new.get("at"), "score": new.get("score"), "grade": new.get("grade"),
                   "prev": old.get("score"), "added": added, "removed": removed, "exit_note": note})
        })
        .collect();
    json!({"since": log[log.len() - 1].get("at"), "changes": changes})
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

// ---- 连着的 Wi-Fi 加没加密：开放网络（不要密码）和早就被破解的 WEP，同一个 Wi-Fi 上的人能看到、改动没加密的流量。
// 只读系统自己报的连接信息：Linux 问 NetworkManager（nmcli），macOS 问 system_profiler，Windows 问 netsh；
// 有线、没连 Wi-Fi、读不出来都是 None（不显示，不乱报）。与 cam.py 一致（tests/shared/wifi.json）
const WIFI_SECS: u64 = 8;
/// 同一张网上多久之内不重查（macOS 的 system_profiler 要一两秒）
const WIFI_REUSE_SECS: u64 = 600;
const WIFI_OPEN_WORDS: &[&str] = &[
    "open",
    "none",
    "--",
    "开放",
    "开放式",
    "无",
    "offen",
    "ouvert",
    "ouverte",
    "abierta",
    "abierto",
    "aperta",
    "aberta",
];
const WIFI_ADVICE: &str = "同一个 Wi-Fi 上的人能看到、改动没加密的流量。公司、商场那种连上后要在网页里登录的 Wi-Fi，系统里也显示没加密，所以只扣 5 分。\
                           Claude 的连接是 HTTPS、有证书校验，内容看不到也改不了；但别在这种网络上忽略证书警告，也别开着 cam 面板的局域网访问。\
                           条件允许就换成要密码的 Wi-Fi，或者用手机热点。";

/// 系统报的加密方式归成三类："open"（没加密）、"wep"、"wpa"（WPA / WPA2 / WPA3、企业版 802.1X、OWE 都算加密了）；认不出来是 None。
pub fn wifi_security_class(raw: &str) -> Option<&'static str> {
    let v = raw.trim().to_lowercase();
    if v.is_empty() {
        return None;
    }
    if v.contains("wep") {
        return Some("wep");
    }
    if ["wpa", "sae", "802.1x", "rsn", "eap", "owe", "wapi"]
        .iter()
        .any(|k| v.contains(k))
    {
        return Some("wpa");
    }
    (WIFI_OPEN_WORDS.contains(&v.as_str()) || v.ends_with("_none")).then_some("open")
}

fn wifi_value(ssid: Option<&str>, security: &str) -> Value {
    json!({"ssid": ssid.filter(|x| !x.is_empty()), "security": security})
}

/// nmcli -t -f ACTIVE,SSID,SECURITY dev wifi 的输出里正连着的那一行 → {ssid, security}；没有返回 None。
/// 每行 yes:名字:WPA2 这样，名字里的冒号写成 \:；SECURITY 是空的或 -- 就是开放网络。
pub fn parse_nmcli_wifi(text: &str) -> Option<Value> {
    for line in text.lines() {
        // 按没被反斜杠转义的冒号切开
        let mut fields: Vec<String> = vec![String::new()];
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if matches!(chars.peek(), Some(':') | Some('\\')) => {
                    fields.last_mut().unwrap().push(chars.next().unwrap());
                }
                ':' => fields.push(String::new()),
                _ => fields.last_mut().unwrap().push(c),
            }
        }
        if fields.len() >= 3 && fields[0].trim().eq_ignore_ascii_case("yes") {
            let sec = fields[2].trim();
            let cls = if sec.is_empty() || sec == "--" {
                Some("open")
            } else {
                wifi_security_class(sec)
            };
            return cls.map(|c| wifi_value(Some(&fields[1]), c));
        }
    }
    None
}

/// netsh wlan show interfaces 的输出里第一块连着的网卡 → {ssid, security}；没连、认不出来返回 None。
/// 各行的名字跟着系统语言变（Authentication / 身份验证），所以不看名字，看 SSID 那行后面几行的值：
/// SSID、BSSID、网络类型、无线电类型、身份验证、密码，第一个认得出加密方式的值就是。
pub fn parse_netsh_wlan(text: &str) -> Option<Value> {
    let (mut ssid, mut started, mut left) = (String::new(), false, 6);
    for line in text.lines() {
        if line.trim().is_empty() {
            if started {
                break;
            }
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if !started {
            if k.eq_ignore_ascii_case("SSID") {
                ssid = v.to_string();
                started = true;
            }
            continue;
        }
        if k.to_uppercase().contains("BSSID") {
            continue;
        }
        if let Some(cls) = wifi_security_class(v) {
            return Some(wifi_value(Some(&ssid), cls));
        }
        left -= 1;
        if left <= 0 {
            break;
        }
    }
    None
}

/// system_profiler SPAirPortDataType -json 的输出里正连着的网络 → {ssid, security}；没连、认不出来返回 None。
/// 新版 macOS 没给定位权限时名字是 <redacted>，当作不知道名字。
pub fn parse_airport_json(text: &str) -> Option<Value> {
    let d: Value = serde_json::from_str(text).ok()?;
    for top in d.get("SPAirPortDataType")?.as_array()? {
        let Some(ifaces) = top
            .get("spairport_airport_interfaces")
            .and_then(|i| i.as_array())
        else {
            continue;
        };
        for iface in ifaces {
            let Some(cur) = iface.get("spairport_current_network_information") else {
                continue;
            };
            if let Some(cls) = s(cur, "spairport_security_mode").and_then(wifi_security_class) {
                let name = s(cur, "_name").filter(|n| !n.to_lowercase().contains("redacted"));
                return Some(wifi_value(name, cls));
            }
        }
    }
    None
}

/// 现在连着的 Wi-Fi：{ssid: 名字或 null, security: "open" | "wep" | "wpa"}；有线、没连、读不出来返回 None。
/// 同一张网上 WIFI_REUSE_SECS 秒内不重查。测试用 CAM_TEST_WIFI="open:CafeFree"（空字符串：没连 Wi-Fi）。
fn wifi_info() -> Option<Value> {
    if let Ok(t) = std::env::var("CAM_TEST_WIFI") {
        let (sec, ssid) = t.split_once(':').unwrap_or((t.as_str(), ""));
        return ["open", "wep", "wpa"]
            .iter()
            .find(|c| **c == sec)
            .map(|c| wifi_value(Some(ssid), c));
    }
    type Cached = Option<(String, Instant, Option<Value>)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cached>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let net = net_fingerprint();
    if let Some((n, at, v)) = cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *n == net && at.elapsed() < Duration::from_secs(WIFI_REUSE_SECS) {
            return v.clone();
        }
    }
    let v = if cfg!(windows) {
        windows_env()
            .get("wlan")
            .and_then(|w| w.as_str())
            .and_then(parse_netsh_wlan)
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("system_profiler");
        c.args(["SPAirPortDataType", "-json"]);
        parse_airport_json(&run_with_timeout(c, Duration::from_secs(WIFI_SECS)))
    } else {
        let mut c = Command::new("nmcli");
        // ACTIVE 那一列在别的语言下不是 yes / no
        c.args(["-t", "-f", "ACTIVE,SSID,SECURITY", "dev", "wifi"])
            .env("LC_ALL", "C");
        parse_nmcli_wifi(&run_with_timeout(c, Duration::from_secs(WIFI_SECS)))
    };
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((net, Instant::now(), v.clone()));
    v
}

// ---- 明文网页有没有被中途改动：取一个内容固定的明文地址（Cloudflare 的 generate_204，本该回一个空的 204），拿到别的东西
// 就是路上有设备在改没加密的流量——要登录的公共 Wi-Fi 的门户页，或者有人往里塞东西。Claude 自己的连接都是 HTTPS、有证书校验，
// 这一项说的是这张网可不可信。直连、不走代理、不跟着跳转，只发一个空请求。“外部 IP 查询”关掉时不做。与 cam.py 一致
const PLAIN_HTTP_SECS: u64 = 4;
const HTTP_TAMPER_ADVICE: &str = "常见于要先登录的公共 Wi-Fi（门户页），也可能是网络里有人往没加密的流量里塞东西。先在浏览器里完成这个 Wi-Fi 的登录再重新检测；\
                                  登录后还这样，就别在这张网上处理要紧的事。Claude 的连接是 HTTPS、有证书校验，内容改不了，但千万别忽略任何证书警告。";

fn plain_http_url() -> String {
    std::env::var("CAM_PLAIN_HTTP_URL")
        .unwrap_or_else(|_| "http://cp.cloudflare.com/generate_204".into())
}

/// 取一下 plain_http_url()：(状态码, Location, 正文开头)；连不上、地址不是 http:// 返回 None。
/// 自己拼一个最简单的 HTTP 请求：不走代理、不跟着跳转。
fn plain_http_probe() -> Option<(u16, String, String)> {
    use std::io::{Read, Write};
    let url = plain_http_url();
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if hostport.is_empty() {
        return None;
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (hostport, 80),
    };
    let timeout = Duration::from_secs(PLAIN_HTTP_SECS);
    let addr = (host, port).to_socket_addrs().ok()?.next()?;
    let mut sock = std::net::TcpStream::connect_timeout(&addr, timeout).ok()?;
    sock.set_read_timeout(Some(timeout)).ok()?;
    sock.set_write_timeout(Some(timeout)).ok()?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: claude-account-manager/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path,
        hostport,
        super::api::VERSION
    );
    sock.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < 4096 {
        match sock.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        // 头收全了、又是没有正文的回应：不用等对面关连接
        if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.starts_with(b"HTTP/1.1 204") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let location = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    Some((status, location, body.chars().take(200).collect()))
}

/// plain_http_probe 拿到的东西对不对：空的 204 是 {ok: true}；不是就是被改了，{ok: false, got: 拿到了什么}。
/// 与 cam.py 的 plain_http_verdict 一致（tests/shared/service.json）。
pub fn plain_http_verdict(status: u16, location: &str, body: &str) -> Value {
    let has_body = !body.trim().is_empty();
    if status == 204 && !has_body {
        return json!({"ok": true});
    }
    if (300..400).contains(&status) && !location.is_empty() {
        let rest = location
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(location);
        let hostport = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let hostport = hostport
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(hostport);
        let host = hostport
            .split(':')
            .next()
            .unwrap_or(hostport)
            .to_lowercase();
        let host = if host.is_empty() {
            location.chars().take(60).collect()
        } else {
            host
        };
        return json!({"ok": false, "got": format!("被带去了 {}（HTTP {}）", host, status)});
    }
    if has_body {
        return json!({"ok": false, "got": format!("HTTP {}，回来的是一段内容（本该是空的）", status)});
    }
    json!({"ok": false, "got": format!("HTTP {}（本该是 204）", status)})
}

/// 明文网页有没有被中途改动；连不上返回 None（不知道）。
fn plain_http_check() -> Option<Value> {
    plain_http_probe().map(|(st, loc, body)| plain_http_verdict(st, &loc, &body))
}

/// 服务检测里直连一下 api.anthropic.com、claude.ai 的端口（只建 TCP 连接，不发数据）最多等多久
const TCP_PROBE_SECS: u64 = 5;

/// 连不上 host 时卡在哪一层，写成一句接在“连不上”后面（没测出来是空字符串）。
/// proxied：请求走的是代理（这时本机直连 host 的情况说明不了问题）；dns_ok：本机解析得出来；
/// tcp_why：直连 host 端口没连上的原因（连上了是 ""，没测是 None）；tls_ok：TLS 握手成没成（没测是 None）。
/// 与 cam.py 的 stuck_stage 一致（tests/shared/service.json）。
pub fn stuck_stage(
    host: &str,
    proxied: bool,
    dns_ok: bool,
    tcp_why: Option<&str>,
    tls_ok: Option<bool>,
) -> String {
    if proxied {
        return format!(
            "请求走的是代理，问题在代理这一段：看代理软件有没有开、它连不连得到 {}",
            host
        );
    }
    if !dns_ok {
        return format!("卡在域名解析这一步：系统解析不出 {}", host);
    }
    match (tcp_why, tls_ok) {
        (None, _) => String::new(),
        (Some(why), _) if !why.is_empty() => format!(
            "域名解析得出来，卡在建 TCP 连接这一步（{}）：多半是网络或防火墙拦了",
            why
        ),
        (_, None) => "TCP 连得上，卡在 TLS 握手或等服务端回应这一步".into(),
        (_, Some(false)) => {
            "TCP 连得上，卡在 TLS 握手这一步：多半是网络里有设备在干扰加密连接".into()
        }
        (_, Some(true)) => "TCP 连接和 TLS 握手都通，卡在等服务端回应这一步".into(),
    }
}

/// 地址里的端口；没写的话 https 是 443、http 是 80。
fn port_of(url: &str) -> u16 {
    let default = if url.starts_with("http://") { 80 } else { 443 };
    let rest = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let hostport = rest.split('/').next().unwrap_or(rest);
    hostport
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(default)
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
    service_check_prog(store, &Progress::none())
}

/// service_check 的本体；prog 给了就把每一步做到哪了记进去（界面的进度卡），结束时不替调用方 close。
pub fn service_check_prog(store: &Store, prog: &Progress) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut add = |kind: &str, level: &str, text: String, advice: &str| {
        let mut item = json!({"kind": kind, "level": level, "text": text, "advice": advice});
        if NOTICE_ADVICE.contains(&advice) {
            item["notice"] = json!(true); // 关注项：Anthropic 那边的状况，不扣环境评分
        }
        out.push(item)
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
    // 本机设的代理（环境变量和系统设置里的）：下面看它在不在运行，也决定进度卡上有没有“代理”那一步
    let mut proxy_entries = proxies.clone();
    proxy_entries.extend(
        system_proxy()
            .into_iter()
            .map(|p| ("系统设置".to_string(), p)),
    );
    let proxy_eps = proxy_endpoints(&proxy_entries);
    let lookup_off = ip_lookup_config(Some(store)).mode == "off";
    for (k, label) in service_steps(lookup_off, !proxy_eps.is_empty()) {
        prog.add(k, label, 10);
    }
    // 解析域名、出口地区、api.anthropic.com、claude.ai 这几件事同时做，各有各的时限：哪件卡住了都不拖着别的
    let t0 = Instant::now();
    let within = |secs: u64| t0 + Duration::from_secs(secs);
    let j_dns: Vec<_> = hosts
        .iter()
        .map(|h| {
            let h = h.clone();
            tracked(
                prog,
                "dns",
                "解析失败",
                |v: &(Result<Vec<String>, String>, u128)| matches!(&v.0, Ok(ips) if !ips.is_empty()),
                move || {
                    let t = Instant::now();
                    (resolve_host(&h), t.elapsed().as_millis())
                },
            )
        })
        .collect();
    let j_region = {
        let store = Store {
            root: store.root.clone(),
        };
        let prog = prog.clone();
        // 顺便更新界面常显的缓存和账号的地区记录
        // 评分的变化等下面全测完一起记
        spawn_job(move || check_region_with(&store, 0, false, &prog).is_ok())
    };
    // 有回应（哪怕是 4xx / 5xx）就算这一步做完了，结果在下面的清单里说
    let j_api = {
        let at = at.clone();
        tracked(
            prog,
            "api",
            "没连上",
            |v: &(u16, Value, u128)| v.0 != 0,
            move || {
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
            },
        )
    };
    let j_web = tracked(
        prog,
        "web",
        "没连上",
        |v: &(u16, Value, u128)| v.0 != 0,
        || {
            let t0 = Instant::now();
            let (code, body) = get_with_in(
                &format!("{}/api/organizations", web_base()),
                &[("Cookie".to_string(), "sessionKey=".to_string())],
                Duration::from_secs(SERVICE_PROBE_SECS),
            );
            (code, body, t0.elapsed().as_millis())
        },
    );
    // 续登录的地址、证书、本机设的代理也同时测
    let auth_urls = token_urls();
    let j_auth: Vec<_> = auth_urls
        .iter()
        .map(|u| {
            let u = u.clone();
            tracked(
                prog,
                "auth",
                "没连上",
                |v: &(u16, String, u128)| v.0 != 0,
                move || {
                    let t0 = Instant::now();
                    let (st, txt) = http_text_in(&u, Duration::from_secs(SERVICE_PROBE_SECS));
                    (st, txt, t0.elapsed().as_millis())
                },
            )
        })
        .collect();
    let j_proxy: Vec<_> = proxy_eps
        .iter()
        .map(|(_, h, p)| {
            let (h, p) = (h.clone(), *p);
            tracked(
                prog,
                "proxy",
                "连不上",
                |v: &String| v.is_empty(),
                move || port_open(&h, p, Duration::from_secs(PROXY_PROBE_SECS)),
            )
        })
        .collect();
    // 没走代理时再直连一下这两个地址的端口（只建 TCP 连接）：连不上的时候好说清楚卡在解析、TCP、TLS 还是等回应（stuck_stage）
    let proxied = !proxies.is_empty() || !proxy_eps.is_empty();
    let ports = [port_of(&api_base()), port_of(&web_base())];
    let j_tcp: Vec<_> = hosts
        .iter()
        .zip(ports)
        .map(|(h, port)| {
            if proxied {
                return None;
            }
            let h = h.clone();
            Some(spawn_job(move || {
                port_open(&h, port, Duration::from_secs(TCP_PROBE_SECS))
            }))
        })
        .collect();
    let j_tls = {
        let h = api_host.clone();
        if api_base().starts_with("https://") {
            tracked(
                prog,
                "tls",
                "没取到证书",
                |v: &Option<(Value, bool)>| v.is_some(),
                move || peer_issuer(&h, 443, Duration::from_secs(TLS_PROBE_SECS)),
            )
        } else {
            prog.skip("tls", "不是 HTTPS 地址");
            spawn_job(|| None)
        }
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
    let tcp_why: Vec<Option<String>> = j_tcp
        .iter()
        .map(|j| {
            j.as_ref().map(|j| {
                wait_job(j, within(TCP_PROBE_SECS + 1))
                    .unwrap_or_else(|| format!("超过 {} 秒没有回应", TCP_PROBE_SECS))
            })
        })
        .collect();
    let dns_got: Vec<_> = j_dns
        .iter()
        .map(|j| wait_job(j, within(SERVICE_DNS_SECS)))
        .collect();
    // 该等的都等过了：还没回来的记成没等到，接下来是整理结果
    let waited: Vec<&str> = SERVICE_STEPS
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| *k != "score")
        .collect();
    prog.expire(&waited);
    prog.start("score");
    let mut dns_ok: Vec<bool> = Vec::new(); // 和 hosts 一一对应：本机解析得出来没有
    let dns_advice = if proxies.is_empty() {
        "检查本机 DNS 或代理设置"
    } else {
        "检查本机 DNS 或代理设置；设了代理的话通常由代理解析，可忽略"
    };
    // 本机解析不了、但这个域名实际连得上（请求走的是代理，域名由代理解析）：不是问题，只说明一下
    let api_up = api.as_ref().is_some_and(|a| a.0 != 0);
    let web_up = web.as_ref().is_some_and(|w| w.0 != 0);
    for (host, got) in hosts.iter().zip(dns_got) {
        let up = (*host == api_host && api_up) || (*host == web_host && web_up);
        dns_ok.push(matches!(&got, Some((Ok(ips), _)) if !ips.is_empty()));
        let why = match got {
            Some((Ok(ips), ms)) if !ips.is_empty() => {
                let shown = ips.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
                // host 本身就是 IP 地址时没有“解析”这回事
                let literal = host
                    .trim_matches(|c| c == '[' || c == ']')
                    .parse::<std::net::IpAddr>()
                    .is_ok();
                if !literal && ips.iter().any(|i| addr_special(i)) {
                    // 解析成了内网 / 保留地址：解析被本机的软件接管了，说明一下
                    add(
                        "dns",
                        "info",
                        format!(
                            "DNS：{} → {}（{} ms）是内网 / 保留地址，解析被本机的代理或安全软件接管了",
                            host, shown, ms
                        ),
                        DNS_SPECIAL_ADVICE,
                    );
                } else {
                    add(
                        "dns",
                        "ok",
                        format!("DNS：{} → {}（{} ms）", host, shown, ms),
                        "",
                    );
                }
                continue;
            }
            Some((Err(e), _)) => format!("解析失败（{}）", e),
            _ => format!("解析超过 {} 秒没有回应", SERVICE_DNS_SECS),
        };
        if up {
            add(
                "dns",
                "info",
                format!("DNS：{} 本机{}，但 {} 连得上", host, why, host),
                DNS_VIA_PROXY_ADVICE,
            );
        } else {
            add("dns", "bad", format!("DNS：{} {}", host, why), dns_advice);
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
                &format!(
                    "{}{}",
                    region_advice(status),
                    if l == "UA" { UA_REGION_NOTE } else { "" }
                ),
            );
            // 几处地址看到的地区都一样时说一声；不一样的那条由 risk_items 给（domain_region）
            let doms = region_domains(&r);
            if !doms.is_empty() && doms.iter().all(|d| s(d, "loc") == Some(l)) {
                add(
                    "domain_region",
                    "ok",
                    format!(
                        "{} 看到的出口地区也是 {}",
                        doms.iter()
                            .map(|d| s(d, "host").unwrap_or(""))
                            .collect::<Vec<_>>()
                            .join("、"),
                        region_label(Some(l))
                    ),
                    "Claude Code 的请求发往 api.anthropic.com，续登录走 platform.claude.com；这几处看到的地区和 claude.ai 一致。",
                );
            }
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
    // 连不上 host 时接在后面的那一句：卡在哪一层
    let stage = |host: &str, tls_ok: Option<bool>| -> String {
        let i = hosts.iter().position(|h| h == host).unwrap_or(0);
        let text = stuck_stage(
            host,
            proxied,
            dns_ok.get(i).copied().unwrap_or(false),
            tcp_why.get(i).and_then(|w| w.as_deref()),
            tls_ok,
        );
        if text.is_empty() {
            text
        } else {
            format!("；{}", text)
        }
    };
    if st == 0 {
        let tls_ok = api_base().starts_with("https://").then_some(tls.is_some());
        add(
            "api",
            "bad",
            format!(
                "{}：连不上（{}）{}",
                api_host,
                msg,
                stage(&api_host, tls_ok)
            ),
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
            "info",
            format!(
                "{}：服务可达，但当前登录已过期或无效（{} ms）",
                api_host, ms
            ),
            "运行一次 claude 会自动续登录，不影响切换账号；不扣环境评分",
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
            SERVER_ERROR_ADVICE,
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
            format!("{}：连不上（{}）{}", web_host, msg, stage(&web_host, None)),
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
            SERVER_ERROR_ADVICE,
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
    // 总体结论放最前面：评分 + 档位 + 最严重的一条。关注项（Anthropic 那边的状况）不扣分、不算本机这边的问题：
    // 本机这边有问题时说最严重的那条，那边也有状况就接在后面；本机这边没问题而那边有状况，写明“本机环境没问题，那边有状况”，级别是“注意”
    let sc = risk_score(&out);
    let pick = |notice: bool| -> Option<Value> {
        let hit = |lv: &str| {
            out.iter()
                .find(|i| s(i, "level") == Some(lv) && is_notice(i) == notice)
                .cloned()
        };
        hit("bad").or_else(|| hit("warn"))
    };
    let (worst, notice) = (pick(false), pick(true));
    let grade = s(&sc, "grade").unwrap_or("");
    let level = if worst.as_ref().is_some_and(|w| s(w, "level") == Some("bad")) || grade == "high" {
        "bad"
    } else if worst.is_some() || notice.is_some() || grade == "medium" {
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
    let failed = out.iter().any(|i| s(i, "kind") == Some("env_failed"));
    let fine = if failed {
        "这次没测到出口地区，先按上一次查到的算，其它没有发现风险点"
    } else {
        "出口地区在支持名单内，没有发现风险点"
    };
    let theirs_text = notice
        .as_ref()
        .map(|n| {
            format!(
                "Anthropic 那边有状况（只提醒，不扣分）：{}",
                s(n, "text").unwrap_or("")
            )
        })
        .unwrap_or_default();
    let (text, advice) = match (&worst, &notice) {
        (Some(w), n) => (
            format!(
                "{} —— {}{}",
                head,
                s(w, "text").unwrap_or(""),
                if n.is_some() {
                    format!("；另外{}", theirs_text)
                } else {
                    String::new()
                }
            ),
            w["advice"].clone(),
        ),
        (None, Some(n)) => (
            format!("{}；本机环境这边{}。{}", head, fine, theirs_text),
            n["advice"].clone(),
        ),
        (None, None) => (
            format!(
                "{}；Claude 服务{}，{}",
                head,
                if failed { "连得上" } else { "正常" },
                fine
            ),
            json!(RISK_SCORE_ADVICE),
        ),
    };
    let summary = json!({"kind": "summary", "level": level, "score": sc["score"], "grade": grade,
                         "text": text, "advice": advice});
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
            note_risk_in(&mut d, None);
            let _ = store.save(&mut d);
        }
    }
    prog.finish("score", true, "");
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
    out.push(src(
        "cp.cloudflare.com（http）",
        "看没加密的网页有没有被中途改动：只取一个空白页，不带任何本机信息",
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
            o.remove("risk_log"); // 分数变化的说明里带着出口地区，一起清
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
