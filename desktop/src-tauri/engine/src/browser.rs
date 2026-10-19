//! 网页版账号：每个账号一个独立的浏览器 Profile，登录状态永久保留。

use super::store::*;
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub fn find_browser() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CAM_BROWSER") {
        return Some(PathBuf::from(p));
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        for p in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ] {
            cands.push(PathBuf::from(p));
        }
    }
    #[cfg(target_os = "windows")]
    {
        for var in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                cands.push(base.join("Google/Chrome/Application/chrome.exe"));
                cands.push(base.join("Microsoft/Edge/Application/msedge.exe"));
                cands.push(base.join("BraveSoftware/Brave-Browser/Application/brave.exe"));
            }
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ] {
            for dir in std::env::split_paths(&path) {
                cands.push(dir.join(name));
            }
        }
    }
    cands.into_iter().find(|p| p.exists())
}

pub fn open_web_account(store: &Store, acc: &Value) -> R<()> {
    if s(acc, "kind") != Some("claude_web") {
        return Err("只有网页版账号可以打开浏览器窗口".into());
    }
    let browser = find_browser().ok_or("没找到 Chrome / Edge / Brave，可用环境变量 CAM_BROWSER 指定浏览器路径")?;
    let prof = store.profiles_dir().join(s(acc, "id").unwrap_or("unknown"));
    std::fs::create_dir_all(&prof).map_err(|e| e.to_string())?;
    let url = "https://claude.ai";
    let app_mode = acc
        .get("manual")
        .and_then(|m| m.get("app_mode"))
        .and_then(|v| v.as_bool())
        == Some(true);
    let mut cmd = Command::new(browser);
    cmd.arg(format!("--user-data-dir={}", prof.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(if app_mode {
            format!("--app={}", url)
        } else {
            url.to_string()
        })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008 | 0x00000200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    }
    cmd.spawn().map_err(|e| format!("启动浏览器失败：{}", e))?;
    Ok(())
}
