# 设计说明

脚本版（`scripts/cam.py`）和桌面版（`desktop/`）读写**同一份数据目录**、实现**同一套 HTTP API**，所以两者可以混用：在桌面版里添加的账号，脚本里能直接切换，反过来也一样。Web 面板（`web/panel.html`）只认这套 API，不关心后端是 Python 还是 Rust。

桌面版的核心逻辑在独立的 crate `cam-engine`（`desktop/src-tauri/engine/`），不依赖 Tauri，可以单独 `cargo test -p cam-engine`。身份识别、账号查找、切号规则、自动续期、编辑账号和到期日这几处容易两边写得不一样，判断规则写成数据放在 `tests/shared/`，`scripts/test_cam.py` 和 `engine/tests/shared.rs` 跑的是同一份。

## 数据目录

默认 `~/.claude-accounts/`，可用环境变量 `CAM_HOME` 覆盖。

```
~/.claude-accounts/
├── accounts.json          # 账号元数据（不含任何密钥）
├── secrets/<id>.json      # 每个账号的凭据，权限 600
├── browser-profiles/<id>/ # 网页版账号各自独立的浏览器 Profile（Cookie 永久保留）
├── backups/<时间戳>/      # 每次切换前的现场备份，保留最近 30 份
├── env-backups/<时间戳>/  # cam env clean --purge 移走的整个 Claude Code 配置
├── accounts.lock          # 数据锁，见“并发与锁”
├── background.lock        # 后台任务锁，见“并发与锁”
└── server-token           # Web 面板访问令牌
```

### accounts.json

```jsonc
{
  "version": 1,
  "active": { "claude_code": "a1b2c3" },      // 当前 Claude Code 正在用的账号
  "accounts": [
    {
      "id": "a1b2c3",
      "kind": "claude_code",                  // claude_code | claude_web | api
      "label": "工作号",
      "email": "me@example.com",
      "plan": "max",                          // 自动获取，失败时用 manual.plan
      "org_name": "…",
      "account_uuid": "…",
      "token_fp": "…",                        // refresh token 的 SHA-256 前 16 位，用来认出当前登录是哪个号
      "manual": {                             // 手动补齐的字段，优先级低于自动值（expires_at 例外，见下）
        "plan": null, "expires_at": "2026-11-03", "auto_renew": true,
        "cycle": "monthly",                   // monthly | yearly，自动续期按月还是按年顺延
        "renew_day": 31,                      // 原始扣费日，由自动续期记下；改 expires_at 时清掉
        "price": "$100/月", "note": ""
      },
      "usage": {                              // 最近一次拉取结果
        "fetched_at": "…", "source": "oauth",
        "five_hour": { "utilization": 37.0, "resets_at": "…" },
        "seven_day": { "utilization": 12.0, "resets_at": "…" },
        "seven_day_opus": null, "seven_day_sonnet": null,
        "cost_month_usd": null,               // API 账号（需要 Admin Key）
        "error": null
      },
      "created_at": "…", "last_switched_at": "…"
    }
  ],
  "switch_log": [ { "id": "a1b2c3", "at": "…" } ], // 用来把本地 token 消耗归属到账号
  "live_mismatch": {                              // 只在凭据和 ~/.claude.json 对不上时存在，见“认出当前登录”
    "at": "…", "account": "a1b2c3", "claude_json_email": "other@example.com", "message": "…"
  }
}
```

## 切号规则

`accounts.json` 里另有三个字段：

```jsonc
"rules": [
  { "id": "r1", "type": "time", "days": [1,2,3,4,5], "start": "09:00", "end": "18:00", "account": "a1b2c3" },
  { "id": "r2", "type": "dir",  "path": "~/work", "account": "a1b2c3" }
],
"rule_state": { "time_rule": "r1" },   // 上一次检查时命中的时间规则
"prefs": { "alert_threshold": 90, "risk_prompt": true, "risk_confirm_below": 60, "risk_strong_below": 30 }
```

