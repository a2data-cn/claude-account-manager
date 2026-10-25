"""CI 调试：把测试日志里的失败用例写成 GitHub 注释（::error / ::notice），仓库的公开接口能读到。"""
import re
import sys

log = open(sys.argv[1], encoding="utf-8", errors="replace").read().replace("\r", "")
mode = sys.argv[2] if len(sys.argv) > 2 else "unittest"


def esc(s, limit=3800):
    s = s[:limit]
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


errors, notices = [], []
if mode == "unittest":
    for m in re.finditer(r"^(FAIL|ERROR): .*$", log, re.M):
        errors.append(m.group(0))
    tail = re.search(r"^Ran \d+ tests.*$", log, re.M)
    if tail:
        errors.append(tail.group(0) + " / " + (re.search(r"^(FAILED|OK).*$", log[tail.end():], re.M) or re.match("", "")).group(0))
    blocks = re.split(r"^={20,}$", log, flags=re.M)
    for b in blocks[1:]:
        b = b.strip()
        if b.startswith(("FAIL:", "ERROR:")):
            notices.append(b)
else:
    for m in re.finditer(r"^test .* \.\.\. FAILED$|^thread '.*' panicked at .*$|^test result: FAILED.*$|^error(\[E\d+\])?: .*$", log, re.M):
        errors.append(m.group(0))
    for m in re.finditer(r"^---- (.*) stdout ----\n(.*?)(?=^---- |^failures:$|\Z)", log, re.M | re.S):
        notices.append("---- " + m.group(1) + " ----\n" + m.group(2).strip())
    fl = re.search(r"^failures:\n((?:    .*\n)+)", log, re.M)
    if fl:
        errors.append("failures: " + " ".join(fl.group(1).split()))

if not errors:
    errors.append("没有匹配到失败用例；日志最后 60 行见 notice")
    notices.append("\n".join(log.splitlines()[-60:]))
for e in errors[:10]:
    print(f"::error title=CI failure::{esc(e, 1000)}")
for n in notices[:10]:
    print(f"::notice title=CI detail::{esc(n)}")
