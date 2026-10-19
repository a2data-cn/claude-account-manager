# 一键编译桌面版（Windows）
# 用法：在 PowerShell 里运行  powershell -ExecutionPolicy Bypass -File scripts\build-desktop-windows.ps1
# 需要：Node.js 18+、Rust（脚本会提示安装）、Visual Studio C++ 生成工具、WebView2（Win10/11 一般已自带）
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Log = Join-Path $Root "build.log"

function Fail($msg) { Write-Host "`n✗ $msg" -ForegroundColor Red; Write-Host "  完整日志：$Log"; exit 1 }

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
Write-Host "▶ 安装前端依赖"
npm install *>> $Log
if ($LASTEXITCODE -ne 0) { Fail "npm install 失败" }

Write-Host "▶ 编译（第一次较慢）"
npm run tauri build *>> $Log
$Installer = Get-ChildItem "src-tauri\target\release\bundle" -Recurse -Include *.msi, *-setup.exe -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $Installer) { Fail "编译没有成功，请把 build.log 发给我" }

Write-Host "`n✓ 安装包：$($Installer.FullName)" -ForegroundColor Green
Start-Process $Installer.FullName
