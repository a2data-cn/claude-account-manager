//! 会话页：这台电脑上正在跑的 Claude Code。进程表找出在跑的 claude，对上它的会话记录（cam 的钩子记的 pid → 同一个工作目录 →
//! 那个目录里最新的记录文件），再从记录里读出用的模型、聊了几轮、上下文用了多少、按内容估的组成。只读本机文件；只看用量、
//! 元数据、工具名和文件路径，消息内容不读出来、不返回。与 cam.py 的“会话页”一节一致（tests/shared/sessions.json）。

use super::claude::{
    claude_config_dir, fake_sessions, list_claude_processes, recent_cpu_rates, session_ages,
    ACTIVE_WINDOW_SECS, CPU_BUSY, PROCESS_LIST_TIMEOUT,
};
use super::store::*;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 钩子说在回答、但这么多秒记录没写、进程也没吃 CPU（按 Esc 打断时 Stop 不来）就当空闲了
pub const SESSION_BUSY_QUIET: f64 = 600.0;
/// 上下文用到窗口的这么多：快满了 / 很满
pub const CONTEXT_HIGH: f64 = 0.70;
pub const CONTEXT_FULL: f64 = 0.90;
/// 认不出的模型按这个算，界面写“约”
pub const CONTEXT_DEFAULT: u64 = 200_000;
/// 模型的上下文窗口，按前缀找、前面的先比（具体的写在前面）。来源：Claude API 文档的模型表（2026-11），与 cam.py 一致
const CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("claude-haiku-4-5", 200_000),
    ("claude-opus-4-5", 200_000),
    ("claude-opus-4-1", 200_000),
    ("claude-sonnet-4-5", 200_000),
    ("claude-opus-4-6", 1_000_000),
    ("claude-opus-4-7", 1_000_000),
    ("claude-opus-4-8", 1_000_000),
    ("claude-sonnet-4-6", 1_000_000),
    ("claude-opus-4", 200_000),
    ("claude-sonnet-4", 200_000),
    ("claude-3", 200_000),
    ("claude-fable", 1_000_000),
    ("claude-mythos", 1_000_000),
    ("claude-opus-5", 1_000_000),
    ("claude-sonnet-5", 1_000_000),
    ("claude-haiku-5", 1_000_000),
];
/// 一张图按这么多 token 估（文档：1092×1092 的图约 1590）
pub const IMAGE_TOKENS: u64 = 1600;
/// 组成里列出占得最多的这么多项
pub const COMPOSITION_TOP: usize = 60;
/// 你发的消息里这些开头的不算一轮：斜杠命令、命令的输出、打断、! 跑的命令
const NOT_PROMPT_PREFIXES: &[&str] = &[
    "<command-",
    "<local-command-",
    "[Request interrupted",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
];
/// 钩子记录文件（cam.py 的钩子写，见 cam.py 的 SESSION_STATE_FILE）
const SESSION_STATE_FILE: &str = "session-state.json";
/// 弹着权限确认等了这么久还没动静就不算了，和 cam.py 的 ATTENTION_MAX_AGE 一致
const ATTENTION_MAX_AGE: f64 = 2.0 * 3600.0;

/// (上下文窗口大小, 准不准)。带 [1m] 的是 1M；认不出来的按 CONTEXT_DEFAULT 算，准不准为 false（界面写“约”）。
pub fn context_window(model: Option<&str>) -> (u64, bool) {
    let m = model.unwrap_or("").trim().to_lowercase();
    if m.ends_with("[1m]") {
        return (1_000_000, true);
    }
    let m = match m.find("claude-") {
        Some(i) => &m[i..],
        None => &m[..],
    };
    for (prefix, size) in CONTEXT_WINDOWS {
        if m.starts_with(prefix) {
            return (*size, true);
        }
    }
    (CONTEXT_DEFAULT, false)
}

/// 上下文用了多少：ok（健康）/ high（快满了）/ full（很满）；算不出来是 None。
pub fn context_health(used: Option<u64>, window: u64) -> Option<&'static str> {
    let used = used?;
    if window == 0 {
        return None;
    }
    let r = used as f64 / window as f64;
    Some(if r >= CONTEXT_FULL {
        "full"
    } else if r >= CONTEXT_HIGH {
        "high"
    } else {
        "ok"
    })
}

/// Claude Code 存会话记录的目录名：工作目录里不是英文字母、数字的字符都换成 -。
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(x)) => !x.is_empty(),
        Some(Value::Array(x)) => !x.is_empty(),
        Some(Value::Object(x)) => !x.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
    }
}

fn num(v: Option<&Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(0.0)
}

/// 一个会话现在是 busy（在回答）/ waiting（等你确认）/ idle（空闲）。规则见 cam.py 的 session_state。
pub fn session_state(
    rec: Option<&Value>,
    waiting: bool,
    written_ago: Option<f64>,
    cpu: Option<f64>,
    now: f64,
) -> &'static str {
    if waiting {
        return "waiting";
    }
    let recent = written_ago.is_some_and(|w| w < ACTIVE_WINDOW_SECS as f64);
    let eating = cpu.is_some_and(|c| c > CPU_BUSY);
    let st = rec.filter(|r| r.is_object()).and_then(|r| s(r, "state"));
    let last = if st.is_some() {
        num(rec.and_then(|r| r.get("last_ts")))
    } else {
        0.0
    };
    match st {
        Some("busy") | Some("waiting") => {
            let mut quiet = now - last;
            if let Some(w) = written_ago {
                quiet = quiet.min(w);
            }
            if quiet < SESSION_BUSY_QUIET || eating {
                "busy"
            } else {
                "idle"
            }
        }
        Some("idle") => match written_ago {
            Some(w) if recent && now - w > last + 5.0 => "busy",
            _ => "idle",
        },
        _ => {
            if recent || eating {
                "busy"
            } else {
                "idle"
            }
        }
    }
}

