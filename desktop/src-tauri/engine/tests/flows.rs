//! 端到端测试：临时目录当 Claude Code 配置目录和数据目录，本地起一个假的 Anthropic 接口，不碰真实账号。
//! 对应 scripts/test_cam.py 里的同名场景。

use cam_engine::api::{account_health, api_switch, public_state, route};
use cam_engine::claude::{
    apply_pending_switch, apply_pending_switch_at, claude_may_be_running, etime_secs,
    import_current, is_claude_cmd, pending_held_error, pending_holders, switch_account,
    PENDING_HELD, PENDING_ID,
};
use cam_engine::health::{
    check_region, clear_region_history, exit_trace, parse_exit_meta, privacy_view,
    region_recheck_due_at, risk_items, risk_score, save_privacy, service_check, system_dns_servers,
    tunnel_kind, RegionRecheck,
};
use cam_engine::rules::{apply_time_rules_at, save_rules};
use cam_engine::store::{parse_time, read_json, try_lock_file, update_json_file, Store};
use cam_engine::usage::{
    ensure_fresh_token, error_body, keep_login_invalid, local_window_usage, refresh_usage,
    token_answer_dead, USAGE_GIVE_UP, USAGE_TIMEOUT,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- 假接口

#[derive(Default)]
struct Mock {
    /// access token -> 邮箱
    tokens: HashMap<String, String>,
    /// refresh token -> (新 access token, 新 refresh token)
    refresh: HashMap<String, (String, String)>,
    /// (方法, 路径, 用到的 refresh token)
    calls: Vec<(String, String, String)>,
    /// /cdn-cgi/trace 返回的地区，空表示 US
    trace_loc: String,
    trace_warp: bool,
    /// 服务器时间相对本机的偏差（秒）
    trace_ts_offset: i64,
    /// 官方状态页：空表示 none
    status_indicator: String,
    /// /meta 和 /ipapi 返回的运营商，空表示 Comcast
    meta_org: String,
    /// /meta 和 /ipapi 返回的国家，空表示和 trace 一致
    meta_country: String,
    /// /ipapi（ip-api.com 格式）的 IP 类型：空表示 residential；"fail" 表示查询失败
    ipapi_type: String,
    /// /rdap/<ip> 返回的注册国：空表示 US；"-" 表示记录没写
    rdap_country: String,
    /// /trace6 的地区：空表示 IPv6 不可用
    trace6_loc: String,
    /// /cdn-cgi/trace 返回的出口 IP，空表示 1.2.3.4
    trace_ip: String,
    /// /api/oauth/usage 固定回这个状态码（比如 429）；0 表示照常
    usage_code: u16,
    /// /api/oauth/profile 固定回这个状态码；0 表示照常
    profile_code: u16,
    /// /v1/oauth/token 固定回 400 和这段原文（可以不是 JSON）；空表示照常
    token_error: String,
    /// 这些路径（按前缀）一律回 503：接口连得上但答不上来
    down: Vec<String>,
    /// 这些路径（按前缀）收到请求先卡着不回（卡住的接口），drain() 让等着的马上放行
    slow: Vec<String>,
    /// 这些路径（按前缀）回一段不是 HTTP 的东西：对方一下就失败（连接被重置那一类，状态码 0）
    broken: Vec<String>,
}

fn mock() -> &'static Mutex<Mock> {
    static M: OnceLock<Mutex<Mock>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 卡住的请求（Mock::slow）等的信号：(放行过几回, 现在卡着几个)
fn hold() -> &'static (Mutex<(u64, usize)>, Condvar) {
    static H: OnceLock<(Mutex<(u64, usize)>, Condvar)> = OnceLock::new();
    H.get_or_init(Default::default)
}

/// 假接口里卡一个请求：等到 drain() 放行，最多 30 秒
fn held() {
    let (h, cv) = hold();
    let mut g = h.lock().unwrap();
    let (round, end) = (g.0, Instant::now() + Duration::from_secs(30));
    g.1 += 1;
    while g.0 == round && Instant::now() < end {
        g = cv.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
    }
    g.1 -= 1;
    cv.notify_all();
}

/// 让假接口里还卡着的请求马上放行、等它们走完，免得拖进下一个用例。
fn drain() {
    mock().lock().unwrap().slow.clear();
    let (h, cv) = hold();
    let mut g = h.lock().unwrap();
    g.0 += 1;
    cv.notify_all();
    let end = Instant::now() + Duration::from_secs(5);
    while g.1 > 0 && Instant::now() < end {
        g = cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
    }
}

fn mock_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let srv = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", srv.server_addr().to_ip().unwrap());
        // 几个线程一起接：卡着不回的请求（slow）不挡着别的
        let serve = move || {
            for mut req in srv.incoming_requests() {
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.as_str().trim_start_matches("Bearer ").to_string())
                    .unwrap_or_default();
                let path = req.url().to_string();
                let mut m = mock().lock().unwrap();
                let rt = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|b| b["refresh_token"].as_str().map(String::from))
                    .unwrap_or_default();
                m.calls.push((req.method().to_string(), path.clone(), rt.clone()));
                if m.slow.iter().any(|p| path.starts_with(p.as_str())) {
                    drop(m);
                    held();
                    m = mock().lock().unwrap();
                }
                if m.broken.iter().any(|p| path.starts_with(p.as_str())) {
                    drop(m);
                    let mut w = req.into_writer();
                    let _ = w.write_all(b"not http\r\n\r\n").and_then(|_| w.flush());
                    continue;
                }
                if m.down.iter().any(|p| path.starts_with(p.as_str())) {
                    drop(m);
                    let _ = req.respond(tiny_http::Response::from_string(json!({"error": {"message": "down"}}).to_string()).with_status_code(503));
                    continue;
                }
                // /cf/…：Cloudflare 自己的 trace（claude.ai 没回音时的备用），节点写成 LAX 好区分
                if path == "/cdn-cgi/trace" || path == "/cf/cdn-cgi/trace" {
                    let loc = if m.trace_loc.is_empty() { "US".to_string() } else { m.trace_loc.clone() };
                    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() + m.trace_ts_offset as f64;
                    let warp = if m.trace_warp { "on" } else { "off" };
                    let ip = if m.trace_ip.is_empty() { "1.2.3.4".to_string() } else { m.trace_ip.clone() };
                    drop(m);
                    let colo = if path.starts_with("/cf/") { "LAX" } else { "SJC" };
                    let body = format!("fl=1\nip={}\nts={:.3}\nloc={}\ncolo={}\nwarp={}\n", ip, ts, loc, colo, warp);
                    let _ = req.respond(tiny_http::Response::from_string(body));
                    continue;
                }
                if path.starts_with("/ipapiis") || path.starts_with("/ipinfo") {
                    // ipapi.is：带 key 返回完整信息和标记，不带 key 只有简版；ipinfo.io：运营商和国家，没有标记
                    let org = if m.meta_org.is_empty() { "Comcast Cable Communications, LLC".to_string() } else { m.meta_org.clone() };
                    let country = if !m.meta_country.is_empty() {
                        m.meta_country.clone()
                    } else if m.trace_loc.is_empty() {
                        "US".to_string()
                    } else {
                        m.trace_loc.clone()
                    };
                    let t = if m.ipapi_type.is_empty() { "residential".to_string() } else { m.ipapi_type.clone() };
                    let rdap = match m.rdap_country.as_str() {
                        "-" => None,
                        "" => Some("us".to_string()),
                        c => Some(c.to_lowercase()),
                    };
                    drop(m);
                    let (code, body) = if path.starts_with("/ipinfo") {
                        (200, json!({"ip": "1.2.3.4", "org": format!("AS7922 {}", org), "city": "San Jose", "region": "California", "country": country}))
                    } else if t == "fail" {
                        (429, json!({"error": "quota"}))
                    } else if !path.contains("key=") {
                        (200, json!({"ip": "1.2.3.4", "company": org, "asn": format!("AS7922 {}", org), "city": "San Jose",
                                     "region": "California", "country": "United States"}))
                    } else {
                        let mut asn = json!({"asn": 7922, "org": org});
                        if let Some(c) = rdap {
                            asn["country"] = json!(c);
                        }
                        (200, json!({"ip": "1.2.3.4", "is_datacenter": t == "hosting", "is_vpn": t == "proxy", "is_proxy": false,
                                     "is_tor": t == "tor", "is_mobile": t == "mobile", "asn": asn,
                                     "company": {"name": org, "type": if t == "hosting" { "hosting" } else { "isp" }},
                                     "location": {"country_code": country, "city": "San Jose", "state": "California"}}))
                    };
                    let _ = req.respond(tiny_http::Response::from_string(body.to_string()).with_status_code(code));
                    continue;
                }
                if path.starts_with("/ipapi") {
                    let body = if m.ipapi_type == "fail" {
                        json!({"status": "fail", "message": "quota"})
                    } else {
                        let org = if m.meta_org.is_empty() { "Comcast Cable Communications, LLC" } else { m.meta_org.as_str() };
                        let country = if !m.meta_country.is_empty() {
                            m.meta_country.clone()
                        } else if m.trace_loc.is_empty() {
                            "US".to_string()
                        } else {
                            m.trace_loc.clone()
                        };
                        let t = m.ipapi_type.as_str();
                        json!({"status": "success", "countryCode": country, "city": "San Jose", "regionName": "California",
                               "isp": org, "org": org, "as": format!("AS7922 {}", org), "mobile": t == "mobile",
                               "proxy": t == "proxy", "hosting": t == "hosting", "query": "1.2.3.4"})
                    }
                    .to_string();
                    drop(m);
                    let _ = req.respond(tiny_http::Response::from_string(body));
                    continue;
                }
                if path.starts_with("/rdap/") {
                    let body = match m.rdap_country.as_str() {
                        "-" => json!({"handle": "NET-1"}),
                        "" => json!({"handle": "NET-1", "country": "US"}),
                        c => json!({"handle": "NET-1", "country": c}),
                    }
                    .to_string();
                    drop(m);
                    let _ = req.respond(tiny_http::Response::from_string(body));
                    continue;
                }
                if path == "/trace4" {
                    let loc = if m.trace_loc.is_empty() { "US".to_string() } else { m.trace_loc.clone() };
                    drop(m);
                    let _ = req.respond(tiny_http::Response::from_string(format!("ip=1.2.3.4\nloc={}\ncolo=SJC\n", loc)));
                    continue;
                }
                if path == "/trace6" {
                    let loc = m.trace6_loc.clone();
                    drop(m);
                    let _ = if loc.is_empty() {
                        req.respond(tiny_http::Response::from_string("{}").with_status_code(404))
                    } else {
                        req.respond(tiny_http::Response::from_string(format!("ip=2001:db8::1\nloc={}\ncolo=HKG\n", loc)))
                    };
                    continue;
                }
                if path == "/meta" {
                    let org = if m.meta_org.is_empty() { "Comcast Cable Communications, LLC" } else { m.meta_org.as_str() };
                    let country = if !m.meta_country.is_empty() {
                        m.meta_country.clone()
                    } else if m.trace_loc.is_empty() {
                        "US".to_string()
                    } else {
                        m.trace_loc.clone()
                    };
                    let body = json!({"clientIp": "1.2.3.4", "asn": 7922, "asOrganization": org, "city": "San Jose",
                                      "region": "California", "country": country})
                    .to_string();
                    drop(m);
                    let _ = req.respond(tiny_http::Response::from_string(body));
                    continue;
                }
                if path == "/status.json" {
                    let ind = if m.status_indicator.is_empty() { "none" } else { m.status_indicator.as_str() };
                    let desc = match ind {
                        "minor" => "Degraded Performance",
                        "major" => "Partial System Outage",
                        "critical" => "Major System Outage",
                        _ => "All Systems Operational",
                    };
                    let body = json!({"status": {"indicator": ind, "description": desc}}).to_string();
                    drop(m);
                    let _ = req.respond(tiny_http::Response::from_string(body));
                    continue;
                }
                if path == "/v1/oauth/token" && !m.token_error.is_empty() {
                    let body = m.token_error.clone();
                    drop(m);
                    let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(400));
                    continue;
                }
                let (code, out) = match path.as_str() {
                    "/api/organizations" => (403, json!({"error": {"message": "unauthorized"}})),
                    "/api/oauth/usage" if m.usage_code != 0 => (m.usage_code, json!({"error": {"message": "rate limited"}})),
                    "/api/oauth/profile" if m.profile_code != 0 => (m.profile_code, json!({"error": {"message": "rate limited"}})),
                    "/api/oauth/usage" => match m.tokens.get(&auth) {
                        Some(_) => (200, json!({"five_hour": {"utilization": 12.0, "resets_at": "2099-01-01T00:00:00Z"}})),
                        None => (401, json!({"error": {"message": "invalid token"}})),
                    },
                    "/api/oauth/profile" => match m.tokens.get(&auth) {
                        Some(e) => (200, json!({"account": {"email": e}, "organization": {"name": "org"}})),
                        None => (401, json!({"error": {"message": "invalid token"}})),
                    },
                    "/v1/oauth/token" => match m.refresh.get(&rt) {
                        Some((at, nrt)) => (200, json!({"access_token": at, "refresh_token": nrt, "expires_in": 28800})),
                        None => (400, json!({"error": {"message": "invalid_grant"}})),
                    },
                    _ => (404, json!({})),
                };
                drop(m);
                let _ = req.respond(tiny_http::Response::from_string(out.to_string()).with_status_code(code));
            }
        };
        for _ in 0..16 {
            std::thread::spawn(serve.clone());
        }
        base
    })
}

fn token_calls(rt: &str) -> usize {
    mock()
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.1 == "/v1/oauth/token" && c.2 == rt)
        .count()
}

// ---------------------------------------------------------------- 测试环境

/// 环境变量是全进程共享的，测试一个一个跑。
fn serial() -> MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

struct Env {
    root: PathBuf,
    _g: MutexGuard<'static, ()>,
}

impl Env {
    fn new() -> Env {
        let g = serial();
        let base = mock_base();
        drain(); // 上一个用例要是留着卡住的请求，先放掉
        *mock().lock().unwrap() = Mock::default();
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("cam-engine-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(root.join("claude/projects/p1")).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", root.join("claude"));
        std::env::set_var("CAM_HOME", root.join("cam"));
        std::env::set_var("CAM_NO_KEYCHAIN", "1");
        std::env::set_var("CAM_TEST_CLAUDE_PIDS", "");
        std::env::set_var("CAM_API_BASE", base);
        std::env::set_var("CAM_CLAUDE_WEB_BASE", base);
        std::env::set_var("CAM_TOKEN_URLS", format!("{}/v1/oauth/token", base));
        std::env::set_var("CAM_STATUS_URL", format!("{}/status.json", base));
        std::env::set_var("CAM_CF_META_URL", format!("{}/meta", base));
        std::env::set_var("CAM_IPAPI_URL", format!("{}/ipapi", base));
        std::env::set_var("CAM_IPAPI_IS_URL", format!("{}/ipapiis", base));
        std::env::set_var("CAM_IPAPI_IS_KEY", "test-key"); // 默认按“配了 key”测，带类型标记
        std::env::set_var("CAM_IPINFO_URL", format!("{}/ipinfo", base));
        std::env::set_var("CAM_META_TTL", "0"); // 测试里每次都重查运营商；缓存单独测
        std::env::remove_var("CAM_IP_LOOKUP");
        std::env::set_var("CAM_RDAP_URL", format!("{}/rdap/", base));
        std::env::set_var("CAM_CF_TRACE4_URL", format!("{}/trace4", base));
        std::env::set_var("CAM_CF_TRACE6_URL", format!("{}/trace6", base));
        // claude.ai 没回音时的备用 trace 也指到假接口，不去问真的 www.cloudflare.com
        std::env::set_var("CAM_CF_TRACE_URL", format!("{}/cf/cdn-cgi/trace", base));
        // 本机连的是哪张网：固定住，要测换网络的用例自己改
        std::env::set_var("CAM_TEST_NET", "net-a");
        // 各种时限用回正式的值，要调短的用例自己设
        for k in [
            "CAM_TEST_TRACE_HEDGE_MS",
            "CAM_TEST_META_BUDGET_MS",
            "CAM_TEST_META_SOURCE_MS",
            "CAM_TEST_USAGE_TIMEOUT_MS",
            "CAM_TEST_USAGE_GIVE_UP_MS",
        ] {
            std::env::remove_var(k);
        }
        std::env::set_var("CAM_TEST_LOCALE", "en_US");
        std::env::set_var("CAM_TEST_DNS", "192.168.1.1");
        std::env::set_var("CAM_TEST_PROXY", ""); // 不读本机的系统代理设置
        std::env::set_var("CAM_TEST_TZ", "America/New_York=US"); // 跑测试的机器时区不定，固定成和假出口一致
        std::env::set_var("CAM_TEST_ROUTE_IF", "");
        for k in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::remove_var(k);
        }
        let e = Env { root, _g: g };
        std::fs::write(
            e.cj(),
            json!({"numStartups": 42, "projects": {"/work/x": {"allowedTools": ["Bash"]}}})
                .to_string(),
        )
        .unwrap();
        e
    }
    fn cj(&self) -> PathBuf {
        self.root.join("claude/.claude.json")
    }
    fn creds(&self) -> PathBuf {
        self.root.join("claude/.credentials.json")
    }
    fn write_creds(&self, at: &str, rt: &str, expires_ms: i64) {
        let v = json!({"claudeAiOauth": {"accessToken": at, "refreshToken": rt, "expiresAt": expires_ms,
                                         "subscriptionType": "max"}});
        std::fs::write(self.creds(), v.to_string()).unwrap();
    }
    fn login(&self, at: &str, rt: &str, uuid: &str, email: &str, expires_ms: i64) {
        self.write_creds(at, rt, expires_ms);
        let mut cj = read_json(&self.cj()).unwrap().unwrap();
        cj["oauthAccount"] = json!({"accountUuid": uuid, "emailAddress": email});
        std::fs::write(self.cj(), cj.to_string()).unwrap();
    }
    fn live(&self) -> (Value, Value) {
        let c = read_json(&self.creds()).unwrap().unwrap();
        (
            c["claudeAiOauth"].clone(),
            read_json(&self.cj()).unwrap().unwrap(),
        )
    }
    fn data(&self) -> Value {
        Store::new().load().unwrap()
    }
    fn id_of(&self, email: &str) -> String {
        self.data()["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["email"] == email)
            .map(|a| a["id"].as_str().unwrap().to_string())
            .unwrap_or_else(|| panic!("没有 {}", email))
    }
    fn secret_rt(&self, id: &str) -> Value {
        Store::new().load_secret(id)["claudeAiOauth"]["refreshToken"].clone()
    }
    /// 导入 A、B 两个账号，当前是 B。
    fn two_accounts(&self, a_expires: i64) -> (String, String) {
        let store = Store::new();
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com", a_expires);
        import_current(&store, Some("A")).unwrap();
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com", later());
        import_current(&store, Some("B")).unwrap();
        (self.id_of("a@x.com"), self.id_of("b@x.com"))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn later() -> i64 {
    now_ms() + 3_600_000
}
fn past() -> i64 {
    now_ms() - 60_000
}

// ---------------------------------------------------------------- 切换

#[test]
fn switch_keeps_everything_and_writes_back_refreshed_token() {
    let e = Env::new();
    let (_, b) = e.two_accounts(later());
    let store = Store::new();
    e.login("at-B2", "rt-B2", "uuid-B", "b@x.com", later()); // Claude Code 自己刷新了 B
    let r = switch_account(&store, "A", false).unwrap();
    assert!(r.changed);
    let (live, cj) = e.live();
    assert_eq!(live["accessToken"], "at-A1");
    assert_eq!(cj["oauthAccount"]["emailAddress"], "a@x.com");
    assert_eq!(cj["numStartups"], 42);
    assert_eq!(
        cj["projects"],
        json!({"/work/x": {"allowedTools": ["Bash"]}})
    );
    assert_eq!(e.secret_rt(&b), "rt-B2");
    assert_eq!(std::fs::read_dir(store.backups_dir()).unwrap().count(), 1);
    switch_account(&store, "2", false).unwrap();
    assert_eq!(e.live().0["refreshToken"], "rt-B2");
    assert!(!switch_account(&store, "b@x.com", false).unwrap().changed);
}

#[test]
fn old_session_writing_back_unchanged_token_is_filed_under_its_owner() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    // 切换前开着的 claude 会话把 A 的凭据原样写回；~/.claude.json 仍然写着 B
    e.write_creds("at-A1", "rt-A1", later());
    import_current(&store, None).unwrap();
    let d = e.data();
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.secret_rt(&a), "rt-A1");
    assert_eq!(
        Store::new().load_secret(&a)["oauthAccount"]["emailAddress"],
        "a@x.com"
    );
    assert_eq!(d["active"]["claude_code"], json!(a));
    let msg = d["live_mismatch"]["message"].as_str().unwrap();
    assert!(msg.contains("“A”") && msg.contains("b@x.com"), "{}", msg);
    assert_eq!(public_state(&store, &d)["warning"], json!(msg));
    // 重新切到 B 后恢复一致
    switch_account(&store, "B", false).unwrap();
    assert!(e.data().get("live_mismatch").is_none());
    assert_eq!(e.live().0["refreshToken"], "rt-B1");
}

