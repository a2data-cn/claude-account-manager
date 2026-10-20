//! Claude Desktop 使用独立用户数据目录；切换时退出旧实例，再验证目标目录的实例已启动。
//! 不读取或覆盖 Claude Code 的凭据和会话。

use super::store::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn default_user_data() -> PathBuf {
    if let Some(p) = std::env::var_os("CAM_CLAUDE_DESKTOP_DATA") {
        return PathBuf::from(p);
    }
    if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support/Claude")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home_dir)
            .join("Claude")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".config"))
            .join("Claude")
    }
}

pub fn profile_dir(store: &Store, acc: &Value) -> PathBuf {
    if s(acc, "desktop_profile") == Some("default") {
        default_user_data()
    } else {
        store
            .root
            .join("desktop-profiles")
            .join(s(acc, "id").unwrap_or("unknown"))
    }
}

fn profile_has_login(dir: &Path) -> bool {
    if let Ok(Some(v)) = read_json(&dir.join("config.json")) {
        if [
            "oauth:tokenCache",
            "oauth:tokenCacheV2",
            "lastKnownAccountUuid",
        ]
        .iter()
        .any(|k| {
            v.get(*k).is_some_and(|v| match v {
                Value::String(s) => !s.trim().is_empty(),
                Value::Object(m) => !m.is_empty(),
                _ => false,
            })
        }) {
            return true;
        }
    }
    ["Cookies", "Network/Cookies"].iter().any(|p| {
        std::fs::metadata(dir.join(p))
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    })
}

#[derive(Clone, Debug)]
struct DesktopProcess {
    pid: u32,
    // 缺少参数不是默认目录的证据；无法确认时保留 None。
    profile: Option<PathBuf>,
}

