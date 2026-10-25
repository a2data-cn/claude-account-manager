"""CI 调试：把测试日志里的失败用例写成 GitHub 注释（::error / ::notice），仓库的公开接口能读到。

用法：ci-annotate.py <日志> <unittest|cargo> [起始序号]
每个步骤最多 10 条 error、10 条 notice，所以可以分几步、用起始序号分页。
"""
import re
import sys

for _s in (sys.stdout, sys.stderr):  # Windows 英文系统的管道是 cp1252，中文会报错
    if hasattr(_s, "reconfigure"):
        _s.reconfigure(encoding="utf-8", errors="replace")

log = open(sys.argv[1], encoding="utf-8", errors="replace").read().replace("\r", "")
mode = sys.argv[2] if len(sys.argv) > 2 else "unittest"
start = int(sys.argv[3]) if len(sys.argv) > 3 else 0


def esc(s, limit=3800):
    s = s[:limit]
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def squeeze(block, head=2, tail=14):
    """保留开头几行（用例名）和结尾几行（真正的报错），中间的调用栈省略。"""
    lines = block.splitlines()
    if len(lines) <= head + tail + 1:
        return block
    return "\n".join(lines[:head] + ["    …（中间 %d 行省略）…" % (len(lines) - head - tail)] + lines[-tail:])


errors, notices = [], []
if mode == "unittest":
    for m in re.finditer(r"^(FAIL|ERROR): .*$", log, re.M):
        errors.append(m.group(0))
    tail = re.search(r"^Ran \d+ tests.*$", log, re.M)
    if tail:
        verdict = re.search(r"^(FAILED|OK).*$", log[tail.end():], re.M)
        errors.insert(0, tail.group(0) + " / " + (verdict.group(0) if verdict else "?"))
    blocks = re.split(r"^={20,}$", log, flags=re.M)
    for b in blocks[1:]:
        b = b.strip()
        if b.startswith(("FAIL:", "ERROR:")):
            notices.append(squeeze(b))
else:
    for m in re.finditer(r"^test .* \.\.\. FAILED$|^thread '.*' panicked at .*$|^test result: FAILED.*$|^error(\[E\d+\])?: .*$", log, re.M):
        errors.append(m.group(0))
    for m in re.finditer(r"^---- (.*) stdout ----\n(.*?)(?=^---- |^failures:$|\Z)", log, re.M | re.S):
        notices.append(squeeze("---- " + m.group(1) + " ----\n" + m.group(2).strip(), 3, 10))
    fl = re.search(r"^failures:\n((?:    .*\n)+)", log, re.M)
    if fl:
        errors.insert(0, "failures: " + " ".join(fl.group(1).split()))

if not errors and start == 0:
    errors.append("没有匹配到失败用例；日志最后 60 行见 notice")
    notices.append("\n".join(log.splitlines()[-60:]))
for e in errors[start:start + 10]:
    print(f"::error title=CI failure {start}+::{esc(e, 1000)}")
for n in notices[start:start + 10]:
    print(f"::notice title=CI detail {start}+::{esc(n)}")
print(f"共 {len(errors)} 条失败、{len(notices)} 段详情；本步输出第 {start} 到 {start + 9} 条")
