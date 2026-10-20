//! Match a cached Desktop identity to its current account UUID before importing email.
use super::{desktop_app::profile_dir, store::*};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::Command;

fn bounded(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)
        .ok()?
        .take((limit + 1) as u64)
        .read_to_end(&mut data)
        .ok()?;
    (data.len() <= limit).then_some(data)
}

fn body(data: &[u8]) -> Option<Value> {
    let mut decoded = Vec::new();
    let bytes = if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        ruzstd::decoding::StreamingDecoder::new_with_max_window_size(data, 1024 * 1024)
            .ok()?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut decoded)
            .ok()?;
        if decoded.len() > 1024 * 1024 {
            return None;
        }
        decoded.as_slice()
    } else {
        data
    };
    serde_json::Deserializer::from_slice(bytes)
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn cached_email(blob: &[u8], owner: &str) -> Option<String> {
    if blob.len() < 24 || blob[..8] != [0x30, 0x5c, 0x72, 0xa7, 0x1b, 0x6d, 0xfb, 0xfc] {
        return None;
    }
    let length = u32::from_le_bytes(blob[12..16].try_into().ok()?) as usize;
    if length == 0 || length > 8192 || 24 + length >= blob.len() {
        return None;
    }
    let key = std::str::from_utf8(&blob[24..24 + length]).ok()?;
    if key.strip_prefix("1/0/").unwrap_or(key) != "https://claude.ai/api/account" {
        return None;
    }
    let parsed = body(&blob[24 + length..])?;
    let account = parsed.get("account").unwrap_or(&parsed);
    let uuid = s(account, "uuid").or_else(|| s(account, "account_uuid"))?;
    if !uuid.eq_ignore_ascii_case(owner) {
        return None;
    }
    let email = s(account, "email_address").or_else(|| s(account, "email"))?;
    let (local, domain) = email.split_once('@')?;
    if local.is_empty()
        || !domain.contains('.')
        || domain.contains('@')
        || email.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(email.to_string())
}

pub fn desktop_identity(store: &Store, acc: &Value) -> R<Value> {
    let dir = profile_dir(store, acc);
    let config = bounded(&dir.join("config.json"), 1024 * 1024)
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or(Value::Null);
    let owner = s(&config, "lastKnownAccountUuid")
        .filter(|s| !s.is_empty())
        .ok_or("没有可核对的客户端身份，请先在这个配置里登录，或手动填写邮箱。")?;
    if s(acc, "desktop_account_uuid").is_some_and(|id| !id.eq_ignore_ascii_case(owner)) {
        return Err("这个配置已登录其它账号，请先确认账号与配置的对应关系。".into());
    }
    let mut newest = None;
    if let Ok(entries) = fs::read_dir(dir.join("Cache/Cache_Data")) {
        for entry in entries.filter_map(Result::ok) {
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.ends_with("_0"))
            {
                continue;
            }
            let Some(blob) = bounded(&entry.path(), 512 * 1024) else {
                continue;
            };
            let Some(email) = cached_email(&blob, owner) else {
                continue;
            };
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if modified > std::time::SystemTime::now() + std::time::Duration::from_secs(300) {
                continue;
            }
            if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
                newest = Some((modified, email));
            }
        }
    }
    let (_, email) = newest.ok_or("没有找到与当前客户端身份匹配的邮箱缓存。请在 Claude 中打开账号设置后重试，或手动填写邮箱。")?;
    Ok(json!({"email":email,"desktop_account_uuid":owner}))
}

pub fn allowed_external_url(value: &str) -> bool {
    if value.len() > 16384 || value.chars().any(char::is_control) {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return false;
    }
    let host = url.host_str().unwrap_or("");
    if ["claude.ai", "claude.com", "anthropic.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    {
        return true;
    }
    if host == "accounts.google.com" {
        return url.path() == "/o/oauth2/v2/auth";
    }
    if host == "login.microsoftonline.com" {
        let parts: Vec<_> = url.path().split('/').collect();
        return parts.len() == 5
            && !parts[1].is_empty()
            && parts[1]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            && parts[2..] == ["oauth2", "v2.0", "authorize"];
    }
    false
}

