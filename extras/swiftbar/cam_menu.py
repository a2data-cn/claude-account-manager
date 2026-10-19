"""SwiftBar / xbar 菜单栏插件的主体。

菜单栏常驻显示当前 Claude Code 账号的 5 小时额度，例如“工作号 64%”。
下拉菜单可以看到所有账号的额度，点一下就能切换，也能打开网页账号或刷新用量。

插件每次运行（默认每分钟一次）还会顺带完成以下工作，
这样不开桌面版也能拿到“全自动”的效果：
  - 发现 Claude Code 新登录的账号，自动保存
  - 执行时间规则（进入时间段时切号）
  - 每 10 分钟刷新一次用量（环境变量 CAM_MENU_REFRESH_MIN 可改）
  - 当前账号额度超过提醒阈值时弹一次系统通知

由 install.sh 生成的启动脚本调用，不需要手动运行。
"""
import datetime as dt
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "scripts"))
import cam  # noqa: E402

CAM_PY = os.path.realpath(os.path.join(HERE, "..", "..", "scripts", "cam.py"))
PY = sys.executable
REFRESH_MIN = float(os.environ.get("CAM_MENU_REFRESH_MIN", "10"))
PLAN = {"pro": "Pro", "max": "Max", "team": "Team", "enterprise": "Enterprise", "free": "Free", "api": "API"}


def esc(s):
    """SwiftBar 用 | 分隔参数，标题里的 | 要替换掉。"""
    return str(s or "").replace("|", "｜").replace("\n", " ")


def q(s):
    return '"' + str(s).replace("\\", "\\\\").replace('"', '\\"') + '"'


def action(*args, refresh=True, terminal=False):
    """生成点击后执行 cam 命令的参数串。"""
    parts = [f"bash={q(PY)}", f"param1={q(CAM_PY)}"]
    for i, a in enumerate(args, start=2):
        parts.append(f"param{i}={q(a)}")
    parts.append(f"terminal={'true' if terminal else 'false'}")
    if refresh:
        parts.append("refresh=true")
    return " ".join(parts)


def pct(a, key):
    w = (a.get("usage") or {}).get(key)
    return w["utilization"] if w and w.get("utilization") is not None else None


def color_for(p):
    if p is None:
        return None
    return "#BF3B33,#EE776E" if p >= 90 else "#B87A08,#E0A93B" if p >= 70 else None


def short(label, n=8):
    label = label or ""
    return label if len(label) <= n else label[: n - 1] + "…"


def state_path(store):
    return os.path.join(store.root, ".menubar-state.json")


def load_state(store):
    try:
        with open(state_path(store)) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def save_state(store, st):
    try:
        cam.write_json_atomic(state_path(store), st)
    except OSError:
        pass


def notify(title, body):
    if sys.platform != "darwin" or os.environ.get("CAM_MENU_NO_NOTIFY"):
        return
    script = f"display notification {json.dumps(body)} with title {json.dumps(title)}"
    try:
        subprocess.run(["osascript", "-e", script], timeout=5, capture_output=True)
    except (OSError, subprocess.TimeoutExpired):
        pass


def background_work(store):
    """自动导入、时间规则、定时刷新、额度提醒。任何一步失败都不影响菜单显示。

    桌面版或 cam serve 在运行时由它们负责后台任务，这里只显示菜单，避免几个进程同时轮询。
    """
    lease = store.background_lock()
    if not lease.try_acquire():
        st = load_state(store)
        st["bg_owner"] = "other"
        return st
    try:
        st = _background_work(store)
        st.pop("bg_owner", None)
        return st
    finally:
        lease.release()


