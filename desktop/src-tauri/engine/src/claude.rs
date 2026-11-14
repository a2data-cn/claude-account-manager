//! Claude Code 凭据的读取、快照、切换。逻辑与 cam.py 一一对应。

use super::store::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

pub fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"))
}

pub fn credentials_file() -> PathBuf {
    claude_config_dir().join(".credentials.json")
}

pub fn claude_json_file() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => PathBuf::from(d).join(".claude.json"),
        None => home_dir().join(".claude.json"),
    }
}

fn use_keychain() -> bool {
    cfg!(target_os = "macos")
        && std::env::var("CAM_NO_KEYCHAIN").ok().as_deref() != Some("1")
        && std::env::var_os("CLAUDE_CONFIG_DIR").is_none()
}

// ---------------------------------------------------------------- macOS 钥匙串

fn keychain_read() -> Option<Value> {
    let user = whoami::username();
    let try_cmd = |with_user: bool| -> Option<String> {
        let mut c = Command::new("security");
        c.args(["find-generic-password", "-s", KEYCHAIN_SERVICE]);
        if with_user {
            c.args(["-a", &user]);
        }
        c.arg("-w");
        let out = c.output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let raw = try_cmd(true).or_else(|| try_cmd(false))?;
    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
        return Some(v);
    }
    hex::decode(&raw)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn keychain_write(v: &Value) -> R<()> {
    let payload = hex::encode(serde_json::to_vec(v).map_err(|e| e.to_string())?);
    let user = whoami::username().replace('"', "");
    // 走 stdin，密钥不会出现在进程参数里
    let cmd = format!(
        "add-generic-password -U -a \"{}\" -s \"{}\" -X {}\n",
        user, KEYCHAIN_SERVICE, payload
    );
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法调用 security：{}", e))?;
    child
        .stdin
        .take()
        .ok_or("stdin 不可用")?
        .write_all(cmd.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if !out.status.success() || err.contains("error") {
        return Err(format!("写入钥匙串失败：{}", err.trim()));
    }
    Ok(())
}

// ---------------------------------------------------------------- 当前生效的凭据

/// 凭据文件损坏时报错，不当成“没登录”：否则切换时不会备份它，直接就覆盖了。与 cam.py 一致。
pub fn read_live() -> R<(Option<Value>, Option<&'static str>)> {
    if use_keychain() {
        if let Some(v) = keychain_read() {
            return Ok((Some(v), Some("keychain")));
        }
    }
    Ok(match read_json(&credentials_file())? {
        Some(v) => (Some(v), Some("file")),
        None => (None, None),
    })
}

pub fn write_live(creds: &Value, where_hint: Option<&str>) -> R<()> {
    let target = where_hint.unwrap_or(if use_keychain() { "keychain" } else { "file" });
    if target == "keychain" {
        keychain_write(creds)?;
        if credentials_file().exists() {
            write_json_atomic(&credentials_file(), creds, true)?;
        }
        Ok(())
    } else {
        write_json_atomic(&credentials_file(), creds, true)
    }
}

/// ~/.claude.json 损坏时报错，不当成“没有 oauthAccount”继续往下切。
pub fn read_oauth_account() -> R<Value> {
    Ok(read_json(&claude_json_file())?
        .and_then(|v| v.get("oauthAccount").cloned())
        .unwrap_or(Value::Null))
}

/// 只替换 oauthAccount，~/.claude.json 其它字段原样保留；Claude Code 同时在写也不会丢它的改动。
pub fn write_oauth_account(oa: &Value) -> R<()> {
    let want = oa.clone();
    update_json_file(
        &claude_json_file(),
        |obj| {
            if oa.is_null() {
                obj.remove("oauthAccount");
            } else {
                obj.insert("oauthAccount".into(), oa.clone());
            }
        },
        |now| now.get("oauthAccount").cloned().unwrap_or(Value::Null) == want,
    )
}

pub fn oauth_block(creds: &Value) -> Value {
    creds
        .get("claudeAiOauth")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

pub fn token_fp(rt: &str) -> String {
    let mut h = Sha256::new();
    h.update(rt.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

fn code_indices(data: &Value) -> Vec<usize> {
    accounts(data)
        .iter()
        .enumerate()
        .filter(|(_, a)| s(a, "kind") == Some("claude_code"))
        .map(|(i, _)| i)
        .collect()
}

/// 按 accountUuid、邮箱在 Claude Code 账号里找人。
pub fn match_identity(data: &Value, uuid: Option<&str>, email: Option<&str>) -> Option<usize> {
    let list = code_indices(data);
    let accs = accounts(data);
    if let Some(u) = uuid.filter(|u| !u.is_empty()) {
        if let Some(i) = list
            .iter()
            .find(|&&i| s(&accs[i], "account_uuid") == Some(u))
        {
            return Some(*i);
        }
    }
    let e = email.filter(|e| !e.is_empty())?.to_lowercase();
    list.into_iter()
        .find(|&i| s(&accs[i], "email").map(|x| x.to_lowercase()) == Some(e.clone()))
}

/// 账号已知的身份和给出的身份是否一致；任何一方缺信息时视为一致。
pub fn identity_agrees(acc: &Value, uuid: Option<&str>, email: Option<&str>) -> bool {
    let uuid = uuid.filter(|x| !x.is_empty());
    let email = email.filter(|x| !x.is_empty());
    if let (Some(a), Some(b)) = (s(acc, "account_uuid"), uuid) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (s(acc, "email"), email) {
        return a.to_lowercase() == b.to_lowercase();
    }
    true
}

/// 找出当前登录属于哪个已保存账号。
/// 先比 refresh token 指纹：token 本身就是凭据，比 ~/.claude.json 里的 oauthAccount 可靠
/// （oauthAccount 可能和凭据对不上，比如切换前打开的 claude 会话刷新后把旧账号的 token 写回了）。
/// token 换过（刷新或重新登录）时才退回到 accountUuid / 邮箱。
pub fn identify(data: &Value, oa: &Value, creds: &Value) -> Option<usize> {
    if let Some(rt) = s(&oauth_block(creds), "refreshToken") {
        let fp = token_fp(rt);
        let accs = accounts(data);
        if let Some(i) = code_indices(data)
            .into_iter()
            .find(|&i| s(&accs[i], "token_fp") == Some(fp.as_str()))
        {
            return Some(i);
        }
    }
    match_identity(data, s(oa, "accountUuid"), s(oa, "emailAddress"))
}

/// 用 /api/oauth/profile 问出这个 access token 真正属于谁。返回 (HTTP 状态, (uuid, email))。
/// 状态 0 表示根本没连上（离线），和服务端明确说“不认识这个 token”不是一回事：
/// 前者只是暂时核对不了，后者是答案。
/// 一份登录的主人：(uuid, email)，两样至少有一样。
pub type Owner = (Option<String>, Option<String>);

pub fn probe_token_owner(access_token: &str) -> (u16, Option<Owner>) {
    let (code, prof) = super::usage::get_oauth(access_token, "/api/oauth/profile", 6);
    if code != 200 {
        return (code, None);
    }
    let Some(a) = prof.get("account") else {
        return (code, None);
    };
    let uuid = s(a, "uuid").map(String::from);
    let email = s(a, "email").map(String::from);
    if uuid.is_none() && email.is_none() {
        return (code, None);
    }
    (code, Some((uuid, email)))
}

pub fn token_owner(access_token: &str) -> Option<Owner> {
    probe_token_owner(access_token).1
}

fn apply_meta(acc: &mut Value, oa: &Value, creds: &Value) {
    let ob = oauth_block(creds);
    if let Some(v) = s(oa, "accountUuid") {
        acc["account_uuid"] = json!(v);
    }
    if let Some(v) = s(oa, "emailAddress") {
        acc["email"] = json!(v);
    }
    if let Some(v) = s(oa, "organizationName") {
        acc["org_name"] = json!(v);
    }
    if let Some(v) = s(&ob, "subscriptionType") {
        acc["plan"] = json!(v);
    }
    if let Some(v) = s(&ob, "refreshToken") {
        acc["token_fp"] = json!(token_fp(v));
    }
}

/// secrets 目录里暂存“还没认出主人”的那份登录；账号 id 是 8 位十六进制，不会撞名。与 cam.py 一致。
pub const PENDING_ID: &str = "_pending-live";
const PENDING_MAX_DAYS: i64 = 7;

/// access token 本地看还没过期。过期了 /api/oauth/profile 必然 401，不必白跑一趟。
fn access_token_fresh(ob: &Value) -> bool {
    ob.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0)
        > chrono::Utc::now().timestamp_millis()
}

/// 时间戳 a 晚于 b（两边后端写的格式可能略有差异，解析后再比）。
fn newer(a: Option<&str>, b: Option<&str>) -> bool {
    match (a.and_then(parse_time), b.and_then(parse_time)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

fn is_empty_obj(v: &Value) -> bool {
    v.as_object().map(|o| o.is_empty()).unwrap_or(true)
}

/// 把一份已确认属于 accounts[idx] 的登录存进它名下。oauth_account 只在它描述的就是这个账号时传入。
/// saved_at 记的是这个 refresh token 是什么时候拿到的：token 没换就沿用原来的时间，
/// 这样才能和暂存的登录比谁更新（见 file_pending）。
fn file_credentials(
    store: &Store,
    data: &mut Value,
    idx: usize,
    ob: &Value,
    creds: &Value,
    where_: Option<&str>,
    oauth_account: Option<&Value>,
) -> R<()> {
    let id = s(&accounts(data)[idx], "id")
        .unwrap_or_default()
        .to_string();
    let prev = store.load_secret(&id);
    let same_rt =
        prev.get("claudeAiOauth").and_then(|o| s(o, "refreshToken")) == s(ob, "refreshToken");
    let saved_oa = match oauth_account {
        Some(oa) => oa.clone(),
        None => prev.get("oauthAccount").cloned().unwrap_or(Value::Null),
    };
    let saved_at = match s(&prev, "saved_at") {
        Some(t) if same_rt => t.to_string(),
        _ => now_iso(),
    };
    apply_meta(
        &mut accounts_mut(data)[idx],
        oauth_account.unwrap_or(&Value::Null),
        creds,
    );
    store.save_secret(
        &id,
        &json!({"claudeAiOauth": ob, "oauthAccount": saved_oa, "raw": creds, "store": where_, "saved_at": saved_at}),
    )
}

/// 认不出这份登录是谁的（离线，或 access token 已过期而 refresh token 又换过）时调用：
/// 原样暂存，不往任何账号里写。~/.claude.json 里的名字可能是旧会话写回时留下的别人的名字，
/// 信了就会把 A 的 token 存进 B。能核对时再归档（resolve_pending / adopt_pending_for）。
fn park_unverified(
    store: &Store,
    data: &mut Value,
    candidate: Option<usize>,
    ob: &Value,
    creds: &Value,
    where_: Option<&str>,
    oa: &Value,
) -> R<()> {
    let prev = store.load_secret(PENDING_ID);
    let same_rt =
        prev.get("claudeAiOauth").and_then(|o| s(o, "refreshToken")) == s(ob, "refreshToken");
    let at = match s(&prev, "at") {
        Some(t) if same_rt => t.to_string(),
        _ => now_iso(),
    };
    let (cid, label) = match candidate {
        Some(i) => (
            s(&accounts(data)[i], "id").map(String::from),
            s(&accounts(data)[i], "label").map(String::from),
        ),
        None => (None, None),
    };
    store.save_secret(
        PENDING_ID,
        &json!({
            "claudeAiOauth": ob, "oauthAccount": if is_empty_obj(oa) { Value::Null } else { oa.clone() },
            "raw": creds, "store": where_, "candidate": cid, "at": at
        }),
    )?;
    let who = label
        .map(|l| format!("可能是“{}”，", l))
        .unwrap_or_default();
    data["live_unverified"] = json!({
        "at": at, "candidate": cid,
        "message": format!(
            "Claude Code 当前的登录暂时无法确认属于哪个账号（{}离线或登录已过期，核对不了），这次没有保存。启动一次 claude 或联网后会自动处理。",
            who
        )
    });
    Ok(())
}

fn pending_is_stale(p: &Value) -> bool {
    match s(p, "at").and_then(parse_time) {
        Some(at) => {
            chrono::Utc::now().signed_duration_since(at) > chrono::Duration::days(PENDING_MAX_DAYS)
        }
        None => true,
    }
}

/// 暂存的登录已核对出主人：归到它名下；账号里已有更新的就不覆盖；没有这个账号就新建。
fn file_pending(store: &Store, data: &mut Value, p: &Value, ob: &Value, owner: &Owner) -> R<usize> {
    let (ou, oe) = (owner.0.as_deref(), owner.1.as_deref());
    let idx = match match_identity(data, ou, oe) {
        Some(i) => {
            let id = s(&accounts(data)[i], "id").unwrap_or_default().to_string();
            let sec = store.load_secret(&id);
            if newer(s(&sec, "saved_at"), s(p, "at")) {
                return Ok(i);
            }
            i
        }
        None => {
            accounts_mut(data).push(json!({
                "id": new_id(), "kind": "claude_code", "label": oe.unwrap_or("未命名账号"), "email": oe,
                "plan": null, "org_name": null, "account_uuid": ou, "manual": {}, "usage": null,
                "created_at": now_iso(), "last_switched_at": now_iso()
            }));
            accounts(data).len() - 1
        }
    };
    let oa = p.get("oauthAccount").cloned().unwrap_or(Value::Null);
    let oa_ok = !is_empty_obj(&oa)
        && identity_agrees(
            &accounts(data)[idx],
            s(&oa, "accountUuid"),
            s(&oa, "emailAddress"),
        );
    let creds = p.get("raw").cloned().unwrap_or(Value::Null);
    file_credentials(
        store,
        data,
        idx,
        ob,
        &creds,
        s(p, "store"),
        if oa_ok { Some(&oa) } else { None },
    )?;
    Ok(idx)
}

/// 处理之前暂存的登录：已有账号存着同一个 token、或放了太久就丢掉；能联网核对了就归档。
fn resolve_pending(store: &Store, data: &mut Value, live_fp: Option<&str>) -> R<()> {
    let mut p = store.load_secret(PENDING_ID);
    if is_empty_obj(&p) {
        return Ok(());
    }
    let ob = p.get("claudeAiOauth").cloned().unwrap_or_else(|| json!({}));
    let fp = token_fp(s(&ob, "refreshToken").unwrap_or(""));
    if live_fp == Some(fp.as_str()) {
        let was_released = p
            .as_object_mut()
            .and_then(|o| o.remove("released_at"))
            .is_some_and(|v| v.as_str().is_some_and(|t| !t.is_empty()));
        if was_released {
            store.save_secret(PENDING_ID, &p)?; // 又成了当前登录（旧会话写回来的）
        }
        return Ok(()); // 就是当前生效的这份登录，走 snapshot_live 的正常流程
    }
    let filed = code_indices(data)
        .into_iter()
        .any(|i| s(&accounts(data)[i], "token_fp") == Some(fp.as_str()));
    if pending_is_stale(&p) || filed {
        store.delete_secret(PENDING_ID);
        return Ok(());
    }
    if live_fp.is_some() {
        mark_released(store, &mut p)?;
    }
    if !access_token_fresh(&ob) {
        return Ok(()); // 核对要 access token 还有效；切换到候选账号时会用 refresh token 换一个再核对
    }
    let (code, owner) = probe_token_owner(s(&ob, "accessToken").unwrap_or(""));
    if let Some(o) = owner {
        file_pending(store, data, &p, &ob, &o)?;
        store.delete_secret(PENDING_ID);
    } else if code == 401 && s(&ob, "refreshToken").is_some() {
        // 服务端不认这个 access token 了（比本地算的过期得早，或者已经被换掉）。refresh token 也许还能用：
        // 记成已过期留着，切换到它、手动刷新时用 refresh token 换一个再核对（见 adopt_pending_for）
        let mut ob = ob;
        ob["expiresAt"] = json!(0);
        if p.get("raw").is_some_and(Value::is_object) {
            p["raw"]["claudeAiOauth"] = ob.clone();
        }
        p["claudeAiOauth"] = ob;
        store.save_secret(PENDING_ID, &p)?;
    } else if code == 401 {
        // 没有 refresh token，access token 又不认了，留着也没用；限流、5xx 只是这次核对不了，留着下次再问
        store.delete_secret(PENDING_ID);
    }
    Ok(())
}

/// p 是暂存的那份登录：还没记过 released_at 就记上并存回去。
fn mark_released(store: &Store, p: &mut Value) -> R<()> {
    if is_empty_obj(p) || s(p, "released_at").is_some() {
        return Ok(());
    }
    p["released_at"] = json!(now_iso());
    store.save_secret(PENDING_ID, p)
}

/// 记下暂存的登录从现在起不是当前登录了：这之后才启动的 claude 会话读到的是别的登录（见 pending_holders）。
pub(crate) fn release_pending(store: &Store) -> R<()> {
    mark_released(store, &mut store.load_secret(PENDING_ID))
}

/// ps 的 etime（[[天-]时:]分:秒）换成秒；看不懂返回 None。与 cam.py 的 _etime_secs 一致。
pub fn etime_secs(text: &str) -> Option<i64> {
    let (days, rest) = text.rsplit_once('-').unwrap_or(("", text));
    let mut total = if days.is_empty() {
        0
    } else {
        days.trim().parse::<i64>().ok()? * 24
    };
    let parts: Vec<i64> = rest
        .split(':')
        .map(|x| x.trim().parse::<i64>())
        .collect::<Result<_, _>>()
        .ok()?;
    let (mins, secs) = match parts[..] {
        [h, m, sec] => {
            total += h;
            (m, sec)
        }
        [m, sec] => (m, sec),
        _ => return None,
    };
    Some(total * 3600 + mins * 60 + secs)
}

/// 会话自己带登录的环境变量：只看有没有，值不读出来。与 cam.py 的 SESSION_SECRET_ENV 一致
const SESSION_OWN_TOKEN_ENV: &[u8] = b"CLAUDE_CODE_OAUTH_TOKEN";
const SESSION_API_KEY_ENV: [&[u8]; 2] = [b"ANTHROPIC_API_KEY", b"ANTHROPIC_AUTH_TOKEN"];

/// 一个 claude 会话用的是哪个登录（/proc/<pid>/environ 的内容，读不到传 None）：own_token（自带 token）、api_key、
/// config_dir（别的配置目录）、machine（本机登录）、unknown。环境变量只看有没有，值当场丢掉；CLAUDE_CONFIG_DIR 的值
/// 只用来和本机的配置目录（mine，已经解析过符号链接）比。与 cam.py 的 claude_sessions 里的判断一致。
fn session_login(environ: Option<&[u8]>, mine: &Path) -> &'static str {
    let Some(raw) = environ else {
        return "unknown";
    };
    let (mut own, mut key, mut dir) = (false, false, None);
    for kv in raw.split(|b| *b == 0) {
        let (k, v) = match kv.iter().position(|b| *b == b'=') {
            Some(i) => (&kv[..i], &kv[i + 1..]),
            None => (kv, &kv[kv.len()..]),
        };
        if k == SESSION_OWN_TOKEN_ENV {
            own |= !v.is_empty();
        } else if SESSION_API_KEY_ENV.contains(&k) {
            key |= !v.is_empty();
        } else if k == b"CLAUDE_CONFIG_DIR" {
            dir = Some(String::from_utf8_lossy(v).into_owned());
        }
    }
    let elsewhere = dir.filter(|d| !d.is_empty()).is_some_and(|d| {
        let p = match d.strip_prefix('~') {
            Some("") => home_dir(),
            Some(rest) if rest.starts_with('/') => home_dir().join(&rest[1..]),
            _ => PathBuf::from(&d),
        };
        real_path(p) != mine
    });
    if own {
        "own_token"
    } else if key {
        "api_key"
    } else if elsewhere {
        "config_dir"
    } else {
        "machine"
    }
}

/// 解析掉符号链接的路径；解析不了（不存在）就原样
fn real_path(p: PathBuf) -> PathBuf {
    fs::canonicalize(&p).unwrap_or(p)
}

/// /proc/<pid>/stat 里进程是开机后多少秒启动的：第 22 项，单位是时钟滴答；进程名里可能有空格，从最后一个 ")" 后面数。
fn proc_start_secs(stat: &str) -> Option<f64> {
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let ticks = rest.split_whitespace().nth(19)?.parse::<u64>().ok()?;
    Some(ticks as f64 / PROC_TICKS_PER_SEC)
}

/// 这些 claude 进程各自 (已经运行了多少秒, 用的哪个登录)，看不出来的是 None。Linux 读 /proc，登录方式也看得出；
/// macOS 用 ps 的 etime，只有运行时长；Windows 什么都看不出。与 cam.py 的 claude_sessions / _ps_etimes 对应。
fn session_ages(pids: &[u32]) -> HashMap<u32, (Option<f64>, Option<String>)> {
    if let Some(fake) = fake_sessions() {
        return fake
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.pid, (f.secs, f.login)))
            .collect();
    }
    let mut out = HashMap::new();
    if cfg!(target_os = "windows") || pids.is_empty() {
        return out;
    }
    if Path::new("/proc/self").exists() {
        let uptime = fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok());
        let mine = real_path(claude_config_dir());
        for &pid in pids {
            let dir = Path::new("/proc").join(pid.to_string());
            let login = session_login(fs::read(dir.join("environ")).ok().as_deref(), &mine);
            let started = fs::read(dir.join("stat"))
                .ok()
                .and_then(|raw| proc_start_secs(&String::from_utf8_lossy(&raw)));
            let secs = uptime.zip(started).map(|(up, st)| (up - st).max(0.0));
            out.insert(pid, (secs, Some(login.to_string())));
        }
    } else {
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        let mut c = Command::new("ps");
        c.args(["-o", "pid=,etime=", "-p", &list.join(",")]);
        for line in super::health::run_with_timeout(c, Duration::from_secs(10)).lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if let [pid, etime] = f[..] {
                if let Ok(pid) = pid.parse::<u32>() {
                    out.insert(pid, (etime_secs(etime).map(|x| x as f64), None));
                }
            }
        }
    }
    out
}

/// 可能还拿着暂存的这份登录的 claude 进程有几个；进程没查出来时返回 None（当作有）。
/// 替它换 token 之前问这个：还有会话拿着它的话，两边各换一次 refresh token 会互相顶掉。
/// 它不再是当前登录之后（released_at）才启动的会话读到的是别的登录，不算；Linux 上还看得出会话用的不是本机登录
/// （自带 token、API Key、别的配置目录），也不算。启动时间看不出来的（Windows）都算。与 cam.py 一致。
pub fn pending_holders(p: &Value) -> Option<usize> {
    let procs = list_claude_processes()?;
    let Some(released) = s(p, "released_at").and_then(parse_time) else {
        return Some(procs.len());
    };
    if procs.is_empty() {
        return Some(0);
    }
    // 运行了不到这么久的，是它让位之后才启动的
    let since = chrono::Utc::now()
        .signed_duration_since(released)
        .num_milliseconds() as f64
        / 1000.0
        - 5.0;
    let pids: Vec<u32> = procs.iter().map(|(pid, _)| *pid).collect();
    let info = session_ages(&pids);
    let holds = |pid: &u32| match info.get(pid) {
        Some((secs, login)) => {
            !matches!(
                login.as_deref(),
                Some("own_token" | "api_key" | "config_dir")
            ) && !secs.is_some_and(|age| age < since)
        }
        None => true,
    };
    Some(pids.iter().filter(|pid| holds(pid)).count())
}

/// 报错里带这句的归成 pending_login（见 api::health_kind_of_error），不是登录失效。与 cam.py 的 PENDING_HELD 一致
pub const PENDING_HELD: &str = "登录还暂存着没核对";

/// 暂存的登录这次为什么没核对成（adopt_pending_for 的返回值），换成给人看的话。
fn pending_why(reason: &str, switching: bool) -> String {
    match (reason, switching) {
        ("sessions", true) => {
            "有 claude 会话可能还在用它。等这些会话退出后再切换，或者切换时选“现在就切”".into()
        }
        ("sessions", false) => {
            "有 claude 会话可能还在用它，等它们退出后再刷新，或者直接切换到这个账号".into()
        }
        ("live", true) => {
            "它就是 Claude Code 现在用的登录。在 claude 里发一句话让它续上，再切换".into()
        }
        ("live", false) => "它就是 Claude Code 现在用的登录，在 claude 里发一句话它就会续上".into(),
        _ => format!("这次没核对成（{}），稍后再试", reason),
    }
}

/// 替账号续登录时暂存的登录没核对成，给的说法。网络问题照网络问题报；别的归成 pending_login，不说登录失效。
/// sure=false：暂存的那份不一定是这个账号的（它保存的登录被服务端拒绝了，才想到可能是被暂存的那份顶掉的）。
pub fn pending_held_error(reason: &str, sure: bool) -> String {
    if reason.starts_with("续登录失败") {
        return reason.to_string();
    }
    let head = if sure {
        format!("这个账号更新的一份{}", PENDING_HELD)
    } else {
        format!(
            "保存的那份登录被服务端拒绝了，不过还有一份{}，可能就是这个账号更新的那份",
            PENDING_HELD
        )
    };
    format!("{}：{}。先不用重新登录", head, pending_why(reason, false))
}

/// 替 target 续登录、切换到 target 之前：暂存的登录可能是它的、又比它保存的新，现在核对一下。
/// access token 过期就用 refresh token 换一个再问。换之前要确认没有 claude 会话还拿着它（pending_holders）；
/// force=true 是用户明说“现在就切”（已经告诉过他正在运行的会话要重开），不再等。
/// 核对出来是谁的就归到谁名下：真是 target 的，接下来就用上最新的 token；其实是别人的，也不会写进 target。
/// only_candidate=false：暂存时猜的不是 target 也核对（target 保存的登录刚被服务端拒绝，可能就是被这份顶掉的：
/// 旧会话写回登录时 ~/.claude.json 里留的是别人的名字，猜的人就不对）。
///
/// 返回空串表示处理完了（或者没有和 target 有关的暂存）；否则是这次没核对成的原因：sessions、live、
/// “续登录失败…”（网络）或“问不出这份登录是谁的”，暂存还留着。这时 target 自己保存的 refresh token 多半已经被
/// 暂存的那份顶掉了，调用方不要再拿它去续：服务端会拒绝，看起来就像登录失效了。与 cam.py 一致。
pub(crate) fn adopt_pending_for(
    store: &Store,
    data: &mut Value,
    target_id: &str,
    force: bool,
    only_candidate: bool,
) -> R<String> {
    let done = String::new;
    let mut p = store.load_secret(PENDING_ID);
    if is_empty_obj(&p) || (only_candidate && s(&p, "candidate") != Some(target_id)) {
        return Ok(done());
    }
    if newer(s(&store.load_secret(target_id), "saved_at"), s(&p, "at")) {
        return Ok(done()); // target 后来又拿到过更新的 token，暂存的这份对它没用了
    }
    let mut ob = p
        .get("claudeAiOauth")
        .filter(|o| o.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut owner = match s(&ob, "accessToken") {
        Some(at) if access_token_fresh(&ob) => token_owner(at),
        _ => None,
    };
    if owner.is_none() {
        let Some(rt) = s(&ob, "refreshToken").map(String::from) else {
            return Ok(done()); // 没有 refresh token 的登录顶不掉 target 保存的那份，也没法换新再核对：留着等 resolve_pending
        };
        let live = read_live()?.0.map(|c| oauth_block(&c));
        let live_rt = live.as_ref().and_then(|o| s(o, "refreshToken"));
        if token_fp(&rt) == token_fp(live_rt.unwrap_or("")) {
            return Ok("live".into()); // 当前登录从不替它续
        }
        if !force && pending_holders(&p) != Some(0) {
            return Ok("sessions".into());
        }
        let tok = match super::usage::refresh_oauth(&rt) {
            Ok(tok) => tok,
            Err(e) if e.contains("登录已失效") => {
                store.delete_secret(PENDING_ID); // refresh token 本身已作废，这份登录没用了
                return Ok(done());
            }
            Err(e) => return Ok(e),
        };
        ob["accessToken"] = tok["access_token"].clone();
        if let Some(r) = s(&tok, "refresh_token") {
            ob["refreshToken"] = json!(r);
        }
        let secs = tok
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .filter(|n| *n != 0)
            .unwrap_or(28800);
        ob["expiresAt"] = json!(chrono::Utc::now().timestamp_millis() + secs * 1000);
        if !p.get("raw").is_some_and(Value::is_object) {
            p["raw"] = json!({});
        }
        p["raw"]["claudeAiOauth"] = ob.clone();
        p["claudeAiOauth"] = ob.clone();
        store.save_secret(PENDING_ID, &p)?; // 旧的 refresh token 已作废：新的先存回暂存，下面核对失败也不能丢这份登录
        owner = token_owner(s(&ob, "accessToken").unwrap_or(""));
    }
    let Some(o) = owner else {
        return Ok("问不出这份登录是谁的".into()); // 还是核对不了：暂存留着
    };
    file_pending(store, data, &p, &ob, &o)?;
    store.delete_secret(PENDING_ID);
    Ok(done())
}

/// 把当前生效的登录保存回对应账号；未知账号按需自动导入。返回账号下标。
/// 只有确认了这份登录是谁的才会存进账号：refresh token 指纹和某个账号保存的一致，或者联网用
/// /api/oauth/profile 问出了主人。两样都做不到（离线、access token 过期）时暂存起来，不碰任何账号。
/// 凭据和 ~/.claude.json 里的 oauthAccount 对不上时，以凭据为准：token 存进它真正的主人名下，
/// 不会覆盖别的账号，并在 data["live_mismatch"] 里留下说明。逻辑与 cam.py 的 snapshot_live 一致。
pub fn snapshot_live(
    store: &Store,
    data: &mut Value,
    label: Option<&str>,
    create: bool,
) -> R<Option<usize>> {
    let (creds, where_) = read_live()?;
    let creds = match creds {
        Some(c) if s(&oauth_block(&c), "accessToken").is_some() => c,
        _ => return Ok(None),
    };
    let ob = oauth_block(&creds);
    let oa = read_oauth_account()?;
    let has_oa = !is_empty_obj(&oa);
    let (oa_uuid, oa_email) = (s(&oa, "accountUuid"), s(&oa, "emailAddress"));
    let live_fp = token_fp(s(&ob, "refreshToken").unwrap_or(""));
    resolve_pending(store, data, Some(&live_fp))?;
    let has_code_accs = !code_indices(data).is_empty();
    let mut idx = identify(data, &oa, &creds);
    let by_token = idx
        .map(|i| s(&accounts(data)[i], "token_fp") == Some(live_fp.as_str()))
        .unwrap_or(false);
    let mut owner: Option<Owner> = None;
    let mut unreachable = false;
    if !by_token {
        // token 换过了，只能靠 oauthAccount 认人；先联网核对一下这份凭据到底是谁的
        if access_token_fresh(&ob) {
            let (code, o) = probe_token_owner(s(&ob, "accessToken").unwrap_or(""));
            // 离线、限流、5xx 都是“这次核对不了”：不能按 ~/.claude.json 里的名字把它存进别人名下；只有 401 是服务端明确不认
            unreachable = o.is_none() && code != 401;
            owner = o;
        } else {
            unreachable = true; // 过期了，问也是 401
        }
        if let Some((ou, oe)) = &owner {
            let agrees = idx
                .map(|i| identity_agrees(&accounts(data)[i], ou.as_deref(), oe.as_deref()))
                .unwrap_or(false);
            if !agrees {
                idx = match_identity(data, ou.as_deref(), oe.as_deref());
            }
        }
    }
    if !(by_token || owner.is_some()) && unreachable && has_code_accs {
        park_unverified(store, data, idx, &ob, &creds, where_, &oa)?;
        if let Some(i) = idx {
            let id = s(&accounts(data)[i], "id").unwrap_or_default().to_string();
            set_active_code(data, Some(&id)); // 只是显示用的最佳猜测
        }
        return Ok(idx);
    }
    if let Some(o) = data.as_object_mut() {
        o.remove("live_unverified");
    }
    let pend = store.load_secret(PENDING_ID);
    if pend
        .get("claudeAiOauth")
        .and_then(|o| s(o, "refreshToken"))
        .map(token_fp)
        == Some(live_fp.clone())
    {
        store.delete_secret(PENDING_ID); // 暂存的就是这份，现在认出来了
    }
    // oauthAccount 是否描述的就是这份凭据的主人
    let oa_ok = has_oa
        && match idx {
            Some(i) => identity_agrees(&accounts(data)[i], oa_uuid, oa_email),
            None => match &owner {
                None => true,
                Some((ou, oe)) => {
                    identity_agrees(&json!({"account_uuid": ou, "email": oe}), oa_uuid, oa_email)
                }
            },
        };
    let idx = match idx {
        Some(i) => {
            if let Some(l) = label {
                accounts_mut(data)[i]["label"] = json!(l);
            }
            i
        }
        None => {
            if !create {
                return Ok(None);
            }
            let id = new_id();
            let email = owner
                .as_ref()
                .and_then(|o| o.1.clone())
                .or_else(|| oa_email.filter(|_| oa_ok).map(String::from));
            let lbl = label
                .map(String::from)
                .or_else(|| email.clone())
                .unwrap_or_else(|| "未命名账号".into());
            accounts_mut(data).push(json!({
                "id": id, "kind": "claude_code", "label": lbl, "email": email, "plan": null,
                "org_name": null, "account_uuid": owner.as_ref().and_then(|o| o.0.clone()), "manual": {},
                "usage": null, "created_at": now_iso(), "last_switched_at": now_iso()
            }));
            if active_code_id(data).is_none() {
                push_switch_log(data, &id);
            }
            accounts(data).len() - 1
        }
    };
    let id = s(&accounts(data)[idx], "id")
        .unwrap_or_default()
        .to_string();
    if oa_ok {
        if let Some(o) = data.as_object_mut() {
            o.remove("live_mismatch");
        }
    } else if has_oa {
        // oauthAccount 是别人的：不用它改这个账号的资料，也不把它存进这个账号
        let label = s(&accounts(data)[idx], "label").unwrap_or("").to_string();
        data["live_mismatch"] = json!({
            "at": now_iso(), "account": id, "claude_json_email": oa_email,
            "message": format!(
                "Claude Code 实际在用的登录属于“{}”，但 ~/.claude.json 里写的是 {}。通常是切换前打开的 claude 会话刷新 token 后写回了旧账号。退出所有 claude 会话后重新切换一次即可。",
                label, oa_email.unwrap_or("-")
            )
        });
    } else if let Some(o) = data.as_object_mut() {
        o.remove("live_mismatch"); // ~/.claude.json 里根本没有 oauthAccount，谈不上对不上
    }
    file_credentials(
        store,
        data,
        idx,
        &ob,
        &creds,
        where_,
        if oa_ok { Some(&oa) } else { None },
    )?;
    set_active_code(data, Some(&id));
    Ok(Some(idx))
}

fn backup_live(store: &Store, tag: &str) -> R<PathBuf> {
    let d = store.new_backup_dir(tag)?;
    let (creds, where_) = read_live()?;
    if let Some(c) = creds {
        write_json_atomic(
            &d.join("credentials.json"),
            &json!({"store": where_, "data": c}),
            true,
        )?;
    }
    let cj = claude_json_file();
    if cj.exists() {
        let dst = d.join("claude.json");
        fs::copy(&cj, &dst).map_err(|e| e.to_string())?;
        set_private(&dst, false);
    }
    store.prune_backups();
    Ok(d)
}

/// 当前登录已经是 id 这个号，但 ~/.claude.json 里的 oauthAccount 不是它的（上次切换只完成了一半，
/// 或旧会话把凭据写回了）：用它保存的 oauthAccount 改回来。返回是否改了。与 cam.py 一致。
fn repair_oauth_account(store: &Store, data: &mut Value, id: &str) -> R<bool> {
    let oa = read_oauth_account()?;
    let has_oa = oa.as_object().map(|o| !o.is_empty()).unwrap_or(false);
    let Some(acc) = accounts(data).iter().find(|a| s(a, "id") == Some(id)) else {
        return Ok(false);
    };
    if has_oa && identity_agrees(acc, s(&oa, "accountUuid"), s(&oa, "emailAddress")) {
        return Ok(false);
    }
    let saved = store
        .load_secret(id)
        .get("oauthAccount")
        .cloned()
        .unwrap_or(Value::Null);
    if !saved.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        return Ok(false);
    }
    write_oauth_account(&saved)?;
    if let Some(o) = data.as_object_mut() {
        o.remove("live_mismatch");
    }
    Ok(true)
}

pub struct SwitchResult {
    pub id: String,
    pub label: String,
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

/// Claude Code 在 access token 过期前 5 分钟就自己续；切换前按同一个标准先替目标账号续好。与 cam.py 一致。
const SWITCH_RENEW_SECS: i64 = 300;

/// 切换前替目标账号续登录没成功时给用户看的话：说清楚没切、当前登录没动、怎么办。
fn switch_renew_error(label: &str, err: &str) -> String {
    if err.contains("登录已失效") {
        format!(
            "没有切换：“{}”保存的登录已失效（refresh token 被服务端拒绝），切过去 Claude Code 也用不了。当前登录没有动。用这个账号重新登录 Claude Code 后点“导入当前登录”，再切换",
            label
        )
    } else {
        format!(
            "没有切换：“{}”保存的登录过期了，切换前要先续一下，但{}。当前登录没有动",
            label, err
        )
    }
}

/// 目标账号更新的一份登录还暂存着、这次没核对成：保存的那份多半已经被它顶掉，切过去用不久，所以不切。
/// sure=false：暂存的那份不一定是它的（见 pending_held_error）。
fn switch_pending_error(label: &str, reason: &str, sure: bool) -> String {
    if reason.starts_with("续登录失败") {
        return format!(
            "没有切换：“{}”更新的一份登录还暂存着，切换前要先核对一下，但{}。当前登录没有动",
            label, reason
        );
    }
    let head = if sure {
        format!("“{}”更新的一份{}", label, PENDING_HELD)
    } else {
        format!(
            "“{}”保存的那份登录被服务端拒绝了，不过还有一份{}，可能就是它更新的那份",
            label, PENDING_HELD
        )
    };
    format!(
        "没有切换：{}。{}。当前登录没有动，先不用重新登录",
        head,
        pending_why(reason, true)
    )
}

/// target 保存的登录本地看还没过期，但有一份更新的登录暂存着没核对（暂存时猜的是别的账号）：问一下服务端还认不认它。
/// 旧会话把 target 换过的登录写回来、~/.claude.json 里留的却是别人的名字时就是这样：target 名下那份已经被顶掉，
/// 照着切过去，Claude Code 一续就被拒绝。只有服务端明确不认（401）才算；离线、限流都当它还好，不耽误切换。
fn saved_login_rejected(store: &Store, target_id: &str, ob: &Value) -> bool {
    let p = store.load_secret(PENDING_ID);
    let parked_rt = p.get("claudeAiOauth").and_then(|o| s(o, "refreshToken"));
    if parked_rt.is_none() || s(&p, "candidate") == Some(target_id) {
        return false;
    }
    if newer(s(&store.load_secret(target_id), "saved_at"), s(&p, "at")) {
        return false;
    }
    probe_token_owner(s(ob, "accessToken").unwrap_or("")).0 == 401
}

/// 切换前拿到 target 能用的登录（oauth 块）；拿不到就返回 Err，当前登录一个字都没动。调用方持有数据锁。
/// 暂存着的更新登录先核对、归档（adopt_pending_for）；保存的登录快过期了就用它自己的 refresh token 续上再切
/// （它不是当前登录，没人跟它抢）。续不上就不切：refresh token 作废了的话，切过去 Claude Code 也用不了，
/// 还把能用的当前登录换掉了。与 cam.py 的 _login_for_switch 一致。
fn login_for_switch(
    store: &Store,
    data: &mut Value,
    target_id: &str,
    label: &str,
    force: bool,
) -> R<Value> {
    let held = adopt_pending_for(store, data, target_id, force, true)?;
    if !held.is_empty() {
        // 保存的那份多半已经被暂存的顶掉：access token 就算还没过期，切过去也用不久
        return Err(switch_pending_error(label, &held, true));
    }
    let saved = || {
        store
            .load_secret(target_id)
            .get("claudeAiOauth")
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut err = String::new();
    for retry in [true, false] {
        let ob = saved();
        let Some(rt) = s(&ob, "refreshToken") else {
            return Err("该账号没有保存可用的凭据，请先用这个账号登录 Claude Code 后再导入".into());
        };
        let expires = ob.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0);
        if expires > chrono::Utc::now().timestamp_millis() + SWITCH_RENEW_SECS * 1000
            && !(retry && saved_login_rejected(store, target_id, &ob))
        {
            return Ok(ob);
        }
        match super::usage::renew_saved_login(store, data, target_id) {
            Ok(renewed) => return Ok(renewed),
            Err(e) => err = e,
        }
        if !(retry && err.contains("登录已失效")) {
            break;
        }
        // 被拒绝的也许只是旧的那份：还有登录暂存着没核对的话（当时猜的是别的账号），看看是不是 target 更新的登录
        let held = adopt_pending_for(store, data, target_id, force, false)?;
        if !held.is_empty() {
            return Err(switch_pending_error(label, &held, false));
        }
        if s(&saved(), "refreshToken") == Some(rt) {
            break; // 没有暂存，或者暂存的是别人的：这个号的登录确实失效了
        }
    }
    let pending_here = data.get("pending_switch").and_then(|p| s(p, "account")) == Some(target_id);
    if err.contains("登录已失效") && pending_here {
        if let Some(o) = data.as_object_mut() {
            o.remove("pending_switch"); // 等下去也切不过去，登记作废
        }
    }
    Err(switch_renew_error(label, &err))
}

/// 切换 Claude Code 登录到 key 指的账号。
/// force=true：用户已经确认“现在就切”（或者没有 claude 会话在跑），目标账号暂存的登录不用等会话退出就核对。
pub fn switch_account(store: &Store, key: &str, force: bool) -> R<SwitchResult> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let ti = find_index(&data, key)?;
    let target = accounts(&data)[ti].clone();
    let tid = s(&target, "id").unwrap_or_default().to_string();
    let label = s(&target, "label").unwrap_or("").to_string();
    if s(&target, "kind") != Some("claude_code") {
        return Err("只有 Claude Code 账号需要切换；网页账号请点“在电脑上打开”".into());
    }
    // 1、2：快照并回写
    let cur = snapshot_live(store, &mut data, None, true)?;
    if let Some(ci) = cur {
        if s(&accounts(&data)[ci], "id") == Some(tid.as_str()) {
            repair_oauth_account(store, &mut data, &tid)?;
            if let Some(o) = data.as_object_mut() {
                o.remove("pending_switch");
            }
            store.save(&mut data)?;
            return Ok(SwitchResult {
                id: tid,
                label,
                changed: false,
                backup: None,
            });
        }
    }
    // 3：拿到目标账号能用的登录；拿不到就不切，什么都不写
    let ob = match login_for_switch(store, &mut data, &tid, &label, force) {
        Ok(ob) => ob,
        Err(e) => {
            store.save(&mut data)?; // 快照可能刚导入了新账号、回写了当前账号的 token，先存下；等待中的切换也留着
            return Err(e);
        }
    };
    let secret = store.load_secret(&tid);
    // 4：备份
    let backup = backup_live(store, &format!("before-{}", tid))?;
    let (live, where_) = read_live()?;
    // 只换 claudeAiOauth：凭据里的其它内容（例如 mcpOAuth，MCP 服务器的登录）保持现在的样子，
    // 不拿目标账号当初保存的旧副本整个覆盖
    let mut creds = live.filter(|v| v.is_object()).unwrap_or_else(|| json!({}));
    creds["claudeAiOauth"] = ob;
    // 5：先写 ~/.claude.json 再写凭据，凭据写失败就把 oauthAccount 改回去。
    // 万一中途崩溃，凭据还是原来那个号的，再切一次就能完成；反过来的话会被当成“已经是当前账号”。
    let prev_oa = read_oauth_account()?;
    write_oauth_account(secret.get("oauthAccount").unwrap_or(&Value::Null))?;
    if let Err(e) = write_live(&creds, where_) {
        let _ = write_oauth_account(&prev_oa); // 回滚失败时保留原来的错误
        return Err(e);
    }
    release_pending(store)?; // 还暂存着的登录（别的账号的）从这时起不是当前登录了
    if let Some(o) = data.as_object_mut() {
        o.remove("live_mismatch"); // 凭据和 oauthAccount 现在是同一个账号的
        o.remove("live_unverified"); // 现在生效的登录是我们刚写的，当然认识
        o.remove("pending_switch"); // 这次切换优先于之前登记的等待切换
    }
    // 6：记录
    set_active_code(&mut data, Some(&tid));
    accounts_mut(&mut data)[ti]["last_switched_at"] = json!(now_iso());
    push_switch_log(&mut data, &tid);
    let loc = super::health::cached_region_loc(&data, super::health::REGION_FRESH_SECS); // 给风险提示记一笔地区
    let reg_now = data.get("region").cloned();
    super::health::note_account_region(&mut data, Some(&tid), loc.as_deref(), reg_now.as_ref());
    store.save(&mut data)?;
    Ok(SwitchResult {
        id: tid,
        label,
        changed: true,
        backup: Some(backup),
    })
}

pub fn import_current(store: &Store, label: Option<&str>) -> R<Value> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let idx = snapshot_live(store, &mut data, label, true)?;
    if let Some(m) = data
        .get("live_unverified")
        .and_then(|u| s(u, "message"))
        .map(String::from)
    {
        store.save(&mut data)?; // 暂存的记录要留着
        return Err(m);
    }
    let idx = idx.ok_or("当前没有登录 Claude Code。请先运行 claude 并登录，再导入")?;
    let acc = accounts(&data)[idx].clone();
    store.save(&mut data)?;
    Ok(acc)
}

