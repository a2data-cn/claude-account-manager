#!/bin/bash
# 一键安装 cam（macOS / Linux）。macOS 上也可以在访达里双击“安装.command”。
# 用法：bash install.sh [--yes] [--skip-verify] [--uninstall]
#   --yes          不提问，全部按默认选择（菜单栏小工具装；Claude Code 和后台服务不装）
#   --skip-verify  跳过 SHA256SUMS 文件校验（自己改过代码时用）
#   --uninstall    卸载：去掉 cam 命令、加进 shell 配置的那一行、菜单栏插件、后台服务
# 可以重复运行，装好的部分会跳过。账号数据（~/.claude-accounts）和 Claude Code 配置（~/.claude）都不会动。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
YES=0
VERIFY=1
UNINSTALL=0
for arg in "$@"; do
  case "$arg" in
    -y|--yes) YES=1 ;;
    --skip-verify) VERIFY=0 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "不认识的参数：$arg（bash install.sh --help 查看用法）"; exit 2 ;;
  esac
done

OS="$(uname)"
BIN_DIR="$HOME/.local/bin"
LINK="$BIN_DIR/cam"
TARGET="$ROOT/scripts/cam"
MARK="# cam：由 claude-account-manager 的 install.sh 添加"  # 卸载时按这个标记找回加进 shell 配置的那一行
PATH_LINE='export PATH="$HOME/.local/bin:$PATH"'
UNIT="cam-serve.service"
UNIT_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$UNIT"
MENU_PY="$ROOT/extras/swiftbar/cam_menu.py"

step() { echo; echo "▶ $*"; }
ok() { echo "  ✓ $*"; }
note() { echo "  · $*"; }
warn() { echo "  ! $*"; }
fail() { echo; echo "✗ $*"; exit 1; }

# ask "问题" y|n：直接回车用默认值；--yes 时不问，直接用默认值。没有终端时从标准输入读一行，读不到也用默认值
ask() {
  local def="$2" ans
  if [[ $YES == 1 ]]; then
    echo "  $1 → $([[ $def == y ]] && echo 是 || echo 否)（--yes，按默认）"
    [[ $def == y ]]
    return
  fi
  printf '  %s %s ' "$1" "$([[ $def == y ]] && echo '[Y/n]' || echo '[y/N]')"
  read -r ans || ans=""
  [[ -t 0 ]] || echo "$ans"
  case "$(printf '%s' "${ans:-$def}" | tr '[:upper:]' '[:lower:]')" in
    y|yes|是) return 0 ;;
    *) return 1 ;;
  esac
}

# 当前用户的 shell 配置文件：zsh → ~/.zshrc；bash → macOS 上 ~/.bash_profile（终端开的是登录 shell），Linux 上 ~/.bashrc
shell_rc() {
  case "$(basename "${SHELL:-}")" in
    zsh) echo "${ZDOTDIR:-$HOME}/.zshrc" ;;
    bash) if [[ $OS == Darwin ]]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi ;;
    *) echo "" ;;
  esac
}

swiftbar_plugin() {
  local dir="${CAM_SWIFTBAR_DIR:-}"  # 测试时指到临时目录
  [[ -n "$dir" ]] || dir="$(defaults read com.ameba.SwiftBar PluginDirectory 2>/dev/null || true)"
  dir="${dir:-$HOME/Library/Application Support/SwiftBar/Plugins}"
  echo "${dir/#\~/$HOME}/claude-accounts.1m.sh"
}

# 后台 cam serve 的 systemd 用户服务：不加 --lan，只监听 127.0.0.1
unit_text() {
  cat <<EOF
[Unit]
Description=cam 管理面板和后台任务（只限本机访问）
After=network-online.target

[Service]
ExecStart="$PY" "$ROOT/scripts/cam.py" serve
Restart=on-failure
RestartSec=30
Environment=PYTHONUNBUFFERED=1

[Install]
WantedBy=default.target
EOF
}