- 时间规则：星期 1 = 周一；`end` 早于 `start` 表示跨午夜，星期按开始那天算。多条重叠时取第一条。只在命中的规则发生变化（进入或离开时间段）时切换，所以时间段内手动换号不会被改回。进入时有 `claude` 会话在跑就登记成 `pending_switch`（见“切换流程”），会话退出后再切；时间段过了还没切成就作废。
- 目录规则：`cam claude` 启动时按当前目录做最长前缀匹配，切换后再执行 `claude`。
- 额度提醒：当前账号 5 小时额度 ≥ 阈值时提醒一次（同一个额度窗口只提醒一次），附上多久后重置。**不会自动切换，也不推荐换哪个号。**
- 启动前风险提醒（只在 `cam claude`）：缓存超过一小时时，`spawn_background_check` 另起 `cam.py _check-region` 后台进程做完整检测（同一时间只跑一个），前台只用 `quick_trace` 花最多 3 秒问一次 claude.ai，套到数据副本上算提醒（`with_quick_region`，出口 IP 变了时丢掉旧运营商信息）。`launch_tier(score, region_status, prefs)` 给出 ok / remind / confirm / strong 四档——≥90 一行带过；< 90 打印提醒；< `risk_confirm_below`（默认 60）问 y/N；< `risk_strong_below`（默认 30）或出口不在支持名单内要输入"继续"。非交互环境只打印。`risk_prompt: false` 或 `CAM_NO_RISK_PROMPT=1` 关掉。**不是拦截**：每一档都是用户自己选择继续或取消。
- 自动发现：后台检测凭据文件变化（macOS 每 30 秒查一次钥匙串），发现没见过的账号就自动导入。

## 三类账号

| 类型 | 切换方式 | 用量来源 | 到期来源 |
|---|---|---|---|
| `claude_code` | 替换 Claude Code 的登录凭据，聊天记录/项目/设置共用不动 | `api.anthropic.com/api/oauth/usage`（5 小时、7 天额度）+ 本地 JSONL 日志统计 token | 手动填写（官方无接口），可设"每月自动续期" |
| `claude_web` | 每个账号一个独立浏览器 Profile，"切换"就是打开对应窗口 | 可选填 `sessionKey`，走 claude.ai 内部接口；失败则只显示手动信息 | 手动填写 |
| `api` | 不需要切换，只做展示（可复制 Key） | Admin Key → Cost/Usage Report；普通 Key → 仅校验有效 | 无 |

## 切换流程（保证数据不丢）

整个流程在数据锁（`accounts.lock`）里完成。

1. **先快照**：读取当前正在生效的凭据（Linux/Windows 读 `~/.claude/.credentials.json`；macOS 读钥匙串 `Claude Code-credentials`，没有再读文件）以及 `~/.claude.json` 里的 `oauthAccount`。
2. **回写**：认出它属于哪个已保存账号（见下一节），把最新 token 写回该账号（Claude Code 运行时会刷新 token，不回写的话旧 token 会失效）。如果是没见过的账号，自动导入为新账号，绝不丢弃。认不出是谁的（见下一节的“暂存”）就不写，切换到候选账号前会先核对一次。
3. **备份**：把凭据文件和 `~/.claude.json` 复制到 `backups/<时间戳>/`。
4. **写入**：原子写入目标账号的凭据，`~/.claude.json` 只替换 `oauthAccount` 字段，其它字段原样保留（合并方式见“并发与锁”）。
5. **记录**：更新 `active` 和 `switch_log`，清掉 `live_mismatch`、`live_unverified` 和 `pending_switch`。

不动的东西：`~/.claude/projects/`（会话记录）、`settings.json`、`CLAUDE.md`、MCP 配置、插件。所以切换后 `claude --continue` / `--resume` 照常可用。

正在运行的 `claude` 进程内存里还是旧 token，切换后需要重启这些会话；不重启的话，它刷新 token 后还会把旧账号写回凭据文件。所以切换后会扫一遍进程（Linux 读 `/proc`，macOS 用 `ps`，Windows 用 `Win32_Process`），认得出原生安装版和 npm 版的 Claude Code，不算 Claude 桌面 App。有的话在返回消息、系统通知、终端输出里提示进程数和 pid；`cam switch` 在交互终端里会在切换前先问一句。

