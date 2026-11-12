@echo off
rem cam 启动器（Windows）：把 scripts 目录加入 PATH 后可直接运行 cam
rem 优先用 python；只装了 py 启动器（python.org 安装时没勾 Add python.exe to PATH）时用 py -3
rem 走快入口 cam_run.py：用得上编译好的字节码，每条命令快一百毫秒左右。旁边没有它、或者命令是 serve 时直接运行 cam.py
rem （别的地方靠命令行里的 "cam.py serve" 认出后台在跑的面板服务）
if /i "%~1"=="serve" goto plain
if not exist "%~dp0cam_run.py" goto plain
python -c "" >nul 2>nul && goto fastpython
py -3 "%~dp0cam_run.py" %*
exit /b %errorlevel%
:fastpython
python "%~dp0cam_run.py" %*
exit /b %errorlevel%
:plain
python -c "" >nul 2>nul && goto python
py -3 "%~dp0cam.py" %*
exit /b %errorlevel%
:python
python "%~dp0cam.py" %*