uninstall() {
  echo "卸载 cam（账号数据 ~/.claude-accounts 和 Claude Code 配置都不动）"
  step "cam 命令"
  if [[ -L "$LINK" ]]; then
    local dest
    dest="$(readlink "$LINK")"
    if [[ "$dest" == "$TARGET" || ( "$dest" == */scripts/cam && ! -e "$LINK" ) ]]; then
      rm -f "$LINK"
      ok "已删除 $LINK"
    else
      warn "$LINK 指向 $dest，不是这份代码装的，没有删"
    fi
  elif [[ -e "$LINK" ]]; then
    warn "$LINK 不是 install.sh 装的，没有删"
  else
    note "没有 $LINK"
  fi
  local rc tmp removed=0
  for rc in "${ZDOTDIR:-$HOME}/.zshrc" "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile"; do
    [[ -f "$rc" ]] && grep -qF "$MARK" "$rc" || continue
    tmp="$(mktemp)"
    grep -vF "$MARK" "$rc" > "$tmp" || true
    cat "$tmp" > "$rc"  # 写回原文件，保留权限；配置文件是软链接（dotfiles）时改的是链接指向的文件
    rm -f "$tmp"
    ok "已从 $rc 去掉 install.sh 加的那一行 PATH"
    removed=1
  done
  [[ $removed == 1 ]] || note "shell 配置里没有 install.sh 加的内容"

  if [[ $OS == Darwin ]]; then
    step "菜单栏小工具"
    local plugin
    plugin="$(swiftbar_plugin)"
    if [[ -f "$plugin" ]]; then
      rm -f "$plugin"
      open -g "swiftbar://refreshallplugins" 2>/dev/null || true
      ok "已删除插件 $plugin（SwiftBar 本身留着，不用了可以把它从“应用程序”里删掉）"
    else
      note "没装菜单栏插件"
    fi
  fi

  if [[ -f "$UNIT_FILE" ]]; then
    step "后台服务"
    systemctl --user disable --now "$UNIT" >/dev/null 2>&1 || true
    rm -f "$UNIT_FILE"
    systemctl --user daemon-reload >/dev/null 2>&1 || true
    ok "已停止并删除 $UNIT"
  fi

  echo
  echo "✓ 卸载完成。代码目录 $ROOT 和账号数据 ~/.claude-accounts 都还在；数据目录里有各账号的登录凭据，确定不要了再自己删。"
}

if [[ $UNINSTALL == 1 ]]; then
  uninstall
  exit 0
fi

echo "安装 cam（Claude 多账号管理）：$ROOT"

# ---- 1. Python：后面的校验和 cam 本身都要用
step "检查 Python"
PY="$(command -v python3 || true)"
if [[ -z "$PY" ]]; then
  if [[ $OS == Darwin ]]; then
    fail "没找到 python3。到 https://www.python.org/downloads/ 下载安装（或者 brew install python），装好后重新运行"
  fi
  fail "没找到 python3。用系统的包管理器装一个（例如 sudo apt install python3），装好后重新运行"
fi
if ! "$PY" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' >/dev/null 2>&1; then
  [[ $OS == Darwin ]] && note "macOS 第一次运行 python3 可能弹窗要装“命令行开发者工具”，装完重新运行本脚本"
  fail "Python 版本太旧或者不能用（$("$PY" -V 2>&1 | head -1)），需要 3.8 以上"
fi
ok "$("$PY" -V 2>&1)（$PY）"

# ---- 2. 校验文件：安装任何东西之前先确认代码完整、没被改过
step "校验文件（SHA256SUMS）"
if [[ $VERIFY == 0 ]]; then
  warn "按 --skip-verify 跳过了校验"
elif ! "$PY" "$ROOT/scripts/cam.py" verify; then
  fail "文件校验没通过，安装停止，什么都没改。重新下载完整的代码再装；上面列出的文件如果是你自己改的，加 --skip-verify 跳过"
fi

# ---- 3. cam 命令：软链接到代码目录，git pull / cam update 之后自动就是新版本
step "安装 cam 命令"
chmod +x "$TARGET" 2>/dev/null || true
mkdir -p "$BIN_DIR"
if [[ -L "$LINK" && "$(readlink "$LINK")" == "$TARGET" ]]; then
  ok "已经装好了：$LINK → $TARGET"
elif [[ -e "$LINK" && ! -L "$LINK" ]]; then
  warn "$LINK 已经存在，而且不是 install.sh 装的，没有覆盖。先删掉它再重新运行；在那之前可以直接用 $TARGET"
else
  [[ -L "$LINK" ]] && note "原来指向 $(readlink "$LINK")，改成这份代码"
  ln -sfn "$TARGET" "$LINK"
  ok "$LINK → $TARGET"
fi
NEW_SHELL=0
case ":$PATH:" in
  *":$BIN_DIR:"*) ok "$BIN_DIR 已经在 PATH 里" ;;
  *)
    NEW_SHELL=1
    RC="$(shell_rc)"
    if [[ -z "$RC" ]]; then
      warn "$BIN_DIR 不在 PATH 里。当前 shell（${SHELL:-未知}）请自己在配置里加一行：$PATH_LINE"
    elif [[ -f "$RC" ]] && grep -qF "$MARK" "$RC"; then
      ok "$RC 里已经加过 PATH 了，新开的终端生效"
    else
      [[ -s "$RC" && -n "$(tail -c 1 "$RC")" ]] && echo >> "$RC"  # 原文件最后一行没换行时先补一个
      printf '%s  %s\n' "$PATH_LINE" "$MARK" >> "$RC"
      ok "在 $RC 末尾加了一行，把 $BIN_DIR 加进 PATH（新开的终端生效）"
    fi
    ;;
