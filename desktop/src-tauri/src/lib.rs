mod engine;
mod server;

use crate::engine::{
    api, claude, rules,
    store::{accounts, active_code_id, read_json, s, write_json_atomic, Store},
    usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

// ---------------------------------------------------------------- 桌面版设置

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    /// 开启内置 Web 面板（手机访问）
    server_enabled: bool,
    /// 允许局域网访问；关闭时只监听 127.0.0.1
    lan: bool,
    port: u16,
    /// 自动刷新用量间隔（分钟），0 表示关闭
    auto_refresh_min: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            server_enabled: false,
            lan: true,
            port: 8787,
            auto_refresh_min: 10,
        }
    }
}

fn settings_path() -> std::path::PathBuf {
    Store::new().root.join("desktop.json")
}

fn load_settings() -> Settings {
    read_json(&settings_path())
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

struct AppState {
    server: Mutex<Option<server::ServerHandle>>,
    settings: Mutex<Settings>,
}

// ---------------------------------------------------------------- 通知前端与托盘

fn notify_changed(app: &AppHandle) {
    let _ = app.emit("state-changed", ());
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = rebuild_tray(&handle) {
            eprintln!("托盘菜单更新失败：{e}");
        }
    });
}

fn pct(a: &Value, k: &str) -> Option<f64> {
    a.get("usage")?.get(k)?.get("utilization")?.as_f64()
}

fn usage_brief(a: &Value) -> String {
    match (pct(a, "five_hour"), pct(a, "seven_day")) {
        (Some(f), Some(w)) => format!("    5h {:.0}%  ·  7d {:.0}%", f, w),
        (Some(f), None) => format!("    5h {:.0}%", f),
        _ => String::new(),
    }
}

fn build_menu(app: &AppHandle) -> tauri::Result<(Menu<tauri::Wry>, Option<String>)> {
    let data = Store::new().load().unwrap_or_else(|_| json!({}));
    let active = active_code_id(&data);
    let menu = Menu::new(app)?;
    let mut title = None;

    let code: Vec<&Value> = accounts(&data)
        .iter()
        .filter(|a| s(a, "kind") == Some("claude_code"))
        .collect();
    menu.append(&MenuItem::with_id(
        app,
        "hdr-code",
        "Claude Code 账号",
        false,
        None::<&str>,
    )?)?;
    if code.is_empty() {
        menu.append(&MenuItem::with_id(
            app,
            "noop",
            "  还没有，打开主窗口导入",
            false,
            None::<&str>,
        )?)?;
    }
    for a in code {
        let id = s(a, "id").unwrap_or_default();
        let is_active = active.as_deref() == Some(id);
        if is_active {
            title = pct(a, "five_hour").map(|p| format!("{:.0}%", p));
        }
        let text = format!(
            "{}{}",
            s(a, "label").or(s(a, "email")).unwrap_or("未命名"),
            usage_brief(a)
        );
        menu.append(&CheckMenuItem::with_id(
            app,
            format!("switch:{id}"),
            text,
            true,
            is_active,
            None::<&str>,
        )?)?;
    }

    let web: Vec<&Value> = accounts(&data)
        .iter()
        .filter(|a| s(a, "kind") == Some("claude_web"))
        .collect();
    if !web.is_empty() {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        menu.append(&MenuItem::with_id(app, "hdr-web", "网页版", false, None::<&str>)?)?;
        for a in web {
            let id = s(a, "id").unwrap_or_default();
            let text = format!("打开 {}", s(a, "label").unwrap_or("未命名"));
            menu.append(&MenuItem::with_id(app, format!("open:{id}"), text, true, None::<&str>)?)?;
        }
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "refresh", "刷新用量", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?)?;
    Ok((menu, title))
}

