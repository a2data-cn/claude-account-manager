import json
import os
import pathlib
import struct
import tempfile
import types
import unittest
from unittest import mock

import cam
import cam_identity


def cache_entry(owner, email, endpoint="https://claude.ai/api/account"):
    key = ("1/0/" + endpoint).encode()
    header = bytes.fromhex("305c72a71b6dfbfc") + struct.pack("<IIII", 5, len(key), 0, 0)
    return header + key + json.dumps({"uuid": owner, "email_address": email}).encode() + b"\xab" * 32


class IdentityTest(unittest.TestCase):
    def test_verification_uses_the_bound_profile_and_never_restarts_a_desktop_login(self):
        import cam_vault
        with tempfile.TemporaryDirectory() as directory:
            account = {"id": "target", "kind": "claude_web", "email": "work@example.com"}
            store = types.SimpleNamespace(root=directory, profiles_dir=os.path.join(directory, "profiles"),
                                          load=lambda: {"active": {}, "accounts": [account]})
            cam_vault.setup(store, {"master_password": "synthetic master password"})
            state = cam_vault.save_entry(store, {"provider": "credential", "email": "work@example.com", "account_id": "target"})
            entry = state["entries"][0]["id"]
            url = "https://claude.ai/magic-link?ticket=example-only"
            with mock.patch.object(cam, "open_web_account") as browser, mock.patch.object(cam, "open_external_url") as external:
                cam.open_verification_url(store, entry, url)
                browser.assert_called_once_with(store, account, url=url)
                external.assert_not_called()
            account.update(kind="claude_desktop", desktop_profile="target")
            expected = os.path.normcase(os.path.realpath(cam.desktop_profile_dir(store, account)))
            with mock.patch.object(cam, "_desktop_processes", return_value=[(42, expected)]), \
                    mock.patch.object(cam, "open_external_url") as external, mock.patch.object(cam, "_quit_claude_desktop") as quit_app:
                cam.open_verification_url(store, entry, url)
                external.assert_called_once_with(url)
                quit_app.assert_not_called()
            with mock.patch.object(cam, "_desktop_processes", return_value=[(42, "/wrong-profile")]), mock.patch.object(cam, "open_external_url") as external:
                with self.assertRaises(cam.CamError):
                    cam.open_verification_url(store, entry, url)
                external.assert_not_called()
            for invalid in ("https://evil.example/magic-link", "https://accounts.google.com/o/oauth2/v2/auth"):
                with self.assertRaises(cam.CamError):
                    cam.open_verification_url(store, entry, invalid)
            account["email"] = "other@example.com"
            with self.assertRaises(cam.CamError):
                cam.open_verification_url(store, entry, url)
            cam_vault.lock(store)

    def test_only_current_owner_and_exact_identity_endpoint_can_supply_email(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "config.json").write_text(json.dumps({"lastKnownAccountUuid": "account-A"}))
            cache = root / "Cache" / "Cache_Data"
            cache.mkdir(parents=True)
            (cache / "wrong-account_0").write_bytes(cache_entry("account-B", "wrong@example.com"))
            (cache / "foreign_0").write_bytes(cache_entry("account-A", "wrong@example.com", "https://evil.example/https://claude.ai/api/account"))
            with self.assertRaises(cam_identity.IdentityError):
                cam_identity.read_desktop_identity(directory)
            (cache / "correct_0").write_bytes(cache_entry("account-A", "work@example.com"))
            identity = cam_identity.read_desktop_identity(directory)
            self.assertEqual(identity, {"email": "work@example.com", "desktop_account_uuid": "account-A"})
            with self.assertRaises(cam_identity.IdentityError):
                cam_identity.read_desktop_identity(directory, "account-B")

    def test_missing_or_corrupt_current_identity_never_imports_an_old_email(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            cache = root / "Cache" / "Cache_Data"
            cache.mkdir(parents=True)
            (cache / "correct_0").write_bytes(cache_entry("account-A", "old@example.com"))
            for raw in ("{}", "broken", "[]"):
                (root / "config.json").write_text(raw)
                with self.assertRaises(cam_identity.IdentityError):
                    cam_identity.read_desktop_identity(directory)

    def test_external_browser_urls_have_a_strict_allowlist(self):
        for url in ("https://claude.ai/magic-link?ticket=example", "https://accounts.google.com/o/oauth2/v2/auth?state=example",
                    "https://login.microsoftonline.com/common/oauth2/v2.0/authorize"):
            self.assertTrue(cam_identity.allowed_external_url(url), url)
        for url in ("file:///etc/passwd", "http://claude.ai/login", "https://claude.ai.evil.example/login", "https://user:secret@claude.ai/login",
                    "https://claude.ai:9443/login", "https://accounts.google.com/other", "https://evil.example/?next=https://claude.ai", None):
            self.assertFalse(cam_identity.allowed_external_url(url), url)

    def test_python_router_uses_encrypted_vault_and_never_exposes_oauth_tokens(self):
        with tempfile.TemporaryDirectory() as directory:
            store = types.SimpleNamespace(root=directory)
            self.assertFalse(cam.vault_route(store, "GET", ["vault"])["initialized"])
            self.assertFalse(os.path.exists(os.path.join(directory, "vault.sqlite3")))
            cam.vault_route(store, "POST", ["vault", "setup"], {"master_password": "synthetic master password"})
            state = cam.vault_route(store, "POST", ["vault", "entries"], {"provider": "outlook", "email": "work@example.com", "password": "synthetic account password"})
            entry = state["entries"][0]
            self.assertNotIn("password", entry)
            self.assertNotIn("secret", entry)
            self.assertEqual(cam.vault_route(store, "POST", ["vault", "entries", entry["id"], "reveal"])["password"], "synthetic account password")
            with self.assertRaises(cam.CamError):
                cam.vault_route(store, "POST", ["vault", "entries", entry["id"], "connect"])
            state = cam.vault_route(store, "POST", ["vault", "lock"])
            self.assertEqual(state["entries"], [])
            with self.assertRaises(cam.CamError):
                cam.vault_route(store, "POST", ["vault", "entries", entry["id"], "reveal"])


if __name__ == "__main__":
    unittest.main()
