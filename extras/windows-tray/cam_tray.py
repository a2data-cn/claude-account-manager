"""Windows 托盘小工具的 Python 部分。cam-tray.ps1 负责画图标和菜单，内容都由这里算。

菜单和 macOS 菜单栏小工具（extras/swiftbar/cam_menu.py）是同一份：这里调用它的 render，
把 SwiftBar 格式的菜单行转成 JSON 交给 cam-tray.ps1，两边显示的内容、点了做什么都一样。
每次算菜单前顺带跑一轮后台任务（自动保存新登录、时间规则、等待中的切换、刷新用量、额度提醒），
和菜单栏小工具共用后台锁：桌面版或 cam serve 在跑时由它们做。

用法（都由 cam-tray.ps1 调用，输出一行 JSON）：
  cam_tray.py menu [像素]         托盘图标、提示文字和菜单：{"tip", "badge", "level", "icon", "watch", "items", "notes", "mini"}
                                 像素是托盘图标的实际大小（随屏幕缩放：16、20、24…），不给就是 16
  cam_tray.py light [像素]        只看状态、不做后台任务不算菜单：{"tip", "badge", "level", "icon", "watch", "notes"}；
                                 开着 Claude Code 会话时托盘每 20 秒问一次，图标的动画（工作中、等你确认）跟得上
  cam_tray.py do <种类> 参数…    菜单里点了某一项：{"notes": [{"title", "body"}]}，通知由托盘弹出
    menu switch|update-check|update|checkup …  菜单栏小工具自己的命令
    cam  usage|open|panel …                     cam 命令，出错时把报错写进通知
    terminal [cam 参数…]                        新开一个终端窗口运行 cam
    icon <样式>                                 换托盘图标的样式（记到账号库 prefs.menu_icon，和 macOS 菜单栏共用这一项）
"""
import contextlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "swiftbar"))
import cam_menu  # noqa: E402

cam = cam_menu.cam
CREATE_NEW_CONSOLE = 0x00000010

# Windows 菜单的字体（微软雅黑）里不一定有这几个符号，换成一定显示得出来的
CHARS = str.maketrans({"▰": "■", "▱": "□", "⚠": "！", "⏳": "…"})
# 菜单栏小工具的三种颜色（浅色模式那一个）对应托盘图标的底色
LEVELS = {cam_menu.GREEN: "green", cam_menu.YELLOW: "yellow", cam_menu.RED: "red"}

# 托盘图标的样式：macOS 菜单栏的五种，加上 Windows 才有的“数字方块”（托盘图标旁边不能写字，要看数字只能画在图标里）。
# 没选过时用数字方块（以前的样子）；在 Mac 上选的样式这里也认
TRAY_STYLES = (("badge", "数字方块（图标里写 5 小时 %）"),) + tuple(cam_menu.ICON_STYLES)
FRAMES, FRAME_MS = cam_menu.FRAMES, int(cam_menu.FRAME * 1000)  # 动画和菜单栏流式模式一样：0.25 秒一帧，8 帧两秒一圈

PARAM = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')


def parse_params(s):
    """SwiftBar 的参数串：key=value 或 key="带空格、\\" 转义的值"。"""
    out = {}
    for k, v in PARAM.findall(s or ""):
        if v.startswith('"'):
            v = re.sub(r"\\(.)", r"\1", v[1:-1])
        out[k] = v
    return out


def light(c):
    """菜单栏小工具的颜色写成“浅色,深色”，托盘菜单的底色是浅的，取前一个。"""
    return (c or "").split(",")[0]


def item_of(text, p):
    """一行菜单转成 JSON 菜单项。点了要做的事放在 run 里，交给 cam_tray.py do。"""
    it = {"text": text.translate(CHARS).strip()}
    if p.get("sfimage") == "checkmark.circle.fill":  # 当前在用的账号（环境评分那行的盾牌图标不算）
        it["checked"] = True
    c = p.get("color")
    if c == "gray" or p.get("size"):
        it["dim"] = True
    elif c:
        it["color"] = light(c)
    if "bash" in p:
        args = [p[f"param{i}"] for i in range(2, 20) if f"param{i}" in p]
        script = p.get("param1")
        if p.get("terminal") == "true":
            it["run"] = ["terminal", *args]
        elif script == cam_menu.MENU_PY:
            it["run"] = ["menu", *args]
        else:
            it["run"] = ["cam", *args]
        it["refresh"] = p.get("refresh") == "true"
    return it