/// “3 个会话 · 1 个在回答 · 1 个等你确认”。answering 为 None（看不出是哪个在回答）时只写“正在回答”。与 cam.py 一致
pub fn session_phrase(n: usize, busy: bool, answering: Option<usize>, waiting: usize) -> String {
    let mut parts = vec![format!("{n} 个会话")];
    match answering {
        Some(a) if a > 0 => parts.push(format!("{a} 个在回答")),
        _ if busy => parts.push("正在回答".into()),
        _ => {}
    }
    if waiting > 0 {
        parts.push(format!("{waiting} 个等你确认"));
    }
    parts.join(" · ")
}

/// 估 token：UTF-8 字节数除以 4（向上取整），和 cam.py 的 _est_tokens 一样只用来比大小
fn est_tokens(nbytes: u64) -> u64 {
    nbytes.div_ceil(4)
}

fn text_bytes(v: Option<&Value>) -> u64 {
    v.and_then(|x| x.as_str()).map_or(0, |t| t.len() as u64)
}

/// 工具参数里所有字符串值加起来多少字节（键名不算，数字和真假不算）
fn leaf_bytes(v: &Value) -> u64 {
    match v {
        Value::String(t) => t.len() as u64,
        Value::Object(m) => m.values().map(leaf_bytes).sum(),
        Value::Array(a) => a.iter().map(leaf_bytes).sum(),
        _ => 0,
    }
}

/// 记录里的这一行是不是你发的一条消息（算一轮）。与 cam.py 的 _is_prompt 一致
fn is_prompt(ev: &Value) -> bool {
    let Some(msg) = ev.get("message").filter(|m| m.is_object()) else {
        return false;
    };
    if s(ev, "type") != Some("user")
        || truthy(ev.get("isMeta"))
        || truthy(ev.get("isCompactSummary"))
        || truthy(ev.get("isSidechain"))
    {
        return false;
    }
    let (texts, other): (Vec<&str>, bool) = match msg.get("content") {
        Some(Value::String(c)) => (vec![c.as_str()], false),
        Some(Value::Array(blocks)) => {
            let blocks: Vec<&Value> = blocks.iter().filter(|b| b.is_object()).collect();
            if blocks.iter().any(|b| s(b, "type") == Some("tool_result")) {
                return false;
            }
            let texts = blocks
                .iter()
                .filter(|b| s(b, "type") == Some("text"))
                .filter_map(|b| s(b, "text"))
                .collect();
            (texts, blocks.iter().any(|b| s(b, "type") == Some("image")))
        }
        _ => return false,
    };
    match texts.first() {
        None => other,
        Some(t) => {
            let t = t.trim_start();
            !NOT_PROMPT_PREFIXES.iter().any(|p| t.starts_with(p))
        }
    }
}

/// 这一行里有没有 "key":val（冒号后面也认一个空格）。按文字找，不解析
fn has_kv(line: &str, key: &str, val: &str) -> bool {
    line.contains(&format!("\"{key}\":{val}")) || line.contains(&format!("\"{key}\": {val}"))
}

/// 这一行里第一个 "key":"…" 的值（JSON 字符串原样，还没反转义）
fn first_str_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pats = [format!("\"{key}\":\""), format!("\"{key}\": \"")];
    let (at, plen) = pats
        .iter()
        .filter_map(|p| line.find(p.as_str()).map(|i| (i, p.len())))
        .min_by_key(|(i, _)| *i)?;
    let rest = &line[at + plen..];
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(&rest[..i]),
            _ => i += 1,
        }
    }
    None
}

fn json_line(raw: &[u8]) -> Option<Value> {
    let v: Value = serde_json::from_slice(raw)
        .or_else(|_| serde_json::from_str(&String::from_utf8_lossy(raw)))
        .ok()?;
    v.is_object().then_some(v)
}

fn mtime_secs(m: &fs::Metadata) -> f64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// 记录读到哪了、读出来的东西。见 cam.py 的 _scan_session_meta
#[derive(Clone, Default)]
struct MetaEntry {
    pos: u64,
    size: i64,
    mtime: f64,
    turns: u64,
    first_ts: Option<String>,
    cwd: Option<String>,
    assist: Vec<(u64, usize)>,
    boundary: Option<u64>,
    turns_at_boundary: u64,
    last: Option<(String, Value)>,
    used_at: f64,
}

/// 从 pos 开始一行一行读到最后一个完整的行（文件一分钟没动过时没换行符的最后一行也算），每行交给 f(行首位置, 内容)。
/// 返回读到的位置。
fn read_lines(
    path: &Path,
    pos: u64,
    stale: bool,
    mut f: impl FnMut(u64, &[u8]),
) -> std::io::Result<u64> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(pos))?;
    let mut r = BufReader::with_capacity(1 << 16, file);
    let mut pos = pos;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = r.read_until(b'\n', &mut buf)?;
        if n == 0 || (!buf.ends_with(b"\n") && !stale) {
            break;
        }
        let start = pos;
        pos += n as u64;
        f(start, &buf);
    }
    Ok(pos)
}

