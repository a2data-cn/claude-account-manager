//! 只读 Claude Desktop 自己的额度响应缓存和用量历史；不读取 Cookie 或解密登录。
//! Chromium Simple Cache 的格式独立实现，只读取经过 URL 校验的 usage 响应正文。

use super::{desktop_app::profile_dir, store::*};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

const MAX_CACHE_INPUT: usize = 64 * 1024;
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAGIC: [u8; 8] = [0x30, 0x5c, 0x72, 0xa7, 0x1b, 0x6d, 0xfb, 0xfc];
const WINDOWS: [(&str, &str, i64); 8] = [
    ("five_hour", "fh", 5 * 3600),
    ("seven_day", "sd", 7 * 86400),
    ("seven_day_opus", "so", 7 * 86400),
    ("seven_day_sonnet", "sn", 7 * 86400),
    ("seven_day_oauth_apps", "oa", 7 * 86400),
    ("seven_day_cowork", "cw", 7 * 86400),
    ("seven_day_omelette", "om", 7 * 86400),
    ("omelette_promotional", "op", 7 * 86400),
];

#[derive(Clone)]
struct Snapshot {
    body: Value,
    timestamp: DateTime<Utc>,
    org: Option<String>,
    history: bool,
}

enum Selection {
    Found(Snapshot),
    Missing,
    Ambiguous,
}

fn uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || !bytes.iter().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn valid_percent(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
}

fn acceptable_time(time: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    time.timestamp_millis() > 0 && time.signed_duration_since(now).num_milliseconds() <= 300_000
}