**等会话结束再切**：切换前发现有会话在跑时，可以不打断它——把目标记在 `accounts.json` 的 `pending_switch`（`{"account", "reason": "manual"|"rule", "rule", "at"}`）里，由持有后台租约的那个进程（桌面版、`cam serve`、菜单栏插件）每轮检查：没有 `claude` 进程了就执行切换并清掉登记。目标账号被删、已经是当前账号、或时间规则的时间段已经过了，登记作废。手动切换会覆盖登记。`cam switch --wait` 自己也会轮询等待。面板 / 桌面版的切换接口在没指定 `mode` 且有会话在跑时返回 `choice_required`，由前端弹三选一。

## 认出当前登录

凭据和 `~/.claude.json` 里的 `oauthAccount` 不一定是同一个人的。最常见的情况是：切换前打开的 `claude` 会话刷新 token 后，把旧账号的凭据写了回去，而 `oauthAccount` 已经是新账号。所以以凭据为准：

1. **比 refresh token 指纹**（`token_fp`）。token 本身就是凭据，对上了就是这个号。
2. token 换过（刷新过或重新登录）时才看 `oauthAccount` 的 `accountUuid` / 邮箱。这时先用 access token 请求一次 `/api/oauth/profile`，问出这份凭据真正的主人，和 `oauthAccount` 说的不一致就以 profile 为准。
3. 都对不上就导入为新账号。

如果最后认定的主人和 `oauthAccount` 不一致：token 存进真正主人的名下，不用这份 `oauthAccount` 改任何账号的资料，在 `accounts.json` 里写一条 `live_mismatch`。`cam`、`cam doctor`、菜单栏插件和 `/api/state` 的 `warning` 会显示这条说明。凭据和 `oauthAccount` 重新一致后（比如下次切换成功）自动清掉。

**暂存认不出主人的登录**：认人靠两样——refresh token 指纹和某个账号保存的一致（`by_token`），或联网用 `/api/oauth/profile` 问出主人。两样都做不到（离线，或 access token 已过期而 refresh token 又换过）时，不能信 `~/.claude.json` 里的 `oauthAccount`：它可能是旧会话写回时留下的别人的名字，信了就会把 A 的 token 存进 B。这时原样暂存到 `secrets/_pending-live.json`（含候选账号和首次看到的时间），`accounts.json` 里记 `live_unverified` 给界面提示，`active` 只按候选显示。之后每次快照先处理暂存：已有账号存着同一个 token 或放了 7 天就丢掉；access token 还有效就联网核对，核对出主人且比它保存的新（`saved_at` 记的是这个 refresh token 拿到的时间，token 没换不更新）就归档。切换到候选账号前再核对一次，access token 过期就用暂存的 refresh token 换新再问——这时没有会话在用它。归到谁名下由核对结果决定，所以即使其实是别的账号的，也不会写进候选账号。服务端明确说 token 无效（而不是连不上）时，仍按 `oauthAccount` 处理，和以前一样。

## Token 刷新策略

- **当前生效账号**：只读不刷新，由 Claude Code 自己刷新，避免两边抢 refresh token。
- **未生效账号**：access token 过期时用 refresh token 换新，新 token（含轮换后的 refresh token）立即写回 `secrets/<id>.json`，并更新 `token_fp`。
  - 只有用户主动刷新（`cam usage`、面板 / 桌面版的“刷新”、`POST /api/refresh`）才会换新。后台定时刷新（`cam serve`、桌面版、菜单栏插件）不换，过期的号显示“登录已过期”，这样少一些和 Claude Code 抢 refresh token 的机会。
  - 换新在数据锁里进行，拿到锁后再确认一次这个号仍然不是当前账号、也没被别的进程刷新过，所以和切换互斥：不会出现刚切过去、Claude Code 和这里拿着同一个 refresh token 各刷一次，把号弄成需要重新登录的情况。锁里的请求超时 8 秒。
  - token 接口返回 400/401 说明 refresh token 已失效，提示用这个号重新登录 Claude Code 后 `cam import`；其它错误提示稍后再试。
