//! 端到端测试：临时目录当 Claude Code 配置目录和数据目录，本地起一个假的 Anthropic 接口，不碰真实账号。
//! 对应 scripts/test_cam.py 里的同名场景。

use cam_engine::api::public_state;
use cam_engine::claude::{import_current, is_claude_cmd, switch_account};
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
                let (code, out) = match path.as_str() {
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
        std::env::set_var("CAM_API_BASE", base);
        std::env::set_var("CAM_TOKEN_URLS", format!("{}/v1/oauth/token", base));
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
