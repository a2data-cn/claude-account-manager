@echo off
rem Windows：双击运行，用 PowerShell 执行同一目录下的 install.ps1
rem 参数原样传过去，例如 安装.cmd -Yes 或 安装.cmd -Uninstall
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" %*
set code=%errorlevel%
echo.
pause
exit /b %code%
