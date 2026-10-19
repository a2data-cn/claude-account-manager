//! 数据目录读写，与脚本版 cam.py 完全兼容。

use rand::RngCore;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub type R<T> = Result<T, String>;

pub const BACKUP_KEEP: usize = 30;

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// 解析 ISO 时间或 YYYY-MM-DD，统一成 UTC。
pub fn parse_time(v: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(&v.replace(' ', "T")) {
        return Some(d.with_timezone(&chrono::Utc));
    }
    if let Ok(d) = chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(d.and_utc());
    }
    let day = v.get(..10)?;
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
}

pub fn new_id() -> String {
    let mut b = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

pub fn random_token() -> String {
    let mut b = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut b);
    // URL 安全
    hex::encode(b)
}

#[cfg(unix)]
pub fn set_private(path: &Path, dir: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if dir { 0o700 } else { 0o600 };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
pub fn set_private(_path: &Path, _dir: bool) {}

pub fn read_json(path: &Path) -> R<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| format!("文件损坏，无法解析：{}（{}）", path.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取 {} 失败：{}", path.display(), e)),
    }
}

/// 符号链接解析到它指向的文件（指向的文件不存在也行），其它情况原样返回。
pub fn resolve_link(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..16 {
        match fs::read_link(&p) {
            Ok(t) => p = if t.is_absolute() { t } else { p.parent().map(|d| d.join(&t)).unwrap_or(t) },
            Err(_) => break,
        }
    }
    p
}

pub fn read_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

/// 先写临时文件再原子替换，避免写到一半崩溃导致文件损坏。
/// 目标是符号链接（比如 dotfiles 管理的 ~/.claude.json）时写到链接指向的文件，保留链接本身。
pub fn write_json_atomic(path: &Path, data: &Value, private: bool) -> R<()> {
    let path = &resolve_link(path);
    let dir = path.parent().ok_or("无效路径")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".tmp-{}", new_id()));
    let text = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("写入 {} 失败：{}", tmp.display(), e))?;
        f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if private {
            set_private(&tmp, false);
        } else if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(meta.permissions().mode() & 0o777));
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("替换 {} 失败：{}", path.display(), e)
    })
}

/// 读-改-写一个别人也会写的 JSON 文件（~/.claude.json，Claude Code 运行时也会写）。
/// 写入前再读一次，内容变了就重新合并；写完再读回来确认改动还在（`check` 返回 false 就重来）。
pub fn update_json_file(
    path: &Path,
    mut mutate: impl FnMut(&mut Map<String, Value>),
    check: impl Fn(&Value) -> bool,
) -> R<()> {
    let real = resolve_link(path);
    for _ in 0..5 {
        let before = read_bytes(&real);
        let mut obj = match &before {
            Some(b) if !b.iter().all(u8::is_ascii_whitespace) => match serde_json::from_slice::<Value>(b) {
                Ok(Value::Object(m)) => m,
                Ok(_) => Map::new(),
                Err(e) => return Err(format!("文件损坏，无法解析：{}（{}）", path.display(), e)),
            },
            _ => Map::new(),
        };
        mutate(&mut obj);
        if read_bytes(&real) != before {
            continue;
        }
        write_json_atomic(&real, &Value::Object(obj), false)?;
        if check(&read_json(&real)?.unwrap_or_else(|| json!({}))) {
            return Ok(());
        }
    }
    Err(format!("{} 一直在被其它程序改写，请先退出正在运行的 claude 再试", path.display()))
}

pub struct Store {
    pub root: PathBuf,
}

/// 系统级文件锁（Unix flock，Windows LockFileEx），和 cam.py 的 FileLock 是同一种锁。
/// 进程退出或崩溃时由操作系统释放；文件关闭（drop）即解锁。
pub struct LockGuard(#[allow(dead_code)] fs::File);

fn open_lock_file(path: &Path) -> R<fs::File> {
    if let Some(d) = path.parent() {
        let _ = fs::create_dir_all(d);
    }
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("无法打开锁文件 {}：{}", path.display(), e))?;
    set_private(path, false);
    Ok(f)
}

