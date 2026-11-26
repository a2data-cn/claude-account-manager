"""Read-only mailbox OAuth, IMAP and Claude login-message summaries."""
from __future__ import annotations

import base64
import contextlib
import datetime as dt
import email
import email.policy
import email.utils
import hashlib
import html
import http.server
import imaplib
import json
import math
import os
import re
import secrets
import socket
import ssl
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

import cam_vault as vault


class MailboxError(Exception):
    pass


_jobs = {}
_jobs_guard = threading.RLock()
_MAX_MESSAGE = 256 * 1024
_MAX_HTTP = 2 * 1024 * 1024
_TTL = 300
_LOOKBACK = dt.timedelta(days=7)
_DETAIL_MAX = 2 * 1024 * 1024
_PROVIDERS_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), os.pardir, "web", "mail-providers.json")
_providers_cache = None


def _providers():
    global _providers_cache
    if _providers_cache is None:
        try:
            with open(_PROVIDERS_FILE, encoding="utf-8") as f:
                data = json.load(f)
            if not isinstance(data, dict) or not isinstance(data.get("providers"), list):
                raise ValueError()
        except (OSError, ValueError):
            data = {"providers": [], "unknown": {"provider": "imap", "note": ""}}
        _providers_cache = data
    return _providers_cache


def _prefix_match(domain, prefix):
    rest = domain[len(prefix):] if domain.startswith(prefix) else ""
    return 0 < len(rest) <= 6 and all(part.isascii() and part.isalpha() for part in rest.split("."))


def detect(address):
    """按邮箱后缀猜收信方式和服务器（web/mail-providers.json），和 Rust 版同一份规则。"""
    if not isinstance(address, str) or not re.fullmatch(r"[^@\s<>]+@[^@\s<>]+\.[^@\s<>]+", address.strip()):
        raise MailboxError("先填完整的邮箱地址")
    domain = address.strip().rsplit("@", 1)[1].lower().rstrip(".")
    data = _providers()
    found = next((item for item in data["providers"] if isinstance(item, dict) and
                  (domain in item.get("domains", ()) or any(_prefix_match(domain, p) for p in item.get("prefixes", ())))),
                 data.get("unknown") or {})
    return {"provider": found.get("provider", "imap"), "imap_host": found.get("imap_host", ""), "imap_port": int(found.get("imap_port", 993)),
            "name": found.get("name", ""), "note": found.get("note", ""), "oauth": found.get("oauth") or None}


def _root(store):
    return os.path.realpath(os.path.abspath(store.root))


def _now():
    return dt.datetime.now(dt.timezone.utc)


def _iso(value):
    return value.astimezone(dt.timezone.utc).isoformat()


def _entry(store, entry_id):
    try:
        entry = vault.get_entry(store, entry_id)
        if entry["provider"] in ("gmail", "outlook") and not (entry["config"].get("client_id") or "").strip():
            # 应用 ID 只要填一次：这个条目没填时，用同一种邮箱里填过的那个（按添加顺序取第一个）
            for other in vault.status(store)["entries"]:
                client_id = (other.get("config") or {}).get("client_id") or ""
                if other["id"] != entry_id and other["provider"] == entry["provider"] and client_id.strip():
                    entry["config"]["client_id"] = client_id.strip()
                    if entry["provider"] == "outlook" and not entry["config"].get("tenant"):
                        entry["config"]["tenant"] = other["config"].get("tenant") or "common"
                    break
        return entry
    except vault.VaultError as e:
        raise MailboxError(str(e)) from e


class _EntryLock:
    def __init__(self, store, entry_id):
        self.fd = None
        name = "mailbox-" + hashlib.sha256(entry_id.encode()).hexdigest() + ".lock"
        fd = os.open(os.path.join(_root(store), name), os.O_RDWR | os.O_CREAT, 0o600)
        try:
            if os.name == "nt":
                import msvcrt
                msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as e:
            os.close(fd)
            raise MailboxError("这个邮箱正在授权或读取，请稍后重试") from e
        self.fd = fd

    def release(self):
        if self.fd is None:
            return
        if os.name == "nt":
            import msvcrt
            os.lseek(self.fd, 0, os.SEEK_SET)
            msvcrt.locking(self.fd, msvcrt.LK_UNLCK, 1)
        os.close(self.fd)
        self.fd = None

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.release()


