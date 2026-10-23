//! 托盘图标和提示文字。图标是应用图标右下角加一个额度圆点，颜色和 macOS 菜单栏小工具、
//! Windows 托盘小工具一样：5 小时额度 70% 以下绿、70–89% 黄、90% 以上红。
//! 只做像素运算和拼字符串，不依赖 Tauri，方便测试。

pub const GREEN: [u8; 3] = [0x2E, 0xA0, 0x5A];
pub const YELLOW: [u8; 3] = [0xE0, 0xA9, 0x3B];
pub const RED: [u8; 3] = [0xE5, 0x48, 0x4D];

/// 5 小时额度用了百分之几 → 圆点颜色；没有额度数据时不画圆点
pub fn quota_color(five_hour: Option<f64>) -> Option<[u8; 3]> {
    let p = five_hour.filter(|p| p.is_finite())?;
    Some(if p >= 90.0 {
        RED
    } else if p >= 70.0 {
        YELLOW
    } else {
        GREEN
    })
}

/// 鼠标放到托盘图标上显示的文字：当前账号和额度。Windows 的托盘提示最多 127 个字，这里远远不到
pub fn tooltip(label: Option<&str>, five_hour: Option<f64>, seven_day: Option<f64>) -> String {
    let Some(label) = label else {
        return "Claude 账号管理".to_string();
    };
    let mut parts = vec![label.to_string()];
    if let Some(p) = five_hour {
        parts.push(format!("5 小时 {p:.0}%"));
    }
    if let Some(p) = seven_day {
        parts.push(format!("7 天 {p:.0}%"));
    }
    format!("Claude 账号：{}", parts.join(" · "))
}

/// 在 RGBA 图标的右下角画一个带白边的圆点（边缘抗锯齿），返回新的像素；数据和尺寸对不上时原样返回
pub fn with_dot(rgba: &[u8], width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || out.len() != w * h * 4 {
        return out;
    }
    let size = w.min(h) as f64;
    let r = size * 0.24;
    let ring = (size * 0.07).max(1.0);
    let (cx, cy) = (w as f64 - r - ring, h as f64 - r - ring);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
            // 圆点连白边盖住多少（0–1），其中圆点本身占多少
            let cover = (r + ring + 0.5 - d).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            let dot = (r + 0.5 - d).clamp(0.0, 1.0);
            let i = (y * w + x) * 4;
            let below = out[i + 3] as f64 / 255.0;
            let alpha = cover + below * (1.0 - cover);
            for k in 0..3 {
                let top = 255.0 * (1.0 - dot) + color[k] as f64 * dot;
                let v = (top * cover + out[i + k] as f64 * below * (1.0 - cover)) / alpha;
                out[i + k] = v.round().clamp(0.0, 255.0) as u8;
            }
            out[i + 3] = (alpha * 255.0).round() as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_follow_the_menubar_thresholds() {
        assert_eq!(quota_color(None), None);
        assert_eq!(quota_color(Some(f64::NAN)), None);
        assert_eq!(quota_color(Some(0.0)), Some(GREEN));
        assert_eq!(quota_color(Some(69.9)), Some(GREEN));
        assert_eq!(quota_color(Some(70.0)), Some(YELLOW));
        assert_eq!(quota_color(Some(89.9)), Some(YELLOW));
        assert_eq!(quota_color(Some(90.0)), Some(RED));
        assert_eq!(quota_color(Some(120.0)), Some(RED));
    }

    #[test]
    fn tooltip_names_the_account_and_its_quota() {
        assert_eq!(tooltip(None, None, None), "Claude 账号管理");
        assert_eq!(tooltip(Some("工作号"), None, None), "Claude 账号：工作号");
        assert_eq!(
            tooltip(Some("工作号"), Some(64.4), Some(30.0)),
            "Claude 账号：工作号 · 5 小时 64% · 7 天 30%"
        );
    }

    #[test]
    fn dot_sits_in_the_bottom_right_corner() {
        let (w, h) = (32u32, 32u32);
        let blank = vec![0u8; (w * h * 4) as usize];
        let img = with_dot(&blank, w, h, RED);
        let px = |x: usize, y: usize| {
            let i = (y * w as usize + x) * 4;
            [img[i], img[i + 1], img[i + 2], img[i + 3]]
        };
        assert_eq!(px(0, 0), [0, 0, 0, 0]); // 左上角不动
        assert_eq!(px(23, 23), [RED[0], RED[1], RED[2], 255]); // 圆点中间
        assert_eq!(px(31, 23)[..3], [255, 255, 255]); // 右边是白边

        // 盖在不透明的图标上：圆点不透明，图标别处原样保留
        let opaque: Vec<u8> = (0..w * h).flat_map(|_| [10, 20, 30, 255]).collect();
        let img = with_dot(&opaque, w, h, GREEN);
        assert_eq!(img[..4], [10, 20, 30, 255]);
        let c = ((23 * w + 23) * 4) as usize;
        assert_eq!(img[c..c + 4], [GREEN[0], GREEN[1], GREEN[2], 255]);
        // 数据长度和尺寸对不上：原样返回
        assert_eq!(with_dot(&[1, 2, 3], 2, 2, RED), vec![1, 2, 3]);
    }
}
