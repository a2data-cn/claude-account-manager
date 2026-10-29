#!/usr/bin/env python3
"""cam — Claude 多账号管理（脚本版）

只依赖 Python 3.8+ 标准库，macOS / Windows / Linux 通用。
与桌面版共享数据目录 ~/.claude-accounts（可用 CAM_HOME 覆盖）。

常用命令：
  cam import [--label 名字]        把当前 Claude Code 登录的账号存进来
  cam login [名字]                 登录另一个账号并存进来（先存好当前账号，再 claude auth login）
  cam ls                           列出所有账号和用量
  cam switch <账号>                切换 Claude Code 账号（id / 序号 / 名字 / 邮箱）
  cam usage                        刷新并显示用量
  cam set <账号> --expires 2026-11-03 --auto-renew
  cam add-web --label 名字         添加网页版账号（独立浏览器 Profile）
  cam open <账号>                  打开网页版账号窗口
  cam add-api --label 名字 --key sk-ant-... [--admin-key sk-ant-admin...]
  cam serve [--lan] [--port 8787]  启动 Web 面板（手机也能看）
  cam panel                        在浏览器打开 Web 面板（没在运行就在后台启动，只限本机）
  cam local [--days 30]          本地 Claude Code token 消耗统计
  cam env install                  用官方安装器安装 Claude Code
  cam env clean [--logout|--purge] 清理 Claude Code 环境（默认只预演，加 --apply 执行）
  cam update [--check]             手动更新（git 快进；更新后自检不过就退回原来的版本）
  cam verify                       按 SHA256SUMS 校验代码文件是否完整
  cam doctor [--json] [--offline]  全面体检：账号登录、Claude 服务、本机 Claude Code、cam 自己、聊天机器人
"""
from __future__ import annotations

import argparse
import base64
import calendar
import contextlib
import datetime as dt
import getpass
import glob
import hashlib
import http.client
import http.server
import io
import json
import math
import os
import platform
import re
import secrets as pysecrets
import shutil
import signal
import socket
import socketserver
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor

if os.name == "nt":
    import msvcrt
else:
    import fcntl

VERSION = "1.3.1"

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
    except PermissionError:
        raise CamError(*unreadable_message(path))
    except json.JSONDecodeError as e:
        raise CamError(f"文件损坏，无法解析：{path}（{e}）")


def read_bytes(path):
    try:
        with open(path, "rb") as f:
            return f.read()
    except FileNotFoundError:
        return None
    except PermissionError:
        raise CamError(*unreadable_message(path))


def unreadable_message(path):
    """没有权限读某个文件时的说明和修法：属主是谁、权限多少、当前用户是谁。

    常见原因是用 sudo 跑过 claude，~/.claude.json 之类的文件就归了 root；属主不是自己时给 chown，是自己时给 chmod。
    返回 (说明, 怎么办)，正好是 CamError 的两个参数。
    """
    me = getpass.getuser()
    try:
        st = os.stat(path)
    except OSError:
        return f"没有权限读 {path}（当前用户 {me}）", f"ls -l {path} 看看它的属主和权限"
    mode = f"0{st.st_mode & 0o777:o}"
    owner = None
    if not IS_WIN:
        try:
            import pwd
            owner = pwd.getpwuid(st.st_uid).pw_name
        except (ImportError, KeyError):
            owner = str(st.st_uid)
    if owner is not None and owner != me:
        return (f"没有权限读 {path}（属主 {owner}，权限 {mode}；当前用户 {me}）",
                f"这个文件应该归你，用 sudo 跑过 claude 会变成 root 的：sudo chown {me} {path}")
    return f"没有权限读 {path}（权限 {mode}，当前用户 {me}）", f"chmod u+rw {path}"


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
    """给用户看的错误。advice 是“→ 怎么办”，体检和命令行都会单独显示。"""

    def __init__(self, msg, advice=""):
        super().__init__(msg)
        self.advice = advice


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
            r = subprocess.run(["powershell", "-NoProfile", "-Command", ps], capture_output=True, timeout=15)
            r.stdout = decode_output(r.stdout)
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


SWITCH_RENEW_SECS = 300  # Claude Code 在 access token 过期前 5 分钟就自己续；切换前按同一个标准先替目标账号续好


def switch_renew_error(label, err):
    """切换前替目标账号续登录没成功时给用户看的话：说清楚没切、当前登录没动、怎么办。"""
    if "登录已失效" in str(err):
        return (f"没有切换：“{label}”保存的登录已失效（refresh token 被服务端拒绝），切过去 Claude Code 也用不了。"
                "当前登录没有动。用这个账号重新登录一次再切换：运行 cam login（或在 claude 里 /login 后运行 cam import）")
    return f"没有切换：“{label}”保存的登录过期了，切换前要先续一下，但{err}。当前登录没有动"


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
        if (ob.get("expiresAt") or 0) <= (time.time() + SWITCH_RENEW_SECS) * 1000:
            # 第 3 步：目标账号保存的登录过期了，先用它自己的 refresh token 续上再切（它不是当前登录，没人跟它抢）。
            # 续不上就不切，什么都不写：refresh token 作废了的话，切过去 Claude Code 也用不了，还把能用的当前登录换掉了
            try:
                ob = renew_saved_login(store, target)
            except CamError as e:
                if "登录已失效" in str(e) and (data.get("pending_switch") or {}).get("account") == target["id"]:
                    data.pop("pending_switch", None)  # 等下去也切不过去，登记作废
                store.save(data)  # 快照可能刚导入了新账号、回写了当前账号的 token，先存下
                raise CamError(switch_renew_error(target["label"], e))
        backup = backup_live(store, f"before-{target['id']}")  # 第 4 步
        live, where = read_live_credentials()
        # 只换 claudeAiOauth：凭据里的其它内容（例如 mcpOAuth，MCP 服务器的登录）保持现在的样子，
        # 不拿目标账号当初保存的旧副本整个覆盖
        creds = dict(live) if isinstance(live, dict) else {}
        creds["claudeAiOauth"] = ob
        # 第 5 步：先写 ~/.claude.json 再写凭据，凭据写失败就把 oauthAccount 改回去。
        # 万一中途崩溃，凭据还是原来那个号的，再切一次就能完成；反过来的话会被当成“已经是当前账号”。
        prev_oa = read_oauth_account()
        write_oauth_account(secret.get("oauthAccount"))
        try:
            write_live_credentials(creds, where)
        except BaseException:
            try:
                write_oauth_account(prev_oa)
            except Exception:  # noqa: BLE001  回滚失败时保留原来的错误
                pass
            raise
        data.pop("live_mismatch", None)  # 凭据和 oauthAccount 现在是同一个账号的
        data.pop("live_unverified", None)  # 现在生效的登录是我们刚写的，当然认识
        data.pop("pending_switch", None)  # 这次切换优先于之前登记的等待切换
        data["active"]["claude_code"] = target["id"]  # 第 6 步
        target["last_switched_at"] = now_iso()
        data["switch_log"].append({"id": target["id"], "at": now_iso()})
        note_account_region(data, target["id"], cached_region_loc(data), data.get("region"))  # 给风险提示和诊断记一笔地区
        store.save(data)
        return target, backup, True


# --------------------------------------------------------------------------- 网络

# 默认证书一张都没加载到时，按顺序试这些（macOS 自带 /etc/ssl/cert.pem；后面是各 Linux 发行版和 Homebrew 的位置）
CA_FALLBACKS = ("/etc/ssl/cert.pem", "/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/ca-bundle.pem", "/opt/homebrew/etc/openssl@3/cert.pem", "/usr/local/etc/openssl@3/cert.pem",
                "/usr/local/etc/openssl/cert.pem")


def make_ssl_context():
    """返回 (SSLContext, 改用的证书文件或 None)。

    python.org 装的 Python（macOS 常见）默认不带根证书，所有 https 都会 CERTIFICATE_VERIFY_FAILED，
    要手动运行一次 Install Certificates.command。这里在默认证书一张都没加载到时，依次改用 SSL_CERT_FILE、
    certifi（装了的话）和系统自带的证书文件，开箱即用。证书照常校验，不会关掉校验。
    """
    ctx = ssl.create_default_context()
    if ctx.cert_store_stats().get("x509_ca"):
        return ctx, None
    paths = ssl.get_default_verify_paths()
    try:  # 按目录存放的证书要用到时才读进来，统计是 0 也可能是好的
        if paths.capath and os.path.isdir(paths.capath) and os.listdir(paths.capath):
            return ctx, None
    except OSError:
        pass
    cands = [os.environ.get("SSL_CERT_FILE")]
    try:
        import certifi  # noqa: PLC0415  不是必需的依赖
        cands.append(certifi.where())
    except ImportError:
        pass
    for f in cands + list(CA_FALLBACKS):
        if f and os.path.isfile(f):
            try:
                ctx.load_verify_locations(cafile=f)
                return ctx, f
            except (ssl.SSLError, OSError):
                continue
    return ctx, None


SSL_CTX, SSL_CA_FALLBACK = make_ssl_context()


def is_cert_error(msg):
    return "CERTIFICATE_VERIFY_FAILED" in (msg or "")


CERT_ADVICE = ("本机 Python 不认识 HTTPS 证书（python.org 版 Python 常见）。运行一次 "
               f'open "/Applications/Python {sys.version_info.major}.{sys.version_info.minor}/Install Certificates.command"'
               "，或设置环境变量 SSL_CERT_FILE 指向证书文件")


def http_json(method, url, headers=None, body=None, timeout=20):
    h = {"User-Agent": USER_AGENT, "Accept": "application/json"}
    h.update(headers or {})
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout, context=SSL_CTX) as r:
            txt = r.read().decode("utf-8", "replace")
            return r.status, (json.loads(txt) if txt.strip() else {})
    except urllib.error.HTTPError as e:
        txt = e.read().decode("utf-8", "replace")
        try:
            return e.code, json.loads(txt)
        except json.JSONDecodeError:
            return e.code, {"error": {"message": txt[:200]}}
    except (urllib.error.URLError, socket.timeout, OSError, http.client.HTTPException, ValueError) as e:
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
        ob = renew_saved_login(store, acc)
        store.save(data)
        return ob["accessToken"], ob


def renew_saved_login(store, acc):
    """用 acc 自己保存的 refresh token 换一个新的 access token，存回它名下，返回新的 oauth 块。

    调用方持有数据锁，并且已经确认 acc 不是当前生效的登录（没有 Claude Code 跟它抢这个 refresh token），
    还要负责保存 data（acc 的 token_fp 改了）。refresh token 已作废或连不上时抛 CamError（见 refresh_oauth）。
    """
    secret = store.load_secret(acc["id"])
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
    store.save_secret(acc["id"], secret)
    acc["token_fp"] = token_fp(ob["refreshToken"])
    return ob


def pick_window(body, key):
    w = body.get(key)
    if not isinstance(w, dict):
        return None
    u = w.get("utilization")
    if u is None:
        return None
    return {"utilization": float(u), "resets_at": w.get("resets_at")}


def scoped_windows(body):
    """单独计算的周额度，例如“7 天 · Fable”。接口在 limits 里按模型（或产品）给出。"""
    out = []
    for lim in body.get("limits") or []:
        if not isinstance(lim, dict) or lim.get("kind") != "weekly_scoped":
            continue
        scope = lim.get("scope") or {}
        name = ((scope.get("model") or {}).get("display_name") or (scope.get("surface") or {}).get("display_name"))
        pct = lim.get("percent")
        if not name or isinstance(pct, bool) or not isinstance(pct, (int, float)):
            continue
        out.append({"name": str(name), "utilization": float(pct), "resets_at": lim.get("resets_at")})
    return out


def week_breakdown(body):
    """7 天额度花在了哪里：Claude Code / 网页聊天 / Cowork…，只留占比大于 0 的。"""
    b = body.get("seven_day_breakdown")
    out = []
    for r in (b.get("rows") if isinstance(b, dict) else None) or []:
        pct = r.get("percent") if isinstance(r, dict) else None
        if isinstance(pct, bool) or not isinstance(pct, (int, float)) or pct <= 0:
            continue
        out.append({"key": r.get("key"), "name": r.get("display_name") or r.get("key"), "percent": float(pct)})
    return out


def parse_usage_body(body, source):
    """把 /api/oauth/usage（或 claude.ai 的 usage）响应整理成存下来的格式。"""
    usage = {"source": source}
    for k in ("five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"):
        usage[k] = pick_window(body, k)
    models = scoped_windows(body)
    if models:
        usage["models"] = models
        # 新接口里 Opus / Sonnet 的周额度也在 limits 里，别显示两遍
        names = {m["name"].lower() for m in models}
        for k, n in (("seven_day_opus", "opus"), ("seven_day_sonnet", "sonnet")):
            if n in names:
                usage[k] = None
    parts = week_breakdown(body)
    if parts:
        usage["breakdown"] = parts
    extra = body.get("extra_usage")
    if isinstance(extra, dict) and extra.get("is_enabled"):
        usage["extra_usage"] = {k: extra.get(k) for k in ("monthly_limit", "used_credits", "utilization")}
    return usage


# 按 API 标价折算的价格（美元 / 百万 token）：输入、输出、缓存读取。缓存写入是输入价的 1.25 倍（5 分钟）
# 或 2 倍（1 小时）。订阅账号不按这个收费，这里只用来比较用量大小、推算额度窗口。按前缀匹配，长的在前。
PRICES = (
    ("fable-5-1", 10, 50, 0.25), ("mythos-5-1", 10, 50, 0.25), ("fable-5", 10, 50, 1.0), ("mythos-5", 10, 50, 1.0),
    ("opus-5-5", 4, 20, 0.20), ("opus-5", 5, 25, 0.50), ("opus-4-8", 5, 25, 0.50), ("opus-4-7", 5, 25, 0.50),
    ("opus-4-6", 5, 25, 0.50), ("opus-4-5", 5, 25, 0.50), ("opus-4", 15, 75, 1.50),
    ("sonnet-5", 2, 10, 0.20), ("sonnet-4", 3, 15, 0.30), ("haiku-5-5", 0.10, 0.50, 0.01), ("haiku-4-5", 1, 5, 0.10),
)


def message_cost(model, u):
    """一条消息按 API 价折算多少美元；不认识的模型返回 None。"""
    m = (model or "").lower()
    i = m.find("claude-")
    m = m[i + 7:] if i >= 0 else m
    rate = next((r for r in PRICES if m.startswith(r[0])), None)
    if not rate:
        return None
    n = lambda k: int(u.get(k) or 0)
    cc = u.get("cache_creation") if isinstance(u.get("cache_creation"), dict) else None
    if cc:
        w5, w1 = int(cc.get("ephemeral_5m_input_tokens") or 0), int(cc.get("ephemeral_1h_input_tokens") or 0)
    else:
        w5, w1 = n("cache_creation_input_tokens"), 0
    inp, out, cr = rate[1], rate[2], rate[3]
    # Haiku 5.5 的提示超过 10 万 token 时整条按 5 倍价
    if rate[0] == "haiku-5-5" and n("input_tokens") + n("cache_read_input_tokens") + w5 + w1 > 100_000:
        inp, out, cr = inp * 5, out * 5, cr * 5
    return (n("input_tokens") * inp + n("output_tokens") * out + n("cache_read_input_tokens") * cr
            + w5 * inp * 1.25 + w1 * inp * 2) / 1e6


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
    usage = parse_usage_body(body, "oauth")
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
        with urllib.request.urlopen(req, timeout=timeout, context=SSL_CTX) as r:
            raw = r.read().decode("utf-8", "replace")
            code = r.status
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", "replace")
        code = e.code
    except (urllib.error.URLError, socket.timeout, OSError, http.client.HTTPException, ValueError) as e:
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
    if org.get("rate_limit_tier"):
        acc["rate_limit_tier"] = org["rate_limit_tier"]

    # 账号邮箱，失败不影响用量
    code_a, acct, _ = web_get("/api/account", sk)
    if code_a == 200 and isinstance(acct, dict):
        email = acct.get("email_address") or (acct.get("account") or {}).get("email_address")
        if email:
            acc["email"] = email

    code, body, raw = web_get(f"/api/organizations/{org['uuid']}/usage", sk)
    if code != 200 or not isinstance(body, dict):
        raise CamError(web_error(code, body, raw))
    return parse_usage_body(body, "claude.ai")


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


_DESKTOP_CACHE_LIMIT = 64 * 1024
_DESKTOP_USAGE_LIMIT = 4 * 1024 * 1024
_DESKTOP_USAGE_URL = re.compile(r"(?:1/0/)?https://claude\.ai/api/organizations/"
                              r"([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})"
                              r"/usage")
_DESKTOP_HISTORY_KEYS = {"fh": "five_hour", "sd": "seven_day", "so": "seven_day_opus", "sn": "seven_day_sonnet",
                         "oa": "seven_day_oauth_apps", "cw": "seven_day_cowork", "om": "seven_day_omelette",
                         "op": "omelette_promotional"}
_DESKTOP_WINDOWS = tuple(_DESKTOP_HISTORY_KEYS.values())


def _desktop_usage_result(source, status, notice, **fields):
    return {"source": source, "cache_status": status, "notice": notice, "checked_at": now_iso(),
            "fetched_at": None, "cache_org_uuid": None, "cache_stale": status != "ok",
            **{key: None for key in _DESKTOP_WINDOWS}, **fields}


def _desktop_percent(value):
    return (isinstance(value, (int, float)) and not isinstance(value, bool)
            and 0 <= value <= 100 and math.isfinite(value))