fn scan_meta(path: &Path, entry: Option<MetaEntry>) -> std::io::Result<MetaEntry> {
    let md = fs::metadata(path)?;
    let mut e = match entry {
        Some(e) if md.len() >= e.pos => e,
        _ => MetaEntry {
            size: -1,
            ..Default::default()
        },
    };
    let mtime = mtime_secs(&md);
    if md.len() as i64 == e.size && mtime == e.mtime {
        return Ok(e);
    }
    let stale = now_secs() - mtime > 60.0;
    let pos = read_lines(path, e.pos, stale, |start, raw| {
        let line = String::from_utf8_lossy(raw);
        if has_kv(&line, "isSidechain", "true") {
            return;
        }
        if e.first_ts.is_none() {
            e.first_ts = first_str_value(&line, "timestamp").map(String::from);
        }
        if e.cwd.is_none() {
            e.cwd = first_str_value(&line, "cwd")
                .and_then(|raw| serde_json::from_str::<String>(&format!("\"{raw}\"")).ok());
        }
        if line.contains("\"compact_boundary\"") {
            if let Some(ev) = json_line(raw) {
                if s(&ev, "type") == Some("system") && s(&ev, "subtype") == Some("compact_boundary")
                {
                    e.boundary = Some(start);
                    e.turns_at_boundary = e.turns;
                }
            }
            return;
        }
        if has_kv(&line, "type", "\"assistant\"") && line.contains("\"usage\"") {
            e.assist.push((start, raw.len()));
            if e.assist.len() > 6 {
                e.assist.remove(0);
            }
            return;
        }
        if has_kv(&line, "type", "\"user\"") && !line.contains("\"tool_result\"") {
            if let Some(ev) = json_line(raw) {
                if is_prompt(&ev) {
                    e.turns += 1;
                }
            }
        }
    })?;
    e.pos = pos;
    e.size = md.len() as i64;
    e.mtime = mtime;
    e.last = None;
    // 最后一条真回复（出错时 Claude Code 写的 <synthetic> 不算）的模型和用量
    let mut f = fs::File::open(path)?;
    for &(start, n) in e.assist.iter().rev() {
        let mut buf = vec![0u8; n];
        f.seek(SeekFrom::Start(start))?;
        if f.read_exact(&mut buf).is_err() {
            continue;
        }
        let Some(ev) = json_line(&buf) else { continue };
        let Some(msg) = ev.get("message") else {
            continue;
        };
        if msg.get("usage").is_some_and(|u| u.is_object()) && s(msg, "model") != Some("<synthetic>")
        {
            e.last = Some((
                s(msg, "model").unwrap_or("").to_string(),
                msg["usage"].clone(),
            ));
            break;
        }
    }
    Ok(e)
}

/// 一个会话记录的摘要（见 cam.py 的 transcript_meta）
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    pub turns: u64,
    pub first_ts: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub context_used: Option<u64>,
    pub cache_hit: Option<f64>,
    pub boundary: Option<u64>,
    pub turns_at_boundary: u64,
}

static META: Mutex<Option<HashMap<PathBuf, MetaEntry>>> = Mutex::new(None);
static COMP: Mutex<Option<HashMap<PathBuf, CompEntry>>> = Mutex::new(None);

fn tok(u: &Value, k: &str) -> u64 {
    u.get(k)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .unwrap_or(0)
}

/// 一个会话记录的摘要：几轮、第一条的时间、工作目录、最后一条回复的模型、上下文用了多少（输入 + 缓存读取 + 缓存写入 +
/// 输出）、那次请求的缓存命中。读不了时返回 None。
pub fn transcript_meta(path: &Path) -> Option<Meta> {
    let e = {
        let mut g = META.lock().unwrap_or_else(|e| e.into_inner());
        let cache = g.get_or_insert_with(HashMap::new);
        let mut e = scan_meta(path, cache.remove(path)).ok()?;
        e.used_at = now_secs();
        cache.insert(path.to_path_buf(), e.clone());
        e
    };
    let (model, usage) = match &e.last {
        Some((m, u)) => (Some(m.clone()).filter(|m| !m.is_empty()), Some(u)),
        None => (None, None),
    };
    let (used, inp, cr) = match usage {
        Some(u) if u.as_object().is_some_and(|o| !o.is_empty()) => {
            let inp = tok(u, "input_tokens")
                + tok(u, "cache_read_input_tokens")
                + tok(u, "cache_creation_input_tokens");
            (
                Some(inp + tok(u, "output_tokens")),
                inp,
                tok(u, "cache_read_input_tokens"),
            )
        }
        _ => (None, 0, 0),
    };
    Some(Meta {
        turns: e.turns,
        first_ts: e.first_ts,
        cwd: e.cwd,
        model,
        context_used: used,
        cache_hit: (inp > 0).then(|| super::usage::round4(cr as f64 / inp as f64)),
        boundary: e.boundary,
        turns_at_boundary: e.turns_at_boundary,
    })
}

/// 组成里的一项
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub cat: &'static str,
    pub key: String,
    pub name: String,
    pub detail: String,
    pub count: u64,
    pub tokens: u64,
}

#[derive(Clone, Default)]
struct CompEntry {
    start: u64,
    pos: u64,
    size: i64,
    mtime: f64,
    turn: u64,
    acc: HashMap<String, Item>,
    tools: HashMap<String, (String, Option<String>)>,
    used_at: f64,
}

fn comp_add(
    acc: &mut HashMap<String, Item>,
    key: String,
    cat: &'static str,
    name: String,
    detail: String,
    tokens: u64,
    count: u64,
) {
    let it = acc.entry(key.clone()).or_insert(Item {
        cat,
        key,
        name,
        detail,
        count: 0,
        tokens: 0,
    });
    it.tokens += tokens;
    it.count += count;
}

