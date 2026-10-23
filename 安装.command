#!/bin/bash
# macOS：在访达里双击运行。打开终端、进入本文件所在的目录，运行 install.sh（参数原样传过去）。
# 第一次双击如果提示“无法打开”，右键点它选“打开”。
cd "$(dirname "$0")" || exit 1
bash ./install.sh "$@"
status=$?
echo
read -r -n 1 -s -p "按任意键关闭这个窗口…" || true
echo
exit $status
