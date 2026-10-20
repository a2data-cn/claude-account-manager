"""Encrypted local account vault shared with the Rust backend; no dependency on cam."""
from __future__ import annotations

import contextlib
import datetime as dt
import hashlib
import json
import math
import os
import re
import secrets
import sqlite3
import threading


class VaultError(Exception):
    pass


_keys = {}
_locks = {}
_guard = threading.Lock()
_CONFIG = {"client_id", "tenant", "imap_host", "imap_port", "imap_username", "notes"}
_SECRET = {"password", "client_secret", "access_token", "refresh_token", "expires_at"}


def _root(store):
    return os.path.realpath(os.path.abspath(store.root))


def _mutex(store):
    with _guard:
        return _locks.setdefault(_root(store), threading.RLock())


def _now():
    return dt.datetime.now(dt.timezone.utc).isoformat()


def _aes(key):
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    except ImportError as e:
        raise VaultError("请先安装账号库依赖：python3 -m pip install -r scripts/requirements-vault.txt") from e
    return AESGCM(key)


def _encrypt(key, data, aad):
    nonce = secrets.token_bytes(12)
    return b"CAM1" + nonce + _aes(key).encrypt(nonce, data, aad)


def _decrypt(key, blob, aad):
    if not isinstance(blob, bytes) or len(blob) < 32 or blob[:4] != b"CAM1":
        raise VaultError("账号库密文格式不正确")
    try:
        return _aes(key).decrypt(blob[4:16], blob[16:], aad)
    except VaultError:
        raise
    except Exception as e:
        raise VaultError("主密码不正确，或账号库密文已损坏") from e


def _key(store):
    key = _keys.get(_root(store))
    if key is None:
        raise VaultError("账号库已锁定，请先解锁")
    return bytes(key)


@contextlib.contextmanager
def _entry_write_lock(store, entry_id):
    """与邮箱授权共用跨进程锁，避免授权期间编辑条目导致旧 token 归入新身份。"""
    if not isinstance(entry_id, str):
        raise VaultError("账号条目 ID 不正确")
    name = "mailbox-" + hashlib.sha256(entry_id.encode()).hexdigest() + ".lock"
    fd = os.open(os.path.join(_root(store), name), os.O_RDWR | os.O_CREAT, 0o600)
    try:
        try:
            if os.name == "nt":
                import msvcrt
                msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as e:
            raise VaultError("这个邮箱正在授权或读取，请稍后再修改条目") from e
        yield
    finally:
        if os.name == "nt":
            import msvcrt
            with contextlib.suppress(OSError):
                os.lseek(fd, 0, os.SEEK_SET)
                msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
        os.close(fd)


def _permissions(root):
    if os.name == "nt":
        return
    os.chmod(root, 0o700)
    for suffix in ("", "-wal", "-shm", "-journal"):
        path = os.path.join(root, "vault.sqlite3" + suffix)
        if os.path.exists(path):
            os.chmod(path, 0o600)


@contextlib.contextmanager
def _db(store, create=False):
    root = _root(store)
    path = os.path.join(root, "vault.sqlite3")
    if not create and not os.path.isfile(path):
        raise VaultError("账号库尚未初始化")
    connection = None
    try:
        if create:
            os.makedirs(root, mode=0o700, exist_ok=True)
        connection = sqlite3.connect(path, timeout=5)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA busy_timeout=5000")
        connection.execute("PRAGMA secure_delete=ON")
        _permissions(root)
        yield connection
    except sqlite3.Error as e:
        raise VaultError("本地账号库无法读写，请确认文件完整且没有被占用") from e
    finally:
        if connection is not None:
            connection.close()
            _permissions(root)


def _meta(connection):
    return {row["key"]: bytes(row["value"]) for row in connection.execute("SELECT key,value FROM vault_meta")}


def _read_secret(row, key):
    raw = _decrypt(key, bytes(row["secret"]), ("cam-vault-entry:" + row["id"]).encode())
    try:
        secret = json.loads(raw)
        if not isinstance(secret, dict):
            raise ValueError()
        return secret
    except (ValueError, UnicodeError) as e:
        raise VaultError("账号库秘密记录格式已损坏") from e


def _public(row, secret):
    return {"id": row["id"], "label": row["label"], "email": row["email"], "provider": row["provider"],
            "account_id": row["account_id"], "config": json.loads(row["config_json"]),
            "has_password": bool(secret.get("password")), "has_client_secret": bool(secret.get("client_secret")),
            "connected": (bool(secret.get("access_token") or secret.get("refresh_token")) if row["provider"] in ("gmail", "outlook")
                          else bool(secret.get("password")) if row["provider"] == "imap" else False),
            "created_at": row["created_at"], "updated_at": row["updated_at"]}


