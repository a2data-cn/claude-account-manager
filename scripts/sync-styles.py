"""把 web/panel.html 里的样式同步到桌面版 desktop/src/styles.css（保留桌面版额外样式）。"""
import os, re
root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
html = open(os.path.join(root, "web", "panel.html"), encoding="utf-8").read()
css = re.search(r"<style>\n(.*?)</style>", html, re.S).group(1)
path = os.path.join(root, "desktop", "src", "styles.css")
old = open(path, encoding="utf-8").read()
marker = "/* ---- 桌面版额外样式 ---- */"
extra = old[old.index(marker):] if marker in old else ""
head = "/* 与 web/panel.html 共用同一套样式，改动时请同步（scripts/sync-styles.py 可重新生成前半部分） */\n"
open(path, "w", encoding="utf-8").write(head + css + "\n" + extra)
print("已同步", path)
