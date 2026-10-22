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

由 install.sh 生成的启动脚本调用，不需要手动运行。
"""
import datetime as dt
import json
import os
import sys

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "scripts"))
import cam  # noqa: E402

CAM_PY = os.path.realpath(os.path.join(HERE, "..", "..", "scripts", "cam.py"))
MENU_PY = os.path.realpath(__file__)
PY = sys.executable
REFRESH_MIN = float(os.environ.get("CAM_MENU_REFRESH_MIN", "10"))
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


def heading(text, icon):
    """分区标题：灰色小字加图标。"""
    return f"{text} | size=12 color=gray sfimage={icon} sfcolor={GRAY}"


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


def health_of(store, a):
    """账号健康状态（正常 / 注意 / 异常），规则见 cam.account_health。"""
    has = a["kind"] == "claude_code" and bool((store.load_secret(a["id"]).get("claudeAiOauth") or {}).get("refreshToken"))
    return cam.account_health(a, has, cam.days_left(a))


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
        # 图标颜色跟着 5 小时额度走：<70% 绿、70–89% 黄、≥90% 红；文字只在快满时变色
        out(f"{esc(short(title_acc.get('label') or title_acc.get('email')))} {p:.0f}% | "
            f"sfimage=person.2.circle sfcolor={color_for(p) or GREEN}" + (f" color={color_for(p)}" if color_for(p) else ""))
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
        out(heading("Claude Code 账号", "terminal"))
        code.sort(key=lambda a: (a["id"] != act_id, pct(a, "five_hour") if pct(a, "five_hour") is not None else 999))
        for a in code:
            plan = PLAN.get(a.get("plan") or (a.get("manual") or {}).get("plan") or "", "")
            plan += f" {cam.plan_tier(a)}" if plan and cam.plan_tier(a) else ""
            h = health_of(store, a)
            flag = f"  ⚠ {h['label']}" if h["level"] in ("warn", "error") else ""

            def text(name):
                return f"{esc(name)}{('  ' + plan) if plan else ''}{flag}{usage_nums(a)}"

            params = row_icon(a["id"] == act_id, h["level"])
            if a["id"] != act_id:
                c = RED if h["level"] == "error" else color_for(pct(a, "five_hour"))
                params += f" {switch_action(a['id'])}" + (f" color={c}" if c else "")
            out(f"{text(a.get('label') or a.get('email') or '未命名')} | {params}")
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
            dl = cam.days_left(a)
            if dl is not None and dl >= 0:
                exp = (a.get("manual") or {}).get("expires_at")
                out(f"--{dl} 天后到期（{exp}）" + (" | color=#BF3B33,#EE776E" if dl <= 2 else ""))
            if a.get("email") and a.get("email") != a.get("label"):
                out(f"--{esc(a['email'])} | color=gray")
            if a["id"] != act_id:
                out(f"--切换到这个账号 | {switch_action(a['id'])}")
            out(f"--只刷新这个账号 | {action('usage', a['id'])}")
            if a.get("email") and a.get("email") != a.get("label"):
                # 按住 Option 时这一行换成邮箱；要紧跟在上一行（及其子菜单）后面才会配成一对
                out(f"{text(a['email'])} | {params} alternate=true")
        out("---")

    # ---- 网页版
    web = [a for a in accs if a["kind"] == "claude_web"]
    if web:
        out(heading("网页版", "globe"))
        web.sort(key=lambda a: pct(a, "five_hour") if pct(a, "five_hour") is not None else 999)
        for a in web:
            f5 = pct(a, "five_hour")
            plan = PLAN.get(a.get("plan") or (a.get("manual") or {}).get("plan") or "", "")
            plan += f" {cam.plan_tier(a)}" if plan and cam.plan_tier(a) else ""
            u = a.get("usage") or {}

            def text(name):
                return f"{esc(name)}{('  ' + plan) if plan else ''}{usage_nums(a)}"

            params = (row_icon(False, "warn" if u.get("error") else "ok") + f" {action('open', a['id'], refresh=False)}"
                      + (f" color={color_for(f5)}" if color_for(f5) else ""))
            out(f"{text(a.get('label') or a.get('email') or '未命名')} | {params}")
            # 子菜单
            out(f"--打开浏览器窗口 | {action('open', a['id'], refresh=False)}")
            for k, nm in (("five_hour", "5 小时"), ("seven_day", "7 天")):
                w = u.get(k)
                if w:
                    out(f"--{nm}：已用 {w['utilization']:.0f}%，{cam.fmt_reset(w.get('resets_at'))}")
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
            if a.get("email") and a.get("email") != a.get("label"):
                out(f"{text(a['email'])} | {params} alternate=true")  # 按住 Option 显示邮箱
        out("---")

    # ---- Claude 桌面版
    desk = [a for a in accs if a["kind"] == "claude_desktop"]
    if desk:
        out(heading("Claude 桌面版", "desktopcomputer"))
        act = (data.get("active") or {}).get("claude_desktop")
        for a in desk:
            name = esc(a.get("label") or a.get("email") or "未命名")
            mark = " · 上次打开" if a["id"] == act else ""
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
        out(heading("API", "key"))
        for a in api:
            u = a.get("usage") or {}
            cost = f"    本月 ${u['cost_month_usd']:.2f}" if u.get("cost_month_usd") is not None else ""
            out(f"{esc(a['label'])}{cost}")
        out("---")

    # ---- 操作
    out(f"打开管理面板 | {action('panel', refresh=False)} sfimage=macwindow")  # 改名、规则、隐私设置都在面板里
    out(f"刷新全部用量 | {action('usage')} sfimage=arrow.clockwise")
    out(f"打开终端查看 | {action(terminal=True, refresh=False)} sfimage=terminal")
    if st.get("bg_owner") == "other":
        out("自动刷新由桌面版或 cam serve 负责 | size=12 color=gray")
    elif st.get("last_refresh"):
        t = dt.datetime.fromtimestamp(float(st["last_refresh"])).strftime("%H:%M")
        out(f"用量更新于 {t} | size=12 color=gray")
    notice = (data.get("live_mismatch") or data.get("live_unverified") or {}).get("message")
    if notice:
        out(f"⚠ {esc(notice)[:90]} | size=12 color=#B87A08,#E0A93B")
    pend = cam.pending_switch_view(data)
    if pend:
        out(f"⏳ {esc(pend['message'])[:90]} | size=12 color=#5A6676,#94A0AF")
    risks = cam.risk_items(data)
    if cam.region_view(data)["status"]:  # 后台检测过才有评分
        sc = cam.risk_score(risks)
        c = {"low": GREEN, "medium": YELLOW, "high": RED}[sc["grade"]]
        icon = "checkmark.shield.fill" if sc["grade"] == "low" else "exclamationmark.shield.fill"
        out(f"环境评分 {sc['score']}/100：{esc(sc['label'])[:60]} | size=12 color={c} sfimage={icon} sfcolor={c}")
    for r in risks:  # 出口不支持 / 出口变化 / 时区 / 时钟 / 官方状态页 / 账号多地区：只提醒，不拦
        if r["level"] not in ("warn", "bad"):
            continue
        out(f"⚠ {esc(r['text'])[:90]} | size=12 color=#BF3B33,#E06C66")
        if r["advice"]:
            out(f"   {esc(r['advice'])[:90]} | size=11 color=#5A6676,#94A0AF")
    if st.get("last_error"):
        out(f"⚠ {esc(st['last_error'])[:90]} | size=12 color=#B87A08,#E0A93B")


def main():
    store = cam.Store()
    if sys.argv[1:2] == ["switch"] and len(sys.argv) > 2:  # 菜单里点了切换，见 switch_action
        sys.exit(switch_from_menu(store, sys.argv[2]))
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
