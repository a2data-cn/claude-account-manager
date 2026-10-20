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
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor

if os.name == "nt":
    import msvcrt
else:
    import fcntl

VERSION = "1.2.1"

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
        note_account_region(data, target["id"], cached_region_loc(data))  # 给风险提示记一笔地区
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
        with urllib.request.urlopen(req, timeout=timeout, context=SSL_CTX) as r:
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
        print(color(f"[cam] 环境风险 {sc['score']}/100：{sc['label']}", "d"), file=err)
        return True
    print(color(f"[cam] 环境风险 {sc['score']}/100：{sc['label']}", "r" if tier == "strong" else "y"), file=err)
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
    except EOFError:
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
    accs = []
    active = data["active"].get("claude_code")
    for a in data["accounts"]:
        p = {k: a.get(k) for k in PUBLIC_FIELDS}
        p["is_active"] = a["id"] == active
        p["days_left"] = days_left(a)
        p["plan_display"] = a.get("plan") or (a.get("manual") or {}).get("plan")
        has_creds = False
        if a["kind"] == "claude_code":
            has_creds = bool((store.load_secret(a["id"]).get("claudeAiOauth") or {}).get("refreshToken"))
        p["health"] = account_health(a, has_creds, p["days_left"])
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
            "pending_switch": pending_switch_view(data), "region": region_view(data), "risks": risk_items(data),
            "risk_score": risk_score(risk_items(data)), "generated_at": now_iso()}


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
                    return self._send(200, {"items": service_check(store)})
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
                    check_region(store)  # 界面常显的出口地区，随用量刷新一起更新
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
        for iss in (a.get("health") or {}).get("issues") or []:
            c = {"error": "r", "warn": "y"}.get(iss["level"], "d")
            print("     " + color(f"{'✗ 异常' if iss['level'] == 'error' else '⚠ 注意' if iss['level'] == 'warn' else '·'}：{iss['text']}。{iss['advice']}", c))
        if (a.get("manual") or {}).get("note"):
            print("     " + color("备注：" + a["manual"]["note"], "d"))
    if st.get("warning"):
        print("\n" + color("⚠ " + st["warning"], "y"))
    risks = [r for r in st.get("risks") or [] if r["level"] in ("warn", "bad")]
    for i, r in enumerate(risks):
        print(("\n" if i == 0 else "") + color(f"! {r['text']}。{r['advice']}", "y"))
    sc = st.get("risk_score") or {}
    if sc and (st.get("region") or {}).get("status"):  # 后台检测过才有意义
        c = {"low": "g", "medium": "y", "high": "r"}[sc["grade"]]
        print(("" if risks else "\n") + color(f"环境风险评分 {sc['score']}/100：{sc['label']}", c)
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
    for r in risk_items(data):  # 只提醒，不拦
        if r["level"] in ("warn", "bad"):
            print(color(f"! {r['text']}。{r['advice']}", "y"))
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
    except (urllib.error.URLError, socket.timeout, OSError) as e:
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
    reuse = prev.get("ip") == out["ip"] and prev.get("lookup_mode") == cfg["mode"] and age is not None and age < META_TTL
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


def local_env():
    """本机的区域设置快照，和出口一起缓存，免得界面每次刷新都去跑系统命令。"""
    tz = system_timezone()
    lc = system_locale()
    return {"tz": tz[0] if tz else None, "tz_cc": tz[1] if tz else [],
            "locale": lc[0] if lc else None, "locale_cc": lc[1] if lc else None, "dns": system_dns_servers()}


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


def _run_text(cmd, timeout=8):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout
    except (OSError, subprocess.SubprocessError):
        return ""


def system_proxy():
    """系统级代理设置（不是环境变量）：["HTTPS 127.0.0.1:7890", "PAC http://…"] 之类，没有时为空列表。"""
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


def note_account_region(data, acc_id, loc):
    """记一笔“这个账号在这个出口地区用过”，给风险提示用（每个账号只留最近几个地区）。"""
    if not acc_id or not loc:
        return
    for a in data.get("accounts") or []:
        if a["id"] == acc_id:
            regs = [r for r in (a.get("regions") or []) if r.get("loc") != loc]
            regs.insert(0, {"loc": loc, "at": now_iso()})
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
        if old.get("loc") and reg["loc"] and old["loc"] != reg["loc"]:
            reg["prev_loc"], reg["changed_at"] = old["loc"], reg["checked_at"]
        elif old.get("prev_loc"):
            reg["prev_loc"], reg["changed_at"] = old["prev_loc"], old.get("changed_at")
        data["region"] = reg
        if sp:
            data["status_page"] = dict(sp, checked_at=reg["checked_at"])
        else:
            data.pop("status_page", None)
        note_account_region(data, (data.get("active") or {}).get("claude_code"), reg["loc"])
        ss = data.get("service_status")
        if reg["loc"] and isinstance(ss, dict):  # 连得上 claude.ai 了：网络、证书、claude.ai 的旧问题不再算
            ss["items"] = [i for i in ss.get("items") or [] if i.get("kind") == "api"]
            if not ss["items"]:
                data.pop("service_status", None)
        store.save(data)
    return region_view(data)


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
        add("proxy_ip", "warn", f"出口 IP 被数据库标记为代理 / VPN 出口（{reg.get('org') or '?'}，AS{reg.get('asn')}）",
            "服务端的 IP 库同样能查到这个标记，这是最常见的风控信号之一；人所在地仍按实际位置判断。")
    elif ip_type == "hosting" or (ip_type is None and hosting_exit(reg.get("org"))):
        add("isp", "warn", f"出口 IP 属于 {reg.get('org') or '?'}（AS{reg.get('asn')}），是机房 / 云服务器，不是家庭或公司宽带",
            "这类出口通常被当作 VPN / 代理流量看待，是常见的风控信号之一；人所在地仍按实际位置判断。")
    if reg.get("reg_loc") and loc and reg["reg_loc"] != loc:
        add("ip_origin", "info", f"出口 IP 注册在 {region_label(reg['reg_loc'])}，定位在 {region_label(loc)}（广播 IP）",
            "不同地理库可能把它判到不同国家，服务端看到的国家不一定和这里一致。")
    if reg.get("v4_loc") and reg.get("v6_loc") and reg["v4_loc"] != reg["v6_loc"]:
        add("ipv6_mismatch", "warn",
            f"IPv4 出口在 {region_label(reg['v4_loc'])}，IPv6 出口在 {region_label(reg['v6_loc'])}（{reg.get('v6_ip')}），两个协议走了不同的出口",
            "VPN / 代理通常只接管 IPv4，IPv6 直接从本机网络出去，服务端通过 IPv6 看到的就是本机网络的真实出口。"
            "如果 IPv6 这边在不支持地区，政策按实际所在地判断，风险与 IPv4 出口无关。")
    env = reg.get("env") if isinstance(reg.get("env"), dict) else None
    if env is None and reg.get("checked_at"):
        env = local_env()  # 旧版缓存没有这一段
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
    tz = (env["tz"], env.get("tz_cc") or []) if env and env.get("tz") else system_timezone()
    if tz and loc and tz[1] and loc not in tz[1]:
        zone, home = tz[0], tz[1][0]
        text = f"系统时区 {zone} 指向 {region_label(home)}，与出口地区 {region_label(loc)} 不一致"
        if region_status(home) == "unsupported":
            add("timezone", "warn", text,
                "出口在支持地区，但本机设置显示人可能在不支持地区。Anthropic 按使用者实际所在地判断，不按出口 IP；"
                "人确实在不支持地区的话，风险与出口无关。")
        else:
            add("timezone", "info", text, "出差或经过代理都会这样；人所在地在支持名单内就没问题。")
    lc = (env["locale"], env.get("locale_cc")) if env and env.get("locale") else system_locale()
    if lc and loc and lc[1] and lc[1] != loc:
        name, home = lc
        text = f"系统语言 / 区域 {name} 指向 {region_label(home)}，与出口地区 {region_label(loc)} 不一致"
        if region_status(home) == "unsupported":
            add("locale", "warn", text,
                "系统语言属于 Anthropic 会收集的设备环境信息。出口在支持地区但系统区域指向不支持地区，"
                "说明人可能在不支持地区；政策按实际所在地判断。")
        else:
            add("locale", "info", text, "用外语系统或出差都会这样；人所在地在支持名单内就没问题。")
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
            "Anthropic 账号仅限本人使用，多人共用同一账号违反使用条款。")
    return out