fn iso(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn bounded_json(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_OUTPUT as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take((MAX_OUTPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_OUTPUT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn endpoint_org(key: &[u8]) -> Option<String> {
    let key = std::str::from_utf8(key).ok()?;
    let key = key.strip_prefix("1/0/").unwrap_or(key);
    let org = key
        .strip_prefix("https://claude.ai/api/organizations/")?
        .strip_suffix("/usage")?;
    uuid(org)
}

fn decode_body(body: &[u8]) -> Option<Value> {
    let bytes = if body.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        // StreamingDecoder stops at the end of its single frame; the Simple Cache
        // EOF record after that frame is not part of the compressed JSON.
        let decoder =
            ruzstd::decoding::StreamingDecoder::new_with_max_window_size(body, MAX_OUTPUT as u64)
                .ok()?;
        let mut decoded = Vec::new();
        decoder
            .take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut decoded)
            .ok()?;
        if decoded.len() > MAX_OUTPUT {
            return None;
        }
        decoded
    } else {
        if body.len() > MAX_OUTPUT {
            return None;
        }
        body.to_vec()
    };
    // Simple Cache stores an EOF record after the response. Parse exactly one JSON
    // value without treating those binary trailer bytes as part of the document.
    serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn has_window_value(body: &Value, history: bool) -> bool {
    WINDOWS.iter().any(|(name, short, _)| {
        if history {
            return body.get(*short).and_then(valid_percent).is_some();
        }
        let Some(window) = body.get(*name) else {
            return false;
        };
        if window.get("utilization").and_then(valid_percent).is_none() {
            return false;
        }
        match window.get("resets_at") {
            None | Some(Value::Null) => true,
            Some(Value::String(reset)) => parse_time(reset).is_some(),
            _ => false,
        }
    })
}

fn read_cache_entry(path: &Path, now: DateTime<Utc>) -> Option<Snapshot> {
    let mut file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() < 24 || meta.len() > MAX_CACHE_INPUT as u64 {
        return None;
    }
    let timestamp: DateTime<Utc> = meta.modified().ok()?.into();
    if !acceptable_time(timestamp, now) {
        return None;
    }
    let mut header = [0u8; 24];
    file.read_exact(&mut header).ok()?;
    if header[..8] != MAGIC {
        return None;
    }
    let key_len = u32::from_le_bytes(header[12..16].try_into().ok()?) as usize;
    if key_len == 0 || key_len > 8192 || 24 + key_len >= meta.len() as usize {
        return None;
    }
    let mut key = vec![0u8; key_len];
    file.read_exact(&mut key).ok()?;
    let org = endpoint_org(&key)?;
    // Only after authenticating the cache key do we read its HTTP response body.
    let mut body = Vec::new();
    file.take((MAX_CACHE_INPUT - 24 - key_len + 1) as u64)
        .read_to_end(&mut body)
        .ok()?;
    if body.len() + 24 + key_len > MAX_CACHE_INPUT {
        return None;
    }
    let body = decode_body(&body)?;
    if !has_window_value(&body, false) {
        return None;
    }
    Some(Snapshot {
        body,
        timestamp,
        org: Some(org),
        history: false,
    })
}

fn choose(mut candidates: Vec<Snapshot>, org_hint: Option<&str>) -> Selection {
    if let Some(hint) = org_hint {
        candidates.retain(|s| s.org.as_deref() == Some(hint));
    } else {
        let orgs: HashSet<&str> = candidates.iter().filter_map(|s| s.org.as_deref()).collect();
        if orgs.len() > 1 {
            return Selection::Ambiguous;
        }
    }
    candidates
        .into_iter()
        .max_by_key(|s| s.timestamp)
        .map(Selection::Found)
        .unwrap_or(Selection::Missing)
}

fn cache_snapshot(dir: &Path, org_hint: Option<&str>, now: DateTime<Utc>) -> Selection {
    let Ok(entries) = fs::read_dir(dir.join("Cache/Cache_Data")) else {
        return Selection::Missing;
    };
    let candidates = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with("_0")))
        .filter_map(|e| read_cache_entry(&e.path(), now))
        .collect();
    choose(candidates, org_hint)
}

fn history_snapshot(dir: &Path, org_hint: Option<&str>, now: DateTime<Utc>) -> Selection {
    let Some(history) = bounded_json(&dir.join("plan-usage-history.json")) else {
        return Selection::Missing;
    };
    let Some(samples) = history.get("samples").and_then(Value::as_array) else {
        return Selection::Missing;
    };
    let version = history.get("version").and_then(Value::as_u64);
    if !matches!(version, Some(1) | Some(2)) {
        return Selection::Missing;
    }
    let candidates = samples
        .iter()
        .filter_map(|sample| {
            let timestamp = DateTime::<Utc>::from_timestamp_millis(sample.get("t")?.as_i64()?)?;
            if !acceptable_time(timestamp, now) {
                return None;
            }
            let (body, org) = if version == Some(2) {
                let org = match sample.get("org") {
                    Some(Value::String(org)) => Some(uuid(org)?),
                    Some(Value::Null) | None => None,
                    _ => return None,
                };
                (sample.get("u")?.clone(), org)
            } else {
                (
                    json!({"fh": sample.get("fh"), "sd": sample.get("sd")}),
                    None,
                )
            };
            if !has_window_value(&body, true) {
                return None;
            }
            Some(Snapshot {
                body,
                timestamp,
                org,
                history: true,
            })
        })
        .collect();
    choose(candidates, org_hint)
}

fn empty_usage(now: DateTime<Utc>, status: &str, notice: &str) -> Value {
    let mut usage = json!({
        "source": "desktop_cache", "fetched_at": null, "checked_at": iso(now),
        "cache_stale": true, "cache_status": status, "cache_org_uuid": null, "notice": notice,
    });
    for (name, _, _) in WINDOWS {
        usage[name] = Value::Null;
    }
    usage
}

fn present(snapshot: Snapshot, now: DateTime<Utc>) -> Value {
    let age = now
        .signed_duration_since(snapshot.timestamp)
        .num_seconds()
        .max(0);
    let mut usage = empty_usage(now, "ok", "");
    usage["source"] = json!(if snapshot.history {
        "desktop_history"
    } else {
        "desktop_cache"
    });
    usage["fetched_at"] = json!(iso(snapshot.timestamp));
    usage["cache_stale"] = json!(age > 30 * 60);
    usage["cache_org_uuid"] = json!(snapshot.org);
    let mut available = false;
    for (name, short, lifetime) in WINDOWS {
        let (percent, reset) = if snapshot.history {
            (
                snapshot.body.get(short).and_then(valid_percent),
                Value::Null,
            )
        } else {
            let window = snapshot.body.get(name).unwrap_or(&Value::Null);
            let reset = window.get("resets_at").cloned().unwrap_or(Value::Null);
            (window.get("utilization").and_then(valid_percent), reset)
        };
        let Some(percent) = percent else {
            continue;
        };
        let valid = match &reset {
            Value::Null => age < lifetime,
            Value::String(reset) => parse_time(reset).is_some_and(|t| t > now),
            _ => false,
        };
        if valid {
            usage[name] = json!({"utilization": percent, "resets_at": reset});
            available = true;
        }
    }
    if !available {
        usage["cache_status"] = json!("expired");
        usage["notice"] =
            json!("本地额度快照已过期。请打开这个账号的 Claude 设置 → 用量，再回来刷新。");
    } else if snapshot.history {
        usage["notice"] = json!("显示 Claude 保存在本机的历史额度快照；历史文件没有重置时间。请在 Claude 设置 → 用量更新后再刷新。");
    } else if age > 30 * 60 {
        usage["notice"] =
            json!("显示超过 30 分钟的本地额度缓存。请在 Claude 设置 → 用量更新后再刷新。");
    } else {
        usage["notice"] = json!("读取 Claude 本机缓存；点击刷新不会向服务器请求最新额度。可先打开 Claude 设置 → 用量更新缓存。");
    }
    usage
}

/// 只过滤对外显示，保留存档的原采样时间；跨过重置时间后不能继续展示旧百分比。和 cam.py 的 _desktop_usage_view 一致
pub fn desktop_usage_view(usage: &Value) -> Value {
    desktop_usage_view_at(usage, Utc::now())
}

pub fn desktop_usage_view_at(usage: &Value, now: DateTime<Utc>) -> Value {
    let mut shown = usage.clone();
    let age = s(usage, "fetched_at")
        .and_then(parse_time)
        .map(|t| now.signed_duration_since(t).num_seconds())
        .unwrap_or(i64::MAX);
    shown["cache_stale"] = json!(age > 1800);
    let had = WINDOWS
        .iter()
        .any(|(n, _, _)| usage.get(*n).is_some_and(|w| !w.is_null()));
    let mut any = false;
    for (name, _, lifetime) in WINDOWS {
        let w = usage.get(name).cloned().unwrap_or(Value::Null);
        let percent_ok = w
            .as_object()
            .and_then(|o| o.get("utilization"))
            .and_then(valid_percent)
            .is_some();
        let valid = percent_ok
            && match w.get("resets_at") {
                Some(Value::String(r)) => parse_time(r).is_some_and(|t| t > now),
                Some(Value::Null) | None => age < lifetime,
                _ => false,
            };
        if valid {
            any = true;
            shown[name] = w;
        } else {
            shown[name] = Value::Null;
        }
    }
    if had && !any {
        shown["cache_status"] = json!("expired");
        shown["notice"] = json!("本地额度快照已过期，请在 Claude 设置中查看用量后刷新。");
    }
    shown
}

/// 返回 (用量, 可合并进账号的身份元数据)。所有路径均来自这个 Desktop 账号的 profile。
/// 没有缓存属于正常状态，不应回落到 API Key 或显示虚构的 0%。
pub fn fetch_desktop_usage(store: &Store, acc: &Value) -> R<(Value, Map<String, Value>)> {
    fetch_at(store, acc, Utc::now())
}

fn fetch_at(store: &Store, acc: &Value, now: DateTime<Utc>) -> R<(Value, Map<String, Value>)> {
    if s(acc, "kind") != Some("claude_desktop") {
        return Err("只有 Claude 桌面版账号可以读取桌面缓存".into());
    }
    let dir = profile_dir(store, acc);
    let config_path = dir.join("config.json");
    let config = bounded_json(&config_path);
    if config_path.exists() && !config.as_ref().is_some_and(Value::is_object) {
        return Ok((empty_usage(now, "missing", "Claude 本地配置暂时无法读取或格式损坏，未展示可能属于旧账号的额度。请关闭 Claude 后重试。"), Map::new()));
    }
    let config = config.unwrap_or(Value::Null);
    let observed = s(&config, "lastKnownAccountUuid")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let expected = s(acc, "desktop_account_uuid");
    let mut metadata = Map::new();
    if expected
        .zip(observed)
        .is_some_and(|(a, b)| !a.eq_ignore_ascii_case(b))
    {
        return Ok((
            empty_usage(
                now,
                "missing",
                "这个目录已登录另一个 Claude 账号，未展示旧账号额度。请重新绑定对应的桌面账号。",
            ),
            metadata,
        ));
    }
    if let Some(observed) = observed {
        metadata.insert("desktop_account_uuid".into(), json!(observed));
    }
    // Claude 的普通账户组织来自 Cookie lastActiveOrg，不是 config 的 deploymentOrganizationUuid。
    // 不读 Cookie，也不猜测组织键；仅接受用户/既有元数据明确提供的 org_uuid。
    let hint = s(acc, "org_uuid").and_then(uuid);
    let selected = match cache_snapshot(&dir, hint.as_deref(), now) {
        Selection::Ambiguous => Selection::Ambiguous,
        Selection::Missing => history_snapshot(&dir, hint.as_deref(), now),
        Selection::Found(cached) => match history_snapshot(&dir, hint.as_deref(), now) {
            Selection::Ambiguous => Selection::Ambiguous,
            Selection::Found(history)
                if hint.is_none()
                    && cached
                        .org
                        .as_ref()
                        .zip(history.org.as_ref())
                        .is_some_and(|(a, b)| a != b) =>
            {
                Selection::Ambiguous
            }
            Selection::Found(history) if history.timestamp > cached.timestamp => {
                Selection::Found(history)
            }
            _ => Selection::Found(cached),
        },
    };
    let usage = match selected {
        Selection::Found(snapshot) => present(snapshot, now),
        Selection::Ambiguous => empty_usage(
            now,
            "ambiguous",
            "这个目录含多个组织的额度缓存，无法确认当前组织。请明确账号的组织 UUID 后再刷新。",
        ),
        Selection::Missing => empty_usage(
            now,
            "missing",
            "尚未找到这个账号的本地额度缓存。请打开 Claude，进入设置 → 用量，再回来刷新。",
        ),
    };
    Ok((usage, metadata))
}

#[cfg(test)]
mod view_tests {
    use super::desktop_usage_view_at;
    use chrono::{Duration, Utc};
    use serde_json::json;

    #[test]
    fn hides_windows_past_their_reset() {
        // 存档里的采样是 10:00，五小时窗 10:30 重置；到 11:00 看，五小时的不能再显示旧百分比，七天的还在
        let now = Utc::now();
        let iso = |t: chrono::DateTime<Utc>| t.to_rfc3339();
        let usage = json!({"source": "desktop_cache", "cache_status": "ok", "notice": "x",
            "fetched_at": iso(now - Duration::minutes(60)),
            "five_hour": {"utilization": 40.0, "resets_at": iso(now - Duration::minutes(30))},
            "seven_day": {"utilization": 12.0, "resets_at": iso(now + Duration::days(2))}});
        let v = desktop_usage_view_at(&usage, now);
        assert!(v["five_hour"].is_null());
        assert_eq!(v["seven_day"]["utilization"], 12.0);
        assert_eq!(v["cache_status"], "ok");
        assert_eq!(v["cache_stale"], true);
        let all_old = json!({"source": "desktop_cache", "cache_status": "ok", "fetched_at": iso(now - Duration::hours(6)),
            "five_hour": {"utilization": 40.0, "resets_at": null}});
        let v = desktop_usage_view_at(&all_old, now);
        assert!(v["five_hour"].is_null());
        assert_eq!(v["cache_status"], "expired");
        assert_eq!(
            v["notice"],
            "本地额度快照已过期，请在 Claude 设置中查看用量后刷新。"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::time::SystemTime;

    const ORG: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const OTHER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    struct Fixture {
        store: Store,
        acc: Value,
        root: std::path::PathBuf,
        now: DateTime<Utc>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cam-desktop-usage-{}-{}",
                std::process::id(),
                new_id()
            ));
            let store = Store { root: root.clone() };
            let acc = json!({"id":"one", "kind":"claude_desktop", "desktop_profile":"one"});
            Self {
                store,
                acc,
                root,
                now: parse_time("2026-10-20T12:00:00Z").unwrap(),
            }
        }
        fn dir(&self) -> std::path::PathBuf {
            profile_dir(&self.store, &self.acc)
        }
        fn cache(&self, name: &str, bytes: &[u8], timestamp: DateTime<Utc>) {
            let path = self.dir().join("Cache/Cache_Data").join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(SystemTime::from(timestamp)))
                .unwrap();
        }
        fn history(&self, body: Value) {
            fs::create_dir_all(self.dir()).unwrap();
            fs::write(self.dir().join("plan-usage-history.json"), body.to_string()).unwrap();
        }
        fn usage(&self) -> Value {
            fetch_at(&self.store, &self.acc, self.now).unwrap().0
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn shared() -> Value {
        serde_json::from_str(include_str!("../../../../tests/shared/desktop_usage.json")).unwrap()
    }
    fn entry(org: &str, body: Value) -> Vec<u8> {
        let key = format!("1/0/https://claude.ai/api/organizations/{org}/usage");
        let mut bytes = vec![0u8; 24];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[12..16].copy_from_slice(&(key.len() as u32).to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(body.to_string().as_bytes());
        bytes.extend_from_slice(&[0xab; 32]);
        bytes
    }

    #[test]
    fn missing_cache_is_a_notice_without_key_error_or_fake_zero() {
        let f = Fixture::new();
        let usage = f.usage();
        assert_eq!(usage["cache_status"], "missing");
        assert!(usage["five_hour"].is_null());
        assert!(usage["fetched_at"].is_null());
        assert!(usage.get("error").is_none());
        assert!(!usage.to_string().contains("API Key"));
    }

    #[test]
    fn plaintext_and_zstd_chromium_entries_ignore_eof_trailers() {
        let fixtures = shared();
        for key in ["plain_entry_hex", "zstd_entry_hex"] {
            let f = Fixture::new();
            f.cache(
                "aaa_0",
                &hex::decode(fixtures[key].as_str().unwrap()).unwrap(),
                f.now,
            );
            let usage = f.usage();
            assert_eq!(usage["source"], "desktop_cache", "{key}: {usage}");
            assert_eq!(usage["five_hour"]["utilization"], 37.5, "{key}: {usage}");
            assert_eq!(usage["seven_day"]["utilization"], 68.25);
            assert_eq!(usage["cache_org_uuid"], ORG);
        }
    }

    #[test]
    fn cache_is_per_profile_and_latest_valid_mtime_wins() {
        let f = Fixture::new();
        f.cache(
            "old_0",
            &entry(ORG, json!({"five_hour":{"utilization":10}})),
            f.now - chrono::Duration::minutes(10),
        );
        f.cache(
            "new_0",
            &entry(ORG, json!({"five_hour":{"utilization":30}})),
            f.now - chrono::Duration::minutes(2),
        );
        f.cache("bad_0", b"broken", f.now);
        assert_eq!(f.usage()["five_hour"]["utilization"], 30.0);
        let other = json!({"id":"two", "kind":"claude_desktop", "desktop_profile":"two"});
        assert_eq!(
            fetch_at(&f.store, &other, f.now).unwrap().0["cache_status"],
            "missing"
        );
        assert_eq!(
            f.usage()["fetched_at"],
            iso(f.now - chrono::Duration::minutes(2))
        );
    }

    #[test]
    fn multiple_organizations_require_an_explicit_hint() {
        let mut f = Fixture::new();
        f.cache(
            "one_0",
            &entry(ORG, json!({"five_hour":{"utilization":10}})),
            f.now,
        );
        f.cache(
            "two_0",
            &entry(OTHER, json!({"five_hour":{"utilization":90}})),
            f.now,
        );
        assert_eq!(f.usage()["cache_status"], "ambiguous");
        f.acc["org_uuid"] = json!(ORG);
        assert_eq!(f.usage()["five_hour"]["utilization"], 10.0);
        assert_eq!(f.usage()["cache_org_uuid"], ORG);
    }

    #[test]
    fn expired_reset_is_hidden_without_manufacturing_zero() {
        let f = Fixture::new();
        f.cache(
            "expired_0",
            &entry(
                ORG,
                json!({"five_hour":{"utilization":80,"resets_at":"2026-10-20T11:00:00Z"}}),
            ),
            f.now,
        );
        let usage = f.usage();
        assert_eq!(usage["cache_status"], "expired");
        assert!(usage["five_hour"].is_null());
    }

    #[test]
    fn history_v1_v2_are_timestamped_and_age_each_window_independently() {
        let f = Fixture::new();
        let t = (f.now - chrono::Duration::hours(6)).timestamp_millis();
        for history in [
            json!({"version":1,"samples":[{"t":t,"fh":12,"sd":40}]}),
            json!({"version":2,"samples":[{"t":t,"org":ORG,"u":{"fh":12,"sd":40}}]}),
        ] {
            f.history(history);
            let usage = f.usage();
            assert_eq!(usage["source"], "desktop_history");
            assert!(usage["five_hour"].is_null());
            assert_eq!(usage["seven_day"]["utilization"], 40.0);
            assert!(usage["seven_day"]["resets_at"].is_null());
            assert_eq!(usage["cache_stale"], true);
            assert_eq!(usage["fetched_at"], iso(f.now - chrono::Duration::hours(6)));
        }
    }

    #[test]
    fn history_multiple_organizations_and_future_samples_are_rejected() {
        let f = Fixture::new();
        let t = f.now.timestamp_millis();
        f.history(json!({"version":2,"samples":[{"t":t,"org":ORG,"u":{"fh":10}},{"t":t,"org":OTHER,"u":{"fh":90}}]}));
        assert_eq!(f.usage()["cache_status"], "ambiguous");
        f.history(json!({"version":1,"samples":[{"t":t + 301000,"fh":30}]}));
        assert_eq!(f.usage()["cache_status"], "missing");
        f.history(json!({"version":1,"samples":[{"t":t - 8 * 86400000,"fh":30,"sd":40}]}));
        assert_eq!(f.usage()["cache_status"], "expired");
    }

    #[test]
    fn newest_source_wins_and_old_history_cannot_revive_expired_cache() {
        let f = Fixture::new();
        let older = f.now - chrono::Duration::minutes(10);
        f.cache(
            "old_0",
            &entry(ORG, json!({"five_hour":{"utilization":10}})),
            older,
        );
        f.history(
            json!({"version":2,"samples":[{"t":f.now.timestamp_millis(),"org":ORG,"u":{"fh":50}}]}),
        );
        assert_eq!(f.usage()["source"], "desktop_history");
        assert_eq!(f.usage()["five_hour"]["utilization"], 50.0);
        f.cache(
            "new_0",
            &entry(
                ORG,
                json!({"five_hour":{"utilization":90,"resets_at":"2026-10-20T11:00:00Z"}}),
            ),
            f.now,
        );
        f.history(
            json!({"version":2,"samples":[{"t":older.timestamp_millis(),"org":ORG,"u":{"fh":50}}]}),
        );
        assert_eq!(f.usage()["source"], "desktop_cache");
        assert!(f.usage()["five_hour"].is_null());
    }

    #[test]
    fn different_organizations_across_sources_require_a_hint() {
        let mut f = Fixture::new();
        f.cache(
            "one_0",
            &entry(ORG, json!({"five_hour":{"utilization":10}})),
            f.now - chrono::Duration::minutes(2),
        );
        f.history(json!({"version":2,"samples":[{"t":f.now.timestamp_millis(),"org":OTHER,"u":{"fh":90}}]}));
        assert_eq!(f.usage()["cache_status"], "ambiguous");
        assert!(f.usage()["five_hour"].is_null());
        f.acc["org_uuid"] = json!(ORG);
        assert_eq!(f.usage()["source"], "desktop_cache");
        assert_eq!(f.usage()["five_hour"]["utilization"], 10.0);
    }

    #[test]
    fn explicit_org_never_uses_unknown_org_history_and_bad_config_is_soft_missing() {
        let mut f = Fixture::new();
        f.acc["org_uuid"] = json!(ORG);
        f.history(json!({"version":1,"samples":[{"t":f.now.timestamp_millis(),"fh":20}]}));
        assert_eq!(f.usage()["cache_status"], "missing");
        f.history(json!({"version":2,"samples":[{"t":f.now.timestamp_millis(),"org":null,"u":{"fh":20}}]}));
        assert_eq!(f.usage()["cache_status"], "missing");
        f.cache(
            "valid_0",
            &entry(ORG, json!({"five_hour":{"utilization":20}})),
            f.now,
        );
        fs::write(f.dir().join("config.json"), "broken").unwrap();
        assert_eq!(f.usage()["cache_status"], "missing");
        assert!(f.usage()["five_hour"].is_null());
    }

    #[test]
    fn wrong_account_never_exposes_a_previous_accounts_usage() {
        let mut f = Fixture::new();
        f.cache(
            "one_0",
            &entry(ORG, json!({"five_hour":{"utilization":90}})),
            f.now,
        );
        fs::write(
            f.dir().join("config.json"),
            r#"{"lastKnownAccountUuid":"account-b"}"#,
        )
        .unwrap();
        f.acc["desktop_account_uuid"] = json!("account-a");
        let (usage, metadata) = fetch_at(&f.store, &f.acc, f.now).unwrap();
        assert!(usage["five_hour"].is_null());
        assert!(usage["notice"].as_str().unwrap().contains("另一个"));
        assert!(metadata.is_empty());
    }

    #[test]
    fn malformed_unrelated_oversized_and_invalid_percent_data_are_ignored() {
        let f = Fixture::new();
        f.cache("too_large_0", &vec![0; MAX_CACHE_INPUT + 1], f.now);
        f.cache(
            "out_of_range_0",
            &entry(ORG, json!({"five_hour":{"utilization":101}})),
            f.now,
        );
        f.cache(
            "future_0",
            &entry(ORG, json!({"five_hour":{"utilization":10}})),
            f.now + chrono::Duration::minutes(6),
        );
        let mut unrelated = entry(ORG, json!({"five_hour":{"utilization":10}}));
        let key_end = 24 + u32::from_le_bytes(unrelated[12..16].try_into().unwrap()) as usize;
        unrelated[key_end - 1] = b'x';
        f.cache("unrelated_0", &unrelated, f.now);
        assert_eq!(f.usage()["cache_status"], "missing");
        let oversized = hex::decode(shared()["oversize_zstd_hex"].as_str().unwrap()).unwrap();
        assert!(decode_body(&oversized).is_none());
    }
}
