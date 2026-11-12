"""cam.py 的端到端测试：伪造 HOME、伪造 Anthropic 接口，不碰真实账号。

运行：python3 scripts/test_cam.py
"""
import http.server
import importlib
import base64
import contextlib
import datetime as dt
import getpass
import io
import signal
import socket
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import types
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
    ipapi_type = "residential"  # /ipapiis（ipapi.is）和 /ipapi（ip-api.com）的 IP 类型：residential / hosting / proxy / tor / mobile / None=查询失败
    trace_ip = "1.2.3.4"  # /cdn-cgi/trace 返回的出口 IP
    rdap_country = "US"  # /rdap/<ip> 返回的注册国，None 表示记录没写
    trace6_loc = None  # /trace6 的地区，None 表示 IPv6 不可用
    status_indicator = "none"  # 官方状态页：none / minor / major / critical
    status_summary = "operational"  # 状态页摘要 /summary.json：operational / incident；/summary-down.json 总是 503
    usage_code = None  # 设了就让 /api/oauth/usage 返回这个状态码（比如 429 限流）
    release_latest = "2.1.0"  # /releases/latest：Claude Code 原生安装器的最新版本号
    down = ()  # 这些路径（按前缀）一律回 503：接口连得上但答不上来
    slow = {}  # 路径前缀 -> 秒：收到请求先等这么久再回（卡住的接口）；hold.set() 让等着的马上放行
    hold = threading.Event()
    active = 0  # 正在处理的请求数
    active_lock = threading.Lock()

    def log_message(self, *a):
        pass

    def handle(self):
        with MockAnthropic.active_lock:
            MockAnthropic.active += 1
        try:
            super().handle()
        finally:
            with MockAnthropic.active_lock:
                MockAnthropic.active -= 1

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
        secs = next((s for p, s in MockAnthropic.slow.items() if self.path.startswith(p)), 0)
        if secs:
            MockAnthropic.hold.wait(secs)
        if self.path.startswith(tuple(MockAnthropic.down)):
            return self._send(503, {"error": {"message": "down"}})
        if self.path in ("/cdn-cgi/trace", "/cf/cdn-cgi/trace"):  # /cf/…：Cloudflare 自己的 trace（claude.ai 没回音时的备用），节点写成 LAX 好区分
            colo = "LAX" if self.path.startswith("/cf/") else "SJC"
            return self._send_html(200, f"fl=1\nip={self.trace_ip}\nts={time.time() + self.trace_ts_offset:.3f}\n"
                                        f"loc={self.trace_loc}\ncolo={colo}\nwarp={'on' if self.trace_warp else 'off'}\n")
        if self.path.startswith("/ipapiis"):  # ipapi.is：带 key 返回完整信息和标记，不带 key 只有简版
            t = self.ipapi_type
            if t is None:
                return self._send(429, {"error": "quota"})
            if "key=" not in self.path:
                return self._send(200, {"ip": self.trace_ip, "company": self.meta_org, "asn": f"AS7922 {self.meta_org}",
                                        "city": "San Jose", "region": "California", "country": "United States"})
            asn = {"asn": 7922, "org": self.meta_org}
            if self.rdap_country:
                asn["country"] = self.rdap_country.lower()
            return self._send(200, {"ip": self.trace_ip, "is_datacenter": t == "hosting", "is_vpn": t == "proxy", "is_proxy": False,
                                    "is_tor": t == "tor", "is_mobile": t == "mobile", "asn": asn,
                                    "company": {"name": self.meta_org, "type": "hosting" if t == "hosting" else "isp"},
                                    "location": {"country_code": self.meta_country or self.trace_loc, "city": "San Jose",
                                                 "state": "California"}})
        if self.path.startswith("/ipinfo"):
            return self._send(200, {"ip": self.trace_ip, "org": f"AS7922 {self.meta_org}", "city": "San Jose",
                                    "region": "California", "country": self.meta_country or self.trace_loc})
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
        if self.path == "/summary.json":
            comps = [{"name": n, "status": "operational"} for n in
                     ("claude.ai", "Claude API (api.anthropic.com)", "Claude Code", "Claude Cowork")]
            status, incidents = {"indicator": "none", "description": "All Systems Operational"}, []
            if self.status_summary == "incident":
                comps[2]["status"] = "partial_outage"
                comps[3]["status"] = "major_outage"  # 和这里无关的组件，不列出来
                status = {"indicator": "major", "description": "Partial System Outage"}
                incidents = [{"name": "Elevated errors on Claude Code", "status": "identified", "impact": "major"},
                             {"name": "Earlier issue", "status": "resolved", "impact": "minor"}]
            return self._send(200, {"page": {"name": "Claude"}, "status": status, "components": comps, "incidents": incidents})
        if self.path == "/summary-down.json":
            return self._send(503, {"error": "down"})
        if self.path in ("/releases/latest", "/releases/stable"):
            return self._send_html(200, self.release_latest + "\n")
        if self.path == "/api/oauth/usage" and self.usage_code:
            return self._send(self.usage_code, {"error": {"message": "rate limited"}})
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


