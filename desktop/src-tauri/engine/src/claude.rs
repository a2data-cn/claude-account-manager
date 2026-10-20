//! Claude Code 凭据的读取、快照、切换。逻辑与 cam.py 一一对应。

use super::store::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
    hex::decode(&raw).ok().and_then(|b| serde_json::from_slice(&b).ok())
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
    creds.get("claudeAiOauth").cloned().unwrap_or_else(|| json!({}))
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
        if let Some(i) = list.iter().find(|&&i| s(&accs[i], "account_uuid") == Some(u)) {
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
    ob.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0) > chrono::Utc::now().timestamp_millis()
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
    let id = s(&accounts(data)[idx], "id").unwrap_or_default().to_string();
    let prev = store.load_secret(&id);
    let same_rt = prev.get("claudeAiOauth").and_then(|o| s(o, "refreshToken")) == s(ob, "refreshToken");
    let saved_oa = match oauth_account {
        Some(oa) => oa.clone(),
        None => prev.get("oauthAccount").cloned().unwrap_or(Value::Null),
    };
    let saved_at = match s(&prev, "saved_at") {
        Some(t) if same_rt => t.to_string(),
        _ => now_iso(),
    };
    apply_meta(&mut accounts_mut(data)[idx], oauth_account.unwrap_or(&Value::Null), creds);
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
    let same_rt = prev.get("claudeAiOauth").and_then(|o| s(o, "refreshToken")) == s(ob, "refreshToken");
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
    let who = label.map(|l| format!("可能是“{}”，", l)).unwrap_or_default();
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
        Some(at) => chrono::Utc::now().signed_duration_since(at) > chrono::Duration::days(PENDING_MAX_DAYS),
        None => true,
    }
}

/// 暂存的登录已核对出主人：归到它名下；账号里已有更新的就不覆盖；没有这个账号就新建。
fn file_pending(
    store: &Store,
    data: &mut Value,
    p: &Value,
    ob: &Value,
    owner: &Owner,
) -> R<usize> {
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
        && identity_agrees(&accounts(data)[idx], s(&oa, "accountUuid"), s(&oa, "emailAddress"));
    let creds = p.get("raw").cloned().unwrap_or(Value::Null);
    file_credentials(store, data, idx, ob, &creds, s(p, "store"), if oa_ok { Some(&oa) } else { None })?;
    Ok(idx)
}

/// 处理之前暂存的登录：已有账号存着同一个 token、或放了太久就丢掉；能联网核对了就归档。
fn resolve_pending(store: &Store, data: &mut Value, live_fp: Option<&str>) -> R<()> {
    let p = store.load_secret(PENDING_ID);
    if is_empty_obj(&p) {
        return Ok(());
    }
    let ob = p.get("claudeAiOauth").cloned().unwrap_or_else(|| json!({}));
    let fp = token_fp(s(&ob, "refreshToken").unwrap_or(""));
    if live_fp == Some(fp.as_str()) {
        return Ok(()); // 就是当前生效的这份登录，走 snapshot_live 的正常流程
    }
    let filed = code_indices(data)
        .into_iter()
        .any(|i| s(&accounts(data)[i], "token_fp") == Some(fp.as_str()));
    if pending_is_stale(&p) || filed {
        store.delete_secret(PENDING_ID);
        return Ok(());
    }
    if !access_token_fresh(&ob) {
        return Ok(()); // 核对要 access token 还有效；切换到候选账号时会用 refresh token 换一个再核对
    }
    let (code, owner) = probe_token_owner(s(&ob, "accessToken").unwrap_or(""));
    if let Some(o) = owner {
        file_pending(store, data, &p, &ob, &o)?;
        store.delete_secret(PENDING_ID);
    } else if code != 0 {
        store.delete_secret(PENDING_ID); // 服务端明确不认这个 token 了，留着也没用
    }
    Ok(())
}

