import { useEffect, useState } from "react";
import { api } from "../api";
import { fmtTokens } from "../format";
import type { Account, LocalRow } from "../types";

const COLORS = ["var(--ok)", "var(--live)", "var(--warn)", "#8B6BD6", "#2D9BB0", "#C0568F"];

export function LocalUsage({ accounts, stamp }: { accounts: Account[]; stamp: string }) {
  const [rows, setRows] = useState<LocalRow[] | null>(null);

  useEffect(() => {
    api<{ rows: LocalRow[] }>("GET", "/api/local-usage?days=14")
      .then((r) => setRows(r.rows))
      .catch(() => setRows(null));
  }, [stamp]);

  if (!rows || !rows.length) return null;
  const names = Object.fromEntries(accounts.map((a) => [a.id, a.label || a.email || a.id]));
  const ids = [...new Set(rows.map((r) => r.account_id))];
  const color = Object.fromEntries(
    ids.map((id, i) => [id, id === "unattributed" ? "var(--muted)" : COLORS[i % COLORS.length]]),
  );
  const days: string[] = [];
  for (let i = 13; i >= 0; i--) days.push(new Date(Date.now() - i * 86400e3).toLocaleDateString("sv-SE"));
  const byDay: Record<string, LocalRow[]> = Object.fromEntries(days.map((d) => [d, []]));
  rows.forEach((r) => byDay[r.date]?.push(r));
  const max = Math.max(1, ...days.map((d) => byDay[d].reduce((s, r) => s + r.output, 0)));
  const label = (id: string) => names[id] || "未归属";

  return (
    <section className="block">
      <h2>本机 Claude Code 消耗（近 14 天，输出 token）</h2>
      <div className="panel">
        <p className="hint">按切换记录把每条消息归到当时登录的账号；切换前产生的记录显示为“未归属”。</p>
        <div className="bars" role="img" aria-label="每日输出 token 柱状图">
          {days.map((d) => {
            const tip = d + "\n" + (byDay[d].length ? byDay[d].map((r) => `${label(r.account_id)}：${fmtTokens(r.output)}`).join("\n") : "无");
            return (
              <div className="day" key={d} data-tip={tip}>
                {byDay[d].map((r) => (
                  <div key={r.account_id} className="seg" style={{ height: `${(r.output / max) * 100}%`, background: color[r.account_id] }} />
                ))}
              </div>
            );
          })}
        </div>
        <div className="axis">
          <span>{days[0].slice(5)}</span>
          <span>峰值 {fmtTokens(max)}</span>
          <span>{days[13].slice(5)}</span>
        </div>
        <div className="legend">
          {ids.map((id) => (
            <span key={id}>
              <i style={{ background: color[id] }} />
              {label(id)}
            </span>
          ))}
        </div>
      </div>
    </section>
  );
}