# 环境风险评分：满分 100，按 (kind, level) 扣分；没列出的 warn 扣 10、bad 扣 30。只是把提醒汇总成一个数，不拦任何操作。
RISK_WEIGHTS = {
    ("region", "warn"): 60, ("region_unknown", "warn"): 30, ("tls", "bad"): 0, ("account_regions", "warn"): 20, ("region_change", "warn"): 15, ("isp", "warn"): 20,
    ("region_mismatch", "warn"): 15, ("timezone", "warn"): 25, ("timezone", "info"): 5, ("locale", "warn"): 15,
    ("locale", "info"): 3, ("clock", "warn"): 10, ("warp", "info"): 5, ("vpn", "info"): 10, ("proxy", "info"): 5,
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
        add("isp", "info", f"网络服务商：{r['org']}（AS{r.get('asn')}），IP 类型：{IP_TYPE_NAMES[ip_type]}{src_note}",
            "数据库判定为普通宽带或运营商网络，不是机房 / 代理出口" if ip_type == "residential" else "手机网络的出口 IP 会变，地区判定也可能跳动")
    elif r.get("org") and ip_type is None and not hosting_exit(r["org"]):
        add("isp", "info", f"网络服务商：{r['org']}（AS{r.get('asn')}），IP 类型：数据源没给标记{src_note}",
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
    # 代理：环境变量 + 系统设置；VPN：默认路由
    parts = [f"{k}={v}" for k, v in proxies.items()] + [f"系统设置 {p}" for p in system_proxy()]
    add("proxy", "info" if parts else "ok", "代理：" + ("；".join(parts) if parts else "无（直连）"),
        "请求经过代理，上面的出口地区是代理的出口，不是本机网络的出口" if parts else "直连，出口地区就是本机网络的出口")
    tun = default_route_tunnel()
    if tun:
        add("vpn", "info", f"VPN：默认路由走 {tun[0]}（{tun[1]}）",
            "出口地区是 VPN 的出口，不代表本机所在地；Anthropic 按使用者实际所在地判断是否支持。")
    else:
        add("vpn", "ok", "VPN：默认路由没有经过隧道接口", "只看本机默认路由；透明代理和路由器上的 VPN 看不出来")
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
    for it in items:
        print(f"{marks[it['level']]} {it['text']}")
        if it["advice"]:
            print(color(f"    → {it['advice']}", "d"))
    if SSL_CA_FALLBACK:
        print(color(f"    · 本机 Python 没带根证书，已自动改用 {SSL_CA_FALLBACK}（证书照常校验）", "d"))


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
        elif c == "risk":
            cmd_risk(store, args)
        elif c == "json":
            print(json.dumps(public_state(store, store.load()), ensure_ascii=False, indent=2))
        return 0
    except CamError as e:
        print(color("错误：", "r") + str(e), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
