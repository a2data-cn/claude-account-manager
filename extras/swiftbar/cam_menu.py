"""SwiftBar / xbar 菜单栏插件的主体。

菜单栏常驻显示当前 Claude Code 账号的 5 小时额度，例如“工作号 64%”。
下拉菜单可以看到所有账号的额度，点一下就能切换，也能打开网页账号或刷新用量。

插件每次运行（默认每分钟一次）还会顺带完成以下工作，
这样不开桌面版也能拿到“全自动”的效果：
  - 发现 Claude Code 新登录的账号，自动保存
  - 执行时间规则（进入时间段时切号）
  - 每 10 分钟刷新一次用量（环境变量 CAM_MENU_REFRESH_MIN 可改）
  - 当前账号额度超过提醒阈值时弹一次系统通知

菜单里点切换时，有 claude 会话在跑就先登记、等它们退出再切；结果和出错都用系统通知告诉你。

“检查更新”只在点的时候联网（git fetch），查到新版本后菜单里多一项“更新（N 个新改动）”，
点了才更新（cam update：快进、自检，不过就退回）。不会自己定时检查，也不会自动更新。

“全面体检”在后台跑 cam doctor --json，用通知说结论；“在终端看完整体检结果”打开终端运行 cam doctor。

由 install.sh 生成的启动脚本调用，不需要手动运行。
Windows 托盘小工具（extras/windows-tray/cam_tray.py）也用这里的 render 和各个命令，改菜单时两边一起生效。
"""
import base64
import functools
import datetime as dt
import json
import math
import os
import struct
import subprocess
import sys
import time
import zlib

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "scripts"))
import cam  # noqa: E402

CAM_PY = os.path.realpath(os.path.join(HERE, "..", "..", "scripts", "cam.py"))
MENU_PY = os.path.realpath(__file__)
PY = sys.executable
REFRESH_MIN = float(os.environ.get("CAM_MENU_REFRESH_MIN", "10"))
# 全局快捷键的前缀（SwiftBar 注册，任何应用里按都生效）：+C 打开菜单，+1 / 2 / 3… 切到第几个号（按添加顺序）。设成空就不注册
HOTKEY = os.environ.get("CAM_MENU_HOTKEY", "CTRL+OPTION+CMD")
# 点菜单栏图标弹出的卡片小面板（SwiftBar 的 webview，页面是 cam serve 的 /mini）；CAM_MENU_MINI=0 关掉，只用系统菜单
MINI = os.environ.get("CAM_MENU_MINI", "1") != "0"
MINI_WIDTH = 380
# 量出来的高度（Linux 上 Noto 字体量的，Mac 上苹方高一截，各项都多留了一点）：
# 基数是红绿灯 + 底下三个圆按钮 + 页脚；下面是“额度 / 用量”两个页签的那一条。
# “额度”页：当前的 Claude Code 号是占一整行的大卡（名字、邮箱、几个会话在跑、每个额度一根条带重置倒计时、节奏结论和小线、今天本机用了多少、到期）；
# 其余的号一行两张小卡（含间距；卡里是名字、几个额度环和它们的重置时间、7 天节奏小线、一句结论、到期）；两个号以上时上面有一块汇总（含一行提醒）。
# “用量”页：时间范围、四格数字、按小时 / 按天的柱子，下面每个 Claude Code 号一行（最多算四行）。
# 弹窗点开后高度就定了，换页签时变不了：按两页里高的那页算，矮的那页按钮和页脚贴底，中间空着。到上限后整页上下滑
MINI_BASE, MINI_TABS, MINI_SUM, MINI_BIG, MINI_ROW, MINI_MAX = 195, 44, 112, 300, 209, 860  # 上限放得下大卡加一行两个小号
MINI_USAGE, MINI_UROW = 380, 50
MINI_SESSIONS = 370  # “会话”页签：汇总一行 + 五个会话左右（每个两行，五十多像素），比“用量”页签最矮的时候还矮一点


def mini_quota_height(accs):
    """“额度”页签里账号卡和汇总的高度（不含红绿灯、页签、按钮和页脚）。和 mini.html 一个规则：有 Claude Code 号时当前的
    （没有当前的就第一个）是一张大卡，其余的号一行两张；两个号以上时上面有汇总。"""
    n = sum(1 for a in accs if a.get("kind") in ("claude_code", "claude_desktop"))
    code = any(a.get("kind") == "claude_code" for a in accs)
    rest = n - 1 if code else n
    return (MINI_BIG if code else 0) + (rest + 1) // 2 * MINI_ROW + (MINI_SUM if n >= 2 else 0) or MINI_ROW


def mini_usage_height(accs):
    """“用量”页签的高度：固定的一截，加每个 Claude Code 号一行（本机记录只有 Claude Code 的，最少一行、最多算四行）。"""
    n = sum(1 for a in accs if a.get("kind") == "claude_code")
    return MINI_USAGE + MINI_UROW * max(1, min(4, n))


def mini_sessions_height():
    """“会话”页签的高度：一行汇总加五个会话左右；会话再多时这一页自己上下滑，不为它把弹窗撑高。"""
    return MINI_SESSIONS


def mini_size(accs):
    """弹窗的宽、高：卡片里只放 Claude Code 的号和桌面版的号（网页号、API 号不在卡片里）。
    高度按“额度”“用量”“会话”三个页签里最高的那个算（点开后换页签时弹窗变不了大小），到上限后整页上下滑。"""
    return MINI_WIDTH, min(MINI_MAX, MINI_BASE + MINI_TABS + max(mini_quota_height(accs), mini_usage_height(accs),
                                                                  mini_sessions_height()))


LINE = 40  # 提醒、建议这类长句最多显示多少个字（SwiftBar 的 length 参数：截短，完整内容放鼠标悬停提示），免得把菜单撑得很宽
PLAN = {"pro": "Pro", "max": "Max", "team": "Team", "enterprise": "Enterprise", "free": "Free", "api": "API"}

# 浅色 / 深色模式各一个颜色
GREEN = "#1F8A4C,#4CC27A"
YELLOW = "#B87A08,#E0A93B"
RED = "#BF3B33,#EE776E"
GRAY = "#8E8E93,#98989D"


def esc(s):
    """SwiftBar 用 | 分隔参数，标题里的 | 要替换掉。"""
    return str(s or "").replace("|", "｜").replace("\n", " ")


def q(s):
    return '"' + str(s).replace("\\", "\\\\").replace('"', '\\"') + '"'


def action(*args, refresh=True, terminal=False, script=CAM_PY):
    """生成点击后执行 cam 命令（script=MENU_PY 时是本插件的命令）的参数串。"""
    parts = [f"bash={q(PY)}", f"param1={q(script)}"]
    for i, a in enumerate(args, start=2):
        parts.append(f"param{i}={q(a)}")
    parts.append(f"terminal={'true' if terminal else 'false'}")
    if refresh:
        parts.append("refresh=true")
    return " ".join(parts)


def switch_action(acc_id):
    """点一下切换：交给本插件的 switch 命令，它会用系统通知告诉你结果（菜单没有终端，cam 的输出没人看得到）。"""
    return action("switch", acc_id, script=MENU_PY)


def pct(a, key):
    usage = a.get("usage") or {}
    w = usage.get(key)
    if a.get("kind") == "claude_desktop":
        if usage.get("source") not in ("desktop_cache", "desktop_history"):
            return None
        now = dt.datetime.now(dt.timezone.utc)
        reset = cam.parse_iso((w or {}).get("resets_at"))
        sampled = cam.parse_iso(usage.get("fetched_at"))
        lifetime = 5 * 3600 if key == "five_hour" else 7 * 86400
        if (reset and reset <= now) or (not reset and (not sampled or (now - sampled).total_seconds() >= lifetime)):
            return None
    return w["utilization"] if w and w.get("utilization") is not None else None


def color_for(p):
    if p is None:
        return None
    return RED if p >= 90 else YELLOW if p >= 70 else None


def bar(p, n=10):
    """额度小进度条，例如 ▰▰▰▰▱▱▱▱▱▱ 42%。"""
    k = min(n, max(0, round(p / 100 * n)))
    return "▰" * k + "▱" * (n - k) + f" {p:.0f}%"


