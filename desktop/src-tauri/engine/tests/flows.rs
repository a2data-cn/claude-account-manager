//! 端到端测试：临时目录当 Claude Code 配置目录和数据目录，本地起一个假的 Anthropic 接口，不碰真实账号。
//! 对应 scripts/test_cam.py 里的同名场景。

use cam_engine::api::{api_switch, public_state, route};
use cam_engine::claude::{
    apply_pending_switch, apply_pending_switch_at, import_current, is_claude_cmd, switch_account, PENDING_ID,
};
use cam_engine::health::{
    check_region, clear_region_history, parse_exit_meta, privacy_view, risk_items, risk_score, save_privacy, service_check,
    system_dns_servers, tunnel_kind,
};
use cam_engine::rules::{apply_time_rules_at, save_rules};
use cam_engine::store::{read_json, try_lock_file, update_json_file, Store};
use cam_engine::usage::{ensure_fresh_token, refresh_usage};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
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
}

fn mock() -> &'static Mutex<Mock> {
    static M: OnceLock<Mutex<Mock>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn mock_base() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", srv.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
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
                if path == "/cdn-cgi/trace" {
                    let loc = if m.trace_loc.is_empty() { "US".to_string() } else { m.trace_loc.clone() };
                    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() + m.trace_ts_offset as f64;
                    let warp = if m.trace_warp { "on" } else { "off" };
                    let ip = if m.trace_ip.is_empty() { "1.2.3.4".to_string() } else { m.trace_ip.clone() };
                    drop(m);
                    let body = format!("fl=1\nip={}\nts={:.3}\nloc={}\ncolo=SJC\nwarp={}\n", ip, ts, loc, warp);
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
                let (code, out) = match path.as_str() {
                    "/api/organizations" => (403, json!({"error": {"message": "unauthorized"}})),
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
        });
        base
    })
}

fn token_calls(rt: &str) -> usize {
    mock().lock().unwrap().calls.iter().filter(|c| c.1 == "/v1/oauth/token" && c.2 == rt).count()
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
        *mock().lock().unwrap() = Mock::default();
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("cam-engine-test-{}-{}", std::process::id(), n));
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
        std::env::set_var("CAM_TEST_LOCALE", "en_US");
        std::env::set_var("CAM_TEST_DNS", "192.168.1.1");
        std::env::set_var("CAM_TEST_PROXY", ""); // 不读本机的系统代理设置
        std::env::set_var("CAM_TEST_TZ", "America/New_York=US"); // 跑测试的机器时区不定，固定成和假出口一致
        std::env::set_var("CAM_TEST_ROUTE_IF", "");
        for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::remove_var(k);
        }
        let e = Env { root, _g: g };
        std::fs::write(
            e.cj(),
            json!({"numStartups": 42, "projects": {"/work/x": {"allowedTools": ["Bash"]}}}).to_string(),
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
        (c["claudeAiOauth"].clone(), read_json(&self.cj()).unwrap().unwrap())
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
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
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
    let r = switch_account(&store, "A").unwrap();
    assert!(r.changed);
    let (live, cj) = e.live();
    assert_eq!(live["accessToken"], "at-A1");
    assert_eq!(cj["oauthAccount"]["emailAddress"], "a@x.com");
    assert_eq!(cj["numStartups"], 42);
    assert_eq!(cj["projects"], json!({"/work/x": {"allowedTools": ["Bash"]}}));
    assert_eq!(e.secret_rt(&b), "rt-B2");
    assert_eq!(std::fs::read_dir(store.backups_dir()).unwrap().count(), 1);
    switch_account(&store, "2").unwrap();
    assert_eq!(e.live().0["refreshToken"], "rt-B2");
    assert!(!switch_account(&store, "b@x.com").unwrap().changed);
}

#[test]
fn old_session_writing_back_unchanged_token_is_filed_under_its_owner() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A").unwrap();
    switch_account(&store, "B").unwrap();
    // 切换前开着的 claude 会话把 A 的凭据原样写回；~/.claude.json 仍然写着 B
    e.write_creds("at-A1", "rt-A1", later());
    import_current(&store, None).unwrap();
    let d = e.data();
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.secret_rt(&a), "rt-A1");
    assert_eq!(Store::new().load_secret(&a)["oauthAccount"]["emailAddress"], "a@x.com");
    assert_eq!(d["active"]["claude_code"], json!(a));
    let msg = d["live_mismatch"]["message"].as_str().unwrap();
    assert!(msg.contains("“A”") && msg.contains("b@x.com"), "{}", msg);
    assert_eq!(public_state(&store, &d)["warning"], json!(msg));
    // 重新切到 B 后恢复一致
    switch_account(&store, "B").unwrap();
    assert!(e.data().get("live_mismatch").is_none());
    assert_eq!(e.live().0["refreshToken"], "rt-B1");
}