- **拉用量前先核对**：先请求 `/api/oauth/profile`，返回的 uuid / 邮箱和账号记录的不一致（凭据被旧会话覆盖了），就不拿这份凭据查用量，直接报错。

## 并发与锁

桌面版、`cam` 命令、`cam serve`、菜单栏插件可能同时在跑，Claude Code 自己也会写 `~/.claude.json`。

- **数据锁 `accounts.lock`**：读改写 `accounts.json` 和 `secrets/` 时持有。Unix 用 `flock`，Windows 用 `LockFileEx`，Python 和 Rust 用的是同一种系统锁，所以两边互斥；每次加锁单独打开文件，同一进程的不同线程之间也互斥。锁由操作系统持有，进程崩溃时自动释放，不会有残留锁。等锁最多 30 秒。旧版本用的是独占创建 `.lock` 文件、超过 60 秒当残留删掉，换了文件名就不会和旧版互相误删。
- **后台任务锁 `background.lock`**：自动导入、时间规则、定时刷新、额度提醒只让一个进程做，不等待，谁先拿到谁做。桌面版和 `cam serve` 拿到后一直持有到退出，没拿到的每隔一会儿再试一次（接手退出的那个），期间只同步显示数据；菜单栏插件每次运行时试一下，拿不到就只画菜单。
- **`~/.claude.json`**：读出来改完，写之前再读一次，内容变了（Claude Code 刚写过）就基于新内容重新改；写完再读回确认 `oauthAccount` 是想要的值。最多重试 5 次，还不行就提示先退出正在运行的 claude。
- **符号链接**：`~/.claude.json` 等文件是符号链接（比如用 dotfiles 管理）时，写到链接指向的文件，保留链接本身。

## 服务检查与环境风险提示（只提示，不拦）

`cam doctor` / `GET /api/service-check` 实测服务，并把使用环境里的风险点逐项列出，每项带 `kind`、`level`、`text`、`advice`。所有判断只用来提示，**不拦任何操作**，是否继续由用户决定。

