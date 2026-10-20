#!/usr/bin/env python3
"""cam — Claude 多账号管理（脚本版）

只依赖 Python 3.8+ 标准库，macOS / Windows / Linux 通用。
与桌面版共享数据目录 ~/.claude-accounts（可用 CAM_HOME 覆盖）。

常用命令：
  cam import [--label 名字]        把当前 Claude Code 登录的账号存进来
  cam ls                           列出所有账号和用量
  cam switch <账号>                切换 Claude Code 账号（id / 序号 / 名字 / 邮箱）
  cam usage                        刷新并显示用量
  cam set <账号> --expires 2026-11-03 --auto-renew
  cam add-web --label 名字         添加网页版账号（独立浏览器 Profile）
  cam open <账号>                  打开网页版账号窗口
  cam add-api --label 名字 --key sk-ant-... [--admin-key sk-ant-admin...]
  cam serve [--lan] [--port 8787]  启动 Web 面板（手机也能看）
  cam local [--days 30]            本地 Claude Code token 消耗统计
  cam env install                  用官方安装器安装 Claude Code
  cam env clean [--logout|--purge] 清理 Claude Code 环境（默认只预演，加 --apply 执行）
"""
from __future__ import annotations

import argparse
import base64
import calendar
import datetime as dt
import getpass
import glob
import hashlib
import http.server
import json
import os
import platform
import secrets as pysecrets
import shutil
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

if os.name == "nt":
    import msvcrt
else:
    import fcntl

VERSION = "1.1.0"

# --------------------------------------------------------------------------- 路径

IS_MAC = sys.platform == "darwin"
IS_WIN = os.name == "nt"

KEYCHAIN_SERVICE = "Claude Code-credentials"
OAUTH_CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
API_BASE = os.environ.get("CAM_API_BASE", "https://api.anthropic.com")
TOKEN_URLS = [u for u in os.environ.get(
    "CAM_TOKEN_URLS",
    "https://platform.claude.com/v1/oauth/token,https://console.anthropic.com/v1/oauth/token",
).split(",") if u]
CLAUDE_WEB_BASE = os.environ.get("CAM_CLAUDE_WEB_BASE", "https://claude.ai")
USER_AGENT = f"claude-account-manager/{VERSION}"
BACKUP_KEEP = 30


def home() -> str:
    return os.path.expanduser("~")


def cam_home() -> str:
    return os.environ.get("CAM_HOME") or os.path.join(home(), ".claude-accounts")


def claude_config_dir() -> str:
    return os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(home(), ".claude")


def claude_credentials_file() -> str:
    return os.path.join(claude_config_dir(), ".credentials.json")


def claude_json_file() -> str:
    # 自定义 CLAUDE_CONFIG_DIR 时 .claude.json 在该目录内，否则在用户主目录
    if os.environ.get("CLAUDE_CONFIG_DIR"):
        return os.path.join(os.environ["CLAUDE_CONFIG_DIR"], ".claude.json")
    return os.path.join(home(), ".claude.json")


def use_keychain() -> bool:
    if os.environ.get("CAM_NO_KEYCHAIN") == "1":
        return False
    return IS_MAC and not os.environ.get("CLAUDE_CONFIG_DIR")


# --------------------------------------------------------------------------- 小工具

def now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def parse_iso(s):
    if not s or not isinstance(s, str):
        return None
    try:
        s = s.replace("Z", "+00:00")
        d = dt.datetime.fromisoformat(s)
        if d.tzinfo is None:
            d = d.replace(tzinfo=dt.timezone.utc)
        return d
    except ValueError:
        try:
            d = dt.datetime.strptime(s[:10], "%Y-%m-%d")
            return d.replace(tzinfo=dt.timezone.utc)
        except ValueError:
            return None


def parse_day(s):
    """严格的 YYYY-MM-DD（后面可以跟 T 或空格开头的时间），返回 date；其它写法返回 None。

    到期日两边都按这个规则认，避免一边能存、另一边读不出来。
    """
    if not isinstance(s, str) or len(s) < 10 or (len(s) > 10 and s[10] not in "T "):
        return None
    head = s[:10]
    # 只认 ASCII 数字：str.isdigit 也认全角数字，Rust 版不认，存下去 Rust 版就读不出来了
    if not (head.isascii() and head[:4].isdigit() and head[4] == "-" and head[5:7].isdigit() and head[7] == "-" and head[8:].isdigit()):
        return None
    try:
        return dt.date(int(head[:4]), int(head[5:7]), int(head[8:]))
    except ValueError:
        return None


def read_json(path, default=None):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return default
    except json.JSONDecodeError as e:
        raise CamError(f"文件损坏，无法解析：{path}（{e}）")


def read_bytes(path):
    try:
        with open(path, "rb") as f:
            return f.read()
    except FileNotFoundError:
        return None


def write_json_atomic(path, data, private=False):
    # 目标是符号链接（比如 dotfiles 管理的 ~/.claude.json）时写到链接指向的文件，保留链接本身
    path = os.path.realpath(path)
    d = os.path.dirname(path)
    os.makedirs(d, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".tmp-", dir=d)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=2)
            f.flush()
            os.fsync(f.fileno())
        if private and not IS_WIN:
            os.chmod(tmp, 0o600)
        elif not IS_WIN and os.path.exists(path):
            os.chmod(tmp, os.stat(path).st_mode & 0o777)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


class CamError(Exception):
    pass


class FileLock:
    """跨进程锁：Unix 用 flock，Windows 用 LockFileEx（Rust 版 File::lock 用的是同一种锁）。

    锁由操作系统持有，进程退出或崩溃时自动释放，不会留下残留锁，也不会被别人误删。
    每次加锁都单独打开文件，所以同一进程里的不同线程之间也互斥。
    """

    def __init__(self, path, timeout=30.0):
        self.path = path
        self.timeout = timeout
        self.fd = None

    def try_acquire(self):
        """不等待，拿到锁返回 True。"""
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT, 0o600)
        try:
            if IS_WIN:
                msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
            else:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(fd)
            return False
        self.fd = fd
        return True

    def release(self):
        if self.fd is None:
            return
        if IS_WIN:
            try:
                os.lseek(self.fd, 0, os.SEEK_SET)
                msvcrt.locking(self.fd, msvcrt.LK_UNLCK, 1)
            except OSError:
                pass
        os.close(self.fd)  # Unix 上关闭文件即释放 flock
        self.fd = None

    def __enter__(self):
        deadline = time.monotonic() + self.timeout
        while not self.try_acquire():
            if time.monotonic() > deadline:
                raise CamError(f"数据目录被占用（{self.path}），请稍后重试")
            time.sleep(0.05)
        return self

    def __exit__(self, *a):
        self.release()


# --------------------------------------------------------------------------- 存储

class Store:
    def __init__(self, root=None):
        self.root = root or cam_home()
        self.accounts_path = os.path.join(self.root, "accounts.json")
        self.secrets_dir = os.path.join(self.root, "secrets")
        self.backups_dir = os.path.join(self.root, "backups")
        self.profiles_dir = os.path.join(self.root, "browser-profiles")
        os.makedirs(self.root, exist_ok=True)
        if not IS_WIN:
            try:
                os.chmod(self.root, 0o700)
            except OSError:
                pass

    def lock(self):
        # 旧版本用的是 .lock（独占创建文件），换成新文件名，避免旧版把新锁当残留删掉
        return FileLock(os.path.join(self.root, "accounts.lock"))

    def background_lock(self):
        """后台任务（自动导入、时间规则、定时刷新）只让一个进程做：桌面版、cam serve、菜单栏插件谁先拿到谁做。"""
        return FileLock(os.path.join(self.root, "background.lock"), timeout=0)

    def load(self):
        data = read_json(self.accounts_path, None) or {}
        data.setdefault("version", 1)
        data.setdefault("active", {})
        data.setdefault("accounts", [])
        data.setdefault("switch_log", [])
        data.setdefault("rules", [])
        for a in data["accounts"]:
            a.setdefault("manual", {})
            a.setdefault("usage", None)
        return data

    def save(self, data):
        data["switch_log"] = data.get("switch_log", [])[-2000:]
        write_json_atomic(self.accounts_path, data)

    def secret_path(self, acc_id):
        return os.path.join(self.secrets_dir, f"{acc_id}.json")

    def load_secret(self, acc_id):
        return read_json(self.secret_path(acc_id), {}) or {}

    def save_secret(self, acc_id, secret):
        os.makedirs(self.secrets_dir, exist_ok=True)
        if not IS_WIN:
            try:
                os.chmod(self.secrets_dir, 0o700)
            except OSError:
                pass
        write_json_atomic(self.secret_path(acc_id), secret, private=True)

    def delete_secret(self, acc_id):
        try:
            os.remove(self.secret_path(acc_id))
        except FileNotFoundError:
            pass

    def new_backup_dir(self, tag):
        stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
        d = os.path.join(self.backups_dir, f"{stamp}-{tag}")
        i = 1
        while os.path.exists(d):
            d = os.path.join(self.backups_dir, f"{stamp}-{tag}-{i}")
            i += 1
        os.makedirs(d)
        if not IS_WIN:
            os.chmod(d, 0o700)
        return d

    def prune_backups(self):
        """只清理切换前的现场备份（<时间戳>-before-<id>），保留最近 BACKUP_KEEP 份。

        删除账号、退出登录留下的备份里有唯一一份凭据，不自动清理。
        """
        try:
            items = sorted(n for n in os.listdir(self.backups_dir) if n[16:].startswith("before-"))
        except FileNotFoundError:
            return
        for name in items[:-BACKUP_KEEP]:
            shutil.rmtree(os.path.join(self.backups_dir, name), ignore_errors=True)


def new_id():
    return pysecrets.token_hex(4)


def find_account(data, key):
    accs = data["accounts"]
    if key is None:
        raise CamError("需要指定账号")
    key = str(key).strip()
    if key.isdigit() and 1 <= int(key) <= len(accs) and not any(a["id"] == key for a in accs):
        return accs[int(key) - 1]
    for field in ("id", "label", "email"):
        hits = [a for a in accs if (a.get(field) or "").lower() == key.lower()]
        if len(hits) == 1:
            return hits[0]
        if len(hits) > 1:
            raise CamError(f"“{key}”匹配到多个账号，请用 id")
    hits = [a for a in accs if a["id"].startswith(key)]
    if len(hits) == 1:
        return hits[0]
    raise CamError(f"找不到账号：{key}（用 cam ls 查看）")


# --------------------------------------------------------------------------- Claude Code 凭据

def keychain_read():
    """读不出来（没有条目、被拒绝、超时）都返回 None。"""
    # 有些版本 account 不是用户名，查不到时退一步只按 service 查
    for by_user in (["-a", getpass.getuser()], []):
        try:
            r = subprocess.run(["security", "find-generic-password", "-s", KEYCHAIN_SERVICE, *by_user, "-w"],
                               capture_output=True, text=True, timeout=15)
        except (OSError, subprocess.TimeoutExpired):
            return None
        if r.returncode == 0:
            break
    else:
        return None
    raw = r.stdout.strip()
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        try:  # 某些情况下以十六进制返回
            return json.loads(bytes.fromhex(raw).decode())
        except Exception:
            return None


def keychain_exists():
    """钥匙串里有没有 Claude Code 的条目（只查是否存在，不读内容，不会弹授权窗）。查不了时按“有”算，宁可不删。"""
    try:
        r = subprocess.run(["security", "find-generic-password", "-s", KEYCHAIN_SERVICE],
                           capture_output=True, timeout=15)
    except (OSError, subprocess.TimeoutExpired):
        return True
    return r.returncode == 0


def keychain_write(obj):
    payload = json.dumps(obj, separators=(",", ":")).encode().hex()
    user = getpass.getuser().replace('"', '')
    # 通过 stdin 交互模式写入，密钥不会出现在进程参数里
    cmd = f'add-generic-password -U -a "{user}" -s "{KEYCHAIN_SERVICE}" -X {payload}\n'
    try:
        r = subprocess.run(["security", "-i"], input=cmd, capture_output=True, text=True, timeout=15)
    except subprocess.TimeoutExpired:
        raise CamError("写入钥匙串超时（可能在等授权弹窗）。重新运行，弹窗时点“始终允许”")
    except OSError as e:
        raise CamError(f"无法调用 security：{e}")
    if r.returncode != 0 or "error" in (r.stderr or "").lower():
        raise CamError(f"写入钥匙串失败：{r.stderr.strip() or r.returncode}")


def read_live_credentials():
    """返回 (creds_dict or None, where)。"""
    if use_keychain():
        kc = keychain_read()
        if kc:
            return kc, "keychain"
    data = read_json(claude_credentials_file(), None)
    if data:
        return data, "file"
    return None, None


def write_live_credentials(creds, where_hint=None):
    target = where_hint
    if target is None:
        target = "keychain" if use_keychain() else "file"
    if target == "keychain":
        keychain_write(creds)
        # 同时存在文件版本时一并更新，避免 Claude Code 读到旧文件
        if os.path.exists(claude_credentials_file()):
            write_json_atomic(claude_credentials_file(), creds, private=True)
    else:
        write_json_atomic(claude_credentials_file(), creds, private=True)


def read_oauth_account():
    cj = read_json(claude_json_file(), {}) or {}
    return cj.get("oauthAccount")


def update_json_file(path, mutate, attempts=5):
    """读-改-写一个别人也会写的 JSON 文件（~/.claude.json，Claude Code 运行时也会写）。

    写入前再读一次，内容变了就重新合并；写完再读回来确认改动还在。这样不会用旧内容覆盖掉
    Claude Code 刚写进去的项目设置。
    """
    real = os.path.realpath(path)
    for _ in range(attempts):
        before = read_bytes(real)
        try:
            obj = json.loads(before) if before and before.strip() else {}
        except json.JSONDecodeError as e:
            raise CamError(f"文件损坏，无法解析：{path}（{e}）")
        if not isinstance(obj, dict):
            obj = {}
        want = mutate(obj)
        if read_bytes(real) != before:
            continue
        write_json_atomic(real, obj)
        if want is None or want((read_json(real, {}) or {})):
            return obj
    raise CamError(f"{path} 一直在被其它程序改写，请先退出正在运行的 claude 再试")


def write_oauth_account(oauth_account):
    def mutate(cj):
        if oauth_account is None:
            cj.pop("oauthAccount", None)
        else:
            cj["oauthAccount"] = oauth_account
        return lambda now: now.get("oauthAccount") == oauth_account
    update_json_file(claude_json_file(), mutate)


def oauth_block(creds):
    return (creds or {}).get("claudeAiOauth") or {}


def token_fp(rt):
    return hashlib.sha256(rt.encode()).hexdigest()[:16]


def match_identity(accs, uuid=None, email=None):
    """按 accountUuid、邮箱在账号列表里找人。"""
    if uuid:
        for a in accs:
            if a.get("account_uuid") == uuid:
                return a
    email = (email or "").lower()
    if email:
        for a in accs:
            if (a.get("email") or "").lower() == email:
                return a
    return None


