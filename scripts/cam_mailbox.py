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


def _root(store):
    return os.path.realpath(os.path.abspath(store.root))


def _now():
    return dt.datetime.now(dt.timezone.utc)


def _iso(value):
    return value.astimezone(dt.timezone.utc).isoformat()


def _entry(store, entry_id):
    try:
        return vault.get_entry(store, entry_id)
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


def _http_json(method, url, token=None, form=None, headers=None):
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
            raw = response.read(_MAX_HTTP + 1)
        if len(raw) > _MAX_HTTP:
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
    return any(host == domain or host.endswith("." + domain) for domain in ("anthropic.com", "claude.ai"))


def _summary(message_id, sender, subject, received, body, since):
    addresses = email.utils.getaddresses([sender])
    if len(addresses) != 1:
        return None
    address = addresses[0][1]
    if not re.fullmatch(r"[^@\s<>]+@[^@\s<>]+", address) or not _official_host(address.rsplit("@", 1)[1]):
        return None
    if received is None or received < since or received > _now() + dt.timedelta(minutes=5):
        return None
    original_body = html.unescape(body)
    body = html.unescape(re.sub(r"<[^>]{0,2000}>", " ", body))
    text = subject + "\n" + body
    if not re.search(r"verification|verify|sign[ -]?in|log[ -]?in|authentication|验证码|验证|登录", text, re.I):
        return None
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
    if not codes and not links:
        return None
    return {"id": str(message_id), "sender": address, "subject": subject[:500], "received_at": _iso(received),
            "codes": codes, "verification_links": links[:10]}


def _parse_time(value):
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        return parsed.replace(tzinfo=dt.timezone.utc) if parsed.tzinfo is None else parsed.astimezone(dt.timezone.utc)
    except (ValueError, TypeError, AttributeError):
        return None


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


def _gmail(entry, since):
    token = entry["secret"]["access_token"]
    query = urllib.parse.urlencode({"q": f"after:{int(since.timestamp())} {{from:anthropic.com from:claude.ai}}", "maxResults": 20})
    listing = _http_json("GET", "https://gmail.googleapis.com/gmail/v1/users/me/messages?" + query, token=token)
    found = []
    for item in listing.get("messages", [])[:20]:
        message_id = item.get("id")
        if not isinstance(message_id, str):
            continue
        record = _http_json("GET", "https://gmail.googleapis.com/gmail/v1/users/me/messages/" + urllib.parse.quote(message_id, safe="") + "?format=full", token=token)
        if record.get("sizeEstimate", 0) > _MAX_MESSAGE:
            continue
        payload = record.get("payload") or {}
        headers = {h.get("name", "").lower(): h.get("value", "") for h in payload.get("headers", [])}
        texts = []
        def walk(part, depth=0):
            if not isinstance(part, dict) or depth > 10:
                return
            if part.get("mimeType") in ("text/plain", "text/html") and not part.get("filename"):
                data = (part.get("body") or {}).get("data", "")
                if isinstance(data, str) and len(data) <= 2 * _MAX_MESSAGE:
                    try:
                        texts.append(base64.urlsafe_b64decode(data + "=" * (-len(data) % 4)).decode("utf-8", "replace"))
                    except ValueError:
                        pass
            for child in part.get("parts", [])[:50]:
                walk(child, depth + 1)
        walk(payload)
        text = "\n".join(texts)
        if len(text.encode("utf-8")) > _MAX_MESSAGE:
            continue
        try:
            received = dt.datetime.fromtimestamp(float(record.get("internalDate", 0)) / 1000, dt.timezone.utc)
        except (ValueError, TypeError, OverflowError, OSError):
            continue
        summary = _summary(message_id, headers.get("from", ""), headers.get("subject", ""), received, text, since)
        if summary:
            found.append(summary)
    return found


def _outlook(entry, since):
    query = urllib.parse.urlencode({"$top": 20, "$orderby": "receivedDateTime desc", "$filter": "receivedDateTime ge " + _iso(since),
                                   "$select": "id,from,subject,receivedDateTime,body"})
    listing = _http_json("GET", "https://graph.microsoft.com/v1.0/me/mailFolders/inbox/messages?" + query,
                         token=entry["secret"]["access_token"], headers={"Prefer": 'outlook.body-content-type="text"'})
    found = []
    for record in listing.get("value", [])[:20]:
        sender = ((record.get("from") or {}).get("emailAddress") or {}).get("address", "")
        content = (record.get("body") or {}).get("content") or ""
        if not isinstance(content, str) or len(content.encode("utf-8")) > _MAX_MESSAGE:
            continue
        summary = _summary(record.get("id", ""), sender, record.get("subject", ""), _parse_time(record.get("receivedDateTime")),
                           content, since)
        if summary:
            found.append(summary)
    return found


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