class QuietServer(http.server.ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        if isinstance(sys.exc_info()[1], (BrokenPipeError, ConnectionResetError, ConnectionAbortedError)):
            return  # 测超时的用例里客户端等不及先走了，不用打一屏报错
        super().handle_error(request, client_address)


class CamTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.srv = QuietServer(("127.0.0.1", 0), MockAnthropic)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{cls.srv.server_address[1]}"
        os.environ["CAM_API_BASE"] = base
        os.environ["CAM_CLAUDE_WEB_BASE"] = base
        os.environ["CAM_TOKEN_URLS"] = base + "/v1/oauth/token"
        os.environ["CAM_STATUS_URL"] = base + "/status.json"
        os.environ["CAM_STATUS_SUMMARY_URLS"] = base + "/summary.json"
        os.environ["CAM_CLAUDE_RELEASES_URL"] = base + "/releases"
        os.environ["CAM_CF_META_URL"] = base + "/meta"
        os.environ["CAM_IPAPI_URL"] = base + "/ipapi"
        os.environ["CAM_IPAPI_IS_URL"] = base + "/ipapiis"
        os.environ["CAM_IPAPI_IS_KEY"] = "test-key"  # 默认按“配了 key”测，带类型标记
        os.environ["CAM_IPINFO_URL"] = base + "/ipinfo"
        os.environ["CAM_META_TTL"] = "0"  # 测试里每次都重查运营商；缓存单独测
        os.environ["CAM_NO_BG_CHECK"] = "1"  # 不起后台检测进程
        os.environ.pop("CAM_IP_LOOKUP", None)
        os.environ["CAM_RDAP_URL"] = base + "/rdap/"
        os.environ["CAM_CF_TRACE4_URL"] = base + "/trace4"
        os.environ["CAM_CF_TRACE6_URL"] = base + "/trace6"
        os.environ["CAM_CF_TRACE_URL"] = base + "/cf/cdn-cgi/trace"
        os.environ["CAM_TEST_NET"] = "net-a"  # 本机连的是哪张网：固定住，要测换网络的用例自己改
        os.environ["CAM_TEST_LOCALE"] = "en_US"
        os.environ["CAM_TEST_DNS"] = "192.168.1.1"
        os.environ["CAM_TEST_PROXY"] = ""  # 不读本机的系统代理设置（macOS 上开着系统代理时结果会变）
        os.environ["CAM_TEST_TZ"] = "America/New_York=US"  # 跑测试的机器时区不定，固定成和 mock 出口一致
        os.environ["CAM_TEST_ROUTE_IF"] = ""
        os.environ["CAM_NO_KEYCHAIN"] = "1"
        os.environ["CAM_NO_NOTIFY"] = "1"  # 不弹真的系统通知；要测的地方单独换掉
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
        MockAnthropic.status_summary = "operational"
        MockAnthropic.usage_code = None
        MockAnthropic.release_latest = "2.1.0"
        MockAnthropic.meta_org = "Comcast Cable Communications, LLC"
        MockAnthropic.meta_country = None
        MockAnthropic.ipapi_type = "residential"
        MockAnthropic.rdap_country = "US"
        MockAnthropic.trace6_loc = None
        MockAnthropic.trace_ip = "1.2.3.4"
        MockAnthropic.down = ()
        MockAnthropic.slow = {}
        os.environ["CAM_TEST_NET"] = "net-a"
        # 跑测试的机器上可能真有 claude 在运行，测试里统一当作没有；cam doctor 读的 /proc 换成一个空的假目录
        self._real_procs = self.cam.running_claude_processes
        self.cam.running_claude_processes = lambda: []
        os.environ["CAM_TEST_PROC_ROOT"] = os.path.join(self.tmp, "proc")
        os.makedirs(os.environ["CAM_TEST_PROC_ROOT"])

    def tearDown(self):
        self.drain()
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

    def drain(self):
        """让 mock 里还卡着的请求马上放行、等它们走完，免得拖进下一个用例。"""
        MockAnthropic.slow = {}
        MockAnthropic.hold.set()
        end = time.time() + 5
        while MockAnthropic.active and time.time() < end:
            time.sleep(0.01)
        MockAnthropic.hold = threading.Event()

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

    # ---- 切换前核对目标账号的登录、只换凭据里的 Claude 登录

    def test_switch_renews_expired_target_first(self):
        a_id, b_id = self._two_accounts(a_expires=self._expired_ms())  # A 保存的 access token 已过期，当前是 B
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2")}
        self.assertEqual(self.run_cam("switch", "A"), 0)
        live, cj = self.live()
        self.assertEqual((live["accessToken"], live["refreshToken"]), ("at-A2", "rt-A2"))
        self.assertEqual(cj["oauthAccount"]["emailAddress"], "a@x.com")
        self.assertEqual(self.cam.Store().load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A2")
        self.assertEqual([c for c in MockAnthropic.calls if c[1] == "/v1/oauth/token"],
                         [("POST", "/v1/oauth/token", "rt-A1")])  # 当前登录（B）从不替它续

    def test_switch_to_dead_login_keeps_live_login(self):
        a_id, b_id = self._two_accounts(a_expires=self._expired_ms())  # 假接口不认 rt-A1：A 的 refresh token 已作废
        store = self.cam.Store()

        def live_files():
            with open(self.creds_path, "rb") as f, open(self.cj_path, "rb") as g:
                return f.read(), g.read()

        before = live_files()
        # 有会话在跑时登记了等它切到 A；会话退出后切不过去，登记作废，不会一直重试
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.cam.api_switch(store, a_id, "wait")
        self.cam.running_claude_processes = lambda: []
        with self.assertRaises(self.cam.CamError) as cm:
            self.cam.apply_pending_switch(store)
        self.assertIn("已失效", str(cm.exception))
        self.assertIn("当前登录没有动", str(cm.exception))
        self.assertNotIn("pending_switch", self.data())
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(self.run_cam("switch", "A"), 1)
        self.assertIn("cam login", err.getvalue())
        self.assertEqual(live_files(), before)
        self.assertEqual(self.data()["active"]["claude_code"], b_id)
        self.assertFalse(os.path.isdir(os.path.join(self.tmp, ".claude-accounts", "backups")))
        # 连不上 token 接口：同样不切
        urls = self.cam.TOKEN_URLS
        self.cam.TOKEN_URLS = ["http://127.0.0.1:1/v1/oauth/token"]
        try:
            with self.assertRaises(self.cam.CamError) as cm:
                self.cam.switch_code_account(store, "A")
        finally:
            self.cam.TOKEN_URLS = urls
        self.assertIn("续登录失败", str(cm.exception))
        self.assertIn("当前登录没有动", str(cm.exception))
        self.assertEqual(live_files(), before)

    def test_switch_replaces_only_the_claude_login_in_credentials(self):
        def add(**extra):
            with open(self.creds_path) as f:
                c = json.load(f)
            c.update(extra)
            with open(self.creds_path, "w") as f:
                json.dump(c, f)

        def creds_now():
            with open(self.creds_path) as f:
                return json.load(f)

        # A 导入时凭据里有一份 MCP 服务器的登录；后来（B 在用时）MCP 重新登录过，还多了别的字段
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        add(mcpOAuth={"srv": {"accessToken": "mcp-old"}})
        self.run_cam("import", "--label", "A")
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        self.run_cam("import", "--label", "B")
        add(mcpOAuth={"srv": {"accessToken": "mcp-new"}}, otherKey=1)
        self.assertEqual(self.run_cam("switch", "A"), 0)
        c = creds_now()
        self.assertEqual(c["claudeAiOauth"]["refreshToken"], "rt-A1")
        self.assertEqual(c["mcpOAuth"], {"srv": {"accessToken": "mcp-new"}})  # 不退回 A 当初那份
        self.assertEqual(c["otherKey"], 1)
        self.assertEqual(self.run_cam("switch", "B"), 0)
        c = creds_now()
        self.assertEqual(c["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual((c["mcpOAuth"], c["otherKey"]), ({"srv": {"accessToken": "mcp-new"}}, 1))

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

        def ev(mid, hours_ago, out, model="claude-opus-4-1-20250805"):
            return json.dumps({"type": "assistant", "requestId": "r" + mid,
                               "timestamp": (now - dt.timedelta(hours=hours_ago)).isoformat().replace("+00:00", "Z"),
                               "message": {"id": mid, "model": model, "usage": {"input_tokens": 1, "output_tokens": out,
                                                                                "cache_read_input_tokens": 5}}})
        p = os.path.join(self.tmp, ".claude", "projects", "proj1", "x.jsonl")
        with open(p, "w") as f:
            f.write("\n".join([ev("m1", 3, 100), ev("m1", 3, 100), ev("m2", 0.5, 7), ev("m0", 10, 1000, "claude-sonnet-4-5"),
                               "not json", '{"type":"user"}']) + "\n")
        rep = self.cam.local_usage(self.data(), 7)
        by = {}
        for r in rep["rows"]:
            by.setdefault(r["account_id"], 0)
            by[r["account_id"]] += r["output"]
        self.assertEqual(by.get(a_id), 100)  # 去重
        self.assertEqual(by.get("bbbb"), 7)
        self.assertEqual(by.get("unattributed"), 1000)
        # 按模型的分布（给面板画占比），同样去重、同样按天
        by_model = {}
        for r in rep["models"]:
            by_model[r["model"]] = by_model.get(r["model"], 0) + r["output"]
        self.assertEqual(by_model, {"claude-opus-4-1-20250805": 107, "claude-sonnet-4-5": 1000})

    def test_local_events_cache_reads_only_new_lines(self):
        """日志按文件缓存：没变不重读，追加了只读新加的行，没写完的最后一行留到下次，文件被重写就从头读。"""
        import datetime as dt
        now = dt.datetime.now(dt.timezone.utc)

        def ev(mid, out):
            return json.dumps({"type": "assistant", "requestId": "r" + mid, "timestamp": now.isoformat().replace("+00:00", "Z"),
                               "message": {"id": mid, "model": "claude-opus-4-1-20250805", "usage": {"input_tokens": 1, "output_tokens": out}}})
        p = os.path.join(self.tmp, ".claude", "projects", "proj2", "y.jsonl")
        os.makedirs(os.path.dirname(p), exist_ok=True)
        since = now - dt.timedelta(days=1)
        self.cam._LOCAL_CACHE.clear()
        outs = lambda: sorted(u["output_tokens"] for _, _, u in self.cam.local_events(since))  # noqa: E731
        with open(p, "w") as f:
            f.write(ev("a", 10) + "\n" + ev("b", 20))  # 第二行还没写完（没有换行）
        self.assertEqual(outs(), [10])
        with open(p, "a") as f:
            f.write("\n" + ev("c", 30) + "\n")
        self.assertEqual(outs(), [10, 20, 30])
        self.assertEqual(self.cam._LOCAL_CACHE[p][0], os.path.getsize(p))  # 读到了文件末尾
        with mock.patch.object(self.cam, "open", create=True, side_effect=AssertionError("文件没变不该重读")):
            self.assertEqual(outs(), [10, 20, 30])
        with open(p, "w") as f:  # 文件被重写、变短：从头读
            f.write(ev("d", 40) + "\n")
        self.assertEqual(outs(), [40])
        os.remove(p)
        self.assertEqual(outs(), [])
        self.assertNotIn(p, self.cam._LOCAL_CACHE)  # 删掉的文件不留在缓存里

    def test_incident_and_diagnostic(self):
        """账号刚出问题那一刻记现场（出口 IP、环境、会话数），同一问题持续着不重复记；用过的出口补 IP 和次数；导出的诊断不含密钥。"""
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import", "--label", "A")
        d = self.data()
        aid = d["accounts"][0]["id"]
        d["region"] = {"loc": "SG", "ip": "1.2.3.4", "org": "Singtel", "city": "Singapore", "ip_type": "residential", "warp": False,
                       "checked_at": self.cam.now_iso(), "env": {"tz": "Asia/Shanghai", "locale": "zh_CN", "proxy": [], "tunnel": None, "dns": [{"ip": "8.8.8.8", "name": "Google", "country": None}]}}
        self.cam.note_account_region(d, aid, "SG", d["region"])
        self.cam.note_account_region(d, aid, "SG", d["region"])
        self.cam.Store().save(d)
        with mock.patch.object(self.cam, "fetch_code_usage", side_effect=self.cam.CamError("登录已失效，无法续期")):
            self.cam.refresh_usage(self.cam.Store(), aid)
            self.cam.refresh_usage(self.cam.Store(), aid)
        acc = self.data()["accounts"][0]
        self.assertEqual(len(acc["incidents"]), 1, acc["incidents"])
        inc = acc["incidents"][0]
        self.assertEqual((inc["kind"], inc["region"]["ip"], inc["region"]["org"], inc["env"]["tz"], inc["sessions"]),
                         ("login_invalid", "1.2.3.4", "Singtel", "Asia/Shanghai", 0))
        self.assertEqual((acc["regions"][0]["loc"], acc["regions"][0]["ip"], acc["regions"][0]["times"]), ("SG", "1.2.3.4", 2))
        store = self.cam.Store()
        rep = self.cam.account_diagnostic(store, store.load(), "A")
        text = self.cam.diagnostic_text(rep)
        for secret in ("rt-A1", "at-A1", "refreshToken", "accessToken"):
            self.assertNotIn(secret, text)
            self.assertNotIn(secret, json.dumps(rep, ensure_ascii=False))
        for s in ("# cam 账号诊断：A", "登录已失效", "1.2.3.4", "Singtel", "家庭 / 公司宽带", "Asia/Shanghai", "出问题时的现场（最近 1 次）"):
            self.assertIn(s, text)
        # 旧版本记的出口只有地区没有 IP、检测失败时现场是空的：报告照样生成
        d = self.data()
        d["accounts"][0]["regions"].append({"loc": "US", "at": self.cam.now_iso()})
        d["accounts"][0]["incidents"].append({"at": self.cam.now_iso(), "kind": "login_invalid", "error": "x", "region": {}, "env": {}, "sessions": 0})
        store.save(d)
        bare = self.cam.diagnostic_text(self.cam.account_diagnostic(store, store.load(), "A"))
        self.assertIn("US（美国）（运营商未知，类型未知）", bare)
        # 代理地址里的账号密码不进报告、不进面板
        with mock.patch.dict(os.environ, {"HTTPS_PROXY": "http://alice:S3cretPass@proxy.local:8080"}):
            parts = self.cam.proxy_parts()
        self.assertTrue(any(p == "HTTPS_PROXY=http://***@proxy.local:8080" for p in parts), parts)
        self.assertFalse(any("S3cretPass" in p for p in parts))
        self.assertEqual(self.cam.mask_proxy("user:pw@10.0.0.1:3128"), "***@10.0.0.1:3128")
        self.assertEqual(self.cam.mask_proxy("HTTPS 127.0.0.1:7890"), "HTTPS 127.0.0.1:7890")
        self.assertTrue(self.cam.diagnostic_filename(rep).startswith("cam-诊断-A-"))
        pub = self.cam.public_state(store, store.load())["accounts"][0]  # 面板账号详情里直接显示的摘要
        self.assertEqual((pub["last_incident"]["ip"], pub["last_incident"]["text"], pub["exits"][0]["ip"], pub["exits"][0]["times"]),
                         ("1.2.3.4", "登录已失效，无法续期", "1.2.3.4", 2))
        # 命令行和接口
        self.assertEqual(self.run_cam("diag", "A"), 0)
        out = os.path.join(self.tmp, "diag.md")
        self.assertEqual(self.run_cam("diag", "1", "--out", out), 0)
        self.assertIn("## 当前环境", open(out, encoding="utf-8").read())
        token = self.cam.server_token(store)
        srv = self.cam.ThreadingServer(("127.0.0.1", 0), self.cam.make_handler(store, token))
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        req = urllib.request.Request(f"http://127.0.0.1:{srv.server_address[1]}/api/accounts/{aid}/diagnostic",
                                     headers={"Authorization": "Bearer " + token})
        with opener.open(req) as r:
            body = json.load(r)
        self.assertIn("1.2.3.4", body["text"])
        self.assertTrue(body["filename"].endswith(".md"))

    def test_menubar_launcher_fix(self):
        """老版本写的启动脚本（弹窗不常驻，点一下卡半秒）：插件按当时的模式重跑一次安装脚本改写；没装、已是新版的不动，失败也只试一次。"""
        plugins = os.path.join(self.tmp, "plugins")
        os.makedirs(plugins)
        store = self.cam.Store()
        cam_menu = self._menu_plugin()
        path = os.path.join(plugins, self.cam.MENUBAR_PLUGIN)
        runs = []

        def fake_run(cmd, **kw):
            runs.append((cmd, kw.get("env", {}).get("CAM_PYTHON")))
            return mock.Mock(returncode=0, stdout="", stderr="")
        with mock.patch.dict(os.environ, {"CAM_SWIFTBAR_DIR": plugins}), mock.patch.object(cam_menu.subprocess, "run", side_effect=fake_run):
            st = {}
            cam_menu.launcher_fix(store, st)  # 没装
            self.assertEqual(runs, [])
            with open(path, "w", encoding="utf-8") as f:
                f.write("#!/bin/bash\n# <swiftbar.persistentWebView>false</swiftbar.persistentWebView>\n# <swiftbar.type>streamable</swiftbar.type>\nexec python3 cam_menu.py stream\n")
            cam_menu.launcher_fix(store, st)
            self.assertEqual(len(runs), 1)
            self.assertEqual(runs[0][0][2], "--stream")  # 保持它现在的模式
            self.assertTrue(runs[0][0][1].endswith(os.path.join("swiftbar", "install.sh")), runs[0])
            self.assertEqual(runs[0][1], sys.executable)
            cam_menu.launcher_fix(store, st)  # 假的安装脚本没改文件：不再重跑
            cam_menu.launcher_fix(store, cam_menu.load_state(store))  # 下一分钟的新进程从状态文件里知道试过了
            self.assertEqual(len(runs), 1)
            with open(path, "w", encoding="utf-8") as f:
                f.write("#!/bin/bash\n# <swiftbar.persistentWebView>true</swiftbar.persistentWebView>\nexec python3 cam_menu.py\n")
            cam_menu.launcher_fix(store, {})  # 新版的启动脚本不动
            self.assertEqual(len(runs), 1)

    def test_menubar_settings(self):
        """卡片小面板“设置”页：动画模式看启动脚本、图标样式记在 prefs；改模式重跑安装脚本（这里不真跑）。"""
        plugins = os.path.join(self.tmp, "plugins")
        os.makedirs(plugins)
        store = self.cam.Store()
        with mock.patch.dict(os.environ, {"CAM_SWIFTBAR_DIR": plugins}):
            st = self.cam.menubar_settings(store)
            self.assertEqual((st["available"], st["installed"], st["mode"], st["icon"]), (True, False, None, "bars"))
            with open(os.path.join(plugins, self.cam.MENUBAR_PLUGIN), "w", encoding="utf-8") as f:
                f.write("#!/bin/bash\n# <swiftbar.type>streamable</swiftbar.type>\nexec python3 cam_menu.py stream\n")
            self.assertEqual(self.cam.menubar_settings(store)["mode"], "stream")
            r = self.cam.save_menubar_settings(store, {"icon": "lamp"})
            self.assertEqual((r["icon"], store.load()["prefs"]["menu_icon"], r["restarting"]), ("lamp", "lamp", False))
            with self.assertRaises(self.cam.CamError):
                self.cam.save_menubar_settings(store, {"icon": "neon"})
            self.assertFalse(self.cam.save_menubar_settings(store, {"mode": "stream"})["restarting"])  # 已经是流式，不动
            ran = []
            fake = lambda cmd, **kw: (ran.append(cmd), types.SimpleNamespace(returncode=0, stdout="", stderr=""))[1]  # noqa: E731
            with mock.patch.object(self.cam.subprocess, "run", side_effect=fake):
                r = self.cam.save_menubar_settings(store, {"mode": "classic"})
            self.assertTrue(r["restarting"])
            self.assertTrue(ran and ran[0][0] == "bash" and ran[0][1].replace("\\", "/").endswith("extras/swiftbar/install.sh")
                            and ran[0][2] == "--classic", ran)  # Windows 上是反斜杠
            fail = lambda cmd, **kw: types.SimpleNamespace(returncode=1, stdout="", stderr="没装 SwiftBar")  # noqa: E731
            with mock.patch.object(self.cam.subprocess, "run", side_effect=fail), self.assertRaises(self.cam.CamError):
                self.cam.save_menubar_settings(store, {"mode": "classic"})

    def test_local_window_usage(self):
        """当前窗口里本机用了多少：按切换记录归属、按窗口起点截断、按模型的周额度只算那个模型。"""
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import", "--label", "A")
        d = self.data()
        a_id = d["accounts"][0]["id"]
        import datetime as dt
        now = dt.datetime.now(dt.timezone.utc)

        def ev(mid, hours_ago, model, out, cache_read=0):
            return json.dumps({"type": "assistant", "requestId": "r" + mid,
                               "timestamp": (now - dt.timedelta(hours=hours_ago)).isoformat().replace("+00:00", "Z"),
                               "message": {"id": mid, "model": model,
                                           "usage": {"input_tokens": 10, "output_tokens": out, "cache_read_input_tokens": cache_read}}})
        p = os.path.join(self.tmp, ".claude", "projects", "proj1", "w.jsonl")
        with open(p, "w") as f:
            f.write("\n".join([ev("m1", 1, "claude-opus-5-5", 1000, 100000), ev("m1", 1, "claude-opus-5-5", 1000, 100000),
                               ev("m2", 2, "claude-fable-5-1", 2000), ev("m3", 30, "claude-fable-5-1", 500),
                               ev("m4", 200, "claude-opus-5-5", 9999)]) + "\n")
        usage = {"five_hour": {"utilization": 20, "resets_at": (now + dt.timedelta(hours=2.5)).isoformat()},
                 "seven_day": {"utilization": 10, "resets_at": (now + dt.timedelta(days=5)).isoformat()},
                 "models": [{"name": "Fable", "utilization": 8, "resets_at": (now + dt.timedelta(days=5)).isoformat()}]}
        # 没有切换记录：都算当前账号
        d["switch_log"] = []
        got = self.cam.local_window_usage(d, {a_id: usage}, now)[a_id]["windows"]
        self.assertEqual(got["five_hour"]["messages"], 2)          # m1（去重）、m2；m3 在 5 小时窗口之前
        self.assertEqual(got["five_hour"]["tokens"], 3020)
        self.assertAlmostEqual(got["five_hour"]["cost_usd"], (10 * 4 + 1000 * 20 + 100000 * 0.2 + 10 * 10 + 2000 * 50) / 1e6, places=4)
        self.assertEqual(got["seven_day"]["messages"], 3)          # m4 在 7 天窗口之前
        self.assertEqual(got["Fable"]["messages"], 2)              # 只算 Fable 的
        self.assertEqual(got["Fable"]["tokens"], 2520)
        # 一小时前切到了别的号：之后的消息不算 A 的，最早一次切换之前的算不出来
        d["switch_log"] = [{"id": a_id, "at": (now - dt.timedelta(hours=10)).isoformat()},
                           {"id": "bbbb", "at": (now - dt.timedelta(hours=1.5)).isoformat()}]
        got = self.cam.local_window_usage(d, {a_id: usage}, now)[a_id]["windows"]
        self.assertEqual(got["five_hour"]["messages"], 1)          # 只有 m2
        self.assertEqual(got["seven_day"]["messages"], 1)          # m3 在第一次切换之前
        self.assertEqual(self.cam.local_window_usage(d, {a_id: {"five_hour": None}}, now), {})

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

    def _two_accounts(self, a_expires=None):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com", expires_ms=a_expires)
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

        # 100 分：不问，直接启动。没检测过时只快速问一次 claude.ai 的出口，完整检测交给后台进程（测试里关掉了）
        MockAnthropic.calls = []
        self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=ask_with(""), interactive=True))
        self.assertEqual(asked, [])
        self.assertEqual([c[1] for c in MockAnthropic.calls], ["/cdn-cgi/trace"])
        self.assertIsNone(self.data().get("region"))
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
        os.environ["CAM_MENU_MINI"] = "0"  # 测试里不去起 cam serve（卡片小面板要它）
        sys.path.insert(0, os.path.join(HERE, "..", "extras", "swiftbar"))
        import cam_menu
        cam_menu = importlib.reload(cam_menu)
        cam_menu.cam = self.cam
        store = self.cam.Store()
        lines = []
        cam_menu.render(store, cam_menu.background_work(store), out=lines.append, rich=False)  # 这里测文字版；图形版见 test_menubar_rich_layout
        self.assertTrue(lines[0].startswith("Claude ⚠"))
        # 没账号时给两个点了就在终端里跑的入口：存下当前登录（cam import）、登录另一个号（cam login）
        self.assertTrue(any('param2="import" terminal=true' in l for l in lines), lines)
        self.assertTrue(any('param2="login" terminal=true' in l for l in lines), lines)
        # 后台归 cam serve 做时，它记的错误也显示出来：没账号时放在最上面，截短、完整内容在悬停提示里
        write(os.path.join(store.root, self.cam.SERVE_STATE), json.dumps({"pid": 1, "last_error": "自动导入：没有权限读 x。sudo chown me x"}))
        lines = []
        cam_menu.render(store, {"bg_owner": "other"}, out=lines.append, rich=False)
        self.assertIn(f"⚠ 自动导入：没有权限读 x。sudo chown me x | color={cam_menu.RED} length={cam_menu.LINE}", lines)
        os.remove(os.path.join(store.root, self.cam.SERVE_STATE))
        # 登录后插件运行一次就自动保存，标题显示账号
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.login_then = None
        lines = []
        cam_menu.render(store, cam_menu.background_work(store), out=lines.append, rich=False)
        self.assertIn("a@x.com", lines[0])
        self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")
        lines = []
        cam_menu.render(store, cam_menu.background_work(store), out=lines.append, rich=False)
        switch_lines = [l for l in lines if 'param2="switch"' in l]
        self.assertTrue(switch_lines, lines)
        self.assertTrue(all("|" in l for l in lines if l != "---"))

    def test_menubar_rich_layout(self):
        # macOS 上菜单栏图标是额度环、每个账号下面一行进度条图：SwiftBar 的 image 参数，浅色、深色两张 PNG，
        # 画两倍像素再用 width / height 定成点数（Retina 上才清楚）；套餐放进 badge。Windows 托盘不认这些，仍用文字进度条
        cam_menu = self._menu_plugin()
        MockAnthropic.tokens = {"at-A1": {"u5": 10.0, "u7": 5.0, "email": "a@x.com", "plan": "max"},
                                "at-B1": {"u5": 75.0, "u7": 30.0, "email": "b@x.com", "plan": "max"}}
        self._two_accounts()  # 当前是 B
        self.assertEqual(self.run_cam("usage"), 0)
        store = self.cam.Store()
        lines = []
        cam_menu.render(store, {}, out=lines.append, rich=True)
        title = lines[0]
        self.assertTrue(title.startswith("B 75% | image="), title)  # 百分比是文字（系统字体清楚），图标只是四格信号
        self.assertIn(" width=16 height=16", title)
        pics = re.search(r"image=(\S+)", title).group(1).split(",")
        self.assertEqual(len(pics), 2)
        for pic in pics:
            png = base64.b64decode(pic)
            self.assertTrue(png.startswith(b"\x89PNG"))
            self.assertEqual(struct.unpack(">II", png[16:24]), (16 * cam_menu.SCALE, 16 * cam_menu.SCALE))  # 16 点 × 像素倍数
            self.assertIn(b"pHYs", png)
        # 红绿灯：没会话在跑、没问题时标题只有一行（右下角绿点），菜单第一行写“空闲 · 一切正常”
        self.assertEqual(lines[1], "---")
        self.assertTrue(lines[2].startswith("● 空闲 · 一切正常 | size=12 color=" + cam_menu.GREEN), lines[2])
        # 开着 claude 但没在执行（等你输入）：还是绿的，标题一行，第一行写着几个会话开着
        with mock.patch.object(self.cam, "running_claude_processes", return_value=[(1, "claude"), (2, "claude")]), \
                mock.patch.object(self.cam, "claude_activity", return_value=False):
            idle = []
            cam_menu.render(store, {}, out=idle.append, rich=True)
        self.assertEqual(idle[1], "---", idle[:3])
        self.assertTrue(idle[2].startswith("● 空闲 · 2 个 claude 会话开着 | size=12 color=" + cam_menu.GREEN), idle[2])
        # 有 claude 会话真在执行任务：橙点，标题两行实心 / 空心交替（SwiftBar 每 5 秒换一张），第二行不进下拉菜单、不带快捷键
        with mock.patch.object(self.cam, "running_claude_processes", return_value=[(1, "claude"), (2, "claude")]), \
                mock.patch.object(self.cam, "claude_activity", return_value=True):
            busy = []
            cam_menu.render(store, {}, out=busy.append, rich=True)
        self.assertEqual(busy[2], "---", busy[:3])
        self.assertTrue(busy[0].startswith("B 75% | image=") and busy[1].startswith("B 75% | image="), busy[:2])
        self.assertNotEqual(busy[0].split("image=")[1].split()[0], busy[1].split("image=")[1].split()[0])  # 两张图不一样
        self.assertTrue(all(" dropdown=false" in l for l in busy[:2]), busy[:2])
        self.assertIn(" shortcut=", busy[0])
        self.assertNotIn(" shortcut=", busy[1])
        self.assertNotIn("dropdown=false", lines[0])
        self.assertTrue(busy[3].startswith("● 工作中 · 2 个 claude 会话在跑 | size=12 color=" + cam_menu.YELLOW), busy[3])
        # 流式模式：一帧就是整份菜单，标题只有一行；工作中时相邻两帧的图标不一样（橙灯呼吸），空闲时每帧都一样
        cam_menu.STREAMING = True
        try:
            with mock.patch.object(self.cam, "running_claude_processes", return_value=[(1, "claude")]), \
                    mock.patch.object(self.cam, "claude_activity", return_value=True):
                f0, f1 = cam_menu.stream_frame(store, {}, 0.0), cam_menu.stream_frame(store, {}, 0.25)
            self.assertEqual(cam_menu.LAST_LIGHT[2], "pulse")
            self.assertNotEqual(f0, f1)
            self.assertEqual(f0.split("\n---\n")[0].count("\n"), 0)  # 第一个 --- 前只有一行标题
            self.assertNotIn(" dropdown=false", f0)
            self.assertTrue(f0.startswith("B 75% | image="), f0[:40])
            # 模式两行当开关：打勾的是现在的模式，另一行点了就切
            self.assertIn('param2="mode" param3="classic"', f0)
            self.assertIn("--流式：插件常驻，动画顺滑 | size=12 checked=true", f0)  # 模式和样式收在“更多”子菜单里
            self.assertTrue(any('param2="mode" param3="stream"' in l for l in lines), lines[-8:])
            self.assertTrue(any(l.startswith("--经典") and l.endswith(" checked=true") for l in lines), lines[-8:])
            # 图标样式两行当开关：默认信号格打勾；选红绿灯后记在账号库 prefs 里，标题的图换了、红绿灯那行打勾
            self.assertTrue(any(l.startswith("--信号格") and l.endswith(" checked=true") for l in lines), lines[-4:])
            self.assertTrue(any('param2="icon" param3="lamp"' in l for l in lines), lines[-4:])
            self.assertEqual(cam_menu.set_icon_from_menu(store, "lamp"), 0)
            self.assertEqual(store.load()["prefs"]["menu_icon"], "lamp")
            lamp = []
            cam_menu.render(store, {}, out=lamp.append, rich=True)
            self.assertNotEqual(lamp[0].split("image=")[1].split()[0], lines[0].split("image=")[1].split()[0])
            self.assertTrue(any(l.startswith("--红绿灯") and l.endswith(" checked=true") for l in lamp), lamp[-4:])
            cam_menu.set_icon_from_menu(store, "bars")
            i0, i1 = cam_menu.stream_frame(store, {}, 0.0), cam_menu.stream_frame(store, {}, 0.25)
            self.assertEqual(cam_menu.LAST_LIGHT[2], "")
            self.assertEqual(i0, i1)
        finally:
            cam_menu.STREAMING = False
        rows = [l for l in lines[2:] if l.startswith(("A |", "B |"))]  # 从 --- 后面数，不算标题行
        self.assertEqual(len(rows), 2, lines)
        self.assertTrue(all('badge="Max' in l for l in rows), rows)
        # 全局快捷键：标题行 +C 打开菜单；账号行 +1 / +2 按添加顺序（当前号 B 排在最前，但它是第二个加的）
        self.assertTrue(title.endswith(" shortcut=CTRL+OPTION+CMD+C"), title)
        # 标题行带卡片小面板的 href + webview（本机 cam serve 的 /mini）；CAM_MENU_MINI=0、起不来或是桌面版的服务就不带
        self.assertNotIn("webview=", title)
        cam_menu.MINI = True
        fresh = mock.patch.object(cam_menu.cam, "serve_outdated", return_value=False)  # 后台 cam serve 的代码不旧
        fresh.start()
        self.addCleanup(fresh.stop)
        try:
            with mock.patch.object(cam_menu.cam, "ensure_panel", return_value=(8787, "tok")), \
                 mock.patch.object(cam_menu.cam, "mini_available", return_value=True):
                lines2 = []
                cam_menu.render(store, {}, out=lines2.append, rich=True)
            w, h = cam_menu.mini_size(store.load()["accounts"])
            self.assertIn(f" href=http://127.0.0.1:8787/mini?t=tok webview=true webvieww={w} webviewh={h} shortcut=", lines2[0])
            # 弹窗高度按账号数算：一个号最矮，其它 Claude Code 号每多两个加一行方形小卡，到上限就整页上下滑；API 账号不在卡片里
            code = {"kind": "claude_code"}
            one = cam_menu.mini_size([code])[1]
            self.assertEqual(w, cam_menu.MINI_WIDTH)
            self.assertEqual(cam_menu.mini_size([code, {"kind": "api"}])[1], one)
            two = cam_menu.mini_size([code] * 2)[1]
            self.assertEqual(two, one + cam_menu.MINI_SEC + cam_menu.MINI_CARD)
            self.assertEqual(cam_menu.mini_size([code] * 3)[1], two)
            self.assertEqual(cam_menu.mini_size([code] * 4)[1], two + cam_menu.MINI_CARD)
            self.assertEqual(cam_menu.mini_size([code] * 40)[1], cam_menu.MINI_MAX)
            web, desk = {"kind": "claude_web"}, {"kind": "claude_desktop"}
            self.assertEqual(cam_menu.mini_size([code, web])[1], one)  # 网页号不进卡片
            self.assertEqual(cam_menu.mini_size([code, desk])[1], one + cam_menu.MINI_SEC + cam_menu.MINI_ROW)  # 桌面号单独一组，药丸卡一行一个
            self.assertEqual(cam_menu.mini_size([code, code, desk])[1], two + cam_menu.MINI_SEC + cam_menu.MINI_ROW)  # 两组各占一行
            self.assertEqual(cam_menu.mini_size([desk])[1], one + cam_menu.MINI_DESK)  # 没有 Code 号：大卡是桌面号（没有本机消耗、多一个“打开”按钮）
            self.assertEqual(cam_menu.mini_size([desk, desk])[1], one + cam_menu.MINI_DESK + cam_menu.MINI_SEC + cam_menu.MINI_ROW)
            self.assertEqual(cam_menu.mini_size([code, desk, desk])[1], one + cam_menu.MINI_SEC + 2 * cam_menu.MINI_ROW)  # 桌面号一行一个
            # 8787 上是桌面版的服务（没有 /mini 页）：在下一个空端口另起一个 cam serve 给卡片用，不是干脆不给卡片；桌面版的服务不动
            told = []
            cam_menu.notify = lambda title, body: told.append(title)
            ensure = lambda store, port=8787, need_mini=False: (8788, "tok") if need_mini else (8787, "tok")
            with mock.patch.object(cam_menu.cam, "ensure_panel", side_effect=ensure), \
                 mock.patch.object(cam_menu.cam, "mini_available", lambda port, token: port == 8788), \
                 mock.patch.object(cam_menu.cam, "restart_stale_serve", return_value=False) as rs:
                lines2 = []
                cam_menu.render(store, {}, out=lines2.append, rich=True)
            self.assertIn("href=http://127.0.0.1:8788/mini?t=tok webview=true", lines2[0])
            self.assertFalse(rs.called)
            self.assertEqual(told, [])
            # 后台 cam serve 的代码过时（code_stamp 对不上）：结束它、起新的，通知一次
            with mock.patch.object(cam_menu.cam, "ensure_panel", return_value=(8787, "tok")), \
                 mock.patch.object(cam_menu.cam, "mini_available", return_value=True), \
                 mock.patch.object(cam_menu.cam, "serve_outdated", return_value=True), \
                 mock.patch.object(cam_menu.cam, "restart_stale_serve", return_value=True) as rs:
                lines2 = []
                cam_menu.render(store, {}, out=lines2.append, rich=True)
            self.assertTrue(rs.called)
            self.assertIn("webview=true", lines2[0])
            self.assertEqual(told, ["面板已更新"])
            # 过时但没能重启（找不到它的进程）：先用旧的那份卡片，提醒一次怎么手动弄，之后不再每轮提醒；不是干脆不给卡片
            told.clear()
            cam_menu._STALE_TOLD = False
            for _ in range(2):
                with mock.patch.object(cam_menu.cam, "ensure_panel", return_value=(8787, "tok")), \
                     mock.patch.object(cam_menu.cam, "mini_available", return_value=True), \
                     mock.patch.object(cam_menu.cam, "serve_outdated", return_value=True), \
                     mock.patch.object(cam_menu.cam, "restart_stale_serve", return_value=False):
                    lines2 = []
                    cam_menu.render(store, {}, out=lines2.append, rich=True)
                self.assertIn("href=http://127.0.0.1:8787/mini?t=tok webview=true", lines2[0])
            self.assertEqual(told, ["卡片用的还是旧代码"])
            cam_menu._STALE_TOLD = False
            # 流式模式靠源文件的修改时间认出代码更新
            self.assertEqual(len(cam_menu.source_stamp()), 2)
            with mock.patch.object(cam_menu.cam, "ensure_panel", side_effect=self.cam.CamError("起不来")):
                lines2 = []
                cam_menu.render(store, {}, out=lines2.append, rich=True)
            self.assertNotIn("webview=", lines2[0])
        finally:
            cam_menu.MINI = False
        self.assertTrue(rows[0].startswith("B |") and rows[0].endswith(" shortcut=CTRL+OPTION+CMD+2"), rows)
        self.assertTrue(rows[1].endswith(" shortcut=CTRL+OPTION+CMD+1"), rows)
        self.assertFalse(any("alternate=true" in l for l in lines), lines)  # 不再有按 Option 才显示的邮箱行：macOS 26 上会和正常行一起显示
        # 名字宽度规则：汉字算 2、字母算 1，12 以内直接显示；邮箱或太宽就显示序号
        for label, numbered in (("工作号", False), ("Pro号-CC", False), ("主力Max号", False), ("一二三四五六", False),
                                ("一二三四五六七", True), ("abcdefghijklm", True), ("b@x.com", True), ("", True)):
            self.assertEqual(cam_menu.needs_number({"label": label}), numbered, label)
        bars = [l for l in lines if "size=11" in l and "| image=" in l]
        self.assertEqual(len(bars), 2, lines)
        self.assertTrue(any(l.startswith("5 小时 75%，") and "7 天 30%" in l for l in bars), bars)
        self.assertTrue(all(" width=56 height=6" in l for l in bars), bars)
        self.assertNotIn("▰", "\n".join(lines))
        # macOS 26 上 SwiftBar 高亮时会给带 color 的行重排标题，图标、图片就丢了：rich 模式里带图的行一律不带 color
        for l in lines[1:]:  # 标题行是菜单栏按钮，不是菜单项，带 color 没事
            text, _, params = l.partition(" | ")
            if "image=" in params:
                self.assertFalse(re.search(r"(^| )color=", params), l)
        # 菜单栏放不下长名字：名字是邮箱或超过 6 个字时显示序号（和 cam 列表、快捷键的数字一致），账号行前面也标上序号
        self.assertEqual(cam_menu.title_name({"label": "工作号"}, 1), "工作号")
        self.assertEqual(cam_menu.title_name({"label": "a@x.com", "email": "a@x.com"}, 2), "2号")
        self.assertEqual(cam_menu.title_name({"label": "一二三四五六七"}, 3), "3号")
        self.assertEqual(self.run_cam("set", "B", "--label", "b@x.com"), 0)
        lines = []
        cam_menu.render(store, {}, out=lines.append, rich=True)
        self.assertTrue(lines[0].startswith("2号 75% | image="), lines[0])
        self.assertTrue(any(l.startswith("--起个短名") and 'param2="panel"' in l for l in lines), lines)  # 显示序号时账号子菜单里提示起名
        self.assertTrue(any(l.startswith("2 · b@x.com | sfimage=checkmark.circle.fill") and "shortcut=CTRL+OPTION+CMD+2" in l for l in lines), lines)
        self.assertEqual(self.run_cam("set", "2", "--label", "B"), 0)
        self.assertEqual([cam_menu.level_of(p) for p in (10, 70, 90)], ["ok", "warn", "bad"])
        self.assertNotEqual(cam_menu.quota_ring(10, 0), cam_menu.quota_ring(90, 0))
        self.assertNotEqual(cam_menu.quota_bar(75, 0), cam_menu.quota_bar(75, 1))  # 深色模式另一张
        # 不传 rich：macOS 上开，别处关
        lines = []
        cam_menu.render(store, {}, out=lines.append)
        self.assertEqual("| image=" in lines[0], self.cam.IS_MAC)  # sfimage= 里也有 image=，要连竖线一起比
        if not self.cam.IS_MAC:
            self.assertIn("▰", "\n".join(lines))
        # CAM_MENU_HOTKEY 设成空就一个快捷键都不注册
        cam_menu.HOTKEY = ""
        lines = []
        cam_menu.render(store, {}, out=lines.append, rich=True)
        self.assertNotIn("shortcut=", "\n".join(lines))

    def test_menubar_lamp_icon(self):
        """红绿灯（三盏灯）样式：图标 10×18 点、按 2 倍像素画（SwiftBar 在 Retina 上按 2 倍重画，一个图像素对一个屏幕像素）；
        呼吸和闪的每一帧亮着的灯都看得清（不会暗到和灯体一个颜色），没亮的灯只是淡淡的；信号格呼吸时每格渐亮。"""
        import zlib
        cam_menu = self._menu_plugin()

        def pixels(png):  # 解开自己写的 PNG：每行开头一个过滤字节 0，后面是 RGBA
            w, h = struct.unpack(">II", png[16:24])
            pos, idat = 8, b""
            while pos < len(png):
                n, tag = struct.unpack(">I4s", png[pos:pos + 8])
                if tag == b"IDAT":
                    idat += png[pos + 8:pos + 8 + n]
                pos += 12 + n
            raw, stride = zlib.decompress(idat), 1 + w * 4
            return w, h, lambda x, y: tuple(raw[y * stride + 1 + x * 4:y * stride + 5 + x * 4])

        S = cam_menu.SCALE
        self.assertEqual(S, 2)
        self.assertEqual((cam_menu.icon_size("lamp"), cam_menu.icon_size("bars")), ((12, 20), (16, 16)))
        orange = lambda px: px[0] >= 200 and 100 <= px[1] <= 200 and px[2] <= 110 and px[3] >= 240  # noqa: E731
        # 黄闪：工作中亮半秒灭半秒（phase 0–0.25 亮、0.25–0.5 灭，再来一遍），等你确认快一倍；灭的时候是淡淡的橙（镜片的颜色）
        for kind, phase, lit in (("", 0, True), ("pulse", 0, True), ("pulse", 0.3, False), ("pulse", 0.5, True), ("pulse", 0.375, False),
                                 ("blink", 0, True), ("blink", 0.125, False), ("blink", 0.375, False), ("blink", 0.5, True)):
            for dark in (0, 1):
                w, h, px = pixels(cam_menu.status_icon(dark, "busy", kind, phase, "lamp"))
                self.assertEqual((w, h), (12 * S, 20 * S))
                mid = px(6 * S, 10 * S)  # 中间那盏（橙）的圆心
                self.assertTrue(orange(mid) if lit else mid[0] >= 200 and mid[2] <= 110 and 60 <= mid[3] <= 120, (kind, phase, dark, mid))
                top = px(6 * S, round(4.2 * S))  # 上面那盏（红）没亮：和线框一个颜色的淡点（不是红的），不是全透明也不是实心
                if kind != "blink":  # 等你确认时三盏一起闪，下面单独查
                    self.assertEqual((top[0], top[1], top[2]), tuple(cam_menu.GLYPH[dark][:3]), (kind, phase, dark, top))
                    self.assertTrue(40 <= top[3] <= 110, (kind, phase, dark, top))
                self.assertEqual(px(0, 0)[3], 0, (kind, phase, dark))  # 圆角外面是透明的
                self.assertGreaterEqual(px(round(0.6 * S), 10 * S)[3], 180, (kind, phase, dark))  # 左边的线框
                self.assertEqual(px(round(2.2 * S), 10 * S)[3], 0, (kind, phase, dark))  # 线框和灯之间是空的
        # 等你确认（blink）：三盏灯一起闪——亮的那拍红、橙、绿都是实心，灭的那拍三盏都只剩淡淡的镜片色
        for dark in (0, 1):
            w, h, px = pixels(cam_menu.status_icon(dark, "busy", "blink", 0, "lamp"))
            r, g = px(6 * S, round(4.2 * S)), px(6 * S, round(15.8 * S))
            self.assertTrue(r[0] >= 200 and r[1] <= 120 and r[3] >= 240, r)
            self.assertTrue(g[1] >= 180 and g[0] <= 120 and g[3] >= 240, g)
            w, h, px = pixels(cam_menu.status_icon(dark, "busy", "blink", 0.125, "lamp"))
            r, g = px(6 * S, round(4.2 * S)), px(6 * S, round(15.8 * S))
            self.assertTrue(r[0] >= 200 and 60 <= r[3] <= 120 and g[1] >= 180 and 60 <= g[3] <= 120, (r, g))
        w, h, px = pixels(cam_menu.status_icon(0, "ok", "", 0, "lamp"))
        g = px(6 * S, round(15.8 * S))
        self.assertTrue(g[1] >= 180 and g[0] <= 110, g)  # 空闲：下面那盏绿灯亮
        # 信号格呼吸到一半：前两格全亮、第三格半亮、第四格没亮
        w, h, px = pixels(cam_menu.status_icon(0, "busy", "pulse", 0.5, "bars"))
        y = 16 * S - 3 * S
        bars = [px(round(i * 4 * S + 1.5 * S), y) for i in range(4)]
        self.assertTrue(orange(bars[0]) and orange(bars[1]), bars)
        self.assertLess(bars[3][3], 100, bars)
        self.assertTrue(bars[3][3] < bars[2][3] < bars[0][3], bars)
        # 选了三盏灯样式：标题里的图按 10×18 点显示
        MockAnthropic.tokens = {"at-A1": {"u5": 10.0, "u7": 5.0, "email": "a@x.com", "plan": "max"},
                                "at-B1": {"u5": 75.0, "u7": 30.0, "email": "b@x.com", "plan": "max"}}
        self._two_accounts()
        self.assertEqual(self.run_cam("usage"), 0)
        store = self.cam.Store()
        self.assertEqual(cam_menu.set_icon_from_menu(store, "lamp"), 0)
        lines = []
        cam_menu.render(store, {}, out=lines.append, rich=True)
        self.assertIn(" width=12 height=20", lines[0])
        png = base64.b64decode(re.search(r"image=(\S+)", lines[0]).group(1).split(",")[0])
        self.assertEqual(struct.unpack(">II", png[16:24]), (12 * S, 20 * S))

    def test_claude_activity(self):
        """红绿灯只在 claude 真的在执行任务时闪黄：最近 20 秒内写过会话记录，或者进程（含子进程）在吃 CPU；开着等输入不算。"""
        cam = self.cam
        self.assertEqual(cam._cpu_seconds("0:01.23"), 1.23)
        self.assertEqual(cam._cpu_seconds("01:02:03"), 3723.0)
        self.assertEqual(cam._cpu_seconds("1-00:00:01"), 86401.0)
        self.assertEqual(cam._cpu_seconds("x"), 0.0)
        root = os.path.join(cam.claude_config_dir(), "projects", "p", "sub")
        os.makedirs(root, exist_ok=True)
        path = os.path.join(root, "s.jsonl")
        with open(path, "w") as f:
            f.write("{}\n")
        reset = lambda: cam._ACTIVITY.update(t=0.0, cpu=None, busy=False)  # noqa: E731
        with mock.patch.object(cam, "claude_cpu_seconds", return_value={1: 0.0}):
            reset()
            self.assertTrue(cam.claude_activity([1], max_age=0))  # 刚写过记录
            old = time.time() - 60
            os.utime(path, (old, old))
            reset()
            self.assertFalse(cam.claude_activity([1], max_age=0))  # 记录是一分钟前的，CPU 第一次采样比不了
            self.assertFalse(cam.claude_activity([], max_age=0))  # 没有进程
        # CPU：两次采样之间进程（含子进程）用了 1 秒 CPU，10 秒里就是 10%，算在执行；新出现的进程（上次没有）不比
        cam._ACTIVITY.update(t=time.monotonic() - 10, cpu={1: 5.0}, busy=False)
        with mock.patch.object(cam, "claude_cpu_seconds", return_value={1: 6.0, 2: 100.0}):
            self.assertTrue(cam.claude_activity([1, 2], max_age=0))
        cam._ACTIVITY.update(t=time.monotonic() - 10, cpu={1: 5.0}, busy=False)
        with mock.patch.object(cam, "claude_cpu_seconds", return_value={1: 5.1, 2: 100.0}):
            self.assertFalse(cam.claude_activity([1, 2], max_age=0))  # 1%：闲着
            self.assertFalse(cam.claude_activity([1, 2], max_age=5))  # 缓存里的结论
        real = cam.claude_cpu_seconds([os.getpid()])  # 真的采一次：本进程的 CPU 时间，格式都认得
        if real is not None:
            self.assertGreaterEqual(real[os.getpid()], 0.0)
        # session_count 数进程时一起判断，session_busy 给面板和红绿灯用
        cam._SESSION_CACHE[:] = [0.0, 0]
        with mock.patch.object(cam, "running_claude_processes", return_value=[(1, "claude")]), \
                mock.patch.object(cam, "claude_activity", return_value=True):
            self.assertEqual(cam.session_count(max_age=0), 1)
            self.assertTrue(cam.session_busy())
        with mock.patch.object(cam, "running_claude_processes", return_value=[]):
            self.assertEqual(cam.session_count(max_age=0), 0)
            self.assertFalse(cam.session_busy())
        reset()

    def test_hooks_and_attention(self):
        """Claude Code 弹权限确认时红绿灯三盏灯一起闪：cam hooks install 往 settings.json 加钩子（别的钩子不动，重复装不重复）；
        钩子事件由 cam hook 从 stdin 读，PermissionRequest 记下会话、答过了 / 会话结束清掉；记录文件在提示之后又写了也算答过了。"""
        cam = self.cam
        path = cam.claude_settings_file()
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            json.dump({"theme": "dark", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}, f)
        self.assertFalse(cam.hooks_status()["installed"])
        st = cam.hooks_install()
        self.assertTrue(st["installed"])
        with open(path, encoding="utf-8") as f:
            s = json.load(f)
        self.assertEqual(s["theme"], "dark")
        self.assertEqual(len(s["hooks"]["Stop"]), 2)  # 用户自己的那条还在
        self.assertEqual(s["hooks"]["Stop"][0]["hooks"][0]["command"], "say done")
        ours = s["hooks"]["PermissionRequest"][0]["hooks"][0]
        self.assertTrue(cam.HOOK_MARK.search(ours["command"]) and ours["async"] and ours["type"] == "command", ours)
        self.assertEqual(sorted(s["hooks"]), sorted(set(cam.HOOK_EVENTS) | {"Stop"}))
        cam.hooks_install()  # 再装一次不重复
        with open(path, encoding="utf-8") as f:
            self.assertEqual(len(json.load(f)["hooks"]["Stop"]), 2)
        self.assertEqual(self.run_cam("hooks"), 0)
        # 钩子事件：PermissionRequest 记下来（只记工具和说明，不记命令本身）
        tr = os.path.join(self.tmp, ".claude", "projects", "p", "s1.jsonl")
        os.makedirs(os.path.dirname(tr), exist_ok=True)
        with open(tr, "w") as f:
            f.write("{}\n")
        old = time.time() - 30
        os.utime(tr, (old, old))
        ev = lambda name, **kw: json.dumps({"hook_event_name": name, "session_id": "s1", "transcript_path": tr, "cwd": "/x", **kw})  # noqa: E731
        with mock.patch.object(sys, "stdin", io.StringIO(ev("PermissionRequest", tool_name="Bash",
                                                            tool_input={"command": "rm -rf node_modules", "description": "删掉 node_modules"}))):
            self.assertEqual(cam.main(["hook"]), 0)
        with open(cam.attention_path(), encoding="utf-8") as f:
            rec = json.load(f)["sessions"]["s1"]
        self.assertEqual((rec["tool"], rec["what"], rec["transcript"]), ("Bash", "删掉 node_modules", tr))
        self.assertNotIn("rm -rf", json.dumps(rec))
        cam._SESSION_CACHE[:] = [0.0, 0]
        with mock.patch.object(cam, "running_claude_processes", return_value=[(1, "claude")]), \
                mock.patch.object(cam, "claude_activity", return_value=False):
            att = cam.attention_view()
            self.assertEqual((att["count"], att["text"]), (1, "Claude Code 在等你确认 · Bash：删掉 node_modules"))
            later = time.time() + 5  # 记录文件在提示之后又写了：答过了
            os.utime(tr, (later, later))
            self.assertIsNone(cam.attention_view())
            os.utime(tr, (old, old))
            self.assertIsNotNone(cam.attention_view())
            with mock.patch.object(sys, "stdin", io.StringIO(ev("Stop"))):  # 会话结束：清掉
                self.assertEqual(cam.main(["hook"]), 0)
            self.assertIsNone(cam.attention_view())
            with mock.patch.object(sys, "stdin", io.StringIO(ev("PermissionRequest", tool_name="Edit"))):
                cam.main(["hook"])
            self.assertEqual(cam.attention_view()["text"], "Claude Code 在等你确认 · Edit")
            data = cam._load_attention()
            data["sessions"]["s1"]["since_ts"] -= 3 * 3600  # 过期的不算
            cam._save_attention(data)
            self.assertIsNone(cam.attention_view())
            with mock.patch.object(sys, "stdin", io.StringIO("not json")):  # 坏的输入不报错、不输出
                self.assertEqual(cam.main(["hook"]), 0)
            with mock.patch.object(sys, "stdin", io.StringIO(ev("PermissionRequest", tool_name="Bash"))):
                cam.main(["hook"])
            # 菜单栏：等你确认 → 三盏灯一起闪（标题两帧）、第一行写着在等你确认；卡片 / 面板从 /api/state 的 attention 拿同一句
            cam_menu = self._menu_plugin()
            MockAnthropic.tokens = {"at-A1": {"u5": 10.0, "u7": 5.0, "email": "a@x.com", "plan": "max"},
                                    "at-B1": {"u5": 75.0, "u7": 30.0, "email": "b@x.com", "plan": "max"}}
            self._two_accounts()
            self.assertEqual(self.run_cam("usage"), 0)
            store = cam.Store()
            lines = []
            cam_menu.render(store, {}, out=lines.append, rich=True)
            self.assertEqual(lines[2], "---", lines[:3])
            self.assertTrue(lines[3].startswith("● Claude Code 在等你确认 · Bash | size=12 color=" + cam_menu.YELLOW), lines[3])
            self.assertEqual(cam.public_state(store, store.load())["attention"]["text"], "Claude Code 在等你确认 · Bash")
        with mock.patch.object(cam, "running_claude_processes", return_value=[]):
            cam._SESSION_CACHE[:] = [0.0, 0]
            self.assertIsNone(cam.attention_view())  # 没有 claude 进程在跑
        # 设置页的开关
        store = cam.Store()
        self.assertTrue(cam.menubar_settings(store)["hooks"])
        self.assertFalse(cam.save_menubar_settings(store, {"hooks": False})["hooks"])
        with open(path, encoding="utf-8") as f:
            s = json.load(f)
        self.assertEqual((list(s["hooks"]), len(s["hooks"]["Stop"]), s["theme"]), (["Stop"], 1, "dark"))  # 只剩用户自己的
        self.assertEqual(self.run_cam("hooks", "remove"), 0)
        self.assertTrue(cam.save_menubar_settings(store, {"hooks": True})["hooks"])

    def test_hook_fast_entry_lock_and_legacy_command(self):
        """钩子每次工具调用都跑一遍：走 cam_hook.py 这个快入口；没有会话在等时什么都不写；几个钩子同时来不互相盖掉；
        旧版本装的 cam.py hook 照样算装了，重新装一次换成快入口。"""
        cam = self.cam
        shim = os.path.join(HERE, "cam_hook.py")
        fast, legacy = cam.hook_command(), cam.hook_command(fast=False)
        self.assertTrue(fast.rstrip("\"'").endswith("cam_hook.py"), fast)
        self.assertTrue(legacy.endswith(" hook") and "cam.py" in legacy, legacy)
        self.assertTrue(cam.HOOK_MARK.search(fast) and cam.HOOK_MARK.search(legacy))
        with mock.patch.object(cam.os.path, "isfile", lambda p: False):  # 单独拷走了 cam.py，旁边没有 cam_hook.py
            self.assertEqual(cam.hook_command(), legacy)
        # 旧版本装的命令：算装了，标出 slow；重新装一次就换掉，不留两条
        path = cam.claude_settings_file()
        os.makedirs(os.path.dirname(path), exist_ok=True)
        group = lambda cmd: [{"hooks": [{"type": "command", "command": cmd, "timeout": 10, "async": True}]}]  # noqa: E731

        def flags():
            st = cam.hooks_status()
            return st["installed"], st["slow"], st["stale"]

        with open(path, "w", encoding="utf-8") as f:
            json.dump({"hooks": {ev: group(legacy) for ev in cam.HOOK_EVENTS}}, f)
        self.assertEqual(flags(), (True, True, False))
        self.assertEqual([(i["level"], i["advice"]) for i in cam.hook_items()], [("info", "cam hooks install（重新装一次，换成快入口）")])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(self.run_cam("hooks"), 0)
        self.assertIn("旧写法", out.getvalue())
        cam.hooks_install()
        self.assertEqual(flags(), (True, False, False))
        self.assertEqual([(i["level"], i["text"]) for i in cam.hook_items()], [("ok", "Claude Code 的钩子已装：弹权限确认时红绿灯三盏灯一起闪")])
        self.assertIn(cam.hook_items()[0], cam.checkup_cam(cam.Store(), cam.Store().load()))  # 体检里有这一行
        with open(path, encoding="utf-8") as f:
            hooks = json.load(f)["hooks"]
        self.assertTrue(all(len(hooks[ev]) == 1 and hooks[ev][0]["hooks"][0]["command"] == fast for ev in cam.HOOK_EVENTS), hooks)
        with open(path, "w", encoding="utf-8") as f:  # 指向别处的（代码搬过家、换了 python）才算过期
            json.dump({"hooks": {ev: group("/old/python3 /old/place/cam.py hook") for ev in cam.HOOK_EVENTS}}, f)
        self.assertEqual(flags(), (False, False, True))
        item = cam.hook_items()[0]  # 体检：指向的文件不在了，钩子其实不起作用
        self.assertEqual(item["level"], "warn")
        self.assertIn("/old/python3 已经不在了", item["text"])
        other = os.path.join(self.tmp, "elsewhere", "cam.py")  # 另一份 cam，文件都在：能跑，只是告诉一声
        write(other, "")
        with open(path, "w", encoding="utf-8") as f:
            json.dump({"hooks": {ev: group(f'"{sys.executable}" "{other}" hook') for ev in cam.HOOK_EVENTS}}, f)
        self.assertEqual((flags(), cam.hook_items()[0]["level"]), ((False, False, True), "info"))
        with open(path, "w", encoding="utf-8") as f:
            json.dump({"hooks": {"Stop": group(fast)}}, f)
        self.assertEqual((cam.hook_items()[0]["level"], "只装了一部分" in cam.hook_items()[0]["text"]), ("warn", True))
        os.remove(path)
        self.assertEqual(cam.hook_items(), [])  # 没装钩子：体检里不提
        shutil.rmtree(cam.cam_home())  # 上面跑体检时建的
        # 没有会话在等：清除事件（每次工具跑完都来一个）不建目录、不写文件
        ev = lambda name, sid="s1", **kw: {"hook_event_name": name, "session_id": sid, **kw}  # noqa: E731
        cam.hook_event(ev("PostToolUse"))
        self.assertFalse(os.path.exists(cam.cam_home()))
        cam.hook_event(ev("PermissionRequest", tool_name="Bash"))
        cam.hook_event(ev("PermissionRequest", "s2", tool_name="Edit"))
        self.assertEqual(sorted(cam._load_attention()["sessions"]), ["s1", "s2"])
        with mock.patch.object(cam, "_save_attention", side_effect=AssertionError("没变化不该重写文件")):
            cam.hook_event(ev("PostToolUse", "s3"))  # 别的会话的事件
        cam.hook_event(ev("Stop"))
        self.assertEqual(sorted(cam._load_attention()["sessions"]), ["s2"])
        cam.hook_event(ev("SessionEnd", "s2"))
        self.assertFalse(os.path.exists(cam.attention_path()))  # 没有会话在等了：文件删掉，下一个事件一看没有就走
        # 真的当钩子跑：两个入口都不输出、返回 0；同时来的 8 个事件一个都不丢
        env = dict(os.environ, CAM_HOME=os.path.join(self.tmp, "hookhome"))
        att = os.path.join(env["CAM_HOME"], cam.ATTENTION_FILE)

        def start(argv):
            return subprocess.Popen([sys.executable] + argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)

        def finish(proc, payload):
            out, err = proc.communicate(payload if isinstance(payload, bytes) else json.dumps(payload).encode(), timeout=60)
            self.assertEqual((proc.returncode, out, err), (0, b"", b""))

        finish(start([shim]), b"not json")
        self.assertFalse(os.path.exists(env["CAM_HOME"]))
        procs = [start([shim]) for _ in range(8)]
        for i, proc in enumerate(procs):  # 先都喂进去，再一起等：读—改—写撞在一起
            proc.stdin.write(json.dumps(ev("PermissionRequest", f"c{i}", tool_name="Bash",
                                           tool_input={"command": "secret-cmd", "description": "跑测试"})).encode())
            proc.stdin.close()
        for proc in procs:
            self.assertEqual(proc.wait(timeout=60), 0)
            self.assertEqual(proc.stdout.read() + proc.stderr.read(), b"")
            proc.stdout.close()
            proc.stderr.close()
        with open(att, encoding="utf-8") as f:
            raw = f.read()
        self.assertEqual(sorted(json.loads(raw)["sessions"]), [f"c{i}" for i in range(8)])
        self.assertEqual(json.loads(raw)["sessions"]["c3"]["what"], "跑测试")
        self.assertNotIn("secret-cmd", raw)
        self.assertIsNotNone(cam.parse_iso(json.loads(raw)["sessions"]["c3"]["since"]))
        finish(start([os.path.join(HERE, "cam.py"), "hook"]), ev("Stop", "c0"))  # 旧写法的入口：同一段代码
        with open(att, encoding="utf-8") as f:
            self.assertEqual(sorted(json.load(f)["sessions"]), [f"c{i}" for i in range(1, 8)])

    def test_usage_time_is_when_the_numbers_are_from(self):
        """用量没取到时（断网、限流、登录过期而后台不替它续）旧数字留着，“用量更新”的时间也还是旧数字的时间，不写成刚刚；
        另记 checked_at（什么时候查的），体检用它看后台有没有在查。"""
        cam = self.cam
        a_id, b_id = self._two_accounts()
        MockAnthropic.tokens = {"at-A1": {"u5": 10.0, "u7": 5.0, "email": "a@x.com", "plan": "max"},
                                "at-B1": {"u5": 75.0, "u7": 30.0, "email": "b@x.com", "plan": "max"}}
        self.assertEqual(self.run_cam("usage"), 0)
        store = cam.Store()
        u = next(a for a in store.load()["accounts"] if a["id"] == a_id)["usage"]
        self.assertEqual((u["error"], u["fetched_at"]), (None, u["checked_at"]))
        old = "2026-01-01T00:00:00+00:00"
        with store.lock():
            d = store.load()
            for a in d["accounts"]:
                a["usage"]["fetched_at"] = a["usage"]["checked_at"] = old
            store.save(d)
        MockAnthropic.usage_code = 429
        cam.refresh_usage(store)
        d = store.load()
        u = next(a for a in d["accounts"] if a["id"] == a_id)["usage"]
        self.assertIn("429", u["error"])
        self.assertEqual((u["five_hour"]["utilization"], u["fetched_at"]), (10.0, old))  # 数字和它的时间都是上次那份
        self.assertGreater(cam.parse_iso(u["checked_at"]), cam.parse_iso(old))
        fresh = [(i["level"], i["text"]) for i in cam.usage_freshness(d, True, 10)]
        self.assertEqual(fresh, [("ok", "A：用量上次刷新 刚刚"), ("ok", "B：用量上次刷新 刚刚")])  # 查是查了的，没取到另有一条说
        MockAnthropic.usage_code = None
        cam.refresh_usage(store)
        u = next(a for a in store.load()["accounts"] if a["id"] == a_id)["usage"]
        self.assertEqual((u["error"], u["fetched_at"]), (None, u["checked_at"]))
        self.assertGreater(cam.parse_iso(u["fetched_at"]), cam.parse_iso(old))
        # 体检里的一句话：窗口早就重置了的数字要标出来
        self.assertEqual(cam._usage_brief({"five_hour": {"utilization": 8, "resets_at": "2020-01-01T00:00:00Z"},
                                           "seven_day": {"utilization": 11, "resets_at": "2999-01-01T00:00:00Z"}}),
                         "5 小时已用 8%（这个窗口已经重置，是旧数字），7 天已用 11%")
        self.assertEqual(cam._usage_brief({"five_hour": {"utilization": 3, "resets_at": None}}), "5 小时已用 3%")

    def test_parse_iso_fractional_seconds(self):
        """桌面版（Rust）写的时间带 9 位小数秒；Python 3.10 及以前的 fromisoformat 只认 3 位或 6 位，先凑成 6 位再解析。"""
        cam = self.cam
        self.assertEqual(cam._iso_for_python("2026-11-10T12:24:19.123456789+00:00"), "2026-11-10T12:24:19.123456+00:00")
        self.assertEqual(cam._iso_for_python("2026-11-10T12:24:19.5Z"), "2026-11-10T12:24:19.500000+00:00")
        self.assertEqual(cam._iso_for_python("2026-11-10T12:24:19+08:00"), "2026-11-10T12:24:19+08:00")
        for raw, micro in (("2026-11-10T12:24:19.123456789+00:00", 123456), ("2026-11-10T12:24:19.5Z", 500000),
                           ("2026-11-10T12:24:19.12+00:00", 120000), ("2026-11-10T12:24:19Z", 0), ("2026-11-10 12:24:19.1234", 123400)):
            d = cam.parse_iso(raw)
            self.assertEqual((d.hour, d.minute, d.second, d.microsecond, d.utcoffset()), (12, 24, 19, micro, dt.timedelta(0)), raw)
        self.assertEqual(cam.parse_iso("2026-11-10T20:24:19.25+08:00"), cam.parse_iso("2026-11-10T12:24:19.250Z"))
        self.assertEqual(cam.parse_iso("2026-11-10").isoformat(), "2026-11-10T00:00:00+00:00")
        self.assertIsNone(cam.parse_iso("不是时间"))
        self.assertIsNone(cam.parse_iso(None))

    def test_server_does_not_log_client_disconnects(self):
        # 面板关掉、卡片收起时请求半路断开：不算出错，日志里不打调用栈；别的异常照常打
        srv = self.cam.ThreadingServer(("127.0.0.1", 0), http.server.BaseHTTPRequestHandler)
        try:
            for exc, noisy in ((ConnectionResetError(104, "reset"), False), (BrokenPipeError(32, "pipe"), False), (ValueError("x"), True)):
                err = io.StringIO()
                with contextlib.redirect_stderr(err):
                    try:
                        raise exc
                    except Exception:  # noqa: BLE001
                        srv.handle_error(None, ("127.0.0.1", 1))
                self.assertEqual(bool(err.getvalue()), noisy, exc)
        finally:
            srv.server_close()

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

    # ---- 通知：菜单栏点切换、cam serve 的后台任务

    def _menu_plugin(self):
        os.environ["CAM_MENU_NO_NOTIFY"] = "1"
        os.environ["CAM_MENU_REFRESH_MIN"] = "0"
        os.environ["CAM_MENU_MINI"] = "0"  # 测试里不去起 cam serve（卡片小面板要它）
        sys.path.insert(0, os.path.join(HERE, "..", "extras", "swiftbar"))
        import cam_menu
        cam_menu = importlib.reload(cam_menu)
        cam_menu.cam = self.cam
        return cam_menu

    def test_menubar_switch_reports_result_by_notification(self):
        a_id, b_id = self._two_accounts()  # 当前是 B
        cam_menu = self._menu_plugin()
        store = self.cam.Store()
        sent = []
        with mock.patch.object(self.cam, "notify", lambda t, b: sent.append((t, b))), \
                mock.patch.dict(os.environ, {"CAM_MENU_NO_NOTIFY": ""}):
            cam_menu.notify("标题", "正文")  # 插件和 cam serve 用同一个实现
        self.assertEqual(sent, [("标题", "正文")])
        told = []
        cam_menu.notify = lambda title, body: told.append((title, body))
        # 点切换跑的是插件自己的 switch 命令：SwiftBar 没有终端，cam 的输出没人看得到
        lines = []
        cam_menu.render(store, {}, out=lines.append)
        clicks = [l for l in lines if 'param2="switch"' in l]
        self.assertTrue(clicks and all(f'param1="{menu_arg(cam_menu.MENU_PY)}"' in l for l in clicks), clicks)
        # 有会话在跑：不打断，登记等待并通知
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.assertEqual(cam_menu.switch_from_menu(store, a_id), 0)
        self.assertEqual(told[-1][0], "等会话结束后切换")
        d = self.data()
        self.assertEqual((d["active"]["claude_code"], d["pending_switch"]["account"]), (b_id, a_id))
        # 会话退出后，插件下一轮切过去，再通知一次
        self.cam.running_claude_processes = lambda: []
        cam_menu.background_work(store)
        self.assertEqual(told[-1][0], "已切换账号")
        self.assertEqual(self.data()["active"]["claude_code"], a_id)
        # 没有会话：直接切；出错也通知
        self.assertEqual(cam_menu.switch_from_menu(store, b_id), 0)
        self.assertEqual(told[-1][0], "已切换账号")
        self.assertEqual(cam_menu.switch_from_menu(store, "nobody"), 1)
        self.assertEqual(told[-1][0], "切换失败")
        self.assertIn("找不到账号", told[-1][1])

    # ---- Windows 托盘小工具（extras/windows-tray）：菜单和 macOS 菜单栏小工具是同一份

    def _tray(self):
        self._menu_plugin()
        sys.path.insert(0, os.path.join(HERE, "..", "extras", "windows-tray"))
        import cam_tray
        return importlib.reload(cam_tray)

    def test_windows_tray_menu_matches_menubar(self):
        tray = self._tray()
        store = self.cam.Store()
        out = tray.menu(store, tray.Notes())
        self.assertEqual((out["badge"], out["level"]), ("!", "gray"))  # 还没有账号
        MockAnthropic.tokens = {"at-A1": {"u5": 10.0, "u7": 5.0, "email": "a@x.com", "plan": "max"},
                                "at-B1": {"u5": 75.0, "u7": 30.0, "email": "b@x.com", "plan": "max"}}
        a_id, b_id = self._two_accounts()  # 当前是 B
        self.assertEqual(self.run_cam("usage"), 0)
        out = tray.menu(store, tray.Notes())
        dumped = json.dumps(out, ensure_ascii=False)  # 原样交给 cam-tray.ps1
        # 图标：当前账号 5 小时额度的数字，70–89% 黄底
        self.assertEqual((out["badge"], out["level"]), ("75", "yellow"))
        self.assertIn("B", out["tip"])
        self.assertIn("75%", out["tip"])
        rows = [i for i in out["items"] if i.get("children")]
        a_row = next(r for r in rows if r["text"].startswith("A"))
        b_row = next(r for r in rows if r["text"].startswith("B"))
        self.assertTrue(b_row.get("checked"))
        self.assertFalse(a_row.get("checked"))
        self.assertEqual([i["text"] for i in out["items"] if i.get("checked")], [b_row["text"]])  # 只勾当前账号
        self.assertIn("■", a_row["text"])  # 进度条换成雅黑里一定有的字符
        self.assertNotIn("▰", dumped)
        # 切换在子菜单里，交给菜单栏小工具自己的 switch（结果用通知告诉用户），切完刷新菜单
        self.assertIn({"text": "切换到这个账号", "run": ["menu", "switch", a_id], "refresh": True}, a_row["children"])
        # 按住 Option 才显示的邮箱行没带过来；分隔线不连着、不在末尾
        self.assertFalse(any(i.get("text", "").startswith("a@x.com") for i in out["items"]))
        seps = [n for n, i in enumerate(out["items"]) if i.get("sep")]
        self.assertTrue(seps and all(b - a > 1 for a, b in zip(seps, seps[1:])), seps)
        self.assertFalse(out["items"][-1].get("sep"))
        self.assertIsNone(out["mini"])  # 测试里 CAM_MENU_MINI=0：没有卡片页，左键退回弹菜单
        acts = {i["text"]: i["run"] for i in out["items"] if i.get("run")}
        self.assertEqual(acts["打开管理面板"], ["cam", "panel"])
        self.assertEqual(acts["刷新用量"], ["cam", "usage"])
        self.assertEqual(acts["全面体检"], ["menu", "checkup"])
        more = next(i for i in out["items"] if i.get("text") == "更多")  # 不常用的收在“更多”里
        self.assertTrue(any(c.get("text") == "在终端看完整体检结果" and c.get("run") == ["terminal", "doctor"] for c in more["children"]), more)
        # 90% 以上红底，用满写“满”；后台定时刷新发现超过提醒阈值，通知交给托盘弹
        MockAnthropic.tokens["at-B1"]["u5"] = 100.0
        tray.cam_menu.REFRESH_MIN = 10
        out = tray.menu(store, tray.Notes())
        self.assertEqual((out["badge"], out["level"]), ("满", "red"))
        self.assertTrue(out["notes"])
        self.assertEqual(tray.parse_params('bash="/a b/py" param1="x \\"y\\"" refresh=true'),
                         {"bash": "/a b/py", "param1": 'x "y"', "refresh": "true"})

    def test_windows_tray_actions(self):
        a_id, b_id = self._two_accounts()  # 当前是 B
        tray = self._tray()
        store = self.cam.Store()
        notes = tray.Notes()
        self.assertEqual(tray.do(store, "menu", ["switch", a_id], notes), 0)
        self.assertEqual(notes.items[-1]["title"], "已切换账号")
        self.assertEqual(self.data()["active"]["claude_code"], a_id)
        self.assertEqual(tray.do(store, "menu", ["switch"], notes), 2)
        self.assertEqual(tray.do(store, "nothing", [], notes), 2)
        # cam 命令出错：最后一行报错写进通知
        self.assertEqual(tray.do(store, "cam", ["open", "nobody"], notes), 1)
        self.assertEqual(notes.items[-1]["title"], "没有完成")
        self.assertIn("nobody", notes.items[-1]["body"])
        # 新开终端：cmd /k 后面整串再包一层引号，路径里有空格也不会被拆开
        with mock.patch.object(tray.subprocess, "Popen") as popen:
            self.assertEqual(tray.do(store, "terminal", ["doctor"], notes), 0)
        line = popen.call_args[0][0]
        self.assertTrue(line.startswith('cmd.exe /k "') and line.endswith(' doctor"'), line)
        self.assertEqual(popen.call_args[1]["creationflags"], tray.CREATE_NEW_CONSOLE)
        # 命令行入口：输出一行 JSON
        buf = io.StringIO()
        with mock.patch.object(sys, "stdout", buf):
            self.assertEqual(tray.main(["do", "menu", "switch", b_id]), 0)
        self.assertEqual(json.loads(buf.getvalue())["notes"][-1]["title"], "已切换账号")

    def test_background_round_notifies_like_menubar(self):
        a_id, b_id = self._two_accounts()  # 当前是 B
        store = self.cam.Store()
        told, st = [], {}

        def tell(title, body):
            told.append(title)

        # 新登录的账号、额度超过阈值
        MockAnthropic.tokens = {"at-C1": {"u5": 95.0, "u7": 1.0, "email": "c@x.com", "plan": "max"}}
        self.login("at-C1", "rt-C1", "uuid-C", "c@x.com")
        self.cam.background_round(store, st, 10, tell)
        self.assertEqual(told, ["发现新账号", "c@x.com 的 5 小时额度已用 95%"])
        # 同一个额度窗口不重复提醒；等待中的切换完成了要说一声
        told.clear()
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.cam.api_switch(store, a_id, "wait")
        self.cam.running_claude_processes = lambda: []
        st["last_refresh"] = 0
        self.cam.background_round(store, st, 10, tell)
        self.assertEqual(told, ["已切换账号"])
        # 等待中的切换切不过去（目标账号的登录作废了）：登记作废，告诉用户
        told.clear()
        sec = store.load_secret(b_id)
        sec["claudeAiOauth"]["expiresAt"] = self._expired_ms()
        store.save_secret(b_id, sec)
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.cam.api_switch(store, b_id, "wait")
        self.cam.running_claude_processes = lambda: []
        self.cam.background_round(store, st, 10, tell)
        self.assertEqual(told, ["等待中的切换没有完成"])
        self.assertIn("已失效", st["last_error"])
        self.assertNotIn("pending_switch", self.data())
        # macOS：标题和正文作为参数交给 osascript，不拼进脚本
        calls = []
        with mock.patch.object(self.cam, "IS_MAC", True), mock.patch.dict(os.environ, {"CAM_NO_NOTIFY": ""}), \
                mock.patch.object(self.cam.subprocess, "run", lambda cmd, **kw: calls.append(cmd)):
            self.cam.notify('带"引号的标题', "正文")
        self.assertEqual((calls[0][0], calls[0][-2:]), ("osascript", ['带"引号的标题', "正文"]))

    @unittest.skipUnless(sys.platform.startswith("linux"), "用假的 notify-send 验证，只在 Linux 上跑")
    def test_serve_sends_system_notifications(self):
        self._two_accounts()
        bindir = os.path.join(self.tmp, "bin")
        os.makedirs(bindir)
        sent = os.path.join(self.tmp, "notify.txt")
        with open(os.path.join(bindir, "notify-send"), "w") as f:
            f.write(f"#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\" >> {sent}\n")
        os.chmod(os.path.join(bindir, "notify-send"), 0o755)
        # cam serve 拿着后台锁时菜单栏插件不再跑后台任务，通知要由 serve 自己弹出来
        MockAnthropic.tokens = {"at-C1": {"u5": 1.0, "u7": 1.0, "email": "c@x.com", "plan": "max"}}
        self.login("at-C1", "rt-C1", "uuid-C", "c@x.com")
        env = dict(os.environ, PATH=bindir + os.pathsep + os.environ.get("PATH", ""), PYTHONUNBUFFERED="1")
        env.pop("CAM_NO_NOTIFY", None)
        log = open(os.path.join(self.tmp, "serve.log"), "w")
        p = subprocess.Popen([sys.executable, os.path.join(HERE, "cam.py"), "serve", "--port", "0", "--refresh-every", "0"],
                             env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and not os.path.exists(sent):
                time.sleep(0.1)
        finally:
            p.terminate()
            p.wait(10)
            log.close()
        with open(os.path.join(self.tmp, "serve.log")) as f:
            out = f.read()
        self.assertTrue(os.path.exists(sent), out)
        with open(sent) as f:
            self.assertEqual(f.read().splitlines(), ["发现新账号|已自动保存：c@x.com"])
        self.assertIn("[通知] 发现新账号：已自动保存：c@x.com", out)

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
        MockAnthropic.tokens = {"at-A1": {"u5": 1.0, "u7": 1.0, "email": "a@x.com", "plan": "max"}}  # A 保存的登录是好的
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
        # A 保存的那份 access token 服务端还认（认不认由服务端定；不认的情况见 test_parked_login_guessed_for_another_account）
        MockAnthropic.tokens = {"at-A1": {"u5": 1.0, "u7": 1.0, "email": "a@x.com", "plan": "max"}}
        self.run_cam("switch", "A")
        MockAnthropic.refresh_map = {"rt-A2": ("at-A3", "rt-A3")}
        MockAnthropic.tokens = {"at-A3": {"u5": 1.0, "u7": 1.0, "email": "a@x.com", "plan": "max"}}
        self.run_cam("switch", "B")
        store = self.cam.Store()
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B1")  # B 用的是自己保存的
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual(store.load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A3")  # 换新后归到 A
        self.assertEqual(store.load_secret(self.cam.PENDING_ID), {})

    # ---- 暂存着更新的登录时，不在用的号不能被报成“登录已失效”；切换前先核对

    A_OK = {"u5": 1.0, "u7": 1.0, "email": "a@x.com", "plan": "max"}
    B_OK = {"u5": 7.0, "u7": 8.0, "email": "b@x.com", "plan": "max"}

    def _acc(self, acc_id):
        return next(a for a in self.data()["accounts"] if a["id"] == acc_id)

    def _token_calls(self):
        return [c[2] for c in MockAnthropic.calls if c[1] == "/v1/oauth/token"]

    def _parked_b_then_on_a(self, released_secs_ago=None):
        """B 换过 token（rt-B2），cam 运行时核对不了，暂存着；随后切到 A。B 名下保存的还是被顶掉的 rt-B1。
        服务端只认 rt-B2（换出 at-B3 / rt-B3）和 A 的 at-A1。"""
        a_id, b_id = self._switched_to_b()
        self.write_creds("at-B2", "rt-B2", expires_ms=self._expired_ms())
        MockAnthropic.tokens = {"at-A1": self.A_OK}
        self.run_cam("switch", "A")
        self.assertEqual(self.live()[0]["refreshToken"], "rt-A1")
        store = self.cam.Store()
        pend = store.load_secret(self.cam.PENDING_ID)
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B1")
        self.assertEqual((pend["candidate"], pend["claudeAiOauth"]["refreshToken"]), (b_id, "rt-B2"))
        self.assertTrue(pend.get("released_at"))  # 切走时记下：它从这时起不是当前登录
        if released_secs_ago:
            ago = dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=released_secs_ago)
            pend["released_at"] = ago.isoformat(timespec="seconds")
            store.save_secret(self.cam.PENDING_ID, pend)
        MockAnthropic.refresh_map = {"rt-B2": ("at-B3", "rt-B3")}
        MockAnthropic.tokens = {"at-A1": self.A_OK, "at-B3": self.B_OK}
        MockAnthropic.calls = []
        return a_id, b_id

    def _one_session(self, etime):
        """当作有一个 claude 会话在跑（pid 4321，已经运行了 etime）；看不出它用的哪个登录（macOS / Windows 就是这样）。"""
        self.cam.running_claude_processes = lambda: [(4321, "claude")]
        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(self.cam, "claude_sessions", return_value=None))
        stack.enter_context(mock.patch.object(self.cam, "_ps_etimes", return_value={4321: etime}))
        return stack

    def test_refresh_uses_parked_login_not_the_superseded_one(self):
        # 保存的 access token 本地看还没过期（服务端已经不认）
        a_id, b_id = self._parked_b_then_on_a()
        self.cam.refresh_usage(self.cam.Store())
        store, b = self.cam.Store(), self._acc(b_id)
        self.assertIsNone(b["usage"]["error"])
        self.assertEqual(b["usage"]["five_hour"]["utilization"], 7.0)
        self.assertEqual(self.cam.account_health(b, True, None)["level"], "ok")
        self.assertFalse(b.get("incidents"))
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B3")
        self.assertEqual(store.load_secret(self.cam.PENDING_ID), {})
        self.assertEqual(self._token_calls(), ["rt-B2"])  # 被顶掉的 rt-B1 一次都没拿去续
        self.assertEqual(self.live()[0]["refreshToken"], "rt-A1")  # 当前登录没动
        self.assertNotIn("live_unverified", self.data())

    def test_refresh_uses_parked_login_when_saved_one_expired(self):
        # 保存的 access token 也过期了（隔了几个小时再打开面板）：cam doctor 同样不报失效
        a_id, b_id = self._parked_b_then_on_a()
        store = self.cam.Store()
        sec = store.load_secret(b_id)
        sec["claudeAiOauth"]["expiresAt"] = self._expired_ms()
        store.save_secret(b_id, sec)
        b = self._group(self._section(self._checkup(), "accounts"), "B")
        self.assertFalse([i for i in b if i["level"] in ("bad", "warn")], b)
        self.assertEqual(b[-1]["text"], "账号状态：正常")
        self.assertEqual(self.cam.Store().load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B3")
        self.assertEqual(self._token_calls(), ["rt-B2"])

    def test_parked_login_held_by_session_is_not_login_invalid(self):
        a_id, b_id = self._parked_b_then_on_a(released_secs_ago=3600)
        # 这个会话在 B 让位之前就开着（跑了两小时），可能还拿着 rt-B2：两个 refresh token 都不去换，也不说登录失效
        with self._one_session("02:00:00"):
            self.cam.refresh_usage(self.cam.Store())
            b = self._acc(b_id)
            err = b["usage"]["error"]
            self.assertIn(self.cam.PENDING_HELD, err)
            self.assertIn("先不用重新登录", err)
            self.assertNotIn("登录已失效", err)
            h = self.cam.account_health(b, True, None)
            self.assertEqual((h["level"], h["label"], [i["kind"] for i in h["issues"]]), ("info", "正常", ["pending_login"]))
            self.assertFalse(b.get("incidents"))
            self.assertEqual(self._token_calls(), [])
            self.assertEqual(self.cam.Store().load_secret(self.cam.PENDING_ID)["claudeAiOauth"]["refreshToken"], "rt-B2")
            # cam doctor：一条说明，不是“要处理”
            items = self._group(self._section(self._checkup(), "accounts"), "B")
            self.assertFalse([i for i in items if i["level"] in ("bad", "warn")], items)
            self.assertTrue([i for i in items if self.cam.PENDING_HELD in i["text"] and "不用重新登录" in i["advice"]], items)
            self.assertEqual(self._token_calls(), [])
        # 进程没查出来（ps 出错、超时）：当作可能有，同样不换
        self.cam.running_claude_processes = lambda: self.cam.ProcsUnknown()
        self.cam.refresh_usage(self.cam.Store())
        self.assertIn(self.cam.PENDING_HELD, self._acc(b_id)["usage"]["error"])
        self.assertEqual(self._token_calls(), [])
        # 会话是 B 让位之后才开的（只跑了十分钟，读到的是 A 的登录）：不算，照常核对并用上
        with self._one_session("10:00"):
            self.cam.refresh_usage(self.cam.Store())
        self.assertIsNone(self._acc(b_id)["usage"]["error"])
        self.assertEqual(self.cam.Store().load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B3")
        self.assertEqual(self._token_calls(), ["rt-B2"])

    def test_pending_holders_skips_sessions_with_their_own_login(self):
        cam = self.cam
        self.assertEqual([cam._etime_secs(t) for t in ("00:05", "10:00", "02:00:00", "1-00:00:01", "x", "1:2:3:4")],
                         [5, 600, 7200, 86401, None, None])
        ago = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=1)).isoformat(timespec="seconds")
        self.assertEqual(cam.pending_holders({"released_at": ago}), 0)
        cam.running_claude_processes = lambda: [(1, "claude"), (2, "claude"), (3, "claude"), (4, "claude")]
        sessions = [{"pid": 1, "secs": 7200, "login": "machine"}, {"pid": 2, "secs": 7200, "login": "own_token"},
                    {"pid": 3, "secs": 60, "login": "machine"}, {"pid": 4, "secs": 7200, "login": "unknown"}]
        with mock.patch.object(cam, "claude_sessions", return_value=sessions):
            self.assertEqual(cam.pending_holders({"released_at": ago}), 2)  # 自带 token 的、让位之后才开的不算
            self.assertEqual(cam.pending_holders({}), 4)  # 还是当前登录（没让位过）：都算
        cam.running_claude_processes = lambda: cam.ProcsUnknown()
        self.assertIsNone(cam.pending_holders({"released_at": ago}))

    def test_switch_checks_parked_login_first(self):
        a_id, b_id = self._parked_b_then_on_a()
        store = self.cam.Store()
        with self._one_session("02:00:00"):
            # 自动的切换（规则、等待中的切换）不替它换 token：不切，当前登录不动，也不让人去重新登录
            with self.assertRaises(self.cam.CamError) as cm:
                self.cam.switch_code_account(store, b_id)
            msg = str(cm.exception)
            self.assertTrue(msg.startswith("没有切换：“B”更新的一份登录还暂存着没核对"), msg)
            self.assertIn("现在就切", msg)
            self.assertIn("先不用重新登录", msg)
            self.assertEqual(self.live()[0]["refreshToken"], "rt-A1")
            self.assertEqual(self.data()["active"]["claude_code"], a_id)
            self.assertEqual(self._token_calls(), [])
            # 用户说现在就切（面板的“现在就切”、cam switch -y）：核对暂存的那份并用上
            r = self.cam.api_switch(store, b_id, "now")
            self.assertTrue(r["ok"] and r["changed"], r)
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B3")
        self.assertEqual(self.cam.Store().load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B3")
        self.assertEqual(self.cam.Store().load_secret(self.cam.PENDING_ID), {})
        self.assertEqual(self._token_calls(), ["rt-B2"])

    def test_switch_network_error_while_checking_parked_login(self):
        a_id, b_id = self._parked_b_then_on_a()
        with mock.patch.object(self.cam, "TOKEN_URLS", ["http://127.0.0.1:1/v1/oauth/token"]):
            with self.assertRaises(self.cam.CamError) as cm:
                self.cam.switch_code_account(self.cam.Store(), b_id, force=True)
        msg = str(cm.exception)
        self.assertIn("续登录失败", msg)  # 网络问题照网络问题说
        self.assertIn("当前登录没有动", msg)
        self.assertNotIn("重新登录", msg)
        self.assertEqual(self.live()[0]["refreshToken"], "rt-A1")
        self.assertEqual(self.cam.Store().load_secret(self.cam.PENDING_ID)["claudeAiOauth"]["refreshToken"], "rt-B2")
        self.assertEqual(self.cam._health_kind_of_error("claude_code", self.cam.pending_held_error("续登录失败（网络错误：x），稍后再试")), "network")

    def test_parked_login_guessed_for_another_account(self):
        a_id, b_id = self._switched_to_b()
        self.run_cam("switch", "A")
        # 切到 A 之前就开着的会话续了 B 的登录、写回凭据文件；~/.claude.json 里还是 A，cam 运行时又核对不了：暂存，猜的是 A
        self.write_creds("at-B2", "rt-B2", expires_ms=self._expired_ms())
        self.run_cam("ls")
        store = self.cam.Store()
        self.assertEqual(store.load_secret(self.cam.PENDING_ID)["candidate"], a_id)
        MockAnthropic.refresh_map = {"rt-B2": ("at-B3", "rt-B3")}
        MockAnthropic.tokens = {"at-A1": self.A_OK, "at-B3": self.B_OK}
        MockAnthropic.calls = []
        # 刷新：B 保存的那份被拒绝了，但暂存的那份就是 Claude Code 现在用的登录，不能替它换：不下“失效”的结论
        self.cam.refresh_usage(store)
        err = self._acc(b_id)["usage"]["error"]
        self.assertIn("保存的那份登录被服务端拒绝了", err)
        self.assertIn(self.cam.PENDING_HELD, err)
        self.assertEqual(self.cam.account_health(self._acc(b_id), True, None)["level"], "info")
        self.assertNotIn("rt-B2", self._token_calls())
        # 切到 B：保存的 access token 本地看没过期，但服务端不认；照着切过去 Claude Code 一续就要重新登录，所以不切
        MockAnthropic.calls = []
        with self.assertRaises(self.cam.CamError) as cm:
            self.cam.switch_code_account(store, b_id, force=True)
        self.assertIn("没有切换：“B”保存的那份登录被服务端拒绝了", str(cm.exception))
        self.assertIn("在 claude 里发一句话", str(cm.exception))
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B2")
        # Claude Code 换成了别的登录（重新登录了 A）：暂存的那份不再是当前登录，再切 B 时核对出是 B 的，用上
        self.login("at-A9", "rt-A9", "uuid-A", "a@x.com")
        MockAnthropic.tokens["at-A9"] = self.A_OK
        MockAnthropic.calls = []
        acc, _, changed = self.cam.switch_code_account(self.cam.Store(), b_id, force=True)
        self.assertTrue(changed)
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B3")
        store = self.cam.Store()
        self.assertEqual(store.load_secret(b_id)["claudeAiOauth"]["refreshToken"], "rt-B3")
        self.assertEqual(store.load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A9")
        self.assertEqual(store.load_secret(self.cam.PENDING_ID), {})

    def test_rejected_access_token_is_renewed_once(self):
        a_id, b_id = self._two_accounts()  # B 是当前登录；A 保存的 access token 本地看没过期
        MockAnthropic.tokens = {"at-B1": self.B_OK, "at-A2": self.A_OK}  # 服务端不认 at-A1 了
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2")}
        self.cam.refresh_usage(self.cam.Store())
        a = self._acc(a_id)
        self.assertIsNone(a["usage"]["error"])
        self.assertEqual(self.cam.Store().load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A2")
        self.assertEqual(self._token_calls(), ["rt-A1"])
        # 当前登录被拒绝：从不替它续，原样报
        MockAnthropic.tokens = {"at-A2": self.A_OK}
        MockAnthropic.refresh_map = {"rt-B1": ("at-B2", "rt-B2")}
        MockAnthropic.calls = []
        self.cam.refresh_usage(self.cam.Store())
        self.assertIn("401", self._acc(b_id)["usage"]["error"])
        self.assertEqual(self._token_calls(), [])
        # 后台那轮（不许换 token）也不续
        MockAnthropic.tokens = {"at-B1": self.B_OK}
        MockAnthropic.refresh_map = {"rt-A2": ("at-A3", "rt-A3")}
        MockAnthropic.calls = []
        self.cam.refresh_usage(self.cam.Store(), allow_token_refresh=False)
        self.assertEqual(self._token_calls(), [])

    def test_token_endpoint_answers_that_do_not_mean_dead(self):
        cam = self.cam
        dead = cam._token_answer_dead
        self.assertTrue(dead(400, {"error": "invalid_grant", "error_description": "Refresh token not found or invalid"}))
        self.assertTrue(dead(400, {"error": {"type": "invalid_request_error", "message": "invalid_grant"}}))
        self.assertTrue(dead(401, {"error": {"message": "revoked"}}))
        self.assertFalse(dead(400, {"error": {"message": "<html>Bad Request</html>"}, "_not_json": True}))  # 代理、网关回的
        self.assertFalse(dead(400, {"error": "invalid_client"}))  # 请求本身不对，和这个账号的登录无关
        self.assertFalse(dead(400, {"error": {"type": "invalid_request_error", "message": "unsupported_grant_type"}}))
        self.assertFalse(dead(403, {"error": {"message": "forbidden"}}))
        self.assertFalse(dead(400, ["x"]))
        self.assertFalse(dead(500, {"error": "invalid_grant"}))
        for st, body in ((400, {"error": {"message": "<html>blocked</html>"}, "_not_json": True}), (400, {"error": "invalid_client"})):
            with mock.patch.object(cam, "http_json", return_value=(st, body)):
                with self.assertRaises(cam.CamError) as cm:
                    cam.refresh_oauth("rt")
            self.assertIn("续登录失败", str(cm.exception))
            self.assertEqual(cam._health_kind_of_error("claude_code", str(cm.exception)), "network")
        with mock.patch.object(cam, "http_json", return_value=(400, {"error": "invalid_grant"})):
            with self.assertRaises(cam.CamError) as cm:
                cam.refresh_oauth("rt")
        self.assertEqual(cam._health_kind_of_error("claude_code", str(cm.exception)), "login_invalid")

    def test_http_json_marks_non_json_error_bodies(self):
        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length") or 0))
                body = b"<html>Bad Request</html>" if self.path == "/html" else b'["x"]'
                self.send_response(400)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        try:
            base = f"http://127.0.0.1:{srv.server_address[1]}"
            for path in ("/html", "/list"):
                st, body = self.cam.http_json("POST", base + path, body={"a": 1})
                self.assertEqual(st, 400)
                self.assertTrue(isinstance(body, dict) and body.get("_not_json"), body)
                self.assertFalse(self.cam._token_answer_dead(st, body))
        finally:
            srv.shutdown()
            srv.server_close()

    def test_login_invalid_survives_background_round(self):
        a_id, b_id = self._two_accounts(a_expires=self._expired_ms())  # 假接口不认 rt-A1：A 的登录确实作废了
        MockAnthropic.tokens = {"at-B1": self.B_OK}
        store = self.cam.Store()
        self.cam.refresh_usage(store)
        a = self._acc(a_id)
        self.assertEqual(self.cam.account_health(a, True, None)["level"], "error")
        self.assertEqual(a["usage"]["error_fp"], a["token_fp"])
        self.assertEqual(len(a["incidents"]), 1)
        # 后台那轮不替它续，只会得到“登录过期了”：结论还是失效，现场也不重复记
        self.cam.refresh_usage(store, allow_token_refresh=False)
        a = self._acc(a_id)
        self.assertIn("登录已失效", a["usage"]["error"])
        self.assertEqual(self.cam.account_health(a, True, None)["level"], "error")
        self.assertEqual(len(a["incidents"]), 1)
        # 重新登录 A 并保存后（token 换了）按新的结果来
        self.login("at-A5", "rt-A5", "uuid-A", "a@x.com")
        MockAnthropic.tokens["at-A5"] = self.A_OK
        self.cam.refresh_usage(store, allow_token_refresh=False)
        a = self._acc(a_id)
        self.assertIsNone(a["usage"]["error"])
        self.assertNotIn("error_fp", a["usage"])
        # 没查出过失效的号，后台那轮的“登录过期了”还是提示，不是异常
        keep = self.cam.keep_login_invalid
        u = {"error": "登录已过期。后台不会替它续登录（避免和 Claude Code 抢 refresh token），手动刷新一次或切换到这个账号即可"}
        acc = {"kind": "claude_code", "token_fp": "f1", "usage": {"error": "登录已失效（x）", "error_fp": "f0"}}
        keep(acc, u)
        self.assertNotIn("登录已失效", u["error"])  # 之后保存的登录换过：不沿用
        self.assertNotIn("error_fp", u)

    def test_unknown_process_list_defers_automatic_switch(self):
        cam = self.cam
        a_id, b_id = self._two_accounts()
        store = cam.Store()
        self.assertFalse(cam.claude_may_be_running([]))
        self.assertTrue(cam.claude_may_be_running([(1, "claude")]))
        self.assertTrue(cam.claude_may_be_running(cam.ProcsUnknown()))
        with store.lock():
            data = store.load()
            cam.defer_switch(data, a_id, "manual")
            store.save(data)
        cam.running_claude_processes = lambda: cam.ProcsUnknown()
        self.assertIsNone(cam.apply_pending_switch(store))  # 没查出来：当作有会话，先不切
        self.assertEqual(self.data()["active"]["claude_code"], b_id)
        self.assertTrue(self.data().get("pending_switch"))
        cam.running_claude_processes = lambda: []
        self.assertEqual(cam.apply_pending_switch(store)[0]["id"], a_id)
        # 面板上的会话数：这次没查出来就沿用上次的，不归零
        cam.running_claude_processes = lambda: [(1, "claude"), (2, "claude")]
        self.assertEqual(cam.session_count(max_age=0), 2)
        cam.running_claude_processes = lambda: cam.ProcsUnknown()
        self.assertEqual(cam.session_count(max_age=0), 2)

    # ---- cam login：先存好当前账号，再换号登录

    def _fake_claude_login(self, fn):
        """把 cam login 里的 claude auth login 换成 fn（模拟在浏览器里登录），claude 命令当作已安装。"""
        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(self.cam, "claude_auth_login", fn))
        stack.enter_context(mock.patch.object(self.cam.shutil, "which",
                                              lambda name, *a, **k: "/fake/bin/claude" if name == "claude" else None))
        return stack

    def test_login_saves_current_login_before_adding_new(self):
        self.login("at-A1", "rt-A1", "uuid-A", "a@x.com")
        self.run_cam("import", "--label", "A")
        a_id = self.data()["accounts"][0]["id"]
        # import 之后 Claude Code 自己续过 A 的 token：cam 还只存着 rt-A1，它在服务端已经作废了
        self.write_creds("at-A2", "rt-A2")
        MockAnthropic.tokens = {t: {"u5": 1.0, "u7": 1.0, "email": e, "plan": "max"}
                                for t, e in (("at-A2", "a@x.com"), ("at-B1", "b@x.com"), ("at-B2", "b@x.com"))}
        logins = []

        def login_b(exe):
            logins.append(exe)
            self.assertEqual(self.cam.Store().load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A2")  # 换号前已存好
            self.login("at-B1", "rt-B1", "uuid-B", "b@x.com")  # 浏览器里登录了 B，替换当前登录
            return 0

        out = io.StringIO()
        with self._fake_claude_login(login_b), contextlib.redirect_stdout(out):
            self.assertEqual(self.run_cam("login", "B"), 0)
        self.assertEqual(logins, ["/fake/bin/claude"])
        self.assertIn("已添加：B", out.getvalue())
        d = self.data()
        b = [a for a in d["accounts"] if a.get("email") == "b@x.com"][0]
        self.assertEqual((b["label"], d["active"]["claude_code"]), ("B", b["id"]))
        self.assertEqual(self.cam.Store().load_secret(b["id"])["claudeAiOauth"]["refreshToken"], "rt-B1")
        # 切回 A 用的是续过的那份
        self.assertEqual(self.run_cam("switch", "A"), 0)
        self.assertEqual(self.live()[0]["refreshToken"], "rt-A2")
        # 重新登录已有的账号：更新它，不重复添加
        with self._fake_claude_login(lambda exe: self.login("at-B2", "rt-B2", "uuid-B", "b@x.com") or 0):
            self.assertEqual(self.run_cam("login"), 0)
        self.assertEqual(len(self.data()["accounts"]), 2)
        self.assertEqual(self.cam.Store().load_secret(b["id"])["claudeAiOauth"]["refreshToken"], "rt-B2")
        # claude auth login 没完成（取消、太旧没有这个命令）：报错，数据不动
        with self._fake_claude_login(lambda exe: 1), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(self.run_cam("login"), 1)
        self.assertEqual(len(self.data()["accounts"]), 2)

    def test_login_refuses_when_current_login_cannot_be_verified(self):
        a_id, b_id = self._switched_to_b()
        # B 换过 token，access token 又过期了：认不出是谁的，这时换号登录就会把它丢掉
        self.write_creds("at-B2", "rt-B2", expires_ms=self._expired_ms())
        called = []
        err = io.StringIO()
        with self._fake_claude_login(lambda exe: called.append(exe) or 0), contextlib.redirect_stderr(err):
            self.assertEqual(self.run_cam("login"), 1)
        self.assertEqual(called, [])
        self.assertIn("没有换号登录", err.getvalue())
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B2")
        self.assertEqual(self.cam.Store().load_secret(self.cam.PENDING_ID)["claudeAiOauth"]["refreshToken"], "rt-B2")

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

    def test_switch_without_terminal_waits_instead(self):
        a_id, b_id = self._two_accounts()  # 当前是 B
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        out = io.StringIO()
        with mock.patch.object(sys, "stdin", io.StringIO()), contextlib.redirect_stdout(out):  # 没有终端（菜单栏、脚本）
            self.assertEqual(self.run_cam("switch", "A"), 0)
        d = self.data()
        self.assertEqual((d["active"]["claude_code"], d["pending_switch"]["account"]), (b_id, a_id))  # 没打断会话
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B1")
        self.assertIn("等它们退出后会自动切换到 A", out.getvalue())
        self.assertIn("cam switch --cancel-wait", out.getvalue())
        # --now（同 -y）照常马上切
        with mock.patch.object(sys, "stdin", io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.run_cam("switch", "A", "--now"), 0)
        d = self.data()
        self.assertEqual(d["active"]["claude_code"], a_id)
        self.assertNotIn("pending_switch", d)

    def test_manual_pending_switch_expires_and_is_listed(self):
        a_id, b_id = self._two_accounts()
        store = self.cam.Store()
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.cam.api_switch(store, a_id, "wait")
        # cam / cam ls 里看得到，并说明怎么取消
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(self.run_cam("ls"), 0)
        self.assertIn("自动切换到“A”", out.getvalue())
        self.assertIn("cam switch --cancel-wait", out.getvalue())

        def registered_hours_ago(h):
            with store.lock():
                d = store.load()
                d["pending_switch"]["at"] = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=h)).isoformat()
                store.save(d)

        # 放了 7 小时：不再显示；会话还在也马上清掉，之后不会再切
        registered_hours_ago(7)
        self.assertIsNone(self.cam.public_state(store, self.data())["pending_switch"])
        self.assertIsNone(self.cam.apply_pending_switch(store))
        self.assertNotIn("pending_switch", self.data())
        self.cam.running_claude_processes = lambda: []
        self.assertIsNone(self.cam.apply_pending_switch(store))
        self.assertEqual(self.data()["active"]["claude_code"], b_id)
        # 5 小时的照常等，会话退出后切过去
        self.cam.running_claude_processes = lambda: [(1, "claude")]
        self.cam.api_switch(store, a_id, "wait")
        registered_hours_ago(5)
        self.assertEqual(self.cam.public_state(store, self.data())["pending_switch"]["account"], a_id)
        self.cam.running_claude_processes = lambda: []
        self.assertEqual(self.cam.apply_pending_switch(store)[0]["id"], a_id)
        # 时间规则的登记跟着时间段作废，不按这个过期
        self.assertFalse(self.cam.pending_switch_expired({"reason": "rule", "at": "2000-01-01T00:00:00+00:00"}))

    @unittest.skipIf(os.name == "nt", "Windows 没有 SIGHUP")
    def test_wait_is_cancelled_when_terminal_closes(self):
        a_id, b_id = self._two_accounts()  # 当前是 B
        self.cam.WAIT_POLL_SECS = 0
        self.addCleanup(setattr, self.cam, "WAIT_POLL_SECS", 5)
        handler = signal.getsignal(signal.SIGHUP)
        for sig in (signal.SIGHUP, signal.SIGTERM):
            seen = []

            def procs():
                seen.append(1)
                if len(seen) == 3:  # 等了一会儿：终端被关掉（SIGHUP）/ 进程被 kill（SIGTERM）
                    os.kill(os.getpid(), sig)
                return [(4242, "claude")]

            self.cam.running_claude_processes = procs
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(self.run_cam("switch", "A", "--wait"), 0)
            d = self.data()
            self.assertNotIn("pending_switch", d)  # 没留下一个看不见的登记
            self.assertEqual(d["active"]["claude_code"], b_id)
        self.assertEqual(signal.getsignal(signal.SIGHUP), handler)  # 等完恢复原来的处理

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
        self.assertTrue(all(set(i) - {"ms"} == {"kind", "level", "text", "advice"} for i in items[1:]))
        self.assertTrue(all(isinstance(i["ms"], int) for i in items if i["kind"] in ("api", "web")))  # 延迟，cam doctor 单列
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
            self.assertEqual(lc["level"], "info")  # 语言只代表语言习惯，只作参考
            self.assertEqual(self.cam.risk_score([lc])["score"], 95)
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
        MockAnthropic.ipapi_type = None  # ipapi.is 查询失败 → 退到 ipinfo.io（没有标记）→ 按运营商名判断
        self.cam.check_region(store, max_age=0)
        self.assertIsNone(self.data()["region"]["ip_type"])
        self.assertEqual(self.data()["region"]["meta_source"], "ipinfo.io")
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

    def test_ip_lookup_sources_and_privacy(self):
        a_id, b_id = self._two_accounts()
        store = self.cam.Store()
        hits = lambda prefix: [c[1] for c in MockAnthropic.calls if c[1].startswith(prefix)]
        # 默认：ipapi.is（带 key，有类型标记），不碰 ip-api.com
        MockAnthropic.calls = []
        self.cam.check_region(store, max_age=0)
        reg = self.data()["region"]
        self.assertEqual((reg["meta_source"], reg["ip_type"], reg["lookup_mode"]), ("ipapi.is", "residential", "auto"))
        self.assertEqual(hits("/ipapiis"), ["/ipapiis?key=test-key"])
        self.assertFalse([p for p in hits("/ipapi") if not p.startswith("/ipapiis")])
        self.assertFalse(hits("/rdap/"))  # 带 key 的 ipapi.is 自己给注册国
        self.assertIn("（数据：ipapi.is）", next(i for i in self.cam.service_check(store) if i["kind"] == "isp")["text"])
        # 没配 key：ipapi.is 简版，没有类型标记，另查 RDAP 拿注册国
        os.environ["CAM_IPAPI_IS_KEY"] = ""
        try:
            MockAnthropic.calls = []
            self.cam.check_region(store, max_age=0)
            reg = self.data()["region"]
            self.assertEqual((reg["meta_source"], reg["ip_type"], reg["org"], reg["asn"]),
                             ("ipapi.is", None, "Comcast Cable Communications, LLC", 7922))
            self.assertEqual(hits("/ipapiis"), ["/ipapiis"])
            self.assertEqual(len(hits("/rdap/")), 1)
            # 保存的 key 生效，且不写进 accounts.json
            pv = self.cam.save_privacy(store, {"ipapi_key": "saved-key", "ipinfo_token": "tok"})
            self.assertTrue(pv["has_ipapi_key"] and pv["has_ipinfo_token"])
            self.assertNotIn("saved-key", open(store.accounts_path, encoding="utf-8").read())
            self.assertIsNone(self.data()["region"].get("meta_at"))  # 换了 key，缓存的运营商信息作废
            MockAnthropic.calls = []
            self.cam.check_region(store, max_age=0)
            self.assertEqual(hits("/ipapiis"), ["/ipapiis?key=saved-key"])
            self.assertFalse(self.cam.save_privacy(store, {"ipapi_key": ""})["has_ipapi_key"])
        finally:
            os.environ["CAM_IPAPI_IS_KEY"] = "test-key"
        # ip-api 模式：先问 ip-api.com
        pv = self.cam.save_privacy(store, {"ip_lookup": "ip-api"})
        self.assertEqual(pv["ip_lookup"], "ip-api")
        self.assertTrue(any("ip-api.com" in s["name"] and s["warn"] for s in pv["sources"]))
        self.cam.check_region(store, max_age=0)
        self.assertEqual(self.data()["region"]["meta_source"], "ip-api.com")
        # 关闭：只问 claude.ai 和状态页
        pv = self.cam.save_privacy(store, {"ip_lookup": "off"})
        self.assertEqual([s["name"] for s in pv["sources"]], ["claude.ai/cdn-cgi/trace", "status.anthropic.com"])
        self.assertEqual({s["group"] for s in pv["sources"]}, {"anthropic"})
        self.assertEqual([m["title"] for m in pv["modes"]], ["自动（推荐）", "加上 ip-api.com", "关闭"])
        MockAnthropic.calls = []
        self.cam.check_region(store, max_age=0)
        self.assertEqual(sorted({c[1] for c in MockAnthropic.calls}), ["/cdn-cgi/trace", "/status.json"])
        reg = self.data()["region"]
        self.assertEqual((reg["loc"], reg["lookup_mode"], reg["org"], reg["ip_type"]), ("US", "off", None, None))
        items = self.cam.service_check(store)
        self.assertIn("外部 IP 查询已关闭", next(i for i in items if i["kind"] == "isp")["text"])
        self.assertFalse([i for i in items if i["kind"] in ("ipv6", "ip_origin")])
        self.assertEqual(self.cam.public_state(store, self.data())["region"]["lookup_mode"], "off")
        with self.assertRaises(self.cam.CamError):
            self.cam.save_privacy(store, {"ip_lookup": "everything"})
        self.cam.save_privacy(store, {"ip_lookup": "auto"})
        # 清除地区记录
        self.cam.check_region(store, max_age=0)
        self.assertTrue(any(a.get("regions") for a in self.data()["accounts"]))
        pv = self.cam.clear_region_history(store)
        self.assertEqual(pv["history"], {"accounts": 0, "checked_at": None})
        self.assertNotIn("region", self.data())
        # 命令行
        self.assertEqual(self.run_cam("risk", "--ip-lookup", "off"), 0)
        self.assertEqual(self.cam.privacy_view(store)["ip_lookup"], "off")
        self.assertEqual(self.run_cam("risk", "--ip-lookup", "auto", "--clear-history"), 0)
        self.assertEqual(self.cam.privacy_view(store)["ip_lookup"], "auto")

    def test_meta_cached_while_exit_ip_unchanged(self):
        self._two_accounts()
        store = self.cam.Store()
        old_ttl = self.cam.META_TTL
        self.cam.META_TTL = 86400
        try:
            MockAnthropic.calls = []
            self.cam.check_region(store, max_age=0)
            self.cam.check_region(store, max_age=0)
            self.assertEqual(len([c for c in MockAnthropic.calls if c[1].startswith("/ipapiis")]), 1)
            self.assertEqual(self.data()["region"]["org"], "Comcast Cable Communications, LLC")
            MockAnthropic.trace_ip = "5.6.7.8"  # 出口 IP 变了才重查
            self.cam.check_region(store, max_age=0)
            self.assertEqual(len([c for c in MockAnthropic.calls if c[1].startswith("/ipapiis")]), 2)
        finally:
            self.cam.META_TTL = old_ttl

    def test_stale_environment_results_are_labelled(self):
        """后台没在跑时环境检测结果会很旧：从那次结果算出来的提示标上是多久以前测的，旧的官方故障不再提，并说怎么重新检测。"""
        self._two_accounts()
        store = self.cam.Store()

        def put(checked_at, sp_at="same"):
            with store.lock():
                d = store.load()
                d["region"] = {"loc": "US", "ip": "1.2.3.4", "org": "Amazon.com, Inc.", "asn": 16509, "ip_type": "hosting", "skew": 300,
                               "warp": False, "checked_at": checked_at,
                               "env": {"tz": "America/New_York", "tz_cc": ["US"], "locale": None, "locale_cc": None, "dns": [],
                                       "proxy": ["HTTPS_PROXY=http://127.0.0.1:7890"], "tunnel": None}}
                d["status_page"] = {"indicator": "major", "description": "Partial System Outage"}
                if sp_at:
                    d["status_page"]["checked_at"] = checked_at if sp_at == "same" else sp_at
                store.save(d)
            return self.data()

        def kinds(items):
            return [r["kind"] for r in items]

        ago = lambda **kw: (dt.datetime.now(dt.timezone.utc) - dt.timedelta(**kw)).replace(microsecond=0).isoformat()
        # 刚测的：原样，官方故障照常提
        fresh = self.cam.risk_items(put(self.cam.now_iso()))
        self.assertEqual(kinds(fresh), ["proxy", "isp", "clock", "status", "accounts"])
        self.assertFalse([r for r in fresh if "检测的）" in r["text"]])
        self.assertEqual(self.cam.risk_score(fresh)["score"], 100 - 5 - 20 - 10 - 30)
        # 两天前测的：每条标出来，两天前的故障不算，多一条 env_stale；分数只少了故障那一项
        old = self.cam.risk_items(put(ago(days=2)))
        self.assertEqual(kinds(old), ["proxy", "isp", "clock", "accounts", "env_stale"])
        by = {r["kind"]: r for r in old}
        for k in ("proxy", "isp", "clock"):
            self.assertTrue(by[k]["text"].endswith("（2 天前检测的）"), by[k]["text"])
        self.assertNotIn("检测的）", by["accounts"]["text"])
        self.assertEqual((by["env_stale"]["level"], by["env_stale"]["text"]),
                         ("info", "环境检测结果是 2 天前的，出口、代理、时区这些可能已经变了"))
        self.assertIn("cam doctor", by["env_stale"]["advice"])
        self.assertEqual(self.cam.risk_score(old)["score"], 100 - 5 - 20 - 10)
        # 调用方自己说了是多久前的（离线体检）：不标、不加 env_stale，旧故障同样不提
        plain = self.cam.risk_items(self.data(), note_age=False)
        self.assertEqual(kinds(plain), ["proxy", "isp", "clock", "accounts"])
        self.assertFalse([r for r in plain if "检测的）" in r["text"]])
        # 旧版的状态页没单独记时间：按出口那次检测的时间算
        self.assertIn("status", kinds(self.cam.risk_items(put(self.cam.now_iso(), sp_at=None))))
        self.assertNotIn("status", kinds(self.cam.risk_items(put(ago(hours=3), sp_at=None))))
        # 出口刚测过、状态页是更早那次的（不该出现，出现了也不提旧故障）
        self.assertNotIn("status", kinds(self.cam.risk_items(put(self.cam.now_iso(), sp_at=ago(hours=2)))))
        # 一小时以内不算旧
        self.assertNotIn("env_stale", kinds(self.cam.risk_items(put(ago(minutes=50)))))
        # cam status / cam risk / 切换时的提醒都带上时间；离线体检开头说一次，下面每条不重复
        put(ago(days=2))

        def printed(*args):
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                self.run_cam(*args)
            return buf.getvalue()

        out = printed("ls")
        self.assertIn("（2 天前检测的）", out)
        self.assertIn("（2 天前检测；cam doctor 实测并查看完整的服务检查）", out)
        out = printed("risk")
        self.assertIn("当前（2 天前检测）：", out)
        self.assertIn("重新检测：运行 cam doctor", out)
        res = self.cam.checkup_service(store, offline=True)
        self.assertIn("（2 天前）", res[0]["text"])
        self.assertFalse([i for i in res if "检测的）" in i["text"] or i.get("kind") == "env_stale"])
        self.assertIn("isp", [i.get("kind") for i in res])
        # 启动 claude 前的快速检测把出口换成刚测的：不再标旧
        d2 = self.cam.with_quick_region(self.data(), {"loc": "US", "ip": "1.2.3.4", "colo": "SJC", "ms": 10, "warp": False, "skew": 0})
        self.assertFalse([r for r in self.cam.risk_items(d2) if "检测的）" in r["text"] or r["kind"] == "env_stale"])

    def test_background_error_is_not_wiped_by_the_same_round(self):
        """后台一轮里环境检测（或更前面的步骤）出了错，后面刷新用量成功也不把这条错误清掉；下一次整轮都没出错才清。"""
        self._two_accounts()
        store = self.cam.Store()
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}
        st, told = {}, []
        tell = lambda title, body: told.append(title)
        with mock.patch.object(self.cam, "check_region", side_effect=OSError("boom")):
            self.cam.background_round(store, st, 10, tell)
        self.assertEqual(st.get("last_error"), "环境检测：boom")
        self.assertTrue((self.data()["accounts"][1].get("usage") or {}).get("five_hour"))  # 用量照样刷新了
        # 没到刷新时间的轮次不动它
        self.cam.background_round(store, st, 10, tell)
        self.assertEqual(st.get("last_error"), "环境检测：boom")
        # 下一次刷新整轮没出错：清掉
        st["last_refresh"] = 0
        self.cam.background_round(store, st, 10, tell)
        self.assertNotIn("last_error", st)
        # 更前面的步骤（时间规则）出错，同一轮刷新成功：也留着
        st["last_refresh"] = 0
        with mock.patch.object(self.cam, "apply_time_rules", side_effect=self.cam.CamError("规则坏了")):
            self.cam.background_round(store, st, 10, tell)
        self.assertEqual(st.get("last_error"), "时间规则：规则坏了")
        # 刷新用量自己出错：记的是它
        st["last_refresh"] = 0
        with mock.patch.object(self.cam, "refresh_usage", side_effect=self.cam.CamError("读不了")):
            self.cam.background_round(store, st, 10, tell)
        self.assertEqual(st.get("last_error"), "刷新用量：读不了")

    def test_quick_region_for_launch(self):
        self._two_accounts()
        store = self.cam.Store()
        self.assertFalse(self.cam.spawn_background_check())  # 测试里关掉了后台进程
        MockAnthropic.trace_loc = "HK"
        self.cam.check_region(store, max_age=0)
        with store.lock():
            d = store.load()
            d["region"]["checked_at"] = "2020-01-01T00:00:00+00:00"  # 很久以前查的
            store.save(d)
        MockAnthropic.trace_loc, MockAnthropic.trace_ip = "US", "5.6.7.8"
        q = self.cam.quick_trace(3)
        self.assertEqual((q["loc"], q["ip"]), ("US", "5.6.7.8"))
        d2 = self.cam.with_quick_region(self.data(), q)
        self.assertEqual((d2["region"]["loc"], d2["region"]["prev_loc"]), ("US", "HK"))
        self.assertNotIn("org", d2["region"])  # 出口 IP 变了，旧运营商信息不再适用
        self.assertEqual(self.data()["region"]["loc"], "HK")  # 只是副本，不保存
        self.assertIn("region_change", [r["kind"] for r in self.cam.risk_items(d2)])
        MockAnthropic.calls = []
        self.cam.risk_prompt_before_launch(store, ask=lambda _: "y", interactive=True)
        self.assertEqual([c[1] for c in MockAnthropic.calls], ["/cdn-cgi/trace"])
        # 后台进程的入口会做完整检测并保存
        self.cam.background_region_check(store)
        self.assertEqual(self.data()["region"]["ip"], "5.6.7.8")

    def test_ssl_fallback_and_consistent_scores(self):
        import ssl
        # python.org 版 Python 没带根证书：默认一张都没加载到时，改用系统自带的证书文件，照常校验
        real = (ssl.create_default_context, ssl.get_default_verify_paths, self.cam.CA_FALLBACKS)
        bundle = next((f for f in ("/etc/ssl/certs/ca-certificates.crt", "/etc/ssl/cert.pem", "/etc/pki/tls/certs/ca-bundle.crt")
                       if os.path.isfile(f)), None)
        if bundle:
            empty = os.path.join(self.tmp, "no-such-dir")
            try:
                ssl.create_default_context = lambda: ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
                ssl.get_default_verify_paths = lambda: real[1]()._replace(capath=empty, cafile=None)
                self.cam.CA_FALLBACKS = ("/nonexistent/cert.pem", bundle)
                env = {k: v for k, v in os.environ.items() if k != "SSL_CERT_FILE"}
                with mock.patch.dict(os.environ, env, clear=True), mock.patch.dict(sys.modules, {"certifi": None}):
                    ctx, used = self.cam.make_ssl_context()  # 没有 certifi：用系统证书文件
                self.assertEqual(used, bundle)
                self.assertGreater(ctx.cert_store_stats()["x509_ca"], 0)
                fake = type(sys)("certifi")
                fake.where = lambda: bundle
                self.cam.CA_FALLBACKS = ("/nonexistent/cert.pem",)
                with mock.patch.dict(os.environ, env, clear=True), mock.patch.dict(sys.modules, {"certifi": fake}):
                    ctx, used = self.cam.make_ssl_context()  # 装了 certifi：优先用它
                self.assertEqual(used, bundle)
                with mock.patch.dict(os.environ, dict(env, SSL_CERT_FILE=bundle), clear=True), \
                        mock.patch.dict(sys.modules, {"certifi": None}):
                    self.assertEqual(self.cam.make_ssl_context()[1], bundle)  # SSL_CERT_FILE 最优先
            finally:
                ssl.create_default_context, ssl.get_default_verify_paths, self.cam.CA_FALLBACKS = real
        self.assertTrue(self.cam.is_cert_error("[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed"))
        # 出口查不到：顶部分数不再是 100，并单独扣分（不按“不在名单”扣 60）
        self._two_accounts()
        store = self.cam.Store()
        with store.lock():
            d = store.load()
            d["region"] = {"loc": None, "checked_at": self.cam.now_iso()}
            store.save(d)
        risks = self.cam.risk_items(self.data())
        self.assertIn("region_unknown", [r["kind"] for r in risks])
        self.assertEqual(self.cam.risk_score(risks)["score"], 70)
        # 服务检测发现 api / claude.ai 连不上：记下来，顶部分数和服务检测一致
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}  # 当前登录可用
        MockAnthropic.cloudflare = True  # claude.ai 返回人机验证页 → web warn
        items = self.cam.service_check(store)
        ss = self.data().get("service_status") or {}
        self.assertEqual([i["kind"] for i in ss.get("items", [])], ["web"])
        self.assertIn("web", [r["kind"] for r in self.cam.risk_items(self.data())])
        self.assertEqual(self.cam.risk_score(self.cam.risk_items(self.data()))["score"], items[0]["score"])
        # 后台连上 claude.ai 的 trace 证明不了人机验证已经没了：这条留着，顶部分数仍和服务检测一致
        self.cam.check_region(store, max_age=0)
        self.assertEqual([i["kind"] for i in self.data()["service_status"]["items"]], ["web"])
        self.assertEqual(self.cam.risk_score(self.cam.risk_items(self.data()))["score"], items[0]["score"])
        # 下一次服务检测发现已经好了，才清掉
        MockAnthropic.cloudflare = False
        self.cam.service_check(store)
        self.assertNotIn("service_status", self.data())
        self.assertEqual(self.cam.risk_score(self.cam.risk_items(self.data()))["score"], 100)
        # “连不上”这一类，后台连上 claude.ai 就能证明好了，直接清掉
        with store.lock():
            d = store.load()
            d["service_status"] = {"checked_at": self.cam.now_iso(), "items": [
                {"kind": "web", "level": "bad", "text": "claude.ai：连不上（网络错误）", "advice": ""},
                {"kind": "dns", "level": "bad", "text": "DNS：claude.ai 解析失败", "advice": ""},
                {"kind": "api", "level": "bad", "text": "api.anthropic.com：连不上", "advice": ""}]}
            store.save(d)
        self.cam.check_region(store, max_age=0)
        self.assertEqual([i["kind"] for i in self.data()["service_status"]["items"]], ["api"])

    def test_failed_check_keeps_last_good_result(self):
        """环境检测这次没连上：上次查到的出口留着不动，只记一笔“最近一次没连上”；那份结果旧了才按查不到提醒。"""
        import datetime as dt
        self._two_accounts()
        store = self.cam.Store()
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}
        both = ("/cdn-cgi/trace", "/cf/cdn-cgi/trace")
        score = lambda: self.cam.risk_score(self.cam.risk_items(self.data()))["score"]
        kinds = lambda items: [i["kind"] for i in items]
        self.cam.check_region(store, max_age=0)
        good = self.data()["region"]
        self.assertEqual((good["loc"], good["ip"], good["net"], good.get("failed_at")), ("US", "1.2.3.4", "net-a", None))
        self.assertEqual(score(), 100)
        # claude.ai 和 Cloudflare 的 trace 都答不上来
        MockAnthropic.down = both
        MockAnthropic.calls = []
        view = self.cam.check_region(store, max_age=0)
        self.assertTrue(set(both) <= {c[1] for c in MockAnthropic.calls})  # 两边都问过
        reg = self.data()["region"]
        for k in ("loc", "ip", "colo", "org", "ip_type", "checked_at", "net", "meta_at"):
            self.assertEqual(reg.get(k), good.get(k), k)
        self.assertTrue(reg["failed_at"])
        self.assertEqual(reg["fail_count"], 1)
        self.assertTrue(reg["env_at"])  # 本机设置不用联网，照常更新
        self.assertEqual((view["loc"], view["status"], view["fail_count"], view["failed_at"]),
                         ("US", "supported", 1, reg["failed_at"]))
        self.assertEqual(self.cam.public_state(store, self.data())["region"]["fail_count"], 1)
        risks = self.cam.risk_items(self.data())
        fail = [r for r in risks if r["kind"] == "env_failed"]
        self.assertEqual(len(fail), 1)
        self.assertEqual(fail[0]["level"], "info")
        self.assertIn("最近一次环境检测没连上", fail[0]["text"])
        self.assertNotIn("region_unknown", kinds(risks))
        self.assertEqual(self.cam.risk_score(risks)["score"], 100)  # 上次的结果还新：只说一声，不扣分
        # 刚失败过：定时的那种检测一分钟内不重试；手动的（max_age=0）马上重试
        MockAnthropic.calls = []
        self.cam.check_region(store)
        self.assertEqual(MockAnthropic.calls, [])
        self.cam.check_region(store, max_age=0)
        self.assertEqual(self.data()["region"]["fail_count"], 2)
        # 服务检测：出口那行说明是上次查到的，同一条提示，分数和顶部一致；总体结论不说“出口在支持名单内”
        items = self.cam.service_check(store)
        row = next(i for i in items if i["kind"] == "region")
        self.assertEqual(row["level"], "ok")
        self.assertIn("这次没连上", row["text"])
        self.assertEqual(kinds(items).count("env_failed"), 1)
        self.assertEqual(items[0]["score"], score())
        self.assertIn("这次没测到出口地区", items[0]["text"])
        self.assertNotIn("出口地区在支持名单内", items[0]["text"])
        self.assertEqual(self.data()["region"]["fail_count"], 3)

        def backdate(hours):
            with store.lock():
                d = store.load()
                d["region"]["checked_at"] = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=hours)).replace(microsecond=0).isoformat()
                store.save(d)

        # 上次查到的已经是两小时前的：出口现在在哪说不准，按“查不到”提醒并扣分，不再另报一条“结果旧了”
        backdate(2)
        risks = self.cam.risk_items(self.data())
        self.assertEqual(kinds(risks).count("region_unknown"), 1)
        self.assertNotIn("env_failed", kinds(risks))
        self.assertNotIn("env_stale", kinds(risks))
        unknown = next(r for r in risks if r["kind"] == "region_unknown")
        self.assertIn("连续 3 次检测都没连上", unknown["text"])
        self.assertIn("上一次查到是", unknown["text"])
        self.assertEqual(self.cam.risk_score(risks)["score"], 70)
        items = self.cam.service_check(store)
        self.assertEqual(kinds(items).count("region_unknown"), 1)
        self.assertNotIn("env_stale", kinds(items))
        self.assertEqual((items[0]["score"], score()), (70, 70))
        # 上次查到的出口不在支持名单：“不在名单”那条已经扣过分，没连上只说一声，不叠一条“查不到”
        with store.lock():
            d = store.load()
            d["region"]["loc"] = "CN"
            store.save(d)
        risks = self.cam.risk_items(self.data())
        self.assertEqual((kinds(risks).count("region"), kinds(risks).count("env_failed")), (1, 1))
        self.assertNotIn("region_unknown", kinds(risks))
        self.assertEqual(next(r for r in risks if r["kind"] == "env_failed")["level"], "info")
        # 连上了：失败记录清掉，换成新查到的
        MockAnthropic.down = ()
        self.cam.check_region(store, max_age=0)
        reg = self.data()["region"]
        self.assertEqual((reg["loc"], reg["prev_loc"]), ("US", "CN"))
        self.assertNotIn("failed_at", reg)
        self.assertNotIn("fail_count", reg)
        self.assertEqual(self.cam.public_state(store, self.data())["region"]["fail_count"], 0)
        self.assertNotIn("env_failed", kinds(self.cam.risk_items(self.data())))
        # 从来没查到过的：还是记成查不到
        with store.lock():
            d = store.load()
            d.pop("region")
            store.save(d)
        MockAnthropic.down = both
        self.cam.check_region(store, max_age=0)
        reg = self.data()["region"]
        self.assertIsNone(reg["loc"])
        self.assertEqual(reg["fail_count"], 1)
        risks = self.cam.risk_items(self.data())
        self.assertEqual(kinds(risks).count("region_unknown"), 1)
        self.assertIn("出口地区查不到", next(r for r in risks if r["kind"] == "region_unknown")["text"])
        self.assertEqual(self.cam.risk_score(risks)["score"], 70)

    def test_exit_trace_asks_cloudflare_when_claude_ai_is_slow(self):
        """claude.ai 的 trace 两秒没回音（这里调成 0.2 秒）或者答不上来：同时问 Cloudflare，谁先给出结果用谁的。"""
        paths = lambda: [c[1] for c in MockAnthropic.calls]

        def timed(fn):
            t0 = time.monotonic()
            return fn(), time.monotonic() - t0

        with mock.patch.object(self.cam, "TRACE_HEDGE_SECS", 0.2):
            # 正常：只问 claude.ai
            got = self.cam.exit_trace(3)
            self.assertEqual((got["loc"], got["colo"]), ("US", "SJC"))
            self.assertEqual(paths(), ["/cdn-cgi/trace"])
            # claude.ai 卡住：0.2 秒后同时问 Cloudflare，用它的结果，不等满 3 秒
            MockAnthropic.calls = []
            MockAnthropic.slow = {"/cdn-cgi/trace": 30}
            got, took = timed(lambda: self.cam.exit_trace(3))
            self.assertEqual((got["loc"], got["colo"]), ("US", "LAX"))
            self.assertLess(took, 2)
            self.assertEqual(paths(), ["/cdn-cgi/trace", "/cf/cdn-cgi/trace"])
            self.drain()
            # 设置里关了外部查询（fallback=False）：只问 claude.ai，到时限返回 None
            MockAnthropic.calls = []
            MockAnthropic.slow = {"/cdn-cgi/trace": 30}
            got, took = timed(lambda: self.cam.exit_trace(0.6, fallback=False))
            self.assertIsNone(got)
            self.assertLess(took, 2)
            self.assertEqual(paths(), ["/cdn-cgi/trace"])
            self.drain()
            # 两边都卡住：等到时限返回 None，不会更久
            MockAnthropic.calls = []
            MockAnthropic.slow = {"/cdn-cgi/trace": 30, "/cf/cdn-cgi/trace": 30}
            got, took = timed(lambda: self.cam.exit_trace(0.8))
            self.assertIsNone(got)
            self.assertGreaterEqual(took, 0.7)
            self.assertLess(took, 2.5)
            self.assertEqual(sorted(paths()), ["/cdn-cgi/trace", "/cf/cdn-cgi/trace"])
            self.drain()
        with mock.patch.object(self.cam, "TRACE_HEDGE_SECS", 5):
            # claude.ai 一下就答不上来（503）：不等那 5 秒，马上问 Cloudflare
            MockAnthropic.calls = []
            MockAnthropic.down = ("/cdn-cgi/trace",)
            got, took = timed(lambda: self.cam.exit_trace(6))
            self.assertEqual(got["colo"], "LAX")
            self.assertLess(took, 2)
            # 两边都答不上来：马上返回 None
            MockAnthropic.down = ("/cdn-cgi/trace", "/cf/cdn-cgi/trace")
            got, took = timed(lambda: self.cam.exit_trace(6))
            self.assertIsNone(got)
            self.assertLess(took, 2)
            # 启动 claude 前的快速检测只问 claude.ai，不问别家
            MockAnthropic.calls = []
            self.assertIsNone(self.cam.quick_trace(3))
            self.assertEqual(paths(), ["/cdn-cgi/trace"])

    def test_stuck_third_parties_do_not_hold_up_the_exit_check(self):
        """查运营商、注册国、IPv4 / IPv6 出口的第三方接口卡住：到时限当作查不到，出口地区照样记下来，不算检测失败。"""
        self._two_accounts()
        store = self.cam.Store()
        MockAnthropic.slow = dict.fromkeys(("/ipapiis", "/ipinfo", "/ipapi", "/meta", "/rdap", "/trace4", "/trace6"), 30)
        with mock.patch.object(self.cam, "META_BUDGET_SECS", 0.6), mock.patch.object(self.cam, "META_SOURCE_TIMEOUT", 0.25):
            t0 = time.monotonic()
            self.cam.check_region(store, max_age=0)
            took = time.monotonic() - t0
        self.assertLess(took, 3)
        reg = self.data()["region"]
        self.assertEqual((reg["loc"], reg["ip"], reg.get("org"), reg.get("v4_loc"), reg.get("failed_at")),
                         ("US", "1.2.3.4", None, None, None))
        self.assertIsNone(reg.get("meta_at"))  # 运营商没查到：不缓存，下次重查
        paths = [c[1] for c in MockAnthropic.calls]
        self.assertEqual(len([p for p in paths if p.startswith("/ipapiis")]), 1)
        self.assertNotIn("/meta", paths)  # 前两个数据源把时限用完了，后面的不再问
        self.assertEqual(self.cam.risk_score(self.cam.risk_items(self.data()))["score"], 100)
        self.drain()
        # 接口恢复后下一次就补上
        self.cam.check_region(store, max_age=0)
        reg = self.data()["region"]
        self.assertEqual((reg["org"], reg["v4_loc"]), ("Comcast Cable Communications, LLC", "US"))

    def test_run_parallel_budget_and_limits(self):
        rp = self.cam.run_parallel
        stop = threading.Event()
        self.addCleanup(stop.set)
        stuck = lambda: stop.wait(30)
        self.assertEqual(rp({"a": lambda: 1, "b": lambda: "x"}, 2), {"a": 1, "b": "x"})
        self.assertEqual(rp({}, 2), {})
        # 到了总时限还没做完的那件是 None，不接着等
        t0 = time.monotonic()
        self.assertEqual(rp({"fast": lambda: 1, "stuck": stuck}, 0.3), {"fast": 1, "stuck": None})
        self.assertLess(time.monotonic() - t0, 2)
        # 某一件有自己更短的时限：别的都做完了就只等它到那个时限
        t0 = time.monotonic()
        self.assertEqual(rp({"fast": lambda: 1, "stuck": stuck}, 20, {"stuck": 0.2}), {"fast": 1, "stuck": None})
        self.assertLess(time.monotonic() - t0, 2)
        # 它的时限不影响别的：慢一点但在总时限内的照样等到
        self.assertEqual(rp({"slowish": lambda: time.sleep(0.3) or "done", "stuck": stuck}, 5, {"stuck": 0.1}),
                         {"slowish": "done", "stuck": None})
        # 哪件抛了错原样抛出
        with self.assertRaises(ZeroDivisionError):
            rp({"ok": lambda: 1, "boom": lambda: 1 // 0}, 2)

    def test_background_rechecks_on_network_change_and_retries_failures(self):
        """两次定时检测之间：本机换了网络马上补测一次；上次没连上的过一分钟重试，再失败间隔翻倍。"""
        import datetime as dt
        self._two_accounts()
        store = self.cam.Store()
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}
        st, told = {}, []
        tell = lambda title, body: told.append(title)

        def traces_in_round(refresh_min=10):
            MockAnthropic.calls = []
            self.cam.background_round(store, st, refresh_min, tell)
            return len([c for c in MockAnthropic.calls if c[1] == "/cdn-cgi/trace"])

        self.assertEqual(traces_in_round(), 1)  # 第一轮：定时检测
        self.assertEqual((st["net"], self.data()["region"]["net"]), ("net-a", "net-a"))
        self.assertEqual(traces_in_round(), 0)  # 没到时间、网络也没变：不测
        # 换了网络（换 Wi-Fi、开关接管整机的 VPN）：马上补测一次
        os.environ["CAM_TEST_NET"] = "net-b"
        MockAnthropic.trace_ip = "5.6.7.8"
        self.assertEqual(traces_in_round(), 1)
        reg = self.data()["region"]
        self.assertEqual((reg["ip"], reg["net"], st["net"]), ("5.6.7.8", "net-b", "net-b"))
        self.assertEqual(traces_in_round(), 0)
        # 半分钟内又变了一次：先不测（切换网络时地址会连着变几回），过了半分钟才测
        os.environ["CAM_TEST_NET"] = "net-c"
        self.assertEqual(traces_in_round(), 0)
        self.assertEqual(st["net"], "net-b")
        st["net_at"] -= 31
        self.assertEqual(traces_in_round(), 1)
        self.assertEqual((st["net"], self.data()["region"]["net"]), ("net-c", "net-c"))
        # 断网（没有可用的路由）不算换了网络
        os.environ["CAM_TEST_NET"] = ""
        st["net_at"] -= 31
        self.assertEqual(traces_in_round(), 0)
        self.assertEqual(st["net"], "net-c")
        # 关掉了定时刷新（refresh_min=0）的也不补测
        os.environ["CAM_TEST_NET"] = "net-d"
        self.assertEqual(traces_in_round(0), 0)
        # 轮到定时检测那一轮、刚好又换了网络：不因为“五分钟内测过”跳过；网络没变的才跳过
        st["last_refresh"] = 0
        self.assertEqual(traces_in_round(), 1)
        self.assertEqual((st["net"], self.data()["region"]["net"]), ("net-d", "net-d"))
        st["last_refresh"] = 0
        self.assertEqual(traces_in_round(), 0)

        def fail_age(secs):
            with store.lock():
                d = store.load()
                d["region"]["failed_at"] = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=secs)).replace(microsecond=0).isoformat()
                store.save(d)

        # 检测没连上：一分钟内不重试
        MockAnthropic.down = ("/cdn-cgi/trace", "/cf/cdn-cgi/trace")
        self.cam.check_region(store, max_age=0)
        self.assertEqual(self.data()["region"]["fail_count"], 1)
        self.assertEqual(traces_in_round(), 0)
        fail_age(61)
        self.assertEqual(traces_in_round(), 1)
        self.assertEqual(self.data()["region"]["fail_count"], 2)
        # 第二次失败后等两分钟，第三次后等四分钟……最长不超过定时检测的间隔
        fail_age(61)
        self.assertEqual(traces_in_round(), 0)
        fail_age(121)
        self.assertEqual(traces_in_round(), 1)
        self.assertEqual(self.data()["region"]["fail_count"], 3)
        fail_age(121)
        self.assertEqual(traces_in_round(), 0)
        self.assertEqual(traces_in_round(2), 1)  # 两分钟一轮的话最长等两分钟
        self.assertEqual(self.data()["region"]["fail_count"], 4)
        self.assertEqual(self.data()["region"]["loc"], "US")  # 一直留着上次查到的
        # 连上了：恢复正常，不算后台出错，也不弹通知
        MockAnthropic.down = ()
        fail_age(1000)
        self.assertEqual(traces_in_round(), 1)
        self.assertNotIn("failed_at", self.data()["region"])
        self.assertEqual(traces_in_round(), 0)
        self.assertNotIn("last_error", st)
        self.assertEqual(told, [])

    def test_launch_prompt_rechecks_after_network_change_or_failure(self):
        """启动 claude 前：刚测过、网络没变就不联网；换了网络或上次没连上，花一次快速检测看现在的出口。"""
        self._two_accounts()
        store = self.cam.Store()
        self.cam.check_region(store, max_age=0)

        def launch_calls():
            MockAnthropic.calls = []
            self.assertTrue(self.cam.risk_prompt_before_launch(store, ask=lambda _: "y", interactive=True))
            return [c[1] for c in MockAnthropic.calls]

        self.assertEqual(launch_calls(), [])
        os.environ["CAM_TEST_NET"] = "net-b"
        self.assertEqual(launch_calls(), ["/cdn-cgi/trace"])
        self.assertEqual(self.data()["region"]["net"], "net-a")  # 快速检测只用来算这次的提醒，不保存
        # 后台那次完整检测（这里直接调它的入口）不因为“一分钟内测过”跳过
        MockAnthropic.calls = []
        self.cam.background_region_check(store)
        self.assertEqual(self.data()["region"]["net"], "net-b")
        self.assertEqual(launch_calls(), [])
        # 上次没连上：留着的结果不直接信
        MockAnthropic.down = ("/cdn-cgi/trace", "/cf/cdn-cgi/trace")
        self.cam.check_region(store, max_age=0)
        MockAnthropic.down = ()
        self.assertEqual(launch_calls(), ["/cdn-cgi/trace"])
        d2 = self.cam.with_quick_region(self.data(), self.cam.quick_trace(3))
        self.assertNotIn("failed_at", d2["region"])
        self.assertNotIn("env_failed", [r["kind"] for r in self.cam.risk_items(d2)])
        self.assertTrue(self.data()["region"]["failed_at"])  # 只是副本

    def test_usage_gives_up_when_the_network_hangs(self):
        """读账号资料等了好几秒都没连上：不接着读用量（同样要白等那么久）；一下就失败的才接着试。"""
        a_id, b_id = self._two_accounts()
        store = self.cam.Store()
        acc = next(a for a in self.data()["accounts"] if a["id"] == b_id)
        seen = []

        def hang(method, url, headers=None, body=None, timeout=20):
            seen.append((url.rsplit("/", 1)[-1], timeout))
            time.sleep(0.15)
            return 0, {"error": {"message": "timed out"}}

        with mock.patch.object(self.cam, "USAGE_GIVE_UP", 0.1), mock.patch.object(self.cam, "http_json", hang):
            with self.assertRaises(self.cam.CamError):
                self.cam.fetch_code_usage(store, dict(acc), allow_refresh=False)
        self.assertEqual(seen, [("profile", self.cam.USAGE_TIMEOUT)])
        self.assertLess(self.cam.USAGE_TIMEOUT, 20)
        seen.clear()

        def reset(method, url, headers=None, body=None, timeout=20):
            seen.append((url.rsplit("/", 1)[-1], timeout))
            if url.endswith("/profile"):
                return 0, {"error": {"message": "connection reset"}}
            return 200, {"five_hour": {"utilization": 12.0, "resets_at": "2099-01-01T00:00:00Z"},
                         "seven_day": {"utilization": 3.0, "resets_at": "2099-01-05T00:00:00Z"}}

        with mock.patch.object(self.cam, "http_json", reset):
            usage = self.cam.fetch_code_usage(store, dict(acc), allow_refresh=False)
        self.assertEqual(seen, [("profile", self.cam.USAGE_TIMEOUT), ("usage", self.cam.USAGE_TIMEOUT)])
        self.assertEqual(usage["five_hour"]["utilization"], 12.0)

    def test_header_score_matches_service_check(self):
        self._two_accounts()
        store = self.cam.Store()
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 1.0, "email": "b@x.com", "plan": "max"}}
        for route, proxy in (("", ""), ("wg0", ""), ("utun4", "http://127.0.0.1:7890")):
            os.environ["CAM_TEST_ROUTE_IF"] = route
            if proxy:
                os.environ["HTTPS_PROXY"] = proxy
                os.environ["NO_PROXY"] = "127.0.0.1,localhost"
            try:
                items = self.cam.service_check(store)
                header = self.cam.risk_score(self.cam.risk_items(self.data()))["score"]
                self.assertEqual(items[0]["score"], header, (route, proxy, [i["kind"] for i in items]))
            finally:
                os.environ["CAM_TEST_ROUTE_IF"] = ""
                os.environ.pop("HTTPS_PROXY", None)
                os.environ.pop("NO_PROXY", None)
        self.assertEqual(self.data()["region"]["env"]["tunnel"], ["utun4", "VPN / 隧道接口"])  # 最后一轮的缓存

    def test_review_regressions(self):
        self._two_accounts()
        store = self.cam.Store()
        old_ttl = self.cam.META_TTL
        self.cam.META_TTL = 86400
        try:
            # IP 信息全部查失败：不缓存，下次重查
            MockAnthropic.ipapi_type = None  # ipapi.is 返回 429
            real_ipinfo = self.cam.IPINFO_URL
            self.cam.IPINFO_URL, self.cam.CF_META_URL = "", ""
            self.cam.check_region(store, max_age=0)
            self.assertIsNone(self.data()["region"].get("meta_at"))
            self.cam.IPINFO_URL = real_ipinfo
            MockAnthropic.ipapi_type = "residential"
            MockAnthropic.calls = []
            self.cam.check_region(store, max_age=0)
            self.assertEqual(len([c for c in MockAnthropic.calls if c[1].startswith("/ipapiis")]), 1)
            self.assertEqual(self.data()["region"]["org"], "Comcast Cable Communications, LLC")
        finally:
            self.cam.META_TTL = old_ttl
            self.cam.CF_META_URL = os.environ["CAM_CF_META_URL"]
        # 中间失败过一次，下一次成功时仍能发现出口变了（US → 失败 → HK）
        real_base = self.cam.CLAUDE_WEB_BASE
        try:
            self.cam.CLAUDE_WEB_BASE = "http://127.0.0.1:9"  # 连不上
            real_trace = self.cam.http_text
            self.cam.http_text = lambda url, timeout=8: (0, "网络错误") if "cdn-cgi/trace" in url else real_trace(url, timeout)
            self.cam.check_region(store, max_age=0)
            reg = self.data()["region"]
            self.assertEqual((reg["loc"], reg["last_loc"], reg["fail_count"]), ("US", "US", 1))  # 留着上次查到的，记一笔没连上
            self.assertTrue(reg["failed_at"])
        finally:
            self.cam.CLAUDE_WEB_BASE = real_base
            self.cam.http_text = real_trace
        MockAnthropic.trace_loc = "HK"
        self.cam.check_region(store, max_age=0)
        self.assertEqual((self.data()["region"]["prev_loc"], self.data()["region"]["loc"]), ("US", "HK"))
        # 子进程输出不是 UTF-8 时不崩（Windows 上的 GBK）
        self.assertEqual(self.cam.decode_output("以太网".encode("utf-8")), "以太网")
        self.assertIsInstance(self.cam.decode_output("以太网".encode("gbk")), str)
        self.assertEqual(self.cam.asn_label(None), "AS?")
        # 顶部状态只算一次风险，不现场跑系统命令
        calls = []
        real_env = self.cam.local_env
        self.cam.local_env = lambda: calls.append(1) or real_env()
        try:
            with store.lock():
                d = store.load()
                d["region"].pop("env", None)  # 旧版缓存
                store.save(d)
            self.cam.public_state(store, self.data())
            self.assertEqual(calls, [])
        finally:
            self.cam.local_env = real_env

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
        self.assertIn("实际所在地", reg["advice"])
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
        def go():
            try:
                out["tok"] = self.cam.ensure_fresh_token(store, a_id)
            except self.cam.CamError as e:
                out["err"] = str(e)
        with store.lock():
            t = threading.Thread(target=go)
            t.start()
            time.sleep(0.3)  # 线程发现 A 过期，正在等锁
            # 等锁期间切到了 A（相当于 switch_code_account 做的事）
            d = store.load()
            d["active"]["claude_code"] = a_id
            store.save(d)
            self.login("at-A1", "rt-A1", "uuid-A", "a@x.com", expires_ms=past)
        t.join(10)
        # 现在 A 是当前账号而且登录过期了：不替它续（交给 Claude Code），也不拿过期的 token 白跑一趟
        self.assertIn("等 Claude Code 自己续", out.get("err", ""), out)
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
        import io
        with mock.patch.object(sys, "stdin", io.StringIO()):  # 在终端里跑测试时 stdin 是交互的，这里固定成非交互
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

    # ---- 全面体检（cam doctor）

    def _stub_claude(self, version="2.1.0"):
        """PATH 里放一个假的 claude，不运行本机真装的那个。"""
        bindir = os.path.join(self.tmp, "stub-bin")
        stub(bindir, "claude", f'echo "{version} (Claude Code)"')
        patcher = mock.patch.dict(os.environ, {"PATH": os.pathsep.join([bindir, "/usr/bin", "/bin"])})
        patcher.start()
        self.addCleanup(patcher.stop)

    def _checkup(self, offline=False):
        self._stub_claude()
        return self.cam.full_checkup(self.cam.Store(), offline)

    @staticmethod
    def _section(res, key):
        return next(s["items"] for s in res["sections"] if s["key"] == key)

    @staticmethod
    def _group(items, label):
        return [i for i in items if (i.get("group") or "").startswith((label + " ", label + "（"))]

    def _fake_proc(self, pid, argv, env=None, cwd=None, age=None):
        """在假的 /proc 里放一个进程：命令行、环境变量、工作目录、启动了多久（秒）。"""
        root = os.environ["CAM_TEST_PROC_ROOT"]
        d = os.path.join(root, str(pid))
        os.makedirs(d)
        with open(os.path.join(d, "cmdline"), "wb") as f:
            f.write(b"\0".join(a.encode() for a in argv) + b"\0")
        if env is not None:
            with open(os.path.join(d, "environ"), "wb") as f:
                f.write(b"\0".join(f"{k}={v}".encode() for k, v in env.items()) + b"\0")
        if cwd:
            os.symlink(cwd, os.path.join(d, "cwd"))
        if age is not None:
            ticks = os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100
            write(os.path.join(root, "uptime"), "100000.00 50.00\n")
            fields = ["S"] + ["0"] * 18 + [str(int((100000 - age) * ticks)), "0", "0"]
            write(os.path.join(d, "stat"), f"{pid} (claude code) " + " ".join(fields) + "\n")

    def test_checkup_renews_saved_login_but_never_the_live_one(self):
        a_id, b_id = self._two_accounts(a_expires=self._expired_ms())  # A 不在用，保存的 access token 过期了；B 是当前登录
        self.write_creds("at-B1", "rt-B1", self._expired_ms())  # 当前登录的 access token 也过期了
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2"), "rt-B1": ("at-B2", "rt-B2")}
        MockAnthropic.tokens = {"at-A2": {"u5": 12.0, "u7": 40.0, "email": "a@x.com", "plan": "max"}}
        acc = self._section(self._checkup(), "accounts")
        a, b = self._group(acc, "A"), self._group(acc, "B")
        self.assertTrue(a and b, acc)
        self.assertEqual(a[0]["level"], "ok")
        self.assertIn("刚用它自己的 refresh token 续了一次", a[0]["text"])
        self.assertIn("用量接口正常：5 小时已用 12%，7 天已用 40%", [i["text"] for i in a])
        self.assertEqual(a[-1]["text"], "账号状态：正常")
        self.assertEqual(self.cam.Store().load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A2")
        # 当前登录从不替它续：只说过期了，用量接口的 401 也只是提示
        self.assertEqual([c for c in MockAnthropic.calls if c[1] == "/v1/oauth/token"], [("POST", "/v1/oauth/token", "rt-A1")])
        self.assertEqual(self.live()[0]["refreshToken"], "rt-B1")
        self.assertIn("（Claude Code，Max，当前登录）", b[0]["group"])
        self.assertIn("当前登录：access token 已过期", b[0]["text"])
        self.assertIn("用量没更新：当前登录的 access token 过期了，等 Claude Code 自己续", [i["text"] for i in b])
        self.assertTrue(all(i["level"] in ("ok", "info") for i in b), b)

    def test_checkup_flags_revoked_saved_login(self):
        a_id, b_id = self._two_accounts(a_expires=self._expired_ms())  # 假接口不认 rt-A1：A 的登录已作废
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 2.0, "email": "b@x.com", "plan": "max"}}
        res = self._checkup()
        acc = self._section(res, "accounts")
        a = self._group(acc, "A")
        bad = [i for i in a if i["level"] == "bad"]
        self.assertEqual(len(bad), 1, a)  # 只报一次，健康状态里不重复
        self.assertIn("登录已失效", bad[0]["text"])
        self.assertTrue(bad[0]["advice"].startswith("cam login A"), bad[0])
        self.assertEqual(a[-1]["text"], "账号状态：异常（见上面几条）")
        self.assertTrue(res["summary"]["first"].startswith("A a@x.com"), res["summary"])
        self.assertTrue(self.cam.checkup_brief(res["summary"]).startswith(f"{res['summary']['bad']} 项要处理：A a@x.com"))
        b = self._group(acc, "B")
        self.assertEqual(b[0]["level"], "ok")
        self.assertIn("当前登录：access token", b[0]["text"])
        self.assertIn("用量接口正常：5 小时已用 1%，7 天已用 2%", [i["text"] for i in b])
        self.assertNotIn(("POST", "/v1/oauth/token", "rt-B1"), MockAnthropic.calls)
        self.assertEqual(self.cam.checkup_brief({"bad": 0, "warn": 0, "first": None}), "一切正常")
        self.assertEqual(self.cam.checkup_brief({"bad": 0, "warn": 3, "first": "x"}), "3 项留意：x")

    def test_checkup_expiry_rate_limit_web_and_api(self):
        a_id, b_id = self._two_accounts()
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 2.0, "email": "b@x.com", "plan": "max"}}  # 不认 A 的 access token
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2")}  # 替它续一次（A 不是当前登录），换来的还是不认
        today = dt.date.today()
        self.run_cam("set", "A", "--expires", (today + dt.timedelta(days=3)).isoformat())
        self.run_cam("set", "B", "--expires", (today - dt.timedelta(days=2)).isoformat())
        self.run_cam("add-web", "--label", "W", "--session-key", "sk-web-secret")
        self.run_cam("add-web", "--label", "W2")
        self.run_cam("add-api", "--label", "K", "--key", "sk-ant-api-badkey1234567890")
        MockAnthropic.web_keys = {"sk-web-secret": {"email": "w@x.com", "plan": "pro", "u5": 5.0, "u7": 6.0}}
        acc = self._section(self._checkup(), "accounts")
        pick = lambda label: [(i["level"], i["text"]) for i in self._group(acc, label)]
        a, b = pick("A"), pick("B")
        self.assertIn(("warn", "用量接口不认这个登录（HTTP 401）"), a)
        self.assertEqual([c[2] for c in MockAnthropic.calls if c[1] == "/v1/oauth/token"], ["rt-A1"])  # 只续一次，不反复试
        self.assertIn(("warn", f"3 天后到期（{(today + dt.timedelta(days=3)).isoformat()}）"), a)
        self.assertIn(("bad", f"订阅已过期 2 天（{(today - dt.timedelta(days=2)).isoformat()}）"), b)
        self.assertEqual(sum(1 for _, t in b if "订阅已过期" in t), 1)  # 健康状态里不重复
        self.assertIn(("ok", "sessionKey 有效，用量接口正常：5 小时已用 5%，7 天已用 6%"), pick("W"))
        self.assertIn(("info", "没填 sessionKey，不自动读取用量（只记到期日也可以）"), pick("W2"))
        k = self._group(acc, "K")
        self.assertEqual((k[0]["level"], k[0]["advice"]), ("bad", "cam set K --api-key 新的 Key"))
        self.assertIn("API Key 无效", k[0]["text"])
        # 用量接口限流：只是留意，健康状态不重复报
        MockAnthropic.usage_code = 429
        b = [i for i in self._group(self._section(self._checkup(), "accounts"), "B") if "429" in i["text"]]
        self.assertEqual([(i["level"], i["text"]) for i in b], [("warn", "用量接口限流（HTTP 429）")])

    def test_checkup_status_page(self):
        base = os.environ["CAM_API_BASE"]
        items = self.cam.status_summary_items(self.cam.fetch_status_summary())
        self.assertEqual([(i["level"], i["text"]) for i in items], [("ok", "官方状态页：全部正常（127.0.0.1）")])
        MockAnthropic.status_summary = "incident"
        s = self.cam.fetch_status_summary()
        self.assertEqual(s["incidents"], [{"name": "Elevated errors on Claude Code", "status": "identified"}])  # 已解决的不列
        self.assertEqual(s["components"], [{"name": "Claude Code", "status": "partial_outage"}])  # Cowork 和这里无关
        items = self.cam.status_summary_items(s)
        self.assertEqual([(i["level"], i["text"]) for i in items], [
            ("bad", "官方状态页：Partial System Outage（major）"),
            ("info", "没解决的事件：Elevated errors on Claude Code（已找到原因）"),
            ("info", "Claude Code：部分故障")])
        self.assertIsNone(self.cam.parse_status_summary({"status": {}}))
        self.assertIsNone(self.cam.parse_status_summary(["x"]))
        # 第一个地址连不上就换下一个；都连不上只是“留意”，不算要处理
        with mock.patch.object(self.cam, "STATUS_SUMMARY_URLS", [base + "/summary-down.json", base + "/summary.json"]):
            self.assertEqual(self.cam.fetch_status_summary()["indicator"], "major")
        with mock.patch.object(self.cam, "STATUS_SUMMARY_URLS", [base + "/summary-down.json"]):
            items = self.cam.status_summary_items(self.cam.fetch_status_summary())
        self.assertEqual(items[0]["level"], "warn")
        self.assertIn("官方状态页连不上", items[0]["text"])
        # 体检里：状态页的完整摘要代替服务检查里那一行；api / web 后面有一行延迟
        MockAnthropic.status_indicator = "major"
        svc = self._section(self._checkup(), "service")
        self.assertEqual([i for i in svc if i.get("kind") == "status"], [])
        self.assertIn("没解决的事件：Elevated errors on Claude Code（已找到原因）", [i["text"] for i in svc])
        lat = [i for i in svc if i.get("kind") == "latency"]
        self.assertRegex(lat[0]["text"], r"^延迟：127\.0\.0\.1 \d+ ms，127\.0\.0\.1 \d+ ms$")
        self.assertEqual(svc[[i.get("kind") for i in svc].index("latency") - 1]["kind"], "web")

    def test_checkup_lists_sessions_and_which_login_they_use(self):
        self._fake_proc(101, ["claude", "--session-id", "s1", "--channels", "plugin:discord"],
                        {"TMUX": "/tmp/tmux-1000/claude-discord,99,0", "PATH": "/bin"}, "/work/main", 7500)
        self._fake_proc(102, ["/home/u/.local/share/claude/versions/2.1.0", "--resume", "s2"],
                        {"FEISHU_SESSION": "shop", "FEISHU_STATE_DIR": "/h/.claude/channels/feishu-shop",
                         "CLAUDE_CODE_OAUTH_TOKEN": "sk-ant-oat01-SECRETVALUE"}, "/work/shop", 90)
        self._fake_proc(103, ["node", "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"],
                        {"DISCORD_STATE_DIR": "/h/.claude/channels/discord-ops"}, "/work/ops", 3 * 86400 + 3600)
        self._fake_proc(104, ["claude"], {"TMUX": "/tmp/tmux-1000/claude-dingtalk-hr,5,0", "ANTHROPIC_API_KEY": "sk-ant-api-SECRET2"})
        self._fake_proc(105, ["claude"], {"CLAUDE_CONFIG_DIR": os.path.join(self.tmp, "other-claude")})
        self._fake_proc(106, ["claude"], {"CLAUDE_CONFIG_DIR": os.path.join(self.tmp, ".claude")})  # 就是本机的配置目录
        self._fake_proc(107, ["claude"])  # 别的用户的进程：读不到环境变量
        self._fake_proc(108, ["python3", "x.py"], {"CLAUDE_CODE_OAUTH_TOKEN": "nope"})  # 不是 claude
        sessions = {s["pid"]: s for s in self.cam.claude_sessions()}
        self.assertEqual(sorted(sessions), [101, 102, 103, 104, 105, 106, 107])
        self.assertEqual({p: s["login"] for p, s in sessions.items()},
                         {101: "machine", 102: "own_token", 103: "machine", 104: "api_key", 105: "config_dir",
                          106: "machine", 107: "unknown"})
        self.assertEqual({p: s["bot"] for p, s in sessions.items()},
                         {101: ("discord", "main"), 102: ("feishu", "shop"), 103: ("discord", "ops"),
                          104: ("dingtalk", "hr"), 105: None, 106: None, 107: None})
        items = self.cam.session_items()
        texts = [i["text"] for i in items]
        self.assertEqual(texts[0], "正在运行的 claude 会话：7 个（3 个用本机登录：切换账号后它们还拿着旧账号的 token，要重开）")
        self.assertEqual(texts[1], "pid 101 · Discord 机器人主会话 · /work/main · 已运行 2 小时 5 分 · 本机登录，切号会跟着换")
        self.assertEqual(items[1]["advice"], "想让它一直用同一个号、不跟着切换：在聊天里发 /account（没有别的号先 /account add 接入一个），或在电脑上运行 claude-discord account main set")
        self.assertEqual(texts[2], "pid 102 · 飞书机器人 shop · /work/shop · 已运行 1 分钟 · 用自己的号（CLAUDE_CODE_OAUTH_TOKEN），切号不影响")
        self.assertEqual(items[2]["advice"], "")
        self.assertIn("pid 103 · Discord 机器人 ops · /work/ops · 已运行 3 天 1 小时 · 本机登录", texts[3])
        self.assertIn("钉钉机器人 hr", texts[4])
        self.assertIn("用 API Key", texts[4])
        self.assertIn("用单独的配置目录", texts[5])
        self.assertIn("读不到它的环境变量", texts[7])
        dump = json.dumps(self._checkup(offline=True), ensure_ascii=False)
        self.assertIn("飞书机器人 shop", dump)
        for secret in ("SECRETVALUE", "SECRET2", "nope", "other-claude"):  # 只看有没有，不读出值
            self.assertNotIn(secret, dump)
        # 别的系统：列出 ps 给的东西，说看不出用的哪个登录
        with mock.patch.object(self.cam, "proc_root", lambda: None), \
                mock.patch.object(self.cam, "_ps_etimes", lambda pids: {7: "01:02:03"}):
            self.cam.running_claude_processes = lambda: [(7, "claude --resume x")]
            items = self.cam.session_items()
        self.assertEqual(items[1]["text"], "pid 7 · 已运行 01:02:03 · claude --resume x")
        self.assertIn("看不出来", items[1]["advice"])

    def test_checkup_cam_section(self):
        a_id, b_id = self._two_accounts()
        store = self.cam.Store()
        old = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=2)).isoformat()
        with store.lock():
            d = store.load()
            for a in d["accounts"]:
                a["usage"] = {"fetched_at": old, "error": None}
            store.save(d)
        texts = lambda items: [(i["level"], i["text"]) for i in items]
        items = texts(self.cam.checkup_cam(store, store.load()))
        self.assertIn(("info", f"后台任务：没有程序在做（桌面版、cam serve、{self.cam.MENU_TOOL}都没在运行）"), items)
        self.assertIn(("info", "A：用量上次刷新 2 小时前"), items)  # 没有后台在跑，不算没在刷新
        # cam serve 拿着后台锁，心跳记在数据目录里
        self._fake_proc(110, ["python3", "/x/scripts/cam.py", "serve", "--port", "8787"])
        write(os.path.join(store.root, ".serve-state.json"), json.dumps(
            {"pid": 110, "at": self.cam.now_iso(), "refresh_min": 10, "last_error": "刷新用量：boom"}))
        lease = store.background_lock()
        self.assertTrue(lease.try_acquire())
        try:
            items = texts(self.cam.checkup_cam(store, store.load()))
        finally:
            lease.release()
        self.assertTrue(any(lv == "ok" and t.startswith("后台任务：cam serve 在做（pid 110），上一轮") for lv, t in items), items)
        self.assertIn(("warn", "后台上一轮出错：刷新用量：boom"), items)
        self.assertIn(("warn", "A：用量上次刷新 2 小时前，后台没在刷新它"), items)
        # 菜单栏插件：每分钟跑一轮，看它的状态文件
        write(os.path.join(store.root, ".menubar-state.json"), "{}")
        self.assertTrue(any(t.startswith(f"后台任务：{self.cam.MENU_TOOL}每分钟做一轮") for _, t in texts(self.cam.checkup_cam(store, store.load()))))
        # 有新版本、代码目录有改动、文件对不上
        upd = {"checkout": True, "behind": 2, "ahead": 0, "dirty": [" M README.md"], "checked_at": self.cam.now_iso(), "message": "x"}
        with mock.patch.object(self.cam, "saved_update_check", lambda: upd), \
                mock.patch.object(self.cam, "verify_sums", lambda: (5, ["x.py（内容不一致）"])):
            items = self.cam.checkup_cam(store, store.load())
        got = texts(items)
        self.assertIn(("warn", "有 2 个新改动可以更新（刚刚检查的）"), got)
        self.assertIn(("info", "代码目录里有 1 个文件改过没提交（检查更新时看到的）"), got)
        self.assertIn(("bad", "1 个文件和 SHA256SUMS 对不上：x.py（内容不一致）"), got)
        # 备份：切换一次就有一份
        self.assertEqual(self.run_cam("switch", "A"), 0)
        self.assertTrue(any(t.startswith("备份：1 份，最新") for _, t in texts(self.cam.backup_items(store))))
        if os.name == "nt":
            return
        # 权限：目录 0700，密钥文件 0600；凭据文件 0600
        self.assertEqual(texts(self.cam.permission_items(store))[0][0], "ok")
        os.chmod(store.secrets_dir, 0o755)
        os.chmod(store.secret_path(a_id), 0o644)
        perm = self.cam.permission_items(store)
        self.assertEqual([(i["level"], i["advice"]) for i in perm],
                         [("warn", f"chmod 700 {store.secrets_dir}"), ("warn", f"chmod 600 {store.secret_path(a_id)}")])
        self.assertIn("的权限是 0755", perm[0]["text"])
        os.chmod(self.creds_path, 0o644)
        self._stub_claude()
        local = self.cam.checkup_local_claude(store)
        self.assertIn(("warn", f"chmod 600 {self.creds_path}"), [(i["level"], i["advice"]) for i in local])

    def test_checkup_survives_corrupt_files(self):
        self._two_accounts()
        with open(self.creds_path, "w", encoding="utf-8") as f:
            f.write("{坏了")
        res = self._checkup(offline=True)
        local = [(i["level"], i["text"]) for i in self._section(res, "claude_code")]
        self.assertEqual(sum(1 for lv, t in local if "文件损坏" in t), 1, local)  # 报一次，其它几节照常
        self.assertTrue(self._group(self._section(res, "accounts"), "A"))
        # 一节出错只影响这一节
        with mock.patch.object(self.cam, "verify_sums", mock.Mock(side_effect=RuntimeError("炸了"))):
            res = self._checkup(offline=True)
        self.assertEqual([(i["level"], i["text"]) for i in self._section(res, "cam")], [("bad", "这一节没查完：RuntimeError: 炸了")])
        self.assertTrue(self._section(res, "accounts"))

    @unittest.skipIf(os.name == "nt" or (hasattr(os, "geteuid") and os.geteuid() == 0), "靠文件权限制造读不了的情况，Windows 和 root 下不成立")
    def test_checkup_explains_unreadable_claude_json(self):
        # 2026-10-29 Mac 上 ~/.claude.json 读不了，体检整节只报 "这一节没查完：PermissionError"。要说清哪个文件、属主、怎么修
        self._two_accounts()
        me = getpass.getuser()
        os.chmod(self.cj_path, 0)
        try:
            with self.assertRaises(self.cam.CamError) as cm:
                self.cam.read_json(self.cj_path)
            self.assertEqual(str(cm.exception), f"没有权限读 {self.cj_path}（权限 00，当前用户 {me}）")
            self.assertEqual(cm.exception.advice, f"chmod u+rw {self.cj_path}")
            # 属主是别人（用 sudo 跑过 claude）时给 chown
            with mock.patch("pwd.getpwuid", return_value=mock.Mock(pw_name="root")):
                msg, fix = self.cam.unreadable_message(self.cj_path)
            self.assertEqual(msg, f"没有权限读 {self.cj_path}（属主 root，权限 00；当前用户 {me}）")
            self.assertIn(f"sudo chown {me} {self.cj_path}", fix)
            res = self._checkup(offline=True)
            for key in ("accounts", "service", "claude_code", "cam"):
                self.assertFalse(any("没查完" in i["text"] for i in self._section(res, key)), (key, self._section(res, key)))
            local = self._section(res, "claude_code")
            self.assertEqual([(i["level"], i["advice"]) for i in local if "没有权限读" in i["text"]],
                             [("bad", f"chmod u+rw {self.cj_path}")], local)
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                rc = self.cam.main(["current"])
            self.assertEqual(rc, 1, err.getvalue())
            self.assertIn("没有权限读", err.getvalue())
            self.assertIn(f"→ chmod u+rw {self.cj_path}", err.getvalue())
        finally:
            os.chmod(self.cj_path, 0o600)

    @unittest.skipIf(os.name == "nt", "用 sh 脚本模拟机器人命令，Windows 上跳过")
    def test_checkup_runs_chat_bot_doctors(self):
        bindir = os.path.join(self.tmp, ".local", "bin")
        ran = os.path.join(self.tmp, "ran.txt")
        stub(bindir, "claude-discord", f"""echo "discord $*" >> "{ran}"
cat <<'OUT'
工具
  ✓ claude
  ✗ 没有机器人 token
    → 在 Claude Code 里运行 /discord:configure <token>
  ✗ 还没配对 Discord 账号
    → 私信机器人任意一句话
OUT
exit 2""")
        stub(bindir, "claude-feishu", f'echo "feishu $*" >> "{ran}"\necho "  ✓ 全部正常"')
        stub(bindir, "claude-dingtalk", f'echo "dingtalk $*" >> "{ran}"\nexec sleep 5')
        with mock.patch.object(self.cam, "BOT_DOCTOR_TIMEOUT", 1):
            bots = self._section(self._checkup(), "bots")
        self.assertEqual([(i["level"], i["text"], i["advice"]) for i in bots], [
            ("bad", "Discord 机器人：没有机器人 token", "在 Claude Code 里运行 /discord:configure <token>"),
            ("bad", "Discord 机器人：还没配对 Discord 账号", "私信机器人任意一句话"),
            ("ok", "飞书机器人：全部正常（claude-feishu doctor）", ""),
            ("warn", "钉钉机器人：claude-dingtalk doctor 超过 1 秒没跑完", "手动运行 claude-dingtalk doctor 看看卡在哪"),
        ])
        with open(ran) as f:
            self.assertEqual(sorted(f.read().split("\n")[:-1]), ["dingtalk doctor", "discord doctor", "feishu doctor"])
        # 离线不跑（它们要联网）；一个都没装就没有这一节
        os.remove(ran)
        bots = self._section(self._checkup(offline=True), "bots")
        self.assertEqual(bots[0]["text"], "Discord 机器人：离线模式没有运行 claude-discord doctor（它要联网）")
        self.assertFalse(os.path.exists(ran))
        shutil.rmtree(bindir)
        self.assertNotIn("bots", [s["key"] for s in self._checkup(offline=True)["sections"]])

    def test_doctor_json_and_offline(self):
        a_id, b_id = self._two_accounts(a_expires=self._expired_ms())
        MockAnthropic.refresh_map = {"rt-A1": ("at-A2", "rt-A2")}
        MockAnthropic.tokens = {"at-A2": {"u5": 1.0, "u7": 2.0, "email": "a@x.com", "plan": "max"},
                                "at-B1": {"u5": 3.0, "u7": 4.0, "email": "b@x.com", "plan": "max"}}
        self.run_cam("add-api", "--label", "K", "--key", "sk-ant-api-supersecretvalue1234")
        self.run_cam("add-web", "--label", "W", "--session-key", "sk-web-secret")
        self._stub_claude()
        ran = os.path.join(self.tmp, "ran.txt")
        stub(os.path.join(self.tmp, ".local", "bin"), "claude-feishu", f'echo ran >> "{ran}"')

        def doctor(*args):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(self.run_cam("doctor", *args), 0)
            txt = out.getvalue()
            for secret in ("rt-A1", "rt-A2", "at-A2", "at-B1", "rt-B1", "supersecretvalue", "sk-web-secret"):
                self.assertNotIn(secret, txt)
            return txt

        # 离线：不续登录、不联网、不跑机器人自检
        MockAnthropic.calls = []
        res = json.loads(doctor("--offline", "--json"))
        self.assertTrue(res["offline"])
        self.assertEqual(MockAnthropic.calls, [])
        self.assertFalse(os.path.exists(ran))
        self.assertFalse(os.path.exists(os.path.join(self.cam.Store().root, "background.lock")))  # 看一眼后台锁，不为这个去建它
        a = self._group(self._section(res, "accounts"), "A")
        self.assertIn("离线没测能不能续", a[0]["text"])
        self.assertTrue(self._section(res, "service")[0]["text"].startswith("离线模式"))
        self.assertEqual(self.cam.Store().load_secret(a_id)["claudeAiOauth"]["refreshToken"], "rt-A1")
        # 联网
        res = json.loads(doctor("--json"))
        self.assertEqual(set(res), {"version", "generated_at", "offline", "header", "sections", "summary"})
        self.assertFalse(res["offline"])
        self.assertEqual([s["key"] for s in res["sections"]], ["accounts", "service", "claude_code", "cam", "bots"])
        self.assertEqual([s["title"] for s in res["sections"]], ["账号", "Claude 服务", "本机 Claude Code", "cam 自己", "聊天机器人"])
        for sec in res["sections"]:
            self.assertEqual(set(sec), {"key", "title", "items"})
            for it in sec["items"]:
                self.assertIn(it["level"], ("ok", "warn", "bad", "info"))
                self.assertTrue(isinstance(it["text"], str) and it["text"] and isinstance(it["advice"], str), it)
                self.assertLessEqual(set(it), {"level", "text", "advice", "group", "kind", "score", "grade", "ms"})
        s = res["summary"]
        self.assertEqual(set(s), {"bad", "warn", "text", "first"})
        items = [i for sec in res["sections"] for i in sec["items"] if i.get("kind") != "summary"]
        self.assertEqual((s["bad"], s["warn"]), (sum(i["level"] == "bad" for i in items), sum(i["level"] == "warn" for i in items)))
        self.assertEqual(s["text"], f"{s['bad']} 项要处理，{s['warn']} 项留意")
        if os.name != "nt":  # 机器人命令是 sh 脚本模拟的，Windows 上跑不了
            self.assertTrue(os.path.exists(ran))
        # 文字版：标记顶格（install.sh 按 ^(✗|!) 挑出要留意的行），第一行是版本，最后一行是总结
        lines = doctor().splitlines()
        self.assertTrue(lines[0].startswith("cam "))
        self.assertTrue(lines[-1].startswith("总结："))
        self.assertIn("1. 账号", lines)
        self.assertTrue(any(l.startswith("[A a@x.com（Claude Code") for l in lines))
        self.assertTrue(any(l[:2] in ("✓ ", "! ", "✗ ", "· ") for l in lines))
        self.assertTrue(all(l.startswith("    → ") for l in lines if l.lstrip().startswith("→")), lines)

    def test_checkup_http_api_and_menu(self):
        self._two_accounts()
        MockAnthropic.tokens = {"at-B1": {"u5": 1.0, "u7": 2.0, "email": "b@x.com", "plan": "max"}}
        self._stub_claude()
        store = self.cam.Store()
        token = self.cam.server_token(store)
        srv = self.cam.ThreadingServer(("127.0.0.1", 0), self.cam.make_handler(store, token))
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def call(path, tok=token):
            req = urllib.request.Request(f"http://127.0.0.1:{srv.server_address[1]}{path}", headers={"Authorization": f"Bearer {tok}"})
            try:
                with opener.open(req, timeout=60) as r:
                    return r.status, r.read().decode()
            except urllib.error.HTTPError as e:
                with e:
                    return e.code, e.read().decode()

        try:
            self.assertEqual(call("/api/checkup?probe=1", tok="wrong")[0], 401)
            self.assertEqual(call("/api/checkup", tok="")[0], 401)
            self.assertEqual(call("/api/checkup?probe=1"), (200, '{"ok": true}'))  # 面板据此显示“全面体检”按钮
            st, txt = call("/api/checkup?offline=1")
            self.assertEqual(st, 200)
            self.assertTrue(json.loads(txt)["offline"])
            st, txt = call("/api/checkup")
            self.assertEqual(st, 200, txt)
            res = json.loads(txt)
            self.assertFalse(res["offline"])
            self.assertEqual(res["sections"][0]["key"], "accounts")
            for secret in ("rt-A1", "at-B1", "rt-B1"):
                self.assertNotIn(secret, txt)
        finally:
            srv.shutdown()
            srv.server_close()
        # 菜单栏：“全面体检”在后台跑 cam doctor --json 并通知；另一项在终端里看完整结果
        cam_menu = self._menu_plugin()
        told = []
        cam_menu.notify = lambda title, body: told.append((title, body))
        lines = []
        cam_menu.render(store, {}, out=lines.append)
        item = next(l for l in lines if l.startswith("全面体检 |"))
        self.assertIn(f'param1="{menu_arg(cam_menu.MENU_PY)}" param2="checkup"', item)
        self.assertIn("terminal=false", item)
        term = next(l for l in lines if l.startswith("--在终端看完整体检结果 |"))
        self.assertIn(f'param1="{menu_arg(cam_menu.CAM_PY)}" param2="doctor" terminal=true', term)
        self.assertEqual(cam_menu.checkup_from_menu(), 0)
        self.assertEqual(told[-1][0], "全面体检")
        self.assertRegex(told[-1][1], r"^(一切正常|\d+ 项要处理：.+|\d+ 项留意：.+)$")
        with mock.patch.object(cam_menu, "CAM_PY", os.path.join(self.tmp, "nope.py")):
            self.assertEqual(cam_menu.checkup_from_menu(), 1)
        self.assertEqual(told[-1][0], "全面体检没有完成")

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


    def test_desktop_profile_switch(self):
        os.environ["CAM_DESKTOP_DRY_RUN"] = "1"
        data_dir = os.path.join(self.tmp, "claude-desktop")
        os.makedirs(data_dir)
        with open(os.path.join(data_dir, "config.json"), "w", encoding="utf-8") as f:
            json.dump({"lastKnownAccountUuid": "abc"}, f)
        os.environ["CAM_CLAUDE_DESKTOP_DATA"] = data_dir
        try:
            store = self.cam.Store()
            cur = self.cam.add_account(store, "claude_desktop", "当前", use_current=True)
            self.assertEqual(cur["desktop_profile"], "default")
            msg = self.cam.open_desktop_account(store, cur)
            self.assertNotIn("点一次邮箱", msg)
            with self.assertRaises(self.cam.CamError):
                self.cam.add_account(store, "claude_desktop", "重复", use_current=True)
            other = self.cam.add_account(store, "claude_desktop", "第二个")
            self.assertNotEqual(other["desktop_profile"], "default")
            self.assertTrue(os.path.isdir(os.path.join(store.root, "desktop-profiles", other["id"])))
            msg = self.cam.open_desktop_account(store, other)
            self.assertIn("点一次邮箱", msg)
            self.assertIn("不会覆盖", msg)
            backups = [n for n in os.listdir(store.backups_dir) if "before-desktop" in n]
            self.assertTrue(backups)
            with open(os.path.join(store.backups_dir, backups[-1], "config.json"), encoding="utf-8") as f:
                saved = json.load(f)
            self.assertEqual(saved["lastKnownAccountUuid"], "abc")
            # 原来的登录文件还在，备份不是把内容搬走
            self.assertTrue(os.path.isfile(os.path.join(data_dir, "config.json")))
            data = store.load()
            self.assertEqual(data["active"]["claude_desktop"], other["id"])
            view = self.cam.public_state(store, data)
            marked = next(a for a in view["accounts"] if a["id"] == other["id"])
            self.assertTrue(marked["is_active"])
            current = next(a for a in view["accounts"] if a["id"] == cur["id"])
            self.assertFalse(current["is_active"])
        finally:
            os.environ.pop("CAM_DESKTOP_DRY_RUN", None)
            os.environ.pop("CAM_CLAUDE_DESKTOP_DATA", None)


    def test_desktop_refresh_skips_api_and_hides_old_errors(self):
        store = self.cam.Store()
        acc = self.cam.add_account(store, "claude_desktop", "桌面")
        data = store.load()
        data["accounts"][0]["usage"] = {"error": "没有填写 API Key", "key_valid": False}
        store.save(data)
        view = self.cam.public_state(store, data)["accounts"][0]
        self.assertIsNone(view["usage"])
        self.assertEqual(view["health"]["level"], "ok")
        with mock.patch.object(self.cam, "snapshot_live"), mock.patch.object(self.cam, "fetch_api_usage") as fetch:
            data = self.cam.refresh_usage(store, acc["id"])
        fetch.assert_not_called()
        self.assertEqual(data["accounts"][0]["usage"]["cache_status"], "missing")
        self.assertIsNone(data["accounts"][0]["usage"]["error"])

    def test_desktop_main_process_matching_and_profile_parsing(self):
        with mock.patch.dict(os.environ, {"CAM_CLAUDE_DESKTOP": "/custom/desktop-app"}):
            with mock.patch.object(self.cam, "IS_MAC", False), mock.patch.object(self.cam, "IS_WIN", False):
                for cmd in ("/usr/bin/claude-desktop", "/opt/Claude/claude --user-data-dir=/tmp/a",
                            "/custom/desktop-app --user-data-dir=/tmp/a"):
                    self.assertTrue(self.cam._is_desktop_cmd(cmd), cmd)
                for cmd in ("/usr/local/bin/claude", "node /tmp/@anthropic-ai/claude-code/cli.js",
                            "/opt/Claude/claude --type=renderer", "sh -c /usr/bin/claude-desktop"):
                    self.assertFalse(self.cam._is_desktop_cmd(cmd), cmd)
            with mock.patch.object(self.cam, "IS_MAC", True), mock.patch.object(self.cam, "IS_WIN", False):
                self.assertTrue(self.cam._is_desktop_cmd("/Applications/Claude.app/Contents/MacOS/Claude"))
                self.assertFalse(self.cam._is_desktop_cmd("/Applications/Claude.app/Contents/Frameworks/Claude Helper"))
            with mock.patch.object(self.cam, "IS_MAC", False), mock.patch.object(self.cam, "IS_WIN", True):
                self.assertTrue(self.cam._is_desktop_cmd(r'C:\Users\me\AppData\Local\AnthropicClaude\app-1.2\claude.exe'))
                self.assertTrue(self.cam._is_desktop_cmd(r'"C:\Program Files\WindowsApps\AnthropicPBC.Claude_1.2\Claude.exe"'))
                self.assertFalse(self.cam._is_desktop_cmd(r'C:\Users\me\.local\bin\claude.exe'))
        given = os.path.realpath(os.path.join(self.tmp, "with spaces"))
        expected = os.path.normcase(given)  # Windows 上统一成小写比较
        for flag in (f'--user-data-dir="{given}"', f'--user-data-dir "{given}" --other-flag',
                     f'--user-data-dir={given}'):
            self.assertEqual(self.cam._desktop_profile_from_command("Claude " + flag), expected)
        self.assertIsNone(self.cam._desktop_profile_from_command("Claude"))

    @unittest.skipIf(os.name == "nt", "模拟的是 ps 和 lsof 的输出，Windows 上走的是 PowerShell")
    def test_desktop_process_query_failures_and_macos_file_evidence(self):
        failed = subprocess.CompletedProcess([], 1, stdout="", stderr="permission denied")
        with mock.patch.object(self.cam.subprocess, "run", return_value=failed):
            with self.assertRaisesRegex(self.cam.CamError, "无法检测"):
                self.cam._desktop_processes()
        with mock.patch.object(self.cam.subprocess, "run", side_effect=OSError("no ps")):
            with self.assertRaisesRegex(self.cam.CamError, "无法检测"):
                self.cam._desktop_processes()
        root = os.path.realpath(os.path.join(self.tmp, "profile"))
        ps = subprocess.CompletedProcess([], 0, stdout=" 101 /Applications/Claude.app/Contents/MacOS/Claude\n")
        files = subprocess.CompletedProcess([], 0, stdout=f"p101\nn{root}/Network/Cookies\nn{root}/config.json\n")
        with mock.patch.object(self.cam, "IS_MAC", True), mock.patch.object(self.cam, "IS_WIN", False), \
                mock.patch.object(self.cam.subprocess, "run", side_effect=[ps, files]):
            self.assertEqual(self.cam._desktop_processes(), [(101, root)])

    def test_desktop_same_account_restarts_and_backs_up_after_quit(self):
        store = self.cam.Store()
        acc = self.cam.add_account(store, "claude_desktop", "A")
        directory = self.cam.desktop_profile_dir(store, acc)
        data = store.load()
        data["active"]["claude_desktop"] = acc["id"]
        store.save(data)
        events = []
        def quit_app():
            events.append("quit")
            with open(os.path.join(directory, "config.json"), "w") as f:
                json.dump({"oauth:tokenCacheV2": "after-quit"}, f)
        real_backup = self.cam._backup_desktop_login
        def backup(s, path):
            events.append("backup")
            return real_backup(s, path)
        with mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, directory)]), \
                mock.patch.object(self.cam, "_find_claude_desktop", return_value="/fake/Claude"), \
                mock.patch.object(self.cam, "_quit_claude_desktop", side_effect=quit_app), \
                mock.patch.object(self.cam, "_backup_desktop_login", side_effect=backup), \
                mock.patch.object(self.cam, "_launch_claude_desktop", side_effect=lambda *a: events.append("launch")):
            self.cam.open_desktop_account(store, acc)
        self.assertEqual(events, ["quit", "backup", "launch"])
        backups = os.listdir(store.backups_dir)
        with open(os.path.join(store.backups_dir, backups[0], "config.json")) as f:
            self.assertEqual(json.load(f)["oauth:tokenCacheV2"], "after-quit")

    def test_desktop_unknown_profile_blocks_switch_before_exit(self):
        store = self.cam.Store()
        acc = self.cam.add_account(store, "claude_desktop", "A")
        with mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, None)]), \
                mock.patch.object(self.cam, "_quit_claude_desktop") as quit_app:
            with self.assertRaisesRegex(self.cam.CamError, "无法确认"):
                self.cam.open_desktop_account(store, acc)
        quit_app.assert_not_called()
        self.assertIsNone(store.load()["active"].get("claude_desktop"))

    def test_desktop_operation_lock_blocks_switch_and_removal(self):
        store = self.cam.Store()
        acc = self.cam.add_account(store, "claude_desktop", "A")
        lock = self.cam.FileLock(os.path.join(store.root, "desktop-switch.lock"))
        self.assertTrue(lock.try_acquire())
        try:
            with self.assertRaisesRegex(self.cam.CamError, "正在切换或移除"):
                self.cam.open_desktop_account(store, acc)
            with self.assertRaisesRegex(self.cam.CamError, "正在切换或移除"):
                self.cam.remove_account(store, acc["id"])
        finally:
            lock.release()

    def test_desktop_failed_launch_recovers_actual_profile_and_reports_failure(self):
        store = self.cam.Store()
        acc = self.cam.add_account(store, "claude_desktop", "A")
        previous = os.path.join(self.tmp, "actually-running")
        with mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, previous)]), \
                mock.patch.object(self.cam, "_find_claude_desktop", return_value="/fake/Claude"), \
                mock.patch.object(self.cam, "_quit_claude_desktop") as quit_app, \
                mock.patch.object(self.cam, "_launch_claude_desktop", side_effect=[self.cam.CamError("target failed"), None]) as launch:
            with self.assertRaisesRegex(self.cam.CamError, "已重新打开原来的登录目录"):
                self.cam.open_desktop_account(store, acc)
        self.assertEqual(quit_app.call_count, 2)
        self.assertEqual(launch.call_args_list[-1].args, ("/fake/Claude", previous))
        self.assertIsNone(store.load()["active"].get("claude_desktop"))
        with mock.patch.object(self.cam, "_desktop_processes", return_value=[]), \
                mock.patch.object(self.cam, "_find_claude_desktop", return_value="/fake/Claude"), \
                mock.patch.object(self.cam, "_quit_claude_desktop"), \
                mock.patch.object(self.cam, "_launch_claude_desktop", side_effect=self.cam.CamError("failed")) as launch:
            with self.assertRaisesRegex(self.cam.CamError, "也没能重新打开"):
                self.cam.open_desktop_account(store, acc)
        self.assertEqual(launch.call_args_list[-1].args[1], self.cam.default_desktop_user_data())

    def test_desktop_backup_and_state_failures_also_restore_previous_profile(self):
        store = self.cam.Store()
        previous = self.cam.add_account(store, "claude_desktop", "old")
        target = self.cam.add_account(store, "claude_desktop", "new")
        previous_dir = self.cam.desktop_profile_dir(store, previous)
        for failure in ("_backup_desktop_login", "_remember_desktop"):
            with mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, previous_dir)]), \
                    mock.patch.object(self.cam, "_find_claude_desktop", return_value="/fake/Claude"), \
                    mock.patch.object(self.cam, "_quit_claude_desktop") as quit_app, \
                    mock.patch.object(self.cam, "_launch_claude_desktop") as launch, \
                    mock.patch.object(self.cam, failure, side_effect=OSError("disk error")):
                with self.assertRaisesRegex(self.cam.CamError, "已重新打开原来的登录目录"):
                    self.cam.open_desktop_account(store, target)
            self.assertEqual(quit_app.call_count, 2)
            self.assertEqual(launch.call_args_list[-1].args, ("/fake/Claude", previous_dir))
            self.assertEqual(store.load()["active"]["claude_desktop"], previous["id"])

    def test_desktop_launcher_checks_exit_and_real_profile(self):
        directory = os.path.realpath(os.path.join(self.tmp, "profile"))
        launcher = mock.Mock()
        launcher.wait.return_value = 1
        with mock.patch.object(self.cam, "IS_MAC", True), mock.patch.object(self.cam.subprocess, "Popen", return_value=launcher) as popen:
            with self.assertRaisesRegex(self.cam.CamError, "macOS 没能启动"):
                self.cam._launch_claude_desktop("/fake/Claude.app", directory)
            self.assertEqual(popen.call_args.args[0][:2], ["open", "-n"])
        launcher.wait.return_value = 0
        launcher.poll.return_value = 0
        with mock.patch.object(self.cam, "IS_MAC", True), \
                mock.patch.object(self.cam.subprocess, "Popen", return_value=launcher), \
                mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, None)]), \
                mock.patch.object(self.cam.time, "monotonic", side_effect=[0, 11]):
            with self.assertRaisesRegex(self.cam.CamError, "未能确认"):
                self.cam._launch_claude_desktop("/fake/Claude.app", directory)
        with mock.patch.object(self.cam, "IS_MAC", False), mock.patch.object(self.cam, "IS_WIN", False), \
                mock.patch.object(self.cam.subprocess, "Popen", return_value=launcher):
            with self.assertRaisesRegex(self.cam.CamError, "启动后退出"):
                self.cam._launch_claude_desktop("/fake/Claude", directory)
        launcher.poll.return_value = None
        with mock.patch.object(self.cam, "IS_MAC", False), mock.patch.object(self.cam, "IS_WIN", False), \
                mock.patch.object(self.cam.subprocess, "Popen", return_value=launcher), \
                mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, os.path.normcase(directory))]):
            self.cam._launch_claude_desktop("/fake/Claude", directory)

    def test_desktop_removal_preserves_active_and_running_profiles(self):
        store = self.cam.Store()
        acc = self.cam.add_account(store, "claude_desktop", "A")
        directory = os.path.realpath(self.cam.desktop_profile_dir(store, acc))
        if os.name != "nt":
            self.assertEqual(os.stat(directory).st_mode & 0o777, 0o700)
        data = store.load()
        data["active"]["claude_desktop"] = acc["id"]
        store.save(data)
        with self.assertRaisesRegex(self.cam.CamError, "当前使用"):
            self.cam.remove_account(store, acc["id"])
        data["active"].pop("claude_desktop")
        store.save(data)
        for profile in (os.path.normcase(directory), None):  # _desktop_processes 返回的路径是 normcase 过的
            with mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, profile)]):
                with self.assertRaisesRegex(self.cam.CamError, "可能仍在使用"):
                    self.cam.remove_account(store, acc["id"])
            self.assertTrue(os.path.isdir(directory))
            self.assertTrue(self.cam.find_account(store.load(), acc["id"]))
        with mock.patch.object(self.cam, "_desktop_processes", return_value=[(101, os.path.join(self.tmp, "other"))]):
            _, backup = self.cam.remove_account(store, acc["id"])
        self.assertFalse(os.path.exists(directory))
        self.assertTrue(os.path.isdir(os.path.join(backup, "desktop-profile")))

    def test_desktop_network_cookie_login_and_backup(self):
        store = self.cam.Store()
        directory = os.path.join(self.tmp, "desktop-data")
        os.makedirs(os.path.join(directory, "Network"))
        for name in ("Cookies", "Cookies-wal", "Cookies-shm"):
            with open(os.path.join(directory, "Network", name), "w") as f:
                f.write("login cache")
        self.assertTrue(self.cam._desktop_has_login(directory))
        self.cam._backup_desktop_login(store, directory)
        backup = os.path.join(store.backups_dir, os.listdir(store.backups_dir)[0], "Network")
        self.assertEqual(sorted(os.listdir(backup)), ["Cookies", "Cookies-shm", "Cookies-wal"])

    def _desktop_quota_setup(self):
        store = self.cam.Store()
        acc = self.cam.add_account(store, "claude_desktop", "quota")
        directory = self.cam.desktop_profile_dir(store, acc)
        fixture = shared("desktop_usage.json")
        return store, acc, directory, fixture

    def _desktop_cache_entry(self, directory, fixture, body=None, org=None, name="sample_0", age=0, raw=None, url=None):
        key = ("1/0/" + (url or f"https://claude.ai/api/organizations/{org or fixture['org_uuid']}/usage")).encode()
        header = bytes.fromhex("305c72a71b6dfbfc") + (5).to_bytes(4, "little") + len(key).to_bytes(4, "little") + bytes(8)
        payload = raw if raw is not None else json.dumps(body or fixture["body"]).encode() + bytes([171]) * 32
        path = os.path.join(directory, "Cache", "Cache_Data", name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(header + key + payload)
        stamp = time.time() - age
        os.utime(path, (stamp, stamp))
        return path

    def _desktop_history(self, directory, samples, version=2):
        with open(os.path.join(directory, "plan-usage-history.json"), "w") as f:
            json.dump({"version": version, "samples": samples}, f)

    def test_desktop_quota_reads_plain_and_zstd_cache_with_eof(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        path = os.path.join(directory, "Cache", "Cache_Data", "sample_0")
        os.makedirs(os.path.dirname(path))
        for field in ("plain_entry_base64", "zstd_entry_base64"):
            with open(path, "wb") as f:
                f.write(base64.b64decode(fixture[field]))
            result = self.cam.fetch_desktop_usage(store, acc)
            self.assertEqual(result["source"], "desktop_cache")
            self.assertEqual(result["cache_status"], "ok")
            self.assertEqual(result["five_hour"], fixture["body"]["five_hour"])
            self.assertEqual(result["seven_day_sonnet"], fixture["body"]["seven_day_sonnet"])
            self.assertEqual(result["cache_org_uuid"], fixture["org_uuid"])

    def test_desktop_quota_refresh_preserves_sample_time_and_account_binding(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        path = self._desktop_cache_entry(directory, fixture, age=120)
        with open(os.path.join(directory, "config.json"), "w") as f:
            json.dump({"lastKnownAccountUuid": "owner-A"}, f)
        self._desktop_history(directory, [{"t": (time.time() - 300) * 1000, "org": fixture["org_uuid"], "u": {"fh": 90}}])
        with mock.patch.object(self.cam, "snapshot_live"), mock.patch.object(self.cam, "web_get") as web:
            data = self.cam.refresh_usage(store, acc["id"])
        web.assert_not_called()
        saved = data["accounts"][0]
        self.assertEqual(saved["desktop_account_uuid"], "owner-A")
        self.assertEqual(saved["usage"]["five_hour"]["utilization"], 37.5)
        self.assertAlmostEqual(self.cam.parse_iso(saved["usage"]["fetched_at"]).timestamp(), os.path.getmtime(path), places=3)
        self.assertGreater(self.cam.parse_iso(saved["usage"]["checked_at"]), self.cam.parse_iso(saved["usage"]["fetched_at"]))
        self.assertEqual(self.cam.public_state(store, data)["accounts"][0]["usage"]["source"], "desktop_cache")

    def test_desktop_quota_organization_ambiguity_and_explicit_hint(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        self._desktop_cache_entry(directory, fixture, name="first_0")
        self._desktop_cache_entry(directory, fixture, org=fixture["other_org_uuid"], name="second_0")
        self._desktop_history(directory, [{"t": time.time() * 1000, "org": fixture["org_uuid"], "u": {"fh": 90}}])
        # 这些 config 字段并非客户端实际的组织来源，不能用来解除歧义。
        with open(os.path.join(directory, "config.json"), "w") as f:
            json.dump({"lastKnownOrgUuid": fixture["org_uuid"], "deploymentOrganizationUuid": fixture["org_uuid"]}, f)
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["cache_status"], "ambiguous")
        self.assertIsNone(result["five_hour"])
        acc["org_uuid"] = fixture["org_uuid"]
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_org_uuid"], fixture["org_uuid"])

    def test_desktop_quota_rejects_expired_invalid_and_future_cache(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        body = {"five_hour": {"utilization": 10, "resets_at": "2000-01-01T00:00:00Z"},
                "seven_day": {"utilization": float("nan"), "resets_at": "2099-01-01T00:00:00Z"},
                "seven_day_opus": {"utilization": True, "resets_at": "2099-01-01T00:00:00Z"},
                "seven_day_sonnet": {"utilization": 101, "resets_at": "2099-01-01T00:00:00Z"}}
        self._desktop_cache_entry(directory, fixture, body=body)
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["cache_status"], "expired")
        self.assertTrue(all(result[k] is None for k in self.cam._DESKTOP_WINDOWS))
        self._desktop_cache_entry(directory, fixture, age=-600)
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "missing")

    def test_desktop_quota_without_reset_uses_window_age_and_stale_flag(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        self._desktop_cache_entry(directory, fixture, body={"five_hour": {"utilization": 10}, "seven_day": {"utilization": 20}}, age=6 * 3600)
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertTrue(result["cache_stale"])
        self.assertIsNone(result["five_hour"])
        self.assertEqual(result["seven_day"], {"utilization": 20, "resets_at": None})

    def test_desktop_quota_strict_cache_url_and_input_limit(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        for url in (f"https://evil.test/https://claude.ai/api/organizations/{fixture['org_uuid']}/usage",
                    f"https://claude.ai.evil.test/api/organizations/{fixture['org_uuid']}/usage",
                    f"https://claude.ai/api/organizations/{fixture['org_uuid']}/usage-other"):
            self._desktop_cache_entry(directory, fixture, url=url)
            self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "missing", url)
        self._desktop_cache_entry(directory, fixture, raw=b" " * (64 * 1024))
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "missing")

    def test_desktop_quota_zstd_output_limit_and_missing_decoder(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        with self.assertRaisesRegex(ValueError, "超过读取上限"):
            self.cam._desktop_decode_usage(base64.b64decode(fixture["oversize_zstd_base64"]))
        path = self._desktop_cache_entry(directory, fixture)
        with open(path, "wb") as f:
            f.write(base64.b64decode(fixture["zstd_entry_base64"]))
        original_import = __import__
        def without_zstd(name, *args, **kwargs):
            if name in ("compression", "zstandard"):
                raise ImportError(name)
            return original_import(name, *args, **kwargs)
        with mock.patch("builtins.__import__", side_effect=without_zstd):
            result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["cache_status"], "unsupported_compression")
        self.assertIsNone(result["five_hour"])

    def test_desktop_quota_history_v1_v2_and_aging(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        sampled = (time.time() - 6 * 3600) * 1000
        self._desktop_history(directory, [{"t": sampled, "fh": 22, "sd": 44}], version=1)
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["source"], "desktop_history")
        self.assertIsNone(result["five_hour"])
        self.assertEqual(result["seven_day"], {"utilization": 44, "resets_at": None})
        self.assertIsNone(result["cache_org_uuid"])
        self._desktop_history(directory, [{"t": time.time() * 1000, "org": fixture["org_uuid"],
                                           "u": {"fh": 12, "sd": 23, "so": 34, "sn": 45}}])
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["five_hour"]["utilization"], 12)
        self.assertEqual(result["seven_day_opus"]["utilization"], 34)
        self.assertEqual(result["cache_org_uuid"], fixture["org_uuid"])

    def test_desktop_quota_history_ambiguity_future_and_size_limit(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        samples = [{"t": time.time() * 1000, "org": org, "u": {"fh": 22}} for org in (fixture["org_uuid"], fixture["other_org_uuid"])]
        self._desktop_history(directory, samples)
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "ambiguous")
        acc["org_uuid"] = fixture["org_uuid"]
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "ok")
        self._desktop_history(directory, [{"t": (time.time() + 600) * 1000, "u": {"fh": 22}}])
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "missing")
        with open(os.path.join(directory, "plan-usage-history.json"), "wb") as f:
            f.write(b" " * (4 * 1024 * 1024 + 1))
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "missing")

    def test_desktop_quota_account_mismatch_never_rebinds_or_reads_cached_quota(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        acc["desktop_account_uuid"] = "saved-owner"
        with open(os.path.join(directory, "config.json"), "w") as f:
            json.dump({"lastKnownAccountUuid": "different-owner"}, f)
        self._desktop_cache_entry(directory, fixture)
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["cache_status"], "missing")
        self.assertIsNone(result["five_hour"])
        self.assertEqual(acc["desktop_account_uuid"], "saved-owner")

    def test_desktop_quota_uses_newest_sample_without_reviving_expired_cache(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        expired = {"five_hour": {"utilization": 99, "resets_at": "2000-01-01T00:00:00Z"}}
        self._desktop_cache_entry(directory, fixture, body=expired)
        self._desktop_history(directory, [{"t": (time.time() - 300) * 1000, "org": fixture["org_uuid"], "u": {"fh": 77}}])
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "expired")
        self._desktop_cache_entry(directory, fixture, age=300)
        self._desktop_history(directory, [{"t": time.time() * 1000, "org": fixture["org_uuid"], "u": {"fh": 12}}])
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["source"], "desktop_history")
        self.assertEqual(result["five_hour"]["utilization"], 12)

    def test_desktop_quota_rejects_unknown_history_org_with_hint_and_cross_source_conflicts(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        acc["org_uuid"] = fixture["org_uuid"]
        self._desktop_history(directory, [{"t": time.time() * 1000, "fh": 20, "sd": 30}], version=1)
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "missing")
        self._desktop_history(directory, [{"t": time.time() * 1000, "org": "invalid-org", "u": {"fh": 20}}])
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "missing")
        acc.pop("org_uuid")
        self._desktop_cache_entry(directory, fixture, age=300)
        self._desktop_history(directory, [{"t": time.time() * 1000, "org": fixture["other_org_uuid"], "u": {"fh": 20}}])
        self.assertEqual(self.cam.fetch_desktop_usage(store, acc)["cache_status"], "ambiguous")

    def test_desktop_quota_newer_expired_history_blocks_older_cache(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        self._desktop_cache_entry(directory, fixture, age=10 * 86400)
        self._desktop_history(directory, [{"t": (time.time() - 8 * 86400) * 1000,
                                           "org": fixture["org_uuid"], "u": {"fh": 20, "sd": 30}}])
        result = self.cam.fetch_desktop_usage(store, acc)
        self.assertEqual(result["source"], "desktop_history")
        self.assertEqual(result["cache_status"], "expired")
        self.assertIsNone(result["five_hour"])

    def test_desktop_quota_malformed_config_is_soft_missing(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        self._desktop_cache_entry(directory, fixture)
        with open(os.path.join(directory, "config.json"), "w") as f:
            f.write("{unfinished")
        with mock.patch.object(self.cam, "snapshot_live"):
            saved = self.cam.refresh_usage(store, acc["id"])["accounts"][0]
        self.assertEqual(saved["usage"]["cache_status"], "missing")
        self.assertIsNone(saved["usage"]["five_hour"])
        self.assertIsNone(saved["usage"]["error"])

    def test_desktop_quota_public_and_cli_hide_reset_windows_and_label_snapshots(self):
        store, acc, directory, fixture = self._desktop_quota_setup()
        data = store.load()
        sample = data["accounts"][0]
        data["active"]["claude_desktop"] = acc["id"]
        sample["usage"] = {"source": "desktop_cache", "fetched_at": "2000-01-01T00:00:00Z", "cache_status": "ok",
                           "five_hour": {"utilization": 99, "resets_at": "2000-01-02T00:00:00Z"}}
        view = self.cam.public_state(store, data)["accounts"][0]
        self.assertIsNone(view["usage"]["five_hour"])
        self.assertEqual(view["usage"]["cache_status"], "expired")
        self.assertEqual(sample["usage"]["five_hour"]["utilization"], 99)  # 展示不改存档
        with mock.patch("sys.stdout", new_callable=io.StringIO) as output:
            self.cam.print_accounts(store, data)
        text = output.getvalue()
        self.assertIn("[上次打开]", text)
        self.assertIn("桌面本地缓存", text)
        self.assertIn("记录时间", text)
        self.assertIn("旧快照", text)
        self.assertNotIn("99.0%", text)
    def test_review_round_fixes(self):
        cam = self.cam
        # 星期范围先校验再展开；代理地址端口不是数字也照样遮密码
        self.assertEqual(cam.parse_days("1-5"), [1, 2, 3, 4, 5])
        for bad in ("1-4000000000", "5-1", "0,3", [8]):
            with self.assertRaises(cam.CamError):
                cam.parse_days(bad)
        self.assertEqual(cam.mask_proxy("http://u:p@host:port"), "http://***@host:port")
        self.assertEqual(cam.mask_proxy("http://u:p@host:8080/path"), "http://***@host:8080")
        self.assertEqual(cam.mask_proxy("http://alice:S3cretPass@proxy.local:8080"), "http://***@proxy.local:8080")
        # Electron 子进程、Linux 上同名的桌面版都不算 Claude Code 会话
        with mock.patch.object(cam, "_desktop_candidates", return_value=["/opt/Claude/claude"]):
            self.assertFalse(cam._is_claude_cmd("/opt/Claude/claude", "/opt/Claude/claude --type=renderer --x"))
            self.assertFalse(cam._is_claude_cmd("/opt/Claude/claude", "/opt/Claude/claude"))
            self.assertTrue(cam._is_claude_cmd("/usr/local/bin/claude", "/usr/local/bin/claude --resume"))
        # 当前账号的 access token 过期：是提示不是异常；expires_in 为 null 也不崩
        self.assertEqual(cam._health_kind_of_error("claude_code", "当前登录的 access token 过期了，等 Claude Code 自己续"), "live_expired")
        self.assertEqual(cam.account_health({"kind": "claude_code", "usage": {"error": "当前登录的 access token 过期了，等 Claude Code 自己续"}}, True, None)["level"], "info")
        store = cam.Store()
        far = int((time.time() + 3600) * 1000)
        data = store.load()
        for i, (lab, rt) in enumerate((("A", "rtA"), ("B", "rtB"))):
            aid = f"id{i}"
            data["accounts"].append({"id": aid, "kind": "claude_code", "label": lab, "email": f"{lab.lower()}@x.com",
                                     "account_uuid": f"uuid-{lab}", "token_fp": cam.token_fp(rt), "manual": {}, "usage": None})
            store.save_secret(aid, {"claudeAiOauth": {"accessToken": f"at{lab}", "refreshToken": rt, "expiresAt": far}, "saved_at": cam.now_iso()})
        data["active"]["claude_code"] = "id0"
        store.save(data)
        with mock.patch.object(cam, "refresh_oauth", return_value={"access_token": "newA", "expires_in": None}):
            cam.renew_saved_login(store, data["accounts"][0])
        self.assertEqual(store.load_secret("id0")["claudeAiOauth"]["accessToken"], "newA")
        # 当前登录是未知的 token、~/.claude.json 还写着 B、服务端 503：不能把它存进 B 名下，要暂存并提示核对不了
        live = {"claudeAiOauth": {"accessToken": "atX", "refreshToken": "rtX", "expiresAt": far}}
        with mock.patch.object(cam, "read_live_credentials", return_value=(live, "file")), \
             mock.patch.object(cam, "read_oauth_account", return_value={"accountUuid": "uuid-B", "emailAddress": "b@x.com"}), \
             mock.patch.object(cam, "probe_token_owner", return_value=(503, None)):
            cam.snapshot_live(store, data)
        self.assertEqual(store.load_secret("id1")["claudeAiOauth"]["refreshToken"], "rtB")
        self.assertTrue(data.get("live_unverified"))
        self.assertEqual(store.load_secret(cam.PENDING_ID)["claudeAiOauth"]["refreshToken"], "rtX")
        # 暂存的 token 核对时限流：留着下次再问
        with mock.patch.object(cam, "probe_token_owner", return_value=(429, None)):
            cam.resolve_pending(store, data, "nope")
        self.assertEqual(store.load_secret(cam.PENDING_ID)["claudeAiOauth"]["expiresAt"], far)
        # 401：access token 不认了，refresh token 也许还能用。记成已过期留着（切换、手动刷新时换一个再核对），不再拿它白问
        with mock.patch.object(cam, "probe_token_owner", return_value=(401, None)) as probe:
            cam.resolve_pending(store, data, "nope")
            cam.resolve_pending(store, data, "nope")
        self.assertEqual(probe.call_count, 1)
        pend = store.load_secret(cam.PENDING_ID)
        self.assertEqual((pend["claudeAiOauth"]["refreshToken"], pend["claudeAiOauth"]["expiresAt"]), ("rtX", 0))
        self.assertEqual(pend["raw"]["claudeAiOauth"]["expiresAt"], 0)
        self.assertTrue(pend.get("released_at"))  # 核对时它已经不是当前登录了
        # 没有 refresh token 的，401 就删：留着也换不出新的
        pend["claudeAiOauth"] = {"accessToken": "atY", "expiresAt": far}
        store.save_secret(cam.PENDING_ID, pend)
        with mock.patch.object(cam, "probe_token_owner", return_value=(401, None)):
            cam.resolve_pending(store, data, "nope")
        self.assertFalse(store.load_secret(cam.PENDING_ID))
        # 移除网页号时浏览器 Profile 搬不动（窗口还开着）：全部搬回去，账号和密钥都还在
        self.run_cam("add-web", "--label", "W", "--session-key", "sk-ant-test")
        data = store.load()
        wid = next(a["id"] for a in data["accounts"] if a["kind"] == "claude_web")
        os.makedirs(os.path.join(store.profiles_dir, wid), exist_ok=True)
        real_move = shutil.move

        def flaky_move(src, dst):
            if dst.endswith("browser-profile"):
                raise PermissionError("locked")
            return real_move(src, dst)
        with mock.patch.object(cam.shutil, "move", flaky_move), self.assertRaises(cam.CamError):
            cam.remove_account(store, wid)
        data = store.load()
        self.assertTrue(any(a["id"] == wid for a in data["accounts"]))
        self.assertEqual(store.load_secret(wid).get("session_key"), "sk-ant-test")
        # 接口：参数不合法回 400 不是 500；页面带禁止嵌套的头；不带令牌的 401 用来认 cam 的面板
        token = cam.server_token(store)
        if not cam.IS_WIN:
            self.assertEqual(os.stat(os.path.join(store.root, "server-token")).st_mode & 0o777, 0o600)
        srv = cam.ThreadingServer(("127.0.0.1", 0), cam.make_handler(store, token))
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        port = srv.server_address[1]
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def call(method, path, body=None, headers=None):
            h = {"Authorization": f"Bearer {token}", "Content-Type": "application/json", **(headers or {})}
            req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method,
                                         data=json.dumps(body).encode() if body is not None else None, headers=h)
            try:
                with opener.open(req) as r:
                    return r.status, json.loads(r.read().decode()), r.headers
            except urllib.error.HTTPError as e:
                return e.code, json.loads(e.read().decode()), e.headers
        self.assertEqual(call("GET", "/api/local-usage?days=abc")[0], 400)
        self.assertEqual(call("POST", "/api/accounts", {"kind": "claude_web", "label": ["x"]})[0], 400)
        self.assertEqual(call("POST", "/api/accounts", {"kind": "claude_code", "label": {"a": 1}})[0], 400)
        st, _, hdr = call("GET", "/api/state")
        self.assertEqual((st, hdr["X-Frame-Options"]), (200, "DENY"))
        with opener.open(f"http://127.0.0.1:{port}/") as r:
            self.assertEqual(r.headers["Content-Security-Policy"], "frame-ancestors 'none'")
        self.assertTrue(cam.cam_panel_at(port))
        self.assertFalse(cam.cam_panel_at(1))
        self.assertTrue(cam.panel_running(port, token))
        with socket.create_connection(("127.0.0.1", port), timeout=5) as sk:  # 负的 Content-Length：直接拒绝，不挂住线程
            sk.sendall(f"POST /api/refresh HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nContent-Length: -1\r\n\r\n".encode())
            self.assertIn(b" 400 ", sk.recv(200))
        self.assertEqual(call("PUT", "/api/menubar", {"mode": "stream"})[0], 400)  # 没装插件：不跑安装脚本
        # 卡片的“切换前确认”开关：记在 prefs 里，卡片再打开还在；只能是 true / false
        self.assertEqual(call("PUT", "/api/menubar", {"confirm_switch": "yes"})[0], 400)
        st, body, _ = call("PUT", "/api/menubar", {"confirm_switch": True})
        self.assertEqual((st, body["confirm_switch"]), (200, True))
        self.assertTrue(call("GET", "/api/menubar")[1]["confirm_switch"])
        self.assertFalse(call("PUT", "/api/menubar", {"confirm_switch": False})[1]["confirm_switch"])


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

    def test_region_check_rules(self):
        """环境检测没连上之后的规则：这一次能不能跳过、隔多久重试、界面上提示哪一条。"""
        import datetime as dt
        fx = shared("region_check.json")
        for c in fx["fresh"]:
            self.assertEqual(self.cam.region_check_fresh(c["age"], c["fage"], c["max_age"], c["moved"]), c["expect"], c["name"])
        for c in fx["retry_wait"]:
            self.assertEqual(self.cam.region_retry_wait(c["fail_count"], c["refresh_min"]), c["expect"], c)
        ago = lambda secs: (dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=secs)).replace(microsecond=0).isoformat()
        for c in fx["fail_item"]:
            r = c["region"]
            reg = {"loc": r["loc"], "fail_count": r.get("fail_count")}
            for key, field in (("checked_secs_ago", "checked_at"), ("failed_secs_ago", "failed_at")):
                if r.get(key) is not None:
                    reg[field] = ago(r[key])
            got = self.cam.region_fail_item(reg)
            if c["expect"] is None:
                self.assertIsNone(got, c["name"])
                continue
            self.assertEqual((got["kind"], got["level"]), (c["expect"]["kind"], c["expect"]["level"]), c["name"])
            for part in c["expect"]["contains"]:
                self.assertIn(part, got["text"], c["name"])
            self.assertEqual(set(got), {"kind", "level", "text", "advice"}, c["name"])
        for c in fx["net_fingerprint"]:
            self.assertEqual(self.cam.net_fingerprint_of(c["addrs"]), c["expect"], c["name"])

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

    def test_usage(self):
        fx = shared("usage.json")
        for c in fx["parse"]:
            self.assertEqual(self.cam.parse_usage_body(c["body"], c["source"]), c["expect"], c["name"])
        for c in fx["cost"]:
            got = self.cam.message_cost(c["model"], c["usage"])
            if c["expect"] is None:
                self.assertIsNone(got, c["name"])
            else:
                self.assertAlmostEqual(got, c["expect"], places=9, msg=c["name"])



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