# ---- 画给 SwiftBar 的小图：菜单栏的额度环、账号行下面的进度条
# 只用标准库写 PNG：按 Retina 画两倍像素，PNG 里标成 144 dpi，再用 width / height 定成点数。
# 颜色是苹果的系统色，浅色 / 深色模式各一组（SwiftBar 的 image 参数可以写“浅色图,深色图”两张）。
# SwiftBar 拿到图后按 width / height 在主屏的倍数上重画一遍（NSImage.lockFocus，Retina 就是 2 倍位图）：
# 画成 2 倍，一个图像素正好对一个屏幕像素；画成 3 倍反而要 48 缩 32，边缘发糊。1 倍屏缩一半也是整数倍。
SCALE = 2
FILL = {"ok": ((52, 199, 89), (48, 209, 88)), "warn": ((255, 159, 10), (255, 179, 64)), "bad": ((255, 59, 48), (255, 105, 97))}
TRACK = ((0, 0, 0, 0.13), (255, 255, 255, 0.24))


def level_of(p):
    """额度分级：<70% 绿（ok）、70–89% 橙（warn）、≥90% 红（bad），和 color_for 一致。"""
    return "bad" if p >= 90 else "warn" if p >= 70 else "ok"


def _png(w, h, rows):
    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    dpm = round(72 * SCALE / 0.0254)  # 像素 / 米：标成 SCALE 倍 dpi，macOS 就按 1/SCALE 的点数显示（width / height 参数再定一次）
    raw = b"".join(b"\x00" + bytes(r) for r in rows)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"pHYs", struct.pack(">IIB", dpm, dpm, 1)) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def _raster(w, h, shapes, ss=4):
    """把几个形状画成 RGBA 像素行。shapes 按先后叠放，每个是 (在不在形状里(x, y), (r, g, b, a))；每个像素采 ss×ss 个点抗锯齿。"""
    rows = []
    for y in range(h):
        row = bytearray()
        for x in range(w):
            r = g = b = a = 0.0
            for j in range(ss):
                for i in range(ss):
                    px, py = x + (i + 0.5) / ss, y + (j + 0.5) / ss
                    for inside, (cr, cg, cb, ca) in reversed(shapes):
                        if inside(px, py):
                            r, g, b, a = r + cr * ca, g + cg * ca, b + cb * ca, a + ca
                            break
            row += bytes((round(r / a), round(g / a), round(b / a), round(255 * a / (ss * ss)))) if a else b"\0\0\0\0"
        rows.append(row)
    return rows


def _pill(x0, x1, cy, r):
    """两端是圆的横条：从 x0 到 x1，粗 2r。"""
    def inside(px, py):
        qx = min(max(px, x0 + r), x1 - r)
        return (px - qx) ** 2 + (py - cy) ** 2 <= r * r
    return inside


@functools.lru_cache(maxsize=512)
def quota_bar(p, dark, w=56, h=6):
    """账号行下面的进度条：淡灰的底，已用的部分按额度上色。按参数缓存：流式模式每半秒渲染一次，不能每次重画。"""
    W, H = w * SCALE, h * SCALE
    r = H / 2
    x1 = max(H, W * min(max(p, 0), 100) / 100)  # 再少也画一个圆点
    return _png(W, H, _raster(W, H, [(_pill(0, W, r, r), TRACK[dark]), (_pill(0, x1, r, r), (*FILL[level_of(p)][dark], 1.0))]))


def quota_ring(p, dark, size=16, thick=2.5):
    """菜单栏的额度环：淡灰的底环，从 12 点开始顺时针画出已用的比例。"""
    S = size * SCALE
    c, ro, ri = S / 2, S / 2, S / 2 - thick * SCALE
    sweep = 2 * math.pi * min(max(p, 0), 100) / 100

    def ring(px, py):
        d = (px - c) ** 2 + (py - c) ** 2
        return ri * ri <= d <= ro * ro

    def arc(px, py):
        return ring(px, py) and math.atan2(px - c, c - py) % (2 * math.pi) <= sweep

    return _png(S, S, _raster(S, S, [(ring, TRACK[dark]), (arc, (*FILL[level_of(p)][dark], 1.0))]))


ICON_W = ICON_H = 16  # 菜单栏图标（点）：四格信号
LAMP_W, LAMP_H = 12, 20  # 红绿灯样式：竖的灯体，比信号格宽一点、高一点（SwiftBar 自己的图标是 21 点），三盏灯各 5 点才看得清
DIM = ((0, 0, 0, 0.22), (255, 255, 255, 0.3))  # 没亮的信号格
# 红绿灯样式画成系统图标的样子（像电池图标：单色的线框，只有有状态的那块是彩色的）：灯体是线框、没亮的灯是淡淡的点，
# 都跟菜单栏文字一个颜色（浅色栏黑、深色栏白，带一点透明），只有亮着的那盏灯是彩色的。实心的灯体在半透明的菜单栏上
# 不管画成深灰还是白都像贴上去的一块，和旁边的系统图标不搭
GLYPH = ((0, 0, 0, 0.85), (255, 255, 255, 0.9))
ICON_STYLES = (("bars", "信号格（像手机信号）"), ("lamp", "红绿灯（竖的三盏灯）"), ("ring", "圆环（5 小时用了多少）"),
               ("rings", "双环（外圈 5 小时、内圈 7 天）"), ("twobar", "两根小条（上 5 小时、下 7 天）"))
QUOTA_STYLES = ("ring", "rings", "twobar")  # 图里画额度的样式：图跟着 5 小时 / 7 天的数变，要把数传给 status_icon
TWOBAR_W, TWOBAR_H = 22, 14  # 两根小条：像电池那么宽，两根条各 4 点粗
# 菜单栏图标旁边写什么（账号库 prefs.menu_text，顺序见 cam.MENU_TEXT）：菜单“更多”里一项一个开关
MENU_TEXT_LABELS = (("name", "名字"), ("five", "5 小时 %"), ("seven", "7 天 %"), ("sessions", "在回答的会话数"))


def icon_size(style):
    """菜单栏图标显示成多少点：(宽, 高)。"""
    return (LAMP_W, LAMP_H) if style == "lamp" else (TWOBAR_W, TWOBAR_H) if style == "twobar" else (ICON_W, ICON_H)


def title_text(items, name, p5, p7, answering):
    """菜单栏图标旁边的字，按 prefs.menu_text 拼：名字、5 小时 %、7 天 %（两个都要时写“17% · 34%”）、在回答的会话数（“● 2”；
    看不出几个时只写“●”，没有在回答的不写）。看不出来的项不写；一项都没有就只剩图标。"""
    parts = [name] if "name" in items and name else []
    nums = [f"{p:.0f}%" for k, p in (("five", p5), ("seven", p7)) if k in items and p is not None]
    if nums:
        parts.append(" · ".join(nums))
    if "sessions" in items and answering:
        parts.append("●" if answering is True else f"● {answering}")
    return " ".join(parts)


def _mix(a, b, g):
    """两个 RGBA 颜色从 a 往 b 过渡 g（0–1）。"""
    return tuple(x + (y - x) * g for x, y in zip(a, b))