/// 路径的目录部分（斜杠统一成 /），和 Python 的 os.path.dirname 一样：根目录下的文件是 "/"
fn dirname(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "",
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// 读过的文件显示成 文件名 + 短目录：在工作目录里的写相对目录，在家目录里的写 ~/…（与 cam.py 的 _short_dir 一致）
fn short_dir(path: &str, cwd: Option<&str>) -> String {
    let path = path.replace('\\', "/");
    let mut d = dirname(&path).to_string();
    let base = cwd.unwrap_or("").replace('\\', "/");
    let base = base.trim_end_matches('/');
    if !base.is_empty() && (d == base || d.starts_with(&format!("{base}/"))) {
        let rel = d[base.len()..].trim_matches('/');
        return if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
    }
    let h = home_dir().to_string_lossy().replace('\\', "/");
    let h = h.trim_end_matches('/');
    if !h.is_empty() && (d == h || d.starts_with(&format!("{h}/"))) {
        d = format!("~{}", &d[h.len()..]);
    }
    if d.is_empty() {
        d
    } else {
        format!("{d}/")
    }
}

fn file_item(fp: &str, cwd: Option<&str>) -> (String, String, String) {
    let unix = fp.replace('\\', "/");
    let name = match basename(&unix) {
        "" => fp.to_string(),
        b => b.to_string(),
    };
    (format!("file:{fp}"), name, short_dir(fp, cwd))
}

/// 把记录里的一行算进组成。与 cam.py 的 _comp_line 一致
fn comp_line(c: &mut CompEntry, ev: &Value, cwd: Option<&str>) {
    let typ = s(ev, "type");
    let Some(msg) = ev.get("message").filter(|m| m.is_object()) else {
        return;
    };
    if truthy(ev.get("isSidechain")) || !matches!(typ, Some("user") | Some("assistant")) {
        return;
    }
    let user = typ == Some("user");
    if user && is_prompt(ev) {
        c.turn += 1;
    }
    let summary = truthy(ev.get("isCompactSummary"));
    let turn = c.turn;
    let chat = |acc: &mut HashMap<String, Item>, nbytes: u64| {
        let (key, name) = if summary {
            ("summary".to_string(), "压缩后的摘要".to_string())
        } else if turn > 0 {
            (format!("turn:{turn}"), format!("第 {turn} 轮"))
        } else {
            ("start".to_string(), "开头".to_string())
        };
        comp_add(acc, key, "chat", name, String::new(), est_tokens(nbytes), 0);
    };
    let blocks = match msg.get("content") {
        Some(Value::String(t)) => {
            chat(&mut c.acc, t.len() as u64);
            return;
        }
        Some(Value::Array(a)) => a,
        _ => return,
    };
    for b in blocks.iter().filter(|b| b.is_object()) {
        match s(b, "type") {
            Some("text") => chat(&mut c.acc, text_bytes(b.get("text"))),
            Some("thinking") => chat(&mut c.acc, text_bytes(b.get("thinking"))),
            Some("image") if user => comp_add(
                &mut c.acc,
                "img:you".into(),
                "images",
                "你发的图片".into(),
                String::new(),
                IMAGE_TOKENS,
                1,
            ),
            Some("tool_use") if !user => {
                let name = s(b, "name")
                    .filter(|x| !x.is_empty())
                    .unwrap_or("?")
                    .to_string();
                let empty = Value::Object(Map::new());
                let inp = b.get("input").filter(|v| v.is_object()).unwrap_or(&empty);
                let fp = (name == "Read")
                    .then(|| s(inp, "file_path"))
                    .flatten()
                    .map(String::from);
                let id = s(b, "id").unwrap_or("").to_string();
                c.tools.insert(id, (name.clone(), fp.clone()));
                let t = est_tokens(leaf_bytes(inp));
                match fp {
                    Some(fp) => {
                        let (key, nm, detail) = file_item(&fp, cwd);
                        comp_add(&mut c.acc, key, "files", nm, detail, t, 1);
                    }
                    None => comp_add(
                        &mut c.acc,
                        format!("tool:{name}"),
                        "tools",
                        name,
                        String::new(),
                        t,
                        1,
                    ),
                }
            }
            Some("tool_result") if user => {
                let (name, fp) = c
                    .tools
                    .get(s(b, "tool_use_id").unwrap_or(""))
                    .cloned()
                    .unwrap_or(("?".to_string(), None));
                let mut nbytes = 0;
                match b.get("content") {
                    Some(Value::String(t)) => nbytes = t.len() as u64,
                    Some(Value::Array(rs)) => {
                        for r in rs.iter().filter(|r| r.is_object()) {
                            match s(r, "type") {
                                Some("text") => nbytes += text_bytes(r.get("text")),
                                Some("image") => comp_add(
                                    &mut c.acc,
                                    format!("img:{name}"),
                                    "images",
                                    format!("{name} 返回的图片"),
                                    String::new(),
                                    IMAGE_TOKENS,
                                    1,
                                ),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                match fp {
                    Some(fp) => {
                        let (key, nm, detail) = file_item(&fp, cwd);
                        comp_add(&mut c.acc, key, "files", nm, detail, est_tokens(nbytes), 0);
                    }
                    None => comp_add(
                        &mut c.acc,
                        format!("tool:{name}"),
                        "tools",
                        name,
                        String::new(),
                        est_tokens(nbytes),
                        0,
                    ),
                }
            }
            _ => {}
        }
    }
}

fn scan_comp(
    path: &Path,
    entry: Option<CompEntry>,
    start: u64,
    turns_before: u64,
    cwd: Option<&str>,
) -> std::io::Result<CompEntry> {
    let md = fs::metadata(path)?;
    let mut e = match entry {
        Some(e) if e.start == start && md.len() >= e.pos => e,
        _ => CompEntry {
            start,
            pos: start,
            size: -1,
            turn: turns_before,
            ..Default::default()
        },
    };
    let mtime = mtime_secs(&md);
    if md.len() as i64 == e.size && mtime == e.mtime {
        return Ok(e);
    }
    let stale = now_secs() - mtime > 60.0;
    let pos = read_lines(path, e.pos, stale, |_, raw| {
        let line = String::from_utf8_lossy(raw);
        if has_kv(&line, "isSidechain", "true")
            || !(has_kv(&line, "type", "\"user\"") || has_kv(&line, "type", "\"assistant\""))
        {
            return;
        }
        if let Some(ev) = json_line(raw) {
            comp_line(&mut e, &ev, cwd);
        }
    })?;
    e.pos = pos;
    e.size = md.len() as i64;
    e.mtime = mtime;
    Ok(e)
}

/// 把累计的条目整理成界面要的：分类合计、占得最多的 top 项。见 cam.py 的 composition_view
pub fn composition_view(acc: &HashMap<String, Item>, total: Option<u64>, top: usize) -> Value {
    let mut items: Vec<Item> = acc
        .values()
        .filter(|it| it.tokens > 0 || it.count > 0)
        .cloned()
        .collect();
    let mut counted: u64 = items.iter().map(|it| it.tokens).sum();
    let exact = total.is_some();
    let total = total.unwrap_or(counted);
    let scaled = (counted > total && total > 0) || (counted > 0 && total == 0);
    if scaled {
        for it in items.iter_mut() {
            it.tokens = (it.tokens as u128 * total as u128 / counted as u128) as u64;
        }
        counted = items.iter().map(|it| it.tokens).sum();
    }
    let mut cats: Vec<(&str, u64)> = ["chat", "files", "tools", "images"]
        .iter()
        .map(|c| (*c, 0))
        .collect();
    for it in &items {
        if let Some(c) = cats.iter_mut().find(|(k, _)| *k == it.cat) {
            c.1 += it.tokens;
        }
    }
    cats.push(("other", total.saturating_sub(counted)));
    items.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.key.cmp(&b.key)));
    let n = items.len();
    let mut categories = Map::new();
    for (k, v) in cats {
        categories.insert(k.to_string(), json!(v));
    }
    json!({
        "total": total, "exact": exact, "scaled": scaled, "categories": categories,
        "items": items.iter().take(top).map(|it| json!({"cat": it.cat, "key": it.key, "name": it.name,
            "detail": it.detail, "count": it.count, "tokens": it.tokens})).collect::<Vec<_>>(),
        "item_count": n,
    })
}

/// 一个会话记录现在的上下文按内容估的组成（只看最后一次压缩之后）。读不了时返回 None。
pub fn transcript_composition(path: &Path, meta: &Meta, top: usize) -> Option<Value> {
    let start = meta.boundary.unwrap_or(0);
    let before = if meta.boundary.is_some() {
        meta.turns_at_boundary
    } else {
        0
    };
    let acc = {
        let mut g = COMP.lock().unwrap_or_else(|e| e.into_inner());
        let cache = g.get_or_insert_with(HashMap::new);
        let mut e = scan_comp(path, cache.remove(path), start, before, meta.cwd.as_deref()).ok()?;
        e.used_at = now_secs();
        let acc = e.acc.clone();
        cache.insert(path.to_path_buf(), e);
        acc
    };
    let mut out = composition_view(&acc, meta.context_used, top);
    out["since_compact"] = json!(meta.boundary.is_some());
    Some(out)
}

/// 不在跑的会话的缓存：十分钟没用到就扔掉
fn prune_caches(keep: &HashSet<PathBuf>) {
    let now = now_secs();
    if let Some(m) = META.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.retain(|p, e| keep.contains(p) || now - e.used_at <= 600.0);
    }
    if let Some(m) = COMP.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.retain(|p, e| keep.contains(p) || now - e.used_at <= 600.0);
    }
}

/// 一个 claude 进程能看出来的：工作目录、已经跑了几秒、用的哪个登录
#[derive(Clone, Default, Debug)]
struct ProcInfo {
    cwd: Option<String>,
    secs: Option<f64>,
    login: Option<String>,
}

/// macOS 上 lsof 查到的工作目录：(pid, 命令行) → 目录。同一个进程的工作目录不会变，查到就记着
static MAC_CWDS: Mutex<Option<HashMap<(u32, String), String>>> = Mutex::new(None);

/// Windows 进程表里的启动时间 → 每个进程已经运行了几秒（启动时间在将来一分钟以后的不算）
fn win_ages(rows: &[super::health::WinProc], now: f64) -> HashMap<u32, f64> {
    rows.iter()
        .filter_map(|(pid, _, _, _, started)| {
            let t = (*started)? as f64;
            (t <= now + 60.0).then_some((*pid, (now - t).max(0.0)))
        })
        .collect()
}

/// 正在跑的 claude 进程各自的工作目录、运行时长、登录方式。Linux 读 /proc；macOS 用 ps 和 lsof；
/// Windows 只看得出运行时长（进程表里的启动时间）。
fn proc_info(procs: &[(u32, String)]) -> HashMap<u32, ProcInfo> {
    let mut out: HashMap<u32, ProcInfo> = procs
        .iter()
        .map(|(p, _)| (*p, ProcInfo::default()))
        .collect();
    if procs.is_empty() {
        return out;
    }
    if let Some(fake) = fake_sessions() {
        for f in fake.unwrap_or_default() {
            if let Some(i) = out.get_mut(&f.pid) {
                *i = ProcInfo {
                    cwd: f.cwd,
                    secs: f.secs,
                    login: f.login,
                };
            }
        }
        return out;
    }
    if cfg!(target_os = "windows") {
        // 和 list_claude_processes 共用一次进程表；有了运行时长，才能按切换记录认出会话用的是哪个号
        if let Some(rows) = super::health::win_processes(std::time::Duration::from_secs(1)) {
            for (pid, secs) in win_ages(&rows, now_secs()) {
                if let Some(i) = out.get_mut(&pid) {
                    i.secs = Some(secs);
                }
            }
        }
        return out;
    }
    let pids: Vec<u32> = procs.iter().map(|(p, _)| *p).collect();
    for (pid, (secs, login)) in session_ages(&pids) {
        if let Some(i) = out.get_mut(&pid) {
            i.secs = secs;
            i.login = login;
        }
    }
    if Path::new("/proc/self").exists() {
        for (pid, i) in out.iter_mut() {
            i.cwd = fs::read_link(format!("/proc/{pid}/cwd"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
        }
        return out;
    }
    let mut g = MAC_CWDS.lock().unwrap_or_else(|e| e.into_inner());
    let cache = g.get_or_insert_with(HashMap::new);
    let need: Vec<String> = procs
        .iter()
        .filter(|(p, c)| !cache.contains_key(&(*p, c.clone())))
        .map(|(p, _)| p.to_string())
        .collect();
    if !need.is_empty() {
        let mut c = Command::new("lsof");
        c.args(["-a", "-d", "cwd", "-p", &need.join(","), "-Fn"]);
        let txt = super::health::run_with_timeout(c, PROCESS_LIST_TIMEOUT);
        let mut pid = None;
        for line in txt.lines() {
            if let Some(p) = line.strip_prefix('p') {
                pid = p.parse::<u32>().ok();
            } else if let (Some(dir), Some(p)) = (line.strip_prefix('n'), pid) {
                if let Some((_, cmd)) = procs.iter().find(|(q, _)| *q == p) {
                    cache.insert((p, cmd.clone()), dir.to_string());
                }
            }
        }
    }
    cache.retain(|(p, _), _| out.contains_key(p));
    for (p, cmd) in procs {
        if let Some(dir) = cache.get(&(*p, cmd.clone())) {
            if let Some(i) = out.get_mut(p) {
                i.cwd = Some(dir.clone());
            }
        }
    }
    out
}

fn cam_home() -> PathBuf {
    std::env::var_os("CAM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude-accounts"))
}

fn read_sessions_map(name: &str) -> Map<String, Value> {
    fs::read_to_string(cam_home().join(name))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("sessions").and_then(|x| x.as_object()).cloned())
        .unwrap_or_default()
}

/// 弹着权限确认、还没答的会话 id（规则和 claude::attention_view 一样）
fn waiting_sessions(running: bool) -> HashSet<String> {
    if !running {
        return HashSet::new();
    }
    let now = now_secs();
    read_sessions_map("attention.json")
        .into_iter()
        .filter(|(_, v)| {
            let since = num(v.get("since_ts"));
            if now - since > ATTENTION_MAX_AGE {
                return false;
            }
            let written = s(v, "transcript")
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| mtime_secs(&m));
            !matches!(written, Some(w) if w > since + 2.0)
        })
        .map(|(k, _)| k)
        .collect()
}

