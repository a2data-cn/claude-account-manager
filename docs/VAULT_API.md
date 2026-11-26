# 本地账号库与邮箱接入合同

账号库是现有账号记录的补充，不迁移或覆盖 `accounts.json`。同一份账号库由 Rust 桌面版和 Python 服务读取。收信方式：IMAP（邮箱密码 / 授权码 / 应用专用密码）、Gmail / Google Workspace 授权、Outlook / Hotmail / Microsoft 365 授权。按邮箱后缀自动识别收信方式和 IMAP 服务器的规则在 `web/mail-providers.json`（Python 运行时读，Rust 编译时嵌入），用例在 `tests/shared/mail_detect.json`。

## 存储与加密

文件：`CAM_HOME/vault.sqlite3`，目录 0700、数据库及日志文件尽可能 0600。SQLite busy timeout 5 秒，所有 SQL 使用参数绑定。连接按操作创建，不跨线程共享。

```sql
CREATE TABLE IF NOT EXISTS vault_meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS vault_entries (
  id TEXT PRIMARY KEY,
  label TEXT NOT NULL,
  email TEXT NOT NULL,
  provider TEXT NOT NULL,
  account_id TEXT,
  config_json TEXT NOT NULL,
  secret BLOB NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
```

`vault_meta` 中保存 `version`（字节 `1`）、`salt`（随机 32 字节）和 `check`（下述加密格式）。密钥通过 PBKDF2-HMAC-SHA256，600000 次、32 字节派生，主密码 UTF-8。`check` 明文为 `cam-vault-v1`，AAD 为 `cam-vault-check`。每条 secret 为 JSON 对象，AAD 为 `cam-vault-entry:<id>`。加密为 AES-256-GCM，BLOB 格式：ASCII `CAM1` + 12 字节随机 nonce + ciphertext + 16 字节 tag。主密码和派生密钥不落盘，解锁密钥只保存在各进程内存；重开或切换到另一后端需要重新解锁。锁定后清除该进程的密钥。不要启动时自动创建或读取真实用户的库做测试。

秘密 JSON 字段：`password`, `client_secret`, `access_token`, `refresh_token`, `expires_at`（Unix 秒）。配置 JSON 仅允许公开字段 `client_id`, `tenant`, `imap_host`, `imap_port`, `imap_username`, `notes`。secret 不出现在普通状态、日志或 URL 中，账号密码仅通过明确的 reveal 操作返回。

## HTTP API

所有路径沿用现有 API 的 Bearer 认证。返回 JSON；正常 200，参数/锁定等错误 400，错误正文 `{error: "说明"}`。

- `GET /api/vault` → `{initialized, unlocked, entries}`。锁定时 entries 为空。
- `POST /api/vault/setup`，`{master_password}`：主密码至少 8 字符，只能初始化一次，成功直接解锁并返回状态。
- `POST /api/vault/unlock`，`{master_password}`：解锁并返回状态。
- `POST /api/vault/lock`：锁定并返回状态。
- `POST /api/vault/entries`：创建或更新，正文 `{id?, label, email, provider, account_id?, config?, password?, client_secret?, clear_password?}`。provider 是 `credential|gmail|outlook|imap`。缺少的字段更新时保留；空密码表示保留，只有 clear_password=true 清除。返回完整状态。
- `DELETE /api/vault/entries/<id>`：只移除账号库条目，返回完整状态。
- `POST /api/vault/entries/<id>/reveal` → `{password}`，不返回 OAuth token 或 client secret。
- `POST /api/vault/entries/<id>/connect` → `{job_id, authorization_url, expires_at}`。仅 Gmail/Outlook。必须先解锁并配置 client_id；client_secret 可选。用户在系统浏览器打开 authorization_url。后端不自动打开浏览器。
- `GET /api/vault/oauth/<job_id>` → `{status: "pending|connected|error", message, entry_id}`。任务与本地数据目录绑定，不返回 token。状态可供前端每 2 秒轮询，最长 5 分钟。
- `POST /api/vault/detect`，`{email}` → `{provider, imap_host, imap_port, name, note, oauth}`：按后缀猜收信方式（不需要解锁，不联网）。认不出时 provider 为 `imap`、imap_host 为空；`oauth` 为 `gmail|outlook|null`，表示也可以改用浏览器授权。邮箱格式不对返回 400。
- `POST /api/vault/entries/<id>/messages`，`{since?: ISO8601, all?: bool}` → `{entry_id, checked_at, all, status: "found|empty", messages: [{id, sender, sender_name, subject, received_at, preview, official, login, codes: [string], verification_links: [string]}]}`。默认最近 15 分钟，最多回溯 7 天、最多 20 封候选邮件。`all=false` 只列明确来自 `anthropic.com` / `claude.ai` / `claude.com` 及其子域的邮件；`all=true` 列收件箱全部，但只从官方邮件里取正文找验证码，别人的邮件只有标题、发件人和服务商给的摘要（IMAP 只取信头）。`preview` 是开头约 160 字的纯文本。官方登录邮件提取 6–8 位验证码和 HTTPS 官方 Claude/Anthropic 域的验证链接（`login=true`）。不写入邮件内容，不请求验证链接，不发送、删除或标记已读邮件（IMAP 用 EXAMINE 和 BODY.PEEK）。
- `POST /api/vault/entries/<id>/message`，`{id}` → `{id, sender, sender_name, subject, received_at, official, login, codes, verification_links, html, text, truncated}`：点开一封信。`id` 只允许 `[A-Za-z0-9_=+/-]`，IMAP 为数字 UID。`html` 是原邮件的 HTML，后端去掉 script 整块和 meta / base / link / iframe / frame / frameset / object / embed / applet / form 标签，最多 2 MB；界面放进 `sandbox="allow-popups allow-popups-to-escape-sandbox"` 的 iframe（不许脚本、不同源），CSP `default-src 'none'; style-src 'unsafe-inline'`，图片和字体只在官方邮件或用户点“显示图片”后允许 `https:`，`<base target="_blank">` 让链接开新标签页。
- `POST /api/vault/entries/<id>/open-verification`，`{url}`：用户明确打开验证链接。绑定网页账号时使用其独立浏览器配置；绑定 Desktop 时要求对应配置正在运行，打开链接期间不退出或重启客户端；未绑定时使用本机系统浏览器。拒绝邮箱与绑定账号不符、非官方地址或锁库状态。
- `POST /api/accounts/<id>/identify`：尝试补全已登录账号的邮箱，返回现有完整 public_state。Desktop 只读本地身份配置及经过 URL 验证的 `/api/account` 缓存，不能猜测邮箱或把别号的缓存导入；无法识别时返回说明。Code/Web 可复用已有只读资料刷新。

