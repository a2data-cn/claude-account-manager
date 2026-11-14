# 快速开始

cam 帮你在一台电脑上管理**自己的**多个 Claude 账号：一键切换 Claude Code 的登录、给每个号开一份独立的 Claude 桌面版配置、看 5 小时和 7 天额度、记套餐和到期日。

这一页是从零到能用的最短路径，大约十分钟。每一步的细节见 [README](../README.md)。

## 1. 安装

先把代码拿到本机：

```bash
git clone https://github.com/a2data-cn/claude-account-manager.git
```

也可以在 GitHub 页面点 Code → Download ZIP 下载后解压。用 `git clone` 的话，以后一条命令就能更新。

| 系统 | 做法 |
|---|---|
| macOS | 在访达里双击 `安装.command`，或者在终端运行 `bash install.sh` |
| Windows | 双击 `安装.cmd` |
| Linux | 终端运行 `bash install.sh` |

安装脚本会：
- 检查 Python 3.8 以上。Windows 上没有的话会问你，再用系统自带的 winget 装
- 按 `SHA256SUMS` 校验文件
- 装好 `cam` 命令
- macOS 装菜单栏小工具，Windows 装托盘小工具
- 最后运行一次 `cam doctor`

它不碰你的账号数据，也不碰 Claude Code 的配置。想全部按默认、一句都不问，就加 `--yes`（Windows 写 `-Yes`）。

装完新开一个终端，运行 `cam --version` 能看到版本号就好了。还没装 Claude Code 的，运行 `cam env install`。