/// 工作目录对应的会话记录 [(修改时间, 路径)]，新的在前。只看那个目录这一层（子目录里是子 agent 的记录）
fn project_transcripts(cwd: Option<&str>) -> Vec<(f64, PathBuf)> {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return vec![];
    };
    let dir = claude_config_dir()
        .join("projects")
        .join(project_dir_name(cwd));
    let mut out: Vec<(f64, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .filter_map(|e| Some((mtime_secs(&e.metadata().ok()?), e.path())))
        .collect();
    out.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.cmp(&a.1))
    });
    out
}

/// 一个进程对上的会话
struct Matched {
    pid: u32,
    sid: Option<String>,
    rec: Option<Value>,
    path: Option<PathBuf>,
    how: Option<&'static str>,
}

/// 把正在跑的 claude 进程对上会话。与 cam.py 的 match_sessions 一致
fn match_sessions(
    procs: &[(u32, String)],
    info: &HashMap<u32, ProcInfo>,
    recs: &Map<String, Value>,
) -> Vec<Matched> {
    let mut rec_items: Vec<(&String, &Value)> =
        recs.iter().filter(|(_, r)| r.is_object()).collect();
    rec_items.sort_by(|a, b| {
        num(b.1.get("last_ts"))
            .partial_cmp(&num(a.1.get("last_ts")))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut by_pid: HashMap<u64, Vec<&String>> = HashMap::new();
    for (sid, r) in &rec_items {
        for p in r
            .get("pids")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(p) = p.as_u64() {
                by_pid.entry(p).or_default().push(sid);
            }
        }
    }
    let mut used_sids: HashSet<String> = HashSet::new();
    let mut used_files: HashSet<PathBuf> = HashSet::new();
    let mut hit: HashMap<u32, (String, &'static str)> = HashMap::new();
    for (pid, _) in procs {
        if let Some(sid) = by_pid
            .get(&(*pid as u64))
            .and_then(|v| v.iter().find(|s| !used_sids.contains(s.as_str())))
        {
            used_sids.insert((*sid).clone());
            hit.insert(*pid, ((*sid).clone(), "hook"));
        }
    }
    for (pid, _) in procs {
        let Some(cwd) = info.get(pid).and_then(|i| i.cwd.as_deref()) else {
            continue;
        };
        if hit.contains_key(pid) {
            continue;
        }
        if let Some((sid, _)) = rec_items
            .iter()
            .find(|(sid, r)| !used_sids.contains(sid.as_str()) && s(r, "cwd") == Some(cwd))
        {
            used_sids.insert((*sid).clone());
            hit.insert(*pid, ((*sid).clone(), "cwd"));
        }
    }
    let norm = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut rows: Vec<Matched> = Vec::new();
    for (pid, _) in procs {
        let (sid, how) = match hit.get(pid) {
            Some((s, h)) => (Some(s.clone()), Some(*h)),
            None => (None, None),
        };
        let rec = sid.as_ref().and_then(|s| recs.get(s)).cloned();
        let mut path = rec
            .as_ref()
            .and_then(|r| s(r, "transcript"))
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        if let (Some(sid), None) = (&sid, &path) {
            let cwd = info
                .get(pid)
                .and_then(|i| i.cwd.clone())
                .or_else(|| rec.as_ref().and_then(|r| s(r, "cwd")).map(String::from));
            if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
                let cand = claude_config_dir()
                    .join("projects")
                    .join(project_dir_name(&cwd))
                    .join(format!("{sid}.jsonl"));
                path = cand.exists().then_some(cand);
            }
        }
        if let Some(p) = &path {
            used_files.insert(norm(p));
        }
        rows.push(Matched {
            pid: *pid,
            sid,
            rec,
            path,
            how,
        });
    }
    for row in rows.iter_mut().filter(|r| r.sid.is_none()) {
        let cwd = info.get(&row.pid).and_then(|i| i.cwd.clone());
        for (_, p) in project_transcripts(cwd.as_deref()) {
            if !used_files.insert(norm(&p)) {
                continue;
            }
            let sid = p
                .file_stem()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            row.rec = if used_sids.contains(&sid) {
                None
            } else {
                recs.get(&sid).cloned()
            };
            used_sids.insert(sid.clone());
            row.sid = Some(sid);
            row.path = Some(p);
            row.how = Some("newest");
            break;
        }
    }
    if cfg!(target_os = "windows") {
        let mut left: Vec<(String, Value)> = rec_items
            .iter()
            .filter(|(sid, _)| !used_sids.contains(sid.as_str()))
            .map(|(s, r)| ((*s).clone(), (*r).clone()))
            .collect();
        for row in rows.iter_mut().filter(|r| r.sid.is_none()) {
            if left.is_empty() {
                break;
            }
            let (sid, rec) = left.remove(0);
            row.path = s(&rec, "transcript")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from);
            row.sid = Some(sid);
            row.rec = Some(rec);
            row.how = Some("guess");
        }
    }
    rows
}