@functools.lru_cache(maxsize=256)
def status_icon(dark, level="ok", kind="", phase=0.0, style="bars", p5=None, p7=None, k=SCALE):
    """菜单栏图标，五种样式，菜单或卡片小面板的设置里选：四格信号（像手机信号）、竖的小红绿灯，以及画额度的三种——
    圆环（5 小时用了多少）、双环（外圈 5 小时、内圈 7 天）、两根小条（上 5 小时、下 7 天）。p5 / p7 是整数百分比
    （画额度的样式才用，别的样式传 None，缓存才命中）。百分比的数字用系统字体写在旁边的文字里，菜单栏这么小，画出来的数字不如文字清楚。

    颜色是状态：绿 = 空闲正常，橙 = 工作中 / 要注意 / 等你确认，红 = 要处理。kind="pulse"（工作中）时随 phase（0–1）
    信号格一格格渐亮涨上去再从头来 / 红绿灯照真红绿灯的黄闪：亮着的那盏灯一亮一灭（灭的时候还看得见镜片的颜色），红绿不动；
    "blink"（等你确认：Claude Code 弹着权限提示、或者切换要你确认）时信号格全亮全灭、红绿灯三盏灯一起闪，比黄闪快一倍。
    经典模式只有两帧（phase 0 和 0.375：两种动画都是一帧亮一帧灭），
    流式模式每 0.25 秒一帧。画好的图按参数缓存。k 是一点画几个像素：菜单栏是 2 倍图；Windows 托盘按屏幕缩放画成实际像素
    （16 像素高的托盘图标，红绿灯 k=0.8）。
    """
    if kind == "pulse":
        glow = 0.5 - 0.5 * math.cos(2 * math.pi * phase)  # 0 → 1 → 0
    elif kind == "blink":
        glow = 1.0 if (phase % 0.5) < 0.25 else 0.0  # 每秒亮灭一次
    else:
        glow = 1.0
    if style in QUOTA_STYLES:
        return _quota_icon(dark, level, kind, phase, style, p5, p7, k)
    if style == "lamp":
        W, H = round(LAMP_W * k), round(LAMP_H * k)
        stroke, br, lr, cx = 1.2 * k, 4.5 * k, 2.5 * k, W / 2  # 线框粗细、灯体圆角、灯的半径
        lamps = (("bad", 4.2 * k), ("busy", 10.0 * k), ("ok", 15.8 * k))  # 红、橙、绿三盏灯的圆心 y
        ink = GLYPH[dark]

        def rrect(inset, r):  # 圆角矩形，四边往里缩 inset
            def inside(px, py):
                if not (inset <= px <= W - inset and inset <= py <= H - inset):
                    return False
                qx, qy = min(max(px, inset + r), W - inset - r), min(max(py, inset + r), H - inset - r)
                return (px - qx) ** 2 + (py - qy) ** 2 <= r * r
            return inside

        outer, inner = rrect(0, br), rrect(stroke, br - stroke)

        def frame(px, py):  # 灯体的线框
            return outer(px, py) and not inner(px, py)

        def lamp(cy, r):
            return lambda px, py: (px - cx) ** 2 + (py - cy) ** 2 <= r * r

        # 动画照真红绿灯的黄闪：只有亮着的那盏灯一亮一灭，灭的时候还看得见镜片的颜色（35%），红绿不动，没有光晕。
        # 工作中每秒闪一次（亮半秒灭半秒），等你确认快一倍
        if kind == "pulse":
            lit = (phase % 0.5) < 0.25
        elif kind == "blink":
            lit = (phase % 0.25) < 0.125
        else:
            lit = True
        bulbs = []
        for lv, cy in lamps:
            color = FILL[{"busy": "warn"}.get(lv, lv)][dark]
            if lv == level or kind == "blink":  # 等你确认（blink）时三盏灯一起闪
                bulbs.append((lamp(cy, lr), (*color, 1.0 if lit else 0.35)))
            else:
                bulbs.append((lamp(cy, lr), (*ink[:3], ink[3] * 0.3)))  # 没亮的灯：淡淡的一个点，看得出三盏都在
        return _png(W, H, _raster(W, H, [(frame, ink)] + bulbs))
    S = round(ICON_H * k)
    if kind == "pulse":
        fill = 1 + phase * 3  # 亮的格数 1 → 4 再从头来，每一格渐亮
        lit_of = lambda i: min(max(fill - i, 0.0), 1.0)  # noqa: E731
    else:
        lit_of = lambda i: glow  # noqa: E731
    bw, gap, r = 3 * k, 1 * k, 1.5 * k  # 格子宽 3 点、间隔 1 点、顶是圆的
    bottom = S - 1 * k
    tone = (*FILL[{"busy": "warn"}.get(level, level)][dark], 1.0)

    def bar(i, h):
        cx, top = i * (bw + gap) + bw / 2, bottom - h * k

        def inside(px, py):
            qy = min(max(py, top + r), bottom - r)
            return (px - cx) ** 2 + (py - qy) ** 2 <= r * r
        return inside

    shapes = [(bar(i, h), _mix(DIM[dark], tone, lit_of(i))) for i, h in enumerate((4.5, 7.5, 10.5, 13.5))]
    return _png(S, S, _raster(S, S, shapes))


def _arc(c, ro, ri, p):
    """从 12 点开始顺时针的一段环（两头是圆的）：圆心 c，外半径 ro、内半径 ri，占 p%。p 满 100 是整个环。"""
    sweep = 2 * math.pi * min(max(p, 0), 100) / 100
    rm, rc = (ro + ri) / 2, (ro - ri) / 2
    ends = [(c, c - rm), (c + rm * math.sin(sweep), c - rm * math.cos(sweep))]

    def inside(px, py):
        d = (px - c) ** 2 + (py - c) ** 2
        if ri * ri <= d <= ro * ro and (sweep >= 2 * math.pi or math.atan2(px - c, c - py) % (2 * math.pi) <= sweep):
            return True
        return any((px - x) ** 2 + (py - y) ** 2 <= rc * rc for x, y in ends)
    return inside


def _annulus(c, ro, ri):
    return lambda px, py: ri * ri <= (px - c) ** 2 + (py - c) ** 2 <= ro * ro


def _quota_icon(dark, level, kind, phase, style, p5, p7, k=SCALE):
    """画额度的三种菜单栏图标：颜色是状态（绿 = 空闲正常，橙 = 工作中 / 要注意，红 = 要处理，和红绿灯一样），长短是额度。
    5 小时用状态色，7 天用菜单栏文字的颜色。还没有 5 小时的数时画满一圈 / 一整根，只当状态灯用。

    动画和红绿灯一个节奏：工作中（pulse）彩色那段每秒闪一次（亮半秒、暗半秒，暗的时候留三成五的颜色，像红绿灯的镜片），
    等你确认（blink）快一倍、而且整个图标一起闪（底环和 7 天那段也暗下去），一眼和工作中分得开。
    经典模式的两帧（phase 0 和 0.375）一亮一暗。"""
    if kind == "pulse":
        lit = (phase % 0.5) < 0.25
    elif kind == "blink":
        lit = (phase % 0.25) < 0.125
    else:
        lit = True
    fade = 0.25 if kind == "blink" and not lit else 1.0  # 等你确认的暗拍：整个图标只剩两成五

    def a(rgba, m=1.0):
        return (*rgba[:3], rgba[3] * m * fade)
    tone = (*FILL[{"busy": "warn"}.get(level, level)][dark], 1.0)
    col = a(tone, 1.0 if lit or kind == "blink" else 0.35)
    ink = a(GLYPH[dark])
    track = a(TRACK[dark])
    p5v = 100 if p5 is None else p5
    if style == "twobar":
        W, H = round(TWOBAR_W * k), round(TWOBAR_H * k)
        r = 2 * k
        y5, y7 = 3.5 * k, 10.5 * k
        x0, x1 = 0.5 * k, W - 0.5 * k

        def fill(p):
            return x0 + max(2 * r, (x1 - x0) * min(max(p, 0), 100) / 100)
        shapes = [(_pill(x0, x1, y5, r), track), (_pill(x0, x1, y7, r), track)]
        if p5v > 0:
            shapes.append((_pill(x0, fill(p5v), y5, r), col))
        if p7 is not None and p7 > 0:
            shapes.append((_pill(x0, fill(p7), y7, r), ink))
        return _png(W, H, _raster(W, H, shapes))
    S = round(ICON_W * k)
    c = S / 2
    if style == "ring":
        ro, ri = c - 0.5 * k, c - 3.0 * k
        shapes = [(_annulus(c, ro, ri), track)]
        if p5v > 0:
            shapes.append((_arc(c, ro, ri, p5v), col))
        return _png(S, S, _raster(S, S, shapes))
    ro, ri = c - 0.5 * k, c - 2.7 * k  # 双环：外圈 2.2 点粗，空 1 点，内圈 2 点粗
    ro2, ri2 = ri - 1.0 * k, ri - 3.0 * k
    shapes = [(_annulus(c, ro, ri), track), (_annulus(c, ro2, ri2), track)]
    if p5v > 0:
        shapes.append((_arc(c, ro, ri, p5v), col))
    if p7 is not None and p7 > 0:
        shapes.append((_arc(c, ro2, ri2, p7), ink))
    return _png(S, S, _raster(S, S, shapes))


def image_param(draw, p, w, h):
    """SwiftBar 的 image 参数：浅色、深色两张图，以及显示成多少点。"""
    pics = ",".join(base64.b64encode(draw(p, dark)).decode() for dark in (0, 1))
    return f"image={pics} width={w} height={h}"


WEEKDAYS = "一二三四五六日"


def when_short(t):
    """今天“19:00”，明天“明天 09:00”，一周内“周日 14:00”，再远“11-28 09:00”（和面板一样）。t 是 Unix 秒。"""
    d = dt.datetime.fromtimestamp(t).astimezone()
    days = (d.date() - dt.date.today()).days
    hm = d.strftime("%H:%M")
    if days == 0:
        return hm
    if days == 1:
        return f"明天 {hm}"
    if 1 < days < 7:
        return f"周{WEEKDAYS[d.weekday()]} {hm}"
    return d.strftime("%m-%d ") + hm