def identity_agrees(acc, uuid=None, email=None):
    """账号已知的身份和给出的身份是否一致；任何一方缺信息时视为一致。"""
    if acc.get("account_uuid") and uuid:
        return acc["account_uuid"] == uuid
    if acc.get("email") and email:
        return acc["email"].lower() == email.lower()
    return True


def identify_code_account(data, oauth_account, creds):
    """找出当前登录属于哪个已保存账号。

    先比 refresh token 指纹：token 本身就是凭据，比 ~/.claude.json 里的 oauthAccount 可靠
    （oauthAccount 可能和凭据对不上，比如切换前打开的 claude 会话刷新后把旧账号的 token 写回了）。
    token 换过（刷新或重新登录）时才退回到 accountUuid / 邮箱。
    """
    code_accs = [a for a in data["accounts"] if a["kind"] == "claude_code"]
    rt = oauth_block(creds).get("refreshToken")
    if rt:
        fp = token_fp(rt)
        for a in code_accs:
            if a.get("token_fp") == fp:
                return a
    oa = oauth_account or {}
    return match_identity(code_accs, oa.get("accountUuid"), oa.get("emailAddress"))


def probe_token_owner(access_token, timeout=6):
    """用 /api/oauth/profile 问出这个 access token 真正属于谁。

    返回 (HTTP 状态, {"uuid","email"} 或 None)。状态 0 表示根本没连上（离线），和服务端明确说
    “不认识这个 token”不是一回事：前者只是暂时核对不了，后者是答案。
    """
    st, prof = http_json("GET", f"{API_BASE}/api/oauth/profile", headers=oauth_headers(access_token),
                         timeout=timeout)
    if st != 200 or not isinstance(prof, dict):
        return st, None
    a = prof.get("account") or {}
    if not (a.get("uuid") or a.get("email")):
        return st, None
    return st, {"uuid": a.get("uuid"), "email": a.get("email")}


def token_owner(access_token, timeout=6):
    return probe_token_owner(access_token, timeout)[1]


def apply_oauth_account_meta(acc, oauth_account, creds):
    oa = oauth_account or {}
    ob = oauth_block(creds)
    if oa.get("accountUuid"):
        acc["account_uuid"] = oa["accountUuid"]
    if oa.get("emailAddress"):
        acc["email"] = oa["emailAddress"]
    if oa.get("organizationName"):
        acc["org_name"] = oa["organizationName"]
    if ob.get("subscriptionType"):
        acc["plan"] = ob["subscriptionType"]
    if ob.get("refreshToken"):
        acc["token_fp"] = token_fp(ob["refreshToken"])


PENDING_ID = "_pending-live"  # secrets 目录里暂存“还没认出主人”的那份登录；账号 id 是 8 位十六进制，不会撞名
PENDING_MAX_DAYS = 7


def access_token_fresh(ob):
    """access token 本地看还没过期。过期了 /api/oauth/profile 必然 401，不必白跑一趟。"""
    return (ob.get("expiresAt") or 0) > time.time() * 1000


def file_credentials(store, acc, ob, creds, where, oauth_account=None):
    """把一份已确认属于 acc 的登录存进它名下。oauth_account 只在它描述的就是 acc 时传入。

    saved_at 记的是这个 refresh token 是什么时候拿到的：token 没换就沿用原来的时间，
    这样才能和暂存的登录比谁更新（见 _file_pending）。
    """
    prev = store.load_secret(acc["id"])
    same_rt = (prev.get("claudeAiOauth") or {}).get("refreshToken") == ob.get("refreshToken")
    apply_oauth_account_meta(acc, oauth_account, creds)
    store.save_secret(acc["id"], {
        "claudeAiOauth": ob, "oauthAccount": oauth_account if oauth_account else prev.get("oauthAccount"),
        "raw": creds, "store": where,
        "saved_at": prev.get("saved_at") if same_rt and prev.get("saved_at") else now_iso(),
    })


def park_unverified(store, data, candidate, ob, creds, where, oa):
    """认不出这份登录是谁的（离线，或 access token 已过期而 refresh token 又换过）时调用：
    原样暂存，不往任何账号里写。~/.claude.json 里的名字可能是旧会话写回时留下的别人的名字，
    信了就会把 A 的 token 存进 B。能核对时再归档（resolve_pending / adopt_pending_for）。
    """
    prev = store.load_secret(PENDING_ID)
    same_rt = (prev.get("claudeAiOauth") or {}).get("refreshToken") == ob.get("refreshToken")
    at = prev.get("at") if same_rt and prev.get("at") else now_iso()
    cid = candidate["id"] if candidate else None
    store.save_secret(PENDING_ID, {"claudeAiOauth": ob, "oauthAccount": oa or None, "raw": creds,
                                   "store": where, "candidate": cid, "at": at})
    who = f"可能是“{candidate['label']}”，" if candidate else ""
    data["live_unverified"] = {
        "at": at, "candidate": cid,
        "message": (f"Claude Code 当前的登录暂时无法确认属于哪个账号（{who}离线或登录已过期，核对不了），"
                    "这次没有保存。启动一次 claude 或联网后会自动处理。"),
    }


def _newer(a, b):
    """时间戳 a 晚于 b（两边后端写的格式可能略有差异，解析后再比）。"""
    ta, tb = parse_iso(a), parse_iso(b)
    return bool(ta and tb and ta > tb)


def _pending_is_stale(p):
    at = parse_iso(p.get("at"))
    return at is None or dt.datetime.now(dt.timezone.utc) - at > dt.timedelta(days=PENDING_MAX_DAYS)


def _file_pending(store, data, p, ob, owner):
    """暂存的登录已核对出主人：归到它名下；账号里已有更新的就不覆盖；没有这个账号就新建。"""
    code_accs = [a for a in data["accounts"] if a["kind"] == "claude_code"]
    acc = match_identity(code_accs, owner["uuid"], owner["email"])
    if acc is None:
        acc = {
            "id": new_id(), "kind": "claude_code", "label": owner.get("email") or "未命名账号",
            "email": owner.get("email"), "plan": None, "org_name": None, "account_uuid": owner.get("uuid"),
            "manual": {}, "usage": None, "created_at": now_iso(), "last_switched_at": now_iso(),
        }
        data["accounts"].append(acc)
    elif _newer(store.load_secret(acc["id"]).get("saved_at"), p.get("at")):
        return acc
    oa = p.get("oauthAccount") or {}
    oa_ok = bool(oa) and identity_agrees(acc, oa.get("accountUuid"), oa.get("emailAddress"))
    file_credentials(store, acc, ob, p.get("raw"), p.get("store"), oa if oa_ok else None)
    return acc


def resolve_pending(store, data, live_fp=None):
    """处理之前暂存的登录：已有账号存着同一个 token、或放了太久就丢掉；能联网核对了就归档。"""
    p = store.load_secret(PENDING_ID)
    if not p:
        return
    ob = p.get("claudeAiOauth") or {}
    fp = token_fp(ob.get("refreshToken") or "")
    if live_fp is not None and fp == live_fp:
        return  # 就是当前生效的这份登录，走 snapshot_live 的正常流程
    if _pending_is_stale(p) or any(a.get("token_fp") == fp for a in data["accounts"] if a["kind"] == "claude_code"):
        store.delete_secret(PENDING_ID)
        return
    if not access_token_fresh(ob):
        return  # 核对要 access token 还有效；切换到候选账号时会用 refresh token 换一个再核对
    st, owner = probe_token_owner(ob.get("accessToken") or "")
    if owner:
        _file_pending(store, data, p, ob, owner)
        store.delete_secret(PENDING_ID)
    elif st != 0:
        store.delete_secret(PENDING_ID)  # 服务端明确不认这个 token 了，留着也没用


def adopt_pending_for(store, data, target):
    """切换到 target 之前：暂存的登录可能是它的、又比它保存的新，现在核对一下。

    access token 过期就用 refresh token 换一个再问（这时没有 claude 会话在用它，不会互相抢）。
    核对出来是谁的就归到谁名下：真是 target 的，切换就用上最新的 token；其实是别人的，也不会写进 target。
    """
    p = store.load_secret(PENDING_ID)
    if not p or p.get("candidate") != target["id"]:
        return
    if _newer(store.load_secret(target["id"]).get("saved_at"), p.get("at")):
        return  # target 后来又拿到过更新的 token，暂存的这份对它没用了
    ob = dict(p.get("claudeAiOauth") or {})
    owner = token_owner(ob["accessToken"]) if access_token_fresh(ob) and ob.get("accessToken") else None
    if not owner and ob.get("refreshToken") and not running_claude_processes():
        try:
            tok = refresh_oauth(ob["refreshToken"])
        except CamError as e:
            if "登录已失效" in str(e):  # refresh token 本身已作废，这份登录没用了
                store.delete_secret(PENDING_ID)
            return
        ob["accessToken"] = tok["access_token"]
        if tok.get("refresh_token"):
            ob["refreshToken"] = tok["refresh_token"]
        ob["expiresAt"] = int((time.time() + (tok.get("expires_in") or 28800)) * 1000)
        raw = dict(p.get("raw") or {})
        raw["claudeAiOauth"] = ob
        p["raw"] = raw
        owner = token_owner(ob["accessToken"])
    if not owner:
        return  # 还是核对不了：暂存留着，这次用 target 已保存的登录
    _file_pending(store, data, p, ob, owner)
    store.delete_secret(PENDING_ID)


def snapshot_live(store, data, label=None, create_if_unknown=True):
    """把当前生效的 Claude Code 登录保存回对应账号；未知账号自动导入。返回账号或 None。

    只有确认了这份登录是谁的才会存进账号：refresh token 指纹和某个账号保存的一致，或者联网用
    /api/oauth/profile 问出了主人。两样都做不到（离线、access token 过期）时暂存起来，不碰任何账号。
    凭据和 ~/.claude.json 里的 oauthAccount 对不上时，以凭据为准：token 存进它真正的主人名下，
    不会覆盖别的账号，并在 data["live_mismatch"] 里留下说明。
    """
    creds, where = read_live_credentials()
    ob = oauth_block(creds)
    if not creds or not ob.get("accessToken"):
        return None
    oa = read_oauth_account() or {}
    code_accs = [a for a in data["accounts"] if a["kind"] == "claude_code"]
    live_fp = token_fp(ob.get("refreshToken") or "")
    resolve_pending(store, data, live_fp)
    acc = identify_code_account(data, oa, creds)
    by_token = acc is not None and acc.get("token_fp") == live_fp
    oa_uuid, oa_email = oa.get("accountUuid"), oa.get("emailAddress")
    owner, unreachable = None, False
    if not by_token:
        # token 换过了，只能靠 oauthAccount 认人；先联网核对一下这份凭据到底是谁的
        if access_token_fresh(ob):
            st, owner = probe_token_owner(ob["accessToken"])
            unreachable = st == 0
        else:
            unreachable = True  # 过期了，问也是 401
        if owner and (acc is None or not identity_agrees(acc, owner["uuid"], owner["email"])):
            acc = match_identity(code_accs, owner["uuid"], owner["email"])
    if not (by_token or owner) and unreachable and code_accs:
        park_unverified(store, data, acc, ob, creds, where, oa)
        if acc is not None:
            data["active"]["claude_code"] = acc["id"]  # 只是显示用的最佳猜测
        return acc
    data.pop("live_unverified", None)
    pend = store.load_secret(PENDING_ID)
    if pend and token_fp((pend.get("claudeAiOauth") or {}).get("refreshToken") or "") == live_fp:
        store.delete_secret(PENDING_ID)  # 暂存的就是这份，现在认出来了
    # oauthAccount 是否描述的就是这份凭据的主人
    if acc is not None:
        oa_ok = bool(oa) and identity_agrees(acc, oa_uuid, oa_email)
    else:
        oa_ok = bool(oa) and (owner is None or identity_agrees(
            {"account_uuid": owner["uuid"], "email": owner["email"]}, oa_uuid, oa_email))
    if acc is None:
        if not create_if_unknown:
            return None
        email = (owner or {}).get("email") or (oa_email if oa_ok else None)
        acc = {
            "id": new_id(), "kind": "claude_code",
            "label": label or email or "未命名账号",
            "email": email, "plan": None, "org_name": None, "account_uuid": (owner or {}).get("uuid"),
            "manual": {}, "usage": None, "created_at": now_iso(), "last_switched_at": now_iso(),
        }
        data["accounts"].append(acc)
        # 首次导入时记一条切换日志，便于本地用量归属
        if data["active"].get("claude_code") is None:
            data["switch_log"].append({"id": acc["id"], "at": now_iso()})
    elif label:
        acc["label"] = label
    if oa_ok:
        data.pop("live_mismatch", None)
    elif oa:
        # oauthAccount 是别人的：不用它改这个账号的资料，也不把它存进这个账号
        data["live_mismatch"] = {
            "at": now_iso(), "account": acc["id"], "claude_json_email": oa_email,
            "message": (f"Claude Code 实际在用的登录属于“{acc['label']}”，但 ~/.claude.json 里写的是 {oa_email}。"
                        "通常是切换前打开的 claude 会话刷新 token 后写回了旧账号。退出所有 claude 会话后重新切换一次即可。"),
        }
    else:
        data.pop("live_mismatch", None)  # ~/.claude.json 里根本没有 oauthAccount，谈不上对不上
    file_credentials(store, acc, ob, creds, where, oa if oa_ok else None)
    data["active"]["claude_code"] = acc["id"]
    return acc


def auto_import(store):
    """检测 Claude Code 当前登录；是新账号就自动保存。返回新账号或 None。"""
    with store.lock():
        data = store.load()
        before = {a["id"] for a in data["accounts"]}
        acc = snapshot_live(store, data)
        store.save(data)
    if acc and acc["id"] not in before:
        return acc
    return None


def live_fingerprint():
    """登录状态的指纹：文件修改时间；macOS 钥匙串取内容哈希。"""
    parts = []
    for p in (claude_credentials_file(), claude_json_file()):
        try:
            parts.append(str(os.path.getmtime(p)))
        except OSError:
            parts.append("-")
    if use_keychain():
        kc = keychain_read()
        rt = oauth_block(kc).get("refreshToken") if kc else None
        parts.append(hashlib.sha256((rt or "").encode()).hexdigest()[:12])
    return "|".join(parts)


def backup_live(store, tag):
    d = store.new_backup_dir(tag)
    creds, where = read_live_credentials()
    if creds:
        write_json_atomic(os.path.join(d, "credentials.json"), {"store": where, "data": creds}, private=True)
    if os.path.exists(claude_json_file()):
        shutil.copy2(claude_json_file(), os.path.join(d, "claude.json"))
        if not IS_WIN:
            os.chmod(os.path.join(d, "claude.json"), 0o600)
    store.prune_backups()
    return d