def status(store):
    with _mutex(store):
        if not os.path.isfile(os.path.join(_root(store), "vault.sqlite3")):
            return {"initialized": False, "unlocked": False, "entries": []}
        with _db(store) as db:
            if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='vault_meta'").fetchone():
                return {"initialized": False, "unlocked": False, "entries": []}
            meta = _meta(db)
            initialized = meta.get("version") == b"1" and len(meta.get("salt", b"")) == 32 and bool(meta.get("check"))
            key = _keys.get(_root(store))
            entries = [_public(row, _read_secret(row, bytes(key))) for row in db.execute("SELECT * FROM vault_entries ORDER BY created_at,id")] if key is not None and initialized else []
            return {"initialized": initialized, "unlocked": initialized and key is not None, "entries": entries}


def _password(body):
    password = body.get("master_password") if isinstance(body, dict) else None
    if not isinstance(password, str) or len(password) < 8:
        raise VaultError("主密码至少需要 8 个字符")
    return password


def setup(store, body):
    password = _password(body)
    with _mutex(store), _db(store, create=True) as db:
        db.execute("BEGIN IMMEDIATE")
        db.execute("CREATE TABLE IF NOT EXISTS vault_meta (key TEXT PRIMARY KEY, value BLOB NOT NULL)")
        db.execute("CREATE TABLE IF NOT EXISTS vault_entries (id TEXT PRIMARY KEY,label TEXT NOT NULL,email TEXT NOT NULL,provider TEXT NOT NULL,account_id TEXT,config_json TEXT NOT NULL,secret BLOB NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL)")
        if db.execute("SELECT 1 FROM vault_meta LIMIT 1").fetchone():
            raise VaultError("账号库已经初始化，不能覆盖")
        salt = secrets.token_bytes(32)
        key = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, 600000, 32)
        check = _encrypt(key, b"cam-vault-v1", b"cam-vault-check")
        db.executemany("INSERT INTO vault_meta(key,value) VALUES(?,?)", [("version", b"1"), ("salt", salt), ("check", check)])
        db.commit()
        _keys[_root(store)] = bytearray(key)
    return status(store)


def unlock(store, body):
    password = _password(body)
    with _mutex(store), _db(store) as db:
        meta = _meta(db)
        if meta.get("version") != b"1" or len(meta.get("salt", b"")) != 32:
            raise VaultError("账号库格式不正确或尚未初始化")
        key = hashlib.pbkdf2_hmac("sha256", password.encode(), meta["salt"], 600000, 32)
        if _decrypt(key, meta.get("check", b""), b"cam-vault-check") != b"cam-vault-v1":
            raise VaultError("主密码不正确，或账号库已损坏")
        _keys[_root(store)] = bytearray(key)
    return status(store)


def lock(store):
    with _mutex(store):
        previous = _keys.pop(_root(store), None)
        if previous is not None:
            previous[:] = bytes(len(previous))
    return status(store)


def _find(db, entry_id):
    if not isinstance(entry_id, str):
        raise VaultError("账号条目 ID 不正确")
    row = db.execute("SELECT * FROM vault_entries WHERE id=?", (entry_id,)).fetchone()
    if row is None:
        raise VaultError("账号库中没有这个条目")
    return row


def _email(value, required=False):
    if not isinstance(value, str) or len(value) > 254 or any(c in value for c in "\r\n\x00"):
        raise VaultError("邮箱格式不正确")
    value = value.strip()
    if (required or value) and not re.fullmatch(r"[^\s@]+@[^\s@]+\.[^\s@]+", value):
        raise VaultError("请输入完整邮箱地址")
    return value


def _config(value, previous):
    if not isinstance(value, dict) or set(value) - _CONFIG:
        raise VaultError("邮箱配置包含不支持的字段")
    result = dict(previous)
    for name, item in value.items():
        if name == "imap_port":
            if isinstance(item, bool) or not isinstance(item, int) or not 1 <= item <= 65535:
                raise VaultError("IMAP 端口应为 1–65535 的整数")
        elif not isinstance(item, str) or len(item) > 4096 or "\x00" in item or (name != "notes" and any(c in item for c in "\r\n")):
            raise VaultError("邮箱配置格式不正确")
        result[name] = item
    return result


def _write_secret(db, entry_id, secret, key):
    payload = json.dumps(secret, ensure_ascii=False, separators=(",", ":")).encode()
    blob = _encrypt(key, payload, ("cam-vault-entry:" + entry_id).encode())
    db.execute("UPDATE vault_entries SET secret=?,updated_at=? WHERE id=?", (blob, _now(), entry_id))