def _http_json(method, url, token=None, form=None, headers=None, limit=_MAX_HTTP):
    request_headers = {"Accept": "application/json", **(headers or {})}
    if token:
        request_headers["Authorization"] = "Bearer " + token
    data = None
    if form is not None:
        request_headers["Content-Type"] = "application/x-www-form-urlencoded"
        data = urllib.parse.urlencode(form).encode()
    request = urllib.request.Request(url, data=data, headers=request_headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=20, context=ssl.create_default_context()) as response:
            raw = response.read(limit + 1)
        if len(raw) > limit:
            raise MailboxError("邮箱返回的数据超过读取上限")
        return json.loads(raw)
    except urllib.error.HTTPError as e:
        if e.code in (400, 401):
            raise MailboxError("邮箱授权无效或已过期，请重新授权") from e
        if e.code == 403:
            raise MailboxError("邮箱权限不足，企业账号可能需要管理员批准只读邮件权限") from e
        raise MailboxError(f"邮箱服务暂时无法读取（HTTP {e.code}）") from e
    except (OSError, ValueError, urllib.error.URLError) as e:
        raise MailboxError("无法读取邮箱服务，请检查网络及邮箱授权") from e


def _oauth_config(entry):
    provider = entry["provider"]
    if provider not in ("gmail", "outlook"):
        raise MailboxError("只有 Gmail/Workspace 和 Outlook/Microsoft 365 使用浏览器授权")
    config = entry["config"]
    client_id = config.get("client_id", "").strip()
    if not client_id or any(c.isspace() for c in client_id):
        raise MailboxError("请先填写自己注册的桌面 OAuth 应用 Client ID")
    if provider == "gmail":
        return ("https://accounts.google.com/o/oauth2/v2/auth", "https://oauth2.googleapis.com/token",
                "https://www.googleapis.com/auth/gmail.readonly")
    tenant = config.get("tenant", "common") or "common"
    if not re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9.-]{0,251}[A-Za-z0-9])?", tenant) or ".." in tenant:
        raise MailboxError("Microsoft tenant 应为 common、租户 ID 或域名")
    base = "https://login.microsoftonline.com/" + tenant + "/oauth2/v2.0/"
    return base + "authorize", base + "token", "offline_access User.Read Mail.Read"


def _identity(entry, access_token):
    if entry["provider"] == "gmail":
        profile = _http_json("GET", "https://gmail.googleapis.com/gmail/v1/users/me/profile", token=access_token)
        addresses = [profile.get("emailAddress")] if isinstance(profile, dict) else []
    else:
        profile = _http_json("GET", "https://graph.microsoft.com/v1.0/me?$select=mail,userPrincipalName", token=access_token)
        addresses = [profile.get("mail"), profile.get("userPrincipalName")] if isinstance(profile, dict) else []
    for actual in addresses:
        if isinstance(actual, str) and actual and actual.casefold() == entry["email"].casefold():
            return actual
    raise MailboxError("授权邮箱与这个条目填写的邮箱不一致，未保存授权或读取邮件")


def _token_fields(reply, previous):
    if not isinstance(reply, dict) or not isinstance(reply.get("access_token"), str) or not reply["access_token"]:
        raise MailboxError("邮箱没有返回可用授权，请重新授权")
    try:
        lifetime = float(reply.get("expires_in", 3600))
        if not math.isfinite(lifetime) or lifetime < 0:
            raise ValueError()
        lifetime = min(lifetime, 86400 * 365)
    except (ValueError, TypeError):
        raise MailboxError("邮箱授权有效期格式不正确")
    result = {"access_token": reply["access_token"], "expires_at": time.time() + lifetime}
    refresh = reply.get("refresh_token") or previous.get("refresh_token")
    if refresh:
        result["refresh_token"] = refresh
    return result


def _fingerprint(entry):
    return (entry["provider"], entry["email"].casefold(), entry["config"].get("client_id"),
            entry["config"].get("tenant"), entry["secret"].get("client_secret"))


def _finish(job, status, message):
    with _jobs_guard:
        if job["status"] != "pending":
            return
        job["status"], job["message"] = status, message
        job["done"].set()