/// 不等待，拿到锁返回 Some。
pub fn try_lock_file(path: &Path) -> R<Option<LockGuard>> {
    let f = open_lock_file(path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(LockGuard(f))),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(format!("加锁失败：{}", e)),
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        let root = std::env::var_os("CAM_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".claude-accounts"));
        let _ = fs::create_dir_all(&root);
        set_private(&root, true);
        Store { root }
    }

    pub fn accounts_path(&self) -> PathBuf {
        self.root.join("accounts.json")
    }
    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("browser-profiles")
    }
    pub fn secret_path(&self, id: &str) -> PathBuf {
        self.secrets_dir().join(format!("{}.json", id))
    }

    /// 跨进程锁（与 cam.py 用同一个锁文件、同一种锁）。同一进程里的不同线程之间也互斥。
    /// 旧版本用的是 .lock（独占创建文件），换成新文件名，避免旧版把新锁当残留删掉。
    pub fn lock(&self) -> R<LockGuard> {
        let path = self.root.join("accounts.lock");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(g) = try_lock_file(&path)? {
                return Ok(g);
            }
            if Instant::now() > deadline {
                return Err(format!("数据目录被占用（{}），请稍后重试", path.display()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 后台任务（自动导入、时间规则、定时刷新）只让一个进程做：
    /// 桌面版、cam serve、菜单栏插件谁先拿到谁做，拿到的进程一直持有到退出。
    pub fn try_background_lock(&self) -> R<Option<LockGuard>> {
        try_lock_file(&self.root.join("background.lock"))
    }

    pub fn load(&self) -> R<Value> {
        let mut data = read_json(&self.accounts_path())?.unwrap_or_else(|| json!({}));
        if !data.is_object() {
            data = json!({});
        }
        let obj = data.as_object_mut().unwrap();
        obj.entry("version").or_insert(json!(1));
        obj.entry("active").or_insert(json!({}));
        obj.entry("accounts").or_insert(json!([]));
        obj.entry("switch_log").or_insert(json!([]));
        obj.entry("rules").or_insert(json!([]));
        if let Some(arr) = obj.get_mut("accounts").and_then(|v| v.as_array_mut()) {
            for a in arr.iter_mut() {
                if let Some(o) = a.as_object_mut() {
                    if !o.get("manual").map(|m| m.is_object()).unwrap_or(false) {
                        o.insert("manual".into(), json!({}));
                    }
                    o.entry("usage").or_insert(Value::Null);
                }
            }
        }
        Ok(data)
    }

    pub fn save(&self, data: &mut Value) -> R<()> {
        if let Some(log) = data.get_mut("switch_log").and_then(|v| v.as_array_mut()) {
            let n = log.len();
            if n > 2000 {
                log.drain(0..n - 2000);
            }
        }
        write_json_atomic(&self.accounts_path(), data, false)
    }

    pub fn load_secret(&self, id: &str) -> Value {
        read_json(&self.secret_path(id))
            .ok()
            .flatten()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| Value::Object(Map::new()))
    }

    pub fn save_secret(&self, id: &str, v: &Value) -> R<()> {
        let d = self.secrets_dir();
        fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        set_private(&d, true);
        write_json_atomic(&self.secret_path(id), v, true)
    }

    pub fn new_backup_dir(&self, tag: &str) -> R<PathBuf> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let base = self.backups_dir();
        let mut d = base.join(format!("{}-{}", stamp, tag));
        let mut i = 1;
        while d.exists() {
            d = base.join(format!("{}-{}-{}", stamp, tag, i));
            i += 1;
        }
        fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        set_private(&d, true);
        Ok(d)
    }

    pub fn prune_backups(&self) {
        if let Ok(rd) = fs::read_dir(self.backups_dir()) {
            let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            names.sort();
            if names.len() > BACKUP_KEEP {
                for p in &names[..names.len() - BACKUP_KEEP] {
                    let _ = fs::remove_dir_all(p);
                }
            }
        }
    }

    pub fn server_token(&self) -> String {
        let p = self.root.join("server-token");
        if let Ok(s) = fs::read_to_string(&p) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
        let t = random_token();
        let _ = fs::write(&p, &t);
        set_private(&p, false);
        t
    }

    pub fn reset_server_token(&self) -> String {
        let _ = fs::remove_file(self.root.join("server-token"));
        self.server_token()
    }
}

// ---------------------------------------------------------------- Value 小工具

pub fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str()).filter(|x| !x.is_empty())
}

pub fn accounts(data: &Value) -> &Vec<Value> {
    static EMPTY: Vec<Value> = Vec::new();
    data.get("accounts").and_then(|v| v.as_array()).unwrap_or(&EMPTY)
}

pub fn accounts_mut(data: &mut Value) -> &mut Vec<Value> {
    if !data.get("accounts").map(|v| v.is_array()).unwrap_or(false) {
        data["accounts"] = json!([]);
    }
    data["accounts"].as_array_mut().unwrap()
}

pub fn active_code_id(data: &Value) -> Option<String> {
    data.get("active")
        .and_then(|a| a.get("claude_code"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

pub fn set_active_code(data: &mut Value, id: Option<&str>) {
    if !data.get("active").map(|v| v.is_object()).unwrap_or(false) {
        data["active"] = json!({});
    }
    data["active"]["claude_code"] = id.map(|x| json!(x)).unwrap_or(Value::Null);
}

pub fn push_switch_log(data: &mut Value, id: &str) {
    if !data.get("switch_log").map(|v| v.is_array()).unwrap_or(false) {
        data["switch_log"] = json!([]);
    }
    data["switch_log"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": id, "at": now_iso()}));
}

/// 和 cam.py 的 find_account 规则一致：序号 / id / 名称 / 邮箱 / id 前缀。
pub fn find_index(data: &Value, key: &str) -> R<usize> {
    let accs = accounts(data);
    let key = key.trim();
    let lower = key.to_lowercase();
    if let Ok(n) = key.parse::<usize>() {
        if n >= 1 && n <= accs.len() && !accs.iter().any(|a| s(a, "id") == Some(key)) {
            return Ok(n - 1);
        }
    }
    for field in ["id", "label", "email"] {
        let hits: Vec<usize> = accs
            .iter()
            .enumerate()
            .filter(|(_, a)| s(a, field).map(|x| x.to_lowercase() == lower).unwrap_or(false))
            .map(|(i, _)| i)
            .collect();
        if hits.len() == 1 {
            return Ok(hits[0]);
        }
        if hits.len() > 1 {
            return Err(format!("“{}”匹配到多个账号，请用 id", key));
        }
    }
    let hits: Vec<usize> = accs
        .iter()
        .enumerate()
        .filter(|(_, a)| s(a, "id").map(|x| x.starts_with(key)).unwrap_or(false))
        .map(|(i, _)| i)
        .collect();
    if hits.len() == 1 && !key.is_empty() {
        return Ok(hits[0]);
    }
    Err(format!("找不到账号：{}", key))
}
