# 设计说明

脚本版（`scripts/cam.py`）和桌面版（`desktop/`）读写**同一份数据目录**、实现**同一套 HTTP API**，所以两者可以混用：在桌面版里添加的账号，脚本里能直接切换，反过来也一样。Web 面板（`web/panel.html`）只认这套 API，不关心后端是 Python 还是 Rust。

桌面版的核心逻辑在独立的 crate `cam-engine`（`desktop/src-tauri/engine/`），不依赖 Tauri，可以单独 `cargo test -p cam-engine`。身份识别、账号查找、切号规则、自动续期、编辑账号和到期日、环境检测没连上时怎么算、官方状态页、服务检测、桌面版状态、提醒、每天最高用量、Windows 查询的解析这几处容易两边写得不一样，判断规则写成数据放在 `tests/shared/`，`scripts/test_cam.py` 和 `engine/tests/shared.rs` 跑的是同一份。

`cam` 启动器（`scripts/cam`、`scripts/cam.cmd`）除了 `serve` 都从 `scripts/cam_run.py` 进：它把 `cam` 当模块运行（`runpy.run_module`），用得上 `__pycache__` 里编译好的字节码，不用每条命令都把 `cam.py` 重新编译一遍；`cam.py` 里只有少数命令用得上的标准库模块（`ssl`、`http.client`、`urllib.request`、`http.server`、`concurrent.futures`）到用的时候才导入。旁边没有 `cam_run.py` 时启动器直接运行 `cam.py`。`cam serve` 一直直接运行 `cam.py`：别的地方靠命令行里的 `cam.py serve` 认出后台在跑的面板服务。

## 数据目录

默认 `~/.claude-accounts/`，可用环境变量 `CAM_HOME` 覆盖。

账号资料、密码与邮箱授权集中存放在额外的 `vault.sqlite3`，不迁移或覆盖下面的 `accounts.json`。SQLite 元数据包含名称、邮箱、连接配置和可选的 Claude `account_id` 绑定；密码、Client Secret 和 OAuth token 单独加密。具体数据库格式、密钥派生、AES-GCM AAD、接口和邮箱协议统一见 [VAULT_API.md](VAULT_API.md)。

```
~/.claude-accounts/
├── accounts.json          # 账号元数据（不含任何密钥）
├── secrets/<id>.json      # 每个账号的凭据，权限 600
├── browser-profiles/<id>/ # 网页版账号各自独立的浏览器 Profile（Cookie 随会话有效期保留）
├── desktop-profiles/<id>/ # Claude 桌面版账号各自的用户数据目录（登录 Cookie 和 oauth 缓存）
├── backups/<时间戳>/      # 每次切换前的现场备份，保留最近 30 份
├── env-backups/<时间戳>-purge/  # cam env clean --purge（面板的“彻底清理”）移走的整个配置（~/.claude、~/.claude.json、.claude.json.backup*、缓存目录的 claude-cli-nodejs）+ purge.json（moved：从哪移来的；fix_backup / env：接着清掉别处中转设置的那次一键清理和结果；恢复过没有），--restore 放回去并撤销那次一键清理
├── env-backups/<时间戳>-fix/  # cam env fix 改之前的文件原样（file-N）和 manifest.json（每个文件改前 / 改后的指纹和改了哪几处、环境变量原值，系统环境变量带 system、要管理员权限的文件带 admin；每步写一次，只有自己能读；留最近 10 次），--undo 据它改回去：文件没再动过整份放回，动过只改回那几处。要管理员权限的几项攒到最后：把要做的写进 admin-job.json（系统环境变量删 / 设、把这个目录里准备好的内容换到托管设置或共用的 PowerShell 配置上，并核对文件指纹），已经是管理员就直接做，不是就弹一次 UAC 起 `cam env admin-job` 做完写 admin-result.json，用完两个都删
├── accounts.lock          # 数据锁，见“并发与锁”
├── background.lock        # 后台任务锁，见“并发与锁”
├── attention.json         # 哪些 Claude Code 会话弹着权限确认在等你（钩子写的，没有会话在等时没有这个文件），见“并发与锁”
├── attention.lock         # 写 attention.json、session-state.json 时的锁
├── session-state.json     # 每个 Claude Code 会话现在在干什么（钩子写的，会话页和红绿灯“几个在回答”用），见“会话页”
├── update-check.json      # 上次 cam update --check 的结果（菜单栏、面板据它显示“更新（N 个新改动）”）
├── usage-samples.json     # 每次刷新用量记的采样点，画 7 天额度的节奏线（见“7 天额度的节奏”），只在刷新用量时写
├── scan-cache/<配置目录的哈希>/<记录路径的哈希>.json  # 脚本版：本机 Claude Code 记录读到哪、读出了哪些带用量的消息（只有时间、模型、用量数字和 id，没有聊天内容），下一个进程接着读；见“本机 Claude Code 消耗”
├── update.lock            # 同一时间只让一个 cam update 在跑
└── server-token           # Web 面板访问令牌
```

### accounts.json