def _complete_oauth(job, code):
    store, entry_id = job["store"], job["entry_id"]
    current = _entry(store, entry_id)
    if _fingerprint(current) != job["fingerprint"]:
        raise MailboxError("账号配置已更改，请重新开始授权")
    _, token_url, _ = _oauth_config(current)
    form = {"client_id": current["config"]["client_id"], "grant_type": "authorization_code", "code": code,
            "redirect_uri": job["redirect_uri"], "code_verifier": job["verifier"]}
    if current["secret"].get("client_secret"):
        form["client_secret"] = current["secret"]["client_secret"]
    reply = _http_json("POST", token_url, form=form)
    fields = _token_fields(reply, current["secret"])
    _identity(current, fields["access_token"])
    # 用户可能在网络请求期间锁库、删除条目或更改邮箱，写入前再次核对。
    try:
        with _jobs_guard, vault._mutex(store):
            if job["status"] != "pending" or time.time() >= job["expires_at"]:
                raise MailboxError("授权已超时，请重新开始")
            if _fingerprint(_entry(store, entry_id)) != job["fingerprint"]:
                raise MailboxError("账号配置已更改，请重新开始授权")
            vault.save_secret(store, entry_id, fields)
    except vault.VaultError as e:
        raise MailboxError(str(e)) from e


def connect(store, entry_id):
    entry = _entry(store, entry_id)
    authorization, _, scope = _oauth_config(entry)
    entry_lock = _EntryLock(store, entry_id)
    job = {"store": store, "root": _root(store), "entry_id": entry_id, "status": "pending", "message": "等待浏览器授权",
           "expires_at": time.time() + _TTL, "state": secrets.token_urlsafe(32), "verifier": secrets.token_urlsafe(64),
           "fingerprint": _fingerprint(entry), "done": threading.Event(), "closed": threading.Event()}
    class Callback(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, status, message):
            raw = message.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(raw)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            parsed = urllib.parse.urlparse(self.path)
            if parsed.path != "/oauth/callback":
                return self.reply(404, "Not found")
            query = urllib.parse.parse_qs(parsed.query)
            state = (query.get("state") or [""])[0]
            if not secrets.compare_digest(state, job["state"]):
                return self.reply(400, "授权 state 不匹配，请返回工具重新授权。")
            if job["status"] != "pending" or time.time() >= job["expires_at"]:
                _finish(job, "error", "授权已超时，请重新开始")
                return self.reply(400, "授权已结束，请返回工具。")
            if query.get("error"):
                _finish(job, "error", "邮箱授权未完成；如为企业账号，请确认管理员允许所需的只读权限")
                return self.reply(400, "授权未完成，请返回工具。")
            code = (query.get("code") or [""])[0]
            if not code:
                return self.reply(400, "缺少授权 code，请重新授权。")
            try:
                _complete_oauth(job, code)
                _finish(job, "connected", "邮箱已连接，只会读取登录验证码和验证链接摘要")
                self.reply(200, "邮箱已连接，可以关闭此页面并返回工具。")
            except Exception as e:
                message = str(e) if isinstance(e, (MailboxError, vault.VaultError)) else "邮箱授权失败，请重新开始"
                _finish(job, "error", message)
                self.reply(400, "邮箱未连接，请返回工具查看原因。")
    try:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Callback)
        server.daemon_threads = True
    except OSError as e:
        entry_lock.release()
        raise MailboxError("无法创建本机 OAuth 回调端口") from e
    redirect_host = "localhost" if entry["provider"] == "outlook" else "127.0.0.1"
    job["redirect_uri"] = f"http://{redirect_host}:{server.server_address[1]}/oauth/callback"
    job_id = secrets.token_hex(16)
    with _jobs_guard:
        _jobs[job_id] = job
    threading.Thread(target=server.serve_forever, daemon=True).start()
    def cleanup():
        job["done"].wait(_TTL)
        _finish(job, "error", "授权已超时，请重新开始")
        server.shutdown()
        server.server_close()
        job.pop("verifier", None)
        job.pop("fingerprint", None)
        entry_lock.release()
        job["closed"].set()
    threading.Thread(target=cleanup, daemon=True).start()
    challenge = base64.urlsafe_b64encode(hashlib.sha256(job["verifier"].encode()).digest()).decode().rstrip("=")
    query = {"client_id": entry["config"]["client_id"], "response_type": "code", "redirect_uri": job["redirect_uri"],
             "scope": scope, "state": job["state"], "code_challenge": challenge, "code_challenge_method": "S256"}
    if entry["provider"] == "gmail":
        query.update(access_type="offline", prompt="consent")
    return {"job_id": job_id, "authorization_url": authorization + "?" + urllib.parse.urlencode(query), "expires_at": job["expires_at"]}


def oauth_status(store, job_id):
    with _jobs_guard:
        job = _jobs.get(job_id)
        if job is None or job["root"] != _root(store):
            raise MailboxError("没有这个本机授权任务，请重新开始授权")
        if time.time() >= job["expires_at"]:
            _finish(job, "error", "授权已超时，请重新开始")
        return {"status": job["status"], "message": job["message"], "entry_id": job["entry_id"]}