#[test]
fn old_session_writing_back_refreshed_token_is_verified_by_profile() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-A2".into(), "a@x.com".into());
    e.write_creds("at-A2", "rt-A2", later());
    import_current(&store, None).unwrap();
    assert_eq!(e.secret_rt(&a), "rt-A2");
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.data()["active"]["claude_code"], json!(a));
}

fn pending_empty(store: &Store) -> bool {
    store
        .load_secret(PENDING_ID)
        .as_object()
        .map(|o| o.is_empty())
        .unwrap_or(true)
}

#[test]
fn unverifiable_login_is_parked_not_filed() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    // 旧会话刷新了 A 的 token 后写回，等到这里运行时 access token 已过期：核对不了，不能信 ~/.claude.json 里的 B
    e.write_creds("at-A2", "rt-A2", past());
    let n = mock().lock().unwrap().calls.len();
    let err = import_current(&store, Some("X")).unwrap_err();
    assert!(err.contains("无法确认"), "{}", err);
    assert_eq!(mock().lock().unwrap().calls.len(), n); // 过期了不用白问
    assert_eq!(e.secret_rt(&a), "rt-A1");
    assert_eq!(e.secret_rt(&b), "rt-B1"); // 没被 A 的 token 覆盖
    let p = store.load_secret(PENDING_ID);
    assert_eq!(p["claudeAiOauth"]["refreshToken"], "rt-A2");
    assert_eq!(p["candidate"], json!(b));
    let d = e.data();
    assert_eq!(d["active"]["claude_code"], json!(b)); // 显示上先按 ~/.claude.json 猜
    assert!(public_state(&store, &d)["warning"]
        .as_str()
        .unwrap()
        .contains("无法确认"));
    // 旧会话又续了一次 access token（refresh token 没换）：现在能核对，归到 A 名下，B 不动，暂存清掉
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-A3".into(), "a@x.com".into());
    e.write_creds("at-A3", "rt-A2", later());
    import_current(&store, None).unwrap();
    assert_eq!(e.secret_rt(&a), "rt-A2");
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert!(pending_empty(&store));
    let d = e.data();
    assert_eq!(d["active"]["claude_code"], json!(a));
    assert!(d.get("live_mismatch").is_some() && d.get("live_unverified").is_none());
}

#[test]
fn offline_login_is_parked_until_online() {
    let e = Env::new();
    let (_a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    // B 自己换了 token（正常轮换），但这会儿没网
    e.write_creds("at-B2", "rt-B2", later());
    std::env::set_var("CAM_API_BASE", "http://127.0.0.1:1");
    let r = import_current(&store, None);
    std::env::set_var("CAM_API_BASE", mock_base());
    assert!(r.unwrap_err().contains("无法确认"));
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(store.load_secret(PENDING_ID)["candidate"], json!(b));
    assert!(e.data().get("live_unverified").is_some());
    // 网络恢复后自动归档到 B，提醒消失
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-B2".into(), "b@x.com".into());
    import_current(&store, None).unwrap();
    assert_eq!(e.secret_rt(&b), "rt-B2");
    assert!(pending_empty(&store));
    let d = e.data();
    assert!(d.get("live_unverified").is_none() && d.get("live_mismatch").is_none());
}

#[test]
fn switch_adopts_parked_login_after_refresh() {
    let e = Env::new();
    let (_a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    // B 换过 token，但这里运行时 access token 已过期：先暂存；随后切走，B 换过的 token 只剩暂存这一份
    e.write_creds("at-B2", "rt-B2", past());
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-A1".into(), "a@x.com".into()); // A 保存的登录是好的
    switch_account(&store, "A", false).unwrap();
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.live().0["refreshToken"], "rt-A1");
    // 切回 B：用暂存的 refresh token 换新并核对，确认是 B 的才存进 B，切换就用上最新的
    {
        let mut m = mock().lock().unwrap();
        m.refresh
            .insert("rt-B2".into(), ("at-B3".into(), "rt-B3".into()));
        m.tokens.insert("at-B3".into(), "b@x.com".into());
    }
    switch_account(&store, "B", false).unwrap();
    assert_eq!(e.live().0["refreshToken"], "rt-B3");
    assert_eq!(e.secret_rt(&b), "rt-B3");
    assert!(pending_empty(&store));
    assert!(e.data().get("live_unverified").is_none());
}

#[test]
fn switch_files_parked_login_under_its_real_owner() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    // 暂存的其实是旧会话写回的 A 的 token，但 ~/.claude.json 说是 B
    e.write_creds("at-A2", "rt-A2", past());
    // A 保存的那份 access token 服务端还认（不认的情况见 parked_login_guessed_for_another_account）
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-A1".into(), "a@x.com".into());
    switch_account(&store, "A", false).unwrap();
    {
        let mut m = mock().lock().unwrap();
        m.refresh
            .insert("rt-A2".into(), ("at-A3".into(), "rt-A3".into()));
        m.tokens.insert("at-A3".into(), "a@x.com".into());
    }
    switch_account(&store, "B", false).unwrap();
    assert_eq!(e.live().0["refreshToken"], "rt-B1"); // B 用的是自己保存的
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.secret_rt(&a), "rt-A3"); // 换新后归到 A
    assert!(pending_empty(&store));
}

// ---- 暂存着更新的登录时，不在用的号不能被报成“登录已失效”；切换前先核对

/// 假接口认的 access token（整个换掉）：(access token, 邮箱)
fn set_tokens(list: &[(&str, &str)]) {
    mock().lock().unwrap().tokens = list
        .iter()
        .map(|(t, e)| (t.to_string(), e.to_string()))
        .collect();
}

/// 假接口认的 refresh token（整个换掉）：(refresh token, 新 access token, 新 refresh token)
fn set_refresh(list: &[(&str, &str, &str)]) {
    mock().lock().unwrap().refresh = list
        .iter()
        .map(|(rt, at, nrt)| (rt.to_string(), (at.to_string(), nrt.to_string())))
        .collect();
}

fn clear_calls() {
    mock().lock().unwrap().calls.clear();
}

/// 拿去 token 接口换过的 refresh token，按先后
fn token_log() -> Vec<String> {
    mock()
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.1 == "/v1/oauth/token")
        .map(|c| c.2.clone())
        .collect()
}

fn profile_calls() -> usize {
    mock()
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.1 == "/api/oauth/profile")
        .count()
}