def _imap(entry, since):
    config = entry["config"]
    host = config.get("imap_host", "")
    if not isinstance(host, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", host):
        raise MailboxError("请填写企业 IMAP 服务器主机名；连接始终使用经过验证的 TLS")
    password = entry["secret"].get("password")
    if not password:
        raise MailboxError("请先保存企业 IMAP 密码或应用专用密码")
    connection = None
    found = []
    try:
        connection = _imap_connection(host, config.get("imap_port", 993), ssl.create_default_context())
        connection.login(config.get("imap_username") or entry["email"], password)
        if connection.select("INBOX", readonly=True)[0] != "OK":
            raise MailboxError("无法只读打开邮箱 INBOX")
        month = ("Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec")[since.month - 1]
        date = f"{since.day:02d}-{month}-{since.year}"
        result, rows = connection.uid("search", None, "SINCE", date, "OR", "FROM", '"anthropic.com"', "FROM", '"claude.ai"')
        if result != "OK":
            raise MailboxError("企业邮箱搜索失败")
        for message_id in (rows[0] or b"").split()[-20:][::-1]:
            result, metadata = connection.uid("fetch", message_id, "(RFC822.SIZE INTERNALDATE)")
            raw_meta = b" ".join(x for x in metadata if isinstance(x, bytes))
            size = re.search(rb"RFC822.SIZE (\d+)", raw_meta)
            internal = imaplib.Internaldate2tuple(raw_meta)
            if result != "OK" or not size or int(size.group(1)) > _MAX_MESSAGE or internal is None:
                continue
            received = dt.datetime.fromtimestamp(time.mktime(internal), dt.timezone.utc)
            if received < since:
                continue
            result, data = connection.uid("fetch", message_id, f"(BODY.PEEK[]<0.{_MAX_MESSAGE + 1}>)")
            if result != "OK":
                continue
            raw = next((x[1] for x in data if isinstance(x, tuple) and len(x) > 1 and isinstance(x[1], bytes)), b"")
            if not raw or len(raw) > _MAX_MESSAGE:
                continue
            parsed = email.message_from_bytes(raw, policy=email.policy.default)
            summary = _summary(message_id.decode("ascii", "replace"), str(parsed.get("From", "")), str(parsed.get("Subject", "")), received, _mime_text(parsed), since)
            if summary:
                found.append(summary)
    except (OSError, imaplib.IMAP4.error, ssl.SSLError) as e:
        raise MailboxError("企业 IMAP 连接或认证失败，请检查 TLS 主机、端口和应用专用密码") from e
    finally:
        if connection is not None:
            with contextlib.suppress(Exception):
                connection.logout()
    return found


def messages(store, entry_id, body):
    if not isinstance(body, dict):
        raise MailboxError("请求体应为 JSON 对象")
    now = _now()
    since = _parse_time(body.get("since")) if body.get("since") is not None else now - dt.timedelta(minutes=15)
    if since is None or since > now:
        raise MailboxError("since 应为过去的 ISO8601 时间")
    if since < now - dt.timedelta(hours=1):
        raise MailboxError("只能读取最近 1 小时内的登录邮件，请缩短查询时间范围")
    entry = _entry(store, entry_id)
    with _EntryLock(store, entry_id):
        if entry["provider"] in ("gmail", "outlook"):
            entry = _authorized_entry(store, entry_id)
            summaries = _gmail(entry, since) if entry["provider"] == "gmail" else _outlook(entry, since)
        elif entry["provider"] == "imap":
            summaries = _imap(_entry(store, entry_id), since)
        else:
            raise MailboxError("普通账号没有邮箱连接，请选择 Gmail、Outlook 或企业 IMAP")
    summaries.sort(key=lambda item: item["received_at"], reverse=True)
    return {"entry_id": entry_id, "checked_at": _iso(_now()), "status": "found" if summaries else "empty", "messages": summaries[:20]}
