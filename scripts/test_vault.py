"""Isolated vault, OAuth and read-only mailbox regression tests; no real accounts."""
import base64
import contextlib
import datetime as dt
import hashlib
import importlib.util
import io
import json
import os
import sqlite3
import sys
import tempfile
import time
import types
import unittest
import urllib.error
import urllib.parse
import urllib.request
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
import cam_vault as vault
import cam_mailbox as mailbox


class VaultTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = types.SimpleNamespace(root=self.tmp.name)
        vault.setup(self.store, {"master_password": "test master password"})

    def tearDown(self):
        for job in list(mailbox._jobs.values()):
            if job["root"] == mailbox._root(self.store):
                mailbox._finish(job, "error", "test cleanup")
                job["closed"].wait(2)
        vault.lock(self.store)
        self.tmp.cleanup()

    def add(self, provider="credential", **fields):
        state = vault.save_entry(self.store, {"label": "test", "email": "person@example.com", "provider": provider,
                                             "config": {"client_id": "test-client"} if provider in ("gmail", "outlook") else {}, **fields})
        return state["entries"][-1]["id"]

    def test_status_does_not_create_an_account_database(self):
        other = types.SimpleNamespace(root=os.path.join(self.tmp.name, "missing"))
        self.assertEqual(vault.status(other), {"initialized": False, "unlocked": False, "entries": []})
        self.assertFalse(os.path.exists(other.root))

    def test_encrypted_crud_reveal_and_locked_state(self):
        entry_id = self.add(password="private-password-8273", client_secret="private-client-secret")
        state = vault.status(self.store)
        self.assertTrue(state["entries"][0]["has_password"])
        self.assertNotIn("private-password", json.dumps(state))
        self.assertEqual(vault.reveal_entry(self.store, entry_id), {"password": "private-password-8273"})
        with open(os.path.join(self.tmp.name, "vault.sqlite3"), "rb") as f:
            raw = f.read()
        self.assertNotIn(b"private-password-8273", raw)
        self.assertNotIn(b"private-client-secret", raw)
        if os.name != "nt":
            self.assertEqual(os.stat(self.tmp.name).st_mode & 0o777, 0o700)
            self.assertEqual(os.stat(os.path.join(self.tmp.name, "vault.sqlite3")).st_mode & 0o777, 0o600)
        self.assertEqual(vault.lock(self.store)["entries"], [])
        with self.assertRaisesRegex(vault.VaultError, "锁定"):
            vault.reveal_entry(self.store, entry_id)
        vault.unlock(self.store, {"master_password": "test master password"})
        self.assertEqual(vault.delete_entry(self.store, entry_id)["entries"], [])

    def test_master_password_and_tampered_ciphertext_fail(self):
        entry_id = self.add(password="example")
        with self.assertRaises(vault.VaultError):
            vault.setup(self.store, {"master_password": "replacement password"})
        vault.lock(self.store)
        with self.assertRaises(vault.VaultError):
            vault.unlock(self.store, {"master_password": "wrong password"})
        self.assertFalse(vault.status(self.store)["unlocked"])
        vault.unlock(self.store, {"master_password": "test master password"})
        with contextlib.closing(sqlite3.connect(os.path.join(self.tmp.name, "vault.sqlite3"))) as db, db:
            blob = bytearray(db.execute("SELECT secret FROM vault_entries WHERE id=?", (entry_id,)).fetchone()[0])
            blob[-1] ^= 1
            db.execute("UPDATE vault_entries SET secret=? WHERE id=?", (bytes(blob), entry_id))
        with self.assertRaisesRegex(vault.VaultError, "损坏"):
            vault.reveal_entry(self.store, entry_id)

    def test_password_update_semantics_and_identity_changes_clear_tokens(self):
        entry_id = self.add("gmail", password="keep", client_secret="old-client-secret")
        vault.save_secret(self.store, entry_id, {"access_token": "at", "refresh_token": "rt", "expires_at": time.time() + 100})
        vault.save_entry(self.store, {"id": entry_id, "password": "", "config": {"notes": "line one\nline two"}})
        self.assertEqual(vault.reveal_entry(self.store, entry_id)["password"], "keep")
        self.assertEqual(vault.get_entry(self.store, entry_id)["secret"]["refresh_token"], "rt")
        vault.save_entry(self.store, {"id": entry_id, "client_secret": "changed"})
        self.assertNotIn("refresh_token", vault.get_entry(self.store, entry_id)["secret"])
        vault.save_secret(self.store, entry_id, {"access_token": "at", "refresh_token": "rt"})
        vault.save_entry(self.store, {"id": entry_id, "email": "different@example.com", "clear_password": True})
        secret = vault.get_entry(self.store, entry_id)["secret"]
        self.assertNotIn("access_token", secret)
        self.assertEqual(vault.reveal_entry(self.store, entry_id), {"password": ""})
        with self.assertRaises(vault.VaultError):
            vault.save_entry(self.store, {"id": entry_id, "config": {"password": "must-not-be-public"}})

    def test_shared_rust_python_crypto_fixture_and_separate_process_keys(self):
        path = os.path.join(os.path.dirname(__file__), "..", "tests", "shared", "vault_crypto.json")
        with open(path) as f:
            fixture = json.load(f)
        key = hashlib.pbkdf2_hmac("sha256", fixture["master_password"].encode(), bytes.fromhex(fixture["salt_hex"]), 600000, 32)
        self.assertEqual(key.hex(), fixture["derived_key_hex"])
        self.assertEqual(vault._decrypt(key, bytes.fromhex(fixture["check_hex"]), b"cam-vault-check"), b"cam-vault-v1")
        secret = vault._decrypt(key, bytes.fromhex(fixture["entry_blob_hex"]), ("cam-vault-entry:" + fixture["entry_id"]).encode())
        self.assertEqual(json.loads(secret), fixture["entry_secret"])
        with self.assertRaises(vault.VaultError):
            vault._decrypt(key, bytes.fromhex(fixture["entry_blob_hex"]), b"cam-vault-entry:other-id")
        spec = importlib.util.spec_from_file_location("other_backend_vault", vault.__file__)
        other = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(other)
        self.assertFalse(other.status(self.store)["unlocked"])
        self.assertTrue(other.unlock(self.store, {"master_password": "test master password"})["unlocked"])
        other.lock(self.store)

    def callback(self, connection, **parameters):
        job = mailbox._jobs[connection["job_id"]]
        query = urllib.parse.urlencode({"state": job["state"], **parameters})
        return urllib.request.urlopen(job["redirect_uri"] + "?" + query, timeout=5)

    def test_oauth_pkce_state_callback_and_exact_owner(self):
        entry_id = self.add("gmail")
        connection = mailbox.connect(self.store, entry_id)
        job = mailbox._jobs[connection["job_id"]]
        verifier = job["verifier"]
        query = urllib.parse.parse_qs(urllib.parse.urlparse(connection["authorization_url"]).query)
        self.assertEqual(query["scope"], ["https://www.googleapis.com/auth/gmail.readonly"])
        self.assertEqual(query["code_challenge_method"], ["S256"])
        challenge = base64.urlsafe_b64encode(hashlib.sha256(job["verifier"].encode()).digest()).decode().rstrip("=")
        self.assertEqual(query["code_challenge"], [challenge])
        with self.assertRaises(urllib.error.HTTPError):
            self.callback(connection, state="wrong-state", code="fake-code")
        self.assertEqual(mailbox.oauth_status(self.store, connection["job_id"])["status"], "pending")
        calls = []
        def fake_http(method, url, **kwargs):
            calls.append((method, url, kwargs))
            return {"access_token": "fake-access", "refresh_token": "fake-refresh", "expires_in": 3600} if method == "POST" else {"emailAddress": "person@example.com"}
        with mock.patch.object(mailbox, "_http_json", side_effect=fake_http):
            with self.callback(connection, code="fake-code") as response:
                self.assertEqual(response.status, 200)
        self.assertEqual(mailbox.oauth_status(self.store, connection["job_id"])["status"], "connected")
        self.assertEqual(calls[0][2]["form"]["redirect_uri"], job["redirect_uri"])
        self.assertEqual(calls[0][2]["form"]["code_verifier"], verifier)
        self.assertTrue(vault.status(self.store)["entries"][0]["connected"])
        self.assertNotIn("fake-access", json.dumps(mailbox.oauth_status(self.store, connection["job_id"])))

    def test_oauth_wrong_owner_locked_vault_and_changed_configuration_never_save_tokens(self):
        for mode in ("wrong-owner", "locked", "changed"):
            if not vault.status(self.store)["unlocked"]:
                vault.unlock(self.store, {"master_password": "test master password"})
            entry_id = self.add("gmail", label=mode)
            connection = mailbox.connect(self.store, entry_id)
            if mode == "locked":
                vault.lock(self.store)
            if mode == "changed":
                with self.assertRaisesRegex(vault.VaultError, "正在授权"):
                    vault.save_entry(self.store, {"id": entry_id, "email": "changed@example.com"})
                # 模拟未实现条目锁的旧后端；callback 的最终身份检查仍然应阻止写入。
                with contextlib.closing(sqlite3.connect(os.path.join(self.tmp.name, "vault.sqlite3"))) as db, db:
                    db.execute("UPDATE vault_entries SET email=? WHERE id=?", ("changed@example.com", entry_id))
            def fake_http(method, url, **kwargs):
                return {"access_token": "never-save", "refresh_token": "never-save-refresh"} if method == "POST" else {"emailAddress": "wrong@example.com"}
            with mock.patch.object(mailbox, "_http_json", side_effect=fake_http):
                with self.assertRaises(urllib.error.HTTPError):
                    self.callback(connection, code="fake")
            self.assertEqual(mailbox.oauth_status(self.store, connection["job_id"])["status"], "error")
            if mode == "locked":
                vault.unlock(self.store, {"master_password": "test master password"})
            self.assertNotIn("access_token", vault.get_entry(self.store, entry_id)["secret"])

    def test_oauth_timeout_cross_directory_and_entry_lock(self):
        entry_id = self.add("outlook")
        connection = mailbox.connect(self.store, entry_id)
        job = mailbox._jobs[connection["job_id"]]
        self.assertTrue(job["redirect_uri"].startswith("http://localhost:"))
        with self.assertRaisesRegex(mailbox.MailboxError, "正在授权"):
            mailbox.messages(self.store, entry_id, {})
        with self.assertRaises(mailbox.MailboxError):
            mailbox.oauth_status(types.SimpleNamespace(root="/not-this-vault"), connection["job_id"])
        job["expires_at"] = time.time() - 1
        self.assertEqual(mailbox.oauth_status(self.store, connection["job_id"])["status"], "error")
        self.assertTrue(job["closed"].wait(2))

    def test_oauth_missing_client_and_invalid_tenant(self):
        entry_id = self.add("outlook", config={})
        with self.assertRaisesRegex(mailbox.MailboxError, "Client ID"):
            mailbox.connect(self.store, entry_id)
        vault.save_entry(self.store, {"id": entry_id, "config": {"client_id": "client", "tenant": "evil/path"}})
        with self.assertRaisesRegex(mailbox.MailboxError, "tenant"):
            mailbox.connect(self.store, entry_id)

    def test_refresh_preserves_refresh_token_and_rechecks_owner(self):
        entry_id = self.add("gmail")
        vault.save_secret(self.store, entry_id, {"refresh_token": "keep-refresh", "expires_at": 0})
        def fake_http(method, url, **kwargs):
            if method == "POST":
                return {"access_token": "new-access", "expires_in": 3600}
            if url.endswith("/profile"):
                return {"emailAddress": "person@example.com"}
            return {"messages": []}
        with mock.patch.object(mailbox, "_http_json", side_effect=fake_http):
            result = mailbox.messages(self.store, entry_id, {})
        self.assertEqual(result["status"], "empty")
        secret = vault.get_entry(self.store, entry_id)["secret"]
        self.assertEqual(secret["refresh_token"], "keep-refresh")
        self.assertEqual(secret["access_token"], "new-access")
        with mock.patch.object(mailbox, "_http_json", return_value={"emailAddress": "other@example.com"}):
            with self.assertRaisesRegex(mailbox.MailboxError, "不一致"):
                mailbox.messages(self.store, entry_id, {})

    def test_outlook_owner_matches_mail_or_user_principal_name(self):
        entry_id = self.add("outlook")
        entry = vault.get_entry(self.store, entry_id)
        for profile in ({"mail": "alias@example.com", "userPrincipalName": "PERSON@EXAMPLE.COM"},
                        {"mail": "PERSON@EXAMPLE.COM", "userPrincipalName": "other@example.com"}):
            with mock.patch.object(mailbox, "_http_json", return_value=profile):
                self.assertEqual(mailbox._identity(entry, "token").casefold(), "person@example.com")
        with mock.patch.object(mailbox, "_http_json", return_value={"mail": "alias@example.com", "userPrincipalName": "other@example.com"}):
            with self.assertRaisesRegex(mailbox.MailboxError, "不一致"):
                mailbox._identity(entry, "token")

    def test_sender_link_and_time_filtering_never_returns_body(self):
        since = mailbox._now() - dt.timedelta(minutes=15)
        received = mailbox._now()
        body = ('Your verification code is 123456. <a href="https://claude.ai/magic-link?token=example">Sign in</a> '
                'https://claude.ai.evil.test/auth?code=bad https://evil.test/auth?code=bad '
                'https://user@claude.ai/auth?code=bad https://claude.ai:invalid/auth?code=bad')
        summary = mailbox._item("1", "Claude <noreply@mail.anthropic.com>", "Sign in to Claude", received, body, since, False)
        self.assertEqual(summary["codes"], ["123456"])
        self.assertEqual(summary["verification_links"], ["https://claude.ai/magic-link?token=example"])
        self.assertTrue(summary["official"] and summary["login"])
        self.assertEqual((summary["sender"], summary["sender_name"]), ("noreply@mail.anthropic.com", "Claude"))
        self.assertNotIn("body", summary)
        self.assertLessEqual(len(summary["preview"]), 160)
        self.assertIsNone(mailbox._item("1", "fake@anthropic.com.evil.test", "Sign in", received, body, since, False))
        self.assertIsNone(mailbox._item("1", "noreply@claude.ai", "Sign in", received - dt.timedelta(hours=2), body, since, False))
        # 看全部邮件时别人发的也列出来，但不从里面找验证码和链接
        other = mailbox._item("1", "fake@anthropic.com.evil.test", "Sign in", received, body, since, True)
        self.assertEqual((other["official"], other["login"], other["codes"], other["verification_links"]), (False, False, [], []))
        # email.claude.com 发的欢迎信也算官方邮件，只是没有验证码
        welcome = mailbox._item("2", "Claude Team <no-reply@email.claude.com>", "Welcome to Claude", received,
                                "<style>p{color:#123456}</style><p>Let's get you set up.</p>", since, False)
        self.assertEqual((welcome["official"], welcome["login"], welcome["codes"]), (True, False, []))
        self.assertEqual(welcome["preview"], "Let's get you set up.")

    def test_verification_link_token_digits_are_not_one_time_codes(self):
        since = mailbox._now() - dt.timedelta(minutes=15)
        link = "https://claude.ai/magic-link?token=123456"
        for body in ("Sign in using " + link, '<a href="' + link + '">Verify sign in</a>'):
            summary = mailbox._item("1", "noreply@anthropic.com", "Sign in to Claude", mailbox._now(), body, since, False)
            self.assertEqual(summary["codes"], [])
            self.assertEqual(summary["verification_links"], [link])
        summary = mailbox._item("1", "noreply@anthropic.com", "Sign in to Claude", mailbox._now(),
                                "Your code is 654321. " + link, since, False)
        self.assertEqual(summary["codes"], ["654321"])

    def test_login_summary_rejects_multiple_from_addresses(self):
        since = mailbox._now() - dt.timedelta(minutes=15)
        for sender in ("noreply@anthropic.com, attacker@evil.test", "noreply@anthropic.com, noreply@claude.ai"):
            self.assertIsNone(mailbox._item("1", sender, "Sign in to Claude", mailbox._now(),
                                            "Verification code: 123456", since, False))

    def test_gmail_and_outlook_return_summaries_only_and_never_write_mail(self):
        for provider in ("gmail", "outlook"):
            entry_id = self.add(provider, label=provider)
            vault.save_secret(self.store, entry_id, {"access_token": "access", "expires_at": time.time() + 3600})
            text = "Your Claude verification code is 123456. https://claude.ai/magic-link?token=sample"
            calls = []
            def fake_http(method, url, **kwargs):
                calls.append((method, url))
                if url.endswith("/profile"):
                    return {"emailAddress": "person@example.com"}
                if "/v1.0/me?" in url:
                    return {"mail": "person@example.com"}
                if "?format=full" in url:
                    return {"sizeEstimate": len(text), "internalDate": str(time.time() * 1000), "payload": {"mimeType": "text/plain",
                            "headers": [{"name": "From", "value": "noreply@anthropic.com"}, {"name": "Subject", "value": "Sign in to Claude"}],
                            "body": {"data": base64.urlsafe_b64encode(text.encode()).decode()}}}
                if "gmail.googleapis" in url:
                    return {"messages": [{"id": "msg"}]}
                return {"value": [{"id": "msg", "from": {"emailAddress": {"address": "noreply@anthropic.com"}}, "subject": "Sign in to Claude",
                                   "receivedDateTime": mailbox._iso(mailbox._now()), "body": {"content": text}}]}
            with mock.patch.object(mailbox, "_http_json", side_effect=fake_http):
                result = mailbox.messages(self.store, entry_id, {})
            self.assertEqual(result["status"], "found")
            self.assertEqual(result["messages"][0]["codes"], ["123456"])
            self.assertNotIn("body", result["messages"][0])
            self.assertTrue(all(method == "GET" for method, _ in calls))
            self.assertFalse(any("magic-link" in url for _, url in calls))
            with open(os.path.join(self.tmp.name, "vault.sqlite3"), "rb") as f:
                self.assertNotIn(text.encode(), f.read())

    def test_imap_uses_verified_tls_readonly_select_and_body_peek(self):
        entry_id = self.add("imap", password="app-password", config={"imap_host": "imap.example.com"})
        raw = b"From: noreply@anthropic.com\r\nSubject: Sign in to Claude\r\nContent-Type: text/plain\r\n\r\nVerification code: 123456"
        internal = dt.datetime.now(dt.timezone.utc).strftime('%d-%b-%Y %H:%M:%S')
        head = raw.split(b"Content-Type")[0] + b"\r\n"
        meta = f'1 (UID 1 RFC822.SIZE {len(raw)} INTERNALDATE "{internal} +0000" BODY[HEADER.FIELDS (FROM SUBJECT)] {{{len(head)}}}'.encode()
        client = mock.Mock()
        client.select.return_value = ("OK", [b"1"])
        client.uid.side_effect = [("OK", [b"1"]), ("OK", [(meta, head), b")"]), ("OK", [(b"body", raw), b")"])]
        if sys.version_info >= (3, 9):
            with mock.patch.object(mailbox.imaplib, "IMAP4_SSL", return_value=client) as factory:
                result = mailbox.messages(self.store, entry_id, {})
            self.assertTrue(factory.call_args.kwargs["ssl_context"].check_hostname)
            self.assertEqual(factory.call_args.kwargs["timeout"], 20)
        else:  # Python 3.8: no constructor timeout, the module builds the connection in a subclass
            with mock.patch.object(mailbox, "_imap_connection", return_value=client) as factory:
                result = mailbox.messages(self.store, entry_id, {})
            self.assertTrue(factory.call_args.args[2].check_hostname)
        self.assertEqual(result["status"], "found")
        self.assertEqual(result["messages"][0]["codes"], ["123456"])
        search = client.uid.call_args_list[0].args
        self.assertIn('"claude.com"', search)
        self.assertTrue(all("PEEK" in call.args[-1] for call in client.uid.call_args_list[1:]))
        client.select.assert_called_once_with("INBOX", readonly=True)
        self.assertIn("BODY.PEEK", client.uid.call_args_list[-1].args[-1])
        self.assertFalse(any(call.args[0].lower() in ("store", "copy", "delete", "expunge") for call in client.uid.call_args_list))
        client.logout.assert_called_once()

    def test_messages_missing_imap_settings_and_invalid_since(self):
        entry_id = self.add("imap", password="example")
        with self.assertRaisesRegex(mailbox.MailboxError, "收信服务器"):
            mailbox.messages(self.store, entry_id, {})
        with self.assertRaisesRegex(mailbox.MailboxError, "since"):
            mailbox.messages(self.store, entry_id, {"since": "not-a-date"})

    def test_pending_oauth_prevents_all_entry_mutations(self):
        entry_id = self.add("gmail")
        connection = mailbox.connect(self.store, entry_id)
        for operation in (lambda: vault.save_entry(self.store, {"id": entry_id, "label": "changed"}),
                          lambda: vault.delete_entry(self.store, entry_id),
                          lambda: vault.set_email(self.store, entry_id, "changed@example.com")):
            with self.assertRaisesRegex(vault.VaultError, "正在授权"):
                operation()
        job = mailbox._jobs[connection["job_id"]]
        mailbox._finish(job, "error", "test cancel")
        self.assertTrue(job["closed"].wait(2))
        vault.save_entry(self.store, {"id": entry_id, "label": "now allowed"})
        self.assertEqual(vault.get_entry(self.store, entry_id)["label"], "now allowed")

    def test_message_candidate_limit_and_seven_day_lookback(self):
        entry_id = self.add("gmail")
        vault.save_secret(self.store, entry_id, {"access_token": "access", "expires_at": time.time() + 3600})
        ids = [{"id": str(i)} for i in range(50)]
        urls = []
        def fake_http(method, url, **kwargs):
            urls.append(url)
            if url.endswith("/profile"):
                return {"emailAddress": "person@example.com"}
            if "?format=full" in url:
                return {"sizeEstimate": mailbox._MAX_MESSAGE + 1}
            return {"messages": ids}
        with mock.patch.object(mailbox, "_http_json", side_effect=fake_http):
            with self.assertRaisesRegex(mailbox.MailboxError, "最近 7 天"):
                mailbox.messages(self.store, entry_id, {"since": "2000-01-01T00:00:00Z"})
            with self.assertRaisesRegex(mailbox.MailboxError, "最近 7 天"):
                mailbox.messages(self.store, entry_id, {"since": mailbox._iso(mailbox._now() - dt.timedelta(days=8))})
            with self.assertRaisesRegex(mailbox.MailboxError, "all"):
                mailbox.messages(self.store, entry_id, {"all": "yes"})
            self.assertEqual(urls, [])
            since = mailbox._iso(mailbox._now() - dt.timedelta(minutes=30))
            result = mailbox.messages(self.store, entry_id, {"since": since})
        self.assertEqual(result["status"], "empty")
        self.assertEqual(sum("?format=full" in url for url in urls), 20)
        query = next(url for url in urls if "messages?" in url)
        stamp = int(urllib.parse.parse_qs(urllib.parse.urlparse(query).query)["q"][0].split("after:")[1].split()[0])
        self.assertLessEqual(abs(time.time() - stamp - 1800), 2)

    def test_detect_shared_fixture(self):
        path = os.path.join(os.path.dirname(__file__), os.pardir, "tests", "shared", "mail_detect.json")
        with open(path, encoding="utf-8") as f:
            cases = json.load(f)["cases"]
        for case in cases:
            with self.subTest(email=case["email"]):
                if case["expect"] is None:
                    with self.assertRaises(mailbox.MailboxError):
                        mailbox.detect(case["email"])
                    continue
                got = mailbox.detect(case["email"])
                self.assertEqual({k: got[k] for k in case["expect"]}, case["expect"])
                self.assertEqual(got["imap_port"], 993)
                self.assertTrue(got["note"])

    def test_oauth_client_id_is_shared_between_entries_of_the_same_kind(self):
        first = vault.save_entry(self.store, {"label": "a", "email": "a@outlook.com", "provider": "outlook",
                                              "config": {"client_id": "my-app", "tenant": "consumers"}})["entries"][-1]["id"]
        second = vault.save_entry(self.store, {"label": "b", "email": "b@outlook.com", "provider": "outlook", "config": {}})["entries"][-1]["id"]
        gmail = vault.save_entry(self.store, {"label": "c", "email": "c@gmail.com", "provider": "gmail", "config": {}})["entries"][-1]["id"]
        self.assertEqual(mailbox._entry(self.store, second)["config"]["client_id"], "my-app")
        self.assertEqual(mailbox._entry(self.store, second)["config"]["tenant"], "consumers")
        self.assertEqual(mailbox._entry(self.store, first)["config"]["client_id"], "my-app")
        self.assertFalse(mailbox._entry(self.store, gmail)["config"].get("client_id"))
        with self.assertRaisesRegex(mailbox.MailboxError, "Client ID"):
            mailbox.connect(self.store, gmail)
        self.assertFalse(vault.get_entry(self.store, second)["config"].get("client_id"))  # 借用不写回账号库

    def test_all_mail_lists_everyone_but_only_reads_official_bodies(self):
        entry_id = self.add("gmail")
        vault.save_secret(self.store, entry_id, {"access_token": "access", "expires_at": time.time() + 3600})
        login = "Your Claude verification code is 123456."
        now_ms = str(time.time() * 1000)
        def head(sender, subject):
            return [{"name": "From", "value": sender}, {"name": "Subject", "value": subject}]
        urls = []
        def fake_http(method, url, **kwargs):
            urls.append(url)
            if url.endswith("/profile"):
                return {"emailAddress": "person@example.com"}
            if "/messages?" in url:
                return {"messages": [{"id": "a1"}, {"id": "b2"}, {"id": "bad id"}]}
            if "/a1?format=metadata" in url:
                return {"sizeEstimate": 100, "internalDate": now_ms, "snippet": "Your Claude code &amp; link", "payload": {"headers": head("Anthropic <noreply@mail.anthropic.com>", "Sign in")}}
            if "/a1?format=full" in url:
                return {"sizeEstimate": 100, "internalDate": now_ms, "snippet": "x", "payload": {"mimeType": "text/plain", "headers": head("noreply@mail.anthropic.com", "Sign in"),
                        "body": {"data": base64.urlsafe_b64encode(login.encode()).decode()}}}
            if "/b2?format=metadata" in url:
                return {"sizeEstimate": 100, "internalDate": now_ms, "snippet": "Hello &#39;friend&#39;", "payload": {"headers": head("Friend <f@example.org>", "Hi")}}
            raise AssertionError(url)
        with mock.patch.object(mailbox, "_http_json", side_effect=fake_http):
            result = mailbox.messages(self.store, entry_id, {"all": True, "since": mailbox._iso(mailbox._now() - dt.timedelta(days=7))})
        self.assertTrue(result["all"])
        by_id = {m["id"]: m for m in result["messages"]}
        self.assertEqual(set(by_id), {"a1", "b2"})
        self.assertEqual(by_id["a1"]["codes"], ["123456"])
        self.assertEqual((by_id["b2"]["official"], by_id["b2"]["codes"], by_id["b2"]["preview"]), (False, [], "Hello 'friend'"))
        self.assertIn("in%3Ainbox", next(u for u in urls if "/messages?" in u))
        self.assertFalse(any("/b2?format=full" in u for u in urls))

    def test_detail_returns_original_html_without_scripts(self):
        entry_id = self.add("outlook")
        vault.save_secret(self.store, entry_id, {"access_token": "access", "expires_at": time.time() + 3600})
        page = ('<html><head><meta http-equiv="refresh" content="0;url=https://evil.test"><base href="https://evil.test/"></head>'
                '<body onload="x()"><script>steal()</script><p style="color:red">Sign in to Claude</p>'
                '<a href="https://claude.ai/magic-link?token=t">Sign in</a><iframe src="https://evil.test"></iframe>'
                '<form action="https://evil.test"><input></form><img src="https://cdn.example/logo.png"></body></html>')
        calls = []
        def fake_http(method, url, **kwargs):
            calls.append((method, url, kwargs.get("headers")))
            if "/v1.0/me?" in url:
                return {"mail": "person@example.com"}
            return {"id": "AAMk-1=", "from": {"emailAddress": {"name": "Anthropic", "address": "noreply@mail.anthropic.com"}},
                    "subject": "Your secure link to Claude", "receivedDateTime": mailbox._iso(mailbox._now()),
                    "body": {"contentType": "html", "content": page}}
        with mock.patch.object(mailbox, "_http_json", side_effect=fake_http):
            detail = mailbox.message(self.store, entry_id, {"id": "AAMk-1="})
            with self.assertRaisesRegex(mailbox.MailboxError, "编号"):
                mailbox.message(self.store, entry_id, {"id": "../me/sendMail"})
        self.assertTrue(all(method == "GET" for method, _, _ in calls))
        for bad in ("<script", "steal()", "http-equiv", "<base", "<iframe", "<form"):
            self.assertNotIn(bad, detail["html"])
        self.assertIn('style="color:red"', detail["html"])
        self.assertIn("https://cdn.example/logo.png", detail["html"])
        self.assertEqual(detail["verification_links"], ["https://claude.ai/magic-link?token=t"])
        self.assertTrue(detail["official"] and detail["login"])
        self.assertEqual(detail["sender_name"], "Anthropic")

    def test_imap_detail_is_read_only_and_outlook_password_refusal_is_explained(self):
        entry_id = self.add("imap", password="pw", config={"imap_host": "outlook.office365.com"})
        raw = (b"From: Friend <f@example.org>\r\nSubject: Hi\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n"
               b"--b\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--b\r\nContent-Type: text/html\r\n\r\n<b>rich</b><script>x</script>\r\n--b--\r\n")
        internal = dt.datetime.now(dt.timezone.utc).strftime('%d-%b-%Y %H:%M:%S')
        client = mock.Mock()
        client.select.return_value = ("OK", [b"1"])
        client.uid.side_effect = [("OK", [f'7 (UID 7 RFC822.SIZE {len(raw)} INTERNALDATE "{internal} +0000")'.encode()]), ("OK", [(b"7 (BODY[]<0> {1}", raw), b")"])]
        with mock.patch.object(mailbox, "_imap_connection", return_value=client):
            detail = mailbox.message(self.store, entry_id, {"id": "7"})
            with self.assertRaisesRegex(mailbox.MailboxError, "编号"):
                mailbox.message(self.store, entry_id, {"id": "7:*"})
        self.assertEqual((detail["text"].strip(), detail["official"]), ("plain body", False))
        self.assertIn("<b>rich</b>", detail["html"])
        self.assertNotIn("<script", detail["html"])
        self.assertTrue(all("PEEK" in call.args[-1] or "RFC822.SIZE" in call.args[-1] for call in client.uid.call_args_list))
        client.select.assert_called_with("INBOX", readonly=True)
        refusing = mock.Mock()
        refusing.login.side_effect = mailbox.imaplib.IMAP4.error(b"AUTHENTICATE failed.")
        with mock.patch.object(mailbox, "_imap_connection", return_value=refusing):
            with self.assertRaisesRegex(mailbox.MailboxError, "Outlook 授权"):
                mailbox.messages(self.store, entry_id, {})
        refusing.logout.assert_called()

    def test_sql_parameters_and_invalid_secret_fields(self):
        entry_id = self.add(password="safe")
        with self.assertRaises(vault.VaultError):
            vault.delete_entry(self.store, "' OR 1=1 --")
        self.assertEqual(vault.reveal_entry(self.store, entry_id)["password"], "safe")
        for body in ({"expires_at": float("nan")}, {"unknown": "secret"}, {"refresh_token": None}):
            with self.assertRaises(vault.VaultError):
                vault.save_secret(self.store, entry_id, body)


if __name__ == "__main__":
    unittest.main(verbosity=2)
