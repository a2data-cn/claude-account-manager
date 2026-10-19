# Claude 账号管理

管理多个 Claude 账号：一键切换，查看每个号的 5 小时和 7 天额度、重置时间、到期日。支持桌面（macOS / Windows / Linux）、手机浏览器和命令行，三端共用同一份数据。

支持两类账号：
- **Claude Code（命令行）**：走 `api.anthropic.com`，能一键切换登录、自动读取额度。
- **网页版 claude.ai**：走 `claude.ai`，每个号一个独立浏览器窗口长期保持登录；填入一次 sessionKey 后也能自动显示额度。适合只能访问网页、无法使用命令行接口的环境（例如公司网络屏蔽了 `api.anthropic.com`）。

| | 脚本版 `cam` | 桌面版 | 手机 / 其它电脑 |
|---|---|---|---|
| 需要什么 | Python 3.8+，无第三方依赖 | 安装包 | 浏览器 |
| 切换 Claude Code 账号 | ✓ | ✓（托盘菜单一键切换） | ✓（切的是电脑上的账号） |
| 查看用量 / 到期 | ✓ | ✓（macOS 菜单栏直接显示 %） | ✓ |
| 自动发现新登录的账号 | `cam` / `cam serve` 运行时 | 后台常驻 | — |
| 时间规则自动切号 | `cam serve` 运行时 | 后台常驻 | 可设置 |
| 目录规则自动切号 | `cam claude` | 可设置（通过 `cam claude` 生效） | 可设置 |
| 额度提醒 | 终端输出 | 系统通知 | — |

## 菜单栏小工具（macOS，推荐）

菜单栏常驻显示当前账号和 5 小时额度，例如“工作号 64%”，超过 70% 变黄，超过 90% 变红。点开能看到每个号的 5 小时 / 7 天额度、重置时间和到期日，点一下就能切换。

```bash
bash extras/swiftbar/install.sh
```

