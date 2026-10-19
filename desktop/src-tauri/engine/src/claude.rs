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

/// 用 /api/oauth/profile 问出这个 access token 真正属于谁，返回 (uuid, email)；问不到返回 None。
pub fn token_owner(access_token: &str) -> Option<(Option<String>, Option<String>)> {
    let (code, prof) = super::usage::get_oauth(access_token, "/api/oauth/profile", 6);
    if code != 200 {
        return None;
    }
    let a = prof.get("account")?;
    let uuid = s(a, "uuid").map(String::from);
    let email = s(a, "email").map(String::from);
    if uuid.is_none() && email.is_none() {
        return None;
    }
    Some((uuid, email))
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
/// 凭据和 ~/.claude.json 里的 oauthAccount 对不上时，以凭据为准：token 存进它真正的主人名下，
/// 不会覆盖别的账号，并在 data["live_mismatch"] 里留下说明。逻辑与 cam.py 的 snapshot_live 一致。
pub fn snapshot_live(store: &Store, data: &mut Value, label: Option<&str>, create: bool) -> R<Option<usize>> {
    let (creds, where_) = read_live();
    let creds = match creds {
        Some(c) if s(&oauth_block(&c), "accessToken").is_some() => c,
        _ => return Ok(None),
    };
    let ob = oauth_block(&creds);
    let oa = read_oauth_account();
    let has_oa = oa.as_object().map(|o| !o.is_empty()).unwrap_or(false);
    let (oa_uuid, oa_email) = (s(&oa, "accountUuid"), s(&oa, "emailAddress"));
    let mut idx = identify(data, &oa, &creds);
    let live_fp = token_fp(s(&ob, "refreshToken").unwrap_or(""));
    let by_token = idx
        .map(|i| s(&accounts(data)[i], "token_fp") == Some(live_fp.as_str()))
        .unwrap_or(false);
    let mut owner: Option<(Option<String>, Option<String>)> = None;
    if !by_token {
        // token 换过了，只能靠 oauthAccount 认人；先联网核对一下这份凭据到底是谁的
        owner = token_owner(s(&ob, "accessToken").unwrap_or(""));
        if let Some((ou, oe)) = &owner {
            let agrees = idx
                .map(|i| identity_agrees(&accounts(data)[i], ou.as_deref(), oe.as_deref()))
                .unwrap_or(false);
            if !agrees {
                idx = match_identity(data, ou.as_deref(), oe.as_deref());
            }
        }
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
    let saved_oa = if oa_ok {
        if let Some(o) = data.as_object_mut() {
            o.remove("live_mismatch");
        }
        oa.clone()
    } else {
        // oauthAccount 是别人的：不用它改这个账号的资料，也不把它存进这个账号
        let prev = store.load_secret(&id).get("oauthAccount").cloned().unwrap_or(Value::Null);
        if has_oa {
            let label = s(&accounts(data)[idx], "label").unwrap_or("").to_string();
            data["live_mismatch"] = json!({
                "at": now_iso(), "account": id, "claude_json_email": oa_email,
                "message": format!(
                    "Claude Code 实际在用的登录属于“{}”，但 ~/.claude.json 里写的是 {}。通常是切换前打开的 claude 会话刷新 token 后写回了旧账号。退出所有 claude 会话后重新切换一次即可。",
                    label, oa_email.unwrap_or("-")
                )
            });
        }
        prev
    };
    {
        let acc = &mut accounts_mut(data)[idx];
        apply_meta(acc, if oa_ok { &saved_oa } else { &Value::Null }, &creds);
    }
    store.save_secret(
        &id,
        &json!({
            "claudeAiOauth": ob, "oauthAccount": saved_oa, "raw": creds,
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
    if let Some(o) = data.as_object_mut() {
        o.remove("live_mismatch"); // 凭据和 oauthAccount 现在是同一个账号的
    }
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
    let me = std::process::id();
    let mut found = Vec::new();
    if cfg!(target_os = "windows") {
        let ps = "Get-CimInstance Win32_Process -Filter \"Name='claude.exe' or Name='node.exe'\" | ForEach-Object { \"$($_.ProcessId)`t$($_.Name)`t$($_.CommandLine)\" }";
        if let Ok(out) = Command::new("powershell").args(["-NoProfile", "-Command", ps]).output() {
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

pub fn running_warning(procs: &[(u32, String)]) -> Option<String> {
    if procs.is_empty() {
        return None;
    }
    let mut pids: Vec<String> = procs.iter().take(5).map(|(p, _)| p.to_string()).collect();
    if procs.len() > 5 {
        pids.push("…".into());
    }
    Some(format!(
        "有 {} 个 claude 进程正在运行（pid {}），它们还在用切换前的账号，并可能在刷新 token 后把旧账号写回。请退出这些会话后重开（可用 claude --continue 接着聊）。",
        procs.len(),
        pids.join("、")
    ))
}
