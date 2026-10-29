# Claude 账号管理

管理多个 Claude 账号：切换 Claude Code 登录、打开各自独立的 Claude Desktop 配置，查看额度、记录套餐和到期日。Claude Code 和已配置 sessionKey 的网页账号可查询 5 小时、7 天额度；Claude Desktop 读取各配置的本地额度快照。管理工具提供桌面应用（macOS / Windows / Linux）、手机浏览器和命令行，三端共用同一份数据；Claude Desktop 客户端切换的兼容范围见下文。

支持四类账号：

- **Claude Code（命令行）**：走 `api.anthropic.com`，能一键切换登录、自动读取额度。
- **Claude Desktop（官方 Claude 客户端）**：每个号使用独立的本机配置，切换时退出并重启客户端。读取客户端自己的本地额度缓存与历史记录，显示记录时间；首次需要登录，会话过期后也可能需要重新登录。
- **网页版 claude.ai**：每个号一个独立浏览器窗口；填入 sessionKey 后可自动显示额度，会话过期后需要更新。适合只能访问网页、无法使用命令行接口的环境（例如公司网络屏蔽了 `api.anthropic.com`）。
- **API Key**：管理 Key 和备注，填写 Admin Key 后可读取本月花费。

| | 脚本版 `cam` | 桌面版 | 手机 / 其它电脑 |
|---|---|---|---|
| 需要什么 | Python 3.8+；旧版 Python 读取 Desktop 压缩缓存需可选依赖，见下文 | 安装包 | 浏览器 |
| 切换 Claude Code 账号 | ✓ | ✓（托盘菜单一键切换） | ✓（切的是电脑上的账号） |
| 打开 / 切换 Claude Desktop 配置 | `cam open` | ✓ | ✓（重启的是电脑上的客户端） |
| 查看额度 / 到期 | 按账号类型 | 按账号类型（Desktop 为本地快照） | 按账号类型 |
| 自动发现新登录的账号 | `cam` / `cam serve` 运行时 | 后台常驻 | — |
| 时间规则自动切号 | `cam serve` 运行时 | 后台常驻 | 可设置 |
| 目录规则自动切号 | `cam claude` | 可设置（通过 `cam claude` 生效） | 可设置 |
| 额度提醒 | 终端输出 | 系统通知 | — |

## 安装