fn path_key(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(target_os = "windows") {
        key.to_lowercase()
    } else {
        key
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

fn app_executable(app: &Path) -> PathBuf {
    if app.extension().and_then(|e| e.to_str()) == Some("app") {
        app.join("Contents/MacOS/Claude")
    } else {
        app.to_path_buf()
    }
}

fn executable_candidates(app: Option<&Path>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [
        "/Applications/Claude.app/Contents/MacOS/Claude",
        "/usr/bin/claude-desktop",
        "/opt/Claude/claude",
        "/usr/lib/claude-desktop/claude",
        "/usr/lib/claude-desktop/claude-desktop",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    paths.push(home_dir().join("Applications/Claude.app/Contents/MacOS/Claude"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        paths.push(PathBuf::from(&local).join("Programs/Claude/Claude.exe"));
        paths.push(PathBuf::from(local).join("AnthropicClaude/claude.exe"));
    }
    if let Some(programs) = std::env::var_os("PROGRAMFILES") {
        paths.push(PathBuf::from(programs).join("Claude/Claude.exe"));
    }
    if let Some(app) = app {
        paths.push(app_executable(app));
    }
    if let Some(app) = std::env::var_os("CAM_CLAUDE_DESKTOP") {
        paths.push(app_executable(Path::new(&app)));
    }
    paths.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
    paths
}

// ps 在 Unix 上不保留 argv 的引号，因此用完整的已知可执行路径做前缀匹配，
// 而不是寻找命令行任意位置的 Claude 字样（shell/CLI/Helper 都不能被退出）。
fn unix_desktop_command<'a>(cmd: &'a str, candidates: &[PathBuf]) -> Option<&'a str> {
    for exe in candidates {
        let exe = exe.to_string_lossy();
        let rest = cmd.strip_prefix(exe.as_ref()).or_else(|| {
            cmd.strip_prefix('"')?
                .strip_prefix(exe.as_ref())?
                .strip_prefix('"')
        });
        if let Some(rest) = rest {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                if !rest
                    .split_whitespace()
                    .any(|a| a == "--type" || a.starts_with("--type="))
                {
                    return Some(rest);
                }
            }
        }
    }
    None
}

fn command_profile(cmd: &str) -> Option<PathBuf> {
    let flag = "--user-data-dir";
    let mut offset = 0;
    while let Some(pos) = cmd[offset..].find(flag) {
        let start = offset + pos;
        offset = start + flag.len();
        if start > 0 && !cmd[..start].ends_with([' ', '\t', '"', '\'']) {
            continue;
        }
        let tail = &cmd[offset..];
        let value = if let Some(v) = tail.strip_prefix('=') {
            v
        } else if tail.starts_with(char::is_whitespace) {
            tail.trim_start()
        } else {
            continue;
        };
        let value = if let Some(quote) = value.chars().next().filter(|c| *c == '"' || *c == '\'') {
            let rest = &value[quote.len_utf8()..];
            &rest[..rest.find(quote)?]
        } else {
            // Unix ps shows a space-containing argument without quotes. The switch we
            // pass is last; stop at a subsequent switch when handling other launchers.
            let end = value.find(" --").unwrap_or(value.len());
            value[..end]
                .trim_end_matches('"')
                .trim_end_matches('\'')
                .trim()
        };
        if !value.is_empty() && !value.starts_with("--") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn profile_from_open_files(text: &str) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for name in text.lines().filter_map(|l| l.strip_prefix('n')) {
        for suffix in [
            "/Network/Cookies-wal",
            "/Network/Cookies-shm",
            "/Network/Cookies",
            "/Cookies-wal",
            "/Cookies-shm",
            "/Cookies",
            "/config.json.journal",
            "/config.json",
            "/Local State",
            "/Local Storage/leveldb/LOCK",
            "/Session Storage/LOCK",
        ] {
            if let Some(root) = name.strip_suffix(suffix) {
                let path = PathBuf::from(root);
                if found.as_ref().is_some_and(|p| !same_path(p, &path)) {
                    return None;
                }
                found = Some(path);
                break;
            }
        }
    }
    found
}

trait DesktopRuntime {
    fn processes(&self, app: Option<&Path>) -> R<Vec<DesktopProcess>>;
    fn quit(&self, app: &Path) -> R<()>;
    fn launch(&self, app: &Path, dir: &Path) -> R<()>;
}

struct SystemRuntime;

impl DesktopRuntime for SystemRuntime {
    fn processes(&self, app: Option<&Path>) -> R<Vec<DesktopProcess>> {
        let candidates = executable_candidates(app);
        let mut found = Vec::new();
        #[cfg(target_os = "windows")]
        {
            let script = "$ErrorActionPreference='Stop'; @(Get-CimInstance Win32_Process | Select-Object ProcessId,Name,ExecutablePath,CommandLine) | ConvertTo-Json -Compress";
            let out = Command::new("powershell")
                .args(["-NoProfile", "-Command", script])
                .output()
                .map_err(|e| format!("无法检查 Claude 桌面版进程：{e}"))?;
            if !out.status.success() {
                return Err("无法检查 Claude 桌面版进程（PowerShell 查询失败）".into());
            }
            let rows: Value = serde_json::from_slice(&out.stdout)
                .map_err(|e| format!("无法解析桌面版进程列表：{e}"))?;
            let rows = rows.as_array().cloned().unwrap_or_else(|| vec![rows]);
            for row in rows {
                let Some(exe) = s(&row, "ExecutablePath") else {
                    if s(&row, "Name").is_some_and(|n| n.eq_ignore_ascii_case("claude.exe")) {
                        return Err("无法读取 Claude 进程路径，不能安全区分桌面版和 Claude Code；请先手动退出后再操作".into());
                    }
                    continue;
                };
                let norm = exe.replace('\\', "/").to_lowercase();
                let known = (norm.contains("/anthropicclaude/")
                    || (norm.contains("/windowsapps/anthropic.claude")
                        || norm.contains("/windowsapps/anthropicpbc.claude")))
                    && norm.ends_with("/claude.exe");
                if !known && !candidates.iter().any(|p| same_path(p, Path::new(exe))) {
                    continue;
                }
                let cmd = s(&row, "CommandLine").unwrap_or("");
                if cmd
                    .split_whitespace()
                    .any(|a| a == "--type" || a.starts_with("--type="))
                {
                    continue;
                }
                if let Some(pid) = row["ProcessId"]
                    .as_u64()
                    .and_then(|p| u32::try_from(p).ok())
                {
                    found.push(DesktopProcess {
                        pid,
                        profile: command_profile(cmd),
                    });
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let out = Command::new("ps")
                .args(["-axww", "-o", "pid=,command="])
                .output()
                .map_err(|e| format!("无法检查 Claude 桌面版进程：{e}"))?;
            if !out.status.success() {
                return Err("无法检查 Claude 桌面版进程（ps 查询失败）".into());
            }
            let listing = String::from_utf8(out.stdout)
                .map_err(|e| format!("无法解析桌面版进程列表：{e}"))?;
            for line in listing.lines() {
                let line = line.trim();
                let Some((pid, cmd)) = line.split_once(char::is_whitespace) else {
                    continue;
                };
                let Some(args) = unix_desktop_command(cmd.trim_start(), &candidates) else {
                    continue;
                };
                let pid = pid
                    .parse::<u32>()
                    .map_err(|e| format!("无效桌面版进程编号：{e}"))?;
                let mut profile = command_profile(args);
                #[cfg(target_os = "macos")]
                if profile.is_none() {
                    // 默认从 Dock 打开的实例没有 user-data-dir 参数。以打开的登录数据库
                    // 验证真实目录，不能把缓存 active 或“缺少参数”当成默认目录。
                    let out = Command::new("lsof")
                        .args(["-a", "-p", &pid.to_string(), "-Fn"])
                        .output()
                        .map_err(|e| format!("无法确认 Claude 桌面版的数据目录：{e}"))?;
                    if out.status.success() {
                        profile = profile_from_open_files(&String::from_utf8_lossy(&out.stdout));
                    } else if out.status.code() != Some(1) {
                        return Err("无法确认 Claude 桌面版的数据目录（lsof 查询失败）".into());
                    }
                }
                found.push(DesktopProcess { pid, profile });
            }
        }
        Ok(found)
    }

    fn quit(&self, app: &Path) -> R<()> {
        let running = self.processes(Some(app))?;
        if running.is_empty() {
            return Ok(());
        }
        if cfg!(target_os = "macos") && app.extension().and_then(|e| e.to_str()) == Some("app") {
            let escaped = app
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            let script = format!("tell application \"{escaped}\" to quit");
            let status = Command::new("osascript")
                .args(["-e", &script])
                .status()
                .map_err(|e| format!("无法请求 Claude 桌面版退出：{e}"))?;
            if !status.success() {
                return Err("Claude 桌面版没有接受退出请求，请先手动退出后再切换".into());
            }
        } else {
            for p in running {
                let mut cmd = if cfg!(target_os = "windows") {
                    let mut c = Command::new("taskkill");
                    c.args(["/PID", &p.pid.to_string()]);
                    c
                } else {
                    let mut c = Command::new("kill");
                    c.args(["-TERM", &p.pid.to_string()]);
                    c
                };
                let status = cmd
                    .status()
                    .map_err(|e| format!("无法退出 Claude 桌面版：{e}"))?;
                if !status.success() && self.processes(Some(app))?.iter().any(|x| x.pid == p.pid) {
                    return Err("Claude 桌面版没有退出，请先手动退出后再切换".into());
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if self.processes(Some(app))?.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("Claude 桌面版还在运行，请先手动退出它再切换".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn launch(&self, app: &Path, dir: &Path) -> R<()> {
        launch_verified(app, dir, || self.processes(Some(app)))
    }
}

// 进程观察可注入，测试真实启动器退出码时无需访问用户正在运行的应用。
fn launch_verified(
    app: &Path,
    dir: &Path,
    mut processes: impl FnMut() -> R<Vec<DesktopProcess>>,
) -> R<()> {
    let arg = format!("--user-data-dir={}", dir.display());
    let via_open =
        cfg!(target_os = "macos") && app.extension().and_then(|e| e.to_str()) == Some("app");
    let mut cmd = if via_open {
        let mut c = Command::new("open");
        c.args(["-n", "-a"]).arg(app).args(["--args", &arg]);
        c
    } else {
        let mut c = Command::new(app);
        c.arg(&arg);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 Claude 桌面版失败：{e}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut launcher_done = !via_open;
    let mut confirmed_since: Option<Instant> = None;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                break Err(format!("Claude 桌面版启动程序已退出（{status}）"));
            }
            Ok(Some(_)) => launcher_done = true,
            Ok(None) => {}
            Err(e) => break Err(format!("无法检查桌面版启动程序：{e}")),
        }
        match processes() {
            Ok(procs)
                if launcher_done
                    && !procs.is_empty()
                    && procs
                        .iter()
                        .all(|p| p.profile.as_ref().is_some_and(|p| same_path(p, dir))) =>
            {
                let since = confirmed_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(500) {
                    break Ok(());
                }
            }
            Ok(_) => confirmed_since = None,
            Err(e) => break Err(e),
        }
        if Instant::now() >= deadline {
            break Err("没有确认 Claude 桌面版使用目标数据目录启动；未记录切换成功".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if result.is_err() && via_open && !launcher_done {
        let _ = child.kill();
    }
    // 回收我们创建的启动进程；应用主体允许继续运行。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    result
}

fn find_app() -> R<PathBuf> {
    if let Some(p) = std::env::var_os("CAM_CLAUDE_DESKTOP") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!(
            "CAM_CLAUDE_DESKTOP 指向的文件不存在：{}",
            p.display()
        ));
    }
    let mut cands = Vec::new();
    if cfg!(target_os = "macos") {
        cands.push(PathBuf::from("/Applications/Claude.app"));
        cands.push(home_dir().join("Applications/Claude.app"));
    } else if cfg!(target_os = "windows") {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let base = PathBuf::from(&local).join("AnthropicClaude");
            if let Ok(rd) = std::fs::read_dir(&base) {
                for e in rd.flatten() {
                    let exe = e.path().join("claude.exe");
                    if exe.exists() {
                        cands.push(exe);
                    }
                }
            }
            cands.sort_by(|a, b| b.cmp(a));
            cands.push(PathBuf::from(&local).join("Programs/Claude/Claude.exe"));
            cands.push(PathBuf::from(local).join("AnthropicClaude/claude.exe"));
        }
        if let Some(programs) = std::env::var_os("PROGRAMFILES") {
            cands.push(PathBuf::from(programs).join("Claude/Claude.exe"));
        }
    } else {
        cands.extend(
            [
                "/usr/bin/claude-desktop",
                "/opt/Claude/claude",
                "/usr/lib/claude-desktop/claude",
            ]
            .iter()
            .map(PathBuf::from),
        );
    }
    cands
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| "没找到 Claude 桌面版。请先安装 Claude，再切换。".into())
}

const LOGIN_FILES: [&str; 14] = [
    "config.json",
    "config.json.journal",
    "Cookies",
    "Cookies-journal",
    "Cookies-wal",
    "Cookies-shm",
    "Local State",
    "Network/Cookies",
    "Network/Cookies-journal",
    "Network/Cookies-wal",
    "Network/Cookies-shm",
    "config.json-wal",
    "config.json-shm",
    "Preferences",
];

/// 退出后才复制登录数据库及其日志。不会复制对话和 Cowork 虚拟机。
fn backup_login(store: &Store, dir: &Path) -> R<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let dest = store.new_backup_dir("before-desktop")?;
    for name in LOGIN_FILES {
        let src = dir.join(name);
        if src.is_file() {
            let target = dest.join(name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建登录备份目录失败：{e}"))?;
                set_private(parent, true);
            }
            std::fs::copy(&src, &target).map_err(|e| format!("备份 {name} 失败：{e}"))?;
            set_private(&target, false);
        }
    }
    store.prune_backups();
    Ok(())
}

fn remember(store: &Store, id: &str) -> R<()> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let i = find_index(&data, id)?;
    if !data.get("active").map(|v| v.is_object()).unwrap_or(false) {
        data["active"] = serde_json::json!({});
    }
    data["active"]["claude_desktop"] = serde_json::json!(id);
    accounts_mut(&mut data)[i]["last_switched_at"] = serde_json::json!(now_iso());
    store.save(&mut data)
}

fn previous_profile(store: &Store, procs: &[DesktopProcess], default: &Path) -> R<PathBuf> {
    let mut live: Option<PathBuf> = None;
    for p in procs {
        let profile = p.profile.as_ref().ok_or_else(|| format!(
            "无法确认正在运行的 Claude 桌面版（PID {}）的数据目录。请先手动退出 Claude 后再切换", p.pid))?;
        if live.as_ref().is_some_and(|prev| !same_path(prev, profile)) {
            return Err("有多个不同目录的 Claude 桌面版在运行，请先手动退出后再切换".into());
        }
        live = Some(profile.clone());
    }
    if let Some(profile) = live {
        return Ok(profile);
    }
    let data = store.load()?;
    let active = data.get("active").and_then(|a| s(a, "claude_desktop"));
    Ok(accounts(&data)
        .iter()
        .find(|a| s(a, "id") == active && active.is_some())
        .map(|a| profile_dir(store, a))
        .unwrap_or_else(|| default.to_path_buf()))
}

// 缓存可能早已偏离真实窗口。恢复后只标记能由目录确认的账号，不能确认就清空。
fn reconcile_active(store: &Store, profile: Option<&Path>) -> R<()> {
    let _g = store.lock()?;
    let mut data = store.load()?;
    let id = profile
        .and_then(|profile| {
            accounts(&data).iter().find(|a| {
                s(a, "kind") == Some("claude_desktop") && same_path(&profile_dir(store, a), profile)
            })
        })
        .and_then(|a| s(a, "id"))
        .map(String::from);
    if !data.get("active").is_some_and(|a| a.is_object()) {
        data["active"] = serde_json::json!({});
    }
    data["active"]["claude_desktop"] = serde_json::json!(id);
    store.save(&mut data)
}

fn rollback(store: &Store, runtime: &impl DesktopRuntime, app: &Path, previous: &Path) -> String {
    let restored = runtime
        .quit(app)
        .and_then(|_| runtime.launch(app, previous));
    let mut message = match &restored {
        Ok(()) => "已确认重新打开原来的数据目录。".to_string(),
        Err(e) => format!("自动恢复原窗口失败：{e}。原目录仍保留，请手动打开 Claude。"),
    };
    if let Err(e) = reconcile_active(store, restored.is_ok().then_some(previous)) {
        message.push_str(&format!("账号状态未能更新：{e}。"));
    }
    message
}

fn open_with_runtime(
    store: &Store,
    acc: &Value,
    runtime: &impl DesktopRuntime,
    app: Option<&Path>,
    default: &Path,
    dry: bool,
) -> R<String> {
    if s(acc, "kind") != Some("claude_desktop") {
        return Err("只有 Claude 桌面版账号可以这样切换".into());
    }
    let _switch = try_lock_file(&store.root.join("desktop-switch.lock"))?
        .ok_or("另一个 Claude 桌面版操作正在进行，请稍后重试")?;
    let id = s(acc, "id").ok_or("桌面版账号缺少 ID")?;
    // 拿操作锁后重新读取，不能用路由在等待操作之前取得的旧账号。
    let data = store.load()?;
    let current_acc = &accounts(&data)[find_index(&data, id)?];
    let label = s(current_acc, "label").unwrap_or("未命名");
    let dir = profile_dir(store, current_acc);
    let procs = if dry {
        Vec::new()
    } else {
        runtime.processes(app)?
    };
    let previous = previous_profile(store, &procs, default)?;
    let existed = dir.exists();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if !existed || dir.starts_with(&store.root) {
        set_private(&dir, true);
    }
    if !dry {
        runtime.quit(app.ok_or("没找到 Claude 桌面版")?)?;
    }
    if let Err(e) = backup_login(store, &previous) {
        let restored = app
            .filter(|_| !dry)
            .map(|a| rollback(store, runtime, a, &previous))
            .unwrap_or_default();
        return Err(format!("没有完成登录备份，未切换账号：{e}。{restored}"));
    }
    if !dry {
        let app = app.ok_or("没找到 Claude 桌面版")?;
        if let Err(e) = runtime.launch(app, &dir) {
            return Err(format!(
                "没能打开 {label}：{e}。{}",
                rollback(store, runtime, app, &previous)
            ));
        }
        if let Err(e) = remember(store, id) {
            return Err(format!(
                "未能记录切换状态：{e}。{}",
                rollback(store, runtime, app, &previous)
            ));
        }
    } else {
        remember(store, id)?;
    }
    let mut msg = format!("已切换到 {label}。每个号的登录和对话都留在自己的目录里，不会覆盖别的号，也不会动 Claude Code 的项目和会话。");
    if !profile_has_login(&dir) {
        msg.push_str("这个配置还没登录过，请在窗口里点一次邮箱。登录成功后再切换可继续使用。");
    }
    Ok(msg)
}

/// 整个操作与 Python 共用 desktop-switch.lock。查到未知目录时不退出用户的窗口。
pub fn open_desktop_account(store: &Store, acc: &Value) -> R<String> {
    let dry = std::env::var("CAM_DESKTOP_DRY_RUN").ok().as_deref() == Some("1");
    // 先验证安装路径，不能退出现有窗口以后才发现找不到应用。
    let app = if dry { None } else { Some(find_app()?) };
    open_with_runtime(
        store,
        acc,
        &SystemRuntime,
        app.as_deref(),
        &default_user_data(),
        dry,
    )
}

/// 邮件验证必须接在原客户端登录流程上，不能为了打开链接重启应用或选错目录。
pub fn ensure_verification_target(store: &Store, acc: &Value) -> R<()> {
    let app = find_app()?;
    let processes = SystemRuntime.processes(Some(&app))?;
    let expected = profile_dir(store, acc);
    if processes.is_empty()
        || processes.iter().any(|p| {
            p.profile
                .as_ref()
                .is_none_or(|dir| !same_path(dir, &expected))
        })
    {
        return Err("请先打开绑定的 Claude 桌面版账号，并在那个窗口发起邮箱登录，再打开验证链接；验证时不会重启客户端。".into());
    }
    Ok(())
}

/// 调用者必须先持有 desktop-switch.lock，再持 accounts.lock，避免与切换的锁顺序相反。
/// 默认目录只解除账号记录；独立目录在确认未被使用前不能移动。
pub fn ensure_removable(store: &Store, acc: &Value) -> R<()> {
    if s(acc, "kind") != Some("claude_desktop") {
        return Ok(());
    }
    let data = store.load()?;
    let id = s(acc, "id");
    if id.is_some() && data.get("active").and_then(|a| s(a, "claude_desktop")) == id {
        return Err("这是 Claude 桌面版当前账号，请先切换到其它账号再移除".into());
    }
    if s(acc, "desktop_profile") == Some("default") {
        return Ok(());
    }
    if std::env::var("CAM_DESKTOP_DRY_RUN").ok().as_deref() == Some("1") {
        return Ok(());
    }
    let app = std::env::var_os("CAM_CLAUDE_DESKTOP").map(PathBuf::from);
    ensure_profile_unused(
        &profile_dir(store, acc),
        &SystemRuntime.processes(app.as_deref())?,
    )
}

fn ensure_profile_unused(dir: &Path, procs: &[DesktopProcess]) -> R<()> {
    if procs
        .iter()
        .any(|p| p.profile.as_ref().is_none_or(|p| same_path(p, dir)))
    {
        return Err(
            "Claude 桌面版正在使用此目录，或无法确认运行目录；请先手动退出 Claude 再移除账号"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    struct Fixture {
        store: Store,
        root: PathBuf,
        acc: Value,
        previous: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cam-desktop-lifecycle-{}-{}",
                std::process::id(),
                new_id()
            ));
            let store = Store {
                root: root.join("store"),
            };
            let previous = root.join("previous");
            std::fs::create_dir_all(&previous).unwrap();
            std::fs::write(previous.join("config.json"), "before quit").unwrap();
            let acc = json!({"id":"new", "label":"New", "kind":"claude_desktop", "desktop_profile":"new"});
            let mut data = json!({"active":{}, "accounts":[acc.clone()]});
            store.save(&mut data).unwrap();
            Self {
                store,
                root,
                acc,
                previous,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    struct MockRuntime {
        procs: RefCell<R<Vec<DesktopProcess>>>,
        events: RefCell<Vec<String>>,
        failures: RefCell<Vec<bool>>,
        flush: Option<PathBuf>,
    }
    impl MockRuntime {
        fn new(profile: Option<PathBuf>, failures: Vec<bool>) -> Self {
            Self {
                procs: RefCell::new(Ok(vec![DesktopProcess { pid: 42, profile }])),
                events: RefCell::new(Vec::new()),
                failures: RefCell::new(failures),
                flush: None,
            }
        }
    }
    impl DesktopRuntime for MockRuntime {
        fn processes(&self, _: Option<&Path>) -> R<Vec<DesktopProcess>> {
            self.procs.borrow().clone()
        }
        fn quit(&self, _: &Path) -> R<()> {
            self.events.borrow_mut().push("quit".into());
            if let Some(path) = &self.flush {
                std::fs::write(path, "after quit").unwrap();
            }
            Ok(())
        }
        fn launch(&self, _: &Path, dir: &Path) -> R<()> {
            self.events
                .borrow_mut()
                .push(format!("launch:{}", dir.display()));
            if self.failures.borrow_mut().remove(0) {
                Err("launcher exited 47".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn main_process_matching_excludes_cli_shell_and_helpers() {
        let cands = vec![
            PathBuf::from("/Applications/Claude.app/Contents/MacOS/Claude"),
            PathBuf::from("/opt/Claude/claude"),
            PathBuf::from("/custom dir/desktop"),
        ];
        for cmd in [
            "/Applications/Claude.app/Contents/MacOS/Claude --user-data-dir=/x",
            "/opt/Claude/claude --user-data-dir=/x",
            "/custom dir/desktop --user-data-dir=/x",
        ] {
            assert!(unix_desktop_command(cmd, &cands).is_some(), "{cmd}");
        }
        for cmd in ["claude", "/bin/sh -c /opt/Claude/claude", "/opt/Claude/claude-code", "/Applications/Claude.app/Contents/MacOS/Claude --type=renderer", "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper"] {
            assert!(unix_desktop_command(cmd, &cands).is_none(), "{cmd}");
        }
    }

    #[test]
    fn profile_evidence_preserves_spaces_and_rejects_ambiguity() {
        assert_eq!(
            command_profile("--user-data-dir=/a path/Profile --other"),
            Some(PathBuf::from("/a path/Profile"))
        );
        assert_eq!(
            command_profile("--user-data-dir=\"C:\\User Data\\Profile\" --other"),
            Some(PathBuf::from("C:\\User Data\\Profile"))
        );
        assert_eq!(
            command_profile("--user-data-dir '/a path/Profile'"),
            Some(PathBuf::from("/a path/Profile"))
        );
        assert!(command_profile("--some-other-data-dir=/default").is_none());
        assert_eq!(
            profile_from_open_files(
                "p42\nn/a path/Network/Cookies\nn/a path/Network/Cookies-wal\n"
            ),
            Some(PathBuf::from("/a path"))
        );
        assert!(profile_from_open_files("n/a/Cookies\nn/b/Cookies\n").is_none());
        assert!(profile_from_open_files("n/a/Cache/file\n").is_none());
    }

    #[test]
    fn failed_launcher_restores_previous_without_marking_success() {
        let f = Fixture::new();
        let runtime = MockRuntime::new(Some(f.previous.clone()), vec![true, false]);
        let err = open_with_runtime(
            &f.store,
            &f.acc,
            &runtime,
            Some(Path::new("/fake/app")),
            &f.previous,
            false,
        )
        .unwrap_err();
        assert!(err.contains("已确认重新打开"), "{err}");
        assert!(f.store.load().unwrap()["active"]["claude_desktop"].is_null());
        assert_eq!(runtime.events.borrow().len(), 4); // quit, failed launch, quit, restored launch
        assert!(runtime.events.borrow()[3].ends_with(f.previous.to_str().unwrap()));
    }

    #[test]
    fn rollback_failure_is_reported_and_unknown_process_is_untouched() {
        let f = Fixture::new();
        let runtime = MockRuntime::new(Some(f.previous.clone()), vec![true, true]);
        let err = open_with_runtime(
            &f.store,
            &f.acc,
            &runtime,
            Some(Path::new("/fake/app")),
            &f.previous,
            false,
        )
        .unwrap_err();
        assert!(err.contains("自动恢复原窗口失败"));
        let unknown = MockRuntime::new(None, vec![]);
        assert!(open_with_runtime(
            &f.store,
            &f.acc,
            &unknown,
            Some(Path::new("/fake/app")),
            &f.previous,
            false
        )
        .unwrap_err()
        .contains("无法确认"));
        assert!(unknown.events.borrow().is_empty());
        *unknown.procs.borrow_mut() = Err("process inspection denied".into());
        assert!(open_with_runtime(
            &f.store,
            &f.acc,
            &unknown,
            Some(Path::new("/fake/app")),
            &f.previous,
            false
        )
        .unwrap_err()
        .contains("inspection denied"));
    }

    #[test]
    fn backup_happens_after_quit_and_same_account_reopens() {
        let f = Fixture::new();
        let mut data = f.store.load().unwrap();
        data["active"]["claude_desktop"] = json!("new");
        f.store.save(&mut data).unwrap();
        let mut runtime = MockRuntime::new(Some(f.previous.clone()), vec![false]);
        runtime.flush = Some(f.previous.join("config.json"));
        open_with_runtime(
            &f.store,
            &f.acc,
            &runtime,
            Some(Path::new("/fake/app")),
            &f.previous,
            false,
        )
        .unwrap();
        assert_eq!(runtime.events.borrow()[0], "quit");
        let backup = std::fs::read_dir(f.store.backups_dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(backup.join("config.json")).unwrap(),
            "after quit"
        );
        assert_eq!(
            f.store.load().unwrap()["active"]["claude_desktop"],
            json!("new")
        );
    }

    #[test]
    fn desktop_operation_lock_prevents_side_effects_and_removal_is_guarded() {
        let f = Fixture::new();
        let runtime = MockRuntime::new(Some(f.previous.clone()), vec![]);
        let lock = try_lock_file(&f.store.root.join("desktop-switch.lock"))
            .unwrap()
            .unwrap();
        assert!(open_with_runtime(
            &f.store,
            &f.acc,
            &runtime,
            Some(Path::new("/fake/app")),
            &f.previous,
            false
        )
        .unwrap_err()
        .contains("正在进行"));
        assert!(runtime.events.borrow().is_empty());
        drop(lock);
        assert!(ensure_profile_unused(
            &f.previous,
            &[DesktopProcess {
                pid: 42,
                profile: None
            }]
        )
        .is_err());
        assert!(ensure_profile_unused(
            &f.previous,
            &[DesktopProcess {
                pid: 42,
                profile: Some(f.previous.clone())
            }]
        )
        .is_err());
        assert!(ensure_profile_unused(
            &f.previous,
            &[DesktopProcess {
                pid: 42,
                profile: Some(f.root.join("other"))
            }]
        )
        .is_ok());
        let mut data = f.store.load().unwrap();
        data["active"]["claude_desktop"] = json!("new");
        f.store.save(&mut data).unwrap();
        assert!(ensure_removable(&f.store, &f.acc)
            .unwrap_err()
            .contains("当前账号"));
    }

    #[test]
    fn failed_backup_restores_the_window_and_keeps_active_unconfirmed() {
        let f = Fixture::new();
        std::fs::write(f.store.backups_dir(), "not a directory").unwrap();
        let runtime = MockRuntime::new(Some(f.previous.clone()), vec![false]);
        let err = open_with_runtime(
            &f.store,
            &f.acc,
            &runtime,
            Some(Path::new("/fake/app")),
            &f.previous,
            false,
        )
        .unwrap_err();
        assert!(err.contains("没有完成登录备份"), "{err}");
        assert!(err.contains("已确认重新打开"), "{err}");
        assert_eq!(runtime.events.borrow().len(), 3); // quit, rollback quit, restored launch
        assert!(f.store.load().unwrap()["active"]["claude_desktop"].is_null());
    }

    #[test]
    fn missing_active_uses_default_and_rollback_reconciles_stale_active() {
        let f = Fixture::new();
        assert!(same_path(
            &previous_profile(&f.store, &[], &f.previous).unwrap(),
            &f.previous
        ));
        let mut data = f.store.load().unwrap();
        let old = json!({"id":"old", "kind":"claude_desktop", "desktop_profile":"old"});
        accounts_mut(&mut data).push(old.clone());
        data["active"]["claude_desktop"] = json!("new");
        f.store.save(&mut data).unwrap();
        let actual = profile_dir(&f.store, &old);
        let runtime = MockRuntime::new(Some(actual.clone()), vec![true, false]);
        assert!(open_with_runtime(
            &f.store,
            &f.acc,
            &runtime,
            Some(Path::new("/fake/app")),
            &f.previous,
            false
        )
        .is_err());
        assert_eq!(
            f.store.load().unwrap()["active"]["claude_desktop"],
            json!("old")
        );
    }

    #[cfg(unix)]
    #[test]
    fn actual_failed_child_process_cannot_report_launch_success() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let fake = f.root.join("fake-desktop");
        std::fs::write(&fake, "#!/bin/sh\nexit 47\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = launch_verified(&fake, &f.previous, || Ok(Vec::new())).unwrap_err();
        assert!(err.contains("退出"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn successful_bootstrapper_requires_confirmed_target_process() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let fake = f.root.join("fake-bootstrapper");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        launch_verified(&fake, &f.previous, || {
            Ok(vec![DesktopProcess {
                pid: 42,
                profile: Some(f.previous.clone()),
            }])
        })
        .unwrap();
        assert!(
            launch_verified(&fake, &f.previous, || Err("inspection denied".into()))
                .unwrap_err()
                .contains("inspection denied")
        );
    }

    #[test]
    fn modern_login_cache_and_network_cookie_backup() {
        let f = Fixture::new();
        std::fs::write(
            f.previous.join("config.json"),
            r#"{"oauth:tokenCacheV2":{"entry":"encrypted"}}"#,
        )
        .unwrap();
        assert!(profile_has_login(&f.previous));
        std::fs::create_dir_all(f.previous.join("Network")).unwrap();
        std::fs::write(f.previous.join("Network/Cookies"), "cookie db").unwrap();
        std::fs::write(f.previous.join("Network/Cookies-wal"), "wal").unwrap();
        backup_login(&f.store, &f.previous).unwrap();
        let backup = std::fs::read_dir(f.store.backups_dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(backup.join("Network/Cookies-wal")).unwrap(),
            "wal"
        );
    }
}
