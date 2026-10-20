"""Read only the identity cache belonging to a known Claude Desktop account."""
import datetime as dt
import io
import json
import os
import re
import struct
import urllib.parse


class IdentityError(Exception):
    pass


def _json_body(payload):
    limit = 1024 * 1024
    if payload.startswith(bytes.fromhex("28b52ffd")):
        try:
            try:
                from compression import zstd
            except ImportError:
                import zstandard
                with zstandard.ZstdDecompressor(max_window_size=1024).stream_reader(io.BytesIO(payload), read_across_frames=False) as reader:
                    payload = reader.read(limit + 1)
            else:
                decoder = zstd.ZstdDecompressor()
                payload = decoder.decompress(payload, max_length=limit + 1)
                if not decoder.eof:
                    return None
        except (ImportError, ValueError, Exception):
            return None
    if len(payload) > limit:
        return None
    try:
        value, _ = json.JSONDecoder().raw_decode(payload.decode("utf-8", "replace").lstrip())
        return value if isinstance(value, dict) else None
    except (ValueError, UnicodeError):
        return None


def read_desktop_identity(directory, expected_account=None):
    try:
        with open(os.path.join(directory, "config.json"), "rb") as handle:
            raw = handle.read(1024 * 1024 + 1)
        config = json.loads(raw) if len(raw) <= 1024 * 1024 else None
    except (OSError, ValueError):
        config = None
    owner = config.get("lastKnownAccountUuid") if isinstance(config, dict) else None
    if not isinstance(owner, str) or not owner:
        raise IdentityError("没有可核对的客户端身份，请先在这个配置里登录，或手动填写邮箱。")
    if expected_account and owner.lower() != expected_account.lower():
        raise IdentityError("这个配置已登录其它账号，请先确认账号与配置的对应关系。")
    newest = None
    cache = os.path.join(directory, "Cache", "Cache_Data")
    try:
        entries = os.scandir(cache)
    except OSError:
        entries = []
    try:
        for entry in entries:
            if not entry.name.endswith("_0") or not entry.is_file():
                continue
            try:
                with open(entry.path, "rb") as handle:
                    blob = handle.read(512 * 1024 + 1)
                if len(blob) > 512 * 1024 or len(blob) < 24 or blob[:8] != bytes.fromhex("305c72a71b6dfbfc"):
                    continue
                length = struct.unpack_from("<I", blob, 12)[0]
                if not 0 < length < min(8193, len(blob) - 24):
                    continue
                key = blob[24:24 + length].decode("utf-8")
                if key.startswith("1/0/"):
                    key = key[4:]
                if key != "https://claude.ai/api/account":
                    continue
                value = _json_body(blob[24 + length:])
                account = value.get("account", value) if isinstance(value, dict) else None
                if not isinstance(account, dict):
                    continue
                uuid = account.get("uuid") or account.get("account_uuid")
                email = account.get("email_address") or account.get("email")
                if not isinstance(uuid, str) or uuid.lower() != owner.lower():
                    continue
                if not isinstance(email, str) or not re.fullmatch(r"[^\s@]+@[^\s@]+\.[^\s@]+", email):
                    continue
                sampled = entry.stat().st_mtime
                if sampled > dt.datetime.now(dt.timezone.utc).timestamp() + 300:
                    continue
                if newest is None or sampled > newest[0]:
                    newest = (sampled, email)
            except (OSError, ValueError, UnicodeError, struct.error):
                continue
    finally:
        if hasattr(entries, "close"):
            entries.close()
    if newest is None:
        raise IdentityError("没有找到与当前客户端身份匹配的邮箱缓存。请在 Claude 中打开账号设置后重试，或手动填写邮箱。")
    return {"email": newest[1], "desktop_account_uuid": owner}


def allowed_external_url(value):
    if not isinstance(value, str) or len(value) > 16384 or any(ord(c) < 32 for c in value):
        return False
    try:
        parsed = urllib.parse.urlsplit(value)
        if parsed.scheme != "https" or parsed.username or parsed.password or parsed.port not in (None, 443):
            return False
        host = (parsed.hostname or "").lower()
        if any(host == domain or host.endswith("." + domain) for domain in ("claude.ai", "claude.com", "anthropic.com")):
            return True
        if host == "accounts.google.com":
            return parsed.path == "/o/oauth2/v2/auth"
        return host == "login.microsoftonline.com" and bool(re.fullmatch(r"/[A-Za-z0-9.-]+/oauth2/v2\.0/authorize", parsed.path))
    except ValueError:
        return False