def _desktop_uuid(value):
    return (value.lower() if isinstance(value, str) and
            re.fullmatch(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}", value) else None)


def _desktop_decode_usage(payload):
    if payload.startswith(b"\x28\xb5\x2f\xfd"):
        try:
            try:
                from compression import zstd
            except ImportError:
                import zstandard
                with zstandard.ZstdDecompressor().stream_reader(io.BytesIO(payload), read_across_frames=False) as reader:
                    payload = reader.read(_DESKTOP_USAGE_LIMIT + 1)
            else:
                decoder = zstd.ZstdDecompressor()
                payload = decoder.decompress(payload, max_length=_DESKTOP_USAGE_LIMIT + 1)
                if not decoder.eof:
                    raise ValueError("zstd 响应不完整或超过读取上限")
        except ImportError as e:
            raise CamError("unsupported_compression") from e
    if len(payload) > _DESKTOP_USAGE_LIMIT:
        raise ValueError("桌面版用量缓存超过读取上限")
    raw = payload.decode("utf-8", "replace").lstrip()
    # Chromium 在 body 后附加二进制 EOF 和元数据；只解析第一个 JSON 对象。
    parsed, _ = json.JSONDecoder().raw_decode(raw)
    if not isinstance(parsed, dict) or not any(key in parsed for key in _DESKTOP_WINDOWS):
        raise ValueError("不是额度响应")
    return parsed


def _desktop_cached_usage(directory, org_hint):
    now = dt.datetime.now(dt.timezone.utc)
    entries = []
    unsupported = False
    cache = os.path.join(directory, "Cache", "Cache_Data")
    for path in glob.glob(os.path.join(cache, "*_0")):
        try:
            with open(path, "rb") as f:
                blob = f.read(_DESKTOP_CACHE_LIMIT + 1)
            if len(blob) > _DESKTOP_CACHE_LIMIT or len(blob) < 24:
                continue
            if blob[:8] != bytes.fromhex("305c72a71b6dfbfc"):
                continue
            key_len = struct.unpack_from("<I", blob, 12)[0]
            if not key_len or key_len > 8192 or key_len >= len(blob) - 24:
                continue
            key = blob[24:24 + key_len].decode("utf-8", "strict")
            match = _DESKTOP_USAGE_URL.fullmatch(key)
            if not match:
                continue
            org = match.group(1).lower()
            if org_hint and org != org_hint:
                continue
            sampled = dt.datetime.fromtimestamp(os.path.getmtime(path), dt.timezone.utc)
            if sampled.timestamp() <= 0 or sampled > now + dt.timedelta(minutes=5):
                continue
            try:
                body = _desktop_decode_usage(blob[24 + key_len:])
            except CamError as e:
                unsupported = unsupported or str(e) == "unsupported_compression"
                continue
            if not any(isinstance(body.get(k), dict) and _desktop_percent(body[k].get("utilization")) and
                       (body[k].get("resets_at") is None or parse_iso(body[k].get("resets_at"))) for k in _DESKTOP_WINDOWS):
                continue
            entries.append((sampled, org, body))
        except (OSError, UnicodeError, ValueError, OverflowError, struct.error):
            continue
        except Exception:
            # 损坏的单个 Chromium cache entry 不应中断其它条目或其它账号。
            continue
    if not entries:
        return _desktop_usage_result("desktop_cache", "unsupported_compression" if unsupported else "missing",
                                     "未能解压桌面版用量缓存，请安装 scripts/requirements-desktop.txt。" if unsupported else
                                     "还没有桌面版额度缓存，请打开对应账号的 Claude，在设置中查看用量后刷新。")
    if len({org for _, org, _ in entries}) > 1 and not org_hint:
        return _desktop_usage_result("desktop_cache", "ambiguous", "发现多个组织的额度缓存，无法确认当前组织；请在 Claude 中查看目标组织的用量。")
    sampled, org, body = max(entries, key=lambda row: row[0])
    windows = {}
    for key in _DESKTOP_WINDOWS:
        window = body.get(key)
        if not isinstance(window, dict) or not _desktop_percent(window.get("utilization")):
            windows[key] = None
            continue
        resets = parse_iso(window.get("resets_at"))
        without_reset = (window.get("resets_at") is None and
                         (now - sampled).total_seconds() < (5 * 3600 if key == "five_hour" else 7 * 86400))
        windows[key] = ({"utilization": window["utilization"], "resets_at": window.get("resets_at")}
                        if (resets and resets > now) or without_reset else None)
    available = any(windows.values())
    return _desktop_usage_result("desktop_cache", "ok" if available else "expired",
                                 "来自 Claude 桌面版的本地额度缓存；在客户端查看用量后可更新。" if available else
                                 "桌面版额度缓存已过重置时间，请在 Claude 设置中查看用量后刷新。",
                                 fetched_at=sampled.isoformat(), cache_org_uuid=org,
                                 cache_stale=(now - sampled).total_seconds() > 1800, **windows)


def _desktop_history_usage(directory, org_hint):
    path = os.path.join(directory, "plan-usage-history.json")
    try:
        with open(path, "rb") as f:
            raw = f.read(_DESKTOP_USAGE_LIMIT + 1)
        if len(raw) > _DESKTOP_USAGE_LIMIT:
            return None
        history = json.loads(raw)
        version = history.get("version")
        if isinstance(version, bool) or version not in (1, 2) or not isinstance(history.get("samples"), list):
            return None
    except (OSError, UnicodeError, ValueError, AttributeError):
        return None
    now = dt.datetime.now(dt.timezone.utc)
    samples = []
    for sample in history["samples"]:
        if not isinstance(sample, dict):
            continue
        timestamp = sample.get("t")
        if (not isinstance(timestamp, (int, float)) or isinstance(timestamp, bool)
                or not 0 < timestamp <= (now.timestamp() + 300) * 1000 or not math.isfinite(timestamp)):
            continue
        try:
            sampled = dt.datetime.fromtimestamp(timestamp / 1000, dt.timezone.utc)
        except (ValueError, OverflowError, OSError):
            continue
        if timestamp <= 0 or sampled > now + dt.timedelta(minutes=5):
            continue
        raw_org = sample.get("org") if version == 2 else None
        org = _desktop_uuid(raw_org)
        if raw_org is not None and org is None:
            continue
        if org_hint and org != org_hint:
            continue
        values = sample.get("u") if version == 2 else sample
        if not isinstance(values, dict):
            continue
        windows = {}
        for short, key in _DESKTOP_HISTORY_KEYS.items():
            age_limit = 5 * 3600 if short == "fh" else 7 * 86400
            value = values.get(short)
            windows[key] = ({"utilization": value, "resets_at": None}
                            if _desktop_percent(value) and (now - sampled).total_seconds() < age_limit else None)
        if any(_desktop_percent(values.get(short)) for short in _DESKTOP_HISTORY_KEYS):
            samples.append((sampled, org, windows))
    if not samples:
        return None
    if len({org for _, org, _ in samples if org}) > 1 and not org_hint:
        return _desktop_usage_result("desktop_history", "ambiguous", "用量历史包含多个组织，无法确认当前组织；请打开 Claude 查看用量。")
    sampled, org, windows = max(samples, key=lambda row: row[0])
    available = any(windows.values())
    return _desktop_usage_result("desktop_history", "ok" if available else "expired",
                                 "来自 Claude 桌面版的历史采样，未提供重置时间；请以客户端当前用量为准。" if available else
                                 "本地额度历史已过期，请在 Claude 设置中查看用量后刷新。",
                                 fetched_at=sampled.isoformat(), cache_org_uuid=org,
                                 cache_stale=(now - sampled).total_seconds() > 1800, **windows)


def fetch_desktop_usage(store, acc):
    directory = desktop_profile_dir(store, acc)
    try:
        config = read_json(os.path.join(directory, "config.json"), {}) or {}
    except (CamError, OSError, UnicodeError):
        return _desktop_usage_result("desktop_cache", "missing", "桌面配置暂时无法读取，请打开 Claude 确认登录后刷新。")
    if not isinstance(config, dict):
        return _desktop_usage_result("desktop_cache", "missing", "桌面配置格式无法识别，请打开 Claude 确认登录后刷新。")
    actual = config.get("lastKnownAccountUuid") if isinstance(config, dict) else None
    expected = acc.get("desktop_account_uuid")
    if expected and actual and actual != expected:
        return _desktop_usage_result("desktop_cache", "missing", "这个桌面配置已登录为另一个账号，未读取旧额度缓存；请确认账号对应关系。")
    if actual and isinstance(actual, str):
        acc["desktop_account_uuid"] = actual
    # lastActiveOrg 是加密 Cookie，不猜不存在的 config org 字段。
    org_hint = _desktop_uuid(acc.get("org_uuid"))
    cached = _desktop_cached_usage(directory, org_hint)
    if cached["cache_status"] == "ambiguous":
        return cached
    history = _desktop_history_usage(directory, org_hint)
    if history and history["cache_status"] == "ambiguous":
        return history
    if (not org_hint and history and cached.get("cache_org_uuid") and history.get("cache_org_uuid") and
            cached["cache_org_uuid"] != history["cache_org_uuid"]):
        return _desktop_usage_result("desktop_cache", "ambiguous", "本地缓存和历史记录属于不同组织，无法确认当前组织；请在 Claude 中确认目标组织。")
    if cached["cache_status"] in ("ok", "expired"):
        # 已过重置时间的较新响应不能被旧历史重新显示为可用额度。
        if (history and history["cache_status"] in ("ok", "expired") and
                parse_iso(history.get("fetched_at")) > parse_iso(cached.get("fetched_at"))):
            return history
        return cached
    return history or cached


def _desktop_usage_view(usage):
    """只过滤对外显示，保留存档的原采样时间；跨过重置时间后不能继续展示旧百分比。"""
    shown = dict(usage)
    now = dt.datetime.now(dt.timezone.utc)
    sampled = parse_iso(usage.get("fetched_at"))
    age = (now - sampled).total_seconds() if sampled else float("inf")
    shown["cache_stale"] = age > 1800
    had_windows = any(usage.get(k) for k in _DESKTOP_WINDOWS)
    for key in _DESKTOP_WINDOWS:
        window = usage.get(key)
        if not isinstance(window, dict) or not _desktop_percent(window.get("utilization")):
            shown[key] = None
            continue
        resets = parse_iso(window.get("resets_at"))
        lifetime = 5 * 3600 if key == "five_hour" else 7 * 86400
        valid = resets > now if resets else window.get("resets_at") is None and age < lifetime
        shown[key] = window if valid else None
    if had_windows and not any(shown.get(k) for k in _DESKTOP_WINDOWS):
        shown["cache_status"] = "expired"
        shown["notice"] = "本地额度快照已过期，请在 Claude 设置中查看用量后刷新。"
    return shown


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
            elif acc["kind"] == "claude_desktop":
                u = fetch_desktop_usage(store, acc)
            else:
                u = fetch_api_usage(store, acc)
            u["error"] = None
        except CamError as e:
            u = dict(acc.get("usage") or {})
            u["error"] = str(e)
        except Exception as e:  # 不让单个账号拖垮整体
            u = dict(acc.get("usage") or {})
            u["error"] = f"{type(e).__name__}: {e}"
        if acc["kind"] != "claude_desktop":
            u["fetched_at"] = now_iso()
        meta = {k: acc.get(k) for k in ("email", "plan", "org_name", "rate_limit_tier", "auto", "account_uuid", "desktop_account_uuid")
                if acc.get(k)}
        results[acc["id"]] = (u, meta)

    threads = [threading.Thread(target=work, args=(a,)) for a in targets]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    # 本机记录里每个 Claude Code 号当前窗口用了多少，和刚拉到的百分比对照着看
    fetched = {aid: u for aid, (u, _) in results.items()
               if not u.get("error") and next((a["kind"] for a in targets if a["id"] == aid), None) == "claude_code"}
    try:
        local = local_window_usage(data, fetched)
    except Exception:  # 统计失败不影响用量本身
        local = {}
    for aid, u in fetched.items():
        u["local"] = local.get(aid)
    with store.lock():
        fresh = store.load()
        for a in fresh["accounts"]:
            if a.get("kind") == "claude_desktop" and (a.get("usage") or {}).get("source") not in ("desktop_cache", "desktop_history"):
                # 旧版本误把桌面账号交给 API 用量读取器；桌面登录目录不提供此接口。
                a["usage"] = None
            if a["id"] in results:
                u, meta = results[a["id"]]
                note_incident(fresh, a, u)
                a["usage"] = u
                a.update(meta)
        roll_renewals(fresh)
        store.save(fresh)
        return fresh


INCIDENT_KINDS = ("wrong_owner", "login_invalid", "session_invalid", "key_invalid")  # 这几种算“账号出事了”，要记现场
INCIDENT_KEEP = 10


def note_incident(data, acc, new_usage):
    """账号刚出问题的那一刻把现场记下来：出口（IP、地区、运营商、IP 类型）、本机环境（时区、语言、代理、VPN、DNS）、
    几个 claude 会话在跑、环境评分。同一种问题持续着不重复记；每个号留最近 INCIDENT_KEEP 条。给“导出诊断”用，只记录不干预。"""
    err = (new_usage or {}).get("error")
    kind = _health_kind_of_error(acc.get("kind"), err) if err else None
    if kind not in INCIDENT_KINDS:
        return
    prev = (acc.get("usage") or {}).get("error")
    if prev and _health_kind_of_error(acc.get("kind"), prev) == kind:
        return
    reg = data.get("region") or {}
    env = reg.get("env") or {}
    try:
        score = risk_score(risk_items(data))["score"]
    except Exception:  # noqa: BLE001  评分算不出来也要记
        score = None
    acc.setdefault("incidents", []).insert(0, {
        "at": now_iso(), "kind": kind, "error": err,
        "region": {k: reg.get(k) for k in ("loc", "ip", "org", "city", "ip_type", "warp", "colo", "skew", "lookup_mode", "checked_at")},
        "env": {k: env.get(k) for k in ("tz", "locale", "dns", "proxy", "tunnel")},
        "sessions": session_count(), "active": (data.get("active") or {}).get("claude_code") == acc["id"], "risk_score": score})
    acc["incidents"] = acc["incidents"][:INCIDENT_KEEP]


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
PENDING_SWITCH_HOURS = 6  # 手动登记的等待切换放多久：过了还没等到会话结束就作废，免得几小时后突然换号


def pending_switch_expired(p, now=None):
    """手动登记的等待切换放太久了。时间规则的登记跟着时间段作废，不看这个。"""
    if p.get("reason") == "rule":
        return False
    at = parse_iso(p.get("at"))
    now = now or dt.datetime.now(dt.timezone.utc)
    return at is None or now - at > dt.timedelta(hours=PENDING_SWITCH_HOURS)


def pending_switch_view(data):
    """登记中的“等会话结束再切换”，给界面显示用；没有、已过期或目标账号已删除则 None。"""
    p = data.get("pending_switch")
    if not p or pending_switch_expired(p):
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

    返回 (账号, 备份目录, reason)，没切则 None。目标账号没了、已经是当前账号、手动登记放了 PENDING_SWITCH_HOURS
    小时还没等到、或时间规则的时间段已经过了，就作废（过期的不等会话退出，马上清掉）。
    切换本身走 switch_code_account，它会清掉登记。
    """
    data = store.load()
    p = data.get("pending_switch")
    if not p:
        return None
    if not pending_switch_expired(p) and running_claude_processes():
        return None
    with store.lock():
        data = store.load()
        p = data.get("pending_switch")
        if not p:
            return None
        known = any(a["id"] == p.get("account") for a in data["accounts"])
        drop = not known or data["active"].get("claude_code") == p["account"] or pending_switch_expired(p)
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


RISK_PROMPT_DEFAULTS = {"risk_prompt": True, "risk_confirm_below": 60, "risk_strong_below": 30}


def risk_prefs(data):
    p = dict(RISK_PROMPT_DEFAULTS)
    p.update({k: v for k, v in (data.get("prefs") or {}).items() if k in RISK_PROMPT_DEFAULTS})
    return p


def save_risk_prefs(store, prompt=None, confirm=None, strong=None):
    with store.lock():
        data = store.load()
        prefs = data.setdefault("prefs", {})
        cur = risk_prefs(data)
        confirm = cur["risk_confirm_below"] if confirm is None else confirm
        strong = cur["risk_strong_below"] if strong is None else strong
        for name, v in (("confirm", confirm), ("strong", strong)):
            if not isinstance(v, int) or not 0 <= v <= 100:
                raise CamError(f"{name} 阈值应为 0–100 的整数")
        if strong > confirm:
            raise CamError("strong 阈值不能高于 confirm 阈值")
        prefs["risk_confirm_below"], prefs["risk_strong_below"] = confirm, strong
        if prompt is not None:
            prefs["risk_prompt"] = bool(prompt)
        store.save(data)
        return risk_prefs(data)


def launch_tier(score, region_status_, prefs):
    """启动前的提醒档位：ok（一行带过）/ remind（打印提醒）/ confirm（问 y/N）/ strong（要输入“继续”）。
    出口不在支持名单内时不看分数，直接最高档。"""
    if region_status_ == "unsupported" or score < prefs["risk_strong_below"]:
        return "strong"
    if score < prefs["risk_confirm_below"]:
        return "confirm"
    return "remind" if score < 90 else "ok"


def with_quick_region(data, q):
    """把快速检测到的出口套到一份数据副本上（不保存），用来算启动前的提醒。出口 IP 变了时，旧的运营商信息不再适用。"""
    data = json.loads(json.dumps(data))
    reg = dict(data.get("region") or {})
    now = now_iso()
    if reg.get("loc") and q.get("loc") and reg["loc"] != q["loc"]:
        reg["prev_loc"], reg["changed_at"] = reg["loc"], now
    if reg.get("ip") != q.get("ip"):
        for k in META_FIELDS + STACK_FIELDS:
            reg.pop(k, None)
    reg.update({k: q.get(k) for k in ("loc", "ip", "colo", "ms", "warp", "skew")}, checked_at=now)
    data["region"] = reg
    return data


def spawn_background_check():
    """另起一个后台进程做完整的环境检测（cam.py _check-region），不等它。CAM_NO_BG_CHECK=1 时不起。"""
    if os.environ.get("CAM_NO_BG_CHECK"):
        return False
    kw = {"stdin": subprocess.DEVNULL, "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL, "close_fds": True}
    if IS_WIN:
        kw["creationflags"] = 0x00000008 | 0x08000000  # DETACHED_PROCESS | CREATE_NO_WINDOW
    else:
        kw["start_new_session"] = True
    try:
        subprocess.Popen([sys.executable, os.path.abspath(__file__), "_check-region"], **kw)
        return True
    except OSError:
        return False


def background_region_check(store):
    """后台进程的入口：同一时间只跑一个，一分钟内查过就不再查。"""
    lock = FileLock(os.path.join(store.root, "region-check.lock"), timeout=0)
    if not lock.try_acquire():
        return
    try:
        check_region(store, max_age=60)
    finally:
        lock.release()


def privacy_sources(cfg):
    """当前设置下会访问哪些外部服务、各发了什么。只发出口 IP，不发任何账号信息。"""
    def src(name, what, group="third", warn=False):
        return {"name": name, "what": what, "group": group, "warn": warn}

    out = [src("claude.ai/cdn-cgi/trace", "出口国家、IP、时钟。访问 Claude 时它本来就看得到", "anthropic"),
           src("status.anthropic.com", "官方状态页，不带任何本机信息", "anthropic")]
    if cfg["mode"] == "off":
        return out
    if cfg["mode"] == "ip-api":
        out.append(src("ip-api.com（http）", "运营商、城市、机房 / 代理标记。免费接口仅限非商业用途", warn=True))
    out += [src("api.ipapi.is", "运营商、城市" + ("、机房 / VPN / Tor 标记" if cfg["ipapi_key"] else "（没配 key：每天 30 次，不带类型标记）")),
            src("ipinfo.io", "上一家查不到时：运营商、城市"),
            src("speed.cloudflare.com", "再查不到时：运营商、城市"),
            src("rdap.org", "出口 IP 的注册国"),
            src("1.1.1.1 / Cloudflare IPv6", "分别测 IPv4、IPv6 出口"),
            src("www.cloudflare.com", "claude.ai 连不上时，代替它看出口国家")]
    return out


def privacy_view(store):
    cfg = ip_lookup_config(store)
    data = store.load()
    return {"ip_lookup": cfg["mode"],
            "modes": [{"value": k, "label": v, "title": IP_LOOKUP_TITLES[k][0], "desc": IP_LOOKUP_TITLES[k][1]}
                      for k, v in IP_LOOKUP_MODES.items()],
            "env_override": bool(os.environ.get("CAM_IP_LOOKUP")),
            "has_ipapi_key": bool(cfg["ipapi_key"]), "has_ipinfo_token": bool(cfg["ipinfo_token"]),
            "meta_ttl_hours": round(META_TTL / 3600, 1), "sources": privacy_sources(cfg),
            "history": {"accounts": sum(1 for a in data["accounts"] if a.get("regions")),
                        "checked_at": (data.get("region") or {}).get("checked_at")}}


def _drop_meta(data):
    reg = data.get("region")
    if isinstance(reg, dict):
        for k in META_FIELDS + STACK_FIELDS:
            reg.pop(k, None)


def save_privacy(store, body):
    """改外部 IP 查询的设置：{"ip_lookup": auto / ip-api / off, "ipapi_key": "…", "ipinfo_token": "…"}，key 传空字符串表示删除。
    改了数据源或 key，缓存的运营商信息作废，下次检测重新查。规则和 Rust 版 health::save_privacy 一致。"""
    mode = body.get("ip_lookup")
    if mode is not None and mode not in IP_LOOKUP_MODES:
        raise CamError("外部 IP 查询应为 auto / ip-api / off")
    with store.lock():
        data = store.load()
        changed = False
        if mode is not None and mode != (data.get("prefs") or {}).get("ip_lookup", "auto"):
            data.setdefault("prefs", {})["ip_lookup"] = mode
            changed = True
        sec = store.load_secret(LOOKUP_SECRET_ID)
        sec_changed = False
        for k in ("ipapi_key", "ipinfo_token"):
            v = body.get(k)
            if isinstance(v, str):
                v = v.strip()
                if v != (sec.get(k) or ""):
                    if v:
                        sec[k] = v
                    else:
                        sec.pop(k, None)
                    sec_changed = True
        if sec_changed:
            store.save_secret(LOOKUP_SECRET_ID, sec)
        if changed or sec_changed:
            _drop_meta(data)
            store.save(data)
    return privacy_view(store)


def clear_region_history(store):
    """清掉缓存的出口信息和每个账号的使用地区记录。"""
    with store.lock():
        data = store.load()
        data.pop("region", None)
        data.pop("status_page", None)
        for a in data["accounts"]:
            a.pop("regions", None)
        store.save(data)
    return privacy_view(store)


def risk_prompt_before_launch(store, ask=input, interactive=None):
    """启动 claude 前把环境风险打出来；分数低时让用户确认。只提醒、只问，不拦：返回 False 表示用户自己选择了不启动。

    CAM_NO_RISK_PROMPT=1 或 cam risk --off 可关掉。非交互环境（脚本、CI）只打印不问。
    """
    if os.environ.get("CAM_NO_RISK_PROMPT"):
        return True
    data = store.load()
    prefs = risk_prefs(data)
    if not prefs["risk_prompt"]:
        return True
    if not cached_region_loc(data):
        # 一小时内没检测过：完整检测放到后台进程里做，这里只花最多 3 秒问一次 claude.ai 的出口，不拖慢启动
        spawn_background_check()
        q = quick_trace(3)
        if q:
            data = with_quick_region(data, q)
        else:
            print(color("[cam] 3 秒内没测到出口地区，先按上次的结果提醒；完整检测在后台进行", "d"), file=sys.stderr)
    risks = risk_items(data)
    sc = risk_score(risks)
    tier = launch_tier(sc["score"], region_view(data)["status"], prefs)
    err = sys.stderr
    if tier == "ok":
        print(color(f"[cam] 环境评分 {sc['score']}/100：{sc['label']}", "d"), file=err)
        return True
    print(color(f"[cam] 环境评分 {sc['score']}/100：{sc['label']}", "r" if tier == "strong" else "y"), file=err)
    for r in risks:
        if r["level"] in ("warn", "bad"):
            print(color(f"[cam]   ! {r['text']}。{r['advice']}", "y"), file=err)
    print(color("[cam]   cam doctor 查看完整的服务检查；cam risk 调整提醒阈值", "d"), file=err)
    if tier == "remind":
        return True
    if interactive is None:
        interactive = sys.stdin.isatty()
    if not interactive:
        print(color("[cam] 非交互环境，不能确认，照常启动", "d"), file=err)
        return True
    try:
        if tier == "confirm":
            go = ask("[cam] 是否继续启动 claude？[y/N]：").strip().lower() == "y"
        else:
            go = ask("[cam] 不建议在当前环境使用。确认继续请输入「继续」，直接回车取消：").strip() == "继续"
    except (EOFError, KeyboardInterrupt):
        print(file=err)
        go = False
    if not go:
        print(color("[cam] 已取消启动", "d"), file=err)
    return go


def run_claude(store, args):
    """按目录规则切号后启动 claude，参数原样透传。启动前按环境风险提醒并（分数低时）让用户确认。"""
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
    if not risk_prompt_before_launch(store):
        return 1
    if IS_WIN:
        return subprocess.call([exe] + args)
    os.execv(exe, [exe] + args)


# --------------------------------------------------------------------------- 本地用量统计

def local_events(since):
    """本机 Claude Code 记录里每条带用量的消息：(时间, 模型, usage)。同一条消息只算一次。"""
    root = os.path.join(claude_config_dir(), "projects")
    seen = set()
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
                yield ts, msg.get("model") or "", msg["usage"]


def owner_finder(data):
    """按切换记录找某个时刻登录的是哪个号。从没切换过时都算当前账号；最早一次切换之前的算不出来，返回 None。"""
    log = sorted(((parse_iso(e["at"]), e["id"]) for e in data.get("switch_log", []) if parse_iso(e.get("at"))),
                 key=lambda x: x[0])
    active = (data.get("active") or {}).get("claude_code")

    def owner(ts):
        if not log:
            return active
        found = None
        for at, aid in log:
            if at > ts:
                break
            found = aid
        return found
    return owner


def local_usage(data, days=30):
    """本机 Claude Code 记录按天汇总：rows 按 (日期, 账号)，models 按 (日期, 模型)——面板用前者画每天的柱子、后者画按模型的分布。"""
    since = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=days)
    owner_of = owner_finder(data)
    daily, by_model = {}, {}  # (date, acc_id) -> dict；(date, model) -> dict

    def add(table, key, u, cost):
        rec = table.setdefault(key, {"input": 0, "output": 0, "cache_write": 0, "cache_read": 0, "messages": 0, "cost_usd": 0.0})
        rec["input"] += int(u.get("input_tokens") or 0)
        rec["output"] += int(u.get("output_tokens") or 0)
        rec["cache_write"] += int(u.get("cache_creation_input_tokens") or 0)
        rec["cache_read"] += int(u.get("cache_read_input_tokens") or 0)
        rec["messages"] += 1
        rec["cost_usd"] += cost

    for ts, model, u in local_events(since):
        day = ts.astimezone().date().isoformat()
        cost = message_cost(model, u) or 0.0
        add(daily, (day, owner_of(ts) or "unattributed"), u, cost)
        add(by_model, (day, model or "unknown"), u, cost)
    rows = [{"date": d, "account_id": a, **v, "cost_usd": round(v["cost_usd"], 4)} for (d, a), v in sorted(daily.items())]
    models = [{"date": d, "model": m, **v, "cost_usd": round(v["cost_usd"], 4)} for (d, m), v in sorted(by_model.items())]
    return {"days": days, "rows": rows, "models": models}


WINDOW_SECS = {"five_hour": 5 * 3600, "seven_day": 7 * 86400}


def local_window_usage(data, usages, now=None):
    """每个 Claude Code 账号当前额度窗口里，本机记录到的用量和按 API 价折算的金额。

    usages 是 {账号 id: 刚拉到的 usage}。窗口起点 = 重置时间 - 窗口长度（按模型的周额度也是 7 天）。
    界面拿它和接口给的百分比对照，推算整个窗口大约能用多少。返回 {账号 id: {...}}。
    """
    now = now or dt.datetime.now(dt.timezone.utc)
    spans = {}  # 账号 id -> [(名字, 模型名或 None, 起点)]
    for aid, u in usages.items():
        for key, secs in WINDOW_SECS.items():
            r = parse_iso(((u or {}).get(key) or {}).get("resets_at"))
            if r:
                spans.setdefault(aid, []).append((key, None, r - dt.timedelta(seconds=secs)))
        for m in (u or {}).get("models") or []:
            r = parse_iso(m.get("resets_at"))
            if r:
                spans.setdefault(aid, []).append((m["name"], m["name"].lower(), r - dt.timedelta(days=7)))
    if not spans:
        return {}
    owner_of = owner_finder(data)
    out = {aid: {"at": now.isoformat(), "windows": {}} for aid in spans}
    for ts, model, u in local_events(min(start for v in spans.values() for _, _, start in v)):
        aid = owner_of(ts)
        if aid not in spans or ts > now:
            continue
        cost = message_cost(model, u)
        tokens = sum(int(u.get(k) or 0) for k in ("input_tokens", "output_tokens", "cache_creation_input_tokens"))
        for name, family, start in spans[aid]:
            if ts < start or (family and family not in model.lower()):
                continue
            w = out[aid]["windows"].setdefault(name, {"tokens": 0, "cost_usd": 0.0, "messages": 0})
            w["tokens"] += tokens
            w["cost_usd"] += cost or 0.0
            w["messages"] += 1
    for v in out.values():
        for w in v["windows"].values():
            w["cost_usd"] = round(w["cost_usd"], 4)
    return out


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


# --------------------------------------------------------------------------- Claude 桌面版
# 登录不在 Claude Code 的钥匙串里，而在桌面版自己的用户数据目录（加密的 oauth 缓存和 Cookie）。
# 每个账号一份目录。第一次打开时在窗口里点一次邮箱；之后切换会退出当前的 Claude，再用那份目录打开。

def default_desktop_user_data():
    env = os.environ.get("CAM_CLAUDE_DESKTOP_DATA")
    if env:
        return env
    if IS_MAC:
        return os.path.join(os.path.expanduser("~"), "Library", "Application Support", "Claude")
    if IS_WIN:
        return os.path.join(os.environ.get("APPDATA") or os.path.expanduser("~"), "Claude")
    return os.path.join(os.environ.get("XDG_CONFIG_HOME") or os.path.join(os.path.expanduser("~"), ".config"), "Claude")


def desktop_profile_dir(store, acc):
    if acc.get("desktop_profile") == "default":
        return default_desktop_user_data()
    return os.path.join(store.root, "desktop-profiles", acc["id"])


@contextlib.contextmanager
def desktop_operation_lock(store):
    """与 Rust 共用，覆盖退出、备份、启动和移除；始终先于 accounts.lock 获取。"""
    lock = FileLock(os.path.join(store.root, "desktop-switch.lock"), timeout=0)
    if not lock.try_acquire():
        raise CamError("Claude 桌面版正在切换或移除账号，请稍后重试")
    try:
        yield
    finally:
        lock.release()


def _desktop_has_login(path):
    cfg = os.path.join(path, "config.json")
    if os.path.isfile(cfg):
        try:
            with open(cfg, encoding="utf-8") as f:
                data = json.load(f)
            if any(data.get(k) for k in ("oauth:tokenCache", "oauth:tokenCacheV2", "lastKnownAccountUuid")):
                return True
        except (OSError, json.JSONDecodeError, AttributeError):
            pass
    return any(os.path.isfile(os.path.join(path, name)) and os.path.getsize(os.path.join(path, name)) > 0
               for name in ("Cookies", os.path.join("Network", "Cookies")))


def _desktop_candidates():
    if IS_MAC:
        return ["/Applications/Claude.app", os.path.expanduser("~/Applications/Claude.app")]
    if IS_WIN:
        local = os.environ.get("LOCALAPPDATA") or ""
        cands = [os.path.join(local, "Programs", "Claude", "Claude.exe")]
        for base in (os.environ.get("PROGRAMFILES"), os.environ.get("PROGRAMFILES(X86)")):
            if base:
                cands.append(os.path.join(base, "Claude", "Claude.exe"))
        cands += sorted(glob.glob(os.path.join(local, "AnthropicClaude", "app-*", "claude.exe")), reverse=True)
        cands.append(os.path.join(local, "AnthropicClaude", "claude.exe"))
        return cands
    return ["/usr/bin/claude-desktop", "/opt/Claude/claude", "/usr/lib/claude-desktop/claude"]


def _desktop_executable(app):
    return os.path.join(app, "Contents", "MacOS", "Claude") if app.endswith(".app") else app


def _is_desktop_cmd(cmd):
    c = (cmd or "").strip().replace("\\", "/")
    if re.search(r"(?:^|\s)--type(?:=|\s)", c):
        return False  # Electron helper，不把它当成可切换的主进程
    candidates = _desktop_candidates()
    if os.environ.get("CAM_CLAUDE_DESKTOP"):
        candidates.append(os.environ["CAM_CLAUDE_DESKTOP"])
    for app in candidates:
        for executable in (_desktop_executable(app), os.path.realpath(_desktop_executable(app))):
            exe = executable.replace("\\", "/")
            flags = re.IGNORECASE if IS_WIN else 0
            if re.match(r'^"?' + re.escape(exe) + r'"?(?:\s|$)', c, flags):
                return True
    if IS_WIN:
        # 安装版本号不固定；只匹配明确的 Claude 安装目录，不能按 claude.exe 的名字杀进程。
        return bool(re.match(r'^"?[a-z]:/(?:[^"\n]*/)?(?:anthropicclaude/app-[^/]+|'
                             r'program files/windowsapps/anthropicpbc\.claude[^/]+)/claude\.exe"?(?:\s|$)',
                             c, re.IGNORECASE))
    return bool(re.match(r'^"?/[^\n]*?/Claude\.app/Contents/MacOS/Claude"?(?:\s|$)', c)) if IS_MAC else False


def _desktop_profile_from_command(command):
    match = re.search(r'(?:^|\s)--user-data-dir(?:=|\s+)("[^"]+"|\x27[^\x27]+\x27|.*?)(?=\s+--[A-Za-z-]|$)',
                      command or "")
    if not match:
        return None
    path = match.group(1).strip().strip('"\x27')
    return os.path.normcase(os.path.realpath(path)) if path else None


def _desktop_profile_from_files(pid):
    """macOS ps 可能不保留启动参数，用主进程已打开的登录文件核对目录。"""
    try:
        out = subprocess.run(["lsof", "-a", "-p", str(pid), "-Fn"], capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if out.returncode != 0:
        return None
    profiles = set()
    for line in out.stdout.splitlines():
        if not line.startswith("n/"):
            continue
        path = line[1:]
        markers = _LOGIN_FILES + (os.path.join("Local Storage", "leveldb", "LOCK"), os.path.join("Session Storage", "LOCK"))
        for name in sorted(markers, key=len, reverse=True):
            suffix = os.sep + name
            if path.endswith(suffix):
                profiles.add(os.path.normcase(os.path.realpath(path[:-len(suffix)])))
                break
    return next(iter(profiles)) if len(profiles) == 1 else None


def _desktop_processes():
    """返回 [(pid, profile 或 None)]。查不到进程列表时必须报错，不能当作已退出。"""
    if os.environ.get("CAM_DESKTOP_DRY_RUN") == "1":
        return []
    found = []
    try:
        if IS_WIN:
            ps = ("$ErrorActionPreference='Stop'; Get-CimInstance Win32_Process "
                  "| ForEach-Object { \"$($_.ProcessId)`t$($_.ExecutablePath) $($_.CommandLine)\" }")
            out = subprocess.run(["powershell", "-NoProfile", "-Command", ps],
                                 capture_output=True, text=True, timeout=15)
            lines = out.stdout.splitlines()
        else:
            out = subprocess.run(["ps", "-axww", "-o", "pid=,command="], capture_output=True, text=True, timeout=10)
            lines = out.stdout.splitlines()
    except (OSError, subprocess.TimeoutExpired) as e:
        raise CamError(f"无法检测 Claude 桌面版进程，未执行切换或删除：{e}") from e
    if out.returncode != 0:
        raise CamError("无法检测 Claude 桌面版进程，未执行切换或删除；请先确认进程查询权限")
    for line in lines:
        parts = line.strip().split("\t", 1) if IS_WIN else line.strip().split(None, 1)
        if len(parts) != 2:
            continue
        pid, cmd = parts
        if _is_desktop_cmd(cmd):
            try:
                pid = int(pid.strip())
            except ValueError:
                continue
            profile = _desktop_profile_from_command(cmd)
            if profile is None and IS_MAC:
                profile = _desktop_profile_from_files(pid)
            found.append((pid, profile))
    return found


def _desktop_pids():
    return [pid for pid, _ in _desktop_processes()]


def _find_claude_desktop():
    env = os.environ.get("CAM_CLAUDE_DESKTOP")
    if env:
        if os.path.exists(env):
            return env
        raise CamError(f"CAM_CLAUDE_DESKTOP 指向的文件不存在：{env}")
    for c in _desktop_candidates():
        if c and os.path.exists(c):
            return c
    raise CamError("没找到 Claude 桌面版。请先安装 Claude，再切换。")


def _quit_claude_desktop():
    if not _desktop_pids():
        return
    try:
        if IS_MAC:
            subprocess.run(["osascript", "-e", 'tell application "Claude" to quit'],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10, check=True)
        else:
            for pid in _desktop_pids():
                cmd = ["taskkill", "/PID", str(pid)] if IS_WIN else ["kill", str(pid)]
                subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10, check=True)
    except (OSError, subprocess.SubprocessError) as e:
        raise CamError(f"无法正常退出 Claude 桌面版，请手动退出后重试：{e}") from e
    deadline = time.time() + 8
    while _desktop_pids():
        if time.time() > deadline:
            raise CamError("Claude 桌面版还在运行。请先手动退出它，再切换，否则会继续用原来的登录。")
        time.sleep(0.25)


def _launch_claude_desktop(app, directory):
    arg = f"--user-data-dir={directory}"
    if IS_MAC and app.endswith(".app"):
        cmd = ["open", "-n", "-a", app, "--args", arg]
    else:
        cmd = [app, arg]
    kw = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL, "stdin": subprocess.DEVNULL}
    if IS_WIN:
        kw["creationflags"] = 0x00000008 | 0x00000200
    else:
        kw["start_new_session"] = True
    try:
        launcher = subprocess.Popen(cmd, **kw)
        is_open = IS_MAC and app.endswith(".app")
        if is_open and launcher.wait(timeout=15) != 0:
            raise CamError("macOS 没能启动 Claude 桌面版")
        expected = os.path.normcase(os.path.realpath(directory))
        deadline = time.monotonic() + 10
        verified_since = None
        while True:
            status = launcher.poll()
            if status is not None and (status != 0 or not is_open):
                raise CamError(f"Claude 桌面版启动后退出（状态 {status}）")
            processes = _desktop_processes()
            now = time.monotonic()
            if processes and all(profile == expected for _, profile in processes):
                if verified_since is None:
                    verified_since = now
                elif now - verified_since >= 0.5:
                    return
            else:
                verified_since = None
            if now >= deadline:
                raise CamError("未能确认 Claude 桌面版使用了目标登录目录，请手动检查后重试")
            time.sleep(0.2)
    except (OSError, subprocess.SubprocessError) as e:
        raise CamError(f"无法启动 Claude 桌面版：{e}") from e


def _remember_desktop(store, acc_id):
    with store.lock():
        data = store.load()
        acc = find_account(data, acc_id)
        data.setdefault("active", {})["claude_desktop"] = acc["id"]
        acc["last_switched_at"] = now_iso()
        store.save(data)


_LOGIN_FILES = ("config.json", "config.json.journal", "config.json-wal", "config.json-shm",
                "Cookies", "Cookies-journal", "Cookies-wal", "Cookies-shm",
                "Local State", "Preferences", os.path.join("Network", "Cookies"), os.path.join("Network", "Cookies-journal"),
                os.path.join("Network", "Cookies-wal"), os.path.join("Network", "Cookies-shm"))


def _backup_desktop_login(store, directory):
    """只备份登录文件。对话和虚拟机留在原目录，不整份拷走，也不写进别的号。"""
    if not os.path.isdir(directory):
        return
    dest = store.new_backup_dir("before-desktop")
    for name in _LOGIN_FILES:
        src = os.path.join(directory, name)
        if os.path.isfile(src):
            dst = os.path.join(dest, name)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy2(src, dst)
            os.chmod(dst, 0o600)
    store.prune_backups()


def _leaving_desktop_profile(store, data, processes):
    profiles = {profile for _, profile in processes}
    if None in profiles or len(profiles) > 1:
        raise CamError("无法确认正在运行的 Claude 桌面版登录目录，请先手动退出 Claude 再切换")
    if profiles:
        return next(iter(profiles))
    cur = (data.get("active") or {}).get("claude_desktop")
    if cur:
        try:
            acc = find_account(data, cur)
            if acc.get("kind") == "claude_desktop":
                return desktop_profile_dir(store, acc)
        except CamError:
            pass
    return default_desktop_user_data()


def _restore_desktop_active(store, profile):
    """回退后只标记已验证的目录；无法恢复或未绑定的目录不假装仍是旧账号。"""
    with store.lock():
        data = store.load()
        expected = os.path.normcase(os.path.realpath(profile)) if profile else None
        current = next((a for a in data["accounts"] if a.get("kind") == "claude_desktop" and expected is not None
                        and os.path.normcase(os.path.realpath(desktop_profile_dir(store, a))) == expected), None)
        data["active"]["claude_desktop"] = current["id"] if current else None
        store.save(data)


def open_desktop_account(store, acc):
    with desktop_operation_lock(store):
        return _open_desktop_account_locked(store, acc)


def _open_desktop_account_locked(store, acc):
    """正常退出、备份，再启动并核验目标目录；成功后才更新 active。"""
    # 获取桌面锁后重读，防止排队期间账号已被另一个进程删除或更改。
    acc = find_account(store.load(), acc["id"])
    if acc.get("kind") != "claude_desktop":
        raise CamError("只有 Claude 桌面版账号可以这样切换")
    directory = desktop_profile_dir(store, acc)
    dry = os.environ.get("CAM_DESKTOP_DRY_RUN") == "1"
    data = store.load()
    processes = [] if dry else _desktop_processes()
    previous = _leaving_desktop_profile(store, data, processes)
    existed = os.path.isdir(directory)
    os.makedirs(directory, exist_ok=True)
    # 只收紧我们自己建的目录。绑定“当前已登录”时目录是 Claude 自己的，不改权限。
    if not existed or directory.startswith(store.root + os.sep):
        try:
            os.chmod(directory, 0o700)
        except OSError:
            pass
    label = acc.get("label") or "未命名"
    fresh = not _desktop_has_login(directory)
    msg = f"已切换到 {label}。每个号的登录和对话都留在自己的目录里，不会覆盖别的号，也不会动 Claude Code 的项目和会话。"
    if fresh:
        msg += "这个配置还没登录过，请在窗口里点一次邮箱。登录有效期间可直接切换；失效后需要重新登录。"
    if dry:
        _backup_desktop_login(store, previous)
        _remember_desktop(store, acc["id"])
        return msg
    app = _find_claude_desktop()
    _quit_claude_desktop()
    try:
        _backup_desktop_login(store, previous)
        _launch_claude_desktop(app, directory)
        _remember_desktop(store, acc["id"])
    except Exception as e:
        recovered = False
        try:
            _quit_claude_desktop()
            _launch_claude_desktop(app, previous)
            recovered = True
            recovery = "已重新打开原来的登录目录。"
        except Exception as rollback_error:
            recovery = f"也没能重新打开原来的登录目录，请手动打开 Claude。恢复失败：{rollback_error}。"
        try:
            _restore_desktop_active(store, previous if recovered else None)
        except Exception as state_error:
            recovery += f"当前账号记录无法保存：{state_error}。"
        raise CamError(f"没能打开 {label}。{recovery}原来的登录和对话都还在。{e}") from e
    return msg


# --------------------------------------------------------------------------- 对外视图

PUBLIC_FIELDS = ("id", "kind", "label", "email", "plan", "org_name", "rate_limit_tier", "manual", "usage",
                 "created_at", "last_switched_at", "auto", "desktop_profile")


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


HEALTH_LEVELS = ("ok", "info", "warn", "error")
HEALTH_LABEL = {"ok": "正常", "info": "正常", "warn": "注意", "error": "异常"}
HEALTH_TEXT = {  # kind → (level, 一句话, 怎么办)。规则和 Rust 版 api::account_health 一致（tests/shared/health.json）
    "no_credentials": ("error", "没有保存可用的凭据", "用这个账号登录 Claude Code（在 claude 里 /login）后点“导入当前登录”，或运行 cam import。"),
    "wrong_owner": ("error", "保存的登录实际属于别的账号", "可能被旧的 claude 会话覆盖了。用这个账号重新登录 Claude Code 后导入，会自动修正。"),
    "login_invalid": ("error", "登录已失效，无法续期", "用这个账号在 Claude Code 里重新登录一次（/login），会自动保存。在那之前切换到它也用不了。"),
    "session_invalid": ("error", "sessionKey 已失效", "在浏览器里重新登录 claude.ai，复制新的 sessionKey 填到“编辑”里。"),
    "key_invalid": ("error", "API Key 无效或没有填写", "在“编辑”里换一个有效的 Key。"),
    "stale": ("info", "用量没更新：登录过期了，后台不替不常用的账号续登录", "点“刷新”会续一次；切换到这个账号时也会自动续。"),
    "network": ("warn", "用量暂时拿不到（网络或服务问题）", "稍后再刷新；和账号本身无关，不影响切换。"),
    "usage_error": ("warn", "用量获取失败", "稍后再刷新；持续出现请把错误信息发给作者。"),
    "expired": ("warn", "订阅已过期", "续费后在“编辑”里改到期日；没续的话这个号可能已经降级为免费版。"),
}


def _health_kind_of_error(kind, err):
    """把用量报错归类成健康问题的种类；None 表示这不是问题（例如网页号本来就没填 sessionKey）。"""
    if "实际属于" in err:
        return "wrong_owner"
    if "没有保存凭据" in err:
        return "no_credentials"
    if "登录已失效" in err or "没有 refresh token" in err:
        return "login_invalid"
    if "未填写 sessionKey" in err:
        return None
    if "sessionKey 已过期或无效" in err:
        return "session_invalid"
    if "没有填写 API Key" in err:
        return "key_invalid"
    if "后台不会替它续登录" in err:
        return "stale"
    if any(k in err for k in ("人机验证", "网络错误", "续登录失败", "HTTP 5", "HTTP 429")):
        return "network"
    return "usage_error"


def account_health(acc, has_creds, days):
    """账号健康状态 {"level", "label", "issues": [{"kind", "level", "text", "advice", "detail"}]}。只用于显示。"""
    kinds = []
    u = acc.get("usage") or {}
    if acc.get("kind") == "claude_code" and not has_creds:
        kinds.append(("no_credentials", None))
    err = u.get("error")
    if err:
        k = _health_kind_of_error(acc.get("kind"), err)
        if k and k not in [x for x, _ in kinds]:
            kinds.append((k, err))
    if acc.get("kind") == "api" and u.get("key_valid") is False and "key_invalid" not in [x for x, _ in kinds]:
        kinds.append(("key_invalid", u.get("key_error")))
    if isinstance(days, int) and days < 0:
        kinds.append(("expired", None))
    issues = []
    for k, detail in kinds:
        level, text, advice = HEALTH_TEXT[k]
        if k == "expired":
            text = f"订阅已过期 {-days} 天"
        issues.append({"kind": k, "level": level, "text": text, "advice": advice, "detail": detail})
    level = max((i["level"] for i in issues), key=HEALTH_LEVELS.index, default="ok")
    return {"level": level, "label": HEALTH_LABEL[level], "issues": issues}


def public_state(store, data):
    risks = risk_items(data)
    accs = []
    active = data["active"].get("claude_code")
    active_desktop = data["active"].get("claude_desktop")
    for a in data["accounts"]:
        p = {k: a.get(k) for k in PUBLIC_FIELDS}
        legacy_desktop = (a.get("kind") == "claude_desktop" and
                          (a.get("usage") or {}).get("source") not in ("desktop_cache", "desktop_history"))
        if legacy_desktop:
            p["usage"] = None
        elif a.get("kind") == "claude_desktop":
            p["usage"] = _desktop_usage_view(a["usage"])
        p["is_active"] = a["id"] == (active_desktop if a.get("kind") == "claude_desktop" else active)
        p["days_left"] = days_left(a)
        p["plan_display"] = a.get("plan") or (a.get("manual") or {}).get("plan")
        has_creds = False
        if a["kind"] == "claude_code":
            has_creds = bool((store.load_secret(a["id"]).get("claudeAiOauth") or {}).get("refreshToken"))
        p["health"] = account_health(dict(a, usage=None) if legacy_desktop else a,
                                     has_creds, p["days_left"])
        inc = (a.get("incidents") or [None])[0]  # 上次出问题的现场摘要，面板账号详情直接显示；完整的在“导出诊断”里
        ir, ie = (inc or {}).get("region") or {}, (inc or {}).get("env") or {}
        p["last_incident"] = {"at": inc["at"], "text": HEALTH_TEXT.get(inc["kind"], ("", inc["kind"], ""))[1], "loc": ir.get("loc"),
                              "ip": ir.get("ip"), "org": ir.get("org"), "ip_type": IP_TYPE_NAMES.get(ir.get("ip_type")),
                              "proxy": bool(ie.get("proxy")), "tunnel": bool(ie.get("tunnel")), "sessions": inc.get("sessions") or 0} if inc else None
        p["exits"] = [{"loc": r.get("loc"), "ip": r.get("ip"), "org": r.get("org"), "at": r.get("at"), "times": r.get("times") or 1}
                      for r in (a.get("regions") or [])[:3]]
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
            "pending_switch": pending_switch_view(data), "region": region_view(data), "risks": risks,
            "risk_score": risk_score(risks), "sessions": {"count": session_count()}, "generated_at": now_iso()}


_SESSION_CACHE = [0.0, 0]  # [上次数的时间, 个数]


def session_count(max_age=20):
    """正在运行的 Claude Code 会话数，给面板和卡片小面板的红绿灯显示用。

    查进程表要几百毫秒（Windows 上要一秒多），所以缓存 max_age 秒；切换前的判断不走这里，用 running_claude_processes 现查。
    """
    now = time.monotonic()
    if now - _SESSION_CACHE[0] > max_age:
        try:
            _SESSION_CACHE[:] = [now, len(running_claude_processes())]
        except Exception:  # noqa: BLE001  数不出来就沿用上次的
            _SESSION_CACHE[0] = now
    return _SESSION_CACHE[1]


KIND_NAMES = {"claude_code": "Claude Code", "claude_web": "claude.ai 网页版", "claude_desktop": "Claude 桌面版", "api": "API Key"}
PLAN_NAMES = {"free": "Free", "pro": "Pro", "max": "Max", "team": "Team", "enterprise": "Enterprise", "api": "API"}


def _when(iso):
    """ISO 时间 → 本机时区的“2026-10-29 15:40”。"""
    t = parse_iso(iso) if iso else None
    return t.astimezone().strftime("%Y-%m-%d %H:%M") if t else "–"


def account_diagnostic(store, data, key):
    """一个账号的诊断报告（不含任何密钥）：资料和现在的问题、出问题时的现场、用过的出口、当前环境和全部风险项、最近切换、本机。"""
    acc = find_account(data, key)
    pub = next(a for a in public_state(store, data)["accounts"] if a["id"] == acc["id"])
    names = {a["id"]: a["label"] for a in data["accounts"]}
    risks = risk_items(data)
    return {"generated_at": now_iso(), "cam_version": VERSION, "host": socket.gethostname(), "platform": platform.platform(),
            "account": pub, "incidents": acc.get("incidents") or [], "regions": acc.get("regions") or [],
            "region": region_view(data), "env": (data.get("region") or {}).get("env") or {},
            "risks": [r for r in risks if r.get("kind") != "summary"], "risk_score": risk_score(risks),
            "status_page": data.get("status_page"), "sessions": session_count(), "pending_switch": pending_switch_view(data),
            "switches": [{"at": e["at"], "to": names.get(e["id"], e["id"]), "this": e["id"] == acc["id"]}
                         for e in data.get("switch_log", [])[-30:] if e.get("at")]}


def diagnostic_filename(rep):
    label = re.sub(r"[\\/:*?\"<>|\s]+", "_", rep["account"].get("label") or "账号")[:30]
    return f"cam-诊断-{label}-{_when(rep['generated_at']).replace(' ', '-').replace(':', '')}.md"


def dns_name(d):
    """DNS 条目（local_env 里是 {ip, name, country}）→ “8.8.8.8（Google）”。"""
    if not isinstance(d, dict):
        return str(d)
    return (d.get("ip") or "?") + (f"（{d['name']}）" if d.get("name") else "")


def diagnostic_text(rep):
    """诊断报告的 Markdown，给人看、也方便发给别人。"""
    a, reg, env, sc = rep["account"], rep["region"], rep["env"], rep["risk_score"]

    def exit_line(r):
        ip_type = IP_TYPE_NAMES.get(r.get("ip_type"), r.get("ip_type") or "类型未知")
        bits = [region_label(r.get("loc"))] + ([r["ip"]] if r.get("ip") else []) + [f"{r.get('org') or '运营商未知'}，{ip_type}"]
        return " ".join(bits[:2]) + f"（{bits[2]}" + (f"，{r['city']}" if r.get("city") else "") + "）"

    def env_line(e):
        px = e.get("proxy") or []
        tun = e.get("tunnel")
        return (f"时区 {e.get('tz') or '未知'} · 语言 {e.get('locale') or '未知'} · 代理 {'；'.join(px) if px else '无'} · "
                f"VPN {tun[0] + '（' + tun[1] + '）' if tun else '无'} · DNS {'、'.join(dns_name(d) for d in e.get('dns') or []) or '未知'}")

    L = [f"# cam 账号诊断：{a['label']}" + (f"（{a['email']}）" if a.get("email") and a["email"] != a["label"] else ""),
         f"生成于 {_when(rep['generated_at'])} · 本机 {rep['host']}（{rep['platform']}）· cam {rep['cam_version']} · 不含任何密钥", "",
         "## 账号"]
    plan = PLAN_NAMES.get(a.get("plan_display"), a.get("plan_display") or "未知") + (f" {plan_tier(a)}" if plan_tier(a) else "")
    L.append(f"- 类型 {KIND_NAMES.get(a['kind'], a['kind'])} · 套餐 {plan}" + (f" · 组织 {a['org_name']}" if a.get("org_name") else "")
             + (" · 正在用" if a.get("is_active") else ""))
    L.append(f"- 添加于 {_when(a.get('created_at'))} · 最近切换 {_when(a.get('last_switched_at'))}")
    u = a.get("usage") or {}
    for key, name in (("five_hour", "5 小时"), ("seven_day", "7 天")):
        w = u.get(key) or {}
        if w.get("utilization") is not None:
            L.append(f"- {name}额度已用 {w['utilization']:.0f}%，{_when(w.get('resets_at'))} 重置")
    if u.get("fetched_at"):
        L.append(f"- 用量更新于 {_when(u['fetched_at'])}" + (f"，报错：{u['error']}" if u.get("error") else ""))
    L += ["", "## 现在的问题"]
    issues = (a.get("health") or {}).get("issues") or []
    L += [f"- [{ {'error': '异常', 'warn': '注意', 'info': '提示'}.get(i['level'], i['level'])}] {i['text']}"
          + (f"：{i['detail']}" if i.get("detail") and not str(i["detail"]).startswith(i["text"]) else "")
          + (f"\n  怎么办：{i['advice']}" if i.get("advice") else "") for i in issues] or ["- 没有发现问题"]
    L += ["", f"## 出问题时的现场（最近 {len(rep['incidents'])} 次）"]
    for inc in rep["incidents"]:
        L.append(f"- {_when(inc['at'])} {HEALTH_TEXT.get(inc['kind'], ('', inc['kind'], ''))[1]}：{inc.get('error') or ''}")
        L.append(f"  出口 {exit_line(inc.get('region') or {})} · WARP {'是' if (inc.get('region') or {}).get('warp') else '否'}"
                 f" · {env_line(inc.get('env') or {})} · {inc.get('sessions') or 0} 个 claude 会话在跑"
                 + (f" · 环境评分 {inc['risk_score']}/100" if inc.get("risk_score") is not None else "")
                 + ("" if inc.get("active") else " · 当时不是正在用的号"))
    if not rep["incidents"]:
        L.append("- 还没有记录（账号出问题那一刻才会记）")
    L += ["", "## 这个账号用过的出口"]
    L += [f"- {exit_line(r)} · 最近 {_when(r.get('at'))} · 首次 {_when(r.get('first_at') or r.get('at'))} · {r.get('times') or 1} 次"
          for r in rep["regions"]] or ["- 还没有记录"]
    L += ["", "## 当前环境"]
    if reg.get("checked_at"):
        L.append(f"- 出口 {exit_line(reg)} · WARP {'是' if reg.get('warp') else '否'} · 查询方式 {reg.get('lookup_mode') or '自动'} · 检测于 {_when(reg['checked_at'])}")
        L.append(f"- {reg.get('message') or ''}" + (f"（{reg['advice']}）" if reg.get("advice") else ""))
    else:
        L.append("- 还没有检测过出口地区")
    L.append(f"- {env_line(env)}")
    L.append(f"- 环境评分 {sc['score']}/100（{sc['label']}）")
    L += [f"- [{ {'bad': '风险', 'warn': '注意', 'info': '提示', 'ok': '正常'}.get(r['level'], r['level'])}] {r['text']}"
          + (f"（{r['advice']}）" if r.get("advice") else "") for r in rep["risks"]]
    sp = rep.get("status_page") or {}
    if sp.get("text") or sp.get("description"):
        L.append(f"- Anthropic 状态页：{sp.get('text') or sp.get('description')}")
    L += ["", "## 最近切换"]
    L += [f"- {_when(e['at'])} → {e['to']}" + ("（这个号）" if e.get("this") else "") for e in rep["switches"][-15:]] or ["- 没有切换记录"]
    L += ["", "## 本机", f"- 正在跑的 claude 会话：{rep.get('sessions') or 0} 个"
          + (f" · 等待中的切换：{rep['pending_switch']['message']}" if rep.get("pending_switch") else "")]
    L.append("- 导出的 IP 是 cam 检测到的这台电脑的出口；Anthropic 那边的记录拿不到。只记录、只导出，不拦任何操作。")
    return "\n".join(L) + "\n"


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


def ensure_desktop_removable(store, acc, data):
    """调用方持有 desktop-switch.lock 和 accounts.lock；绝不搬走活跃的登录目录。"""
    if acc.get("kind") != "claude_desktop":
        return
    if data.get("active", {}).get("claude_desktop") == acc["id"]:
        raise CamError("这是 Claude 桌面版当前使用的账号，请先切换到其它桌面版账号再移除")
    if acc.get("desktop_profile") == "default":
        return  # 仅解除绑定，不搬动 Claude 自己的默认目录
    expected = os.path.normcase(os.path.realpath(desktop_profile_dir(store, acc)))
    if any(profile is None or profile == expected for _, profile in _desktop_processes()):
        raise CamError("这个桌面版登录目录可能仍在使用，请先手动退出 Claude 再移除")


def remove_account(store, key):
    with desktop_operation_lock(store):
        return _remove_account_locked(store, key)


def _remove_account_locked(store, key):
    with store.lock():
        data = store.load()
        acc = find_account(data, key)
        if data["active"].get("claude_code") == acc["id"]:
            raise CamError("这是 Claude Code 当前正在用的账号，请先切换到其它账号再删除")
        ensure_desktop_removable(store, acc, data)
        d = store.new_backup_dir(f"removed-{acc['id']}")
        rules = [r for r in data.get("rules", []) if r.get("account") == acc["id"]]
        write_json_atomic(os.path.join(d, "account.json"), dict(acc, rules=rules), private=True)
        sp = store.secret_path(acc["id"])
        if os.path.exists(sp):
            shutil.move(sp, os.path.join(d, "secret.json"))
        prof = os.path.join(store.profiles_dir, acc["id"])
        if os.path.isdir(prof):  # 网页账号的浏览器 Profile（含登录 Cookie）也一起移走
            shutil.move(prof, os.path.join(d, "browser-profile"))
        # 绑定“当前已登录”的桌面版目录就是 Claude 自己的数据，只解除记录，不搬走
        if acc.get("kind") == "claude_desktop" and acc.get("desktop_profile") != "default":
            desk = os.path.join(store.root, "desktop-profiles", acc["id"])
            if os.path.isdir(desk):
                shutil.move(desk, os.path.join(d, "desktop-profile"))
        if data["active"].get("claude_desktop") == acc["id"]:
            data["active"]["claude_desktop"] = None
        data["accounts"] = [a for a in data["accounts"] if a["id"] != acc["id"]]
        data["rules"] = [r for r in data.get("rules", []) if r.get("account") != acc["id"]]
        store.save(data)
        return acc, d


# --------------------------------------------------------------------------- Web 服务

def panel_html(name="panel.html"):
    here = os.path.dirname(os.path.abspath(__file__))
    for p in (os.path.join(here, "..", "web", name), os.path.join(here, name)):
        if os.path.exists(p):
            with open(p, "r", encoding="utf-8") as f:
                return f.read()
    return f"<h1>{name} not found</h1><p>请把 web/{name} 放在 cam.py 旁边。</p>"


def mini_html():
    """菜单栏小工具点开的卡片小面板（SwiftBar 的 webview），和面板用同一套接口。"""
    return panel_html("mini.html")


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


def identify_account(store, key):
    import cam_identity
    account = find_account(store.load(), key)
    if account.get("kind") == "claude_desktop":
        try:
            found = cam_identity.read_desktop_identity(desktop_profile_dir(store, account), account.get("desktop_account_uuid"))
            with store.lock():
                fresh = store.load()
                current = find_account(fresh, account["id"])
                verified = cam_identity.read_desktop_identity(desktop_profile_dir(store, current), current.get("desktop_account_uuid"))
                if found != verified:
                    raise CamError("识别期间客户端登录发生变化，请重试")
                current.update(found)
                store.save(fresh)
        except cam_identity.IdentityError as error:
            raise CamError(str(error)) from error
    elif account.get("kind") in ("claude_code", "claude_web"):
        refresh_usage(store, account["id"], allow_token_refresh=False)
    else:
        raise CamError("这个账号类型的邮箱请手动填写")
    data = store.load()
    if not find_account(data, account["id"]).get("email"):
        raise CamError("暂时无法识别邮箱，请确认对应账号已登录，或手动填写")
    result = public_state(store, data)
    result["identity_message"] = "已从对应账号的登录资料补全邮箱"
    return result


def vault_route(store, method, parts, body=None):
    """可选账号库模块按需加载，未安装加密依赖时不影响其它功能。"""
    try:
        import cam_vault
        import cam_mailbox
    except ImportError as error:
        raise CamError("账号库需要安装依赖：python3 -m pip install -r scripts/requirements-vault.txt") from error
    body = body or {}
    try:
        if method == "GET" and parts == ["vault"]:
            return cam_vault.status(store)
        if method == "POST" and parts in (["vault", "setup"], ["vault", "unlock"]):
            return getattr(cam_vault, parts[1])(store, body)
        if method == "POST" and parts == ["vault", "lock"]:
            return cam_vault.lock(store)
        if method == "POST" and parts == ["vault", "entries"]:
            return cam_vault.save_entry(store, body)
        if len(parts) == 3 and parts[:2] == ["vault", "oauth"] and method == "GET":
            return cam_mailbox.oauth_status(store, parts[2])
        if len(parts) >= 3 and parts[:2] == ["vault", "entries"]:
            if len(parts) == 3 and method == "DELETE":
                return cam_vault.delete_entry(store, parts[2])
            if len(parts) == 4 and method == "POST":
                if parts[3] == "reveal":
                    return cam_vault.reveal_entry(store, parts[2])
                if parts[3] == "connect":
                    return cam_mailbox.connect(store, parts[2])
                if parts[3] == "messages":
                    return cam_mailbox.messages(store, parts[2], body)
                if parts[3] == "open-verification":
                    return open_verification_url(store, parts[2], body.get("url"))
        raise CamError("未找到账号库接口")
    except (cam_vault.VaultError, cam_mailbox.MailboxError) as error:
        raise CamError(str(error)) from error


def open_in_browser(url):
    """用系统默认浏览器打开地址。调用方自己负责检查地址是否可信。"""
    if IS_MAC:
        command = ["open", url]
    elif IS_WIN:
        command = ["rundll32.exe", "url.dll,FileProtocolHandler", url]
    else:
        command = ["xdg-open", url]
    try:
        subprocess.run(command, check=True, timeout=15, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except (OSError, subprocess.SubprocessError) as error:
        raise CamError("系统浏览器未打开，请复制地址重试") from error


def open_external_url(url):
    import cam_identity
    if not cam_identity.allowed_external_url(url):
        raise CamError("只允许打开 Google / Microsoft 授权页或 Claude / Anthropic 的 HTTPS 验证地址")
    open_in_browser(url)
    return {"ok": True}


def open_verification_url(store, entry_id, url):
    import cam_identity
    import cam_vault
    if not cam_identity.allowed_external_url(url):
        raise CamError("验证链接必须是 Claude / Anthropic 官方 HTTPS 地址")
    host = (urllib.parse.urlsplit(url).hostname or "").lower()
    if not any(host == domain or host.endswith("." + domain) for domain in ("claude.ai", "claude.com", "anthropic.com")):
        raise CamError("验证链接必须是 Claude / Anthropic 官方 HTTPS 地址")
    with desktop_operation_lock(store):
        entry = cam_vault.get_entry(store, entry_id)
        if entry.get("account_id"):
            account = find_account(store.load(), entry["account_id"])
            if account.get("email") and entry.get("email") and account["email"].casefold() != entry["email"].casefold():
                raise CamError("绑定的 Claude 账号与邮箱条目不一致，请先核对绑定关系")
            if account.get("kind") == "claude_web":
                open_web_account(store, account, url=url)
                return {"ok": True, "message": "已在绑定账号的独立浏览器窗口打开验证链接"}
            if account.get("kind") == "claude_desktop":
                expected = os.path.normcase(os.path.realpath(desktop_profile_dir(store, account)))
                processes = _desktop_processes()
                if not processes or any(profile != expected for _, profile in processes):
                    raise CamError("请先打开绑定的 Claude 桌面版账号，并在那个窗口发起邮箱登录，再打开验证链接；验证时不会重启客户端。")
        open_external_url(url)
        return {"ok": True, "message": "已在本机浏览器打开验证链接，客户端保持运行"}


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
            if method == "GET" and path == "/mini":
                return self._send(200, raw=mini_html().encode(), ctype="text/html; charset=utf-8")
            if method == "GET" and path == "/manifest.webmanifest":
                return self._send(200, MANIFEST, ctype="application/manifest+json")
            if not path.startswith("/api/"):
                return self._send(404, {"error": "not found"})
            if not self._authed():
                return self._send(401, {"error": "需要访问令牌"})
            parts = [p for p in path.split("/") if p][1:]
            try:
                if parts[:1] == ["vault"]:
                    return self._send(200, vault_route(store, method, parts, self._body() if method == "POST" else {}))
                if method == "POST" and parts == ["open-external"]:
                    return self._send(200, open_external_url(self._body().get("url")))
                if method == "POST" and parts == ["open-panel"]:  # 卡片小面板里点“打开管理面板”：在这台电脑的浏览器里打开
                    return self._send(200, {"ok": True, "url": open_panel(store, self.server.server_address[1])})
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
                    return self._send(200, {"items": service_check(store)})
                if method == "GET" and parts == ["checkup"]:  # 全面体检，同 cam doctor --json。只有脚本版有：桌面版返回 404，面板据此隐藏按钮
                    q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                    if (q.get("probe") or [""])[0]:
                        return self._send(200, {"ok": True})
                    return self._send(200, full_checkup(store, offline=bool((q.get("offline") or [""])[0])))
                if method == "GET" and parts == ["update"]:  # 只有脚本版有：桌面版的服务返回 404，面板据此隐藏“检查更新”
                    q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                    if (q.get("saved") or [""])[0]:
                        return self._send(200, saved_update_check() or {"checked_at": None, "version": VERSION})
                    return self._send(200, update_check())
                if method == "POST" and parts == ["update"]:
                    return self._send(200, apply_update(restart_later=True))
                if method == "GET" and parts == ["privacy"]:
                    return self._send(200, privacy_view(store))
                if method == "PUT" and parts == ["privacy"]:
                    return self._send(200, save_privacy(store, self._body()))
                if method == "DELETE" and parts == ["privacy", "history"]:
                    return self._send(200, clear_region_history(store))
                if method == "POST" and parts == ["accounts"]:
                    b = self._body()
                    if b.get("kind") == "claude_code":
                        import_current(store, b.get("label"))
                    else:
                        add_account(store, b.get("kind"), b.get("label"), email=b.get("email"),
                                    session_key=b.get("session_key"), api_key=b.get("api_key"),
                                    admin_key=b.get("admin_key"), app_mode=b.get("app_mode"),
                                    use_current=b.get("use_current"))
                    return self._send(200, public_state(store, store.load()))
                if len(parts) >= 2 and parts[0] == "accounts":
                    aid = parts[1]
                    if method == "GET" and parts[2:] == ["diagnostic"]:  # 面板“导出诊断”：报告 + Markdown + 文件名
                        rep = account_diagnostic(store, store.load(), aid)
                        return self._send(200, {"report": rep, "text": diagnostic_text(rep), "filename": diagnostic_filename(rep)})
                    if method == "POST" and parts[2:] == ["identify"]:
                        return self._send(200, identify_account(store, aid))
                    if method == "POST" and parts[2:] == ["switch"]:
                        return self._send(200, api_switch(store, aid, self._body().get("mode")))
                    if method == "POST" and parts[2:] == ["open"]:
                        acc = find_account(store.load(), aid)
                        if acc.get("kind") == "claude_desktop":
                            return self._send(200, {"ok": True, "message": open_desktop_account(store, acc)})
                        open_web_account(store, acc)
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
            "background_color": "#F4F2EC", "theme_color": "#F4F2EC"}


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
    risks = [r for r in risk_items(data) if r["level"] in ("warn", "bad")]  # 只提醒，不拦
    procs = running_claude_processes() if target["id"] != data["active"].get("claude_code") else []
    if procs and mode != "now":
        if mode != "wait":
            return {"ok": False, "choice_required": True, "running_claude": len(procs), "account": target["id"],
                    "label": target["label"], "risks": risks,
                    "message": running_summary(procs) + "。等它们结束后再切换不会打断正在跑的任务；现在就切的话它们需要重开。"}
        with store.lock():
            data = store.load()
            defer_switch(data, target["id"], "manual")
            store.save(data)
        return {"ok": True, "changed": False, "deferred": True, "running_claude": len(procs), "account": target["id"],
                "risks": risks,
                "message": f"有 {len(procs)} 个 claude 会话在运行，等它们退出后会自动切换到 {target['label']}"}
    acc, backup, changed = switch_code_account(store, aid)
    procs = running_claude_processes() if changed else []
    msg = (f"已切换到 {acc['label']}。" + (running_warning(procs) or "正在运行的 claude 会话需要重启")
           if changed else "已经是当前账号")
    return {"ok": True, "changed": changed, "account": acc["id"], "running_claude": len(procs), "message": msg,
            "backup": backup, "risks": risks}


def notify(title, body):
    """弹一条系统通知：macOS 用 osascript，其它系统有 notify-send 就用它，都没有就不弹。出错不影响调用方。

    标题和正文作为参数传给脚本，不拼进 AppleScript 源码，引号、中文都不用转义。CAM_NO_NOTIFY=1 可关掉。
    """
    if os.environ.get("CAM_NO_NOTIFY"):
        return
    if IS_MAC:
        cmd = ["osascript", "-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)",
               "-e", "end run", title, body]
    elif not IS_WIN and shutil.which("notify-send"):
        cmd = ["notify-send", title, body]
    else:
        return
    try:
        subprocess.run(cmd, timeout=5, capture_output=True)
    except (OSError, subprocess.SubprocessError):
        pass


def errtext(e):
    """错误的一句话；CamError 带的“怎么办”也接在后面。"""
    advice = getattr(e, "advice", "")
    return f"{e}。{advice}" if advice else str(e)


def background_round(store, st, refresh_min, tell, import_live=True):
    """后台任务跑一轮：自动导入新登录、时间规则、等待切换、定时刷新用量和额度提醒。

    菜单栏插件每次运行跑一轮，cam serve 每隔几秒跑一轮，两边弹的通知一样（tell(标题, 正文)）。
    st 记两轮之间的状态（last_refresh、warned、last_error），调用方负责保存。任何一步出错都只记进
    last_error，不影响后面的步骤。import_live=False 跳过自动导入（cam serve 在登录状态没变时用它省掉读钥匙串）。
    """
    if import_live:
        try:
            acc = auto_import(store)
            if acc:
                tell("发现新账号", f"已自动保存：{acc['label']}")
                st["last_refresh"] = 0
        except Exception as e:  # noqa: BLE001
            st["last_error"] = f"自动导入：{errtext(e)}"
    try:
        res = apply_time_rules(store)
        if res and res[2]:
            warn = running_warning(running_claude_processes()) or "正在运行的会话需要重启。"
            tell("已按规则切换账号", f"Claude Code 已切换到 {res[0]['label']}。{warn}")
    except Exception as e:  # noqa: BLE001
        st["last_error"] = f"时间规则：{errtext(e)}"
    try:
        res = apply_pending_switch(store)
        if res:
            tell("已切换账号", f"claude 会话已退出，Claude Code 已切换到 {res[0]['label']}"
                 + ("（时间规则）" if res[2] == "rule" else ""))
    except Exception as e:  # noqa: BLE001
        st["last_error"] = f"等待切换：{errtext(e)}"
        try:
            if not store.load().get("pending_switch"):  # 登记已作废（目标账号的登录失效了），不会再试，告诉用户一声
                tell("等待中的切换没有完成", str(e))
        except Exception:  # noqa: BLE001
            pass
    now = time.time()
    if refresh_min > 0 and now - float(st.get("last_refresh") or 0) >= refresh_min * 60:
        st["last_refresh"] = now
        try:
            # 定时刷新不替不常用的账号续登录，见 refresh_usage
            try:
                check_region(store)  # 界面常显的出口地区，随用量刷新一起更新
            except Exception as e:  # noqa: BLE001  检测出错不影响刷新用量
                st["last_error"] = f"环境检测：{errtext(e)}"
            data = refresh_usage(store, allow_token_refresh=False)
            st.pop("last_error", None)
            limit = rules_view(data)["alert_threshold"]
            act = data["active"].get("claude_code")
            warned = set(st.get("warned") or [])
            # 当前 Claude Code 账号 + 所有有额度的网页账号，超过阈值都提醒一次（每个额度窗口一次）
            for a in data["accounts"]:
                if a["id"] != act and a["kind"] != "claude_web":
                    continue
                w = (a.get("usage") or {}).get("five_hour") or {}
                if w.get("utilization") is None:
                    continue
                key = f"{a['id']}|{w.get('resets_at')}"
                if w["utilization"] >= limit and key not in warned:
                    warned.add(key)
                    tell(f"{a['label']} 的 5 小时额度已用 {w['utilization']:.0f}%", fmt_reset(w.get("resets_at")))
            st["warned"] = sorted(warned)[-50:]
        except Exception as e:  # noqa: BLE001
            st["last_error"] = f"刷新用量：{errtext(e)}"
    return st


def save_serve_state(store, st, refresh_min):
    """cam serve 后台的心跳（至多一分钟写一次）：cam doctor 据此说上一轮是多久前、有没有出错。不含密钥。"""
    lr = st.get("last_refresh")
    try:
        write_json_atomic(os.path.join(store.root, SERVE_STATE), {
            "pid": os.getpid(), "at": now_iso(), "refresh_min": refresh_min, "last_error": st.get("last_error"),
            "last_refresh": dt.datetime.fromtimestamp(lr, dt.timezone.utc).replace(microsecond=0).isoformat() if lr else None})
    except OSError:
        pass


def serve(store, host, port, auto_refresh_min):
    token = server_token(store)
    srv = ThreadingServer((host, port), make_handler(store, token))
    print(f"Web 面板已启动（Ctrl+C 退出）")
    print(f"  本机：  http://127.0.0.1:{port}/?t={token}")
    if host == "0.0.0.0":
        for ip in lan_ips():
            print(f"  局域网：http://{ip}:{port}/?t={token}")
        print("  手机和电脑在同一网络下打开上面的地址即可；在外网访问建议用 Tailscale 等组网工具，不要直接暴露端口。")
    def tell(title, body):
        print(f"[通知] {title}：{body}")
        notify(title, body)

    def watch():
        """后台：发现新登录自动导入；按间隔自动刷新用量；快用完时提醒。

        同一台电脑上只让一个进程做这些事（桌面版、cam serve、菜单栏插件谁先拿到后台锁谁做），
        避免几个进程同时轮询、同时刷新。拿到后台锁后菜单栏插件就不弹通知了，所以这里弹同样的系统通知
        （和插件走同一个 background_round），不只是打印出来：从菜单栏“打开管理面板”起的 cam serve 没有终端。
        """
        lease = store.background_lock()
        told = False
        last_fp = None
        st = {}
        last_err = None
        beat, beat_key = 0.0, None
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
                background_round(store, st, auto_refresh_min, tell, import_live=fp != last_fp)
                last_fp = fp
                if time.time() - beat >= 60 or (st.get("last_refresh"), st.get("last_error")) != beat_key:
                    beat, beat_key = time.time(), (st.get("last_refresh"), st.get("last_error"))
                    save_serve_state(store, st, auto_refresh_min)
                if st.get("last_error") != last_err:
                    last_err = st.get("last_error")
                    if last_err:
                        print(f"[后台] {last_err}", file=sys.stderr)
            except Exception as e:
                print("后台任务出错：", e, file=sys.stderr)
            time.sleep(30 if use_keychain() else 5)

    threading.Thread(target=watch, daemon=True).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


def panel_running(port, token):
    """本机 port 上是不是已经有一个用同一令牌的面板（cam serve 或桌面版开的）。不走代理。"""
    req = urllib.request.Request(f"http://127.0.0.1:{port}/api/state", headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=2) as r:
            return r.status == 200
    except (urllib.error.URLError, OSError, http.client.HTTPException, ValueError):
        return False


def port_free(port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


def ensure_panel(store, port=8787):
    """确保本机有面板在跑（cam serve 或桌面版），返回 (端口, 令牌)。

    没有就在后台起一个只监听 127.0.0.1 的 cam serve，它一直运行到注销或重启。端口被别的程序占了就往后找。
    """
    token = server_token(store)
    for p in range(port, port + 10):
        if panel_running(p, token):
            return p, token
        if not port_free(p):
            continue
        kw = {"stdin": subprocess.DEVNULL, "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL,
              "close_fds": True}
        if IS_WIN:
            kw["creationflags"] = 0x00000008 | 0x08000000  # DETACHED_PROCESS | CREATE_NO_WINDOW
        else:
            kw["start_new_session"] = True
        subprocess.Popen([sys.executable, os.path.abspath(__file__), "serve", "--port", str(p)], **kw)
        for _ in range(40):
            time.sleep(0.25)
            if panel_running(p, token):
                return p, token
        raise CamError("面板没能启动。在终端运行 cam serve 看看报错")
    raise CamError(f"{port}–{port + 9} 端口都被别的程序占用了，用 cam panel --port 换一个")


def mini_available(port, token):
    """本机 port 上的面板有没有卡片小面板页（只有脚本版的 cam serve 有；桌面版的服务没有）。"""
    req = urllib.request.Request(f"http://127.0.0.1:{port}/mini", headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=2) as r:
            return r.status == 200
    except (urllib.error.URLError, OSError, http.client.HTTPException, ValueError):
        return False


def restart_stale_serve(store, port, token):
    """port 上在跑的是旧代码的 cam serve（没有 /mini 页）：代码更新后它没重启。

    只动 cam serve（桌面版的服务不碰）：按它记的心跳找到 pid、确认命令行是 cam serve，结束它并等端口空出来。
    返回有没有结束掉，调用方再用 ensure_panel 起新的。
    """
    try:
        req = urllib.request.Request(f"http://127.0.0.1:{port}/api/state", headers={"Authorization": f"Bearer {token}"})
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=2) as r:
            if json.loads(r.read().decode()).get("backend") != "python":
                return False
    except (urllib.error.URLError, OSError, http.client.HTTPException, ValueError):
        return False
    pid = _read_json_quiet(os.path.join(store.root, SERVE_STATE)).get("pid")
    if not pid or not any(p == pid and _is_cam_serve(cmd) for p, cmd in process_table()):
        return False
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        return False
    for _ in range(40):
        if port_free(port):
            return True
        time.sleep(0.25)
    return False


def open_panel(store, port=8787):
    """在浏览器打开 Web 面板（菜单栏的“打开管理面板”）。返回打开的地址。"""
    p, token = ensure_panel(store, port)
    url = f"http://127.0.0.1:{p}/?t={token}"
    open_in_browser(url)
    return url


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
    local = d.astimezone()
    if h >= 24:
        return f"{h // 24}天{h % 24}小时后重置（{local.month}月{local.day}日 {local:%H:%M}）"
    return (f"{h}小时{m}分后重置" if h else f"{m}分后重置") + f"（{local:%H:%M}）"


KIND_NAME = {"claude_code": "Code", "claude_web": "网页", "claude_desktop": "桌面版", "api": "API"}
BREAKDOWN_NAME = {"claude_code": "Claude Code", "chat": "网页聊天", "cowork": "Cowork", "other": "其它"}


def plan_tier(acc):
    """Max 的档位（5x / 20x），从接口给的 rate_limit_tier 里取；不是 Max 或拿不到时返回空。"""
    m = re.search(r"max_(\d+)x", acc.get("rate_limit_tier") or "")
    plan = acc.get("plan_display") or acc.get("plan") or (acc.get("manual") or {}).get("plan")
    return f"{m.group(1)}x" if m and plan == "max" else ""


def usage_windows(u):
    """要显示的额度窗口：(名字, 窗口, 窗口秒数, 本机记录)。按模型的周额度排在 7 天后面。"""
    local = ((u.get("local") or {}).get("windows")) or {}
    out = []
    for k, name in (("five_hour", "5小时"), ("seven_day", "7天"), ("seven_day_opus", "Opus"), ("seven_day_sonnet", "Sonnet")):
        if u.get(k):
            out.append((name, u[k], WINDOW_SECS.get(k, 7 * 86400), local.get(k)))
        if k == "seven_day":
            out += [(m["name"], m, 7 * 86400, local.get(m["name"])) for m in u.get("models") or []]
    return out


def window_pace(w, secs, now=None):
    """已用百分比比窗口已过去的时间多出多少个百分点；窗口刚开始（过去不到 3%）时不算。"""
    r = parse_iso(w.get("resets_at"))
    if not r:
        return None
    now = now or dt.datetime.now(dt.timezone.utc)
    elapsed = 100 - (r - now).total_seconds() / secs * 100
    if elapsed <= 3 or elapsed > 100:
        return None
    return w["utilization"] - elapsed


def window_estimate(w, local):
    """“整窗约 $1009 · 640 万 tokens”：本机这个窗口用掉的 ÷ 已用百分比。用得太少时不估。"""
    if not local or not local.get("cost_usd"):
        return ""
    if w["utilization"] < 5:
        return "整窗估算：用量还少，数据不足"
    k = 100 / w["utilization"]
    return f"整窗约 ${local['cost_usd'] * k:,.0f} · {fmt_count(local['tokens'] * k)} tokens（本机已用 ${local['cost_usd']:,.2f}，按 API 价折算）"


def text_width(s):
    """终端里占几列：中文等宽字符算两列。"""
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in s)


def fmt_count(n):
    return f"{n / 1e8:.1f} 亿" if n >= 1e8 else f"{n / 1e4:.0f} 万" if n >= 1e5 else f"{n / 1e4:.1f} 万" if n >= 1e4 else f"{n:.0f}"


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
        plan = (a.get("plan_display") or "").upper() + (f" {plan_tier(a)}" if plan_tier(a) else "")
        head = f"{mark} {i}. {color(a['label'] or '-', 'b')}  [{KIND_NAME[a['kind']]}]"
        if plan:
            head += f" {color(plan, 'c')}"
        if a.get("email") and a["email"] != a["label"]:
            head += color(f"  {a['email']}", "d")
        if a["kind"] == "claude_desktop" and a["is_active"]:
            head += color("  [上次打开]", "d")
        print(head + color(f"  id={a['id']}", "d"))
        dl = a.get("days_left")
        if dl is not None:
            exp = (a.get("manual") or {}).get("expires_at")
            tag = f"{dl} 天后到期（{exp}）" if dl >= 0 else f"已过期 {-dl} 天（{exp}）"
            if (a.get("manual") or {}).get("auto_renew"):
                tag += " 自动续期"
            print("     " + color(tag, "r" if dl < 3 else "y" if dl < 7 else "d"))
        u = a.get("usage") or {}
        if a["kind"] == "claude_desktop":
            source = {"desktop_cache": "桌面本地缓存", "desktop_history": "桌面历史快照"}.get(u.get("source"), "尚无桌面额度快照")
            sampled = parse_iso(u.get("fetched_at"))
            record = sampled.astimezone().strftime("%Y-%m-%d %H:%M:%S") if sampled else "尚未采样"
            old = " · 旧快照（超过 30 分钟）" if u.get("cache_stale") and sampled else ""
            print("     " + color(f"{source} · 记录时间 {record}{old}", "d"))
            if u.get("notice"):
                print("     " + color(u["notice"], "d"))
        for name, w, secs, local in usage_windows(u):
            pace = window_pace(w, secs) if a["kind"] != "claude_desktop" else None
            tail = fmt_reset(w.get("resets_at"))
            if pace is not None and abs(pace) >= 1:
                tail += f" · 配速 {pace:+.0f}%"
            print(f"     {name}{' ' * max(1, 7 - text_width(name))}{bar(w['utilization'])} {w['utilization']:5.1f}%  {color(tail, 'y' if (pace or 0) >= 15 else 'd')}")
            est = window_estimate(w, local)
            if est:
                print("            " + color(est, "d"))
        if u.get("breakdown"):
            print("     " + color("7 天构成：" + " · ".join(f"{BREAKDOWN_NAME.get(b['key'], b['name'])} {b['percent']:.0f}%"
                                                         for b in u["breakdown"]), "d"))
        if u.get("cost_month_usd") is not None:
            print(f"     本月花费 ${u['cost_month_usd']:.2f}")
        if a["kind"] == "api" and a.get("api_key_hint"):
            ok = u.get("key_valid")
            print(f"     Key {a['api_key_hint']} " + ("" if ok is None else color("有效", "g") if ok else color("无效", "r")))
        for iss in (a.get("health") or {}).get("issues") or []:
            c = {"error": "r", "warn": "y"}.get(iss["level"], "d")
            print("     " + color(f"{'✗ 异常' if iss['level'] == 'error' else '⚠ 注意' if iss['level'] == 'warn' else '·'}：{iss['text']}。{iss['advice']}", c))
        if (a.get("manual") or {}).get("note"):
            print("     " + color("备注：" + a["manual"]["note"], "d"))
    if st.get("warning"):
        print("\n" + color("⚠ " + st["warning"], "y"))
    pend = st.get("pending_switch")
    if pend:
        tail = "" if pend.get("reason") == "rule" else f"；{PENDING_SWITCH_HOURS} 小时内没等到就作废"
        print("\n" + color(f"⏳ {pend['message']}（cam switch --cancel-wait 取消{tail}）", "c"))
    risks = [r for r in st.get("risks") or [] if r["level"] in ("warn", "bad")]
    for i, r in enumerate(risks):
        print(("\n" if i == 0 else "") + color(f"! {r['text']}。{r['advice']}", "y"))
    sc = st.get("risk_score") or {}
    if sc and (st.get("region") or {}).get("status"):  # 后台检测过才有意义
        c = {"low": "g", "medium": "y", "high": "r"}[sc["grade"]]
        print(("" if risks else "\n") + color(f"环境评分 {sc['score']}/100：{sc['label']}", c)
              + color("（据后台缓存；cam doctor 实测并查看完整的服务检查）", "d"))


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


def add_account(store, kind, label, email=None, session_key=None, api_key=None, admin_key=None, app_mode=False,
                use_current=False):
    if kind not in ("claude_web", "api", "claude_desktop"):
        raise CamError("kind 只能是 claude_web、claude_desktop 或 api（Claude Code 账号请用导入）")
    if kind == "api" and not api_key and not admin_key:
        raise CamError("API 账号至少需要 API Key 或 Admin Key")
    label = (label or "").strip()[:60] or ("API" if kind == "api" else "桌面账号" if kind == "claude_desktop" else "网页账号")
    with store.lock():
        data = store.load()
        if kind == "claude_desktop" and use_current and any(
                a.get("kind") == "claude_desktop" and a.get("desktop_profile") == "default" for a in data["accounts"]):
            raise CamError("已经有一个账号绑定了当前的 Claude 桌面版登录。其它号不要勾选，第一次打开时再登录。")
        acc = {"id": new_id(), "kind": kind, "label": label, "email": email or None,
               "plan": "api" if kind == "api" else None, "manual": {"app_mode": bool(app_mode)} if kind == "claude_web" else {},
               "usage": None, "created_at": now_iso()}
        if kind == "claude_desktop":
            if use_current:
                acc["desktop_profile"] = "default"
            else:
                acc["desktop_profile"] = acc["id"]
                profile = os.path.join(store.root, "desktop-profiles", acc["id"])
                os.makedirs(profile, mode=0o700, exist_ok=True)
                os.chmod(profile, 0o700)
        sec = {k: v.strip() for k, v in (("session_key", session_key), ("api_key", api_key), ("admin_key", admin_key)) if v}
        if sec:
            store.save_secret(acc["id"], sec)
        data["accounts"].append(acc)
        store.save(data)
        return acc


def cmd_import(store, args):
    acc = import_current(store, args.label)
    print(f"已保存：{acc['label']}（{acc.get('email') or '-'}）id={acc['id']}")
    print("要添加另一个账号：运行 cam login 名字。它会先把当前账号最新的登录存好，再打开浏览器登录新账号，之前的账号不会丢。")


def claude_auth_login(exe):
    """在当前终端里运行 claude auth login（浏览器登录，替换 Claude Code 当前的登录）。返回退出码。"""
    return subprocess.call([exe, "auth", "login"])


def cmd_login(store, args):
    """登录一个 Claude Code 账号并存进来：先把当前登录存回它的账号，再 claude auth login，最后导入新登录。

    直接在 claude 里 /login 的话，当前账号在上次保存之后续过的 token 还没存进来，换号后就丢了，这个号就得重新登录。
    """
    exe = shutil.which("claude")
    if not exe:
        raise CamError("找不到 claude 命令，请先安装 Claude Code（cam env install）")
    with store.lock():
        data = store.load()
        before = {a["id"] for a in data["accounts"]}
        cur = snapshot_live(store, data)  # 和运行 cam 时一样：最新 token 存回它的账号，没见过的号自动导入
        unverified = data.get("live_unverified")
        store.save(data)
    if unverified:
        raise CamError(unverified["message"] + "为了不丢掉它，这次没有换号登录，处理好后再运行 cam login。")
    if cur:
        print(("已保存当前登录：" if cur["id"] in before else "当前登录的是新账号，已保存：")
              + f"{cur['label']}（{cur.get('email') or '-'}）")
    print("正在运行 claude auth login，按提示在浏览器里登录要添加的账号…", flush=True)
    rc = claude_auth_login(exe)
    if rc != 0:
        raise CamError(f"claude auth login 没有完成（退出码 {rc}），之前的账号都已保存。"
                       "Claude Code 太旧没有这个命令的话，先运行 claude update")
    before = {a["id"] for a in store.load()["accounts"]}
    acc = import_current(store, args.label)
    print(color(f"✓ {'已添加' if acc['id'] not in before else '已更新'}：{acc['label']}（{acc.get('email') or '-'}）"
                f"id={acc['id']}", "g"))
    print("  Claude Code 现在登录的就是这个账号；cam switch <账号> 可以切回其它账号")
    warn = running_warning(running_claude_processes())
    if warn:
        print(color("  ⚠ " + warn, "y"))


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


def _interrupt(signum, frame):
    raise KeyboardInterrupt


def wait_and_switch(store, target, procs):
    """登记等待切换，然后在这里等到 claude 会话都退出再切。
    桌面版 / cam serve / 菜单栏插件也在跑的话，谁先发现会话退出谁切，这里只是报告结果。

    Ctrl-C、关掉终端（SIGHUP）、被 kill（SIGTERM）都撤掉登记：这里等着的人走了，不该留一个看不见的登记，
    几小时后突然把号换掉。出错时也撤掉。
    """
    old = {}
    for name in ("SIGHUP", "SIGTERM", "SIGBREAK"):  # SIGBREAK 是 Windows 上的 Ctrl-Break
        sig = getattr(signal, name, None)
        if sig is not None:
            try:
                old[sig] = signal.signal(sig, _interrupt)
            except (ValueError, OSError):  # 不在主线程里设不了，只能靠 Ctrl-C
                pass
    try:
        with store.lock():
            data = store.load()
            defer_switch(data, target["id"], "manual")
            store.save(data)
        print(f"等 {len(procs)} 个 claude 会话退出后自动切换到 {target['label']}…（Ctrl-C 或关掉终端都会取消）")
        shown = len(procs)
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
                    print(f"等待已被取消（或超过 {PENDING_SWITCH_HOURS} 小时作废），没有切换")
                return
            n = len(running_claude_processes())
            if n != shown:
                shown = n
                print(f"  还有 {n} 个会话在运行…")
            time.sleep(WAIT_POLL_SECS)
    except KeyboardInterrupt:
        cancel_pending_switch(store)
        try:
            print("\n已取消，没有切换")
        except OSError:  # 终端已经关了
            pass
    except CamError:
        cancel_pending_switch(store)  # 这次没切成：登记一起撤掉，免得后台过一会儿又悄悄去切
        raise
    finally:
        for sig, handler in old.items():
            signal.signal(sig, handler)


def cmd_switch(store, args):
    if args.cancel_wait:
        print("已取消等待中的切换" if cancel_pending_switch(store) else "没有等待中的切换")
        return
    if not args.account:
        raise CamError("请指定要切换到的账号")
    data = store.load()
    target = find_account(data, args.account)
    for r in risk_items(data):  # 只提醒，不拦
        if r["level"] in ("warn", "bad"):
            print(color(f"! {r['text']}。{r['advice']}", "y"))
    procs = running_claude_processes() if target["id"] != data["active"].get("claude_code") else []
    wait = args.wait
    asked = False
    if procs and not wait and not args.yes and not sys.stdin.isatty():
        # 没有终端可问（菜单栏、脚本）：不打断正在跑的会话，和面板里选“等”一样，登记后马上返回，
        # 由桌面版、cam serve 或菜单栏插件在会话都退出后切换
        r = api_switch(store, target["id"], "wait")
        print(r["message"] + ("（cam switch --cancel-wait 可取消）" if r.get("deferred") else ""))
        return
    if procs and not wait and not args.yes:
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


def cmd_risk(store, args):
    """查看 / 设置 cam claude 启动前的风险提醒档位，以及外部 IP 查询（隐私）设置。"""
    if args.on or args.off or args.confirm is not None or args.strong is not None:
        p = save_risk_prefs(store, prompt=True if args.on else False if args.off else None,
                            confirm=args.confirm, strong=args.strong)
        print("已保存。", end="")
    body = {k: v for k, v in (("ip_lookup", args.ip_lookup), ("ipapi_key", args.ipapi_key),
                                 ("ipinfo_token", args.ipinfo_token)) if v is not None}
    if body:
        save_privacy(store, body)
        print("已保存外部 IP 查询设置。")
    if args.clear_history:
        clear_region_history(store)
        print("已清除出口信息和账号使用地区记录。")
    data = store.load()
    p = risk_prefs(data)
    sc = risk_score(risk_items(data))
    reg = region_view(data)
    state = "开" if p["risk_prompt"] else "关"
    print(f"启动前风险提醒：{state}（CAM_NO_RISK_PROMPT=1 也可临时关掉）")
    print(f"  90 分以上：一行带过，直接启动")
    print(f"  {p['risk_confirm_below']}–89 分：打印提醒，直接启动")
    print(f"  {p['risk_strong_below']}–{p['risk_confirm_below'] - 1} 分：问“是否继续 [y/N]”")
    print(f"  {p['risk_strong_below']} 分以下，或出口不在支持名单内：要输入“继续”才启动")
    if reg["status"]:
        print(f"当前（据后台缓存）：{sc['score']}/100，{sc['label']}；档位 {launch_tier(sc['score'], reg['status'], p)}")
    else:
        print("当前：还没有检测过，运行 cam doctor 或 cam claude 时会检测")
    pv = privacy_view(store)
    print(f"\n外部 IP 查询：{pv['ip_lookup']}（{IP_LOOKUP_MODES[pv['ip_lookup']]}）"
          + ("，被环境变量 CAM_IP_LOOKUP 覆盖" if pv["env_override"] else ""))
    print(f"  ipapi.is key：{'已配置' if pv['has_ipapi_key'] else '没配'}；ipinfo token：{'已配置' if pv['has_ipinfo_token'] else '没配'}"
          f"；出口 IP 没变时运营商信息 {pv['meta_ttl_hours']:g} 小时内不重查")
    print("  会访问：" + "；".join(s["name"] for s in pv["sources"]) + "（只发出口 IP，不发账号信息）")
    print("  改法：cam risk --ip-lookup auto|ip-api|off，--ipapi-key KEY（空字符串删除），--ipinfo-token TOKEN，--clear-history")


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
        with urllib.request.urlopen(req, timeout=timeout, context=SSL_CTX) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except (urllib.error.URLError, socket.timeout, OSError, http.client.HTTPException, ValueError) as e:
        return 0, f"网络错误：{getattr(e, 'reason', e)}"


# Anthropic 官方公布的支持国家/地区（https://www.anthropic.com/supported-countries，2026-10 抓取），
# ISO 3166-1 alpha-2。名单会变，变了改这里。这里的判断只用来提示，不拦任何操作。
SUPPORTED_REGIONS = set("""
AL DZ AD AO AG AR AM AU AT AZ BS BH BD BB BE BZ BJ BT BO BA BW BR BN BG BF BI CV KH CM CA CF TD CL CO KM CG CD
CR CI HR CY CZ DK DJ DM DO EC EG SV GQ ER EE SZ ET FJ FI FR GA GM GE DE GH GR GD GT GN GW GY HT HN HU IS IN ID
IQ IE IL IT JM JP JO KZ KE KI KW KG LA LV LB LS LR LY LI LT LU MG MW MY MV ML MT MH MR MU MX FM MD MC MN ME MA
MZ NA NR NP NL NZ NI NE NG MK NO OM PK PW PS PA PG PY PE PH PL PT QA RO RW KN LC VC WS SM ST SA SN RS SC SL SG
SK SI SO SB ZA KR SS ES LK SD SR SE CH TW TJ TZ TH TL TG TO TT TN TR TM TV UG UA AE GB US UY UZ VU VA VN ZM ZW
""".split())
# 官方名单写的是国家；这些属地按所属国家看待（美国、英国、法国、荷兰、丹麦、芬兰、挪威、新西兰、澳大利亚）
REGION_DEPENDENCIES = set("""
PR GU VI AS MP GI IM JE GG BM KY FK VG RE GP MQ GF YT NC PF PM WF BL MF AW CW SX BQ GL FO AX SJ CK NU TK CX CC NF
""".split())
# 给提示用的中文名，只收常见的
REGION_NAMES = {"CN": "中国大陆", "HK": "香港", "MO": "澳门", "RU": "俄罗斯", "IR": "伊朗", "KP": "朝鲜", "SY": "叙利亚",
                "CU": "古巴", "BY": "白俄罗斯", "VE": "委内瑞拉", "AF": "阿富汗", "MM": "缅甸", "YE": "也门", "T1": "Tor 出口",
                "US": "美国", "JP": "日本", "SG": "新加坡", "TW": "台湾", "KR": "韩国", "GB": "英国", "DE": "德国",
                "FR": "法国", "NL": "荷兰", "CA": "加拿大", "AU": "澳大利亚", "MY": "马来西亚", "TH": "泰国", "VN": "越南",
                "ID": "印尼", "PH": "菲律宾", "IN": "印度", "AE": "阿联酋", "TR": "土耳其"}
REGION_ADVICE = {
    "supported": "出口 IP 这一项没问题。Anthropic 结合 IP 和其他信号综合判断所在地，下面的代理 / VPN、时区、出口变化等就是这些信号里本机能看到的部分。",
    "unsupported": "按 Anthropic 政策这个地区不提供服务，继续使用可能导致账号被限制或封禁。建议换到支持地区的网络出口后再使用。",
    "unknown": "连不上 Cloudflare，判断不了出口地区。检查网络或代理后重新检测。",
}


STATUS_URL = os.environ.get("CAM_STATUS_URL", "https://status.anthropic.com/api/v2/status.json")
CF_META_URL = os.environ.get("CAM_CF_META_URL", "https://speed.cloudflare.com/meta")  # 出口 IP 的运营商、城市
IPINFO_URL = os.environ.get("CAM_IPINFO_URL", "https://ipinfo.io/json")  # Cloudflare 的 meta 接口拒绝时退到这里
# 出口 IP 的运营商 / 城市 / 类型。默认顺序 ipapi.is → ipinfo.io → Cloudflare，都可以商用：
#   ipapi.is 免费注册 key 每天 1000 次、带机房 / VPN / Tor 标记，官方写明可商用；不带 key 每天 30 次、不带标记
#   ipinfo.io 免费版可商用但要求注明来源（界面上标“数据：ipinfo.io”），不带类型标记
# ip-api.com 免费接口仅限非商业用途、只有 http，只在设置里选了“ip-api”时才用
IPAPI_IS_URL = os.environ.get("CAM_IPAPI_IS_URL", "https://api.ipapi.is/")
IPAPI_URL = os.environ.get("CAM_IPAPI_URL",
                           "http://ip-api.com/json/?fields=status,countryCode,city,regionName,isp,org,as,mobile,proxy,hosting,query")
IP_LOOKUP_MODES = {
    "auto": "自动：ipapi.is → ipinfo.io → Cloudflare（都可以商用）",
    "ip-api": "自动，并优先用 ip-api.com（带机房 / 代理标记；仅限非商业用途，只有 http）",
    "off": "关闭：只查 claude.ai 自己的出口信息和 Anthropic 状态页，不查运营商和 IP 类型",
}
IP_LOOKUP_TITLES = {  # 界面上的短标题和一句话说明，和 Rust 版 health::IP_LOOKUP_TITLES 一致
    "auto": ("自动（推荐）", "ipapi.is → ipinfo.io → Cloudflare，都可以商用"),
    "ip-api": ("加上 ip-api.com", "多一个带机房 / 代理标记的来源；仅限非商业用途，只有 http"),
    "off": ("关闭", "只看出口国家和官方状态，不查运营商和 IP 类型"),
}
LOOKUP_SECRET_ID = "_ip-lookup"  # ipapi.is 的 key、ipinfo 的 token 存在 secrets 目录，不进 accounts.json
META_TTL = int(os.environ.get("CAM_META_TTL", "86400"))  # 出口 IP 没变时，运营商 / 类型 / 注册国多久重查一次（秒）
META_FIELDS = ("asn", "org", "city", "meta_loc", "ip_type", "reg_loc", "meta_source", "meta_at")
STACK_FIELDS = ("v4_ip", "v4_loc", "v6_ip", "v6_loc")
RDAP_URL = os.environ.get("CAM_RDAP_URL", "https://rdap.org/ip/")  # IP 的注册国（原生 / 广播 IP）；空字符串表示不查
CF_TRACE4_URL = os.environ.get("CAM_CF_TRACE4_URL", "https://1.1.1.1/cdn-cgi/trace")  # 只走 IPv4 的出口
CF_TRACE6_URL = os.environ.get("CAM_CF_TRACE6_URL", "https://[2606:4700:4700::1111]/cdn-cgi/trace")  # 只走 IPv6 的出口
IP_TYPE_NAMES = {"residential": "家庭 / 公司宽带", "hosting": "机房 / 云服务器", "proxy": "代理 / VPN 出口", "tor": "Tor 出口节点",
                 "mobile": "手机网络"}
# 公共 DNS → (名称, 国家)。Google / Cloudflare / Quad9 这类任播的全球都有节点，国家记 None，不拿来判断位置
KNOWN_DNS = {
    "8.8.8.8": ("Google", None), "8.8.4.4": ("Google", None), "2001:4860:4860::8888": ("Google", None),
    "2001:4860:4860::8844": ("Google", None), "1.1.1.1": ("Cloudflare", None), "1.0.0.1": ("Cloudflare", None),
    "2606:4700:4700::1111": ("Cloudflare", None), "2606:4700:4700::1001": ("Cloudflare", None), "9.9.9.9": ("Quad9", None),
    "149.112.112.112": ("Quad9", None), "208.67.222.222": ("OpenDNS", None), "208.67.220.220": ("OpenDNS", None),
    "223.5.5.5": ("阿里 DNS", "CN"), "223.6.6.6": ("阿里 DNS", "CN"), "2400:3200::1": ("阿里 DNS", "CN"),
    "2400:3200:baba::1": ("阿里 DNS", "CN"), "119.29.29.29": ("腾讯 DNSPod", "CN"), "182.254.116.116": ("腾讯 DNSPod", "CN"),
    "114.114.114.114": ("114DNS", "CN"), "114.114.115.115": ("114DNS", "CN"), "180.76.76.76": ("百度 DNS", "CN"),
    "1.2.4.8": ("CNNIC", "CN"), "210.2.4.8": ("CNNIC", "CN"), "168.95.1.1": ("中华电信 HiNet", "TW"),
    "168.95.192.1": ("中华电信 HiNet", "TW"), "168.126.63.1": ("KT", "KR"), "168.126.63.2": ("KT", "KR"),
    "77.88.8.8": ("Yandex", "RU"), "77.88.8.1": ("Yandex", "RU"),
}
# 出口 IP 的运营商名里有这些就当作云服务商 / 机房（不是家庭或公司宽带）
HOSTING_WORDS = ("hosting", "cloud", "data center", "datacenter", "server", "vps", "dedicated", "colocation",
                 "amazon", "aws", "google llc", "google cloud", "microsoft corporation", "azure", "digitalocean", "linode",
                 "akamai", "vultr", "choopa", "ovh", "hetzner", "alibaba", "aliyun", "tencent", "huawei", "oracle", "m247",
                 "datacamp", "leaseweb", "contabo", "scaleway", "upcloud", "hostinger", "psychz", "quadranet",
                 "colocrossing", "zenlayer", "gcore", "g-core", "cloudflare", "packethub", "clouvider", "performive",
                 "bandwagon", "racknerd", "ucloud", "kamatera", "ionos", "fly.io", "oneprovider", "hostpapa")
REGION_CHANGE_HOURS = 24      # 出口地区变了以后，提醒保留多久
ACCOUNT_REGION_DAYS = 7       # 同一账号几天内从多个地区使用过就提醒
ACCOUNT_REGION_KEEP = 8       # 每个账号最多记几个地区
CLOCK_SKEW_WARN = 120         # 本机时钟与服务器相差超过这么多秒就提醒
REGION_FRESH_SECS = 3600      # 切换账号时，多久以内的出口地区检测结果算有效
# 默认路由走这些接口就当作 VPN / 隧道（macOS utun、Linux tun/wg、各家 VPN 客户端）
TUNNEL_PREFIXES = ("utun", "tun", "tap", "wg", "ppp", "ipsec", "tailscale", "zt", "nebula", "proton", "nordlynx",
                   "mullvad", "clash", "mihomo", "sing", "hysteria", "surge")
TUNNEL_WORDS = ("tap", "tun", "wintun", "wireguard", "vpn", "tunnel", "nordlynx", "clash", "mihomo", "sing-box",
                "surge", "shadowsocks", "v2ray", "hysteria", "tailscale", "zerotier", "anyconnect", "fortinet",
                "globalprotect", "pulse secure", "openvpn", "expressvpn", "mullvad", "proton")


def region_label(loc):
    return f"{loc}（{REGION_NAMES[loc]}）" if loc in REGION_NAMES else (loc or "未知")


def region_status(loc):
    """supported / unsupported / unknown。"""
    if not loc:
        return "unknown"
    return "supported" if loc in SUPPORTED_REGIONS or loc in REGION_DEPENDENCIES else "unsupported"


def region_message(loc, status):
    if status == "unknown":
        return "出口地区：查不到"
    return f"出口地区 {region_label(loc)} " + ("在 Anthropic 支持名单内" if status == "supported" else "不在 Anthropic 支持名单内")


def fmt_ago(s):
    d = parse_iso(s)
    if not d:
        return ""
    secs = max(0, int((dt.datetime.now(dt.timezone.utc) - d).total_seconds()))
    if secs < 60:
        return "刚刚"
    h, m = divmod(secs // 60, 60)
    if h >= 24:
        return f"{h // 24} 天前"
    return f"{h} 小时前" if h else f"{m} 分钟前"


def parse_trace(txt, t0):
    kv = dict(line.split("=", 1) for line in txt.splitlines() if "=" in line)
    skew = None
    try:
        skew = int(round(time.time() - float(kv["ts"])))
    except (KeyError, ValueError):
        pass
    return {"loc": (kv.get("loc") or "").strip().upper() or None, "ip": kv.get("ip"), "colo": kv.get("colo"),
            "ms": int((time.time() - t0) * 1000),
            "warp": kv.get("warp", "off").strip() in ("on", "plus") or kv.get("gateway", "off").strip() == "on",
            "skew": skew}


def quick_trace(timeout=3):
    """只问 claude.ai 一次出口信息（启动 claude 前用，最多等 timeout 秒）。查不到返回 None。"""
    t0 = time.time()
    st, txt = http_text(f"{CLAUDE_WEB_BASE}/cdn-cgi/trace", timeout)
    if st != 200 or "loc=" not in txt:
        return None
    return parse_trace(txt, t0)


def _age_secs(iso):
    t = parse_iso(iso)
    return (dt.datetime.now(dt.timezone.utc) - t).total_seconds() if t else None


def detect_region(timeout=6, cfg=None, prev=None):
    """从 Cloudflare 看出口地区（claude.ai 在它后面，它看到的就是 claude.ai 看到的），再并行查运营商 / 类型 / 注册国 /
    IPv4 / IPv6 出口。出口 IP 和数据源都没变、且 META_TTL 内查过时，运营商那几项直接沿用 prev，不再问第三方。
    设置为 off 时只查 claude.ai。返回 dict 或 None（claude.ai 都连不上）。"""
    cfg = cfg or ip_lookup_config()
    off = cfg["mode"] == "off"
    t0 = time.time()
    st, txt = http_text(f"{CLAUDE_WEB_BASE}/cdn-cgi/trace", timeout)
    if (st != 200 or "loc=" not in txt) and not off:
        st, txt = http_text("https://www.cloudflare.com/cdn-cgi/trace", timeout)
    if st != 200 or "loc=" not in txt:
        return None
    out = parse_trace(txt, t0)
    out.update({k: None for k in META_FIELDS + STACK_FIELDS})
    out["lookup_mode"] = cfg["mode"]
    if off:
        return out
    prev = prev or {}
    age = _age_secs(prev.get("meta_at"))
    reuse = (bool(out["ip"]) and prev.get("ip") == out["ip"] and prev.get("lookup_mode") == cfg["mode"]
             and age is not None and age < META_TTL)
    jobs = {"v4": lambda: trace_only(CF_TRACE4_URL), "v6": lambda: trace_only(CF_TRACE6_URL)}
    if not reuse:
        jobs["meta"] = lambda: fetch_exit_meta(cfg)
        if not cfg["ipapi_key"]:  # 带 key 的 ipapi.is 自己会给注册国
            jobs["rdap"] = lambda: fetch_ip_registration(out["ip"])
    with ThreadPoolExecutor(max_workers=len(jobs)) as ex:
        futs = {k: ex.submit(f) for k, f in jobs.items()}
        res = {k: f.result() for k, f in futs.items()}
    if reuse:
        out.update({k: prev.get(k) for k in META_FIELDS})
    else:
        if res.get("meta"):
            out.update(res["meta"])
        if not out["reg_loc"]:
            out["reg_loc"] = res.get("rdap") if "rdap" in res else fetch_ip_registration(out["ip"])
        if out["org"] or out["meta_loc"]:  # 查到了才缓存；全失败的话下次重查
            out["meta_at"] = now_iso()
    if res["v4"]:
        out["v4_ip"], out["v4_loc"] = res["v4"]
    if res["v6"]:
        out["v6_ip"], out["v6_loc"] = res["v6"]
    return out


def ip_lookup_config(store=None):
    """外部 IP 查询的设置 {"mode", "ipapi_key", "ipinfo_token"}。环境变量 CAM_IP_LOOKUP / CAM_IPAPI_IS_KEY /
    CAM_IPINFO_TOKEN 优先于保存的设置。"""
    mode, key, tok = "auto", "", ""
    if store is not None:
        try:
            mode = (store.load().get("prefs") or {}).get("ip_lookup") or "auto"
            sec = store.load_secret(LOOKUP_SECRET_ID)
            key, tok = sec.get("ipapi_key") or "", sec.get("ipinfo_token") or ""
        except Exception:  # noqa: BLE001  读不到设置就用默认值
            pass
    mode = os.environ.get("CAM_IP_LOOKUP") or mode
    return {"mode": mode if mode in IP_LOOKUP_MODES else "auto",
            "ipapi_key": os.environ.get("CAM_IPAPI_IS_KEY") or key,
            "ipinfo_token": os.environ.get("CAM_IPINFO_TOKEN") or tok}


def _with_param(url, name, value):
    if not value:
        return url
    return f"{url}{'&' if '?' in url else '?'}{name}={urllib.parse.quote(value)}"


def meta_sources(cfg):
    """按顺序要问的运营商数据源 [(名字, 地址)]；off 时为空。"""
    if cfg["mode"] == "off":
        return []
    out = [("ip-api.com", IPAPI_URL)] if cfg["mode"] == "ip-api" else []
    return out + [("ipapi.is", _with_param(IPAPI_IS_URL, "key", cfg["ipapi_key"])),
                  ("ipinfo.io", _with_param(IPINFO_URL, "token", cfg["ipinfo_token"])),
                  ("Cloudflare", CF_META_URL)]


def fetch_exit_meta(cfg=None, timeout=5):
    """出口 IP 的 ASN、运营商、城市、国家和类型，按 meta_sources 的顺序问，第一个查到的为准。查不到返回 None。"""
    for name, url in meta_sources(cfg or ip_lookup_config()):
        if not url:
            continue
        st, txt = http_text(url, timeout)
        if st != 200:
            continue
        try:
            m = parse_exit_meta(json.loads(txt))
        except json.JSONDecodeError:
            m = None
        if m:
            m["meta_source"] = name
            return m
    return None


def parse_exit_meta(j):
    """统一几种格式：ipapi.is {asn:{asn,org,country}, company:{type}, is_datacenter, is_vpn, is_proxy, is_tor, is_mobile, location:{…}}、
    ip-api.com {as, isp, org, countryCode, city, regionName, hosting, proxy, mobile}、
    Cloudflare {asn, asOrganization, city, region, country}、ipinfo {org: "AS7922 Comcast…", city, region, country}。
    返回 {asn, org, city, meta_loc, ip_type, reg_loc}；ip_type 为 residential / hosting / proxy / tor / mobile / None（这个来源不带标记）。"""
    if not isinstance(j, dict) or j.get("status") == "fail":
        return None
    asn, org, ip_type, reg_loc = None, None, None, None
    loc_src = j.get("location") if isinstance(j.get("location"), dict) else j
    if isinstance(j.get("asn"), dict):  # ipapi.is
        asn = j["asn"].get("asn")
        org = j["asn"].get("org") or (j.get("company") or {}).get("name")
        reg_loc = str(j["asn"].get("country") or "").strip().upper() or None
        ctype = (j.get("company") or {}).get("type")
        if j.get("is_tor"):
            ip_type = "tor"
        elif j.get("is_vpn") or j.get("is_proxy"):
            ip_type = "proxy"
        elif j.get("is_datacenter") or ctype == "hosting":
            ip_type = "hosting"
        elif j.get("is_mobile"):
            ip_type = "mobile"
        elif ctype:
            ip_type = "residential"
    else:
        asn = j.get("asn") if isinstance(j.get("asn"), int) else None
        org = j.get("asOrganization") or j.get("isp") or None
        as_field = (j.get("as") if isinstance(j.get("as"), str) else j.get("asn") if isinstance(j.get("asn"), str)
                    else (j.get("org") if not org else None))
        if as_field:
            head, _, rest = str(as_field).partition(" ")
            if head.startswith("AS") and head[2:].isdigit():
                asn = asn or int(head[2:])
                org = org or rest.strip() or None
            else:
                org = org or str(as_field)
        if not org and isinstance(j.get("company"), str):  # ipapi.is 不带 key 的简版
            org = j["company"]
        if "hosting" in j or "proxy" in j:  # ip-api.com
            ip_type = "proxy" if j.get("proxy") else "hosting" if j.get("hosting") else "mobile" if j.get("mobile") else "residential"
    city = ", ".join(x for x in (loc_src.get("city"), loc_src.get("region") or loc_src.get("regionName") or loc_src.get("state")) if x)
    country = loc_src.get("country_code") or j.get("countryCode") or loc_src.get("country") or ""
    loc = str(country).strip().upper() if len(str(country).strip()) == 2 else None
    if not org and not loc:
        return None
    return {"asn": asn, "org": org, "city": city or None, "meta_loc": loc, "ip_type": ip_type, "reg_loc": reg_loc}


def fetch_ip_registration(ip, timeout=6):
    """IP 在 RIR 登记的国家（RDAP）。查不到返回 None（ARIN 的分配记录常常没写国家）。"""
    if not RDAP_URL or not ip:
        return None
    st, txt = http_text(f"{RDAP_URL}{ip}", timeout)
    if st != 200:
        return None
    try:
        j = json.loads(txt)
    except json.JSONDecodeError:
        return None
    c = str((j or {}).get("country") or "").strip().upper() if isinstance(j, dict) else ""
    return c if len(c) == 2 else None


def trace_only(url, timeout=4):
    """只用某一个协议（IPv4 或 IPv6 的 IP 直连）访问 Cloudflare trace，返回 (ip, loc) 或 None（该协议不可用）。"""
    if not url:
        return None
    st, txt = http_text(url, timeout)
    if st != 200 or "loc=" not in txt:
        return None
    kv = dict(line.split("=", 1) for line in txt.splitlines() if "=" in line)
    return kv.get("ip"), (kv.get("loc") or "").strip().upper() or None


def is_private_ip(ip):
    import ipaddress
    try:
        a = ipaddress.ip_address(ip.split("%")[0])
    except ValueError:
        return False
    return a.is_private or a.is_loopback or a.is_link_local


def system_dns_servers():
    """本机配置的 DNS 服务器 [{"ip", "name", "country", "private"}]。只读本机配置，不往外发任何探测。
    测试用 CAM_TEST_DNS="8.8.8.8,223.5.5.5"（空字符串表示查不到）。"""
    t = os.environ.get("CAM_TEST_DNS")
    ips = []
    if t is not None:
        ips = [x.strip() for x in t.split(",") if x.strip()]
    elif IS_WIN:
        out = _run_text(["powershell", "-NoProfile", "-Command",
                         "Get-DnsClientServerAddress -ErrorAction SilentlyContinue | ForEach-Object { $_.ServerAddresses }"], timeout=15)
        ips = [l.strip() for l in out.splitlines() if l.strip()]
    elif IS_MAC:
        for line in _run_text(["scutil", "--dns"]).splitlines():
            line = line.strip()
            if line.startswith("nameserver[") and ":" in line:
                ips.append(line.split(":", 1)[1].strip())
    else:
        try:
            with open("/etc/resolv.conf", encoding="utf-8") as f:
                ips = [l.split()[1] for l in f if l.startswith("nameserver") and len(l.split()) > 1]
        except OSError:
            ips = []
        if any(ip.startswith("127.0.0.53") for ip in ips):  # systemd-resolved 的本机代理，真正的上游看 resolvectl
            for line in _run_text(["resolvectl", "status"]).splitlines():
                if "DNS Servers:" in line or "Current DNS Server:" in line:
                    ips.extend(line.split(":", 1)[1].split())
    out, seen = [], set()
    for ip in ips:
        ip = ip.strip().split("%")[0]
        if not ip or ip in seen or ip.startswith("127.0.0.53"):
            continue
        seen.add(ip)
        name, country = KNOWN_DNS.get(ip, (None, None))
        out.append({"ip": ip, "name": name, "country": country, "private": is_private_ip(ip)})
    return out[:6]


PROXY_ENV_KEYS = ("HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy")


def proxy_parts():
    """当前生效的代理：环境变量 + 系统设置，例如 ["HTTPS_PROXY=…", "系统设置 HTTPS 127.0.0.1:7890"]。"""
    return ([f"{k}={os.environ[k]}" for k in PROXY_ENV_KEYS if os.environ.get(k)]
            + [f"系统设置 {p}" for p in system_proxy()])


def local_env():
    """本机的区域设置、代理和 VPN 快照，和出口一起缓存，免得界面每次刷新都去跑系统命令。
    顶部环境卡和服务检测都用这一份，两边的分数才一致。"""
    tz = system_timezone()
    lc = system_locale()
    tun = default_route_tunnel()
    return {"tz": tz[0] if tz else None, "tz_cc": tz[1] if tz else [],
            "locale": lc[0] if lc else None, "locale_cc": lc[1] if lc else None, "dns": system_dns_servers(),
            "proxy": proxy_parts(), "tunnel": list(tun) if tun else None}


def proxy_vpn_items(env):
    """代理、VPN 两行（服务检测和顶部环境卡共用）。env 是 local_env() 的结果。"""
    out = []
    parts = env.get("proxy") or []
    out.append({"kind": "proxy", "level": "info" if parts else "ok", "text": "代理：" + ("；".join(parts) if parts else "无（直连）"),
                "advice": "请求经过代理，上面的出口地区是代理的出口，不是本机网络的出口" if parts else "直连，出口地区就是本机网络的出口"})
    tun = env.get("tunnel")
    if tun:
        out.append({"kind": "vpn", "level": "info", "text": f"VPN：默认路由走 {tun[0]}（{tun[1]}）",
                    "advice": "出口地区是 VPN 的出口，不代表本机所在地；Anthropic 按使用者实际所在地判断是否支持。"})
    else:
        out.append({"kind": "vpn", "level": "ok", "text": "VPN：默认路由没有经过隧道接口",
                    "advice": "只看本机默认路由；透明代理和路由器上的 VPN 看不出来"})
    return out


LOCALE_ADVICE = ("系统语言只代表语言习惯，不代表所在地，所以只作参考、扣 5 分。想让它和所在地一致：在系统设置的“语言与地区”里"
                 "把“地区”改成你实际所在的国家（语言可以继续用中文）。")


def asn_label(asn):
    return f"AS{asn}" if asn else "AS?"


def hosting_exit(org):
    """运营商名像云服务商 / 机房时为 True（这类出口通常被当作 VPN / 代理流量）。"""
    low = (org or "").lower()
    return bool(low) and any(w in low for w in HOSTING_WORDS)


def system_locale():
    """本机系统语言 / 区域 (名称, 国家代码) 或 None。测试用 CAM_TEST_LOCALE="zh_CN"（空字符串表示查不到）。"""
    t = os.environ.get("CAM_TEST_LOCALE")
    if t is not None:
        name = t
    elif IS_MAC:
        name = _run_text(["defaults", "read", "-g", "AppleLocale"]).strip()
    elif IS_WIN:
        name = _run_text(["powershell", "-NoProfile", "-Command", "(Get-Culture).Name"], timeout=15).strip()
    else:
        name = os.environ.get("LC_ALL") or os.environ.get("LC_MESSAGES") or os.environ.get("LANG") or ""
    name = name.split(".", 1)[0].split("@", 1)[0].strip()
    if not name or name.upper() in ("C", "POSIX"):
        return None
    parts = name.replace("-", "_").split("_")
    cc = next((p.upper() for p in parts[1:] if len(p) == 2 and p.isalpha()), None)
    return name, cc


def fetch_status_page(timeout=6):
    """Anthropic 官方状态页（status.anthropic.com）的总体状态：{indicator, description} 或 None（连不上）。"""
    st, txt = http_text(STATUS_URL, timeout)
    if st != 200:
        return None
    try:
        status = (json.loads(txt) or {}).get("status") or {}
    except (json.JSONDecodeError, AttributeError):
        return None
    if not isinstance(status, dict) or not status.get("indicator"):
        return None
    return {"indicator": str(status["indicator"]), "description": str(status.get("description") or "")}


def system_timezone():
    """本机时区 (时区名, [国家代码]) 或 None。国家代码来自 tzdata 的 zone1970.tab，查不到时为空列表。

    测试用 CAM_TEST_TZ="Asia/Hong_Kong=HK"（空字符串表示查不到）。
    """
    t = os.environ.get("CAM_TEST_TZ")
    if t is not None:
        if not t:
            return None
        zone, _, cc = t.partition("=")
        return zone, [c for c in cc.split(",") if c]
    zone = (os.environ.get("TZ") or "").lstrip(":")
    if not zone and not IS_WIN:
        try:
            link = os.readlink("/etc/localtime")
            zone = link.split("zoneinfo/", 1)[1] if "zoneinfo/" in link else ""
        except OSError:
            zone = ""
    if not zone and IS_WIN:
        try:
            zone = subprocess.run(["tzutil", "/g"], capture_output=True, text=True, timeout=5).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            zone = ""
    if not zone:
        return None
    countries = []
    for tab in ("/usr/share/zoneinfo/zone1970.tab", "/var/db/timezone/zoneinfo/zone1970.tab",
                "/usr/share/zoneinfo/zone.tab", "/var/db/timezone/zoneinfo/zone.tab"):
        try:
            with open(tab, encoding="utf-8") as f:
                for line in f:
                    parts = line.rstrip("\n").split("\t")
                    if not line.startswith("#") and len(parts) >= 3 and parts[2] == zone:
                        countries = [c for c in parts[0].split(",") if c]
                        break
        except OSError:
            continue
        if countries:
            break
    return zone, countries


def decode_output(b):
    """子进程输出：先按 UTF-8，不行再按系统编码（Windows 上常是 GBK / OEM 代码页），解不了的字符用替换符，不抛异常。"""
    if not b:
        return ""
    try:
        return b.decode("utf-8")
    except UnicodeDecodeError:
        import locale  # noqa: PLC0415
        return b.decode(locale.getpreferredencoding(False) or "utf-8", errors="replace")


def _run_text(cmd, timeout=8):
    try:
        return decode_output(subprocess.run(cmd, capture_output=True, timeout=timeout).stdout)
    except (OSError, subprocess.SubprocessError, ValueError):
        return ""


def system_proxy():
    """系统级代理设置（不是环境变量）：["HTTPS 127.0.0.1:7890", "PAC http://…"] 之类，没有时为空列表。
    测试用 CAM_TEST_PROXY="HTTPS 127.0.0.1:7890"（空字符串表示没有）。"""
    t = os.environ.get("CAM_TEST_PROXY")
    if t is not None:
        return [x for x in t.split(",") if x]
    out = []
    if IS_MAC:
        kv = {}
        for line in _run_text(["scutil", "--proxy"]).splitlines():
            if " : " in line:
                k, _, v = line.strip().partition(" : ")
                kv[k] = v.strip()
        for name, key in (("HTTPS", "HTTPS"), ("HTTP", "HTTP"), ("SOCKS", "SOCKS")):
            if kv.get(f"{key}Enable") == "1" and kv.get(f"{key}Proxy"):
                out.append(f"{name} {kv[f'{key}Proxy']}:{kv.get(f'{key}Port', '')}")
        if kv.get("ProxyAutoConfigEnable") == "1":
            out.append(f"PAC {kv.get('ProxyAutoConfigURLString', '')}")
    elif IS_WIN:
        key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings"
        txt = _run_text(["reg", "query", key])
        kv = {}
        for line in txt.splitlines():
            parts = line.split(None, 2)
            if len(parts) == 3 and parts[1].startswith("REG_"):
                kv[parts[0]] = parts[2].strip()
        if kv.get("ProxyEnable", "").lower() in ("0x1", "1") and kv.get("ProxyServer"):
            out.append(f"系统代理 {kv['ProxyServer']}")
        if kv.get("AutoConfigURL"):
            out.append(f"PAC {kv['AutoConfigURL']}")
    elif shutil.which("gsettings"):
        mode = _run_text(["gsettings", "get", "org.gnome.system.proxy", "mode"]).strip().strip("'")
        if mode == "manual":
            host = _run_text(["gsettings", "get", "org.gnome.system.proxy.https", "host"]).strip().strip("'")
            port = _run_text(["gsettings", "get", "org.gnome.system.proxy.https", "port"]).strip()
            out.append(f"HTTPS {host}:{port}" if host else "系统代理（手动）")
        elif mode == "auto":
            out.append("PAC " + _run_text(["gsettings", "get", "org.gnome.system.proxy", "autoconfig-url"]).strip().strip("'"))
    return out


def tunnel_kind(name, desc=""):
    """接口名 / 描述看起来是 VPN 或隧道时返回说明，否则 None。"""
    low = (name or "").lower()
    if any(low.startswith(p) for p in TUNNEL_PREFIXES):
        return "VPN / 隧道接口"
    d = (desc or "").lower()
    if any(w in d for w in TUNNEL_WORDS):
        return "VPN / 隧道适配器"
    return None


def default_route_tunnel():
    """默认路由走的接口像 VPN / 隧道时返回 (接口名, 说明)，否则 None。只看默认路由，透明代理和路由器上的 VPN 看不出来。

    测试用 CAM_TEST_ROUTE_IF="utun4"（空字符串表示没有）。
    """
    t = os.environ.get("CAM_TEST_ROUTE_IF")
    if t is not None:
        k = tunnel_kind(t)
        return (t, k) if t and k else None
    name, desc = "", ""
    if IS_WIN:
        ps = ("$r = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 -ErrorAction SilentlyContinue | "
              "Sort-Object RouteMetric, InterfaceMetric | Select-Object -First 1; "
              "if ($r) { $a = Get-NetAdapter -InterfaceIndex $r.InterfaceIndex -ErrorAction SilentlyContinue; "
              "\"$($r.InterfaceAlias)`t$($a.InterfaceDescription)\" }")
        line = _run_text(["powershell", "-NoProfile", "-Command", ps], timeout=15).strip().splitlines()
        if line:
            name, _, desc = line[0].partition("\t")
    elif IS_MAC:
        for line in _run_text(["route", "-n", "get", "default"]).splitlines():
            if line.strip().startswith("interface:"):
                name = line.split(":", 1)[1].strip()
    else:
        words = _run_text(["ip", "route", "show", "default"]).split()
        if "dev" in words:
            name = words[words.index("dev") + 1] if words.index("dev") + 1 < len(words) else ""
    k = tunnel_kind(name, desc)
    return (name, k) if name and k else None


def note_account_region(data, acc_id, loc, reg=None):
    """记一笔“这个账号在这个出口地区用过”，给风险提示和诊断导出用（每个账号只留最近几个地区）。

    reg 是当时的出口检测结果，有的话把 IP、运营商、城市、IP 类型一起记下；同一地区只留一条，次数累加、首次时间不变。
    """
    if not acc_id or not loc:
        return
    for a in data.get("accounts") or []:
        if a["id"] == acc_id:
            old = next((r for r in (a.get("regions") or []) if r.get("loc") == loc), None) or {}
            regs = [r for r in (a.get("regions") or []) if r.get("loc") != loc]
            entry = {"loc": loc, "at": now_iso(), "first_at": old.get("first_at") or old.get("at") or now_iso(),
                     "times": int(old.get("times") or 0) + 1}
            for k in ("ip", "org", "city", "ip_type"):
                v = (reg or {}).get(k) or old.get(k)
                if v:
                    entry[k] = v
            regs.insert(0, entry)
            a["regions"] = regs[:ACCOUNT_REGION_KEEP]


def cached_region_loc(data, max_age=REGION_FRESH_SECS):
    """后台最近一次检测到的出口地区；超过 max_age 秒或没检测过时为 None。"""
    reg = data.get("region") or {}
    at = parse_iso(reg.get("checked_at"))
    if reg.get("loc") and at and (dt.datetime.now(dt.timezone.utc) - at).total_seconds() <= max_age:
        return reg["loc"]
    return None


def region_view(data):
    """界面常显用的出口地区状态（不联网，用后台上次检测的结果）。没检测过时 status 为 None。"""
    c = data.get("region") or {}
    if not c.get("checked_at"):
        return {"loc": None, "ip": None, "status": None, "checked_at": None, "message": "还没有检测过出口地区", "advice": "",
                "warp": False, "prev_loc": None, "changed_at": None, "org": None, "city": None, "ip_type": None,
                "meta_source": None, "lookup_mode": None}
    status = region_status(c.get("loc"))
    return {"loc": c.get("loc"), "ip": c.get("ip"), "status": status, "checked_at": c["checked_at"],
            "message": region_message(c.get("loc"), status), "advice": REGION_ADVICE[status],
            "warp": bool(c.get("warp")), "prev_loc": c.get("prev_loc"), "changed_at": c.get("changed_at"),
            "org": c.get("org"), "city": c.get("city"), "ip_type": c.get("ip_type"), "meta_source": c.get("meta_source"),
            "lookup_mode": c.get("lookup_mode")}


def check_region(store, max_age=300):
    """后台定时调用：检测出口地区（连带 WARP、时钟偏差）和 Anthropic 官方状态页，记到 data["region"] / data["status_page"]，
    并给当前账号记一笔地区。max_age 秒内查过就跳过。只记录，不干预任何操作。"""
    data = store.load()
    at = parse_iso((data.get("region") or {}).get("checked_at"))
    if at and (dt.datetime.now(dt.timezone.utc) - at).total_seconds() < max_age:
        return region_view(data)
    cfg = ip_lookup_config(store)
    with ThreadPoolExecutor(max_workers=3) as ex:  # 出口、状态页、本机设置三件事并行，互不等待
        f_r = ex.submit(detect_region, 6, cfg, data.get("region") or {})
        f_sp = ex.submit(fetch_status_page)
        f_env = ex.submit(local_env)
        r, sp, env = f_r.result() or {}, f_sp.result(), f_env.result()
    with store.lock():
        data = store.load()
        old = data.get("region") or {}
        reg = {k: r.get(k) for k in ("loc", "ip", "colo", "ms", "skew") + META_FIELDS + STACK_FIELDS}
        reg.update(warp=bool(r.get("warp")), lookup_mode=cfg["mode"], env=env, checked_at=now_iso())
        last = old.get("loc") or old.get("last_loc")  # 中间失败过一次也和最后一次知道的出口比
        reg["last_loc"] = reg["loc"] or last
        if last and reg["loc"] and last != reg["loc"]:
            reg["prev_loc"], reg["changed_at"] = last, reg["checked_at"]
        elif old.get("prev_loc"):
            reg["prev_loc"], reg["changed_at"] = old["prev_loc"], old.get("changed_at")
        data["region"] = reg
        if sp:
            data["status_page"] = dict(sp, checked_at=reg["checked_at"])
        else:
            data.pop("status_page", None)
        note_account_region(data, (data.get("active") or {}).get("claude_code"), reg["loc"], reg)
        ss = data.get("service_status")
        if reg["loc"] and isinstance(ss, dict):  # 连得上 claude.ai 了：只清掉它能证明已经好了的（DNS、证书、claude.ai 连不上）
            ss["items"] = [i for i in ss.get("items") or [] if not resolved_by_trace(i)]
            if not ss["items"]:
                data.pop("service_status", None)
        store.save(data)
    return region_view(data)


def resolved_by_trace(item):
    """后台连上了 claude.ai 的 trace，就能证明这条服务检测问题已经好了：DNS、证书、claude.ai 连不上。
    人机验证、服务端错误、api.anthropic.com 的问题证明不了，留到下次服务检测或一小时后过期。"""
    k = item.get("kind")
    return k in ("dns", "tls") or (k == "web" and "连不上" in (item.get("text") or ""))


def risk_items(data, include_region=True):
    """根据后台缓存和本机设置算出的风险提示（不联网）：[{"kind", "level", "text", "advice"}]。

    只提示，不拦任何操作；界面把 warn / bad 的常显在顶部，info 只在服务检查里列出。
    """
    out = []

    def add(kind, level, text, advice=""):
        out.append({"kind": kind, "level": level, "text": text, "advice": advice})

    now = dt.datetime.now(dt.timezone.utc)
    reg = data.get("region") or {}
    loc = reg.get("loc") if reg.get("checked_at") else None
    if include_region and loc and region_status(loc) == "unsupported":
        add("region", "warn", region_message(loc, "unsupported"), REGION_ADVICE["unsupported"])
    if include_region and reg.get("checked_at") and not loc:
        add("region_unknown", "warn", "出口地区查不到：上次检测连不上 claude.ai 和 Cloudflare", REGION_ADVICE["unknown"])
    if include_region and reg.get("checked_at"):  # 代理 / VPN：和服务检测用同一份缓存，扣分一致
        e = reg.get("env") if isinstance(reg.get("env"), dict) else {}
        if "proxy" in e:
            out.extend(dict(i) for i in proxy_vpn_items(e) if i["level"] != "ok")
    if include_region:  # 最近一次服务检测发现的连接问题（后台检测到网络恢复后会清掉）
        ss = data.get("service_status") or {}
        at = parse_iso(ss.get("checked_at"))
        if at and (now - at).total_seconds() < 3600:
            for it in ss.get("items") or []:
                add(it["kind"], it["level"], it["text"], it.get("advice", ""))
    changed = parse_iso(reg.get("changed_at"))
    if reg.get("prev_loc") and loc and changed and (now - changed).total_seconds() < REGION_CHANGE_HOURS * 3600:
        add("region_change", "warn",
            f"出口地区变化：{region_label(reg['prev_loc'])} → {region_label(loc)}（{fmt_ago(reg['changed_at'])}）",
            "网络环境变了（出差、换网络、开关 VPN 或代理都会这样）。Anthropic 按使用者实际所在地判断是否支持，"
            "同一账号短时间内从多个地区使用也容易触发风控。确认现在人所在的地区在支持名单内再继续。")
    if loc and reg.get("meta_loc") and reg["meta_loc"] != loc:
        add("region_mismatch", "warn",
            f"出口地区判定不一致：trace 接口看到 {region_label(loc)}，meta 接口看到 {region_label(reg['meta_loc'])}",
            "多半是 IPv4 和 IPv6 走了不同出口，或代理只分流了一部分流量。这种情况下服务端看到的地区可能和你以为的不一样，"
            "先统一出口再判断。")
    ip_type = reg.get("ip_type")
    if ip_type == "tor":
        add("tor_exit", "warn", f"出口 IP 是 Tor 出口节点（{reg.get('org') or '?'}）",
            "Tor 出口几乎一定会被当作高风险流量，账号很容易被限制；人所在地仍按实际位置判断。")
    elif ip_type == "proxy":
        add("proxy_ip", "warn", f"出口 IP 被数据库标记为代理 / VPN 出口（{reg.get('org') or '?'}，{asn_label(reg.get('asn'))}）",
            "服务端的 IP 库同样能查到这个标记，这是最常见的风控信号之一；人所在地仍按实际位置判断。")
    elif ip_type == "hosting" or (ip_type is None and hosting_exit(reg.get("org"))):
        add("isp", "warn", f"出口 IP 属于 {reg.get('org') or '?'}（{asn_label(reg.get('asn'))}），是机房 / 云服务器，不是家庭或公司宽带",
            "这类出口通常被当作 VPN / 代理流量看待，是常见的风控信号之一；人所在地仍按实际位置判断。")
    if reg.get("reg_loc") and loc and reg["reg_loc"] != loc:
        add("ip_origin", "info", f"出口 IP 注册在 {region_label(reg['reg_loc'])}，定位在 {region_label(loc)}（广播 IP）",
            "不同地理库可能把它判到不同国家，服务端看到的国家不一定和这里一致。")
    if reg.get("v4_loc") and reg.get("v6_loc") and reg["v4_loc"] != reg["v6_loc"]:
        add("ipv6_mismatch", "warn",
            f"IPv4 出口在 {region_label(reg['v4_loc'])}，IPv6 出口在 {region_label(reg['v6_loc'])}（{reg.get('v6_ip')}），两个协议走了不同的出口",
            "VPN / 代理通常只接管 IPv4，IPv6 直接从本机网络出去，服务端通过 IPv6 看到的就是本机网络的真实出口。"
            "如果 IPv6 这边在不支持地区，政策按实际所在地判断，风险与 IPv4 出口无关。")
    env = reg.get("env") if isinstance(reg.get("env"), dict) else None  # 旧版缓存没有这一段，就当作不知道
    for d in (env or {}).get("dns") or []:
        if d.get("country") and loc and d["country"] != loc:
            text = f"DNS 解析器 {d['ip']}（{d.get('name') or '公共 DNS'}，{region_label(d['country'])}）与出口地区 {region_label(loc)} 不一致"
            if region_status(d["country"]) == "unsupported":
                add("dns_resolver", "warn", text,
                    "本机在用不支持地区的公共 DNS，说明人或网络可能在不支持地区；服务端也可能据此看出实际网络。政策按实际所在地判断。"
                    "如果人确实已经在支持地区，多半是以前手动填的 DNS 没改回来：系统网络设置里把 DNS 改回自动获取（或 1.1.1.1 / 8.8.8.8 这类任播 DNS）。")
            else:
                add("dns_resolver", "info", text, "出差或代理分流都会这样；人所在地在支持名单内就没问题。")
            break
    if reg.get("warp"):
        add("warp", "info", "出口经过 Cloudflare WARP / Zero Trust",
            "上面的出口地区是 WARP 的出口，不是本机网络的真实出口；Anthropic 按使用者所在地判断。")
    skew = reg.get("skew")
    if isinstance(skew, (int, float)) and abs(skew) > CLOCK_SKEW_WARN:
        add("clock", "warn", f"本机时钟与服务器相差 {int(skew):+d} 秒",
            "时钟偏差大会让登录续期和 HTTPS 校验失败，也是常见的风控信号。打开系统的自动校时。")
    sp = data.get("status_page") or {}
    ind = sp.get("indicator")
    if ind and ind != "none":
        add("status", "bad" if ind in ("major", "critical") else "warn",
            f"Anthropic 官方状态页：{sp.get('description') or ind}（{ind}）",
            "这是 Anthropic 侧的故障，不是本机或账号的问题；等恢复后再试，详情见 status.anthropic.com。")
    tz = (env["tz"], env.get("tz_cc") or []) if loc and env and env.get("tz") else None
    if tz and loc and tz[1] and loc not in tz[1]:
        zone, home = tz[0], tz[1][0]
        text = f"系统时区 {zone} 指向 {region_label(home)}，与出口地区 {region_label(loc)} 不一致"
        if region_status(home) == "unsupported":
            add("timezone", "warn", text,
                "出口在支持地区，但本机设置显示人可能在不支持地区。Anthropic 按使用者实际所在地判断，不按出口 IP；"
                "人确实在不支持地区的话，风险与出口无关。")
        else:
            add("timezone", "info", text, "出差或经过代理都会这样；人所在地在支持名单内就没问题。"
                "想让它和所在地一致：在系统设置的“日期与时间”里按所在城市选时区。")
    lc = (env["locale"], env.get("locale_cc")) if loc and env and env.get("locale") else None
    if lc and loc and lc[1] and lc[1] != loc:
        name, home = lc
        text = f"系统语言 / 区域 {name} 指向 {region_label(home)}，与出口地区 {region_label(loc)} 不一致"
        # 语言设置只代表语言习惯（新加坡、美国很多人也用 zh_CN），不代表所在地：只做参考，扣 5 分
        add("locale", "info", text, LOCALE_ADVICE)
    act = (data.get("active") or {}).get("claude_code")
    acc = next((a for a in data.get("accounts") or [] if a["id"] == act), None)
    if acc:
        cutoff = now - dt.timedelta(days=ACCOUNT_REGION_DAYS)
        locs = [r["loc"] for r in acc.get("regions") or []
                if r.get("loc") and parse_iso(r.get("at")) and parse_iso(r["at"]) >= cutoff]
        if len(locs) > 1:
            add("account_regions", "warn",
                f"当前账号“{acc['label']}”{ACCOUNT_REGION_DAYS} 天内从 {len(locs)} 个地区使用过：{'、'.join(region_label(l) for l in locs)}",
                "同一账号短时间在多个地区使用（出差、VPN、多人共用）是常见的风控触发点，可能被要求重新验证或限制使用。")
    n = sum(1 for a in data.get("accounts") or [] if a.get("kind") == "claude_code")
    if n >= 2:
        add("accounts", "info", f"本机保存了 {n} 个 Claude Code 账号",
            "按 Anthropic 消费者条款，账号和登录信息不能给别人用或分享；一个人自己用多个号条款没有明确禁止。")
    return out


# 环境风险评分：满分 100，按 (kind, level) 扣分；没列出的 warn 扣 10、bad 扣 30。只是把提醒汇总成一个数，不拦任何操作。
RISK_WEIGHTS = {
    ("region", "warn"): 60, ("region_unknown", "warn"): 30, ("tls", "bad"): 0, ("account_regions", "warn"): 20, ("region_change", "warn"): 15, ("isp", "warn"): 20,
    ("region_mismatch", "warn"): 15, ("timezone", "warn"): 25, ("timezone", "info"): 5,
    ("locale", "info"): 5, ("clock", "warn"): 10, ("warp", "info"): 5, ("vpn", "info"): 10, ("proxy", "info"): 5,
    ("status", "bad"): 30, ("status", "warn"): 10, ("api", "bad"): 40, ("api", "warn"): 5, ("web", "bad"): 10,
    ("web", "warn"): 5, ("dns", "bad"): 20, ("accounts", "info"): 0, ("device", "info"): 0, ("scope", "info"): 0,
    ("tor_exit", "warn"): 40, ("proxy_ip", "warn"): 25, ("ipv6_mismatch", "warn"): 15, ("dns_resolver", "warn"): 15,
    ("dns_resolver", "info"): 3, ("ip_origin", "info"): 5, ("ipv6", "info"): 0, ("dns_servers", "info"): 0,
}
RISK_GRADES = (  # (最低分, 档位, 说明)
    (90, "low", "风险低，可以使用"),
    (70, "medium", "有风险，建议先处理提醒项再使用"),
    (0, "high", "风险高，不建议在当前环境使用；是否继续由你决定"),
)
RISK_SCORE_ADVICE = "评分只汇总本机能看到的信号，满分也不代表一定合规；低于 90 时逐条看提醒和建议。"


def risk_score(items):
    """把风险项汇总成 {score, grade, label, advice}。只用于提示。"""
    score = 100
    for it in items:
        if it.get("kind") == "summary":
            continue
        score -= RISK_WEIGHTS.get((it.get("kind"), it["level"]), {"warn": 10, "bad": 30}.get(it["level"], 0))
    score = max(0, score)
    grade, label = next((g, l) for floor, g, l in RISK_GRADES if score >= floor)
    return {"score": score, "grade": grade, "label": label, "advice": RISK_SCORE_ADVICE}


def region_denied(msg):
    low = (msg or "").lower()
    return any(k in low for k in ("region", "country", "not available", "unsupported", "not allowed", "地区"))


def service_check(store=None):
    """检测 Claude 相关服务和使用环境：DNS、出口地区、api.anthropic.com、claude.ai、代理、VPN，
    再加上 risk_items 的各项（出口变化、时钟、官方状态页、时区、账号地区）。

    返回 [{"kind", "level", "text", "advice"}]，级别为 ok / bad / warn / info，第一项是总体结论。
    不打印任何密钥，不干预任何操作。传了 store 会顺便刷新界面常显的缓存。
    """
    out = []

    def add(kind, level, text, advice=""):
        out.append({"kind": kind, "level": level, "text": text, "advice": advice})

    proxies = {k: os.environ[k] for k in ("HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy")
               if os.environ.get(k)}
    api_host = urllib.parse.urlparse(API_BASE).hostname or API_BASE
    web_host = urllib.parse.urlparse(CLAUDE_WEB_BASE).hostname or CLAUDE_WEB_BASE
    for host in dict.fromkeys((api_host, web_host)):
        try:
            ips = sorted({ai[4][0] for ai in socket.getaddrinfo(host, 443, proto=socket.IPPROTO_TCP)})
            add("dns", "ok", f"DNS：{host} → {', '.join(ips[:3])}")
        except OSError as e:
            add("dns", "bad", f"DNS：{host} 解析失败（{e}）",
                "检查本机 DNS 或代理设置" + ("；设了代理的话通常由代理解析，可忽略" if proxies else ""))
    # 出口地区：claude.ai 在 Cloudflare 后面，/cdn-cgi/trace 会返回它看到的访问者信息
    data = {}
    if store is not None:
        check_region(store, max_age=0)  # 顺便更新界面常显的缓存和账号的地区记录
        data = store.load()
        r = data.get("region") or {}
    else:
        r = detect_region(cfg=ip_lookup_config()) or {}
    if r.get("loc"):
        status = region_status(r["loc"])
        add("region", "ok" if status == "supported" else "warn",
            f"{region_message(r['loc'], status)}（IP {r.get('ip')}" + (f"，{r['city']}" if r.get("city") else "")
            + f"，Cloudflare 节点 {r.get('colo')}，{r.get('ms')} ms）", REGION_ADVICE[status])
    else:
        add("region_unknown", "warn", "出口地区：查不到（连不上 claude.ai 和 Cloudflare 的 trace 接口）", REGION_ADVICE["unknown"])
    ip_type = r.get("ip_type")
    src_note = f"（数据：{r['meta_source']}）" if r.get("meta_source") else ""
    if r.get("lookup_mode") == "off":
        add("isp", "info", "网络服务商 / IP 类型：外部 IP 查询已关闭",
            "出口地区照常检测；想看运营商、IP 类型、IPv6 和注册国，在设置里把“外部 IP 查询”改回自动（cam risk --ip-lookup auto）")
    elif r.get("org") and ip_type in ("residential", "mobile"):
        add("isp", "info", f"网络服务商：{r['org']}（{asn_label(r.get('asn'))}），IP 类型：{IP_TYPE_NAMES[ip_type]}{src_note}",
            "数据库判定为普通宽带或运营商网络，不是机房 / 代理出口" if ip_type == "residential" else "手机网络的出口 IP 会变，地区判定也可能跳动")
    elif r.get("org") and ip_type is None and not hosting_exit(r["org"]):
        add("isp", "info", f"网络服务商：{r['org']}（{asn_label(r.get('asn'))}），IP 类型：数据源没给标记{src_note}",
            "按运营商名看不像机房。想要数据库判定：到 ipapi.is 免费注册一个 key（每天 1000 次，可商用），cam risk --ipapi-key 填进去")
    elif not r.get("org"):
        add("isp", "info", "网络服务商：查不到（IP 信息接口都连不上）", "判断不了出口是不是机房 / 代理；不影响其它检查")
    if r.get("lookup_mode") == "off":
        pass
    elif r.get("reg_loc") and r.get("loc") and r["reg_loc"] == r["loc"]:
        add("ip_origin", "ok", f"IP 归属：注册在 {region_label(r['reg_loc'])}，与定位一致（原生 IP）")
    elif r.get("loc") and not r.get("reg_loc"):
        add("ip_origin_unknown", "info", "IP 归属：注册国查不到（RDAP 记录没写国家）", "美国 ARIN 的记录常常不写，不影响其它检查，不扣分")
    if r.get("lookup_mode") == "off":
        pass
    elif r.get("v4_loc") and r.get("v6_loc") and r["v4_loc"] == r["v6_loc"]:
        add("ipv6", "ok", f"IPv6：可用，出口 {r.get('v6_ip')}，地区与 IPv4 一致")
    elif r.get("loc") and not r.get("v6_loc"):
        add("ipv6", "ok", "IPv6：不可用，只有 IPv4 出口", "不会出现 IPv6 漏出真实出口的问题")
    dns = (r.get("env") or {}).get("dns") or []
    if dns:
        parts = [f"{d['ip']}（{'局域网 / 本机' if d['private'] else d.get('name') or '公共 DNS'}{'，' + region_label(d['country']) if d.get('country') else ''}）" for d in dns]
        add("dns_servers", "info", "DNS 解析器：" + "；".join(parts),
            "局域网 DNS 最终走路由器配置的上游；Google / Cloudflare 这类任播 DNS 全球有节点，不代表位置；某国公共 DNS 说明本机网络大概率在那个国家。"
            "人在支持地区的话，用当地网络自动获取的 DNS 或任播 DNS 即可；出差前手动填过国内 DNS 的，到了支持地区记得在系统网络设置里改回自动获取。只读本机配置，没有做泄漏探测")
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
    region_hint = "通常是出口地区不在 Anthropic 支持范围，换一个出口后重试"
    if st == 0:
        add("api", "bad", f"{api_host}：连不上（{msg}）", "检查网络或代理；公司网络可能屏蔽了这个域名")
    elif st == 403 or region_denied(msg):
        add("api", "bad", f"{api_host}：拒绝访问（HTTP {st}：{msg or '-'}）", region_hint)
    elif at and st == 200:
        add("api", "ok", f"{api_host}：服务正常，当前登录可用（{ms} ms）")
    elif at and st == 401:
        add("api", "warn", f"{api_host}：服务可达，但当前登录已过期或无效（{ms} ms）", "运行一次 claude 会自动续登录，不影响切换账号")
    elif st == 401:
        add("api", "ok", f"{api_host}：服务可达（未登录，{ms} ms）")
    elif st >= 500:
        add("api", "bad", f"{api_host}：服务端错误（HTTP {st}）", "可能是 Anthropic 故障，稍后再试，可查看 status.anthropic.com")
    else:
        add("api", "warn", f"{api_host}：HTTP {st}（{msg or '-'}，{ms} ms）", "少见的返回，稍后重试；持续出现请把这一行发给作者")
    if st:
        out[-1]["ms"] = ms  # 延迟，cam doctor 单列一行
    # claude.ai：不带登录，看可达性和有没有被网络管控拦截
    t0 = time.time()
    code, body, raw = web_get("/api/organizations", "", timeout=10)
    ms = int((time.time() - t0) * 1000)
    msg = ((body or {}).get("error") or {}).get("message", "") if isinstance(body, dict) else ""
    if code == 0:
        add("web", "bad", f"{web_host}：连不上（{raw}）", "检查网络或代理")
    elif is_cloudflare(raw):
        add("web", "warn", f"{web_host}：返回人机验证页（{ms} ms）",
            "公司网络或代理触发了 Cloudflare 人机验证；只影响网页账号读取额度，不影响 Claude Code 切换")
    elif region_denied(msg):
        add("web", "bad", f"{web_host}：拒绝访问（HTTP {code}：{msg}）", region_hint)
    elif code in (200, 401, 403):
        add("web", "ok", f"{web_host}：可达（{ms} ms）")
    elif code >= 500:
        add("web", "bad", f"{web_host}：服务端错误（HTTP {code}）", "稍后再试")
    else:
        add("web", "warn", f"{web_host}：HTTP {code}（{msg or (raw or '')[:80]}，{ms} ms）", "少见的返回，稍后重试")
    if code:
        out[-1]["ms"] = ms
    # 代理：环境变量 + 系统设置；VPN：默认路由
    env = r.get("env") if isinstance(r.get("env"), dict) and "proxy" in r.get("env", {}) else local_env()
    out.extend(proxy_vpn_items(env))
    out.extend(risk_items(data, include_region=False))
    if any(is_cert_error(i["text"]) for i in out):
        add("tls", "bad", "HTTPS 证书校验失败（CERTIFICATE_VERIFY_FAILED），上面连不上的几项都是这个原因", CERT_ADVICE)
    elif SSL_CA_FALLBACK:
        add("tls", "info", f"证书：本机 Python 没带根证书，已自动改用 {SSL_CA_FALLBACK}", "证书照常校验；想用 Python 自己的证书可以运行一次 Install Certificates.command")
    add("device", "info", f"设备：{platform.system()} {platform.release()}，{platform.machine()}，Python {platform.python_version()}",
        "设备类型、系统、时区、语言这些设备环境信息 Anthropic 会收集；这里只是列出来，本工具不会改动或伪装任何设备标识。")
    add("scope", "info", "检测范围：本机能看到的信号（出口 IP、IP 类型、IPv4 / IPv6、注册国、运营商、代理 / VPN、WARP、DNS 配置、时钟、时区、系统语言、账号使用地区）",
        "按 Anthropic 官方说明，所在地是结合 IP 和其他信号综合判断的；账号侧本机看不到的信号不在检测范围内，"
        "所以“没有发现风险点”不等于一定合规，只表示本机能查到的都正常。")
    # 总体结论放最前面：评分 + 档位 + 最严重的一条
    sc = risk_score(out)
    first_bad = next((i for i in out if i["level"] == "bad"), None)
    first_warn = next((i for i in out if i["level"] == "warn"), None)
    worst = first_bad or first_warn
    head = f"总体：{sc['score']}/100，{sc['label']}"
    if first_bad or sc["grade"] == "high":
        level = "bad"
    elif first_warn or sc["grade"] == "medium":
        level = "warn"
    else:
        level = "ok"
    out.insert(0, {"kind": "summary", "level": level, "score": sc["score"], "grade": sc["grade"],
                   "text": head + (f" —— {worst['text']}" if worst else "；Claude 服务正常，出口地区在支持名单内，没有发现风险点"),
                   "advice": worst["advice"] if worst else RISK_SCORE_ADVICE})
    if store is not None:  # 连接问题记下来，界面顶部的环境卡和这里的分数保持一致
        bad = [i for i in out if i["kind"] in ("api", "web", "dns", "tls") and i["level"] in ("warn", "bad")]
        with store.lock():
            d = store.load()
            if bad:
                d["service_status"] = {"checked_at": now_iso(), "items": bad}
            else:
                d.pop("service_status", None)
            store.save(d)
    return out


# --------------------------------------------------------------------------- 全面体检（cam doctor）

STATUS_SUMMARY_URLS = [u for u in os.environ.get(
    "CAM_STATUS_SUMMARY_URLS",
    "https://status.claude.com/api/v2/summary.json,https://status.anthropic.com/api/v2/summary.json",
).split(",") if u]
# 官方原生安装器取版本号的地方（claude.ai/install.sh 里的 DOWNLOAD_BASE_URL），<渠道> 文件里就是版本号；空字符串表示不查
CLAUDE_RELEASES_URL = os.environ.get("CAM_CLAUDE_RELEASES_URL", "https://downloads.claude.ai/claude-code-releases")
# 状态页里和这里有关的组件：Claude Code、API、claude.ai，再加上续登录用的 Console（platform.claude.com）
STATUS_COMPONENT_WORDS = ("claude code", "api", "claude.ai", "console")
INCIDENT_STATUS = {"investigating": "调查中", "identified": "已找到原因", "monitoring": "已修复，观察中",
                   "scheduled": "已安排", "in_progress": "进行中", "verifying": "验证中"}
COMPONENT_STATUS = {"degraded_performance": "性能下降", "partial_outage": "部分故障", "major_outage": "严重故障",
                    "under_maintenance": "维护中"}
BOT_CLIS = (("claude-discord", "discord"), ("claude-feishu", "feishu"), ("claude-dingtalk", "dingtalk"))  # claude-channels 装在 ~/.local/bin
BOT_NAMES = {"discord": "Discord", "feishu": "飞书", "dingtalk": "钉钉"}
BOT_DOCTOR_TIMEOUT = 60
USAGE_STALE_SECS = 1800  # 有后台在跑时，用量多久没更新算“没在刷新”
MENUBAR_STATE = ".menubar-state.json"  # 菜单栏插件每轮跑完存的状态（extras/swiftbar/cam_menu.py，Windows 托盘也用它）
MENU_TOOL = "托盘小工具" if IS_WIN else "菜单栏插件"  # macOS 的菜单栏插件和 Windows 的托盘小工具跑的是同一份后台任务
SERVE_STATE = ".serve-state.json"  # cam serve 后台每分钟记一次心跳
SESSION_SECRET_ENV = ("CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN")  # 只看有没有，不读值
SESSION_INFO_ENV = ("TMUX", "FEISHU_SESSION", "DINGTALK_SESSION", "DISCORD_STATE_DIR", "CLAUDE_CONFIG_DIR")
LOGIN_TEXT = {"machine": "本机登录，切号会跟着换", "own_token": "用自己的号（CLAUDE_CODE_OAUTH_TOKEN），切号不影响",
              "api_key": "用 API Key（环境变量），不走订阅登录，切号不影响",
              "config_dir": "用单独的配置目录（CLAUDE_CONFIG_DIR），切号不影响",
              "unknown": "读不到它的环境变量，看不出用的哪个登录"}
PLAN_NAME = {"pro": "Pro", "max": "Max", "team": "Team", "enterprise": "Enterprise", "free": "Free", "api": "API"}
KIND_LONG = {"claude_code": "Claude Code", "claude_web": "网页版", "claude_desktop": "桌面版", "api": "API"}
HEALTH_TO_CHECK = {"error": "bad", "warn": "warn", "info": "info", "ok": "ok"}
CHECK_MARKS = {"ok": ("✓", "g"), "warn": ("!", "y"), "bad": ("✗", "r"), "info": ("·", "d")}


def ck(level, text, advice="", **extra):
    """体检的一条结果。level：ok ✓ / warn ! / bad ✗ / info ·；advice 是出了问题时的“→ 怎么办”。"""
    return {"level": level, "text": text, "advice": advice or "", **extra}


def fmt_span(secs):
    """一段时长：“2 天 3 小时”“3 小时 12 分”“12 分钟”。"""
    m = max(0, int(secs)) // 60
    h, m = divmod(m, 60)
    d, h = divmod(h, 24)
    if d:
        return f"{d} 天 {h} 小时" if h else f"{d} 天"
    if h:
        return f"{h} 小时 {m} 分" if m else f"{h} 小时"
    return f"{m} 分钟" if m else "不到 1 分钟"


def _version_key(v):
    m = re.match(r"(\d+)\.(\d+)\.(\d+)", v or "")
    return tuple(int(x) for x in m.groups()) if m else None


def latest_claude_version(timeout=6):
    """官方原生安装器给的最新版本：(渠道, 版本号)，按 settings.json 的 autoUpdatesChannel（默认 latest）。查不到返回 None。"""
    if not CLAUDE_RELEASES_URL:
        return None
    try:
        s = read_json(os.path.join(claude_config_dir(), "settings.json"), {}) or {}
    except CamError:
        s = {}
    ch = "stable" if isinstance(s, dict) and s.get("autoUpdatesChannel") == "stable" else "latest"
    st, txt = http_text(f"{CLAUDE_RELEASES_URL.rstrip('/')}/{ch}", timeout)
    v = txt.strip() if st == 200 else ""
    return (ch, v) if re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", v) else None


def parse_status_summary(j):
    """Statuspage 的 summary.json → {indicator, description, incidents: [{name, status}], components: [{name, status}]}。

    只留没解决的事件和不正常的相关组件（Claude Code / API / claude.ai / Console）。格式不对返回 None。
    """
    st = j.get("status") if isinstance(j, dict) else None
    if not isinstance(st, dict) or not st.get("indicator"):
        return None
    incidents = [{"name": str(i.get("name") or "?"), "status": str(i.get("status") or "")}
                 for i in j.get("incidents") or [] if isinstance(i, dict) and i.get("status") not in ("resolved", "postmortem")]
    comps = [{"name": str(c["name"]), "status": str(c["status"])}
             for c in j.get("components") or [] if isinstance(c, dict) and c.get("name") and c.get("status")
             and c["status"] != "operational" and any(w in str(c["name"]).lower() for w in STATUS_COMPONENT_WORDS)]
    return {"indicator": str(st["indicator"]), "description": str(st.get("description") or ""),
            "incidents": incidents, "components": comps}


def fetch_status_summary(timeout=6):
    """官方状态页的摘要：先 status.claude.com，连不上再 status.anthropic.com。都不行返回 None。"""
    for url in STATUS_SUMMARY_URLS:
        st, txt = http_text(url, timeout)
        if st != 200:
            continue
        try:
            s = parse_status_summary(json.loads(txt))
        except (json.JSONDecodeError, ValueError):
            continue
        if s:
            return dict(s, page=urllib.parse.urlparse(url).hostname or url)
    return None


def status_summary_items(s):
    if s is None:
        hosts = "、".join(dict.fromkeys(urllib.parse.urlparse(u).hostname or u for u in STATUS_SUMMARY_URLS))
        return [ck("warn", f"官方状态页连不上（{hosts}）", "不影响别的检查；需要时在浏览器里打开 status.claude.com 看")]
    ind = s["indicator"]
    out = [ck("ok", f"官方状态页：全部正常（{s['page']}）") if ind == "none" else
           ck("bad" if ind in ("major", "critical") else "warn", f"官方状态页：{s['description'] or ind}（{ind}）",
              f"这是 Anthropic 那边的故障，不是本机或账号的问题；等恢复后再试，详情见 {s['page']}")]
    for i in s["incidents"]:
        out.append(ck("info", f"没解决的事件：{i['name']}（{INCIDENT_STATUS.get(i['status'], i['status'])}）"))
    for c in s["components"]:  # 总体正常时组件出问题也要提醒；总体已经报了就只列出来
        out.append(ck("warn" if ind == "none" else "info", f"{c['name']}：{COMPONENT_STATUS.get(c['status'], c['status'])}",
                      f"详情见 {s['page']}" if ind == "none" else ""))
    return out


def proc_root():
    """Linux 的 /proc（测试用 CAM_TEST_PROC_ROOT 指向一个假的）；别的系统返回 None。"""
    root = os.environ.get("CAM_TEST_PROC_ROOT")
    if root:
        return root
    return "/proc" if os.path.isdir("/proc/self") else None


def _proc_file(root, pid, name):
    try:
        with open(os.path.join(root, str(pid), name), "rb") as f:
            return f.read()
    except OSError:
        return None


def process_table():
    """本机进程 [(pid, 命令行)]，查不到时返回空列表。用来认出 cam serve、桌面版。"""
    root = proc_root()
    out = []
    if root:
        try:
            pids = [p for p in os.listdir(root) if p.isdigit()]
        except OSError:
            return []
        for pid in pids:
            argv = [a.decode("utf-8", "replace") for a in (_proc_file(root, pid, "cmdline") or b"").split(b"\0") if a]
            if argv:
                out.append((int(pid), " ".join(argv)))
        return out
    try:
        if IS_WIN:
            ps = "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId)`t$($_.CommandLine)\" }"
            txt = decode_output(subprocess.run(["powershell", "-NoProfile", "-Command", ps], capture_output=True, timeout=20).stdout)
        else:
            txt = subprocess.run(["ps", "-A", "-o", "pid=,args="], capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    for line in txt.splitlines():
        pid, _, cmd = line.strip().replace("\t", " ", 1).partition(" ")
        if pid.isdigit() and cmd.strip():
            out.append((int(pid), cmd.strip()))
    return out


def _is_cam_serve(cmd):
    return bool(re.search(r"cam\.py[\"']?\s+serve(\s|$)", cmd))


def _is_cam_desktop(cmd):
    low = cmd.replace("\\", "/").lower()
    return "/claude accounts.app/" in low or bool(re.search(r"(^|/)(claude-accounts|claude accounts)(\.exe)?([\s\"]|$)", low))


def session_bot(env):
    """会话是不是聊天机器人的：(discord / feishu / dingtalk, 会话名) 或 None。

    先看机器人启动脚本设的 FEISHU_SESSION / DINGTALK_SESSION / DISCORD_STATE_DIR，再看 tmux 的 socket 名（claude-discord-<名称>）。
    """
    for key, bot in (("FEISHU_SESSION", "feishu"), ("DINGTALK_SESSION", "dingtalk")):
        if env.get(key):
            return bot, env[key]
    base = os.path.basename((env.get("DISCORD_STATE_DIR") or "").rstrip("/"))
    if base == "discord" or base.startswith("discord-"):
        return "discord", base[8:] or "main"
    sock = os.path.basename((env.get("TMUX") or "").split(",")[0])
    m = re.fullmatch(r"claude-(discord|feishu|dingtalk)(?:-([\w.-]+))?", sock)
    return (m.group(1), m.group(2) or "main") if m else None


def bot_name(bot):
    """“Discord 机器人”“飞书机器人”。"""
    return BOT_NAMES[bot] + (" " if BOT_NAMES[bot].isascii() else "") + "机器人"


def bot_label(bot, name):
    """“Discord 机器人主会话”“飞书机器人 shop”。"""
    return bot_name(bot) + ("主会话" if name == "main" else f" {name}")


def _session_env(raw):
    """从 environ 里只挑要用的：机器人标记留原值，CLAUDE_CODE_OAUTH_TOKEN 这类只记有没有，值当场丢掉。"""
    env, has = {}, set()
    for kv in (raw or b"").split(b"\0"):
        k, _, v = kv.partition(b"=")
        k = k.decode("utf-8", "replace")
        if k in SESSION_SECRET_ENV:
            if v:
                has.add(k)
        elif k in SESSION_INFO_ENV:
            env[k] = v.decode("utf-8", "replace")
    return env, has


def claude_sessions():
    """正在运行的 Claude Code 会话（读 /proc，只有 Linux）：[{"pid", "cwd", "secs", "bot", "login"}]；别的系统返回 None。

    login 是 LOGIN_TEXT 里的一种。环境变量只看有没有，不读出值；CLAUDE_CONFIG_DIR 的值只用来和本机的配置目录比，不显示。
    """
    root = proc_root()
    if not root:
        return None
    try:
        with open(os.path.join(root, "uptime")) as f:
            uptime = float(f.read().split()[0])
    except (OSError, ValueError, IndexError):
        uptime = None
    ticks = os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100
    mine = os.path.realpath(claude_config_dir())
    out = []
    try:
        pids = sorted(int(p) for p in os.listdir(root) if p.isdigit())
    except OSError:
        return []
    for pid in pids:
        if pid == os.getpid():
            continue
        argv = [a.decode("utf-8", "replace") for a in (_proc_file(root, pid, "cmdline") or b"").split(b"\0") if a]
        if not argv or not _is_claude_cmd(argv[0], " ".join(argv)):
            continue
        raw = _proc_file(root, pid, "environ")
        env, has = _session_env(raw)
        if raw is None:
            login = "unknown"
        elif "CLAUDE_CODE_OAUTH_TOKEN" in has:
            login = "own_token"
        elif has:
            login = "api_key"
        elif env.get("CLAUDE_CONFIG_DIR") and os.path.realpath(os.path.expanduser(env["CLAUDE_CONFIG_DIR"])) != mine:
            login = "config_dir"
        else:
            login = "machine"
        try:
            cwd = os.readlink(os.path.join(root, str(pid), "cwd"))
        except OSError:
            cwd = None
        secs, stat = None, _proc_file(root, pid, "stat")
        if uptime is not None and stat:
            try:  # 第 22 项是开机后多少个时钟周期启动的；进程名里可能有空格，从最后一个 ) 后面数
                secs = max(0.0, uptime - int(stat.rsplit(b")", 1)[1].split()[19]) / ticks)
            except (IndexError, ValueError):
                pass
        out.append({"pid": pid, "cwd": cwd, "secs": secs, "bot": session_bot(env), "login": login})
    return out


def _ps_etimes(pids):
    """macOS 等：ps 给的运行时长（[[天-]时:]分:秒）。"""
    if not pids or IS_WIN:
        return {}
    try:
        txt = subprocess.run(["ps", "-o", "pid=,etime=", "-p", ",".join(str(p) for p in pids)],
                             capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        return {}
    return {int(p[0]): p[1] for p in (l.split() for l in txt.splitlines()) if len(p) == 2 and p[0].isdigit()}


def session_items():
    sessions = claude_sessions()
    if sessions is None:  # macOS / Windows：列出 ps 给的东西，看不出用的哪个登录
        procs = running_claude_processes()
        out = [ck("info", f"正在运行的 claude 会话：{len(procs)} 个" + ("（切换账号后需要重开）" if procs else ""))]
        etimes = _ps_etimes([p for p, _ in procs])
        for pid, cmd in procs[:20]:
            out.append(ck("info", f"pid {pid}" + (f" · 已运行 {etimes[pid]}" if pid in etimes else "") + f" · {cmd[:120]}",
                          "用的哪个登录看不出来（只有 Linux 能读到会话的环境变量）"))
        return out
    machine = sum(1 for s in sessions if s["login"] == "machine")
    out = [ck("info", f"正在运行的 claude 会话：{len(sessions)} 个"
              + (f"（{machine} 个用本机登录：切换账号后它们还拿着旧账号的 token，要重开）" if machine else ""))]
    for s in sessions:
        parts = [f"pid {s['pid']}"]
        if s["bot"]:
            parts.append(bot_label(*s["bot"]))
        parts.append(s["cwd"] or "工作目录读不到")
        if s["secs"] is not None:
            parts.append(f"已运行 {fmt_span(s['secs'])}")
        parts.append(LOGIN_TEXT[s["login"]])
        advice = ""
        if s["bot"] and s["login"] == "machine":
            bot, name = s["bot"]
            advice = f"想让它一直用同一个号、不跟着切换：在聊天里发 /account（没有别的号先 /account add 接入一个），或在电脑上运行 claude-{bot} account {name} set"
        out.append(ck("info", " · ".join(parts), advice))
    return out


# ---- 1. 账号

def _account_group(p):
    """账号小标题：名字、邮箱（和 cam ls 一样，名字就是邮箱时不重复）、类型、套餐、当前登录。"""
    bits = [KIND_LONG.get(p["kind"], p["kind"])]
    plan = p.get("plan_display")
    if plan:
        bits.append(PLAN_NAME.get(plan, plan) + (f" {plan_tier(p)}" if plan_tier(p) else ""))
    elif p["kind"] in ("claude_code", "claude_web"):
        bits.append("套餐未知")
    if p.get("is_active"):
        bits.append("上次打开" if p["kind"] == "claude_desktop" else "当前登录")
    email = f" {p['email']}" if p.get("email") and p["email"] != p.get("label") else ""
    return f"{p.get('label') or p['id']}{email}（{'，'.join(bits)}）"


def _usage_brief(u):
    return "，".join(f"{n}已用 {u[k]['utilization']:.0f}%" for k, n in (("five_hour", "5 小时"), ("seven_day", "7 天"))
                    if isinstance(u.get(k), dict) and u[k].get("utilization") is not None)


def checkup_account(store, p, live_ob, before_exp, offline):
    """一个账号：登录还能不能续、到期日、用量（上次出错）、健康状态。p 是 public_state 里的账号。"""
    out = []
    label, kind, u = p.get("label") or p["id"], p["kind"], p.get("usage") or {}
    err = u.get("error")
    covered = {"expired"}  # 下面单独写过的问题，健康状态里不再重复
    relogin = f"cam login {label}（在浏览器里重新登录这个账号，登录完自动保存）"
    when = f"（{fmt_ago(u['fetched_at'])}）" if offline and u.get("fetched_at") else ""
    left = 0
    if kind == "claude_code":
        ob = live_ob or store.load_secret(p["id"]).get("claudeAiOauth") or {}
        left = (ob.get("expiresAt") or 0) / 1000 - time.time()
        if not ob.get("refreshToken"):
            covered.add("no_credentials")
            out.append(ck("bad", "没有保存可用的凭据，切换到它也用不了", relogin))
        elif p.get("is_active"):  # 当前登录从不替它续，交给 Claude Code
            out.append(ck("ok", f"当前登录：access token {fmt_span(left)}后过期，Claude Code 会自己续") if left > 0 else
                       ck("info", "当前登录：access token 已过期，Claude Code 下次运行时会自己续（cam 从不替当前登录续）"))
        elif err and ("登录已失效" in err or "没有 refresh token" in err):
            covered.add("login_invalid")
            out.append(ck("bad", "登录已失效：保存的 refresh token 被服务端拒绝，切换到它也用不了", relogin))
        elif err and "续登录失败" in err:
            covered.add("network")
            out.append(ck("warn", f"保存的登录过期了，替它续一次没成功：{err}", "多半是网络问题，稍后再运行 cam doctor"))
        elif offline:
            out.append(ck("info", f"保存的登录：access token {fmt_span(left)}后过期") if left > 120 else
                       ck("info", "保存的 access token 已过期：刷新用量或切换到它时会用它自己的 refresh token 续（离线没测能不能续）"))
        elif before_exp is not None and (ob.get("expiresAt") or 0) > before_exp:
            out.append(ck("ok", f"保存的登录还能续：刚用它自己的 refresh token 续了一次，新的 access token {fmt_span(left)}后过期"))
        elif not err:
            out.append(ck("ok", f"保存的登录有效：access token {fmt_span(left)}后过期"))
    # 用量：网页号就是 sessionKey 能不能用，API 号是 Key 能不能用
    hk = _health_kind_of_error(kind, err) if err else None
    if kind == "claude_desktop":
        if u.get("cache_status") == "ok":
            out.append(ck("ok" if not u.get("cache_stale") else "info", f"本地额度：记录于 {fmt_ago(u.get('fetched_at'))}"
                          + ("（超过 30 分钟的旧快照）" if u.get("cache_stale") else "")))
        else:
            out.append(ck("info", u.get("notice") or "还没有桌面版的本地额度记录"))
    elif kind == "claude_web" and not p.get("has_session_key"):
        out.append(ck("info", "没填 sessionKey，不自动读取用量（只记到期日也可以）"))
    elif err and hk not in covered:
        covered.add(hk)
        if "HTTP 429" in err:
            out.append(ck("warn", f"用量接口限流（HTTP 429）{when}", "过几分钟再刷新；和账号本身无关，不影响切换"))
        elif "HTTP 401" in err and kind == "claude_code":
            if p.get("is_active") and left <= 0:
                out.append(ck("info", "用量接口返回 401：access token 过期了，等 Claude Code 自己续"))
            else:
                out.append(ck("warn", f"用量接口不认这个登录（HTTP 401）{when}",
                              "在 claude 里发一句话让它续一次；还不行就 /login 重新登录" if p.get("is_active")
                              else "切换过去时 Claude Code 会试着续；续不上就 " + relogin))
        elif hk:
            level, _, advice = HEALTH_TEXT[hk]
            out.append(ck(HEALTH_TO_CHECK[level], f"{'上次' if offline else ''}取用量出错{when}：{err}",
                          relogin if hk in ("login_invalid", "wrong_owner") else advice))
    elif kind == "api":
        kv = u.get("key_valid")
        if kv is False:
            covered.add("key_invalid")
            out.append(ck("bad", f"API Key 无效{when}：{u.get('key_error') or ''}", f"cam set {label} --api-key 新的 Key"))
        elif kv is True:
            out.append(ck("ok", f"API Key 有效{when}"))
        if u.get("cost_month_usd") is not None:
            out.append(ck("info", f"本月花费 ${u['cost_month_usd']:.2f}"))
    elif u.get("fetched_at"):
        brief = _usage_brief(u)
        head = ("sessionKey 有效，" if kind == "claude_web" else "") + (f"用量上次刷新 {fmt_ago(u['fetched_at'])}" if offline else "用量接口正常")
        out.append(ck("ok", head + (f"：{brief}" if brief else "")))
    else:
        out.append(ck("info", "还没取过用量" + ("（离线模式没测）" if offline else "")))
    # 到期日：7 天内提醒，过了算要处理；自动续期的只是告诉一声
    d = p.get("days_left")
    if d is not None:
        m = p.get("manual") or {}
        date = f"（{m['expires_at']}）" if m.get("expires_at") else ""
        auto = m.get("auto_renew") is True
        if d < 0:
            out.append(ck("bad", f"订阅已过期 {-d} 天{date}",
                          f"续费后运行 cam set {label} --expires 新的到期日；没续的话这个号可能已经降级为免费版"))
        elif d <= 7 and not auto:
            out.append(ck("warn", ("今天到期" if d == 0 else f"{d} 天后到期") + date,
                          f"要续的话记得续费；续了以后 cam set {label} --expires 新的到期日（每月自动扣费的加 --auto-renew）"))
        else:
            out.append(ck("info", f"{d} 天后{'自动续期' if auto else '到期'}{date}"))
    h = p.get("health") or {}
    for i in h.get("issues") or []:
        if i["kind"] not in covered:
            out.append(ck(HEALTH_TO_CHECK[i["level"]], i["text"], i["advice"]))
    out.append(ck("ok", "账号状态：正常") if h.get("level") in (None, "ok", "info")
               else ck("info", f"账号状态：{h['label']}（见上面几条）"))
    return out


def checkup_accounts(store, before_exp, offline, refresh_error=None):
    data = store.load()
    st = public_state(store, data)
    out = []
    if refresh_error:
        out.append(ck("warn", f"刷新用量没有完成：{refresh_error}", "稍后再运行 cam doctor"))
    if not st["accounts"]:
        out.append(ck("info", "还没有保存账号", "Claude Code：cam login 名字（每个账号各登录一次）；网页版：cam add-web --label 名字"))
        return out
    try:
        creds, _ = read_live_credentials()
        live = identify_code_account(data, read_oauth_account(), creds) if creds else None
    except CamError:  # 文件损坏：第 3 节会报，这里当作认不出当前登录
        creds, live = None, None
    for p in st["accounts"]:
        group = _account_group(p)
        live_ob = oauth_block(creds) if live and live["id"] == p["id"] and p.get("is_active") else None
        for it in checkup_account(store, p, live_ob, before_exp.get(p["id"]), offline):
            it["group"] = group
            out.append(it)
    return out


# ---- 2. Claude 服务

def checkup_service(store, offline, svc=None, summary=None):
    if offline:  # 不联网：用后台上次检测的缓存
        data = store.load()
        reg = region_view(data)
        out = [ck("info", "离线模式：没有实测 Claude 服务，下面是后台上次检测的结果"
                  + (f"（{fmt_ago(reg['checked_at'])}）" if reg.get("checked_at") else "（还没检测过）"))]
        if reg.get("status"):
            ok = reg["status"] == "supported"
            out.append(ck("ok" if ok else "warn", reg["message"], "" if ok else reg["advice"], kind="region"))
        out += [ck(r["level"], r["text"], r["advice"], kind=r["kind"]) for r in risk_items(data)]
        if SSL_CA_FALLBACK:
            out.append(ck("info", f"证书：本机 Python 没带根证书，已自动改用 {SSL_CA_FALLBACK}（证书照常校验）"))
        return out
    items = [dict(i) for i in svc or []]
    if summary is not None:  # 下面有状态页的完整摘要，服务检查里那一行就不重复了
        items = [i for i in items if i.get("kind") != "status"]
    hosts = {"api": urllib.parse.urlparse(API_BASE).hostname or API_BASE,
             "web": urllib.parse.urlparse(CLAUDE_WEB_BASE).hostname or CLAUDE_WEB_BASE}
    lat = [(hosts[i["kind"]], i["ms"]) for i in items if i.get("kind") in hosts and i.get("ms") is not None]
    if lat:  # 放在 api / web 两行后面
        slow = max(ms for _, ms in lat) >= 3000
        at = max(n for n, i in enumerate(items) if i.get("kind") in hosts) + 1
        items.insert(at, ck("warn" if slow else "ok", "延迟：" + "，".join(f"{h} {ms} ms" for h, ms in lat),
                            "超过 3 秒多半是网络或代理慢，Claude Code 会觉得卡" if slow else "", kind="latency"))
    return items + status_summary_items(summary)


# ---- 3. 本机 Claude Code

def checkup_local_claude(store, latest=None):
    out = []
    found = claude_installs()
    if found:
        p, real, kind = found[0]
        ver = claude_version(p)
        out.append(ck("ok", f"Claude Code 命令：{p}（{kind}，{ver or '版本未知'}）"))
        if len(found) > 1:
            out.append(ck("warn", f"PATH 里有 {len(found)} 份 claude，实际运行的是第一个："
                          + "；".join(f"{q}（{k}）" + (f" → {r}" if r != q else "") for q, r, k in found),
                          "多份并存容易出现“升级了却没生效”。保留一份即可，npm 版可用 npm uninstall -g @anthropic-ai/claude-code 卸载"))
        cur = re.search(r"\d+\.\d+\.\d+", ver or "")
        if latest and cur and _version_key(latest[1]) and _version_key(cur.group(0)):
            if _version_key(latest[1]) > _version_key(cur.group(0)):
                out.append(ck("warn", f"Claude Code 有新版本 {latest[1]}（现在是 {cur.group(0)}，{latest[0]} 渠道）",
                              "brew upgrade claude-code" if kind == "Homebrew" else "运行 claude update 更新"))
            else:
                out.append(ck("ok", f"Claude Code 已是最新版本（{latest[0]} 渠道 {latest[1]}）"))
    else:
        out.append(ck("bad", "Claude Code 命令：没找到", "运行 cam env install 用官方安装器安装"))
    for f in override_findings():
        out.append(ck("warn", f"{f}：它会优先于订阅登录，切换账号可能不生效", "不需要的话去掉它"))
    cfg = claude_config_dir()
    out.append(ck("info", f"配置目录：{cfg}" + ("" if os.path.isdir(cfg) else "（不存在）")))
    f = claude_credentials_file()
    if os.path.exists(f):
        try:
            has = bool(oauth_block(read_json(f, {}) or {}).get("refreshToken"))
            out.append(ck("ok" if has else "bad", f"凭据文件：{f}" + ("" if has else "（里面没有登录信息）"),
                          "" if has else "运行 claude 登录一次"))
        except CamError as e:
            out.append(ck("bad", str(e), e.advice or "在 claude 里 /login 重新登录一次会重写这个文件"))
        try:
            mode = os.stat(f).st_mode & 0o777
        except OSError:
            mode = 0
        if not IS_WIN and mode & 0o077:
            out.append(ck("warn", f"凭据文件的权限是 0{mode:o}，同一台电脑上的其他用户可能读得到", f"chmod 600 {f}"))
    else:
        out.append(ck("info", "凭据文件：不存在" + ("（macOS 一般存在钥匙串里，正常）" if IS_MAC else "")))
    if IS_MAC:
        if not use_keychain():
            out.append(ck("info", "钥匙串：已跳过（设置了 CLAUDE_CONFIG_DIR 或 CAM_NO_KEYCHAIN）"))
        else:
            kc = keychain_read()
            if kc and oauth_block(kc).get("refreshToken"):
                out.append(ck("ok", "钥匙串：有 Claude Code 登录信息"))
            elif kc:
                out.append(ck("bad", "钥匙串：有条目但没有登录信息", "运行 claude 登录一次"))
            elif keychain_exists():
                out.append(ck("bad", "钥匙串：条目存在但读取被拒绝", "重新运行时如果弹窗，请点“始终允许”"))
            else:
                out.append(ck("bad", "钥匙串：没有 Claude Code 登录信息", "运行 claude 登录一次"))
    try:
        creds = read_live_credentials()[0]
    except CamError:
        creds = None  # 上面已经报过凭据文件损坏
    try:
        oa = read_oauth_account() or {}
    except CamError as e:
        oa = {}
        out.append(ck("bad", str(e), e.advice or "这个文件是 Claude Code 写的；在 claude 里 /login 一次通常会修好"))
    out.append(ck("ok", f"当前登录邮箱：{oa['emailAddress']}") if oa.get("emailAddress") else
               ck("bad", "当前登录邮箱：无（Claude Code 还没登录）", "运行 claude 登录一次"))
    data = store.load()
    if creds and oauth_block(creds).get("accessToken"):
        acc = identify_code_account(data, oa, creds)
        out.append(ck("ok", f"当前登录已保存为“{acc['label']}”") if acc else
                   ck("bad", "当前登录还没保存", "运行 cam（或 cam import）会自动保存"))
    for key in ("live_mismatch", "live_unverified"):
        if (data.get(key) or {}).get("message"):
            out.append(ck("warn", data[key]["message"]))
    return out + session_items()


# ---- 4. cam 自己

def _read_json_quiet(path):
    try:
        j = read_json(path, {})
    except CamError:
        return {}
    return j if isinstance(j, dict) else {}


def background_status(store, data):
    """谁在做后台任务（桌面版 / cam serve / 菜单栏插件）、上一轮什么时候。返回 (是否有人在做, 刷新间隔分钟或 None, 条目)。

    后台锁（background.lock）被拿着就是有人在做；不拿锁试一下就看不出来，试完马上放掉。
    """
    lease = store.background_lock()
    held = not lease.try_acquire()
    if not held:
        lease.release()
    procs = process_table()
    serve = [pid for pid, cmd in procs if _is_cam_serve(cmd)]
    desk = [pid for pid, cmd in procs if _is_cam_desktop(cmd)]
    sv = _read_json_quiet(os.path.join(store.root, SERVE_STATE))
    mb_path = os.path.join(store.root, MENUBAR_STATE)
    try:
        mb_age = time.time() - os.path.getmtime(mb_path)
    except OSError:
        mb_age = None
    mb = _read_json_quiet(mb_path) if mb_age is not None else {}
    beat = parse_iso(sv.get("at")) if sv.get("pid") in serve else None
    out, refresh_min, err = [], None, None
    if held and beat:
        refresh_min, err = sv.get("refresh_min"), sv.get("last_error")
        age = (dt.datetime.now(dt.timezone.utc) - beat).total_seconds()
        out.append(ck("ok" if age < 300 else "warn", f"后台任务：cam serve 在做（pid {sv['pid']}），上一轮 {fmt_span(age)}前",
                      "" if age < 300 else "后台好几分钟没跑一轮了，可能卡住了；重启 cam serve 试试"))
    elif held and desk:
        out.append(ck("ok", f"后台任务：桌面版在做（pid {'、'.join(map(str, desk))}）",
                      "桌面版不记每一轮的时间，看下面各账号的用量刷新时间"))
    elif held and serve:
        out.append(ck("ok", f"后台任务：cam serve 在做（pid {'、'.join(map(str, serve))}）"))
    elif mb_age is not None and mb_age < 180:
        err = mb.get("last_error")  # 刷新间隔是插件自己的设置（默认 10 分钟），这里不知道，按 30 分钟判断
        out.append(ck("ok", f"后台任务：{MENU_TOOL}每分钟做一轮，上一轮 {fmt_span(mb_age)}前"))
    elif held:
        out.append(ck("ok", "后台任务：有程序拿着后台锁在做（没认出是哪个）"))
    else:
        busy = data.get("pending_switch") or any(r.get("type") == "time" for r in data.get("rules") or [])
        how = ("重新运行 安装.cmd 可以装托盘小工具" if IS_WIN else "装菜单栏小工具：bash extras/swiftbar/install.sh" if IS_MAC
               else "重新运行 install.sh 可以装成后台服务")
        out.append(ck("warn" if busy else "info", f"后台任务：没有程序在做（桌面版、cam serve、{MENU_TOOL}都没在运行）"
                      + ("，时间规则和等待中的切换没人执行" if busy else ""),
                      f"不会自动保存新登录、定时刷新用量、执行时间规则。需要时运行 cam serve 或 cam panel；{how}"))
    if not IS_WIN and not IS_MAC and os.path.exists(serve_unit_file()) and not serve:
        out.append(ck("warn", f"装了后台服务 {SERVE_UNIT}，但它没在运行",
                      f"systemctl --user restart {SERVE_UNIT}；看报错：journalctl --user -u {SERVE_UNIT} -n 50"))
    if err:
        out.append(ck("warn", f"后台上一轮出错：{err}", "偶尔一次不要紧；一直出现的话按错误信息处理"))
    return bool(held or (mb_age is not None and mb_age < 180)), refresh_min, out


def usage_freshness(data, running, refresh_min):
    """各账号上次刷新用量是什么时候（体检之前的，不算这次体检自己刷新的）。有后台在跑时超过 30 分钟算没在刷新。"""
    if refresh_min == 0:
        return [ck("info", "后台没开定时刷新用量（刷新间隔设成了 0）")]
    limit = max(USAGE_STALE_SECS, (refresh_min or 0) * 120)
    now = dt.datetime.now(dt.timezone.utc)
    out = []
    for a in data["accounts"]:
        u = a.get("usage") or {}
        if a["kind"] == "claude_web" and (not u or "未填写 sessionKey" in (u.get("error") or "")):
            continue  # 没填 sessionKey 的网页号本来就不读用量
        t = parse_iso(u.get("checked_at") if a["kind"] == "claude_desktop" else u.get("fetched_at"))
        name = a.get("label") or a["id"]
        if t is None:
            out.append(ck("info", f"{name}：还没刷新过用量"))
        elif running and (now - t).total_seconds() > limit:
            out.append(ck("warn", f"{name}：用量上次刷新 {fmt_ago(t.isoformat())}，后台没在刷新它",
                          "有后台在跑时一般每 10 分钟左右刷新一次；看看上面后台任务有没有出错，手动刷新：cam usage"))
        else:
            out.append(ck("ok" if running else "info", f"{name}：用量上次刷新 {fmt_ago(t.isoformat())}"))
    return out


def checkup_cam(store, data):
    out = [ck("info", f"数据目录：{store.root}，已保存 {len(data['accounts'])} 个账号")]
    running, refresh_min, items = background_status(store, data)
    out += items + usage_freshness(data, running, refresh_min)
    pend = pending_switch_view(data)
    if pend:
        out.append(ck("info", pend["message"], "cam switch --cancel-wait 可取消"))
    upd = saved_update_check()
    if upd is None:
        out.append(ck("info", "还没检查过更新", "cam update --check 联网看看有没有新版本（菜单栏、面板里也能点）"))
    elif not upd.get("checkout"):
        out.append(ck("info", "这份代码不是用 git 下载的，没法用 cam update 更新", upd.get("message") or ""))
    else:
        ago = fmt_ago(upd.get("checked_at"))
        if upd.get("failed"):
            out.append(ck("warn", f"上次更新后自检没过，已经退回原来的版本：{upd['failed']}"))
        if upd.get("behind"):
            blocked = upd.get("ahead") or upd.get("dirty")
            out.append(ck("warn", f"有 {upd['behind']} 个新改动可以更新（{ago}检查的）",
                          upd.get("message", "") if blocked else "cam update（先 cam update --check 看改了什么）"))
        else:
            out.append(ck("ok", f"更新：{upd.get('message') or '已经是最新版本'}（{ago}检查的）"))
        if upd.get("dirty"):
            out.append(ck("info", f"代码目录里有 {len(upd['dirty'])} 个文件改过没提交（检查更新时看到的）",
                          "有新版本时要先提交或撤销这些改动才能 cam update"))
    try:
        n, bad = verify_sums()
        out.append(ck("bad", f"{len(bad)} 个文件和 {SUMS_FILE} 对不上：" + "、".join(bad[:3]) + ("…" if len(bad) > 3 else ""),
                      "代码不完整或被改过：重新下载一份完整的代码；确认是自己改的，在项目目录运行 python3 scripts/gen-sums.py")
                   if bad else ck("ok", f"代码文件完整：{n} 个文件都和 {SUMS_FILE} 一致"))
    except CamError as e:
        out.append(ck("warn", str(e)))
    out += permission_items(store)
    return out + backup_items(store)


def permission_items(store):
    """数据目录和密钥文件只能自己读：目录 0700，secrets 里的文件和 server-token 0600。"""
    if IS_WIN:
        return []
    paths = [(store.root, 0o700), (store.secrets_dir, 0o700), (os.path.join(store.root, "server-token"), 0o600)]
    paths += [(p, 0o600) for p in sorted(glob.glob(os.path.join(store.secrets_dir, "*.json")))]
    bad = []
    for path, want in paths:
        try:
            mode = os.stat(path).st_mode & 0o777
        except OSError:
            continue
        if mode & ~want:
            bad.append((path, mode, want))
    if not bad:
        return [ck("ok", "数据目录权限正常：目录只有自己能进（0700），密钥文件只有自己能读（0600）")]
    out = [ck("warn", f"{path} 的权限是 0{mode:o}，同一台电脑上的其他用户可能读得到", f"chmod {want:o} {path}")
           for path, mode, want in bad[:5]]
    if len(bad) > 5:
        out.append(ck("warn", f"还有 {len(bad) - 5} 个文件权限也太宽", f"chmod 600 {store.secrets_dir}/*.json"))
    return out


def backup_items(store):
    try:
        names = [n for n in os.listdir(store.backups_dir) if os.path.isdir(os.path.join(store.backups_dir, n))]
    except OSError:
        names = []
    if not names:
        return [ck("info", "备份：还没有（每次切换前会自动备份当前登录）")]
    newest = max(names, key=lambda n: os.path.getmtime(os.path.join(store.backups_dir, n)))
    at = dt.datetime.fromtimestamp(os.path.getmtime(os.path.join(store.backups_dir, newest)))
    return [ck("info", f"备份：{len(names)} 份，最新 {at:%Y-%m-%d %H:%M}（{newest}），共 {human_size(path_size(store.backups_dir))}",
               f"在 {store.backups_dir}；切换前的现场备份只留最近 {BACKUP_KEEP} 份，删除账号、退出登录留下的不会自动清理")]


# ---- 5. 聊天机器人（claude-channels 装的 claude-discord / claude-feishu / claude-dingtalk）

def installed_bots():
    out = []
    for cli, bot in BOT_CLIS:
        path = os.path.join(home(), ".local", "bin", cli)
        if os.path.isfile(path) and os.access(path, os.X_OK):
            out.append((cli, bot, path))
    return out


def run_bot_doctor(path):
    """运行 <机器人命令> doctor，返回 (退出码, 输出)；超时退出码是 None。"""
    try:
        r = subprocess.run([path, "doctor"], capture_output=True, timeout=BOT_DOCTOR_TIMEOUT, stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
        return None, ""
    except OSError as e:
        return -1, str(e)
    return r.returncode, decode_output(r.stdout) + decode_output(r.stderr)


def bot_doctor_items(cli, bot, rc, text):
    """从机器人自检的输出里挑出 ✗ 的行（下一行“→ …”是怎么办）；没有就是全部正常。"""
    name = bot_name(bot)
    lines = [re.sub(r"\x1b\[[0-9;]*m", "", l).strip() for l in (text or "").splitlines()]
    out = []
    for i, line in enumerate(lines):
        if line.startswith("✗"):
            nxt = lines[i + 1] if i + 1 < len(lines) else ""
            out.append(ck("bad", f"{name}：{line[1:].strip()}", nxt[1:].strip() if nxt.startswith("→") else f"运行 {cli} doctor 看详情"))
    if rc is None:
        out.append(ck("warn", f"{name}：{cli} doctor 超过 {BOT_DOCTOR_TIMEOUT} 秒没跑完", f"手动运行 {cli} doctor 看看卡在哪"))
    elif not out and rc != 0:
        out.append(ck("warn", f"{name}：{cli} doctor 出错（退出码 {rc}）：{brief(text, last=True)}", f"手动运行 {cli} doctor 看完整输出"))
    elif not out:
        out.append(ck("ok", f"{name}：全部正常（{cli} doctor）"))
    return out


def checkup_bots(offline, results):
    bots = installed_bots()
    if not bots:
        return None  # 没装就不显示这一节
    out = []
    for cli, bot, _ in bots:
        if offline:
            out.append(ck("info", f"{bot_name(bot)}：离线模式没有运行 {cli} doctor（它要联网）"))
        else:
            out += bot_doctor_items(cli, bot, *results.get(cli, (-1, "没有运行")))
    return out


# ---- 汇总

def _quiet(fn, *args):
    """在线程里跑一项检查：出错时返回 (None, 错误)，不让一项拖垮整个体检。"""
    try:
        return fn(*args), None
    except Exception as e:  # noqa: BLE001
        return None, f"{type(e).__name__}: {e}" if not isinstance(e, CamError) else str(e)


def full_checkup(store, offline=False):
    """cam doctor：全面体检。返回 {"version", "generated_at", "offline", "header", "sections": [{"key", "title", "items"}],
    "summary": {"bad", "warn", "text", "first"}}，每条 {"level", "text", "advice"}（账号的条目多一个 "group"）。不含任何密钥。

    联网时（不加 --offline）几件事并行：刷新所有账号的用量（和 cam usage 一样，不在用的 Claude Code 号 access token
    快过期了就用它自己的 refresh token 续一次，续不上就是登录失效了；当前登录从不替它续，交给 Claude Code）、
    实测 Claude 服务、官方状态页、查 Claude Code 新版本、运行聊天机器人的自检。
    """
    before = store.load()  # 体检之前的样子：看后台有没有在刷新、哪个号是这次续的
    active = before["active"].get("claude_code")
    before_exp = {a["id"]: ((_quiet(store.load_secret, a["id"])[0] or {}).get("claudeAiOauth") or {}).get("expiresAt") or 0
                  for a in before["accounts"] if a["kind"] == "claude_code" and a["id"] != active}
    svc = summary = latest = refresh_error = None
    bot_results = {}
    if not offline:
        bots = installed_bots()
        with ThreadPoolExecutor(max_workers=4 + len(bots)) as ex:
            f_ref = ex.submit(_quiet, refresh_usage, store)
            f_svc = ex.submit(_quiet, service_check, store)
            f_sum = ex.submit(_quiet, fetch_status_summary)
            f_rel = ex.submit(_quiet, latest_claude_version)
            f_bots = {cli: ex.submit(run_bot_doctor, path) for cli, _, path in bots}
            refresh_error = f_ref.result()[1]
            svc, svc_err = f_svc.result()
            if svc_err:
                svc = [ck("bad", f"服务检查出错：{svc_err}", "稍后再运行 cam doctor")]
            summary, latest = f_sum.result()[0], f_rel.result()[0]
            bot_results = {cli: f.result() for cli, f in f_bots.items()}

    def section(key, title, fn, *args):  # 一节出错只影响这一节
        items, err = _quiet(fn, *args)
        return {"key": key, "title": title, "items": items if err is None else [ck("bad", f"这一节没查完：{err}", "把这一行发给作者")]}

    sections = [section("accounts", "账号", checkup_accounts, store, before_exp, offline, refresh_error),
                section("service", "Claude 服务", checkup_service, store, offline, svc, summary),
                section("claude_code", "本机 Claude Code", checkup_local_claude, store, latest),
                section("cam", "cam 自己", checkup_cam, store, before)]
    bots = checkup_bots(offline, bot_results)
    if bots is not None:
        sections.append({"key": "bots", "title": "聊天机器人", "items": bots})
    counted = [i for s in sections for i in s["items"] if i.get("kind") != "summary"]  # 服务检查的总体结论是重复的，不计数
    bad = [i for i in counted if i["level"] == "bad"]
    warn = [i for i in counted if i["level"] == "warn"]
    first = (bad or warn or [None])[0]  # 菜单栏通知里说的“第一项”，带上账号名
    return {"version": VERSION, "generated_at": now_iso(), "offline": bool(offline),
            "header": checkup_header(),
            "sections": sections,
            "summary": {"bad": len(bad), "warn": len(warn), "text": f"{len(bad)} 项要处理，{len(warn)} 项留意",
                        "first": ((f"{first['group']}：" if first.get("group") else "") + first["text"]) if first else None}}


def checkup_header():
    return f"cam {VERSION}，Python {platform.python_version()}，{platform.system()} {platform.release()}"


def checkup_brief(summary):
    """一句话结论（菜单栏通知用）：一切正常 / N 项要处理：第一项 / M 项留意：第一项。"""
    if summary["bad"]:
        return f"{summary['bad']} 项要处理：{summary['first']}"
    if summary["warn"]:
        return f"{summary['warn']} 项留意：{summary['first']}"
    return "一切正常"


def print_checkup(res):
    for n, sec in enumerate(res["sections"], 1):
        print("\n" + color(f"{n}. {sec['title']}", "b"))
        group = None
        for it in sec["items"]:
            if it.get("group") and it["group"] != group:
                group = it["group"]
                print(color(f"[{group}]", "c"))
            mark, c = CHECK_MARKS[it["level"]]
            print(f"{color(mark, c)} {it['text']}")  # 标记顶格：install.sh 按 ^(✗|!) 挑出要留意的行
            if it.get("advice"):
                print(color(f"    → {it['advice']}", "d"))
    s = res["summary"]
    print("\n" + color(f"总结：{s['text']}" + ("，一切正常" if not s["bad"] and not s["warn"] else ""),
                       "r" if s["bad"] else "y" if s["warn"] else "g"))


def cmd_doctor(store, as_json=False, offline=False):
    """全面体检，逐项列出 ✓ / ! / ✗ / ·，有问题的带一句怎么办。不打印任何密钥。"""
    if as_json:
        print(json.dumps(full_checkup(store, offline), ensure_ascii=False, indent=2))
        return 0
    print(checkup_header())  # 第一行：install.sh 安装完会把它显示出来
    if not offline:
        print(color("正在体检：刷新各账号的用量（不在用的号快过期了会替它续一次）、实测 Claude 服务，要十几秒…", "d"), flush=True)
    print_checkup(full_checkup(store, offline))
    return 0


# --------------------------------------------------------------------------- 文件校验与手动更新

SUMS_FILE = "SHA256SUMS"  # 项目根目录的校验清单，scripts/gen-sums.py 生成，sha256sum -c / shasum -a 256 -c 也能直接用
UPDATE_CHECK_FILE = "update-check.json"  # 数据目录里存上次检查更新的结果，菜单栏和面板据它显示“更新（N 个新改动）”
FETCH_TIMEOUT = 60
SERVE_UNIT = "cam-serve.service"  # install.sh 在 Linux 上装的后台 cam serve（systemd 用户服务）
NOT_CHECKOUT = ("这份代码不是用 git 下载的（没有 .git 目录），没法用 cam update 更新。下载新版本的完整代码，解压后重新运行"
                "里面的安装脚本（macOS 双击“安装.command”，Linux 运行 bash install.sh，Windows 双击“安装.cmd”），"
                "cam 命令会改用新目录；账号数据在 ~/.claude-accounts，不受影响。")


def code_root():
    """cam.py 所在的项目目录（scripts 的上一级）。install.sh 装的 cam 命令、菜单栏插件、cam serve 用的都是这份代码。"""
    return os.path.dirname(os.path.dirname(os.path.realpath(__file__)))


def file_sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def verify_sums(root=None):
    """按项目根目录的 SHA256SUMS 逐个核对文件。

    返回 (核对了几个文件, [对不上的文件和原因])。只核对清单里列出的文件，多出来的不管；没有 SHA256SUMS 抛 CamError。
    """
    root = root or code_root()
    try:
        with open(os.path.join(root, SUMS_FILE), "r", encoding="utf-8") as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        raise CamError(f"没找到 {SUMS_FILE}，没法校验。代码可能不完整，重新下载一份")
    n, bad = 0, []
    for line in lines:
        if not line.strip():
            continue
        m = re.match(r"([0-9a-fA-F]{64}) [ *](.+)$", line)
        parts = m.group(2).split("/") if m else []
        if not m or not parts[0] or ":" in parts[0] or ".." in parts or "\\" in m.group(2):  # 只认项目里的相对路径
            bad.append(f"{SUMS_FILE} 里有一行看不懂：{line[:80]}")
            continue
        n += 1
        try:
            same = file_sha256(os.path.join(root, *parts)) == m.group(1).lower()
        except FileNotFoundError:
            bad.append(f"{m.group(2)}（缺少这个文件）")
            continue
        except OSError as e:
            bad.append(f"{m.group(2)}（读不了：{e.strerror or e}）")
            continue
        if not same:
            bad.append(f"{m.group(2)}（内容不一致）")
    return n, bad


def cmd_verify():
    n, bad = verify_sums()
    if bad:
        print(color(f"✗ 有 {len(bad)} 个文件和 {SUMS_FILE} 对不上：", "r"))
        for b in bad:
            print(f"    {b}")
        print("  代码不完整或被改过：重新下载一份完整的代码。确认是你自己改的，在项目目录运行 python3 scripts/gen-sums.py 重新生成")
        return 1
    print(color("✓", "g") + f" {n} 个文件都和 {SUMS_FILE} 一致")
    return 0


def brief(text, last=False):
    """报错里最有用的一行：git 的看开头（ssh / fatal 那行），Python 的看最后一行（异常类型和说明）。"""
    lines = [l.strip() for l in (text or "").splitlines() if l.strip() and not l.strip().startswith("hint:")]
    if not lines:
        return "没有输出"
    return (lines[-1] if last else lines[0])[:300]


def git(root, *args, timeout=30):
    """在代码目录里跑 git。不弹账号密码输入（没有终端时直接失败），超时抛 CamError。"""
    env = dict(os.environ, GIT_TERMINAL_PROMPT="0")
    try:
        return subprocess.run(["git", "-C", root, "-c", "core.quotepath=false", *args], capture_output=True, text=True,
                              encoding="utf-8", errors="replace", timeout=timeout, env=env, stdin=subprocess.DEVNULL)
    except OSError:
        raise CamError("没找到 git 命令，先安装 git 再试")
    except subprocess.TimeoutExpired:
        raise CamError(f"git {args[0]} 超过 {timeout} 秒没完成，检查网络后重试")


def git_out(root, *args, timeout=30):
    r = git(root, *args, timeout=timeout)
    if r.returncode != 0:
        raise CamError(f"git {args[0]} 失败：{brief(r.stderr or r.stdout)}")
    return r.stdout.rstrip()


def is_git_checkout(root):
    """root 是不是一个 git 仓库的根目录（git clone 下来的代码）。下载的压缩包没有 .git，不算。"""
    if not os.path.exists(os.path.join(root, ".git")):
        return False
    r = git(root, "rev-parse", "--show-toplevel")
    return r.returncode == 0 and os.path.normcase(os.path.realpath(r.stdout.strip())) == os.path.normcase(os.path.realpath(root))


def local_changes(root):
    """代码目录里没提交的改动，git status 的格式。只看 git 管理的文件：新建的文件不影响快进，也不会被退回时删掉。"""
    return [l for l in git_out(root, "status", "--porcelain", "--untracked-files=no").splitlines() if l.strip()]


def update_state_path():
    return os.path.join(cam_home(), UPDATE_CHECK_FILE)


def save_update_check(res):
    try:
        write_json_atomic(update_state_path(), res)
    except OSError:
        pass
    return res


def saved_update_check(root=None):
    """上次检查更新的结果，不联网。代码已经不是检查时那个版本（比如自己 git pull 过）时，不再说“有新改动”。"""
    root = root or code_root()
    try:
        res = read_json(update_state_path(), None)
    except CamError:
        return None
    if not isinstance(res, dict) or res.get("root") != root:
        return None
    if res.get("behind"):
        try:
            head = git_out(root, "rev-parse", "HEAD")
        except CamError:
            head = None
        if head != res.get("head"):
            res = dict(res, behind=0, commits=[], stale=True, message="代码在检查之后变过了，重新检查一下")
    return res


def update_check(root=None):
    """联网检查有没有新版本（git fetch，最多等 FETCH_TIMEOUT 秒），只看不改。

    返回并保存 {"checkout", "upstream", "head", "behind", "ahead", "commits": [{"hash", "date", "subject"}], "dirty", "message"}。
    不是 git 仓库时 checkout=False，message 说明怎么换新版本；没有上游、fetch 失败抛 CamError。
    """
    root = root or code_root()
    res = {"checked_at": now_iso(), "version": VERSION, "root": root, "checkout": False, "behind": 0, "ahead": 0,
           "commits": [], "dirty": []}
    if not is_git_checkout(root):
        res["message"] = NOT_CHECKOUT
        return save_update_check(res)
    res["checkout"] = True
    r = git(root, "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}")
    if r.returncode != 0:
        raise CamError("当前分支没有对应的远程分支，没法检查更新。在代码目录运行 git branch -u origin/main 设好后再试")
    res["upstream"] = r.stdout.strip()
    r = git(root, "fetch", "--quiet", timeout=FETCH_TIMEOUT)
    if r.returncode != 0:
        raise CamError(f"git fetch 失败，没连上代码仓库：{brief(r.stderr or r.stdout)}")
    res["head"] = git_out(root, "rev-parse", "HEAD")
    res["behind"] = int(git_out(root, "rev-list", "--count", "HEAD..@{u}") or 0)
    res["ahead"] = int(git_out(root, "rev-list", "--count", "@{u}..HEAD") or 0)
    log = git_out(root, "log", "-n", "50", "--format=%h%x1f%cI%x1f%s", "HEAD..@{u}")
    res["commits"] = [dict(zip(("hash", "date", "subject"), l.split("\x1f", 2))) for l in log.splitlines() if l]
    res["dirty"] = local_changes(root)
    if not res["behind"]:
        res["message"] = f"已经是最新版本（{res['upstream']}，{res['head'][:7]}）"
    elif res["ahead"]:
        res["message"] = (f"有 {res['behind']} 个新改动，但本地有 {res['ahead']} 个提交不在 {res['upstream']} 上，没法直接快进。"
                          "在代码目录自己合并（git pull --rebase）")
    elif res["dirty"]:
        res["message"] = f"有 {res['behind']} 个新改动；代码目录里有没提交的改动，先提交或撤销才能更新"
    else:
        res["message"] = f"有 {res['behind']} 个新改动可以更新"
    return save_update_check(res)


def update_self_check(root):
    """更新后的自检，不碰真实数据。返回没通过的原因，通过返回 None。

    1. 文件和 SHA256SUMS 一致；2. scripts/*.py 都能编译；3. 用临时的 HOME、数据目录、Claude 配置目录跑 cam --help 和 cam ls。
    """
    try:
        _, bad = verify_sums(root)
    except CamError as e:
        return str(e)
    if bad:
        return f"{len(bad)} 个文件和 {SUMS_FILE} 对不上：" + "、".join(bad[:5]) + ("…" if len(bad) > 5 else "")
    for path in sorted(glob.glob(os.path.join(root, "scripts", "*.py"))):
        try:
            with open(path, "rb") as f:
                compile(f.read(), path, "exec", dont_inherit=True)
        except (SyntaxError, ValueError) as e:
            return f"scripts/{os.path.basename(path)} 编译不过：{e}"
    tmp = tempfile.mkdtemp(prefix="cam-selfcheck-")
    try:
        env = dict(os.environ, HOME=tmp, USERPROFILE=tmp, CAM_HOME=os.path.join(tmp, "cam"),
                   CLAUDE_CONFIG_DIR=os.path.join(tmp, "claude"), CAM_NO_KEYCHAIN="1", CAM_NO_NOTIFY="1",
                   CAM_NO_BG_CHECK="1", NO_COLOR="1", PYTHONIOENCODING="utf-8", PYTHONDONTWRITEBYTECODE="1")
        for args in (["--help"], ["ls"]):
            shown = "cam " + " ".join(args)
            try:
                r = subprocess.run([sys.executable, os.path.join(root, "scripts", "cam.py"), *args], env=env, cwd=tmp,
                                   capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=60,
                                   stdin=subprocess.DEVNULL)
            except (OSError, subprocess.SubprocessError) as e:
                return f"{shown} 没能运行：{e}"
            if r.returncode != 0:
                return f"{shown} 出错（退出码 {r.returncode}）：{brief(r.stderr or r.stdout, last=True)}"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return None


def serve_unit_file():
    return os.path.join(os.environ.get("XDG_CONFIG_HOME") or os.path.join(home(), ".config"), "systemd", "user", SERVE_UNIT)


def restart_serve_service(later=False):
    """重启 install.sh 装的后台 cam serve，让它用上新代码。没在运行就不动它。

    later=True：发起更新的可能就是这个服务自己（面板里点的更新），先把结果返回给页面，一秒后再请 systemd 重启。
    """
    env = dict(os.environ)  # 延后执行时也用现在的环境

    def run(*args, timeout=30):
        try:
            return subprocess.run(["systemctl", "--user", *args], capture_output=True, timeout=timeout, env=env).returncode == 0
        except (OSError, subprocess.SubprocessError):
            return False
    if not run("is-active", "--quiet", SERVE_UNIT, timeout=10):
        return "后台的 cam serve 服务没在运行，下次启动时就是新版本"
    if later:
        threading.Timer(1.0, run, ("--no-block", "try-restart", SERVE_UNIT)).start()
        return "后台的 cam serve 服务马上重启，几秒后刷新页面就是新版本"
    if run("try-restart", SERVE_UNIT):
        return "已重启后台的 cam serve 服务"
    return f"后台的 cam serve 服务没能重启，手动运行 systemctl --user restart {SERVE_UNIT}"


def update_restart_notes(later=False):
    """更新后哪些正在运行的东西要重开才用上新版本。"""
    notes = []
    if not IS_WIN and not IS_MAC and os.path.exists(serve_unit_file()) and shutil.which("systemctl"):
        notes.append(restart_serve_service(later))
    if IS_MAC:
        notes.append("菜单栏小工具下次刷新（一分钟内）就是新版本")
    notes.append("自己开着的 cam serve（包括 cam panel 在后台起的）要关掉重开才是新版本")
    notes.append("桌面版是单独的安装包，不跟着更新，要更新就重新编译安装")
    return notes


def apply_update(root=None, restart_later=False):
    """cam update：快进到远程分支的最新版本，自检通过才算数，不通过就退回原来的版本。

    只在代码目录没有未提交的改动时才动手，所以退回用 git reset --hard 不会丢东西。不碰数据目录。
    同一时间只让一个更新在跑（命令行、菜单栏、面板都可能发起）。
    """
    root = root or code_root()
    lease = FileLock(os.path.join(cam_home(), "update.lock"))
    if not lease.try_acquire():
        raise CamError("另一个更新正在进行，稍后再试")
    try:
        return _apply_update(root, restart_later)
    finally:
        lease.release()


def _apply_update(root, restart_later):
    if not is_git_checkout(root):
        raise CamError(NOT_CHECKOUT)
    dirty = local_changes(root)
    if dirty:
        raise CamError("代码目录里有没提交的改动，没有更新，什么都没改：\n    " + "\n    ".join(dirty[:20])
                       + ("\n    …" if len(dirty) > 20 else "")
                       + f"\n  先在 {root} 里提交或撤销这些改动（git stash 可以先收起来），再更新")
    res = update_check(root)
    if not res["behind"]:
        return dict(res, updated=False, restart=[])
    if res["ahead"]:
        raise CamError(res["message"] + "。什么都没改")
    old = res["head"]
    r = git(root, "merge", "--ff-only", "--quiet", "@{u}", timeout=120)
    if r.returncode != 0:
        raise CamError(f"快进合并没成功，什么都没改：{brief(r.stderr or r.stdout)}")
    problem = update_self_check(root)
    if problem:
        r = git(root, "reset", "--hard", "--quiet", old)
        if r.returncode != 0:
            raise CamError(f"更新后自检没过（{problem}），退回原来的版本也失败了：{brief(r.stderr)}。"
                           f"在 {root} 里运行 git reset --hard {old[:12]} 手动退回")
        save_update_check(dict(res, failed=problem))
        raise CamError(f"更新后自检没过，已经退回原来的版本：{problem}")
    new = git_out(root, "rev-parse", "HEAD")
    save_update_check(dict(res, checked_at=now_iso(), head=new, behind=0, commits=[],
                           message=f"已经是最新版本（{res['upstream']}，{new[:7]}）"))
    return dict(res, updated=True, old=old, new=new, restart=update_restart_notes(restart_later),
                message=f"已更新到 {new[:7]}（原来是 {old[:7]}），{res['behind']} 个新改动，自检通过")


def print_commits(res):
    for c in res["commits"]:
        print(f"    {color(c['hash'], 'c')} {c['date'][:10]}  {c['subject']}")
    if res["behind"] > len(res["commits"]):
        print(f"    …还有 {res['behind'] - len(res['commits'])} 个")


def cmd_update(args):
    """cam update [--check] [--json]。只在用户运行时联网，从不自动更新。"""
    if args.json:
        try:
            res = update_check() if args.check else apply_update()
        except CamError as e:
            print(json.dumps({"error": str(e)}, ensure_ascii=False, indent=2))
            return 1
        print(json.dumps(res, ensure_ascii=False, indent=2))
        return 0 if res["checkout"] else 1
    print(color("正在检查更新（git fetch）…", "d"), flush=True)
    res = update_check() if args.check else apply_update()
    if not res["checkout"]:
        print(res["message"])
        return 1
    if not res["behind"]:
        print(color("✓", "g") + f" {res['message']}")
    elif args.check:
        print(f"{res['upstream']} 上有 {res['behind']} 个新改动：")
        print_commits(res)
        print(color(res["message"], "y") if res["ahead"] or res["dirty"]
              else "运行 cam update 更新（只做快进；更新后自检没过会自动退回原来的版本）")
    else:
        print(color("✓", "g") + f" {res['message']}。这次的改动：")
        print_commits(res)
        for note in res["restart"]:
            print(color(f"  · {note}", "d"))
    return 0


def cmd_vault(store, args):
    state = vault_route(store, "GET", ["vault"])
    if args.vault_cmd == "setup":
        password = getpass.getpass("设置本地账号库主密码（至少 8 字符）：")
        if password != getpass.getpass("再次输入主密码："):
            raise CamError("两次主密码不一致")
        vault_route(store, "POST", ["vault", "setup"], {"master_password": password})
        print("本地账号库已创建，密码和邮箱授权将加密保存。")
        return
    if not state["initialized"]:
        raise CamError("先运行 cam vault setup 创建本地账号库")
    if not state["unlocked"]:
        state = vault_route(store, "POST", ["vault", "unlock"], {"master_password": getpass.getpass("账号库主密码：")})
    if args.cmd == "mail":
        if args.mail_cmd == "read":
            result = vault_route(store, "POST", ["vault", "entries", args.entry, "messages"], {"since": args.since} if args.since else {})
            print(json.dumps(result, ensure_ascii=False, indent=2))
        else:
            result = vault_route(store, "POST", ["vault", "entries", args.entry, "connect"])
            print("请在本机系统浏览器打开此地址完成邮箱授权：\n" + result["authorization_url"])
            if args.open_browser:
                open_external_url(result["authorization_url"])
            while True:
                status = vault_route(store, "GET", ["vault", "oauth", result["job_id"]])
                if status["status"] != "pending":
                    if status["status"] == "error":
                        raise CamError(status.get("message") or "邮箱授权未完成")
                    print(status.get("message") or "邮箱已连接")
                    break
                time.sleep(0.5)
        return
    if args.vault_cmd in (None, "ls"):
        for entry in state["entries"]:
            print(f"{entry['id']}  {entry['label']}  {entry['email']}  [{entry['provider']}]" + (" 已连接" if entry["connected"] else ""))
        if not state["entries"]:
            print("账号库还没有条目，可在管理界面的“账号库与邮箱”里添加。")
    elif args.vault_cmd == "add":
        body = {"label": args.label or args.email, "email": args.email, "provider": args.provider,
                "account_id": find_account(store.load(), args.account)["id"] if args.account else None,
                "config": {"client_id": args.client_id or "", "tenant": args.tenant,
                           "imap_host": args.imap_host or "", "imap_port": args.imap_port,
                           "imap_username": args.username or args.email}}
        if args.provider in ("credential", "imap"):
            body["password"] = getpass.getpass("账号密码 / 邮箱应用专用密码（可留空）：")
        if args.provider == "gmail":
            body["client_secret"] = getpass.getpass("Google 桌面应用 client secret（可留空）：")
        result = vault_route(store, "POST", ["vault", "entries"], body)
        print("已加密保存。")
        for entry in result["entries"]:
            print(f"{entry['id']}  {entry['email']}  [{entry['provider']}]")
    elif args.vault_cmd == "rm":
        vault_route(store, "DELETE", ["vault", "entries", args.entry])
        print("已移除账号库条目，Claude 账号配置保持原样。")
    elif args.vault_cmd == "reveal":
        print(vault_route(store, "POST", ["vault", "entries", args.entry, "reveal"])["password"])


def main(argv=None):
    if os.name == "nt":  # Windows 上输出到管道时按系统代码页编码，英文系统（cp1252）遇到中文会直接报错；托盘和测试都按 UTF-8 读
        for stream in (sys.stdout, sys.stderr):
            if not stream.isatty() and hasattr(stream, "reconfigure"):
                stream.reconfigure(encoding="utf-8", errors="replace")
    p = argparse.ArgumentParser(prog="cam", description="Claude 多账号管理（脚本版）",
                                formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    p.add_argument("--version", action="version", version=VERSION)
    sub = p.add_subparsers(dest="cmd")

    s = sub.add_parser("identify", help="从已登录的 Claude 配置补全账号邮箱")
    s.add_argument("account")
    s = sub.add_parser("vault", help="本地加密账号库，密码通过隐藏输入读取")
    vsub = s.add_subparsers(dest="vault_cmd")
    vsub.add_parser("setup", help="创建账号库并设置主密码")
    vsub.add_parser("ls", help="解锁并列出账号库条目")
    v = vsub.add_parser("add", help="添加账号资料或邮箱连接")
    v.add_argument("--email", required=True)
    v.add_argument("--provider", choices=("credential", "gmail", "outlook", "imap"), default="credential")
    v.add_argument("--label")
    v.add_argument("--account", help="绑定已有的 Claude 账号")
    v.add_argument("--client-id")
    v.add_argument("--tenant", default="common")
    v.add_argument("--imap-host")
    v.add_argument("--imap-port", type=int, default=993)
    v.add_argument("--username")
    for name in ("rm", "reveal"):
        v = vsub.add_parser(name, help="移除条目" if name == "rm" else "明确显示已保存的账号密码")
        v.add_argument("entry")
    s = sub.add_parser("mail", help="授权邮箱或读取 Claude 登录验证码")
    msub = s.add_subparsers(dest="mail_cmd", required=True)
    m = msub.add_parser("connect", help="等待本机浏览器完成 Gmail / Outlook 授权")
    m.add_argument("entry")
    m.add_argument("--open-browser", action="store_true")
    m = msub.add_parser("read", help="只读获取近期 Claude 登录邮件摘要")
    m.add_argument("entry")
    m.add_argument("--since")

    s = sub.add_parser("import", help="保存当前 Claude Code 登录的账号")
    s.add_argument("--label")
    s = sub.add_parser("login", help="登录另一个 Claude Code 账号并保存（先存好当前账号，再 claude auth login）")
    s.add_argument("label", nargs="?", help="给新账号起的名字（可选）")
    sub.add_parser("ls", aliases=["list"], help="列出账号（不联网）")
    s = sub.add_parser("usage", help="刷新用量并列出")
    s.add_argument("account", nargs="?")
    s = sub.add_parser("switch", aliases=["use"], help="切换 Claude Code 账号")
    s.add_argument("account", nargs="?")
    s.add_argument("-y", "--yes", "--now", action="store_true",
                   help="有 claude 正在运行时也不再确认，直接切（不加时：终端里会问；没有终端时登记成等它们退出后再切）")
    s.add_argument("-w", "--wait", action="store_true", help="有 claude 正在运行时等它们都退出后再切（本命令会一直等）")
    s.add_argument("--cancel-wait", action="store_true", help="取消登记中的等待切换")
    sub.add_parser("current", aliases=["whoami"], help="显示当前 Claude Code 账号")
    s = sub.add_parser("add-web", help="添加网页版账号")
    s.add_argument("--label", required=True)
    s.add_argument("--email")
    s.add_argument("--session-key", help="可选，claude.ai 的 sessionKey Cookie，用于自动获取用量")
    s.add_argument("--app", action="store_true", help="以独立应用窗口打开")
    s = sub.add_parser("add-desktop", help="添加 Claude 桌面版账号（每个号一份登录，第一次点一次邮箱）")
    s.add_argument("--label", required=True)
    s.add_argument("--email")
    s.add_argument("--current", action="store_true", help="绑定这台电脑上已经登录的 Claude 桌面版（只适合第一个号）")
    s = sub.add_parser("open", help="打开网页版窗口，或切换 Claude 桌面版账号")
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
    s = sub.add_parser("diag", help="一个账号的诊断报告：现在的问题、出问题时的现场（出口 IP、环境）、用过的出口、当前环境，不含密钥")
    s.add_argument("account", help="账号名字、序号或邮箱")
    s.add_argument("--json", action="store_true", help="输出 JSON 而不是 Markdown")
    s.add_argument("--out", help="存成文件")
    s = sub.add_parser("serve", help="启动 Web 面板")
    s.add_argument("--lan", action="store_true", help="允许局域网（手机）访问")
    s.add_argument("--port", type=int, default=8787)
    s.add_argument("--refresh-every", type=int, default=10, help="自动刷新用量间隔（分钟，0 关闭）")
    s = sub.add_parser("panel", help="在浏览器打开 Web 面板（没在运行就在后台启动，只限本机访问）")
    s.add_argument("--port", type=int, default=8787)
    s = sub.add_parser("json", help="输出账号状态 JSON（给其它脚本用）")
    sub.add_parser("claude", help="按目录规则切号后启动 claude（参数原样透传）")
    s = sub.add_parser("doctor", help="全面体检：账号登录、Claude 服务、本机 Claude Code、cam 自己、聊天机器人（不显示密钥）")
    s.add_argument("--json", action="store_true", help="输出 JSON（给菜单栏、脚本用）：分区 → 每条 {level, text, advice}")
    s.add_argument("--offline", action="store_true", help="不联网：不续登录、不刷新用量、不测服务，只看本机和上次的结果")
    s = sub.add_parser("update", help="手动更新 cam：git 快进到最新版本，更新后自检不过就退回（从不自动更新）")
    s.add_argument("--check", action="store_true", help="只检查有没有新版本，列出新改动，不更新")
    s.add_argument("--json", action="store_true", help="输出 JSON（给菜单栏、脚本用）")
    sub.add_parser("verify", help="按 SHA256SUMS 校验代码文件是否完整（安装和更新时会自动做）")
    s = sub.add_parser("env", help="Claude Code 环境：检查 / 安装 / 清理")
    esub = s.add_subparsers(dest="env_cmd")
    e = esub.add_parser("doctor", aliases=["check"], help="同 cam doctor：全面体检")
    e.add_argument("--json", action="store_true", help="输出 JSON")
    e.add_argument("--offline", action="store_true", help="不联网")
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
    s = sub.add_parser("risk", help="cam claude 启动前的环境风险提醒档位（只提醒和确认，不拦截）")
    s.add_argument("--confirm", type=int, metavar="分", help="低于这个分数时问“是否继续 [y/N]”（默认 60）")
    s.add_argument("--strong", type=int, metavar="分", help="低于这个分数时要输入“继续”才启动（默认 30）")
    s.add_argument("--on", action="store_true", help="开启启动前提醒（默认开）")
    s.add_argument("--off", action="store_true", help="关闭启动前提醒")
    s.add_argument("--ip-lookup", choices=list(IP_LOOKUP_MODES), help="外部 IP 查询：auto（默认，可商用的数据源）/ ip-api（加上 ip-api.com，仅限非商业）/ off（关闭）")
    s.add_argument("--ipapi-key", metavar="KEY", help="ipapi.is 的 key（免费注册，每天 1000 次，可商用），空字符串删除")
    s.add_argument("--ipinfo-token", metavar="TOKEN", help="ipinfo.io 的 token，空字符串删除")
    s.add_argument("--clear-history", action="store_true", help="清除缓存的出口信息和账号使用地区记录")
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
    if argv[:1] == ["_check-region"]:  # 启动 claude 前另起的后台检测进程
        background_region_check(Store())
        return 0
    # cam claude …：后面的参数原样交给 claude，不经过 argparse
    if argv[:1] == ["claude"]:
        try:
            return run_claude(Store(), argv[1:]) or 0
        except CamError as e:
            print(color("错误：", "r") + str(e), file=sys.stderr)
            return 1
    args = p.parse_args(argv)
    if args.cmd == "verify":  # 只读代码目录，不建数据目录
        try:
            return cmd_verify()
        except CamError as e:
            print(color("错误：", "r") + str(e), file=sys.stderr)
            return 1
    store = Store()
    try:
        c = args.cmd
        if c == "mail":
            args.vault_cmd = None
        if c in (None, "ls", "list", "json"):
            acc = auto_import(store)
            if acc and c != "json":
                print(color(f"发现新登录的账号，已自动保存：{acc['label']}\n", "g"))
        if c in ("vault", "mail"):
            cmd_vault(store, args)
        elif c == "identify":
            result = identify_account(store, args.account)
            print(result["identity_message"])
        elif c is None:
            print_accounts(store, store.load())
            print(color("\n提示：cam usage 刷新用量，cam switch <账号> 切换，cam -h 查看全部命令", "d"))
        elif c == "import":
            cmd_import(store, args)
        elif c == "login":
            cmd_login(store, args)
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
        elif c == "add-desktop":
            acc = add_account(store, "claude_desktop", args.label, email=args.email, use_current=args.current)
            print(f"已添加桌面版账号 {acc['label']}，id={acc['id']}")
            print(f"运行 cam open {acc['label']} 切换。第一次会在窗口里点一次邮箱，之后不用再点。")
        elif c == "open":
            acc = find_account(store.load(), args.account)
            if acc.get("kind") == "claude_desktop":
                print(open_desktop_account(store, acc))
            else:
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
                      f"缓存读 {r['cache_read']:>12,}  合计(不含缓存读) {tot:>11,}  ≈ ${r['cost_usd']:,.2f}")
            if rep["rows"]:
                print(color(f"合计 ≈ ${sum(r['cost_usd'] for r in rep['rows']):,.2f}（按 API 价折算，订阅不按这个收费，只用来比较用量）", "d"))
        elif c == "diag":
            rep = account_diagnostic(store, store.load(), args.account)
            text = json.dumps(rep, ensure_ascii=False, indent=2) if args.json else diagnostic_text(rep)
            if args.out:
                with open(args.out, "w", encoding="utf-8") as f:
                    f.write(text)
                print(f"已写入 {args.out}")
            else:
                print(text)
        elif c == "serve":
            serve(store, "0.0.0.0" if args.lan else "127.0.0.1", args.port, args.refresh_every)
        elif c == "panel":
            print(f"已在浏览器打开管理面板：{open_panel(store, args.port)}")
        elif c == "doctor" or (c == "env" and args.env_cmd in (None, "doctor", "check")):
            return cmd_doctor(store, getattr(args, "json", False), getattr(args, "offline", False))
        elif c == "env" and args.env_cmd == "install":
            cmd_env_install(store, args)
        elif c == "env" and args.env_cmd == "clean":
            cmd_env_clean(store, args)
        elif c == "rule":
            cmd_rule(store, args)
        elif c == "risk":
            cmd_risk(store, args)
        elif c == "json":
            print(json.dumps(public_state(store, store.load()), ensure_ascii=False, indent=2))
        elif c == "update":
            return cmd_update(args)
        return 0
    except CamError as e:
        print(color("错误：", "r") + str(e), file=sys.stderr)
        if e.advice:
            print("  → " + e.advice, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