esac

# ---- 4. Claude Code
step "检查 Claude Code"
CLAUDE="$(command -v claude || true)"
[[ -z "$CLAUDE" && -x "$HOME/.local/bin/claude" ]] && CLAUDE="$HOME/.local/bin/claude"
if [[ -n "$CLAUDE" ]]; then
  ok "已经装好了：$CLAUDE"
else
  note "没找到 claude 命令。cam 切换的是 Claude Code 的登录，要先装 Claude Code"
  if ask "现在用 Anthropic 官方安装器装 Claude Code 吗？" n; then
    "$PY" "$ROOT/scripts/cam.py" env install -y || warn "没装成，见上面的输出；以后可以运行 cam env install 再试"
  else
    note "以后需要时运行 cam env install"
  fi
fi

# ---- 5. macOS：菜单栏小工具
if [[ $OS == Darwin ]]; then
  step "菜单栏小工具（SwiftBar 插件）"
  PLUGIN="$(swiftbar_plugin)"
  if [[ -f "$PLUGIN" ]] && grep -qF "$MENU_PY" "$PLUGIN"; then
    ok "已经装好了：$PLUGIN"
  else
    note "菜单栏常驻显示当前账号和 5 小时额度，点一下切换；顺带在后台保存新登录、执行时间规则、刷新用量、额度提醒"
    if ask "安装菜单栏小工具吗？（没有 SwiftBar 时会用 Homebrew 装）" y; then
      bash "$ROOT/extras/swiftbar/install.sh" || warn "菜单栏小工具没装成，见上面的输出；以后可以运行 bash extras/swiftbar/install.sh 再试"
    else
      note "以后想装：bash extras/swiftbar/install.sh"
    fi
  fi
fi

# ---- 6. Linux：后台 cam serve（默认不装，要问过才装）
if [[ $OS == Linux ]]; then
  step "后台服务（可选）"
  if [[ -f "$UNIT_FILE" ]]; then
    if [[ "$(unit_text)" != "$(cat "$UNIT_FILE")" ]]; then  # 代码目录或 Python 换了位置
      unit_text > "$UNIT_FILE"
      systemctl --user daemon-reload >/dev/null 2>&1 || true
      systemctl --user try-restart "$UNIT" >/dev/null 2>&1 || true
      note "已按这份代码更新 $UNIT_FILE"
    fi
    ok "已经装好了：$UNIT（systemctl --user status $UNIT 查看）"
  elif ! command -v systemctl >/dev/null 2>&1; then
    note "这台电脑没有 systemd，不装后台服务。需要时运行 cam serve 或 cam panel"
  else
    note "后台服务会一直运行一个只限本机访问的 cam serve：自动保存新登录、执行时间规则、定时刷新用量、额度快用完时提醒"
    if ask "要装成登录后自动运行的后台服务吗？" n; then
      mkdir -p "$(dirname "$UNIT_FILE")"
      unit_text > "$UNIT_FILE"
      if systemctl --user daemon-reload && systemctl --user enable --now "$UNIT"; then
        ok "已装好并启动 $UNIT（只监听 127.0.0.1:8787；cam panel 打开面板，systemctl --user status $UNIT 查看）"
      else
        warn "服务文件写好了（$UNIT_FILE），但没能启动，见上面的输出。可以先手动运行 cam serve"
      fi
    else
      note "没装。需要时运行 cam serve 或 cam panel；以后想装，重新运行一次 install.sh"
    fi
  fi
fi

# ---- 7. 环境检查
step "环境检查（cam doctor，会联网测一下 Claude 服务，要几秒钟）"
DOCTOR="$("$PY" "$ROOT/scripts/cam.py" doctor 2>&1)" || true
echo "  ${DOCTOR%%$'\n'*}"
PROBLEMS="$(printf '%s\n' "$DOCTOR" | grep -E '^(✗|!) ' || true)"
if [[ -n "$PROBLEMS" ]]; then
  printf '%s\n' "$PROBLEMS" | sed 's/^/  /'
  note "上面几项要留意，运行 cam doctor 看完整结果和建议"
else
  ok "没发现问题（完整结果：cam doctor）"
fi

echo
echo "✓ 安装完成"
echo
echo "下一步："
[[ $NEW_SHELL == 1 ]] && echo "  · 新开一个终端，cam 命令才找得到（或者在当前终端先运行 $PATH_LINE）"
echo "  · cam login 工作号    登录一个 Claude 账号，浏览器里登录完自动保存；每个账号换个名字各登录一次"
echo "  · cam                 列出所有账号和额度；cam switch 名字 切换"
echo "  · cam panel           在浏览器打开管理面板"
echo "  · 更新：cam update（只在你运行时更新）；卸载：bash install.sh --uninstall"
