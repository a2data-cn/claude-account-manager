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

pub fn read_live() -> (Option<Value>, Option<&'static str>) {
    if use_keychain() {
        if let Some(v) = keychain_read() {
            return (Some(v), Some("keychain"));
        }
    }
    match read_json(&credentials_file()) {
        Ok(Some(v)) => (Some(v), Some("file")),
        _ => (None, None),
    }
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

pub fn read_oauth_account() -> Value {
    read_json(&claude_json_file())
        .ok()
        .flatten()
        .and_then(|v| v.get("oauthAccount").cloned())
        .unwrap_or(Value::Null)
}

/// 只替换 oauthAccount，~/.claude.json 其它字段原样保留。
pub fn write_oauth_account(oa: &Value) -> R<()> {
    let path = claude_json_file();
    let mut cj = read_json(&path)?.filter(|v| v.is_object()).unwrap_or_else(|| json!({}));
    let obj = cj.as_object_mut().unwrap();
    if oa.is_null() {
        obj.remove("oauthAccount");
    } else {
        obj.insert("oauthAccount".into(), oa.clone());
    }
    write_json_atomic(&path, &cj, false)
}

pub fn oauth_block(creds: &Value) -> Value {
    creds.get("claudeAiOauth").cloned().unwrap_or_else(|| json!({}))
}

fn token_fp(rt: &str) -> String {
    let mut h = Sha256::new();
    h.update(rt.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

pub fn identify(data: &Value, oa: &Value, creds: &Value) -> Option<usize> {
    let uuid = s(oa, "accountUuid");
    let email = s(oa, "emailAddress").map(|x| x.to_lowercase());
    let rt = oauth_block(creds)
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .map(String::from);
    let list: Vec<(usize, &Value)> = accounts(data)
        .iter()
        .enumerate()
        .filter(|(_, a)| s(a, "kind") == Some("claude_code"))
        .collect();
    if let Some(u) = uuid {
        if let Some((i, _)) = list.iter().find(|(_, a)| s(a, "account_uuid") == Some(u)) {
            return Some(*i);
        }
    }
    if let Some(e) = email {
        if let Some((i, _)) = list
            .iter()
            .find(|(_, a)| s(a, "email").map(|x| x.to_lowercase()) == Some(e.clone()))
        {
            return Some(*i);
        }
    }
    if let Some(rt) = rt {
        let fp = token_fp(&rt);
        if let Some((i, _)) = list.iter().find(|(_, a)| s(a, "token_fp") == Some(fp.as_str())) {
            return Some(*i);
        }
    }
    None
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

/// 把当前生效的登录保存回对应账号；未知账号按需自动导入。返回账号下标。
pub fn snapshot_live(store: &Store, data: &mut Value, label: Option<&str>, create: bool) -> R<Option<usize>> {
    let (creds, where_) = read_live();
    let creds = match creds {
        Some(c) if oauth_block(&c).get("accessToken").and_then(|v| v.as_str()).is_some() => c,
        _ => return Ok(None),
    };
    let oa = read_oauth_account();
    let idx = match identify(data, &oa, &creds) {
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
            let lbl = label
                .map(String::from)
                .or_else(|| s(&oa, "emailAddress").map(String::from))
                .unwrap_or_else(|| "未命名账号".into());
            accounts_mut(data).push(json!({
                "id": id, "kind": "claude_code", "label": lbl, "email": null, "plan": null,
                "org_name": null, "account_uuid": null, "manual": {}, "usage": null,
                "created_at": now_iso(), "last_switched_at": now_iso()
            }));
            if active_code_id(data).is_none() {
                push_switch_log(data, &id);
            }
            accounts(data).len() - 1
        }
    };
    {
        let acc = &mut accounts_mut(data)[idx];
        apply_meta(acc, &oa, &creds);
    }
    let id = s(&accounts(data)[idx], "id").unwrap_or_default().to_string();
    store.save_secret(
        &id,
        &json!({
            "claudeAiOauth": oauth_block(&creds), "oauthAccount": oa, "raw": creds,
            "store": where_, "saved_at": now_iso()
        }),
    )?;
    set_active_code(data, Some(&id));
    Ok(Some(idx))
}

fn backup_live(store: &Store, tag: &str) -> R<PathBuf> {
    let d = store.new_backup_dir(tag)?;
    let (creds, where_) = read_live();
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
            store.save(&mut data)?;
            return Ok(SwitchResult {
                id: tid,
                label,
                changed: false,
                backup: None,
            });
        }
    }
    let secret = store.load_secret(&tid);
    let ob = secret.get("claudeAiOauth").cloned().unwrap_or(Value::Null);
    if s(&ob, "refreshToken").is_none() {
        // 快照可能新增了账号，先保存
        store.save(&mut data)?;
        return Err("该账号没有保存可用的凭据，请先用这个账号登录 Claude Code 后再导入".into());
    }
    // 3：备份
    let backup = backup_live(store, &format!("before-{}", tid))?;
    // 4：写入
    let (_, where_) = read_live();
    let mut raw = secret
        .get("raw")
        .cloned()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    raw["claudeAiOauth"] = ob;
    write_live(&raw, where_)?;
    write_oauth_account(secret.get("oauthAccount").unwrap_or(&Value::Null))?;
    // 5：记录
    set_active_code(&mut data, Some(&tid));
    accounts_mut(&mut data)[ti]["last_switched_at"] = json!(now_iso());
    push_switch_log(&mut data, &tid);
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
    let idx = snapshot_live(store, &mut data, label, true)?
        .ok_or("当前没有登录 Claude Code。请先运行 claude 并登录，再导入")?;
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