/// 检测当前登录；是没见过的账号就自动保存。返回新账号的名称。
pub fn auto_import(store: &Store) -> R<Option<String>> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let before: Vec<String> = accounts(&data)
        .iter()
        .filter_map(|a| s(a, "id").map(String::from))
        .collect();
    let idx = snapshot_live(store, &mut data, None, true)?;
    store.save(&mut data)?;
    Ok(idx.and_then(|i| {
        let a = &accounts(&data)[i];
        let id = s(a, "id").unwrap_or_default();
        if before.iter().any(|b| b == id) {
            None
        } else {
            Some(s(a, "label").unwrap_or("新账号").to_string())
        }
    }))
}

/// 登录状态指纹：文件修改时间；macOS 钥匙串取 refresh token 的哈希。
pub fn live_fingerprint(include_keychain: bool) -> String {
    let mt = |p: PathBuf| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|| "-".into())
    };
    let mut fp = format!("{}|{}", mt(credentials_file()), mt(claude_json_file()));
    if include_keychain && use_keychain() {
        let rt = keychain_read()
            .and_then(|v| s(&oauth_block(&v), "refreshToken").map(String::from))
            .unwrap_or_default();
        fp.push('|');
        fp.push_str(&token_fp(&rt));
    }
    fp
}

// ---------------------------------------------------------------- 正在运行的 claude 进程