fn rebuild_tray(app: &AppHandle) -> tauri::Result<()> {
    let (menu, title) = build_menu(app)?;
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu))?;
        // macOS 菜单栏上直接显示当前账号 5 小时额度
        tray.set_title(title)?;
        tray.set_tooltip(Some("Claude 账号管理"))?;
    }
    Ok(())
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    let app = app.clone();
    let id = id.to_string();
    let kind = id.clone();
    match kind.as_str() {
        "quit" => app.exit(0),
        "show" => show_main(&app),
        _ => {
            // 涉及文件和网络的操作放到后台线程
            std::thread::spawn(move || {
                let store = Store::new();
                let res: Result<(), String> = if let Some(acc) = id.strip_prefix("switch:") {
                    claude::switch_account(&store, acc).map(|_| ())
                } else if let Some(acc) = id.strip_prefix("open:") {
                    let (code, out) = api::route(&store, "POST", &format!("/api/accounts/{acc}/open"), &json!({}));
                    if code == 200 {
                        Ok(())
                    } else {
                        Err(out["error"].as_str().unwrap_or("打开失败").to_string())
                    }
                } else if id == "refresh" {
                    usage::refresh_usage(&store, None).map(|_| ())
                } else {
                    Ok(())
                };
                if let Err(e) = res {
                    let _ = app.emit("toast", json!({"message": e, "bad": true}));
                    show_main(&app);
                }
                notify_changed(&app);
            });
        }
    }
}

// ---------------------------------------------------------------- 内置 Web 服务

fn apply_server(app: &AppHandle, st: &AppState, cfg: &Settings) -> Result<(), String> {
    let mut guard = st.server.lock().map_err(|e| e.to_string())?;
    let same = guard
        .as_ref()
        .map(|h| h.port == cfg.port && h.lan == cfg.lan)
        .unwrap_or(false);
    if cfg.server_enabled && same {
        return Ok(());
    }
    if let Some(h) = guard.take() {
        h.stop();
    }
    if cfg.server_enabled {
        let handle = app.clone();
        let on_change: server::OnChange = Arc::new(move || notify_changed(&handle));
        *guard = Some(server::start(cfg.lan, cfg.port, on_change)?);
    }
    Ok(())
}

fn server_info_value(st: &AppState) -> Value {
    let guard = st.server.lock().unwrap();
    let cfg = st.settings.lock().unwrap().clone();
    let token = Store::new().server_token();
    match guard.as_ref() {
        Some(h) => {
            let mut urls = vec![format!("http://127.0.0.1:{}/?t={}", h.port, token)];
            if h.lan {
                for ip in server::lan_ips() {
                    urls.push(format!("http://{}:{}/?t={}", ip, h.port, token));
                }
            }
            json!({"running": true, "port": h.port, "lan": h.lan, "urls": urls, "settings": cfg})
        }
        None => json!({"running": false, "urls": [], "settings": cfg}),
    }
}

// ---------------------------------------------------------------- 前端命令

/// 前端所有数据操作都走这一个命令，内部与 HTTP API 共用同一套路由。
#[tauri::command]
async fn api_call(app: AppHandle, method: String, path: String, body: Option<Value>) -> Result<Value, String> {
    let m = method.to_uppercase();
    let m2 = m.clone();
    let body = body.unwrap_or_else(|| json!({}));
    let (code, out) = tauri::async_runtime::spawn_blocking(move || api::route(&Store::new(), &m2, &path, &body))
        .await
        .map_err(|e| e.to_string())?;
    if code != 200 {
        return Err(out
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("请求失败")
            .to_string());
    }
    if m != "GET" {
        notify_changed(&app);
    }
    Ok(out)
}

#[tauri::command]
fn server_info(state: tauri::State<'_, AppState>) -> Value {
    server_info_value(&state)
}

#[tauri::command]
fn save_settings(app: AppHandle, state: tauri::State<'_, AppState>, settings: Settings) -> Result<Value, String> {
    if settings.port < 1024 {
        return Err("端口需要在 1024–65535 之间".into());
    }
    apply_server(&app, &state, &settings)?;
    write_json_atomic(&settings_path(), &serde_json::to_value(&settings).unwrap(), false)?;
    *state.settings.lock().unwrap() = settings;
    Ok(server_info_value(&state))
}