def convert(lines):
    """菜单栏小工具输出的行 → (标题行, 菜单树)。

    “--”开头的是上一项的子菜单；“---”是分隔线；alternate=true 是按住 Option 才显示的那一行，Windows 没有，跳过。
    """
    head, _, head_params = lines[0].partition(" | ") if lines else ("Claude", "", "")
    items, last = [], None
    for line in lines[1:]:
        if line == "---":
            if items and not items[-1].get("sep"):
                items.append({"sep": True})
            last = None
            continue
        depth = 0
        while line.startswith("--"):
            line, depth = line[2:], depth + 1
        text, _, params = line.partition(" | ")
        p = parse_params(params)
        if p.get("alternate") == "true":
            continue
        it = item_of(text, p)
        if depth == 0:
            items.append(it)
            last = it
        elif last is not None:
            last.setdefault("children", []).append(it)
    while items and items[-1].get("sep"):
        items.pop()
    return (head, parse_params(head_params)), items


def badge_of(title):
    """托盘图标上的字和底色：5 小时额度的百分比，颜色和菜单栏小工具的图标一样（<70% 绿、70–89% 黄、≥90% 红）。"""
    (text, p) = title
    m = re.search(r"(\d+)%$", text)
    level = LEVELS.get(p.get("sfcolor"), "gray")
    if m:
        n = int(m.group(1))
        name = text[: m.start()].strip()
        return ("满" if n >= 100 else str(n)), level, f"{name} · 5 小时额度已用 {n}%"
    if "⚠" in text:
        return "!", "gray", "Claude 账号：还没有账号，点开看怎么添加"
    return "C", "gray", f"Claude 账号：{text}" if text != "Claude" else "Claude 账号"


def tray_style(data):
    s = (data.get("prefs") or {}).get("menu_icon")
    return s if s in dict(TRAY_STYLES) else "badge"


def taskbar_dark():
    """任务栏是不是深色：Windows 设置里“选择你的默认 Windows 模式”是深色时（SystemUsesLightTheme=0）是。读不到当深色（Windows 10 的默认）。
    测试用 CAM_TRAY_DARK=0 / 1。"""
    v = os.environ.get("CAM_TRAY_DARK")
    if v in ("0", "1"):
        return v == "1"
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize") as k:
            return winreg.QueryValueEx(k, "SystemUsesLightTheme")[0] == 0
    except (ImportError, OSError):
        return True


def icon_dir():
    """画好的托盘图标放哪：%LOCALAPPDATA%\\cam\\tray-icons（测试用 CAM_TRAY_ICONS 指到临时目录）。按参数起文件名，画过的不再画。"""
    d = os.environ.get("CAM_TRAY_ICONS") or os.path.join(os.environ.get("LOCALAPPDATA") or tempfile.gettempdir(), "cam", "tray-icons")
    os.makedirs(d, exist_ok=True)
    return d


ICON_KEEP = 400  # 画额度的样式每个百分比一张图，攒多了删掉最早的（每张几百字节）