fn acc_of(e: &Env, id: &str) -> Value {
    e.data()["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == json!(id))
        .cloned()
        .unwrap_or_else(|| panic!("没有账号 {}", id))
}

fn usage_error(e: &Env, id: &str) -> String {
    acc_of(e, id)["usage"]["error"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn incident_count(acc: &Value) -> usize {
    acc["incidents"].as_array().map(Vec::len).unwrap_or(0)
}

/// (level, label, 各条的 kind)
fn health_of(acc: &Value) -> (String, String, Vec<String>) {
    let h = account_health(acc, true, None);
    (
        h["level"].as_str().unwrap_or("").to_string(),
        h["label"].as_str().unwrap_or("").to_string(),
        kinds(h["issues"].as_array().unwrap()),
    )
}

/// 这么多秒以前的时间
fn ago(secs: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// 假装有这些 claude 会话：逗号隔开的 pid[:已运行秒数[:登录方式]]；"unknown" 是进程没查出来；空串是一个都没有
fn fake_sessions(spec: &str) {
    std::env::set_var("CAM_TEST_CLAUDE_PIDS", spec);
}

/// B 换过 token（rt-B2），这里运行时核对不了，暂存着；随后切到 A。B 名下保存的还是被顶掉的 rt-B1。
/// 服务端只认 rt-B2（换出 at-B3 / rt-B3）和 A 的 at-A1。released_secs_ago：把“让位”的时间改成这么多秒以前。
fn parked_b_then_on_a(e: &Env, released_secs_ago: Option<i64>) -> (String, String) {
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    e.write_creds("at-B2", "rt-B2", past());
    set_tokens(&[("at-A1", "a@x.com")]);
    switch_account(&store, "A", false).unwrap();
    assert_eq!(e.live().0["refreshToken"], "rt-A1");
    let mut pend = store.load_secret(PENDING_ID);
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(pend["candidate"], json!(b));
    assert_eq!(pend["claudeAiOauth"]["refreshToken"], "rt-B2");
    // 切走时记下：它从这时起不是当前登录
    assert!(pend["released_at"].as_str().is_some_and(|t| !t.is_empty()));
    if let Some(secs) = released_secs_ago {
        pend["released_at"] = json!(ago(secs));
        store.save_secret(PENDING_ID, &pend).unwrap();
    }
    set_refresh(&[("rt-B2", "at-B3", "rt-B3")]);
    set_tokens(&[("at-A1", "a@x.com"), ("at-B3", "b@x.com")]);
    clear_calls();
    (a, b)
}

#[test]
fn refresh_uses_parked_login_not_the_superseded_one() {
    // 保存的 access token 本地看还没过期（服务端已经不认）
    let e = Env::new();
    let (_a, b) = parked_b_then_on_a(&e, None);
    let store = Store::new();
    refresh_usage(&store, None, true).unwrap();
    let acc = acc_of(&e, &b);
    assert_eq!(acc["usage"]["error"], Value::Null);
    assert_eq!(acc["usage"]["five_hour"]["utilization"], 12.0);
    assert_eq!(health_of(&acc).0, "ok");
    assert_eq!(incident_count(&acc), 0);
    assert_eq!(e.secret_rt(&b), "rt-B3");
    assert!(pending_empty(&store));
    assert_eq!(token_log(), vec!["rt-B2"]); // 被顶掉的 rt-B1 一次都没拿去续
    assert_eq!(e.live().0["refreshToken"], "rt-A1"); // 当前登录没动
    assert!(e.data().get("live_unverified").is_none());
}

#[test]
fn refresh_uses_parked_login_when_saved_one_expired() {
    // 保存的 access token 也过期了（隔了几个小时再打开面板）：同样不报失效
    let e = Env::new();
    let (_a, b) = parked_b_then_on_a(&e, None);
    let store = Store::new();
    let mut sec = store.load_secret(&b);
    sec["claudeAiOauth"]["expiresAt"] = json!(past());
    store.save_secret(&b, &sec).unwrap();
    refresh_usage(&store, None, true).unwrap();
    let acc = acc_of(&e, &b);
    assert_eq!(acc["usage"]["error"], Value::Null);
    assert_eq!(
        health_of(&acc),
        ("ok".to_string(), "正常".to_string(), vec![])
    );
    assert_eq!(e.secret_rt(&b), "rt-B3");
    assert_eq!(token_log(), vec!["rt-B2"]);
}

#[test]
fn parked_login_held_by_session_is_not_login_invalid() {
    let e = Env::new();
    let (_a, b) = parked_b_then_on_a(&e, Some(3600));
    let store = Store::new();
    // 这个会话在 B 让位之前就开着（跑了两小时），可能还拿着 rt-B2：两个 refresh token 都不去换，也不说登录失效
    fake_sessions("4321:7200");
    refresh_usage(&store, None, true).unwrap();
    let acc = acc_of(&e, &b);
    let err = acc["usage"]["error"].as_str().unwrap();
    assert!(err.contains(PENDING_HELD), "{}", err);
    assert!(err.contains("先不用重新登录"), "{}", err);
    assert!(!err.contains("登录已失效"), "{}", err);
    assert_eq!(
        health_of(&acc),
        (
            "info".to_string(),
            "正常".to_string(),
            vec!["pending_login".to_string()]
        )
    );
    let h = account_health(&acc, true, None);
    assert_eq!(h["issues"][0]["text"], "有一份更新的登录暂存着，还没核对");
    assert!(h["issues"][0]["advice"]
        .as_str()
        .unwrap()
        .contains("不用重新登录"));
    assert_eq!(incident_count(&acc), 0);
    assert!(token_log().is_empty());
    assert_eq!(
        store.load_secret(PENDING_ID)["claudeAiOauth"]["refreshToken"],
        "rt-B2"
    );
    // 进程没查出来（ps 出错、超时）：当作可能有，同样不换
    fake_sessions("unknown");
    refresh_usage(&store, None, true).unwrap();
    assert!(usage_error(&e, &b).contains(PENDING_HELD));
    assert!(token_log().is_empty());
    // 会话是 B 让位之后才开的（只跑了十分钟，读到的是 A 的登录）：不算，照常核对并用上
    fake_sessions("4321:600");
    refresh_usage(&store, None, true).unwrap();
    fake_sessions("");
    assert_eq!(acc_of(&e, &b)["usage"]["error"], Value::Null);
    assert_eq!(e.secret_rt(&b), "rt-B3");
    assert_eq!(token_log(), vec!["rt-B2"]);
}

#[test]
fn pending_holders_skips_sessions_with_their_own_login() {
    let _e = Env::new();
    let released = json!({"released_at": ago(3600)});
    assert_eq!(pending_holders(&released), Some(0));
    fake_sessions("1:7200:machine,2:7200:own_token,3:60:machine,4:7200:unknown");
    assert_eq!(pending_holders(&released), Some(2)); // 自带 token 的、让位之后才开的不算
    assert_eq!(pending_holders(&json!({})), Some(4)); // 还是当前登录（没让位过）：都算
                                                      // 用 API Key 的、用别的配置目录的也不算；运行了多久、用的哪个登录都看不出来的（Windows）算
    fake_sessions("5:7200:api_key,6:7200:config_dir,7");
    assert_eq!(pending_holders(&released), Some(1));
    fake_sessions("unknown");
    assert_eq!(pending_holders(&released), None);
    assert_eq!(pending_holders(&json!({})), None);
    fake_sessions("");
    let got: Vec<Option<i64>> = ["00:05", "10:00", "02:00:00", "1-00:00:01", "x", "1:2:3:4"]
        .iter()
        .map(|t| etime_secs(t))
        .collect();
    assert_eq!(
        got,
        vec![Some(5), Some(600), Some(7200), Some(86401), None, None]
    );
}

#[test]
fn switch_checks_parked_login_first() {
    let e = Env::new();
    let (a, b) = parked_b_then_on_a(&e, None);
    let store = Store::new();
    fake_sessions("4321:7200");
    // 自动的切换（规则、等待中的切换）不替它换 token：不切，当前登录不动，也不让人去重新登录
    let msg = switch_account(&store, &b, false).err().unwrap();
    assert!(
        msg.starts_with("没有切换：“B”更新的一份登录还暂存着没核对"),
        "{}",
        msg
    );
    assert!(msg.contains("现在就切"), "{}", msg);
    assert!(msg.contains("先不用重新登录"), "{}", msg);
    assert_eq!(e.live().0["refreshToken"], "rt-A1");
    assert_eq!(e.data()["active"]["claude_code"], json!(a));
    assert!(token_log().is_empty());
    // 用户说现在就切（面板的“现在就切”）：核对暂存的那份并用上
    let r = api_switch(&store, &b, Some("now")).unwrap();
    fake_sessions("");
    assert_eq!(
        (r["ok"].clone(), r["changed"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(e.live().0["refreshToken"], "rt-B3");
    assert_eq!(e.secret_rt(&b), "rt-B3");
    assert!(pending_empty(&store));
    assert_eq!(token_log(), vec!["rt-B2"]);
}

#[test]
fn switch_network_error_while_checking_parked_login() {
    let e = Env::new();
    let (_a, b) = parked_b_then_on_a(&e, None);
    let store = Store::new();
    std::env::set_var("CAM_TOKEN_URLS", "http://127.0.0.1:1/v1/oauth/token");
    let r = switch_account(&store, &b, true);
    std::env::set_var("CAM_TOKEN_URLS", format!("{}/v1/oauth/token", mock_base()));
    let msg = r.err().unwrap();
    assert!(msg.contains("续登录失败"), "{}", msg); // 网络问题照网络问题说
    assert!(msg.contains("当前登录没有动"), "{}", msg);
    assert!(!msg.contains("重新登录"), "{}", msg);
    assert_eq!(e.live().0["refreshToken"], "rt-A1");
    assert_eq!(
        store.load_secret(PENDING_ID)["claudeAiOauth"]["refreshToken"],
        "rt-B2"
    );
    // 替账号续登录时也一样：网络问题不归成“暂存着”
    let held = pending_held_error("续登录失败（网络错误：x），稍后再试", true);
    let acc = json!({"kind": "claude_code", "usage": {"error": held}});
    assert_eq!(health_of(&acc).2, vec!["network"]);
    let held = pending_held_error("sessions", true);
    let acc = json!({"kind": "claude_code", "usage": {"error": held}});
    assert_eq!(health_of(&acc).2, vec!["pending_login"]);
}

#[test]
fn parked_login_guessed_for_another_account() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    switch_account(&store, "A", false).unwrap();
    // 切到 A 之前就开着的会话续了 B 的登录、写回凭据文件；~/.claude.json 里还是 A，这里运行时又核对不了：暂存，猜的是 A
    e.write_creds("at-B2", "rt-B2", past());
    assert!(import_current(&store, None).is_err());
    assert_eq!(store.load_secret(PENDING_ID)["candidate"], json!(a));
    set_refresh(&[("rt-B2", "at-B3", "rt-B3")]);
    set_tokens(&[("at-A1", "a@x.com"), ("at-B3", "b@x.com")]);
    clear_calls();
    // 刷新：B 保存的那份被拒绝了，但暂存的那份就是 Claude Code 现在用的登录，不能替它换：不下“失效”的结论
    refresh_usage(&store, None, true).unwrap();
    let acc = acc_of(&e, &b);
    let err = acc["usage"]["error"].as_str().unwrap();
    assert!(err.contains("保存的那份登录被服务端拒绝了"), "{}", err);
    assert!(err.contains(PENDING_HELD), "{}", err);
    assert_eq!(health_of(&acc).0, "info");
    assert!(!token_log().contains(&"rt-B2".to_string()));
    // 切到 B：保存的 access token 本地看没过期，但服务端不认；照着切过去 Claude Code 一续就要重新登录，所以不切
    clear_calls();
    let msg = switch_account(&store, &b, true).err().unwrap();
    assert!(
        msg.contains("没有切换：“B”保存的那份登录被服务端拒绝了"),
        "{}",
        msg
    );
    assert!(msg.contains("在 claude 里发一句话"), "{}", msg);
    assert_eq!(e.live().0["refreshToken"], "rt-B2");
    assert!(!token_log().contains(&"rt-B2".to_string())); // 当前登录从不替它续
                                                          // Claude Code 换成了别的登录（重新登录了 A）：暂存的那份不再是当前登录，再切 B 时核对出是 B 的，用上
    e.login("at-A9", "rt-A9", "uuid-A", "a@x.com", later());
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-A9".into(), "a@x.com".into());
    clear_calls();
    assert!(switch_account(&store, &b, true).unwrap().changed);
    assert_eq!(e.live().0["refreshToken"], "rt-B3");
    assert_eq!(e.secret_rt(&b), "rt-B3");
    assert_eq!(e.secret_rt(&a), "rt-A9");
    assert!(pending_empty(&store));
}

#[test]
fn parked_login_the_server_rejects_is_kept_for_its_refresh_token() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A", false).unwrap();
    switch_account(&store, "B", false).unwrap();
    // 当前登录是没见过的 token、~/.claude.json 还写着 B、服务端 503：不能把它存进 B 名下，要暂存并提示核对不了
    let far = later();
    e.write_creds("at-X", "rt-X", far);
    mock().lock().unwrap().profile_code = 503;
    assert!(import_current(&store, None)
        .unwrap_err()
        .contains("无法确认"));
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert!(e.data().get("live_unverified").is_some());
    let pend = store.load_secret(PENDING_ID);
    assert_eq!(pend["claudeAiOauth"]["refreshToken"], "rt-X");
    assert!(pend.get("released_at").is_none()); // 它还是当前登录
                                                // Claude Code 换回了 A 保存的那份登录；暂存的 token 核对时限流：留着下次再问
    e.login("at-A1", "rt-A1", "uuid-A", "a@x.com", later());
    mock().lock().unwrap().profile_code = 429;
    import_current(&store, None).unwrap();
    let pend = store.load_secret(PENDING_ID);
    assert_eq!(pend["claudeAiOauth"]["expiresAt"], json!(far));
    assert!(pend["released_at"].as_str().is_some_and(|t| !t.is_empty())); // 核对时它已经不是当前登录了
                                                                          // 401：access token 不认了，refresh token 也许还能用。记成已过期留着（切换、手动刷新时换一个再核对），不再拿它白问
    mock().lock().unwrap().profile_code = 0;
    clear_calls();
    import_current(&store, None).unwrap();
    import_current(&store, None).unwrap();
    assert_eq!(profile_calls(), 1);
    let pend = store.load_secret(PENDING_ID);
    assert_eq!(pend["claudeAiOauth"]["refreshToken"], "rt-X");
    assert_eq!(pend["claudeAiOauth"]["expiresAt"], json!(0));
    assert_eq!(pend["raw"]["claudeAiOauth"]["expiresAt"], json!(0));
    assert_eq!(e.secret_rt(&a), "rt-A1");
    assert_eq!(e.secret_rt(&b), "rt-B1");
    // 没有 refresh token 的，401 就删：留着也换不出新的
    let mut pend = pend;
    pend["claudeAiOauth"] = json!({"accessToken": "at-Y", "expiresAt": far});
    store.save_secret(PENDING_ID, &pend).unwrap();
    import_current(&store, None).unwrap();
    assert!(pending_empty(&store));
}

#[test]
fn rejected_access_token_is_renewed_once() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later()); // B 是当前登录；A 保存的 access token 本地看没过期
    let store = Store::new();
    set_tokens(&[("at-B1", "b@x.com"), ("at-A2", "a@x.com")]); // 服务端不认 at-A1 了
    set_refresh(&[("rt-A1", "at-A2", "rt-A2")]);
    refresh_usage(&store, None, true).unwrap();
    assert_eq!(acc_of(&e, &a)["usage"]["error"], Value::Null);
    assert_eq!(e.secret_rt(&a), "rt-A2");
    assert_eq!(token_log(), vec!["rt-A1"]);
    // 当前登录被拒绝：从不替它续，原样报
    set_tokens(&[("at-A2", "a@x.com")]);
    set_refresh(&[("rt-B1", "at-B2", "rt-B2")]);
    clear_calls();
    refresh_usage(&store, None, true).unwrap();
    assert!(usage_error(&e, &b).contains("401"));
    assert!(token_log().is_empty());
    // 后台那轮（不许换 token）也不续
    set_tokens(&[("at-B1", "b@x.com")]);
    set_refresh(&[("rt-A2", "at-A3", "rt-A3")]);
    clear_calls();
    refresh_usage(&store, None, false).unwrap();
    assert!(usage_error(&e, &a).contains("401"));
    assert!(token_log().is_empty());
}

#[test]
fn token_endpoint_answers_that_do_not_mean_dead() {
    let dead = token_answer_dead;
    assert!(dead(
        400,
        &json!({"error": "invalid_grant", "error_description": "Refresh token not found or invalid"})
    ));
    assert!(dead(
        400,
        &json!({"error": {"type": "invalid_request_error", "message": "invalid_grant"}})
    ));
    assert!(dead(401, &json!({"error": {"message": "revoked"}})));
    // 代理、网关回的
    assert!(!dead(
        400,
        &json!({"error": {"message": "<html>Bad Request</html>"}, "_not_json": true})
    ));
    assert!(!dead(400, &json!({"error": "invalid_client"}))); // 请求本身不对，和这个账号的登录无关
    assert!(!dead(
        400,
        &json!({"error": {"type": "invalid_request_error", "message": "unsupported_grant_type"}})
    ));
    assert!(!dead(403, &json!({"error": {"message": "forbidden"}})));
    assert!(!dead(400, &json!(["x"])));
    assert!(!dead(500, &json!({"error": "invalid_grant"})));
    // 出错时的响应体不是 JSON 对象的都带上 _not_json
    for text in ["<html>Bad Request</html>", "[\"x\"]", ""] {
        let body = error_body(text);
        assert_eq!(body["_not_json"], json!(true), "{}", body);
        assert!(!dead(400, &body));
    }
    assert_eq!(
        error_body("{\"error\": \"invalid_grant\"}"),
        json!({"error": "invalid_grant"})
    );
    // 真的走一遍：token 接口答的不是“这个 refresh token 不能用了”时，报续登录失败（归成网络问题），不说登录失效
    let e = Env::new();
    let (a, _b) = e.two_accounts(past());
    set_tokens(&[("at-B1", "b@x.com")]);
    let store = Store::new();
    for body in ["<html>blocked</html>", "{\"error\": \"invalid_client\"}"] {
        mock().lock().unwrap().token_error = body.into();
        refresh_usage(&store, None, true).unwrap();
        let acc = acc_of(&e, &a);
        let err = acc["usage"]["error"].as_str().unwrap();
        assert!(err.contains("续登录失败"), "{}", err);
        assert_eq!(health_of(&acc).2, vec!["network"], "{}", err);
        assert_eq!(incident_count(&acc), 0);
    }
    mock().lock().unwrap().token_error = "{\"error\": \"invalid_grant\"}".into();
    refresh_usage(&store, None, true).unwrap();
    let acc = acc_of(&e, &a);
    assert!(usage_error(&e, &a).contains("登录已失效"));
    assert_eq!(health_of(&acc).2, vec!["login_invalid"]);
}

#[test]
fn login_invalid_survives_background_round() {
    let e = Env::new();
    let (a, _b) = e.two_accounts(past()); // 假接口不认 rt-A1：A 的登录确实作废了
    set_tokens(&[("at-B1", "b@x.com")]);
    let store = Store::new();
    refresh_usage(&store, None, true).unwrap();
    let acc = acc_of(&e, &a);
    assert_eq!(health_of(&acc).0, "error");
    assert!(acc["token_fp"].as_str().is_some_and(|f| !f.is_empty()));
    assert_eq!(acc["usage"]["error_fp"], acc["token_fp"]);
    assert_eq!(incident_count(&acc), 1);
    // 后台那轮不替它续，只会得到“登录过期了”：结论还是失效，现场也不重复记
    refresh_usage(&store, None, false).unwrap();
    let acc = acc_of(&e, &a);
    assert!(usage_error(&e, &a).contains("登录已失效"));
    assert_eq!(health_of(&acc).0, "error");
    assert_eq!(incident_count(&acc), 1);
    // 重新登录 A 并保存后（token 换了）按新的结果来
    e.login("at-A5", "rt-A5", "uuid-A", "a@x.com", later());
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-A5".into(), "a@x.com".into());
    refresh_usage(&store, None, false).unwrap();
    let acc = acc_of(&e, &a);
    assert_eq!(acc["usage"]["error"], Value::Null);
    assert!(acc["usage"].get("error_fp").is_none());
    // 没查出过失效的号，后台那轮的“登录过期了”还是提示，不是异常
    let stale = "登录已过期。后台不会替它续登录（避免和 Claude Code 抢 refresh token），手动刷新一次或切换到这个账号即可";
    let mut u = json!({"error": stale});
    let acc = json!({"kind": "claude_code", "token_fp": "f1",
                     "usage": {"error": "登录已失效（x）", "error_fp": "f0"}});
    keep_login_invalid(&acc, &mut u);
    assert_eq!(u["error"], json!(stale)); // 之后保存的登录换过：不沿用
    assert!(u.get("error_fp").is_none());
    // 保存的登录没换过：结论还是失效
    let mut u = json!({"error": stale});
    let acc = json!({"kind": "claude_code", "token_fp": "f1",
                     "usage": {"error": "登录已失效（x）", "error_fp": "f1"}});
    keep_login_invalid(&acc, &mut u);
    assert_eq!(u["error"], json!("登录已失效（x）"));
    assert_eq!(u["error_fp"], json!("f1"));
}

#[test]
fn unknown_process_list_defers_automatic_switch() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    assert!(!claude_may_be_running());
    fake_sessions("1");
    assert!(claude_may_be_running());
    fake_sessions("unknown");
    assert!(claude_may_be_running());
    // 登记了等会话退出后切到 A
    fake_sessions("1");
    route(
        &store,
        "POST",
        &format!("/api/accounts/{a}/switch"),
        &json!({"mode": "wait"}),
    );
    fake_sessions("unknown");
    assert!(apply_pending_switch(&store).unwrap().is_none()); // 没查出来：当作有会话，先不切
    assert_eq!(e.data()["active"]["claude_code"], json!(b));
    assert!(e.data().get("pending_switch").is_some());
    fake_sessions("");
    let (r, _) = apply_pending_switch(&store).unwrap().unwrap();
    assert_eq!(r.id, a);
    // 时间规则也一样：没查出来先登记，不直接切
    save_rules(
        &store,
        &json!({"rules": [{"type": "time", "days": "1-5", "start": "09:00", "end": "18:00", "account": b}]}),
    )
    .unwrap();
    let mon10 = chrono::NaiveDate::from_ymd_opt(2026, 10, 19)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap(); // 周一
    fake_sessions("unknown");
    assert!(apply_time_rules_at(&store, mon10).unwrap().is_none());
    assert_eq!(e.data()["pending_switch"]["account"], json!(b));
    assert_eq!(e.data()["active"]["claude_code"], json!(a));
    // 面板上的会话数：这次没查出来就沿用上次的，不归零
    let count = || public_state(&store, &e.data())["sessions"]["count"].clone();
    fake_sessions("1,2");
    assert_eq!(count(), json!(2));
    fake_sessions("unknown");
    assert_eq!(count(), json!(2));
    fake_sessions("");
    assert_eq!(count(), json!(0));
}

#[test]
fn usage_time_is_when_the_numbers_are_from() {
    // 用量没取到时（断网、限流、登录过期而后台不替它续）旧数字留着，“用量更新”的时间也还是旧数字的时间，不写成刚刚；
    // 另记 checked_at（什么时候查的）
    let e = Env::new();
    let (a, _b) = e.two_accounts(later());
    set_tokens(&[("at-A1", "a@x.com"), ("at-B1", "b@x.com")]);
    let store = Store::new();
    refresh_usage(&store, None, true).unwrap();
    let u = acc_of(&e, &a)["usage"].clone();
    assert_eq!(u["error"], Value::Null);
    assert!(u["fetched_at"].is_string());
    assert_eq!(u["fetched_at"], u["checked_at"]);
    let old = "2026-01-01T00:00:00+00:00";
    {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        for acc in d["accounts"].as_array_mut().unwrap() {
            acc["usage"]["fetched_at"] = json!(old);
            acc["usage"]["checked_at"] = json!(old);
        }
        store.save(&mut d).unwrap();
    }
    let at = |v: &Value| parse_time(v.as_str().unwrap()).unwrap();
    mock().lock().unwrap().usage_code = 429;
    refresh_usage(&store, None, true).unwrap();
    let u = acc_of(&e, &a)["usage"].clone();
    assert!(u["error"].as_str().unwrap().contains("429"), "{}", u);
    // 数字和它的时间都是上次那份
    assert_eq!(u["five_hour"]["utilization"], 12.0);
    assert_eq!(u["fetched_at"], json!(old));
    assert!(at(&u["checked_at"]) > at(&json!(old)));
    mock().lock().unwrap().usage_code = 0;
    refresh_usage(&store, None, true).unwrap();
    let u = acc_of(&e, &a)["usage"].clone();
    assert_eq!(u["error"], Value::Null);
    assert_eq!(u["fetched_at"], u["checked_at"]);
    assert!(at(&u["fetched_at"]) > at(&json!(old)));
}

#[test]
fn stale_environment_results_are_labelled() {
    // 后台没在跑时环境检测结果会很旧：从那次结果算出来的提示标上是多久以前测的，旧的官方故障不再提，并说怎么重新检测
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    // sp_at：状态页的检测时间。"same" 和出口那次一样，None 是旧版数据（没单独记）
    let put = |checked_at: &str, sp_at: Option<&str>| -> Value {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        d["region"] = json!({"loc": "US", "ip": "1.2.3.4", "org": "Amazon.com, Inc.", "asn": 16509, "ip_type": "hosting",
            "skew": 300, "warp": false, "checked_at": checked_at,
            "env": {"tz": "America/New_York", "tz_cc": ["US"], "locale": null, "locale_cc": null, "dns": [],
                    "proxy": ["HTTPS_PROXY=http://127.0.0.1:7890"], "tunnel": null}});
        d["status_page"] = json!({"indicator": "major", "description": "Partial System Outage"});
        if let Some(t) = sp_at {
            d["status_page"]["checked_at"] = json!(if t == "same" { checked_at } else { t });
        }
        store.save(&mut d).unwrap();
        d
    };
    let labelled = |items: &[Value]| {
        items
            .iter()
            .filter(|r| r["text"].as_str().unwrap().contains("检测的）"))
            .count()
    };
    // 刚测的：原样，官方故障照常提
    let fresh = risk_items(&put(&ago(0), Some("same")), true, true);
    assert_eq!(
        kinds(&fresh),
        vec!["proxy", "isp", "clock", "status", "accounts"]
    );
    assert_eq!(labelled(&fresh), 0);
    assert_eq!(risk_score(&fresh)["score"], 100 - 5 - 20 - 10 - 30);
    // 两天前测的：每条标出来，两天前的故障不算，多一条 env_stale；分数只少了故障那一项
    let d = put(&ago(2 * 86400), Some("same"));
    let old = risk_items(&d, true, true);
    assert_eq!(
        kinds(&old),
        vec!["proxy", "isp", "clock", "accounts", "env_stale"]
    );
    for k in ["proxy", "isp", "clock"] {
        let text = by_kind(&old, k).unwrap()["text"].as_str().unwrap();
        assert!(text.ends_with("（2 天前检测的）"), "{}", text);
    }
    assert_eq!(labelled(&old), 3); // 账号数、env_stale 不是那次检测的结果，不标
    let stale = by_kind(&old, "env_stale").unwrap();
    assert_eq!(stale["level"], "info");
    assert_eq!(
        stale["text"],
        "环境检测结果是 2 天前的，出口、代理、时区这些可能已经变了"
    );
    assert!(stale["advice"].as_str().unwrap().contains("cam doctor"));
    assert_eq!(risk_score(&old)["score"], 100 - 5 - 20 - 10);
    // 调用方自己说了是多久前的：不标、不加 env_stale，旧故障同样不提
    let plain = risk_items(&d, true, false);
    assert_eq!(kinds(&plain), vec!["proxy", "isp", "clock", "accounts"]);
    assert_eq!(labelled(&plain), 0);
    // 旧版的状态页没单独记时间：按出口那次检测的时间算
    let has_status = |d: &Value| kinds(&risk_items(d, true, true)).contains(&"status".to_string());
    assert!(has_status(&put(&ago(0), None)));
    assert!(!has_status(&put(&ago(3 * 3600), None)));
    // 出口刚测过、状态页是更早那次的（不该出现，出现了也不提旧故障）
    assert!(!has_status(&put(&ago(0), Some(&ago(2 * 3600)))));
    // 一小时以内不算旧
    let recent = risk_items(&put(&ago(50 * 60), Some("same")), true, true);
    assert!(!kinds(&recent).contains(&"env_stale".to_string()));
    assert_eq!(labelled(&recent), 0);
    // 面板上的风险提示（/api/state）带着标注；实测一遍之后就没有了，状态页和出口记的是同一个检测时间
    let st = public_state(&store, &put(&ago(2 * 86400), Some("same")));
    assert!(kinds(st["risks"].as_array().unwrap()).contains(&"env_stale".to_string()));
    mock().lock().unwrap().status_indicator = "major".into();
    check_region(&store, 0).unwrap();
    let d = e.data();
    assert_eq!(d["status_page"]["checked_at"], d["region"]["checked_at"]);
    let now = risk_items(&d, true, true);
    assert!(kinds(&now).contains(&"status".to_string()));
    assert!(!kinds(&now).contains(&"env_stale".to_string()));
    assert_eq!(labelled(&now), 0);
    // 状态页这次没连上：留着上次的（它自己带时间，过一小时不再算数），出口照常更新
    let before = d["status_page"].clone();
    {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        d["region"]["checked_at"] = json!(ago(600));
        store.save(&mut d).unwrap();
    }
    std::env::set_var("CAM_STATUS_URL", "http://127.0.0.1:1/status.json");
    let r = check_region(&store, 0);
    std::env::set_var("CAM_STATUS_URL", format!("{}/status.json", mock_base()));
    r.unwrap();
    let d = e.data();
    assert_eq!(d["status_page"], before);
    let checked = parse_time(d["region"]["checked_at"].as_str().unwrap()).unwrap();
    assert!(checked > parse_time(&ago(300)).unwrap());
    assert!(d["region"].get("failed_at").is_none());
}

fn fake_claude(running: bool) {
    std::env::set_var("CAM_TEST_CLAUDE_PIDS", if running { "4242" } else { "" });
}

#[test]
fn api_switch_waits_for_sessions_then_background_applies() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    fake_claude(true);
    let (code, r) = route(
        &store,
        "POST",
        &format!("/api/accounts/{a}/switch"),
        &json!({}),
    );
    assert_eq!(code, 200);
    assert_eq!(r["choice_required"], json!(true));
    assert!(e.data().get("pending_switch").is_none());
    let (_, r) = route(
        &store,
        "POST",
        &format!("/api/accounts/{a}/switch"),
        &json!({"mode": "wait"}),
    );
    assert_eq!(r["deferred"], json!(true));
    let st = public_state(&store, &e.data());
    assert_eq!(st["pending_switch"]["account"], json!(a));
    assert!(st["pending_switch"]["message"]
        .as_str()
        .unwrap()
        .contains("“A”"));
    assert!(apply_pending_switch(&store).unwrap().is_none()); // 会话还在，不切
    assert_eq!(e.data()["active"]["claude_code"], json!(b));
    fake_claude(false);
    let (r, reason) = apply_pending_switch(&store).unwrap().unwrap();
    assert!(r.changed && r.id == a && reason == "manual");
    let d = e.data();
    assert_eq!(d["active"]["claude_code"], json!(a));
    assert!(d.get("pending_switch").is_none());
    assert!(public_state(&store, &d)["pending_switch"].is_null());
    // mode=now 直接切；DELETE 取消登记；手动切换清掉登记
    fake_claude(true);
    let (_, r) = route(
        &store,
        "POST",
        &format!("/api/accounts/{b}/switch"),
        &json!({"mode": "now"}),
    );
    assert_eq!(r["changed"], json!(true));
    route(
        &store,
        "POST",
        &format!("/api/accounts/{a}/switch"),
        &json!({"mode": "wait"}),
    );
    let (code, st) = route(&store, "DELETE", "/api/pending-switch", &json!({}));
    assert_eq!(code, 200);
    assert!(st["pending_switch"].is_null());
    route(
        &store,
        "POST",
        &format!("/api/accounts/{a}/switch"),
        &json!({"mode": "wait"}),
    );
    switch_account(&store, "A", false).unwrap();
    assert!(e.data().get("pending_switch").is_none());
    fake_claude(false);
}

#[test]
fn time_rule_waits_for_session_and_expires_with_window() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    save_rules(
        &store,
        &json!({"rules": [{"type": "time", "days": "1-5", "start": "09:00", "end": "18:00", "account": a}]}),
    )
    .unwrap();
    let mon10 = chrono::NaiveDate::from_ymd_opt(2026, 10, 19)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap(); // 周一
    fake_claude(true);
    assert!(apply_time_rules_at(&store, mon10).unwrap().is_none()); // 有会话在跑：先登记
    let d = e.data();
    assert_eq!(d["pending_switch"]["account"], json!(a));
    assert_eq!(d["pending_switch"]["reason"], json!("rule"));
    assert_eq!(d["active"]["claude_code"], json!(b));
    assert!(
        apply_time_rules_at(&store, mon10 + chrono::Duration::hours(1))
            .unwrap()
            .is_none()
    ); // 同一时间段不重复
    assert!(
        apply_pending_switch_at(&store, mon10 + chrono::Duration::hours(1))
            .unwrap()
            .is_none()
    ); // 会话还在
    fake_claude(false);
    // 时间段过了还没切成：作废
    assert!(
        apply_pending_switch_at(&store, mon10 + chrono::Duration::hours(9))
            .unwrap()
            .is_none()
    );
    let d = e.data();
    assert!(d.get("pending_switch").is_none());
    assert_eq!(d["active"]["claude_code"], json!(b));
    // 第二天再进入时间段：会话退出后切过去
    apply_time_rules_at(&store, mon10 + chrono::Duration::hours(10)).unwrap(); // 离开时间段
    fake_claude(true);
    let tue10 = mon10 + chrono::Duration::days(1);
    assert!(apply_time_rules_at(&store, tue10).unwrap().is_none());
    assert!(e.data().get("pending_switch").is_some());
    fake_claude(false);
    let (r, reason) = apply_pending_switch_at(&store, tue10).unwrap().unwrap();
    assert!(r.id == a && reason == "rule");
    assert_eq!(e.data()["active"]["claude_code"], json!(a));
}