公开账号库条目：`{id,label,email,provider,account_id,config,has_password,has_client_secret,connected,created_at,updated_at}`。`connected` 表示持有邮箱授权（OAuth refresh/access token 或 IMAP 密码），不代表已验证实时连通。

## 后端函数接口

Rust `vault` 模块：`status(store)`, `setup(store, body)`, `unlock(store, body)`, `lock(store)`, `save_entry(store, body)`, `delete_entry(store,id)`, `reveal_entry(store,id)` 均返回 `R<Value>`。供邮箱模块使用 `get_entry(store,id)->R<Value>` 返回私有条目（公开元数据加 `secret` 对象），`save_secret(store,id,secret)->R<()>` 合并秘密字段，`set_email(store,id,email)->R<()>`。Python `cam_vault` 同名函数与返回格式，失败抛模块 `VaultError`。

Rust `mailbox` 模块：`connect(store,id)->R<Value>`, `oauth_status(store,job_id)->R<Value>`, `messages(store,id,body)->R<Value>`, `message(store,id,body)->R<Value>`, `detect(email)->R<Value>`。Python `cam_mailbox` 同名函数，失败抛 `MailboxError`。通过主程序路由统一转换错误。Gmail / Outlook 条目没填 `client_id` 时，读邮箱前借同一种邮箱里第一个填过的（Outlook 连租户一起），只在内存里用，不写回账号库。

## 邮箱协议

OAuth 使用随机 state 和 PKCE S256，在本机 loopback 临时端口等待授权回调；回调只接受约定 path，必须校验 state，超时 5 分钟；交换 token 时传同一 redirect_uri 和 verifier。禁止记录授权 code/token。用户需要配置自己注册的桌面 OAuth 应用 ID，产品中给清楚配置提示，不使用第三方项目的客户端 ID。

Gmail / Workspace：`https://accounts.google.com/o/oauth2/v2/auth`，`https://oauth2.googleapis.com/token`，scope `https://www.googleapis.com/auth/gmail.readonly`，access_type=offline,prompt=consent；用 Gmail `/gmail/v1/users/me/profile` 获取真实邮箱并核对目标条目。邮件通过 Gmail API messages.list + messages.get(format=full) 只读读取。

Outlook / Microsoft 365：`https://login.microsoftonline.com/<tenant>/oauth2/v2.0/authorize` 和 `/token`，tenant 默认 common，只允许简单租户 ID/域。scope `offline_access User.Read Mail.Read`，公开客户端授权码 + PKCE。通过 Graph `/v1.0/me?$select=mail,userPrincipalName` 核对邮箱，再读取 `/me/mailFolders/inbox/messages`。企业管理员可能需要批准权限，原样提供可理解的授权状态，不改租户策略。

自动刷新 access token 时保留旧 refresh_token（服务未轮换时），失败要求重新授权。授权/刷新同一条目需互斥，不能让两个刷新互相覆盖。

IMAP：TLS 证书和主机名正常验证，默认端口 993；只读打开 INBOX（EXAMINE / readonly SELECT），FETCH BODY.PEEK 避免标记已读，限制候选数、超时和消息尺寸。不使用明文端口或关闭证书验证。微软个人邮箱可能拒绝密码登录（`AUTHENTICATE failed` / `BasicAuthBlocked`），这时提示改用 Outlook 授权；cam 不拿保存的密码去自动登录任何网页换授权。

## 验证

临时 SQLite 库、假的 token/mail API、假的 IMAP 会话；校验加密文件不存在明文密码、错误主密码/篡改密文失败、Rust/Python 格式互通、锁定保护、OAuth state/PKCE/超时、邮箱身份不符不保存、token 刷新不丢 refresh_token、无邮件写操作、发件人/URL/时效过滤、缺设置时明确提示。真实邮箱授权和真实 Claude 登录只由用户在产品中触发。
