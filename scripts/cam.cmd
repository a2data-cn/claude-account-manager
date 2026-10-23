@echo off
rem cam 启动器（Windows）：把 scripts 目录加入 PATH 后可直接运行 cam
rem 优先用 python；只装了 py 启动器（python.org 安装时没勾 Add python.exe to PATH）时用 py -3
python -c "" >nul 2>nul && goto python
py -3 "%~dp0cam.py" %*
exit /b %errorlevel%
:python
python "%~dp0cam.py" %*