ROOT = os.path.dirname(HERE)
OFFLINE = "http://127.0.0.1:9"  # 没有服务的端口：cam doctor 的联网检测马上失败，不访问外网


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def menu_arg(path):
    """菜单栏插件把路径写进 bash= / param1= 时会把反斜杠写成两个（Windows 路径才有）。"""
    return path.replace("\\", "\\\\")


def stub(bindir, name, body):
    """在 bindir 里放一个假命令（shell 脚本），放到 PATH 最前面就能顶替真的。"""
    write(os.path.join(bindir, name), "#!/bin/sh\n" + body + "\n")
    os.chmod(os.path.join(bindir, name), 0o755)


class SumsTest(unittest.TestCase):
    """SHA256SUMS：仓库里的清单必须是最新的；cam verify 能查出改过、缺少的文件。"""

    @classmethod
    def setUpClass(cls):
        sys.path.insert(0, HERE)
        import cam
        cls.cam = cam

    @unittest.skipUnless(shutil.which("git") and os.path.exists(os.path.join(ROOT, ".git")), "不是 git 仓库")
    def test_sums_file_is_current(self):
        r = subprocess.run([sys.executable, os.path.join(HERE, "gen-sums.py"), "--check"], capture_output=True,
                           text=True, encoding="utf-8")
        self.assertEqual(r.returncode, 0, "SHA256SUMS 不是最新的：改完代码后运行 python3 scripts/gen-sums.py 重新生成\n" + r.stderr)

    def test_verify(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp, True)
        write(os.path.join(tmp, "a.txt"), "甲\n")
        write(os.path.join(tmp, "安装.cmd"), "@echo off\r\n")
        write(os.path.join(tmp, "sub", "b.py"), "print(1)\n")
        lines = [f"{self.cam.file_sha256(os.path.join(tmp, *n.split('/')))}  {n}" for n in ("a.txt", "sub/b.py", "安装.cmd")]
        write(os.path.join(tmp, "SHA256SUMS"), "\n".join(lines) + "\n")
        self.assertEqual(self.cam.verify_sums(tmp), (3, []))
        if shutil.which("sha256sum"):  # 格式和 sha256sum -c 兼容
            self.assertEqual(subprocess.run(["sha256sum", "-c", "--quiet", "SHA256SUMS"], cwd=tmp).returncode, 0)
        write(os.path.join(tmp, "sub", "b.py"), "print(2)\n")
        os.remove(os.path.join(tmp, "a.txt"))
        n, bad = self.cam.verify_sums(tmp)
        self.assertEqual(n, 3)
        self.assertEqual(bad, ["a.txt（缺少这个文件）", "sub/b.py（内容不一致）"])
        # 清单里的路径只能在项目里面
        write(os.path.join(tmp, "SHA256SUMS"), "0" * 64 + "  ../outside\n" + "0" * 64 + "  /etc/passwd\n")
        self.assertEqual(len(self.cam.verify_sums(tmp)[1]), 2)
        os.remove(os.path.join(tmp, "SHA256SUMS"))
        with self.assertRaises(self.cam.CamError):
            self.cam.verify_sums(tmp)
        # cam verify：对不上时退出码 1，列出文件
        write(os.path.join(tmp, "SHA256SUMS"), "\n".join(lines) + "\n")
        out = io.StringIO()
        with mock.patch.object(self.cam, "code_root", lambda: tmp), contextlib.redirect_stdout(out):
            self.assertEqual(self.cam.main(["verify"]), 1)
        self.assertIn("sub/b.py（内容不一致）", out.getvalue())


