import { useEffect, useState } from "react";
import { api } from "../api";
import { usd, wan } from "../format";
import { maskMail, usePrefs } from "../prefs";
import type { Account, LocalRow } from "../types";

const COLORS = ["var(--accent)", "var(--ok)", "var(--live)", "var(--violet)", "var(--warn)", "#2D9BB0"]; // 和手机面板同一套
const RANGES: [string, string][] = [["1", "今天"], ["7", "7 天"], ["30", "30 天"]];

function savedRange() {
  try {
    const v = localStorage.getItem("cam-local-range");
    return RANGES.some(([r]) => r === v) ? (v as string) : "7";
  } catch {
    return "7";
  }
}

const cost = (rs: LocalRow[]) => rs.reduce((n, r) => n + (r.cost_usd || 0), 0);

export function LocalUsage({ accounts, stamp }: { accounts: Account[]; stamp: string }) {
  const { mask } = usePrefs();
  const [range, setRange] = useState(savedRange);
  const [rows, setRows] = useState<LocalRow[] | null>(null);

  useEffect(() => {
    api<{ rows: LocalRow[] }>("GET", `/api/local-usage?days=${range}`)
      .then((r) => setRows(r.rows))
      .catch(() => setRows(null));
  }, [stamp, range]);

  const pick = (v: string) => {
    setRange(v);
    try {
      localStorage.setItem("cam-local-range", v);
    } catch {
      /* 存不了就只在这次打开时有效 */
    }
  };

  // 今天只看今天的记录（接口按“最近 N 天”取，会带上昨天的）；7 天 / 30 天画每天的柱子
  const today = new Date().toLocaleDateString("sv-SE");
  const shown = (rows || []).filter((r) => range !== "1" || r.date === today);
  if (!rows || (!shown.length && range !== "1")) return null;
  const names = Object.fromEntries(accounts.map((a) => [a.id, maskMail(a.label || a.email || a.id, mask)]));
  const label = (id: string) => names[id] || "未归属";
  const ids = [...new Set(shown.map((r) => r.account_id))];
  const color = Object.fromEntries(
    ids.map((id, i) => [id, id === "unattributed" ? "var(--muted)" : COLORS[i % COLORS.length]]),
  );
  const sum = (k: "input" | "output" | "cache_write" | "cache_read" | "messages") => shown.reduce((n, r) => n + r[k], 0);
  const days: string[] = [];
  const n = Number(range);
  for (let i = n - 1; i >= 0 && n > 1; i--) days.push(new Date(Date.now() - i * 86400e3).toLocaleDateString("sv-SE"));
  const byDay: Record<string, LocalRow[]> = Object.fromEntries(days.map((d) => [d, []]));
  shown.forEach((r) => byDay[r.date]?.push(r));
  const max = Math.max(0.01, ...days.map((d) => cost(byDay[d])));

  return (
    <section className="block">
      <div className="sec-head">
        <h2>本机 Claude Code 消耗</h2>
        <span className="hint">这台电脑上 claude 的用量，按天、按账号</span>
        <span className="sec-acts">
          <span className="seg-mini" role="group" aria-label="时间范围">
            {RANGES.map(([v, t]) => (
              <button key={v} type="button" aria-pressed={v === range} onClick={() => pick(v)}>
                {t}
              </button>
            ))}
          </span>
        </span>
      </div>
      <div className="cards local">
        <div className="panel ecard wide">
          <h3>
            <span className="eico">💰</span>汇总<span className="hint">金额按 API 价折算，订阅不按这个收费，只用来比较用量大小</span>
          </h3>
          <div className="local-sum">
          <b>≈ {usd(cost(shown))}</b>
          <span>
            {shown.length
              ? `输出 ${wan(sum("output"))} · 输入和缓存写入 ${wan(sum("input") + sum("cache_write"))} · 缓存读取 ${wan(sum("cache_read"))} tokens · ${sum("messages")} 条消息`
              : "今天还没有用过"}
          </span>
        </div>
          <div className="legend">
            {ids.map((id) => (
              <span key={id}>
                <i style={{ background: color[id] }} />
                {label(id)} <b>{usd(cost(shown.filter((r) => r.account_id === id)))}</b>
              </span>
            ))}
          </div>
        </div>
        {days.length > 0 && (
          <div className="panel ecard wide">
            <h3>
              <span className="eico">📅</span>每天的折算金额<span className="hint">按账号叠，悬停看明细</span>
            </h3>
            <div className="bars" role="img" aria-label="每日消耗柱状图">
              {days.map((d) => {
                const tip =
                  d + "\n" + (byDay[d].length ? byDay[d].map((r) => `${label(r.account_id)}：${usd(r.cost_usd || 0)} · 输出 ${wan(r.output)}`).join("\n") : "无");
                return (
                  <div className="day" key={d} data-tip={tip}>
                    {byDay[d].map((r) => (
                      <div key={r.account_id} className="seg" style={{ height: `${((r.cost_usd || 0) / max) * 100}%`, background: color[r.account_id] }} />
                    ))}
                  </div>
                );
              })}
            </div>
            <div className="axis">
              <span>{days[0].slice(5)}</span>
              <span>单日最多 {usd(max)}</span>
              <span>{days[days.length - 1].slice(5)}</span>
            </div>
          </div>
        )}
      </div>
      <p className="hint" style={{ margin: "10px 0 0" }}>
        按切换记录把每条消息归到当时登录的账号；切换前产生的记录显示为“未归属”。
      </p>
    </section>
  );
}