#[test]
fn old_session_writing_back_refreshed_token_is_verified_by_profile() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A").unwrap();
    switch_account(&store, "B").unwrap();
    mock().lock().unwrap().tokens.insert("at-A2".into(), "a@x.com".into());
    e.write_creds("at-A2", "rt-A2", later());
    import_current(&store, None).unwrap();
    assert_eq!(e.secret_rt(&a), "rt-A2");
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.data()["active"]["claude_code"], json!(a));
}

fn pending_empty(store: &Store) -> bool {
    store.load_secret(PENDING_ID).as_object().map(|o| o.is_empty()).unwrap_or(true)
}

#[test]
fn unverifiable_login_is_parked_not_filed() {
    let e = Env::new();
    let (a, b) = e.two_accounts(later());
    let store = Store::new();
    switch_account(&store, "A").unwrap();
    switch_account(&store, "B").unwrap();
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
    assert!(public_state(&store, &d)["warning"].as_str().unwrap().contains("无法确认"));
    // 旧会话又续了一次 access token（refresh token 没换）：现在能核对，归到 A 名下，B 不动，暂存清掉
    mock().lock().unwrap().tokens.insert("at-A3".into(), "a@x.com".into());
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
    switch_account(&store, "A").unwrap();
    switch_account(&store, "B").unwrap();
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
    mock().lock().unwrap().tokens.insert("at-B2".into(), "b@x.com".into());
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
    switch_account(&store, "A").unwrap();
    switch_account(&store, "B").unwrap();
    // B 换过 token，但这里运行时 access token 已过期：先暂存；随后切走，B 换过的 token 只剩暂存这一份
    e.write_creds("at-B2", "rt-B2", past());
    switch_account(&store, "A").unwrap();
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.live().0["refreshToken"], "rt-A1");
    // 切回 B：用暂存的 refresh token 换新并核对，确认是 B 的才存进 B，切换就用上最新的
    {
        let mut m = mock().lock().unwrap();
        m.refresh.insert("rt-B2".into(), ("at-B3".into(), "rt-B3".into()));
        m.tokens.insert("at-B3".into(), "b@x.com".into());
    }
    switch_account(&store, "B").unwrap();
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
    switch_account(&store, "A").unwrap();
    switch_account(&store, "B").unwrap();
    // 暂存的其实是旧会话写回的 A 的 token，但 ~/.claude.json 说是 B
    e.write_creds("at-A2", "rt-A2", past());
    switch_account(&store, "A").unwrap();
    {
        let mut m = mock().lock().unwrap();
        m.refresh.insert("rt-A2".into(), ("at-A3".into(), "rt-A3".into()));
        m.tokens.insert("at-A3".into(), "a@x.com".into());
    }
    switch_account(&store, "B").unwrap();
    assert_eq!(e.live().0["refreshToken"], "rt-B1"); // B 用的是自己保存的
    assert_eq!(e.secret_rt(&b), "rt-B1");
    assert_eq!(e.secret_rt(&a), "rt-A3"); // 换新后归到 A
    assert!(pending_empty(&store));
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
    let (code, r) = route(&store, "POST", &format!("/api/accounts/{a}/switch"), &json!({}));
    assert_eq!(code, 200);
    assert_eq!(r["choice_required"], json!(true));
    assert!(e.data().get("pending_switch").is_none());
    let (_, r) = route(&store, "POST", &format!("/api/accounts/{a}/switch"), &json!({"mode": "wait"}));
    assert_eq!(r["deferred"], json!(true));
    let st = public_state(&store, &e.data());
    assert_eq!(st["pending_switch"]["account"], json!(a));
    assert!(st["pending_switch"]["message"].as_str().unwrap().contains("“A”"));
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
    let (_, r) = route(&store, "POST", &format!("/api/accounts/{b}/switch"), &json!({"mode": "now"}));
    assert_eq!(r["changed"], json!(true));
    route(&store, "POST", &format!("/api/accounts/{a}/switch"), &json!({"mode": "wait"}));
    let (code, st) = route(&store, "DELETE", "/api/pending-switch", &json!({}));
    assert_eq!(code, 200);
    assert!(st["pending_switch"].is_null());
    route(&store, "POST", &format!("/api/accounts/{a}/switch"), &json!({"mode": "wait"}));
    switch_account(&store, "A").unwrap();
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
    let mon10 = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap().and_hms_opt(10, 0, 0).unwrap(); // 周一
    fake_claude(true);
    assert!(apply_time_rules_at(&store, mon10).unwrap().is_none()); // 有会话在跑：先登记
    let d = e.data();
    assert_eq!(d["pending_switch"]["account"], json!(a));
    assert_eq!(d["pending_switch"]["reason"], json!("rule"));
    assert_eq!(d["active"]["claude_code"], json!(b));
    assert!(apply_time_rules_at(&store, mon10 + chrono::Duration::hours(1)).unwrap().is_none()); // 同一时间段不重复
    assert!(apply_pending_switch_at(&store, mon10 + chrono::Duration::hours(1)).unwrap().is_none()); // 会话还在
    fake_claude(false);
    // 时间段过了还没切成：作废
    assert!(apply_pending_switch_at(&store, mon10 + chrono::Duration::hours(9)).unwrap().is_none());
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
fn service_check_reports_region_and_services() {
    let e = Env::new();
    e.two_accounts(later()); // 当前登录 B
    mock().lock().unwrap().tokens.insert("at-B1".into(), "b@x.com".into());
    let items = service_check(&Store::new());
    let text = items.iter().map(|i| i["text"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("\n");
    assert_eq!(items[0]["level"], "ok", "{}", text);
    assert!(text.contains("Claude 服务正常"), "{}", text);
    assert!(text.contains("出口地区 US（美国） 在 Anthropic 支持名单内"), "{}", text);
    assert!(text.contains("服务正常，当前登录可用"), "{}", text);
    assert!(text.contains("可达"), "{}", text);
    assert!(items.iter().all(|i| i.get("advice").is_some()));
    assert_eq!((items[0]["score"].as_i64(), items[0]["grade"].as_str()), (Some(100), Some("low")));
    assert!(items[0]["text"].as_str().unwrap().contains("100/100，风险低，可以使用"), "{}", text);
    // 不在支持名单内：提醒并给建议，总体结论跟着变
    // （不在名单 60 + 出口从 US 变到 CN 15 + 当前账号两地使用 20 + 时区指向美国 5 → 0 分，风险高）
    mock().lock().unwrap().trace_loc = "CN".into();
    let items = service_check(&Store::new());
    assert_eq!(items[0]["level"], "bad");
    assert_eq!((items[0]["score"].as_i64(), items[0]["grade"].as_str()), (Some(0), Some("high")));
    assert!(items[0]["text"].as_str().unwrap().contains("0/100，风险高，不建议在当前环境使用"), "{}", items[0]);
    assert!(items[0]["text"].as_str().unwrap().contains("出口地区 CN"));
    let reg = items.iter().find(|i| i["text"].as_str().unwrap().starts_with("出口地区")).unwrap();
    assert_eq!(reg["level"], "warn");
    assert!(reg["text"].as_str().unwrap().contains("CN（中国大陆） 不在 Anthropic 支持名单内"));
    assert!(reg["advice"].as_str().unwrap().contains("封禁"));
    // 属地按所属国家看待
    mock().lock().unwrap().trace_loc = "PR".into();
    let items = service_check(&Store::new());
    let reg = items.iter().find(|i| i["text"].as_str().unwrap().starts_with("出口地区")).unwrap();
    assert_eq!(reg["level"], "ok");
    mock().lock().unwrap().trace_loc = String::new();
}

fn kinds(items: &[Value]) -> Vec<String> {
    items.iter().map(|i| i["kind"].as_str().unwrap_or("").to_string()).collect()
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
    assert_eq!((st["risk_score"]["score"].as_i64(), st["risk_score"]["grade"].as_str()), (Some(100), Some("low")));
    assert!(risks.iter().all(|r| r.as_object().unwrap().len() == 4 && r.get("advice").is_some()));
    // 出口从 US 变到 HK：不在名单 + 出口变化；当前账号 B 记了两个地区
    mock().lock().unwrap().trace_loc = "HK".into();
    check_region(&store, 0).unwrap();
    let d = e.data();
    let items = risk_items(&d, true);
    let reg = by_kind(&items, "region").unwrap();
    assert_eq!(reg["level"], "warn");
    assert!(reg["text"].as_str().unwrap().contains("HK（香港） 不在 Anthropic 支持名单内"));
    let ch = by_kind(&items, "region_change").unwrap();
    assert_eq!(ch["level"], "warn");
    assert!(ch["text"].as_str().unwrap().contains("US（美国） → HK（香港）"), "{}", ch);
    assert!(ch["text"].as_str().unwrap().contains("刚刚"), "{}", ch);
    let ar = by_kind(&items, "account_regions").unwrap();
    assert!(ar["text"].as_str().unwrap().contains("2 个地区使用过：HK（香港）、US（美国）"), "{}", ar);
    assert_eq!(d["region"]["prev_loc"], "US");
    let sc = public_state(&store, &d)["risk_score"].clone(); // 不在名单 60 + 出口变化 15 + 多地区 20 + 时区指向美国 5 → 0 分
    assert_eq!((sc["score"].as_i64(), sc["grade"].as_str()), (Some(0), Some("high")));
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
        risk_items(&e.data(), true)
    };
    let items = risks_with("CAM_TEST_TZ", "Asia/Hong_Kong=HK");
    let tz = by_kind(&items, "timezone").unwrap();
    assert_eq!(tz["level"], "warn");
    assert!(tz["text"].as_str().unwrap().contains("Asia/Hong_Kong 指向 HK（香港），与出口地区 SG（新加坡） 不一致"), "{}", tz);
    assert_eq!(by_kind(&risks_with("CAM_TEST_TZ", "Asia/Tokyo=JP"), "timezone").unwrap()["level"], "info");
    assert!(by_kind(&risks_with("CAM_TEST_TZ", "Asia/Singapore=SG,MY"), "timezone").is_none());
    std::env::set_var("CAM_TEST_TZ", "America/New_York=US");
    // 系统语言指向不支持地区：warn；指向支持地区：info
    let lc = by_kind(&risks_with("CAM_TEST_LOCALE", "zh_CN.UTF-8"), "locale").cloned().unwrap();
    assert_eq!(lc["level"], "warn");
    assert!(lc["text"].as_str().unwrap().contains("zh_CN 指向 CN（中国大陆），与出口地区 SG（新加坡） 不一致"), "{}", lc);
    assert_eq!(by_kind(&risks_with("CAM_TEST_LOCALE", "ja-JP"), "locale").unwrap()["level"], "info");
    std::env::set_var("CAM_TEST_LOCALE", "en_US");
    // DNS 解析器：局域网和任播的不提；不支持地区的公共 DNS 与出口不一致 → warn；支持地区 → info
    assert!(by_kind(&risks_with("CAM_TEST_DNS", "192.168.1.1,8.8.8.8"), "dns_resolver").is_none());
    let d = by_kind(&risks_with("CAM_TEST_DNS", "192.168.1.1,223.5.5.5"), "dns_resolver").cloned().unwrap();
    assert_eq!(d["level"], "warn");
    assert!(d["text"].as_str().unwrap().contains("223.5.5.5（阿里 DNS，CN（中国大陆））与出口地区 SG（新加坡） 不一致"), "{}", d);
    assert_eq!(by_kind(&risks_with("CAM_TEST_DNS", "168.95.1.1"), "dns_resolver").unwrap()["level"], "info");
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
    let items = risk_items(&d, true);
    let p = by_kind(&items, "proxy_ip").unwrap();
    assert_eq!(p["level"], "warn");
    assert!(p["text"].as_str().unwrap().contains("被数据库标记为代理 / VPN 出口"), "{}", p);
    let v6 = by_kind(&items, "ipv6_mismatch").unwrap();
    assert_eq!(v6["level"], "warn");
    assert!(v6["text"].as_str().unwrap().contains("IPv4 出口在 SG（新加坡），IPv6 出口在 HK（香港）（2001:db8::1）"), "{}", v6);
    let o = by_kind(&items, "ip_origin").unwrap();
    assert_eq!(o["level"], "info");
    assert!(o["text"].as_str().unwrap().contains("注册在 CA（加拿大），定位在 SG（新加坡）（广播 IP）"), "{}", o);
    assert_eq!(d["region"]["ip_type"], "proxy");
    // 手机网络、IPv6 不可用、注册国一致：都不提；服务检查里各有一行
    {
        let mut m = mock().lock().unwrap();
        m.ipapi_type = "mobile".into();
        m.trace6_loc = String::new();
        m.rdap_country = "SG".into();
    }
    check_region(&store, 0).unwrap();
    let items = risk_items(&e.data(), true);
    assert!(["proxy_ip", "isp", "ipv6_mismatch", "ip_origin"].iter().all(|k| by_kind(&items, k).is_none()));
    let items = service_check(&store);
    assert!(by_kind(&items, "isp").unwrap()["text"].as_str().unwrap().contains("IP 类型：手机网络"));
    assert!(by_kind(&items, "ipv6").unwrap()["text"].as_str().unwrap().contains("IPv6：不可用"));
    assert_eq!(by_kind(&items, "ip_origin").unwrap()["level"], "ok");
    assert!(by_kind(&items, "dns_servers").unwrap()["text"].as_str().unwrap().contains("192.168.1.1（局域网 / 本机）"));
    // 机房出口（数据库标记）+ 两个接口看到的国家不一致；数据源没有标记时按运营商名判断
    {
        let mut m = mock().lock().unwrap();
        m.meta_org = "DigitalOcean, LLC".into();
        m.meta_country = "CA".into();
        m.ipapi_type = "hosting".into();
        m.rdap_country = String::new();
    }
    check_region(&store, 0).unwrap();
    let items = risk_items(&e.data(), true);
    let isp = by_kind(&items, "isp").unwrap();
    assert_eq!(isp["level"], "warn");
    assert!(isp["text"].as_str().unwrap().contains("DigitalOcean, LLC（AS7922），是机房 / 云服务器"), "{}", isp);
    let mm = by_kind(&items, "region_mismatch").unwrap();
    assert_eq!(mm["level"], "warn");
    assert!(mm["text"].as_str().unwrap().contains("trace 接口看到 SG（新加坡），meta 接口看到 CA（加拿大）"), "{}", mm);
    mock().lock().unwrap().ipapi_type = "fail".into(); // ipapi.is 查询失败 → 退到 ipinfo.io（没有标记）→ 按运营商名判断
    check_region(&store, 0).unwrap();
    assert!(e.data()["region"]["ip_type"].is_null());
    assert_eq!(e.data()["region"]["meta_source"], "ipinfo.io");
    assert_eq!(by_kind(&risk_items(&e.data(), true), "isp").unwrap()["level"], "warn");
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
    let items = risk_items(&d, true);
    assert_eq!(by_kind(&items, "warp").unwrap()["level"], "info");
    let clock = by_kind(&items, "clock").unwrap();
    assert_eq!(clock["level"], "warn");
    assert!(clock["text"].as_str().unwrap().contains("+600 秒"), "{}", clock);
    let st = by_kind(&items, "status").unwrap();
    assert_eq!(st["level"], "bad");
    assert!(st["text"].as_str().unwrap().contains("Partial System Outage"), "{}", st);
    assert_eq!(d["status_page"]["indicator"], "major");
    // 服务检查把风险项都列出来，总体结论跟着最严重的走；出口地区只出现一次
    let items = service_check(&store);
    assert_eq!(items[0]["level"], "bad");
    assert!(items[0]["text"].as_str().unwrap().contains("Anthropic 官方状态页"), "{}", items[0]);
    assert_eq!(items[0]["grade"], "high");
    assert!(items[0]["score"].as_i64().unwrap() < 70);
    // 评分单独算
    let sc = risk_score(&[json!({"kind": "timezone", "level": "warn"}), json!({"kind": "clock", "level": "warn"})]);
    assert_eq!((sc["score"].as_i64(), sc["grade"].as_str()), (Some(65), Some("high")));
    let sc = risk_score(&[json!({"kind": "region_change", "level": "warn"}), json!({"kind": "warp", "level": "info"})]);
    assert_eq!((sc["score"].as_i64(), sc["grade"].as_str()), (Some(80), Some("medium")));
    assert_eq!(risk_score(&[json!({"kind": "new_kind", "level": "warn"})])["score"], 90);
    let ks = kinds(&items);
    assert_eq!(ks.iter().filter(|k| *k == "region").count(), 1);
    for k in [
        "summary", "dns", "region", "isp", "ip_origin", "ipv6", "dns_servers", "api", "web", "proxy", "vpn", "region_change",
        "warp", "clock", "status", "accounts", "device", "scope",
    ] {
        assert!(ks.iter().any(|x| x == k), "缺少 {}：{:?}", k, ks);
    }
    assert!(by_kind(&items, "region").unwrap()["text"].as_str().unwrap().contains("San Jose, California"));
    assert!(by_kind(&items, "isp").unwrap()["text"].as_str().unwrap().contains("Comcast"));
    assert_eq!(ks.last().map(String::as_str), Some("scope"));
    assert!(items.last().unwrap()["advice"].as_str().unwrap().contains("不等于一定合规"));
    // 状态恢复后缓存里的状态页记录也清掉
    {
        let mut m = mock().lock().unwrap();
        m.trace_warp = false;
        m.trace_ts_offset = 0;
        m.status_indicator = String::new();
    }
    check_region(&store, 0).unwrap();
    let items = risk_items(&e.data(), true);
    assert!(["status", "warp", "clock"].iter().all(|k| by_kind(&items, k).is_none()));
    // 切换接口带上提醒，但照样切；切过去的账号也记了一笔当前地区
    let res = api_switch(&store, "A", None).unwrap();
    assert_eq!(res["changed"], true);
    assert!(res["risks"].as_array().unwrap().iter().any(|r| r["kind"] == "region_change"), "{}", res);
    let d = e.data();
    assert_eq!(d["active"]["claude_code"], json!(a));
    let acc = d["accounts"].as_array().unwrap().iter().find(|x| x["id"] == json!(a)).unwrap();
    assert_eq!(acc["regions"][0]["loc"], "SG");
    // 运营商信息的几种来源格式
    let m = parse_exit_meta(&json!({"org": "AS7922 Comcast Cable", "city": "Denver", "country": "us"})).unwrap();
    assert_eq!(m, json!({"asn": 7922, "org": "Comcast Cable", "city": "Denver", "meta_loc": "US", "ip_type": null, "reg_loc": null}));
    assert_eq!(parse_exit_meta(&json!({"asn": 14061, "asOrganization": "DigitalOcean, LLC", "country": "SG"})).unwrap()["asn"], 14061);
    assert!(parse_exit_meta(&json!({"ip": "1.2.3.4"})).is_none());
    assert!(parse_exit_meta(&json!({"status": "fail", "message": "quota"})).is_none());
    let m = parse_exit_meta(&json!({"status": "success", "countryCode": "HK", "city": "Hong Kong", "regionName": "",
                                    "isp": "Choopa", "as": "AS20473 The Constant Company", "hosting": true, "proxy": false, "mobile": false}))
    .unwrap();
    assert_eq!((m["asn"].as_u64(), m["org"].as_str(), m["meta_loc"].as_str(), m["ip_type"].as_str(), m["city"].as_str()),
               (Some(20473), Some("Choopa"), Some("HK"), Some("hosting"), Some("Hong Kong")));
    let m = parse_exit_meta(&json!({"ip": "1.2.3.4", "is_tor": true, "is_vpn": false, "is_datacenter": true,
                                    "asn": {"asn": 4134, "org": "Chinanet", "country": "cn"}, "company": {"name": "Chinanet", "type": "isp"},
                                    "location": {"country_code": "SG", "city": "Singapore", "state": "Central"}}))
    .unwrap();
    assert_eq!(
        (m["asn"].as_u64(), m["org"].as_str(), m["meta_loc"].as_str(), m["ip_type"].as_str(), m["reg_loc"].as_str(), m["city"].as_str()),
        (Some(4134), Some("Chinanet"), Some("SG"), Some("tor"), Some("CN"), Some("Singapore, Central"))
    );
    // DNS 服务器分类
    std::env::set_var("CAM_TEST_DNS", "10.0.0.1,8.8.8.8,223.5.5.5,fe80::1%en0,1.2.3.4");
    let dns = system_dns_servers();
    std::env::set_var("CAM_TEST_DNS", "192.168.1.1");
    let rows: Vec<(String, bool, Option<String>, Option<String>)> = dns
        .iter()
        .map(|d| (d["ip"].as_str().unwrap().to_string(), d["private"].as_bool().unwrap(),
                  d["name"].as_str().map(String::from), d["country"].as_str().map(String::from)))
        .collect();
    assert_eq!(rows, vec![
        ("10.0.0.1".to_string(), true, None, None),
        ("8.8.8.8".to_string(), false, Some("Google".to_string()), None),
        ("223.5.5.5".to_string(), false, Some("阿里 DNS".to_string()), Some("CN".to_string())),
        ("fe80::1".to_string(), true, None, None),
        ("1.2.3.4".to_string(), false, None, None),
    ]);
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

fn hits(prefix: &str) -> Vec<String> {
    mock().lock().unwrap().calls.iter().filter(|c| c.1.starts_with(prefix)).map(|c| c.1.clone()).collect()
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
    assert_eq!((reg["meta_source"].as_str(), reg["ip_type"].as_str(), reg["lookup_mode"].as_str()),
               (Some("ipapi.is"), Some("residential"), Some("auto")));
    assert_eq!(hits("/ipapiis"), vec!["/ipapiis?key=test-key".to_string()]);
    assert!(hits("/ipapi").iter().all(|p| p.starts_with("/ipapiis")));
    assert!(hits("/rdap/").is_empty());
    let items = service_check(&store);
    assert!(by_kind(&items, "isp").unwrap()["text"].as_str().unwrap().contains("（数据：ipapi.is）"));
    // 没配 key：ipapi.is 简版，没有类型标记，另查 RDAP
    std::env::set_var("CAM_IPAPI_IS_KEY", "");
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    let reg = e.data()["region"].clone();
    assert_eq!((reg["meta_source"].as_str(), reg["org"].as_str(), reg["asn"].as_u64()),
               (Some("ipapi.is"), Some("Comcast Cable Communications, LLC"), Some(7922)));
    assert!(reg["ip_type"].is_null());
    assert_eq!(hits("/ipapiis"), vec!["/ipapiis".to_string()]);
    assert_eq!(hits("/rdap/").len(), 1);
    // 保存的 key 生效，且不写进 accounts.json；换 key 后缓存的运营商信息作废
    let pv = save_privacy(&store, &json!({"ipapi_key": "saved-key", "ipinfo_token": "tok"})).unwrap();
    assert!(pv["has_ipapi_key"].as_bool().unwrap() && pv["has_ipinfo_token"].as_bool().unwrap());
    let raw = std::fs::read_to_string(e.root.join("cam/accounts.json")).unwrap();
    assert!(!raw.contains("saved-key"));
    assert!(e.data()["region"].get("meta_at").is_none());
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    assert_eq!(hits("/ipapiis"), vec!["/ipapiis?key=saved-key".to_string()]);
    assert_eq!(save_privacy(&store, &json!({"ipapi_key": ""})).unwrap()["has_ipapi_key"], false);
    std::env::set_var("CAM_IPAPI_IS_KEY", "test-key");
    // ip-api 模式：先问 ip-api.com
    let pv = save_privacy(&store, &json!({"ip_lookup": "ip-api"})).unwrap();
    assert_eq!(pv["ip_lookup"], "ip-api");
    assert!(pv["sources"].as_array().unwrap().iter().any(|s| s["name"].as_str().unwrap().contains("ip-api.com") && s["warn"] == true));
    check_region(&store, 0).unwrap();
    assert_eq!(e.data()["region"]["meta_source"], "ip-api.com");
    // 关闭：只问 claude.ai 和状态页
    let pv = save_privacy(&store, &json!({"ip_lookup": "off"})).unwrap();
    let names: Vec<&str> = pv["sources"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["claude.ai/cdn-cgi/trace", "status.anthropic.com"]);
    assert!(pv["sources"].as_array().unwrap().iter().all(|s| s["group"] == "anthropic"));
    let titles: Vec<&str> = pv["modes"].as_array().unwrap().iter().map(|m| m["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["自动（推荐）", "加上 ip-api.com", "关闭"]);
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    let mut paths: Vec<String> = mock().lock().unwrap().calls.iter().map(|c| c.1.clone()).collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths, vec!["/cdn-cgi/trace".to_string(), "/status.json".to_string()]);
    let reg = e.data()["region"].clone();
    assert_eq!((reg["loc"].as_str(), reg["lookup_mode"].as_str()), (Some("US"), Some("off")));
    assert!(reg["org"].is_null() && reg["ip_type"].is_null());
    let items = service_check(&store);
    assert!(by_kind(&items, "isp").unwrap()["text"].as_str().unwrap().contains("外部 IP 查询已关闭"));
    assert!(by_kind(&items, "ipv6").is_none() && by_kind(&items, "ip_origin").is_none());
    assert_eq!(public_state(&store, &e.data())["region"]["lookup_mode"], "off");
    assert!(save_privacy(&store, &json!({"ip_lookup": "everything"})).is_err());
    save_privacy(&store, &json!({"ip_lookup": "auto"})).unwrap();
    // 清除地区记录；接口路由
    check_region(&store, 0).unwrap();
    assert!(e.data()["accounts"].as_array().unwrap().iter().any(|a| a.get("regions").is_some()));
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
    assert_eq!(e.data()["region"]["org"], "Comcast Cable Communications, LLC");
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
    let risks = risk_items(&e.data(), true);
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
    let kinds_now = kinds(&risk_items(&e.data(), true));
    assert!(kinds_now.contains(&"web".to_string()) && kinds_now.contains(&"api".to_string()));
    check_region(&store, 0).unwrap();
    let d = e.data();
    assert_eq!(d["service_status"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(d["service_status"]["items"][0]["kind"], "api");
    assert!(by_kind(&risk_items(&d, true), "region_unknown").is_none());
}

#[test]
fn header_score_matches_service_check() {
    let e = Env::new();
    e.two_accounts(later());
    mock().lock().unwrap().tokens.insert("at-B1".into(), "b@x.com".into());
    let store = Store::new();
    for route in ["", "wg0", "utun4"] {
        std::env::set_var("CAM_TEST_ROUTE_IF", route);
        let items = service_check(&store);
        let header = risk_score(&risk_items(&e.data(), true))["score"].clone();
        assert_eq!(items[0]["score"], header, "{} {:?}", route, kinds(&items));
    }
    std::env::set_var("CAM_TEST_ROUTE_IF", "");
    assert_eq!(e.data()["region"]["env"]["tunnel"], json!(["utun4", "VPN / 隧道接口"]));
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
    assert!(e.data()["region"].get("meta_at").map(|v| v.is_null()).unwrap_or(true));
    std::env::set_var("CAM_IPINFO_URL", format!("{}/ipinfo", base));
    std::env::set_var("CAM_CF_META_URL", format!("{}/meta", base));
    mock().lock().unwrap().ipapi_type = String::new();
    mock().lock().unwrap().calls.clear();
    check_region(&store, 0).unwrap();
    assert_eq!(hits("/ipapiis").len(), 1);
    assert_eq!(e.data()["region"]["org"], "Comcast Cable Communications, LLC");
    std::env::set_var("CAM_META_TTL", "0");
    // 中间失败过一次，下一次成功时仍能发现出口变了（US → 失败 → HK）
    std::env::set_var("CAM_IP_LOOKUP", "off"); // off 时不去问真的 www.cloudflare.com
    std::env::set_var("CAM_CLAUDE_WEB_BASE", "http://127.0.0.1:9");
    check_region(&store, 0).unwrap();
    std::env::set_var("CAM_CLAUDE_WEB_BASE", &base);
    std::env::remove_var("CAM_IP_LOOKUP");
    let d = e.data();
    assert!(d["region"]["loc"].is_null());
    assert_eq!(d["region"]["last_loc"], "US");
    mock().lock().unwrap().trace_loc = "HK".into();
    check_region(&store, 0).unwrap();
    let d = e.data();
    assert_eq!((d["region"]["prev_loc"].as_str(), d["region"]["loc"].as_str()), (Some("US"), Some("HK")));
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
    assert!(reg["message"].as_str().unwrap().contains("在 Anthropic 支持名单内"));
    // 缓存期内不再联网
    mock().lock().unwrap().trace_loc = "CN".into();
    let n = mock().lock().unwrap().calls.iter().filter(|c| c.1 == "/cdn-cgi/trace").count();
    check_region(&store, 300).unwrap();
    assert_eq!(mock().lock().unwrap().calls.iter().filter(|c| c.1 == "/cdn-cgi/trace").count(), n);
    assert_eq!(public_state(&store, &e.data())["region"]["status"], "supported");
    // 过期后重新检测：不在名单内只是提示，切换不受影响
    check_region(&store, 0).unwrap();
    let reg = public_state(&store, &e.data())["region"].clone();
    assert_eq!(reg["status"], "unsupported");
    assert!(reg["advice"].as_str().unwrap().contains("建议"));
    assert!(switch_account(&store, "A").unwrap().changed);
    mock().lock().unwrap().trace_loc = String::new();
}

#[test]
fn new_login_before_claude_json_is_updated_becomes_new_account() {
    let e = Env::new();
    let (_, b) = e.two_accounts(later());
    mock().lock().unwrap().tokens.insert("at-C1".into(), "c@x.com".into());
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
    mock().lock().unwrap().refresh.insert("rt-A1".into(), ("at-A2".into(), "rt-A2".into()));
    let d = refresh_usage(&Store::new(), None, false).unwrap();
    assert_eq!(token_calls("rt-A1"), 0);
    assert!(d["accounts"][0]["usage"]["error"].as_str().unwrap().contains("过期"));
}

#[test]
fn manual_refresh_rotates_inactive_token_and_saves_it() {
    let e = Env::new();
    let (a, _) = e.two_accounts(past());
    {
        let mut m = mock().lock().unwrap();
        m.refresh.insert("rt-A1".into(), ("at-A2".into(), "rt-A2".into()));
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
    switch_account(&Store::new(), "A").unwrap();
    assert_eq!(e.live().0["accessToken"], "at-A2");
}

#[test]
fn refresh_rechecks_active_account_inside_lock() {
    let e = Env::new();
    let (a, _) = e.two_accounts(past());
    mock().lock().unwrap().refresh.insert("rt-A1".into(), ("at-A2".into(), "rt-A2".into()));
    let store = Store::new();
    let guard = store.lock().unwrap();
    let a2 = a.clone();
    let t = std::thread::spawn(move || ensure_fresh_token(&Store::new(), &a2, true));
    std::thread::sleep(Duration::from_millis(300)); // 线程发现 A 过期，正在等锁
    // 等锁期间切到了 A（相当于 switch_account 做的事）
    let mut d = store.load().unwrap();
    d["active"]["claude_code"] = json!(a);
    store.save(&mut d).unwrap();
    e.login("at-A1", "rt-A1", "uuid-A", "a@x.com", past());
    drop(guard);
    let (tok, _) = t.join().unwrap().unwrap();
    assert_eq!(tok, "at-A1");
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
    assert!(d["accounts"][0]["usage"]["error"].as_str().unwrap().contains("重新登录"));
}

#[test]
fn usage_refuses_credentials_that_belong_to_another_account() {
    let e = Env::new();
    let (a, _) = e.two_accounts(later());
    let store = Store::new();
    let mut sec = store.load_secret(&a);
    sec["claudeAiOauth"]["accessToken"] = json!("at-B1");
    store.save_secret(&a, &sec).unwrap();
    mock().lock().unwrap().tokens.insert("at-B1".into(), "b@x.com".into());
    let d = refresh_usage(&store, None, true).unwrap();
    let acc = &d["accounts"][0];
    assert!(acc["usage"]["error"].as_str().unwrap().contains("b@x.com"));
    assert_eq!(acc["email"], "a@x.com");
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
    switch_account(&Store::new(), "A").unwrap();
    assert!(std::fs::symlink_metadata(e.cj()).unwrap().file_type().is_symlink());
    assert_eq!(read_json(&target).unwrap().unwrap()["oauthAccount"]["emailAddress"], "a@x.com");
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
    let py = ["python3", "python"]
        .into_iter()
        .find(|p| std::process::Command::new(p).arg("--version").output().is_ok());
    let Some(py) = py else {
        eprintln!("没有 Python，跳过");
        return;
    };
    let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts");
    let code = "import sys,time; sys.path.insert(0, sys.argv[1]); import cam; \
                l = cam.Store().lock(); l.__enter__(); print('locked', flush=True); time.sleep(60)";
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
    child.kill().unwrap();
    child.wait().unwrap();
    let t0 = Instant::now();
    drop(Store::new().lock().unwrap());
    assert!(t0.elapsed() < Duration::from_secs(2));
}

// ---------------------------------------------------------------- 进程识别

#[test]
fn claude_process_detection_rules() {
    let yes = [
        ("claude", "claude --resume"),
        ("/Users/a/.local/share/claude/versions/2.1.3", "claude"),
        ("C:\\Users\\a\\.local\\bin\\claude.exe", ""),
        ("node", "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"),
        ("/Users/a/Library/Application Support/Claude/claude-code/2.0/claude", ""),
    ];
    let no = [
        ("/usr/lib/claude-desktop/claude-desktop", ""),
        ("/Applications/Claude.app/Contents/MacOS/Claude", ""),
        ("C:\\Users\\a\\AppData\\Local\\AnthropicClaude\\app-1\\claude.exe", ""),
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