def pace_text(samples, a, now=None):
    """7 天额度按采样点推算的结论（cam.usage_pace）：(一句话, 颜色)。还推算不出来时 None。只是提醒，不切号。"""
    try:
        p = cam.usage_pace(samples.get(a["id"]), cam._shown_usage(a), time.time() if now is None else now)
    except Exception:  # noqa: BLE001  只是显示用
        return None
    if not p or not p.get("state"):
        return None
    by = "最近一天" if p["basis"] == "recent" else "这周平均"
    if p["state"] == "full":
        return f"7 天额度用满了，{when_short(p['resets_at'])} 恢复", RED
    if p["state"] == "out":
        h = max(1, round(p["early_secs"] / 3600))
        early = f"{h // 24} 天 {h % 24} 小时" if h >= 24 else f"{h} 小时"
        return f"按{by}的速度，7 天额度 {when_short(p['out_at'])} 左右用完（比重置早 {early}）", YELLOW
    return f"按{by}的速度，7 天额度用不完，重置时大约还剩 {p['left']}%", None


def detail_line(a):
    """账号行下面那行小字：5 小时已用多少、多久后重置、7 天已用多少。"""
    f5, f7 = pct(a, "five_hour"), pct(a, "seven_day")
    parts = []
    if f5 is not None:
        reset = cam.fmt_reset(((a.get("usage") or {}).get("five_hour") or {}).get("resets_at"))
        parts.append(f"5 小时 {f5:.0f}%" + (f"，{reset.split('（')[0]}" if reset else ""))
    if f7 is not None:
        parts.append(f"7 天 {f7:.0f}%")
    return " · ".join(parts)


def usage_nums(a):
    """账号行右边的额度：5 小时用进度条，7 天只写数字。"""
    f5, f7 = pct(a, "five_hour"), pct(a, "seven_day")
    nums = []
    if f5 is not None:
        nums.append(bar(f5))
    if f7 is not None:
        nums.append(f"7天 {f7:.0f}%")
    return ("    " + " · ".join(nums)) if nums else ""


def row_icon(active, level):
    """账号行前面的图标：当前在用的打勾，有问题的标三角（黄 / 红），其余空心圆。"""
    c = RED if level == "error" else YELLOW if level == "warn" else None
    if active:
        return f"sfimage=checkmark.circle.fill sfcolor={c or GREEN}"
    if c:
        return f"sfimage=exclamationmark.triangle.fill sfcolor={c}"
    return "sfimage=circle"


def heading(text):
    """分区标题：灰色小字，像系统菜单的分组标题。

    不带图标：macOS 26 上 SwiftBar 高亮菜单项时会给带 color 的行重排标题，图标和图片就丢了（它的
    menu(_:willHighlight:) 只重建 attributedTitle）。所以带 color 的行一律不放图标、图片。
    """
    return f"{text} | size=12 color=gray"


def short(label, n=8):
    label = label or ""
    return label if len(label) <= n else label[: n - 1] + "…"


def display_width(s):
    """名字占多宽：汉字、全角算 2，字母数字算 1。"""
    return sum(2 if ord(ch) > 0x2E7F else 1 for ch in s)


NAME_WIDTH = 12  # 菜单栏放得下的名字宽度：6 个汉字或 12 个字母


def needs_number(acc):
    """名字是邮箱、没有名字或太宽（超过 6 个汉字 / 12 个字母）时，菜单栏放不下，用序号代替。面板和卡片小面板用同一条规则。"""
    label = acc.get("label") or ""
    return not label or "@" in label or display_width(label) > NAME_WIDTH


def title_name(acc, n):
    """菜单栏上的名字：短名字直接显示；不然显示“1号”这样的序号（和 cam 列表、快捷键的数字一致），详细的在菜单里看。"""
    return f"{n}号" if needs_number(acc) else acc["label"]


def state_path(store):
    return os.path.join(store.root, cam.MENUBAR_STATE)  # cam doctor 据它的修改时间看插件上一轮是什么时候


def load_state(store):
    try:
        with open(state_path(store), encoding="utf-8") as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def save_state(store, st):
    try:
        cam.write_json_atomic(state_path(store), st)
    except OSError:
        pass


def notify(title, body):
    """系统通知，和 cam serve 用同一个实现（cam.notify）。CAM_MENU_NO_NOTIFY=1 关掉。"""
    if os.environ.get("CAM_MENU_NO_NOTIFY"):
        return
    cam.notify(title, body)


def switch_from_menu(store, key):
    """菜单里点了切换。SwiftBar 运行它时没有终端，所以结果和出错都弹通知。

    有 claude 会话在跑时不打断它们，和面板里选“等”一样：先登记，会话都退出后由后台切换，切完再通知一次。
    """
    try:
        r = cam.api_switch(store, key, "wait")
    except Exception as e:  # noqa: BLE001  什么错都要让用户看到
        notify("切换失败", str(e))
        return 1
    title = "等会话结束后切换" if r.get("deferred") else "已切换账号" if r.get("changed") else "没有切换"
    notify(title, r["message"])
    return 0


def check_update_from_menu():
    """菜单里点了“检查更新”：联网查一次，用通知告诉结果。查到新改动后菜单里会出现“更新（N 个新改动）”。"""
    try:
        r = cam.update_check()
    except Exception as e:  # noqa: BLE001
        notify("检查更新失败", str(e))
        return 1
    if not r["checkout"]:
        notify("没法自动更新", r["message"])
    elif r["behind"]:
        subjects = "；".join(c["subject"] for c in r["commits"][:3]) + ("…" if r["behind"] > 3 else "")
        tail = "点菜单里的“更新”安装" if not r["ahead"] and not r["dirty"] else r["message"]
        notify(f"有 {r['behind']} 个新改动", f"{subjects}。{tail}")
    else:
        notify("已经是最新版本", r["message"])
    return 0


def update_from_menu():
    """菜单里点了“更新（N 个新改动）”：运行 cam update，用通知告诉结果（成功、已经最新、自检没过已退回、没法更新的原因）。"""
    try:
        r = cam.apply_update()
    except Exception as e:  # noqa: BLE001
        notify("更新没有完成", str(e))
        return 1
    notify("已更新" if r.get("updated") else "已经是最新版本", r["message"])
    return 0


def checkup_from_menu():
    """菜单里点了“全面体检”：在后台跑 cam doctor --json，用通知说结论（一切正常 / N 项要处理：第一项）。

    体检要联网十几秒；完整结果点“在终端看完整体检结果”。
    """
    try:
        r = subprocess.run([PY, CAM_PY, "doctor", "--json"], capture_output=True, text=True, encoding="utf-8",
                           errors="replace", timeout=300, stdin=subprocess.DEVNULL)
        res = json.loads(r.stdout)
    except Exception as e:  # noqa: BLE001  超时、输出不是 JSON：都要让用户看到
        notify("全面体检没有完成", str(e) or type(e).__name__)
        return 1
    notify("全面体检", cam.checkup_brief(res["summary"]))
    return 0


def background_work(store):
    """自动导入、时间规则、定时刷新、额度提醒。任何一步失败都不影响菜单显示。

    桌面版或 cam serve 在运行时由它们负责后台任务，这里只显示菜单，避免几个进程同时轮询。
    """
    lease = store.background_lock()
    if not lease.try_acquire():
        st = load_state(store)
        st["bg_owner"] = "other"
        st.pop("last_error", None)  # 上次自己做后台任务留下的错误，现在归别人做了，不再显示
        return st
    try:
        st = _background_work(store)
        st.pop("bg_owner", None)
        return st
    finally:
        lease.release()


def _background_work(store):
    # 和 cam serve 跑的是同一轮任务、弹同样的通知；状态存在数据目录里，下一分钟接着用
    st = cam.background_round(store, load_state(store), REFRESH_MIN, notify)
    save_state(store, st)
    return st


STREAMING = False  # 流式模式（见 stream）
FRAMES, FRAME = 8, 0.25  # 流式动画：一圈几帧、每帧几秒（一圈 2 秒：黄闪亮半秒灭半秒，等你确认快一倍，信号格涨落 2 秒一个来回）
SESSION_MAX_AGE = 0  # 数 claude 进程的缓存秒数：经典模式每分钟是个新进程不用缓存，流式模式 5 秒
LAST_LIGHT = None  # render 最近算出的红绿灯，stream 用它决定要不要继续推动画帧
_MINI_CACHE = [0.0, None]  # [探测时间, (端口, 令牌)]


def mini_endpoint(store):
    """本机卡片小面板的 (端口, 令牌)，macOS 的弹窗和 Windows 的小窗都用它。

    要本机有 cam serve 在跑，没有就在后台起一个（只监听 127.0.0.1）；在跑的是旧代码的就重启它；
    在跑的是桌面版的服务时没有这个页面，返回 None。
    """
    if not MINI:
        return None
    if STREAMING and _MINI_CACHE[0] and time.monotonic() - _MINI_CACHE[0] < 60:  # 流式模式每半秒渲染一次，探测一分钟一次就够（没探到也记着，不每帧重试）
        return _MINI_CACHE[1]
    ep = _probe_mini(store)
    _MINI_CACHE[:] = [time.monotonic(), ep]
    return ep