/// argv0 是可执行文件（或进程名），cmdline 是完整命令行。规则与 cam.py 的 _is_claude_cmd 一致。
pub fn is_claude_cmd(argv0: &str, cmdline: &str) -> bool {
    let exe = argv0.replace('\\', "/");
    let name = exe.rsplit('/').next().unwrap_or("");
    let low_exe = exe.to_lowercase();
    // Claude 桌面 App（macOS 上叫 Claude，Windows 上装在 AnthropicClaude 下）不算
    if low_exe.contains("/claude.app/")
        || low_exe.contains("anthropicclaude")
        || low_exe.contains("/windowsapps/")
    {
        return false;
    }
    if name == "claude" || name == "claude.exe" || exe.contains("/claude/versions/") {
        return true; // 原生安装版
    }
    if ["node", "node.exe", "bun", "bun.exe"].contains(&name.to_lowercase().as_str()) {
        let low = cmdline.replace('\\', "/").to_lowercase();
        return low.contains("@anthropic-ai/claude-code") || low.contains("claude-code/cli");
    }
    false
}

/// 查进程表（PowerShell / ps）最多等这么久：卡住的命令不能把界面和后台任务一直拖着。和 cam.py 一样，超时当查不到
const PROCESS_LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// 测试里假装的一个 claude 会话：pid、已经运行了多少秒、用的哪个登录（后两样没写就是看不出来）
struct FakeSession {
    pid: u32,
    secs: Option<f64>,
    login: Option<String>,
}