#[tauri::command]
fn reset_token(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<Value, String> {
    Store::new().reset_server_token();
    // 服务器线程在启动时读取令牌，重置后需要重启服务，旧链接随之失效
    if let Some(h) = state.server.lock().map_err(|e| e.to_string())?.take() {
        h.stop();
    }
    let cfg = state.settings.lock().map_err(|e| e.to_string())?.clone();
    apply_server(&app, &state, &cfg)?;
    Ok(server_info_value(&state))
}

#[tauri::command]
fn open_data_dir() -> Result<(), String> {
    let dir = Store::new().root;
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd)
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 后台：自动刷新 + 监听脚本版改动

fn accounts_mtime() -> Option<SystemTime> {
    std::fs::metadata(Store::new().accounts_path())
        .and_then(|m| m.modified())
        .ok()
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}

/// 当前账号 5 小时额度超过阈值时提醒一次（每个额度窗口只提醒一次），并给出余量最多的号。
fn check_alert(app: &AppHandle, warned: &mut HashSet<String>) {
    let Ok(data) = Store::new().load() else { return };
    let Some(act) = active_code_id(&data) else { return };
    let Some(acc) = accounts(&data).iter().find(|a| s(a, "id") == Some(act.as_str())) else {
        return;
    };
    let Some(w) = acc.get("usage").and_then(|u| u.get("five_hour")) else {
        return;
    };
    let Some(pct) = w.get("utilization").and_then(|v| v.as_f64()) else {
        return;
    };
    let key = format!("{}|{}", act, w.get("resets_at").and_then(|v| v.as_str()).unwrap_or(""));
    if pct < rules::alert_threshold(&data) as f64 || !warned.insert(key) {
        return;
    }
    let name = s(acc, "label").or(s(acc, "email")).unwrap_or("当前账号");
    let tip = match rules::best_alternative(&data, &act) {
        Some((u, n)) => format!("余量最多的是 {}（已用 {:.0}%），可在托盘菜单里一键切换。", n, u),
        None => "暂时没有其它可用的账号。".to_string(),
    };
    notify(app, &format!("{} 的 5 小时额度已用 {:.0}%", name, pct), &tip);
}

fn background(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_mtime = accounts_mtime();
        let mut last_refresh: Option<Instant> = None;
        let mut last_fp = String::new();
        let mut last_kc = Instant::now();
        let mut warned: HashSet<String> = HashSet::new();
        let mut first = true;
        loop {
            let store = Store::new();
            // 1. 发现新登录的账号自动保存（macOS 钥匙串查询较慢，30 秒一次）
            let with_kc = first || last_kc.elapsed() >= Duration::from_secs(30);
            if with_kc {
                last_kc = Instant::now();
            }
            let fp = claude::live_fingerprint(with_kc);
            if fp != last_fp && (with_kc || !cfg!(target_os = "macos")) {
                last_fp = fp;
                if let Ok(Some(name)) = claude::auto_import(&store) {
                    notify(&app, "发现新账号", &format!("已自动保存：{}", name));
                    last_refresh = None;
                }
            }
            first = false;

            // 2. 时间规则
            match rules::apply_time_rules(&store) {
                Ok(Some(r)) if r.changed => {
                    notify(
                        &app,
                        "已按规则切换账号",
                        &format!("进入时间段，Claude Code 已切换到 {}。正在运行的会话需要重启。", r.label),
                    );
                }
                Err(e) => eprintln!("时间规则执行失败：{e}"),
                _ => {}
            }

            // 3. 自动刷新用量 + 额度提醒
            let every = app
                .state::<AppState>()
                .settings
                .lock()
                .map(|c| c.auto_refresh_min)
                .unwrap_or(10);
            if every > 0
                && last_refresh
                    .map(|t| t.elapsed() >= Duration::from_secs(every * 60))
                    .unwrap_or(true)
            {
                last_refresh = Some(Instant::now());
                if usage::refresh_usage(&store, None).is_ok() {
                    check_alert(&app, &mut warned);
                }
            }

            // 4. 脚本版、手机端或本线程改了数据，同步到窗口和托盘
            let m = accounts_mtime();
            if m != last_mtime {
                last_mtime = m;
                notify_changed(&app);
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

// ---------------------------------------------------------------- 入口

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(AppState {
            server: Mutex::new(None),
            settings: Mutex::new(load_settings()),
        })
        .invoke_handler(tauri::generate_handler![
            api_call,
            server_info,
            save_settings,
            reset_token,
            open_data_dir
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let (menu, title) = build_menu(&handle)?;
            let mut tray = TrayIconBuilder::with_id("main")
                .menu(&menu)
                .tooltip("Claude 账号管理")
                .on_menu_event(|app, event| on_menu(app, event.id().as_ref()));
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let tray = tray.build(app)?;
            let _ = tray.set_title(title);

            let state = app.state::<AppState>();
            let cfg = state.settings.lock().unwrap().clone();
            if let Err(e) = apply_server(&handle, &state, &cfg) {
                eprintln!("Web 面板启动失败：{e}");
            }
            background(handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭窗口时缩到托盘，后台继续刷新用量
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("启动失败");
}