_STALE_TOLD = False  # “旧代码的 cam serve 没能重启”只提醒一次


def _probe_mini(store):
    global _STALE_TOLD
    try:
        port, token = cam.ensure_panel(store)
        stale = cam.serve_outdated(port, token)
        if stale and cam.restart_stale_serve(store, port, token):
            # 在跑的是旧代码的 cam serve（git pull / cam update 之后没重启）：结束它再起一个新的
            port, token = cam.ensure_panel(store)
            notify("面板已更新", "后台的 cam serve 用的是旧代码，已重启（自己用 cam serve --lan 开的要重开一次）")
            stale = False
        if not cam.mini_available(port, token):
            # 没有卡片页：桌面版的服务，或者旧得没有这页又没能重启的 cam serve——在下一个空端口起一个 cam serve 给卡片用
            port, token = cam.ensure_panel(store, need_mini=True)
            stale = False
        if stale and not _STALE_TOLD:  # 没能重启（没找到它的进程）：先用旧的那份卡片，说一次怎么手动弄，别干脆不给卡片
            _STALE_TOLD = True
            notify("卡片用的还是旧代码", "后台的 cam serve 没能自动重启：终端运行 pkill -f 'cam.py serve' 结束它，再点一下图标就会起新的")
    except Exception:  # noqa: BLE001  起不来就只用系统菜单
        return None
    return port, token


def mini_href(store, accs):
    """标题行的 href + webview 参数：左键点图标弹出卡片小面板，右键还是系统菜单。"""
    ep = mini_endpoint(store)
    return mini_params(*ep, accs) if ep else ""


def mini_params(port, token, accs):
    w, h = mini_size(accs)
    return f" href=http://127.0.0.1:{port}/mini?t={token} webview=true webvieww={w} webviewh={h}"


def bg_error(store, st):
    """后台上一轮的错误：自己做的看 st，归桌面版 / cam serve 做的看它们记的心跳。"""
    if st.get("bg_owner") != "other":
        return st.get("last_error")
    try:
        with open(os.path.join(store.root, cam.SERVE_STATE), encoding="utf-8") as f:
            return json.load(f).get("last_error")
    except (OSError, ValueError):
        return None


def health_of(store, a):
    """账号健康状态（正常 / 注意 / 异常），规则见 cam.account_health。"""
    has = a["kind"] == "claude_code" and bool((store.load_secret(a["id"]).get("claudeAiOauth") or {}).get("refreshToken"))
    return cam.account_health(a, has, cam.days_left(a))


def status_light(store, data, accs, active, st):
    """红绿灯：(级别, 一句话, 动画种类, 还有几项)。动画：工作中 = pulse（橙灯一呼一吸），等你确认 = blink（一亮一灭），其它不动。规则和卡片小面板一致：
    bad = 要处理（后台出错、登录对不上、当前号登录失效、额度用完）；busy = 等你确认的切换、有 claude 会话在跑、额度快满、
    当前号要注意、别的号有问题；都没有就是 ok。"""
    items = []  # (级别, 文字, 闪)，按严重程度排
    notice = (data.get("live_mismatch") or data.get("live_unverified") or {}).get("message")
    if notice:
        items.append(("bad", notice, False))
    err = bg_error(store, st)
    if err:
        items.append(("bad", f"后台出错：{err}", False))
    h = health_of(store, active) if active else None
    first = (h["issues"][0]["text"] if h["issues"] else h["label"]) if h else ""
    p5 = pct(active, "five_hour") if active else None
    p7 = pct(active, "seven_day") if active else None
    if h and h["level"] == "error":
        items.append(("bad", f"当前号：{first}", False))
    if p5 is not None and p5 >= 100:
        items.append(("bad", "5 小时额度用完了", False))
    n = cam.session_count(max_age=SESSION_MAX_AGE)
    att = cam.attention_view()  # Claude Code 弹着权限确认（yes / no）等你：钩子记下来的（cam hooks install）
    if att:
        items.append(("busy", att["text"], "blink"))
    pend = cam.pending_switch_view(data)
    if pend:
        items.append(("busy", pend["message"], "blink"))
    if n and cam.session_busy():  # 开着但在等你输入的不算工作中
        c = cam.session_counts()  # 几个在回答、几个在等你确认（看不出是哪个在回答时只写“正在回答”）
        items.append(("busy", cam.session_phrase(n, True, c["answering"], c["waiting"]), "pulse"))
    if p5 is not None and p5 >= 90:
        items.append(("busy", f"5 小时额度 {p5:.0f}%，快满了", False))
    elif p7 is not None and p7 >= 90:
        items.append(("busy", f"7 天额度 {p7:.0f}%，快满了", False))
    if h and h["level"] == "warn":
        items.append(("busy", f"当前号：{first}", False))
    for a in accs:
        if active is not None and a["id"] == active["id"]:
            continue
        ha = health_of(store, a)
        if ha["level"] == "error":
            items.append(("busy", f"{a.get('label') or a.get('email') or '未命名'}：{ha['issues'][0]['text'] if ha['issues'] else ha['label']}", False))
    if not items:
        return "ok", f"空闲 · {n} 个 claude 会话开着" if n else "空闲 · 一切正常", "", 0
    level, text, blink = items[0]
    return level, text, blink, len(items) - 1


def title_account(accs, active):
    """菜单栏标题（Windows 托盘图标）显示哪个号：当前 Claude Code 账号；它没有 5 小时额度时退回到 5 小时用得最少的网页账号，
    也没有就还是当前号（可能是 None）。"""
    if active is not None and pct(active, "five_hour") is not None:
        return active
    web = sorted((a for a in accs if a["kind"] == "claude_web" and pct(a, "five_hour") is not None), key=lambda a: pct(a, "five_hour"))
    return web[0] if web else active


