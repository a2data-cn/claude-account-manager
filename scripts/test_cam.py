"""cam.py 的端到端测试：伪造 HOME、伪造 Anthropic 接口，不碰真实账号。

运行：python3 scripts/test_cam.py
"""
import http.server
import importlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))


class MockAnthropic(http.server.BaseHTTPRequestHandler):
    calls = []
    # access token -> 账号资料
    tokens = {}
    refresh_map = {}
    # sessionKey -> 网页账号资料；cloudflare=True 时所有网页请求返回挑战页
    web_keys = {}
    cloudflare = False
    trace_loc = "US"
    trace_warp = False
    trace_ts_offset = 0  # 服务器时间相对本机的偏差（秒）
    meta_org = "Comcast Cable Communications, LLC"  # /meta 和 /ipapi 返回的运营商
    meta_country = None  # /meta 和 /ipapi 返回的国家，None 表示和 trace 一致
    ipapi_type = "residential"  # /ipapi（ip-api.com 格式）的 IP 类型：residential / hosting / proxy / mobile / None=查询失败
    rdap_country = "US"  # /rdap/<ip> 返回的注册国，None 表示记录没写
    trace6_loc = None  # /trace6 的地区，None 表示 IPv6 不可用
    status_indicator = "none"  # 官方状态页：none / minor / major / critical

    def log_message(self, *a):
        pass

    def _send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def _send_html(self, code, html):
        b = html.encode()
        self.send_response(code)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def _cookie_key(self):
        c = self.headers.get("Cookie") or ""
        for part in c.split(";"):
            part = part.strip()
            if part.startswith("sessionKey="):
                return part[len("sessionKey="):]
        return None

    def _web(self):
        """返回 True 表示已处理网页接口。"""
        if self.cloudflare:
            self._send_html(403, "<html><body>Just a moment...<script>cf-chl</script></body></html>")
            return True
        sk = self._cookie_key()
        who = self.web_keys.get(sk)
        if self.path == "/api/organizations":
            if not who:
                return self._send(403, {"error": {"message": "unauthorized"}}) or True
            caps = ["chat"] + ([f"claude_{who['plan']}"] if who.get("plan") else [])
            return self._send(200, [{"uuid": "org-web", "name": who.get("org", "Web Org"), "capabilities": caps}]) or True
        if self.path == "/api/account":
            if not who:
                return self._send(403, {"error": {"message": "unauthorized"}}) or True
            return self._send(200, {"email_address": who["email"]}) or True
        if self.path.startswith("/api/organizations/") and self.path.endswith("/usage"):
            if not who:
                return self._send(403, {"error": {"message": "unauthorized"}}) or True
            return self._send(200, {
                "five_hour": {"utilization": who["u5"], "resets_at": "2099-01-01T00:00:00Z"},
                "seven_day": {"utilization": who["u7"], "resets_at": "2099-01-05T00:00:00Z"},
            }) or True
        return False

    def do_GET(self):
        MockAnthropic.calls.append(("GET", self.path, self.headers.get("Authorization") or self.headers.get("x-api-key")))
        if self.path in ("/api/organizations", "/api/account") or (
                self.path.startswith("/api/organizations/") and self.path.endswith("/usage")):
            if self._web():
                return
        auth = (self.headers.get("Authorization") or "")[7:]
        if self.path == "/cdn-cgi/trace":
            return self._send_html(200, f"fl=1\nip=1.2.3.4\nts={time.time() + self.trace_ts_offset:.3f}\n"
                                        f"loc={self.trace_loc}\ncolo=SJC\nwarp={'on' if self.trace_warp else 'off'}\n")
        if self.path.startswith("/ipapi"):
            if self.ipapi_type is None:
                return self._send(200, {"status": "fail", "message": "quota"})
            return self._send(200, {"status": "success", "countryCode": self.meta_country or self.trace_loc, "city": "San Jose",
                                    "regionName": "California", "isp": self.meta_org, "org": self.meta_org,
                                    "as": f"AS7922 {self.meta_org}", "mobile": self.ipapi_type == "mobile",
                                    "proxy": self.ipapi_type == "proxy", "hosting": self.ipapi_type == "hosting",
                                    "query": "1.2.3.4"})
        if self.path.startswith("/rdap/"):
            return self._send(200, {"handle": "NET-1", "country": self.rdap_country} if self.rdap_country
                              else {"handle": "NET-1"})
        if self.path == "/trace4":
            return self._send_html(200, f"ip=1.2.3.4\nloc={self.trace_loc}\ncolo=SJC\n")
        if self.path == "/trace6":
            if not self.trace6_loc:
                return self._send(404, {})
            return self._send_html(200, f"ip=2001:db8::1\nloc={self.trace6_loc}\ncolo=HKG\n")
        if self.path == "/meta":
            return self._send(200, {"clientIp": "1.2.3.4", "asn": 7922, "asOrganization": self.meta_org, "city": "San Jose",
                                    "region": "California", "country": self.meta_country or self.trace_loc})
        if self.path == "/status.json":
            desc = {"none": "All Systems Operational", "minor": "Degraded Performance", "major": "Partial System Outage",
                    "critical": "Major System Outage"}[self.status_indicator]
            return self._send(200, {"status": {"indicator": self.status_indicator, "description": desc}})
        if self.path == "/api/oauth/usage":
            who = self.tokens.get(auth)
            if not who:
                return self._send(401, {"error": {"message": "invalid token"}})
            return self._send(200, {
                "five_hour": {"utilization": who["u5"], "resets_at": "2099-01-01T00:00:00Z"},
                "seven_day": {"utilization": who["u7"], "resets_at": "2099-01-05T00:00:00Z"},
                "seven_day_opus": None,
            })
        if self.path == "/api/oauth/profile":
            who = self.tokens.get(auth)
            if not who:
                return self._send(401, {"error": {"message": "invalid token"}})
            return self._send(200, {"account": {"email": who["email"], "has_claude_max": who["plan"] == "max",
                                                "has_claude_pro": who["plan"] == "pro"},
                                    "organization": {"name": who["email"] + "'s Org", "rate_limit_tier": "tier_x"}})
        if self.path.startswith("/v1/models"):
            ok = self.headers.get("x-api-key") == "sk-ant-api-good"
            return self._send(200 if ok else 401, {"data": []} if ok else {"error": {"message": "bad key"}})
        if self.path.startswith("/v1/organizations/cost_report"):
            if "page=p2" in self.path:
                return self._send(200, {"data": [{"results": [{"amount": "250"}]}], "has_more": False})
            return self._send(200, {"data": [{"results": [{"amount": "1000.5"}, {"amount": "49.5"}]}],
                                    "has_more": True, "next_page": "p2"})
        self._send(404, {"error": {"message": "nope"}})

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(n) or b"{}")
        MockAnthropic.calls.append(("POST", self.path, body.get("refresh_token")))
        if self.path == "/v1/oauth/token":
            new = self.refresh_map.get(body.get("refresh_token"))
            if not new:
                return self._send(400, {"error": {"message": "invalid_grant"}})
            return self._send(200, {"access_token": new[0], "refresh_token": new[1], "expires_in": 28800})
        self._send(404, {})


def creds(at, rt, expires_ms, sub="max"):
    return {"claudeAiOauth": {"accessToken": at, "refreshToken": rt, "expiresAt": expires_ms,
                              "scopes": ["user:inference", "user:profile"], "subscriptionType": sub}}


def oauth_account(uuid, email):
    return {"accountUuid": uuid, "emailAddress": email, "organizationUuid": "org-" + uuid,
            "organizationName": email + " org", "displayName": email.split("@")[0]}


class CamTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MockAnthropic)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{cls.srv.server_address[1]}"
        os.environ["CAM_API_BASE"] = base
        os.environ["CAM_CLAUDE_WEB_BASE"] = base
        os.environ["CAM_TOKEN_URLS"] = base + "/v1/oauth/token"
        os.environ["CAM_STATUS_URL"] = base + "/status.json"
        os.environ["CAM_CF_META_URL"] = base + "/meta"
        os.environ["CAM_IPAPI_URL"] = base + "/ipapi"
        os.environ["CAM_IPAPI_IS_KEY"] = ""
        os.environ["CAM_RDAP_URL"] = base + "/rdap/"
        os.environ["CAM_CF_TRACE4_URL"] = base + "/trace4"
        os.environ["CAM_CF_TRACE6_URL"] = base + "/trace6"
        os.environ["CAM_TEST_LOCALE"] = "en_US"
        os.environ["CAM_TEST_DNS"] = "192.168.1.1"
        os.environ["CAM_TEST_TZ"] = "America/New_York=US"  # 跑测试的机器时区不定，固定成和 mock 出口一致
        os.environ["CAM_TEST_ROUTE_IF"] = ""
        os.environ["CAM_NO_KEYCHAIN"] = "1"
        os.environ["NO_COLOR"] = "1"
        for k in ("HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"):
            os.environ.pop(k, None)
        sys.path.insert(0, HERE)
        import cam
        cls.cam = importlib.reload(cam)

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        os.environ["HOME"] = self.tmp
        os.environ["USERPROFILE"] = self.tmp
        os.environ.pop("CAM_HOME", None)
        os.environ.pop("CLAUDE_CONFIG_DIR", None)
        os.makedirs(os.path.join(self.tmp, ".claude", "projects", "proj1"))
        self.creds_path = os.path.join(self.tmp, ".claude", ".credentials.json")
        self.cj_path = os.path.join(self.tmp, ".claude.json")
        with open(self.cj_path, "w") as f:
            json.dump({"numStartups": 42, "projects": {"/work/x": {"allowedTools": ["Bash"]}},
                       "mcpServers": {"m": {"command": "x"}}}, f)
        MockAnthropic.calls = []
        MockAnthropic.tokens = {}
        MockAnthropic.refresh_map = {}
        MockAnthropic.web_keys = {}
        MockAnthropic.cloudflare = False
        MockAnthropic.trace_loc = "US"
        MockAnthropic.trace_warp = False
        MockAnthropic.trace_ts_offset = 0
        MockAnthropic.status_indicator = "none"
        MockAnthropic.meta_org = "Comcast Cable Communications, LLC"
        MockAnthropic.meta_country = None
        MockAnthropic.ipapi_type = "residential"
        MockAnthropic.rdap_country = "US"
        MockAnthropic.trace6_loc = None
        # 跑测试的机器上可能真有 claude 在运行，测试里统一当作没有
        self._real_procs = self.cam.running_claude_processes
        self.cam.running_claude_processes = lambda: []

    def tearDown(self):
        self.cam.running_claude_processes = self._real_procs
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---- helpers
    def login(self, at, rt, uuid, email, expires_ms=None, sub="max"):
        """模拟用户在 Claude Code 里 /login。"""
        self.write_creds(at, rt, expires_ms, sub)
        with open(self.cj_path) as f:
            cj = json.load(f)
        cj["oauthAccount"] = oauth_account(uuid, email)
        with open(self.cj_path, "w") as f:
            json.dump(cj, f)

    def write_creds(self, at, rt, expires_ms=None, sub="max"):
        """只改凭据文件，不动 ~/.claude.json（模拟正在运行的 claude 刷新 token 后写回）。"""
        if expires_ms is None:
            expires_ms = int((time.time() + 3600) * 1000)
        with open(self.creds_path, "w") as f:
            json.dump(creds(at, rt, expires_ms, sub), f)

    def live(self):
        with open(self.creds_path) as f, open(self.cj_path) as g:
            return json.load(f)["claudeAiOauth"], json.load(g)

    def run_cam(self, *args):
        return self.cam.main(list(args))

    def data(self):
        return self.cam.Store().load()

    # ---- tests
    def test_import_switch_keeps_everything(self):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.assertEqual(self.run_cam("import", "--label", "A"), 0)
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com", sub="pro")
        self.assertEqual(self.run_cam("import", "--label", "B"), 0)
        d = self.data()
        self.assertEqual([a["label"] for a in d["accounts"]], ["A", "B"])
        b_id = d["accounts"][1]["id"]
        self.assertEqual(d["active"]["claude_code"], b_id)
        self.assertEqual(d["accounts"][1]["plan"], "pro")

        # 会话记录（不应被触碰）
        hist = os.path.join(self.tmp, ".claude", "projects", "proj1", "s.jsonl")
        open(hist, "w").write('{"x":1}\n')

        # Claude Code 运行期间自己刷新了 B 的 token
        self.login("at-B2", "rt-B2", "uuid-B", "b@x.com", sub="pro")

        self.assertEqual(self.run_cam("switch", "A"), 0)
        live, cj = self.live()
        self.assertEqual(live["accessToken"], "at-A1")
        self.assertEqual(cj["oauthAccount"]["emailAddress"], "a@x.com")
        # .claude.json 其它字段原样保留
        self.assertEqual(cj["numStartups"], 42)
        self.assertEqual(cj["mcpServers"], {"m": {"command": "x"}})
        self.assertEqual(open(hist).read(), '{"x":1}\n')
        # B 的最新 token 被回写，没有丢
        sec_b = self.cam.Store().load_secret(b_id)
        self.assertEqual(sec_b["claudeAiOauth"]["refreshToken"], "rt-B2")
        # 有备份
        backups = os.listdir(os.path.join(self.tmp, ".claude-accounts", "backups"))
        self.assertEqual(len(backups), 1)
        bk = json.load(open(os.path.join(self.tmp, ".claude-accounts", "backups", backups[0], "credentials.json")))
        self.assertEqual(bk["data"]["claudeAiOauth"]["accessToken"], "at-B2")

        # 切回 B，拿到的是刷新后的 token
        self.assertEqual(self.run_cam("switch", "2"), 0)
        live, _ = self.live()
        self.assertEqual(live["refreshToken"], "rt-B2")
        # 再切到 B：无变化
        self.assertEqual(self.run_cam("switch", "b@x.com"), 0)
        self.assertEqual(len(os.listdir(os.path.join(self.tmp, ".claude-accounts", "backups"))), 2)

        if os.name != "nt":
            mode = os.stat(self.creds_path).st_mode & 0o777
            self.assertEqual(mode, 0o600)
            mode = os.stat(self.cam.Store().secret_path(b_id)).st_mode & 0o777
            self.assertEqual(mode, 0o600)

    def test_unknown_live_account_is_auto_imported_before_switch(self):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import", "--label", "A")
        # 用户没 import 就直接 /login 了另一个账号 C
        self.login("at-C1", "rt-C1", "uuid-C", "c@x.com")
        self.assertEqual(self.run_cam("switch", "A"), 0)
        d = self.data()
        self.assertIn("c@x.com", [a.get("email") for a in d["accounts"]])
        c = [a for a in d["accounts"] if a.get("email") == "c@x.com"][0]
        self.assertEqual(self.cam.Store().load_secret(c["id"])["claudeAiOauth"]["refreshToken"], "rt-C1")

    def test_switch_errors(self):
        self.assertEqual(self.run_cam("switch", "nobody"), 1)
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import")
        self.run_cam("add-web", "--label", "W")
        self.assertEqual(self.run_cam("switch", "W"), 1)
        # 当前账号不能删
        self.assertEqual(self.run_cam("rm", "a@x.com"), 1)
        self.assertEqual(self.run_cam("rm", "W"), 0)

    def test_usage_refresh_active_and_inactive(self):
        past = int((time.time() - 60) * 1000)
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com", expires_ms=past)
        self.run_cam("import", "--label", "A")
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.run_cam("import", "--label", "B")
        MockAnthropic.tokens = {"at-A2": {"u5": 10.0, "u7": 20.0, "email": "a@x.com", "plan": "max"},
                                "at-B1": {"u5": 95.0, "u7": 50.0, "email": "b@x.com", "plan": "pro"}}
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2")}
        self.assertEqual(self.run_cam("usage"), 0)
        d = self.data()
        a, b = d["accounts"]
        self.assertIsNone(a["usage"]["error"], a["usage"])
        self.assertEqual(a["usage"]["five_hour"]["utilization"], 10.0)
        self.assertEqual(b["usage"]["five_hour"]["utilization"], 95.0)
        self.assertEqual(a["plan"], "max")
        self.assertEqual(b["org_name"], "b@x.com's Org")
        # 非当前账号刷新后，轮换的 refresh token 被保存
        self.assertEqual(self.cam.Store().load_secret(a["id"])["claudeAiOauth"]["refreshToken"], "rt-A2")
        # 当前账号从不刷新
        self.assertNotIn(("POST", "/v1/oauth/token", "rt-B1"), MockAnthropic.calls)
        # 切回 A，用的是刷新后的 token
        self.run_cam("switch", "A")
        self.assertEqual(self.live()[0]["accessToken"], "at-A2")

    def test_usage_error_is_kept_per_account(self):
        self.login("at-bad", "rt-bad", "uuid-A", "a@x.com")
        self.run_cam("import")
        self.run_cam("usage")
        u = self.data()["accounts"][0]["usage"]
        self.assertIn("401", u["error"])

    def test_web_account_usage(self):
        self.run_cam("add-web", "--label", "网页号")
        wid = self.data()["accounts"][0]["id"]
        # 没填 sessionKey：友好提示，不报错崩溃
        self.run_cam("usage")
        u = self.data()["accounts"][0]["usage"]
        self.assertIn("sessionKey", u["error"])

        # 填入 sessionKey 后自动拿到额度、邮箱、套餐
        MockAnthropic.web_keys = {"sk-ant-sid-xyz": {"u5": 42.0, "u7": 15.0, "email": "web@x.com",
                                                     "plan": "pro", "org": "我的组织"}}
        self.run_cam("set", wid, "--session-key", "sk-ant-sid-xyz")
        self.run_cam("usage")
        a = self.data()["accounts"][0]
        self.assertIsNone(a["usage"]["error"], a["usage"])
        self.assertEqual(a["usage"]["five_hour"]["utilization"], 42.0)
        self.assertEqual(a["email"], "web@x.com")
        self.assertEqual(a["plan"], "pro")

        # sessionKey 过期：提示重新复制
        MockAnthropic.web_keys = {}
        self.run_cam("usage")
        self.assertIn("过期", self.data()["accounts"][0]["usage"]["error"])

        # Cloudflare 挑战页：提示是网络拦截，不是账号问题
        MockAnthropic.web_keys = {"sk-ant-sid-xyz": {"u5": 1.0, "u7": 1.0, "email": "web@x.com", "plan": "pro"}}
        MockAnthropic.cloudflare = True
        self.run_cam("usage")
        self.assertIn("验证", self.data()["accounts"][0]["usage"]["error"])

    def test_api_account(self):
        self.run_cam("add-api", "--label", "API", "--key", "sk-ant-api-good", "--admin-key", "sk-ant-admin-x")
        self.run_cam("usage")
        u = self.data()["accounts"][0]["usage"]
        self.assertTrue(u["key_valid"])
        self.assertEqual(u["cost_month_usd"], 13.0)  # (1000.5+49.5+250) 美分

    def test_manual_fields_and_renewal(self):
        self.run_cam("add-web", "--label", "W")
        self.assertEqual(self.run_cam("set", "W", "--expires", "2020-01-31", "--auto-renew", "--price", "$20"), 0)
        acc = self.data()["accounts"][0]
        import datetime as dt
        exp = dt.date.fromisoformat(acc["manual"]["expires_at"])
        self.assertGreaterEqual(exp, dt.date.today())
        self.assertLess((exp - dt.date.today()).days, 32)
        self.assertEqual(self.run_cam("set", "W", "--expires", "bad"), 1)

    def test_local_usage_attribution(self):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import", "--label", "A")
        d = self.data()
        a_id = d["accounts"][0]["id"]
        import datetime as dt
        now = dt.datetime.now(dt.timezone.utc)
        d["switch_log"] = [{"id": a_id, "at": (now - dt.timedelta(hours=5)).isoformat()},
                           {"id": "bbbb", "at": (now - dt.timedelta(hours=1)).isoformat()}]
        self.cam.Store().save(d)

        def ev(mid, hours_ago, out):
            return json.dumps({"type": "assistant", "requestId": "r" + mid,
                               "timestamp": (now - dt.timedelta(hours=hours_ago)).isoformat().replace("+00:00", "Z"),
                               "message": {"id": mid, "usage": {"input_tokens": 1, "output_tokens": out,
                                                                "cache_read_input_tokens": 5}}})
        p = os.path.join(self.tmp, ".claude", "projects", "proj1", "x.jsonl")
        with open(p, "w") as f:
            f.write("\n".join([ev("m1", 3, 100), ev("m1", 3, 100), ev("m2", 0.5, 7), ev("m0", 10, 1000),
                               "not json", '{"type":"user"}']) + "\n")
        rep = self.cam.local_usage(self.data(), 7)
        by = {}
        for r in rep["rows"]:
            by.setdefault(r["account_id"], 0)
            by[r["account_id"]] += r["output"]
        self.assertEqual(by.get(a_id), 100)  # 去重
        self.assertEqual(by.get("bbbb"), 7)
        self.assertEqual(by.get("unattributed"), 1000)

    def test_auto_import_on_ls(self):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.assertEqual(self.run_cam("ls"), 0)
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.assertEqual(self.run_cam(), 0)
        d = self.data()
        self.assertEqual(sorted(a["email"] for a in d["accounts"]), ["a@x.com", "b@x.com"])
        # 再运行不会重复导入
        self.run_cam("ls")
        self.assertEqual(len(self.data()["accounts"]), 2)

    def test_renewal_keeps_original_day(self):
        import datetime as dt
        self.run_cam("add-web", "--label", "W")
        self.run_cam("set", "W", "--expires", "2025-01-31", "--auto-renew")
        m = self.data()["accounts"][0]["manual"]
        self.assertEqual(m["renew_day"], 31)
        d = dt.date.fromisoformat(m["expires_at"])
        import calendar
        self.assertEqual(d.day, min(31, calendar.monthrange(d.year, d.month)[1]))

    def _two_accounts(self):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import", "--label", "A")
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.run_cam("import", "--label", "B")
        d = self.data()
        return d["accounts"][0]["id"], d["accounts"][1]["id"]

    def test_rules_dir_and_time(self):
        import datetime as dt
        a_id, b_id = self._two_accounts()
        work = os.path.join(self.tmp, "work")
        os.makedirs(os.path.join(work, "proj", "sub"))
        os.makedirs(os.path.join(self.tmp, "workshop"))
        self.assertEqual(self.run_cam("rule", "add-dir", work, "A"), 0)
        self.assertEqual(self.run_cam("rule", "add-dir", os.path.join(work, "proj"), "B"), 0)
        self.assertEqual(self.run_cam("rule", "add-dir", work, "nobody"), 1)
        d = self.data()
        self.assertEqual(self.cam.match_dir_rule(d, os.path.join(work, "x"))["account"], a_id) if os.path.isdir(os.path.join(work, "x")) else None
        self.assertEqual(self.cam.match_dir_rule(d, work)["account"], a_id)
        self.assertEqual(self.cam.match_dir_rule(d, os.path.join(work, "proj", "sub"))["account"], b_id)  # 最长匹配
        self.assertIsNone(self.cam.match_dir_rule(d, os.path.join(self.tmp, "workshop")))  # 不是前缀误判

        # 时间规则：周一到周五 9-18 用 A；22:00-02:00（跨午夜）用 B
        store = self.cam.Store()
        self.cam.save_rules(store, {"rules": d["rules"] + [
            {"type": "time", "days": "1-5", "start": "09:00", "end": "18:00", "account": a_id},
            {"type": "time", "days": [5], "start": "22:00", "end": "02:00", "account": b_id}]})
        d = self.data()
        mon10 = dt.datetime(2026, 10, 12, 10, 0)  # 周一
        sat10 = dt.datetime(2026, 10, 17, 10, 0)
        fri23 = dt.datetime(2026, 10, 16, 23, 0)
        sat01 = dt.datetime(2026, 10, 17, 1, 0)
        sun01 = dt.datetime(2026, 10, 18, 1, 0)
        self.assertEqual(self.cam.match_time_rule(d, mon10)["account"], a_id)
        self.assertIsNone(self.cam.match_time_rule(d, sat10))
        self.assertEqual(self.cam.match_time_rule(d, fri23)["account"], b_id)
        self.assertEqual(self.cam.match_time_rule(d, sat01)["account"], b_id)
        self.assertIsNone(self.cam.match_time_rule(d, sun01))

        # 当前是 B；进入周一 10 点的时间段 → 切到 A
        res = self.cam.apply_time_rules(store, mon10)
        self.assertTrue(res and res[2])
        self.assertEqual(self.live()[0]["accessToken"], "at-A1")
        # 时间段内手动切回 B，规则不会再改回去
        self.run_cam("switch", "B")
        self.assertIsNone(self.cam.apply_time_rules(store, mon10.replace(hour=11)))
        self.assertEqual(self.live()[0]["accessToken"], "at-B1")
        # 离开时间段再进入，才会再次生效
        self.cam.apply_time_rules(store, sat10)
        res = self.cam.apply_time_rules(store, mon10)
        self.assertEqual(self.live()[0]["accessToken"], "at-A1")

        with self.assertRaises(self.cam.CamError):
            self.cam.save_rules(store, {"rules": [{"type": "time", "days": "8", "start": "1:00", "end": "2:00", "account": a_id}]})
        with self.assertRaises(self.cam.CamError):
            self.cam.save_rules(store, {"rules": [{"type": "time", "days": "1", "start": "25:00", "end": "2:00", "account": a_id}]})
        with self.assertRaises(self.cam.CamError):
            self.cam.save_rules(store, {"alert_threshold": 0})
        self.assertEqual(self.run_cam("rule", "ls"), 0)

    @unittest.skipIf(os.name == "nt", "用 sh 脚本模拟 claude，Windows 上跳过")
    def test_cam_claude_wrapper_args(self):
        a_id, b_id = self._two_accounts()
        bindir = os.path.join(self.tmp, "bin")
        os.makedirs(bindir)
        fake = os.path.join(bindir, "claude")
        out = os.path.join(self.tmp, "args.txt")
        with open(fake, "w") as f:
            f.write(f"#!/bin/sh\nprintf '%s\\n' \"$@\" > {out}\n")
        os.chmod(fake, 0o755)
        work = os.path.join(self.tmp, "work")
        os.makedirs(work)
        self.run_cam("rule", "add-dir", work, "A")
        import subprocess
        env = dict(os.environ, PATH=bindir + os.pathsep + os.environ.get("PATH", ""))
        r = subprocess.run([sys.executable, os.path.join(HERE, "cam.py"), "claude", "--resume", "-p", "hi there"],
                           cwd=work, env=env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(open(out).read().splitlines(), ["--resume", "-p", "hi there"])
        self.assertEqual(self.live()[0]["accessToken"], "at-A1")

    def test_launch_risk_prompt_tiers(self):
        a_id, b_id = self._two_accounts()
        store = self.cam.Store()
        asked = []

        def ask_with(answer):
            def ask(prompt):
                asked.append(prompt)
                return answer
            return ask

        # 100 分：不问，直接启动
        self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with(""), interactive=True))
        self.assertEqual(asked, [])
        self.assertEqual(self.data()["region"]["loc"], "US")  # 顺手做了一次检测并缓存
        # 出口不在名单：不看分数，要输入“继续”
        MockAnthropic.trace_loc = "CN"
        self.cam.check_region(store, max_age=0)
        self.assertFalse(self.cam.risk_prompt_before_launch(store, ask=ask_with(""), interactive=True))
        self.assertFalse(self.cam.risk_prompt_before_launch(store, ask=ask_with("y"), interactive=True))
        self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with("继续"), interactive=True))
        self.assertIn("输入「继续」", asked[-1])
        # 非交互：只打印，照常启动
        self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with(""), interactive=False))
        # 关掉提醒 / 环境变量临时关掉
        self.assertEqual(self.run_cam("risk", "--off"), 0)
        self.assertFalse(self.cam.risk_prefs(self.data())["risk_prompt"])
        self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with(""), interactive=True))
        self.assertEqual(self.run_cam("risk", "--on"), 0)
        os.environ["CAM_NO_RISK_PROMPT"] = "1"
        try:
            self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with(""), interactive=True))
        finally:
            os.environ.pop("CAM_NO_RISK_PROMPT")
        # 出口回到美国但时区指向香港：75 分 → 60–89 档只提醒不问；把 confirm 阈值调到 80 后变成要问 y/N
        MockAnthropic.trace_loc = "US"
        os.environ["CAM_TEST_TZ"] = "Asia/Hong_Kong=HK"
        self.cam.check_region(store, max_age=0)  # 时区随出口一起缓存，改了要重新检测
        try:
            sc = self.cam.risk_score(self.cam.risk_items(self.data()))
            self.assertEqual(sc["score"], 40)  # 时区指向不支持地区 25 + 出口变化 15 + 当前账号两地使用 20
            prefs = self.cam.risk_prefs(self.data())
            self.assertEqual(self.cam.launch_tier(60, "supported", prefs), "remind")
            self.assertEqual(self.cam.launch_tier(59, "supported", prefs), "confirm")
            self.assertEqual(self.cam.launch_tier(29, "supported", prefs), "strong")
            self.assertEqual(self.cam.launch_tier(100, "unsupported", prefs), "strong")
            self.assertEqual(self.cam.launch_tier(95, "supported", prefs), "ok")
            # 40 分：默认 60 以下问 y/N，默认不继续
            asked.clear()
            self.assertFalse(self.cam.risk_prompt_before_launch(store, ask=ask_with("n"), interactive=True))
            self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with("Y"), interactive=True))
            self.assertIn("[y/N]", asked[-1])
            # 阈值调到 confirm 30 / strong 20 后，40 分只提醒不问
            self.assertEqual(self.run_cam("risk", "--confirm", "30", "--strong", "20"), 0)
            asked.clear()
            self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with(""), interactive=True))
            self.assertEqual(asked, [])
            self.assertEqual(self.run_cam("risk", "--confirm", "70", "--strong", "80"), 1)  # strong 不能高于 confirm
            self.assertEqual(self.run_cam("risk"), 0)
        finally:
            os.environ["CAM_TEST_TZ"] = "America/New_York=US"

    def test_menubar_plugin(self):
        os.environ["CAM_MENU_NO_NOTIFY"] = "1"
        os.environ["CAM_MENU_REFRESH_MIN"] = "0"
        sys.path.insert(0, os.path.join(HERE, "..", "extras", "swiftbar"))
        import cam_menu
        cam_menu = importlib.reload(cam_menu)
        cam_menu.cam = self.cam
        store = self.cam.Store()
        lines = []
        cam_menu.render(store, cam_menu.background_work(store), out=lines.append)
        self.assertTrue(lines[0].startswith("Claude ⚠"))
        # 登录后插件运行一次就自动保存，标题显示账号
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.login_then = None
        lines = []
        cam_menu.render(store, cam_menu.background_work(store), out=lines.append)
        self.assertIn("a@x.com", lines[0])
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        lines = []
        cam_menu.render(store, cam_menu.background_work(store), out=lines.append)
        switch_lines = [l for l in lines if 'param2="switch"' in l]
        self.assertTrue(switch_lines, lines)
        self.assertTrue(all("|" in l for l in lines if l != "---"))

    # ---- 锁

    def test_lock_excludes_threads_and_is_released_when_process_dies(self):
        store = self.cam.Store()
        with store.lock():
            other = self.cam.FileLock(os.path.join(store.root, "accounts.lock"))
            self.assertFalse(other.try_acquire())  # 同一进程的另一次加锁也要等
        self.assertTrue(other.try_acquire())
        other.release()
        # 子进程拿着锁时被强杀：锁由系统回收，不用等 60 秒
        code = ("import sys,time; sys.path.insert(0, sys.argv[1]); import cam; "
                "l = cam.Store().lock(); l.__enter__(); print('locked', flush=True); time.sleep(60)")
        p = subprocess.Popen([sys.executable, "-c", code, HERE], stdout=subprocess.PIPE, text=True)
        try:
            self.assertEqual(p.stdout.readline().strip(), "locked")
            self.assertFalse(other.try_acquire())
        finally:
            p.kill()
            p.wait()
            p.stdout.close()
        t0 = time.monotonic()
        with store.lock():
            pass
        self.assertLess(time.monotonic() - t0, 2)

    def test_background_work_has_single_owner(self):
        store = self.cam.Store()
        lease = store.background_lock()
        self.assertTrue(lease.try_acquire())
        try:
            self.assertFalse(store.background_lock().try_acquire())
            os.environ["CAM_MENU_NO_NOTIFY"] = "1"
            sys.path.insert(0, os.path.join(HERE, "..", "extras", "swiftbar"))
            import cam_menu
            cam_menu = importlib.reload(cam_menu)
            cam_menu.cam = self.cam
            self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
            st = cam_menu.background_work(store)
            self.assertEqual(st.get("bg_owner"), "other")
            self.assertEqual(self.data()["accounts"], [])  # 后台任务归别人，这里没动数据
        finally:
            lease.release()
        self.assertIsNone(cam_menu.background_work(store).get("bg_owner"))
        self.assertEqual(len(self.data()["accounts"]), 1)

    # ---- 账号识别：凭据和 oauthAccount 对不上时不能把 token 存错账号

    def _switched_to_b(self):
        a_id, b_id = self._two_accounts()
        self.run_cam("switch", "A")
        self.run_cam("switch", "B")
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B1")
        return a_id, b_id

    def test_old_session_writes_back_unchanged_token(self):
        a_id, b_id = self._switched_to_b()
        # 切换前开着的 claude 会话把 A 的凭据原样写回了；~/.claude.json 仍然写着 B
        self.write_creds("at-A1", "rt-A1")
        self.run_cam("ls")
        store, d = self.cam.Store(), self.data()
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual(store.load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A1")
        self.assertEqual(store.load_secret(a_id)["oauthAccount"]["emailAddress"], "a@x.com")
        self.assertEqual(d["active"]["claude_code"], a_id)  # 实际生效的是 A
        self.assertIn("“A”", d["live_mismatch"]["message"])
        self.assertIn("b@x.com", d["live_mismatch"]["message"])
        b = [a for a in d["accounts"] if a["id"] == b_id][0]
        self.assertEqual(b["email"], "b@x.com")
        # 重新切到 B 后恢复一致，提醒消失
        self.run_cam("switch", "B")
        self.assertNotIn("live_mismatch", self.data())
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B1")

    def test_old_session_writes_back_refreshed_token(self):
        a_id, b_id = self._switched_to_b()
        # 旧会话刷新了 A 的 token 后写回：指纹对不上，靠 profile 接口认出是 A
        MockAnthropic.tokens = {"at-A2": {"u5": 1.0, "u7": 1.0, "email": "a@x.com", "plan": "max"}}
        self.write_creds("at-A2", "rt-A2")
        self.run_cam("ls")
        store, d = self.cam.Store(), self.data()
        self.assertEqual(store.load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A2")
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual(d["active"]["claude_code"], a_id)
        self.assertIn("live_mismatch", d)

    @staticmethod
    def _expired_ms():
        return int((time.time() - 60) * 1000)

    def test_unverifiable_login_is_parked_not_filed(self):
        a_id, b_id = self._switched_to_b()
        # 旧会话刷新了 A 的 token 后写回，等到 cam 运行时 access token 已过期：核对不了，不能信 ~/.claude.json 里的 B
        self.write_creds("at-A2", "rt-A2", expires_ms=self._expired_ms())
        MockAnthropic.calls = []
        self.run_cam("ls")
        self.assertFalse([c for c in MockAnthropic.calls if c[1] == "/api/oauth/profile"])  # 过期了不用白问
        store, d = self.cam.Store(), self.data()
        self.assertEqual(store.load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A1")
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")  # 没被 A 的 token 覆盖
        pend = store.load_secret(self.cam.PENDING_ID)
        self.assertEqual(pend["claudeAiOauth"]["refreshToken"], "rt-A2")
        self.assertEqual(pend["candidate"], b_id)
        self.assertEqual(d["active"]["claude_code"], b_id)  # 显示上先按 ~/.claude.json 猜
        self.assertIn("无法确认", self.cam.public_state(store, d)["warning"])
        with self.assertRaises(self.cam.CamError):
            self.cam.import_current(store, label="X")  # 认不出是谁的，不能导入
        # 旧会话又续了一次 access token（refresh token 没换）：现在能核对，归到 A 名下，B 不动，暂存清掉
        MockAnthropic.tokens = {"at-A3": {"u5": 1.0, "u7": 1.0, "email": "a@x.com", "plan": "max"}}
        self.write_creds("at-A3", "rt-A2")
        self.run_cam("ls")
        store, d = self.cam.Store(), self.data()
        self.assertEqual(store.load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A2")
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual(store.load_secret(self.cam.PENDING_ID), {})
        self.assertEqual(d["active"]["claude_code"], a_id)
        self.assertIn("live_mismatch", d)
        self.assertNotIn("live_unverified", d)

    def test_offline_login_is_parked_until_online(self):
        a_id, b_id = self._switched_to_b()
        # B 自己换了 token（正常轮换），但这会儿没网
        self.write_creds("at-B2", "rt-B2")
        base = self.cam.API_BASE
        self.cam.API_BASE = "http://127.0.0.1:1"
        try:
            self.run_cam("ls")
        finally:
            self.cam.API_BASE = base
        store = self.cam.Store()
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual(store.load_secret(self.cam.PENDING_ID)["candidate"], b_id)
        self.assertIn("live_unverified", self.data())
        # 网络恢复后自动归档到 B，提醒消失
        MockAnthropic.tokens = {"at-B2": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}
        self.run_cam("ls")
        store, d = self.cam.Store(), self.data()
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B2")
        self.assertEqual(store.load_secret(self.cam.PENDING_ID), {})
        self.assertNotIn("live_unverified", d)
        self.assertNotIn("live_mismatch", d)

    def test_switch_adopts_parked_login_after_refresh(self):
        a_id, b_id = self._switched_to_b()
        # B 换过 token，但 cam 运行时 access token 已过期：先暂存；随后切走，B 换过的 token 只剩暂存这一份
        self.write_creds("at-B2", "rt-B2", expires_ms=self._expired_ms())
        self.run_cam("switch", "A")
        store = self.cam.Store()
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual(self.live()[0]["refreshToken"], "rt-A1")
        # 切回 B：用暂存的 refresh token 换新并核对，确认是 B 的才存进 B，切换就用上最新的
        MockAnthropic.refresh_map = {"rt-B2": ("at-B3", "rt-B3")}
        MockAnthropic.tokens = {"at-B3": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}
        self.run_cam("switch", "B")
        store, d = self.cam.Store(), self.data()
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B3")
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B3")
        self.assertEqual(store.load_secret(self.cam.PENDING_ID), {})
        self.assertNotIn("live_unverified", d)

    def test_switch_files_parked_login_under_its_real_owner(self):
        a_id, b_id = self._switched_to_b()
        # 暂存的其实是旧会话写回的 A 的 token，但 ~/.claude.json 说是 B
        self.write_creds("at-A2", "rt-A2", expires_ms=self._expired_ms())
        self.run_cam("switch", "A")
        MockAnthropic.refresh_map = {"rt-A2": ("at-A3", "rt-A3")}
        MockAnthropic.tokens = {"at-A3": {"u5": 1.0, "u7": 1.0, "email": "a@x.com", "plan": "max"}}
        self.run_cam("switch", "B")
        store = self.cam.Store()
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B1")  # B 用的是自己保存的
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual(store.load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A3")  # 换新后归到 A
        self.assertEqual(store.load_secret(self.cam.PENDING_ID), {})

    # ---- 等会话结束再切换

    def test_switch_waits_for_running_sessions(self):
        a_id, b_id = self._two_accounts()  # 当前是 B
        calls = {"n": 0}

        def procs():
            calls["n"] += 1
            return [(4242, "claude")] if calls["n"] <= 2 else []

        self.cam.running_claude_processes = procs
        self.cam.WAIT_POLL_SECS = 0
        self.addCleanup(setattr, self.cam, "WAIT_POLL_SECS", 5)
        self.run_cam("switch", "A", "--wait")
        d = self.data()
        self.assertEqual(d["active"]["claude_code"], a_id)
        self.assertNotIn("pending_switch", d)
        self.assertEqual(self.live()[0]["refreshToken"], "rt-A1")

    def test_api_switch_choice_wait_and_background_apply(self):
        a_id, b_id = self._two_accounts()
        store = self.cam.Store()
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        r = self.cam.api_switch(store, a_id)
        self.assertTrue(r["choice_required"])
        self.assertEqual(r["running_claude"], 1)
        self.assertNotIn("pending_switch", self.data())
        r = self.cam.api_switch(store, a_id, "wait")
        self.assertTrue(r["deferred"])
        st = self.cam.public_state(store, self.data())
        self.assertEqual(st["pending_switch"]["account"], a_id)
        self.assertIn("“A”", st["pending_switch"]["message"])
        self.assertIsNone(self.cam.apply_pending_switch(store))  # 会话还在，不切
        self.assertEqual(self.data()["active"]["claude_code"], b_id)
        self.cam.running_claude_processes = lambda: []
        res = self.cam.apply_pending_switch(store)
        self.assertEqual((res[0]["id"], res[2]), (a_id, "manual"))
        d = self.data()
        self.assertEqual(d["active"]["claude_code"], a_id)
        self.assertNotIn("pending_switch", d)
        self.assertIsNone(self.cam.public_state(store, d)["pending_switch"])
        # mode=now 直接切；取消登记
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        r = self.cam.api_switch(store, b_id, "now")
        self.assertTrue(r["changed"])
        self.assertEqual(self.data()["active"]["claude_code"], b_id)
        self.cam.api_switch(store, a_id, "wait")
        self.assertTrue(self.cam.cancel_pending_switch(store))
        self.assertNotIn("pending_switch", self.data())
        self.assertFalse(self.cam.cancel_pending_switch(store))

    def test_manual_switch_clears_pending_switch(self):
        a_id, b_id = self._two_accounts()
        store = self.cam.Store()
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.cam.api_switch(store, a_id, "wait")
        self.assertIn("pending_switch", self.data())
        self.run_cam("switch", "A", "-y")  # 现在就切
        d = self.data()
        self.assertEqual(d["active"]["claude_code"], a_id)
        self.assertNotIn("pending_switch", d)
        self.run_cam("switch", "--cancel-wait")  # 没有等待中的也不报错

    def test_time_rule_waits_for_session_and_expires_with_window(self):
        import datetime as dt
        a_id, b_id = self._two_accounts()  # 当前是 B
        store = self.cam.Store()
        self.cam.save_rules(store, {"rules": [
            {"type": "time", "days": "1-5", "start": "09:00", "end": "18:00", "account": a_id}]})
        mon10 = dt.datetime(2026, 10, 19, 10, 0)  # 周一
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.assertIsNone(self.cam.apply_time_rules(store, mon10))  # 有会话在跑：先登记
        d = self.data()
        self.assertEqual((d["pending_switch"]["account"], d["pending_switch"]["reason"]), (a_id, "rule"))
        self.assertEqual(d["active"]["claude_code"], b_id)
        self.assertIsNone(self.cam.apply_time_rules(store, mon10.replace(hour=11)))  # 同一时间段不重复
        self.assertIsNone(self.cam.apply_pending_switch(store, mon10.replace(hour=11)))  # 会话还在
        # 时间段过了还没切成：作废
        self.cam.running_claude_processes = lambda: []
        self.assertIsNone(self.cam.apply_pending_switch(store, mon10.replace(hour=19)))
        d = self.data()
        self.assertNotIn("pending_switch", d)
        self.assertEqual(d["active"]["claude_code"], b_id)
        # 第二天再进入时间段：会话退出后切过去
        self.cam.apply_time_rules(store, mon10.replace(hour=20))  # 离开时间段
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        tue10 = mon10 + dt.timedelta(days=1)
        self.assertIsNone(self.cam.apply_time_rules(store, tue10))
        self.assertIn("pending_switch", self.data())
        self.cam.running_claude_processes = lambda: []
        res = self.cam.apply_pending_switch(store, tue10)
        self.assertEqual((res[0]["id"], res[2]), (a_id, "rule"))
        self.assertEqual(self.data()["active"]["claude_code"], a_id)

    # ---- 服务检查

    def test_service_check(self):
        a_id, b_id = self._two_accounts()  # 当前登录 B
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}
        items = self.cam.service_check(self.cam.Store())
        text = "\n".join(i["text"] for i in items)
        self.assertEqual(items[0]["level"], "ok")
        self.assertIn("Claude 服务正常", items[0]["text"])
        self.assertIn("出口地区 US（美国） 在 Anthropic 支持名单内", text)
        self.assertIn("服务正常，当前登录可用", text)
        self.assertIn("可达", text)
        self.assertTrue(all(set(i) == {"kind", "level", "text", "advice"} for i in items[1:]))
        self.assertEqual(set(items[0]), {"kind", "level", "text", "advice", "score", "grade"})
        # 不在支持名单内：提醒并给建议，总体结论跟着变
        self.assertEqual((items[0]["score"], items[0]["grade"]), (100, "low"))
        self.assertIn("100/100，风险低，可以使用", items[0]["text"])
        MockAnthropic.trace_loc = "CN"
        items = self.cam.service_check(self.cam.Store())
        # 不在名单 60 + 出口从 US 变到 CN 15 + 当前账号两地使用 20 + 时区指向美国 5 → 0 分，风险高
        self.assertEqual(items[0]["level"], "bad")
        self.assertEqual((items[0]["score"], items[0]["grade"]), (0, "high"))
        self.assertIn("0/100，风险高，不建议在当前环境使用", items[0]["text"])
        self.assertIn("出口地区 CN", items[0]["text"])
        reg = next(i for i in items if i["text"].startswith("出口地区"))
        self.assertEqual(reg["level"], "warn")
        self.assertIn("CN（中国大陆） 不在 Anthropic 支持名单内", reg["text"])
        self.assertIn("封禁", reg["advice"])
        # 属地按所属国家看待
        MockAnthropic.trace_loc = "PR"
        items = self.cam.service_check(self.cam.Store())
        self.assertEqual(next(i for i in items if i["text"].startswith("出口地区"))["level"], "ok")
        # 没登录：只看可达性
        os.remove(self.creds_path)
        MockAnthropic.trace_loc = "US"
        text = "\n".join(i["text"] for i in self.cam.service_check(self.cam.Store()))
        self.assertIn("服务可达（未登录", text)
        # 被人机验证拦截：有建议说明只影响网页账号
        MockAnthropic.cloudflare = True
        items = self.cam.service_check(None)
        cf = next(i for i in items if "人机验证" in i["text"])
        self.assertEqual(cf["level"], "warn")
        self.assertIn("不影响 Claude Code 切换", cf["advice"])

    def test_risk_items_and_reminders(self):
        a_id, b_id = self._two_accounts()  # 当前登录 B
        store = self.cam.Store()
        self.cam.check_region(store)
        # 没有风险时只有“账号数”这条 info
        st = self.cam.public_state(store, self.data())
        risks = st["risks"]
        self.assertEqual([r["kind"] for r in risks], ["accounts"])
        self.assertEqual((st["risk_score"]["score"], st["risk_score"]["grade"]), (100, "low"))
        self.assertTrue(all(set(r) == {"kind", "level", "text", "advice"} for r in risks))
        # 出口从 US 变到 HK：不在名单 + 出口变化；当前账号 B 记了两个地区
        MockAnthropic.trace_loc = "HK"
        self.cam.check_region(store, max_age=0)
        d = self.data()
        kinds = {r["kind"]: r for r in self.cam.risk_items(d)}
        self.assertEqual(kinds["region"]["level"], "warn")
        self.assertIn("HK（香港） 不在 Anthropic 支持名单内", kinds["region"]["text"])
        self.assertEqual(kinds["region_change"]["level"], "warn")
        self.assertIn("US（美国） → HK（香港）", kinds["region_change"]["text"])
        self.assertIn("刚刚", kinds["region_change"]["text"])
        self.assertIn("2 个地区使用过：HK（香港）、US（美国）", kinds["account_regions"]["text"])
        self.assertEqual(d["region"]["prev_loc"], "US")
        sc = self.cam.public_state(store, d)["risk_score"]  # 不在名单 60 + 出口变化 15 + 多地区 20 + 时区指向美国 5 → 0 分
        self.assertEqual((sc["score"], sc["grade"]), (0, "high"))
        self.assertIn("不建议", sc["label"])
        # 变化提示在 24 小时内保留，再次检测到同一地区不会丢
        self.cam.check_region(store, max_age=0)
        self.assertEqual(self.data()["region"]["prev_loc"], "US")
        # 时区指向不支持地区而出口在支持地区：warn；指向支持地区：info；一致：不提
        MockAnthropic.trace_loc = "SG"
        self.cam.check_region(store, max_age=0)
        def risks_with(**env):  # 本机设置随出口一起缓存，改了环境变量要重新检测一次
            for k, v in env.items():
                os.environ[k] = v
            self.cam.check_region(store, max_age=0)
            return {r["kind"]: r for r in self.cam.risk_items(self.data())}

        try:
            tz = risks_with(CAM_TEST_TZ="Asia/Hong_Kong=HK")["timezone"]
            self.assertEqual(tz["level"], "warn")
            self.assertIn("Asia/Hong_Kong 指向 HK（香港），与出口地区 SG（新加坡） 不一致", tz["text"])
            self.assertEqual(risks_with(CAM_TEST_TZ="Asia/Tokyo=JP")["timezone"]["level"], "info")
            self.assertNotIn("timezone", risks_with(CAM_TEST_TZ="Asia/Singapore=SG,MY"))
        finally:
            os.environ["CAM_TEST_TZ"] = "America/New_York=US"
        # 系统语言指向不支持地区：warn；指向支持地区：info
        try:
            lc = risks_with(CAM_TEST_LOCALE="zh_CN.UTF-8")["locale"]
            self.assertEqual(lc["level"], "warn")
            self.assertIn("zh_CN 指向 CN（中国大陆），与出口地区 SG（新加坡） 不一致", lc["text"])
            self.assertEqual(risks_with(CAM_TEST_LOCALE="ja-JP")["locale"]["level"], "info")
        finally:
            os.environ["CAM_TEST_LOCALE"] = "en_US"
        # DNS 解析器：局域网和任播的不提；不支持地区的公共 DNS 与出口不一致 → warn；和出口同国不提
        try:
            self.assertNotIn("dns_resolver", risks_with(CAM_TEST_DNS="192.168.1.1,8.8.8.8"))
            d = risks_with(CAM_TEST_DNS="192.168.1.1,223.5.5.5")["dns_resolver"]
            self.assertEqual(d["level"], "warn")
            self.assertIn("223.5.5.5（阿里 DNS，CN（中国大陆））与出口地区 SG（新加坡） 不一致", d["text"])
            self.assertEqual(risks_with(CAM_TEST_DNS="168.95.1.1")["dns_resolver"]["level"], "info")
        finally:
            os.environ["CAM_TEST_DNS"] = "192.168.1.1"
        self.cam.check_region(store, max_age=0)
        # IP 类型：数据库标记为代理 → warn；机房 → warn；手机 → 不提；IPv6 出口和 IPv4 不同 → warn；注册国不同 → info
        MockAnthropic.ipapi_type = "proxy"
        MockAnthropic.trace6_loc = "HK"
        MockAnthropic.rdap_country = "CA"
        self.cam.check_region(store, max_age=0)
        kinds = {r["kind"]: r for r in self.cam.risk_items(self.data())}
        self.assertEqual(kinds["proxy_ip"]["level"], "warn")
        self.assertIn("被数据库标记为代理 / VPN 出口", kinds["proxy_ip"]["text"])
        self.assertEqual(kinds["ipv6_mismatch"]["level"], "warn")
        self.assertIn("IPv4 出口在 SG（新加坡），IPv6 出口在 HK（香港）（2001:db8::1）", kinds["ipv6_mismatch"]["text"])
        self.assertEqual(kinds["ip_origin"]["level"], "info")
        self.assertIn("注册在 CA（加拿大），定位在 SG（新加坡）（广播 IP）", kinds["ip_origin"]["text"])
        self.assertEqual(self.data()["region"]["ip_type"], "proxy")
        MockAnthropic.ipapi_type = "mobile"
        MockAnthropic.trace6_loc = None
        MockAnthropic.rdap_country = "SG"
        self.cam.check_region(store, max_age=0)
        kinds = {r["kind"]: r for r in self.cam.risk_items(self.data())}
        self.assertFalse({"proxy_ip", "isp", "ipv6_mismatch", "ip_origin"} & set(kinds))
        items = self.cam.service_check(store)
        self.assertIn("IP 类型：手机网络", next(i for i in items if i["kind"] == "isp")["text"])
        self.assertIn("IPv6：不可用", next(i for i in items if i["kind"] == "ipv6")["text"])
        self.assertEqual(next(i for i in items if i["kind"] == "ip_origin")["level"], "ok")
        self.assertIn("192.168.1.1（局域网 / 本机）", next(i for i in items if i["kind"] == "dns_servers")["text"])
        MockAnthropic.ipapi_type = "residential"
        # 机房出口（数据库标记）+ 两个接口看到的国家不一致；数据源没有标记时按运营商名判断
        MockAnthropic.meta_org = "DigitalOcean, LLC"
        MockAnthropic.meta_country = "CA"
        MockAnthropic.ipapi_type = "hosting"
        self.cam.check_region(store, max_age=0)
        kinds = {r["kind"]: r for r in self.cam.risk_items(self.data())}
        self.assertEqual(kinds["isp"]["level"], "warn")
        self.assertIn("DigitalOcean, LLC（AS7922），是机房 / 云服务器", kinds["isp"]["text"])
        MockAnthropic.ipapi_type = None  # ip-api 查询失败 → 退到 Cloudflare meta（没有标记）→ 关键字判断
        self.cam.check_region(store, max_age=0)
        self.assertIsNone(self.data()["region"]["ip_type"])
        self.assertEqual({r["kind"]: r for r in self.cam.risk_items(self.data())}["isp"]["level"], "warn")
        MockAnthropic.ipapi_type = "residential"
        self.assertEqual(kinds["region_mismatch"]["level"], "warn")
        self.assertIn("trace 接口看到 SG（新加坡），meta 接口看到 CA（加拿大）", kinds["region_mismatch"]["text"])
        MockAnthropic.meta_org = "Comcast Cable Communications, LLC"
        MockAnthropic.meta_country = None
        # WARP、时钟偏差、官方状态页
        MockAnthropic.trace_warp = True
        MockAnthropic.trace_ts_offset = -600
        MockAnthropic.status_indicator = "major"
        self.cam.check_region(store, max_age=0)
        d = self.data()
        kinds = {r["kind"]: r for r in self.cam.risk_items(d)}
        self.assertEqual(kinds["warp"]["level"], "info")
        self.assertEqual(kinds["clock"]["level"], "warn")
        self.assertIn("+600 秒", kinds["clock"]["text"])
        self.assertEqual(kinds["status"]["level"], "bad")
        self.assertIn("Partial System Outage", kinds["status"]["text"])
        self.assertEqual(d["status_page"]["indicator"], "major")
        # 服务检查把风险项都列出来，总体结论跟着最严重的走；出口地区只出现一次
        items = self.cam.service_check(store)
        self.assertEqual(items[0]["level"], "bad")
        self.assertIn("Anthropic 官方状态页", items[0]["text"])
        self.assertEqual(items[0]["grade"], "high")
        self.assertLess(items[0]["score"], 70)
        # 评分单独算：中档
        sc = self.cam.risk_score([{"kind": "timezone", "level": "warn"}, {"kind": "clock", "level": "warn"}])
        self.assertEqual((sc["score"], sc["grade"]), (65, "high"))
        sc = self.cam.risk_score([{"kind": "region_change", "level": "warn"}, {"kind": "warp", "level": "info"}])
        self.assertEqual((sc["score"], sc["grade"]), (80, "medium"))
        self.assertEqual(self.cam.risk_score([{"kind": "new_kind", "level": "warn"}])["score"], 90)
        kinds = [i["kind"] for i in items]
        self.assertEqual(kinds.count("region"), 1)
        for k in ("summary", "dns", "region", "isp", "ip_origin", "ipv6", "dns_servers", "api", "web", "proxy", "vpn",
                  "region_change", "warp", "clock", "status", "accounts", "device", "scope"):
            self.assertIn(k, kinds)
        reg = next(i for i in items if i["kind"] == "region")
        self.assertIn("San Jose, California", reg["text"])
        self.assertIn("Comcast", next(i for i in items if i["kind"] == "isp")["text"])
        self.assertEqual(kinds[-1], "scope")
        self.assertIn("不等于一定合规", items[-1]["advice"])
        # 状态恢复后缓存里的状态页记录也清掉
        MockAnthropic.status_indicator = "none"
        MockAnthropic.trace_warp = False
        MockAnthropic.trace_ts_offset = 0
        self.cam.check_region(store, max_age=0)
        self.assertFalse([r for r in self.cam.risk_items(self.data()) if r["kind"] in ("status", "warp", "clock")])
        # 切换接口带上提醒，但照样切；切过去的账号也记了一笔当前地区
        res = self.cam.api_switch(store, "A")
        self.assertTrue(res["changed"])
        self.assertIn("region_change", [r["kind"] for r in res["risks"]])
        self.assertEqual(self.data()["active"]["claude_code"], a_id)
        a = next(x for x in self.data()["accounts"] if x["id"] == a_id)
        self.assertEqual(a["regions"][0]["loc"], "SG")
        # 运营商信息的几种来源格式
        self.assertEqual(self.cam.parse_exit_meta({"org": "AS7922 Comcast Cable", "city": "Denver", "country": "us"}),
                         {"asn": 7922, "org": "Comcast Cable", "city": "Denver", "meta_loc": "US", "ip_type": None, "reg_loc": None})
        self.assertEqual(self.cam.parse_exit_meta({"asn": 14061, "asOrganization": "DigitalOcean, LLC", "country": "SG"})["asn"], 14061)
        self.assertIsNone(self.cam.parse_exit_meta({"ip": "1.2.3.4"}))
        self.assertIsNone(self.cam.parse_exit_meta({"status": "fail", "message": "quota"}))
        m = self.cam.parse_exit_meta({"status": "success", "countryCode": "HK", "city": "Hong Kong", "regionName": "",
                                      "isp": "Choopa", "as": "AS20473 The Constant Company", "hosting": True, "proxy": False, "mobile": False})
        self.assertEqual((m["asn"], m["org"], m["meta_loc"], m["ip_type"], m["city"]), (20473, "Choopa", "HK", "hosting", "Hong Kong"))
        m = self.cam.parse_exit_meta({"ip": "1.2.3.4", "is_tor": True, "is_vpn": False, "is_datacenter": True,
                                      "asn": {"asn": 4134, "org": "Chinanet", "country": "cn"}, "company": {"name": "Chinanet", "type": "isp"},
                                      "location": {"country_code": "SG", "city": "Singapore", "state": "Central"}})
        self.assertEqual((m["asn"], m["org"], m["meta_loc"], m["ip_type"], m["reg_loc"], m["city"]),
                         (4134, "Chinanet", "SG", "tor", "CN", "Singapore, Central"))
        # DNS 服务器分类
        os.environ["CAM_TEST_DNS"] = "10.0.0.1,8.8.8.8,223.5.5.5,fe80::1%en0,1.2.3.4"
        try:
            dns = self.cam.system_dns_servers()
        finally:
            os.environ["CAM_TEST_DNS"] = "192.168.1.1"
        self.assertEqual([(d["ip"], d["private"], d["name"], d["country"]) for d in dns],
                         [("10.0.0.1", True, None, None), ("8.8.8.8", False, "Google", None), ("223.5.5.5", False, "阿里 DNS", "CN"),
                          ("fe80::1", True, None, None), ("1.2.3.4", False, None, None)])
        # VPN / 隧道判断
        self.assertTrue(self.cam.tunnel_kind("utun4"))
        self.assertTrue(self.cam.tunnel_kind("Ethernet 2", "TAP-Windows Adapter V9"))
        self.assertIsNone(self.cam.tunnel_kind("en0", "Apple Ethernet"))
        os.environ["CAM_TEST_ROUTE_IF"] = "wg0"
        try:
            v = next(i for i in self.cam.service_check(None) if i["kind"] == "vpn")
            self.assertEqual(v["level"], "info")
            self.assertIn("wg0", v["text"])
        finally:
            os.environ["CAM_TEST_ROUTE_IF"] = ""

    def test_region_cached_for_display(self):
        self._two_accounts()
        store = self.cam.Store()
        self.assertIsNone(self.cam.public_state(store, self.data())["region"]["status"])
        self.cam.check_region(store)
        reg = self.cam.public_state(store, self.data())["region"]
        self.assertEqual((reg["loc"], reg["status"]), ("US", "supported"))
        self.assertIn("在 Anthropic 支持名单内", reg["message"])
        # 缓存期内不再联网
        MockAnthropic.trace_loc = "CN"
        MockAnthropic.calls = []
        self.cam.check_region(store)
        self.assertFalse([c for c in MockAnthropic.calls if c[1] == "/cdn-cgi/trace"])
        self.assertEqual(self.cam.public_state(store, self.data())["region"]["status"], "supported")
        # 过期后重新检测：不在名单内只是提示，账号和切换都不受影响
        self.cam.check_region(store, max_age=0)
        reg = self.cam.public_state(store, self.data())["region"]
        self.assertEqual(reg["status"], "unsupported")
        self.assertIn("建议", reg["advice"])
        self.assertTrue(self.cam.switch_code_account(store, "A")[2])

    def test_new_login_before_claude_json_updated(self):
        a_id, b_id = self._two_accounts()
        # 新登录 C：凭据已写入，~/.claude.json 还没来得及更新
        MockAnthropic.tokens = {"at-C1": {"u5": 1.0, "u7": 1.0, "email": "c@x.com", "plan": "pro"}}
        self.write_creds("at-C1", "rt-C1")
        self.run_cam("ls")
        store, d = self.cam.Store(), self.data()
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        c = [a for a in d["accounts"] if a.get("email") == "c@x.com"]
        self.assertEqual(len(c), 1)
        self.assertEqual(store.load_secret(c[0]["id"])["claudeAiOauth"]["refreshToken"], "rt-C1")

    def test_usage_refuses_credentials_of_another_account(self):
        a_id, b_id = self._two_accounts()
        # A 保存的 token 实际上是 B 的（以前版本可能存错过）
        store = self.cam.Store()
        sec = store.load_secret(a_id)
        sec["claudeAiOauth"]["accessToken"] = "at-B1"
        store.save_secret(a_id, sec)
        MockAnthropic.tokens = {"at-B1": {"u5": 5.0, "u7": 5.0, "email": "b@x.com", "plan": "pro"}}
        self.run_cam("usage")
        a = [x for x in self.data()["accounts"] if x["id"] == a_id][0]
        self.assertIn("b@x.com", a["usage"]["error"])
        self.assertEqual(a["email"], "a@x.com")

    # ---- token 刷新

    def test_background_refresh_never_rotates_tokens(self):
        past = int((time.time() - 60) * 1000)
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com", expires_ms=past)
        self.run_cam("import", "--label", "A")
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.run_cam("import", "--label", "B")
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2")}
        d = self.cam.refresh_usage(self.cam.Store(), allow_token_refresh=False)
        self.assertNotIn(("POST", "/v1/oauth/token", "rt-A1"), MockAnthropic.calls)
        self.assertIn("过期", d["accounts"][0]["usage"]["error"])

    def test_refresh_rechecks_active_account_inside_lock(self):
        past = int((time.time() - 60) * 1000)
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com", expires_ms=past)
        self.run_cam("import", "--label", "A")
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.run_cam("import", "--label", "B")
        a_id = self.data()["accounts"][0]["id"]
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2")}
        store = self.cam.Store()
        out = {}
        with store.lock():
            t = threading.Thread(target=lambda: out.setdefault("tok", self.cam.ensure_fresh_token(store, a_id)))
            t.start()
            time.sleep(0.3)  # 线程发现 A 过期，正在等锁
            # 等锁期间切到了 A（相当于 switch_code_account 做的事）
            d = store.load()
            d["active"]["claude_code"] = a_id
            store.save(d)
            self.login("at-A1", "rt-A1", "uuid-A", "a@x.com", expires_ms=past)
        t.join(10)
        self.assertEqual(out["tok"][0], "at-A1")  # 当前账号：只读，不刷新
        self.assertNotIn(("POST", "/v1/oauth/token", "rt-A1"), MockAnthropic.calls)

    def test_invalid_refresh_token_says_relogin(self):
        past = int((time.time() - 60) * 1000)
        self.login("at-A1", "rt-dead", "uuid-A", "a@x.com", expires_ms=past)
        self.run_cam("import", "--label", "A")
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.run_cam("import", "--label", "B")
        self.run_cam("usage")
        self.assertIn("重新登录", self.data()["accounts"][0]["usage"]["error"])

    # ---- ~/.claude.json

    @unittest.skipIf(os.name == "nt", "Windows 上创建符号链接需要权限")
    def test_claude_json_symlink_is_kept(self):
        dot = os.path.join(self.tmp, "dotfiles")
        os.makedirs(dot)
        target = os.path.join(dot, "claude.json")
        shutil.move(self.cj_path, target)
        os.symlink(target, self.cj_path)
        self._two_accounts()
        self.run_cam("switch", "A")
        self.assertTrue(os.path.islink(self.cj_path))
        with open(target) as f:
            self.assertEqual(json.load(f)["oauthAccount"]["emailAddress"], "a@x.com")

    def test_claude_json_concurrent_write_is_merged(self):
        calls = []

        def mutate(cj):
            calls.append(1)
            if len(calls) == 1:
                # 我们读完之后、写入之前，Claude Code 改了文件
                with open(self.cj_path) as f:
                    other = json.load(f)
                other["projects"]["/work/new"] = {"allowedTools": []}
                with open(self.cj_path, "w") as f:
                    json.dump(other, f)
            cj["oauthAccount"] = {"emailAddress": "z@x.com"}

        self.cam.update_json_file(self.cj_path, mutate)
        with open(self.cj_path) as f:
            cj = json.load(f)
        self.assertEqual(len(calls), 2)
        self.assertIn("/work/new", cj["projects"])  # Claude Code 的改动还在
        self.assertEqual(cj["oauthAccount"]["emailAddress"], "z@x.com")
        self.assertEqual(cj["numStartups"], 42)

    # ---- 环境检查与清理

    def _fake_claude_env(self):
        cfg = os.path.join(self.tmp, ".claude")
        for d in ("cache", "shell-snapshots", "statsig", "commands", "skills"):
            os.makedirs(os.path.join(cfg, d), exist_ok=True)
        for f in ("cache/x", "shell-snapshots/s.sh", "statsig/a", "commands/c.md", "skills/s.md",
                  "settings.json", "CLAUDE.md", "projects/proj1/s.jsonl", ".last-cleanup"):
            with open(os.path.join(cfg, f), "w") as fh:
                fh.write("{}" if f.endswith(".json") else "x")
        return cfg

    def test_env_clean_cache_is_dry_run_by_default_and_whitelisted(self):
        cfg = self._fake_claude_env()
        self.assertEqual(self.run_cam("env", "clean"), 0)
        self.assertTrue(os.path.exists(os.path.join(cfg, "cache", "x")))  # 预演不删
        self.assertEqual(self.run_cam("env", "clean", "--apply"), 0)
        for gone in ("cache", "shell-snapshots", "statsig", ".last-cleanup"):
            self.assertFalse(os.path.exists(os.path.join(cfg, gone)), gone)
        for kept in ("commands/c.md", "skills/s.md", "settings.json", "CLAUDE.md", "projects/proj1/s.jsonl"):
            self.assertTrue(os.path.exists(os.path.join(cfg, kept)), kept)

    def test_env_clean_refuses_while_claude_runs(self):
        self._fake_claude_env()
        self.cam.running_claude_processes = lambda: [(123, "claude")]
        self.assertEqual(self.run_cam("env", "clean", "--apply"), 1)
        self.assertTrue(os.path.exists(os.path.join(self.tmp, ".claude", "cache")))
        self.assertEqual(self.run_cam("env", "clean", "--apply", "--force"), 0)

    def test_env_clean_logout_keeps_projects_and_can_switch_back(self):
        self._fake_claude_env()
        a_id, b_id = self._two_accounts()
        self.write_creds("at-B2", "rt-B2")  # Claude Code 刚刷新过 B
        self.assertEqual(self.run_cam("env", "clean", "--logout", "--apply"), 0)
        self.assertFalse(os.path.exists(self.creds_path))
        with open(self.cj_path) as f:
            cj = json.load(f)
        self.assertNotIn("oauthAccount", cj)
        self.assertEqual(cj["projects"], {"/work/x": {"allowedTools": ["Bash"]}})
        self.assertIsNone(self.data()["active"]["claude_code"])
        self.assertEqual(self.cam.Store().load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B2")
        self.assertEqual(self.run_cam("switch", "B"), 0)
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B2")

    def test_env_clean_purge_moves_everything_to_backup(self):
        cfg = self._fake_claude_env()
        a_id, _ = self._two_accounts()
        self.assertEqual(self.run_cam("env", "clean", "--purge", "--apply"), 1)  # 非交互必须 -y
        self.assertTrue(os.path.isdir(cfg))
        self.assertEqual(self.run_cam("env", "clean", "--purge", "--apply", "-y"), 0)
        self.assertFalse(os.path.exists(cfg))
        self.assertFalse(os.path.exists(self.cj_path))
        root = os.path.join(self.tmp, ".claude-accounts", "env-backups")
        dest = os.path.join(root, os.listdir(root)[0])
        self.assertTrue(os.path.exists(os.path.join(dest, "claude-config", "projects", "proj1", "s.jsonl")))
        with open(os.path.join(dest, "claude.json")) as f:
            self.assertEqual(json.load(f)["numStartups"], 42)
        self.assertEqual(len(self.data()["accounts"]), 2)  # cam 的数据不动

    def test_doctor_reports_overrides_and_duplicate_installs(self):
        import contextlib
        import io
        bins = []
        for name in ("a", "b"):
            d = os.path.join(self.tmp, "bin-" + name)
            os.makedirs(d)
            p = os.path.join(d, "claude.cmd" if os.name == "nt" else "claude")
            with open(p, "w") as f:
                f.write("#!/bin/sh\necho 9.9.9\n")
            os.chmod(p, 0o755)
            bins.append(d)
        old_path = os.environ.get("PATH", "")
        os.environ["PATH"] = os.pathsep.join(bins)  # 不带系统 PATH，免得算上本机真装的 claude
        os.environ["ANTHROPIC_API_KEY"] = "sk-ant-x"
        with open(os.path.join(self.tmp, ".claude", "settings.json"), "w") as f:
            json.dump({"apiKeyHelper": "/bin/echo"}, f)
        buf = io.StringIO()
        try:
            with contextlib.redirect_stdout(buf):
                self.cam.cmd_doctor(self.cam.Store())
                self.cam.cmd_env_install(self.cam.Store(), self.cam.argparse.Namespace(force=False, npm=False, yes=False))
        finally:
            os.environ["PATH"] = old_path
            os.environ.pop("ANTHROPIC_API_KEY")
        out = buf.getvalue()
        self.assertIn("ANTHROPIC_API_KEY", out)
        self.assertIn("apiKeyHelper", out)
        self.assertIn("2 份 claude", out)
        self.assertIn("已经装好了", out)
        self.assertNotIn("sk-ant-x", out)

    def test_http_api(self):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import", "--label", "A")
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.run_cam("import", "--label", "B")
        self.run_cam("add-api", "--label", "API", "--key", "sk-ant-api-supersecretvalue1234")
        store = self.cam.Store()
        token = self.cam.server_token(store)
        srv = self.cam.ThreadingServer(("127.0.0.1", 0), self.cam.make_handler(store, token))
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{srv.server_address[1]}"
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def call(method, path, body=None, tok=token):
            req = urllib.request.Request(base + path, method=method,
                                         data=json.dumps(body).encode() if body is not None else None,
                                         headers={"Authorization": f"Bearer {tok}", "Content-Type": "application/json"})
            try:
                with opener.open(req) as r:
                    return r.status, r.read().decode()
            except urllib.error.HTTPError as e:
                return e.code, e.read().decode()

        try:
            st, html = call("GET", "/", tok="")
            self.assertEqual(st, 200)
            self.assertIn("<html", html.lower())
            self.assertEqual(call("GET", "/api/state", tok="wrong")[0], 401)
            st, txt = call("GET", "/api/state")
            self.assertEqual(st, 200)
            for secret in ("rt-A1", "at-B1", "supersecretvalue"):
                self.assertNotIn(secret, txt)
            state = json.loads(txt)
            a_id = state["accounts"][0]["id"]
            st, txt = call("POST", f"/api/accounts/{a_id}/switch")
            self.assertEqual(st, 200, txt)
            self.assertEqual(self.live()[0]["accessToken"], "at-A1")
            st, txt = call("PATCH", f"/api/accounts/{a_id}", {"expires_at": "2099-12-31", "note": "工作用"})
            self.assertEqual(st, 200, txt)
            acc = json.loads(txt)["accounts"][0]
            self.assertEqual(acc["manual"]["note"], "工作用")
            self.assertGreater(acc["days_left"], 1000)
            self.assertEqual(call("PATCH", f"/api/accounts/{a_id}", {"expires_at": "nope"})[0], 400)
            self.assertEqual(call("GET", "/api/local-usage?days=3")[0], 200)
            self.assertEqual(call("DELETE", f"/api/accounts/{a_id}")[0], 400)  # 当前账号
            st, txt = call("PUT", "/api/rules", {"rules": [{"type": "time", "days": [1, 2], "start": "9:00",
                                                             "end": "18:30", "account": a_id}], "alert_threshold": 80})
            self.assertEqual(st, 200, txt)
            rv = json.loads(call("GET", "/api/rules")[1])
            self.assertEqual(rv["alert_threshold"], 80)
            self.assertEqual(rv["rules"][0]["start"], "09:00")
            self.assertEqual(call("PUT", "/api/rules", {"rules": [{"type": "dir", "path": "", "account": a_id}]})[0], 400)
        finally:
            srv.shutdown()


SHARED = os.path.join(HERE, "..", "tests", "shared")


def shared(name):
    with open(os.path.join(SHARED, name), encoding="utf-8") as f:
        return json.load(f)


class SharedFixtureTest(unittest.TestCase):
    """tests/shared 里的数据 Rust 版（desktop/src-tauri/engine/tests）也跑一遍，两边规则必须一致。"""

    @classmethod
    def setUpClass(cls):
        sys.path.insert(0, HERE)
        import cam
        cls.cam = cam

    def test_identify(self):
        fx = shared("identify.json")
        data = {"accounts": fx["accounts"]}
        for c in fx["identify"]:
            creds = {"claudeAiOauth": {"refreshToken": c["refresh_token"]}} if c["refresh_token"] else {}
            acc = self.cam.identify_code_account(data, c["oauth_account"], creds)
            self.assertEqual(acc["id"] if acc else None, c["expect"], c["name"])
        for c in fx["agrees"]:
            self.assertEqual(self.cam.identity_agrees(c["account"], c["uuid"], c["email"]), c["expect"], c["name"])

    def test_account_health(self):
        for c in shared("health.json")["cases"]:
            h = self.cam.account_health(c["account"], c["has_creds"], c["days_left"])
            got = {"level": h["level"], "label": h["label"], "kinds": [i["kind"] for i in h["issues"]]}
            self.assertEqual(got, c["expect"], c["name"])
            self.assertTrue(all(set(i) == {"kind", "level", "text", "advice", "detail"} for i in h["issues"]), c["name"])

    def test_find_account(self):
        fx = shared("find_account.json")
        data = {"accounts": fx["accounts"]}
        for c in fx["cases"]:
            if c["expect"] is None:
                with self.assertRaises(self.cam.CamError, msg=c["name"]):
                    self.cam.find_account(data, c["key"])
            else:
                self.assertEqual(self.cam.find_account(data, c["key"])["id"], c["expect"], c["name"])

    def test_rules(self):
        import datetime as dt
        fx = shared("rules.json")
        data = {"accounts": fx["accounts"]}
        for c in fx["validate"]:
            if c["expect"] is None:
                with self.assertRaises(self.cam.CamError, msg=c["name"]):
                    self.cam.validate_rules(data, c["input"])
            else:
                self.assertEqual(self.cam.validate_rules(data, c["input"]), c["expect"], c["name"])
        data["rules"] = fx["match_rules"]
        for c in fx["match"]:
            r = self.cam.match_time_rule(data, dt.datetime.fromisoformat(c["at"]))
            self.assertEqual(r["id"] if r else None, c["expect"], c["name"])

    def test_renewals(self):
        import datetime as dt
        for c in shared("renewals.json")["cases"]:
            data = {"accounts": [{"id": "x", "manual": dict(c["manual"])}]}
            self.cam.roll_renewals(data, dt.date.fromisoformat(c["today"]))
            self.assertEqual(data["accounts"][0]["manual"], c["expect"], c["name"])

    def test_manual(self):
        import copy
        import datetime as dt
        fx = shared("manual.json")
        for c in fx["parse_day"]:
            d = self.cam.parse_day(c["input"])
            self.assertEqual(d.isoformat() if d else None, c["expect"], c["name"])
        for c in fx["apply"]:
            acc = copy.deepcopy(fx["account"])
            if c["expect"] is None:
                with self.assertRaises(self.cam.CamError, msg=c["name"]):
                    self.cam.apply_manual(acc, c["fields"])
                self.assertEqual(acc, fx["account"], c["name"] + "：出错时不应改动账号")
            else:
                self.cam.apply_manual(acc, c["fields"])
                self.assertEqual(acc, c["expect"], c["name"])
        for c in fx["days_left"]:
            got = self.cam.days_left(c["account"], dt.date.fromisoformat(c["today"]))
            self.assertEqual(got, c["expect"], c["name"])



class KeychainTest(unittest.TestCase):
    """钥匙串只在 macOS 上用，这里伪造 security 命令：卡住超时、命令不存在时不能直接抛 Python 异常。"""

    @classmethod
    def setUpClass(cls):
        sys.path.insert(0, HERE)
        import cam
        cls.cam = cam

    def fake_security(self, *results):
        """每次调用依次取 results 里的一项：(返回码, 输出) 或要抛出的异常。"""
        it = iter(results)

        def run(cmd, **kw):
            r = next(it)
            if isinstance(r, BaseException):
                raise r
            return subprocess.CompletedProcess(cmd, r[0], r[1], "")
        return mock.patch.object(self.cam.subprocess, "run", run)

    def test_read(self):
        timeout = subprocess.TimeoutExpired("security", 15)
        with self.fake_security((44, ""), timeout):  # 按用户名查不到，退一步查时卡住
            self.assertIsNone(self.cam.keychain_read())
        with self.fake_security(OSError("security not found")):
            self.assertIsNone(self.cam.keychain_read())
        with self.fake_security((44, ""), (0, '{"claudeAiOauth": {}}\n')):
            self.assertEqual(self.cam.keychain_read(), {"claudeAiOauth": {}})

    def test_write_and_delete(self):
        timeout = subprocess.TimeoutExpired("security", 15)
        with self.fake_security(timeout), self.assertRaises(self.cam.CamError):
            self.cam.keychain_write({"claudeAiOauth": {}})
        with self.fake_security((44, ""), timeout), self.assertRaises(self.cam.CamError):
            self.cam.keychain_delete()
        with self.fake_security(OSError("security not found")), self.assertRaises(self.cam.CamError):
            self.cam.keychain_delete()


if __name__ == "__main__":
    unittest.main(verbosity=2)