def _is_claude_cmd(argv0, cmdline):
    """argv0 是可执行文件（或进程名），cmdline 是完整命令行。"""
    exe = argv0.replace("\\", "/")
    name = exe.rsplit("/", 1)[-1]
    low_exe = exe.lower()
    # Claude 桌面 App（macOS 上叫 Claude，Windows 上装在 AnthropicClaude 下）不算
    if "/claude.app/" in low_exe or "anthropicclaude" in low_exe or "/windowsapps/" in low_exe:
        return False
    if name in ("claude", "claude.exe") or "/claude/versions/" in exe:  # 原生安装版
        return True
    if name.lower() in ("node", "node.exe", "bun", "bun.exe"):  # npm 安装版
        low = cmdline.replace("\\", "/").lower()
        return "@anthropic-ai/claude-code" in low or "claude-code/cli" in low
    return False


def running_claude_processes():
    """正在运行的 Claude Code 进程 [(pid, 命令行)]，不含本进程。查不到时返回空列表。

    切换账号后，这些进程内存里还是旧账号的 token，刷新后还可能把旧账号写回凭据文件。
    """
    me = os.getpid()
    found = []
    try:
        if IS_WIN:
            # 用完整路径而不是 Name：桌面 App 的判断看的是路径，只给文件名的话
            # Claude 桌面 App（AnthropicClaude\…\claude.exe）会被当成 Claude Code
            ps = ("Get-CimInstance Win32_Process -Filter \"Name='claude.exe' or Name='node.exe' or Name='bun.exe'\" | "
                  "ForEach-Object { \"$($_.ProcessId)`t$(if ($_.ExecutablePath) { $_.ExecutablePath } else { $_.Name })"
                  "`t$($_.CommandLine)\" }")
            r = subprocess.run(["powershell", "-NoProfile", "-Command", ps], capture_output=True, text=True,
                               timeout=15)
            for line in r.stdout.splitlines():
                parts = line.split("\t", 2)
                if len(parts) == 3 and parts[0].isdigit() and _is_claude_cmd(parts[1], parts[2]):
                    found.append((int(parts[0]), parts[2] or parts[1]))
        elif os.path.isdir("/proc/self"):
            for pid in os.listdir("/proc"):
                if not pid.isdigit() or int(pid) == me:
                    continue
                try:
                    with open(f"/proc/{pid}/cmdline", "rb") as f:
                        argv = f.read().split(b"\0")
                except OSError:
                    continue
                argv = [a.decode("utf-8", "replace") for a in argv if a]
                if argv and _is_claude_cmd(argv[0], " ".join(argv)):
                    found.append((int(pid), " ".join(argv)))
        else:
            # macOS：comm 是可执行文件完整路径（可能带空格），args 是完整命令行
            def ps(field):
                r = subprocess.run(["ps", "-A", "-o", f"pid=,{field}="], capture_output=True, text=True, timeout=10)
                out = {}
                for line in r.stdout.splitlines():
                    pid, _, rest = line.strip().partition(" ")
                    if pid.isdigit():
                        out[int(pid)] = rest.strip()
                return out
            comm, args = ps("comm"), ps("args")
            for pid, exe in comm.items():
                if pid != me and _is_claude_cmd(exe, args.get(pid, exe)):
                    found.append((pid, args.get(pid, exe)))
    except (OSError, subprocess.SubprocessError):
        return []
    return found


def running_summary(procs):
    pids = "、".join(str(p) for p, _ in procs[:5]) + ("…" if len(procs) > 5 else "")
    return f"有 {len(procs)} 个 claude 进程正在运行（pid {pids}）"


def running_warning(procs):
    if not procs:
        return ""
    return (running_summary(procs) + "，它们还在用切换前的账号，"
            "并可能在刷新 token 后把旧账号写回。请退出这些会话后重开（可用 claude --continue 接着聊）。")


def repair_oauth_account(store, data, acc):
    """当前登录已经是 acc，但 ~/.claude.json 里的 oauthAccount 不是它的（上次切换只完成了一半，
    或旧会话把凭据写回了）：用 acc 保存的 oauthAccount 改回来。返回是否改了。"""
    oa = read_oauth_account() or {}
    if oa and identity_agrees(acc, oa.get("accountUuid"), oa.get("emailAddress")):
        return False
    saved = store.load_secret(acc["id"]).get("oauthAccount")
    if not saved:
        return False
    write_oauth_account(saved)
    data.pop("live_mismatch", None)
    return True


def switch_code_account(store, key):
    with store.lock():
        data = store.load()
        target = find_account(data, key)
        if target["kind"] != "claude_code":
            raise CamError("只有 Claude Code 账号需要切换；网页账号请用 cam open，API 账号无需切换")
        current = snapshot_live(store, data)  # 第 1、2 步：快照并回写
        if current and current["id"] == target["id"]:
            repair_oauth_account(store, data, target)
            data.pop("pending_switch", None)
            store.save(data)
            return target, None, False
        adopt_pending_for(store, data, target)  # 暂存的登录要是 target 的，先核对并存进去，下面就能用上
        secret = store.load_secret(target["id"])
        ob = secret.get("claudeAiOauth")
        if not ob or not ob.get("refreshToken"):
            store.save(data)  # 快照可能刚导入了新账号，先存下
            raise CamError("该账号没有保存可用的凭据，请先用这个账号登录 Claude Code 后执行 cam import")
        backup = backup_live(store, f"before-{target['id']}")  # 第 3 步
        _, where = read_live_credentials()
        raw = dict(secret.get("raw") or {})
        raw["claudeAiOauth"] = ob
        # 第 4 步：先写 ~/.claude.json 再写凭据，凭据写失败就把 oauthAccount 改回去。
        # 万一中途崩溃，凭据还是原来那个号的，再切一次就能完成；反过来的话会被当成“已经是当前账号”。
        prev_oa = read_oauth_account()
        write_oauth_account(secret.get("oauthAccount"))
        try:
            write_live_credentials(raw, where)
        except BaseException:
            try:
                write_oauth_account(prev_oa)
            except Exception:  # noqa: BLE001  回滚失败时保留原来的错误
                pass
            raise
        data.pop("live_mismatch", None)  # 凭据和 oauthAccount 现在是同一个账号的
        data.pop("live_unverified", None)  # 现在生效的登录是我们刚写的，当然认识
        data.pop("pending_switch", None)  # 这次切换优先于之前登记的等待切换
        data["active"]["claude_code"] = target["id"]  # 第 5 步
        target["last_switched_at"] = now_iso()
        data["switch_log"].append({"id": target["id"], "at": now_iso()})
        store.save(data)
        return target, backup, True


# --------------------------------------------------------------------------- 网络

def http_json(method, url, headers=None, body=None, timeout=20):
    h = {"User-Agent": USER_AGENT, "Accept": "application/json"}
    h.update(headers or {})
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            txt = r.read().decode("utf-8", "replace")
            return r.status, (json.loads(txt) if txt.strip() else {})
    except urllib.error.HTTPError as e:
        txt = e.read().decode("utf-8", "replace")
        try:
            return e.code, json.loads(txt)
        except json.JSONDecodeError:
            return e.code, {"error": {"message": txt[:200]}}
    except (urllib.error.URLError, socket.timeout, OSError) as e:
        return 0, {"error": {"message": f"网络错误：{getattr(e, 'reason', e)}"}}


def err_msg(status, body):
    if isinstance(body, dict):
        e = body.get("error")
        if isinstance(e, dict) and e.get("message"):
            return f"HTTP {status}：{e['message']}"
        if isinstance(e, str):
            return f"HTTP {status}：{e}"
    return f"HTTP {status}"


def oauth_headers(token):
    return {"Authorization": f"Bearer {token}", "anthropic-beta": "oauth-2025-04-20",
            "anthropic-version": "2023-06-01"}


def refresh_oauth(refresh_token):
    last = None
    for url in TOKEN_URLS:
        # 在数据锁里调用，超时设短一些，别让其它操作等太久
        st, body = http_json("POST", url, timeout=8, body={
            "grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": OAUTH_CLIENT_ID})
        if st == 200 and body.get("access_token"):
            return body
        last = err_msg(st, body)
        if st in (400, 401):  # refresh token 本身无效，换地址也没用
            raise CamError(f"登录已失效（{last}），需要用这个账号重新登录 Claude Code 后 cam import")
    raise CamError(f"续登录失败（{last}），稍后再试")


def _usable_token(store, data, acc):
    """不联网能拿到的 access token：当前账号读 Claude Code 正在用的；其它账号读保存的、且没过期的。"""
    if data["active"].get("claude_code") == acc["id"]:
        creds, _ = read_live_credentials()
        ob = oauth_block(creds)
        if ob.get("accessToken") and identify_code_account(data, read_oauth_account(), creds) is acc:
            return ob["accessToken"], ob
        ob = dict(store.load_secret(acc["id"]).get("claudeAiOauth") or {})
        if not ob.get("accessToken"):
            raise CamError("没有保存凭据")
        return ob["accessToken"], ob  # 当前账号从不在这里刷新，交给 Claude Code
    ob = dict(store.load_secret(acc["id"]).get("claudeAiOauth") or {})
    if not ob.get("accessToken"):
        raise CamError("没有保存凭据")
    if (ob.get("expiresAt") or 0) > (time.time() + 120) * 1000:
        return ob["accessToken"], ob
    return None


def ensure_fresh_token(store, acc_id, allow_refresh=True):
    """返回 (access token, oauth 块)。

    当前生效账号只读不刷新，由 Claude Code 自己刷新。其它账号的 token 过期时，在数据锁里重新确认
    “它仍然不是当前账号”后再换新，和切换操作互斥：不会出现刚切过去、Claude Code 和这里拿着同一个
    refresh token 各刷一次、把账号弄成需要重新登录的情况。
    allow_refresh=False 时（后台定时任务）不换 token，只用还没过期的。
    """
    data = store.load()
    acc = next((a for a in data["accounts"] if a["id"] == acc_id), None)
    if acc is None:
        raise CamError("账号已被删除")
    got = _usable_token(store, data, acc)
    if got:
        return got
    if not allow_refresh:
        raise CamError("登录已过期。后台不会替它续登录（避免和 Claude Code 抢 refresh token），"
                       "手动刷新一次或切换到这个账号即可")
    with store.lock():
        data = store.load()
        acc = next((a for a in data["accounts"] if a["id"] == acc_id), None)
        if acc is None:
            raise CamError("账号已被删除")
        got = _usable_token(store, data, acc)  # 等锁期间可能已切成当前账号，或已被别的进程刷新
        if got:
            return got
        secret = store.load_secret(acc_id)
        ob = dict(secret.get("claudeAiOauth") or {})
        if not ob.get("refreshToken"):
            raise CamError("登录已过期且没有 refresh token")
        tok = refresh_oauth(ob["refreshToken"])
        ob["accessToken"] = tok["access_token"]
        if tok.get("refresh_token"):
            ob["refreshToken"] = tok["refresh_token"]
        ob["expiresAt"] = int((time.time() + int(tok.get("expires_in", 28800))) * 1000)
        secret["claudeAiOauth"] = ob
        if isinstance(secret.get("raw"), dict):
            secret["raw"]["claudeAiOauth"] = ob
        secret["saved_at"] = now_iso()
        store.save_secret(acc_id, secret)
        if ob.get("refreshToken"):
            acc["token_fp"] = token_fp(ob["refreshToken"])
            store.save(data)
        return ob["accessToken"], ob


def pick_window(body, key):
    w = body.get(key)
    if not isinstance(w, dict):
        return None
    u = w.get("utilization")
    if u is None:
        return None
    return {"utilization": float(u), "resets_at": w.get("resets_at")}


def fetch_code_usage(store, acc, allow_refresh=True):
    token, ob = ensure_fresh_token(store, acc["id"], allow_refresh)
    # 先拿账号资料：既用来补邮箱、套餐，也用来核对这份凭据是不是这个账号的
    st2, prof = http_json("GET", f"{API_BASE}/api/oauth/profile", headers=oauth_headers(token))
    if st2 == 200 and isinstance(prof, dict):
        a = prof.get("account") or {}
        if not identity_agrees(acc, a.get("uuid"), a.get("email")):
            raise CamError(f"这个账号保存的登录实际属于 {a.get('email') or a.get('uuid')}，"
                           "可能被旧的 claude 会话覆盖了。请用这个账号重新登录 Claude Code 后 cam import")
    st, body = http_json("GET", f"{API_BASE}/api/oauth/usage", headers=oauth_headers(token))
    if st != 200:
        raise CamError(err_msg(st, body))
    usage = {"source": "oauth"}
    for k in ("five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"):
        usage[k] = pick_window(body, k)
    extra = body.get("extra_usage")
    if isinstance(extra, dict) and extra.get("is_enabled"):
        usage["extra_usage"] = {k: extra.get(k) for k in ("monthly_limit", "used_credits", "utilization")}
    # 账号资料（邮箱、套餐），失败不影响用量
    if st2 == 200 and isinstance(prof, dict):
        a = prof.get("account") or {}
        o = prof.get("organization") or {}
        if a.get("uuid") and not acc.get("account_uuid"):
            acc["account_uuid"] = a["uuid"]
        if a.get("email"):
            acc["email"] = a["email"]
        if o.get("name"):
            acc["org_name"] = o["name"]
        plan = None
        if a.get("has_claude_max"):
            plan = "max"
        elif a.get("has_claude_pro"):
            plan = "pro"
        elif o.get("organization_type") in ("claude_team", "claude_enterprise"):
            plan = o["organization_type"].replace("claude_", "")
        if plan:
            acc["plan"] = plan
        if o.get("rate_limit_tier"):
            acc["rate_limit_tier"] = o["rate_limit_tier"]
        for k in ("subscription_status", "subscription_expires_at", "billing_type"):
            if o.get(k):
                acc.setdefault("auto", {})[k] = o[k]
    elif ob.get("subscriptionType") and not acc.get("plan"):
        acc["plan"] = ob["subscriptionType"]
    return usage


# 浏览器样式的请求头：带上这些后 claude.ai 更不容易把请求当成机器人。
# 这仍然是用用户自己的登录态做只读查询，不绕过任何验证。
WEB_UA = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
          "(KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36")


