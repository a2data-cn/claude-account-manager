import type { WindowKey } from "./types";

export const KIND: Record<string, string> = {
  claude_code: "Claude Code",
  claude_desktop: "桌面版",
  claude_web: "网页版",
  api: "API",
};
export const PLAN: Record<string, string> = {
  pro: "Pro", max: "Max", team: "Team", enterprise: "Enterprise", free: "Free", api: "API",
};

export const WINDOW_MS: Record<WindowKey, number> = {
  five_hour: 5 * 3600e3,
  seven_day: 7 * 86400e3,
  seven_day_opus: 7 * 86400e3,
  seven_day_sonnet: 7 * 86400e3,
};
export const WINDOW_NAME: Record<WindowKey, string> = {
  five_hour: "5 小时",
  seven_day: "7 天",
  seven_day_opus: "7 天 Opus",
  seven_day_sonnet: "7 天 Sonnet",
};
export const WINDOW_KEYS: WindowKey[] = ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet"];

export function dur(ms: number): string {
  const m = Math.max(1, Math.round(ms / 60000));
  const h = Math.floor(m / 60);
  const d = Math.floor(h / 24);
  if (d >= 1) return `${d} 天 ${h % 24} 小时`;
  if (h >= 1) return `${h} 小时 ${m % 60} 分`;
  return `${m} 分钟`;
}

/** “10月25日 08:00 重置 · 3 天 19 小时后”，今天之内省掉日期 */
export function resetText(iso: string | null | undefined): string {
  if (!iso) return "";
  const t = new Date(iso);
  const ms = t.getTime() - Date.now();
  if (isNaN(ms)) return "";
  if (ms <= 0) return "已重置";
  const day = t.toDateString() === new Date().toDateString() ? "" : `${t.getMonth() + 1}月${t.getDate()}日 `;
  return `${day}${t.toTimeString().slice(0, 5)} 重置 · ${dur(ms)}后`;
}

export function ago(iso: string | null | undefined): string {
  if (!iso) return "从未";
  const s = (Date.now() - new Date(iso).getTime()) / 1000;
  if (s < 60) return "刚刚";
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return `${Math.floor(s / 86400)} 天前`;
}

export const usd = (n: number) => "$" + (n >= 100 ? Math.round(n).toLocaleString("en-US") : n.toFixed(2));

export const wan = (n: number) =>
  n >= 1e8 ? (n / 1e8).toFixed(1) + " 亿" : n >= 1e5 ? Math.round(n / 1e4) + " 万" : n >= 1e4 ? (n / 1e4).toFixed(1) + " 万" : String(Math.round(n));

export const BREAKDOWN: Record<string, string> = { claude_code: "Claude Code", chat: "网页聊天", cowork: "Cowork", other: "其它" };

/** Max 的档位（5x / 20x）在接口给的 rate_limit_tier 里 */
export function planTier(tier: string | null | undefined, plan: string): string {
  const m = plan === "max" ? /max_(\d+)x/.exec(tier || "") : null;
  return m ? ` ${m[1]}x` : "";
}