/// 测试用：跑测试的机器上可能真有 claude 在运行，由测试用 CAM_TEST_CLAUDE_PIDS 指定“有哪些”：逗号隔开，
/// 每个是 pid[:已运行秒数[:登录方式]]（如 4242、7:7200、8:7200:own_token）；写 unknown 是假装这次没查出来（里层的 None）。
/// 这时进程是假的、查一次不花时间，下面的缓存和限频都不走（测试里改了要马上见效），也不采 CPU、不读 /proc。
fn fake_sessions() -> Option<Option<Vec<FakeSession>>> {
    let v = std::env::var_os("CAM_TEST_CLAUDE_PIDS")?;
    let v = v.to_string_lossy();
    if v.trim() == "unknown" {
        return Some(None);
    }
    let one = |item: &str| {
        let mut f = item.trim().split(':');
        Some(FakeSession {
            pid: f.next()?.parse::<u32>().ok()?,
            secs: f.next().and_then(|x| x.parse::<f64>().ok()),
            login: f.next().filter(|x| !x.is_empty()).map(String::from),
        })
    };
    Some(Some(v.split(',').filter_map(one).collect()))
}

fn faking_processes() -> bool {
    std::env::var_os("CAM_TEST_CLAUDE_PIDS").is_some()
}

/// 正在运行的 Claude Code 进程 (pid, 命令行)，不含本进程。查不到时返回空列表：显示上当 0 个。
/// 要据此做决定的地方（自动切换、替暂存的登录换 token）用 claude_may_be_running / list_claude_processes，把“没查出来”当成“可能有”。
/// 切换账号后，这些进程内存里还是旧账号的 token，刷新后还可能把旧账号写回凭据文件。
pub fn running_claude_processes() -> Vec<(u32, String)> {
    list_claude_processes().unwrap_or_default()
}

