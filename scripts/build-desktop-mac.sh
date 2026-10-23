#!/bin/bash
# 一键编译并安装桌面版（macOS）
# 用法：bash scripts/build-desktop-mac.sh
# 第一次运行会自动安装 Rust（约 5 分钟），编译约 3–8 分钟。失败时日志在 build.log，发给我即可。
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="$ROOT/build.log"
: > "$LOG"
step() { echo; echo "▶ $*"; }
fail() { echo; echo "✗ $*"; echo "  完整日志：$LOG"; exit 1; }

step "检查 Xcode 命令行工具"
if ! xcode-select -p >/dev/null 2>&1; then
  xcode-select --install || true
  fail "已弹出 Xcode 命令行工具安装窗口，装完后重新运行本脚本"
fi

step "检查 Node.js"
if ! command -v npm >/dev/null 2>&1; then
  if command -v brew >/dev/null 2>&1; then
    brew install node >>"$LOG" 2>&1 || fail "安装 Node.js 失败"
  else
    fail "需要 Node.js 18+：到 https://nodejs.org 下载安装后重新运行"
  fi
fi
node -e 'process.exit(parseInt(process.versions.node) >= 18 ? 0 : 1)' || fail "Node.js 版本太旧，需要 18 以上"

step "检查 Rust"
if ! command -v cargo >/dev/null 2>&1; then
  [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "  正在安装 Rust（只装到当前用户目录）…"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >>"$LOG" 2>&1 \
    || fail "安装 Rust 失败（国内网络可参考 https://rsproxy.cn 配置镜像）"
  source "$HOME/.cargo/env"
fi

cd "$ROOT/desktop"
step "安装前端依赖（npm install）"
npm install >>"$LOG" 2>&1 || fail "npm install 失败（国内网络可先运行：npm config set registry https://registry.npmmirror.com）"

step "编译（第一次较慢，请耐心等待）"
rm -rf src-tauri/target/release/bundle/macos   # 清掉上次的产物，否则编译失败时会把旧 App 当成功装上
npm run tauri build -- --bundles app 2>&1 | tee -a "$LOG" | grep -E "^(error|warning: unused|    Finished|\s+Compiling claude-accounts)" || true
APP="$(ls -d src-tauri/target/release/bundle/macos/*.app 2>/dev/null | head -1)"
[[ -n "$APP" ]] || fail "编译没有成功，请把 build.log 发给我"

step "生成校验文件 SHA256SUMS"
BUNDLE="$(dirname "$APP")"
# .app 是个目录，逐个文件算；路径相对 bundle/macos，在那里运行 shasum -a 256 -c SHA256SUMS 就能核对
( cd "$BUNDLE" && find "$(basename "$APP")" -type f | LC_ALL=C sort | while IFS= read -r f; do shasum -a 256 "$f"; done ) \
  > "$BUNDLE/SHA256SUMS" || fail "生成 SHA256SUMS 失败"
echo "  $BUNDLE/SHA256SUMS"

step "安装到“应用程序”"
NAME="$(basename "$APP")"
# 关掉窗口后 App 还在菜单栏里跑着，先退出旧的，否则替换后打开的仍是旧版本
if pkill -f "/Applications/$NAME/Contents/MacOS/" 2>/dev/null; then sleep 1; fi
rm -rf "/Applications/$NAME"
cp -R "$APP" /Applications/ || fail "复制到 /Applications 失败"
xattr -dr com.apple.quarantine "/Applications/$NAME" 2>/dev/null || true
open "/Applications/$NAME"

echo
echo "✓ 已安装：/Applications/$NAME"
echo "  关掉窗口后它会缩到菜单栏继续运行；托盘菜单里可以一键切换账号。"