def _authorized_entry(store, entry_id):
    entry = _entry(store, entry_id)
    secret = entry["secret"]
    token = secret.get("access_token")
    if not token or secret.get("expires_at", 0) <= time.time() + 60:
        refresh = secret.get("refresh_token")
        if not refresh:
            raise MailboxError("邮箱尚未授权或授权已过期，请先连接邮箱")
        _, token_url, scope = _oauth_config(entry)
        form = {"client_id": entry["config"]["client_id"], "grant_type": "refresh_token", "refresh_token": refresh}
        if entry["provider"] == "outlook":
            form["scope"] = scope
        if secret.get("client_secret"):
            form["client_secret"] = secret["client_secret"]
        fields = _token_fields(_http_json("POST", token_url, form=form), secret)
        _identity(entry, fields["access_token"])
        try:
            with vault._mutex(store):
                if _fingerprint(_entry(store, entry_id)) != _fingerprint(entry):
                    raise MailboxError("账号配置已更改，请重试")
                vault.save_secret(store, entry_id, fields)
        except vault.VaultError as e:
            raise MailboxError(str(e)) from e
        entry["secret"].update(fields)
    else:
        _identity(entry, token)
    return entry


def _official_host(host):
    host = (host or "").lower().rstrip(".")
    return any(host == domain or host.endswith("." + domain) for domain in ("anthropic.com", "claude.ai", "claude.com"))  # 和打开链接的白名单一致


_LOGIN_WORDS = re.compile(r"verification|verify|sign[ -]?in|log[ -]?in|authentication|验证码|验证|登录", re.I)
_OFFICIAL_QUERY = "{from:anthropic.com from:claude.ai from:claude.com}"
_GMAIL = "https://gmail.googleapis.com/gmail/v1/users/me/messages"
_GRAPH = "https://graph.microsoft.com/v1.0/me"


def _sender(sender):
    """返回 (地址, 显示名)；发件人不是一个合法地址时地址为 None。"""
    try:
        addresses = email.utils.getaddresses([sender or ""])
    except (TypeError, ValueError):
        return None, ""
    if len(addresses) != 1:
        return None, ""
    name, address = addresses[0]
    return (address if re.fullmatch(r"[^@\s<>]+@[^@\s<>]+", address) else None), name


def _strip_html(body):
    body = re.sub(r"(?is)<(script|style|head)\b[^>]{0,2000}>.*?</\1\s*>", " ", body or "")
    return html.unescape(re.sub(r"<[^>]{0,2000}>", " ", body))


def _preview(text):
    return re.sub(r"\s+", " ", text or "").strip()[:160]


def _login_bits(subject, body):
    """官方登录邮件里的验证码和验证链接；不是登录邮件时都是空的。"""
    original_body = html.unescape(body)
    text = subject + "\n" + html.unescape(re.sub(r"<[^>]{0,2000}>", " ", body))
    if not _LOGIN_WORDS.search(text):
        return [], []
    code_text = re.sub(r"https?://[^\s<>\"']+", " ", text, flags=re.I)
    codes = list(dict.fromkeys(re.findall(r"(?<![A-Za-z0-9])([0-9]{6,8})(?![A-Za-z0-9])", code_text)))[:10]
    links = []
    # 同时从原 HTML 与解码后正文提取；永远不访问这些 URL。
    for value in re.findall(r"https://[^\s<>\"']+", original_body):
        value = value.rstrip(".,;)")
        if len(value) > 8192:
            continue
        try:
            parsed = urllib.parse.urlparse(value)
            port = parsed.port
        except ValueError:
            continue
        if (parsed.scheme == "https" and not parsed.username and not parsed.password and port in (None, 443)
                and _official_host(parsed.hostname) and re.search(r"verify|verification|magic|login|auth|token|code", parsed.path + "?" + parsed.query, re.I)):
            if value not in links:
                links.append(value)
    return codes, links[:10]