#[test]
fn manual_pending_switch_expires() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    let wait_since = |hours: i64| {
        route(
            &store,
            "POST",
            &format!("/api/accounts/{a}/switch"),
            &json!({"mode": "wait"}),
        );
        let mut d = e.data();
        d["pending_switch"]["at"] =
            json!((chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339());
        store.save(&mut d).unwrap();
    };
    fake_claude(true);
    // 登记放了 7 小时：不再显示；会话还在也马上清掉，之后不会再切
    wait_since(7);
    assert!(public_state(&store, &e.data())["pending_switch"].is_null());
    assert!(apply_pending_switch(&store).unwrap().is_none());
    let d = e.data();
    assert!(d.get("pending_switch").is_none());
    assert_eq!(d["active"]["claude_code"], json!(b));
    // 5 小时的照常等，会话退出后切过去
    wait_since(5);
    assert_eq!(
        public_state(&store, &e.data())["pending_switch"]["account"],
        json!(a)
    );
    assert!(apply_pending_switch(&store).unwrap().is_none());
    fake_claude(false);
    let (r, reason) = apply_pending_switch(&store).unwrap().unwrap();
    assert!(r.id == a && reason == "manual");
}

// ---------------------------------------------------------------- 切换前核对目标账号的登录

#[test]
fn switch_renews_expired_target_with_its_own_refresh_token() {
    let e = Env::new();
    let (a, _) = e.two_accounts(past()); // A 保存的 access token 已过期，当前是 B
    let store = Store::new();
    mock()
        .lock()
        .unwrap()
        .refresh
        .insert("rt-A1".into(), ("at-A2".into(), "rt-A2".into()));
    assert!(switch_account(&store, "A", false).unwrap().changed);
    let (live, cj) = e.live();
    assert_eq!(live["accessToken"], "at-A2");
    assert_eq!(live["refreshToken"], "rt-A2");
    assert_eq!(cj["oauthAccount"]["emailAddress"], "a@x.com");
    assert_eq!(e.secret_rt(&a), "rt-A2");
    assert_eq!(token_calls("rt-A1"), 1);
    assert_eq!(token_calls("rt-B1"), 0); // 当前登录从不替它续
}

