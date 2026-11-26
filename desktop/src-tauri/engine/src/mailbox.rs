//! 邮箱只读接入：用户自己的 OAuth 应用、PKCE loopback 回调、TLS IMAP。
//! 只返回登录验证码和官方验证链接，绝不访问这些链接或执行邮件写操作。
use super::{store::*, vault};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use mailparse::MailHeaderMap;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use url::Url;

const MAX_BODY: usize = 256 * 1024;
const CALLBACK_PATH: &str = "/oauth/callback";
static JOBS: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
#[derive(Clone)]
struct Job {
    root: PathBuf,
    entry: String,
    status: String,
    message: String,
    expires: i64,
}
fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn root(store: &Store) -> PathBuf {
    std::fs::canonicalize(&store.root).unwrap_or_else(|_| store.root.clone())
}
fn entry_lock(store: &Store, id: &str) -> R<LockGuard> {
    let hash = hex::encode(Sha256::digest(id.as_bytes()));
    try_lock_file(&store.root.join(format!("mailbox-{hash}.lock")))?
        .ok_or_else(|| "这个邮箱正在授权或刷新，请稍后重试".into())
}
fn random_url() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn provider(entry: &Value) -> R<&str> {
    s(entry, "provider")
        .filter(|p| ["gmail", "outlook", "imap"].contains(p))
        .ok_or_else(|| "此条目只保存账号密码，没有配置收信服务".into())
}
fn tenant(entry: &Value) -> R<String> {
    let value = entry
        .get("config")
        .and_then(|c| s(c, "tenant"))
        .unwrap_or("common");
    if value.is_empty()
        || value.len() > 253
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
    {
        return Err("Microsoft 租户应为 common、organizations、租户 ID 或域名".into());
    }
    Ok(value.into())
}
fn endpoints(entry: &Value) -> R<(String, String, String)> {
    match provider(entry)? {
        "gmail" => Ok((
            "https://accounts.google.com/o/oauth2/v2/auth".into(),
            "https://oauth2.googleapis.com/token".into(),
            "https://www.googleapis.com/auth/gmail.readonly".into(),
        )),
        "outlook" => {
            let t = tenant(entry)?;
            Ok((
                format!("https://login.microsoftonline.com/{t}/oauth2/v2.0/authorize"),
                format!("https://login.microsoftonline.com/{t}/oauth2/v2.0/token"),
                "offline_access User.Read Mail.Read".into(),
            ))
        }
        _ => Err("企业 IMAP 使用 TLS 服务器和应用密码，无需浏览器授权".into()),
    }
}
trait MailApi {
    fn get(&self, url: &str, token: &str) -> R<Value>;
    fn form(&self, url: &str, fields: &[(String, String)]) -> R<Value>;
}
struct HttpApi;
fn response(response: Result<ureq::Response, ureq::Error>) -> R<Value> {
    let response = response.map_err(|e| match e {
        ureq::Error::Status(code, _) => {
            format!("邮箱服务返回 HTTP {code}，请检查授权或企业管理员策略")
        }
        _ => "无法连接邮箱服务，请检查网络后重试".into(),
    })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take((8 * 1024 * 1024 + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取邮箱服务响应")?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("邮箱服务响应过大，已停止读取".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "邮箱服务返回了无法解析的响应".into())
}
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .redirects(0)
        .try_proxy_from_env(true)
        .build()
}
impl MailApi for HttpApi {
    fn get(&self, url: &str, token: &str) -> R<Value> {
        response(
            agent()
                .get(url)
                .set("Authorization", &format!("Bearer {token}"))
                .call(),
        )
    }
    fn form(&self, url: &str, fields: &[(String, String)]) -> R<Value> {
        let fields: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        response(agent().post(url).send_form(&fields))
    }
}
fn client_id(entry: &Value) -> R<&str> {
    entry.get("config").and_then(|c|s(c,"client_id")).filter(|s|!s.trim().is_empty()).ok_or_else(||"请先配置你注册的桌面 OAuth 应用 Client ID；Gmail 需启用 Gmail API，Microsoft 需配置本机回调和委托邮件读取权限".into())
}
fn verify_owner(api: &impl MailApi, entry: &Value, token: &str) -> R<()> {
    let p = provider(entry)?;
    let profile = api.get(
        if p == "gmail" {
            "https://gmail.googleapis.com/gmail/v1/users/me/profile"
        } else {
            "https://graph.microsoft.com/v1.0/me?$select=mail,userPrincipalName"
        },
        token,
    )?;
    let candidates = if p == "gmail" {
        vec![s(&profile, "emailAddress")]
    } else {
        vec![s(&profile, "mail"), s(&profile, "userPrincipalName")]
    };
    let addresses: Vec<&str> = candidates
        .into_iter()
        .flatten()
        .filter(|e| !e.is_empty())
        .collect();
    if addresses.is_empty() {
        return Err("邮箱服务未返回可确认的邮箱地址".into());
    }
    if !addresses
        .iter()
        .any(|actual| actual.eq_ignore_ascii_case(s(entry, "email").unwrap_or("")))
    {
        return Err("授权的邮箱与此条目不一致，请在浏览器选择条目中的邮箱后重新授权".into());
    }
    Ok(())
}
fn token_secret(old: &Value, response: &Value) -> R<Value> {
    let token = s(response, "access_token").ok_or("授权未返回 access token，请重新授权")?;
    let expires = response
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0 && *n <= 31 * 86400)
        .unwrap_or(3600);
    let mut secret = json!({"access_token":token,"expires_at":Utc::now().timestamp()+expires});
    if let Some(refresh) = s(response, "refresh_token").or_else(|| s(old, "refresh_token")) {
        secret["refresh_token"] = json!(refresh);
    }
    Ok(secret)
}
fn unchanged(store: &Store, id: &str, entry: &Value) -> R<()> {
    let current = load_entry(store, id)?;
    if ["email", "provider", "config"]
        .iter()
        .any(|k| current.get(*k) != entry.get(*k))
    {
        return Err("授权期间邮箱设置已修改，请重新授权".into());
    }
    Ok(())
}
fn exchange(
    api: &impl MailApi,
    store: &Store,
    id: &str,
    entry: &Value,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> R<()> {
    unchanged(store, id, entry)?;
    let current = load_entry(store, id)?;
    let (_, token_url, _) = endpoints(entry)?;
    let mut fields = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("client_id".into(), client_id(entry)?.into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), redirect.into()),
        ("code_verifier".into(), verifier.into()),
    ];
    if let Some(secret) = current
        .get("secret")
        .and_then(|secret| s(secret, "client_secret"))
    {
        fields.push(("client_secret".into(), secret.into()));
    }
    let tokens = api.form(&token_url, &fields)?;
    let secret = token_secret(&current["secret"], &tokens)?;
    verify_owner(api, entry, s(&secret, "access_token").unwrap_or_default())?;
    unchanged(store, id, entry)?;
    vault::save_secret(store, id, &secret)
}
fn update_job(id: &str, status: &str, message: &str) {
    if let Ok(mut jobs) = jobs().lock() {
        if let Some(job) = jobs.get_mut(id) {
            job.status = status.into();
            job.message = message.into();
        }
    }
}
fn callback_query(request: &str, state: &str) -> R<Option<String>> {
    let mut parts = request.split_whitespace();
    if parts.next() != Some("GET") {
        return Err("只接受 GET 授权回调".into());
    }
    let target = parts.next().ok_or("无效授权回调")?;
    if !target.starts_with('/') || target.starts_with("//") {
        return Err("无效授权回调路径".into());
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| "无效授权回调")?;
    if url.path() != CALLBACK_PATH {
        return Err("无效授权回调路径".into());
    }
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if query.get("state").map(String::as_str) != Some(state) {
        return Err("授权 state 校验失败".into());
    }
    if query.contains_key("error") {
        return Ok(None);
    }
    query
        .get("code")
        .filter(|c| !c.is_empty() && c.len() < 4096)
        .cloned()
        .map(Some)
        .ok_or_else(|| "授权回调缺少 code".into())
}
pub fn connect(store: &Store, id: &str) -> R<Value> {
    let guard = entry_lock(store, id)?;
    let entry = load_entry(store, id)?;
    let (authorization, _, scope) = endpoints(&entry)?;
    let client = client_id(&entry)?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|_| "无法启动本机授权回调端口")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "无法设置授权回调端口")?;
    let redirect_host = if provider(&entry)? == "outlook" {
        "localhost"
    } else {
        "127.0.0.1"
    };
    let redirect = format!(
        "http://{redirect_host}:{}{CALLBACK_PATH}",
        listener
            .local_addr()
            .map_err(|_| "无法读取回调端口")?
            .port()
    );
    let state = random_url();
    let verifier = random_url();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = Url::parse(&authorization).map_err(|_| "无效授权地址")?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", client),
        ("response_type", "code"),
        ("redirect_uri", redirect.as_str()),
        ("scope", scope.as_str()),
        ("state", state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("login_hint", s(&entry, "email").unwrap_or("")),
    ]);
    if provider(&entry)? == "gmail" {
        url.query_pairs_mut()
            .extend_pairs([("access_type", "offline"), ("prompt", "consent")]);
    }
    let job_id = format!("{}{}", new_id(), new_id());
    let expires = Utc::now().timestamp() + 300;
    {
        let mut jobs = jobs().lock().map_err(|_| "无法创建授权任务")?;
        let now = Utc::now().timestamp();
        jobs.retain(|_, job| job.expires + 600 > now);
        if jobs
            .values()
            .filter(|j| j.root == root(store) && j.status == "pending" && j.expires > now)
            .count()
            >= 20
        {
            return Err("等待中的邮箱授权过多，请完成现有授权后再连接".into());
        }
        jobs.insert(
            job_id.clone(),
            Job {
                root: root(store),
                entry: id.into(),
                status: "pending".into(),
                message: "请在浏览器完成邮箱授权".into(),
                expires,
            },
        );
    }
    let task = job_id.clone();
    let store = Store {
        root: store.root.clone(),
    };
    let entry_id = id.to_string();
    let mut entry = entry;
    entry
        .as_object_mut()
        .ok_or("无效邮箱条目")?
        .remove("secret");
    std::thread::spawn(move || {
        let _guard = guard;
        let deadline = Instant::now() + Duration::from_secs(300);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    let mut line = String::new();
                    let read = BufReader::new(&mut stream).take(8192).read_line(&mut line);
                    let parsed = read
                        .map_err(|_| "无法读取授权回调".to_string())
                        .and_then(|_| callback_query(&line, &state));
                    let valid = parsed.is_ok();
                    let html = if valid {
                        "Authorization received. You may return to Claude Account Manager."
                    } else {
                        "Invalid authorization callback."
                    };
                    let _=write!(stream,"HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",if valid{"200 OK"}else{"400 Bad Request"},html.len(),html);
                    match parsed {
                        Ok(Some(code)) => {
                            match exchange(
                                &HttpApi, &store, &entry_id, &entry, &code, &redirect, &verifier,
                            ) {
                                Ok(()) => update_job(
                                    &task,
                                    "connected",
                                    "邮箱授权已保存，可读取最近的 Claude 登录邮件",
                                ),
                                Err(e) => update_job(&task, "error", &e),
                            }
                            return;
                        }
                        Ok(None) => {
                            update_job(&task, "error", "邮箱授权已取消或被服务拒绝，请重新连接");
                            return;
                        }
                        Err(_) => continue,
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(_) => {
                    update_job(&task, "error", "授权回调端口发生错误，请重试");
                    return;
                }
            }
        }
        update_job(&task, "error", "授权等待已超过 5 分钟，请重新连接邮箱");
    });
    Ok(json!({"job_id":job_id,"authorization_url":url.as_str(),"expires_at":expires}))
}
pub fn oauth_status(store: &Store, id: &str) -> R<Value> {
    let jobs = jobs().lock().map_err(|_| "无法读取授权状态")?;
    let job = jobs
        .get(id)
        .filter(|j| j.root == root(store))
        .ok_or("授权任务不存在或属于另一个数据目录")?;
    let expired = job.status == "pending" && Utc::now().timestamp() > job.expires;
    Ok(
        json!({"status":if expired{"error"}else{&job.status},"message":if expired{"授权等待已超过 5 分钟，请重新连接邮箱"}else{&job.message},"entry_id":job.entry}),
    )
}
fn access_token(api: &impl MailApi, store: &Store, id: &str, entry: &Value) -> R<String> {
    if entry["secret"]["expires_at"]
        .as_f64()
        .filter(|t| t.is_finite())
        .unwrap_or(0.0)
        > (Utc::now().timestamp() + 60) as f64
    {
        if let Some(token) = s(&entry["secret"], "access_token") {
            verify_owner(api, entry, token)?;
            return Ok(token.into());
        }
    }
    let refresh =
        s(&entry["secret"], "refresh_token").ok_or("此邮箱尚未授权或授权已失效，请先连接邮箱")?;
    let (_, endpoint, scope) = endpoints(entry)?;
    let mut fields = vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), refresh.into()),
        ("client_id".into(), client_id(entry)?.into()),
    ];
    if provider(entry)? == "outlook" {
        fields.push(("scope".into(), scope));
    }
    if let Some(secret) = s(&entry["secret"], "client_secret") {
        fields.push(("client_secret".into(), secret.into()));
    }
    let tokens = api.form(&endpoint, &fields)?;
    let secret = token_secret(&entry["secret"], &tokens)?;
    let token = s(&secret, "access_token").unwrap_or_default();
    verify_owner(api, entry, token)?;
    unchanged(store, id, entry)?;
    vault::save_secret(store, id, &secret)?;
    Ok(token.into())
}
fn allowed_domain(domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    ["anthropic.com", "claude.ai", "claude.com"]
        .iter()
        .any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}