def _item(message_id, sender, subject, received, body, since, everything, preview=None):
    """列表里的一封信。everything=False 时只要 Claude / Anthropic 官方发的。"""
    address, name = _sender(sender)
    official = bool(address) and _official_host(address.rsplit("@", 1)[1])
    if not official and not everything:
        return None
    if received is None or received < since or received > _now() + dt.timedelta(minutes=5):
        return None
    subject = subject if isinstance(subject, str) else ""
    codes, links = _login_bits(subject, body) if official and body else ([], [])
    return {"id": str(message_id), "sender": address or str(sender or "")[:200], "sender_name": (name or "")[:200], "subject": subject[:500],
            "received_at": _iso(received), "preview": _preview(preview if preview is not None else _strip_html(body)),
            "official": official, "login": bool(codes or links), "codes": codes, "verification_links": links}


def _safe_html(value):
    """详情页的原邮件 HTML：去掉脚本、跳转和外部引用的标签。界面还会把它放进不许运行脚本的沙盒里显示。"""
    value = re.sub(r"(?is)<script\b.*?(?:</script\s*>|$)", "", value or "")
    value = re.sub(r"(?is)</?(?:meta|base|link|iframe|frame|frameset|object|embed|applet|form)\b[^>]*>", "", value)
    return value[:_DETAIL_MAX]


def _detail(message_id, sender, subject, received, text, html_body, truncated=False):
    address, name = _sender(sender)
    official = bool(address) and _official_host(address.rsplit("@", 1)[1])
    subject = subject if isinstance(subject, str) else ""
    codes, links = _login_bits(subject, "\n".join(x for x in (text, html_body) if x)) if official else ([], [])
    return {"id": str(message_id), "sender": address or str(sender or "")[:200], "sender_name": (name or "")[:200], "subject": subject[:500],
            "received_at": _iso(received) if received else None, "official": official, "login": bool(codes or links),
            "codes": codes, "verification_links": links, "html": _safe_html(html_body),
            "text": (text or _strip_html(html_body or ""))[:200000], "truncated": bool(truncated)}


def _parse_time(value):
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        return parsed.replace(tzinfo=dt.timezone.utc) if parsed.tzinfo is None else parsed.astimezone(dt.timezone.utc)
    except (ValueError, TypeError, AttributeError):
        return None


def _mime_parts(message, limit=_MAX_MESSAGE):
    """按顺序取出正文里的纯文本和 HTML 两部分（附件不要）。"""
    plain, rich = [], []
    for part in (message.walk() if message.is_multipart() else [message]):
        kind = part.get_content_type()
        if kind not in ("text/plain", "text/html") or part.get_content_disposition() == "attachment":
            continue
        raw = part.get_payload(decode=True) or b""
        try:
            decoded = raw[:limit].decode(part.get_content_charset() or "utf-8", "replace")
        except LookupError:
            decoded = raw[:limit].decode("utf-8", "replace")
        (rich if kind == "text/html" else plain).append(decoded)
    return "\n".join(plain)[:limit], "\n".join(rich)[:limit]


def _mime_text(message):
    parts = message.walk() if message.is_multipart() else [message]
    text = []
    for part in parts:
        if part.get_content_type() not in ("text/plain", "text/html") or part.get_content_disposition() == "attachment":
            continue
        raw = part.get_payload(decode=True) or b""
        try:
            text.append(raw[:_MAX_MESSAGE].decode(part.get_content_charset() or "utf-8", "replace"))
        except LookupError:
            text.append(raw[:_MAX_MESSAGE].decode("utf-8", "replace"))
    return "\n".join(text)[:_MAX_MESSAGE]


def _gmail_parts(payload, limit=_MAX_MESSAGE):
    """Gmail 正文：按顺序返回 [(类型, 文字)]。"""
    out = []
    def walk(part, depth=0):
        if not isinstance(part, dict) or depth > 10:
            return
        if part.get("mimeType") in ("text/plain", "text/html") and not part.get("filename"):
            data = (part.get("body") or {}).get("data", "")
            if isinstance(data, str) and len(data) <= 2 * limit:
                try:
                    out.append((part["mimeType"], base64.urlsafe_b64decode(data + "=" * (-len(data) % 4)).decode("utf-8", "replace")))
                except ValueError:
                    pass
        for child in part.get("parts", [])[:50]:
            walk(child, depth + 1)
    walk(payload)
    return out


def _gmail_head(record):
    payload = record.get("payload") if isinstance(record, dict) else None
    headers = {h.get("name", "").lower(): h.get("value", "") for h in (payload or {}).get("headers", []) if isinstance(h, dict)}
    try:
        received = dt.datetime.fromtimestamp(float(record.get("internalDate", 0)) / 1000, dt.timezone.utc)
    except (ValueError, TypeError, OverflowError, OSError):
        received = None
    snippet = record.get("snippet")
    return headers, received, html.unescape(snippet) if isinstance(snippet, str) else ""