def render(store, st, out=print, rich=None, phase=None):
    """rich：菜单栏画额度环、账号下面画进度条图（SwiftBar 认 image 参数；Windows 托盘不认，用文字进度条）。默认 macOS 上开。"""
    rich = cam.IS_MAC if rich is None else rich
    data = store.load()
    accs = data["accounts"]
    samples = cam.load_usage_samples(store)["accounts"]  # 7 天额度的节奏（只读，没有这个文件时是空的）
    pos = {a["id"]: n for n, a in enumerate(accs, 1)}  # 和 cam 列表里的序号一致；菜单栏的“1号”、快捷键的数字都用它
    act_id = data["active"].get("claude_code")
    active = next((a for a in accs if a["id"] == act_id), None)

    # ---- 菜单栏标题（标题行上的快捷键是“打开菜单”）
    # rich 时图标是四格信号（红绿灯）；要动画时（工作中、等你确认）经典模式放两帧在标题里让 SwiftBar 每 5 秒轮换，
    # 流式模式（phase 给定）每帧只有一行标题，由 stream() 每半秒推一帧
    global LAST_LIGHT
    light = status_light(store, data, accs, active, st) if accs else None
    LAST_LIGHT = light
    kind = light[2] if light else ""
    phases = [phase] if phase is not None else ([0.0, 0.375] if kind else [0.0])  # 经典模式两帧：0.375 这一帧两种动画都是灭的

    prefs = data.get("prefs") or {}
    style = prefs.get("menu_icon") if prefs.get("menu_icon") in dict(ICON_STYLES) else "bars"  # 菜单里选的图标样式
    items = cam.menu_text_items(prefs)  # 图标旁边写什么
    answering = 0
    if light and "sessions" in items:  # status_light 刚数过会话（session_count），这里只读它的结果，不再查一遍进程
        c = cam.session_counts()
        answering = c["answering"] or (True if cam.session_busy() else 0)

    def bars_icon(ph, p5=None, p7=None):
        q = style in QUOTA_STYLES  # 画额度的样式要数（取整，图按参数缓存）；信号格、红绿灯不要，免得每个百分比各存一张
        a5, a7 = (round(p5) if q and p5 is not None else None), (round(p7) if q and p7 is not None else None)
        return image_param(lambda _v, dark: status_icon(dark, light[0], kind, ph, style, a5, a7), None, *icon_size(style))

    heads = []
    shown = title_account(accs, active)
    if not accs:
        heads.append("Claude ⚠ | sfimage=person.2.circle")
    elif shown is not None:
        p5, p7 = pct(shown, "five_hour"), pct(shown, "seven_day")
        name = title_name(shown, pos[shown["id"]]) if rich else short(shown.get("label") or shown.get("email"))
        txt = title_text(items, esc(name), p5, p7, answering)
        head = f"{txt} | " if txt else "| "  # 一项都不写：只有图标（SwiftBar 认没有文字的标题行）
        if rich:  # 带图的行不加 color（会把图丢掉，见 heading）：额度高低由图标颜色表示
            heads += [head + bars_icon(ph, p5, p7) for ph in phases]
        elif p5 is not None:
            # 图标颜色跟着 5 小时额度走：<70% 绿、70–89% 黄、≥90% 红；文字只在快满时变色。
            # 不 rich 时用 SF Symbol（Windows 托盘按 sfcolor 定图标底色）
            heads.append(head + f"sfimage=person.2.circle sfcolor={color_for(p5) or GREEN}" + (f" color={color_for(p5)}" if color_for(p5) else ""))
        else:
            heads.append(head + "sfimage=person.2.circle")
    else:
        heads.append("Claude | sfimage=person.2.circle")
    extra = (mini_href(store, accs) if rich else "") + (" dropdown=false" if len(heads) > 1 else "")  # 多行标题不进下拉菜单
    for i, head in enumerate(heads):
        out(head + extra + (f" shortcut={HOTKEY}+C" if HOTKEY and i == 0 else ""))
    out("---")
    if light:  # 下拉菜单第一行：红绿灯说的是什么
        c = {"ok": GREEN, "busy": YELLOW, "bad": RED}[light[0]]
        more = f"（还有 {light[3]} 项，见下面）" if light[3] else ""
        out(f"● {esc(light[1])}{more} | size=12 color={c} length={LINE}")

    if not accs:
        out("还没有账号 | color=gray")
        err = bg_error(store, st)
        if err:  # 自动保存没成功的原因，完整内容在鼠标悬停提示里
            out(f"⚠ {esc(err)} | color={RED} length={LINE}")
        # 点一下就在终端里跑，结果看得见；存好后菜单一分钟内刷新
        out(f"存下现在登录的 Claude Code 账号 | sfimage=square.and.arrow.down {action('import', terminal=True)}")
        out(f"登录另一个账号（在浏览器里登录） | sfimage=person.badge.plus {action('login', terminal=True)}")
        out("网页版账号：终端运行 cam add-web --label 名字 | size=12 color=gray")
        out("---")

    # ---- Claude Code 账号
    code = [a for a in accs if a["kind"] == "claude_code"]
    if code:
        out(heading("Claude Code 账号"))
        code.sort(key=lambda a: (a["id"] != act_id, pct(a, "five_hour") if pct(a, "five_hour") is not None else 999))
        for a in code:
            plan = PLAN.get(a.get("plan") or (a.get("manual") or {}).get("plan") or "", "")
            plan += f" {cam.plan_tier(a)}" if plan and cam.plan_tier(a) else ""
            h = health_of(store, a)
            flag = f"  ⚠ {h['label']}" if h["level"] in ("warn", "error") else ""

            num = f"{pos[a['id']]} · " if rich and needs_number(a) else ""  # 菜单栏显示的是“1号”时，这里标上 1

            def text(name):
                # rich 时套餐放进右边的小标签（badge），额度画成下面一行的进度条
                return f"{num}{esc(name)}{('  ' + plan) if plan and not rich else ''}{flag}{'' if rich else usage_nums(a)}"

            params = row_icon(a["id"] == act_id, h["level"])
            if a["id"] != act_id:
                c = RED if h["level"] == "error" else color_for(pct(a, "five_hour"))
                # 整行上色只给 Windows 托盘：macOS 26 上带 color 的行会丢图标（见 heading），rich 时颜色由图标和进度条表达
                params += f" {switch_action(a['id'])}" + (f" color={c}" if c and not rich else "")
            if rich and plan:
                params += f" badge={q(plan)}"
            if not rich:  # Windows 托盘：名字、套餐、额度分开给，托盘排成“名字 …… 88% · 7天 72%”，名字下面画一根额度条（SwiftBar 看不到这些）
                f5, f7 = pct(a, "five_hour"), pct(a, "seven_day")
                params += f" wname={q(esc(a.get('label') or a.get('email') or '未命名'))}" + (f" wplan={q(plan)}" if plan else "") \
                    + (f" wflag={q(h['label'])}" if flag else "") + (f" wp5={f5:.0f}" if f5 is not None else "") \
                    + (f" wp7={f7:.0f}" if f7 is not None else "")
            # 快捷键只挂在这一行（按住 Option 的那行是同一项，不重复注册）；当前号没有切换动作，按了不做事
            hk = f" shortcut={HOTKEY}+{pos[a['id']]}" if HOTKEY and pos[a["id"]] <= 9 else ""
            out(f"{text(a.get('label') or a.get('email') or '未命名')} | {params}{hk}")
            for i in h["issues"]:
                out(f"--{'⚠' if i['level'] != 'info' else '·'} {esc(i['text'])[:60]} | color={RED if i['level'] == 'error' else YELLOW if i['level'] == 'warn' else 'gray'}")
                out(f"--   {esc(i['advice'])[:80]} | size=11 color=gray")
            # 子菜单：详细信息
            u = a.get("usage") or {}
            for nm, w, secs, local in cam.usage_windows(u):
                nm = {"5小时": "5 小时", "7天": "7 天"}.get(nm, f"7 天 · {nm}")
                pace = cam.window_pace(w, secs)
                tail = f"，配速 {pace:+.0f}%" if pace is not None and abs(pace) >= 1 else ""
                out(f"--{nm}：已用 {w['utilization']:.0f}%，{cam.fmt_reset(w.get('resets_at'))}{tail}"
                    + (f" | color={YELLOW}" if (pace or 0) >= 15 else ""))
                est = cam.window_estimate(w, local)
                if est:
                    out(f"--   {esc(est)} | size=11 color=gray")
            pt = pace_text(samples, a)
            if pt:
                out(f"--{esc(pt[0])}" + (f" | color={pt[1]}" if pt[1] else ""))
            dl = cam.days_left(a)
            if dl is not None and dl >= 0:
                exp = (a.get("manual") or {}).get("expires_at")
                out(f"--{dl} 天后到期（{exp}）" + (" | color=#BF3B33,#EE776E" if dl <= 2 else ""))
            if a.get("email") and a.get("email") != a.get("label"):
                out(f"--{esc(a['email'])} | color=gray")
            if rich and needs_number(a):  # 菜单栏显示的是序号：子菜单里提示起个短名（面板“编辑”里有昵称一键选）
                out(f"--起个短名（打开面板） | sfimage=pencil {action('panel', refresh=False)}")
            if a["id"] != act_id:
                out(f"--切换到这个账号 | {switch_action(a['id'])}")
            out(f"--只刷新这个账号 | {action('usage', a['id'])}")
            if rich and pct(a, "five_hour") is not None:  # 不加 color：带 color 的行会丢图片（见 heading）
                out(f"{detail_line(a)} | {image_param(quota_bar, pct(a, 'five_hour'), 56, 6)} size=11")
        out("---")

    # ---- 网页版
    web = [a for a in accs if a["kind"] == "claude_web"]
    if web:
        out(heading("网页版"))
        web.sort(key=lambda a: pct(a, "five_hour") if pct(a, "five_hour") is not None else 999)
        for a in web:
            f5 = pct(a, "five_hour")
            plan = PLAN.get(a.get("plan") or (a.get("manual") or {}).get("plan") or "", "")
            plan += f" {cam.plan_tier(a)}" if plan and cam.plan_tier(a) else ""
            u = a.get("usage") or {}

            def text(name):
                return f"{esc(name)}{('  ' + plan) if plan and not rich else ''}{'' if rich else usage_nums(a)}"

            params = (row_icon(False, "warn" if u.get("error") else "ok") + f" {action('open', a['id'], refresh=False)}"
                      + (f" color={color_for(f5)}" if color_for(f5) and not rich else "") + (f" badge={q(plan)}" if rich and plan else ""))
            out(f"{text(a.get('label') or a.get('email') or '未命名')} | {params}")
            # 子菜单
            out(f"--打开浏览器窗口 | {action('open', a['id'], refresh=False)}")
            for k, nm in (("five_hour", "5 小时"), ("seven_day", "7 天")):
                w = u.get(k)
                if w:
                    out(f"--{nm}：已用 {w['utilization']:.0f}%，{cam.fmt_reset(w.get('resets_at'))}")
            pt = pace_text(samples, a)
            if pt:
                out(f"--{esc(pt[0])}" + (f" | color={pt[1]}" if pt[1] else ""))
            dl = cam.days_left(a)
            if dl is not None:
                exp = (a.get("manual") or {}).get("expires_at")
                out((f"--{dl} 天后到期（{exp}）" if dl >= 0 else f"--已过期 {-dl} 天")
                    + ("" if dl > 2 else f" | color={RED}"))
            if u.get("error"):
                out(f"--⚠ {esc(u['error'])[:90]} | color={YELLOW}")
            elif not u:
                out("--填入 sessionKey 可显示额度（见 README） | color=gray")
            out(f"--只刷新这个账号 | {action('usage', a['id'])}")
            if rich and f5 is not None:
                out(f"{detail_line(a)} | {image_param(quota_bar, f5, 56, 6)} size=11")
        out("---")

    # ---- Claude 桌面版
    desk = [a for a in accs if a["kind"] == "claude_desktop"]
    if desk:
        out(heading("Claude 桌面版"))
        act = (data.get("active") or {}).get("claude_desktop")
        try:
            app, per = cam.desktop_view(store, data)  # 开着没有、本机有没有登录：缓存 20 秒，不是每一帧都去查
        except Exception:  # noqa: BLE001  只是显示用
            app, per = None, {}
        app = app or {}
        if app.get("installed") is False:
            out("没找到 Claude 桌面版 | color=gray")
        for a in desk:
            name = esc(a.get("label") or a.get("email") or "未命名")
            d = per.get(a["id"]) or {}
            mark = " · 开着" if d.get("running") else " · 上次打开" if a["id"] == act else ""
            mark += {"expired": " · 登录过期", "no": " · 没有登录", "never": " · 还没登录"}.get(d.get("login"), "")
            nums = [f"{label} {value:.0f}%" for key, label in (("five_hour", "5h"), ("seven_day", "7d"))
                    if (value := pct(a, key)) is not None]
            summary = " · 缓存 " + " / ".join(nums) if nums else ""
            out(f"{name}{mark}{summary} | {action('open', a['id'], refresh=False)}")
            out(f"--切换到这个账号 | {action('open', a['id'], refresh=False)}")
            usage = a.get("usage") or {}
            if usage.get("source") in ("desktop_cache", "desktop_history"):
                for key, label in (("five_hour", "5 小时"), ("seven_day", "7 天")):
                    value = pct(a, key)
                    if value is not None:
                        reset = cam.fmt_reset((usage.get(key) or {}).get("resets_at"))
                        out(f"--{label}：已用 {value:.0f}%，{reset or '未提供重置时间'}")
                pt = pace_text(samples, a)
                if pt:
                    out(f"--{esc(pt[0])}" + (f" | color={pt[1]}" if pt[1] else ""))
                sampled = cam.parse_iso(usage.get("fetched_at"))
                if sampled:
                    stale = (dt.datetime.now(dt.timezone.utc) - sampled).total_seconds() > 1800
                    out(f"--记录于 {sampled.astimezone().strftime('%m-%d %H:%M')}{' · 超过 30 分钟的旧快照' if stale else ''} | color=gray")
                if usage.get("notice"):
                    out(f"--{esc(usage['notice'])[:120]} | color=gray")
            else:
                out("--在 Claude 设置中查看用量后，可读取本地额度 | color=gray")
            out(f"--读取本地额度 | {action('usage', a['id'])}")
        out("---")

    # ---- API
    api = [a for a in accs if a["kind"] == "api"]
    if api:
        out(heading("API"))
        for a in api:
            u = a.get("usage") or {}
            cost = f"    本月 ${u['cost_month_usd']:.2f}" if u.get("cost_month_usd") is not None else ""
            out(f"{esc(a['label'])}{cost}")
        out("---")

    # ---- 操作
    # ---- 常用的三项留在顶层，其余收进“更多”，菜单短一点
    out(f"打开管理面板 | {action('panel', refresh=False)} sfimage=macwindow")  # 改名、规则、隐私设置都在面板里
    out(f"刷新用量 | {action('usage')} sfimage=arrow.clockwise")
    out(f"全面体检 | {action('checkup', script=MENU_PY)} sfimage=stethoscope")  # 体检会续登录、刷新用量，跑完刷新菜单
    upd = saved_update()  # 上次点“检查更新”的结果，不联网
    if upd and upd.get("behind"):
        out(f"更新（{upd['behind']} 个新改动） | {action('update', script=MENU_PY)} sfimage=arrow.down.circle")
    out("更多 | sfimage=ellipsis.circle")
    out(f"--打开终端查看 | {action(terminal=True, refresh=False)} sfimage=terminal")
    out(f"--在终端看完整体检结果 | {action('doctor', terminal=True, refresh=False)} sfimage=list.bullet.clipboard")
    # 一键清理在终端里跑：先列出要改什么、本机配置分，问一句再改（改之前备份，cam env fix --undo 改回去）
    out(f"--一键清理环境… | {action('env', 'fix', terminal=True, refresh=False)} sfimage=wand.and.stars")
    out(f"--检查更新 | {action('update-check', script=MENU_PY)} sfimage=arrow.triangle.2.circlepath")
    if rich:  # 动画模式、图标样式：打勾的是现在的，点另一行就换（卡片小面板的“设置”里是真正的开关）
        out("-----")
        out("--动画模式 | size=12 color=gray")
        for mode, label in (("classic", "经典：每分钟刷新，动画 5 秒一帧"), ("stream", "流式：插件常驻，动画顺滑")):
            on = (mode == "stream") == STREAMING
            out(f"--{label} | size=12" + (" checked=true" if on else f" {action('mode', mode, script=MENU_PY, refresh=False)}"))
        out("--图标样式 | size=12 color=gray")
        for key, label in ICON_STYLES:
            out(f"--{label} | size=12" + (" checked=true" if key == style else f" {action('icon', key, script=MENU_PY)}"))
        out("--图标旁边写什么 | size=12 color=gray")  # 打勾的是要写的，点一下勾上 / 去掉
        for key, label in MENU_TEXT_LABELS:
            out(f"--{label} | size=12 {action('text', key, script=MENU_PY)}" + (" checked=true" if key in items else ""))
    if st.get("bg_owner") == "other":
        out("--自动刷新由桌面版或 cam serve 负责 | size=12 color=gray")
    elif st.get("last_refresh"):
        t = dt.datetime.fromtimestamp(float(st["last_refresh"])).strftime("%H:%M")
        out(f"--用量更新于 {t} | size=12 color=gray")
    # ---- 要紧的提醒留顶层：登录对不上、等待中的切换、后台出错
    notice = (data.get("live_mismatch") or data.get("live_unverified") or {}).get("message")
    if notice:
        out(f"⚠ {esc(notice)} | size=12 color=#B87A08,#E0A93B length={LINE}")
    pend = cam.pending_switch_view(data)
    if pend:
        out(f"⏳ {esc(pend['message'])} | size=12 color=#5A6676,#94A0AF length={LINE}")
    # ---- 环境：顶层一行“环境 75/100 · 最要紧的一条”，每条风险和怎么办收进子菜单
    risks = cam.risk_items(data)
    if cam.region_view(data)["status"]:  # 后台检测过才有评分
        sc = cam.risk_score(risks)
        c = {"low": GREEN, "medium": YELLOW, "high": RED}[sc["grade"]]
        mark = "✓" if sc["grade"] == "low" else "⚠"  # 不用 sfimage：带 color 的行会丢图标（见 heading）
        warns = [r for r in risks if r["level"] in ("warn", "bad")]  # 出口不支持 / 出口变化 / 时区 / 时钟 / 官方状态页 / 账号多地区：只提醒，不拦
        out(f"{mark} 环境 {sc['score']}/100" + (f" · {esc(warns[0]['text'])}" if warns else f"：{esc(sc['label'])}") + f" | size=12 color={c} length={LINE}")
        out(f"--{esc(sc['label'])} | size=12 color=gray")
        for r in warns:
            out(f"--⚠ {esc(r['text'])} | size=12 color=#BF3B33,#E06C66 length=70")
            if r["advice"]:
                out(f"--{esc(r['advice'])} | size=11 color=#5A6676,#94A0AF length=70")
        if not warns:
            out("--没有要注意的 | size=12 color=gray")
    err = bg_error(store, st)
    if err and accs:  # 没账号时已经放在最上面了
        out(f"⚠ {esc(err)} | size=12 color=#B87A08,#E0A93B length={LINE}")