def icon_file(style, level, kind, phase, p5, p7, dark, px):
    """画一帧托盘图标存成 PNG，返回路径。和 macOS 菜单栏是同一套画法（cam_menu.status_icon），只是按屏幕的实际像素画：
    图标的长边等于托盘图标的大小（红绿灯竖着、两根小条横着，cam-tray.ps1 摆到正中）。"""
    d = icon_dir()
    path = os.path.join(d, f"{style}-{level}-{kind or 'still'}-{phase:.3f}-{p5}-{p7}-{'d' if dark else 'l'}-{px}.png")
    if not os.path.exists(path):
        w, h = cam_menu.icon_size(style)
        png = cam_menu.status_icon(int(dark), level, kind, phase, style, p5, p7, k=px / max(w, h))
        tmp = f"{path}.{os.getpid()}.tmp"
        with open(tmp, "wb") as f:
            f.write(png)
        os.replace(tmp, path)
        names = [n for n in os.listdir(d) if n.endswith(".png")]
        if len(names) > ICON_KEEP:
            names.sort(key=lambda n: os.path.getmtime(os.path.join(d, n)))
            for n in names[: len(names) - ICON_KEEP // 2]:
                with contextlib.suppress(OSError):
                    os.remove(os.path.join(d, n))
    return path


def icon_of(data, light, badge, level, px):
    """托盘图标：{"style", "kind", "ms", "frames": [PNG 路径…]}，有动画时 8 帧轮着放；数字方块要写字，由 cam-tray.ps1 自己画：
    {"style": "badge", "kind", "ms", "text", "level"}。kind 是红绿灯的动画：pulse = 工作中，blink = 等你确认，空 = 不动。"""
    kind = light[2] if light else ""
    style = tray_style(data)
    if style == "badge" or not light:  # 还没有账号时只有数字方块那种“!”
        return {"style": "badge", "kind": kind, "ms": FRAME_MS, "text": badge, "level": level}
    shown = cam_menu.title_account(data["accounts"], next(
        (a for a in data["accounts"] if a["id"] == data["active"].get("claude_code")), None))
    q = style in cam_menu.QUOTA_STYLES  # 画额度的样式才要数，取整（图按参数存）
    p5 = cam_menu.pct(shown, "five_hour") if q and shown else None
    p7 = cam_menu.pct(shown, "seven_day") if q and shown else None
    p5, p7 = (round(p5) if p5 is not None else None), (round(p7) if p7 is not None else None)
    dark = taskbar_dark()
    phases = [i / FRAMES for i in range(FRAMES)] if kind else [0.0]
    return {"style": style, "kind": kind, "ms": FRAME_MS,
            "frames": [icon_file(style, light[0], kind, ph, p5, p7, dark, px) for ph in phases]}


class Notes:
    """收集要弹的通知，最后放进输出的 JSON，由托盘弹出（Windows 上 cam.notify 不弹）。"""

    def __init__(self):
        self.items = []

    def __call__(self, title, body):
        self.items.append({"title": str(title), "body": str(body or title)})


def menu(store, notes, px=16, full=True):
    """full=False 是 light：不做后台任务（自动保存新登录、刷新用量这些每分钟一轮就够），不带菜单，只给图标和提示文字。"""
    cam_menu.notify = notes
    if full:
        try:
            st = cam_menu.background_work(store)
        except Exception as e:  # noqa: BLE001  和菜单栏小工具一样，后台出错不影响菜单
            st = {"last_error": str(e)}
    else:
        st = cam_menu.load_state(store)
    lines = []
    cam_menu.render(store, st, out=lines.append, rich=False)  # 托盘菜单不认图片，用文字进度条（不管在哪个系统上跑）
    title, items = convert(lines)
    badge, level, tip = badge_of(title)
    light = cam_menu.LAST_LIGHT
    if light:  # 提示文字第二行是红绿灯说的那句（工作中、等你确认、空闲……）
        tip = f"{tip}\n{light[1]}"
    data = store.load()
    out = {"tip": tip, "badge": badge, "level": level, "icon": icon_of(data, light, badge, level, px),
           # 开着会话、或者在动画（等你确认、工作中）：托盘每 20 秒看一次，不然每分钟跟着菜单看
           "watch": bool(light and (light[2] or cam.session_count(max_age=60))), "notes": notes.items}
    if full:
        add_style_menu(items, tray_style(data))
        out.update(items=items, mini=mini_info(store))
    return out


def add_style_menu(items, style):
    """“更多”里加“托盘图标样式”：打勾的是现在的，点另一行就换，换完马上刷新。"""
    more = next((i for i in items if i.get("text") == "更多" and "children" in i), None)
    if more is None:
        more = {"text": "更多", "children": []}
        items.append(more)
    kids = more["children"]
    if kids:
        kids.append({"sep": True})
    kids.append({"text": "托盘图标样式", "dim": True})
    for key, label in TRAY_STYLES:
        kids.append({"text": label, "checked": True} if key == style else {"text": label, "run": ["icon", key], "refresh": True})


def set_style(store, key, notes):
    if key not in dict(TRAY_STYLES):
        notes("没有完成", f"不认识的图标样式：{key}")
        return 2
    with store.lock():
        data = store.load()
        data.setdefault("prefs", {})["menu_icon"] = key
        store.save(data)
    return 0


def mini_info(store):
    """卡片小面板：左键点托盘图标时 cam-tray.ps1 用浏览器的应用模式开一个小窗显示它。没有（桌面版的服务、起不来）就 None，左键退回弹菜单。"""
    ep = cam_menu.mini_endpoint(store)
    if not ep:
        return None
    w, h = cam_menu.mini_size(store.load().get("accounts") or [])
    return {"url": f"http://127.0.0.1:{ep[0]}/mini?t={ep[1]}", "w": w, "h": h}


def last_line(*texts):
    for t in texts:
        rows = [r.strip() for r in (t or "").splitlines() if r.strip()]
        if rows:
            return rows[-1][:240]
    return ""


def run_cam(args, notes):
    """跑一个 cam 命令（刷新用量、打开网页账号、打开面板），出错时用通知告诉用户。"""
    try:
        r = subprocess.run([sys.executable, cam_menu.CAM_PY, *args], capture_output=True, text=True, encoding="utf-8",
                           errors="replace", timeout=600, stdin=subprocess.DEVNULL)
    except (OSError, subprocess.SubprocessError) as e:
        notes("没有完成", str(e) or type(e).__name__)
        return 1
    if r.returncode:
        notes("没有完成", last_line(r.stderr, r.stdout) or f"cam {' '.join(args)} 出错了（退出码 {r.returncode}）")
    return r.returncode


def open_terminal(args):
    """新开一个终端窗口运行 cam，看完不自动关。

    cmd /k 后面整串再包一层引号：cmd 会去掉最外面那一对，里面带空格的路径才保得住。
    """
    line = " ".join(subprocess.list2cmdline([a]) for a in (sys.executable, cam_menu.CAM_PY, *args))
    subprocess.Popen(f'cmd.exe /k "{line}"', creationflags=CREATE_NEW_CONSOLE, close_fds=True)


def do(store, kind, args, notes):
    cam_menu.notify = notes
    if kind == "menu":
        fn = {"switch": lambda: cam_menu.switch_from_menu(store, args[1]),
              "update-check": cam_menu.check_update_from_menu,
              "update": cam_menu.update_from_menu,
              "checkup": cam_menu.checkup_from_menu}.get(args[0] if args else "")
        if fn is None or (args[0] == "switch" and len(args) < 2):
            notes("没有完成", f"不认识的操作：{' '.join(args)}")
            return 2
        return fn()
    if kind == "cam":
        return run_cam(args, notes)
    if kind == "icon":
        return set_style(store, args[0] if args else "", notes)
    if kind == "terminal":
        try:
            open_terminal(args)
        except OSError as e:
            notes("打不开终端", str(e))
            return 1
        return 0
    notes("没有完成", f"不认识的操作：{kind}")
    return 2


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    # Windows 上往管道输出默认用本地代码页，菜单里的 ✓ ✗ 这类字符会写不出来；cam-tray.ps1 按 UTF-8 读
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    notes = Notes()
    code = 0
    try:
        # cam 里有些步骤会往 stdout 打印说明，托盘只认最后那一行 JSON，这里先收起来
        with contextlib.redirect_stdout(io.StringIO()):
            store = cam.Store()
            if argv[:1] == ["do"] and len(argv) > 1:
                code = do(store, argv[1], argv[2:], notes)
                out = {"notes": notes.items}
            else:
                px = int(argv[1]) if argv[1:2] and argv[1].isdigit() else 16
                out = menu(store, notes, min(max(px, 12), 64), full=argv[:1] != ["light"])
    except Exception as e:  # noqa: BLE001  什么错都要让托盘显示出来
        notes("托盘小工具出错了", str(e) or type(e).__name__)
        out = {"tip": "Claude 账号：出错了", "badge": "!", "level": "red", "notes": notes.items,
               "items": [{"text": f"出错了：{str(e)[:120]}", "color": light(cam_menu.RED)}]}
        code = 1
    sys.stdout.write(json.dumps(out, ensure_ascii=False) + "\n")
    return code


if __name__ == "__main__":
    sys.exit(main())
