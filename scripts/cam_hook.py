#!/usr/bin/env python3
"""Claude Code 钩子的快入口：做的事和 `cam.py hook` 一样，只是快。

直接运行 cam.py 时，Python 每次都要把那八千行重新编译一遍（一百毫秒左右）：被直接运行的脚本用不上 __pycache__ 里
编译好的字节码。从这里把 cam 当模块来跑就用得上；cam.py 开头的快路径记完事件就退出，后面的东西不加载。
钩子每次工具调用都跑一遍，所以值得多这一个文件。没有它也行：cam hooks install 会改用 cam.py hook。

什么都不输出、永远返回 0，不能影响 Claude Code。
"""
import os
import runpy
import sys

if __name__ == "__main__":
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path[:1]:
        sys.path.insert(0, here)
    sys.argv[1:] = ["hook"]
    try:
        runpy.run_module("cam", run_name="__main__")
    except BaseException:  # noqa: BLE001 - 包括 cam 自己的 sys.exit：反正都是返回 0
        pass
    sys.exit(0)
