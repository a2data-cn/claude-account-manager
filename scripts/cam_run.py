#!/usr/bin/env python3
"""cam 的快入口：做的事和直接运行 cam.py 一样，只是启动快一百毫秒左右。

直接运行 cam.py 时，Python 每次都要把那八千行重新编译一遍：被直接运行的脚本用不上 __pycache__ 里编译好的字节码。
从这里把 cam 当模块来跑就用得上（第一次运行、或 cam.py 改过之后的第一次，照常编译一遍并存起来）。
启动器 scripts/cam 和 cam.cmd 除了 serve 都走这里；没有这个文件就直接运行 cam.py。
cam serve 不走这里：别的地方靠命令行里的 "cam.py serve" 认出后台在跑的面板服务。
"""
import os
import runpy
import sys

if __name__ == "__main__":
    here = os.path.dirname(os.path.realpath(__file__))
    if here not in sys.path[:1]:
        sys.path.insert(0, here)
    runpy.run_module("cam", run_name="__main__", alter_sys=True)