先把代码拿到本机：推荐 `git clone https://github.com/a2data-cn/claude-account-manager.git`（以后一条命令就能更新），也可以在 GitHub 页面点 Code → Download ZIP 下载压缩包解压。桌面版的现成安装包在 [Releases](https://github.com/a2data-cn/claude-account-manager/releases) 页面。然后：

- **macOS**：在访达里双击 `安装.command`（从网上下载的压缩包第一次可能提示无法打开：到“系统设置 → 隐私与安全性”最下面点“仍要打开”），或者在终端运行 `bash install.sh`
- **Linux**：终端运行 `bash install.sh`
- **Windows**：双击 `安装.cmd`（就是运行 `powershell -ExecutionPolicy Bypass -File install.ps1`）

安装脚本按顺序做这几件事，每一步都会说清楚做了什么：

1. 检查 Python 3.8 以上（cam 只用 Python 自带的标准库）。Windows 上没有的话问一句，用系统自带的 winget 装 Python 官方的 3.13 版（只装给当前用户，不用管理员权限）
2. 按 `SHA256SUMS` 校验每个文件，有对不上的就列出来并停下，什么都不改
3. 装 `cam` 命令：macOS / Linux 在 `~/.local/bin/cam` 建一个指向代码目录的软链接（代码更新后不用重装），`~/.local/bin` 不在 PATH 里时在 `~/.zshrc` 或 `~/.bashrc`（macOS 上的 bash 是 `~/.bash_profile`）末尾加一行；Windows 把 `scripts` 目录加进用户 PATH
4. 没装 Claude Code 时问要不要用官方安装器装（默认不装，以后可以 `cam env install`）
5. macOS 问要不要装菜单栏小工具（默认装）。Linux 问要不要把 `cam serve` 装成只限本机访问的后台服务（systemd 用户服务 `cam-serve`，默认不装）：装了以后不开桌面版也会自动保存新登录、执行时间规则、刷新用量、额度快用完时提醒。Windows 问要不要装托盘小工具（默认装，见下面“托盘小工具（Windows）”），它也做这些后台任务
6. 运行 `cam doctor`，列出要留意的地方和下一步

可以重复运行，装好的部分会跳过。参数：
- `--yes`：不提问，全部按默认选择（Windows 写 `-Yes`）
- `--skip-verify`：跳过文件校验，自己改过代码时用（Windows 写 `-SkipVerify`）
- `--uninstall`：卸载，去掉 `cam` 命令、加进 shell 配置的那一行、菜单栏插件和后台服务（Windows 写 `-Uninstall`，把 `scripts` 从 PATH 里去掉，删掉托盘小工具）

安装和卸载都不碰账号数据（`~/.claude-accounts`）和 Claude Code 的配置（`~/.claude`）。装好后，每个账号在浏览器里登录一次：`cam login 工作号`，见下面“第一次使用”。

**文件校验**：项目根目录的 `SHA256SUMS` 记着每个文件的 SHA-256，安装和更新时自动核对，也可以随时运行 `cam verify`，或者用系统自带的工具在项目目录核对：`shasum -a 256 -c SHA256SUMS`（macOS）、`sha256sum -c SHA256SUMS`（Linux）。改了代码要提交时，运行 `python3 scripts/gen-sums.py` 重新生成（测试会检查它是不是最新的）。桌面版的编译脚本也会在安装包旁边写一份 `SHA256SUMS`。

## 更新

不会自动更新，也不会自己定时去查，只在你要的时候更新：

- **命令行**：`cam update --check` 看有没有新版本、列出新改动；`cam update` 更新
- **macOS 菜单栏**：点“检查更新”，有新版本时菜单里多一项“更新（N 个新改动）”，点了才更新，结果用系统通知告诉你
- **网页面板**（`cam serve` / `cam panel` 打开的）：底部“软件更新”里点“检查更新”，看过改动列表再点“更新”

更新只对 `git clone` 下来的代码有效，过程是：
- 代码目录里有没提交的改动就不更新，列出是哪些文件，什么都不改
- 用 `git merge --ff-only` 快进到远程分支的最新版本，不做合并
- 自检：核对 `SHA256SUMS`、编译所有脚本、在临时目录里跑一遍 `cam --help` 和 `cam ls`（不碰真实账号数据）。自检没过就退回原来的版本，并说明原因
- 更新成功后，Linux 上装了后台服务的会自动重启它；菜单栏小工具一分钟内就用新版本；自己开着的 `cam serve`（包括 `cam panel` 在后台起的）要关掉重开

下载压缩包装的没法这样更新：下载新版本解压，重新运行里面的安装脚本，`cam` 命令会改用新目录，账号数据不受影响。

桌面版是单独的安装包，不跟着 `cam update` 更新：重新编译安装（见下面“桌面版”），或者下载新的安装包覆盖安装，数据目录共用，不受影响。桌面版内置的面板里没有“软件更新”这一块。

## 界面一览

桌面版和手机面板是同一套界面，按"选账号 → 看详情 → 操作"组织。配色是暖色米白和赭橙强调，有明亮和暗黑两套，顶栏右边的 ◐ 按钮在"跟随系统 → 明亮 → 暗黑"之间切换（只记在这台设备上）：

![账号卡横向滑动，下面是选中账号的详情](docs/images/ui-accounts.png)

- **账号条**：每个账号一张卡，横向滑动（手机上用手指滑）。卡上有名称、套餐标签（Free / Pro / Max / Team / Enterprise 不同颜色，拿不到时显示"套餐未知"，可在"编辑"里手动选）、状态（正在用 / 异常 / 注意 / 快到期）和支持的用量。Desktop 显示“上次打开”与本地额度，超过 30 分钟的记录标为“旧快照”。最后一张是"+ 添加账号"，点哪张，下面就显示哪个。
- **详情**：大号额度条（数字是已用百分比，竖线是窗口已过去的时间）、到期 / 价格 / 最近切换 / 备注，右上角是操作：切换到这个账号（网页号是"在电脑上打开"）、刷新、编辑、移除。每个额度条下面有：
  - **重置时刻**：例如"10月25日 08:00 重置 · 3 天 19 小时后"。
  - **配速**：已用比时间快还是慢，例如"配速 +14% · 照这个速度约 59 分钟后用完"或"配速 -31% · 照这个速度到重置约用到 33%"。用得明显比时间快时标黄。
  - **整窗估算**（Claude Code 号）：本机这个窗口的消耗 ÷ 已用百分比，例如"整窗约 $1,559（6221 万 tokens）· 本机已用 $234"。金额按 API 价折算，订阅不按这个收费；已用不到 5% 时写"数据不足"。这个号在别的电脑或网页上也在用的话会偏高。
  - **按模型的周额度**：接口单独给出时多一条，例如"7 天 · Fable"。
  - **7 天构成**：7 天额度花在了哪里，例如"Claude Code 70% · 网页聊天 30%"。

  套餐标签会区分 Max 5x / 20x（接口给出档位时）。账号区标题右边的"已用 / 剩余"切换额度显示方式，"邮箱打码"把邮箱显示成 ch\*\*\*@y\*\*\*.com，方便截图分享；这两个开关只记在当前设备上。
- **异常账号**：卡片标红"异常"，详情最上面写清楚出了什么问题、怎么修，并附原始报错。会判断的情况：没有保存凭据、登录已失效（refresh token 被吊销）、保存的登录属于别的账号、sessionKey 失效、API Key 无效、订阅已过期；网络或服务暂时出错标"注意"，不算异常。

![异常账号的详情](docs/images/ui-abnormal.png)

- **移除**：只从这个工具里移除。Claude Code 的项目、会话记录、设置和 MCP 配置都保留，账号在 Anthropic 那边的订阅和数据也不受影响；这里保存的凭据和它的切号规则移到备份目录，可以找回。Claude Code 当前账号、工具上次打开的 Desktop 配置都需要先切到其它同类账号才能移除。

![移除确认](docs/images/ui-remove.png)

- **环境状态卡**：风险评分、出口（国家 / 城市 / 是否在支持名单）、网络（运营商 / IP 类型）、上次检测时间，"重新检测"会实测一遍并按网络 / 本机 / 服务 / 账号分组展开。环境正常时放在账号详情下面；有风险时自动挪到最上面，逐条列出提醒和建议：

![有风险时环境状态卡在最上面](docs/images/ui-risk.png)

- **下方区块**：切号规则（时间段 / 项目目录、额度提醒阈值）、本机 Claude Code 消耗（可切今天 / 7 天 / 30 天：折算金额、输出和输入 tokens、缓存命中率四个数字，每天的柱子按账号叠、悬停看明细，右边是按模型的占比——Opus / Sonnet / Haiku 各花了多少）、隐私与数据源。完整的 Claude 服务检测在环境卡的"重新检测"里展开，可以收起。桌面版另有"手机与设置"页：开关手机访问并显示二维码、打开数据目录；关窗口缩到托盘，托盘菜单一键切换。

暗黑模式：

![暗黑模式](docs/images/ui-dark.png)

手机上是同一个页面，账号卡用手指左右滑，详情和按钮竖排、加大便于点按；"添加到主屏幕"后用起来和 App 一样：

<img src="docs/images/ui-phone.png" alt="手机面板" width="360">

命令行和 macOS 菜单栏显示同样的信息：`cam` 列表里异常账号下面有"✗ 异常：原因。怎么修"，末尾是环境评分（越高越安全）；菜单栏在账号名后标"⚠ 异常"，子菜单列出原因和建议。截图用的是示例数据。

## 菜单栏小工具（macOS，推荐）

菜单栏常驻一个额度环和当前账号的 5 小时额度，例如“◔ 工作号 64%”，环画出已用的比例，颜色跟着额度变：70% 以下绿、70–89% 橙、90% 以上红。名字是邮箱或超过 6 个字时菜单栏放不下，显示成“1号”这样的序号（和 `cam` 列表、快捷键的数字一致，账号行前面也标着），起个短名就能显示名字：`cam set 1 --label 工作号`。额度环旁边还有一个小信号灯（红 / 橙 / 绿三盏）：亮绿是空闲、一切正常，亮橙是有 `claude` 会话在跑、额度快满或在等你确认，亮红是要处理的事（登录失效、额度用完、后台出错）；点开菜单第一行写着具体是什么情况。默认的经典模式下，工作中 / 等确认时橙灯两种亮度交替（SwiftBar 每 5 秒换一次标题，只能慢闪）；想要顺滑的呼吸灯就装成流式模式：`bash extras/swiftbar/install.sh --stream`（插件进程常驻，画面有变化才推给 SwiftBar，不动时 5 秒看一眼；`--classic` 换回）。菜单底部也有一项“信号灯动画：换成流式 / 换回经典”，点一下就切，SwiftBar 会自动重新打开。点开每个号占两行：第一行是名字和套餐标签（Max 20x），当前在用的号前面打勾、有问题的号标黄 / 红三角；第二行是进度条和“5 小时 64%，2小时10分后重置 · 7 天 30%”。子菜单里是配速、整窗估算、到期日和“切换到这个账号”，点一下就能切换（有 `claude` 会话在跑时先登记，等它们退出再切），切换结果和出错都用系统通知告诉你。按住 Option 键，账号名换成邮箱显示。额度环和进度条跟随浅色 / 深色模式，按 Retina 画。

左键点菜单栏图标弹出一个卡片小面板（SwiftBar 的网页弹窗，样子照 macOS 控制中心）：顶上一条红绿灯——绿是空闲、一切正常，黄是有 `claude` 会话在跑、额度快满或等你确认（点了别的号而有会话在跑时，在这里选“等它们退出再切”还是“现在就切”），红是要处理的事（登录失效、额度用完、后台保存失败）；下面是当前账号的卡片：额度环、套餐标签、5 小时和 7 天的滑杆、本机今天的消耗（tokens、缓存命中率、按 API 价折算的金额）；再往下其它账号两张一行的小卡，点一下切换，账号多了整页上下滑；最后是刷新用量、打开管理面板两个圆按钮和出口、环境评分。跟随浅色 / 深色模式。它用的是本机 `cam serve` 的 `/mini` 页面，没在跑会自动在后台起一个（只监听 127.0.0.1）。右键（或按住 Option 点）还是系统菜单。不想要弹窗，在插件启动脚本里加 `export CAM_MENU_MINI=0`。

全局快捷键（任何应用里按都生效）：⌃⌥⌘C 打开菜单，⌃⌥⌘1、2、3… 切到第几个号（按添加顺序，每行右边标着）。和别的程序撞了的话，在插件启动脚本 `~/Library/Application Support/SwiftBar/Plugins/Claude 账号.1m.sh` 里 `exec` 那行前面加一行 `export CAM_MENU_HOTKEY=CTRL+SHIFT+CMD` 换前缀，设成空（`export CAM_MENU_HOTKEY=`）就不注册。

菜单底部的“打开管理面板”会在浏览器打开网页面板（改名、到期日、切号规则、隐私设置都在里面），“检查更新”见上面“更新”。“全面体检”在后台跑 `cam doctor`，用系统通知告诉你结论（“一切正常”或“N 项要处理：第一项”），“在终端看完整体检结果”打开终端显示全部内容。本机已经有面板在跑（桌面版或 `cam serve`）就直接用；没有就在后台启动一个只限本机访问的 `cam serve`，一直运行到注销或重启。

```bash
bash extras/swiftbar/install.sh    # 一键安装时选了装菜单栏小工具，就不用再运行
```

脚本会自动安装 [SwiftBar](https://github.com/swiftbar/SwiftBar)（需要 Homebrew；没有的话会告诉你去哪下载），并把插件装好。插件每分钟运行一次，同时负责：
- 自动保存新登录的账号
- 执行时间规则
- 每 10 分钟刷新用量
- 额度超过阈值时弹系统通知

所以装了它，不开桌面版也是全自动的。桌面版或 `cam serve` 同时在运行时，后台任务交给它们做，插件只负责显示菜单（菜单底部会写“自动刷新由桌面版或 cam serve 负责”），不会几个程序同时轮询。`cam serve` 接手时弹同样的系统通知（新账号、切换结果、额度提醒；Linux 上有 `notify-send` 就用它），所以从菜单“打开管理面板”之后提醒也不会断。

## 托盘小工具（Windows）

任务栏右下角常驻一个图标，上面写着当前账号 5 小时额度用了百分之几，底色跟着额度变：70% 以下绿、70–89% 黄、90% 以上红，用满了写“满”；鼠标放上去显示是哪个号。左键或右键点开，菜单和 macOS 菜单栏小工具是同一份：每个号的额度进度条和 7 天额度，当前在用的打勾，有问题的标出原因；子菜单里是重置时间、到期日和“切换到这个账号”（有 `claude` 会话在跑时先登记，等它们退出再切）；底部是打开管理面板、刷新用量、全面体检、检查更新。切换结果、额度提醒都用系统通知告诉你。

一键安装（`安装.cmd`）时问到“装托盘小工具吗”回答是（默认）就装好了：放进“启动”文件夹，登录 Windows 后自动在后台运行，不弹黑窗口。和菜单栏小工具一样每分钟顺带做后台任务：自动保存新登录的账号、执行时间规则、每 10 分钟刷新用量、额度超过阈值时提醒（桌面版或 `cam serve` 在运行时由它们做）。

- 看不到图标：点任务栏右下角的 ^ 展开，可以把图标拖到任务栏上常驻
- 菜单最下面“退出托盘小工具”只退出这一次，下次登录 Windows 还会运行；不要了就运行 `安装.cmd -Uninstall`
- `cam update` 更新代码后，托盘小工具一分钟内自己换成新版本
- 只需要 Python（一键安装会帮你装），图标和菜单用的是 Windows 自带的 PowerShell，不用另装别的程序

## 第一次使用（每个账号做一次）

工具不保存密码。Claude 登录要走邮箱验证或 Google 授权，所以每个号的第一次登录需要你自己完成：

1. 终端运行 `cam login 工作号`，在浏览器里登录第一个账号，登录完自动保存（也可以在 `claude` 里 `/login` 后运行 `cam`，或在桌面版 / 手机面板的账号区点"检测新登录"；桌面版和 `cam serve` 在后台也会自动发现）
2. 再运行 `cam login 个人号` 登录第二个账号，依此类推

起名的规矩（面板“编辑”里有一键可选的昵称，也可以自己填）：
- 6 个字以内：菜单栏才放得下，超过（或者用邮箱当名字）就显示成“1号”这样的序号，序号按添加顺序，和快捷键的数字一致
- 一个号一个名，不重复；建议按用途起（工作号、个人号、公司号、团队号、备用号、测试号），同一用途有几个就加序号或套餐（工作2号、工作Max）
- 邮箱另有一行显示，名字里不用带；改名随时可以（`cam set 1 --label 工作号`），切号规则和快捷键都不受影响
- 名字在哪都一样：`cam switch 工作号`、菜单栏、面板、聊天机器人

`cam login` 会先把当前账号最新的登录存好，再用 `claude auth login` 换号登录（需要 Claude Code 2.1 以上），所以之前的号不会丢。不要在 `claude` 里直接 `/login` 换号：当前账号在上次保存之后 Claude Code 续过的登录还没存进来，换号后旧的就作废了，那个号得重新登录（有桌面版、`cam serve` 或菜单栏插件在后台运行时会随时保存，问题不大）。

完成后：
- **切换**：`cam switch 工作号`，或在桌面版 / 托盘 / 手机上点一下
- **用量**：后台每 10 分钟自动刷新
- **登录过期**：后台定时刷新不会替不常用的号续登录（避免和 Claude Code 抢 refresh token），过期后会提示“登录已过期”。手动刷新一次（`cam usage`，或在桌面版 / 菜单栏 / 手机上点“刷新”）就会自动续上；切换到这个号也行
- **到期日**：官方没有接口，在"编辑"里填一次并勾选"自动续期"，之后每个周期会自动顺延

## 切换时不会丢的东西

切换只替换登录凭据（`~/.claude/.credentials.json`，macOS 是钥匙串里的 `Claude Code-credentials`）里的 Claude 登录（`claudeAiOauth`）和 `~/.claude.json` 里的 `oauthAccount` 字段。以下内容都不动：

- 会话记录（`~/.claude/projects/`），切换后 `claude --continue` 照常接着聊
- `settings.json`、`CLAUDE.md`、MCP 服务器、插件、项目权限
- 凭据里的其它内容，例如 MCP 服务器的登录（`mcpOAuth`），保持切换前的样子
- `~/.claude.json` 里的其它所有字段

每次切换前会先做这几件事：
- 把当前账号最新的 token 回写保存。Claude Code 运行时会刷新 token，不回写的话下次切回来就失效了。
- 目标账号保存的登录过期了，先用它自己的 refresh token 续上。续不上（登录已失效，或连不上网）就不切，当前登录原样不动，并告诉你怎么办（失效的号用 `cam login` 重新登录一次）。
- 把切换前的现场备份到 `~/.claude-accounts/backups/`，保留最近 30 份。

正在运行的 `claude` 进程内存里还是旧账号，切换后需要退出重开。不重开的话，它刷新 token 后还可能把旧账号写回凭据文件。所以切换时工具会检查有没有 `claude` 在运行：
- `cam switch` 在终端里会先列出这些进程并问一句是否继续（加 `-y` 不问）；没有终端时（脚本、菜单栏）按"等它们退出再切"处理
- 桌面版、手机面板、时间规则、`cam claude` 切换后会提示还有几个 `claude` 在运行

万一旧会话真的把旧账号写回了，工具以凭据为准，把 token 存回它真正的主人名下，不会覆盖别的号，并在 `cam` / `cam doctor` / 菜单栏里提示“Claude Code 实际在用的登录属于某某”。退出所有 `claude` 会话后重新切换一次即可。

## 脚本版

```bash
# 一键安装后就有 cam 命令（见上面“安装”）；不想装也可以直接 python3 scripts/cam.py

cam                       # 列出所有账号；顺便自动保存新登录的账号
cam login 个人号           # 登录另一个账号并保存（先存好当前账号，再 claude auth login）
cam usage                 # 刷新用量
cam switch 工作号          # 按名称 / 序号 / 邮箱 / id 切换
cam switch 工作号 --wait   # 有 claude 会话在跑时，等它们都退出后再切（不打断正在跑的任务）
cam set 工作号 --expires 2026-11-03 --auto-renew --price '$100/月'
cam serve --lan           # 启动 Web 面板，手机扫链接即可访问
cam panel                 # 在浏览器打开 Web 面板（没在运行就在后台启动，只限本机访问）
cam local --days 7        # 本机每天消耗了多少 token、记在哪个号上、按 API 价折算约多少钱
cam doctor                # 全面体检：账号登录还能不能续、Claude 服务、本机 Claude Code、cam 自己、聊天机器人；不显示密钥
cam doctor --offline      # 不联网：不续登录、不刷新、不测服务，只看本机和上次的结果（--json 给脚本用）
cam env install           # 用 Anthropic 官方安装器安装 Claude Code
cam env clean             # 清理 Claude Code 缓存（默认只预演，加 --apply 才执行）
cam update --check        # 看有没有新版本；cam update 更新（见上面“更新”）
cam verify                # 按 SHA256SUMS 校验代码文件
```

Windows 用 `scripts\cam.cmd`（一键安装后直接 `cam`），用法相同。

### Claude Code 环境：检查、安装、清理

`cam doctor`（也可以写成 `cam env doctor` / `cam env check`）是全面体检，分五节逐项列出，每行 ✓ 正常、! 留意、✗ 要处理、· 说明，有问题的下一行是“→ 怎么办”，最后一行是“N 项要处理，M 项留意”。不显示任何密钥；`--json` 输出同样的内容给脚本用，`--offline` 不联网。

1. **账号**：每个号一段。Claude Code 号看保存的登录还能不能续：不在用的号 access token 过期或快过期时，用它自己的 refresh token 续一次（和刷新用量走同一条路），续不上就是“✗ 登录已失效 → cam login 名字”，不用等到切换时才发现。**当前登录从不替它续**（交给 Claude Code），只说多久过期、用量接口能不能用。另外每个号列出套餐、到期日（7 天内提醒，过了算要处理）、上次取用量的错误（比如 429 限流、401）和健康状态；网页号看 sessionKey 能不能用，API 号看 Key 能不能用
2. **Claude 服务**：下面“服务检查”的全部内容，加上 api.anthropic.com 和 claude.ai 的延迟，以及官方状态页（status.claude.com，连不上再试 status.anthropic.com）的总体状态、没解决的事件、不正常的组件（Claude Code、API、claude.ai、Console）。状态页连不上只算留意
3. **本机 Claude Code**：装在哪、版本、有没有新版本（查官方原生安装器的版本号，查不到就不显示）、下面几类“切换了却不生效”、凭据文件（权限不是 600 会提醒 chmod 600）、当前登录；还有正在运行的 `claude` 会话，每个一行：pid、工作目录、运行了多久、用的哪个登录。Linux 上读 `/proc/<pid>/environ` 里有没有 `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` / `CLAUDE_CONFIG_DIR`（只看有没有，不读出值），用本机登录的标“切号会跟着换”，用自己 token 的标“用自己的号，切号不影响”；Discord / 飞书 / 钉钉机器人的会话按 tmux 名和启动时设的环境变量认出来，写成“Discord 机器人主会话”“飞书机器人 shop”。macOS / Windows 只列出 `ps` 看到的，说明看不出用的哪个登录
4. **cam 自己**：后台任务谁在做（桌面版 / `cam serve` / 菜单栏插件）、上一轮是什么时候；各账号上次刷新用量的时间（有后台在跑却超过 30 分钟没刷新会提醒）；上次检查更新的结果（不联网）；`SHA256SUMS` 校验；数据目录和密钥文件的权限（700 / 600）；备份有几份、最新的是哪份、一共多大
5. **聊天机器人**：装了 `~/.local/bin/claude-discord`、`claude-feishu`、`claude-dingtalk` 的，分别运行它们的 `doctor`（最多等 60 秒），列出 ✗ 的项，没有就是“全部正常”；没装就不显示这一节

python.org 装的 Python（macOS 上常见）默认不带根证书，https 请求会报 `CERTIFICATE_VERIFY_FAILED`。脚本版发现默认证书一张都没加载到时，会自动改用 `SSL_CERT_FILE`、certifi（装了的话）或系统自带的证书文件（macOS 是 `/etc/ssl/cert.pem`），证书照常校验，`cam doctor` 会写明用的是哪个文件。

第 3 节查出的常见“切换了却不生效”：
- PATH 里有好几份 `claude`（比如 npm 版和原生版并存），会列出来并标明实际运行的是哪个
- 设置了 `ANTHROPIC_API_KEY`、`ANTHROPIC_AUTH_TOKEN`、`CLAUDE_CODE_OAUTH_TOKEN` 等环境变量，或者 `settings.json` 里配了 `apiKeyHelper`、`~/.claude.json` 里存着 `primaryApiKey`。这些都优先于订阅登录，切换账号就没有效果
- 当前有几个 `claude` 进程在运行

第 2 节里的"服务检查"实测 Claude 服务本身是否正常，并把使用环境里的风险点逐项列出来，排查"是网络问题、环境问题还是账号问题"。第一行是总体结论：环境评分（满分 100，越高越安全，每个风险点按轻重扣分）加档位——90 分以上"风险低，可以使用"，70–89"有风险，建议先处理提醒项再使用"，70 以下"风险高，不建议在当前环境使用；是否继续由你决定"——再带上最严重的那一条。评分只汇总本机能看到的信号，满分也不代表一定合规。下面每一项都附"→ 这意味着什么、建议怎么做"。**全部只提醒，不拦任何操作**，是否继续由用户自己决定：
- DNS：`api.anthropic.com`、`claude.ai` 解析到哪里
- 出口地区：从 Cloudflare 看到的国家、城市、IP 和节点，并对照 Anthropic 官网公布的支持国家/地区名单（美国、英国、日本等 180 多个；中国大陆、香港、澳门、俄罗斯等不在内）。不在名单内会醒目提醒：按 Anthropic 政策该地区不提供服务，继续使用可能导致账号被限制或封禁，建议换到支持地区的网络出口。在名单内也会说明：这只是 IP 这一项没问题，Anthropic 结合 IP 和其他信号综合判断所在地。名单会变，以官网为准
- 网络服务商和 IP 类型：出口 IP 属于哪个运营商（ASN），数据库判定它是家庭 / 公司宽带、机房 / 云服务器、代理 / VPN 出口、Tor 出口节点还是手机网络。机房、代理、Tor 都会提醒：这类出口通常被当作 VPN / 代理流量看待。数据源见下面"隐私与数据源"，默认都可以商用；没有类型标记时按运营商名里的关键字判断
- IP 归属：IP 在 RIR 登记的国家（RDAP）和定位国是否一致（原生 IP / 广播 IP）。不一致时说明不同地理库可能判到不同国家
- IPv4 / IPv6 双栈：两个协议分别测出口。VPN / 代理通常只接管 IPv4，IPv6 直接从本机网络出去，两边地区不同时提醒"服务端通过 IPv6 看到的就是本机网络的真实出口"
- 出口地区判定一致性：两个接口看到的国家不一致（代理只分流了一部分）时提醒
- DNS 解析器：本机配置的 DNS 服务器是局域网的、任播公共 DNS（Google、Cloudflare 等，不拿来判断位置）还是某个国家的公共 DNS（阿里、腾讯、114、HiNet 等）；用的是不支持地区的公共 DNS 而出口在支持地区时提醒。**只读本机配置，不做泄漏探测**（泄漏探测要往第三方的记录服务发带标识的查询，没有做）
- `api.anthropic.com`：用当前登录实测，区分"服务正常 / 登录已过期 / 拒绝访问（通常是地区不支持）/ 连不上 / 服务端故障"，并显示时延
- `claude.ai`：是否可达，是否被公司网络的人机验证页拦住（只影响网页账号读额度，不影响切号）
- 代理：当前终端的代理环境变量，以及系统级代理设置（macOS 网络设置、Windows Internet 选项、GNOME）；有代理时出口地区就是代理的出口
- VPN：本机默认路由是否走隧道接口（utun / tun / wg / TAP 等）；走隧道时出口地区是 VPN 的出口，不代表人在哪里。只看默认路由，透明代理和路由器上的 VPN 看不出来
- 出口地区变化：和上一次检测相比出口换了国家（出差、换网络、开关 VPN），24 小时内持续提醒。Anthropic 按使用者实际所在地判断，同一账号短时间内多地区使用也容易触发风控
- Cloudflare WARP / Zero Trust：请求经过 WARP 时说明出口不是本机真实出口
- 本机时钟：和服务器相差超过 2 分钟会提醒（影响登录续期、HTTPS 校验，也是风控信号）
- Anthropic 官方状态页：`status.anthropic.com` 当前有无故障，有故障时说明是 Anthropic 侧的问题
- 系统时区：时区指向的国家和出口地区不一致时提醒；指向不支持地区时特别说明"政策按人所在地判断，不按出口 IP"
- 系统语言 / 区域：和出口不一致时只作参考、扣 5 分（语言只代表语言习惯，新加坡、美国很多人也用 zh_CN），建议在"语言与地区"里把地区改成所在国家（macOS 的 AppleLocale、Windows 的 Get-Culture、Linux 的 LANG）
- 设备：系统和架构。设备环境信息 Anthropic 会收集，这里只列出来，**本工具不会改动或伪装任何设备标识**
- 账号使用地区：当前账号 7 天内从几个地区使用过（每次后台检测和切换都会记一笔），多地区使用是常见的风控触发点
- 账号数：本机保存了几个 Claude Code 账号，并提示账号不能给别人用（消费者条款第 2 条）
- 检测范围：最后一条说明本工具只能看到本机和网络层面的信号，账号侧的信号看不到，所以"没有发现风险点"不等于一定合规

桌面版和手机面板的环境卡上点"重新检测"会展开同样的检测结果；`cam serve` 提供的面板旁边还有"全面体检"按钮，展开的是 `cam doctor` 的完整五节（桌面版内置的面板没有这个按钮）。另外桌面版、`cam serve`、菜单栏插件会随用量刷新每隔几分钟记一次出口地区、WARP、时钟偏差和官方状态页，面板和桌面版顶部常显"出口 US ✓"和据缓存算的"环境 92"徽章（越高越安全）（实测的分数以 `cam doctor` 为准）；上面这些风险点里级别为"提醒"或"故障"的（出口不在名单、出口变化、时区指向不支持地区、时钟偏差、官方故障、账号多地区）会常显在页面顶部、菜单栏和 `cam` 列表里，`cam switch` 和面板切换前也会再提醒一次，但照样切。

```bash
cam env install            # 没装就用官方原生安装器装（curl -fsSL https://claude.ai/install.sh | bash，Windows 用 Anthropic 官方的 install.ps1）
cam env install --npm      # 改用 npm install -g @anthropic-ai/claude-code
cam env install --force    # 已经装了也重装，可用来升级
```

运行前会显示完整的安装命令并确认，非交互环境要加 `-y`。

```bash
cam env clean                      # 预演：列出将删除的缓存和大小，不改任何东西
cam env clean --apply              # 删除可以重新生成的缓存（cache、statsig、shell-snapshots、debug 等）
cam env clean --logout --apply     # 同时退出 Claude Code 登录
cam env clean --purge --apply      # 把整个 Claude Code 配置移到备份目录，相当于全新安装
```

- 默认只删一份固定名单里的缓存。会话记录（`projects/`）、`settings.json`、`CLAUDE.md`、commands / agents / skills / plugins、`~/.claude.json` 里的项目配置、cam 自己的数据目录都不动
- `--logout` 退出前先把当前账号的最新凭据存进 cam，之后 `cam switch` 能切回来；退出前的登录状态另外备份在 `backups/`
- `--purge` 不删除，而是把 `~/.claude/`、`~/.claude.json`（macOS 还有钥匙串里的登录）移到 `~/.claude-accounts/env-backups/<时间>/`，要恢复时移回去即可。这一步会带走会话记录和设置，所以会再确认一次（`-y` 跳过）
- 有 `claude` 正在运行时拒绝执行，先退出它们，或加 `--force`

### 切号规则

```bash
# 时间规则：工作日 9 点到 18 点用工作号，进入时间段时自动切换
cam rule add-time 工作号 --days 1-5 --start 09:00 --end 18:00
cam rule add-time 个人号 --days 1-5 --start 18:00 --end 09:00   # 可以跨午夜

# 目录规则：在 ~/work 下用 cam claude 启动时自动用工作号（参数原样透传给 claude）
cam rule add-dir ~/work 工作号
cd ~/work/some-project && cam claude --continue

# 额度提醒阈值（当前号 5 小时额度超过 85% 时提醒，并告诉你多久后重置）
cam rule alert 85
cam rule ls
```

面板“切号规则”顶上一行写着现在的状态：没有规则时不会自动切号（默认只在额度超过阈值时提醒）；有规则时写着在不在时间段里、该用哪个号、下一次什么时候切。时间规则只在"进入时间段"的那一刻切换，时间段内你手动换号，工具不会再改回去。进入时间段时如果有 `claude` 会话正在运行，不会打断它：先记下来，等会话都退出后再切；要是整个时间段都没等到，就作废。

### 切换时有会话在跑

切换只替换凭据，正在运行的 `claude` 会话内存里还是旧账号的 token，需要退出重开。所以有会话在跑时：
- `cam switch` 在终端里会问你：等它们结束后再切（默认）、现在就切、还是取消。`--wait` 直接选"等"，`-y`（`--now`）直接选"现在切"。没有终端时（脚本、菜单栏）直接按"等"登记，打印一句说明就返回
- 桌面版和手机面板会弹同样的三选一；选"等"之后页面顶部会显示"等当前 claude 会话结束后自动切换到 X"，随时可以取消（`cam switch --cancel-wait` 也行）
- 菜单栏 / 托盘点一下切换，有会话在跑时默认就是"等"，并弹通知告诉你；切完、切不成也会通知
- 等待期间由桌面版、`cam serve` 或菜单栏插件在后台盯着，会话一退出就切；`cam switch --wait` 自己也会一直等，等的时候按 Ctrl-C 或关掉终端都会取消
- 登记中的等待切换在 `cam` / `cam ls` 里也看得到；手动登记的 6 小时内没等到会话结束就作废，不会隔天突然换号

想让目录规则对所有 `claude` 命令都生效，可以在 shell 配置里加一行 `alias claude='cam claude'`。

### 启动前的风险提醒（`cam claude`）

`cam claude` 启动前会看一眼环境评分，按档位提醒。一小时内没检测过时，只花最多 3 秒问一次 claude.ai 的出口，完整检测交给后台进程去做，不拖慢启动。**只提醒和确认，不拦截**，是否继续始终由你决定：
- 90 分以上：一行"环境评分 100/100：风险低，可以使用"，直接启动
- 60–89 分：打印分数和每条提醒，直接启动（公司 VPN、出差换网一般落在这档）
- 30–59 分：打印后问"是否继续 [y/N]"，默认不继续，按 y 才启动
- 30 分以下，或出口不在 Anthropic 支持名单内（这条不看分数）：要输入"继续"两个字才启动，直接回车取消

```bash
cam risk                        # 查看当前档位、阈值和缓存的分数
cam risk --confirm 60 --strong 30   # 调整两个阈值
cam risk --off                  # 关掉启动前提醒（--on 打开）；CAM_NO_RISK_PROMPT=1 可临时关掉
```

非交互环境（脚本、CI、没有终端）只打印提醒，不会卡住等输入。桌面版和手机面板不负责启动 `claude`，它们在顶部常显同样的分数和提醒。

### 隐私与数据源

环境检测只发送这台电脑的**出口 IP**，不发送任何账号、邮箱或凭据。会访问哪些服务取决于"外部 IP 查询"设置：

| 设置 | 会访问 | 说明 |
|---|---|---|
| 关闭 `off` | `claude.ai/cdn-cgi/trace`、`status.anthropic.com` | 只看出口国家和官方状态，不查运营商、IP 类型、IPv6、注册国 |
| 自动 `auto`（默认） | 上面两个，加 `api.ipapi.is` → `ipinfo.io` → `speed.cloudflare.com`（按顺序，第一个查到的为准）、`rdap.org`、`1.1.1.1` 和 Cloudflare IPv6 | 都可以商用。ipapi.is 免费注册 key 每天 1000 次、带机房 / VPN / Tor 标记（官方写明可商用）；不配 key 每天 30 次、不带标记。ipinfo.io 免费版可商用但要求注明来源，界面上会写"数据：ipinfo.io" |
| 加上 ip-api `ip-api` | 自动的那些，并优先用 `ip-api.com` | ip-api.com 免费接口**仅限非商业用途**、只有 http，个人自用才选 |

出口 IP 没变时，运营商这些信息一天只查一次，第三方请求很少。

```bash
cam risk --ip-lookup off          # 关闭外部 IP 查询（auto / ip-api / off）
cam risk --ipapi-key 你的key       # 配 ipapi.is 的 key（空字符串删除），--ipinfo-token 同理
cam risk --clear-history          # 清除缓存的出口信息和每个账号的使用地区记录
cam risk                          # 查看当前设置和会访问哪些服务
```

桌面版"手机与设置"页和手机面板底部的"隐私与数据源"也能改。key 存在数据目录的 `secrets` 里，不进 `accounts.json`，不显示在任何界面。环境变量 `CAM_IP_LOOKUP`、`CAM_IPAPI_IS_KEY`、`CAM_IPINFO_TOKEN` 优先于保存的设置。

**给客户用时**：建议每台电脑各注册一个免费的 ipapi.is key（一个 key 每天 1000 次，多台共用会不够），或者你买付费版统一配置；不想把出口 IP 发给第三方的客户，设成 `off` 就行，出口地区和官方名单检测照常。

额度满了自动换下一个号的功能**没有做**。官方条款没有明确禁止一个人拥有多个账号，但明确禁止：把账号给别人用或分享登录信息（消费者条款第 2 条）、转售或代理给别人用、被封后换号继续用、为躲避检测开多个号（使用政策 Do Not Abuse Our Platform）。“一个号额度用完马上换下一个”条款没有直接点名，但可能被认定为绕过系统或防护措施（消费者条款第 3 条），由 Anthropic 判定。所以工具只负责提醒，换不换由你一键决定。需要更多额度，官方方式是升级 Max、开额外用量，或用 API 按量计费。

### 网页版、Claude Desktop 和 API 账号

```bash
cam add-web --label 家里网页号     # 每个网页号一个独立的浏览器 Profile
cam open 家里网页号                # 打开它专属的窗口，首次或会话过期后登录
cam add-desktop --label 工作桌面 --current   # 绑定这台电脑上已经登录的 Claude 桌面版
cam add-desktop --label 家里桌面             # 另一个号：第一次打开时在窗口里登录
cam open 家里桌面                            # 退出当前的 Claude 桌面版，用这个号重新打开
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

### Claude Desktop 客户端切换

在“添加账号”中选择“Claude 桌面版”。第一个号可勾选“使用这台电脑上已经登录的 Claude”；其它号创建独立配置，打开后在 Claude 窗口内登录。每次点击“切换桌面版”或“重新打开”，工具都会正常退出当前 Claude Desktop，等待退出后备份登录文件，再用目标目录启动。请先处理好 Desktop 中正在运行的任务；无法确认已经退出时会停止切换。

只使用 Claude 客户端时，每个账号首次在客户端登录一次即可；读取本地额度无需再登录 Claude Code 或网页版。管理工具的桌面界面、手机面板和命令行共用同一份账号记录，不必分别添加。官方 Desktop、Claude Code 和网页的登录认证各自独立，工具不会把其中一次登录转换成另两个产品的登录；会话过期或被撤销后仍可能需要重新验证。

独立配置存放在 `~/.claude-accounts/desktop-profiles/<id>/`，原来已登录的配置仍使用 Claude 自己的数据目录。配置包含敏感会话数据，只供同一台电脑上的当前系统用户使用，不适合作为跨设备登录备份。会话可能过期、被撤销或要求再次验证。Desktop 切换默认不与 Claude Code 命令行联动；套餐和到期日可以手动记录。

**读取 Desktop 额度**：在账号详情点“读取本地额度”。工具仅读取该配置的 Claude 本地缓存，不需要额外登录、填写 sessionKey 或授权读取钥匙串，也不会为额度查询发送网络请求。优先读取 Claude 用量页保存的 5 小时 / 7 天百分比与重置时间，缺失时尝试本地用量历史。界面同时显示“记录时间”和“最近读取”：前者是 Claude 实际写入这份数据的时间，超过 30 分钟会提示旧快照；后者只是本工具扫描的时间。已过重置时间的窗口不再显示，本地历史没有提供重置时间时会明确注明。

没有记录时，先打开对应配置，在 Claude 的“设置 → 用量”查看一次，再回工具读取。未打开的账号会保留之前的本地记录；客户端尚未写入、缓存失效或格式变化时会说明原因，工具不会把缺失额度显示成 0%。Python 3.14+ 可直接解压压缩缓存；更早版本可安装可选依赖：`python3 -m pip install -r scripts/requirements-desktop.txt`。本地历史读取无需额外依赖，Rust 桌面版内置解压支持。

卡片的“上次打开”只表示工具最后成功启动的配置，不表示持续在线或已验证登录身份。从 Dock、快捷方式或其它程序手动启动 Claude 后，这条记录不会自动更新；使用工具重新打开目标配置可再次执行完整切换。移除这个配置前，需要先用工具打开其它 Desktop 账号。

兼容性：macOS 使用 Claude.app 和 `--user-data-dir` 启动参数。Windows 独立安装版可以尝试；Microsoft Store / MSIX 版尚未完整支持自动发现与启动。未找到客户端时，可用 `CAM_CLAUDE_DESKTOP` 指定可执行文件（macOS 指向 `.app`），用 `CAM_CLAUDE_DESKTOP_DATA` 指定原有用户数据目录；自定义路径仍需实际验证对应版本是否支持启动参数。Linux 没有官方 Claude Desktop，本项目仅保留对部分第三方封装路径的适配。自动化测试使用模拟进程和临时目录，尚不代表各平台真实客户端版本均已实测兼容。

实现参考了 [matsumotory/claude-desktop-switcher](https://github.com/matsumotory/claude-desktop-switcher) 的目录隔离与进程检查、[jspanji/Swivel](https://github.com/jspanji/Swivel) 的退出后保存与会话保护、[nyxon-tech/claude-switcher](https://github.com/nyxon-tech/claude-switcher) 的登录文件范围与 Windows 路径处理。这些项目均采用 MIT 许可；这里借鉴方法，没有复制它们的源码。更多设计与限制见 [设计说明](docs/DESIGN.md)。

本地额度缓存格式参考了 [ajipurn/claude-profiles 的 UsageReader](https://github.com/ajipurn/claude-profiles/blob/main/Sources/ClaudeProfilesCore/UsageReader.swift)，本项目独立实现解析。该仓库未声明开源许可，未复制其源码；它已归档，后续维护迁往 `agent-profiles`。

## 本地账号库与邮箱

打开“账号资料与邮箱”，设置至少 8 个字符的主密码，即可集中保存账号邮箱、密码和邮箱连接。数据位于 `~/.claude-accounts/vault.sqlite3`；名称、邮箱和连接配置是元数据，密码、Client Secret、OAuth 授权通过主密码加密。主密码不落盘，忘记后无法解密已存秘密；桌面版、Python 服务或新的 CLI 进程需要各自解锁。Python 版需要 `python3 -m pip install -r scripts/requirements-vault.txt`，Rust 安装包内置支持。

新增资料时可以绑定已有 Claude 账号，点“读取已登录邮箱”从对应登录资料补全邮箱，识别不到会说明原因。仅保存资料可选“仅保存账号资料”；要自动收验证码，再选择以下邮箱方式并保存：

- **Gmail / Google Workspace**：在自己的 Google Cloud 项目启用 Gmail API，创建“桌面应用”OAuth 客户端，填写 Client ID 和提供的 Client Secret；应用处于测试状态时加入自己的测试邮箱。参见 [Google 桌面应用授权说明](https://developers.google.com/identity/protocols/oauth2/native-app) 和 [Gmail 配置说明](https://developers.google.com/workspace/gmail/api/quickstart/python)。
- **Outlook / Hotmail / Microsoft 365**：在 Microsoft Entra 注册自己的应用，选择需要的个人或组织账号类型，在“移动和桌面应用”添加 `http://localhost/oauth/callback` 并启用公用客户端；填写应用程序 ID，租户默认 `common`，也可指定企业租户。授权申请 `User.Read`、`Mail.Read` 和 `offline_access`，企业管理员可能需要批准。参见 [Microsoft 桌面应用配置](https://learn.microsoft.com/en-us/entra/identity-platform/scenario-desktop-app-configuration)。
- **其它企业 IMAP**：填写服务器、TLS 端口（通常 993）、用户名和应用专用密码。只读收件箱并正常验证 TLS 证书，不内置中国邮箱服务商的专门适配。

Gmail / Outlook 点击“连接邮箱并授权”，在系统浏览器完成首次授权，界面等待授权结果。这里使用你自己的 OAuth 应用 ID，需要先完成上述配置；没有配置时无法完成真实邮箱授权。手机面板可让电脑打开授权页，回调必须在运行服务的同一台电脑完成。

已保存授权后，“读取最近登录邮件”检查近期邮件；“等待新验证码”从点击前 30 秒开始，每 5 秒检查一次，最多等待 2 分钟，找到后停止，也可随时取消。账号库页打开且解锁时，还可以每 30 秒自动读取。切换条目、锁库或关闭这块界面会停止等待并清除页面中的验证码和已显示密码；普通列表始终不包含密码，显式显示后 30 秒自动隐藏，复制仅由用户点击触发。

邮箱授权申请只读邮件权限，界面只显示可信 Claude / Anthropic 登录邮件的验证码和验证链接，不展示完整正文、不发送或删除邮件、不标记已读。验证链接只在你点击后打开，工具不会自动访问或预览一次性链接。**自动取信不代表自动完成 Claude 登录、回跳或多因素验证**；首次邮箱授权与首次 Claude 登录也各自独立。

绑定网页版账号时，验证链接在该账号的独立浏览器窗口打开。绑定 Desktop 时，应先在对应的 Claude 配置中发起登录，并保持该配置运行；打开验证链接会核对目标配置，验证时不退出或重启 Claude，避免破坏正在等待的登录流程。

Claude 当前没有独立账号密码；官方支持 Google 登录或邮箱安全链接。邮箱登录邮件主题为“Secure link to log in to Claude.ai”，发件域包括 `@mail.anthropic.com`。在请求登录的同一设备打开链接可继续登录；换设备打开可能生成验证码，需要回到原设备输入。密码字段用于集中保存你提供的账号或邮箱资料，不会改变 Claude 的认证方式。见 [Claude 官方登录说明](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account)。

命令行同样可用，主密码和账号密码通过隐藏输入读取：

```bash
cam identify 工作桌面
cam vault setup
cam vault add --email you@example.com --provider gmail --client-id YOUR_CLIENT_ID
cam vault ls
cam mail connect <条目ID> --open-browser
cam mail read <条目ID>
```

账号库补充现有账号记录，不迁移或覆盖 `accounts.json`。移除库中资料只移除保存的密码和邮箱授权，不删除 Claude 账号配置。接口和加密格式见 [账号库与邮箱设计](docs/VAULT_API.md)。

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

**不想本地编译**：到 [Releases](https://github.com/a2data-cn/claude-account-manager/releases) 页面下载现成的安装包。它们由 `.github/workflows/build.yml` 自动编出来：每次推送只跑测试，推送 `v1.3.0` 这样的标签时编 macOS（通用版）、Windows、Linux 三个平台的安装包，生成 Release 草稿，检查后发布。不打标签也想编一次：到 Actions 页面选 build → Run workflow，安装包在这次运行的 Artifacts 里。

桌面版功能：
- 关闭窗口后缩到托盘继续运行。托盘菜单列出所有账号和额度，点一下即可切换。托盘图标右下角有一个额度圆点（当前账号 5 小时额度 70% 以下绿、70–89% 黄、90% 以上红），鼠标放上去显示当前账号和 5 小时、7 天额度；macOS 菜单栏上还直接显示 5 小时用量百分比。
- 后台自动发现新登录的账号、执行时间规则、刷新用量，额度超过阈值时弹系统通知。同一台电脑上桌面版、`cam serve`、菜单栏插件只有一个在做这些事，谁先启动谁做，其它的只同步显示。
- 在"手机与设置"里开启 Web 面板后，手机扫二维码就能查看和切换。

安装包没有签名：macOS 第一次打开会提示无法验证开发者，到"系统设置 → 隐私与安全性"最下面点"仍要打开"（较早的 macOS 也可以右键选"打开"）；Windows 第一次运行安装包会提示"Windows 已保护你的电脑"，点"更多信息 → 仍要运行"。

## 手机访问

在桌面版"手机与设置"里开启，或者在电脑上运行 `cam serve --lan`。手机和电脑连同一个 Wi-Fi，扫码或打开打印出来的链接即可。在浏览器菜单里"添加到主屏幕"后，用起来和 App 一样。

链接里带访问令牌，相当于密码。不在同一个网络时，建议在电脑和手机上都装 [Tailscale](https://tailscale.com) 这类组网工具，不要把端口直接暴露到公网。

## 数据与安全

- 所有数据存在 `~/.claude-accounts/`（可用 `CAM_HOME` 改位置），目录权限 700、凭据文件权限 600，不上传任何地方
- 网络请求只发往 `api.anthropic.com`（用量、资料）、`platform.claude.com` / `console.anthropic.com`（续登录），以及你主动填了 sessionKey 时的 `claude.ai`。服务检查和后台的出口地区记录另外会访问哪些服务，见上面"隐私与数据源"，可以关闭；这些请求只带出口 IP，不带任何账号信息。账号的使用地区记录只存在本机 `accounts.json` 里，`cam risk --clear-history` 可清除。`cam env install` 会运行 Anthropic 官方安装器（从 `claude.ai` 下载），或者用 npm 安装。`cam update` 只在你运行（或在菜单栏、面板里点）时访问代码仓库（`git fetch`）。`cam doctor` 另外会读官方状态页（`status.claude.com` / `status.anthropic.com`）和 Claude Code 官方安装器的版本号（`downloads.claude.ai`），不带任何账号信息；`--offline` 时都不访问
- 用量和资料接口是 Claude Code 自己在用的接口，并非公开 API，Anthropic 改动后可能需要更新本工具；拿不到数据时界面会显示原因，不影响切换
- 删除账号时凭据会移到 backups 目录，不直接销毁
- 只有确认了"当前这份登录是谁的"才会存进对应账号：refresh token 和某个账号保存的一致，或者联网核对过。离线、或者 access token 已过期又换过 refresh token 时核对不了，就先暂存（`secrets/_pending-live.json`），不往任何账号里写，界面上会提示；联网后或切换时核对清楚再归档。这样旧会话写回的别人的 token 不会被存错账号

## 目录结构

```
install.sh            一键安装（macOS / Linux），双击用 安装.command
install.ps1           一键安装（Windows），双击用 安装.cmd
SHA256SUMS            所有文件的校验值，scripts/gen-sums.py 生成
AGENTS.md             给 AI 助手看的安装、验证和开发规矩
scripts/cam.py        脚本版（单文件，可单独拷走使用）
scripts/test_cam.py   脚本版端到端测试（伪造 HOME 和接口，不碰真实账号）
scripts/build-desktop-*  桌面版一键编译脚本
extras/swiftbar/      macOS 菜单栏小工具
extras/windows-tray/  Windows 托盘小工具（PowerShell 画图标和菜单，菜单内容和菜单栏小工具同一份）
web/panel.html        Web 面板（手机 / 浏览器），脚本版和桌面版共用
desktop/              Tauri 桌面版
desktop/src-tauri/engine/  核心逻辑 crate cam-engine，与 cam.py 对应的 Rust 实现，不依赖 Tauri
tests/shared/         两边共用的测试数据（身份识别、账号查找、切号规则、自动续期、编辑账号和到期日）
docs/DESIGN.md        数据格式、切换流程、锁、HTTP API
```

## 测试

```bash
python3 -m unittest discover -s scripts -p 'test_*.py' # 脚本、账号库、邮箱和身份识别
cd desktop/src-tauri && cargo test -p cam-engine    # Rust 核心逻辑，不需要 Tauri 的系统依赖
```

两边都会跑 `tests/shared/` 里的同一份数据，改规则时两边要一起改，否则测试会失败。改完任何文件、提交之前运行 `python3 scripts/gen-sums.py` 更新 `SHA256SUMS`，不然测试也会失败。