```jsonc
{
  "version": 1,
  "active": { "claude_code": "a1b2c3", "claude_desktop": "d4e5f6" }, // Desktop 是工具最后成功启动的配置
  "accounts": [
    {
      "id": "a1b2c3",
      "kind": "claude_code",                  // claude_code | claude_desktop | claude_web | api
      "label": "工作号",
      "email": "me@example.com",
      "plan": "max",                          // 自动获取，失败时用 manual.plan
      "rate_limit_tier": "default_claude_max_20x", // 自动获取，界面据此显示 Max 5x / 20x
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
        "fetched_at": "…", "source": "oauth", // fetched_at：这些数字是什么时候取到的；没取到（error 不为空）时数字和它都是上一次的
        "checked_at": "…",                    // 最近一次去查的时间，没取到也记
        "five_hour": { "utilization": 37.0, "resets_at": "…" },
        "seven_day": { "utilization": 12.0, "resets_at": "…" },
        "seven_day_opus": null, "seven_day_sonnet": null,   // 新接口里同名的按模型额度已在 models 里时置空
        "models": [ { "name": "Fable", "utilization": 19.0, "resets_at": "…" } ], // 按模型单独计的周额度（limits 里的 weekly_scoped）
        "breakdown": [ { "key": "claude_code", "name": "Claude Code", "percent": 80.0 } ], // 7 天额度花在了哪里，只留大于 0 的
        "local": {                            // 拉用量时顺便统计：本机记录里当前各窗口用了多少（只有 Claude Code 号）
          "at": "…", "windows": { "five_hour": { "tokens": 0, "cost_usd": 0.0, "messages": 0 }, "seven_day": {…}, "Fable": {…} }
        },                                    // tokens 不含缓存读取；cost_usd 按 API 价折算（message_cost），界面用它推算整窗
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

## 账号资料与邮箱界面

React 通过独立的“账号资料与邮箱”页进入；Web 面板通过同名按钮展开，即使没有任何 Claude 账号也能初始化账号库。界面先读取库状态，未创建时设置主密码，锁定时要求解锁。主密码和输入的秘密只留在当前页面内存，不写入 localStorage；普通条目响应只有 `has_password`、`has_client_secret`、`connected` 等标记，`connected` 只表示有保存授权，不宣称实时连通。

保存表单后才能连接邮箱。Gmail / Workspace、Outlook / Hotmail / Microsoft 365 使用用户自己的桌面 OAuth 应用，连接接口返回授权 URL 和任务 ID；用户触发后通过 `/api/open-external` 在电脑系统浏览器打开，另显示可自行打开的官方授权链接，不使用内嵌 WebView 登录。界面每 2 秒读取任务状态，最多 5 分钟。Google 需要启用 Gmail API、配置桌面 OAuth 客户端和必要测试用户；Microsoft 使用公用客户端、本机 localhost 回调和对应个人 / 组织账户类型。企业批准要求交由正常的提供商授权流程处理。

“读取已登录邮箱”调用账号的 `identify` 接口，保留返回的 `identity_message` 提示，不能猜邮箱。Desktop 的资料来源限定在对应用户数据目录和 URL 经过验证的本地 `/api/account` 缓存；Code / Web 可使用已有的只读身份查询。绑定库条目只是关联，不把不同产品的认证互相转换。

“等待新验证码”携带 `since = 当前时间 - 30 秒`，每 5 秒读取一次，最多 2 分钟，`found` 后停止；“读取最近登录邮件”使用接口默认的近期范围。可选的普通自动读取每 30 秒一次，仅在解锁且账号库界面打开期间运行。每个等待绑定固定 `entry_id`，切换条目、保存、锁库、收起或组件卸载时取消等待、OAuth 轮询，并清除旧验证码与显式密码。异步响应更新页面前校验选择代次、条目 ID 和解锁状态，避免旧邮箱响应落到另一条目；接口报错后复查锁定状态，外部已锁定时清理敏感显示。

密码只在显式 reveal 后显示，30 秒自动隐藏，复制需另一次用户点击。验证邮件只展示经过服务端发件域、时间和链接校验的摘要、验证码与链接；界面再限制 HTTPS 官方域。链接不会被自动请求或预览，只有用户点击才按绑定的 Claude 账号打开。自动读信不等于自动执行 Claude 登录、回跳或多因素验证。

验证链接通过 `POST /api/vault/entries/<id>/open-verification` 打开，独立于邮箱 OAuth 的 `/api/open-external`。后端按条目绑定的 Claude 账号选择目标：网页版使用独立浏览器 profile；Desktop 必须已运行正确的配置，用户应先在该配置中发起登录，验证时不退出或重启客户端，以保留待完成的认证流程。前端每次点击均传当前条目 ID，不直接让默认浏览器决定验证目标。

## 切号规则

`accounts.json` 里另有三个字段：

```jsonc
"rules": [
  { "id": "r1", "type": "time", "days": [1,2,3,4,5], "start": "09:00", "end": "18:00", "account": "a1b2c3" },
  { "id": "r2", "type": "dir",  "path": "~/work", "account": "a1b2c3" }
],
"rule_state": { "time_rule": "r1" },   // 上一次检查时命中的时间规则
"prefs": { "alert_threshold": 90, "risk_prompt": true, "risk_confirm_below": 60, "risk_strong_below": 30, "menu_icon": "bars", "menu_text": ["name", "five"], "confirm_switch": false }
```

- 菜单栏样式：`menu_icon` 是 bars（信号格）、lamp（红绿灯）、ring（圆环：5 小时）、rings（双环：外圈 5 小时、内圈 7 天）、twobar（两根小条：上 5 小时、下 7 天），别的值当 bars。`menu_text` 是图标旁边写什么，从 `cam.MENU_TEXT` = name（名字）、five（5 小时 %）、seven（7 天 %）、sessions（在回答的会话数）里选，按这个顺序存，空列表是只要图标；没有这一项时是默认的 `["name", "five"]`（`menu_text_items`）。菜单栏插件的 `title_text` 按它拼标题（两个百分比都要时“17% · 34%”，会话数“● 2”，只读 `session_count` 刚数过的 `session_counts`，不另查进程），`status_icon(dark, level, kind, phase, style, p5, p7)` 画图：颜色是状态，后三种的长短是额度（取整，按参数缓存），动画和红绿灯同一个节奏（工作中彩色那段每秒一亮一暗，等你确认整个图标快一倍地闪；经典模式的两帧 phase 0 / 0.375 一亮一暗）。

- 时间规则：星期 1 = 周一；`end` 早于 `start` 表示跨午夜，星期按开始那天算。多条重叠时取第一条。只在命中的规则发生变化（进入或离开时间段）时切换，所以时间段内手动换号不会被改回。进入时有 `claude` 会话在跑就登记成 `pending_switch`（见“切换流程”），会话退出后再切；时间段过了还没切成就作废。
- 目录规则：`cam claude` 启动时按当前目录做最长前缀匹配，切换后再执行 `claude`。
- 提醒（`due_alerts(data, st)`，Rust：`alerts::due_alerts`，规则两边按 `tests/shared/alerts.json` 跑）：当前 Claude Code 号和有额度的网页号，5 小时、7 天额度 ≥ 阈值各提醒一次（`st.warned` 记 `id|resets_at` / `id|7d|resets_at`，同一个额度窗口只提醒一次），附上多久后重置；有额度的号（Claude Code、网页、桌面版）某个窗口 ≥ 99.5% 记进 `st.exhausted`，过了它的 `resets_at` 提醒一次“恢复了”；手动填了到期日、没勾自动续期的号，还剩 3 天以内提醒一次、过了到期日再提醒一次（`id|exp|日期|soon/over`）。`st` 是后台的状态（菜单栏插件存在 `.menubar-state.json`，`cam serve` 和桌面版在内存里）。**不会自动切换，也不推荐换哪个号。**
- 每天的最高用量：刷新用量写回时 `note_usage_day`（Rust：`usage::note_usage_day`，`tests/shared/usage_days.json`）把这次取到的 5 小时 / 7 天百分比记进账号的 `usage_days`（`{"2026-11-14": [64.0, 30.5]}`，本地日期，当天取最高，保留一位小数四舍五入，只留最近 30 天）；这次有错误（数字是上一次的）或者数字不是今天取到的不记。`/api/state` 原样带出，面板详情画成 30 根小柱子。
- 7 天额度的节奏（只提醒，不切号，也不推荐换哪个号）：
  - 采样点存在 `usage-samples.json`（不进 accounts.json，一行写完不缩进）：`{"version": 1, "accounts": {"<账号 id>": [[时间, 5 小时 %, 7 天 %, 5 小时重置, 7 天重置], …]}}`，时间都是 Unix 秒，百分比一位小数（`floor(x*10+0.5)/10`），没有的是 null。刷新用量写回 accounts.json 之后、还持有数据锁时 `note_usage_samples`（Rust：`pace::note_usage_samples`）给每个有额度的号（Claude Code、网页、桌面版）记一个点：点的时间是这份用量取到的时间 `fetched_at`，不比上一个点新的不记（桌面版的本地记录可能还是上次那份）、这次有错误的不记、两项都没有数字的不记、比现在晚 5 分钟以上的不记；和上一个点一样、上一个点也和再前一个一样时只把它的时间往后挪（一段没变化只留头尾两个点）；只留最近 8 天（`SAMPLES_KEEP_SECS`）、每个号最多 1200 个点（`SAMPLES_MAX`），删掉的号不留。内容没变不写；读的地方（`/api/state`、`/api/usage-samples`、菜单栏）没有这个文件时当空的，不建文件。
  - 推算 `usage_pace(points, usage, now)`（Rust：`pace::usage_pace`）：7 天窗口 = 重置时间往前 7 天。给出 `start`、`resets_at`、`used`、`even`（照匀速现在应该用到的百分比）、`ahead`（比匀速快）；速度先按最近一天（`PACE_RECENT_SECS`）里最早那个点到这份用量取到的时间算（`basis: "recent"`），最近一天的点不到 3 小时（`PACE_MIN_SPAN`）或者数字比那个点小，就按窗口开始以来的平均（`"window"`）；窗口开始还不到 6 小时（`PACE_MIN_ELAPSED`）不推（`state: null`）。重置前就用完是 `state: "out"`（`out_at`、`early_secs`），用不完是 `"ok"`（`left`：重置时大约还剩百分之几，四舍五入），已经用到 99.5% 是 `"full"`；`per_day` 是每天的速度。没有 7 天额度、重置时间或者不在窗口里时整个是 null。桌面号用的是显示用的那份（过了重置时间的窗口不算）。
  - `/api/state` 的每个号带 `pace`；`GET /api/usage-samples`（`?id=` 只要一个号）给 `{"now", "accounts": {id: {"pace", "points": [[时间, 7 天 %], …]}}}`，`points` 只要这个 7 天窗口里的，最多 240 个（`pace_series`：均匀抽，留最后一个）。两边的规则按 `tests/shared/pace.json` 跑。
  - 界面：管理面板、桌面版的账号卡和卡片小面板画额度环（5 小时、7 天、用得最多的按模型周额度，每个环下面是它的重置时间）、节奏小线和一句结论，详情里是带坐标轴的大图；菜单栏子菜单一行文字。到期那一行用 `days_left`：3 天内（和后台提醒的 `ALERT_SOON_DAYS` 一样）标黄，过了标红，勾了自动续期写每月几号（年付写每年几月几日）。
- 本机 Claude Code 消耗（管理面板、桌面版的“本机 Claude Code 消耗”）：读 `~/.claude/projects/**/*.jsonl` 里带 `usage` 的消息（按文件缓存、只读新追加的部分，同一条消息按 (消息 id, requestId) 只算一次；脚本版把 256 KB 以上的记录的扫描结果另存一份在 `scan-cache/` 里：`_scan_cached` 在进程里没有时先 `_scan_cache_load`（记录没变短、开头 4 KB 的指纹和存的一样才用，不然从头读），又读了 1 MB 以上或者隔了 2 分钟就 `_scan_cache_save` 重写一次，记录删了的缓存由下一个进程清掉；这样重启后的 cam serve、`cam refresh`、菜单栏小工具的后台那一轮不用把几百 MB 的记录从头读一遍，`CAM_NO_SCAN_CACHE=1` 关掉；一条消息的 usage 进内存前先 `_slim_usage`，只留算用量和金额要用的几个数），按 `switch_log` 归到当时登录的号，第一次切换之前的、已经删掉的号算“未归属”；金额按 API 价折算（`message_cost`），订阅不按这个收费，只用来比较。`local_report(data, range)` → 纯计算 `usage_report(events, range, now_local)`（Rust：`local_report::usage_report`，规则两边按 `tests/shared/local_usage.json` 跑），events 是 (本地时间, 模型, usage, 会话 id, 账号)，会话 id 取记录里的 `sessionId`、没有就用文件名：
  - `range`：`today` 今天按小时 24 根；`7` / `30` 往前 7 / 30 个日历天（含今天）按天；`all` 从最早一条记录那天起按天，超过 60 根（`LOCAL_MAX_BARS`）按周（周一起），还超过按月。没有记录的柱子也在 `buckets` 里。
  - 返回 `range`、`unit`（hour / day / week / month）、`start`、`buckets`（按顺序的柱子键：`2026-11-18T09` / `2026-11-18` / 那周周一 / `2026-11`）、`series`（每根柱子里每个号的 `input`、`output`、`cache_write`、`cache_read`、`messages`、`cost_usd`）、`model_series`（同样按模型）、`totals`（再加 `sessions` 会话数、`cache_hit` = 缓存读取 ÷（输入 + 缓存写入 + 缓存读取），四位小数，没记录是 null）、`accounts` / `model_totals`（明细，加 `sessions` 和 `top_model` / `top_account`：消息最多的那个，一样多取名字排前的）、`heatmap`（最近 28 天 `LOCAL_HEAT_DAYS`，`cells[星期几，周一 = 0][钟点]` 的消息条数，和 `range` 无关）。
  - 界面：四格汇总（Token、消息、折算金额、缓存命中率）、叠起来的柱子（Token / 金额 / 消息，按账号 / 按模型，悬停或点一下看明细）、明细（账号 / 模型）、热力图。颜色跟着账号走（账号列表的顺序，`--c1`…`--c6`），模型第一次出现时领一个空着的颜色记在浏览器里；第 7 个起是灰色的“其他”。时间范围、看什么、怎么分记在浏览器里。
  - 卡片小面板的“用量”页签用 `?range=`（今天 / 7 天 / 30 天），和管理面板同一份；“额度”页签的大卡（当前的 Claude Code 号）用 `?range=today` 的 `accounts` 写今天本机用了多少，只在这个页签开着时取，两分钟一次；`cam local` 还用按天的 `local_usage(data, days)`（`/api/local-usage?days=N`：`rows` 按 (日期, 账号)、`models` 按 (日期, 模型)）。
- 会话页（管理面板、桌面版的“会话”，卡片小面板的“会话”页签）：这台电脑上正在跑的 Claude Code。规则两边按 `tests/shared/sessions.json` 跑（Python 在 cam.py 的“会话页”一节，Rust 在 `sessions.rs`）。只读本机文件；只看用量、元数据、工具名和文件路径，消息内容不读出来、不返回、不记日志。
  - 钩子记的状态 `session-state.json`：`{"sessions": {"<session_id>": {"state": busy|idle|waiting, "since_ts", "last_ts", "first_ts", "pids", "cwd", "transcript"}}}`，时间是 Unix 秒。和 `attention.json` 同一批钩子（不多装事件）、同一把锁，只在状态变的时候写：`UserPromptSubmit` → busy、`Stop` → idle、`PermissionRequest` → waiting、这个会话正在 waiting 时 `PostToolUse` / `PostToolUseFailure` / `PermissionDenied` → busy、`SessionEnd` → 删掉。工具事件在会话不是 waiting 时只读一下就走，没有这个文件时连读都不读。`pids` 是钩子进程的父进程和祖父进程（Linux 读 `/proc` 拿祖父进程；别的系统只有父进程），会话页拿它对上正在跑的 claude。3 天（`SESSION_STATE_KEEP`）没事件的记录写的时候顺手删掉；最后一个会话删掉后文件也删掉。
  - 进程 → 会话（`match_sessions`）：先按钩子记的 pid；再按工作目录对钩子记录；剩下的用 `CLAUDE_CONFIG_DIR/projects/<工作目录里不是字母数字的字符换成 -（project_dir_name）>/` 这一层里最新、还没被占走的 `.jsonl`（子目录里是子 agent 的，不看）。工作目录：Linux 读 `/proc/<pid>/cwd`，macOS 用 `lsof -a -d cwd -p … -Fn`（按 (pid, 命令行) 记着，进程退出就扔），Windows 看不出来，剩下的钩子记录按最近有动静的先后分给没对上的进程（`guess`）。每行带 `matched`：hook / cwd / newest / guess。
  - 状态（`session_state(rec, waiting, written_ago, cpu, now)`）：attention 里有、还没答的是 waiting；钩子说 busy（或 waiting 但已经答过）的，除非 `SESSION_BUSY_QUIET`（600 秒）没写记录、这个进程也没在吃 CPU（按 Esc 打断时 `Stop` 不来），都算 busy；钩子说 idle 的，之后（`Stop` 之后 5 秒以外）记录又写了、而且 20 秒内写过才算 busy；没有钩子记录的，20 秒内写过记录或这个进程连同子进程在吃 CPU（每个进程各算各的，`CPU_BUSY`）算 busy。`/api/state` 的 `sessions` 是 `{"count", "busy", "answering", "waiting", "text"}`：`answering` 是 busy 的个数，0 个但整体看着在执行（只知道有在吃 CPU 的、对不上是哪个）时是 null，`text` 是“3 个会话 · 1 个在回答 · 1 个等你确认”（`session_phrase`），红绿灯和菜单栏用它。
  - 记录摘要（`transcript_meta`，按文件缓存、只读新追加的部分，几百 MB 的记录也只在第一次扫一遍：每行先按字节找 `"type":"user"`、`"type":"assistant"`、`"compact_boundary"`、`"isSidechain":true` 这些，冒号后也认一个空格，大多数行不解析）：几轮（`_is_prompt`：你发的消息，不算工具结果、`isMeta`、`isCompactSummary`、子 agent 的、斜杠命令和它的输出、`[Request interrupted`、`!` 跑的命令）、第一条的时间、工作目录、最后一次压缩的位置（`"type":"system","subtype":"compact_boundary"`）、最后一条真回复（`<synthetic>` 不算）的模型和用量。`context_used` = 输入 + 缓存读取 + 缓存写入 + 输出（这条回复本身已经在上下文里了），`cache_hit` = 缓存读取 ÷（输入 + 缓存读取 + 缓存写入），四位小数。
  - 上下文窗口（`context_window(model)`，按前缀找、具体的在前）：Fable / Mythos / Opus 5.x / Sonnet 5.x / Haiku 5.x / Opus 4.6–4.8 / Sonnet 4.6 是 1M；Haiku 4.5、Opus 4.5 及更早、Sonnet 4.5 及更早、Claude 3 是 200K；模型 id 带 `[1m]` 的是 1M；`us.anthropic.claude-…`、`anthropic/claude-…` 这类从 `claude-` 那里比；认不出的按 200K、`window_exact` 为 false（界面写“约”）。来源是 Claude API 文档的模型表（2026-11）。`context_health`：用到 70% 是 high（快满了），90% 是 full（很满），否则 ok（健康）。
  - 组成（`transcript_composition`，只看最后一次压缩之后，也按文件缓存、只读新追加的；压缩位置变了从新位置重来）：每块内容按 UTF-8 字节数除以 4（向上取整）估 token，一张图按 `IMAGE_TOKENS`（1600）。对话：你发的消息、回复的文字和思考（记录里有文字的部分）按轮算，压缩后的摘要单算一项，第一轮之前的叫“开头”；读过的文件：`Read` 的参数和结果按文件路径算（×读了几次，显示文件名 + 工作目录里的相对目录 / `~/…`）；工具结果：别的工具的参数和结果按工具名算（×调了几次；对不上调用的结果算在 `?` 下）；图片：你发的、工具返回的分开算。加起来比 `context_used` 多时每项按比例缩（整数，`tokens * total // counted`），“其他（估算）”= `context_used` − 加起来的（系统提示词、工具定义、记忆文件、只留了签名的思考过程这些记录里没有的），没有用量时总数就是估的加起来、`exact` 为 false。返回 `{"total", "exact", "scaled", "since_compact", "window", "window_exact", "categories": {"chat", "files", "tools", "images", "other"}, "items": [{"cat", "key", "name", "detail", "count", "tokens"}…前 60 项], "item_count"}`，items 按 token 从多到少、一样多按 key。
  - `GET /api/sessions`：`{"known", "count", "answering", "waiting", "hooks", "platform", "generated_at", "sessions": [{"id", "pid", "state", "matched", "project", "cwd", "login", "login_text", "account_id", "account", "model", "started_at", "last_active", "turns", "context_used", "context_window", "window_exact", "cache_hit", "health", "has_transcript"}…]}`，顺序是 waiting、busy、idle，同类按最近写记录的先后；进程没查出来时 `known` 为 false。`account` 是这个进程启动时（按 `switch_log`）登录的号；会话自带 token、用 API Key 或单独配置目录的不算账号，`login_text` 写着。`GET /api/sessions/<id>` 给 `{"session", "composition"}`，会话不在跑了返回 400。面板“会话”页开着时每 5 秒取一次，卡片小面板的“会话”页签开着时每 10 秒。
- 启动前风险提醒（只在 `cam claude`）：缓存不能直接用时（`region_trusted`：超过一小时、最近一次检测没连上、或者本机换了网络），`spawn_background_check` 另起 `cam.py _check-region` 后台进程做完整检测（同一时间只跑一个），前台只用 `quick_trace` 花最多 3 秒问一次 claude.ai，套到数据副本上算提醒（`with_quick_region`，出口 IP 变了时丢掉旧运营商信息）。`launch_tier(score, region_status, prefs)` 给出 ok / remind / confirm / strong 四档——≥90 一行带过；< 90 打印提醒；< `risk_confirm_below`（默认 60）问 y/N；< `risk_strong_below`（默认 30）或出口不在支持名单内要输入"继续"。非交互环境只打印。`risk_prompt: false` 或 `CAM_NO_RISK_PROMPT=1` 关掉。**不是拦截**：每一档都是用户自己选择继续或取消。
- 自动发现：后台检测凭据文件变化（macOS 每 30 秒查一次钥匙串），发现没见过的账号就自动导入。

## 四类账号

| 类型 | 切换方式 | 用量来源 | 到期来源 |
|---|---|---|---|
| `claude_code` | 替换 Claude Code 的登录凭据，聊天记录/项目/设置共用不动 | `api.anthropic.com/api/oauth/usage`（5 小时、7 天额度）+ 本地 JSONL 日志统计 token | 手动填写（官方无接口），可设"每月自动续期" |
| `claude_desktop` | 每个账号一份用户数据目录，每次打开均退出并重启 Claude Desktop，见下节。首次打开需要登录，会话过期后也可能需要重新登录 | Claude 本地 HTTP 缓存，缺失时读取本地用量历史；保留真实记录时间 | 手动填写 |
| `claude_web` | 每个账号一个独立浏览器 Profile，"切换"就是打开对应窗口 | 可选填 `sessionKey`，走 claude.ai 内部接口；失败则只显示手动信息 | 手动填写 |
| `api` | 不需要切换，只做展示（可复制 Key） | Admin Key → Cost/Usage Report；普通 Key → 仅校验有效 | 无 |

## Claude Desktop 切换

Desktop 登录与 Claude Code 凭据分开管理。`desktop_profile: "default"` 使用 Claude 的原有数据目录（macOS 为 `~/Library/Application Support/Claude`，可用 `CAM_CLAUDE_DESKTOP_DATA` 覆盖），不会收紧原目录权限或在移除记录时搬走它；其它配置使用 `desktop-profiles/<id>`。新配置首次启动后由用户在 Claude 内登录。每个配置保存各自的会话数据，默认不向 Desktop 注入 `CLAUDE_CONFIG_DIR`，也不切换命令行的账号。

显示用的状态（只读，切换和移除前的判断不走这里，那里现查进程）：`desktop_view(store, data)`（Rust：`desktop_app::desktop_view`，计算部分 `view_with`）给出 `/api/state` 的 `desktop_app`（`installed`、`version`、`running`——查不出进程列表时为 null、`account`——开着的实例用的是哪个桌面号、`known`——开着的实例都看得出用的哪个目录）和每个桌面号的 `desktop`（`running`、`login`、`login_expires`）；没有桌面号时 `desktop_app` 为 null，也不去查进程。
- 版本：`_desktop_installed`——macOS 读 `Claude.app/Contents/Info.plist` 的 `CFBundleShortVersionString`，否则 `desktop_version_from_path` 从路径里取（`app-0.14.10`、商店版 `AnthropicPBC.Claude_1.0.734.0_…`；装在系统目录的商店版从正在运行的程序路径取），Linux 再问 `dpkg-query`
- 开着没有：`desktop_app_status` 复用 `_desktop_scan`（和切换同一套认进程的规则），缓存 `DESKTOP_STATUS_SECS` = 20 秒，过期了先给旧的、后台重查（和 `session_count` 一样）；Windows / Linux 上没带 `--user-data-dir` 的实例就是 Electron 的默认目录，macOS 靠 lsof（几百毫秒，所以按 (进程, 命令行) 记着查到的目录：`_LSOF_PROFILES` / Rust `cached_profile`，没查到的不记，进程退出就忘）
- Windows 上查进程、查系统设置都要起 PowerShell（每次大约半秒 CPU），合并成两种查询、几秒内共用：`windows_processes(max_age)`（Rust `health::win_processes`）一次 `Win32_Process` 列出全部进程（pid、程序路径、名字、命令行，Tab 分开），Claude Code 会话（1 秒内的直接用）、桌面版状态（3 秒）、面板服务认进程共用，切换、退出、移除桌面版仍然现查（`max_age=0`）；`windows_env()`（Rust 同名）一次查出 DNS 服务器、系统语言、默认路由的网卡（ConvertTo-Json），5 秒内共用。两个输出的解析规则两边按 `tests/shared/windows_env.json` 跑
- Windows 上起控制台程序（powershell、taskkill、reg、git、tzutil、claude --version）都加 `CREATE_NO_WINDOW`（`NO_WINDOW`；Rust 的 `powershell()`、`run_command`、退出桌面版的 taskkill 同样）。后台起的 cam 进程（`cam serve`、环境检测）用 `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`（`BG_FLAGS`）：带一个看不见的控制台，它再起的控制台程序跟着用这个，不弹窗口；不用 `DETACHED_PROCESS`，那样进程没有控制台，每起一个控制台程序 Windows 都给它新开一个窗口（以前托盘、卡片小面板开着时隔一会儿闪一下蓝色的 PowerShell 窗口就是这个）
- 本机登录：`desktop_login` 只读数据目录里 Chromium Cookie 库（`Cookies`、`Network/Cookies`、`Partitions/*/…`）中 `claude.ai` 的 `sessionKey` 的到期时间（不读值；`?mode=ro&immutable=1` 打开，不加锁、不写日志），加上 `config.json` 里有没有 `oauth:tokenCache*` / `lastKnownAccountUuid`，`desktop_login_state` 算出 `yes` / `expired` / `no` / `never` / `unknown`，缓存 20 秒。两条规则两边按 `tests/shared/desktop_status.json` 跑。服务端作废的登录这里看不出

生命周期操作串行执行：定位客户端并检查进程 → 正常退出并等待 → 备份已关闭配置的登录文件 → 以目标 `--user-data-dir` 启动 → 确认启动结果后记录。即使目标与上次记录一致，也重新执行退出和启动，避免用户从 Dock 或快捷方式打开了其它配置后，缓存记录导致打开错误账号。进程枚举失败与“没有进程”区分处理；退出超时停止操作。启动失败时尝试重新打开原配置，恢复结果如实报告。备份只包含登录相关文件，对话、Claude Code 会话和 Cowork 虚拟机仍留在原目录。

`active.claude_desktop` 和公开的 `is_active` 只表示工具最后成功启动的配置，前端显示“上次打开”。它不是持续在线状态，也没有验证邮箱是否与用户填写的一致；用户手动从 Dock 或其它入口启动后不会自动更新。删除这条最后打开的配置时要求先用工具打开其它 Desktop 账号。React 和 Web 面板使用全局操作状态，切换过程中即使选择另一张卡片也不能再发起一次切换。

同机系统钥匙串中的 Desktop safeStorage 加密密钥可能被多个配置共用，目录隔离的是会话数据，不代表每个账号有独立钥匙串。本工具不读写 Desktop 钥匙串。配置和备份仍是敏感数据，只供同一电脑上的当前系统用户使用，不保证跨设备、重装后或会话被撤销后仍能恢复。文件存在只用于提示是否已有登录数据，不能证明服务端登录仍然有效。

macOS 通过 `open -n -a Claude.app --args --user-data-dir=…` 启动。Windows 独立安装版可以尝试；Microsoft Store / MSIX 版仅覆盖部分进程识别，尚未完整支持自动发现与启动。`CAM_CLAUDE_DESKTOP` 可指定客户端可执行文件（macOS 为 `.app`），`CAM_CLAUDE_DESKTOP_DATA` 可指定原有数据目录，自定义路径需要实际验证。Linux 没有官方 Desktop，仅适配部分第三方封装路径。测试以临时配置、模拟进程和启动器验证流程，不声明所有真实客户端版本均已实测支持。

参考项目均为 MIT，仅借鉴设计方法，没有复制源码：

- [matsumotory/claude-desktop-switcher 的 macOS 适配](https://github.com/matsumotory/claude-desktop-switcher/blob/main/crates/core/src/platform/macos.rs)：`open -n`、目录隔离、严格区分进程检测失败和未运行。
- [jspanji/Swivel 的 ProfileManager](https://github.com/jspanji/Swivel/blob/main/Sources/SwivelCore/ProfileManager.swift)：等待退出后再保存，切换不改 Desktop 钥匙串。该项目还实现了快照校验；本项目沿用独立目录方式，没有照搬全目录恢复。
- [nyxon-tech/claude-switcher](https://github.com/nyxon-tech/claude-switcher)：区分登录文件、账号侧栏与共享会话历史，处理 Windows 安装类型差异。普通切换不迁移聊天记录。

### Desktop 本地额度

读取各账号 `profile_dir` 中的本地记录，独立于客户端切换，不退出 Claude、不访问网络、不读取或解密 Cookie / OAuth / Keychain，也不要求用户额外登录。优先解析 `Cache/Cache_Data` 内 Chromium simple-cache 保存的 `https://claude.ai/api/organizations/<uuid>/usage` 响应：支持明文 JSON 与 Zstandard 压缩；窗口结构沿用 `five_hour` / `seven_day` / 模型分窗口的 `{utilization, resets_at}`，百分比为 0–100。缓存缺失时尝试本地用量历史；历史仅提供百分比时保留 `resets_at: null`，不推算重置时间。对应窗口的历史超过 5 小时 / 7 天后不再显示。

公开的 `usage` 保留以下来源与时间信息：`source: desktop_cache | desktop_history`，`fetched_at` 为真实快照时间或缓存文件 mtime，`checked_at` 为本次读取时间，`cache_stale` 在快照超过 30 分钟时为 true，`cache_status` 为 `ok | missing | expired | ambiguous | unsupported_compression`，`cache_org_uuid` 为对应组织，`notice` 用于解释来源和引导。无记录是 `error: null` 加说明，不等于账号异常，也不生成 0% 额度。前端仅读取这两种 Desktop 来源展示百分比，隐藏已到 `resets_at` 的窗口，同时区分“记录时间”和“最近读取”；本地快照不推算消耗速度。

读取限制输入和解压大小，对损坏、组织歧义、未知格式软失败，保留明确状态。Python 3.14+ 使用标准库解压；旧版可安装 `scripts/requirements-desktop.txt` 中的 `zstandard`，无依赖仍可读明文缓存及本地历史。Rust 使用内置 `ruzstd` 解压。客户端缓存格式属于内部实现，升级可能导致读取失败，不能把缓存的存在或新近 mtime 当成实时服务器验证。

格式研究参考 [ajipurn/claude-profiles UsageReader.swift](https://github.com/ajipurn/claude-profiles/blob/main/Sources/ClaudeProfilesCore/UsageReader.swift) 及其测试，本项目独立实现，未复制源码；该归档仓库未声明开源许可，因此不列入上述 MIT 项目。另研究了 [l2succes/claude-switcher](https://github.com/l2succes/claude-switcher/blob/main/main.swift)（MIT）的 OAuth 解密查询和 [Swivel LiveUsageClient](https://github.com/jspanji/Swivel/blob/main/Sources/SwivelCore/LiveUsageClient.swift)（MIT）的 Cookie 查询方案；当前实现采用本地记录方案。

## 切换流程（保证数据不丢）

本节描述 Claude Code；整个流程在数据锁（`accounts.lock`）里完成。

1. **先快照**：读取当前正在生效的凭据（Linux/Windows 读 `~/.claude/.credentials.json`；macOS 读钥匙串 `Claude Code-credentials`，没有再读文件）以及 `~/.claude.json` 里的 `oauthAccount`。
2. **回写**：认出它属于哪个已保存账号（见下一节），把最新 token 写回该账号（Claude Code 运行时会刷新 token，不回写的话旧 token 会失效）。如果是没见过的账号，自动导入为新账号，绝不丢弃。认不出是谁的（见下一节的“暂存”）就不写，切换到候选账号前会先核对一次。
3. **核对目标**：目标账号保存的 access token 已过期或 5 分钟内过期（`SWITCH_RENEW_SECS`，和 Claude Code 自己续的时机一样）时，先用它自己的 refresh token 换新并存回它名下（它不是当前登录，没人跟它抢）。换不了就整个切换作罢，下面的步骤一步都不做，当前登录原样不动：token 接口返回 400/401（refresh token 已作废）时提示用这个号重新登录（`cam login`）后再切，并作废指向它的 `pending_switch`，免得后台一直重试；连不上时提示稍后再试，登记留着。
4. **备份**：把凭据文件和 `~/.claude.json` 复制到 `backups/<时间戳>/`。
5. **写入**：读出当前的凭据，只把 `claudeAiOauth` 换成目标账号的，其它键（例如 MCP 服务器的登录 `mcpOAuth`）保持现在的样子，不拿目标账号当初保存的旧副本整个覆盖；原子写入（macOS 写钥匙串，同时有文件时文件也一起更新）。`~/.claude.json` 只替换 `oauthAccount` 字段，其它字段原样保留（合并方式见“并发与锁”）。
6. **记录**：更新 `active` 和 `switch_log`，清掉 `live_mismatch`、`live_unverified` 和 `pending_switch`。

不动的东西：`~/.claude/projects/`（会话记录）、`settings.json`、`CLAUDE.md`、MCP 配置、插件。所以切换后 `claude --continue` / `--resume` 照常可用。

正在运行的 `claude` 进程内存里还是旧 token，切换后需要重启这些会话；不重启的话，它刷新 token 后还会把旧账号写回凭据文件。所以切换后会扫一遍进程（Linux 读 `/proc`，macOS 用 `ps`，Windows 用 `Win32_Process`），认得出原生安装版和 npm 版的 Claude Code，不算 Claude 桌面 App。有的话在返回消息、系统通知、终端输出里提示进程数和 pid；`cam switch` 在交互终端里会在切换前先问一句。

**等会话结束再切**：切换前发现有会话在跑时，可以不打断它——把目标记在 `accounts.json` 的 `pending_switch`（`{"account", "reason": "manual"|"rule", "rule", "at"}`）里，由持有后台租约的那个进程（桌面版、`cam serve`、菜单栏插件）每轮检查：没有 `claude` 进程了就执行切换并清掉登记。目标账号被删、已经是当前账号、手动登记放了 6 小时（`PENDING_SWITCH_HOURS`，按 `at` 算）还没等到、或时间规则的时间段已经过了，登记作废；过期的不等会话退出就清掉，界面上也不再显示。手动切换会覆盖登记。`cam switch --wait` 自己也会轮询等待，Ctrl-C、SIGHUP（关掉终端）、SIGTERM 都会撤掉登记，切换出错也撤掉，不留一个看不见的登记。面板 / 桌面版的切换接口在没指定 `mode` 且有会话在跑时返回 `choice_required`，由前端弹三选一；`cam switch` 没有终端（脚本、菜单栏插件）且没加 `-y` / `--now` / `--wait` 时按 `mode=wait` 处理。`cam`、`cam ls`、`cam doctor` 都会显示登记中的等待切换和取消方法（`cam switch --cancel-wait`）。

Windows 托盘小工具（`extras/windows-tray`）不另写一套：`cam_tray.py` 调用菜单栏插件的 `render`，把 SwiftBar 格式的菜单行转成 JSON（`{"tip", "badge", "level", "icon", "watch", "items", "notes", "mini"}`），`cam-tray.ps1` 用 WinForms 的 NotifyIcon 画图标和菜单，点了哪一项再交回 `cam_tray.py do`；图标样式记在 `prefs.menu_icon`（和菜单栏共用，多一种 Windows 才有的 `badge` 数字方块，没选过时用它）。数字方块要写字，由 `cam-tray.ps1` 用 System.Drawing 画；别的样式由 `cam_tray.py` 调菜单栏插件的 `status_icon`，按托盘图标的实际像素（`menu 像素`，屏幕缩放 100% 是 16）画成 PNG，存在 `%LOCALAPPDATA%\cam\tray-icons`（按参数起文件名，画过的不再画），`icon.frames` 给路径；工作中 / 等你确认时 8 帧、每帧 0.25 秒，由托盘自己轮着放。任务栏深浅色读注册表 `SystemUsesLightTheme`。右键菜单：账号行由菜单栏插件在不 rich 时多给 `wname` / `wplan` / `wflag` / `wp5` / `wp7`（SwiftBar 看不到），`cam_tray.py` 排成名字 + 右边对齐的额度（`right`，WinForms 的 ShortcutKeyDisplayString）+ 额度条（`bar`、`barColor` / `barColorDark`），套餐放进子菜单第一行；超过 `WIDTH`（36，汉字算 2）的说明拆行、能点的截短并把全文放 `tip`。菜单的画法是 `cam-tray.ps1` 里 Add-Type 编译的 C#（Windows PowerShell 5.1 只认 C# 5）：`CamTray.MenuRenderer`（浅色 / 深色跟着注册表 `AppsUseLightTheme`）、`CamTray.BarItem`、`CamTray.Look.Round`（Windows 11 的 DWM 圆角）。`watch` 为真（开着会话或在动画）时托盘每 20 秒跑一次 `cam_tray.py light`（只算图标和提示，不做后台任务、不算菜单）；后台任务、状态文件 `.menubar-state.json`、通知内容都和菜单栏插件一样，只是通知由托盘的气泡弹出。

菜单栏插件点切换时运行的是插件自己的 `switch` 命令（`api_switch(mode=wait)`），结果和出错都弹系统通知。后台任务一轮做的事（自动导入、时间规则、等待切换、定时刷新和额度提醒）写在 `cam.background_round` 里，菜单栏插件和 `cam serve` 共用，通知也一样（macOS 用 `osascript`，其它系统有 `notify-send` 就用它）：`cam serve` 拿着后台锁时插件不跑后台任务，通知由 serve 弹。等待中的切换因为目标登录失效被作废时也通知一次。

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
  - 切换到一个 access token 过期的号时，切换前在同一把锁里先换新（见“切换流程”第 3 步），换不了就不切。
- **拉用量前先核对**：先请求 `/api/oauth/profile`，返回的 uuid / 邮箱和账号记录的不一致（凭据被旧会话覆盖了），就不拿这份凭据查用量，直接报错。资料和用量两个请求各最多等 `USAGE_TIMEOUT` = 12 秒；资料那个等了 `USAGE_GIVE_UP` = 5 秒以上还没连上（断网、代理节点挂了）就不接着读用量（也是白等同样久），一下就失败的（连接被重置）才接着试。

**登录本身的有效期**：Claude Code 登录时在凭据里存 `refreshTokenExpiresAt`（毫秒，token 接口回复的 `refresh_token_expires_in`，没给按 30 天），续 access token 时取原来的和新给的里更早的（原来没有就还是没有），所以续期延不了它，到期只能重新登录；它在剩 3 天以内时提示正在用的号。cam：
  - `login_exp_valid` / `login_expiry_ms(oauth 块)`：和 Claude Code 一样只认 2025-01-01 以后、10 年以内的数字；access token 比它还晚 3 天以上才过期的不算（Claude Code 这时也不提示）。`login_days` 向上取整，到期是 0。规则在 `tests/shared/login_expiry.json`
  - 数据在各号保存的登录里，accounts.json 不存。`/api/state` 每个号带 `login_expires_at`（ISO）、`login_days_left`；`with_login_expiry(store, data)` 给账号带上 `login_expires_at`（毫秒）和 `login_days_left`，供 `due_alerts`（后台）和 `account_health` 用：剩 `LOGIN_SOON_DAYS` = 7 天以内 `login_expiring`（注意），到期 `login_expired`（异常，这时 `login_invalid` / `stale` / `live_expired` 不再另报）；提醒在剩 3 天、1 天、到期各一次（`<id>|login|<毫秒>|3/1/over`），不在用的号也提醒
  - 续期（`renew_saved_login`、核对暂存登录时的续期）按同一条规则改 `refreshTokenExpiresAt`（`set_renewed_login_expiry`）
  - **重新登录不在用的号**（`cam login 名字`，名字 / id / 邮箱正好是一个已保存的 Claude Code 号、又不是当前号）：`relogin_aside` 在 `<数据目录>/relogin/login-xxxx`（0700）里跑 `claude auth login`，环境带 `CLAUDE_CONFIG_DIR` 和 `CLAUDE_SECURESTORAGE_CONFIG_DIR` 指向它；登录读 `<目录>/.credentials.json`，macOS 上读钥匙串的 `Claude Code-credentials-<目录路径 NFC 的 sha256 前 8 位>`；身份看 `<目录>/.claude.json` 的 `oauthAccount`，没有就问 `/api/oauth/profile`；和这个号对不上、或登录期间它成了当前号就不存。存用 `file_credentials`（和导入一样），最后删目录和钥匙串那一条；上次被打断留下的超过一小时的目录下次删。当前登录、正在跑的会话不受影响。菜单栏 / 托盘子菜单的“重新登录…”就是开终端跑 `cam login <id>`

**添加账号（`cam login`）**：先像 `cam` 一样把当前登录存回它的账号（没见过的号自动导入），再运行 `claude auth login` 换号登录，最后导入新登录（已有的号就更新）。直接 `/login` 的话，当前账号在上次保存之后续过的 token 还没存进来，换号后就作废了。当前登录认不出是谁的（会被暂存，见“暂存认不出主人的登录”）时不换号，免得丢掉它。

## 并发与锁

桌面版、`cam` 命令、`cam serve`、菜单栏插件可能同时在跑，Claude Code 自己也会写 `~/.claude.json`。

- **数据锁 `accounts.lock`**：读改写 `accounts.json` 和 `secrets/` 时持有。Unix 用 `flock`，Windows 用 `LockFileEx`，Python 和 Rust 用的是同一种系统锁，所以两边互斥；每次加锁单独打开文件，同一进程的不同线程之间也互斥。锁由操作系统持有，进程崩溃时自动释放，不会有残留锁。等锁最多 30 秒。旧版本用的是独占创建 `.lock` 文件、超过 60 秒当残留删掉，换了文件名就不会和旧版互相误删。
- **后台任务锁 `background.lock`**：自动导入、时间规则、定时刷新、额度提醒只让一个进程做，不等待，谁先拿到谁做。桌面版和 `cam serve` 拿到后一直持有到退出，没拿到的每隔一会儿再试一次（接手退出的那个），期间只同步显示数据；菜单栏插件每次运行时试一下，拿不到就只画菜单。
- **等你确认的会话 `attention.json`**：`cam hooks install` 装的钩子（`PermissionRequest` 记下会话，`PostToolUse`、`PostToolUseFailure`、`PermissionDenied`、`UserPromptSubmit`、`Stop`、`SessionEnd` 清掉）每个事件起一个进程，几个工具并行跑完时是好几个一起来，读改写时持有 `attention.lock`（一直等到拿到，Windows 上最多 5 秒）。没有会话在等时清除事件一看没有这个文件就退出，不拿锁、不写；最后一个会话清掉后文件删掉；内容没变不重写。只记工具名和说明，不记命令。钩子的命令是 `python scripts/cam_hook.py`：它把 `cam` 当模块运行，用得上 `__pycache__` 里的字节码，`cam.py` 最前面的快路径（只用 `json`、`os`、`sys`、`time`）记完就退出；旁边没有 `cam_hook.py` 时是 `python cam.py hook`，同一段代码，每次多花一百毫秒左右编译。`hooks_status` 把旧写法算作已装并标 `slow`，指向别的路径或别的 python 才算过期。体检（`hook_items`）在“cam 自己”一节加一行：装好了是正常，钩子命令里的 python 或脚本已经不在了是警告（每次工具调用白起一个跑不起来的命令、灯也不闪），只装了一部分是警告，旧写法、指向另一份还在的 cam 只是告诉一声；没装钩子不提。引擎只读这个文件。同一批钩子顺便写 `session-state.json`（每个会话在干什么，见“会话页”），同一把锁，只在状态变的时候写
- **`~/.claude.json`**：读出来改完，写之前再读一次，内容变了（Claude Code 刚写过）就基于新内容重新改；写完再读回确认 `oauthAccount` 是想要的值。最多重试 5 次，还不行就提示先退出正在运行的 claude。
- **符号链接**：`~/.claude.json` 等文件是符号链接（比如用 dotfiles 管理）时，写到链接指向的文件，保留链接本身。

## 检测进度

“重新检测”“全面体检”要等几秒到十几秒，界面要能说出做到哪一步了。`CheckProgress`（Rust 版 `health::Progress`）记一次检测的进度：步骤按 `add(键, 名字, rank)` 登记，一步里的每件事 `start` / `finish(成没成, 怎么没成的)`（一步可以有几件事同时做，全做完才算完，有一件没成就是 fail），到时限还没回来的 `expire` 成 timeout，这次不用做的 `skip`，整次结束 `close`。已经有结论的步骤，晚到的 `finish` 不再改它。`tracked(prog, 键, fn, good, note)` 把一件并行的事包一层：现在算开始，做完按 `good(结果)` 记。没人看进度时传 `None`（Rust 版 `Progress::none()`），什么都不记。

“重新检测”的步骤表是 `service_steps(外部 IP 查询关着没有, 设了代理没有)`：local、dns、exit、isp、stack、plain、api、web、auth、tls、proxy、status、score；关了外部 IP 查询没有 isp / stack / plain，没设代理没有 proxy。“有回应”（哪怕是 4xx / 5xx）就算这一步做完，结果在检测清单里说；没有 IPv6、注册国查不到不算没成。“全面体检”在外面再加 usage、release、bots（装了才有）和整理报告的 accounts / claude_code / cam 三步（只有脚本版有）。

界面自己生成一个 `run`，带在 `/api/service-check?run=…`（或 `/api/checkup?run=…`）里，同时每 300 毫秒问一次 `/api/progress?run=…`；问不到（老版本的服务）就只显示“正在开始”，检测照常。两个版本一致的部分在 `tests/shared/progress.json`。

## 全面体检（cam doctor）

`full_checkup(store, offline)` 返回 `{"version", "generated_at", "offline", "header", "sections": [{"key", "title", "items"}], "summary": {"bad", "warn", "fixable", "text", "first"}}`，每条 `{"level": "ok"|"warn"|"bad"|"info", "text", "advice"}`（账号那一节多一个 `group`：账号小标题；来自服务检查的条目保留 `kind`；“本机 Claude Code”那一节里 `cam env fix` 能一键改的带 `fix: true`，`summary.fixable` 数它们，`cam doctor` 最后提示一句）。`cam doctor` 打印它（标记顶格，`install.sh` 按 `^(✗|!) ` 挑出要留意的行），`--json` 原样输出，`GET /api/checkup` 给面板，菜单栏插件跑 `cam doctor --json` 取 `summary` 发通知。`summary` 不数 `info` 和服务检查的总体结论（`kind: summary`，它重复了最严重的那条）。不含任何密钥。

- 联网时几件事并行：`refresh_usage(store)`（和 `cam usage` 一样：不在用的 Claude Code 号 access token 两分钟内过期就由 `ensure_fresh_token` → `renew_saved_login` 用它自己的 refresh token 续，当前登录从不续）、`service_check`、官方状态页 `summary.json`（`STATUS_SUMMARY_URLS`，先 status.claude.com 再 status.anthropic.com）、`downloads.claude.ai/claude-code-releases/<渠道>` 的版本号、聊天机器人的 `doctor`（60 秒超时）。续没续看续之前保存的 `expiresAt` 有没有变大；续不上（`登录已失效`）记 ✗ 并给 `cam login 名字`
- `--offline`：上面这些都不做，账号用上次的用量和保存的 `expiresAt`，服务一节用后台缓存（`region_view` + `risk_items`）
- 会话（只有 Linux）：`/proc/<pid>/cmdline` 认出 claude（同 `running_claude_processes`），`environ` 里 `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` 只记有没有，`CLAUDE_CONFIG_DIR` 的值只和本机配置目录比；`FEISHU_SESSION` / `DINGTALK_SESSION` / `DISCORD_STATE_DIR`、tmux socket（`claude-discord[-名称]` 等，从 `TMUX` 取）认出机器人会话；运行时长按 `stat` 第 22 项和 `uptime` 算。测试用 `CAM_TEST_PROC_ROOT` 指向假的 `/proc`
- 代码换了新版本（面板里点的更新、`cam update`、`git pull`）：`CODE_STAMP` 是启动时 `code_files()`（`cam.py`、`scripts/cam_*.py`、`web/panel.html`、`web/mini.html`）里最新的修改时间，`/api/state` 带着它（`code_stamp`）和 `code_watch`。`cam serve` 每 3 秒看一次（`watch_code`）：连着两轮看到同一个新时间、没有 `update.lock`（更新正在进行，自检没过会退回）、用新代码跑 `cam --help` 起得来，才停止收新请求、等手上的请求和后台那一轮做完（`SERVE_BUSY`，最多 90 秒），关掉端口，用同样的命令行重启自己（macOS / Linux `exec`，进程号不变；Windows 起一个新的再退出）；起不来就接着用旧代码，打一行日志，文件再变时再试。`CAM_NO_CODE_WATCH=1` 关掉。面板和卡片看到 `code_stamp` 变了整页重新加载（开着对话框时等关掉）；`code_watch` 没有的是这之前的老版本，面板顶上提示怎么换。`ensure_panel`（`cam panel`、菜单栏 / 托盘的卡片）发现在跑的 cam serve 的 `code_stamp` 和磁盘上现在的代码对不上（`serve_outdated` 比的是 `code_stamp_now()`，不是问的那个进程启动时的：常驻的菜单栏插件只在 `cam.py` 和它自己变了时才重新加载），就按命令行找到它结束掉，在同一个端口起新的。`port_free` 在 macOS / Linux 上带 `SO_REUSEADDR`（和 cam serve 绑端口时一样）：结束的服务留下的 TIME_WAIT 连接不算占用，不然要等一分钟
- 后台：`background.lock` 拿不到就是有人在做；`cam serve` 每分钟写 `.serve-state.json`（pid、时间、刷新间隔、上次出错），菜单栏插件每轮写 `.menubar-state.json`（看修改时间），桌面版不记，按进程名认。各账号用量的新旧（后台有没有在查）用体检之前的 `checked_at` 判断；旧版本没记它，那时的 `fetched_at` 就是查的时间。离线体检只读：后台锁文件不存在时不去建它

## 服务检查与环境风险提示（只提示，不拦）

`cam doctor` / `GET /api/service-check` 实测服务，并把使用环境里的风险点逐项列出，每项带 `kind`、`level`、`text`、`advice`。所有判断只用来提示，**不拦任何操作**，是否继续由用户决定。

- 出口地区：`claude.ai/cdn-cgi/trace` 的 `loc`（`exit_trace`：它答不上来、或者 `TRACE_HEDGE_SECS` = 2 秒还没回音，就同时问 `www.cloudflare.com` 的同一个接口，谁先给出结果用谁的，总共最多等 6 秒；off 模式不问 Cloudflare），对照 `SUPPORTED_REGIONS`（Anthropic 官网公布的支持国家/地区）+ `REGION_DEPENDENCIES`（属地按所属国家看待）。同一个 trace 还给出 `warp`（经过 Cloudflare WARP / Zero Trust）和 `ts`（算本机时钟偏差）。再查出口 IP 的 `asn`、运营商（`org`）、城市、国家（`meta_loc`，和 trace 的 `loc` 交叉核对）和类型（`ip_type`：residential / hosting / proxy / tor / mobile / null）：来源由 `prefs.ip_lookup`（auto / ip-api / off）决定，`meta_sources` 给出顺序：ip-api 模式先问 ip-api.com（免费接口仅限非商业），然后 `api.ipapi.is`（有 key 带标记）→ `ipinfo.io` → `speed.cloudflare.com/meta`，第一个查到的为准并记下 `meta_source`（界面注明来源）；`parse_exit_meta` 统一几种格式（含 ipapi.is 不带 key 的简版）；没配 ipapi.is key 时并行查 `rdap.org` 拿注册国 `reg_loc`；`1.1.1.1` 和 `[2606:4700:4700::1111]` 的 trace 分别拿 `v4_loc` / `v6_loc`。这些查询并行进行（`run_parallel`：每件事一个 daemon 线程，结果进队列，到时限就返回——域名解析没有超时可设，卡在里面的那件不拖着调用方，也不拦着进程退出），合起来最多等 `META_BUDGET_SECS` = 8 秒，其中每个运营商数据源最多 `META_SOURCE_TIMEOUT` = 4 秒，到点没答的当作查不到，不拖着出口地区；出口 IP 和设置都没变、且 `META_TTL`（默认一天）内查过时，运营商那几项（`META_FIELDS`）直接沿用缓存，不再问第三方。off 模式只问 `claude.ai/cdn-cgi/trace` 和状态页。key 存在 `secrets/_ip-lookup.json`，不进 accounts.json；`save_privacy` 改设置或 key 时清掉缓存的运营商信息，`clear_region_history` 清掉 `region`、`status_page` 和各账号的 `regions`。`local_env()` 把时区、系统语言、本机 DNS 配置一起存进 `region.env`，界面刷新时不用再跑系统命令。
- 后台缓存：持有后台租约的进程每隔几分钟调一次 `check_region`，把结果写进 `accounts.json` 的 `region`（`loc`、`ip`、`colo`、`ms`、`warp`、`skew`、`checked_at`、`net`，出口换了国家时另记 `prev_loc`、`changed_at`）和 `status_page`（`fetch_status_page`：先要 `status.claude.com/api/v2/summary.json`（连不上换 status.anthropic.com），`parse_status_summary` 只留和这里有关（`status_relevant`，按词比：Claude Code、API、claude.ai、Console）的不正常组件 `components` 和没解决的事件 `incidents`，另记整页的 `indicator`、`description` 和 `page`；summary 都连不上再要 `status.json`，那种只有 `indicator`、`description`，没有 `components`。这次没读到就留着上一次的，它自带 `checked_at`，超过一小时不再算），并给当前账号的 `regions`（`[{"loc", "at"}]`，最多 8 个）记一笔。整个 `check_region`（出口、状态页、本机设置、`net_fingerprint` 同时做）最多等 `REGION_CHECK_BUDGET` = 16 秒。
  - 没连上时保留上一次的结果：这次没问到 trace 而以前查到过，记录原样留着（`checked_at` 还是上次查到的时间），只加 `failed_at`、`fail_count`（连续失败次数）；下次连上写新记录，这两个字段就没有了。从来没查到过的写 `loc: null`。本机设置（`env`、`env_at`）每次都更新。`region_view` 多给 `failed_at`、`fail_count`，界面据此写“最近一次没连上 · 上面是上一次查到的”。
  - `net` 是 `net_fingerprint()`：默认路由用的本机 IPv4 地址和 IPv6 地址的前 64 位（UDP 的 connect 只让系统选路，不发数据包；链路本地地址 fe80::/10 哪张网都是它，不算）合起来的 sha256 前 12 位，只在本机比较；两个版本读写的是同一个字段，算出来的必须一样，计算部分 `net_fingerprint_of` 两边按 `tests/shared/region_check.json` 里的同一份数据测。换 Wi-Fi、插拔网线、开关接管整机流量的 VPN 时会变。
  - `check_region(max_age)` 这次跳不跳过看 `region_check_fresh`：`max_age` 秒内查到过且没换网络就跳过；上次没连上的话只看离那次失败有没有到 `REGION_RETRY_SECS` = 60 秒。两次定时检测之间，`background_round` 每轮问一次 `region_recheck_due`：本机换了网络（`NET_RECHECK_SECS` = 30 秒内最多补测一次），或者上次没连上、到了重试的时候（`region_retry_wait`：60 秒起每连着失败一次翻一倍，最多翻四次，最长不超过定时检测的间隔），就 `check_region(max_age=0)` 补测一次。这三条规则和下面的 `region_fail_item` 两个版本跑同一份 `tests/shared/region_check.json`。切换账号时也用一小时内的缓存给目标账号记一笔。
- `risk_items(data)` 不联网，只看缓存和本机设置（官方状态页那条带 `notice`、不扣分；hosts 文件改了 Claude 的域名是 warn）：出口不在名单（warn）、24 小时内出口变化（当前出口为支持地区 → info、不扣分，否则 warn；网络探测不等于账号登录）、两个接口看到的国家不一致（warn）、IP 类型为 Tor（warn）/ 代理（warn）/ 机房（数据库标记，或没有标记时按 `HOSTING_WORDS`，warn）、注册国与定位国不同（info）、IPv4 / IPv6 出口地区不同（warn）、本机 DNS 是某个国家的公共 DNS 且与出口不同（info、不扣分；只读配置，`KNOWN_DNS`，TUN / 代理可能接管解析，不能据此判断实际 DNS 出口、泄漏或所在地）、WARP（info）、时钟偏差超过 120 秒（warn）、官方状态页有故障（`status_risk`：有 `components` 时只看有关的组件——部分 / 严重故障 bad，性能下降、维护中或者只有没解决的事件 warn，别的组件出故障不算；没有 `components` 的旧记录按整页 `indicator`：minor → warn，major / critical → bad。规则两边按 `tests/shared/status_page.json` 跑）、系统时区指向的国家和出口不一致（指向不支持地区 → warn，否则 info；时区来自 `TZ` / `/etc/localtime` / `tzutil`，国家来自 tzdata 的 `zone1970.tab`）、系统语言 / 区域指向的国家和出口不一致（一律 info、扣 5 分：语言只代表语言习惯；来自 `AppleLocale` / `Get-Culture` / `LANG`）、当前账号 7 天内多地区使用（warn）、本机有 2 个以上 Claude Code 账号（info）。`/api/state` 的 `risks` 就是它，三个界面把 warn / bad 常显在顶部，`cam`、`cam switch` 和切换接口也会带上。缓存旧了（`region.checked_at` 超过 `REGION_FRESH_SECS` = 一小时，后台没在跑时会这样）：从那次检测算出来的每条后面标“（N 天前检测的）”，末尾加一条 `env_stale`（info，不扣分）说怎么重新检测；`status_page` 自带 `checked_at`，超过一小时的故障不再提；`note_age=False`（离线体检，开头已经说了时间）不标。最近一次检测没连上（`region.failed_at`）：出口还按上一次查到的算，另加一条 `region_fail_item`——那份结果还在一小时内、或者那个出口本来就不在名单（已经提醒并扣过分了），是 `env_failed`（info，不扣分）；超过一小时是 `region_unknown`（warn，扣 30：出口现在是哪里说不准）；有这一条就不再加 `env_stale`。后台一轮（`background_round`）里哪一步出了错都记进 `last_error`，同一轮后面的刷新成功也不清，等下一次整轮都没出错才清，`cam doctor` 的“后台上一轮出错”才看得到
- 评分 `risk_score(items)`：满分 100，按 `RISK_WEIGHTS` 的 (kind, level) 扣分（出口不在名单 60、Tor 出口 40、代理 IP 25、时区指向不支持地区 25、机房出口 20、账号多地区 20、切换到不支持地区的出口变化 15、两接口不一致 15、IPv4 / IPv6 不同 15、系统 DNS 配置差异和当前出口已在支持名单内的网络变化不扣分、系统语言不一致 5、时钟 10、VPN 10、api 连不上 40……没列出的 warn 扣 10、bad 扣 30），档位 ≥90 low / 70–89 medium / <70 high。`/api/state` 的 `risk_score` 据缓存的 `risks` 算；服务检查的第一项 `summary` 带实测的 `score`、`grade`，`level` 取 bad / warn / ok 中更严重的（档位 high → bad，medium → warn）。只是把提醒汇总成一个数，不拦任何操作。
- 关注项和扣分规则：条目带 `notice: true` 的是关注项，`risk_points` 给 0 分：官方状态页的公告（`status`）、`api` / `web` 回 429（限流）、529（过载）、5xx（服务端错误）。这些是 Anthropic 那边的状况，不是这台电脑环境的风险，级别照旧（注意 / 异常）、界面标“只提醒，不扣分”；存进 `service_status` 再回到 `risk_items` 时标记带着。服务检查的总体结论把两边分开：本机这边（不带 notice 的）有问题时说最严重的一条，那边也有状况就接在后面“；另外 Anthropic 那边有状况（只提醒，不扣分）：…”；本机这边没问题而那边有状况时写“本机环境这边…没有发现风险点。Anthropic 那边有状况（只提醒，不扣分）：…”，级别是 warn。当前登录已过期（`api` 回 401）是账号的事，级别 info、不扣分。`RISK_RULES` 是给人看的说法（每条对应 `RISK_WEIGHTS` 里扣分的一项），`risk_rules()` 给 `{rules: [{points, text}], notices: [...]}`，`/api/state` 的 `risk_rules`、面板评分卡底下的“扣分规则”、`cam risk --rules` 都是它，两边按 `tests/shared/risk_rules.json` 对：表里列的和实际扣的是同一份。
- 域名有没有被动过手脚（`tests/shared/hosts.json`）：`local_env()` 的 `hosts` 是 `parse_hosts` 从系统 hosts 文件里找出的、指到 `CLAUDE_DOMAINS`（`anthropic.com`、`claude.ai`、`claude.com`、`claudeusercontent.com` 和子域名，官方文档里 Claude Code 要访问的 Anthropic 自己的域名）的条目 `[{host, ip}]`（同一个域名只留第一条，最多 8 条，测试用 `CAM_TEST_HOSTS`），有的话 `risk_items` 出一条 `hosts`（warn，20 分）。服务检查的 DNS 实测（`resolve_host`）：解析不了但这个域名的探测连得上（走代理）是 info；解析成内网 / 保留地址（`addr_special`）是 info；解析不了又连不上才是 bad（20 分）；域名本身是 IP 时不做这些判断。结果后面带解析用的毫秒数。都只读，不改任何东西。
- 这张网安不安全（公共场所连网）：
  - Wi-Fi 加没加密（`tests/shared/wifi.json`）：`local_env()` 的 `wifi` 是 `wifi_info()`：`{ssid, security: "open" | "wep" | "wpa"}`，有线、没连、读不出来是 null；同一张网（`net_fingerprint`）上 `WIFI_REUSE_SECS` = 600 秒内不重查。三个系统各一个解析函数：`parse_nmcli_wifi`（`nmcli -t -f ACTIVE,SSID,SECURITY dev wifi`，`LC_ALL=C`）、`parse_airport_json`（`system_profiler SPAirPortDataType -json` 的 `spairport_current_network_information`）、`parse_netsh_wlan`（`netsh wlan show interfaces`：各行的名字跟着系统语言变，所以看 SSID 那行后面 6 行以内第一个 `wifi_security_class` 认得出的值，不往后猜到配置文件名上）。Windows 上 netsh 的输出放在合并的那次 PowerShell 查询里（`WIN_ENV_PS` 的 `wlan`，脚本里转成 base64：中文系统的代码页过管道不会乱）。open / wep 时 `risk_items` 出一条 `wifi`（warn，5 分：公司、商场要网页登录的 Wi-Fi 也是 open）；`region` 视图带 `wifi`，界面据此写“Wi-Fi 已加密”。测试用 `CAM_TEST_WIFI="open:名字"`。
  - 明文网页有没有被改（`tests/shared/service.json` 的 `plain_http`）：`check_region` 里多一件 `plain_http_check()`（`lookup_mode` 是 off 时不做）：`plain_http_probe` 直连 `PLAIN_HTTP_URL`（默认 `http://cp.cloudflare.com/generate_204`，`CAM_PLAIN_HTTP_URL` 可换；不走代理、不跟着跳转，Rust 版自己拼一个最简单的 HTTP 请求），`plain_http_verdict(状态码, Location, 正文开头)`：空的 204 是 `{ok: true}`，跳走 / 有内容 / 别的状态是 `{ok: false, got}`；记在 `region.plain_http`，没测、没连上就去掉这一项（不留上一次的结论）。`ok` 为 false 时出一条 `http_tamper`（warn，10 分）；`region` 视图的 `plain_http` 是 true / false / null；“会访问的服务”里列着这个地址。
- 连不上时卡在哪一层（`stuck_stage`，`tests/shared/service.json`）：服务检测在没走代理时另外直连一下 `api.anthropic.com`、`claude.ai` 的端口（`port_open`，只建 TCP 连接，`TCP_PROBE_SECS` = 5）；`api` / `web` 连不上时在文字后面接一句：走代理 → “问题在代理这一段”；解析不出来 → “卡在域名解析”；TCP 没连上 → “卡在建 TCP 连接（原因）”；TCP 通、TLS（`peer_issuer` 拿没拿到证书）不成 → “卡在 TLS 握手”；都通 → “卡在等服务端回应”。
- Anthropic 几处地址看到的出口（`tests/shared/security.json` 的 `domains`）：`detect_region` 在出口查到后再问 `domain_trace_urls()`——`API_BASE` 和 `TOKEN_URLS[0]` 的 `/cdn-cgi/trace`（和 `claude.ai` 或彼此是同一个主机端口的不重复问；off 模式也问，它们是 Anthropic 自己的地址），和运营商那几项一起并行、同一个时限，结果记进 `region.domains`（`[{host, ip, loc}]`，没问到地区的不记）。`risk_items`：和 `claude.ai` 的 `loc` 不一样的列成一条 `domain_region`——`claude.ai` 的在支持名单、这几处有不在的是 bad（60 分），否则 warn（15 分）；`region_view` 带 `domains`（每个加 `status`）；服务检查里都一样时多一行 ok。进度卡多一步 `domains`。
- 支持地区（`tests/shared/regions.json`）：`SUPPORTED_REGIONS`（185 个）、`REGION_DEPENDENCIES`（支持国家的海外属地，按所属国家算；香港、澳门不在内）、`REGIONS_UNSUPPORTED`（卡上列的 14 个）、`REGION_NOTES`、`REGIONS_CHECKED`（对着官方名单核对的月份）。`regions_info()` 给 `{supported, dependencies, unsupported, notes, checked, url}`，进 `/api/state` 的 `regions`；面板和桌面版的“支持地区”卡用它，地区的中文名在浏览器里用 `Intl.DisplayNames` 取（几个常说的按 cam 的叫法），搜索只在前端做。`cam regions [名字或代码]` 打印同样的内容（只有脚本版）。出口在乌克兰时服务检查的出口那行补一句被占领地区的说明（`UA_REGION_NOTE`）。
- 安全项（`tests/shared/security.json`）：都在 `local_env()` 里查、和出口一起缓存，只看、不改、不显示内容。`history_keys`：`history_files()`（bash / zsh / fish / PowerShell 的历史、`HISTFILE`、`~/.zsh_sessions/*.history`；测试用 `CAM_TEST_HISTORY`）每个文件读最后 4 MB，`scan_keys` 数带 `sk-ant-种类[0-3 位数字]-至少 16 位` 的行数和种类（`KEY_KINDS`），记 `[{path, count, kinds}]`，`risk_items` 出 `history_key`（warn，5 分，建议里给清理命令）。`perm_bypass`、`auto_run`：`settings_security` 看 `settings.json`、`settings.local.json` 和托管设置里的 `permissions.defaultMode == "bypassPermissions"`（`perm_bypass`，warn，5 分）和钩子（`hook_name` 跳过解释器、选项、`X=1`，只取脚本名；`HOOK_MARK` 认出的 cam 自己的钩子不算），`mcp_servers` 看 `~/.claude.json` 的用户级和项目里的 MCP 服务器，合成 `auto_run`（info，不扣分）。`config_mode`：`~/.claude.json` 组或其他人有权限时（POSIX）出 `config_perm`（info，不扣分）。`remote`：`process_names()`（POSIX `ps -A -o comm=`，Windows 用合并的进程表；测试用 `CAM_TEST_PROCS`）经 `remote_apps` 对 `REMOTE_APPS`（Linux 截到 15 个字的也认），出 `remote_app`（info，不扣分）。`lan`：`lan_exposed()` 的候选是 `desktop.json` 里开着内置面板和局域网访问的端口、进程表里带 `--lan` 的 `cam serve`、设在本机的代理端口，拿本机局域网地址各连一下（`LAN_PROBE_SECS` = 1 秒；测试用 `CAM_TEST_LAN_IP`，这时不看进程表），连得上就记下；面板出 `panel_lan`（Wi-Fi 是 open / wep 时 warn 5 分，否则 info），代理出 `proxy_lan`（info，不扣分）。评分卡上这些和 `creds_perm` 归成“安全”一行。
- 登录凭据文件的权限：`local_env()` 的 `creds_mode` 是 `creds_file_mode()`（POSIX 上 `.credentials.json` 的权限里组或其他人有任何一位就返回 "644" 这样的字符串，只有自己能读、没有这个文件、Windows 都是 null），有的话 `risk_items` 出一条 `creds_perm`（warn，15 分，建议里给 `chmod 600`）。只看，不改。出问题时的现场（`note_incident` 的 `env`）也记 `hosts`、`creds_mode`，诊断报告的环境那一行跟着写。
- claude 被别名 / 同名函数包了一层（脚本版，`claude_wrap_findings`，归在 `env_findings`）：扫 `SHELL_FILES` 和 PowerShell 配置里的 `alias claude=…`、`claude() {…}`、`function claude…`、`Set-Alias / New-Alias claude`（`CLAUDE_WRAP_RE`，注释掉的不算），只说在哪个文件第几行、不显示内容（可能带 Key）；指到 `cam claude` 的不提（函数看到右花括号为止）。级别 info、没有 fix：一键清理把它列在“要你自己处理的”里但不动它，也不算“让 Claude Code 不用订阅登录的设置”——很多是自己加的（自己的项目、常用参数）。
- 分数为什么变：`risk_points(item)` 是一条扣几分，`/api/state` 的每条 `risks` 带 `points`。`note_risk(store)` 在 `check_region`（重测了、没到时间直接返回的两条路都调）和 `service_check` 做完后调（服务检测一开始调的那次 `check_region(note=False)` 不记，等它全测完一起记，所以点一次“重新检测”只记一笔）：`risk_snapshot(data)` 取现在的分数和扣分的那几条（`risk_items(note_age=False)`，文字过一遍 `strip_ago` 去掉“（3 分钟前）”“（刚刚）”），和 `data["risk_log"][0]` 按分数 + (kind, level) 的集合比（`risk_same`，文字里的毫秒数、运营商名变了不算），不一样就在最前面记一笔 `{at, score, grade, exit: {loc, ip, org}, items: [{kind, level, text, points}]}`（`exit` 是当时的出口），最多留 `RISK_LOG_KEEP` = 30 笔，第一笔是开始记的时候；没变不加锁不写盘。`risk_log_view(data)` 给界面：`{since, changes: [{at, score, grade, prev, added, removed, exit_note}]}`，新的在前、最多 `RISK_LOG_SHOWN` = 8 次，`added` / `removed` 是和变之前比多出来、没有了的扣分项（`risk_diff`，同一种有几条按条数比），`exit_note` 是这两次之间出口换了的那一句（`exit_note(prev, cur)`：地区变了写“出口从 A 换到了 B”，地区没变 IP 变了写两个 IP，运营商变了带上运营商；没换是空的）；现在的分数和最后一笔不一样（后台还没来得及记）时最前面补一条 `at` 为 `null` 的。`/api/state` 的 `risk_log` 就是它，环境页评分卡下半截“分数变化”、账号页顶部提醒里的“从几分降下来”、卡片小面板分数后面的 ↓ 都用它；`cam risk` 打印同样的内容，`cam diag` 的报告里带全部 30 笔。`clear_region_history`（`cam risk --clear-history`、面板“清除地区记录”）一起清掉。只记录，不干预任何操作。两边规则见 `tests/shared/risk_log.json`。
- 服务检查另外实测：DNS、`api.anthropic.com`（有登录就用当前登录）、`claude.ai`（不带登录，识别人机验证页）、代理（环境变量 + `scutil --proxy` / 注册表 `Internet Settings` / GNOME `gsettings`）、VPN（默认路由的接口名或适配器描述像隧道：`route -n get default` / `ip route show default` / `Get-NetRoute`）。`device` 列出系统和架构（设备环境信息只列出，不改动、不伪装）。未配置显式代理不代表直连，默认路由未检出隧道也不代表 TUN 未开启（更具体的路由可能接管流量）。最后一条 `scope` 说明检测范围：只有本机能看到的信号，账号侧的信号看不到，所以“没有发现风险点”不等于一定合规。
- 测试钩子：`CAM_STATUS_URL`、`CAM_CF_META_URL`、`CAM_IPINFO_URL`、`CAM_IPAPI_URL`、`CAM_IPAPI_IS_URL` / `CAM_IPAPI_IS_KEY`、`CAM_RDAP_URL`、`CAM_CF_TRACE4_URL` / `CAM_CF_TRACE6_URL`、`CAM_IPINFO_TOKEN`、`CAM_IP_LOOKUP`、`CAM_META_TTL`、`CAM_NO_BG_CHECK`、`CAM_TEST_DNS`、`CAM_CLAUDE_WEB_BASE`、`CAM_TEST_TZ="Asia/Hong_Kong=HK"`、`CAM_TEST_LOCALE="zh_CN"`、`CAM_TEST_ROUTE_IF="utun4"`。

## HTTP API

所有请求需要 `Authorization: Bearer <server-token>`（或首次访问 `?t=<token>`）。超过 1 KB 的回复（页面、`/api/state` 等）在请求头 `Accept-Encoding` 有 gzip（q 不是 0）时用 gzip 压缩，带 `Content-Encoding: gzip`、`Vary: Accept-Encoding`（`accepts_gzip`；桌面版的服务用 engine 的 `web::accepts_gzip` / `web::gzip`）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | Web 面板。页签记在地址 `#` 后面：`#accounts`、`#usage`、`#sessions`、`#env`、`#settings`，旧的区块锚点（`#env`、`#local-block`、`#rules-block`、`#privacy-block`、`#update-block`、`#menubar-block`、`#help-block`）对到所在的页签 |
| GET | `/mini` | 卡片小面板（菜单栏 / 托盘点开的那个），“额度 / 用量 / 会话”三个页签 |
| GET | `/flags.woff2` | 国旗字体（Twemoji 国旗，CC-BY 4.0），不用令牌、不 gzip、`Cache-Control: public, max-age=86400`（桌面版的服务没设缓存）。面板开头在 1×1 的画布上用白色、黑色各画一次瑞士国旗：彩色字形不跟着填充色变，两次一样且不是空白就是系统能显示国旗；不能（Windows 上的 Edge、Chrome，桌面版的 WebView2）才注入 `@font-face CamFlags`（`unicode-range: U+1F1E6-1F1FF`，只管国旗字符）并给 `<html>` 加 `flagfont`，`.flagfont .flag` 用这个字体。桌面版界面在 `desktop/src/flags.ts` 做同样的事，字体由 Vite 打包 |
| POST | `/api/open-panel` | 在这台电脑的浏览器里打开管理面板，返回 `{"ok", "url"}`；body `{"tab": "usage"}` 打开到那一页（只认 `PANEL_TABS` 里的），别的值不带。只有脚本版有 |
| GET | `/api/state` | 账号列表（不含密钥）、当前账号、服务器信息；每个账号带 `health`（`level`: ok / info / warn / error，`label`: 正常 / 注意 / 异常，`issues`: [{`kind`, `level`, `text`, `advice`, `detail`}]，由 `account_health` 根据有没有保存凭据、用量报错和到期日算出，两边规则见 `tests/shared/health.json`）；`warning` 是 `live_mismatch` / `live_unverified` 的说明，`pending_switch` 是登记中的等待切换（含 `message`），没有时为 `null`；`region` 是后台上次检测的出口地区（`loc`、`status`: supported / unsupported / unknown / null、`message`、`advice`、`warp`、`prev_loc`、`changed_at`、`org`、`city`；`failed_at`、`fail_count`：最近一次检测没连上的时间和连续次数，这时其它字段是上一次查到的），`risks` 是据缓存算出的风险提示列表（同 `/api/service-check` 的条目格式，含 `kind`，每条带 `points`：从评分里扣几分），`risk_score` 是据它算的评分（`score`、`grade`: low / medium / high、`label`、`advice`），`risk_log` 是评分最近的变化（`since`、`changes`: [{`at`, `score`, `grade`, `prev`, `added`, `removed`, `exit_note`}]，见上面“分数为什么变”），前端把 warn / bad 的常显在顶部、顶栏显示分数；都只用于提示 |
| POST | `/api/accounts` | `{"kind":"claude_code"}` 导入当前登录；`{"kind":"claude_web"/"claude_desktop"/"api","label",…}` 新增 |
| POST | `/api/refresh` | 刷新全部用量；body `{"id": "…"}` 只刷新一个。算用户主动刷新，会替过期的未生效账号续登录 |
| POST | `/api/accounts/{id}/switch` | 切换 Claude Code 账号。body 可带 `{"mode": "now"|"wait"}`：有会话在跑且没给 `mode` 时返回 `{"ok": false, "choice_required": true, "running_claude", "message"}` 让前端问用户；`wait` 登记等待并返回 `deferred: true`；`now` 或没有会话时直接切，返回 `changed`、`account`、`running_claude`、`message`、`backup`。每种返回都带 `risks`（warn / bad 的风险提示），只提醒不拦 |
| DELETE | `/api/pending-switch` | 取消登记中的等待切换，返回新的 state |
| GET | `/api/privacy` | 外部 IP 查询设置：`ip_lookup`（auto / ip-api / off）、`modes`、`env_override`、`has_ipapi_key`、`has_ipinfo_token`（不返回 key 本身）、`meta_ttl_hours`、`sources`（当前设置下会访问的服务和各发了什么）、`history` |
| PUT | `/api/privacy` | body 可带 `ip_lookup`、`ipapi_key`、`ipinfo_token`（空字符串删除），返回同 GET |
| DELETE | `/api/privacy/history` | 清除缓存的出口信息和各账号的使用地区记录，返回同 GET |
| GET | `/api/menubar` | 菜单栏小工具的设置（卡片小面板和管理面板的“设置”用；只有 Python 版有，桌面版没有菜单栏小工具）：`available`（macOS 或设了 `CAM_SWIFTBAR_DIR`）、`tray`（Windows：设置页显示“托盘图标”，只有样式能选；测试用 `CAM_TRAY_TEST=1`）、`installed`、`mode`（classic / stream）、`icon`、`text`、`confirm_switch`、`hooks`、`plugin` |
| PUT | `/api/menubar` | body 可带 `icon`（bars / lamp / ring / rings / twobar，Windows 托盘还有 badge）、`text`（列表，只能是 name / five / seven / sessions，按 `MENU_TEXT` 的顺序存，可以是空列表）、`confirm_switch`、`hooks`、`mode`（重跑 `extras/swiftbar/install.sh` 并重开 SwiftBar），返回同 GET 加 `ok`、`restarting` |
| GET | `/api/checkup` | 全面体检，同 `cam doctor --json`（格式见“全面体检”），要十几秒；`?offline=1` 不联网；`?run=随机串` 记进度（见 `/api/progress`）；`?probe=1` 只回 `{"ok": true}`，面板据此显示按钮。只有脚本版有，桌面版返回 404 |
| GET | `/api/claude-version` | 面板的 Claude Code 版本卡片：`installed`（`path`、`kind`、`version`，实际运行的那份）、`latest`（`channel`、`version`、`checked_at`）、`outdated`、`can_update`、`advice`。装的版本按程序文件修改时间缓存，最新版本 6 小时查一次（`?refresh=1` 现查），存在 `claude-version.json`。只有脚本版有 |
| POST | `/api/claude-update` | 对实际运行的那份跑 `claude update`（只认官方安装器和 npm 装的），返回 `message` 加同 GET 的字段 |
| GET | `/api/claude-uninstall` | 卸载前给人看的：`targets` 每一份的 `path`、`kind`、`how`（怎么卸）、`auto`（cam 能不能替你卸） |
| POST | `/api/claude-uninstall` | `{"reinstall"?}`：同 `cam env uninstall --apply`（有 claude 在运行或看不出来时拒绝），reinstall 时卸完用官方安装器装一份；返回 `done`、`failed`、`manual`、`left`、`installed_ok`、`version` |
| POST | `/api/claude-install` | 没装时用官方安装器装一份，返回 `message`、`version` |
| GET | `/api/env-purge` | 彻底清理前给人看的：`move`（会移走的路径和大小）、`env`（接着会清掉的别处的中转设置，每条 `text`、`admin`）、`keychain`、`restore_at`（有能恢复的上一次）、`running`、`backups`（以前彻底清理留下的备份：`count`、`size`） |
| POST | `/api/env-purge` | `{"purge": true, "confirm": "彻底清理"}`：同 `cam env clean --purge`（有 claude 在运行或看不出来时拒绝），返回 `backup` 和别处中转设置清理的 `done`、`failed`；`{"restore": true}`：同 `cam env clean --restore`，返回 `restored`、`kept`、`failed`、`notes`；带 `drop_backups: true` 同时把以前留下的备份删掉，`no_backup: true` 这次的也不留（`env_purge_run`：先照常搬走，再 `env_purge_drop_backups` 删；返回多 `dropped`: {`removed`, `freed`, `left`} 和 `kept_backup`，没留备份时 `backup` 是 null）；`{"drop_backups": true, "confirm": "删除备份"}` 只删备份、不清理。`env_purge_drop_backups` 只删 `env-backups/` 正下方、`env_purge_backups` 认得的真目录（是链接的、不在那下面的不碰），连同那一份第二步留的 `-fix` 备份；一键清理自己的 `-fix` 备份不归它管 |
| GET | `/api/env-fix` | 一键清理要改什么（同 `cam env fix --json`）：`fix` / `optional`（官方支持的用法和托管设置，勾上才改）/ `manual` 每条 `level`、`text`、`advice`、`slow`（有 claude 在跑时先跳过的）、`admin`（要管理员权限，Windows 上会弹一次确认）；`running`、`undo_at`（有能撤销的上一次清理）。只提示，不打分。`?probe=1` 只回 `{"ok": true}`；只有脚本版有，桌面版 404，面板据此隐藏按钮 |
| POST | `/api/env-fix` | `{"apply": true, "force"?, "optional"?}`：当场重新算一遍再改（不信请求里的清单），返回 `done`、`failed`、`skipped`、`backup`、`next`；`{"undo": true}`：撤销上一次，返回 `restored`、`failed`、`notes` |
| GET | `/api/service-check` | `{"items": [{"kind", "level": "ok"|"bad"|"warn"|"info", "text", "advice"}]}`：第一项是总体结论（`summary`，带 `score`、`grade`），然后是 `dns`、`region`（对照 Anthropic 官方支持名单）、`api`、`web`（429 / 529 单独说是官方限流 / 过载）、`auth`（续登录的地址 `TOKEN_URLS` 各 GET 一次，`auth_item` 汇总：都连得上 ok，有的连不上 warn，都连不上 bad）、`tls_chain` / `tls_intercept`（`peer_issuer` 只握手看 api.anthropic.com 的证书颁发者，有 http 代理时用 CONNECT 打通道，`der_issuer` 从 DER 里读组织名和通用名，`tls_issuer_item` 对照 `PUBLIC_CA_WORDS`；不是公共证书机构就是网络里有设备在解密 HTTPS，不扣分）、`proxy`（环境变量 + 系统代理）、`vpn`（默认路由是否走隧道）、`proxy_down` / `proxy_up`（`proxy_endpoints` 从代理设置里取主机和端口，最多 4 个，`port_open` 各连一下；连不上的记下 `host`、`port`，后台 `check_region` 的 `proxies_back` 再连上了就把它从 `service_status` 清掉），再加上据缓存算出的风险项 `region_change`、`warp`、`clock`、`status`（官方状态页）、`timezone`、`account_regions`、`accounts`，每项带建议；同 `cam doctor` 的服务检查，要几秒钟，并会顺便刷新 `region` / `status_page` 缓存。解析域名、出口地区、api.anthropic.com、claude.ai、续登录的地址、证书、代理端口同时做，各有时限（解析 `SERVICE_DNS_SECS` = 5 秒，连接 `SERVICE_PROBE_SECS` = 10 秒，证书每次连接 `TLS_PROBE_SECS` = 6 秒，代理端口 `PROXY_PROBE_SECS` = 3 秒），`proxy_endpoints`、`auth_item`、`der_issuer`、`tls_issuer_item` 和新条目的扣分两边按 `tests/shared/service.json` 跑；`api`、`web`、`dns`、`tls`、`auth`、`proxy_down`、`tls_intercept` 有问题的记进 `service_status`（`SERVICE_STATUS_KINDS`），顶部环境卡一起列；哪件卡住了都不拖着别的，到点没答的写“超过 N 秒没有回应”；出口这次没连上而以前查到过时，`region` 那行照旧给上一次的并注明“这次没连上，这是…查到的”，后面跟一条 `env_failed` 或 `region_unknown`（和 `/api/state` 的 `risks` 同一条，两边分数一致）。只检测和提示，不干预任何操作。带 `?run=随机串`（字母数字、`_`、`-`，4–40 位）时把这次检测的进度记下来，见 `/api/progress` |
| GET | `/api/progress?run=…` | 那次“重新检测”/“全面体检”做到哪了：`{"steps": [{"key", "label", "state", "ms", "note"}], "done", "ms"}`，`state` 是 wait（在等）/ run（在做）/ ok（做完，`ms` 是用了多久）/ fail（没连上、没查到，`note` 写原因）/ timeout（超过时限没等到）/ skip（这次不用做，`note` 写原因）。只有步骤名和状态，不带检测到的内容；只留最近 6 次，没有这次返回空的 `steps` |
| POST | `/api/accounts/{id}/open` | 打开该网页账号的独立浏览器，或退出并重新打开目标 Claude Desktop 配置 |
| POST | `/api/accounts/{id}/identify` | 尝试从对应登录资料补全邮箱，Desktop 会核对当前账号 UUID |
| GET / POST | `/api/vault`、`/api/vault/*` | 本地加密账号库、邮箱授权和验证码；完整接口见 [账号库与邮箱设计](VAULT_API.md) |
| PATCH | `/api/accounts/{id}` | 修改 `label` 和 `manual.*` |
| DELETE | `/api/accounts/{id}` | 删除账号（凭据移入 backups，不直接销毁） |
| GET | `/api/local-usage?days=30` | 按天、按账号汇总本地 token 消耗（`cam local`、额度窗口对照） |
| GET | `/api/local-usage?range=today\|7\|30\|all` | 管理面板“本机 Claude Code 消耗”：柱子、合计、明细、热力图（格式见“本机 Claude Code 消耗”）。只读 |
| GET | `/api/sessions` | 会话页：这台电脑上正在跑的 Claude Code，一个会话一行（格式见“会话页”）。只读 |
| GET | `/api/sessions/{id}` | 一个正在跑的会话的上下文组成（按记录估的，格式见“会话页”）；会话不在跑了返回 400。只读 |
| GET | `/api/usage-samples` | 7 天额度的节奏：每个有 7 天额度的号 `{"pace", "points"}`（格式见“7 天额度的节奏”），`?id=` 只要一个号。只读 |
| GET | `/api/rules` | `{"rules": [...], "alert_threshold": 90}` |
| PUT | `/api/rules` | 整体替换规则 / 设置提醒阈值，服务端校验 |
| GET | `/api/update` | **只有脚本版有**（桌面版返回 404，面板据此隐藏“软件更新”）。联网检查新版本（`git fetch`，最多 60 秒），返回 `checkout`（是不是 git 仓库）、`upstream`、`head`、`behind`、`ahead`、`commits`（[{`hash`, `date`, `subject`}]，最多 50 个）、`dirty`（没提交的改动）、`message`，同时存进 `update-check.json`。`?saved=1` 不联网，只返回上次的结果（代码在那之后变过就把 `behind` 当 0）；从没检查过时返回 `{"checked_at": null, "version"}` |
| POST | `/api/update` | 同 `cam update`：有未提交改动时拒绝（400，什么都不改）；`git merge --ff-only @{u}`；自检（`SHA256SUMS`、编译 `scripts/*.py`、临时 HOME 下跑 `cam --help` / `cam ls`），没过就 `git reset --hard` 回原来的提交并返回 400。成功返回 `updated`、`old`、`new`、`commits`、`message`、`restart`（哪些要重开；装了 `cam-serve` 服务、处理这个请求的又不是它时，回复之后一秒请 systemd 重启它）、`restarting`（代码换了，这个 cam serve 几秒内自己用新代码重启，面板等它回来整页刷新） |

返回统一为 JSON；错误为 `{"error": "…"}` 加相应状态码。