/// 发件人的 (地址, 显示名)；不是一个合法地址时地址为 None。
fn sender_info(from: &str) -> (Option<String>, String) {
    match mailparse::addrparse(from) {
        Ok(list) if list.len() == 1 => match &list[0] {
            mailparse::MailAddr::Single(info)
                if info.addr.contains('@')
                    && !info
                        .addr
                        .chars()
                        .any(|c| c.is_whitespace() || c == '<' || c == '>') =>
            {
                (
                    Some(info.addr.clone()),
                    info.display_name.clone().unwrap_or_default(),
                )
            }
            _ => (None, String::new()),
        },
        _ => (None, String::new()),
    }
}
fn official_address(address: &str) -> bool {
    address
        .rsplit_once('@')
        .is_some_and(|(_, d)| allowed_domain(d))
}
fn sender_address(from: &str) -> Option<String> {
    sender_info(from).0.filter(|a| official_address(a))
}
// Code detection uses visible prose. Attribute values, hidden script/style text,
// and URL tokens must not be mistaken for one-time passwords.
fn visible_code_text(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut visible = String::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let low = &lower[i..];
        if low.starts_with("<!--") {
            i += low.find("-->").map(|p| p + 3).unwrap_or(rest.len());
            visible.push(' ');
            continue;
        }
        if rest.starts_with('<') {
            if let Some(end) = rest.find('>') {
                let tag = low[1..end].trim();
                let hidden = if tag == "script" || tag.starts_with("script ") {
                    Some("script")
                } else if tag == "style" || tag.starts_with("style ") {
                    Some("style")
                } else {
                    None
                };
                i += end + 1;
                if let Some(name) = hidden {
                    let close = format!("</{name}");
                    if let Some(end) = lower[i..].find(&close) {
                        i += end;
                        i += text[i..].find('>').map(|p| p + 1).unwrap_or(text.len() - i);
                    } else {
                        i = text.len();
                    }
                }
                visible.push(' ');
                continue;
            }
        }
        if ["https://", "http://", "www."]
            .iter()
            .any(|prefix| low.starts_with(prefix))
        {
            i += rest
                .find(|c: char| c.is_whitespace() || ['<', '>', '"', '\''].contains(&c))
                .unwrap_or(rest.len());
            visible.push(' ');
            continue;
        }
        if rest.starts_with('&') {
            if let Some(end) = rest.find(';').filter(|end| *end <= 16) {
                let entity = &rest[1..end];
                if !entity.is_empty()
                    && entity
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'#')
                {
                    i += end + 1;
                    visible.push(' ');
                    continue;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        visible.push(c);
        i += c.len_utf8();
    }
    visible
}
fn extracts(text: &str) -> (Vec<String>, Vec<String>) {
    let visible = visible_code_text(text);
    let bytes = visible.as_bytes();
    let mut codes = Vec::new();
    let mut i = 0;
    let token_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if (6..=8).contains(&(i - start))
                && (start == 0 || !token_char(bytes[start - 1]))
                && (i == bytes.len() || !token_char(bytes[i]))
            {
                let code = visible[start..i].to_string();
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
        } else {
            i += 1;
        }
    }
    let mut links = Vec::new();
    let mut remaining = text;
    while let Some(pos) = remaining.find("https://") {
        let rest = &remaining[pos..];
        let end = rest
            .find(|c: char| c.is_whitespace() || ['<', '>', '"', '\''].contains(&c))
            .unwrap_or(rest.len());
        let candidate = rest[..end]
            .trim_end_matches(['.', ',', ')', ';'])
            .replace("&amp;", "&");
        remaining = &rest[end..];
        if let Ok(url) = Url::parse(&candidate) {
            let intent =
                format!("{}?{}", url.path(), url.query().unwrap_or("")).to_ascii_lowercase();
            if url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none_or(|p| p == 443)
                && url.host_str().is_some_and(allowed_domain)
                && [
                    "auth", "login", "verify", "signin", "sign-in", "sign_in", "magic", "token",
                    "ticket",
                ]
                .iter()
                .any(|p| intent.contains(p))
                && !links.contains(&candidate)
            {
                links.push(candidate);
            }
        }
        if remaining.is_empty() {
            break;
        }
    }
    codes.truncate(10);
    links.truncate(10);
    (codes, links)
}
const LOOKBACK_DAYS: i64 = 7;
const DETAIL_MAX: usize = 2 * 1024 * 1024;
const PROVIDERS_JSON: &str = include_str!("../../../../web/mail-providers.json");
static PROVIDERS: OnceLock<Value> = OnceLock::new();