class UpdateTest(unittest.TestCase):
    """cam update：用临时的 git 仓库（一个裸仓库当 origin，一个开发用的克隆往里推，一个用户的克隆）测检查、快进、拒绝、退回。"""

    @classmethod
    def setUpClass(cls):
        if not shutil.which("git"):
            raise unittest.SkipTest("没有 git")
        sys.path.insert(0, HERE)
        import cam
        cls.cam = cam

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.bindir = os.path.join(self.tmp, "bin")
        self.calls = os.path.join(self.tmp, "systemctl.txt")
        stub(self.bindir, "systemctl", f'echo "$*" >> "{self.calls}"')
        env = {"HOME": os.path.join(self.tmp, "home"), "USERPROFILE": os.path.join(self.tmp, "home"),
               "CAM_HOME": os.path.join(self.tmp, "cam"), "XDG_CONFIG_HOME": os.path.join(self.tmp, "config"),
               "CLAUDE_CONFIG_DIR": os.path.join(self.tmp, "claude"), "CAM_NO_KEYCHAIN": "1", "CAM_NO_NOTIFY": "1",
               "CAM_MENU_NO_NOTIFY": "", "NO_COLOR": "1", "GIT_CONFIG_NOSYSTEM": "1",
               "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com",
               "PATH": self.bindir + os.pathsep + os.environ.get("PATH", "")}
        patcher = mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(shutil.rmtree, self.tmp, True)
        os.makedirs(env["HOME"])
        self.origin = os.path.join(self.tmp, "origin.git")
        self.dev = os.path.join(self.tmp, "dev")
        self.clone = os.path.join(self.tmp, "clone")
        self.git(self.tmp, "init", "-q", "--bare", self.origin)
        self.git(self.origin, "symbolic-ref", "HEAD", "refs/heads/main")
        self.git(self.tmp, "init", "-q", self.dev)
        self.git(self.dev, "checkout", "-q", "-b", "main")
        self.git(self.dev, "remote", "add", "origin", self.origin)
        os.makedirs(os.path.join(self.dev, "scripts"))
        for name in ("cam.py", "cam", "gen-sums.py"):
            shutil.copy(os.path.join(HERE, name), os.path.join(self.dev, "scripts", name))
        self.push("初始版本", {"README.md": "v1\n"})
        self.git(self.tmp, "clone", "-q", self.origin, self.clone)

    def git(self, cwd, *args):
        return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()

    def push(self, subject, files, sums=True):
        """在开发用的克隆里改文件、（默认）重新生成 SHA256SUMS、提交并推到 origin。"""
        for name, text in files.items():
            write(os.path.join(self.dev, *name.split("/")), text)
        if sums:
            subprocess.run([sys.executable, os.path.join(self.dev, "scripts", "gen-sums.py")], check=True, capture_output=True)
        self.git(self.dev, "add", "-A")
        self.git(self.dev, "commit", "-q", "-m", subject)
        self.git(self.dev, "push", "-q", "origin", "main")
        return self.git(self.dev, "rev-parse", "HEAD")

    def head(self):
        return self.git(self.clone, "rev-parse", "HEAD")

    def read(self, name):
        with open(os.path.join(self.clone, name), encoding="utf-8") as f:
            return f.read()

    def test_check_lists_new_commits(self):
        res = self.cam.update_check(self.clone)
        self.assertEqual((res["checkout"], res["behind"]), (True, 0))
        self.assertIn("已经是最新版本", res["message"])
        before = self.head()
        self.push("加一个功能", {"README.md": "v2\n"})
        self.push("修一个问题", {"README.md": "v3\n"})
        res = self.cam.update_check(self.clone)
        self.assertEqual(res["behind"], 2)
        self.assertEqual([c["subject"] for c in res["commits"]], ["修一个问题", "加一个功能"])
        self.assertEqual(res["upstream"], "origin/main")
        self.assertEqual(self.head(), before)  # 只看不改
        self.assertEqual(self.read("README.md"), "v1\n")
        saved = self.cam.saved_update_check(self.clone)
        self.assertEqual((saved["behind"], len(saved["commits"])), (2, 2))
        # 自己 git pull 过以后，上次的“有 2 个新改动”不再算数
        self.git(self.clone, "pull", "-q", "--ff-only")
        self.assertEqual(self.cam.saved_update_check(self.clone)["behind"], 0)
        # 另一份代码目录的检查结果不拿来用
        self.assertIsNone(self.cam.saved_update_check(self.dev))

    def test_update_fast_forwards(self):
        old = self.head()
        new = self.push("加一个功能", {"README.md": "v2\n", "scripts/extra.py": "X = 1\n"})
        # 装了后台服务（install.sh 在 Linux 上装的 cam-serve）的话，更新后重启它
        write(self.cam.serve_unit_file(), "[Service]\n")
        res = self.cam.apply_update(self.clone)
        self.assertTrue(res["updated"])
        self.assertEqual((res["old"], res["new"], self.head()), (old, new, new))
        self.assertEqual(self.read("README.md"), "v2\n")
        self.assertEqual([c["subject"] for c in res["commits"]], ["加一个功能"])
        if sys.platform.startswith("linux"):
            with open(self.calls) as f:
                self.assertEqual(f.read().splitlines(), ["--user is-active --quiet cam-serve.service",
                                                         "--user try-restart cam-serve.service"])
            self.assertIn("已重启后台的 cam serve 服务", res["restart"])
        self.assertEqual(self.cam.saved_update_check(self.clone)["behind"], 0)
        again = self.cam.apply_update(self.clone)
        self.assertFalse(again["updated"])
        self.assertIn("已经是最新版本", again["message"])
        # 命令行：--check --json 给脚本用
        out = io.StringIO()
        with mock.patch.object(self.cam, "code_root", lambda: self.clone), contextlib.redirect_stdout(out):
            self.assertEqual(self.cam.main(["update", "--check", "--json"]), 0)
        self.assertEqual(json.loads(out.getvalue())["behind"], 0)

    def test_dirty_tree_refused(self):
        old = self.head()
        self.push("加一个功能", {"README.md": "v2\n"})
        write(os.path.join(self.clone, "README.md"), "我自己改的\n")
        with self.assertRaises(self.cam.CamError) as ctx:
            self.cam.apply_update(self.clone)
        self.assertIn("README.md", str(ctx.exception))
        self.assertIn("什么都没改", str(ctx.exception))
        self.assertEqual(self.head(), old)
        self.assertEqual(self.read("README.md"), "我自己改的\n")
        # 新建的文件不算改动，也不会被动到
        self.git(self.clone, "checkout", "-q", "README.md")
        write(os.path.join(self.clone, "notes.txt"), "笔记\n")
        self.assertTrue(self.cam.apply_update(self.clone)["updated"])
        self.assertEqual(self.read("notes.txt"), "笔记\n")

    def test_failed_self_check_rolls_back(self):
        old = self.head()
        cam_py = os.path.join(self.dev, "scripts", "cam.py")
        with open(cam_py, encoding="utf-8") as f:
            good = f.read()
        cases = [
            ({"scripts/cam.py": good + "\ndef broken(:\n"}, True, "编译不过"),
            ({"scripts/cam.py": good.replace('\nif __name__ == "__main__":', '\nraise RuntimeError("坏了")\n\nif __name__ == "__main__":')},
             True, "坏了"),
            ({"README.md": "忘了重新生成校验清单\n"}, False, "README.md（内容不一致）"),
        ]
        for files, sums, reason in cases:
            self.push("有问题的版本", files, sums=sums)
            with self.assertRaises(self.cam.CamError) as ctx:
                self.cam.apply_update(self.clone)
            self.assertIn("更新后自检没过，已经退回原来的版本", str(ctx.exception))
            self.assertIn(reason, str(ctx.exception))
            self.assertEqual(self.head(), old)
            self.assertEqual(self.git(self.clone, "status", "--porcelain"), "")
            saved = self.cam.saved_update_check(self.clone)
            self.assertIn(reason, saved["failed"])
            self.assertGreater(saved["behind"], 0)
            self.push("修好了", {"scripts/cam.py": good, "README.md": "v1\n"})
        self.assertTrue(self.cam.apply_update(self.clone)["updated"])

    def test_not_a_checkout(self):
        plain = os.path.join(self.tmp, "download")
        shutil.copytree(os.path.join(self.clone, "scripts"), os.path.join(plain, "scripts"))
        res = self.cam.update_check(plain)
        self.assertFalse(res["checkout"])
        self.assertIn("安装脚本", res["message"])
        with self.assertRaises(self.cam.CamError) as ctx:
            self.cam.apply_update(plain)
        self.assertIn("不是用 git 下载的", str(ctx.exception))
        with mock.patch.object(self.cam, "code_root", lambda: plain), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.cam.main(["update", "--check"]), 1)

    def test_mini_page_and_open_panel(self):
        # 菜单栏小工具点开的卡片小面板：/mini 不用令牌（和 / 一样），接口要令牌；"打开管理面板"在本机浏览器里打开
        store = self.cam.Store()
        token = self.cam.server_token(store)
        srv = self.cam.ThreadingServer(("127.0.0.1", 0), self.cam.make_handler(store, token))
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        port = srv.server_address[1]
        with opener.open(f"http://127.0.0.1:{port}/mini") as r:
            html = r.read().decode()
        self.assertEqual(r.status, 200)
        self.assertIn("Claude 账号", html)
        self.assertIn("/api/open-panel", html)
        self.assertIn("Claude 桌面版 · 点一下打开", html)  # 桌面号单独一组，点一下先问一句再开；网页号不进卡片
        self.assertIn('"open-now"', html)
        self.assertIn("网页号、API 号在管理面板里", html)
        # /api/state 带正在跑的会话数（红绿灯用），数进程表的结果缓存 20 秒
        self.cam._SESSION_CACHE[0] = 0.0
        with mock.patch.object(self.cam, "running_claude_processes", return_value=[(1, "claude"), (2, "claude")]):
            self.assertEqual(self.cam.session_count(), 2)
        with mock.patch.object(self.cam, "running_claude_processes", return_value=[]):
            self.assertEqual(self.cam.session_count(), 2)  # 还在缓存期
            self.assertEqual(self.cam.session_count(max_age=0), 0)
        # 缓存过期了先把上次的数给出去、后台重数：/api/state 不等 ps（点开卡片小面板第一下不卡）
        def slow_ps():
            time.sleep(0.2)
            return []
        with mock.patch.object(self.cam, "running_claude_processes", side_effect=slow_ps):
            self.cam._SESSION_CACHE[:] = [time.monotonic() - 100, 5]
            self.assertEqual(self.cam.session_count(), 5)
            self.assertEqual(self.cam.session_count(), 5)  # 后台还在数：接着给旧数，不再起线程
            for _ in range(500):
                if self.cam._SESSION_CACHE[1] == 0:
                    break
                time.sleep(0.01)
            self.assertEqual(self.cam.session_count(), 0)
        with mock.patch.object(self.cam, "running_claude_processes", return_value=[]):
            req = urllib.request.Request(f"http://127.0.0.1:{port}/api/state", headers={"Authorization": "Bearer " + token})
            with opener.open(req) as r:
                st = json.load(r)
            self.assertEqual(st["sessions"], {"count": 0, "busy": False})
            self.assertEqual(st["code_stamp"], self.cam.CODE_STAMP)  # 插件据此判断后台 cam serve 是不是旧代码
            self.assertFalse(self.cam.serve_outdated(port, token))
            self.assertFalse(self.cam.serve_outdated(1, token))  # 连不上不算旧
            req = urllib.request.Request(f"http://127.0.0.1:{port}/api/menubar", headers={"Authorization": "Bearer " + token})
            with opener.open(req) as r:
                self.assertEqual(json.load(r)["icon"], "bars")
        self.assertTrue(self.cam.mini_available(port, token))
        self.assertFalse(self.cam.mini_available(1, token))  # 没人听的端口
        opened = []
        with mock.patch.object(self.cam, "open_in_browser", opened.append):
            req = urllib.request.Request(f"http://127.0.0.1:{port}/api/open-panel", method="POST", data=b"{}",
                                         headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
            with opener.open(req) as r:
                body = json.loads(r.read().decode())
        self.assertEqual(opened, [f"http://127.0.0.1:{port}/?t={token}"])  # 用的就是这个在跑的面板，不另起
        self.assertEqual(body["url"], opened[0])
        self.assertEqual((self.cam.ensure_panel(store, port)), (port, token))
        # 旧代码的 cam serve：按进程表里“cam.py serve --port 这个端口”的命令行找到它才结束它（心跳文件里的 pid 不可靠：
        # 只有拿到后台锁的 serve 才写，流式模式的菜单栏插件拿着锁时它一次都没写过）；别的端口的 cam serve、不是 cam serve、桌面版的服务都不动
        self.assertEqual(self.cam.serve_port_of("python3 /a b/scripts/cam.py serve"), 8787)
        self.assertEqual(self.cam.serve_port_of('"C:\\py\\python.exe" "C:\\cam\\scripts\\cam.py" serve --port 8790'), 8790)
        write(os.path.join(store.root, self.cam.SERVE_STATE), json.dumps({"pid": 4242}))
        killed = []
        table = [(4242, "python3 cam.py serve --port 9999"), (4243, f"python3 cam.py serve --port {port}"), (4244, "python3 other.py")]
        with mock.patch.object(self.cam, "process_table", return_value=table), \
             mock.patch.object(self.cam.os, "kill", lambda pid, sig: killed.append((pid, sig))), \
             mock.patch.object(self.cam, "port_free", return_value=True):
            self.assertEqual(self.cam.serve_pid_on(port), 4243)
            self.assertIsNone(self.cam.serve_pid_on(1))
            self.assertTrue(self.cam.restart_stale_serve(store, port, token))
        self.assertEqual(killed, [(4243, signal.SIGTERM)])  # 心跳里记的 4242 是别的端口的，不动
        with mock.patch.object(self.cam, "process_table", return_value=[(4242, "python3 other.py")]), \
             mock.patch.object(self.cam.os, "kill", lambda pid, sig: killed.append((pid, sig))):
            self.assertFalse(self.cam.restart_stale_serve(store, port, token))
        self.assertFalse(self.cam.restart_stale_serve(store, 1, token))  # 没人听的端口
        self.assertEqual(len(killed), 1)
        # 卡片要的面板（need_mini）：在跑的面板没有 /mini 页（桌面版的服务）就跳过它找下一个端口
        self.assertEqual(self.cam.ensure_panel(store, port, need_mini=True), (port, token))
        with mock.patch.object(self.cam, "mini_available", return_value=False), \
             mock.patch.object(self.cam, "port_free", return_value=False):
            with self.assertRaises(self.cam.CamError):
                self.cam.ensure_panel(store, port, need_mini=True)
        # 体检的“面板”一行：只问本机端口、不起新的；旧代码的 cam serve 说怎么重启；桌面版的服务说卡片会另开一个；不认令牌的提醒
        texts = lambda items: [(i["level"], i["text"]) for i in items]
        self.assertEqual(texts(self.cam.panel_items(store, first=port)), [("ok", f"面板：cam serve 在 {port} 端口，卡片小面板可用")])
        with mock.patch.object(self.cam, "serve_outdated", return_value=True):
            items = self.cam.panel_items(store, first=port)
        self.assertEqual(texts(items), [("warn", f"后台的 cam serve（{port} 端口）跑的还是旧代码：更新代码后它没重启")])
        self.assertIn("pkill -f 'cam.py serve'", items[0]["advice"])
        with mock.patch.object(self.cam, "mini_available", return_value=False):
            self.assertEqual(texts(self.cam.panel_items(store, first=port)),
                             [("ok", f"面板：桌面版的服务在 {port} 端口（菜单栏 / 托盘的卡片会在下一个端口另开一个 cam serve）")])
        with mock.patch.object(self.cam, "panel_running", return_value=False):
            self.assertEqual(texts(self.cam.panel_items(store, first=port)), [("warn", f"本机 {port} 端口上有个面板，但不认这台电脑的访问令牌")])
        self.assertEqual(self.cam.panel_items(store, first=1), [])  # 没人听

    def test_http_api_and_menu(self):
        store = self.cam.Store()
        token = self.cam.server_token(store)
        srv = self.cam.ThreadingServer(("127.0.0.1", 0), self.cam.make_handler(store, token))
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        self.addCleanup(srv.server_close)
        self.addCleanup(srv.shutdown)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def call(method, path, tok=token):
            req = urllib.request.Request(f"http://127.0.0.1:{srv.server_address[1]}{path}", method=method,
                                         headers={"Authorization": f"Bearer {tok}"})
            try:
                with opener.open(req) as r:
                    return r.status, json.loads(r.read().decode())
            except urllib.error.HTTPError as e:
                with e:
                    return e.code, json.loads(e.read().decode())

        sys.path.insert(0, os.path.join(ROOT, "extras", "swiftbar"))
        import cam_menu
        told = []

        def menu():
            lines = []
            cam_menu.render(store, {}, out=lines.append)
            return [l for l in lines if "检查更新" in l or "更新（" in l]

        with mock.patch.object(self.cam, "code_root", lambda: self.clone), \
                mock.patch.object(cam_menu, "notify", lambda title, body: told.append((title, body))):
            self.assertEqual(call("GET", "/api/update?saved=1", tok="wrong")[0], 401)
            self.assertEqual(call("GET", "/api/update?saved=1"), (200, {"checked_at": None, "version": self.cam.VERSION}))
            self.assertEqual(len(menu()), 1)  # 只有“检查更新”
            self.push("加一个功能", {"README.md": "v2\n"})
            st, res = call("GET", "/api/update")
            self.assertEqual((st, res["behind"], res["commits"][0]["subject"]), (200, 1, "加一个功能"))
            self.assertEqual(call("GET", "/api/update?saved=1")[1]["behind"], 1)
            self.assertIn("更新（1 个新改动）", menu()[0])
            self.assertIn('param2="update"', menu()[0])
            self.push("再加一个", {"README.md": "v3\n"})
            self.assertEqual(cam_menu.check_update_from_menu(), 0)
            self.assertEqual(told[-1][0], "有 2 个新改动")
            self.assertIn("再加一个", told[-1][1])
            self.assertEqual(cam_menu.update_from_menu(), 0)
            self.assertEqual(told[-1][0], "已更新")
            self.assertEqual(self.read("README.md"), "v3\n")
            self.assertEqual(len(menu()), 1)
            self.push("第三个", {"README.md": "v4\n"})
            write(os.path.join(self.clone, "README.md"), "改过\n")
            st, res = call("POST", "/api/update")
            self.assertEqual(st, 400)
            self.assertIn("什么都没改", res["error"])
            self.git(self.clone, "checkout", "-q", "README.md")
            write(self.cam.serve_unit_file(), "[Service]\n")
            st, res = call("POST", "/api/update")
            self.assertEqual((st, res["updated"]), (200, True))
            self.assertEqual(self.read("README.md"), "v4\n")
        if sys.platform.startswith("linux"):
            # 面板里点的更新：处理请求的可能就是那个后台服务自己，先回复页面，一秒后再让 systemd 重启
            self.assertIn("后台的 cam serve 服务马上重启，几秒后刷新页面就是新版本", res["restart"])
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and "--user --no-block try-restart cam-serve.service" not in self._calls():
                time.sleep(0.1)
            self.assertEqual(self._calls(), ["--user is-active --quiet cam-serve.service",
                                             "--user --no-block try-restart cam-serve.service"])

    def _calls(self):
        try:
            with open(self.calls) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return []


class LauncherTest(unittest.TestCase):
    """cam 命令怎么启动：平常的命令走快入口 cam_run.py（用得上编译好的字节码），serve 和没有快入口时直接运行 cam.py。"""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def run_py(self, *args, env=None):
        home = os.path.join(self.tmp, "home")
        os.makedirs(home, exist_ok=True)
        e = dict(os.environ, HOME=home, USERPROFILE=home, PYTHONIOENCODING="utf-8", NO_COLOR="1",
                 CAM_NO_KEYCHAIN="1", CAM_NO_NOTIFY="1", CAM_NO_BG_CHECK="1")
        for k in ("CAM_HOME", "CLAUDE_CONFIG_DIR"):
            e.pop(k, None)
        e.update(env or {})
        r = subprocess.run([sys.executable, *args], env=e, capture_output=True, text=True, encoding="utf-8", timeout=60)
        return r.returncode, r.stdout

    def test_fast_entry_behaves_like_cam_py(self):
        for args in (["--version"], ["list"], ["no-such-command"]):
            plain = self.run_py(os.path.join(HERE, "cam.py"), *args)
            fast = self.run_py(os.path.join(HERE, "cam_run.py"), *args)
            self.assertEqual(fast, plain, args)
        self.assertEqual(self.run_py(os.path.join(HERE, "cam_run.py"), "--version"), (0, self.cam_version() + "\n"))

    def cam_version(self):
        with open(os.path.join(HERE, "cam.py"), encoding="utf-8") as f:
            return re.search(r'^VERSION = "([^"]+)"', f.read(), re.M).group(1)

    def test_import_does_not_load_network_modules(self):
        """cam list、cam hook 这类不联网的命令不该为 ssl、http.client 这些模块花启动时间：用到才加载。"""
        code = ("import sys; sys.path.insert(0, sys.argv[1]); import cam; "
                "print(','.join(m for m in ('ssl', 'http.client', 'urllib.request', 'concurrent.futures', 'http.server') "
                "if m in sys.modules))")
        rc, out = self.run_py("-c", code, HERE)
        self.assertEqual((rc, out.strip()), (0, ""))

    @unittest.skipIf(sys.platform == "win32", "sh 启动器；Windows 用 cam.cmd")
    def test_launcher_picks_the_entry(self):
        d = os.path.join(self.tmp, "scripts")
        bindir = os.path.join(self.tmp, "bin")
        os.makedirs(d)
        os.makedirs(bindir)
        shutil.copy(os.path.join(HERE, "cam"), os.path.join(d, "cam"))
        os.chmod(os.path.join(d, "cam"), 0o755)
        for name in ("cam.py", "cam_run.py"):
            write(os.path.join(d, name), f"import sys\nprint({name!r}, *sys.argv[1:])\n")
        os.symlink(sys.executable, os.path.join(bindir, "python3"))
        env = dict(os.environ, PATH=os.pathsep.join([bindir, "/usr/bin", "/bin"]))

        def run(cmd, *args):
            r = subprocess.run([cmd, *args], env=env, capture_output=True, text=True, encoding="utf-8", timeout=30)
            return r.stdout.strip()

        cam = os.path.join(d, "cam")
        self.assertEqual(run(cam, "list"), "cam_run.py list")
        self.assertEqual(run(cam), "cam_run.py")
        self.assertEqual(run(cam, "switch", "a b"), "cam_run.py switch a b")
        # serve 直接运行 cam.py：别的地方靠命令行里的 "cam.py serve" 认出后台在跑的面板服务
        self.assertEqual(run(cam, "serve", "--port", "1"), "cam.py serve --port 1")
        # 软链到 PATH 里的目录（install.sh 的装法），相对的软链也认
        link = os.path.join(bindir, "cam")
        os.symlink(cam, link)
        self.assertEqual(run(link, "list"), "cam_run.py list")
        os.remove(link)
        os.symlink(os.path.relpath(cam, bindir), link)
        self.assertEqual(run(link, "serve"), "cam.py serve")
        # 旁边没有 cam_run.py（只更新了一半的旧安装）：照旧运行 cam.py
        os.remove(os.path.join(d, "cam_run.py"))
        self.assertEqual(run(cam, "list"), "cam.py list")

    def test_windows_launcher_mirrors_the_sh_one(self):
        with open(os.path.join(HERE, "cam.cmd"), "rb") as f:
            raw = f.read()
        self.assertNotIn(b"\n", raw.replace(b"\r\n", b""))  # 全是 CRLF：cmd 读只有 LF 的批处理会出怪错
        text = raw.decode("utf-8")
        self.assertIn('if /i "%~1"=="serve" goto plain', text)
        self.assertIn('if not exist "%~dp0cam_run.py" goto plain', text)
        self.assertEqual(text.count('"%~dp0cam_run.py" %*'), 2)  # python 和 py -3 各一处
        self.assertEqual(text.count('"%~dp0cam.py" %*'), 2)


class ShellScriptTest(unittest.TestCase):
    """shell 脚本要能在 macOS 自带的 bash 3.2 上跑。"""

    def test_no_variable_directly_followed_by_cjk(self):
        # bash 3.2 在 UTF-8 locale 下把紧跟在 $NAME 后面的中文字符的首字节也当成变量名（报 "PY�: unbound variable"），
        # 2026-10-29 install.sh 在 Mac 上就是这样挂的。变量后面紧跟中文或中文标点时写成 ${NAME}
        bad = []
        for dirpath, dirs, files in os.walk(ROOT):
            dirs[:] = [d for d in dirs if d not in (".git", "node_modules", "target")]
            for name in files:
                if not name.endswith((".sh", ".command")):
                    continue
                path = os.path.join(dirpath, name)
                with open(path, encoding="utf-8") as f:
                    for n, line in enumerate(f, 1):
                        if re.search(r"\$[A-Za-z_][A-Za-z0-9_]*[^\x00-\x7f]", line):
                            bad.append(f"{os.path.relpath(path, ROOT)}:{n}: {line.strip()}")
        self.assertEqual(bad, [], "变量名后面紧跟中文要写成 ${NAME}，不然 macOS 的 bash 3.2 报 unbound variable")


@unittest.skipIf(os.name == "nt", "install.sh 只用在 macOS / Linux")
class InstallTest(unittest.TestCase):
    """install.sh：临时 HOME，假的 systemctl / claude，不碰真实的家目录和服务。"""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.home = os.path.join(self.tmp, "home")
        self.bindir = os.path.join(self.tmp, "bin")
        self.plugins = os.path.join(self.tmp, "swiftbar")
        self.calls = os.path.join(self.tmp, "calls.txt")
        os.makedirs(self.home)
        stub(self.bindir, "systemctl", f'echo "systemctl $*" >> "{self.calls}"')
        stub(self.bindir, "claude", 'echo "2.1.0 (Claude Code)"')
        stub(self.bindir, "open", f'echo "open $*" >> "{self.calls}"')
        stub(self.bindir, "defaults", "exit 1")
        os.symlink(sys.executable, os.path.join(self.bindir, "python3"))  # PATH 里不放 Python 所在的目录，免得那里正好有 claude
        self.env = {"HOME": self.home, "SHELL": "/bin/bash", "USER": "tester", "CAM_SWIFTBAR_DIR": self.plugins,
                    "PATH": os.pathsep.join([self.bindir, "/usr/bin", "/bin"]),
                    "PYTHONIOENCODING": "utf-8", "NO_COLOR": "1", "CAM_NO_KEYCHAIN": "1", "CAM_NO_NOTIFY": "1",
                    "CAM_NO_BG_CHECK": "1", "CAM_IP_LOOKUP": "off", "CAM_RDAP_URL": "",
                    "CAM_TEST_LOCALE": "en_US", "CAM_TEST_DNS": "192.168.1.1", "CAM_TEST_PROXY": "",
                    "CAM_TEST_TZ": "America/New_York=US", "CAM_TEST_ROUTE_IF": "", "CAM_TEST_NET": "net-a"}
        for k in ("CAM_API_BASE", "CAM_CLAUDE_WEB_BASE", "CAM_STATUS_URL", "CAM_CF_META_URL", "CAM_IPINFO_URL",
                  "CAM_IPAPI_URL", "CAM_IPAPI_IS_URL", "CAM_CF_TRACE4_URL", "CAM_CF_TRACE6_URL", "CAM_CF_TRACE_URL",
                  "CAM_CLAUDE_RELEASES_URL"):
            self.env[k] = OFFLINE
        self.env["CAM_TOKEN_URLS"] = OFFLINE + "/v1/oauth/token"
        self.env["CAM_STATUS_SUMMARY_URLS"] = OFFLINE + "/summary.json"
        self.env["CAM_TEST_PROC_ROOT"] = os.path.join(self.tmp, "proc")  # cam doctor 不读本机真实的进程
        os.makedirs(self.env["CAM_TEST_PROC_ROOT"])
        self.rc = os.path.join(self.home, ".bash_profile" if sys.platform == "darwin" else ".bashrc")
        self.link = os.path.join(self.home, ".local", "bin", "cam")
        self.unit = os.path.join(self.home, ".config", "systemd", "user", "cam-serve.service")

    def install(self, *args, answers="", root=ROOT):
        r = subprocess.run(["bash", os.path.join(root, "install.sh"), *args], env=self.env, input=answers,
                           capture_output=True, text=True, encoding="utf-8", timeout=120)
        return r.returncode, r.stdout + r.stderr

    def calls_made(self):
        try:
            with open(self.calls) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return []

    def test_install_twice_then_uninstall(self):
        write(self.rc, "alias ll='ls -l'")  # 最后一行没换行
        write(os.path.join(self.home, ".claude-accounts", "accounts.json"), "{}\n")
        write(os.path.join(self.home, ".claude", "settings.json"), "{}\n")
        menu_py = os.path.join(ROOT, "extras", "swiftbar", "cam_menu.py")
        if sys.platform == "darwin":  # 菜单栏插件已经装好：不再跑 SwiftBar 的安装（会用 Homebrew）
            write(os.path.join(self.plugins, "Claude 账号.1m.sh"), f'exec python3 "{menu_py}"\n')
        for i in range(2):
            code, out = self.install("--yes", "--skip-verify")
            self.assertEqual(code, 0, out)
            self.assertEqual(os.readlink(self.link), os.path.join(ROOT, "scripts", "cam"))
            with open(self.rc) as f:
                rc = f.read()
            self.assertTrue(rc.startswith("alias ll='ls -l'\nexport PATH=\"$HOME/.local/bin:$PATH\"  # cam："), rc)
            self.assertEqual(rc.count("install.sh 添加"), 1, rc)
            self.assertIn("安装完成", out)
            self.assertIn("cam login", out)
            self.assertIn("已经装好了：" if i else "末尾加了一行", out)
        self.assertFalse(os.path.exists(self.unit))  # 后台服务默认不装
        self.assertEqual(self.calls_made(), [])
        if sys.platform.startswith("linux"):
            code, out = self.install("--skip-verify", answers="y\n")  # 问到“要装成后台服务吗”时回答 y
            self.assertEqual(code, 0, out)
            with open(self.unit) as f:
                unit = f.read()
            self.assertIn(f'"{os.path.join(ROOT, "scripts", "cam.py")}" serve\n', unit)
            self.assertNotIn("--lan", unit)
            self.assertEqual(self.calls_made(), ["systemctl --user daemon-reload", "systemctl --user enable --now cam-serve.service"])
        code, out = self.install("--uninstall")
        self.assertEqual(code, 0, out)
        self.assertFalse(os.path.lexists(self.link))
        with open(self.rc) as f:
            self.assertEqual(f.read(), "alias ll='ls -l'\n")
        self.assertFalse(os.path.exists(self.unit))
        if sys.platform.startswith("linux"):
            self.assertIn("systemctl --user disable --now cam-serve.service", self.calls_made())
        if sys.platform == "darwin":
            self.assertFalse(os.path.exists(os.path.join(self.plugins, "claude-accounts.1m.sh")))
        for kept in (".claude-accounts/accounts.json", ".claude/settings.json"):  # 账号数据和 Claude Code 配置不动
            self.assertTrue(os.path.exists(os.path.join(self.home, kept)), kept)

    def test_verify_failure_stops_install(self):
        root = os.path.join(self.tmp, "code")
        os.makedirs(os.path.join(root, "scripts"))
        shutil.copy(os.path.join(ROOT, "install.sh"), root)
        for name in ("cam.py", "cam"):
            shutil.copy(os.path.join(HERE, name), os.path.join(root, "scripts", name))
        write(os.path.join(root, "SHA256SUMS"), "0" * 64 + "  scripts/cam.py\n")
        code, out = self.install("--yes", root=root)
        self.assertEqual(code, 1, out)
        self.assertIn("scripts/cam.py（内容不一致）", out)
        self.assertIn("文件校验没通过", out)
        self.assertFalse(os.path.lexists(self.link))
        # 没有 claude 时问要不要装，默认不装
        os.remove(os.path.join(self.bindir, "claude"))
        code, out = self.install("--yes", "--skip-verify", root=root)
        self.assertEqual(code, 0, out)
        self.assertIn("现在用 Anthropic 官方安装器装 Claude Code 吗？ → 否", out)
        self.assertEqual(os.readlink(self.link), os.path.join(root, "scripts", "cam"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
