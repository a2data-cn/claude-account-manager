"""生成项目根目录的 SHA256SUMS：每个文件一行“<sha256>  <路径>”，按路径排序。

收哪些文件：git 管理的文件，加上还没 git add、但也没被项目的 .gitignore 忽略的新文件（也就是提交时会带上的那些），
SHA256SUMS 自己除外。只认项目里的 .gitignore，不看各人电脑上的全局忽略设置，这样在哪台电脑上生成都一样。
改完代码、提交之前运行一次：

    python3 scripts/gen-sums.py           重新生成
    python3 scripts/gen-sums.py --check   只检查是不是最新的，不是就以 1 退出（测试用）

安装脚本和 cam update 用 cam verify 核对；也可以直接用 sha256sum -c SHA256SUMS 或 shasum -a 256 -c SHA256SUMS。
"""
import hashlib
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SUMS = "SHA256SUMS"

if os.name == "nt":  # Windows 上输出到管道时按系统代码页编码，英文系统（cp1252）遇到中文会直接报错；测试和脚本都按 UTF-8 读
    for _stream in (sys.stdout, sys.stderr):
        if not _stream.isatty() and hasattr(_stream, "reconfigure"):
            _stream.reconfigure(encoding="utf-8", errors="replace")


def git_files(*args, root=ROOT):
    out = subprocess.run(["git", "-C", root, "ls-files", "-z", *args], capture_output=True, check=True).stdout
    return {n.decode("utf-8") for n in out.split(b"\0") if n}


def tracked_files(root=ROOT):
    names = git_files("--cached", "--others", "--exclude-per-directory=.gitignore", root=root)
    # 已经删掉但还没提交删除的文件不算；子模块是目录，也不算
    return sorted(n for n in names if n != SUMS and os.path.isfile(os.path.join(root, *n.split("/"))))


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def render(root=ROOT):
    return "".join(f"{sha256(os.path.join(root, *n.split('/')))}  {n}\n" for n in tracked_files(root))


def main(argv):
    path = os.path.join(ROOT, SUMS)
    text = render()
    if "--check" in argv:
        try:
            with open(path, "r", encoding="utf-8", newline="") as f:
                current = f.read()
        except FileNotFoundError:
            current = None
        if current != text:
            print(f"{SUMS} 不是最新的，运行 python3 scripts/gen-sums.py 重新生成", file=sys.stderr)
            return 1
        print(f"{SUMS} 是最新的（{text.count(chr(10))} 个文件）")
        return 0
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    print(f"已写入 {path}（{text.count(chr(10))} 个文件）")
    new = sorted(git_files("--others", "--exclude-per-directory=.gitignore") - {SUMS})
    if new:  # 提交时没带上的话，别人那边校验会报“缺少这个文件”
        print("其中这些还没 git add，提交时记得一起带上：\n  " + "\n  ".join(new))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