/// 有 claude 进程在跑，或者这次没查出来。自动替用户做事之前用这个问，不用 running_claude_processes().is_empty()。
/// 与 cam.py 的 claude_may_be_running 一致。
pub fn claude_may_be_running() -> bool {
    match list_claude_processes() {
        Some(procs) => !procs.is_empty(),
        None => true, // 没查出来：当作可能有
    }
}

/// 正在运行的 Claude Code 进程 (pid, 命令行)，不含本进程；进程没查出来（ps / PowerShell 出错或超时、/proc 读不了）时是 None。
/// 与 cam.py 的 running_claude_processes 对应（它查不出来时返回 ProcsUnknown）。
pub fn list_claude_processes() -> Option<Vec<(u32, String)>> {
    if let Some(fake) = fake_sessions() {
        return fake.map(|list| list.iter().map(|f| (f.pid, "claude".to_string())).collect());
    }
    let me = std::process::id();
    let mut found = Vec::new();
    if cfg!(target_os = "windows") {
        // 用完整路径而不是 Name：桌面 App 的判断看的是路径，只给文件名的话
        // Claude 桌面 App（AnthropicClaude\…\claude.exe）会被当成 Claude Code。
        // 和桌面版共用一次进程表（一秒内查过的直接用：PowerShell 查一次本身就要半秒多）；
        // PowerShell 没起来、超时、自己出错了：不是“没有进程”
        let rows = super::health::win_processes(Duration::from_secs(1))?;
        for (pid, exe, name, cmd) in rows {
            let lname = name.to_lowercase();
            if pid == me || !matches!(lname.as_str(), "claude.exe" | "node.exe" | "bun.exe") {
                continue;
            }
            let path = if exe.is_empty() { name } else { exe };
            if is_claude_cmd(&path, &cmd) {
                found.push((pid, if cmd.is_empty() { path } else { cmd }));
            }
        }
    } else if std::path::Path::new("/proc/self").exists() {
        for e in fs::read_dir("/proc").ok()?.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if pid == me {
                continue;
            }
            let Ok(raw) = fs::read(e.path().join("cmdline")) else {
                continue;
            };
            let argv: Vec<String> = raw
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            if let Some(a0) = argv.first() {
                let cmd = argv.join(" ");
                if is_claude_cmd(a0, &cmd) {
                    found.push((pid, cmd));
                }
            }
        }
    } else {
        // macOS：comm 是可执行文件完整路径（可能带空格），args 是完整命令行
        let ps = |field: &str| -> Option<HashMap<u32, String>> {
            let mut m = HashMap::new();
            let mut c = Command::new("ps");
            c.args(["-A", "-o", &format!("pid=,{}=", field)]);
            let (Some(_), out) = super::health::run_command(c, PROCESS_LIST_TIMEOUT) else {
                return None; // ps 没起来或超时
            };
            for line in out.lines() {
                let line = line.trim();
                let (pid, rest) = line.split_once(' ').unwrap_or((line, ""));
                if let Ok(pid) = pid.parse::<u32>() {
                    m.insert(pid, rest.trim().to_string());
                }
            }
            Some(m)
        };
        let (comm, args) = (ps("comm")?, ps("args")?);
        if comm.is_empty() {
            return None; // ps 至少会列出它自己；一行都没有就是没查成
        }
        for (pid, exe) in comm {
            let cmd = args.get(&pid).cloned().unwrap_or_else(|| exe.clone());
            if pid != me && is_claude_cmd(&exe, &cmd) {
                found.push((pid, cmd));
            }
        }
    }
    found.sort();
    Some(found)
}