/// 切换到 target 之前：暂存的登录可能是它的、又比它保存的新，现在核对一下。
/// access token 过期就用 refresh token 换一个再问（这时没有 claude 会话在用它，不会互相抢）。
/// 核对出来是谁的就归到谁名下：真是 target 的，切换就用上最新的 token；其实是别人的，也不会写进 target。
fn adopt_pending_for(store: &Store, data: &mut Value, target_id: &str) -> R<()> {
    let mut p = store.load_secret(PENDING_ID);
    if s(&p, "candidate") != Some(target_id) {
        return Ok(());
    }
    let tsec = store.load_secret(target_id);
    if newer(s(&tsec, "saved_at"), s(&p, "at")) {
        return Ok(()); // target 后来又拿到过更新的 token，暂存的这份对它没用了
    }
    let mut ob = p.get("claudeAiOauth").cloned().unwrap_or_else(|| json!({}));
    let mut owner = match s(&ob, "accessToken") {
        Some(at) if access_token_fresh(&ob) => token_owner(at),
        _ => None,
    };
    if owner.is_none() {
        if let Some(rt) = s(&ob, "refreshToken").map(String::from) {
            if running_claude_processes().is_empty() {
                match super::usage::refresh_oauth(&rt) {
                    Ok(tok) => {
                        ob["accessToken"] = tok["access_token"].clone();
                        if let Some(r) = s(&tok, "refresh_token") {
                            ob["refreshToken"] = json!(r);
                        }
                        let secs = tok.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(28800);
                        ob["expiresAt"] = json!(chrono::Utc::now().timestamp_millis() + secs * 1000);
                        if !p.get("raw").map(|r| r.is_object()).unwrap_or(false) {
                            p["raw"] = json!({});
                        }
                        p["raw"]["claudeAiOauth"] = ob.clone();
                        owner = token_owner(s(&ob, "accessToken").unwrap_or(""));
                    }
                    Err(e) => {
                        if e.contains("登录已失效") {
                            store.delete_secret(PENDING_ID); // refresh token 本身已作废，这份登录没用了
                        }
                        return Ok(());
                    }
                }
            }
        }
    }
    let Some(o) = owner else {
        return Ok(()); // 还是核对不了：暂存留着，这次用 target 已保存的登录
    };
    file_pending(store, data, &p, &ob, &o)?;
    store.delete_secret(PENDING_ID);
    Ok(())
}