脚本会自动安装 [SwiftBar](https://github.com/swiftbar/SwiftBar)（需要 Homebrew；没有的话会告诉你去哪下载），并把插件装好。插件每分钟运行一次，同时负责：
- 自动保存新登录的账号
- 执行时间规则
- 每 10 分钟刷新用量
- 额度超过阈值时弹系统通知

所以装了它，不开桌面版也是全自动的。

## 第一次使用（每个账号做一次）

工具不保存密码。Claude 登录要走邮箱验证或 Google 授权，所以每个号的第一次登录需要你自己完成：

1. 终端运行 `claude`，输入 `/login`，在浏览器里登录第一个账号
2. 运行 `cam`（或打开桌面版），它会自动发现这个账号并保存
3. 再次 `/login` 登录第二个账号，重复第 2 步，依此类推

完成后：
- **切换**：`cam switch 工作号`，或在桌面版 / 托盘 / 手机上点一下
- **用量**：后台每 10 分钟自动刷新
- **登录过期**：不常用的号 token 过期时会在后台自动续上
- **到期日**：官方没有接口，在"编辑"里填一次并勾选"自动续期"，之后每个周期会自动顺延

## 切换时不会丢的东西

切换只替换登录凭据（`~/.claude/.credentials.json`，macOS 是钥匙串里的 `Claude Code-credentials`）和 `~/.claude.json` 里的 `oauthAccount` 字段。以下内容都不动：

- 会话记录（`~/.claude/projects/`），切换后 `claude --continue` 照常接着聊
- `settings.json`、`CLAUDE.md`、MCP 服务器、插件、项目权限
- `~/.claude.json` 里的其它所有字段

每次切换前会先做两件事：
- 把当前账号最新的 token 回写保存。Claude Code 运行时会刷新 token，不回写的话下次切回来就失效了。
- 把切换前的现场备份到 `~/.claude-accounts/backups/`，保留最近 30 份。

正在运行的 `claude` 进程内存里还是旧账号，切换后需要退出重开。

## 脚本版

```bash
# macOS / Linux：把 scripts 加入 PATH（或者直接 python3 scripts/cam.py）
export PATH="$PATH:/path/to/claude-account-manager/scripts"

cam                       # 列出所有账号；顺便自动保存新登录的账号
cam usage                 # 刷新用量
cam switch 工作号          # 按名称 / 序号 / 邮箱 / id 切换
cam set 工作号 --expires 2026-11-03 --auto-renew --price '$100/月'
cam serve --lan           # 启动 Web 面板，手机扫链接即可访问
cam local --days 7        # 本机每天消耗了多少 token、记在哪个号上
cam doctor                # 找不到账号等问题时，逐项检查（不显示密钥）
```

Windows 用 `scripts\cam.cmd`，用法相同。

### 切号规则

```bash
# 时间规则：工作日 9 点到 18 点用工作号，进入时间段时自动切换
cam rule add-time 工作号 --days 1-5 --start 09:00 --end 18:00
cam rule add-time 个人号 --days 1-5 --start 18:00 --end 09:00   # 可以跨午夜

# 目录规则：在 ~/work 下用 cam claude 启动时自动用工作号（参数原样透传给 claude）
cam rule add-dir ~/work 工作号
cd ~/work/some-project && cam claude --continue

# 额度提醒阈值（当前号 5 小时额度超过 85% 时提醒，并告诉你哪个号余量最多）
cam rule alert 85
cam rule ls
```

时间规则只在"进入时间段"的那一刻切换，时间段内你手动换号，工具不会再改回去。

想让目录规则对所有 `claude` 命令都生效，可以在 shell 配置里加一行 `alias claude='cam claude'`。

额度满了自动换下一个号的功能**没有做**。用多个账号轮流规避额度限制违反 Anthropic 使用条款，是封号的主要原因。工具只负责提醒，换不换由你一键决定。

### 网页版和 API 账号

```bash
cam add-web --label 家里网页号     # 每个网页号一个独立的浏览器 Profile
cam open 家里网页号                # 打开它专属的窗口，第一次在里面登录，以后一直保持登录
cam add-api --label 项目A --key sk-ant-api03-... --admin-key sk-ant-admin01-...
```

**让网页账号自动显示额度（填一次 sessionKey）**

1. 在浏览器登录 claude.ai
2. 按 F12 打开开发者工具 → Application / 应用 → Cookies → `https://claude.ai`
3. 找到名为 `sessionKey` 的条目，复制它的值（以 `sk-ant-` 开头）
4. 填进工具：`cam set 家里网页号 --session-key <粘贴的值>`

之后 `cam usage` 会自动显示这个号的 5 小时 / 7 天额度，并读出邮箱和套餐。sessionKey 会过期（通常几周到一两个月），过期后工具会提示"sessionKey 已过期"，重复上面的步骤即可。

若 claude.ai 返回人机验证页，通常是网络被拦截（公司管控），和账号无关，工具会明确提示；这种情况下只能手动记录到期日。不填 sessionKey 也能用：每个号一键打开独立窗口，手动记录到期日并到期提醒。

API 账号填了 Admin Key 后会显示本月花费。

## 桌面版

位于 `desktop/`，基于 Tauri 2 + React。

```bash
cd desktop
npm install
npm run tauri dev      # 开发
npm run tauri build    # 打包，产物在 src-tauri/target/release/bundle/
```

需要先安装 Rust 和 Node 20+。Linux 还要装 `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev`。

**一键编译安装**：`bash scripts/build-desktop-mac.sh`（Windows：`powershell -ExecutionPolicy Bypass -File scripts\build-desktop-windows.ps1`）。脚本会自动安装缺少的 Rust，编译后把 App 装进“应用程序”。如果失败，把生成的 `build.log` 发给作者。

**不想本地编译**：把项目推到 GitHub，`.github/workflows/build.yml` 会自动编译 macOS（通用版）、Windows、Linux 三个平台的安装包，在 Actions 页面下载。推送 `v1.0.0` 这样的标签会生成 Release 草稿。

桌面版功能：
- 关闭窗口后缩到托盘继续运行。托盘菜单列出所有账号和额度，点一下即可切换。macOS 菜单栏直接显示当前账号的 5 小时用量百分比。
- 后台自动发现新登录的账号、执行时间规则、刷新用量，额度超过阈值时弹系统通知。
- 在"手机与设置"里开启 Web 面板后，手机扫二维码就能查看和切换。

macOS 首次打开未签名应用时，需要右键选择"打开"。

## 手机访问

在桌面版"手机与设置"里开启，或者在电脑上运行 `cam serve --lan`。手机和电脑连同一个 Wi-Fi，扫码或打开打印出来的链接即可。在浏览器菜单里"添加到主屏幕"后，用起来和 App 一样。

链接里带访问令牌，相当于密码。不在同一个网络时，建议在电脑和手机上都装 [Tailscale](https://tailscale.com) 这类组网工具，不要把端口直接暴露到公网。

## 数据与安全

- 所有数据存在 `~/.claude-accounts/`（可用 `CAM_HOME` 改位置），目录权限 700、凭据文件权限 600，不上传任何地方
- 网络请求只发往 `api.anthropic.com`（用量、资料）、`platform.claude.com` / `console.anthropic.com`（续登录），以及你主动填了 sessionKey 时的 `claude.ai`
- 用量和资料接口是 Claude Code 自己在用的接口，并非公开 API，Anthropic 改动后可能需要更新本工具；拿不到数据时界面会显示原因，不影响切换
- 删除账号时凭据会移到 backups 目录，不直接销毁

## 目录结构

```
scripts/cam.py        脚本版（单文件，可单独拷走使用）
scripts/test_cam.py   脚本版端到端测试（伪造 HOME 和接口，不碰真实账号）
scripts/build-desktop-*  桌面版一键编译脚本
extras/swiftbar/      macOS 菜单栏小工具
web/panel.html        Web 面板（手机 / 浏览器），脚本版和桌面版共用
desktop/              Tauri 桌面版（src-tauri/src/engine 是与 cam.py 对应的 Rust 实现）
docs/DESIGN.md        数据格式、切换流程、HTTP API
```