- 出口地区：`claude.ai/cdn-cgi/trace`（连不上退到 `www.cloudflare.com`）的 `loc`，对照 `SUPPORTED_REGIONS`（Anthropic 官网公布的支持国家/地区）+ `REGION_DEPENDENCIES`（属地按所属国家看待）。同一个 trace 还给出 `warp`（经过 Cloudflare WARP / Zero Trust）和 `ts`（算本机时钟偏差）。再查出口 IP 的 `asn`、运营商（`org`）、城市、国家（`meta_loc`，和 trace 的 `loc` 交叉核对）和类型（`ip_type`：residential / hosting / proxy / tor / mobile / null）：来源由 `prefs.ip_lookup`（auto / ip-api / off）决定，`meta_sources` 给出顺序：ip-api 模式先问 ip-api.com（免费接口仅限非商业），然后 `api.ipapi.is`（有 key 带标记）→ `ipinfo.io` → `speed.cloudflare.com/meta`，第一个查到的为准并记下 `meta_source`（界面注明来源）；`parse_exit_meta` 统一几种格式（含 ipapi.is 不带 key 的简版）；没配 ipapi.is key 时并行查 `rdap.org` 拿注册国 `reg_loc`；`1.1.1.1` 和 `[2606:4700:4700::1111]` 的 trace 分别拿 `v4_loc` / `v6_loc`。这些查询并行进行；出口 IP 和设置都没变、且 `META_TTL`（默认一天）内查过时，运营商那几项（`META_FIELDS`）直接沿用缓存，不再问第三方。off 模式只问 `claude.ai/cdn-cgi/trace` 和状态页。key 存在 `secrets/_ip-lookup.json`，不进 accounts.json；`save_privacy` 改设置或 key 时清掉缓存的运营商信息，`clear_region_history` 清掉 `region`、`status_page` 和各账号的 `regions`。`local_env()` 把时区、系统语言、本机 DNS 配置一起存进 `region.env`，界面刷新时不用再跑系统命令。
- 后台缓存：持有后台租约的进程每隔几分钟调一次 `check_region`，把结果写进 `accounts.json` 的 `region`（`loc`、`ip`、`colo`、`ms`、`warp`、`skew`、`checked_at`，出口换了国家时另记 `prev_loc`、`changed_at`）和 `status_page`（`status.anthropic.com/api/v2/status.json` 的 `indicator`、`description`；连不上时删掉这个字段），并给当前账号的 `regions`（`[{"loc", "at"}]`，最多 8 个）记一笔。切换账号时也用一小时内的缓存给目标账号记一笔。
- `risk_items(data)` 不联网，只看缓存和本机设置：出口不在名单（warn）、24 小时内出口变化（warn）、两个接口看到的国家不一致（warn）、IP 类型为 Tor（warn）/ 代理（warn）/ 机房（数据库标记，或没有标记时按 `HOSTING_WORDS`，warn）、注册国与定位国不同（info）、IPv4 / IPv6 出口地区不同（warn）、本机 DNS 是某个国家的公共 DNS 且与出口不同（不支持地区 → warn，否则 info；只读配置，`KNOWN_DNS`）、WARP（info）、时钟偏差超过 120 秒（warn）、官方状态页有故障（minor → warn，major / critical → bad）、系统时区指向的国家和出口不一致（指向不支持地区 → warn，否则 info；时区来自 `TZ` / `/etc/localtime` / `tzutil`，国家来自 tzdata 的 `zone1970.tab`）、系统语言 / 区域指向的国家和出口不一致（一律 info、扣 5 分：语言只代表语言习惯；来自 `AppleLocale` / `Get-Culture` / `LANG`）、当前账号 7 天内多地区使用（warn）、本机有 2 个以上 Claude Code 账号（info）。`/api/state` 的 `risks` 就是它，三个界面把 warn / bad 常显在顶部，`cam`、`cam switch` 和切换接口也会带上。
- 评分 `risk_score(items)`：满分 100，按 `RISK_WEIGHTS` 的 (kind, level) 扣分（出口不在名单 60、Tor 出口 40、代理 IP 25、时区指向不支持地区 25、机房出口 20、账号多地区 20、出口变化 15、两接口不一致 15、IPv4 / IPv6 不同 15、DNS 在不支持地区 15、系统语言不一致 5、时钟 10、VPN 10、官方故障 30 / 10、api 连不上 40……没列出的 warn 扣 10、bad 扣 30），档位 ≥90 low / 70–89 medium / <70 high。`/api/state` 的 `risk_score` 据缓存的 `risks` 算；服务检查的第一项 `summary` 带实测的 `score`、`grade`，`level` 取 bad / warn / ok 中更严重的（档位 high → bad，medium → warn）。只是把提醒汇总成一个数，不拦任何操作。
- 服务检查另外实测：DNS、`api.anthropic.com`（有登录就用当前登录）、`claude.ai`（不带登录，识别人机验证页）、代理（环境变量 + `scutil --proxy` / 注册表 `Internet Settings` / GNOME `gsettings`）、VPN（默认路由的接口名或适配器描述像隧道：`route -n get default` / `ip route show default` / `Get-NetRoute`）。`device` 列出系统和架构（设备环境信息只列出，不改动、不伪装）。最后一条 `scope` 说明检测范围：只有本机能看到的信号，账号侧的信号看不到，所以“没有发现风险点”不等于一定合规。
- 测试钩子：`CAM_STATUS_URL`、`CAM_CF_META_URL`、`CAM_IPINFO_URL`、`CAM_IPAPI_URL`、`CAM_IPAPI_IS_URL` / `CAM_IPAPI_IS_KEY`、`CAM_RDAP_URL`、`CAM_CF_TRACE4_URL` / `CAM_CF_TRACE6_URL`、`CAM_IPINFO_TOKEN`、`CAM_IP_LOOKUP`、`CAM_META_TTL`、`CAM_NO_BG_CHECK`、`CAM_TEST_DNS`、`CAM_CLAUDE_WEB_BASE`、`CAM_TEST_TZ="Asia/Hong_Kong=HK"`、`CAM_TEST_LOCALE="zh_CN"`、`CAM_TEST_ROUTE_IF="utun4"`。

## HTTP API