不想用命令行的，可以到 [Releases](https://github.com/a2data-cn/claude-account-manager/releases) 下载桌面版安装包。安装包没有签名，第一次打开时的提示怎么处理，见 README 的“桌面版”。

## 2. 把账号加进来（每个号一次）

### Claude Code（命令行）的号

```bash
cam login 工作号     # 在浏览器里登录第一个号，登录完自动保存
cam login 个人号     # 再登录第二个号，前一个号不会丢
```

- 登录走邮箱链接或 Google 授权，由你自己在浏览器里完成，cam 不代劳。
- 名字建议在 6 个汉字或 12 个字母以内，菜单栏才放得下。
- 不要在 `claude` 里直接 `/login` 换号，那样上一个号保存的登录会作废。换号用 `cam login`。

### Claude 桌面版的号

```bash
cam add-desktop --label 工作桌面 --current   # 这台电脑上已经登录着的那个号
cam add-desktop --label 家里桌面             # 另一个号：第一次打开时在 Claude 窗口里登录
```

也可以在面板里点“添加账号 → Claude 桌面版”。桌面版的登录和 Claude Code 的登录是分开的，同一个邮箱两边各加一次。

### 网页号、API Key（用得到再加）

```bash
cam add-web --label 网页号                       # 每个网页号一个独立的浏览器窗口
cam add-api --label 项目A --key sk-ant-api03-...  # 填了 Admin Key 还能看本月花费
```

## 3. 日常使用

| 想做的事 | 怎么做 |
|---|---|
| 切换 Claude Code 的号 | 菜单栏、托盘或小卡片上点一下；命令行 `cam switch 个人号` |
| 切换桌面版的号 | 小卡片里桌面版那一组点一下；命令行 `cam open 家里桌面`（会退出再重开 Claude 桌面版） |
| 看额度 | 菜单栏、小卡片，或者运行 `cam`。后台每 10 分钟刷新一次，想马上刷新运行 `cam usage` |
| 打开管理面板 | 菜单栏里点“打开管理面板”，或者运行 `cam panel` |
| 在手机上看和切换 | 运行 `cam serve --lan`，手机连同一个 Wi-Fi 扫码打开 |
| 记到期日 | 面板里点“编辑”；命令行 `cam set 工作号 --expires 2026-12-03 --auto-renew` |
| 按时间或目录自动切号 | `cam rule add-time …`、`cam rule add-dir …`，写法见 README 的“切号规则” |

切换时如果有 `claude` 会话正在运行，cam 会问你：等它们结束再切、现在就切，还是取消。正在运行的会话要退出重开，才会用上新的账号。

后台在跑的时候（菜单栏 / 托盘小工具、桌面版、`cam serve` 任何一个开着），这几件事会弹系统通知，每件只提醒一次：
- 当前号的 5 小时或 7 天额度快满了（默认 90%，`cam rule alert 85` 改）
- 某个号用满的额度过了重置时间，可以接着用了
- 填了到期日的号还剩 3 天以内，或者已经过了到期日

管理面板的账号详情里有最近 30 天每天的最高用量，看得出哪个号老是不够用。

桌面版的号在卡片上会标出状态：
- “开着”：客户端现在开着的就是这个号
- “上次打开”：客户端没开，上次用 cam 打开的是这个号
- “登录过期”“没有登录”：下次打开时要在窗口里重新登录

## 4. 遇到问题

先运行 `cam doctor`，它会逐项列出哪里有问题、怎么办。只想看、不想改任何东西，就运行 `cam doctor --offline`。

| 看到的提示 | 意思 | 怎么办 |
|---|---|---|
| 某个号“登录已失效” | 这个号保存的登录被服务端作废了 | `cam login 这个号`，重新登录一次 |
| 某个号“登录已过期” | 不常用的号很久没续 | 运行 `cam usage`，或者切换到它，会自动续上 |
| 切了号，`claude` 还是旧号 | 正在运行的会话还拿着旧的登录 | 退出 `claude` 再重开 |
| 切了号却一直不生效 | 设了 `ANTHROPIC_API_KEY` 之类的环境变量，或者装了好几份 `claude` | 看 `cam doctor` 第 3 节，它会指出是哪一个 |
| 桌面号“登录过期”“没有登录” | 客户端本机记录里没有有效的登录 | `cam open 这个号`，在打开的窗口里登录 |
| 代理连不上 | 设了代理，但代理软件没在运行 | 打开代理软件，或者去掉这个代理设置 |
| 续登录的地址连不上 | 登录到期后会续不上，接着就会显示“需要登录” | 检查网络或代理设置 |
| 官方状态页有故障 | Anthropic 那边出了问题 | 等它恢复 |
| 环境评分偏低 | 本机能看到的风险信号 | 逐条看提醒和建议。cam 只提醒，不拦任何操作 |

还解决不了，就运行 `cam diag 这个号 --out 诊断.md`，导出一份诊断报告发给维护的人。报告里不含任何密钥。

## 5. 更新和卸载

- **更新**：`cam update --check` 先看有哪些改动，再运行 `cam update`。cam 不会自动更新；菜单栏和面板里也有“检查更新”。
- **卸载**：运行 `bash install.sh --uninstall`（Windows 写 `安装.cmd -Uninstall`）。账号数据会留在 `~/.claude-accounts/`，里面有保存的登录；确定不要了，再自己删掉这个文件夹。

## 6. 要知道的几件事

- 所有数据都在本机的 `~/.claude-accounts/`，不上传到任何地方。cam 不显示、不导出任何登录凭据。
- 这个工具是给一个人管理自己的多个号用的。按 Anthropic 的条款，账号和登录不能给别人用。
- 额度用完**不会**自动换到下一个号，cam 只提醒，换不换由你一键决定。环境检测也只提醒，不拦任何操作。
- 每个号的第一次登录都要你自己完成，cam 不会自动登录。

## 常用命令

```
cam                     列出账号和额度（顺便保存新登录的号）
cam login 名字          登录并保存一个新的 Claude Code 号
cam switch 名字         切换 Claude Code 的号（加 --wait 等会话结束再切）
cam usage               马上刷新额度
cam open 名字           切换桌面版的号，或者打开网页号的窗口
cam panel               打开管理面板
cam serve --lan         让手机也能访问面板
cam doctor              全面体检（加 --offline 只看不改）
cam diag 名字           导出这个号的诊断报告（不含密钥）
cam update              更新
cam --help              全部命令
```

想按 Tab 补全命令和账号名：bash 在 `~/.bashrc` 里加一行 `eval "$(cam completion bash)"`，zsh（macOS 默认）在 `~/.zshrc` 里加 `source <(cam completion zsh)`，PowerShell 见 README 的“命令行补全”。