/// 登录方式的说明（只在不是本机登录时显示），和 cam.py 的 LOGIN_TEXT 一致
fn login_text(login: &str) -> Option<&'static str> {
    match login {
        "own_token" => Some("用自己的号（CLAUDE_CODE_OAUTH_TOKEN），切号不影响"),
        "api_key" => Some("用 API Key（环境变量），不走订阅登录，切号不影响"),
        "config_dir" => Some("用单独的配置目录（CLAUDE_CONFIG_DIR），切号不影响"),
        "unknown" => Some("读不到它的环境变量，看不出用的哪个登录"),
        _ => None,
    }
}

fn iso_of(secs: f64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

/// 正在跑的会话一行一个，再加上每行的记录文件。detail=false 时只算状态（红绿灯数“几个在回答”用，不读记录内容）
/// 只有一个 Claude Code 号、它就是当前登录时返回它的 id：用本机登录的会话认不出时间（Windows 上拿不到启动时间、
/// 会话比第一条切换记录还早）也只可能是它。和 cam.py 的 sole_claude_code_account 一致。
fn sole_claude_code_account(data: &Value) -> Option<String> {
    let ids: Vec<&str> = accounts(data)
        .iter()
        .filter(|a| a.get("kind").and_then(Value::as_str) == Some("claude_code"))
        .filter_map(|a| a.get("id").and_then(Value::as_str))
        .collect();
    let active = data
        .get("active")
        .and_then(|a| a.get("claude_code"))
        .and_then(Value::as_str);
    match (ids.as_slice(), active) {
        ([id], Some(act)) if *id == act => Some(act.to_string()),
        _ => None,
    }
}

fn session_rows(
    procs: &[(u32, String)],
    data: Option<&Value>,
    detail: bool,
) -> Vec<(Value, Option<PathBuf>)> {
    let info = proc_info(procs);
    let recs = read_sessions_map(SESSION_STATE_FILE);
    let waiting = waiting_sessions(!procs.is_empty());
    let rates = recent_cpu_rates();
    let now = now_secs();
    let owner_of = data.map(super::usage::owner_finder);
    let only_cc = data.and_then(sole_claude_code_account);
    let names: HashMap<String, String> = data
        .map(accounts)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let id = s(a, "id")?;
            let name = s(a, "label")
                .filter(|x| !x.is_empty())
                .or(s(a, "email"))
                .unwrap_or(id);
            Some((id.to_string(), name.to_string()))
        })
        .collect();
    let matched = match_sessions(procs, &info, &recs);
    let mut out: Vec<(Value, Option<PathBuf>, f64)> = Vec::new();
    for m in &matched {
        let inf = info.get(&m.pid).cloned().unwrap_or_default();
        let mt = m
            .path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|md| mtime_secs(&md));
        let state = session_state(
            m.rec.as_ref(),
            m.sid.as_ref().is_some_and(|s| waiting.contains(s)),
            mt.map(|t| now - t),
            rates.get(&m.pid).copied(),
            now,
        );
        let id = m.sid.clone().unwrap_or_else(|| format!("pid-{}", m.pid));
        let mut row = json!({"id": id, "pid": m.pid, "state": state, "matched": m.how});
        if detail {
            let meta = m.path.as_deref().and_then(transcript_meta);
            let cwd = inf
                .cwd
                .clone()
                .or_else(|| {
                    m.rec
                        .as_ref()
                        .and_then(|r| s(r, "cwd"))
                        .filter(|c| !c.is_empty())
                        .map(String::from)
                })
                .or_else(|| meta.as_ref().and_then(|x| x.cwd.clone()))
                .unwrap_or_default();
            let started = match inf.secs {
                Some(secs) => Some(
                    chrono::Utc::now() - chrono::Duration::milliseconds((secs * 1000.0) as i64),
                ),
                None => meta
                    .as_ref()
                    .and_then(|x| x.first_ts.as_deref())
                    .and_then(parse_time)
                    .or_else(|| {
                        m.rec
                            .as_ref()
                            .and_then(|r| r.get("first_ts"))
                            .and_then(|v| v.as_f64())
                            .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0))
                    }),
            };
            let model = meta.as_ref().and_then(|x| x.model.clone());
            let (win, exact) = context_window(model.as_deref());
            let used = meta.as_ref().and_then(|x| x.context_used);
            let login = inf.login.clone();
            let acc_id = match (&owner_of, login.as_deref()) {
                (Some(f), None | Some("machine") | Some("unknown")) => {
                    started.and_then(f).or_else(|| only_cc.clone())
                }
                _ => None,
            };
            let unix = cwd.replace('\\', "/");
            let h = home_dir().to_string_lossy().replace('\\', "/");
            let h = h.trim_end_matches('/');
            let shown = if !h.is_empty() && (unix == h || unix.starts_with(&format!("{h}/"))) {
                format!("~{}", &unix[h.len()..])
            } else {
                unix.clone()
            };
            let project = match basename(unix.trim_end_matches('/')) {
                "" if shown.is_empty() => "看不出是哪个项目".to_string(),
                "" => shown.clone(),
                b => b.to_string(),
            };
            let o = row.as_object_mut().expect("row is an object");
            o.insert("project".into(), json!(project));
            o.insert(
                "cwd".into(),
                if shown.is_empty() {
                    Value::Null
                } else {
                    json!(shown)
                },
            );
            o.insert("login".into(), json!(login));
            o.insert(
                "login_text".into(),
                json!(login
                    .as_deref()
                    .filter(|l| *l != "machine")
                    .and_then(login_text)),
            );
            o.insert("account_id".into(), json!(acc_id));
            o.insert(
                "account".into(),
                json!(acc_id.as_ref().and_then(|i| names.get(i))),
            );
            o.insert("model".into(), json!(model));
            o.insert(
                "started_at".into(),
                json!(started.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))),
            );
            o.insert("last_active".into(), json!(mt.and_then(iso_of)));
            o.insert("turns".into(), json!(meta.as_ref().map(|x| x.turns)));
            o.insert("context_used".into(), json!(used));
            o.insert("context_window".into(), json!(win));
            o.insert("window_exact".into(), json!(exact));
            o.insert(
                "cache_hit".into(),
                json!(meta.as_ref().and_then(|x| x.cache_hit)),
            );
            o.insert("health".into(), json!(context_health(used, win)));
            o.insert("has_transcript".into(), json!(meta.is_some()));
        }
        out.push((row, m.path.clone(), mt.unwrap_or(f64::MIN)));
    }
    if detail {
        prune_caches(&matched.iter().filter_map(|m| m.path.clone()).collect());
    }
    let order = |st: &str| match st {
        "waiting" => 0,
        "busy" => 1,
        _ => 2,
    };
    out.sort_by(|a, b| {
        order(s(&a.0, "state").unwrap_or(""))
            .cmp(&order(s(&b.0, "state").unwrap_or("")))
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    out.into_iter().map(|(r, p, _)| (r, p)).collect()
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
}

fn live_procs() -> Option<Vec<(u32, String)>> {
    let procs = list_claude_processes()?;
    let pids: Vec<u32> = procs.iter().map(|(p, _)| *p).collect();
    super::claude::claude_busy(&pids); // 顺便采一次 CPU：每个会话在不在吃 CPU 用得上
    Some(procs)
}

/// GET /api/sessions：这台电脑上正在跑的 Claude Code，一个会话一行。与 cam.py 的 sessions_view 一致
pub fn sessions_view(data: &Value) -> Value {
    let Some(procs) = live_procs() else {
        return json!({"known": false, "sessions": [], "count": 0, "answering": 0, "waiting": 0});
    };
    let rows: Vec<Value> = session_rows(&procs, Some(data), true)
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    let count = |st: &str| rows.iter().filter(|r| s(r, "state") == Some(st)).count();
    json!({
        "known": true, "count": rows.len(), "answering": count("busy"), "waiting": count("waiting"),
        "hooks": hooks_installed(), "platform": platform_name(), "generated_at": now_iso(), "sessions": rows,
    })
}

/// GET /api/sessions/<id>：这个会话现在的上下文按内容估的组成（最后一次压缩之后）。会话不在跑了就报错
pub fn session_detail(data: &Value, sid: &str) -> R<Value> {
    let procs = live_procs().ok_or("这次没查到正在运行的进程，稍后再试")?;
    let rows = session_rows(&procs, Some(data), true);
    let (row, path) = rows
        .into_iter()
        .find(|(r, _)| s(r, "id") == Some(sid))
        .ok_or("这个会话已经结束了")?;
    let comp = path.as_deref().and_then(|p| {
        let meta = transcript_meta(p)?;
        let mut c = transcript_composition(p, &meta, COMPOSITION_TOP)?;
        c["window"] = row["context_window"].clone();
        c["window_exact"] = row["window_exact"].clone();
        Some(c)
    });
    Ok(json!({"session": row, "composition": comp}))
}

/// cam 的钩子装没装（看 Claude Code 的 settings.json 里有没有 cam_hook.py / cam.py hook 的命令）。只给界面一句提示用
fn hooks_installed() -> bool {
    let p = claude_config_dir().join("settings.json");
    fs::read_to_string(p).is_ok_and(|t| {
        t.contains("cam_hook.py") || t.contains("cam.py hook") || t.contains("cam.py\" hook")
    })
}

/// 红绿灯用的：(几个在回答, 几个等你确认)。缓存 10 秒；看不出是哪个在回答（只知道有在执行的）时第一个是 None
pub fn session_counts(busy: bool) -> (Option<usize>, usize) {
    static CACHE: Mutex<Option<(Instant, (usize, usize))>> = Mutex::new(None);
    let fresh = || {
        let procs = list_claude_processes().unwrap_or_default();
        let rows = session_rows(&procs, None, false);
        let n = |st: &str| {
            rows.iter()
                .filter(|(r, _)| s(r, "state") == Some(st))
                .count()
        };
        (n("busy"), n("waiting"))
    };
    let (a, w) = if super::claude::faking_processes() {
        fresh()
    } else {
        let mut slot = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        match *slot {
            Some((at, v)) if at.elapsed() < std::time::Duration::from_secs(10) => v,
            _ => {
                let v = fresh();
                *slot = Some((Instant::now(), v));
                v
            }
        }
    };
    (if a == 0 && busy { None } else { Some(a) }, w)
}

/// /api/state 里的 sessions：{"count", "busy", "answering", "waiting", "text"}。与 cam.py 的 sessions_brief 一致
pub fn sessions_brief() -> Value {
    let (n, busy) = super::claude::sessions_view();
    let (answering, waiting) = if n > 0 {
        session_counts(busy)
    } else {
        (Some(0), 0)
    };
    let busy = busy || answering.is_some_and(|a| a > 0);
    let text = if n > 0 {
        session_phrase(n, busy, answering, waiting)
    } else {
        String::new()
    };
    json!({"count": n, "busy": busy, "answering": answering, "waiting": waiting, "text": text})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_start_times_become_ages() {
        let row = |pid: u32, started: Option<i64>| {
            (pid, String::new(), String::new(), String::new(), started)
        };
        let ages = win_ages(
            &[
                row(1, Some(1000)),
                row(2, None),
                row(3, Some(1030)),
                row(4, Some(2000)),
            ],
            1010.0,
        );
        // 3 在将来 20 秒：时钟差一点，算刚启动；4 在将来十几分钟：不对，不算
        assert_eq!(ages, HashMap::from([(1, 10.0), (3, 0.0)]));
    }
}