def _background_work(store):
    st = load_state(store)
    try:
        acc = cam.auto_import(store)
        if acc:
            notify("发现新账号", f"已自动保存：{acc['label']}")
            st["last_refresh"] = 0
    except Exception as e:  # noqa: BLE001
        st["last_error"] = f"自动导入：{e}"
    try:
        res = cam.apply_time_rules(store)
        if res and res[2]:
            warn = cam.running_warning(cam.running_claude_processes()) or "正在运行的会话需要重启。"
            notify("已按规则切换账号", f"Claude Code 已切换到 {res[0]['label']}。{warn}")
    except Exception as e:  # noqa: BLE001
        st["last_error"] = f"时间规则：{e}"
    now = dt.datetime.now().timestamp()
    if REFRESH_MIN > 0 and now - float(st.get("last_refresh") or 0) >= REFRESH_MIN * 60:
        st["last_refresh"] = now
        try:
            # 定时刷新不替不常用的账号续登录，见 cam.refresh_usage
            data = cam.refresh_usage(store, allow_token_refresh=False)
            st.pop("last_error", None)
            limit = cam.rules_view(data)["alert_threshold"]
            act = data["active"].get("claude_code")
            warned = set(st.get("warned") or [])
            # 当前 Claude Code 账号 + 所有有额度的网页账号，超过阈值都提醒一次
            for a in data["accounts"]:
                is_active_code = a["id"] == act
                is_web = a["kind"] == "claude_web"
                if not is_active_code and not is_web:
                    continue
                w = (a.get("usage") or {}).get("five_hour") or {}
                if w.get("utilization") is None:
                    continue
                key = f"{a['id']}|{w.get('resets_at')}"
                if w["utilization"] >= limit and key not in warned:
                    warned.add(key)
                    notify(f"{a['label']} 的 5 小时额度已用 {w['utilization']:.0f}%", cam.fmt_reset(w.get("resets_at")))
            st["warned"] = sorted(warned)[-50:]
        except Exception as e:  # noqa: BLE001
            st["last_error"] = f"刷新用量：{e}"
    save_state(store, st)
    return st