_UPD_CACHE = [0.0, None]


def saved_update():
    """上次“检查更新”的结果。流式模式每半秒渲染一次，而它要跑一次 git：一分钟看一次就够。"""
    if STREAMING and _UPD_CACHE[0] and time.monotonic() - _UPD_CACHE[0] < 60:
        return _UPD_CACHE[1]
    try:
        upd = cam.saved_update_check()
    except Exception:  # noqa: BLE001
        upd = None
    _UPD_CACHE[:] = [time.monotonic(), upd]
    return upd


SOURCE_FILES = (MENU_PY, os.path.realpath(cam.__file__))


def source_ok():
    """新代码能不能导入：在子进程里试一下。坏的（拉到一半、两个文件不是一个版本）就先留在旧代码上，SwiftBar 不会重启死掉的流式插件。"""
    try:
        r = subprocess.run([sys.executable, "-c", "import cam_menu"], cwd=os.path.dirname(MENU_PY),
                           capture_output=True, timeout=60, env=dict(os.environ, CAM_MENU_MINI="0"))
        return r.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def source_stamp():
    """插件和 cam.py 的修改时间：流式模式里代码一变（git pull / cam update）就重新加载自己，不用重启 SwiftBar。"""
    try:
        return tuple(os.path.getmtime(p) for p in SOURCE_FILES)
    except OSError:
        return None