pub fn running_summary(procs: &[(u32, String)]) -> String {
    let mut pids: Vec<String> = procs.iter().take(5).map(|(p, _)| p.to_string()).collect();
    if procs.len() > 5 {
        pids.push("…".into());
    }
    format!(
        "有 {} 个 claude 进程正在运行（pid {}）",
        procs.len(),
        pids.join("、")
    )
}

pub fn running_warning(procs: &[(u32, String)]) -> Option<String> {
    if procs.is_empty() {
        return None;
    }
    Some(format!(
        "{}，它们还在用切换前的账号，并可能在刷新 token 后把旧账号写回。请退出这些会话后重开（可用 claude --continue 接着聊）。",
        running_summary(procs)
    ))
}

// ---------------------------------------------------------------- 等会话结束再切换

/// 手动登记的等待切换放多久：过了还没等到会话结束就作废，免得几小时后突然换号。与 cam.py 一致。
pub const PENDING_SWITCH_HOURS: i64 = 6;

/// 手动登记的等待切换放太久了。时间规则的登记跟着时间段作废，不看这个。
fn pending_switch_expired(p: &Value) -> bool {
    if s(p, "reason") == Some("rule") {
        return false;
    }
    match s(p, "at").and_then(parse_time) {
        Some(at) => {
            chrono::Utc::now().signed_duration_since(at)
                > chrono::Duration::hours(PENDING_SWITCH_HOURS)
        }
        None => true,
    }
}

/// 登记中的“等会话结束再切换”，给界面显示用；没有、已过期或目标账号已删除则 Null。与 cam.py 一致。
pub fn pending_switch_view(data: &Value) -> Value {
    let Some(p) = data
        .get("pending_switch")
        .filter(|p| p.is_object() && !pending_switch_expired(p))
    else {
        return Value::Null;
    };
    let Some(acc) = accounts(data)
        .iter()
        .find(|a| s(a, "id") == s(p, "account"))
    else {
        return Value::Null;
    };
    let label = s(acc, "label").unwrap_or("");
    let reason = s(p, "reason").unwrap_or("manual");
    json!({
        "account": s(acc, "id"), "label": label, "reason": reason,
        "at": p.get("at").cloned().unwrap_or(Value::Null),
        "message": format!(
            "等当前 claude 会话结束后自动切换到“{}”{}",
            label,
            if reason == "rule" { "（时间规则）" } else { "" }
        )
    })
}

/// 记下“等 claude 会话都退出后切到 acc_id”。调用方持有锁并负责保存。
pub fn defer_switch(data: &mut Value, acc_id: &str, reason: &str, rule_id: Option<&str>) {
    data["pending_switch"] =
        json!({"account": acc_id, "reason": reason, "rule": rule_id, "at": now_iso()});
}

pub fn cancel_pending_switch(store: &Store) -> R<bool> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let had = data
        .as_object_mut()
        .map(|o| o.remove("pending_switch").is_some())
        .unwrap_or(false);
    store.save(&mut data)?;
    Ok(had)
}

/// 等会话结束再切换的时候，进程表最多这么久查一次。后台每 5 秒来问一轮，一等可能几个小时，
/// 每轮都查的话 Windows 上就是每 5 秒起一个 PowerShell。
const PENDING_CHECK_EVERY: Duration = Duration::from_secs(15);

/// 还有 claude 会话在跑、等待中的切换要接着等吗。last_busy 是上次查到“有会话在跑”的时间：没过 PENDING_CHECK_EVERY
/// 就不再查，接着等；别的情况现查。查到没有会话的结果不记，所以真要切换时用的一定是刚查到的。
fn sessions_still_running(
    last_busy: &mut Option<Instant>,
    now: Instant,
    list: impl FnOnce() -> bool,
) -> bool {
    if last_busy.is_some_and(|t| now.saturating_duration_since(t) < PENDING_CHECK_EVERY) {
        return true;
    }
    let running = list();
    *last_busy = running.then_some(now);
    running
}

/// 进程没查出来也当作有：宁可晚点切。
fn pending_switch_blocked() -> bool {
    if faking_processes() {
        return claude_may_be_running();
    }
    static LAST_BUSY: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST_BUSY.lock().unwrap_or_else(|e| e.into_inner());
    sessions_still_running(&mut last, Instant::now(), claude_may_be_running)
}

pub fn apply_pending_switch(store: &Store) -> R<Option<(SwitchResult, String)>> {
    apply_pending_switch_at(store, chrono::Local::now().naive_local())
}

/// 后台每轮调用：登记过的等待切换，在没有 claude 会话运行时执行。返回 (切换结果, reason)。
/// 目标账号没了、已经是当前账号、手动登记放了 PENDING_SWITCH_HOURS 小时还没等到、或时间规则的时间段已经过了，
/// 就作废（过期的不等会话退出，马上清掉）。切换本身走 switch_account，它会清掉登记。
/// 与 cam.py 的 apply_pending_switch 一致。
pub fn apply_pending_switch_at(
    store: &Store,
    now: chrono::NaiveDateTime,
) -> R<Option<(SwitchResult, String)>> {
    let data = store.load()?;
    let Some(p) = data.get("pending_switch").filter(|p| p.is_object()) else {
        return Ok(None);
    };
    if !pending_switch_expired(p) && pending_switch_blocked() {
        return Ok(None);
    }
    let (target, reason) = {
        let _g = store.lock()?;
        let mut data = store.load()?;
        let Some(p) = data
            .get("pending_switch")
            .filter(|p| p.is_object())
            .cloned()
        else {
            return Ok(None);
        };
        let target = s(&p, "account").unwrap_or_default().to_string();
        let known = accounts(&data)
            .iter()
            .any(|a| s(a, "id") == Some(target.as_str()));
        let mut drop = !known
            || active_code_id(&data).as_deref() == Some(target.as_str())
            || pending_switch_expired(&p);
        if !drop && s(&p, "reason") == Some("rule") {
            let r = super::rules::match_time_rule(&data, now);
            drop = r.as_ref().and_then(|r| s(r, "id")) != s(&p, "rule"); // 时间段已经过了，或规则变了
        }
        if drop {
            if let Some(o) = data.as_object_mut() {
                o.remove("pending_switch");
            }
            store.save(&mut data)?;
            return Ok(None);
        }
        (target, s(&p, "reason").unwrap_or("manual").to_string())
    };
    let r = switch_account(store, &target, false)?;
    Ok(if r.changed { Some((r, reason)) } else { None })
}

// ---------------------------------------------------------------- 会话在不在执行（红绿灯）

/// 会话记录文件这么多秒内写过就算在执行（跑着的时候每条消息、每个工具结果都往里追加；等你输入时不写）。与 cam.py 的 ACTIVE_WINDOW 一致
const ACTIVE_WINDOW_SECS: u64 = 20;
/// 有一个会话（一个 claude 进程连同它的子孙进程）这段时间平均占 CPU 超过 2% 就算在执行（算 token、画界面、跑工具）；
/// 闲着等输入时接近 0。一个一个会话比，不加在一起：几个闲着的会话各占零点几，加起来也能过 2%。与 cam.py 的 CPU_BUSY 一致
const CPU_BUSY: f64 = 0.02;
/// 两次采样隔得比这久，平均下来的占用说明不了现在（一小时前忙过一阵、现在闲着，平均也能过 2%）：当不知道，这次的当新基准。与 cam.py 一致
const CPU_SAMPLE_MAX_GAP: f64 = 60.0;
/// /proc/<pid>/stat 里 CPU 时间的单位：Linux 给用户态看的滴答固定是每秒 100 个（USER_HZ），和内核自己的 HZ 无关
const PROC_TICKS_PER_SEC: f64 = 100.0;

/// /proc/<pid>/stat 里的 (父进程 pid, 累计 CPU 秒)。进程名在括号里、可能带空格和括号，从最后一个 ")" 后面数：
/// state ppid … utime stime（整行的第 14、15 个字段）。
fn parse_proc_stat(stat: &str) -> Option<(u32, f64)> {
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    if f.len() < 13 {
        return None;
    }
    let ticks = f[11].parse::<u64>().ok()? + f[12].parse::<u64>().ok()?;
    Some((f[1].parse().ok()?, ticks as f64 / PROC_TICKS_PER_SEC))
}

/// ps 的 cputime 换成秒：macOS 是 m:ss.cc，Linux 是 [d-]hh:mm:ss。与 cam.py 的 _cpu_seconds 一致。
fn parse_cputime(text: &str) -> f64 {
    let (days, rest) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<u32>().unwrap_or(0), rest),
        None => (0, text),
    };
    let mut total = 0.0;
    for part in rest.split(':') {
        let Ok(x) = part.parse::<f64>() else {
            return 0.0;
        };
        total = total * 60.0 + x;
    }
    f64::from(days) * 86400.0 + total
}

/// 进程表：每个进程的子进程、每个进程累计的 CPU 秒
type ProcTable = (HashMap<u32, Vec<u32>>, HashMap<u32, f64>);

/// `ps -A -o pid=,ppid=,cputime=` 的输出
fn parse_ps_cpu(out: &str) -> ProcTable {
    let (mut children, mut cpu): ProcTable = Default::default();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(time)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        cpu.insert(pid, parse_cputime(time));
    }
    (children, cpu)
}

/// roots 里每个进程连同它的子孙进程累计用掉的 CPU 秒
fn tree_cpu(roots: &[u32], (children, cpu): &ProcTable) -> HashMap<u32, f64> {
    roots
        .iter()
        .map(|&root| {
            let (mut total, mut stack, mut seen) = (0.0, vec![root], Vec::new());
            while let Some(p) = stack.pop() {
                if seen.contains(&p) {
                    continue;
                }
                seen.push(p);
                total += cpu.get(&p).copied().unwrap_or(0.0);
                stack.extend(children.get(&p).into_iter().flatten());
            }
            (root, total)
        })
        .collect()
}