/// 把当前生效的登录保存回对应账号；未知账号按需自动导入。返回账号下标。
/// 只有确认了这份登录是谁的才会存进账号：refresh token 指纹和某个账号保存的一致，或者联网用
/// /api/oauth/profile 问出了主人。两样都做不到（离线、access token 过期）时暂存起来，不碰任何账号。
/// 凭据和 ~/.claude.json 里的 oauthAccount 对不上时，以凭据为准：token 存进它真正的主人名下，
/// 不会覆盖别的账号，并在 data["live_mismatch"] 里留下说明。逻辑与 cam.py 的 snapshot_live 一致。
pub fn snapshot_live(store: &Store, data: &mut Value, label: Option<&str>, create: bool) -> R<Option<usize>> {
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
            owner = o;
            unreachable = code == 0;
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
    if pend.get("claudeAiOauth").and_then(|o| s(o, "refreshToken")).map(token_fp) == Some(live_fp.clone()) {
        store.delete_secret(PENDING_ID); // 暂存的就是这份，现在认出来了
    }
    // oauthAccount 是否描述的就是这份凭据的主人
    let oa_ok = has_oa
        && match idx {
            Some(i) => identity_agrees(&accounts(data)[i], oa_uuid, oa_email),
            None => match &owner {
                None => true,
                Some((ou, oe)) => identity_agrees(&json!({"account_uuid": ou, "email": oe}), oa_uuid, oa_email),
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
    let id = s(&accounts(data)[idx], "id").unwrap_or_default().to_string();
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
    file_credentials(store, data, idx, &ob, &creds, where_, if oa_ok { Some(&oa) } else { None })?;
    set_active_code(data, Some(&id));
    Ok(Some(idx))
}

fn backup_live(store: &Store, tag: &str) -> R<PathBuf> {
    let d = store.new_backup_dir(tag)?;
    let (creds, where_) = read_live()?;
    if let Some(c) = creds {
        write_json_atomic(&d.join("credentials.json"), &json!({"store": where_, "data": c}), true)?;
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
    let saved = store.load_secret(id).get("oauthAccount").cloned().unwrap_or(Value::Null);
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

pub fn switch_account(store: &Store, key: &str) -> R<SwitchResult> {
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
    adopt_pending_for(store, &mut data, &tid)?; // 暂存的登录要是 target 的，先核对并存进去，下面就能用上
    let secret = store.load_secret(&tid);
    let ob = secret.get("claudeAiOauth").cloned().unwrap_or(Value::Null);
    if s(&ob, "refreshToken").is_none() {
        // 快照可能新增了账号，先保存
        store.save(&mut data)?;
        return Err("该账号没有保存可用的凭据，请先用这个账号登录 Claude Code 后再导入".into());
    }
    // 3：备份
    let backup = backup_live(store, &format!("before-{}", tid))?;
    // 4：先写 ~/.claude.json 再写凭据，凭据写失败就把 oauthAccount 改回去。
    // 万一中途崩溃，凭据还是原来那个号的，再切一次就能完成；反过来的话会被当成“已经是当前账号”。
    let (_, where_) = read_live()?;
    let mut raw = secret
        .get("raw")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    raw["claudeAiOauth"] = ob;
    let prev_oa = read_oauth_account()?;
    write_oauth_account(secret.get("oauthAccount").unwrap_or(&Value::Null))?;
    if let Err(e) = write_live(&raw, where_) {
        let _ = write_oauth_account(&prev_oa); // 回滚失败时保留原来的错误
        return Err(e);
    }
    if let Some(o) = data.as_object_mut() {
        o.remove("live_mismatch"); // 凭据和 oauthAccount 现在是同一个账号的
        o.remove("live_unverified"); // 现在生效的登录是我们刚写的，当然认识
        o.remove("pending_switch"); // 这次切换优先于之前登记的等待切换
    }
    // 5：记录
    set_active_code(&mut data, Some(&tid));
    accounts_mut(&mut data)[ti]["last_switched_at"] = json!(now_iso());
    push_switch_log(&mut data, &tid);
    let loc = super::health::cached_region_loc(&data, super::health::REGION_FRESH_SECS); // 给风险提示记一笔地区
    super::health::note_account_region(&mut data, Some(&tid), loc.as_deref());
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
    if let Some(m) = data.get("live_unverified").and_then(|u| s(u, "message")).map(String::from) {
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
    if low_exe.contains("/claude.app/") || low_exe.contains("anthropicclaude") || low_exe.contains("/windowsapps/") {
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

/// 正在运行的 Claude Code 进程 (pid, 命令行)，不含本进程。查不到时返回空列表。
/// 切换账号后，这些进程内存里还是旧账号的 token，刷新后还可能把旧账号写回凭据文件。
pub fn running_claude_processes() -> Vec<(u32, String)> {
    if let Some(v) = std::env::var_os("CAM_TEST_CLAUDE_PIDS") {
        // 测试用：跑测试的机器上可能真有 claude 在运行，由测试指定“有哪些”
        return v
            .to_string_lossy()
            .split(',')
            .filter_map(|p| p.trim().parse::<u32>().ok())
            .map(|p| (p, "claude".to_string()))
            .collect();
    }
    let me = std::process::id();
    let mut found = Vec::new();
    if cfg!(target_os = "windows") {
        // 用完整路径而不是 Name：桌面 App 的判断看的是路径，只给文件名的话
        // Claude 桌面 App（AnthropicClaude\…\claude.exe）会被当成 Claude Code
        let ps = "Get-CimInstance Win32_Process -Filter \"Name='claude.exe' or Name='node.exe' or Name='bun.exe'\" | ForEach-Object { \"$($_.ProcessId)`t$(if ($_.ExecutablePath) { $_.ExecutablePath } else { $_.Name })`t$($_.CommandLine)\" }";
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-Command", ps]);
        #[cfg(windows)]
        {
            // 桌面版是 GUI 程序（windows_subsystem = "windows"），不加这个会闪一下控制台窗口
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        if let Ok(out) = c.output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if let [pid, name, cmd] = parts[..] {
                    if let Ok(pid) = pid.trim().parse::<u32>() {
                        if pid != me && is_claude_cmd(name, cmd) {
                            found.push((pid, if cmd.is_empty() { name } else { cmd }.to_string()));
                        }
                    }
                }
            }
        }
    } else if std::path::Path::new("/proc/self").exists() {
        let Ok(rd) = fs::read_dir("/proc") else { return found };
        for e in rd.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
            if pid == me {
                continue;
            }
            let Ok(raw) = fs::read(e.path().join("cmdline")) else { continue };
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
        let ps = |field: &str| -> std::collections::HashMap<u32, String> {
            let mut m = std::collections::HashMap::new();
            if let Ok(out) = Command::new("ps").args(["-A", "-o", &format!("pid=,{}=", field)]).output() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let line = line.trim();
                    let (pid, rest) = line.split_once(' ').unwrap_or((line, ""));
                    if let Ok(pid) = pid.parse::<u32>() {
                        m.insert(pid, rest.trim().to_string());
                    }
                }
            }
            m
        };
        let (comm, args) = (ps("comm"), ps("args"));
        for (pid, exe) in comm {
            let cmd = args.get(&pid).cloned().unwrap_or_else(|| exe.clone());
            if pid != me && is_claude_cmd(&exe, &cmd) {
                found.push((pid, cmd));
            }
        }
    }
    found.sort();
    found
}

pub fn running_summary(procs: &[(u32, String)]) -> String {
    let mut pids: Vec<String> = procs.iter().take(5).map(|(p, _)| p.to_string()).collect();
    if procs.len() > 5 {
        pids.push("…".into());
    }
    format!("有 {} 个 claude 进程正在运行（pid {}）", procs.len(), pids.join("、"))
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

/// 登记中的“等会话结束再切换”，给界面显示用；没有或目标账号已删除则 Null。与 cam.py 一致。
pub fn pending_switch_view(data: &Value) -> Value {
    let Some(p) = data.get("pending_switch").filter(|p| p.is_object()) else {
        return Value::Null;
    };
    let Some(acc) = accounts(data).iter().find(|a| s(a, "id") == s(p, "account")) else {
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
    data["pending_switch"] = json!({"account": acc_id, "reason": reason, "rule": rule_id, "at": now_iso()});
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

pub fn apply_pending_switch(store: &Store) -> R<Option<(SwitchResult, String)>> {
    apply_pending_switch_at(store, chrono::Local::now().naive_local())
}

/// 后台每轮调用：登记过的等待切换，在没有 claude 会话运行时执行。返回 (切换结果, reason)。
/// 目标账号没了、已经是当前账号、或时间规则的时间段已经过了，就作废。切换本身走 switch_account，
/// 它会清掉登记。与 cam.py 的 apply_pending_switch 一致。
pub fn apply_pending_switch_at(store: &Store, now: chrono::NaiveDateTime) -> R<Option<(SwitchResult, String)>> {
    let data = store.load()?;
    if !data.get("pending_switch").map(|p| p.is_object()).unwrap_or(false) {
        return Ok(None);
    }
    if !running_claude_processes().is_empty() {
        return Ok(None);
    }
    let (target, reason) = {
        let _g = store.lock()?;
        let mut data = store.load()?;
        let Some(p) = data.get("pending_switch").filter(|p| p.is_object()).cloned() else {
            return Ok(None);
        };
        let target = s(&p, "account").unwrap_or_default().to_string();
        let known = accounts(&data).iter().any(|a| s(a, "id") == Some(target.as_str()));
        let mut drop = !known || active_code_id(&data).as_deref() == Some(target.as_str());
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
    let r = switch_account(store, &target)?;
    Ok(if r.changed { Some((r, reason)) } else { None })
}
