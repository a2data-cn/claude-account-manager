//! 面板服务的小工具：浏览器收不收 gzip、压缩。面板页面 180 多 KB，压缩后 60 多 KB，手机、隔着 Tailscale 打开快不少。
//! 与 cam.py 的 accepts_gzip 一致。

use flate2::{write::GzEncoder, Compression};
use std::io::Write;

/// 比这小的回复不压缩
pub const GZIP_MIN_BYTES: usize = 1024;

/// 请求头 Accept-Encoding 里有没有 gzip（q=0 表示不要）
pub fn accepts_gzip(header: &str) -> bool {
    for part in header.split(',') {
        let (name, params) = part.trim().split_once(';').unwrap_or((part.trim(), ""));
        let name = name.trim().to_lowercase();
        if name != "gzip" && name != "*" {
            continue;
        }
        let q = params.trim().to_lowercase();
        if let Some(v) = q.strip_prefix("q=") {
            match v.parse::<f64>() {
                Ok(x) if x != 0.0 => {}
                _ => continue,
            }
        }
        return true;
    }
    false
}

pub fn gzip(body: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::new(5));
    if e.write_all(body).is_err() {
        return body.to_vec();
    }
    e.finish().unwrap_or_else(|_| body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_negotiation() {
        assert!(accepts_gzip("gzip, deflate, br"));
        assert!(accepts_gzip("gzip;q=1.0, br"));
        assert!(accepts_gzip("*"));
        assert!(!accepts_gzip("gzip;q=0, br"));
        assert!(!accepts_gzip("deflate, br"));
        assert!(!accepts_gzip(""));
        let body = "账号 ".repeat(500).into_bytes();
        let z = gzip(&body);
        assert!(z.len() < body.len() / 4);
        assert_eq!(&z[..2], &[0x1f, 0x8b]);
    }
}