/// 每个 claude 进程连同它的子孙进程累计用掉的 CPU 秒。Linux 读 /proc（精确到 10 毫秒），macOS 用 ps；
/// Windows 不采（只看记录文件），查不出来也返回 None。与 cam.py 的 claude_cpu_seconds 一致。
fn claude_cpu_seconds(pids: &[u32]) -> Option<HashMap<u32, f64>> {
    if cfg!(target_os = "windows") || faking_processes() {
        return None;
    }
    let table = if Path::new("/proc/self").exists() {
        let (mut children, mut cpu): ProcTable = Default::default();
        for e in fs::read_dir("/proc").ok()?.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(raw) = fs::read(e.path().join("stat")) else {
                continue;
            };
            if let Some((ppid, secs)) = parse_proc_stat(&String::from_utf8_lossy(&raw)) {
                children.entry(ppid).or_default().push(pid);
                cpu.insert(pid, secs);
            }
        }
        (children, cpu)
    } else {
        let mut c = Command::new("ps");
        c.args(["-A", "-o", "pid=,ppid=,cputime="]);
        parse_ps_cpu(&super::health::run_with_timeout(c, PROCESS_LIST_TIMEOUT))
    };
    if table.1.is_empty() {
        return None; // 什么都没读到：当查不出来，别拿一堆 0 当基准
    }
    Some(tree_cpu(pids, &table))
}

/// 两次采样之间，占 CPU 最多的那个会话平均占了多少（1 = 一个核跑满）。只比两次都在的进程；隔了不到 1 秒或超过
/// CPU_SAMPLE_MAX_GAP 算不出来。claude 里再起的 claude 也在表里，它的占用已经算在外面那个会话里，取最大的不受影响。
fn cpu_rate(last: &HashMap<u32, f64>, cur: &HashMap<u32, f64>, dt: f64) -> Option<f64> {
    if !(1.0..=CPU_SAMPLE_MAX_GAP).contains(&dt) {
        return None;
    }
    let most = cur
        .iter()
        .filter_map(|(pid, secs)| last.get(pid).map(|was| secs - was))
        .fold(0.0, f64::max);
    Some(most / dt)
}

/// 上一次 CPU 采样：什么时候、每个 claude 进程（含子孙进程）累计的 CPU 秒
#[derive(Default)]
struct CpuSample {
    at: Option<Instant>,
    cpu: Option<HashMap<u32, f64>>,
}

/// 和上一次采样比，这段时间有没有会话在吃 CPU；这次的记下来当下一次的基准。第一次、查不出来、隔太久都算不出来，当没在吃。
/// 离上次不到 1 秒也算不出来，而且基准不换：连着来的请求每次都换的话，永远攒不够 1 秒。
fn cpu_busy(prev: &mut CpuSample, cur: Option<HashMap<u32, f64>>, now: Instant) -> bool {
    let dt = prev
        .at
        .map(|at| now.saturating_duration_since(at).as_secs_f64());
    if prev.cpu.is_some() && dt.is_some_and(|dt| dt < 1.0) {
        return false;
    }
    let rate = match (&prev.cpu, &cur, dt) {
        (Some(last), Some(cur), Some(dt)) => cpu_rate(last, cur, dt),
        _ => None,
    };
    *prev = CpuSample {
        at: Some(now),
        cpu: cur,
    };
    rate.is_some_and(|r| r > CPU_BUSY)
}

/// root 下的会话记录（.jsonl，含子目录）最近 ACTIVE_WINDOW_SECS 秒内有没有写过。
fn transcript_written_recently(root: &Path) -> bool {
    let now = std::time::SystemTime::now();
    super::usage::walk_jsonl(root).iter().any(|p| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            // 修改时间比现在还晚（时钟刚调过）也算刚写过，和 cam.py 一样
            .map(|m| {
                now.duration_since(m)
                    .map_or(true, |d| d.as_secs() < ACTIVE_WINDOW_SECS)
            })
            .unwrap_or(false)
    })
}

/// 这些 claude 进程里有没有真的在执行任务的（不只是开着等你输入）：最近 ACTIVE_WINDOW_SECS 秒内往会话记录里写过，或者
/// 有一个会话这段时间在吃 CPU（长时间跑一个工具、长时间思考时记录文件不动）。CPU 占用要两次采样才算得出，第一次只看记录文件。
/// 与 cam.py 的 claude_activity 一致。
pub fn claude_busy(pids: &[u32]) -> bool {
    static LAST: Mutex<CpuSample> = Mutex::new(CpuSample {
        at: None,
        cpu: None,
    });
    // 采样和比较都在锁里：两个线程交错着来的话，后一个拿到的基准是前一个几毫秒前刚记的
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if pids.is_empty() {
        *last = CpuSample {
            at: Some(Instant::now()),
            cpu: None,
        };
        return false;
    }
    let written = transcript_written_recently(&claude_config_dir().join("projects"));
    let eating = cpu_busy(&mut last, claude_cpu_seconds(pids), Instant::now());
    written || eating
}

/// 界面上的会话数和“在不在执行”缓存多久。查进程表要几百毫秒（Windows 上起一个 PowerShell 要一秒多），还要把会话记录目录
/// 走一遍，而每次操作、界面每次轮询都要 /api/state。
const SESSIONS_TTL: Duration = Duration::from_secs(10);

/// slot 里的值还没过 ttl 就用它，过了（或还没有）就现算一次记下来。
fn cached<T: Copy>(
    slot: &mut Option<(Instant, T)>,
    now: Instant,
    ttl: Duration,
    fresh: impl FnOnce() -> T,
) -> T {
    match slot {
        Some((at, v)) if now.saturating_duration_since(*at) < ttl => *v,
        _ => {
            let v = fresh();
            *slot = Some((now, v));
            v
        }
    }
}

/// 给界面显示用的：(几个 claude 会话开着, 有没有真在执行任务的)，缓存 SESSIONS_TTL。同时来的几个请求只查一次。
/// 切换前的判断不走这里，用 running_claude_processes 现查。与 cam.py 的 session_count / session_busy 对应。
pub fn sessions_view() -> (usize, bool) {
    // 上次数出来的：这次进程没查出来就沿用，不显示成 0 个
    static LAST: Mutex<(usize, bool)> = Mutex::new((0, false));
    let count = || {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(procs) = list_claude_processes() {
            let pids: Vec<u32> = procs.iter().map(|(pid, _)| *pid).collect();
            *last = (pids.len(), claude_busy(&pids));
        }
        *last
    };
    if faking_processes() {
        return count();
    }
    static CACHE: Mutex<Option<(Instant, (usize, bool))>> = Mutex::new(None);
    let mut slot = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cached(&mut slot, Instant::now(), SESSIONS_TTL, count)
}

