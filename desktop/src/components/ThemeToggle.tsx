import { useEffect, useState } from "react";

type Theme = "system" | "light" | "dark";
const THEMES: [Theme, string, string][] = [
  ["system", "◐", "主题：跟随系统"],
  ["light", "☀", "主题：明亮"],
  ["dark", "☾", "主题：暗黑"],
];

function apply(t: Theme) {
  if (t === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = t;
}

/** 主题切换：跟随系统 → 明亮 → 暗黑，记在本机（和手机面板同样的按钮和顺序） */
export function ThemeToggle({ toast }: { toast: (m: string) => void }) {
  const [theme, setTheme] = useState<Theme>(() => {
    try {
      const t = localStorage.getItem("cam-theme");
      return t === "light" || t === "dark" ? t : "system";
    } catch {
      return "system";
    }
  });
  useEffect(() => apply(theme), [theme]);
  const [, icon, label] = THEMES.find((x) => x[0] === theme) || THEMES[0];
  return (
    <button
      type="button"
      className="btn quiet theme-btn"
      title={label}
      aria-label={label}
      onClick={() => {
        const next = THEMES[(THEMES.findIndex((x) => x[0] === theme) + 1) % THEMES.length];
        setTheme(next[0]);
        try {
          localStorage.setItem("cam-theme", next[0]);
        } catch {
          /* 存不了就只在本次会话里生效 */
        }
        toast(next[2]);
      }}
    >
      {icon}
    </button>
  );
}
