# 设计说明

脚本版（`scripts/cam.py`）和桌面版（`desktop/`）读写**同一份数据目录**、实现**同一套 HTTP API**，所以两者可以混用：在桌面版里添加的账号，脚本里能直接切换，反过来也一样。Web 面板（`web/panel.html`）只认这套 API，不关心后端是 Python 还是 Rust。

## 数据目录

默认 `~/.claude-accounts/`，可用环境变量 `CAM_HOME` 覆盖。

```
~/.claude-accounts/
├── accounts.json          # 账号元数据（不含任何密钥）
├── secrets/<id>.json      # 每个账号的凭据，权限 600
├── browser-profiles/<id>/ # 网页版账号各自独立的浏览器 Profile（Cookie 永久保留）
├── backups/<时间戳>/      # 每次切换前的现场备份，保留最近 30 份
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
      "manual": {                             // 手动补齐的字段，优先级低于自动值（expires_at 例外，见下）
        "plan": null, "expires_at": "2026-11-03", "auto_renew": true, "price": "$100/月", "note": ""
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
  "switch_log": [ { "id": "a1b2c3", "at": "…" } ]  // 用来把本地 token 消耗归属到账号
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
"prefs": { "alert_threshold": 90 }
```

- 时间规则：星期 1 = 周一；`end` 早于 `start` 表示跨午夜，星期按开始那天算。多条重叠时取第一条。只在命中的规则发生变化（进入或离开时间段）时切换，所以时间段内手动换号不会被改回。
- 目录规则：`cam claude` 启动时按当前目录做最长前缀匹配，切换后再执行 `claude`。
- 额度提醒：当前账号 5 小时额度 ≥ 阈值时提醒一次（同一个额度窗口只提醒一次），并给出余量最多的账号。**不会自动切换。**
- 自动发现：后台检测凭据文件变化（macOS 每 30 秒查一次钥匙串），发现没见过的账号就自动导入。

## 三类账号

| 类型 | 切换方式 | 用量来源 | 到期来源 |
|---|---|---|---|
| `claude_code` | 替换 Claude Code 的登录凭据，聊天记录/项目/设置共用不动 | `api.anthropic.com/api/oauth/usage`（5 小时、7 天额度）+ 本地 JSONL 日志统计 token | 手动填写（官方无接口），可设"每月自动续期" |
| `claude_web` | 每个账号一个独立浏览器 Profile，"切换"就是打开对应窗口 | 可选填 `sessionKey`，走 claude.ai 内部接口；失败则只显示手动信息 | 手动填写 |
| `api` | 不需要切换，只做展示（可复制 Key） | Admin Key → Cost/Usage Report；普通 Key → 仅校验有效 | 无 |

## 切换流程（保证数据不丢）

1. **先快照**：读取当前正在生效的凭据（Linux/Windows 读 `~/.claude/.credentials.json`；macOS 读钥匙串 `Claude Code-credentials`，没有再读文件）以及 `~/.claude.json` 里的 `oauthAccount`。
2. **回写**：按 `accountUuid`/邮箱找到它属于哪个已保存账号，把最新 token 写回该账号（Claude Code 运行时会刷新 token，不回写的话旧 token 会失效）。如果是没见过的账号，自动导入为新账号，绝不丢弃。
3. **备份**：把凭据文件和 `~/.claude.json` 复制到 `backups/<时间戳>/`。
4. **写入**：原子写入目标账号的凭据，`~/.claude.json` 只替换 `oauthAccount` 字段，其它字段原样保留。
5. **记录**：更新 `active` 和 `switch_log`。

不动的东西：`~/.claude/projects/`（会话记录）、`settings.json`、`CLAUDE.md`、MCP 配置、插件。所以切换后 `claude --continue` / `--resume` 照常可用。

正在运行的 `claude` 进程内存里还是旧 token，切换后需要重启这些会话。

## Token 刷新策略

- **当前生效账号**：只读不刷新，由 Claude Code 自己刷新，避免两边抢 refresh token。
- **未生效账号**：access token 过期时用 refresh token 换新，新 token（含轮换后的 refresh token）立即写回 `secrets/<id>.json`。

## HTTP API

所有请求需要 `Authorization: Bearer <server-token>`（或首次访问 `?t=<token>`）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | Web 面板 |
| GET | `/api/state` | 账号列表（不含密钥）、当前账号、服务器信息 |
| POST | `/api/accounts` | `{"kind":"claude_code"}` 导入当前登录；`{"kind":"claude_web"/"api","label",…}` 新增 |
| POST | `/api/refresh` | 刷新全部用量；body `{"id": "…"}` 只刷新一个 |
| POST | `/api/accounts/{id}/switch` | 切换 Claude Code 账号 |
| POST | `/api/accounts/{id}/open` | 在电脑上打开该网页账号的浏览器窗口 |
| PATCH | `/api/accounts/{id}` | 修改 `label` 和 `manual.*` |
| DELETE | `/api/accounts/{id}` | 删除账号（凭据移入 backups，不直接销毁） |
| GET | `/api/local-usage?days=30` | 按天、按账号汇总本地 token 消耗 |
| GET | `/api/rules` | `{"rules": [...], "alert_threshold": 90}` |
| PUT | `/api/rules` | 整体替换规则 / 设置提醒阈值，服务端校验 |

返回统一为 JSON；错误为 `{"error": "…"}` 加相应状态码。