#[test]
fn switch_to_dead_login_leaves_live_login_untouched() {
    let e = Env::new();
    let (a, b) = e.two_accounts(past()); // 假接口不认 rt-A1：A 的 refresh token 已作废
    let store = Store::new();
    let live_files = || {
        (
            std::fs::read(e.creds()).unwrap(),
            std::fs::read(e.cj()).unwrap(),
        )
    };
    let before = live_files();
    // 有会话在跑时登记了等它切到 A；会话退出后切不过去，登记作废，不会一直重试
    fake_claude(true);
    route(
        &store,
        "POST",
        &format!("/api/accounts/{a}/switch"),
        &json!({"mode": "wait"}),
    );
    fake_claude(false);
    let err = apply_pending_switch(&store).err().unwrap();
    assert!(
        err.contains("已失效") && err.contains("当前登录没有动"),
        "{}",
        err
    );
    assert!(e.data().get("pending_switch").is_none());
    let err = switch_account(&store, "A", false).err().unwrap();
    assert!(err.contains("重新登录"), "{}", err);
    assert_eq!(live_files(), before);
    assert_eq!(e.data()["active"]["claude_code"], json!(b));
    assert_eq!(
        std::fs::read_dir(store.backups_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
    // 连不上 token 接口：同样不切
    std::env::set_var("CAM_TOKEN_URLS", "http://127.0.0.1:1/v1/oauth/token");
    let r = switch_account(&store, "A", false);
    std::env::set_var("CAM_TOKEN_URLS", format!("{}/v1/oauth/token", mock_base()));
    let err = r.err().unwrap();
    assert!(
        err.contains("续登录失败") && err.contains("当前登录没有动"),
        "{}",
        err
    );
    assert_eq!(live_files(), before);
}

#[test]
fn switch_replaces_only_the_claude_login_in_credentials() {
    let e = Env::new();
    let store = Store::new();
    let add = |extra: Value| {
        let mut c = read_json(&e.creds()).unwrap().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        std::fs::write(e.creds(), c.to_string()).unwrap();
    };
    // A 导入时凭据里有一份 MCP 服务器的登录；后来（B 在用时）MCP 重新登录过，还多了别的字段
    e.login("at-A1", "rt-A1", "uuid-A", "a@x.com", later());
    add(json!({"mcpOAuth": {"srv": {"accessToken": "mcp-old"}}}));
    import_current(&store, Some("A")).unwrap();
    e.login("at-B1", "rt-B1", "uuid-B", "b@x.com", later());
    import_current(&store, Some("B")).unwrap();
    add(json!({"mcpOAuth": {"srv": {"accessToken": "mcp-new"}}, "otherKey": 1}));
    switch_account(&store, "A", false).unwrap();
    let c = read_json(&e.creds()).unwrap().unwrap();
    assert_eq!(c["claudeAiOauth"]["refreshToken"], "rt-A1");
    assert_eq!(c["mcpOAuth"], json!({"srv": {"accessToken": "mcp-new"}})); // 不退回 A 当初那份
    assert_eq!(c["otherKey"], json!(1));
    switch_account(&store, "B", false).unwrap();
    let c = read_json(&e.creds()).unwrap().unwrap();
    assert_eq!(c["claudeAiOauth"]["refreshToken"], "rt-B1");
    assert_eq!(c["mcpOAuth"], json!({"srv": {"accessToken": "mcp-new"}}));
    assert_eq!(c["otherKey"], json!(1));
}

#[test]
fn service_check_reports_region_and_services() {
    let e = Env::new();
    e.two_accounts(later()); // 当前登录 B
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-B1".into(), "b@x.com".into());
    let items = service_check(&Store::new());
    let text = items
        .iter()
        .map(|i| i["text"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(items[0]["level"], "ok", "{}", text);
    assert!(text.contains("Claude 服务正常"), "{}", text);
    assert!(
        text.contains("出口地区 US（美国） 在 Anthropic 支持名单内"),
        "{}",
        text
    );
    assert!(text.contains("服务正常，当前登录可用"), "{}", text);
    assert!(text.contains("可达"), "{}", text);
    assert!(items.iter().all(|i| i.get("advice").is_some()));
    assert_eq!(
        (items[0]["score"].as_i64(), items[0]["grade"].as_str()),
        (Some(100), Some("low"))
    );
    assert!(
        items[0]["text"]
            .as_str()
            .unwrap()
            .contains("100/100，风险低，可以使用"),
        "{}",
        text
    );
    // 不在支持名单内：提醒并给建议，总体结论跟着变
    // （不在名单 60 + 出口从 US 变到 CN 15 + 当前账号两地使用 20 + 时区指向美国 5 → 0 分，风险高）
    mock().lock().unwrap().trace_loc = "CN".into();
    let items = service_check(&Store::new());
    assert_eq!(items[0]["level"], "bad");
    assert_eq!(
        (items[0]["score"].as_i64(), items[0]["grade"].as_str()),
        (Some(0), Some("high"))
    );
    assert!(
        items[0]["text"]
            .as_str()
            .unwrap()
            .contains("0/100，风险高，不建议在当前环境使用"),
        "{}",
        items[0]
    );
    assert!(items[0]["text"].as_str().unwrap().contains("出口地区 CN"));
    let reg = items
        .iter()
        .find(|i| i["text"].as_str().unwrap().starts_with("出口地区"))
        .unwrap();
    assert_eq!(reg["level"], "warn");
    assert!(reg["text"]
        .as_str()
        .unwrap()
        .contains("CN（中国大陆） 不在 Anthropic 支持名单内"));
    assert!(reg["advice"].as_str().unwrap().contains("封禁"));
    // 属地按所属国家看待
    mock().lock().unwrap().trace_loc = "PR".into();
    let items = service_check(&Store::new());
    let reg = items
        .iter()
        .find(|i| i["text"].as_str().unwrap().starts_with("出口地区"))
        .unwrap();
    assert_eq!(reg["level"], "ok");
    mock().lock().unwrap().trace_loc = String::new();
}

fn kinds(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|i| i["kind"].as_str().unwrap_or("").to_string())
        .collect()
}

fn by_kind<'a>(items: &'a [Value], kind: &str) -> Option<&'a Value> {
    items.iter().find(|i| i["kind"] == kind)
}

#[test]
fn risk_items_and_reminders() {
    let e = Env::new();
    let (a, _b) = e.two_accounts(later()); // 当前登录 B
    let store = Store::new();
    check_region(&store, 300).unwrap();
    // 没有风险时只有“账号数”这条 info
    let st = public_state(&store, &e.data());
    let risks = st["risks"].as_array().cloned().unwrap();
    assert_eq!(kinds(&risks), vec!["accounts"]);
    assert_eq!(
        (
            st["risk_score"]["score"].as_i64(),
            st["risk_score"]["grade"].as_str()
        ),
        (Some(100), Some("low"))
    );
    assert!(risks
        .iter()
        .all(|r| r.as_object().unwrap().len() == 4 && r.get("advice").is_some()));
    // 出口从 US 变到 HK：不在名单 + 出口变化；当前账号 B 记了两个地区
    mock().lock().unwrap().trace_loc = "HK".into();
    check_region(&store, 0).unwrap();
    let d = e.data();
    let items = risk_items(&d, true, true);
    let reg = by_kind(&items, "region").unwrap();
    assert_eq!(reg["level"], "warn");
    assert!(reg["text"]
        .as_str()
        .unwrap()
        .contains("HK（香港） 不在 Anthropic 支持名单内"));
    let ch = by_kind(&items, "region_change").unwrap();
    assert_eq!(ch["level"], "warn");
    assert!(
        ch["text"]
            .as_str()
            .unwrap()
            .contains("US（美国） → HK（香港）"),
        "{}",
        ch
    );
    assert!(ch["text"].as_str().unwrap().contains("刚刚"), "{}", ch);
    let ar = by_kind(&items, "account_regions").unwrap();
    assert!(
        ar["text"]
            .as_str()
            .unwrap()
            .contains("2 个地区使用过：HK（香港）、US（美国）"),
        "{}",
        ar
    );
    assert_eq!(d["region"]["prev_loc"], "US");
    let sc = public_state(&store, &d)["risk_score"].clone(); // 不在名单 60 + 出口变化 15 + 多地区 20 + 时区指向美国 5 → 0 分
    assert_eq!(
        (sc["score"].as_i64(), sc["grade"].as_str()),
        (Some(0), Some("high"))
    );
    assert!(sc["label"].as_str().unwrap().contains("不建议"));
    // 变化提示在 24 小时内保留，再次检测到同一地区不会丢
    check_region(&store, 0).unwrap();
    assert_eq!(e.data()["region"]["prev_loc"], "US");
    // 时区指向不支持地区而出口在支持地区：warn；指向支持地区：info；一致：不提
    mock().lock().unwrap().trace_loc = "SG".into();
    check_region(&store, 0).unwrap();
    // 本机设置随出口一起缓存，改了环境变量要重新检测一次
    let risks_with = |k: &str, v: &str| {
        std::env::set_var(k, v);
        check_region(&store, 0).unwrap();
        risk_items(&e.data(), true, true)
    };
    let items = risks_with("CAM_TEST_TZ", "Asia/Hong_Kong=HK");
    let tz = by_kind(&items, "timezone").unwrap();
    assert_eq!(tz["level"], "warn");
    assert!(
        tz["text"]
            .as_str()
            .unwrap()
            .contains("Asia/Hong_Kong 指向 HK（香港），与出口地区 SG（新加坡） 不一致"),
        "{}",
        tz
    );
    assert_eq!(
        by_kind(&risks_with("CAM_TEST_TZ", "Asia/Tokyo=JP"), "timezone").unwrap()["level"],
        "info"
    );
    assert!(by_kind(
        &risks_with("CAM_TEST_TZ", "Asia/Singapore=SG,MY"),
        "timezone"
    )
    .is_none());
    std::env::set_var("CAM_TEST_TZ", "America/New_York=US");
    // 系统语言指向不支持地区：warn；指向支持地区：info
    let lc = by_kind(&risks_with("CAM_TEST_LOCALE", "zh_CN.UTF-8"), "locale")
        .cloned()
        .unwrap();
    assert_eq!(lc["level"], "info"); // 语言只代表语言习惯，只作参考
    assert_eq!(risk_score(std::slice::from_ref(&lc))["score"], 95);
    assert!(
        lc["text"]
            .as_str()
            .unwrap()
            .contains("zh_CN 指向 CN（中国大陆），与出口地区 SG（新加坡） 不一致"),
        "{}",
        lc
    );
    assert_eq!(
        by_kind(&risks_with("CAM_TEST_LOCALE", "ja-JP"), "locale").unwrap()["level"],
        "info"
    );
    std::env::set_var("CAM_TEST_LOCALE", "en_US");
    // DNS 解析器：局域网和任播的不提；不支持地区的公共 DNS 与出口不一致 → warn；支持地区 → info
    assert!(by_kind(
        &risks_with("CAM_TEST_DNS", "192.168.1.1,8.8.8.8"),
        "dns_resolver"
    )
    .is_none());
    let d = by_kind(
        &risks_with("CAM_TEST_DNS", "192.168.1.1,223.5.5.5"),
        "dns_resolver",
    )
    .cloned()
    .unwrap();
    assert_eq!(d["level"], "warn");
    assert!(
        d["text"]
            .as_str()
            .unwrap()
            .contains("223.5.5.5（阿里 DNS，CN（中国大陆））与出口地区 SG（新加坡） 不一致"),
        "{}",
        d
    );
    assert_eq!(
        by_kind(&risks_with("CAM_TEST_DNS", "168.95.1.1"), "dns_resolver").unwrap()["level"],
        "info"
    );
    std::env::set_var("CAM_TEST_DNS", "192.168.1.1");
    check_region(&store, 0).unwrap();
    // IP 类型：数据库标记为代理 → warn；IPv6 出口和 IPv4 不同 → warn；注册国不同 → info
    {
        let mut m = mock().lock().unwrap();
        m.ipapi_type = "proxy".into();
        m.trace6_loc = "HK".into();
        m.rdap_country = "CA".into();
    }
    check_region(&store, 0).unwrap();
    let d = e.data();
    let items = risk_items(&d, true, true);
    let p = by_kind(&items, "proxy_ip").unwrap();
    assert_eq!(p["level"], "warn");
    assert!(
        p["text"]
            .as_str()
            .unwrap()
            .contains("被数据库标记为代理 / VPN 出口"),
        "{}",
        p
    );
    let v6 = by_kind(&items, "ipv6_mismatch").unwrap();
    assert_eq!(v6["level"], "warn");
    assert!(
        v6["text"]
            .as_str()
            .unwrap()
            .contains("IPv4 出口在 SG（新加坡），IPv6 出口在 HK（香港）（2001:db8::1）"),
        "{}",
        v6
    );
    let o = by_kind(&items, "ip_origin").unwrap();
    assert_eq!(o["level"], "info");
    assert!(
        o["text"]
            .as_str()
            .unwrap()
            .contains("注册在 CA（加拿大），定位在 SG（新加坡）（广播 IP）"),
        "{}",
        o
    );
    assert_eq!(d["region"]["ip_type"], "proxy");
    // 手机网络、IPv6 不可用、注册国一致：都不提；服务检查里各有一行
    {
        let mut m = mock().lock().unwrap();
        m.ipapi_type = "mobile".into();
        m.trace6_loc = String::new();
        m.rdap_country = "SG".into();
    }
    check_region(&store, 0).unwrap();
    let items = risk_items(&e.data(), true, true);
    assert!(["proxy_ip", "isp", "ipv6_mismatch", "ip_origin"]
        .iter()
        .all(|k| by_kind(&items, k).is_none()));
    let items = service_check(&store);
    assert!(by_kind(&items, "isp").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("IP 类型：手机网络"));
    assert!(by_kind(&items, "ipv6").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("IPv6：不可用"));
    assert_eq!(by_kind(&items, "ip_origin").unwrap()["level"], "ok");
    assert!(by_kind(&items, "dns_servers").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("192.168.1.1（局域网 / 本机）"));
    // 机房出口（数据库标记）+ 两个接口看到的国家不一致；数据源没有标记时按运营商名判断
    {
        let mut m = mock().lock().unwrap();
        m.meta_org = "DigitalOcean, LLC".into();
        m.meta_country = "CA".into();
        m.ipapi_type = "hosting".into();
        m.rdap_country = String::new();
    }
    check_region(&store, 0).unwrap();
    let items = risk_items(&e.data(), true, true);
    let isp = by_kind(&items, "isp").unwrap();
    assert_eq!(isp["level"], "warn");
    assert!(
        isp["text"]
            .as_str()
            .unwrap()
            .contains("DigitalOcean, LLC（AS7922），是机房 / 云服务器"),
        "{}",
        isp
    );
    let mm = by_kind(&items, "region_mismatch").unwrap();
    assert_eq!(mm["level"], "warn");
    assert!(
        mm["text"]
            .as_str()
            .unwrap()
            .contains("trace 接口看到 SG（新加坡），meta 接口看到 CA（加拿大）"),
        "{}",
        mm
    );
    mock().lock().unwrap().ipapi_type = "fail".into(); // ipapi.is 查询失败 → 退到 ipinfo.io（没有标记）→ 按运营商名判断
    check_region(&store, 0).unwrap();
    assert!(e.data()["region"]["ip_type"].is_null());
    assert_eq!(e.data()["region"]["meta_source"], "ipinfo.io");
    assert_eq!(
        by_kind(&risk_items(&e.data(), true, true), "isp").unwrap()["level"],
        "warn"
    );
    {
        let mut m = mock().lock().unwrap();
        m.meta_org = String::new();
        m.meta_country = String::new();
        m.ipapi_type = String::new();
    }
    // WARP、时钟偏差、官方状态页
    {
        let mut m = mock().lock().unwrap();
        m.trace_warp = true;
        m.trace_ts_offset = -600;
        m.status_indicator = "major".into();
    }
    check_region(&store, 0).unwrap();
    let d = e.data();
    let items = risk_items(&d, true, true);
    assert_eq!(by_kind(&items, "warp").unwrap()["level"], "info");
    let clock = by_kind(&items, "clock").unwrap();
    assert_eq!(clock["level"], "warn");
    assert!(
        clock["text"].as_str().unwrap().contains("+600 秒"),
        "{}",
        clock
    );
    let st = by_kind(&items, "status").unwrap();
    assert_eq!(st["level"], "bad");
    assert!(
        st["text"]
            .as_str()
            .unwrap()
            .contains("Partial System Outage"),
        "{}",
        st
    );
    assert_eq!(d["status_page"]["indicator"], "major");
    // 服务检查把风险项都列出来，总体结论跟着最严重的走；出口地区只出现一次
    let items = service_check(&store);
    assert_eq!(items[0]["level"], "bad");
    assert!(
        items[0]["text"]
            .as_str()
            .unwrap()
            .contains("Anthropic 官方状态页"),
        "{}",
        items[0]
    );
    assert_eq!(items[0]["grade"], "high");
    assert!(items[0]["score"].as_i64().unwrap() < 70);
    // 评分单独算
    let sc = risk_score(&[
        json!({"kind": "timezone", "level": "warn"}),
        json!({"kind": "clock", "level": "warn"}),
    ]);
    assert_eq!(
        (sc["score"].as_i64(), sc["grade"].as_str()),
        (Some(65), Some("high"))
    );
    let sc = risk_score(&[
        json!({"kind": "region_change", "level": "warn"}),
        json!({"kind": "warp", "level": "info"}),
    ]);
    assert_eq!(
        (sc["score"].as_i64(), sc["grade"].as_str()),
        (Some(80), Some("medium"))
    );
    assert_eq!(
        risk_score(&[json!({"kind": "new_kind", "level": "warn"})])["score"],
        90
    );
    let ks = kinds(&items);
    assert_eq!(ks.iter().filter(|k| *k == "region").count(), 1);
    for k in [
        "summary",
        "dns",
        "region",
        "isp",
        "ip_origin",
        "ipv6",
        "dns_servers",
        "api",
        "web",
        "proxy",
        "vpn",
        "region_change",
        "warp",
        "clock",
        "status",
        "accounts",
        "device",
        "scope",
    ] {
        assert!(ks.iter().any(|x| x == k), "缺少 {}：{:?}", k, ks);
    }
    assert!(by_kind(&items, "region").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("San Jose, California"));
    assert!(by_kind(&items, "isp").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("Comcast"));
    assert_eq!(ks.last().map(String::as_str), Some("scope"));
    assert!(items.last().unwrap()["advice"]
        .as_str()
        .unwrap()
        .contains("不等于一定合规"));
    // 状态恢复后缓存里的状态页记录也清掉
    {
        let mut m = mock().lock().unwrap();
        m.trace_warp = false;
        m.trace_ts_offset = 0;
        m.status_indicator = String::new();
    }
    check_region(&store, 0).unwrap();
    let items = risk_items(&e.data(), true, true);
    assert!(["status", "warp", "clock"]
        .iter()
        .all(|k| by_kind(&items, k).is_none()));
    // 切换接口带上提醒，但照样切；切过去的账号也记了一笔当前地区
    let res = api_switch(&store, "A", None).unwrap();
    assert_eq!(res["changed"], true);
    assert!(
        res["risks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "region_change"),
        "{}",
        res
    );
    let d = e.data();
    assert_eq!(d["active"]["claude_code"], json!(a));
    let acc = d["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == json!(a))
        .unwrap();
    assert_eq!(acc["regions"][0]["loc"], "SG");
    // 运营商信息的几种来源格式
    let m =
        parse_exit_meta(&json!({"org": "AS7922 Comcast Cable", "city": "Denver", "country": "us"}))
            .unwrap();
    assert_eq!(
        m,
        json!({"asn": 7922, "org": "Comcast Cable", "city": "Denver", "meta_loc": "US", "ip_type": null, "reg_loc": null})
    );
    assert_eq!(
        parse_exit_meta(
            &json!({"asn": 14061, "asOrganization": "DigitalOcean, LLC", "country": "SG"})
        )
        .unwrap()["asn"],
        14061
    );
    assert!(parse_exit_meta(&json!({"ip": "1.2.3.4"})).is_none());
    assert!(parse_exit_meta(&json!({"status": "fail", "message": "quota"})).is_none());
    let m = parse_exit_meta(&json!({"status": "success", "countryCode": "HK", "city": "Hong Kong", "regionName": "",
                                    "isp": "Choopa", "as": "AS20473 The Constant Company", "hosting": true, "proxy": false, "mobile": false}))
    .unwrap();
    assert_eq!(
        (
            m["asn"].as_u64(),
            m["org"].as_str(),
            m["meta_loc"].as_str(),
            m["ip_type"].as_str(),
            m["city"].as_str()
        ),
        (
            Some(20473),
            Some("Choopa"),
            Some("HK"),
            Some("hosting"),
            Some("Hong Kong")
        )
    );
    let m = parse_exit_meta(&json!({"ip": "1.2.3.4", "is_tor": true, "is_vpn": false, "is_datacenter": true,
                                    "asn": {"asn": 4134, "org": "Chinanet", "country": "cn"}, "company": {"name": "Chinanet", "type": "isp"},
                                    "location": {"country_code": "SG", "city": "Singapore", "state": "Central"}}))
    .unwrap();
    assert_eq!(
        (
            m["asn"].as_u64(),
            m["org"].as_str(),
            m["meta_loc"].as_str(),
            m["ip_type"].as_str(),
            m["reg_loc"].as_str(),
            m["city"].as_str()
        ),
        (
            Some(4134),
            Some("Chinanet"),
            Some("SG"),
            Some("tor"),
            Some("CN"),
            Some("Singapore, Central")
        )
    );
    // DNS 服务器分类
    std::env::set_var(
        "CAM_TEST_DNS",
        "10.0.0.1,8.8.8.8,223.5.5.5,fe80::1%en0,1.2.3.4",
    );
    let dns = system_dns_servers();
    std::env::set_var("CAM_TEST_DNS", "192.168.1.1");
    let rows: Vec<(String, bool, Option<String>, Option<String>)> = dns
        .iter()
        .map(|d| {
            (
                d["ip"].as_str().unwrap().to_string(),
                d["private"].as_bool().unwrap(),
                d["name"].as_str().map(String::from),
                d["country"].as_str().map(String::from),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("10.0.0.1".to_string(), true, None, None),
            (
                "8.8.8.8".to_string(),
                false,
                Some("Google".to_string()),
                None
            ),
            (
                "223.5.5.5".to_string(),
                false,
                Some("阿里 DNS".to_string()),
                Some("CN".to_string())
            ),
            ("fe80::1".to_string(), true, None, None),
            ("1.2.3.4".to_string(), false, None, None),
        ]
    );
    // VPN / 隧道判断
    assert!(tunnel_kind("utun4", "").is_some());
    assert!(tunnel_kind("Ethernet 2", "TAP-Windows Adapter V9").is_some());
    assert!(tunnel_kind("en0", "Apple Ethernet").is_none());
    std::env::set_var("CAM_TEST_ROUTE_IF", "wg0");
    let v = by_kind(&service_check(&store), "vpn").unwrap().clone();
    std::env::set_var("CAM_TEST_ROUTE_IF", "");
    assert_eq!(v["level"], "info");
    assert!(v["text"].as_str().unwrap().contains("wg0"), "{}", v);
    mock().lock().unwrap().trace_loc = String::new();
}

/// claude.ai 和 Cloudflare 的 trace 两条路径（给 Mock::down / Mock::slow 用）
fn both_traces() -> Vec<String> {
    vec!["/cdn-cgi/trace".into(), "/cf/cdn-cgi/trace".into()]
}

fn hits(prefix: &str) -> Vec<String> {
    mock()
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.1.starts_with(prefix))
        .map(|c| c.1.clone())
        .collect()
}

#[test]
fn ip_lookup_sources_and_privacy() {
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    // 默认：ipapi.is（带 key，有类型标记），不碰 ip-api.com，也不查 RDAP
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    let reg = e.data()["region"].clone();
    assert_eq!(
        (
            reg["meta_source"].as_str(),
            reg["ip_type"].as_str(),
            reg["lookup_mode"].as_str()
        ),
        (Some("ipapi.is"), Some("residential"), Some("auto"))
    );
    assert_eq!(hits("/ipapiis"), vec!["/ipapiis?key=test-key".to_string()]);
    assert!(hits("/ipapi").iter().all(|p| p.starts_with("/ipapiis")));
    assert!(hits("/rdap/").is_empty());
    let items = service_check(&store);
    assert!(by_kind(&items, "isp").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("（数据：ipapi.is）"));
    // 没配 key：ipapi.is 简版，没有类型标记，另查 RDAP
    std::env::set_var("CAM_IPAPI_IS_KEY", "");
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    let reg = e.data()["region"].clone();
    assert_eq!(
        (
            reg["meta_source"].as_str(),
            reg["org"].as_str(),
            reg["asn"].as_u64()
        ),
        (
            Some("ipapi.is"),
            Some("Comcast Cable Communications, LLC"),
            Some(7922)
        )
    );
    assert!(reg["ip_type"].is_null());
    assert_eq!(hits("/ipapiis"), vec!["/ipapiis".to_string()]);
    assert_eq!(hits("/rdap/").len(), 1);
    // 保存的 key 生效，且不写进 accounts.json；换 key 后缓存的运营商信息作废
    let pv = save_privacy(
        &store,
        &json!({"ipapi_key": "saved-key", "ipinfo_token": "tok"}),
    )
    .unwrap();
    assert!(pv["has_ipapi_key"].as_bool().unwrap() && pv["has_ipinfo_token"].as_bool().unwrap());
    let raw = std::fs::read_to_string(e.root.join("cam/accounts.json")).unwrap();
    assert!(!raw.contains("saved-key"));
    assert!(e.data()["region"].get("meta_at").is_none());
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    assert_eq!(hits("/ipapiis"), vec!["/ipapiis?key=saved-key".to_string()]);
    assert_eq!(
        save_privacy(&store, &json!({"ipapi_key": ""})).unwrap()["has_ipapi_key"],
        false
    );
    std::env::set_var("CAM_IPAPI_IS_KEY", "test-key");
    // ip-api 模式：先问 ip-api.com
    let pv = save_privacy(&store, &json!({"ip_lookup": "ip-api"})).unwrap();
    assert_eq!(pv["ip_lookup"], "ip-api");
    assert!(pv["sources"].as_array().unwrap().iter().any(|s| s["name"]
        .as_str()
        .unwrap()
        .contains("ip-api.com")
        && s["warn"] == true));
    check_region(&store, 0).unwrap();
    assert_eq!(e.data()["region"]["meta_source"], "ip-api.com");
    // 关闭：只问 claude.ai 和状态页
    let pv = save_privacy(&store, &json!({"ip_lookup": "off"})).unwrap();
    let names: Vec<&str> = pv["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["claude.ai/cdn-cgi/trace", "status.anthropic.com"]
    );
    assert!(pv["sources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["group"] == "anthropic"));
    let titles: Vec<&str> = pv["modes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["自动（推荐）", "加上 ip-api.com", "关闭"]);
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    let mut paths: Vec<String> = mock()
        .lock()
        .unwrap()
        .calls
        .iter()
        .map(|c| c.1.clone())
        .collect();
    paths.sort();
    paths.dedup();
    assert_eq!(
        paths,
        vec!["/cdn-cgi/trace".to_string(), "/status.json".to_string()]
    );
    let reg = e.data()["region"].clone();
    assert_eq!(
        (reg["loc"].as_str(), reg["lookup_mode"].as_str()),
        (Some("US"), Some("off"))
    );
    assert!(reg["org"].is_null() && reg["ip_type"].is_null());
    let items = service_check(&store);
    assert!(by_kind(&items, "isp").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("外部 IP 查询已关闭"));
    assert!(by_kind(&items, "ipv6").is_none() && by_kind(&items, "ip_origin").is_none());
    assert_eq!(
        public_state(&store, &e.data())["region"]["lookup_mode"],
        "off"
    );
    assert!(save_privacy(&store, &json!({"ip_lookup": "everything"})).is_err());
    save_privacy(&store, &json!({"ip_lookup": "auto"})).unwrap();
    // 清除地区记录；接口路由
    check_region(&store, 0).unwrap();
    assert!(e.data()["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a.get("regions").is_some()));
    let pv = clear_region_history(&store).unwrap();
    assert_eq!(pv["history"], json!({"accounts": 0, "checked_at": null}));
    assert!(e.data().get("region").is_none());
    let (code, body) = route(&store, "GET", "/api/privacy", &json!({}));
    assert_eq!((code, body["ip_lookup"].as_str()), (200, Some("auto")));
    let (code, body) = route(&store, "PUT", "/api/privacy", &json!({"ip_lookup": "off"}));
    assert_eq!((code, body["ip_lookup"].as_str()), (200, Some("off")));
    let (code, _) = route(&store, "DELETE", "/api/privacy/history", &json!({}));
    assert_eq!(code, 200);
    assert_eq!(privacy_view(&store).unwrap()["ip_lookup"], "off");
}

#[test]
fn meta_cached_while_exit_ip_unchanged() {
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    std::env::set_var("CAM_META_TTL", "86400");
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    check_region(&store, 0).unwrap();
    assert_eq!(hits("/ipapiis").len(), 1);
    assert_eq!(
        e.data()["region"]["org"],
        "Comcast Cable Communications, LLC"
    );
    mock().lock().unwrap().trace_ip = "5.6.7.8".into(); // 出口 IP 变了才重查
    check_region(&store, 0).unwrap();
    assert_eq!(hits("/ipapiis").len(), 2);
    std::env::set_var("CAM_META_TTL", "0");
}

#[test]
fn region_unknown_and_service_status_keep_scores_consistent() {
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    // 出口查不到：顶部分数不再是 100，单独扣 30
    {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        d["region"] = json!({"loc": null, "checked_at": cam_engine::store::now_iso()});
        store.save(&mut d).unwrap();
    }
    let risks = risk_items(&e.data(), true, true);
    assert!(by_kind(&risks, "region_unknown").is_some());
    assert_eq!(risk_score(&risks)["score"], 70);
    // 服务检测记下的连接问题也算进顶部分数；网络恢复后后台检测清掉 claude.ai 的旧问题，api 的保留
    {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        d["service_status"] = json!({"checked_at": cam_engine::store::now_iso(), "items": [
            {"kind": "web", "level": "bad", "text": "claude.ai：连不上", "advice": ""},
            {"kind": "api", "level": "bad", "text": "api.anthropic.com：连不上", "advice": ""}
        ]});
        store.save(&mut d).unwrap();
    }
    let kinds_now = kinds(&risk_items(&e.data(), true, true));
    assert!(kinds_now.contains(&"web".to_string()) && kinds_now.contains(&"api".to_string()));
    check_region(&store, 0).unwrap();
    let d = e.data();
    assert_eq!(d["service_status"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(d["service_status"]["items"][0]["kind"], "api");
    assert!(by_kind(&risk_items(&d, true, true), "region_unknown").is_none());
}

#[test]
fn header_score_matches_service_check() {
    let e = Env::new();
    e.two_accounts(later());
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-B1".into(), "b@x.com".into());
    let store = Store::new();
    for route in ["", "wg0", "utun4"] {
        std::env::set_var("CAM_TEST_ROUTE_IF", route);
        let items = service_check(&store);
        let header = risk_score(&risk_items(&e.data(), true, true))["score"].clone();
        assert_eq!(items[0]["score"], header, "{} {:?}", route, kinds(&items));
    }
    std::env::set_var("CAM_TEST_ROUTE_IF", "");
    assert_eq!(
        e.data()["region"]["env"]["tunnel"],
        json!(["utun4", "VPN / 隧道接口"])
    );
}

#[test]
fn review_regressions() {
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    std::env::set_var("CAM_META_TTL", "86400");
    // IP 信息全部查失败：不缓存，下次重查
    mock().lock().unwrap().ipapi_type = "fail".into();
    let base = mock_base().to_string();
    std::env::set_var("CAM_IPINFO_URL", "");
    std::env::set_var("CAM_CF_META_URL", "");
    check_region(&store, 0).unwrap();
    assert!(e.data()["region"]
        .get("meta_at")
        .map(|v| v.is_null())
        .unwrap_or(true));
    std::env::set_var("CAM_IPINFO_URL", format!("{}/ipinfo", base));
    std::env::set_var("CAM_CF_META_URL", format!("{}/meta", base));
    mock().lock().unwrap().ipapi_type = String::new();
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    assert_eq!(hits("/ipapiis").len(), 1);
    assert_eq!(
        e.data()["region"]["org"],
        "Comcast Cable Communications, LLC"
    );
    std::env::set_var("CAM_META_TTL", "0");
    // 中间失败过一次，下一次成功时仍能发现出口变了（US → 失败 → HK）
    mock().lock().unwrap().down = both_traces();
    check_region(&store, 0).unwrap();
    mock().lock().unwrap().down.clear();
    let reg = e.data()["region"].clone();
    // 留着上次查到的，记一笔没连上
    assert_eq!(
        (
            reg["loc"].as_str(),
            reg["last_loc"].as_str(),
            reg["fail_count"].as_i64()
        ),
        (Some("US"), Some("US"), Some(1))
    );
    assert!(reg["failed_at"].is_string());
    mock().lock().unwrap().trace_loc = "HK".into();
    check_region(&store, 0).unwrap();
    let d = e.data();
    assert_eq!(
        (
            d["region"]["prev_loc"].as_str(),
            d["region"]["loc"].as_str()
        ),
        (Some("US"), Some("HK"))
    );
    mock().lock().unwrap().trace_loc = String::new();
    // 后台连上 claude.ai 证明不了人机验证已经没了：这条留着
    {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        d["service_status"] = json!({"checked_at": cam_engine::store::now_iso(), "items": [
            {"kind": "web", "level": "warn", "text": "claude.ai：返回人机验证页", "advice": ""},
            {"kind": "dns", "level": "bad", "text": "DNS：claude.ai 解析失败", "advice": ""}
        ]});
        store.save(&mut d).unwrap();
    }
    check_region(&store, 0).unwrap();
    let items = e.data()["service_status"]["items"].clone();
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["kind"], "web");
}

#[test]
fn region_cached_for_display() {
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    assert!(public_state(&store, &e.data())["region"]["status"].is_null());
    check_region(&store, 300).unwrap();
    let reg = public_state(&store, &e.data())["region"].clone();
    assert_eq!(reg["loc"], "US");
    assert_eq!(reg["status"], "supported");
    assert!(reg["message"]
        .as_str()
        .unwrap()
        .contains("在 Anthropic 支持名单内"));
    // 缓存期内不再联网
    mock().lock().unwrap().trace_loc = "CN".into();
    let n = mock()
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.1 == "/cdn-cgi/trace")
        .count();
    check_region(&store, 300).unwrap();
    assert_eq!(
        mock()
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.1 == "/cdn-cgi/trace")
            .count(),
        n
    );
    assert_eq!(
        public_state(&store, &e.data())["region"]["status"],
        "supported"
    );
    // 过期后重新检测：不在名单内只是提示，切换不受影响
    check_region(&store, 0).unwrap();
    let reg = public_state(&store, &e.data())["region"].clone();
    assert_eq!(reg["status"], "unsupported");
    assert!(reg["advice"].as_str().unwrap().contains("实际所在地"));
    assert!(switch_account(&store, "A", false).unwrap().changed);
    mock().lock().unwrap().trace_loc = String::new();
}

/// 这个 kind 的有几条
fn count_kind(items: &[Value], kind: &str) -> usize {
    items.iter().filter(|i| i["kind"] == kind).count()
}

/// 把 data["region"] 里的一个时间改成这么多秒以前
fn backdate_region(store: &Store, key: &str, secs: i64) {
    let _g = store.lock().unwrap();
    let mut d = store.load().unwrap();
    d["region"][key] = json!(ago(secs));
    store.save(&mut d).unwrap();
}

#[test]
fn failed_check_keeps_last_good_result() {
    // 环境检测这次没连上：上次查到的出口留着不动，只记一笔“最近一次没连上”；那份结果旧了才按查不到提醒。
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    set_tokens(&[("at-B1", "b@x.com")]);
    let score = || risk_score(&risk_items(&e.data(), true, true))["score"].clone();
    check_region(&store, 0).unwrap();
    let good = e.data()["region"].clone();
    assert_eq!(
        (
            good["loc"].as_str(),
            good["ip"].as_str(),
            good["net"].as_str(),
            good.get("failed_at")
        ),
        (Some("US"), Some("1.2.3.4"), Some("net-a"), None)
    );
    assert_eq!(score(), 100);
    // claude.ai 和 Cloudflare 的 trace 都答不上来
    mock().lock().unwrap().down = both_traces();
    clear_calls();
    let view = check_region(&store, 0).unwrap();
    assert!(both_traces().iter().all(|p| hits(p).len() == 1)); // 两边都问过
    let reg = e.data()["region"].clone();
    for k in [
        "loc",
        "ip",
        "colo",
        "org",
        "ip_type",
        "checked_at",
        "net",
        "meta_at",
    ] {
        assert_eq!(reg[k], good[k], "{}", k);
    }
    assert!(reg["failed_at"].is_string());
    assert_eq!(reg["fail_count"], 1);
    assert!(reg["env_at"].is_string()); // 本机设置不用联网，照常更新
    assert_eq!(
        (
            view["loc"].as_str(),
            view["status"].as_str(),
            view["fail_count"].as_i64(),
            &view["failed_at"]
        ),
        (Some("US"), Some("supported"), Some(1), &reg["failed_at"])
    );
    assert_eq!(public_state(&store, &e.data())["region"]["fail_count"], 1);
    let risks = risk_items(&e.data(), true, true);
    assert_eq!(count_kind(&risks, "env_failed"), 1);
    let fail = by_kind(&risks, "env_failed").unwrap();
    assert_eq!(fail["level"], "info");
    assert!(fail["text"]
        .as_str()
        .unwrap()
        .contains("最近一次环境检测没连上"));
    assert_eq!(count_kind(&risks, "region_unknown"), 0);
    // 上次的结果还新：只说一声，不扣分
    assert_eq!(risk_score(&risks)["score"], 100);
    // 刚失败过：定时的那种检测一分钟内不重试；手动的（max_age=0）马上重试
    clear_calls();
    check_region(&store, 300).unwrap();
    assert!(mock().lock().unwrap().calls.is_empty());
    check_region(&store, 0).unwrap();
    assert_eq!(e.data()["region"]["fail_count"], 2);
    // 服务检测：出口那行说明是上次查到的，同一条提示，分数和顶部一致；总体结论不说“出口在支持名单内”
    let items = service_check(&store);
    let row = by_kind(&items, "region").unwrap();
    assert_eq!(row["level"], "ok");
    assert!(
        row["text"].as_str().unwrap().contains("这次没连上"),
        "{}",
        row
    );
    assert_eq!(count_kind(&items, "env_failed"), 1);
    assert_eq!(items[0]["score"], score());
    let summary = items[0]["text"].as_str().unwrap();
    assert!(summary.contains("这次没测到出口地区"), "{}", summary);
    assert!(!summary.contains("出口地区在支持名单内"), "{}", summary);
    assert_eq!(e.data()["region"]["fail_count"], 3);
    // 上次查到的已经是两小时前的：出口现在在哪说不准，按“查不到”提醒并扣分，不再另报一条“结果旧了”
    backdate_region(&store, "checked_at", 2 * 3600);
    let risks = risk_items(&e.data(), true, true);
    assert_eq!(count_kind(&risks, "region_unknown"), 1);
    assert_eq!(count_kind(&risks, "env_failed"), 0);
    assert_eq!(count_kind(&risks, "env_stale"), 0);
    let unknown = by_kind(&risks, "region_unknown").unwrap()["text"]
        .as_str()
        .unwrap();
    assert!(unknown.contains("连续 3 次检测都没连上"), "{}", unknown);
    assert!(unknown.contains("上一次查到是"), "{}", unknown);
    assert_eq!(risk_score(&risks)["score"], 70);
    let items = service_check(&store);
    assert_eq!(count_kind(&items, "region_unknown"), 1);
    assert_eq!(count_kind(&items, "env_stale"), 0);
    assert_eq!(
        (items[0]["score"].as_i64(), score().as_i64()),
        (Some(70), Some(70))
    );
    // 上次查到的出口不在支持名单：“不在名单”那条已经扣过分，没连上只说一声，不叠一条“查不到”
    {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        d["region"]["loc"] = json!("CN");
        store.save(&mut d).unwrap();
    }
    let risks = risk_items(&e.data(), true, true);
    assert_eq!(
        (
            count_kind(&risks, "region"),
            count_kind(&risks, "env_failed")
        ),
        (1, 1)
    );
    assert_eq!(count_kind(&risks, "region_unknown"), 0);
    assert_eq!(by_kind(&risks, "env_failed").unwrap()["level"], "info");
    // 连上了：失败记录清掉，换成新查到的
    mock().lock().unwrap().down.clear();
    check_region(&store, 0).unwrap();
    let reg = e.data()["region"].clone();
    assert_eq!(
        (reg["loc"].as_str(), reg["prev_loc"].as_str()),
        (Some("US"), Some("CN"))
    );
    assert!(reg.get("failed_at").is_none() && reg.get("fail_count").is_none());
    assert_eq!(public_state(&store, &e.data())["region"]["fail_count"], 0);
    assert_eq!(
        count_kind(&risk_items(&e.data(), true, true), "env_failed"),
        0
    );
    // 从来没查到过的：还是记成查不到
    {
        let _g = store.lock().unwrap();
        let mut d = store.load().unwrap();
        d.as_object_mut().unwrap().remove("region");
        store.save(&mut d).unwrap();
    }
    mock().lock().unwrap().down = both_traces();
    check_region(&store, 0).unwrap();
    let reg = e.data()["region"].clone();
    assert!(reg["loc"].is_null());
    assert_eq!(reg["fail_count"], 1);
    let risks = risk_items(&e.data(), true, true);
    assert_eq!(count_kind(&risks, "region_unknown"), 1);
    assert!(by_kind(&risks, "region_unknown").unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("出口地区查不到"));
    assert_eq!(risk_score(&risks)["score"], 70);
}

#[test]
fn exit_trace_asks_cloudflare_when_claude_ai_is_slow() {
    // claude.ai 的 trace 两秒没回音（这里调成 0.2 秒）或者答不上来：同时问 Cloudflare，谁先给出结果用谁的。
    let _e = Env::new();
    let paths = || -> Vec<String> {
        let m = mock().lock().unwrap();
        m.calls.iter().map(|c| c.1.clone()).collect()
    };
    let timed = |ms: u64, fallback: bool| {
        let t0 = Instant::now();
        (
            exit_trace(Duration::from_millis(ms), fallback),
            t0.elapsed(),
        )
    };
    let place = |got: &Option<Value>| {
        let got = got.as_ref().expect("没有查到出口");
        (
            got["loc"].as_str().unwrap_or("").to_string(),
            got["colo"].as_str().unwrap_or("").to_string(),
        )
    };
    let secs = Duration::from_secs;
    std::env::set_var("CAM_TEST_TRACE_HEDGE_MS", "200");
    // 正常：只问 claude.ai
    let (got, _) = timed(3000, true);
    assert_eq!(place(&got), ("US".into(), "SJC".into()));
    assert_eq!(paths(), ["/cdn-cgi/trace"]);
    // claude.ai 卡住：0.2 秒后同时问 Cloudflare，用它的结果，不等满 3 秒
    clear_calls();
    mock().lock().unwrap().slow = vec!["/cdn-cgi/trace".into()];
    let (got, took) = timed(3000, true);
    assert_eq!(place(&got), ("US".into(), "LAX".into()));
    assert!(took < secs(2), "{:?}", took);
    assert_eq!(paths(), ["/cdn-cgi/trace", "/cf/cdn-cgi/trace"]);
    drain();
    // 设置里关了外部查询（fallback=false）：只问 claude.ai，到时限返回 None
    clear_calls();
    mock().lock().unwrap().slow = vec!["/cdn-cgi/trace".into()];
    let (got, took) = timed(600, false);
    assert!(got.is_none());
    assert!(took < secs(2), "{:?}", took);
    assert_eq!(paths(), ["/cdn-cgi/trace"]);
    drain();
    // 两边都卡住：等到时限返回 None，不会更久
    clear_calls();
    mock().lock().unwrap().slow = both_traces();
    let (got, took) = timed(800, true);
    assert!(got.is_none());
    assert!(
        took >= Duration::from_millis(700) && took < Duration::from_millis(2500),
        "{:?}",
        took
    );
    assert_eq!(paths(), both_traces());
    drain();
    std::env::set_var("CAM_TEST_TRACE_HEDGE_MS", "5000");
    // claude.ai 一下就答不上来（503）：不等那 5 秒，马上问 Cloudflare
    clear_calls();
    mock().lock().unwrap().down = vec!["/cdn-cgi/trace".into()];
    let (got, took) = timed(6000, true);
    assert_eq!(place(&got).1, "LAX");
    assert!(took < secs(2), "{:?}", took);
    // 两边都答不上来：马上返回 None
    mock().lock().unwrap().down = both_traces();
    let (got, took) = timed(6000, true);
    assert!(got.is_none());
    assert!(took < secs(2), "{:?}", took);
    std::env::remove_var("CAM_TEST_TRACE_HEDGE_MS");
}

#[test]
fn stuck_third_parties_do_not_hold_up_the_exit_check() {
    // 查运营商、注册国、IPv4 / IPv6 出口的第三方接口卡住：到时限当作查不到，出口地区照样记下来，不算检测失败。
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    mock().lock().unwrap().slow = [
        "/ipapiis", "/ipinfo", "/ipapi", "/meta", "/rdap", "/trace4", "/trace6",
    ]
    .iter()
    .map(|p| p.to_string())
    .collect();
    std::env::set_var("CAM_TEST_META_BUDGET_MS", "600");
    std::env::set_var("CAM_TEST_META_SOURCE_MS", "250");
    let t0 = Instant::now();
    check_region(&store, 0).unwrap();
    let took = t0.elapsed();
    std::env::remove_var("CAM_TEST_META_BUDGET_MS");
    std::env::remove_var("CAM_TEST_META_SOURCE_MS");
    assert!(took < Duration::from_secs(3), "{:?}", took);
    let reg = e.data()["region"].clone();
    assert_eq!(
        (reg["loc"].as_str(), reg["ip"].as_str()),
        (Some("US"), Some("1.2.3.4"))
    );
    // 运营商没查到：不缓存（meta_at），下次重查
    for k in ["org", "v4_loc", "failed_at", "meta_at"] {
        assert!(reg[k].is_null(), "{} {}", k, reg[k]);
    }
    assert_eq!(hits("/ipapiis").len(), 1);
    assert!(hits("/meta").is_empty()); // 前两个数据源把时限用完了，后面的不再问
    assert_eq!(risk_score(&risk_items(&e.data(), true, true))["score"], 100);
    drain();
    // 接口恢复后下一次就补上
    check_region(&store, 0).unwrap();
    let reg = e.data()["region"].clone();
    assert_eq!(
        (reg["org"].as_str(), reg["v4_loc"].as_str()),
        (Some("Comcast Cable Communications, LLC"), Some("US"))
    );
}

#[test]
fn background_rechecks_on_network_change_and_retries_failures() {
    // 两次定时检测之间：本机换了网络马上补测一次；上次没连上的过一分钟重试，再失败间隔翻倍。
    let e = Env::new();
    e.two_accounts(later());
    let store = Store::new();
    set_tokens(&[("at-B1", "b@x.com")]);
    let mut st = RegionRecheck::default();
    let mut now = now_ms() as f64 / 1000.0;
    let traces = || hits("/cdn-cgi/trace").len();
    // 后台循环（src-tauri/src/lib.rs 的 background）轮到定时刷新的那一轮。返回这一轮问了几次 claude.ai 的 trace
    let timed = |st: &mut RegionRecheck| {
        clear_calls();
        st.note_net();
        check_region(&store, 300).unwrap();
        traces()
    };
    // 两次定时刷新之间的那一轮：该补测才测
    let between = |st: &mut RegionRecheck, refresh_min: u64, now: f64| {
        clear_calls();
        if refresh_min > 0 && region_recheck_due_at(&e.data(), st, refresh_min, now) {
            check_region(&store, 0).unwrap();
        }
        traces()
    };
    let net = || e.data()["region"]["net"].as_str().unwrap_or("").to_string();
    let fails = || e.data()["region"]["fail_count"].as_i64();
    assert_eq!(timed(&mut st), 1); // 第一轮：定时检测
    assert_eq!((st.net.as_str(), net().as_str()), ("net-a", "net-a"));
    // 没到时间、网络也没变：不测
    assert_eq!(between(&mut st, 10, now), 0);
    // 换了网络（换 Wi-Fi、开关接管整机的 VPN）：马上补测一次
    std::env::set_var("CAM_TEST_NET", "net-b");
    mock().lock().unwrap().trace_ip = "5.6.7.8".into();
    assert_eq!(between(&mut st, 10, now), 1);
    assert_eq!(e.data()["region"]["ip"], "5.6.7.8");
    assert_eq!((st.net.as_str(), net().as_str()), ("net-b", "net-b"));
    assert_eq!(between(&mut st, 10, now), 0);
    // 半分钟内又变了一次：先不测（切换网络时地址会连着变几回），过了半分钟才测
    std::env::set_var("CAM_TEST_NET", "net-c");
    assert_eq!(between(&mut st, 10, now + 5.0), 0);
    assert_eq!(st.net, "net-b");
    now += 31.0;
    assert_eq!(between(&mut st, 10, now), 1);
    assert_eq!((st.net.as_str(), net().as_str()), ("net-c", "net-c"));
    // 断网（没有可用的路由）不算换了网络
    std::env::set_var("CAM_TEST_NET", "");
    now += 31.0;
    assert_eq!(between(&mut st, 10, now), 0);
    assert_eq!(st.net, "net-c");
    // 关掉了定时刷新（refresh_min=0）的也不补测
    std::env::set_var("CAM_TEST_NET", "net-d");
    assert_eq!(between(&mut st, 0, now), 0);
    // 轮到定时检测那一轮、刚好又换了网络：不因为“五分钟内测过”跳过；网络没变的才跳过
    assert_eq!(timed(&mut st), 1);
    assert_eq!((st.net.as_str(), net().as_str()), ("net-d", "net-d"));
    assert_eq!(timed(&mut st), 0);
    // 检测没连上：一分钟内不重试
    mock().lock().unwrap().down = both_traces();
    check_region(&store, 0).unwrap();
    assert_eq!(fails(), Some(1));
    assert_eq!(between(&mut st, 10, now), 0);
    backdate_region(&store, "failed_at", 61);
    assert_eq!(between(&mut st, 10, now), 1);
    assert_eq!(fails(), Some(2));
    // 第二次失败后等两分钟，第三次后等四分钟……最长不超过定时检测的间隔
    backdate_region(&store, "failed_at", 61);
    assert_eq!(between(&mut st, 10, now), 0);
    backdate_region(&store, "failed_at", 121);
    assert_eq!(between(&mut st, 10, now), 1);
    assert_eq!(fails(), Some(3));
    backdate_region(&store, "failed_at", 121);
    assert_eq!(between(&mut st, 10, now), 0);
    assert_eq!(between(&mut st, 2, now), 1); // 两分钟一轮的话最长等两分钟
    assert_eq!(fails(), Some(4));
    // 一直留着上次查到的
    assert_eq!(e.data()["region"]["loc"], "US");
    // 连上了：恢复正常
    mock().lock().unwrap().down.clear();
    backdate_region(&store, "failed_at", 1000);
    assert_eq!(between(&mut st, 10, now), 1);
    assert!(e.data()["region"].get("failed_at").is_none());
    assert_eq!(between(&mut st, 10, now), 0);
}

#[test]
fn new_login_before_claude_json_is_updated_becomes_new_account() {
    let e = Env::new();
    let (_, b) = e.two_accounts(later());
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-C1".into(), "c@x.com".into());
    e.write_creds("at-C1", "rt-C1", later());
    import_current(&Store::new(), None).unwrap();
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.secret_rt(&e.id_of("c@x.com")), "rt-C1");
}

// ---------------------------------------------------------------- token 刷新

#[test]
fn background_refresh_never_rotates_tokens() {
    let e = Env::new();
    e.two_accounts(past());
    mock()
        .lock()
        .unwrap()
        .refresh
        .insert("rt-A1".into(), ("at-A2".into(), "rt-A2".into()));
    let d = refresh_usage(&Store::new(), None, false).unwrap();
    assert_eq!(token_calls("rt-A1"), 0);
    assert!(d["accounts"][0]["usage"]["error"]
        .as_str()
        .unwrap()
        .contains("过期"));
}

#[test]
fn manual_refresh_rotates_inactive_token_and_saves_it() {
    let e = Env::new();
    let (a, _) = e.two_accounts(past());
    {
        let mut m = mock().lock().unwrap();
        m.refresh
            .insert("rt-A1".into(), ("at-A2".into(), "rt-A2".into()));
        m.tokens.insert("at-A2".into(), "a@x.com".into());
        m.tokens.insert("at-B1".into(), "b@x.com".into());
    }
    let d = refresh_usage(&Store::new(), None, true).unwrap();
    assert_eq!(token_calls("rt-A1"), 1);
    assert_eq!(token_calls("rt-B1"), 0); // 当前账号从不刷新
    assert_eq!(e.secret_rt(&a), "rt-A2");
    assert_eq!(d["accounts"][0]["usage"]["error"], Value::Null);
    assert_eq!(d["accounts"][0]["usage"]["five_hour"]["utilization"], 12.0);
    // 切回 A 用的是刷新后的 token，而且不需要再联网认人
    switch_account(&Store::new(), "A", false).unwrap();
    assert_eq!(e.live().0["accessToken"], "at-A2");
}

#[test]
fn refresh_rechecks_active_account_inside_lock() {
    let e = Env::new();
    let (a, _) = e.two_accounts(past());
    mock()
        .lock()
        .unwrap()
        .refresh
        .insert("rt-A1".into(), ("at-A2".into(), "rt-A2".into()));
    let store = Store::new();
    let guard = store.lock().unwrap();
    let a2 = a.clone();
    let t = std::thread::spawn(move || ensure_fresh_token(&Store::new(), &a2, true, None));
    std::thread::sleep(Duration::from_millis(300)); // 线程发现 A 过期，正在等锁
                                                    // 等锁期间切到了 A（相当于 switch_account 做的事）
    let mut d = store.load().unwrap();
    d["active"]["claude_code"] = json!(a);
    store.save(&mut d).unwrap();
    e.login("at-A1", "rt-A1", "uuid-A", "a@x.com", past());
    drop(guard);
    // 现在 A 是当前账号而且登录过期了：不替它续（交给 Claude Code），也不拿过期的 token 白跑一趟
    let err = t.join().unwrap().unwrap_err();
    assert!(err.contains("等 Claude Code 自己续"), "{}", err);
    assert_eq!(token_calls("rt-A1"), 0);
}

#[test]
fn invalid_refresh_token_says_relogin() {
    let e = Env::new();
    let store = Store::new();
    e.login("at-A1", "rt-dead", "uuid-A", "a@x.com", past());
    import_current(&store, Some("A")).unwrap();
    e.login("at-B1", "rt-B1", "uuid-B", "b@x.com", later());
    import_current(&store, Some("B")).unwrap();
    let d = refresh_usage(&store, None, true).unwrap();
    assert!(d["accounts"][0]["usage"]["error"]
        .as_str()
        .unwrap()
        .contains("重新登录"));
}

#[test]
fn usage_refuses_credentials_that_belong_to_another_account() {
    let e = Env::new();
    let (a, _) = e.two_accounts(later());
    let store = Store::new();
    let mut sec = store.load_secret(&a);
    sec["claudeAiOauth"]["accessToken"] = json!("at-B1");
    store.save_secret(&a, &sec).unwrap();
    mock()
        .lock()
        .unwrap()
        .tokens
        .insert("at-B1".into(), "b@x.com".into());
    let d = refresh_usage(&store, None, true).unwrap();
    let acc = &d["accounts"][0];
    assert!(acc["usage"]["error"].as_str().unwrap().contains("b@x.com"));
    assert_eq!(acc["email"], "a@x.com");
}

// 读用量的时限比一般请求短，“等了好久都没连上”又比它短
const _: () = assert!(USAGE_GIVE_UP < USAGE_TIMEOUT && USAGE_TIMEOUT < 20);

#[test]
fn usage_gives_up_when_the_network_hangs() {
    // 读账号资料等了好几秒都没连上：不接着读用量（同样要白等那么久）；一下就失败的才接着试。
    let e = Env::new();
    let (_, b) = e.two_accounts(later());
    let store = Store::new();
    set_tokens(&[("at-B1", "b@x.com")]);
    // 账号资料的接口卡住不回。时限调成 0.3 秒，等过 0.1 秒就算“等了好久”
    std::env::set_var("CAM_TEST_USAGE_TIMEOUT_MS", "300");
    std::env::set_var("CAM_TEST_USAGE_GIVE_UP_MS", "100");
    mock().lock().unwrap().slow = vec!["/api/oauth/profile".into()];
    clear_calls();
    let t0 = Instant::now();
    refresh_usage(&store, Some(&b), false).unwrap();
    let took = t0.elapsed();
    std::env::remove_var("CAM_TEST_USAGE_TIMEOUT_MS");
    std::env::remove_var("CAM_TEST_USAGE_GIVE_UP_MS");
    assert!(took < Duration::from_secs(2), "{:?}", took);
    assert!(!usage_error(&e, &b).is_empty());
    assert_eq!((profile_calls(), hits("/api/oauth/usage").len()), (1, 0));
    drain();
    // 一下就失败的（连接被重置那一类）：接着读用量，读得到
    mock().lock().unwrap().broken = vec!["/api/oauth/profile".into()];
    clear_calls();
    refresh_usage(&store, Some(&b), false).unwrap();
    assert_eq!((profile_calls(), hits("/api/oauth/usage").len()), (1, 1));
    let u = acc_of(&e, &b)["usage"].clone();
    assert_eq!(u["error"], Value::Null);
    assert_eq!(u["five_hour"]["utilization"], 12.0);
}

#[test]
fn local_window_usage_counts_current_windows() {
    // 当前窗口里本机用了多少：按切换记录归属、按窗口起点截断、按模型的周额度只算那个模型。对应 test_cam.py 的同名场景
    let e = Env::new();
    let now = chrono::Utc::now();
    let ev = |mid: &str, hours_ago: f64, model: &str, out: i64, cache_read: i64| {
        let ts = now - chrono::Duration::milliseconds((hours_ago * 3600e3) as i64);
        json!({"type": "assistant", "requestId": format!("r{}", mid),
               "timestamp": ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
               "message": {"id": mid, "model": model,
                           "usage": {"input_tokens": 10, "output_tokens": out, "cache_read_input_tokens": cache_read}}})
        .to_string()
    };
    let lines = [
        ev("m1", 1.0, "claude-opus-5-5", 1000, 100000),
        ev("m1", 1.0, "claude-opus-5-5", 1000, 100000),
        ev("m2", 2.0, "claude-fable-5-1", 2000, 0),
        ev("m3", 30.0, "claude-fable-5-1", 500, 0),
        ev("m4", 200.0, "claude-opus-5-5", 9999, 0),
    ];
    std::fs::write(
        e.root.join("claude/projects/p1/w.jsonl"),
        lines.join("\n") + "\n",
    )
    .unwrap();
    let iso = |d: chrono::Duration| (now + d).to_rfc3339();
    let usage = json!({
        "five_hour": {"utilization": 20, "resets_at": iso(chrono::Duration::minutes(150))},
        "seven_day": {"utilization": 10, "resets_at": iso(chrono::Duration::days(5))},
        "models": [{"name": "Fable", "utilization": 8, "resets_at": iso(chrono::Duration::days(5))}]
    });
    let usages = HashMap::from([("A".to_string(), usage)]);
    // 没有切换记录：都算当前账号
    let mut data = json!({"active": {"claude_code": "A"}, "switch_log": []});
    let got = local_window_usage(&data, &usages, now)["A"]["windows"].clone();
    assert_eq!(got["five_hour"]["messages"], 2); // m1（去重）、m2；m3 在 5 小时窗口之前
    assert_eq!(got["five_hour"]["tokens"], 3020);
    let want = (10.0 * 4.0 + 1000.0 * 20.0 + 100000.0 * 0.2 + 10.0 * 10.0 + 2000.0 * 50.0) / 1e6;
    assert!((got["five_hour"]["cost_usd"].as_f64().unwrap() - want).abs() < 1e-4);
    assert_eq!(got["seven_day"]["messages"], 3); // m4 在 7 天窗口之前
    assert_eq!(got["Fable"]["messages"], 2); // 只算 Fable 的
    assert_eq!(got["Fable"]["tokens"], 2520);
    // 一个半小时前切到了别的号：之后的消息不算 A 的，最早一次切换之前的算不出来
    data["switch_log"] = json!([{"id": "A", "at": iso(-chrono::Duration::hours(10))},
                                {"id": "bbbb", "at": iso(-chrono::Duration::minutes(90))}]);
    let got = local_window_usage(&data, &usages, now)["A"]["windows"].clone();
    assert_eq!(got["five_hour"]["messages"], 1); // 只有 m2
    assert_eq!(got["seven_day"]["messages"], 1); // m3 在第一次切换之前
    let none = HashMap::from([("A".to_string(), json!({"five_hour": null}))]);
    assert!(local_window_usage(&data, &none, now).is_empty());
}

// ---------------------------------------------------------------- 文件

#[cfg(unix)]
#[test]
fn claude_json_symlink_is_kept() {
    let e = Env::new();
    let target = e.root.join("dotfiles-claude.json");
    std::fs::rename(e.cj(), &target).unwrap();
    std::os::unix::fs::symlink(&target, e.cj()).unwrap();
    e.two_accounts(later());
    switch_account(&Store::new(), "A", false).unwrap();
    assert!(std::fs::symlink_metadata(e.cj())
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        read_json(&target).unwrap().unwrap()["oauthAccount"]["emailAddress"],
        "a@x.com"
    );
}

#[test]
fn concurrent_change_to_claude_json_is_merged() {
    let e = Env::new();
    let mut calls = 0;
    let path = e.cj();
    update_json_file(
        &path,
        |obj| {
            calls += 1;
            if calls == 1 {
                // 我们读完之后、写入之前，Claude Code 改了文件
                let mut other = read_json(&path).unwrap().unwrap();
                other["projects"]["/work/new"] = json!({});
                std::fs::write(&path, other.to_string()).unwrap();
            }
            obj.insert("oauthAccount".into(), json!({"emailAddress": "z@x.com"}));
        },
        |_| true,
    )
    .unwrap();
    let cj = read_json(&e.cj()).unwrap().unwrap();
    assert_eq!(calls, 2);
    assert!(cj["projects"].get("/work/new").is_some());
    assert_eq!(cj["oauthAccount"]["emailAddress"], "z@x.com");
    assert_eq!(cj["numStartups"], 42);
}

// ---------------------------------------------------------------- 锁

#[test]
fn lock_excludes_other_holders_in_same_process() {
    let _e = Env::new();
    let store = Store::new();
    let path = store.root.join("accounts.lock");
    let g = store.lock().unwrap();
    assert!(try_lock_file(&path).unwrap().is_none());
    drop(g);
    assert!(try_lock_file(&path).unwrap().is_some());
    let bg = store.try_background_lock().unwrap();
    assert!(bg.is_some());
    assert!(store.try_background_lock().unwrap().is_none());
}

/// Python 版拿着锁时 Rust 版拿不到；Python 进程被强杀后锁立刻可用。两边用的是同一种系统锁。
#[test]
fn lock_is_shared_with_python_and_released_when_holder_dies() {
    let _e = Env::new();
    let py = ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok()
    });
    let Some(py) = py else {
        eprintln!("没有 Python，跳过");
        return;
    };
    let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts");
    let code = "import sys,time,os; sys.path.insert(0, sys.argv[1]); import cam; \
                l = cam.Store().lock(); l.__enter__(); \
                d = cam.FileLock(os.path.join(cam.Store().root, 'desktop-switch.lock'), timeout=0); \
                d.__enter__(); print('locked', flush=True); time.sleep(60)";
    let mut child = std::process::Command::new(py)
        .args(["-c", code, scripts.to_str().unwrap()])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut buf = [0u8; 6];
    out.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"locked");
    let path = Store::new().root.join("accounts.lock");
    assert!(try_lock_file(&path).unwrap().is_none());
    let desktop_path = Store::new().root.join("desktop-switch.lock");
    assert!(try_lock_file(&desktop_path).unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
    let t0 = Instant::now();
    drop(Store::new().lock().unwrap());
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(try_lock_file(&desktop_path).unwrap().is_some());
}

// ---------------------------------------------------------------- 进程识别

#[test]
fn state_reports_sessions_and_whether_one_is_really_working() {
    let e = Env::new();
    let store = Store::new();
    let sessions = || public_state(&store, &e.data())["sessions"].clone();
    assert_eq!(sessions(), json!({"count": 0, "busy": false}));
    // 开着但会话记录没动过：在等你输入，不算在执行
    fake_claude(true);
    assert_eq!(sessions(), json!({"count": 1, "busy": false}));
    // 会话记录刚写过：在执行
    std::fs::write(e.root.join("claude/projects/p1/s.jsonl"), "{}\n").unwrap();
    assert_eq!(sessions(), json!({"count": 1, "busy": true}));
    // 会话都关了：记录再新也不算
    fake_claude(false);
    assert_eq!(sessions(), json!({"count": 0, "busy": false}));
}

#[test]
fn incident_records_how_many_sessions_were_running() {
    let e = Env::new();
    let (a, _) = e.two_accounts(past()); // 假接口不认 rt-A1：A 的登录续不上了
    let store = Store::new();
    fake_claude(true);
    let d = refresh_usage(&store, None, true).unwrap();
    fake_claude(false);
    let acc = d["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == json!(a))
        .unwrap();
    let incidents = acc["incidents"].as_array().unwrap();
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0]["kind"], json!("login_invalid"));
    assert_eq!(incidents[0]["sessions"], json!(1));
    // 同一种问题还在：再刷新不重复记
    let d = refresh_usage(&store, None, true).unwrap();
    let acc = d["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == json!(a))
        .unwrap();
    assert_eq!(acc["incidents"].as_array().unwrap().len(), 1);
}

#[test]
fn claude_process_detection_rules() {
    let yes = [
        ("claude", "claude --resume"),
        ("/Users/a/.local/share/claude/versions/2.1.3", "claude"),
        ("C:\\Users\\a\\.local\\bin\\claude.exe", ""),
        (
            "node",
            "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
        ),
        (
            "/Users/a/Library/Application Support/Claude/claude-code/2.0/claude",
            "",
        ),
    ];
    let no = [
        ("/usr/lib/claude-desktop/claude-desktop", ""),
        ("/Applications/Claude.app/Contents/MacOS/Claude", ""),
        (
            "C:\\Users\\a\\AppData\\Local\\AnthropicClaude\\app-1\\claude.exe",
            "",
        ),
        ("/bin/bash", "bash -c /home/x/.local/bin/claude"),
        ("node", "node server.js"),
    ];
    for (a, c) in yes {
        assert!(is_claude_cmd(a, c), "应识别为 claude：{}", a);
    }
    for (a, c) in no {
        assert!(!is_claude_cmd(a, c), "不应识别为 claude：{}", a);
    }
}

#[test]
fn desktop_account_keeps_a_separate_profile() {
    let e = Env::new();
    let data_dir = e.root.join("claude-desktop");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join("config.json"),
        r#"{"lastKnownAccountUuid":"abc"}"#,
    )
    .unwrap();
    std::env::set_var("CAM_DESKTOP_DRY_RUN", "1");
    std::env::set_var("CAM_CLAUDE_DESKTOP_DATA", &data_dir);
    let store = Store::new();
    let (code, st) = route(
        &store,
        "POST",
        "/api/accounts",
        &json!({"kind": "claude_desktop", "label": "当前", "use_current": true}),
    );
    assert_eq!(code, 200, "{}", st);
    let cur = st["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == "当前")
        .unwrap();
    assert_eq!(cur["desktop_profile"], json!("default"));
    let id = cur["id"].as_str().unwrap();
    let (code, r) = route(
        &store,
        "POST",
        &format!("/api/accounts/{id}/open"),
        &json!({}),
    );
    assert_eq!(code, 200, "{}", r);
    assert!(
        !r["message"].as_str().unwrap().contains("点一次邮箱"),
        "{}",
        r
    );
    let (code, err) = route(
        &store,
        "POST",
        "/api/accounts",
        &json!({"kind": "claude_desktop", "label": "重复", "use_current": true}),
    );
    assert_eq!(code, 400, "{}", err);
    let (code, st) = route(
        &store,
        "POST",
        "/api/accounts",
        &json!({"kind": "claude_desktop", "label": "第二个"}),
    );
    assert_eq!(code, 200, "{}", st);
    let other = st["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == "第二个")
        .unwrap();
    let oid = other["id"].as_str().unwrap().to_string();
    assert_ne!(other["desktop_profile"], json!("default"));
    assert!(store.root.join("desktop-profiles").join(&oid).is_dir());
    let (code, r) = route(
        &store,
        "POST",
        &format!("/api/accounts/{oid}/open"),
        &json!({}),
    );
    assert_eq!(code, 200, "{}", r);
    assert!(
        r["message"].as_str().unwrap().contains("点一次邮箱"),
        "{}",
        r
    );
    assert!(r["message"].as_str().unwrap().contains("不会覆盖"), "{}", r);
    let backups: Vec<_> = std::fs::read_dir(store.backups_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("before-desktop"))
        .collect();
    assert!(!backups.is_empty());
    let saved = std::fs::read_to_string(backups[0].path().join("config.json")).unwrap();
    assert!(saved.contains("abc"));
    assert!(data_dir.join("config.json").is_file());
    let d = store.load().unwrap();
    assert_eq!(d["active"]["claude_desktop"], json!(oid));
    let st = public_state(&store, &d);
    let marked = st["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == oid)
        .unwrap();
    assert_eq!(marked["is_active"], json!(true));

    // 旧版的全量刷新曾把 Desktop 误当 API 账号；状态视图立即隐藏旧错误，刷新也不再请求 API。
    let mut d = d;
    d["accounts"][1]["usage"] =
        json!({"error": "没有填写 API Key", "five_hour": {"utilization": 99}});
    store.save(&mut d).unwrap();
    let st = public_state(&store, &d);
    assert!(st["accounts"][1]["usage"].is_null());
    assert_eq!(st["accounts"][1]["health"]["level"], "ok");
    mock().lock().unwrap().calls.clear();
    for only in [None, Some(oid.as_str())] {
        let refreshed = refresh_usage(&store, only, true).unwrap();
        let refreshed = refreshed["accounts"].as_array().unwrap();
        assert!(refreshed.iter().all(|a| a["usage"]["error"].is_null()));
        assert!(refreshed
            .iter()
            .all(|a| a["usage"]["cache_status"] == "missing"));
    }
    assert!(mock().lock().unwrap().calls.is_empty());

    // 刷新链路确实读目标账号自己的 zstd 缓存，且保留缓存时间而不是写成本次刷新时间。
    let fixture: Value =
        serde_json::from_str(include_str!("../../../../tests/shared/desktop_usage.json")).unwrap();
    let cache = store
        .root
        .join("desktop-profiles")
        .join(&oid)
        .join("Cache/Cache_Data");
    std::fs::create_dir_all(&cache).unwrap();
    let entry = cache.join("usage_0");
    std::fs::write(
        &entry,
        hex::decode(fixture["zstd_entry_hex"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let mtime = SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&entry)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    let (code, view) = route(&store, "POST", "/api/refresh", &json!({"id": oid}));
    assert_eq!(code, 200, "{view}");
    let usage = &view["accounts"][1]["usage"];
    assert_eq!(usage["source"], "desktop_cache");
    assert_eq!(usage["five_hour"]["utilization"], 37.5);
    assert_eq!(usage["seven_day"]["utilization"], 68.25);
    assert_eq!(usage["cache_stale"], true);
    assert_eq!(view["accounts"][1]["health"]["level"], "ok");
    assert!(view["accounts"][0]["usage"]["five_hour"].is_null());
    let sampled = cam_engine::store::parse_time(usage["fetched_at"].as_str().unwrap()).unwrap();
    let checked = cam_engine::store::parse_time(usage["checked_at"].as_str().unwrap()).unwrap();
    assert!((checked - sampled).num_seconds() >= 3590);
    assert!(mock().lock().unwrap().calls.is_empty());

    // 当前 Desktop 不能移除，独立目录仍在。移除也和其它进程的切换共用互斥锁。
    let (code, err) = route(
        &store,
        "DELETE",
        &format!("/api/accounts/{oid}"),
        &json!({}),
    );
    assert_eq!(code, 400, "{}", err);
    assert!(store.root.join("desktop-profiles").join(&oid).is_dir());
    let guard = try_lock_file(&store.root.join("desktop-switch.lock"))
        .unwrap()
        .unwrap();
    let (code, err) = route(&store, "DELETE", &format!("/api/accounts/{id}"), &json!({}));
    assert_eq!(code, 400, "{}", err);
    assert!(err["error"].as_str().unwrap().contains("稍后"));
    drop(guard);
    let (code, result) = route(&store, "DELETE", &format!("/api/accounts/{id}"), &json!({}));
    assert_eq!(code, 200, "{}", result);
    assert!(data_dir.join("config.json").is_file());
    assert_eq!(store.load().unwrap()["active"]["claude_desktop"], oid);
    std::env::remove_var("CAM_DESKTOP_DRY_RUN");
    std::env::remove_var("CAM_CLAUDE_DESKTOP_DATA");
}

#[test]
fn vault_routes_encrypt_passwords_and_require_unlock_and_mail_configuration() {
    let _e = Env::new();
    let store = Store::new();
    let (code, initial) = route(&store, "GET", "/api/vault", &json!({}));
    assert_eq!(code, 200);
    assert_eq!(initial["initialized"], false);
    assert!(!store.root.join("vault.sqlite3").exists());
    let (code, state) = route(
        &store,
        "POST",
        "/api/vault/setup",
        &json!({"master_password":"synthetic vault master"}),
    );
    assert_eq!(code, 200, "{state}");
    let (code, state) = route(
        &store,
        "POST",
        "/api/vault/entries",
        &json!({"email":"work@example.com","provider":"outlook","password":"synthetic vault password"}),
    );
    assert_eq!(code, 200, "{state}");
    let entry = &state["entries"][0];
    assert!(entry.get("secret").is_none());
    assert!(entry.get("password").is_none());
    assert_eq!(entry["connected"], false);
    let id = entry["id"].as_str().unwrap();
    let (_, revealed) = route(
        &store,
        "POST",
        &format!("/api/vault/entries/{id}/reveal"),
        &json!({}),
    );
    assert_eq!(revealed["password"], "synthetic vault password");
    let bytes = std::fs::read(store.root.join("vault.sqlite3")).unwrap();
    assert!(!bytes
        .windows(b"synthetic vault password".len())
        .any(|w| w == b"synthetic vault password"));
    let (code, error) = route(
        &store,
        "POST",
        &format!("/api/vault/entries/{id}/connect"),
        &json!({}),
    );
    assert_eq!(code, 400, "{error}");
    assert!(error["error"].as_str().unwrap().contains("Client ID"));
    // 验证地址按邮箱绑定寻找目标账号，非官方地址和错误邮箱不能被打开。
    let (_, web) = route(
        &store,
        "POST",
        "/api/accounts",
        &json!({"kind":"claude_web","label":"Web","email":"work@example.com"}),
    );
    let account = web["accounts"][0]["id"].as_str().unwrap();
    let (code, updated) = route(
        &store,
        "POST",
        "/api/vault/entries",
        &json!({"id":id,"account_id":account}),
    );
    assert_eq!(code, 200, "{updated}");
    let target = cam_engine::identity::verification_target(
        &store,
        id,
        "https://claude.ai/magic-link?ticket=example-only",
    )
    .unwrap()
    .unwrap();
    assert_eq!(target["id"], account);
    assert!(cam_engine::identity::verification_target(
        &store,
        id,
        "https://evil.example/magic-link"
    )
    .is_err());
    assert!(cam_engine::identity::verification_target(
        &store,
        id,
        "https://accounts.google.com/o/oauth2/v2/auth"
    )
    .is_err());
    route(
        &store,
        "POST",
        "/api/vault/entries",
        &json!({"id":id,"email":"other@example.com"}),
    );
    assert!(cam_engine::identity::verification_target(
        &store,
        id,
        "https://claude.ai/magic-link?ticket=example-only"
    )
    .is_err());
    let (code, locked) = route(&store, "POST", "/api/vault/lock", &json!({}));
    assert_eq!(code, 200);
    assert_eq!(locked["entries"], json!([]));
    let (code, _) = route(
        &store,
        "POST",
        &format!("/api/vault/entries/{id}/reveal"),
        &json!({}),
    );
    assert_eq!(code, 400);
    assert!(mock().lock().unwrap().calls.is_empty());
}
