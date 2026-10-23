"""Windows 托盘小工具的 Python 部分。cam-tray.ps1 负责画图标和菜单，内容都由这里算。

菜单和 macOS 菜单栏小工具（extras/swiftbar/cam_menu.py）是同一份：这里调用它的 render，
把 SwiftBar 格式的菜单行转成 JSON 交给 cam-tray.ps1，两边显示的内容、点了做什么都一样。
每次算菜单前顺带跑一轮后台任务（自动保存新登录、时间规则、等待中的切换、刷新用量、额度提醒），
和菜单栏小工具共用后台锁：桌面版或 cam serve 在跑时由它们做。

用法（都由 cam-tray.ps1 调用，输出一行 JSON）：
  cam_tray.py menu               托盘图标、提示文字和菜单：{"tip", "badge", "level", "items", "notes"}
  cam_tray.py do <种类> 参数…    菜单里点了某一项：{"notes": [{"title", "body"}]}，通知由托盘弹出
    menu switch|update-check|update|checkup …  菜单栏小工具自己的命令
    cam  usage|open|panel …                     cam 命令，出错时把报错写进通知
    terminal [cam 参数…]                        新开一个终端窗口运行 cam
"""
import contextlib
import io
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "swiftbar"))
import cam_menu  # noqa: E402

cam = cam_menu.cam
CREATE_NEW_CONSOLE = 0x00000010

# Windows 菜单的字体（微软雅黑）里不一定有这几个符号，换成一定显示得出来的
CHARS = str.maketrans({"▰": "■", "▱": "□", "⚠": "！", "⏳": "…"})
# 菜单栏小工具的三种颜色（浅色模式那一个）对应托盘图标的底色
LEVELS = {cam_menu.GREEN: "green", cam_menu.YELLOW: "yellow", cam_menu.RED: "red"}

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


class Notes:
    """收集要弹的通知，最后放进输出的 JSON，由托盘弹出（Windows 上 cam.notify 不弹）。"""

    def __init__(self):
        self.items = []

    def __call__(self, title, body):
        self.items.append({"title": str(title), "body": str(body or title)})


def menu(store, notes):
    cam_menu.notify = notes
    try:
        st = cam_menu.background_work(store)
    except Exception as e:  # noqa: BLE001  和菜单栏小工具一样，后台出错不影响菜单
        st = {"last_error": str(e)}
    lines = []
    cam_menu.render(store, st, out=lines.append)
    title, items = convert(lines)
    badge, level, tip = badge_of(title)
    return {"tip": tip, "badge": badge, "level": level, "items": items, "notes": notes.items}


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
                out = menu(store, notes)
    except Exception as e:  # noqa: BLE001  什么错都要让托盘显示出来
        notes("托盘小工具出错了", str(e) or type(e).__name__)
        out = {"tip": "Claude 账号：出错了", "badge": "!", "level": "red", "notes": notes.items,
               "items": [{"text": f"出错了：{str(e)[:120]}", "color": light(cam_menu.RED)}]}
        code = 1
    sys.stdout.write(json.dumps(out, ensure_ascii=False) + "\n")
    return code


if __name__ == "__main__":
    sys.exit(main())
