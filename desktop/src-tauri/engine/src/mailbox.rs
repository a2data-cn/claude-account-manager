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
        .take((2 * 1024 * 1024 + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取邮箱服务响应")?;
    if bytes.len() > 2 * 1024 * 1024 {
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
    let current = vault::get_entry(store, id)?;
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
    let current = vault::get_entry(store, id)?;
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
    let entry = vault::get_entry(store, id)?;
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
    ["anthropic.com", "claude.ai"]
        .iter()
        .any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}
fn sender_address(from: &str) -> Option<String> {
    let addresses = mailparse::addrparse(from).ok()?;
    if addresses.len() != 1 {
        return None;
    }
    match &addresses[0] {
        mailparse::MailAddr::Single(info)
            if info
                .addr
                .rsplit_once('@')
                .is_some_and(|(_, d)| allowed_domain(d)) =>
        {
            Some(info.addr.clone())
        }
        _ => None,
    }
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
fn summary(
    id: &str,
    from: &str,
    subject: &str,
    text: &str,
    received: DateTime<Utc>,
    since: DateTime<Utc>,
) -> Option<Value> {
    if received < since || received > Utc::now() + chrono::Duration::minutes(5) {
        return None;
    }
    let sender = sender_address(from)?;
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
        return None;
    }
    let (codes, links) = extracts(&content);
    if codes.is_empty() && links.is_empty() {
        return None;
    }
    Some(
        json!({"id":id,"sender":sender,"subject":subject.chars().take(240).collect::<String>(),"received_at":received.to_rfc3339(),"codes":codes,"verification_links":links}),
    )
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
fn gmail_messages(api: &impl MailApi, token: &str, since: DateTime<Utc>) -> R<Vec<Value>> {
    let mut list = Url::parse("https://gmail.googleapis.com/gmail/v1/users/me/messages").unwrap();
    list.query_pairs_mut()
        .append_pair("maxResults", "20")
        .append_pair(
            "q",
            &format!(
                "after:{} {{from:anthropic.com from:claude.ai}}",
                since.timestamp()
            ),
        );
    let ids = api.get(list.as_str(), token)?;
    let mut result = Vec::new();
    for item in ids
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(20)
    {
        let Some(id) = s(item, "id").filter(|id| {
            id.len() <= 200
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        }) else {
            continue;
        };
        let msg = api.get(
            &format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}?format=full"),
            token,
        )?;
        let Some(date) = s(&msg, "internalDate")
            .and_then(|t| t.parse::<i64>().ok())
            .and_then(DateTime::<Utc>::from_timestamp_millis)
        else {
            continue;
        };
        let payload = &msg["payload"];
        let headers = payload.get("headers").and_then(Value::as_array);
        let header = |name: &str| {
            headers
                .into_iter()
                .flatten()
                .find(|h| s(h, "name").is_some_and(|n| n.eq_ignore_ascii_case(name)))
                .and_then(|h| s(h, "value"))
                .unwrap_or("")
        };
        let mut text = String::new();
        gmail_text(payload, 0, &mut text);
        if let Some(s) = summary(id, header("From"), header("Subject"), &text, date, since) {
            result.push(s);
        }
    }
    Ok(result)
}
fn outlook_messages(api: &impl MailApi, token: &str, since: DateTime<Utc>) -> R<Vec<Value>> {
    let mut url =
        Url::parse("https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages").unwrap();
    url.query_pairs_mut()
        .append_pair("$top", "20")
        .append_pair("$select", "id,subject,from,receivedDateTime,body")
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
        let text = msg.get("body").and_then(|b| s(b, "content")).unwrap_or("");
        if text.len() > MAX_BODY {
            continue;
        }
        if let Some(item) = summary(
            s(msg, "id").unwrap_or(""),
            msg.get("from")
                .and_then(|f| f.get("emailAddress"))
                .and_then(|a| s(a, "address"))
                .unwrap_or(""),
            s(msg, "subject").unwrap_or(""),
            text,
            date,
            since,
        ) {
            result.push(item);
        }
    }
    Ok(result)
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
fn imap_username(entry: &Value) -> R<&str> {
    entry
        .get("config")
        .and_then(|c| s(c, "imap_username"))
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .or_else(|| s(entry, "email").map(str::trim).filter(|u| !u.is_empty()))
        .ok_or_else(|| "请填写 IMAP 用户名".into())
}

fn imap_messages(entry: &Value, since: DateTime<Utc>) -> R<Vec<Value>> {
    let config = &entry["config"];
    let host = s(config, "imap_host")
        .filter(|h| {
            !h.is_empty()
                && h.len() < 254
                && !h.chars().any(char::is_whitespace)
                && !h.contains('/')
        })
        .ok_or("请配置企业 IMAP TLS 主机名和应用密码")?;
    let port = config
        .get("imap_port")
        .and_then(Value::as_u64)
        .unwrap_or(993);
    if port == 0 || port > 65535 {
        return Err("IMAP TLS 端口无效".into());
    }
    let user = imap_username(entry)?;
    let password = s(&entry["secret"], "password").ok_or("请填写企业邮箱应用密码")?;
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
    imap_session(imap::Client::new(tls), user, password, since)
}
fn imap_session<T: Read + Write>(
    mut client: imap::Client<T>,
    user: &str,
    password: &str,
    since: DateTime<Utc>,
) -> R<Vec<Value>> {
    client
        .read_greeting()
        .map_err(|_| "IMAP 服务器未返回有效欢迎信息")?;
    let mut session = client
        .login(user, password)
        .map_err(|_| "IMAP 登录失败，请检查应用密码或企业邮箱策略")?;
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
        let fetched = session
            .fetch(
                sequence,
                "(UID INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.262145>)",
            )
            .map_err(|_| "无法只读获取 IMAP 邮件")?;
        for mail in fetched.iter().rev() {
            if mail.size.is_some_and(|n| n as usize > MAX_BODY) {
                continue;
            }
            let Some(bytes) = mail.body().filter(|b| b.len() <= MAX_BODY) else {
                continue;
            };
            let Ok(parsed) = mailparse::parse_mail(bytes) else {
                continue;
            };
            let Some(date) = mail.internal_date().map(|d| d.with_timezone(&Utc)) else {
                continue;
            };
            let from = parsed.headers.get_first_value("From").unwrap_or_default();
            let subject = parsed
                .headers
                .get_first_value("Subject")
                .unwrap_or_default();
            let mut text = String::new();
            mime_text(&parsed, 0, &mut text);
            if let Some(item) = summary(
                &mail.uid.unwrap_or(mail.message).to_string(),
                &from,
                &subject,
                &text,
                date,
                since,
            ) {
                result.push(item);
            }
        }
    }
    let _ = session.logout();
    Ok(result)
}
pub fn messages(store: &Store, id: &str, body: &Value) -> R<Value> {
    let _guard = entry_lock(store, id)?;
    let entry = vault::get_entry(store, id)?;
    let now = Utc::now();
    let since = match body.get("since") {
        None | Some(Value::Null) => now - chrono::Duration::minutes(15),
        Some(Value::String(t)) => parse_time(t).ok_or("since 应为 ISO8601 时间")?,
        _ => return Err("since 应为 ISO8601 时间".into()),
    };
    if since < now - chrono::Duration::hours(1) || since > now {
        return Err("仅允许读取最近一小时内的邮件".into());
    }
    let mut items = match provider(&entry)? {
        "imap" => imap_messages(&entry, since)?,
        "gmail" => {
            let token = access_token(&HttpApi, store, id, &entry)?;
            gmail_messages(&HttpApi, &token, since)?
        }
        "outlook" => {
            let token = access_token(&HttpApi, store, id, &entry)?;
            outlook_messages(&HttpApi, &token, since)?
        }
        _ => unreachable!(),
    };
    items.sort_by(|a, b| s(b, "received_at").cmp(&s(a, "received_at")));
    items.truncate(20);
    Ok(
        json!({"entry_id":id,"checked_at":now_iso(),"status":if items.is_empty(){"empty"}else{"found"},"messages":items}),
    )
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
        let server=format!("* OK ready\r\na1 OK login\r\n* 1 EXISTS\r\n* FLAGS (\\Seen)\r\na2 OK [READ-ONLY] examined\r\n* 1 FETCH (UID 9 INTERNALDATE \"{}\" RFC822.SIZE {} BODY[] {{{}}}\r\n{})\r\na3 OK fetched\r\n* BYE logout\r\na4 OK done\r\n",now.format("%d-%b-%Y %H:%M:%S +0000"),mail.len(),mail.len(),mail);
        for (configured, expected) in [
            ("", "work@example.com"),
            ("   ", "work@example.com"),
            (" custom@example.com ", "custom@example.com"),
        ] {
            let entry = json!({"email":"work@example.com","config":{"imap_username":configured}});
            let output = Arc::new(Mutex::new(Vec::new()));
            let client = imap::Client::new(MockStream {
                input: Cursor::new(server.as_bytes().to_vec()),
                output: output.clone(),
            });
            let messages = imap_session(
                client,
                imap_username(&entry).unwrap(),
                "fake-password",
                now - chrono::Duration::minutes(15),
            )
            .unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["codes"], json!(["123456"]));
            let sent = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            assert!(sent.contains(&format!("LOGIN \"{expected}\" \"fake-password\"")));
            assert!(sent.contains("EXAMINE \"INBOX\""));
            assert!(sent.contains("BODY.PEEK[]<0.262145>"));
            assert!(!sent.contains(" STORE "));
            assert!(!sent.contains(" SELECT "));
            assert!(!sent.contains(" EXPUNGE"));
        }
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
        let result =
            gmail_messages(&api, "fake-token", now - chrono::Duration::minutes(15)).unwrap();
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
        )
        .unwrap();
        assert_eq!(message["codes"], json!([]));
        assert_eq!(message["verification_links"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn official_sender_links_codes_and_time_filtering() {
        let now = Utc::now();
        let since = now - chrono::Duration::minutes(15);
        let good=summary("1","Claude <no-reply@claude.ai>","Sign in to Claude","Your code is 123456. https://claude.ai/auth/verify?token=abc https://evil.example/auth/verify?token=bad",now,since).unwrap();
        assert_eq!(good["codes"], json!(["123456"]));
        assert_eq!(good["verification_links"].as_array().unwrap().len(), 1);
        assert!(good.get("body").is_none());
        assert!(summary(
            "2",
            "fake@claude.ai.evil.example",
            "Sign in",
            "123456",
            now,
            since
        )
        .is_none());
        assert!(summary(
            "3",
            "a@anthropic.com",
            "Sign in",
            "123456",
            since - chrono::Duration::seconds(1),
            since
        )
        .is_none());
        assert!(summary("4", "a@anthropic.com", "Invoice", "123456", now, since).is_none());
    }
}