def render(store, st, out=print):
    data = store.load()
    accs = data["accounts"]
    act_id = data["active"].get("claude_code")
    active = next((a for a in accs if a["id"] == act_id), None)

    # 标题优先显示当前 Claude Code 账号；没有则退回到有额度的网页账号
    title_acc = active
    if title_acc is None or pct(title_acc, "five_hour") is None:
        web_with_usage = sorted(
            (a for a in accs if a["kind"] == "claude_web" and pct(a, "five_hour") is not None),
            key=lambda a: pct(a, "five_hour"))
        if web_with_usage:
            title_acc = web_with_usage[0]

    # ---- 菜单栏标题
    if not accs:
        out("Claude ⚠ | sfimage=person.2.circle")
    elif title_acc is not None and pct(title_acc, "five_hour") is not None:
        p = pct(title_acc, "five_hour")
        out(f"{esc(short(title_acc.get('label') or title_acc.get('email')))} {p:.0f}% | "
            f"sfimage=person.2.circle" + (f" color={color_for(p)}" if color_for(p) else ""))
    elif active:
        out(f"{esc(short(active.get('label') or active.get('email')))} | sfimage=person.2.circle")
    else:
        out("Claude | sfimage=person.2.circle")
    out("---")

    if not accs:
        out("还没有账号 | color=gray")
        out("用 Claude Code：运行 claude 登录，回来自动保存 | size=12 color=gray")
        out("用网页版：终端运行 cam add-web --label 名字 | size=12 color=gray")
        out("---")

    # ---- Claude Code 账号
    code = [a for a in accs if a["kind"] == "claude_code"]
    if code:
        out("Claude Code 账号 | size=12 color=gray")
        code.sort(key=lambda a: (a["id"] != act_id, pct(a, "five_hour") if pct(a, "five_hour") is not None else 999))
        for a in code:
            f5, f7 = pct(a, "five_hour"), pct(a, "seven_day")
            nums = []
            if f5 is not None:
                nums.append(f"5h {f5:.0f}%")
            if f7 is not None:
                nums.append(f"7d {f7:.0f}%")
            plan = PLAN.get(a.get("plan") or (a.get("manual") or {}).get("plan") or "", "")
            name = esc(a.get("label") or a.get("email") or "未命名")
            text = f"{name}{('  ' + plan) if plan else ''}{('    ' + ' · '.join(nums)) if nums else ''}"
            if a["id"] == act_id:
                out(f"{text} | checked=true")
            else:
                c = color_for(f5)
                out(f"{text} | {action('switch', a['id'])}" + (f" color={c}" if c else ""))
            # 子菜单：详细信息
            u = a.get("usage") or {}
            for k, nm in (("five_hour", "5 小时"), ("seven_day", "7 天"), ("seven_day_opus", "7 天 Opus"),
                          ("seven_day_sonnet", "7 天 Sonnet")):
                w = u.get(k)
                if w:
                    out(f"--{nm}：已用 {w['utilization']:.0f}%，{cam.fmt_reset(w.get('resets_at'))}")
            dl = cam.days_left(a)
            if dl is not None:
                exp = (a.get("manual") or {}).get("expires_at")
                txt = f"{dl} 天后到期（{exp}）" if dl >= 0 else f"已过期 {-dl} 天"
                out(f"--{txt}" + (" | color=#BF3B33,#EE776E" if dl <= 2 else ""))
            if u.get("error"):
                out(f"--⚠ {esc(u['error'])[:80]} | color=#B87A08,#E0A93B")
            if a.get("email") and a.get("email") != a.get("label"):
                out(f"--{esc(a['email'])} | color=gray")
            if a["id"] != act_id:
                out(f"--切换到这个账号 | {action('switch', a['id'])}")
            out(f"--只刷新这个账号 | {action('usage', a['id'])}")
        out("---")

    # ---- 网页版
    web = [a for a in accs if a["kind"] == "claude_web"]
    if web:
        out("网页版 | size=12 color=gray")
        web.sort(key=lambda a: pct(a, "five_hour") if pct(a, "five_hour") is not None else 999)
        for a in web:
            f5, f7 = pct(a, "five_hour"), pct(a, "seven_day")
            nums = []
            if f5 is not None:
                nums.append(f"5h {f5:.0f}%")
            if f7 is not None:
                nums.append(f"7d {f7:.0f}%")
            plan = PLAN.get(a.get("plan") or (a.get("manual") or {}).get("plan") or "", "")
            name = esc(a.get("label") or a.get("email") or "未命名")
            text = f"{name}{('  ' + plan) if plan else ''}{('    ' + ' · '.join(nums)) if nums else ''}"
            out(f"{text} | {action('open', a['id'], refresh=False)}"
                + (f" color={color_for(f5)}" if color_for(f5) else ""))
            # 子菜单
            out(f"--打开浏览器窗口 | {action('open', a['id'], refresh=False)}")
            u = a.get("usage") or {}
            for k, nm in (("five_hour", "5 小时"), ("seven_day", "7 天")):
                w = u.get(k)
                if w:
                    out(f"--{nm}：已用 {w['utilization']:.0f}%，{cam.fmt_reset(w.get('resets_at'))}")
            dl = cam.days_left(a)
            if dl is not None:
                exp = (a.get("manual") or {}).get("expires_at")
                out(f"--{dl} 天后到期（{exp}）" if dl >= 0 else f"--已过期 {-dl} 天"
                    + ("" if dl > 2 else " | color=#BF3B33,#EE776E"))
            if u.get("error"):
                out(f"--⚠ {esc(u['error'])[:90]} | color=#B87A08,#E0A93B")
            elif not u:
                out("--填入 sessionKey 可显示额度（见 README） | color=gray")
            out(f"--只刷新这个账号 | {action('usage', a['id'])}")
        out("---")

    # ---- API
    api = [a for a in accs if a["kind"] == "api"]
    if api:
        out("API | size=12 color=gray")
        for a in api:
            u = a.get("usage") or {}
            cost = f"    本月 ${u['cost_month_usd']:.2f}" if u.get("cost_month_usd") is not None else ""
            out(f"{esc(a['label'])}{cost}")
        out("---")

    # ---- 操作
    out(f"刷新全部用量 | {action('usage')}")
    out(f"打开终端查看 | {action(terminal=True, refresh=False)}")
    if st.get("bg_owner") == "other":
        out("自动刷新由桌面版或 cam serve 负责 | size=12 color=gray")
    elif st.get("last_refresh"):
        t = dt.datetime.fromtimestamp(float(st["last_refresh"])).strftime("%H:%M")
        out(f"用量更新于 {t} | size=12 color=gray")
    if (data.get("live_mismatch") or {}).get("message"):
        out(f"⚠ {esc(data['live_mismatch']['message'])[:90]} | size=12 color=#B87A08,#E0A93B")
    if st.get("last_error"):
        out(f"⚠ {esc(st['last_error'])[:90]} | size=12 color=#B87A08,#E0A93B")


def main():
    store = cam.Store()
    st = {}
    try:
        st = background_work(store)
    except Exception as e:  # noqa: BLE001
        st = {"last_error": str(e)}
    try:
        render(store, st)
    except Exception as e:  # noqa: BLE001
        print("Claude ⚠")
        print("---")
        print(f"出错了：{esc(e)} | color=red")


if __name__ == "__main__":
    main()