def web_get(path, sk, timeout=20):
    """向 claude.ai 发起只读请求。返回 (状态码, 解析后的 JSON 或 None, 原始文本)。"""
    url = f"{CLAUDE_WEB_BASE}{path}"
    h = {
        "User-Agent": WEB_UA,
        "Accept": "application/json, text/plain, */*",
        "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8",
        "Referer": f"{CLAUDE_WEB_BASE}/",
        "Origin": CLAUDE_WEB_BASE,
        "Cookie": f"sessionKey={sk}",
    }
    req = urllib.request.Request(url, headers=h, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read().decode("utf-8", "replace")
            code = r.status
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", "replace")
        code = e.code
    except (urllib.error.URLError, socket.timeout, OSError) as e:
        return 0, None, f"网络错误：{getattr(e, 'reason', e)}"
    try:
        return code, json.loads(raw), raw
    except json.JSONDecodeError:
        return code, None, raw


def is_cloudflare(raw):
    low = (raw or "").lower()
    return any(s in low for s in ("just a moment", "cf-chl", "cloudflare", "challenge-platform", "/cdn-cgi/"))


def web_error(code, body, raw):
    """把 claude.ai 的失败翻译成一句人话。"""
    if is_cloudflare(raw):
        return "claude.ai 返回了人机验证页（通常是网络被公司管控拦截，和账号无关）。可换个网络，或只手动记录到期日。"
    if code in (401, 403):
        return "sessionKey 已过期或无效，请在浏览器里重新复制一次（见 cam set -h）。"
    if code == 0:
        return (body or {}).get("message") if isinstance(body, dict) else str(raw)
    return err_msg(code, body or {"error": {"message": (raw or "")[:120]}})


def plan_from_caps(caps):
    for p in ("claude_max", "claude_pro", "claude_team", "claude_enterprise"):
        if p in (caps or []):
            return p.replace("claude_", "")
    return None


def fetch_web_usage(store, acc):
    sk = store.load_secret(acc["id"]).get("session_key")
    if not sk:
        raise CamError("未填写 sessionKey。在浏览器登录 claude.ai 后复制 sessionKey，运行 "
                       f"cam set {acc['id']} --session-key <值>；或只手动记录到期日。")
    code, orgs, raw = web_get("/api/organizations", sk)
    if code != 200 or not isinstance(orgs, list) or not orgs:
        raise CamError(web_error(code, orgs, raw))
    # 优先选个人订阅那个组织（有 claude_pro / claude_max 能力的）
    org = next((o for o in orgs if plan_from_caps(o.get("capabilities"))), orgs[0])
    if org.get("name"):
        acc["org_name"] = org["name"]
    plan = plan_from_caps(org.get("capabilities"))
    if plan:
        acc["plan"] = plan

    # 账号邮箱，失败不影响用量
    code_a, acct, _ = web_get("/api/account", sk)
    if code_a == 200 and isinstance(acct, dict):
        email = acct.get("email_address") or (acct.get("account") or {}).get("email_address")
        if email:
            acc["email"] = email

    code, body, raw = web_get(f"/api/organizations/{org['uuid']}/usage", sk)
    if code != 200 or not isinstance(body, dict):
        raise CamError(web_error(code, body, raw))
    usage = {"source": "claude.ai"}
    for k in ("five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"):
        usage[k] = pick_window(body, k)
    return usage


def month_start_utc():
    n = dt.datetime.now(dt.timezone.utc)
    return n.replace(day=1, hour=0, minute=0, second=0, microsecond=0)


def fetch_api_usage(store, acc):
    sec = store.load_secret(acc["id"])
    usage = {"source": "api"}
    key = sec.get("api_key")
    if key:
        st, body = http_json("GET", f"{API_BASE}/v1/models?limit=1",
                             headers={"x-api-key": key, "anthropic-version": "2023-06-01"})
        usage["key_valid"] = st == 200
        if st != 200:
            usage["key_error"] = err_msg(st, body)
    admin = sec.get("admin_key")
    if admin:
        h = {"x-api-key": admin, "anthropic-version": "2023-06-01"}
        start = month_start_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        total = 0.0
        page = None
        for _ in range(20):
            q = {"starting_at": start, "limit": 31}
            if page:
                q["page"] = page
            st, body = http_json("GET", f"{API_BASE}/v1/organizations/cost_report?" + urllib.parse.urlencode(q),
                                 headers=h)
            if st != 200:
                raise CamError(err_msg(st, body))
            for bucket in body.get("data", []):
                for r in bucket.get("results", []):
                    try:
                        total += float(r.get("amount") or 0)  # 单位：美分
                    except (TypeError, ValueError):
                        pass
            if not body.get("has_more"):
                break
            page = body.get("next_page")
        usage["cost_month_usd"] = round(total / 100.0, 2)
    if not key and not admin:
        raise CamError("没有填写 API Key")
    return usage


def refresh_usage(store, only_id=None, allow_token_refresh=True):
    """拉取用量。网络请求在锁外进行，最后合并写回；只有换 token 那一步在锁内（见 ensure_fresh_token）。

    后台定时任务传 allow_token_refresh=False：不常用的账号 token 过期后就不再替它续，
    少一些和 Claude Code 抢 refresh token 的机会，也少一些多账号同时在线的请求。
    """
    with store.lock():
        data = store.load()
        # 顺手把 Claude Code 刚刷新过的 token 回写；新登录的账号自动导入
        snapshot_live(store, data)
        store.save(data)
    targets = [a for a in data["accounts"] if only_id in (None, a["id"])]
    results = {}

    def work(acc):
        try:
            if acc["kind"] == "claude_code":
                u = fetch_code_usage(store, acc, allow_token_refresh)
            elif acc["kind"] == "claude_web":
                u = fetch_web_usage(store, acc)
            else:
                u = fetch_api_usage(store, acc)
            u["error"] = None
        except CamError as e:
            u = dict(acc.get("usage") or {})
            u["error"] = str(e)
        except Exception as e:  # 不让单个账号拖垮整体
            u = dict(acc.get("usage") or {})
            u["error"] = f"{type(e).__name__}: {e}"
        u["fetched_at"] = now_iso()
        meta = {k: acc.get(k) for k in ("email", "plan", "org_name", "rate_limit_tier", "auto", "account_uuid")
                if acc.get(k)}
        results[acc["id"]] = (u, meta)

    threads = [threading.Thread(target=work, args=(a,)) for a in targets]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    with store.lock():
        fresh = store.load()
        for a in fresh["accounts"]:
            if a["id"] in results:
                u, meta = results[a["id"]]
                a["usage"] = u
                a.update(meta)
        roll_renewals(fresh)
        store.save(fresh)
        return fresh


def add_months(d: dt.date, n: int, day=None) -> dt.date:
    m = d.month - 1 + n
    y = d.year + m // 12
    m = m % 12 + 1
    return dt.date(y, m, min(day or d.day, calendar.monthrange(y, m)[1]))


def roll_renewals(data, today=None):
    """自动续期：到期日已过且标记自动续期时，顺延一个月（年付则一年）。"""
    today = today or dt.date.today()
    for a in data["accounts"]:
        m = a.get("manual") or {}
        d = parse_day(m.get("expires_at"))
        if not d or m.get("auto_renew") is not True:
            continue
        step = 12 if m.get("cycle") == "yearly" else 1
        # 记住原始扣费日，避免 1 月 31 日顺延到 2 月 28 日后一直停在 28 日
        anchor = m.get("renew_day")
        if not (isinstance(anchor, int) and not isinstance(anchor, bool) and 1 <= anchor <= 31):
            anchor = d.day
        changed = False
        while d < today:
            d = add_months(d, step, anchor)
            changed = True
        if changed:
            m["expires_at"] = d.isoformat()
            m["renew_day"] = anchor


# --------------------------------------------------------------------------- 切号规则
#
# rules: [{"id","type":"dir","path":"~/work","account":"<id>"},
#         {"id","type":"time","days":[1,2,3,4,5],"start":"09:00","end":"18:00","account":"<id>"}]
# 时间规则只在“进入/离开时间段”的那一刻切换，时间段内你手动换号不会被改回去。

DEFAULT_ALERT = 90


def norm_path(p):
    p = os.path.realpath(os.path.expanduser(p))
    return os.path.normcase(p)


def parse_hhmm(v):
    try:
        h, m = str(v).split(":")
        h, m = int(h), int(m)
        if 0 <= h <= 24 and 0 <= m < 60 and h * 60 + m <= 1440:
            return h * 60 + m
    except (ValueError, AttributeError):
        pass
    raise CamError(f"时间格式应为 HH:MM：{v}")


def parse_days(v):
    """'1-5' / '1,3,5' / [1,2] → 排好序的星期列表（1=周一）。"""
    if isinstance(v, list):
        days = [int(x) for x in v]
    else:
        days = []
        for part in str(v).replace("，", ",").split(","):
            part = part.strip()
            if not part:
                continue
            if "-" in part:
                a, b = part.split("-", 1)
                days += list(range(int(a), int(b) + 1))
            else:
                days.append(int(part))
    days = sorted(set(days))
    if not days or any(d < 1 or d > 7 for d in days):
        raise CamError("星期需在 1–7 之间，1 是周一")
    return days


def validate_rules(data, rules):
    ids = {a["id"] for a in data["accounts"] if a["kind"] == "claude_code"}
    out = []
    for r in rules or []:
        if not isinstance(r, dict):
            raise CamError("规则格式不对")
        if r.get("account") not in ids:
            raise CamError("规则里的账号不存在或不是 Claude Code 账号")
        rid = r.get("id") or new_id()
        if r.get("type") == "dir":
            path = str(r.get("path") or "").strip()
            if not path:
                raise CamError("目录规则需要填写目录")
            out.append({"id": rid, "type": "dir", "path": path, "account": r["account"]})
        elif r.get("type") == "time":
            st, en = parse_hhmm(r.get("start")), parse_hhmm(r.get("end"))
            if st == en:
                raise CamError("开始和结束时间不能相同")
            out.append({"id": rid, "type": "time", "days": parse_days(r.get("days") or "1-7"),
                        "start": f"{st // 60:02d}:{st % 60:02d}", "end": f"{en // 60:02d}:{en % 60:02d}",
                        "account": r["account"]})
        else:
            raise CamError("规则类型只能是 dir 或 time")
    return out


def live_rules(data):
    """指向现存 Claude Code 账号的规则。账号被删掉后留下的规则（旧版本删账号时不清理）一律忽略，
    下次保存规则时自然就去掉了。"""
    ids = {a["id"] for a in data["accounts"] if a["kind"] == "claude_code"}
    return [r for r in data.get("rules", []) if isinstance(r, dict) and r.get("account") in ids]


def match_dir_rule(data, cwd):
    """最长前缀匹配。"""
    cwd = norm_path(cwd)
    best = None
    for r in live_rules(data):
        if r.get("type") != "dir":
            continue
        root = norm_path(r["path"])
        if cwd == root or cwd.startswith(root.rstrip(os.sep) + os.sep):
            if best is None or len(root) > len(norm_path(best["path"])):
                best = r
    return best


def match_time_rule(data, when=None):
    when = when or dt.datetime.now()
    wd = when.isoweekday()
    mins = when.hour * 60 + when.minute
    for r in live_rules(data):
        if r.get("type") != "time":
            continue
        st, en = parse_hhmm(r["start"]), parse_hhmm(r["end"])
        if st < en:
            if wd in r["days"] and st <= mins < en:
                return r
        else:  # 跨午夜，例如 22:00–02:00：按开始那天算星期
            if (wd in r["days"] and mins >= st) or (((wd - 2) % 7 + 1) in r["days"] and mins < en):
                return r
    return None


WAIT_POLL_SECS = 5


def pending_switch_view(data):
    """登记中的“等会话结束再切换”，给界面显示用；没有或目标账号已删除则 None。"""
    p = data.get("pending_switch")
    if not p:
        return None
    acc = next((a for a in data["accounts"] if a["id"] == p.get("account")), None)
    if not acc:
        return None
    return {"account": acc["id"], "label": acc["label"], "reason": p.get("reason"), "at": p.get("at"),
            "message": f"等当前 claude 会话结束后自动切换到“{acc['label']}”"
                       + ("（时间规则）" if p.get("reason") == "rule" else "")}


def defer_switch(data, acc_id, reason="manual", rule_id=None):
    """记下“等 claude 会话都退出后切到 acc_id”。调用方持有锁并负责保存。"""
    data["pending_switch"] = {"account": acc_id, "reason": reason, "rule": rule_id, "at": now_iso()}


def cancel_pending_switch(store):
    with store.lock():
        data = store.load()
        had = data.pop("pending_switch", None) is not None
        store.save(data)
        return had


def apply_pending_switch(store, when=None):
    """后台每轮调用：登记过的等待切换，在没有 claude 会话运行时执行。

    返回 (账号, 备份目录, reason)，没切则 None。目标账号没了、已经是当前账号、或时间规则的时间段已经过了，
    就作废。切换本身走 switch_code_account，它会清掉登记。
    """
    data = store.load()
    p = data.get("pending_switch")
    if not p:
        return None
    if running_claude_processes():
        return None
    with store.lock():
        data = store.load()
        p = data.get("pending_switch")
        if not p:
            return None
        known = any(a["id"] == p.get("account") for a in data["accounts"])
        drop = not known or data["active"].get("claude_code") == p["account"]
        if not drop and p.get("reason") == "rule":
            r = match_time_rule(data, when)
            drop = not r or r["id"] != p.get("rule")  # 时间段已经过了，或规则变了
        if drop:
            data.pop("pending_switch", None)
            store.save(data)
            return None
    acc, backup, changed = switch_code_account(store, p["account"])
    return (acc, backup, p.get("reason")) if changed else None


def apply_time_rules(store, when=None):
    """进入新的时间段时切到对应账号。返回切换结果或 None。

    有 claude 会话在跑时不打断它：登记成等待切换，由 apply_pending_switch 在会话退出后执行；
    等到时间段过了还没切成就作废。
    """
    with store.lock():
        data = store.load()
        r = match_time_rule(data, when)
        rid = r["id"] if r else None
        st = data.setdefault("rule_state", {})
        if st.get("time_rule") == rid:
            return None
        st["time_rule"] = rid
        store.save(data)
    if not r or data["active"].get("claude_code") == r["account"]:
        return None
    if running_claude_processes():
        with store.lock():
            data = store.load()
            defer_switch(data, r["account"], "rule", r["id"])
            store.save(data)
        return None
    return switch_code_account(store, r["account"])


def rules_view(data):
    return {"rules": live_rules(data),
            "alert_threshold": (data.get("prefs") or {}).get("alert_threshold", DEFAULT_ALERT)}


def save_rules(store, body):
    with store.lock():
        data = store.load()
        if "rules" in body:
            data["rules"] = validate_rules(data, body["rules"])
            data.setdefault("rule_state", {})["time_rule"] = None  # 规则变了，下次检查时按新规则执行
        if "alert_threshold" in body:
            try:
                t = int(body["alert_threshold"])
            except (TypeError, ValueError):
                raise CamError("提醒阈值应为 1–100 的整数")
            if not 1 <= t <= 100:
                raise CamError("提醒阈值应为 1–100 的整数")
            data.setdefault("prefs", {})["alert_threshold"] = t
        store.save(data)
        return rules_view(data)


def run_claude(store, args):
    """按目录规则切号后启动 claude，参数原样透传。"""
    data = store.load()
    r = match_dir_rule(data, os.getcwd())
    if r:
        acc, _, changed = switch_code_account(store, r["account"])
        if changed:
            print(color(f"[cam] 按目录规则切换到 {acc['label']}", "d"), file=sys.stderr)
            warn = running_warning(running_claude_processes())
            if warn:
                print(color(f"[cam] {warn}", "y"), file=sys.stderr)
    exe = shutil.which("claude")
    if not exe:
        raise CamError("找不到 claude 命令，请确认 Claude Code 已安装")
    if IS_WIN:
        return subprocess.call([exe] + args)
    os.execv(exe, [exe] + args)


# --------------------------------------------------------------------------- 本地用量统计

def local_usage(data, days=30):
    root = os.path.join(claude_config_dir(), "projects")
    since = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=days)
    log = sorted(((parse_iso(e["at"]), e["id"]) for e in data.get("switch_log", []) if parse_iso(e.get("at"))),
                 key=lambda x: x[0])
    seen = set()
    daily = {}  # (date, acc_id) -> dict
    for path in glob.glob(os.path.join(root, "**", "*.jsonl"), recursive=True):
        try:
            if dt.datetime.fromtimestamp(os.path.getmtime(path), dt.timezone.utc) < since:
                continue
            f = open(path, "r", encoding="utf-8", errors="replace")
        except OSError:
            continue
        with f:
            for line in f:
                if '"usage"' not in line:
                    continue
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                msg = ev.get("message") if isinstance(ev, dict) else None
                if not isinstance(msg, dict) or not isinstance(msg.get("usage"), dict):
                    continue
                ts = parse_iso(ev.get("timestamp"))
                if not ts or ts < since:
                    continue
                key = (msg.get("id"), ev.get("requestId"))
                if key != (None, None):
                    if key in seen:
                        continue
                    seen.add(key)
                owner = "unattributed"
                for at, aid in log:
                    if at <= ts:
                        owner = aid
                    else:
                        break
                u = msg["usage"]
                day = ts.astimezone().date().isoformat()
                rec = daily.setdefault((day, owner), {"input": 0, "output": 0, "cache_write": 0, "cache_read": 0,
                                                      "messages": 0})
                rec["input"] += int(u.get("input_tokens") or 0)
                rec["output"] += int(u.get("output_tokens") or 0)
                rec["cache_write"] += int(u.get("cache_creation_input_tokens") or 0)
                rec["cache_read"] += int(u.get("cache_read_input_tokens") or 0)
                rec["messages"] += 1
    out = [{"date": d, "account_id": a, **v} for (d, a), v in sorted(daily.items())]
    return {"days": days, "rows": out}


