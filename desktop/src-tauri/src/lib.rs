use cam_engine as engine;
mod server;

use crate::engine::{
    api, claude, health, rules,
    store::{accounts, active_code_id, read_json, s, write_json_atomic, Store},
    tray, usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::image::Image;
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

/// 托盘上显示的当前账号额度：macOS 菜单栏上的文字、鼠标悬停的提示、图标上圆点的颜色
struct TrayHead {
    title: Option<String>,
    tooltip: String,
    five_hour: Option<f64>,
}

fn build_menu(app: &AppHandle) -> tauri::Result<(Menu<tauri::Wry>, TrayHead)> {
    let data = Store::new().load().unwrap_or_else(|_| json!({}));
    let active = active_code_id(&data);
    let menu = Menu::new(app)?;
    let mut head = TrayHead {
        title: None,
        tooltip: tray::tooltip(None, None, None),
        five_hour: None,
    };

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
            let five = pct(a, "five_hour");
            head = TrayHead {
                title: five.map(|p| format!("{:.0}%", p)),
                tooltip: tray::tooltip(s(a, "label").or(s(a, "email")), five, pct(a, "seven_day")),
                five_hour: five,
            };
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

    let desk: Vec<&Value> = accounts(&data)
        .iter()
        .filter(|a| s(a, "kind") == Some("claude_desktop"))
        .collect();
    if !desk.is_empty() {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        menu.append(&MenuItem::with_id(app, "hdr-desk", "Claude 桌面版", false, None::<&str>)?)?;
        for a in desk {
            let id = s(a, "id").unwrap_or_default();
            let text = format!("切换 {}", s(a, "label").unwrap_or("未命名"));
            menu.append(&MenuItem::with_id(app, format!("open:{id}"), text, true, None::<&str>)?)?;
        }
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
    Ok((menu, head))
}

/// 托盘图标：应用图标右下角加一个额度圆点（颜色见 engine::tray），没有额度数据时就是应用图标
fn tray_icon(app: &AppHandle, five_hour: Option<f64>) -> Option<Image<'static>> {
    let base = app.default_window_icon()?;
    let rgba = match tray::quota_color(five_hour) {
        Some(c) => tray::with_dot(base.rgba(), base.width(), base.height(), c),
        None => base.rgba().to_vec(),
    };
    Some(Image::new_owned(rgba, base.width(), base.height()))
}

fn rebuild_tray(app: &AppHandle) -> tauri::Result<()> {
    let (menu, head) = build_menu(app)?;
    if let Some(icon) = app.tray_by_id("main") {
        icon.set_menu(Some(menu))?;
        // macOS 菜单栏上直接显示当前账号 5 小时额度；Windows 托盘不显示文字，看圆点颜色和鼠标悬停的提示
        icon.set_title(head.title)?;
        icon.set_tooltip(Some(&head.tooltip))?;
        if let Some(img) = tray_icon(app, head.five_hour) {
            icon.set_icon(Some(img))?;
        }
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
                    // 托盘菜单没法弹选择框：有会话在跑就登记等待，退出后自动切（主窗口 / 面板里可以取消）
                    api::api_switch(&store, acc, Some("wait")).map(|r| {
                        if r["deferred"].as_bool() == Some(true) {
                            let msg = r["message"].as_str().unwrap_or("").to_string();
                            let _ = app.emit("toast", json!({"message": msg, "bad": false}));
                            notify(&app, "等会话结束后切换", &msg);
                        }
                    })
                } else if let Some(acc) = id.strip_prefix("open:") {
                    let (code, out) = api::route(&store, "POST", &format!("/api/accounts/{acc}/open"), &json!({}));
                    if code == 200 {
                        if let Some(msg) = out["message"].as_str() {
                            let _ = app.emit("toast", json!({"message": msg, "bad": false}));
                        }
                        Ok(())
                    } else {
                        Err(out["error"].as_str().unwrap_or("打开失败").to_string())
                    }
                } else if id == "refresh" {
                    // 用户点的“刷新用量”，允许替过期的账号续登录
                    usage::refresh_usage(&store, None, true).map(|_| ())
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

/// 当前账号 5 小时额度超过阈值时提醒一次（每个额度窗口只提醒一次）。
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
    let resets = w.get("resets_at").and_then(|v| v.as_str()).unwrap_or("");
    let key = format!("{}|{}", act, resets);
    if pct < rules::alert_threshold(&data) as f64 || !warned.insert(key) {
        return;
    }
    let name = s(acc, "label").or(s(acc, "email")).unwrap_or("当前账号");
    notify(app, &format!("{} 的 5 小时额度已用 {:.0}%", name, pct), &usage::fmt_reset(resets));
}

fn background(app: AppHandle) {
    std::thread::spawn(move || {
        // 后台任务只让一个进程做（桌面版、cam serve、菜单栏插件谁先拿到谁做），拿到后一直持有到退出
        let mut lease: Option<engine::store::LockGuard> = None;
        let mut last_mtime = accounts_mtime();
        let mut last_refresh: Option<Instant> = None;
        let mut last_fp = String::new();
        let mut last_kc = Instant::now();
        let mut warned: HashSet<String> = HashSet::new();
        let mut first = true;
        loop {
            let store = Store::new();
            if lease.is_none() {
                lease = store.try_background_lock().unwrap_or(None);
            }
            if lease.is_none() {
                // cam serve 在做后台任务；这里只把它写的数据同步到窗口和托盘
                let m = accounts_mtime();
                if m != last_mtime {
                    last_mtime = m;
                    notify_changed(&app);
                }
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
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
                    let warn = claude::running_warning(&claude::running_claude_processes())
                        .unwrap_or_else(|| "正在运行的会话需要重启。".into());
                    notify(
                        &app,
                        "已按规则切换账号",
                        &format!("进入时间段，Claude Code 已切换到 {}。{}", r.label, warn),
                    );
                }
                Err(e) => eprintln!("时间规则执行失败：{e}"),
                _ => {}
            }
            // 2b. 登记过“等会话结束再切”的：会话都退出了就执行
            match claude::apply_pending_switch(&store) {
                Ok(Some((r, reason))) => notify(
                    &app,
                    "已切换账号",
                    &format!(
                        "claude 会话已退出，Claude Code 已切换到 {}{}",
                        r.label,
                        if reason == "rule" { "（时间规则）" } else { "" }
                    ),
                ),
                Err(e) => {
                    eprintln!("等待切换执行失败：{e}");
                    // 登记已作废（目标账号的登录失效了），不会再试，告诉用户一声。与 cam.py 的 background_round 一致
                    if store.load().map(|d| d.get("pending_switch").is_none()).unwrap_or(false) {
                        notify(&app, "等待中的切换没有完成", &e);
                    }
                }
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
                // 定时刷新不替不常用的账号续登录，见 usage::refresh_usage
                let _ = health::check_region(&store, 300); // 界面常显的出口地区，随用量刷新一起更新
                if usage::refresh_usage(&store, None, false).is_ok() {
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
            let (menu, head) = build_menu(&handle)?;
            let mut builder = TrayIconBuilder::with_id("main")
                .menu(&menu)
                .tooltip(&head.tooltip)
                .on_menu_event(|app, event| on_menu(app, event.id().as_ref()));
            if let Some(img) = tray_icon(&handle, head.five_hour) {
                builder = builder.icon(img);
            }
            let icon = builder.build(app)?;
            let _ = icon.set_title(head.title);

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