fn providers() -> &'static Value {
    PROVIDERS.get_or_init(|| {
        serde_json::from_str::<Value>(PROVIDERS_JSON)
            .ok()
            .filter(|v| v["providers"].is_array())
            .unwrap_or_else(|| json!({"providers":[],"unknown":{"provider":"imap","note":""}}))
    })
}
fn prefix_match(domain: &str, prefix: &str) -> bool {
    domain.strip_prefix(prefix).is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 6
            && rest
                .split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphabetic()))
    })
}
/// 按邮箱后缀猜收信方式和服务器（web/mail-providers.json），和脚本版同一份规则。
pub fn detect(address: &str) -> R<Value> {
    let address = address.trim();
    let valid = !address
        .chars()
        .any(|c| c.is_whitespace() || c == '<' || c == '>')
        && address.matches('@').count() == 1
        && address.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain
                    .char_indices()
                    .any(|(i, c)| c == '.' && i > 0 && i + 1 < domain.len())
        });
    if !valid {
        return Err("先填完整的邮箱地址".into());
    }
    let domain = address
        .rsplit_once('@')
        .map(|(_, d)| d.to_lowercase())
        .unwrap_or_default();
    let domain = domain.trim_end_matches('.');
    let data = providers();
    let found = data["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| {
            item["domains"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|d| d.as_str() == Some(domain))
                || item["prefixes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|p| prefix_match(domain, p))
        })
        .unwrap_or(&data["unknown"]);
    let text = |key: &str| s(found, key).unwrap_or("").to_string();
    Ok(json!({
        "provider": s(found, "provider").unwrap_or("imap"),
        "imap_host": text("imap_host"),
        "imap_port": found["imap_port"].as_u64().unwrap_or(993),
        "name": text("name"),
        "note": text("note"),
        "oauth": s(found, "oauth").filter(|o| !o.is_empty()),
    }))
}
/// 读邮箱用的条目：Gmail / Outlook 没填应用 ID 时，借同一种邮箱里填过的那个（只在内存里，不写回）。
fn load_entry(store: &Store, id: &str) -> R<Value> {
    let mut entry = vault::get_entry(store, id)?;
    let kind = s(&entry, "provider").unwrap_or("").to_string();
    let has = entry
        .get("config")
        .and_then(|c| s(c, "client_id"))
        .is_some_and(|c| !c.trim().is_empty());
    if (kind == "gmail" || kind == "outlook") && !has {
        let status = vault::status(store)?;
        let other = status["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| {
                s(o, "id") != Some(id)
                    && s(o, "provider") == Some(kind.as_str())
                    && o.get("config")
                        .and_then(|c| s(c, "client_id"))
                        .is_some_and(|c| !c.trim().is_empty())
            });
        if let (Some(other), Some(config)) = (
            other,
            entry.get_mut("config").and_then(Value::as_object_mut),
        ) {
            let client = s(&other["config"], "client_id").unwrap_or("").trim();
            config.insert("client_id".into(), json!(client));
            if kind == "outlook"
                && config
                    .get("tenant")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                let tenant = s(&other["config"], "tenant")
                    .filter(|t| !t.is_empty())
                    .unwrap_or("common");
                config.insert("tenant".into(), json!(tenant));
            }
        }
    }
    Ok(entry)
}
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let decoded = tail.find(';').filter(|e| *e <= 10).and_then(|e| {
            let name = &tail[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ if name.starts_with("#x") || name.starts_with("#X") => {
                    u32::from_str_radix(&name[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if name.starts_with('#') => {
                    name[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
/// HTML 变成纯文字：去掉 script / style / head 整块和所有标签，再解开实体。
fn strip_html(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    'outer: while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with('<') {
            let low = &lower[i + 1..];
            for name in ["script", "style", "head"] {
                if low.starts_with(name)
                    && low[name.len()..].starts_with(|c: char| c == '>' || c.is_ascii_whitespace())
                {
                    let close = format!("</{name}");
                    i = match lower[i..].find(&close) {
                        Some(p) => {
                            let j = i + p;
                            j + text[j..].find('>').map(|q| q + 1).unwrap_or(text.len() - j)
                        }
                        None => text.len(),
                    };
                    out.push(' ');
                    continue 'outer;
                }
            }
            if let Some(end) = rest.find('>').filter(|end| *end <= 2000) {
                i += end + 1;
                out.push(' ');
                continue;
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    decode_entities(&out)
}
fn preview(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}
/// 详情页的原邮件 HTML：去掉脚本、跳转和外部引用的标签。界面还会把它放进不许运行脚本的沙盒里显示。
fn safe_html(html: &str) -> String {
    const DROP: [&str; 10] = [
        "meta", "base", "link", "iframe", "frame", "frameset", "object", "embed", "applet", "form",
    ];
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len().min(DETAIL_MAX));
    let mut i = 0;
    while i < html.len() && out.len() < DETAIL_MAX {
        let rest = &html[i..];
        if rest.starts_with('<') {
            let low = &lower[i + 1..];
            let closing = low.starts_with('/');
            let name: String = low[usize::from(closing)..]
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            if !closing && name == "script" {
                i = match lower[i..].find("</script") {
                    Some(p) => {
                        let j = i + p;
                        j + html[j..].find('>').map(|q| q + 1).unwrap_or(html.len() - j)
                    }
                    None => html.len(),
                };
                continue;
            }
            if DROP.contains(&name.as_str()) {
                i += rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
                continue;
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}
/// 官方登录邮件里的验证码和验证链接；不是登录邮件时都是空的。
fn login_bits(subject: &str, text: &str) -> (Vec<String>, Vec<String>) {
    let content = format!("{subject}\n{text}");
    let lower = content.to_ascii_lowercase();
    if ![
        "login",
        "log in",
        "sign in",
        "sign-in",
        "verification",
        "verify",
        "验证码",
        "登录",
    ]
    .iter()
    .any(|w| lower.contains(w))
    {
        return (Vec::new(), Vec::new());
    }
    extracts(&content)
}
fn clip(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}
/// 列表里的一封信。everything=false 时只要 Claude / Anthropic 官方发的。
#[allow(clippy::too_many_arguments)]
fn summary(
    id: &str,
    from: &str,
    subject: &str,
    text: &str,
    received: DateTime<Utc>,
    since: DateTime<Utc>,
    everything: bool,
    snippet: Option<&str>,
) -> Option<Value> {
    if received < since || received > Utc::now() + chrono::Duration::minutes(5) {
        return None;
    }
    let (address, name) = sender_info(from);
    let official = address.as_deref().is_some_and(official_address);
    if !official && !everything {
        return None;
    }
    let (codes, links) = if official && !text.is_empty() {
        login_bits(subject, text)
    } else {
        (Vec::new(), Vec::new())
    };
    let shown = match snippet {
        Some(snippet) => preview(snippet),
        None => preview(&strip_html(text)),
    };
    Some(json!({
        "id": id,
        "sender": address.unwrap_or_else(|| clip(from, 200)),
        "sender_name": clip(&name, 200),
        "subject": clip(subject, 500),
        "received_at": received.to_rfc3339(),
        "preview": shown,
        "official": official,
        "login": !codes.is_empty() || !links.is_empty(),
        "codes": codes,
        "verification_links": links,
    }))
}
fn detail(
    id: &str,
    from: &str,
    subject: &str,
    received: Option<DateTime<Utc>>,
    text: &str,
    html: &str,
    truncated: bool,
) -> Value {
    let (address, name) = sender_info(from);
    let official = address.as_deref().is_some_and(official_address);
    let (codes, links) = if official {
        login_bits(subject, &format!("{text}\n{html}"))
    } else {
        (Vec::new(), Vec::new())
    };
    let plain = if text.is_empty() {
        strip_html(html)
    } else {
        text.to_string()
    };
    json!({
        "id": id,
        "sender": address.unwrap_or_else(|| clip(from, 200)),
        "sender_name": clip(&name, 200),
        "subject": clip(subject, 500),
        "received_at": received.map(|d| d.to_rfc3339()),
        "official": official,
        "login": !codes.is_empty() || !links.is_empty(),
        "codes": codes,
        "verification_links": links,
        "html": safe_html(html),
        "text": clip(&plain, 200_000),
        "truncated": truncated,
    })
}
fn gmail_text(payload: &Value, depth: usize, out: &mut String) {
    if depth > 8 || out.len() >= MAX_BODY {
        return;
    }
    let mime = s(payload, "mimeType").unwrap_or("");
    if ["text/plain", "text/html"].contains(&mime) {
        if let Some(data) = payload.get("body").and_then(|b| s(b, "data")) {
            if data.len() <= MAX_BODY * 2 {
                if let Ok(bytes) = URL_SAFE_NO_PAD.decode(data.trim_end_matches('=')) {
                    if bytes.len() <= MAX_BODY - out.len() {
                        out.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
            }
        }
    }
    if let Some(parts) = payload.get("parts").and_then(Value::as_array) {
        for p in parts.iter().take(30) {
            gmail_text(p, depth + 1, out);
        }
    }
}
/// Gmail 正文：纯文本和 HTML 分开收（详情页用）。
fn gmail_parts(payload: &Value, depth: usize, plain: &mut String, html: &mut String) {
    if depth > 10 {
        return;
    }
    let mime = s(payload, "mimeType").unwrap_or("");
    if ["text/plain", "text/html"].contains(&mime)
        && s(payload, "filename").unwrap_or("").is_empty()
    {
        if let Some(data) = payload.get("body").and_then(|b| s(b, "data")) {
            if data.len() <= DETAIL_MAX * 2 {
                if let Ok(bytes) = URL_SAFE_NO_PAD.decode(data.trim_end_matches('=')) {
                    let out = if mime == "text/html" {
                        &mut *html
                    } else {
                        &mut *plain
                    };
                    if out.len() + bytes.len() <= DETAIL_MAX {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
            }
        }
    }
    if let Some(parts) = payload.get("parts").and_then(Value::as_array) {
        for p in parts.iter().take(50) {
            gmail_parts(p, depth + 1, plain, html);
        }
    }
}
fn gmail_header(msg: &Value, name: &str) -> String {
    msg["payload"]["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|h| s(h, "name").is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .and_then(|h| s(h, "value"))
        .unwrap_or("")
        .to_string()
}
fn gmail_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
const GMAIL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/messages";
fn gmail_messages(
    api: &impl MailApi,
    token: &str,
    since: DateTime<Utc>,
    everything: bool,
) -> R<Vec<Value>> {
    let mut list = Url::parse(GMAIL).unwrap();
    let filter = if everything {
        "in:inbox"
    } else {
        "{from:anthropic.com from:claude.ai from:claude.com}"
    };
    list.query_pairs_mut()
        .append_pair("maxResults", "20")
        .append_pair("q", &format!("after:{} {filter}", since.timestamp()));
    let ids = api.get(list.as_str(), token)?;
    let mut result = Vec::new();
    for item in ids
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(20)
    {
        let Some(id) = s(item, "id").filter(|id| gmail_id(id)) else {
            continue;
        };
        let base = format!("{GMAIL}/{id}");
        let msg = if everything {
            // 全部邮件：先只拿标题和摘要，官方邮件再取正文找验证码
            let meta = api.get(
                &format!("{base}?format=metadata&metadataHeaders=From&metadataHeaders=Subject"),
                token,
            )?;
            let size = meta["sizeEstimate"].as_u64().unwrap_or(0) as usize;
            if sender_address(&gmail_header(&meta, "From")).is_some() && size <= MAX_BODY {
                api.get(&format!("{base}?format=full"), token)?
            } else {
                meta
            }
        } else {
            api.get(&format!("{base}?format=full"), token)?
        };
        let Some(date) = s(&msg, "internalDate")
            .and_then(|t| t.parse::<i64>().ok())
            .and_then(DateTime::<Utc>::from_timestamp_millis)
        else {
            continue;
        };
        let mut text = String::new();
        gmail_text(&msg["payload"], 0, &mut text);
        let snippet = s(&msg, "snippet").map(decode_entities);
        if let Some(found) = summary(
            id,
            &gmail_header(&msg, "From"),
            &gmail_header(&msg, "Subject"),
            &text,
            date,
            since,
            everything,
            snippet.as_deref().filter(|t| !t.is_empty()),
        ) {
            result.push(found);
        }
    }
    Ok(result)
}
fn gmail_detail(api: &impl MailApi, token: &str, id: &str) -> R<Value> {
    if !gmail_id(id) {
        return Err("邮件编号不正确".into());
    }
    let msg = api.get(&format!("{GMAIL}/{id}?format=full"), token)?;
    let (mut plain, mut html) = (String::new(), String::new());
    gmail_parts(&msg["payload"], 0, &mut plain, &mut html);
    if plain.is_empty() && html.is_empty() {
        plain = s(&msg, "snippet").map(decode_entities).unwrap_or_default();
    }
    let date = s(&msg, "internalDate")
        .and_then(|t| t.parse::<i64>().ok())
        .and_then(DateTime::<Utc>::from_timestamp_millis);
    Ok(detail(
        id,
        &gmail_header(&msg, "From"),
        &gmail_header(&msg, "Subject"),
        date,
        &plain,
        &html,
        msg["sizeEstimate"].as_u64().unwrap_or(0) as usize > DETAIL_MAX,
    ))
}
fn graph_from(msg: &Value) -> String {
    let address = &msg["from"]["emailAddress"];
    match (
        s(address, "name").filter(|n| !n.is_empty()),
        s(address, "address"),
    ) {
        (Some(name), Some(addr)) => format!(
            "\"{}\" <{addr}>",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        ),
        (None, Some(addr)) => addr.to_string(),
        _ => String::new(),
    }
}
fn outlook_messages(
    api: &impl MailApi,
    token: &str,
    since: DateTime<Utc>,
    everything: bool,
) -> R<Vec<Value>> {
    let mut url =
        Url::parse("https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages").unwrap();
    url.query_pairs_mut()
        .append_pair("$top", "20")
        .append_pair(
            "$select",
            "id,subject,from,receivedDateTime,body,bodyPreview",
        )
        .append_pair("$orderby", "receivedDateTime desc")
        .append_pair(
            "$filter",
            &format!(
                "receivedDateTime ge {}",
                since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
        );
    let messages = api.get(url.as_str(), token)?;
    let mut result = Vec::new();
    for msg in messages
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(20)
    {
        let Some(date) = s(msg, "receivedDateTime").and_then(parse_time) else {
            continue;
        };
        let text = msg
            .get("body")
            .and_then(|b| s(b, "content"))
            .filter(|t| t.len() <= MAX_BODY)
            .unwrap_or("");
        if let Some(item) = summary(
            s(msg, "id").unwrap_or(""),
            &graph_from(msg),
            s(msg, "subject").unwrap_or(""),
            text,
            date,
            since,
            everything,
            s(msg, "bodyPreview"),
        ) {
            result.push(item);
        }
    }
    Ok(result)
}
fn outlook_detail(api: &impl MailApi, token: &str, id: &str) -> R<Value> {
    let mut url = Url::parse("https://graph.microsoft.com/v1.0/me/messages").unwrap();
    url.path_segments_mut()
        .map_err(|_| "邮件编号不正确")?
        .push(id);
    url.query_pairs_mut()
        .append_pair("$select", "id,from,subject,receivedDateTime,body");
    let msg = api.get(url.as_str(), token)?;
    let content = msg["body"]["content"].as_str().unwrap_or("");
    let rich = s(&msg["body"], "contentType").is_some_and(|t| t.eq_ignore_ascii_case("html"));
    let cut: String = clip(content, DETAIL_MAX);
    let (plain, html) = if rich {
        ("", cut.as_str())
    } else {
        (cut.as_str(), "")
    };
    Ok(detail(
        id,
        &graph_from(&msg),
        s(&msg, "subject").unwrap_or(""),
        s(&msg, "receivedDateTime").and_then(parse_time),
        plain,
        html,
        content.len() > DETAIL_MAX,
    ))
}
fn mime_text(mail: &mailparse::ParsedMail<'_>, depth: usize, out: &mut String) {
    if depth > 8 || out.len() >= MAX_BODY {
        return;
    }
    if ["text/plain", "text/html"].contains(&mail.ctype.mimetype.as_str()) {
        if let Ok(body) = mail.get_body() {
            if body.len() <= MAX_BODY - out.len() {
                out.push_str(&body);
            }
        }
    }
    for part in mail.subparts.iter().take(30) {
        mime_text(part, depth + 1, out);
    }
}
/// 按顺序取出正文里的纯文本和 HTML（附件不要）。
fn mime_parts(
    mail: &mailparse::ParsedMail<'_>,
    depth: usize,
    plain: &mut String,
    html: &mut String,
) {
    if depth > 10 {
        return;
    }
    let attachment =
        mail.get_content_disposition().disposition == mailparse::DispositionType::Attachment;
    let mime = mail.ctype.mimetype.as_str();
    if !attachment && (mime == "text/plain" || mime == "text/html") {
        if let Ok(body) = mail.get_body() {
            let out = if mime == "text/html" {
                &mut *html
            } else {
                &mut *plain
            };
            if out.len() + body.len() <= DETAIL_MAX {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&body);
            }
        }
    }
    for part in mail.subparts.iter().take(50) {
        mime_parts(part, depth + 1, plain, html);
    }
}
fn imap_username(entry: &Value) -> R<&str> {
    entry
        .get("config")
        .and_then(|c| s(c, "imap_username"))
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .or_else(|| s(entry, "email").map(str::trim).filter(|u| !u.is_empty()))
        .ok_or_else(|| "请填写 IMAP 用户名".into())
}
struct ImapAccount<'a> {
    host: &'a str,
    user: &'a str,
    password: &'a str,
}
fn imap_connect(
    entry: &Value,
) -> R<(
    imap::Client<native_tls::TlsStream<TcpStream>>,
    ImapAccount<'_>,
)> {
    let config = &entry["config"];
    let host = s(config, "imap_host")
        .filter(|h| {
            !h.is_empty()
                && h.len() < 254
                && !h.chars().any(char::is_whitespace)
                && !h.contains('/')
        })
        .ok_or("请填写 IMAP 收信服务器；连接始终使用经过验证的 TLS")?;
    let port = config
        .get("imap_port")
        .and_then(Value::as_u64)
        .unwrap_or(993);
    if port == 0 || port > 65535 {
        return Err("IMAP TLS 端口无效".into());
    }
    let user = imap_username(entry)?;
    let password = s(&entry["secret"], "password")
        .ok_or("请先保存邮箱密码（有的邮箱要填授权码或应用专用密码）")?;
    let address = (host, port as u16)
        .to_socket_addrs()
        .map_err(|_| "无法解析 IMAP 主机名")?
        .next()
        .ok_or("IMAP 主机没有可用地址")?;
    let tcp = TcpStream::connect_timeout(&address, Duration::from_secs(10))
        .map_err(|_| "无法连接 IMAP TLS 服务器")?;
    tcp.set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|_| "无法设置 IMAP 超时")?;
    tcp.set_write_timeout(Some(Duration::from_secs(15)))
        .map_err(|_| "无法设置 IMAP 超时")?;
    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|_| "无法初始化 TLS")?
        .connect(host, tcp)
        .map_err(|_| "IMAP TLS 证书或主机名校验失败")?;
    Ok((
        imap::Client::new(tls),
        ImapAccount {
            host,
            user,
            password,
        },
    ))
}
fn imap_login<T: Read + Write>(
    mut client: imap::Client<T>,
    account: &ImapAccount<'_>,
) -> R<imap::Session<T>> {
    client
        .read_greeting()
        .map_err(|_| "IMAP 服务器未返回有效欢迎信息")?;
    client
        .login(account.user, account.password)
        .map_err(|(error, _)| {
            let text = error.to_string().to_lowercase();
            let host = account.host.to_ascii_lowercase();
            if (host.ends_with("office365.com") || host.ends_with("outlook.com"))
                && ["basicauthblocked", "authenticate failed", "login failed"]
                    .iter()
                    .any(|w| text.contains(w))
            {
                "微软不让这个号用密码收信：把收信方式改成“Outlook 授权”，再点“连接邮箱并授权”在微软页面登录一次".into()
            } else {
                "邮箱连接或登录失败：检查服务器、端口和密码（有的邮箱要填授权码或应用专用密码，并先在网页版设置里打开 IMAP）".into()
            }
        })
}
fn imap_messages(entry: &Value, since: DateTime<Utc>, everything: bool) -> R<Vec<Value>> {
    let (client, account) = imap_connect(entry)?;
    imap_session(client, &account, since, everything)
}
fn imap_session<T: Read + Write>(
    client: imap::Client<T>,
    account: &ImapAccount<'_>,
    since: DateTime<Utc>,
    everything: bool,
) -> R<Vec<Value>> {
    let mut session = imap_login(client, account)?;
    let inbox = session
        .examine("INBOX")
        .map_err(|_| "无法以只读模式打开 INBOX")?;
    let mut result = Vec::new();
    if inbox.exists > 0 {
        let sequence = format!(
            "{}:{}",
            inbox.exists.saturating_sub(19).max(1),
            inbox.exists
        );
        // 先只取最近 20 封的信头；官方邮件再按 UID 取正文找验证码
        let mut wanted = Vec::new();
        {
            let fetched = session
                .fetch(sequence, "(UID INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])")
                .map_err(|_| "无法只读获取邮件")?;
            for mail in fetched.iter().rev() {
                let Some(date) = mail.internal_date().map(|d| d.with_timezone(&Utc)) else {
                    continue;
                };
                let (from, subject) = mail
                    .header()
                    .and_then(|h| mailparse::parse_headers(h).ok())
                    .map(|(headers, _)| {
                        (
                            headers.get_first_value("From").unwrap_or_default(),
                            headers.get_first_value("Subject").unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default();
                let id = mail.uid.unwrap_or(mail.message);
                if let Some(item) = summary(
                    &id.to_string(),
                    &from,
                    &subject,
                    "",
                    date,
                    since,
                    everything,
                    None,
                ) {
                    let read = item["official"] == true
                        && mail.uid.is_some()
                        && mail.size.is_some_and(|n| n as usize <= MAX_BODY);
                    wanted.push((id, read, from, subject, date, item));
                }
            }
        }
        for (id, read, from, subject, date, mut item) in wanted {
            if read {
                if let Ok(full) = session.uid_fetch(id.to_string(), "BODY.PEEK[]<0.262145>") {
                    if let Some(parsed) = full
                        .iter()
                        .next()
                        .and_then(|m| m.body())
                        .filter(|b| b.len() <= MAX_BODY)
                        .and_then(|b| mailparse::parse_mail(b).ok())
                    {
                        let mut text = String::new();
                        mime_text(&parsed, 0, &mut text);
                        if let Some(better) = summary(
                            &id.to_string(),
                            &from,
                            &subject,
                            &text,
                            date,
                            since,
                            everything,
                            None,
                        ) {
                            item = better;
                        }
                    }
                }
            }
            result.push(item);
        }
    }
    let _ = session.logout();
    Ok(result)
}
fn imap_detail_session<T: Read + Write>(
    client: imap::Client<T>,
    account: &ImapAccount<'_>,
    uid: u32,
) -> R<Value> {
    let mut session = imap_login(client, account)?;
    session
        .examine("INBOX")
        .map_err(|_| "无法以只读模式打开 INBOX")?;
    let fetched = session
        .uid_fetch(
            uid.to_string(),
            format!("(UID INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.{DETAIL_MAX}>)"),
        )
        .map_err(|_| "无法只读获取这封邮件")?;
    let mail = fetched
        .iter()
        .find(|m| m.uid == Some(uid))
        .ok_or("这封邮件已经不在收件箱里了")?;
    let bytes = mail.body().ok_or("这封邮件已经不在收件箱里了")?;
    let parsed = mailparse::parse_mail(bytes).map_err(|_| "这封邮件的格式读不出来")?;
    let (mut plain, mut html) = (String::new(), String::new());
    mime_parts(&parsed, 0, &mut plain, &mut html);
    let value = detail(
        &uid.to_string(),
        &parsed.headers.get_first_value("From").unwrap_or_default(),
        &parsed
            .headers
            .get_first_value("Subject")
            .unwrap_or_default(),
        mail.internal_date().map(|d| d.with_timezone(&Utc)),
        &plain,
        &html,
        mail.size.is_some_and(|n| n as usize > DETAIL_MAX),
    );
    let _ = session.logout();
    Ok(value)
}
pub fn messages(store: &Store, id: &str, body: &Value) -> R<Value> {
    let everything = match body.get("all") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err("all 应为 true 或 false".into()),
    };
    let now = Utc::now();
    let since = match body.get("since") {
        None | Some(Value::Null) => now - chrono::Duration::minutes(15),
        Some(Value::String(t)) => parse_time(t).ok_or("since 应为 ISO8601 时间")?,
        _ => return Err("since 应为 ISO8601 时间".into()),
    };
    if since > now {
        return Err("since 应为过去的 ISO8601 时间".into());
    }
    if since < now - chrono::Duration::days(LOOKBACK_DAYS) - chrono::Duration::minutes(1) {
        return Err("只能读取最近 7 天的邮件，请缩短查询时间范围".into());
    }
    let _guard = entry_lock(store, id)?;
    let entry = load_entry(store, id)?;
    let mut items = match provider(&entry)? {
        "imap" => imap_messages(&entry, since, everything)?,
        "gmail" => {
            let token = access_token(&HttpApi, store, id, &entry)?;
            gmail_messages(&HttpApi, &token, since, everything)?
        }
        "outlook" => {
            let token = access_token(&HttpApi, store, id, &entry)?;
            outlook_messages(&HttpApi, &token, since, everything)?
        }
        _ => unreachable!(),
    };
    items.sort_by(|a, b| s(b, "received_at").cmp(&s(a, "received_at")));
    items.truncate(20);
    Ok(json!({
        "entry_id": id,
        "checked_at": now_iso(),
        "all": everything,
        "status": if items.is_empty() { "empty" } else { "found" },
        "messages": items,
    }))
}
/// 点开一封信：原邮件的 HTML（去掉脚本）和纯文本。只读，不标记已读。
pub fn message(store: &Store, id: &str, body: &Value) -> R<Value> {
    let message_id = s(body, "id")
        .filter(|m| {
            !m.is_empty()
                && m.len() <= 1024
                && m.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_=+/-".contains(&b))
        })
        .ok_or("邮件编号不正确")?;
    let _guard = entry_lock(store, id)?;
    let entry = load_entry(store, id)?;
    match provider(&entry)? {
        "imap" => {
            let uid = message_id
                .parse::<u32>()
                .ok()
                .filter(|_| message_id.bytes().all(|b| b.is_ascii_digit()))
                .ok_or("邮件编号不正确")?;
            let (client, account) = imap_connect(&entry)?;
            imap_detail_session(client, &account, uid)
        }
        "gmail" => {
            let token = access_token(&HttpApi, store, id, &entry)?;
            gmail_detail(&HttpApi, &token, message_id)
        }
        "outlook" => {
            let token = access_token(&HttpApi, store, id, &entry)?;
            outlook_detail(&HttpApi, &token, message_id)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FakeApi {
        owner: String,
    }
    impl MailApi for FakeApi {
        fn get(&self, url: &str, _: &str) -> R<Value> {
            if url.contains("profile") {
                Ok(json!({"emailAddress":self.owner}))
            } else {
                Ok(json!({"mail":self.owner}))
            }
        }
        fn form(&self, _: &str, _: &[(String, String)]) -> R<Value> {
            Ok(json!({"access_token":"test-token","expires_in":3600}))
        }
    }
    #[test]
    fn state_path_pkce_and_provider_validation() {
        let state = random_url();
        let verifier = random_url();
        assert_eq!(verifier.len(), 43);
        assert_eq!(
            URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes()))
                .len(),
            43
        );
        assert_eq!(
            callback_query(
                &format!("GET /oauth/callback?state={state}&code=abc HTTP/1.1"),
                &state
            )
            .unwrap(),
            Some("abc".into())
        );
        assert!(
            callback_query("GET /oauth/callback?state=wrong&code=abc HTTP/1.1", &state).is_err()
        );
        assert!(callback_query(
            &format!("GET /other?state={state}&code=abc HTTP/1.1"),
            &state
        )
        .is_err());
        assert!(endpoints(&json!({"provider":"outlook","config":{"tenant":"evil/path"}})).is_err());
        assert!(client_id(&json!({"config":{}})).is_err());
    }
    #[test]
    fn owner_mismatch_and_rotating_token_preservation() {
        let entry = json!({"provider":"gmail","email":"work@example.com"});
        assert!(verify_owner(
            &FakeApi {
                owner: "other@example.com".into()
            },
            &entry,
            "token"
        )
        .is_err());
        assert!(verify_owner(
            &FakeApi {
                owner: "WORK@example.com".into()
            },
            &entry,
            "token"
        )
        .is_ok());
        let new = token_secret(
            &json!({"refresh_token":"old"}),
            &json!({"access_token":"new","expires_in":3600}),
        )
        .unwrap();
        assert_eq!(new["refresh_token"], "old");
    }
    #[test]
    fn python_float_expiry_uses_existing_access_token() {
        let store = Store {
            root: std::env::temp_dir().join(format!("cam-mail-float-expiry-{}", new_id())),
        };
        let entry = json!({"provider":"gmail","email":"work@example.com","config":{"client_id":"fixture-client"},"secret":{"access_token":"existing","refresh_token":"old","expires_at":Utc::now().timestamp() as f64+3600.5}});
        assert_eq!(
            access_token(
                &FakeApi {
                    owner: "work@example.com".into()
                },
                &store,
                "fixture",
                &entry
            )
            .unwrap(),
            "existing"
        );
        assert!(!store.root.exists());
    }

    #[test]
    fn exchange_saves_only_after_mailbox_owner_matches() {
        let store = Store {
            root: std::env::temp_dir().join(format!("cam-mail-exchange-{}", new_id())),
        };
        vault::setup(&store, &json!({"master_password":"mailbox-test-master"})).unwrap();
        let status=vault::save_entry(&store,&json!({"email":"work@example.com","provider":"gmail","config":{"client_id":"fixture-client"}})).unwrap();
        let id = status["entries"][0]["id"].as_str().unwrap();
        let entry = vault::get_entry(&store, id).unwrap();
        assert!(exchange(
            &FakeApi {
                owner: "other@example.com".into()
            },
            &store,
            id,
            &entry,
            "fake-code",
            "http://127.0.0.1/callback",
            "fake-verifier"
        )
        .is_err());
        assert!(vault::get_entry(&store, id).unwrap()["secret"]
            .get("access_token")
            .is_none());
        exchange(
            &FakeApi {
                owner: "work@example.com".into(),
            },
            &store,
            id,
            &entry,
            "fake-code",
            "http://127.0.0.1/callback",
            "fake-verifier",
        )
        .unwrap();
        assert_eq!(
            vault::status(&store).unwrap()["entries"][0]["connected"],
            true
        );
        vault::lock(&store).unwrap();
        std::fs::remove_dir_all(store.root).unwrap();
    }
    #[test]
    fn oauth_status_is_bound_to_directory_and_reports_timeout() {
        let store = Store {
            root: std::env::temp_dir().join(format!("cam-mail-job-{}", new_id())),
        };
        let id = random_url();
        jobs().lock().unwrap().insert(
            id.clone(),
            Job {
                root: root(&store),
                entry: "fixture".into(),
                status: "pending".into(),
                message: "pending".into(),
                expires: Utc::now().timestamp() - 1,
            },
        );
        let status = oauth_status(&store, &id).unwrap();
        assert_eq!(status["status"], "error");
        assert!(status["message"].as_str().unwrap().contains("超过"));
        let other = Store {
            root: store.root.join("other"),
        };
        assert!(oauth_status(&other, &id).is_err());
        jobs().lock().unwrap().remove(&id);
        assert!(!store.root.exists());
    }

    #[test]
    fn imap_mock_session_uses_read_only_and_peek_commands() {
        use std::io::Cursor;
        use std::sync::Arc;
        struct MockStream {
            input: Cursor<Vec<u8>>,
            output: Arc<Mutex<Vec<u8>>>,
        }
        impl Read for MockStream {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.input.read(b)
            }
        }
        impl Write for MockStream {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.output.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let now = Utc::now();
        let mail="From: Claude <no-reply@claude.ai>\r\nSubject: Sign in to Claude\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nYour code is 123456\r\n";
        let head = "From: Claude <no-reply@claude.ai>\r\nSubject: Sign in to Claude\r\n\r\n";
        let other_head = "From: Friend <f@example.org>\r\nSubject: Hi\r\n\r\n";
        let date = now.format("%d-%b-%Y %H:%M:%S +0000");
        let server=format!("* OK ready\r\na1 OK login\r\n* 2 EXISTS\r\n* FLAGS (\\Seen)\r\na2 OK [READ-ONLY] examined\r\n* 1 FETCH (UID 8 INTERNALDATE \"{date}\" RFC822.SIZE 50 BODY[HEADER] {{{}}}\r\n{other_head})\r\n* 2 FETCH (UID 9 INTERNALDATE \"{date}\" RFC822.SIZE {} BODY[HEADER] {{{}}}\r\n{head})\r\na3 OK fetched\r\n* 2 FETCH (UID 9 BODY[]<0> {{{}}}\r\n{mail})\r\na4 OK fetched\r\n* BYE logout\r\na5 OK done\r\n",other_head.len(),mail.len(),head.len(),mail.len());
        for (configured, expected, everything) in [
            ("", "work@example.com", false),
            ("   ", "work@example.com", true),
            (" custom@example.com ", "custom@example.com", false),
        ] {
            let entry = json!({"email":"work@example.com","config":{"imap_username":configured}});
            let output = Arc::new(Mutex::new(Vec::new()));
            let client = imap::Client::new(MockStream {
                input: Cursor::new(server.as_bytes().to_vec()),
                output: output.clone(),
            });
            let account = ImapAccount {
                host: "imap.example.com",
                user: imap_username(&entry).unwrap(),
                password: "fake-password",
            };
            let messages = imap_session(
                client,
                &account,
                now - chrono::Duration::minutes(15),
                everything,
            )
            .unwrap();
            assert_eq!(messages.len(), if everything { 2 } else { 1 });
            assert_eq!(messages[0]["codes"], json!(["123456"]));
            assert_eq!(messages[0]["official"], true);
            if everything {
                assert_eq!(messages[1]["sender"], "f@example.org");
                assert_eq!(messages[1]["codes"], json!([]));
            }
            let sent = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            assert!(sent.contains(&format!("LOGIN \"{expected}\" \"fake-password\"")));
            assert!(sent.contains("EXAMINE \"INBOX\""));
            assert!(sent.contains("BODY.PEEK[HEADER]"));
            assert!(sent.contains("UID FETCH 9 BODY.PEEK[]<0.262145>"));
            assert!(!sent.contains("UID FETCH 8"));
            assert!(!sent.contains(" STORE "));
            assert!(!sent.contains(" SELECT "));
            assert!(!sent.contains(" EXPUNGE"));
        }
        // 点开一封：只读取正文，不标已读；HTML 里的脚本去掉
        let rich = "From: Friend <f@example.org>\r\nSubject: Hi\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--b\r\nContent-Type: text/html\r\n\r\n<b>rich</b><script>x()</script>\r\n--b--\r\n";
        let server = format!("* OK ready\r\na1 OK login\r\n* 1 EXISTS\r\na2 OK [READ-ONLY] examined\r\n* 1 FETCH (UID 7 INTERNALDATE \"{date}\" RFC822.SIZE {} BODY[]<0> {{{}}}\r\n{rich})\r\na3 OK fetched\r\n* BYE logout\r\na4 OK done\r\n", rich.len(), rich.len());
        let output = Arc::new(Mutex::new(Vec::new()));
        let client = imap::Client::new(MockStream {
            input: Cursor::new(server.into_bytes()),
            output: output.clone(),
        });
        let account = ImapAccount {
            host: "imap.example.com",
            user: "work@example.com",
            password: "fake-password",
        };
        let detail = imap_detail_session(client, &account, 7).unwrap();
        assert_eq!(detail["text"].as_str().unwrap().trim(), "plain body");
        assert!(detail["html"].as_str().unwrap().contains("<b>rich</b>"));
        assert!(!detail["html"].as_str().unwrap().contains("script"));
        assert_eq!(detail["official"], false);
        let sent = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(sent.contains("UID FETCH 7 (UID INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.2097152>)"));
        assert!(!sent.contains(" STORE "));
        // 微软拒绝密码登录时说清楚怎么办
        let refusing = "* OK ready\r\na1 NO AUTHENTICATE failed.\r\n";
        let client = imap::Client::new(MockStream {
            input: Cursor::new(refusing.as_bytes().to_vec()),
            output: Arc::new(Mutex::new(Vec::new())),
        });
        let outlook = ImapAccount {
            host: "outlook.office365.com",
            user: "a@outlook.com",
            password: "pw",
        };
        let error = imap_session(client, &outlook, now, false).unwrap_err();
        assert!(error.contains("Outlook 授权"), "{error}");
    }

    #[test]
    fn gmail_mock_only_reads_and_returns_filtered_summaries() {
        use std::cell::RefCell;
        struct ScriptApi {
            calls: RefCell<Vec<String>>,
            responses: RefCell<Vec<Value>>,
        }
        impl MailApi for ScriptApi {
            fn get(&self, url: &str, _: &str) -> R<Value> {
                self.calls.borrow_mut().push(url.into());
                Ok(self.responses.borrow_mut().remove(0))
            }
            fn form(&self, _: &str, _: &[(String, String)]) -> R<Value> {
                panic!("mail reading must not POST")
            }
        }
        let now = Utc::now();
        let api = ScriptApi {
            calls: RefCell::new(Vec::new()),
            responses: RefCell::new(vec![
                json!({"messages":[{"id":"abc123"}]}),
                json!({"internalDate":now.timestamp_millis().to_string(),"payload":{"mimeType":"text/plain","headers":[{"name":"From","value":"no-reply@anthropic.com"},{"name":"Subject","value":"Verification code"}],"body":{"data":URL_SAFE_NO_PAD.encode(b"Your code is 123456")}}}),
            ]),
        };
        let result = gmail_messages(
            &api,
            "fake-token",
            now - chrono::Duration::minutes(15),
            false,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].get("body").is_none());
        assert!(api.calls.borrow()[1].ends_with("?format=full"));
    }

    #[test]
    fn code_detection_ignores_urls_attributes_hidden_text_and_word_fragments() {
        let (codes,links)=extracts("<a id=\"87654321\" href=\"https://claude.ai/login?token=abc123456def\">Sign in</a> https://claude.ai/auth?token=12345678 <script>123456</script> <style>87654321</style> abc123456def _123456_ &#123456;");
        assert!(codes.is_empty());
        assert_eq!(links.len(), 2);
        assert_eq!(
            extracts("Your verification code is <strong>123456</strong>. 验证码：87654321").0,
            vec!["123456", "87654321"]
        );
        let now = Utc::now();
        let message = summary(
            "link-only",
            "Claude <no-reply@mail.anthropic.com>",
            "Secure link to log in to Claude.ai",
            "<a href=\"https://claude.ai/login?token=abc123456def\">Sign in</a>",
            now,
            now - chrono::Duration::minutes(15),
            false,
            None,
        )
        .unwrap();
        assert_eq!(message["codes"], json!([]));
        assert_eq!(message["verification_links"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn official_sender_links_codes_and_time_filtering() {
        let now = Utc::now();
        let since = now - chrono::Duration::minutes(15);
        let good=summary("1","Claude <no-reply@claude.ai>","Sign in to Claude","Your code is 123456. https://claude.ai/auth/verify?token=abc https://evil.example/auth/verify?token=bad",now,since,false,None).unwrap();
        assert_eq!(good["codes"], json!(["123456"]));
        assert_eq!(good["verification_links"].as_array().unwrap().len(), 1);
        assert_eq!(
            (
                good["official"].clone(),
                good["login"].clone(),
                good["sender_name"].clone()
            ),
            (json!(true), json!(true), json!("Claude"))
        );
        assert!(good.get("body").is_none());
        assert!(summary(
            "2",
            "fake@claude.ai.evil.example",
            "Sign in",
            "123456",
            now,
            since,
            false,
            None
        )
        .is_none());
        assert!(summary(
            "3",
            "a@anthropic.com",
            "Sign in",
            "123456",
            since - chrono::Duration::seconds(1),
            since,
            false,
            None
        )
        .is_none());
        // 官方的非登录邮件也列出来，只是没有验证码
        let invoice = summary(
            "4",
            "a@anthropic.com",
            "Invoice",
            "123456",
            now,
            since,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            (invoice["login"].clone(), invoice["codes"].clone()),
            (json!(false), json!([]))
        );
        // email.claude.com 发的欢迎信算官方；预览去掉样式和标签
        let welcome = summary(
            "5",
            "Claude Team <no-reply@email.claude.com>",
            "Welcome to Claude",
            "<style>p{color:#123456}</style><p>Let&#39;s get you set up.</p>",
            now,
            since,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            (welcome["official"].clone(), welcome["preview"].clone()),
            (json!(true), json!("Let's get you set up."))
        );
        // 全部邮件时别人发的也列出来，但不从里面找验证码
        let other = summary(
            "6",
            "fake@claude.ai.evil.example",
            "Sign in",
            "Your code is 123456",
            now,
            since,
            true,
            Some("hello"),
        )
        .unwrap();
        assert_eq!(
            (
                other["official"].clone(),
                other["codes"].clone(),
                other["preview"].clone()
            ),
            (json!(false), json!([]), json!("hello"))
        );
        assert!(summary(
            "7",
            "a@anthropic.com, b@evil.example",
            "Sign in",
            "123456",
            now,
            since,
            false,
            None
        )
        .is_none());
    }

    #[test]
    fn detail_html_drops_scripts_and_navigation() {
        let page = "<html><head><meta http-equiv=\"refresh\" content=\"0;url=https://evil.test\"><base href=\"https://evil.test/\"></head><body onload=\"x()\"><SCRIPT>steal()</SCRIPT><p style=\"color:red\">Sign in to Claude</p><a href=\"https://claude.ai/magic-link?token=t\">Sign in</a><iframe src=\"https://evil.test\"></iframe><form action=\"https://evil.test\"><input></form><img src=\"https://cdn.example/logo.png\">中文</body></html>";
        let d = detail(
            "AAMk-1=",
            "\"Anthropic\" <noreply@mail.anthropic.com>",
            "Your secure link to Claude",
            Some(Utc::now()),
            "",
            page,
            false,
        );
        let html = d["html"].as_str().unwrap();
        for bad in [
            "<SCRIPT",
            "steal()",
            "http-equiv",
            "<base",
            "<iframe",
            "<form",
        ] {
            assert!(!html.contains(bad), "{bad}");
        }
        assert!(
            html.contains("style=\"color:red\"")
                && html.contains("https://cdn.example/logo.png")
                && html.contains("中文")
        );
        assert_eq!(
            d["verification_links"],
            json!(["https://claude.ai/magic-link?token=t"])
        );
        assert_eq!(
            (
                d["official"].clone(),
                d["login"].clone(),
                d["sender_name"].clone()
            ),
            (json!(true), json!(true), json!("Anthropic"))
        );
        assert!(d["text"].as_str().unwrap().contains("Sign in to Claude"));
        assert!(!d["text"].as_str().unwrap().contains("steal"));
    }

    #[test]
    fn gmail_all_mail_reads_official_bodies_only_and_outlook_detail_encodes_id() {
        use std::cell::RefCell;
        struct UrlApi {
            calls: RefCell<Vec<String>>,
        }
        impl MailApi for UrlApi {
            fn get(&self, url: &str, _: &str) -> R<Value> {
                self.calls.borrow_mut().push(url.into());
                let now = Utc::now().timestamp_millis().to_string();
                let head = |from: &str| json!([{"name":"From","value":from},{"name":"Subject","value":"Sign in"}]);
                Ok(if url.contains("/messages?") {
                    json!({"messages":[{"id":"a1"},{"id":"b2"},{"id":"bad id"}]})
                } else if url.contains("/a1?format=metadata") {
                    json!({"sizeEstimate":100,"internalDate":now,"snippet":"code &amp; link","payload":{"headers":head("noreply@mail.anthropic.com")}})
                } else if url.contains("/a1?format=full") {
                    json!({"sizeEstimate":100,"internalDate":now,"payload":{"mimeType":"text/plain","headers":head("noreply@mail.anthropic.com"),"body":{"data":URL_SAFE_NO_PAD.encode(b"Your verification code is 123456")}}})
                } else if url.contains("/b2?format=metadata") {
                    json!({"sizeEstimate":100,"internalDate":now,"snippet":"Hello &#39;friend&#39;","payload":{"headers":head("Friend <f@example.org>")}})
                } else {
                    json!({"id":"x","from":{"emailAddress":{"name":"A, \"B\"","address":"noreply@mail.anthropic.com"}},"subject":"s","receivedDateTime":"2026-11-25T10:00:00Z","body":{"contentType":"html","content":"<p>hi</p><script>x</script>"}})
                })
            }
            fn form(&self, _: &str, _: &[(String, String)]) -> R<Value> {
                panic!("mail reading must not POST")
            }
        }
        let api = UrlApi {
            calls: RefCell::new(Vec::new()),
        };
        let since = Utc::now() - chrono::Duration::days(7);
        let found = gmail_messages(&api, "t", since, true).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["codes"], json!(["123456"]));
        assert_eq!(
            (found[1]["official"].clone(), found[1]["preview"].clone()),
            (json!(false), json!("Hello 'friend'"))
        );
        let calls = api.calls.borrow().clone();
        assert!(calls[0].contains("in%3Ainbox"));
        assert!(!calls.iter().any(|c| c.contains("/b2?format=full")));
        let d = outlook_detail(&api, "t", "AAMk/x+y=").unwrap();
        assert!(api
            .calls
            .borrow()
            .last()
            .unwrap()
            .contains("/me/messages/AAMk%2Fx+y="));
        assert_eq!(
            (d["official"].clone(), d["sender_name"].clone()),
            (json!(true), json!("A, \"B\""))
        );
        assert!(!d["html"].as_str().unwrap().contains("script"));
        assert!(gmail_detail(&api, "t", "../me").is_err());
    }

    #[test]
    fn oauth_client_id_is_shared_between_entries_of_the_same_kind() {
        let store = Store {
            root: std::env::temp_dir().join(format!("cam-mail-share-{}", new_id())),
        };
        vault::setup(&store, &json!({"master_password":"mailbox-test-master"})).unwrap();
        vault::save_entry(&store, &json!({"email":"a@outlook.com","provider":"outlook","config":{"client_id":"my-app","tenant":"consumers"}})).unwrap();
        vault::save_entry(
            &store,
            &json!({"email":"b@outlook.com","provider":"outlook","config":{}}),
        )
        .unwrap();
        let status = vault::save_entry(
            &store,
            &json!({"email":"c@gmail.com","provider":"gmail","config":{}}),
        )
        .unwrap();
        let ids: Vec<String> = status["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        let by_email = |email: &str| {
            status["entries"]
                .as_array()
                .unwrap()
                .iter()
                .position(|e| e["email"] == email)
                .unwrap()
        };
        let second = load_entry(&store, &ids[by_email("b@outlook.com")]).unwrap();
        assert_eq!(
            (
                second["config"]["client_id"].clone(),
                second["config"]["tenant"].clone()
            ),
            (json!("my-app"), json!("consumers"))
        );
        let gmail = load_entry(&store, &ids[by_email("c@gmail.com")]).unwrap();
        assert!(client_id(&gmail).is_err());
        assert!(
            vault::get_entry(&store, &ids[by_email("b@outlook.com")]).unwrap()["config"]
                .get("client_id")
                .is_none_or(|c| c == "")
        );
        vault::lock(&store).unwrap();
        std::fs::remove_dir_all(store.root).unwrap();
    }
}