# --------------------------------------------------------------------------- 网页版账号

def find_browser():
    env = os.environ.get("CAM_BROWSER")
    if env:
        return env
    cands = []
    if IS_MAC:
        cands = ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                 "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                 "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
                 "/Applications/Chromium.app/Contents/MacOS/Chromium"]
    elif IS_WIN:
        pf = [os.environ.get(k, "") for k in ("PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA")]
        for base in pf:
            cands += [os.path.join(base, "Google", "Chrome", "Application", "chrome.exe"),
                      os.path.join(base, "Microsoft", "Edge", "Application", "msedge.exe"),
                      os.path.join(base, "BraveSoftware", "Brave-Browser", "Application", "brave.exe")]
    else:
        for n in ("google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge",
                  "brave-browser"):
            p = shutil.which(n)
            if p:
                cands.append(p)
    for c in cands:
        if c and os.path.exists(c):
            return c
    return None


def open_web_account(store, acc, url=None):
    if acc["kind"] != "claude_web":
        raise CamError("只有网页版账号可以打开浏览器窗口")
    browser = find_browser()
    if not browser:
        raise CamError("没找到 Chrome / Edge / Brave，可用环境变量 CAM_BROWSER 指定浏览器路径")
    prof = os.path.join(store.profiles_dir, acc["id"])
    os.makedirs(prof, exist_ok=True)
    target = url or "https://claude.ai"
    app_mode = (acc.get("manual") or {}).get("app_mode")
    args = [browser, f"--user-data-dir={prof}", "--no-first-run", "--no-default-browser-check",
            f"--app={target}" if app_mode else target]
    kw = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL, "stdin": subprocess.DEVNULL}
    if IS_WIN:
        kw["creationflags"] = 0x00000008 | 0x00000200  # DETACHED_PROCESS | NEW_PROCESS_GROUP
    else:
        kw["start_new_session"] = True
    subprocess.Popen(args, **kw)
    return prof


# --------------------------------------------------------------------------- 对外视图

PUBLIC_FIELDS = ("id", "kind", "label", "email", "plan", "org_name", "rate_limit_tier", "manual", "usage",
                 "created_at", "last_switched_at", "auto")


def days_left(acc, today=None):
    """手动填的到期日优先；没填时用接口返回的订阅到期时间（换算成本地日期）。"""
    manual = (acc.get("manual") or {}).get("expires_at")
    if isinstance(manual, str) and manual:
        d = parse_day(manual)
    else:
        t = parse_iso((acc.get("auto") or {}).get("subscription_expires_at"))
        d = t.astimezone().date() if t else None
    if not d:
        return None
    return (d - (today or dt.date.today())).days


def public_state(store, data):
    accs = []
    active = data["active"].get("claude_code")
    for a in data["accounts"]:
        p = {k: a.get(k) for k in PUBLIC_FIELDS}
        p["is_active"] = a["id"] == active
        p["days_left"] = days_left(a)
        p["plan_display"] = a.get("plan") or (a.get("manual") or {}).get("plan")
        if a["kind"] == "api":
            sec = store.load_secret(a["id"])
            p["api_key_hint"] = mask(sec.get("api_key"))
            p["has_admin_key"] = bool(sec.get("admin_key"))
        if a["kind"] == "claude_web":
            p["has_session_key"] = bool(store.load_secret(a["id"]).get("session_key"))
        accs.append(p)
    return {"version": VERSION, "backend": "python", "host": socket.gethostname(),
            "platform": platform.system(), "active": data["active"], "accounts": accs,
            "warning": (data.get("live_mismatch") or data.get("live_unverified") or {}).get("message"),
            "pending_switch": pending_switch_view(data), "generated_at": now_iso()}


def mask(s):
    if not s:
        return None
    return s[:10] + "…" + s[-4:] if len(s) > 16 else "…"


MANUAL_TEXT = ("plan", "price", "note")
MANUAL_BOOL = ("auto_renew", "app_mode")
SECRET_FIELDS = ("session_key", "api_key", "admin_key")


def apply_manual(acc, fields):
    """把要修改的字段校验后写进 acc（名称和 manual.*），出错时抛 CamError 且不改 acc。

    规则和 Rust 版 api::apply_manual 一致（tests/shared/manual.json）：
    文字字段只收字符串，空字符串表示清除；开关只收 true/false；到期日只收 YYYY-MM-DD。
    只有到期日真的变了才清掉 renew_day，所以界面整份提交表单时不会把月底扣费日弄丢。
    """
    m = dict(acc.get("manual") or {})
    label = acc.get("label")
    if fields.get("label") is not None:
        if not isinstance(fields["label"], str):
            raise CamError("名称应为文字")
        if fields["label"].strip():
            label = fields["label"].strip()[:60]
    for k in MANUAL_TEXT + MANUAL_BOOL + ("expires_at", "cycle"):
        if k not in fields:
            continue
        v = fields[k]
        if v == "":
            v = None
        if k in MANUAL_BOOL:
            if v is not None and not isinstance(v, bool):
                raise CamError(f"{k} 应为 true 或 false")
        elif v is not None and not isinstance(v, str):
            raise CamError("到期日格式应为 YYYY-MM-DD" if k == "expires_at" else f"{k} 应为文字")
        if k == "expires_at" and v is not None:
            if not parse_day(v):
                raise CamError("到期日格式应为 YYYY-MM-DD")
            v = v[:10]
        if k == "expires_at" and v != m.get("expires_at"):
            m.pop("renew_day", None)  # 换了到期日，扣费日按新日期重新算
        if k == "cycle" and v not in (None, "monthly", "yearly"):
            raise CamError("cycle 只能是 monthly 或 yearly")
        m[k] = v
    acc["label"] = label
    acc["manual"] = m


def update_manual(store, key, fields):
    if not isinstance(fields, dict):
        raise CamError("请求体应为 JSON 对象")
    with store.lock():
        data = store.load()
        acc = find_account(data, key)
        apply_manual(acc, fields)
        for k in SECRET_FIELDS:
            v = fields.get(k)
            if v is not None and not isinstance(v, str):
                raise CamError(f"{k} 应为文字")
            if v and v.strip():
                sec = store.load_secret(acc["id"])
                sec[k] = v.strip()
                store.save_secret(acc["id"], sec)
        roll_renewals(data)
        store.save(data)
        return acc


def remove_account(store, key):
    with store.lock():
        data = store.load()
        acc = find_account(data, key)
        if data["active"].get("claude_code") == acc["id"]:
            raise CamError("这是 Claude Code 当前正在用的账号，请先切换到其它账号再删除")
        d = store.new_backup_dir(f"removed-{acc['id']}")
        rules = [r for r in data.get("rules", []) if r.get("account") == acc["id"]]
        write_json_atomic(os.path.join(d, "account.json"), dict(acc, rules=rules), private=True)
        sp = store.secret_path(acc["id"])
        if os.path.exists(sp):
            shutil.move(sp, os.path.join(d, "secret.json"))
        prof = os.path.join(store.profiles_dir, acc["id"])
        if os.path.isdir(prof):  # 网页账号的浏览器 Profile（含登录 Cookie）也一起移走
            shutil.move(prof, os.path.join(d, "browser-profile"))
        data["accounts"] = [a for a in data["accounts"] if a["id"] != acc["id"]]
        data["rules"] = [r for r in data.get("rules", []) if r.get("account") != acc["id"]]
        store.save(data)
        return acc, d


# --------------------------------------------------------------------------- Web 服务

def panel_html():
    here = os.path.dirname(os.path.abspath(__file__))
    for p in (os.path.join(here, "..", "web", "panel.html"), os.path.join(here, "panel.html")):
        if os.path.exists(p):
            with open(p, "r", encoding="utf-8") as f:
                return f.read()
    return "<h1>panel.html not found</h1><p>请把 web/panel.html 放在 cam.py 旁边。</p>"


def server_token(store):
    p = os.path.join(store.root, "server-token")
    try:
        with open(p) as f:
            t = f.read().strip()
            if t:
                return t
    except FileNotFoundError:
        pass
    t = pysecrets.token_urlsafe(24)
    with open(p, "w") as f:
        f.write(t)
    if not IS_WIN:
        os.chmod(p, 0o600)
    return t


def make_handler(store, token):
    class H(http.server.BaseHTTPRequestHandler):
        server_version = f"cam/{VERSION}"

        def log_message(self, fmt, *args):
            if os.environ.get("CAM_VERBOSE"):
                super().log_message(fmt, *args)

        def _send(self, code, obj=None, ctype="application/json; charset=utf-8", raw=None):
            body = raw if raw is not None else json.dumps(obj, ensure_ascii=False).encode()
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Referrer-Policy", "no-referrer")
            self.end_headers()
            self.wfile.write(body)

        def _authed(self):
            h = self.headers.get("Authorization", "")
            got = h[7:] if h.startswith("Bearer ") else ""
            return pysecrets.compare_digest(got.encode(), token.encode())

        def _body(self):
            n = int(self.headers.get("Content-Length") or 0)
            if n > 1_000_000:
                raise CamError("请求过大")
            if not n:
                return {}
            try:
                return json.loads(self.rfile.read(n).decode())
            except json.JSONDecodeError:
                raise CamError("请求体不是合法 JSON")

        def _route(self, method):
            path = urllib.parse.urlparse(self.path).path
            if method == "GET" and path in ("/", "/index.html"):
                return self._send(200, raw=panel_html().encode(), ctype="text/html; charset=utf-8")
            if method == "GET" and path == "/manifest.webmanifest":
                return self._send(200, MANIFEST, ctype="application/manifest+json")
            if not path.startswith("/api/"):
                return self._send(404, {"error": "not found"})
            if not self._authed():
                return self._send(401, {"error": "需要访问令牌"})
            parts = [p for p in path.split("/") if p][1:]
            try:
                if method == "GET" and parts == ["state"]:
                    return self._send(200, public_state(store, store.load()))
                if method == "POST" and parts == ["refresh"]:
                    data = refresh_usage(store, self._body().get("id"))
                    return self._send(200, public_state(store, data))
                if method == "GET" and parts == ["local-usage"]:
                    q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                    days = max(1, min(365, int((q.get("days") or ["30"])[0])))
                    return self._send(200, local_usage(store.load(), days))
                if method == "GET" and parts == ["rules"]:
                    return self._send(200, rules_view(store.load()))
                if method == "PUT" and parts == ["rules"]:
                    return self._send(200, save_rules(store, self._body()))
                if method == "DELETE" and parts == ["pending-switch"]:
                    cancel_pending_switch(store)
                    return self._send(200, public_state(store, store.load()))
                if method == "GET" and parts == ["service-check"]:
                    return self._send(200, {"items": [{"level": lv, "text": t} for lv, t in service_check(store)]})
                if method == "POST" and parts == ["accounts"]:
                    b = self._body()
                    if b.get("kind") == "claude_code":
                        import_current(store, b.get("label"))
                    else:
                        add_account(store, b.get("kind"), b.get("label"), email=b.get("email"),
                                    session_key=b.get("session_key"), api_key=b.get("api_key"),
                                    admin_key=b.get("admin_key"), app_mode=b.get("app_mode"))
                    return self._send(200, public_state(store, store.load()))
                if len(parts) >= 2 and parts[0] == "accounts":
                    aid = parts[1]
                    if method == "POST" and parts[2:] == ["switch"]:
                        return self._send(200, api_switch(store, aid, self._body().get("mode")))
                    if method == "POST" and parts[2:] == ["open"]:
                        open_web_account(store, find_account(store.load(), aid))
                        return self._send(200, {"ok": True, "message": "已在电脑上打开浏览器窗口"})
                    if method == "PATCH" and len(parts) == 2:
                        update_manual(store, aid, self._body())
                        return self._send(200, public_state(store, store.load()))
                    if method == "DELETE" and len(parts) == 2:
                        remove_account(store, aid)
                        return self._send(200, public_state(store, store.load()))
                return self._send(404, {"error": "not found"})
            except CamError as e:
                return self._send(400, {"error": str(e)})
            except Exception as e:
                return self._send(500, {"error": f"{type(e).__name__}: {e}"})

        def do_GET(self):
            self._route("GET")

        def do_POST(self):
            self._route("POST")

        def do_PATCH(self):
            self._route("PATCH")

        def do_DELETE(self):
            self._route("DELETE")

        def do_PUT(self):
            self._route("PUT")

    return H


MANIFEST = {"name": "Claude 账号管理", "short_name": "Claude 账号", "start_url": "/", "display": "standalone",
            "background_color": "#f5f2ea", "theme_color": "#f5f2ea"}


def lan_ips():
    ips = set()
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("10.255.255.255", 1))
        ips.add(s.getsockname()[0])
        s.close()
    except OSError:
        pass
    try:
        for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            ips.add(info[4][0])
    except OSError:
        pass
    return sorted(i for i in ips if not i.startswith("127."))


class ThreadingServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def api_switch(store, aid, mode=None):
    """面板 / 桌面版的切换。有会话在跑且没指定 mode：让前端先问用户；mode=wait：登记等待；mode=now：直接切。"""
    data = store.load()
    target = find_account(data, aid)
    procs = running_claude_processes() if target["id"] != data["active"].get("claude_code") else []
    if procs and mode != "now":
        if mode != "wait":
            return {"ok": False, "choice_required": True, "running_claude": len(procs), "account": target["id"],
                    "label": target["label"],
                    "message": running_summary(procs) + "。等它们结束后再切换不会打断正在跑的任务；现在就切的话它们需要重开。"}
        with store.lock():
            data = store.load()
            defer_switch(data, target["id"], "manual")
            store.save(data)
        return {"ok": True, "changed": False, "deferred": True, "running_claude": len(procs), "account": target["id"],
                "message": f"有 {len(procs)} 个 claude 会话在运行，等它们退出后会自动切换到 {target['label']}"}
    acc, backup, changed = switch_code_account(store, aid)
    procs = running_claude_processes() if changed else []
    msg = (f"已切换到 {acc['label']}。" + (running_warning(procs) or "正在运行的 claude 会话需要重启")
           if changed else "已经是当前账号")
    return {"ok": True, "changed": changed, "account": acc["id"], "running_claude": len(procs), "message": msg,
            "backup": backup}


def serve(store, host, port, auto_refresh_min):
    token = server_token(store)
    srv = ThreadingServer((host, port), make_handler(store, token))
    print(f"Web 面板已启动（Ctrl+C 退出）")
    print(f"  本机：  http://127.0.0.1:{port}/?t={token}")
    if host == "0.0.0.0":
        for ip in lan_ips():
            print(f"  局域网：http://{ip}:{port}/?t={token}")
        print("  手机和电脑在同一网络下打开上面的地址即可；在外网访问建议用 Tailscale 等组网工具，不要直接暴露端口。")
    def watch():
        """后台：发现新登录自动导入；按间隔自动刷新用量；快用完时提醒。

        同一台电脑上只让一个进程做这些事（桌面版、cam serve、菜单栏插件谁先拿到后台锁谁做），
        避免几个进程同时轮询、同时刷新。
        """
        lease = store.background_lock()
        told = False
        last_fp = None
        last_refresh = 0.0
        warned = set()
        while True:
            try:
                if lease.fd is None and not lease.try_acquire():
                    if not told:
                        told = True
                        print("[后台] 桌面版或另一个 cam serve 正在执行后台任务，这里只提供面板")
                    time.sleep(30)
                    continue
                if told:
                    told = False
                    print("[后台] 已接手后台任务")
                fp = live_fingerprint()
                if fp != last_fp:
                    last_fp = fp
                    acc = auto_import(store)
                    if acc:
                        print(f"[自动] 发现新登录的账号，已保存：{acc['label']}")
                        last_refresh = 0.0
                res = apply_time_rules(store)
                if res and res[2]:
                    print(f"[规则] 进入时间段，已切换到 {res[0]['label']}。{running_warning(running_claude_processes())}")
                res = apply_pending_switch(store)
                if res:
                    print(f"[等待切换] claude 会话已退出，已切换到 {res[0]['label']}" + ("（时间规则）" if res[2] == "rule" else ""))
                if auto_refresh_min > 0 and time.time() - last_refresh >= auto_refresh_min * 60:
                    last_refresh = time.time()
                    data = refresh_usage(store, allow_token_refresh=False)
                    limit = rules_view(data)["alert_threshold"]
                    act = data["active"].get("claude_code")
                    for a in data["accounts"]:
                        if a["id"] != act:
                            continue
                        w = (a.get("usage") or {}).get("five_hour") or {}
                        key = (a["id"], w.get("resets_at"))
                        if (w.get("utilization") or 0) >= limit and key not in warned:
                            warned.add(key)
                            print(f"[提醒] {a['label']} 的 5 小时额度已用 {w['utilization']:.0f}%，{fmt_reset(w.get('resets_at'))}")
            except Exception as e:
                print("后台任务出错：", e, file=sys.stderr)
            time.sleep(30 if use_keychain() else 5)

    threading.Thread(target=watch, daemon=True).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


# --------------------------------------------------------------------------- 命令行输出

def color(s, c):
    if not sys.stdout.isatty() or os.environ.get("NO_COLOR"):
        return s
    codes = {"g": "32", "y": "33", "r": "31", "d": "2", "b": "1", "c": "36"}
    return f"\033[{codes[c]}m{s}\033[0m"


def bar(pct, width=10):
    pct = max(0.0, min(100.0, pct))
    n = int(round(pct / 100 * width))
    s = "█" * n + "░" * (width - n)
    return color(s, "r" if pct >= 90 else "y" if pct >= 70 else "g")