所有请求需要 `Authorization: Bearer <server-token>`（或首次访问 `?t=<token>`）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | Web 面板 |
| GET | `/api/state` | 账号列表（不含密钥）、当前账号、服务器信息；每个账号带 `health`（`level`: ok / info / warn / error，`label`: 正常 / 注意 / 异常，`issues`: [{`kind`, `level`, `text`, `advice`, `detail`}]，由 `account_health` 根据有没有保存凭据、用量报错和到期日算出，两边规则见 `tests/shared/health.json`）；`warning` 是 `live_mismatch` / `live_unverified` 的说明，`pending_switch` 是登记中的等待切换（含 `message`），没有时为 `null`；`region` 是后台上次检测的出口地区（`loc`、`status`: supported / unsupported / unknown / null、`message`、`advice`、`warp`、`prev_loc`、`changed_at`、`org`、`city`），`risks` 是据缓存算出的风险提示列表（同 `/api/service-check` 的条目格式，含 `kind`），`risk_score` 是据它算的评分（`score`、`grade`: low / medium / high、`label`、`advice`），前端把 warn / bad 的常显在顶部、顶栏显示分数；都只用于提示 |
| POST | `/api/accounts` | `{"kind":"claude_code"}` 导入当前登录；`{"kind":"claude_web"/"api","label",…}` 新增 |
| POST | `/api/refresh` | 刷新全部用量；body `{"id": "…"}` 只刷新一个。算用户主动刷新，会替过期的未生效账号续登录 |
| POST | `/api/accounts/{id}/switch` | 切换 Claude Code 账号。body 可带 `{"mode": "now"|"wait"}`：有会话在跑且没给 `mode` 时返回 `{"ok": false, "choice_required": true, "running_claude", "message"}` 让前端问用户；`wait` 登记等待并返回 `deferred: true`；`now` 或没有会话时直接切，返回 `changed`、`account`、`running_claude`、`message`、`backup`。每种返回都带 `risks`（warn / bad 的风险提示），只提醒不拦 |
| DELETE | `/api/pending-switch` | 取消登记中的等待切换，返回新的 state |
| GET | `/api/privacy` | 外部 IP 查询设置：`ip_lookup`（auto / ip-api / off）、`modes`、`env_override`、`has_ipapi_key`、`has_ipinfo_token`（不返回 key 本身）、`meta_ttl_hours`、`sources`（当前设置下会访问的服务和各发了什么）、`history` |
| PUT | `/api/privacy` | body 可带 `ip_lookup`、`ipapi_key`、`ipinfo_token`（空字符串删除），返回同 GET |
| DELETE | `/api/privacy/history` | 清除缓存的出口信息和各账号的使用地区记录，返回同 GET |
| GET | `/api/service-check` | `{"items": [{"kind", "level": "ok"|"bad"|"warn"|"info", "text", "advice"}]}`：第一项是总体结论（`summary`，带 `score`、`grade`），然后是 `dns`、`region`（对照 Anthropic 官方支持名单）、`api`、`web`、`proxy`（环境变量 + 系统代理）、`vpn`（默认路由是否走隧道），再加上据缓存算出的风险项 `region_change`、`warp`、`clock`、`status`（官方状态页）、`timezone`、`account_regions`、`accounts`，每项带建议；同 `cam doctor` 的服务检查，要几秒钟，并会顺便刷新 `region` / `status_page` 缓存。只检测和提示，不干预任何操作 |
| POST | `/api/accounts/{id}/open` | 在电脑上打开该网页账号的浏览器窗口 |
| PATCH | `/api/accounts/{id}` | 修改 `label` 和 `manual.*` |
| DELETE | `/api/accounts/{id}` | 删除账号（凭据移入 backups，不直接销毁） |
| GET | `/api/local-usage?days=30` | 按天、按账号汇总本地 token 消耗 |
| GET | `/api/rules` | `{"rules": [...], "alert_threshold": 90}` |
| PUT | `/api/rules` | 整体替换规则 / 设置提醒阈值，服务端校验 |

返回统一为 JSON；错误为 `{"error": "…"}` 加相应状态码。
