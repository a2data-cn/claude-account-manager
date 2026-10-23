# 给 AI 助手的说明

这个仓库是 **cam**：在一台电脑上管理同一个人自己的多个 Claude 账号，切换 Claude Code 登录、打开各账号独立的 Claude Desktop 配置或网页窗口、查看额度、记录套餐和到期日。

- 脚本版 `scripts/cam.py`：只用 Python 3.8+ 标准库，命令行 `cam`，`cam serve` 提供网页面板 `web/panel.html`
- 桌面版 `desktop/`：Tauri + React，核心逻辑在 `desktop/src-tauri/engine`（Rust，和 cam.py 对应），内置同一个网页面板
- macOS 菜单栏插件 `extras/swiftbar/`
- 数据都在 `~/.claude-accounts/`（`CAM_HOME` 可改），格式和流程见 `docs/DESIGN.md`，用户说明见 `README.md`

## 在新电脑上装

- macOS / Linux：`bash install.sh --yes`；Windows：`powershell -ExecutionPolicy Bypass -File install.ps1 -Yes`
- `--yes` 按默认选择：装 `cam` 命令，macOS 装菜单栏小工具，Windows 装托盘小工具（没有 Python 时先用 winget 装）；不装 Claude Code（要装就运行 `cam env install -y`），不装 Linux 后台服务。后台服务要先问过用户再装（不加 `--yes` 重新运行，问到时回答 y）：这台电脑上可能有别的程序在用同一个 Claude Code 登录
- 要人来做的事：
  - 每个 Claude 账号在浏览器里登录一次：`cam login 名字`。走邮箱链接或 Google 授权，由用户自己完成，不要代劳，也不要想办法自动化
  - macOS 装菜单栏小工具时 Homebrew 可能要输密码；第一次读钥匙串时弹窗要点“始终允许”；第一次运行 python3 可能要装“命令行开发者工具”
- 更新：用户要求时运行 `cam update`（先 `cam update --check` 看改动）。不要设定时任务自动更新

## 验证

- `cam doctor`：全面体检（账号登录、Claude 服务、本机 Claude Code 和正在运行的会话、cam 自己、聊天机器人），不显示密钥。联网时会刷新用量、替不在用的号续快过期的登录（当前登录不碰）；只想看、不想改任何东西用 `cam doctor --offline`（`--json` 给脚本用）。`cam verify`：按 `SHA256SUMS` 校验代码文件
- 三套测试，都不碰真实账号：
  - `python3 -m unittest scripts.test_cam`（账号库和身份识别的测试一起跑：`python3 -m unittest discover -s scripts -p 'test_*.py'`）
  - `cd desktop/src-tauri/engine && cargo test`；改了 Rust 还要在 `desktop/src-tauri` 运行 `cargo fmt --all`（自动编译会检查排版，不对就失败）
  - `cd desktop && npx tsc --noEmit`
- 推到 GitHub 后 `.github/workflows/build.yml` 每次推送自动跑这三套测试（脚本版在三个系统上跑，引擎还跑 clippy，有警告就算失败）；桌面版安装包只在推 `v*` 标签或手动运行时编
- 改了 PowerShell 脚本（`install.ps1`、`extras/windows-tray/cam-tray.ps1`）：存成带 BOM 的 UTF-8（Windows PowerShell 5.1 读不带 BOM 的文件会把中文读乱）；双引号字符串里的中文引号要写成 `` `“ `` `` `” ``，不然 PowerShell 把它们当成引号，后面的字会丢。没有 Windows 时至少用 pwsh 的 `[System.Management.Automation.Language.Parser]::ParseFile` 查一遍语法
- 改了 `web/panel.html`：把里面的 `<script>` 取出来 `node --check`；改了样式运行 `python3 scripts/sync-styles.py` 同步到桌面版，不要手改 `desktop/src/styles.css`

## 规矩

- 不打印、不复制、不提交任何密钥：refresh / access token、sessionKey、API Key、`server-token`、账号库里的密码。排查用 `cam doctor`、`cam json`，它们不带密钥
- 不直接写 `~/.claude-accounts`、`~/.claude`、`~/.claude.json`，也不手动改钥匙串，只通过 cam 的命令改
- 测试里不要真的去续 token：用 `scripts/test_cam.py` 里的假接口（`MockAnthropic`）和临时的 HOME / `CAM_HOME` / `CLAUDE_CONFIG_DIR`；`install.sh` 只在临时 HOME 里跑
- 两个版本要一致的规则写在 `tests/shared/`，改 Python 版时 Rust 版一起改
- 改完任何文件、提交之前运行 `python3 scripts/gen-sums.py` 重新生成 `SHA256SUMS`（测试会检查）
- 界面文字用中文，说法和 README 一致

## 故意没做的功能

不要加，用户问起时简单说明（大多和 Anthropic 的条款有关，README 里有原因）：

- 额度用完自动切到下一个账号（只提醒，换不换由用户一键决定）
- 自动登录：代填验证码、自动点登录邮件里的链接完成 Claude 登录
- 伪装或隐藏所在地、设备（改出口、时区、语言、设备标识等）
- 按地区或环境评分自动拦截（环境检测只提醒，不拦任何操作）
- 多人共用一个账号
- 超出现有范围的 Claude Desktop 登录搬运：不把一个产品的登录转成另一个产品的，不把 Desktop 的会话带到别的电脑

## 聊天机器人

Discord / 飞书 / 钉钉机器人不在这个仓库，在单独的 claude-channels 仓库里，有它自己的安装脚本。