def _gmail_id(value):
    return value if isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,200}", value) else None


def _gmail(entry, since, everything=False):
    token = entry["secret"]["access_token"]
    query = urllib.parse.urlencode({"q": f"after:{int(since.timestamp())} " + ("in:inbox" if everything else _OFFICIAL_QUERY), "maxResults": 20})
    listing = _http_json("GET", _GMAIL + "?" + query, token=token)
    found = []
    for item in listing.get("messages", [])[:20]:
        message_id = _gmail_id(item.get("id") if isinstance(item, dict) else None)
        if not message_id:
            continue
        url = _GMAIL + "/" + urllib.parse.quote(message_id, safe="")
        if everything:  # 全部邮件：先只拿标题和摘要，官方邮件再取正文找验证码
            record = _http_json("GET", url + "?format=metadata&metadataHeaders=From&metadataHeaders=Subject", token=token)
            address = _sender(_gmail_head(record)[0].get("from", ""))[0]
            if address and _official_host(address.rsplit("@", 1)[1]) and record.get("sizeEstimate", 0) <= _MAX_MESSAGE:
                record = _http_json("GET", url + "?format=full", token=token)
        else:
            record = _http_json("GET", url + "?format=full", token=token)
            if record.get("sizeEstimate", 0) > _MAX_MESSAGE:
                continue
        headers, received, snippet = _gmail_head(record)
        text = "\n".join(value for _, value in _gmail_parts(record.get("payload") or {}))
        if len(text.encode("utf-8")) > _MAX_MESSAGE:
            text = ""
        summary = _item(message_id, headers.get("from", ""), headers.get("subject", ""), received, text, since, everything,
                        preview=snippet or None)
        if summary:
            found.append(summary)
    return found


def _gmail_detail(entry, message_id):
    record = _http_json("GET", _GMAIL + "/" + urllib.parse.quote(message_id, safe="") + "?format=full",
                        token=entry["secret"]["access_token"], limit=4 * _DETAIL_MAX)
    headers, received, snippet = _gmail_head(record)
    parts = _gmail_parts(record.get("payload") or {}, _DETAIL_MAX)
    plain = "\n".join(v for k, v in parts if k == "text/plain")
    rich = "\n".join(v for k, v in parts if k == "text/html")
    return _detail(message_id, headers.get("from", ""), headers.get("subject", ""), received, plain or (snippet if not rich else ""), rich,
                   truncated=record.get("sizeEstimate", 0) > _DETAIL_MAX)


def _outlook(entry, since, everything=False):
    query = urllib.parse.urlencode({"$top": 20, "$orderby": "receivedDateTime desc", "$filter": "receivedDateTime ge " + _iso(since),
                                   "$select": "id,from,subject,receivedDateTime,body,bodyPreview"})
    listing = _http_json("GET", _GRAPH + "/mailFolders/inbox/messages?" + query,
                         token=entry["secret"]["access_token"], headers={"Prefer": 'outlook.body-content-type="text"'})
    found = []
    for record in listing.get("value", [])[:20]:
        if not isinstance(record, dict):
            continue
        address = ((record.get("from") or {}).get("emailAddress") or {})
        sender = email.utils.formataddr((address.get("name") or "", address.get("address") or "")) if address.get("address") else ""
        content = (record.get("body") or {}).get("content") or ""
        if not isinstance(content, str) or len(content.encode("utf-8")) > _MAX_MESSAGE:
            content = ""
        preview = record.get("bodyPreview")
        summary = _item(record.get("id", ""), sender, record.get("subject", ""), _parse_time(record.get("receivedDateTime")),
                        content, since, everything, preview=preview if isinstance(preview, str) else None)
        if summary:
            found.append(summary)
    return found


def _outlook_detail(entry, message_id):
    record = _http_json("GET", _GRAPH + "/messages/" + urllib.parse.quote(message_id, safe="") + "?$select=id,from,subject,receivedDateTime,body",
                        token=entry["secret"]["access_token"], limit=4 * _DETAIL_MAX)
    address = ((record.get("from") or {}).get("emailAddress") or {})
    sender = email.utils.formataddr((address.get("name") or "", address.get("address") or "")) if address.get("address") else ""
    body = record.get("body") or {}
    content = body.get("content") if isinstance(body.get("content"), str) else ""
    rich = (body.get("contentType") or "").lower() == "html"
    return _detail(message_id, sender, record.get("subject", ""), _parse_time(record.get("receivedDateTime")),
                   "" if rich else content[:_DETAIL_MAX], content[:_DETAIL_MAX] if rich else "", truncated=len(content) > _DETAIL_MAX)


