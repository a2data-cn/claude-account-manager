#!/bin/bash
# 安装 Mac 菜单栏小工具（SwiftBar 插件）
# 用法：bash extras/swiftbar/install.sh            经典模式：插件每分钟跑一次
#       bash extras/swiftbar/install.sh --stream   流式模式：插件进程常驻，信号灯能顺滑地呼吸 / 闪（SwiftBar 的 streamable 插件）
#       bash extras/swiftbar/install.sh --classic  换回经典模式
set -euo pipefail

MODE=classic
for arg in "$@"; do
  case "$arg" in
    --stream) MODE=stream ;;
    --classic) MODE=classic ;;
    *) echo "不认识的参数：${arg}（可用 --stream 或 --classic）"; exit 1 ;;
  esac
done

if [[ "$(uname)" != "Darwin" ]]; then
  echo "菜单栏小工具只支持 macOS。Windows 运行 安装.cmd 装托盘小工具，或者用桌面版。"
  exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
MENU_PY="$HERE/cam_menu.py"

# 1. Python：优先用当前终端里的 python3（SwiftBar 运行插件时 PATH 很短，必须写绝对路径）
PY="$(command -v python3 || true)"
if [[ -z "$PY" ]]; then
  echo "没找到 python3，请先安装：https://www.python.org/downloads/macos/"
  exit 1
fi
PY="$(cd "$(dirname "$PY")" && pwd)/$(basename "$PY")"
"$PY" -c 'import sys; assert sys.version_info >= (3, 8)' || { echo "需要 Python 3.8 以上"; exit 1; }

# 2. SwiftBar
if [[ ! -d "/Applications/SwiftBar.app" && ! -d "$HOME/Applications/SwiftBar.app" ]]; then
  if command -v brew >/dev/null 2>&1; then
    echo "正在安装 SwiftBar…"
    brew install --cask swiftbar
  else
    echo "需要先安装 SwiftBar（免费开源的菜单栏插件工具）："
    echo "  方式一：安装 Homebrew 后运行 brew install --cask swiftbar"
    echo "  方式二：到 https://github.com/swiftbar/SwiftBar/releases 下载 SwiftBar.zip，解压后拖进“应用程序”"
    echo "装好后重新运行本脚本。"
    exit 1
  fi
fi

# 3. 插件目录：沿用 SwiftBar 已设置的目录，没有就用默认目录
PLUGIN_DIR="$(defaults read com.ameba.SwiftBar PluginDirectory 2>/dev/null || true)"
if [[ -z "$PLUGIN_DIR" ]]; then
  PLUGIN_DIR="$HOME/Library/Application Support/SwiftBar/Plugins"
  defaults write com.ameba.SwiftBar PluginDirectory "$PLUGIN_DIR"
fi
PLUGIN_DIR="${PLUGIN_DIR/#\~/$HOME}"
mkdir -p "$PLUGIN_DIR"

# 4. 写入启动脚本（文件名里的 1m 表示每分钟刷新一次菜单；流式模式下 SwiftBar 不按它刷新，由插件自己推画面）
TYPE_LINE="# 经典模式：每分钟运行一次"
MODE_ARG=""
if [[ $MODE == stream ]]; then
  TYPE_LINE="# <swiftbar.type>streamable</swiftbar.type>"
  MODE_ARG=" stream"
fi
# 文件名就是弹窗的标题（“SwiftBar: Claude 账号”）；旧版本叫 claude-accounts.1m.sh，装新的时顺手删掉，免得出现两个图标
PLUGIN="$PLUGIN_DIR/Claude 账号.1m.sh"
rm -f "$PLUGIN_DIR/claude-accounts.1m.sh"
cat > "$PLUGIN" <<EOF
#!/bin/bash
# <xbar.title>Claude 账号</xbar.title>
# <xbar.desc>Claude 多账号额度与一键切换</xbar.desc>
# <swiftbar.hideAbout>true</swiftbar.hideAbout>
# <swiftbar.hideRunInTerminal>true</swiftbar.hideRunInTerminal>
# <swiftbar.hideDisablePlugin>true</swiftbar.hideDisablePlugin>
# <swiftbar.hideSwiftBar>false</swiftbar.hideSwiftBar>
# <swiftbar.hideLastUpdated>true</swiftbar.hideLastUpdated>
# <swiftbar.persistentWebView>false</swiftbar.persistentWebView>
$TYPE_LINE
export PATH="\$PATH:/usr/local/bin:/opt/homebrew/bin:\$HOME/.local/bin:\$HOME/.npm-global/bin"
exec "$PY" "$MENU_PY"$MODE_ARG "\$@"
EOF
chmod +x "$PLUGIN"

# 5. 先跑一次，顺便触发钥匙串授权
echo "正在检查…（如果弹出“python3 想访问钥匙串”，请点“始终允许”）"
"$PY" "$MENU_PY" | head -1 | sed 's/ |.*//' | xargs -I{} echo "菜单栏将显示：{}"

open -a SwiftBar
open -g "swiftbar://refreshallplugins" 2>/dev/null || true

echo
echo "✓ 已安装到 ${PLUGIN}（${MODE}）"
if [[ $MODE == stream ]]; then
  echo "  流式模式：插件进程常驻，信号灯顺滑呼吸；想换回每分钟一次的经典模式：bash extras/swiftbar/install.sh --classic"
else
  echo "  经典模式：每分钟刷新一次，信号灯 5 秒一闪；想要顺滑的呼吸灯：bash extras/swiftbar/install.sh --stream"
fi
echo "  菜单栏会出现 Claude 账号图标；点开可查看所有账号额度、一键切换。"
echo "  想开机自动运行：SwiftBar 菜单 → Preferences → 勾选 Launch at Login。"
echo "  卸载：删除上面这个文件即可。"
