import type { WindowKey } from "./types";

export const KIND: Record<string, string> = { claude_code: "Claude Code", claude_web: "网页版", api: "API" };
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

export function untilText(iso: string | null | undefined): string {
  if (!iso) return "";
  const ms = new Date(iso).getTime() - Date.now();
  if (isNaN(ms)) return "";
  if (ms <= 0) return "已重置";
  const m = Math.round(ms / 60000);
  const h = Math.floor(m / 60);
  const d = Math.floor(h / 24);
  if (d >= 1) return `${d} 天 ${h % 24} 小时后重置`;
  if (h >= 1) return `${h} 小时 ${m % 60} 分后重置`;
  return `${m} 分钟后重置`;
}

export function ago(iso: string | null | undefined): string {
  if (!iso) return "从未";
  const s = (Date.now() - new Date(iso).getTime()) / 1000;
  if (s < 60) return "刚刚";
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`;
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`;
  return `${Math.floor(s / 86400)} 天前`;
}

export function fmtTokens(n: number): string {
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(0) + "k";
  return String(n);
}