def fmt_reset(s):
    d = parse_iso(s)
    if not d:
        return ""
    delta = d - dt.datetime.now(dt.timezone.utc)
    secs = int(delta.total_seconds())
    if secs <= 0:
        return "已重置"
    h, m = divmod(secs // 60, 60)
    if h >= 24:
        return f"{h // 24}天{h % 24}小时后重置"
    return f"{h}小时{m}分后重置" if h else f"{m}分后重置"


KIND_NAME = {"claude_code": "Code", "claude_web": "网页", "api": "API"}


def print_accounts(store, data):
    st = public_state(store, data)
    if not st["accounts"]:
        print("还没有账号。")
        print("  用 Claude Code：先运行 claude 登录，再运行 cam，会自动保存")
        print("  用网页版：cam add-web --label 名字，然后 cam open 名字")
        print("  找不到原因：cam doctor")
        return
    for i, a in enumerate(st["accounts"], 1):
        mark = color("●", "g") if a["is_active"] else " "
        plan = (a.get("plan_display") or "").upper()
        head = f"{mark} {i}. {color(a['label'] or '-', 'b')}  [{KIND_NAME[a['kind']]}]"
        if plan:
            head += f" {color(plan, 'c')}"
        if a.get("email") and a["email"] != a["label"]:
            head += color(f"  {a['email']}", "d")
        print(head + color(f"  id={a['id']}", "d"))
        dl = a.get("days_left")
        if dl is not None:
            exp = (a.get("manual") or {}).get("expires_at")
            tag = f"{dl} 天后到期（{exp}）" if dl >= 0 else f"已过期 {-dl} 天（{exp}）"
            if (a.get("manual") or {}).get("auto_renew"):
                tag += " 自动续期"
            print("     " + color(tag, "r" if dl < 3 else "y" if dl < 7 else "d"))
        u = a.get("usage") or {}
        for k, name in (("five_hour", "5小时"), ("seven_day", "7天  "), ("seven_day_opus", "Opus "),
                        ("seven_day_sonnet", "Sonnet")):
            w = u.get(k)
            if w:
                print(f"     {name} {bar(w['utilization'])} {w['utilization']:5.1f}%  {color(fmt_reset(w.get('resets_at')), 'd')}")
        if u.get("cost_month_usd") is not None:
            print(f"     本月花费 ${u['cost_month_usd']:.2f}")
        if a["kind"] == "api" and a.get("api_key_hint"):
            ok = u.get("key_valid")
            print(f"     Key {a['api_key_hint']} " + ("" if ok is None else color("有效", "g") if ok else color("无效", "r")))
        if u.get("error"):
            print("     " + color("⚠ " + u["error"], "y"))
        if (a.get("manual") or {}).get("note"):
            print("     " + color("备注：" + a["manual"]["note"], "d"))
    if st.get("warning"):
        print("\n" + color("⚠ " + st["warning"], "y"))


# --------------------------------------------------------------------------- 命令

def import_current(store, label=None):
    with store.lock():
        data = store.load()
        acc = snapshot_live(store, data, label=label)
        if data.get("live_unverified"):
            store.save(data)
            raise CamError(data["live_unverified"]["message"])
        if not acc:
            raise CamError("当前没有登录 Claude Code。请先运行 claude 并登录，再导入")
        if label:
            acc["label"] = label
        store.save(data)
        return acc


def add_account(store, kind, label, email=None, session_key=None, api_key=None, admin_key=None, app_mode=False):
    if kind not in ("claude_web", "api"):
        raise CamError("kind 只能是 claude_web 或 api（Claude Code 账号请用导入）")
    if kind == "api" and not api_key and not admin_key:
        raise CamError("API 账号至少需要 API Key 或 Admin Key")
    label = (label or "").strip()[:60] or ("API" if kind == "api" else "网页账号")
    with store.lock():
        data = store.load()
        acc = {"id": new_id(), "kind": kind, "label": label, "email": email or None,
               "plan": "api" if kind == "api" else None, "manual": {"app_mode": bool(app_mode)} if kind == "claude_web" else {},
               "usage": None, "created_at": now_iso()}
        sec = {k: v.strip() for k, v in (("session_key", session_key), ("api_key", api_key), ("admin_key", admin_key)) if v}
        if sec:
            store.save_secret(acc["id"], sec)
        data["accounts"].append(acc)
        store.save(data)
        return acc


def cmd_import(store, args):
    acc = import_current(store, args.label)
    print(f"已保存：{acc['label']}（{acc.get('email') or '-'}）id={acc['id']}")
    print("要添加另一个账号：在 claude 里执行 /login 换号登录，然后再运行一次 cam import。之前的账号已保存，不会丢。")


def cmd_add_web(store, args):
    acc = add_account(store, "claude_web", args.label, email=args.email, session_key=args.session_key, app_mode=args.app)
    print(f"已添加网页账号 {acc['label']}，id={acc['id']}")
    print(f"运行 cam open {acc['id']} 打开它专属的浏览器窗口并登录一次，以后登录状态会一直保留。")


def cmd_add_api(store, args):
    acc = add_account(store, "api", args.label, api_key=args.key, admin_key=args.admin_key)
    print(f"已添加 API 账号 {acc['label']}，id={acc['id']}")


def confirm(question):
    """交互终端里问一句 y/N；非交互（脚本、管道）时返回 False。"""
    if not sys.stdin.isatty():
        return False
    try:
        return input(f"{question} [y/N] ").strip().lower() in ("y", "yes")
    except EOFError:
        return False


def ask_choice(options, default):
    """options: [(键, 说明)]。返回选中的键；直接回车或输入结束用默认值。"""
    legend = " / ".join(f"[{k}] {t}" for k, t in options)
    keys = {k for k, _ in options}
    while True:
        try:
            ans = input(f"{legend}（默认 {default}）：").strip().lower()
        except EOFError:
            return default
        if not ans:
            return default
        if ans in keys:
            return ans


def wait_and_switch(store, target, procs):
    """登记等待切换，然后在这里等到 claude 会话都退出再切。
    桌面版 / cam serve / 菜单栏插件也在跑的话，谁先发现会话退出谁切，这里只是报告结果。"""
    with store.lock():
        data = store.load()
        defer_switch(data, target["id"], "manual")
        store.save(data)
    print(f"等 {len(procs)} 个 claude 会话退出后自动切换到 {target['label']}…（Ctrl-C 取消）")
    shown = len(procs)
    try:
        while True:
            res = apply_pending_switch(store)
            if res:
                acc, backup, _ = res
                print(color(f"✓ 已切换到 {acc['label']}（{acc.get('email') or '-'}）", "g"))
                print(color(f"  切换前的登录状态已备份到 {backup}", "d"))
                return
            data = store.load()
            if not data.get("pending_switch"):
                if data["active"].get("claude_code") == target["id"]:
                    print(color(f"✓ 已切换到 {target['label']}", "g"))
                else:
                    print("等待已被取消，没有切换")
                return
            n = len(running_claude_processes())
            if n != shown:
                shown = n
                print(f"  还有 {n} 个会话在运行…")
            time.sleep(WAIT_POLL_SECS)
    except KeyboardInterrupt:
        cancel_pending_switch(store)
        print("\n已取消，没有切换")


def cmd_switch(store, args):
    if args.cancel_wait:
        print("已取消等待中的切换" if cancel_pending_switch(store) else "没有等待中的切换")
        return
    if not args.account:
        raise CamError("请指定要切换到的账号")
    data = store.load()
    target = find_account(data, args.account)
    procs = running_claude_processes() if target["id"] != data["active"].get("claude_code") else []
    wait = args.wait
    asked = False
    if procs and not wait and not args.yes and sys.stdin.isatty():
        asked = True
        print(color("⚠ " + running_summary(procs) + "。等它们结束后再切不会打断正在跑的任务；现在就切的话它们需要重开。", "y"))
        choice = ask_choice([("w", "等它们结束后再切换"), ("y", "现在就切换"), ("n", "取消")], "w")
        if choice == "n":
            print("已取消")
            return
        wait = choice == "w"
    if procs and wait:
        wait_and_switch(store, target, procs)
        return
    acc, backup, changed = switch_code_account(store, args.account)
    if not changed:
        print(f"已经是当前账号：{acc['label']}")
        return
    print(color(f"✓ 已切换到 {acc['label']}（{acc.get('email') or '-'}）", "g"))
    print(color(f"  切换前的登录状态已备份到 {backup}", "d"))
    print("  会话记录、项目、设置都没有动；正在运行的 claude 需要退出重开，可用 claude --continue 接着聊。")
    if not asked:
        # 记录里的 active 可能过时（切换时的快照才会修正），所以真切换了就再查一次
        warn = running_warning(procs or running_claude_processes())
        if warn:
            print(color("  ⚠ " + warn, "y"))


def cmd_set(store, args):
    fields = {}
    for k in ("label", "plan", "price", "note", "cycle", "session_key", "api_key", "admin_key"):
        v = getattr(args, k, None)
        if v is not None:
            fields[k] = v
    if args.expires is not None:
        fields["expires_at"] = args.expires
    if args.auto_renew:
        fields["auto_renew"] = True
    if args.no_auto_renew:
        fields["auto_renew"] = False
    if not fields:
        raise CamError("没有要修改的内容，见 cam set -h")
    acc = update_manual(store, args.account, fields)
    print(f"已更新 {acc['label']}")


def cmd_rule(store, args):
    data = store.load()
    rc = args.rule_cmd or "ls"
    if rc == "ls":
        names = {a["id"]: a["label"] for a in data["accounts"]}
        rv = rules_view(data)
        if not rv["rules"]:
            print("还没有规则。例：cam rule add-dir ~/work 工作号；cam rule add-time 工作号 --days 1-5 --start 09:00 --end 18:00")
        cur = match_time_rule(data)
        for r in rv["rules"]:
            who = names.get(r["account"], r["account"])
            if r["type"] == "dir":
                print(f"{r['id']}  目录 {r['path']} → {who}")
            else:
                mark = color("  ← 当前时间段", "g") if cur and cur["id"] == r["id"] else ""
                days = ",".join(str(d) for d in r["days"])
                print(f"{r['id']}  时间 周{days} {r['start']}–{r['end']} → {who}{mark}")
        print(color(f"额度提醒阈值：{rv['alert_threshold']}%", "d"))
        return
    rules = rules_view(data)["rules"]
    if rc == "add-dir":
        acc = find_account(data, args.account)
        rules.append({"type": "dir", "path": args.path, "account": acc["id"]})
    elif rc == "add-time":
        acc = find_account(data, args.account)
        rules.append({"type": "time", "days": args.days, "start": args.start, "end": args.end, "account": acc["id"]})
    elif rc == "rm":
        n = len(rules)
        rules = [r for r in rules if r.get("id") != args.rule_id]
        if len(rules) == n:
            raise CamError(f"找不到规则 {args.rule_id}")
    elif rc == "alert":
        save_rules(store, {"alert_threshold": args.percent})
        print(f"已设置：5 小时额度超过 {args.percent}% 时提醒")
        return
    save_rules(store, {"rules": rules})
    print("规则已保存")
    if rc == "add-time":
        res = apply_time_rules(store)
        if res and res[2]:
            print(f"当前正处于该时间段，已切换到 {res[0]['label']}")


# --------------------------------------------------------------------------- Claude Code 环境：检查、安装、清理

NATIVE_INSTALL_SH = "https://claude.ai/install.sh"
NATIVE_INSTALL_PS1 = "https://claude.ai/install.ps1"
# 可以重新生成的缓存。按名字逐个删除，不用通配符，名单以外的东西一概不动
CACHE_ITEMS = ("cache", "statsig", "shell-snapshots", "session-env", "debug", "ide", "telemetry", "paste-cache",
               "downloads", ".last-cleanup", ".last-update-result.json")
# 这些环境变量会让 Claude Code 不走订阅登录，切换账号看起来就“不生效”
OVERRIDE_ENV = ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX")


def install_kind(real):
    low = real.replace("\\", "/").lower()
    if "/.claude/local/" in low:
        return "npm（本地）"
    if "node_modules" in low or "/npm/" in low:
        return "npm"
    if "/.local/share/claude/" in low or "/.local/bin/" in low:
        return "原生"
    if "homebrew" in low or "/cellar/" in low or "caskroom" in low:
        return "Homebrew"
    return "其它"


def claude_installs():
    """PATH 里所有的 claude，按真实路径去重，排在前面的是实际会运行的那个。返回 [(路径, 真实路径, 安装方式)]。"""
    names = ("claude.exe", "claude.cmd", "claude") if IS_WIN else ("claude",)
    seen, out = set(), []
    for d in os.environ.get("PATH", "").split(os.pathsep):
        for n in names:
            p = os.path.join(d, n)
            if os.path.isfile(p) and os.access(p, os.X_OK):
                real = os.path.realpath(p)
                if real not in seen:
                    seen.add(real)
                    out.append((p, real, install_kind(real)))
    return out


def claude_version(exe):
    try:
        r = subprocess.run([exe, "--version"], capture_output=True, text=True, timeout=20)
        return (r.stdout or r.stderr).strip().splitlines()[0] if r.returncode == 0 else None
    except (OSError, subprocess.SubprocessError, IndexError):
        return None


def override_findings():
    """会让 Claude Code 不用订阅登录的设置（它们优先于登录凭据）。"""
    out = [f"环境变量 {k} 已设置" for k in OVERRIDE_ENV if os.environ.get(k)]
    for name in ("settings.json", "settings.local.json"):
        path = os.path.join(claude_config_dir(), name)
        try:
            s = read_json(path, {}) or {}
        except CamError:
            continue
        if not isinstance(s, dict):
            continue
        if s.get("apiKeyHelper"):
            out.append(f"{path} 里设置了 apiKeyHelper")
        env = s.get("env") if isinstance(s.get("env"), dict) else {}
        out += [f"{path} 的 env 里设置了 {k}" for k in OVERRIDE_ENV if env.get(k)]
    try:
        if (read_json(claude_json_file(), {}) or {}).get("primaryApiKey"):
            out.append(f"{claude_json_file()} 里存着 API Key（primaryApiKey）")
    except CamError:
        pass
    return out


def path_size(p):
    if not os.path.isdir(p) or os.path.islink(p):
        try:
            return os.lstat(p).st_size
        except OSError:
            return 0
    total = 0
    for root, _, files in os.walk(p):
        for f in files:
            try:
                total += os.lstat(os.path.join(root, f)).st_size
            except OSError:
                pass
    return total


def human_size(n):
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024 or unit == "GB":
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024


def remove_path(p):
    if os.path.isdir(p) and not os.path.islink(p):
        shutil.rmtree(p)
    else:
        os.unlink(p)


def keychain_delete():
    try:
        r = subprocess.run(["security", "delete-generic-password", "-s", KEYCHAIN_SERVICE, "-a", getpass.getuser()],
                           capture_output=True, timeout=15)
        if r.returncode != 0:  # 和 keychain_read 一样，account 不是用户名时只按 service 删
            subprocess.run(["security", "delete-generic-password", "-s", KEYCHAIN_SERVICE],
                           capture_output=True, timeout=15)
    except subprocess.TimeoutExpired:
        raise CamError("删除钥匙串里的 Claude Code 登录超时（可能在等授权弹窗）。重新运行，弹窗时点“始终允许”")
    except OSError as e:
        raise CamError(f"无法调用 security：{e}")


def cmd_env_install(store, args):
    ok = color("✓", "g")
    found = claude_installs()
    if found and not args.force:
        print(f"{ok} 已经装好了：{found[0][0]}（{install_kind(found[0][1])}，{claude_version(found[0][0]) or '版本未知'}）")
        print("  要重新安装或升级，加 --force")
        print_install_conflicts(found)
        return
    if args.npm:
        npm = shutil.which("npm")
        if not npm:
            raise CamError("没找到 npm。请先装 Node.js 18 以上，或去掉 --npm 改用官方原生安装器")
        cmd, shown = [npm, "install", "-g", "@anthropic-ai/claude-code"], "npm install -g @anthropic-ai/claude-code"
    elif IS_WIN:
        shown = f"irm {NATIVE_INSTALL_PS1} | iex"
        cmd = ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", shown]
    else:
        if not shutil.which("curl"):
            raise CamError("没找到 curl，请先安装 curl，或加 --npm 用 npm 安装")
        shown = f"curl -fsSL {NATIVE_INSTALL_SH} | bash"
        cmd = ["bash", "-c", shown]
    print(f"将运行 Anthropic 官方安装命令：{shown}")
    if not args.yes:
        if not sys.stdin.isatty():
            raise CamError("非交互环境请加 -y 确认")
        if not confirm("继续？"):
            print("已取消")
            return
    rc = subprocess.call(cmd)
    if rc != 0:
        raise CamError(f"安装命令失败（退出码 {rc}），见上面的输出")
    found = claude_installs()
    if not found:
        local = os.path.join(home(), ".local", "bin", "claude.exe" if IS_WIN else "claude")
        if not os.path.exists(local):
            raise CamError("安装命令跑完了，但没找到 claude，见上面的输出")
        print(f"{ok} 已安装到 {local}（{claude_version(local) or '版本未知'}）")
        print(color(f"  但 {os.path.dirname(local)} 不在 PATH 里，把它加进 PATH 后重开终端", "y"))
    else:
        print(f"{ok} 已安装：{found[0][0]}（{claude_version(found[0][0]) or '版本未知'}）")
        print_install_conflicts(found)
    print("下一步：运行 claude 登录，然后运行 cam，会自动保存这个账号")


def print_install_conflicts(found):
    if len(found) <= 1:
        return
    print(color(f"⚠ PATH 里有 {len(found)} 份 claude，实际运行的是第一个：", "y"))
    for p, real, kind in found:
        print(f"    {p}（{kind}）" + (f" → {real}" if real != p else ""))
    print("  多份并存容易出现“升级了却没生效”。保留一份即可，npm 版可用 npm uninstall -g @anthropic-ai/claude-code 卸载")


def mark_logged_out(store):
    with store.lock():
        data = store.load()
        data["active"]["claude_code"] = None
        data.pop("live_mismatch", None)
        store.save(data)


def cmd_env_clean(store, args):
    """分层清理。默认只预演；--apply 才执行。会话记录、设置、CLAUDE.md、插件和 cam 自己的数据永远不动（--purge 除外）。"""
    cfg = claude_config_dir()
    cache = [os.path.join(cfg, n) for n in CACHE_ITEMS if os.path.lexists(os.path.join(cfg, n))]
    if args.purge:
        root_real = os.path.realpath(store.root)
        if root_real == os.path.realpath(cfg) or root_real.startswith(os.path.realpath(cfg) + os.sep):
            raise CamError(f"cam 的数据目录 {store.root} 在 {cfg} 里面，不能整体移走")
        print("将把整个 Claude Code 配置移到备份目录（不删除，随时可以移回来）：")
        for p in (cfg, claude_json_file()):
            if os.path.lexists(p):
                print(f"  {p}（{human_size(path_size(p))}）")
        if use_keychain():
            print("  钥匙串里的 Claude Code 登录（内容先存进备份）")
        print("包括会话记录、项目设置、CLAUDE.md、插件——相当于全新安装。已保存的账号凭据会先存进 cam，以后能切回来。")
    else:
        print("将删除这些可以重新生成的缓存：" if cache else "没有需要清理的缓存。")
        for p in cache:
            print(f"  {p}（{human_size(path_size(p))}）")
        if args.logout:
            print("将退出 Claude Code 登录：删除凭据" + ("（含钥匙串）" if use_keychain() else "")
                  + "，并去掉 ~/.claude.json 里的 oauthAccount。当前账号的最新凭据会先存进 cam，可以随时切回来。")
        print(color("不会动：会话记录（projects/）、settings.json、CLAUDE.md、commands/agents/skills/plugins、"
                    "~/.claude.json 里的项目配置、cam 的数据目录。", "d"))
    if not args.apply:
        print(color("\n这只是预演，加 --apply 才会执行。", "y"))
        return
    procs = running_claude_processes()
    if procs and not args.force:
        raise CamError(f"有 {len(procs)} 个 claude 进程正在运行，清理可能弄坏它们。请先退出，或加 --force")
    if args.purge and not args.yes:
        if not sys.stdin.isatty():
            raise CamError("非交互环境请加 -y 确认")
        if not confirm("确定要把整个 Claude Code 配置移走？"):
            print("已取消")
            return
    kc = None
    if (args.logout or args.purge) and use_keychain():
        kc = keychain_read()
        if not kc and keychain_exists():
            # 读不出来就删，最新的 refresh token 就没了
            raise CamError("钥匙串里有 Claude Code 的登录，但读取失败（被拒绝或超时）。为了不丢掉最新的登录，"
                           "这次什么都没改。重新运行，弹窗时点“始终允许”")
    if args.logout or args.purge:
        with store.lock():
            data = store.load()
            snapshot_live(store, data)  # 先把当前账号的最新 token 存好
            store.save(data)
    if args.purge:
        dest = os.path.join(store.root, "env-backups", dt.datetime.now().strftime("%Y%m%d-%H%M%S"))
        os.makedirs(dest)
        if not IS_WIN:
            os.chmod(dest, 0o700)
        if kc:
            write_json_atomic(os.path.join(dest, "keychain.json"), kc, private=True)
            keychain_delete()
        cj = claude_json_file()
        if os.path.lexists(cfg):
            shutil.move(cfg, os.path.join(dest, "claude-config"))
        if os.path.lexists(cj):
            shutil.move(cj, os.path.join(dest, "claude.json"))
        mark_logged_out(store)
        print(color(f"✓ 已移到 {dest}", "g"))
        print("  需要恢复时，把里面的 claude-config 移回 " + cfg + "，claude.json 移回 " + cj)
        return
    for p in cache:
        remove_path(p)
    if cache:
        print(color(f"✓ 已清理 {len(cache)} 项缓存", "g"))
    if args.logout:
        backup = backup_live(store, "env-logout")
        if kc:
            keychain_delete()
        if os.path.exists(claude_credentials_file()):
            os.unlink(claude_credentials_file())
        if os.path.exists(claude_json_file()):
            write_oauth_account(None)
        mark_logged_out(store)
        print(color(f"✓ 已退出登录，退出前的登录状态备份在 {backup}", "g"))
        print("  运行 claude 重新登录，或 cam switch <账号> 切回已保存的账号")


def http_text(url, timeout=8):
    """GET 纯文本。返回 (状态, 文本)；状态 0 表示没连上。"""
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except (urllib.error.URLError, socket.timeout, OSError) as e:
        return 0, f"网络错误：{getattr(e, 'reason', e)}"


# Anthropic 公开的“不提供服务的地区”里常见的几个（名单会变，只用来提示，实际以接口实测为准）
UNSUPPORTED_REGIONS = {"CN": "中国大陆", "HK": "香港", "MO": "澳门", "RU": "俄罗斯", "IR": "伊朗", "KP": "朝鲜",
                       "SY": "叙利亚", "CU": "古巴", "BY": "白俄罗斯", "VE": "委内瑞拉", "AF": "阿富汗", "MM": "缅甸",
                       "YE": "也门"}


def region_denied(msg):
    low = (msg or "").lower()
    return any(k in low for k in ("region", "country", "not available", "unsupported", "not allowed", "地区"))


def service_check(store=None):
    """检测 Claude 相关服务是否正常：DNS、出口地区、api.anthropic.com、claude.ai、代理。

    返回 [(级别, 文字)]，级别为 ok / bad / warn / info。不打印任何密钥。
    """
    out = []
    proxies = {k: os.environ[k] for k in ("HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy")
               if os.environ.get(k)}
    api_host = urllib.parse.urlparse(API_BASE).hostname or API_BASE
    web_host = urllib.parse.urlparse(CLAUDE_WEB_BASE).hostname or CLAUDE_WEB_BASE
    for host in dict.fromkeys((api_host, web_host)):
        try:
            ips = sorted({ai[4][0] for ai in socket.getaddrinfo(host, 443, proto=socket.IPPROTO_TCP)})
            out.append(("ok", f"DNS：{host} → {', '.join(ips[:3])}"))
        except OSError as e:
            out.append(("bad", f"DNS：{host} 解析失败（{e}）" + ("；设了代理的话由代理解析，可忽略" if proxies else "")))
    # 出口地区：claude.ai 在 Cloudflare 后面，/cdn-cgi/trace 会返回它看到的访问者信息
    t0 = time.time()
    st, txt = http_text(f"{CLAUDE_WEB_BASE}/cdn-cgi/trace")
    if st != 200 or "loc=" not in txt:
        st, txt = http_text("https://www.cloudflare.com/cdn-cgi/trace")
    if st == 200 and "loc=" in txt:
        kv = dict(line.split("=", 1) for line in txt.splitlines() if "=" in line)
        loc, ip, colo = kv.get("loc", "?"), kv.get("ip", "?"), kv.get("colo", "?")
        if loc in UNSUPPORTED_REGIONS:
            out.append(("warn", f"出口地区：{loc}（{UNSUPPORTED_REGIONS[loc]}，IP {ip}）—— Anthropic 公开名单里不支持的地区，"
                                "以下面的实测为准"))
        else:
            out.append(("ok", f"出口地区：{loc}（IP {ip}，Cloudflare 节点 {colo}，{int((time.time() - t0) * 1000)} ms）"))
    else:
        out.append(("info", "出口地区：查不到（连不上 Cloudflare 的 trace 接口）"))
    # api.anthropic.com：有登录就用当前登录实测
    creds = read_live_credentials()[0] if store is not None else None
    at = oauth_block(creds).get("accessToken") if creds else None
    t0 = time.time()
    if at:
        st, body = http_json("GET", f"{API_BASE}/api/oauth/profile", headers=oauth_headers(at), timeout=10)
    else:
        st, body = http_json("GET", f"{API_BASE}/api/oauth/usage", timeout=10)
    ms = int((time.time() - t0) * 1000)
    msg = ((body or {}).get("error") or {}).get("message", "") if isinstance(body, dict) else ""
    if st == 0:
        out.append(("bad", f"{api_host}：连不上（{msg}）"))
    elif st == 403 or region_denied(msg):
        out.append(("bad", f"{api_host}：拒绝访问（HTTP {st}：{msg or '-'}），通常是出口地区不支持，换个出口再试"))
    elif at and st == 200:
        out.append(("ok", f"{api_host}：服务正常，当前登录可用（{ms} ms）"))
    elif at and st == 401:
        out.append(("warn", f"{api_host}：服务可达，但当前登录已过期或无效，Claude Code 下次运行时会自动续（{ms} ms）"))
    elif st == 401:
        out.append(("ok", f"{api_host}：服务可达（未登录，{ms} ms）"))
    elif st >= 500:
        out.append(("bad", f"{api_host}：服务端错误（HTTP {st}），可能是 Anthropic 故障，稍后再试"))
    else:
        out.append(("warn", f"{api_host}：HTTP {st}（{msg or '-'}，{ms} ms）"))
    # claude.ai：不带登录，看可达性和有没有被网络管控拦截
    t0 = time.time()
    code, body, raw = web_get("/api/organizations", "", timeout=10)
    ms = int((time.time() - t0) * 1000)
    msg = ((body or {}).get("error") or {}).get("message", "") if isinstance(body, dict) else ""
    if code == 0:
        out.append(("bad", f"{web_host}：连不上（{raw}）"))
    elif is_cloudflare(raw):
        out.append(("warn", f"{web_host}：返回人机验证页（通常是网络被公司管控拦截），网页账号的额度会拿不到（{ms} ms）"))
    elif region_denied(msg):
        out.append(("bad", f"{web_host}：拒绝访问（HTTP {code}：{msg}），通常是出口地区不支持"))
    elif code in (200, 401, 403):
        out.append(("ok", f"{web_host}：可达（{ms} ms）"))
    elif code >= 500:
        out.append(("bad", f"{web_host}：服务端错误（HTTP {code}），稍后再试"))
    else:
        out.append(("warn", f"{web_host}：HTTP {code}（{msg or (raw or '')[:80]}，{ms} ms）"))
    out.append(("info", "代理：" + ("；".join(f"{k}={v}" for k, v in proxies.items()) if proxies else "无（直连）")))
    return out


def cmd_doctor(store):
    """逐项检查，不打印任何密钥。"""
    ok, bad, info, warn = color("✓", "g"), color("✗", "r"), color("·", "d"), color("!", "y")
    print(f"cam {VERSION}，Python {platform.python_version()}，{platform.system()} {platform.release()}")
    found = claude_installs()
    if found:
        p, real, kind = found[0]
        print(f"{ok} Claude Code 命令：{p}（{kind}，{claude_version(p) or '版本未知'}）")
        print_install_conflicts(found)
    else:
        print(f"{bad} Claude Code 命令：没找到。运行 cam env install 用官方安装器安装")
    for f in override_findings():
        print(f"{warn} {f}：它会优先于订阅登录，切换账号可能不生效")
    procs = running_claude_processes()
    print(f"{info} 正在运行的 claude 进程：{len(procs)} 个" + ("（切换账号后需要重开）" if procs else ""))
    print(f"{info} 配置目录：{claude_config_dir()}" + ("" if os.path.isdir(claude_config_dir()) else "（不存在）"))
    f = claude_credentials_file()
    if os.path.exists(f):
        has = bool(oauth_block(read_json(f, {}) or {}).get("refreshToken"))
        print(f"{ok if has else bad} 凭据文件：{f}" + ("" if has else "（里面没有登录信息）"))
    else:
        print(f"{info} 凭据文件：不存在" + ("（macOS 一般存在钥匙串里，正常）" if IS_MAC else ""))
    if IS_MAC:
        if not use_keychain():
            print(f"{info} 钥匙串：已跳过（设置了 CLAUDE_CONFIG_DIR 或 CAM_NO_KEYCHAIN）")
        else:
            kc = keychain_read()
            if kc and oauth_block(kc).get("refreshToken"):
                print(f"{ok} 钥匙串：有 Claude Code 登录信息")
            elif kc:
                print(f"{bad} 钥匙串：有条目但没有登录信息")
            else:
                r = subprocess.run(["security", "find-generic-password", "-s", KEYCHAIN_SERVICE],
                                   capture_output=True, text=True)
                if r.returncode == 0:
                    print(f"{bad} 钥匙串：条目存在但读取被拒绝。重新运行时如果弹窗，请点“始终允许”")
                else:
                    print(f"{bad} 钥匙串：没有 Claude Code 登录信息")
    creds, where = read_live_credentials()
    oa = read_oauth_account() or {}
    print(f"{ok if oa.get('emailAddress') else bad} 当前登录邮箱："
          + (oa.get("emailAddress") or "无（Claude Code 还没登录，运行 claude 登录一次）"))
    data = store.load()
    acc = identify_code_account(data, oa, creds) if creds else None
    print(f"{info} 数据目录：{store.root}，已保存 {len(data['accounts'])} 个账号")
    if creds and oauth_block(creds).get("accessToken"):
        print(f"{ok if acc else bad} 当前登录" + (f"已保存为“{acc['label']}”" if acc else "还没保存，运行 cam 会自动保存"))
    if (data.get("live_mismatch") or {}).get("message"):
        print(f"{warn} {data['live_mismatch']['message']}")
    exp = oauth_block(creds).get("expiresAt") if creds else None
    if exp:
        left = exp / 1000 - dt.datetime.now().timestamp()
        print(f"{info} access token " + (f"{int(left // 60)} 分钟后过期（Claude Code 会自动续）" if left > 0 else "已过期（Claude Code 下次运行时会自动续）"))
    pend = pending_switch_view(data)
    if pend:
        print(f"{info} {pend['message']}（cam switch --cancel-wait 可取消）")
    print("\n服务检查")
    marks = {"ok": ok, "bad": bad, "warn": warn, "info": info}
    items = service_check(store)
    for level, text in items:
        print(f"{marks[level]} {text}")
    if any("CERTIFICATE_VERIFY_FAILED" in t for _, t in items):
        print("    这是 python.org 版 Python 没装根证书，运行一次即可修好：")
        print(f'    open "/Applications/Python {sys.version_info.major}.{sys.version_info.minor}/Install Certificates.command"')


def main(argv=None):
    p = argparse.ArgumentParser(prog="cam", description="Claude 多账号管理（脚本版）",
                                formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    p.add_argument("--version", action="version", version=VERSION)
    sub = p.add_subparsers(dest="cmd")

    s = sub.add_parser("import", help="保存当前 Claude Code 登录的账号")
    s.add_argument("--label")
    sub.add_parser("ls", aliases=["list"], help="列出账号（不联网）")
    s = sub.add_parser("usage", help="刷新用量并列出")
    s.add_argument("account", nargs="?")
    s = sub.add_parser("switch", aliases=["use"], help="切换 Claude Code 账号")
    s.add_argument("account", nargs="?")
    s.add_argument("-y", "--yes", action="store_true", help="有 claude 正在运行时也不再确认，直接切")
    s.add_argument("-w", "--wait", action="store_true", help="有 claude 正在运行时等它们都退出后再切（本命令会一直等）")
    s.add_argument("--cancel-wait", action="store_true", help="取消登记中的等待切换")
    sub.add_parser("current", aliases=["whoami"], help="显示当前 Claude Code 账号")
    s = sub.add_parser("add-web", help="添加网页版账号")
    s.add_argument("--label", required=True)
    s.add_argument("--email")
    s.add_argument("--session-key", help="可选，claude.ai 的 sessionKey Cookie，用于自动获取用量")
    s.add_argument("--app", action="store_true", help="以独立应用窗口打开")
    s = sub.add_parser("open", help="打开网页版账号的浏览器窗口")
    s.add_argument("account")
    s = sub.add_parser("add-api", help="添加 API Key 账号")
    s.add_argument("--label", required=True)
    s.add_argument("--key")
    s.add_argument("--admin-key")
    s = sub.add_parser("set", help="手动补齐账号信息")
    s.add_argument("account")
    s.add_argument("--label")
    s.add_argument("--plan")
    s.add_argument("--expires", help="到期/续费日 YYYY-MM-DD，传空字符串清除")
    s.add_argument("--auto-renew", action="store_true")
    s.add_argument("--no-auto-renew", action="store_true")
    s.add_argument("--cycle", choices=["monthly", "yearly"])
    s.add_argument("--price")
    s.add_argument("--note")
    s.add_argument("--session-key")
    s.add_argument("--api-key")
    s.add_argument("--admin-key")
    s = sub.add_parser("rm", aliases=["remove"], help="删除账号（凭据移入备份）")
    s.add_argument("account")
    s = sub.add_parser("local", help="本地 Claude Code token 消耗统计")
    s.add_argument("--days", type=int, default=7)
    s = sub.add_parser("serve", help="启动 Web 面板")
    s.add_argument("--lan", action="store_true", help="允许局域网（手机）访问")
    s.add_argument("--port", type=int, default=8787)
    s.add_argument("--refresh-every", type=int, default=10, help="自动刷新用量间隔（分钟，0 关闭）")
    s = sub.add_parser("json", help="输出账号状态 JSON（给其它脚本用）")
    sub.add_parser("claude", help="按目录规则切号后启动 claude（参数原样透传）")
    sub.add_parser("doctor", help="检查环境，排查找不到账号等问题（不显示密钥）")
    s = sub.add_parser("env", help="Claude Code 环境：检查 / 安装 / 清理")
    esub = s.add_subparsers(dest="env_cmd")
    esub.add_parser("doctor", aliases=["check"], help="同 cam doctor：检查环境和 Claude 服务")
    e = esub.add_parser("install", help="用 Anthropic 官方安装器安装 Claude Code")
    e.add_argument("--npm", action="store_true", help="改用 npm 全局安装")
    e.add_argument("--force", action="store_true", help="已经装了也重新安装（可用来升级）")
    e.add_argument("-y", "--yes", action="store_true", help="不再确认")
    e = esub.add_parser("clean", help="清理 Claude Code 环境（默认只预演，不改任何东西）")
    e.add_argument("--logout", action="store_true", help="同时退出登录（凭据先存进 cam，随时能切回来）")
    e.add_argument("--purge", action="store_true",
                   help="把整个 Claude Code 配置（含会话记录、设置）移到 cam 的 env-backups 目录，相当于全新安装")
    e.add_argument("--apply", action="store_true", help="真正执行；不加只显示将要做什么")
    e.add_argument("--force", action="store_true", help="有 claude 正在运行时也执行")
    e.add_argument("-y", "--yes", action="store_true", help="--purge 时不再确认")
    s = sub.add_parser("rule", help="切号规则")
    rsub = s.add_subparsers(dest="rule_cmd")
    rsub.add_parser("ls", help="列出规则")
    r = rsub.add_parser("add-dir", help="目录规则：在该目录（含子目录）里用 cam claude 时自动切到指定账号")
    r.add_argument("path")
    r.add_argument("account")
    r = rsub.add_parser("add-time", help="时间规则：进入时间段时自动切到指定账号")
    r.add_argument("account")
    r.add_argument("--days", default="1-5", help="星期，1=周一，例如 1-5 或 6,7（默认 1-5）")
    r.add_argument("--start", required=True, help="HH:MM")
    r.add_argument("--end", required=True, help="HH:MM，可以跨午夜")
    r = rsub.add_parser("rm", help="删除规则")
    r.add_argument("rule_id")
    r = rsub.add_parser("alert", help="设置额度提醒阈值（百分比）")
    r.add_argument("percent", type=int)

    argv = list(sys.argv[1:] if argv is None else argv)
    # cam claude …：后面的参数原样交给 claude，不经过 argparse
    if argv[:1] == ["claude"]:
        try:
            return run_claude(Store(), argv[1:]) or 0
        except CamError as e:
            print(color("错误：", "r") + str(e), file=sys.stderr)
            return 1
    args = p.parse_args(argv)
    store = Store()
    try:
        c = args.cmd
        if c in (None, "ls", "list", "json"):
            acc = auto_import(store)
            if acc and c != "json":
                print(color(f"发现新登录的账号，已自动保存：{acc['label']}\n", "g"))
        if c is None:
            print_accounts(store, store.load())
            print(color("\n提示：cam usage 刷新用量，cam switch <账号> 切换，cam -h 查看全部命令", "d"))
        elif c == "import":
            cmd_import(store, args)
        elif c in ("ls", "list"):
            print_accounts(store, store.load())
        elif c == "usage":
            data = refresh_usage(store, find_account(store.load(), args.account)["id"] if args.account else None)
            print_accounts(store, data)
        elif c in ("switch", "use"):
            cmd_switch(store, args)
        elif c in ("current", "whoami"):
            with store.lock():
                data = store.load()
                acc = snapshot_live(store, data, create_if_unknown=False)
                store.save(data)
            if acc:
                print(f"{acc['label']}（{acc.get('email') or '-'}）id={acc['id']}")
            else:
                oa = read_oauth_account()
                print("当前登录的账号还没保存，运行 cam import" if oa else "Claude Code 当前未登录")
        elif c == "add-web":
            cmd_add_web(store, args)
        elif c == "open":
            acc = find_account(store.load(), args.account)
            open_web_account(store, acc)
            print(f"已打开 {acc['label']} 的浏览器窗口")
        elif c == "add-api":
            cmd_add_api(store, args)
        elif c == "set":
            cmd_set(store, args)
        elif c in ("rm", "remove"):
            acc, d = remove_account(store, args.account)
            print(f"已删除 {acc['label']}，凭据已移到 {d}")
        elif c == "local":
            data = store.load()
            names = {a["id"]: a["label"] for a in data["accounts"]}
            rep = local_usage(data, args.days)
            if not rep["rows"]:
                print("最近没有本地使用记录")
            for r in rep["rows"]:
                tot = r["input"] + r["output"] + r["cache_write"]
                print(f"{r['date']}  {names.get(r['account_id'], '未归属'):<16} 消息 {r['messages']:>5}  "
                      f"输入 {r['input']:>10,}  输出 {r['output']:>9,}  缓存写 {r['cache_write']:>10,}  "
                      f"缓存读 {r['cache_read']:>12,}  合计(不含缓存读) {tot:>11,}")
        elif c == "serve":
            serve(store, "0.0.0.0" if args.lan else "127.0.0.1", args.port, args.refresh_every)
        elif c == "doctor" or (c == "env" and args.env_cmd in (None, "doctor", "check")):
            cmd_doctor(store)
        elif c == "env" and args.env_cmd == "install":
            cmd_env_install(store, args)
        elif c == "env" and args.env_cmd == "clean":
            cmd_env_clean(store, args)
        elif c == "rule":
            cmd_rule(store, args)
        elif c == "json":
            print(json.dumps(public_state(store, store.load()), ensure_ascii=False, indent=2))
        return 0
    except CamError as e:
        print(color("错误：", "r") + str(e), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