def _imap_connection(host, port, context):
    if sys.version_info >= (3, 9):
        return imaplib.IMAP4_SSL(host, port, ssl_context=context, timeout=20)
    # Python 3.8 has no constructor timeout; give both TCP and TLS handshakes a deadline.
    class TimedIMAP(imaplib.IMAP4_SSL):
        def _create_socket(self):
            sock = socket.create_connection((self.host, self.port), timeout=20)
            try:
                return self.ssl_context.wrap_socket(sock, server_hostname=self.host)
            except Exception:
                sock.close()
                raise
    return TimedIMAP(host, port, ssl_context=context)


def _imap_open(entry):
    """按条目里的服务器登录 IMAP，只读打开收件箱。返回 (连接, 服务器)。"""
    config = entry["config"]
    host = config.get("imap_host", "")
    if not isinstance(host, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", host):
        raise MailboxError("请填写 IMAP 收信服务器；连接始终使用经过验证的 TLS")
    password = entry["secret"].get("password")
    if not password:
        raise MailboxError("请先保存邮箱密码（有的邮箱要填授权码或应用专用密码）")
    connection = _imap_connection(host, config.get("imap_port", 993), ssl.create_default_context())
    try:
        connection.login(config.get("imap_username") or entry["email"], password)
        if connection.select("INBOX", readonly=True)[0] != "OK":
            raise MailboxError("无法只读打开邮箱 INBOX")
    except BaseException:
        with contextlib.suppress(Exception):
            connection.logout()
        raise
    return connection, host


def _imap_error(error, host):
    text = str(error).lower()
    if host.lower().endswith(("office365.com", "outlook.com")) and ("basicauthblocked" in text or "authenticate failed" in text or "login failed" in text):
        return MailboxError("微软不让这个号用密码收信：把收信方式改成“Outlook 授权”，再点“连接邮箱并授权”在微软页面登录一次")
    return MailboxError("邮箱连接或登录失败：检查服务器、端口和密码（有的邮箱要填授权码或应用专用密码，并先在网页版设置里打开 IMAP）")


def _imap_literal(data):
    """FETCH 结果里的元数据和第一段内容。"""
    meta, body = [], b""
    for item in data or []:
        if isinstance(item, tuple):
            meta.append(item[0] if isinstance(item[0], bytes) else b"")
            if not body and len(item) > 1 and isinstance(item[1], bytes):
                body = item[1]
        elif isinstance(item, bytes):
            meta.append(item)
    return b" ".join(meta), body


def _imap(entry, since, everything=False):
    connection, host = None, entry["config"].get("imap_host", "")
    found = []
    try:
        connection, host = _imap_open(entry)
        month = ("Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec")[since.month - 1]
        criteria = ["SINCE", f"{since.day:02d}-{month}-{since.year}"]
        if not everything:
            criteria += ["OR", "FROM", '"anthropic.com"', "OR", "FROM", '"claude.ai"', "FROM", '"claude.com"']
        result, rows = connection.uid("search", None, *criteria)
        if result != "OK":
            raise MailboxError("邮箱搜索失败")
        for message_id in (rows[0] or b"").split()[-20:][::-1]:
            result, data = connection.uid("fetch", message_id, "(RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)])")
            raw_meta, raw_head = _imap_literal(data)
            size = re.search(rb"RFC822.SIZE (\d+)", raw_meta)
            internal = imaplib.Internaldate2tuple(raw_meta)
            if result != "OK" or not size or internal is None:
                continue
            received = dt.datetime.fromtimestamp(time.mktime(internal), dt.timezone.utc)
            if received < since:
                continue
            try:
                head = email.message_from_bytes(raw_head, policy=email.policy.default)
                sender, subject = str(head.get("From", "")), str(head.get("Subject", ""))
            except Exception:
                sender, subject = "", ""
            uid = message_id.decode("ascii", "replace")
            summary = _item(uid, sender, subject, received, "", since, everything)
            if summary is None:
                continue
            if summary["official"] and int(size.group(1)) <= _MAX_MESSAGE:
                result, data = connection.uid("fetch", message_id, f"(BODY.PEEK[]<0.{_MAX_MESSAGE + 1}>)")
                raw = _imap_literal(data)[1] if result == "OK" else b""
                if raw and len(raw) <= _MAX_MESSAGE:
                    parsed = email.message_from_bytes(raw, policy=email.policy.default)
                    summary = _item(uid, str(parsed.get("From", "")), str(parsed.get("Subject", "")), received, _mime_text(parsed), since, everything) or summary
            found.append(summary)
    except (OSError, imaplib.IMAP4.error, ssl.SSLError) as e:
        raise _imap_error(e, host) from e
    finally:
        if connection is not None:
            with contextlib.suppress(Exception):
                connection.logout()
    return found


def _imap_detail(entry, message_id):
    if not message_id.isdigit():
        raise MailboxError("邮件编号不正确")
    connection, host = None, entry["config"].get("imap_host", "")
    try:
        connection, host = _imap_open(entry)
        uid = message_id.encode("ascii")
        result, data = connection.uid("fetch", uid, "(RFC822.SIZE INTERNALDATE)")
        raw_meta = _imap_literal(data)[0]
        size = re.search(rb"RFC822.SIZE (\d+)", raw_meta)
        internal = imaplib.Internaldate2tuple(raw_meta)
        if result != "OK" or not size:
            raise MailboxError("这封邮件已经不在收件箱里了")
        result, data = connection.uid("fetch", uid, f"(BODY.PEEK[]<0.{_DETAIL_MAX}>)")
        raw = _imap_literal(data)[1] if result == "OK" else b""
        if not raw:
            raise MailboxError("这封邮件已经不在收件箱里了")
        parsed = email.message_from_bytes(raw, policy=email.policy.default)
        plain, rich = _mime_parts(parsed, _DETAIL_MAX)
        received = dt.datetime.fromtimestamp(time.mktime(internal), dt.timezone.utc) if internal else None
        return _detail(message_id, str(parsed.get("From", "")), str(parsed.get("Subject", "")), received, plain, rich,
                       truncated=int(size.group(1)) > _DETAIL_MAX)
    except (OSError, imaplib.IMAP4.error, ssl.SSLError) as e:
        raise _imap_error(e, host) from e
    finally:
        if connection is not None:
            with contextlib.suppress(Exception):
                connection.logout()


def messages(store, entry_id, body):
    if not isinstance(body, dict):
        raise MailboxError("请求体应为 JSON 对象")
    everything = body.get("all", False)
    if not isinstance(everything, bool):
        raise MailboxError("all 应为 true 或 false")
    now = _now()
    since = _parse_time(body.get("since")) if body.get("since") is not None else now - dt.timedelta(minutes=15)
    if since is None or since > now:
        raise MailboxError("since 应为过去的 ISO8601 时间")
    if since < now - _LOOKBACK - dt.timedelta(minutes=1):
        raise MailboxError("只能读取最近 7 天的邮件，请缩短查询时间范围")
    entry = _entry(store, entry_id)
    with _EntryLock(store, entry_id):
        if entry["provider"] in ("gmail", "outlook"):
            entry = _authorized_entry(store, entry_id)
            summaries = (_gmail if entry["provider"] == "gmail" else _outlook)(entry, since, everything)
        elif entry["provider"] == "imap":
            summaries = _imap(_entry(store, entry_id), since, everything)
        else:
            raise MailboxError("普通账号没有邮箱连接，请选择收信方式")
    summaries.sort(key=lambda item: item["received_at"], reverse=True)
    return {"entry_id": entry_id, "checked_at": _iso(_now()), "all": everything, "status": "found" if summaries else "empty", "messages": summaries[:20]}


def message(store, entry_id, body):
    """点开一封信：原邮件的 HTML（去掉脚本）和纯文本。只读，不标记已读。"""
    if not isinstance(body, dict):
        raise MailboxError("请求体应为 JSON 对象")
    message_id = body.get("id")
    if not isinstance(message_id, str) or not re.fullmatch(r"[A-Za-z0-9_=+/-]{1,1024}", message_id):
        raise MailboxError("邮件编号不正确")
    entry = _entry(store, entry_id)
    with _EntryLock(store, entry_id):
        if entry["provider"] in ("gmail", "outlook"):
            entry = _authorized_entry(store, entry_id)
            if entry["provider"] == "gmail":
                if not _gmail_id(message_id):
                    raise MailboxError("邮件编号不正确")
                return _gmail_detail(entry, message_id)
            return _outlook_detail(entry, message_id)
        if entry["provider"] == "imap":
            return _imap_detail(_entry(store, entry_id), message_id)
    raise MailboxError("普通账号没有邮箱连接，请选择收信方式")