def save_entry(store, body):
    if not isinstance(body, dict):
        raise VaultError("请求体应为 JSON 对象")
    if body.get("id"):
        with _entry_write_lock(store, body["id"]):
            return _save_entry_locked(store, body)
    return _save_entry_locked(store, body)


def _save_entry_locked(store, body):
    if not isinstance(body, dict):
        raise VaultError("请求体应为 JSON 对象")
    with _mutex(store), _db(store) as db:
        key = _key(store)
        db.execute("BEGIN IMMEDIATE")
        row = _find(db, body["id"]) if body.get("id") else None
        provider = body.get("provider", row["provider"] if row else "credential")
        if provider not in ("credential", "gmail", "outlook", "imap"):
            raise VaultError("只支持普通账号、Gmail/Workspace、Outlook/Microsoft 365 或企业 IMAP")
        email = _email(body.get("email", row["email"] if row else ""), provider != "credential")
        label = body.get("label", row["label"] if row else email)
        account_id = body.get("account_id", row["account_id"] if row else None)
        if not isinstance(label, str) or len(label) > 200 or (account_id is not None and not isinstance(account_id, str)):
            raise VaultError("名称或关联账号格式不正确")
        old_config = json.loads(row["config_json"]) if row else {}
        config = _config(body.get("config", {}), old_config)
        secret = _read_secret(row, key) if row else {}
        if row and (provider != row["provider"] or email.casefold() != row["email"].casefold() or
                    any(config.get(k) != old_config.get(k) for k in ("client_id", "tenant")) or
                    (body.get("client_secret") and body["client_secret"] != secret.get("client_secret"))):
            for field in ("access_token", "refresh_token", "expires_at"):
                secret.pop(field, None)
        if "clear_password" in body and not isinstance(body["clear_password"], bool):
            raise VaultError("clear_password 应为 true 或 false")
        for field in ("password", "client_secret"):
            if field in body:
                if not isinstance(body[field], str):
                    raise VaultError("密码应为文字")
                if body[field]:
                    secret[field] = body[field]
        if body.get("clear_password"):
            secret.pop("password", None)
        entry_id = row["id"] if row else secrets.token_hex(16)
        now = _now()
        if row:
            db.execute("UPDATE vault_entries SET label=?,email=?,provider=?,account_id=?,config_json=?,updated_at=? WHERE id=?",
                       (label.strip() or email or "账号", email, provider, account_id, json.dumps(config, ensure_ascii=False), now, entry_id))
        else:
            db.execute("INSERT INTO vault_entries VALUES(?,?,?,?,?,?,?,?,?)", (entry_id, label.strip() or email or "账号", email,
                       provider, account_id, json.dumps(config, ensure_ascii=False), b"", now, now))
        _write_secret(db, entry_id, secret, key)
        db.commit()
    return status(store)


def delete_entry(store, entry_id):
    with _entry_write_lock(store, entry_id):
        return _delete_entry_locked(store, entry_id)


def _delete_entry_locked(store, entry_id):
    with _mutex(store), _db(store) as db:
        _key(store)
        _find(db, entry_id)
        db.execute("DELETE FROM vault_entries WHERE id=?", (entry_id,))
        db.commit()
    return status(store)


def get_entry(store, entry_id):
    with _mutex(store), _db(store) as db:
        key = _key(store)
        row = _find(db, entry_id)
        secret = _read_secret(row, key)
        return {**_public(row, secret), "secret": secret}


def reveal_entry(store, entry_id):
    return {"password": get_entry(store, entry_id)["secret"].get("password", "")}


def save_secret(store, entry_id, secret):
    if not isinstance(secret, dict) or set(secret) - _SECRET:
        raise VaultError("秘密字段不正确")
    with _mutex(store), _db(store) as db:
        key = _key(store)
        db.execute("BEGIN IMMEDIATE")
        previous = _read_secret(_find(db, entry_id), key)
        for field, value in secret.items():
            if field == "expires_at":
                if isinstance(value, bool) or not isinstance(value, (int, float)) or not 0 <= value <= 1e15 or not math.isfinite(value):
                    raise VaultError("授权有效期格式不正确")
            elif not isinstance(value, str):
                raise VaultError("秘密字段应为文字")
            previous[field] = value
        _write_secret(db, entry_id, previous, key)
        db.commit()


def set_email(store, entry_id, email):
    with _entry_write_lock(store, entry_id):
        return _set_email_locked(store, entry_id, email)


def _set_email_locked(store, entry_id, email):
    email = _email(email, True)
    with _mutex(store), _db(store) as db:
        _key(store)
        _find(db, entry_id)
        db.execute("UPDATE vault_entries SET email=?,updated_at=? WHERE id=?", (email, _now(), entry_id))
        db.commit()