/// 有没有 Claude Code 会话在等你确认（权限提示 yes / no）：cam 的钩子（cam hooks install）把等着的会话记在
/// CAM_HOME/attention.json 里；提示出现之后会话记录又写了的算答过了，超过两小时的算过期；没有 claude 进程在跑也不算。
/// 和 cam.py 的 attention_view 对应。
pub fn attention_view(procs_running: bool) -> Value {
    if !procs_running {
        return Value::Null;
    }
    let root = std::env::var_os("CAM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude-accounts"));
    let Ok(text) = fs::read_to_string(root.join("attention.json")) else {
        return Value::Null;
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return Value::Null;
    };
    let Some(sessions) = data.get("sessions").and_then(|s| s.as_object()) else {
        return Value::Null;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut live: Vec<&Value> = sessions
        .values()
        .filter(|s| {
            let since = s.get("since_ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if now - since > 2.0 * 3600.0 {
                return false;
            }
            let written = s
                .get("transcript")
                .and_then(|v| v.as_str())
                .and_then(|p| fs::metadata(p).ok())
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64());
            !matches!(written, Some(w) if w > since + 2.0)
        })
        .collect();
    if live.is_empty() {
        return Value::Null;
    }
    live.sort_by(|a, b| {
        let f = |v: &Value| v.get("since_ts").and_then(|x| x.as_f64()).unwrap_or(0.0);
        f(a).partial_cmp(&f(b)).unwrap_or(std::cmp::Ordering::Equal)
    });
    let first = live[0];
    let tool = first.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    let what = first.get("what").and_then(|v| v.as_str()).unwrap_or("");
    let label = if what.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}：{what}")
    };
    let text = if live.len() > 1 {
        format!("{} 个 Claude Code 会话在等你确认", live.len())
    } else if label.is_empty() {
        "Claude Code 在等你确认".to_string()
    } else {
        format!("Claude Code 在等你确认 · {label}")
    };
    json!({ "count": live.len(), "text": text, "since": first.get("since").cloned().unwrap_or(Value::Null) })
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use std::cell::Cell;
    use std::time::SystemTime;

    fn sample(pairs: &[(u32, f64)]) -> Option<HashMap<u32, f64>> {
        Some(pairs.iter().copied().collect())
    }
    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn proc_stat_gives_parent_and_cpu_seconds_whatever_the_process_name() {
        let rest = "S 1000 1234 1234 34816 1234 4194304 100 0 0 0 250 50 7 3 20 0 11 0 12345";
        assert_eq!(
            parse_proc_stat(&format!("1234 (claude) {rest}")),
            Some((1000, 3.0))
        );
        // 进程名里有空格和括号：从最后一个 ")" 后面数
        assert_eq!(
            parse_proc_stat(&format!("77 (tmux: server (1)) {rest}")),
            Some((1000, 3.0))
        );
        assert_eq!(parse_proc_stat("1234 (claude) S 1000 1234"), None);
        assert_eq!(parse_proc_stat("no parenthesis here"), None);
        assert_eq!(parse_proc_stat(""), None);
    }

    #[test]
    fn etime_reads_ps_elapsed_time() {
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
    fn proc_stat_gives_when_the_process_started() {
        // 第 22 项（开机后多少个时钟滴答启动的）是 12345
        let rest = "S 1000 1234 1234 34816 1234 4194304 100 0 0 0 250 50 7 3 20 0 11 0 12345 99";
        let want = 12345.0 / PROC_TICKS_PER_SEC;
        assert!(close(
            proc_start_secs(&format!("1234 (claude) {rest}")).unwrap(),
            want
        ));
        assert!(close(
            proc_start_secs(&format!("77 (tmux: server (1)) {rest}")).unwrap(),
            want
        ));
        assert_eq!(proc_start_secs("1234 (claude) S 1000 1234"), None);
        assert_eq!(proc_start_secs("no parenthesis here"), None);
    }

    #[test]
    fn session_login_only_looks_at_which_variables_are_set() {
        let mine = std::env::temp_dir().join(format!("cam-engine-cfg-{}", new_id()));
        let env = |pairs: &[&str]| pairs.join("\0").into_bytes();
        let login = |raw: &[u8]| session_login(Some(raw), &mine);
        assert_eq!(session_login(None, &mine), "unknown"); // environ 读不到
        assert_eq!(login(&env(&["PATH=/usr/bin", "HOME=/home/me"])), "machine");
        assert_eq!(login(b""), "machine");
        assert_eq!(
            login(&env(&["PATH=/usr/bin", "CLAUDE_CODE_OAUTH_TOKEN=x"])),
            "own_token"
        );
        assert_eq!(login(&env(&["ANTHROPIC_API_KEY=x"])), "api_key");
        assert_eq!(login(&env(&["ANTHROPIC_AUTH_TOKEN=x"])), "api_key");
        // 两样都有：自带 token 的优先
        assert_eq!(
            login(&env(&["ANTHROPIC_API_KEY=x", "CLAUDE_CODE_OAUTH_TOKEN=x"])),
            "own_token"
        );
        // 设了但是空的不算
        assert_eq!(
            login(&env(&["CLAUDE_CODE_OAUTH_TOKEN=", "ANTHROPIC_API_KEY="])),
            "machine"
        );
        // 配置目录就是本机这个：本机登录；别的目录：各用各的
        let same = format!("CLAUDE_CONFIG_DIR={}", mine.display());
        assert_eq!(login(&env(&[&same])), "machine");
        assert_eq!(
            login(&env(&["CLAUDE_CONFIG_DIR=/somewhere/else/claude"])),
            "config_dir"
        );
        assert_eq!(login(&env(&["CLAUDE_CONFIG_DIR="])), "machine");
    }

    #[test]
    fn cputime_reads_both_ps_formats() {
        assert!(close(parse_cputime("0:01.50"), 1.5)); // macOS：分:秒.百分秒
        assert!(close(parse_cputime("12:34.56"), 754.56));
        assert!(close(parse_cputime("01:02:03"), 3723.0)); // Linux：时:分:秒
        assert!(close(parse_cputime("2-01:00:00"), 2.0 * 86400.0 + 3600.0));
        assert!(close(parse_cputime("abc"), 0.0));
        assert!(close(parse_cputime(""), 0.0));
    }

    #[test]
    fn tree_cpu_adds_up_each_session_with_its_descendants() {
        let table = parse_ps_cpu(
            "  100     1   0:10.00\n  101   100   0:02.50\n  102   101   0:00.50\n  200     1   1:00.00\nnot a row\n",
        );
        let got = tree_cpu(&[100, 200, 999], &table);
        assert!(close(got[&100], 13.0)); // 自己 10 秒 + 子进程 2.5 秒 + 孙进程 0.5 秒
        assert!(close(got[&200], 60.0));
        assert!(close(got[&999], 0.0)); // 表里没有的进程
        assert_eq!(got.len(), 3);
        // 父子关系绕圈（pid 重用时两次读到的表对不上）也要停得下来
        let (mut children, mut cpu): ProcTable = Default::default();
        children.insert(1, vec![2]);
        children.insert(2, vec![1]);
        cpu.insert(1, 1.0);
        cpu.insert(2, 2.0);
        assert!(close(tree_cpu(&[1], &(children, cpu))[&1], 3.0));
    }

    #[test]
    fn cpu_rate_is_the_busiest_session_not_the_sum() {
        // 四个闲着的会话 10 秒里各用 0.06 秒（0.6%），加起来 2.4%，但没有一个在执行
        let last = sample(&[(1, 10.0), (2, 20.0), (3, 30.0), (4, 40.0)]).unwrap();
        let idle = sample(&[(1, 10.06), (2, 20.06), (3, 30.06), (4, 40.06)]).unwrap();
        let rate = cpu_rate(&last, &idle, 10.0).unwrap();
        assert!(close(rate, 0.006), "{rate}");
        assert!(rate < CPU_BUSY);
        // 其中一个用了 0.5 秒（5%）
        let one = sample(&[(1, 10.06), (2, 20.5), (3, 30.06), (4, 40.06)]).unwrap();
        assert!(close(cpu_rate(&last, &one, 10.0).unwrap(), 0.05));
    }

    #[test]
    fn cpu_rate_only_compares_pids_in_both_samples() {
        let last = sample(&[(10, 5.0), (11, 1.0)]).unwrap();
        // 11 退出了；12 是新开的，它之前累计的 300 秒不是这段时间用的
        let cur = sample(&[(10, 5.5), (12, 300.0)]).unwrap();
        assert!(close(cpu_rate(&last, &cur, 10.0).unwrap(), 0.05));
        // 没有两次都在的进程：没在吃
        let other = sample(&[(12, 300.0)]).unwrap();
        assert!(close(cpu_rate(&last, &other, 10.0).unwrap(), 0.0));
        // 累计秒数变小了（pid 被别的进程重用）不算成负的
        let reused = sample(&[(10, 0.2)]).unwrap();
        assert!(close(cpu_rate(&last, &reused, 10.0).unwrap(), 0.0));
    }

    #[test]
    fn cpu_rate_is_unknown_when_the_gap_is_too_short_or_too_long() {
        let last = sample(&[(10, 5.0)]).unwrap();
        let cur = sample(&[(10, 8.0)]).unwrap();
        assert_eq!(cpu_rate(&last, &cur, 0.5), None);
        assert!(cpu_rate(&last, &cur, 1.0).is_some());
        assert!(cpu_rate(&last, &cur, 60.0).is_some());
        assert_eq!(cpu_rate(&last, &cur, 60.5), None);
        assert_eq!(cpu_rate(&last, &cur, 3600.0), None);
    }

    #[test]
    fn cpu_busy_needs_two_samples_and_compares_with_the_previous_one() {
        let t0 = Instant::now();
        let mut prev = CpuSample::default();
        // 第一次没有基准
        assert!(!cpu_busy(&mut prev, sample(&[(10, 100.0)]), t0));
        assert!(cpu_busy(&mut prev, sample(&[(10, 101.0)]), t0 + secs(10))); // 10%
        assert!(!cpu_busy(&mut prev, sample(&[(10, 101.1)]), t0 + secs(20))); // 1%
                                                                              // 这次查不出来：没在吃，下一次也没有基准可比
        assert!(!cpu_busy(&mut prev, None, t0 + secs(30)));
        assert!(!cpu_busy(&mut prev, sample(&[(10, 200.0)]), t0 + secs(40)));
        assert!(cpu_busy(&mut prev, sample(&[(10, 203.0)]), t0 + secs(50)));
    }

    #[test]
    fn cpu_busy_takes_a_new_baseline_after_a_long_gap() {
        let t0 = Instant::now();
        let mut prev = CpuSample::default();
        assert!(!cpu_busy(&mut prev, sample(&[(10, 100.0)]), t0));
        // 隔了两分钟：这中间用掉的 400 秒说明不了现在
        assert!(!cpu_busy(&mut prev, sample(&[(10, 500.0)]), t0 + secs(120)));
        assert_eq!(prev.at, Some(t0 + secs(120)));
        // 和新基准比只有 1%；要是还和两分钟前的比就成了在执行
        assert!(!cpu_busy(&mut prev, sample(&[(10, 500.1)]), t0 + secs(130)));
        assert!(cpu_busy(&mut prev, sample(&[(10, 503.1)]), t0 + secs(140)));
    }

    #[test]
    fn cpu_busy_keeps_the_old_baseline_when_asked_again_within_a_second() {
        let t0 = Instant::now();
        let mut prev = CpuSample::default();
        assert!(!cpu_busy(&mut prev, sample(&[(10, 100.0)]), t0));
        // 0.4 秒后又来问：算不出来，基准不换
        let soon = t0 + Duration::from_millis(400);
        assert!(!cpu_busy(&mut prev, sample(&[(10, 100.3)]), soon));
        assert!(!cpu_busy(&mut prev, None, soon));
        assert_eq!(prev.at, Some(t0));
        assert!(close(prev.cpu.as_ref().unwrap()[&10], 100.0));
        // 10 秒后和最早那次比：0.5 秒 / 10 秒 = 5%
        assert!(cpu_busy(&mut prev, sample(&[(10, 100.5)]), t0 + secs(10)));
        // 上次没查出来（没有基准）时，不到 1 秒也把这次的记下来
        let mut prev = CpuSample {
            at: Some(t0),
            cpu: None,
        };
        assert!(!cpu_busy(&mut prev, sample(&[(10, 100.0)]), soon));
        assert_eq!(prev.at, Some(soon));
        assert!(prev.cpu.is_some());
    }

    #[test]
    fn transcript_counts_as_written_only_within_the_window() {
        let root = std::env::temp_dir().join(format!(
            "cam-engine-busy-{}-{}",
            std::process::id(),
            new_id()
        ));
        let file = root.join("-home-me-proj").join("session.jsonl");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        assert!(!transcript_written_recently(&root)); // 还没有记录
        fs::write(root.join("notes.txt"), "x").unwrap();
        assert!(!transcript_written_recently(&root)); // 不是 .jsonl 的不算
        fs::write(&file, "{}\n").unwrap();
        assert!(transcript_written_recently(&root));
        let touch = |t: SystemTime| {
            fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        touch(SystemTime::now() - secs(ACTIVE_WINDOW_SECS + 40));
        assert!(!transcript_written_recently(&root));
        touch(SystemTime::now() + secs(3600)); // 时钟调过：比现在还晚也算刚写过
        assert!(transcript_written_recently(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cached_value_is_reused_until_the_ttl_passes() {
        let t0 = Instant::now();
        let (mut slot, calls) = (None, Cell::new(0));
        let mut get = |now| {
            cached(&mut slot, now, secs(10), || {
                calls.set(calls.get() + 1);
                calls.get()
            })
        };
        assert_eq!(get(t0), 1);
        assert_eq!(get(t0 + secs(9)), 1);
        assert_eq!(get(t0 + secs(10)), 2); // 到点了重算
        assert_eq!(get(t0 + secs(19)), 2);
        assert_eq!(get(t0 + secs(25)), 3);
        assert_eq!(get(t0), 3); // 时间往回走不重算也不出错
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn pending_switch_lists_processes_at_most_once_per_interval() {
        let t0 = Instant::now();
        let (mut last, listed) = (None, Cell::new(0));
        let mut waiting = |now, running: bool| {
            sessions_still_running(&mut last, now, || {
                listed.set(listed.get() + 1);
                running
            })
        };
        assert!(waiting(t0, true));
        assert_eq!(listed.get(), 1);
        // 15 秒内再问不查进程表，接着等（哪怕会话其实已经关了）
        assert!(waiting(t0 + secs(5), false));
        assert!(waiting(t0 + secs(10), false));
        assert_eq!(listed.get(), 1);
        // 到 15 秒现查：没有会话了，可以切
        assert!(!waiting(t0 + secs(15), false));
        assert_eq!(listed.get(), 2);
        // “没有会话”的结果不记：下一次照样现查，切换前用的一定是刚查到的
        assert!(!waiting(t0 + secs(16), false));
        assert_eq!(listed.get(), 3);
        assert!(waiting(t0 + secs(17), true));
        assert!(waiting(t0 + secs(31), false));
        assert_eq!(listed.get(), 4);
        assert!(!waiting(t0 + secs(32), false));
        assert_eq!(listed.get(), 5);
    }
}