def stream_frame(store, st, phase):
    """流式模式的一帧：整份菜单的文字。出错也给一份能显示的菜单，进程不能死（SwiftBar 不会重启它）。"""
    lines = []
    try:
        render(store, st, out=lines.append, rich=True, phase=phase)
    except Exception as e:  # noqa: BLE001
        lines = ["Claude ⚠ | sfimage=exclamationmark.triangle", "---", f"出错了：{esc(e)} | color=red"]
    return "\n".join(lines) + "\n"


def stream(store):
    """流式模式（SwiftBar 的 streamable 插件，安装时 --stream）：进程常驻，每份菜单以一行 ~~~ 开头推给 SwiftBar，有变化才推。

    后台任务照旧每分钟一轮；信号灯要动画时（工作中、等你确认）每 0.25 秒推一帧，8 帧一个周期（图按参数缓存，不重画；
    一帧和上一帧一样就不推，闪的时候只在亮暗切换时推）；不动时 5 秒看一眼。SwiftBar 对标题行数不变的更新做增量刷新，菜单开着也不会被关掉。
    """
    global STREAMING, SESSION_MAX_AGE
    STREAMING, SESSION_MAX_AGE = True, 5
    st, last_bg, last, frame, phase, stamp = {}, None, None, 0, 0.0, source_stamp()
    while True:
        if os.getppid() == 1:  # SwiftBar 没了（进程被 launchd 收养）：别留着一直跑后台任务
            return
        now_stamp = source_stamp()
        if now_stamp and stamp and now_stamp != stamp:  # 代码更新了：原地换成新代码（pid 和 stdout 不变，SwiftBar 继续读）
            time.sleep(1)  # git / cam update 可能还在写文件，等一秒再换；写到一半时 stamp 是 None，这轮跳过
            stamp = source_stamp()  # 坏的新代码：记下这个版本不再试，等下次再改
            if source_ok():
                sys.stdout.flush()
                try:
                    os.execv(sys.executable, [sys.executable, MENU_PY, "stream"])
                except OSError:
                    pass  # 换不过去就继续用旧代码跑
        now = time.monotonic()
        if last_bg is None or now - last_bg >= 60:
            try:
                st = background_work(store)
            except Exception as e:  # noqa: BLE001
                st = {"last_error": str(e)}
            if last_bg is None:
                launcher_fix(store, st)  # 启动脚本是老版本写的就改写一次（之后 SwiftBar 会重启这个进程）
            last_bg = now
        block = stream_frame(store, st, phase)
        if block != last:
            try:
                sys.stdout.write("~~~\n" + block)
                sys.stdout.flush()
            except (BrokenPipeError, OSError):  # SwiftBar 关了
                return
            last = block
        if LAST_LIGHT and LAST_LIGHT[2]:
            frame = (frame + 1) % FRAMES  # 按帧号算 phase，不累加浮点数：十帧后正好回到 0，缓存的图才能复用
            phase = frame / FRAMES
            time.sleep(FRAME)
        else:
            frame, phase = 0, 0.0
            time.sleep(5)


def set_mode_from_menu(mode):
    """菜单里切换经典 / 流式：重跑安装脚本改写 SwiftBar 的启动脚本（插件类型写在里面），再重启 SwiftBar 让它按新模式加载。"""
    script = os.path.join(os.path.dirname(MENU_PY), "install.sh")
    try:  # 插件继续用现在这个 python：SwiftBar 的 PATH 很短，不让脚本重找
        r = subprocess.run(["bash", script, f"--{mode}"], capture_output=True, text=True, timeout=180,
                           env=dict(os.environ, CAM_PYTHON=sys.executable))
    except (OSError, subprocess.SubprocessError) as e:
        notify("切换失败", str(e)[:200])
        return 1
    if r.returncode != 0:
        notify("切换失败", (r.stderr or r.stdout).strip()[-200:] or "安装脚本出错")
        return 1
    notify("菜单栏小工具", f"已切换到{'流式' if mode == 'stream' else '经典'}模式，SwiftBar 正在重新打开")
    # SwiftBar 只在加载插件时看类型，所以退出再打开一次；放到独立进程里，别跟着本进程一起被收掉
    subprocess.Popen(["bash", "-c", "sleep 1; osascript -e 'quit app \"SwiftBar\"'; sleep 1; open -a SwiftBar"],
                     start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return 0


def launcher_fix(store, st):
    """SwiftBar 插件目录里的启动脚本是老版本写的（persistentWebView=false：每次点开都新建网页再加载，点一下卡半秒）：
    按它现在的模式重跑一次安装脚本改写（SwiftBar 看到文件变了会重新加载插件）。只试一次，失败也不每分钟重跑。"""
    if not (cam.IS_MAC or os.environ.get("CAM_SWIFTBAR_DIR")):
        return
    path = os.path.join(cam.swiftbar_plugin_dir(), cam.MENUBAR_PLUGIN)
    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return  # 没装（比如在终端直接运行 cam_menu.py 看效果）
    if "persistentWebView>false<" not in text or st.get("launcher_fixed") == path:
        return
    st["launcher_fixed"] = path
    save_state(store, st)
    mode = "stream" if "<swiftbar.type>streamable</swiftbar.type>" in text else "classic"
    script = os.path.join(os.path.dirname(MENU_PY), "install.sh")
    try:
        r = subprocess.run(["bash", script, f"--{mode}"], capture_output=True, text=True, timeout=180,
                           env=dict(os.environ, CAM_PYTHON=sys.executable))
    except (OSError, subprocess.SubprocessError):
        return
    if r.returncode == 0:
        notify("菜单栏小工具已更新", "卡片弹窗改成常驻，点开不再每次重新加载；SwiftBar 会重新加载一次插件")


def set_icon_from_menu(store, style):
    """菜单里选图标样式：记到账号库的 prefs 里（走 cam 的 Store），插件下一次渲染就换。"""
    with store.lock():
        data = store.load()
        data.setdefault("prefs", {})["menu_icon"] = style
        store.save(data)
    return 0


def toggle_text_from_menu(store, key):
    """菜单里点“图标旁边写什么”的一项：勾上的去掉、没勾的勾上，记到账号库的 prefs.menu_text（按 cam.MENU_TEXT 的顺序）。"""
    with store.lock():
        data = store.load()
        prefs = data.setdefault("prefs", {})
        on = cam.menu_text_items(prefs)
        prefs["menu_text"] = [k for k in cam.MENU_TEXT if (k in on) != (k == key)]
        store.save(data)
    return 0


def main():
    store = cam.Store()
    if sys.argv[1:2] == ["stream"]:
        return stream(store)
    if sys.argv[1:2] == ["icon"] and sys.argv[2:3] and sys.argv[2] in dict(ICON_STYLES):
        sys.exit(set_icon_from_menu(store, sys.argv[2]))
    if sys.argv[1:2] == ["text"] and sys.argv[2:3] and sys.argv[2] in cam.MENU_TEXT:
        sys.exit(toggle_text_from_menu(store, sys.argv[2]))
    if sys.argv[1:2] == ["mode"] and sys.argv[2:3] and sys.argv[2] in ("stream", "classic"):
        sys.exit(set_mode_from_menu(sys.argv[2]))
    if sys.argv[1:2] == ["switch"] and len(sys.argv) > 2:  # 菜单里点了切换，见 switch_action
        sys.exit(switch_from_menu(store, sys.argv[2]))
    if sys.argv[1:2] == ["update-check"]:
        sys.exit(check_update_from_menu())
    if sys.argv[1:2] == ["update"]:
        sys.exit(update_from_menu())
    if sys.argv[1:2] == ["checkup"]:
        sys.exit(checkup_from_menu())
    st = {}
    try:
        st = background_work(store)
    except Exception as e:  # noqa: BLE001
        st = {"last_error": str(e)}
    launcher_fix(store, st)
    try:
        render(store, st)
    except Exception as e:  # noqa: BLE001
        print("Claude ⚠")
        print("---")
        print(f"出错了：{esc(e)} | color=red")


if __name__ == "__main__":
    main()
