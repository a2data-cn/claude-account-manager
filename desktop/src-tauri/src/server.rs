//! 内置 Web 面板服务器：让手机 / 其它电脑通过浏览器查看和切换账号。
//! 页面与脚本版 `cam serve` 是同一个 web/panel.html。

use crate::engine::api::route;
use crate::engine::store::{Store, R};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;
use std::thread::JoinHandle;
use tiny_http::{Header, Request, Response, Server};

const PANEL: &str = include_str!("../../../web/panel.html");
const MANIFEST: &str = r##"{"name":"Claude 账号管理","short_name":"Claude 账号","start_url":"/","display":"standalone","background_color":"#F4F2EC","theme_color":"#F4F2EC"}"##;

pub type OnChange = Arc<dyn Fn() + Send + Sync>;

pub struct ServerHandle {
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
    pub port: u16,
    pub lan: bool,
}

impl ServerHandle {
    pub fn stop(mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

fn send(req: Request, code: u16, ctype: &str, body: Vec<u8>) {
    let resp = Response::from_data(body)
        .with_status_code(code)
        .with_header(header("Content-Type", ctype))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"));
    let _ = req.respond(resp);
}

fn send_json(req: Request, code: u16, v: &Value) {
    send(
        req,
        code,
        "application/json; charset=utf-8",
        serde_json::to_vec(v).unwrap_or_default(),
    );
}

/// 常量时间比较，避免通过响应时间猜令牌。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn handle(mut req: Request, token: &str, on_change: &OnChange) {
    let method = req.method().to_string().to_uppercase();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("/").to_string();

    if method == "GET" && (path == "/" || path == "/index.html") {
        return send(req, 200, "text/html; charset=utf-8", PANEL.as_bytes().to_vec());
    }
    if method == "GET" && path == "/manifest.webmanifest" {
        return send(req, 200, "application/manifest+json", MANIFEST.as_bytes().to_vec());
    }
    if !path.starts_with("/api/") {
        return send_json(req, 404, &json!({"error": "not found"}));
    }
    let auth = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    let got = auth.strip_prefix("Bearer ").unwrap_or("");
    if !ct_eq(got.as_bytes(), token.as_bytes()) {
        return send_json(req, 401, &json!({"error": "需要访问令牌"}));
    }
    let mut raw = String::new();
    if req.as_reader().take(1_000_000).read_to_string(&mut raw).is_err() {
        return send_json(req, 400, &json!({"error": "请求体不是合法 UTF-8"}));
    }
    let body: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return send_json(req, 400, &json!({"error": "请求体不是合法 JSON"})),
        }
    };
    let store = Store::new();
    let (code, out) = route(&store, &method, &url, &body);
    if method != "GET" && code == 200 {
        on_change();
    }
    send_json(req, code, &out);
}

pub fn start(lan: bool, port: u16, on_change: OnChange) -> R<ServerHandle> {
    let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
    let server = Server::http(format!("{}:{}", host, port)).map_err(|e| format!("端口 {} 启动失败：{}", port, e))?;
    let server = Arc::new(server);
    let token = Store::new().server_token();
    let srv = server.clone();
    let thread = std::thread::spawn(move || {
        for req in srv.incoming_requests() {
            let token = token.clone();
            let on_change = on_change.clone();
            std::thread::spawn(move || handle(req, &token, &on_change));
        }
    });
    Ok(ServerHandle {
        server,
        thread: Some(thread),
        port,
        lan,
    })
}

pub fn lan_ips() -> Vec<String> {
    // 第一个地址会用来生成二维码。家庭 / 公司网段（10.x、172.16–31.x、192.168.x）排前面，
    // 否则按字符串排序时 Tailscale（100.x）等虚拟网卡会排到真正的局域网地址前面
    let mut ips: Vec<(bool, String)> = local_ip_address::list_afinet_netifas()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, ip)| match ip {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => {
                Some((!v4.is_private(), v4.to_string()))
            }
            _ => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips.into_iter().map(|(_, ip)| ip).collect()
}
