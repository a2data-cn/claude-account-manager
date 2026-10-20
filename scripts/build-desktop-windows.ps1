# 一键编译桌面版（Windows）
# 用法：在 PowerShell 里运行  powershell -ExecutionPolicy Bypass -File scripts\build-desktop-windows.ps1
# 需要：Node.js 18+、Rust（脚本会提示安装）、Visual Studio C++ 生成工具、WebView2（Win10/11 一般已自带）
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Log = Join-Path $Root "build.log"

function Fail($msg) { Write-Host "`n✗ $msg" -ForegroundColor Red; Write-Host "  完整日志：$Log"; exit 1 }

# npm 和 cargo 把正常进度也写到 stderr；Windows PowerShell 在 Stop 模式下会把重定向的 stderr 当成错误，
# 第一行输出就中断脚本。所以外部命令在 Continue 模式下跑，只看退出码。
function Run($step, [scriptblock]$cmd) {
  Write-Host "▶ $step"
  $ErrorActionPreference = "Continue"
  & $cmd *>> $Log
  return $LASTEXITCODE
}

Write-Host "▶ 检查 Node.js"
if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
  if (Get-Command winget -ErrorAction SilentlyContinue) { winget install -e --id OpenJS.NodeJS.LTS } else { Fail "需要 Node.js：https://nodejs.org" }
  Fail "Node.js 已安装，请重新打开 PowerShell 再运行本脚本"
}

Write-Host "▶ 检查 Rust"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  if (Get-Command winget -ErrorAction SilentlyContinue) {
    winget install -e --id Rustlang.Rustup
    winget install -e --id Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
    Fail "Rust 和 C++ 生成工具已安装，请重新打开 PowerShell 再运行本脚本"
  }
  Fail "需要 Rust：https://rustup.rs （安装时选择默认选项，会顺带提示安装 C++ 生成工具）"
}

Set-Location (Join-Path $Root "desktop")
if ((Run "安装前端依赖" { npm install }) -ne 0) { Fail "npm install 失败" }

# 清掉上次的产物，否则编译失败时会把旧安装包当成功
Remove-Item -Recurse -Force "src-tauri\target\release\bundle" -ErrorAction SilentlyContinue
if ((Run "编译（第一次较慢）" { npm run tauri build }) -ne 0) { Fail "编译没有成功，请把 build.log 发给我" }
$Installer = Get-ChildItem "src-tauri\target\release\bundle" -Recurse -Include *.msi, *-setup.exe -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $Installer) { Fail "编译没有成功，请把 build.log 发给我" }

Write-Host "`n✓ 安装包：$($Installer.FullName)" -ForegroundColor Green
Start-Process $Installer.FullName