pub fn open_external(value: &str) -> R<()> {
    if !allowed_external_url(value) {
        return Err(
            "只允许打开 Google / Microsoft 授权页或 Claude / Anthropic 的 HTTPS 验证地址".into(),
        );
    }
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        Command::new("xdg-open")
    };
    let status = command
        .arg(value)
        .status()
        .map_err(|_| "无法打开系统浏览器，请复制地址在浏览器中打开")?;
    if status.success() {
        Ok(())
    } else {
        Err("系统浏览器未打开，请复制地址重试".into())
    }
}

pub fn verification_target(store: &Store, id: &str, value: &str) -> R<Option<Value>> {
    let host = url::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(String::from));
    if !allowed_external_url(value)
        || !host.as_deref().is_some_and(|host| {
            ["claude.ai", "claude.com", "anthropic.com"]
                .iter()
                .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        })
    {
        return Err("验证链接必须是 Claude / Anthropic 官方 HTTPS 地址".into());
    }
    let entry = super::vault::get_entry(store, id)?;
    let Some(account_id) = s(&entry, "account_id").filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let data = store.load()?;
    let account = accounts(&data)[find_index(&data, account_id)?].clone();
    if let (Some(expected), Some(mailbox)) = (s(&account, "email"), s(&entry, "email")) {
        if !expected.is_empty() && !mailbox.is_empty() && !expected.eq_ignore_ascii_case(mailbox) {
            return Err("绑定的 Claude 账号与邮箱条目不一致，请先核对绑定关系".into());
        }
    }
    Ok(Some(account))
}

pub fn open_verification(store: &Store, id: &str, value: &str) -> R<Value> {
    // 与切换/移除互斥，确保验证链接交给正在登录的那个配置。
    let _guard = try_lock_file(&store.root.join("desktop-switch.lock"))?
        .ok_or("账号正在切换，请稍后打开验证链接")?;
    let target = verification_target(store, id, value)?;
    if let Some(account) = target.as_ref() {
        match s(account, "kind") {
            Some("claude_web") => {
                super::browser::open_web_url(store, account, value)?;
                return Ok(json!({"ok":true,"message":"已在绑定账号的独立浏览器窗口打开验证链接"}));
            }
            Some("claude_desktop") => {
                super::desktop_app::ensure_verification_target(store, account)?
            }
            _ => {}
        }
    }
    open_external(value)?;
    Ok(json!({"ok":true,"message":"已在本机浏览器打开验证链接，客户端保持运行"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(account: &str, email: &str, endpoint: &str) -> Vec<u8> {
        let key = format!("1/0/https://claude.ai{endpoint}");
        let mut blob = vec![0x30, 0x5c, 0x72, 0xa7, 0x1b, 0x6d, 0xfb, 0xfc];
        blob.extend_from_slice(&5u32.to_le_bytes());
        blob.extend_from_slice(&(key.len() as u32).to_le_bytes());
        blob.extend_from_slice(&[0u8; 8]);
        blob.extend_from_slice(key.as_bytes());
        blob.extend_from_slice(
            json!({"uuid":account,"email_address":email})
                .to_string()
                .as_bytes(),
        );
        blob.extend_from_slice(&[0xabu8; 32]);
        blob
    }
    #[test]
    fn email_requires_matching_owner_and_identity_endpoint() {
        assert_eq!(
            cached_email(&entry("A", "work@example.com", "/api/account"), "A"),
            Some("work@example.com".into())
        );
        assert!(cached_email(&entry("B", "other@example.com", "/api/account"), "A").is_none());
        assert!(cached_email(&entry("A", "work@example.com", "/api/organizations"), "A").is_none());
        assert!(cached_email(&entry("A", "invalid", "/api/account"), "A").is_none());
    }
    #[test]
    fn external_urls_reject_offsite_lookalikes_and_local_schemes() {
        for value in [
            "https://claude.ai/magic-link?token=example",
            "https://accounts.google.com/o/oauth2/v2/auth?state=example",
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        ] {
            assert!(allowed_external_url(value), "{value}");
        }
        for value in [
            "file:///private/tmp/a",
            "http://claude.ai/login",
            "https://claude.ai.evil.example/login",
            "https://evil.example/?next=https://claude.ai",
            "https://accounts.google.com/anything",
            "https://user:secret@claude.ai/login",
        ] {
            assert!(!allowed_external_url(value), "{value}");
        }
    }
}
