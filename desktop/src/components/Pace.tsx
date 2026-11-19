import type { Account, Pace, SessionsBrief, Window5 } from "../types";
import { KIND, dur, resetText } from "../format";
import { maskMail, usePrefs } from "../prefs";
import { Ico } from "./Icon";

/** 额度环、7 天额度的节奏、一句结论、到期。和网页面板 web/panel.html 一个规则：
 *  节奏是后端按采样点算好的 a.pace（usage_pace），节奏线的点来自 /api/usage-samples */

const ALERT_SOON_DAYS = 3; // 到期日前几天标黄（和后台提醒一样）
const FULL_AT = 99.5; // 用到这么多算用满了（和后台提醒一样）
const WEEKDAY = "日一二三四五六";
const pad2 = (n: number) => String(n).padStart(2, "0");
export type Points = [number, number][];

/** 卡片上的时间：今天“19:00”，明天“明天 09:00”，一周内“周日 14:00”，再远“11-28 09:00”。t 是秒或 ISO 时间 */
export function whenShort(t: number | string | null | undefined): string {
  const d = typeof t === "number" ? new Date(t * 1000) : new Date(t || "");
  if (isNaN(d.getTime())) return "";
  const hm = `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  const now = new Date();
  const days = Math.round((new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime() - new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()) / 864e5);
  if (days === 0) return hm;
  if (days === 1) return `明天 ${hm}`;
  if (days > 1 && days < 7) return `周${WEEKDAY[d.getDay()]} ${hm}`;
  return `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${hm}`;
}

const levelCls = (pct: number) => (pct >= 90 ? "hot" : pct >= 70 ? "warn" : "");
const desktopUsage = (a: Account) => a.usage?.source === "desktop_cache" || a.usage?.source === "desktop_history";
function windowGone(a: Account, w: Window5, spanMs: number) {
  if (a.kind !== "claude_desktop") return false;
  if (w.resets_at) return new Date(w.resets_at).getTime() <= Date.now();
  const sampled = new Date(a.usage?.fetched_at || "").getTime();
  return !Number.isFinite(sampled) || Date.now() - sampled >= spanMs;
}

export interface RingInfo {
  label: string;
  w: Window5;
  pct: number;
}

/** 卡片上的额度环：5 小时、7 天，再加用得最多的那个按模型的周额度（如 Fable）。桌面号过了重置时间的窗口不画 */
export function quotaRings(a: Account): RingInfo[] {
  const u = a.usage || {};
  const out: RingInfo[] = [];
  if (a.kind === "claude_desktop" && !desktopUsage(a)) return out;
  const add = (label: string, w: Window5 | null | undefined, spanMs: number) => {
    if (!w || typeof w.utilization !== "number" || windowGone(a, w, spanMs)) return;
    out.push({ label, w, pct: Math.max(0, Math.min(100, w.utilization)) });
  };
  add("5h", u.five_hour, 5 * 3600e3);
  add("7d", u.seven_day, 7 * 86400e3);
  const top = [...(u.models || [])].filter((m) => typeof m.utilization === "number").sort((x, y) => y.utilization - x.utilization)[0];
  if (top) add(top.name, top, 7 * 86400e3);
  return out;
}

export function RingSvg({ pct, cls }: { pct: number; cls: string }) {
  const r = 18;
  const c = 2 * Math.PI * r;
  return (
    <svg className="ring" viewBox="0 0 44 44" aria-hidden="true">
      <circle className="rt" cx="22" cy="22" r={r} />
      <circle className={`rf ${cls}`} cx="22" cy="22" r={r} strokeDasharray={c.toFixed(2)} strokeDashoffset={(c * (1 - pct / 100)).toFixed(2)} />
      <text x="22" y="22" textAnchor="middle" dominantBaseline="central" style={Math.round(pct) >= 100 ? { fontSize: "9.5px" } : undefined}>
        {Math.round(pct)}
        <tspan>%</tspan>
      </text>
    </svg>
  );
}

/** 环 + 下面它的名字和几点重置 */
export function RingBox({ r }: { r: RingInfo }) {
  const { remain } = usePrefs();
  const when = r.w.resets_at ? whenShort(r.w.resets_at) : r.pct === 0 ? "未开始" : "–";
  return (
    <span className="rg" title={`${r.label} 已用 ${Math.round(r.pct)}%${r.w.resets_at ? "，" + resetText(r.w.resets_at) : ""}`}>
      <RingSvg pct={remain ? 100 - r.pct : r.pct} cls={levelCls(r.pct)} />
      <span className="rl">{r.label}</span>
      <span className="rw">{when}</span>
    </span>
  );
}

/** 当前 Claude Code 号：这台电脑上几个会话开着、几个在回答、几个等你确认 */
export function SessionLine({ a, sessions }: { a: Account; sessions?: SessionsBrief | null }) {
  const n = sessions?.count || 0;
  if (!a.is_active || a.kind !== "claude_code" || n <= 0) return null;
  const busy = !!sessions?.busy || !!sessions?.waiting;
  const what = [
    sessions?.answering ? `${sessions.answering} 个在回答` : sessions?.busy ? "正在回答" : "",
    sessions?.waiting ? `${sessions.waiting} 个等你确认` : "",
  ]
    .filter(Boolean)
    .join(" · ");
  return (
    <span className={`t-sess${busy ? " busy" : ""}`}>
      <i />
      {n} 个会话{busy ? "在跑 · " + what : "开着 · 空闲"}
    </span>
  );
}

export interface Line {
  cls: "" | "warn" | "hot";
  text: string;
}

/** 一句结论：用满了几点恢复 / 按现在的速度什么时候用完 / 够用。long：详情和汇总里的长说法 */
export function verdict(a: Account, long = false): Line | null {
  const u = a.usage || {};
  const p = a.pace;
  const full = (w?: Window5 | null) => !!w && typeof w.utilization === "number" && w.utilization >= FULL_AT;
  if (full(u.five_hour) && !windowGone(a, u.five_hour as Window5, 5 * 3600e3))
    return { cls: "hot", text: `5 小时${long ? "额度" : ""}用满${u.five_hour?.resets_at ? ` · ${whenShort(u.five_hour.resets_at)} 恢复` : ""}` };
  const m = (u.models || []).find(full);
  if (m) return { cls: "hot", text: `${m.name} 用满${m.resets_at ? ` · ${whenShort(m.resets_at)} 刷新` : ""}` };
  if (!p) return null;
  const by = p.basis === "recent" ? "最近一天" : "这周平均";
  if (p.state === "full") return { cls: "hot", text: `7 天${long ? "额度" : ""}用满 · ${whenShort(p.resets_at)} 恢复` };
  if (p.state === "out" && p.out_at !== null)
    return {
      cls: "warn",
      text: long ? `按${by}的速度，7 天额度 ${whenShort(p.out_at)} 左右用完，比重置早 ${dur((p.early_secs || 0) * 1000)}` : `7 天 ${whenShort(p.out_at)} 左右用完`,
    };
  if (p.state === "ok") return { cls: "", text: long ? `按${by}的速度用不完，重置时大约还剩 ${p.left}%` : `够用 · 重置时约剩 ${p.left}%` };
  return { cls: "", text: long ? "这个 7 天窗口刚开始，用一段时间以后再估什么时候用完" : "7 天窗口刚开始" };
}

/** 到期：3 天内标黄、过了标红、自动续期写每月几号；没记到期日不显示 */
export function expiryLine(a: Account): Line | null {
  const d = a.days_left;
  const m = a.manual || {};
  if (d === null || d === undefined) return null;
  const t = m.expires_at ? new Date(`${String(m.expires_at).slice(0, 10)}T00:00:00`) : new Date(a.auto?.subscription_expires_at || "");
  const ok = !isNaN(t.getTime());
  const date = ok ? `${pad2(t.getMonth() + 1)}-${pad2(t.getDate())}` : "";
  if (m.auto_renew) return { cls: "", text: `自动续期${ok ? (m.cycle === "yearly" ? ` · 每年 ${date}` : ` · 每月 ${t.getDate()} 日`) : ""}` };
  if (d < 0) return { cls: "hot", text: `已过到期日${date ? `（${date}）` : ""}` };
  if (d <= ALERT_SOON_DAYS) return { cls: "warn", text: d === 0 ? `今天到期${date ? `（${date}）` : ""}` : `到期 ${date} · 还有 ${d} 天` };
  return { cls: "", text: `到期 ${date}` };
}

/** 7 天额度的节奏线：虚线是匀速用到重置正好用完，实线是实际已用（采样点 + 现在这个数），比匀速快的部分涂浅黄，
 *  点线按速度往后推（重置前用完的标黄、画到用完那一点；用不完的画到重置时还剩多少）。big：详情里带坐标轴的大图 */
export function PaceSvg({ a, points, big = false }: { a: Account; points?: Points; big?: boolean }) {
  const p = a.pace as Pace;
  // 大图按窗口宽度画：窗口很窄时 800 宽的图缩下去字就看不清了，这时画得和窗口一样宽、高一点（和网页面板一样）
  const W = big ? (window.innerWidth > 720 ? 800 : Math.max(280, Math.round(window.innerWidth - 66))) : 240;
  const H = big ? 230 : 34;
  const pad = big ? { l: 40, r: 16, t: 16, b: 26 } : { l: 3, r: 5, t: 4, b: 4 };
  const span = p.resets_at - p.start;
  const even = (t: number) => ((t - p.start) / span) * 100;
  const X = (t: number) => pad.l + Math.max(0, Math.min(1, (t - p.start) / span)) * (W - pad.l - pad.r);
  const Y = (v: number) => pad.t + (1 - Math.max(0, Math.min(100, v)) / 100) * (H - pad.t - pad.b);
  const xy = (t: number, v: number) => `${X(t).toFixed(1)},${Y(v).toFixed(1)}`;
  const now = Date.now() / 1000;
  const got = Date.parse(a.usage?.fetched_at || "") / 1000;
  const at = Number.isFinite(got) && got <= now && got >= p.start ? got : now;
  const pts: Points = (points || []).filter(([t, v]) => t >= p.start && t < at && typeof v === "number").concat([[at, p.used]]);
  const ink = p.state === "out" || p.state === "full" ? "var(--warn)" : "var(--ink)";
  // 比匀速快的部分：实线在虚线上面的那几段，和虚线围起来（在交点处切开）
  const runs: Points[] = [];
  let run: Points = [];
  pts.forEach(([t, v], i) => {
    const dv = v - even(t);
    if (i > 0) {
      const [t0, v0] = pts[i - 1];
      const d0 = v0 - even(t0);
      if (d0 > 0 !== dv > 0 && d0 !== dv) {
        const tc = t0 + (d0 / (d0 - dv)) * (t - t0);
        const c: [number, number] = [tc, even(tc)];
        if (d0 > 0) {
          run.push(c);
          runs.push(run);
          run = [];
        } else run = [c];
      }
    }
    if (dv > 0) run.push([t, v]);
  });
  if (run.length) runs.push(run);
  const end: [number, number] | null = p.state === "out" && p.out_at !== null ? [p.out_at, 100] : p.state === "ok" && p.left !== null ? [p.resets_at, 100 - p.left] : null;
  const right = X(at) > W * 0.7;
  const anchor = right ? "end" : "start";
  const dx = right ? -9 : 9;
  const above = p.used > even(at);
  return (
    <svg viewBox={`0 0 ${W} ${H}`} role="img" aria-label={`7 天额度的节奏：已用 ${Math.round(p.used)}%，按匀速应该在 ${Math.round(p.even)}%`}>
      {big &&
        [0, 25, 50, 75, 100].map((g) => (
          <g key={g}>
            <line x1={pad.l} x2={W - pad.r} y1={Y(g)} y2={Y(g)} stroke="var(--line)" strokeDasharray={g ? "2 4" : undefined} />
            <text x={pad.l - 7} y={Y(g) + 4} fontSize="11" fill="var(--muted)" textAnchor="end">
              {g}%
            </text>
          </g>
        ))}
      {big &&
        Array.from({ length: 8 }, (_, i) => {
          const t = p.start + i * 86400;
          return (
            <text key={i} x={X(t)} y={H - 6} fontSize="11" fill="var(--muted)" textAnchor={i === 0 ? "start" : i === 7 ? "end" : "middle"}>
              周{WEEKDAY[new Date(t * 1000).getDay()]}
            </text>
          );
        })}
      {runs
        .filter((r) => r.length > 1)
        .map((r, i) => (
          <polygon key={i} points={r.map(([t, v]) => xy(t, v)).concat([...r].reverse().map(([t]) => xy(t, even(t)))).join(" ")} fill="var(--warn-soft)" />
        ))}
      <line x1={X(p.start)} y1={Y(0)} x2={X(p.resets_at)} y2={Y(100)} stroke="var(--muted)" strokeWidth={big ? 1.4 : 1} strokeDasharray="4 4" opacity=".75" />
      {pts.length > 1 && (
        <polyline points={pts.map(([t, v]) => xy(t, v)).join(" ")} fill="none" stroke={ink} strokeWidth={big ? 2.2 : 1.6} strokeLinejoin="round" strokeLinecap="round" />
      )}
      {end && (
        <line
          x1={X(at)}
          y1={Y(p.used)}
          x2={X(end[0])}
          y2={Y(end[1])}
          stroke={p.state === "out" ? "var(--warn)" : "var(--muted)"}
          strokeWidth={big ? 2 : 1.5}
          strokeDasharray={`1 ${big ? 5 : 3.5}`}
          strokeLinecap="round"
        />
      )}
      {end && p.state === "out" && <circle cx={X(end[0])} cy={Y(end[1])} r={big ? 4.5 : 2.6} fill="var(--surface)" stroke="var(--warn)" strokeWidth={big ? 2 : 1.4} />}
      {big && <line x1={X(now)} x2={X(now)} y1={pad.t} y2={H - pad.b} stroke="var(--muted)" opacity=".45" />}
      {big && (
        <text x={X(at) + dx} y={Y(p.used) + (above ? -10 : 18)} fontSize="12" fontWeight="650" fill="var(--ink)" textAnchor={anchor}>
          现在 {Math.round(p.used)}%
        </text>
      )}
      {big && (
        <text x={X(now) + dx} y={Y(even(now)) + (above ? 18 : -10)} fontSize="11.5" fill="var(--muted)" textAnchor={anchor}>
          匀速应在 {Math.round(p.even)}%
        </text>
      )}
      {big && p.state === "out" && p.out_at !== null && (
        <text x={X(p.out_at) - 9} y={Y(100) + 4} fontSize="12" fontWeight="650" fill="var(--warn)" textAnchor="end">
          {whenShort(p.out_at)} 用完
        </text>
      )}
      {big && p.state === "ok" && p.left !== null && (
        <text x={X(p.resets_at) - 4} y={Y(100 - p.left) - 9} fontSize="11.5" fill="var(--muted)" textAnchor="end">
          重置时约剩 {p.left}%
        </text>
      )}
      <circle cx={X(at)} cy={Y(p.used)} r={big ? 4.5 : 2.6} fill={ink} stroke="var(--surface)" strokeWidth={big ? 2 : 1.2} />
    </svg>
  );
}

/** 详情里的 7 天额度节奏：大图、图例、四个数 */
export function PaceCard({ a, points }: { a: Account; points?: Points }) {
  const p = a.pace as Pace;
  const v = verdict(a, true);
  const left = p.resets_at * 1000 - Date.now();
  const by = p.basis === "recent" ? "按最近一天的速度往后推" : "按这周平均的速度往后推";
  const stat = (k: string, val: string, cls = "") => (
    <div className="pc-stat" key={k}>
      <span>{k}</span>
      <b className={cls}>{val}</b>
    </div>
  );
  const third =
    p.state === "out" && p.out_at !== null
      ? stat("按现在的速度用完", `${whenShort(p.out_at)} 左右`, "warn")
      : p.state === "ok"
        ? stat("重置时大约还剩", `${p.left}%`)
        : p.state === "full"
          ? stat("已经用满", `${whenShort(p.resets_at)} 恢复`, "hot")
          : stat("什么时候用完", "还不好估");
  return (
    <div className="ecard sub wide pace-card">
      <h3>
        <Ico name="signal" />7 天额度的节奏
        <span className="hint">
          {whenShort(p.start)} 开始 · {whenShort(p.resets_at)} 重置
        </span>
      </h3>
      <div className="pace-big">
        <PaceSvg a={a} points={points} big />
      </div>
      <div className="pace-lgd">
        <span>
          <i />
          实际已用
        </span>
        <span>
          <i className="dash" />
          匀速用到重置正好用完
        </span>
        <span>
          <i className="soft" />
          比节奏快的部分
        </span>
        {(p.state === "out" || p.state === "ok") && (
          <span>
            <i className={`dot ${p.state}`} />
            {by}
          </span>
        )}
      </div>
      <div className="pc-stats">
        {stat("现在已用", `${Math.round(p.used)}%`)}
        {stat("按匀速应该在", `${Math.round(p.even)}%`)}
        {third}
        {stat("离重置还有", left > 0 ? dur(left) : "已重置")}
      </div>
      {v && <p className="hint pc-note">{v.text}。按记下来的用量估的，只是提醒，换不换号由你决定。</p>}
    </div>
  );
}

const KIND_ORDER = ["claude_code", "claude_desktop", "claude_web", "api"];

/** 有额度的号的汇总：5 小时、7 天平均已用（看剩余时是平均剩余）；有号照现在的速度会在重置前用完时多一行提醒 */
export function SummaryBar({ accounts }: { accounts: Account[] }) {
  const { remain, mask } = usePrefs();
  const q = accounts.filter((a) => a.kind !== "api").map((a) => [a, quotaRings(a)] as const).filter(([, r]) => r.length);
  const outs = accounts.filter((a) => a.pace?.state === "out");
  if (q.length < 2 && !outs.length) return null;
  const avg = (lab: string) => {
    const v = q.map(([, r]) => r.find((x) => x.label === lab)).filter((x): x is RingInfo => !!x).map((x) => x.pct);
    return v.length ? v.reduce((n, x) => n + x, 0) / v.length : null;
  };
  const count: Record<string, number> = {};
  q.forEach(([a]) => {
    count[a.kind] = (count[a.kind] || 0) + 1;
  });
  const kinds = KIND_ORDER.filter((k) => count[k]).map((k) => `${KIND[k]} ${count[k]} 个`).join(" · ");
  const meter = (name: string, v: number | null) => {
    if (v === null) return <div />;
    const shown = remain ? 100 - v : v;
    return (
      <div className="sb-m">
        <div className="row">
          <span>{name}</span>
          <b>
            平均{remain ? "剩余" : "已用"} {Math.round(shown)}%
          </b>
        </div>
        <div className="g-bar">
          <div className={`g-fill ${levelCls(v)}`} style={{ width: `${shown}%` }} />
        </div>
      </div>
    );
  };
  const first = outs[0];
  return (
    <div className="panel sumbar">
      <div className="sb-t">
        <b>有额度的 {q.length} 个号</b>
        <span className="hint">{kinds}</span>
      </div>
      {meter("5 小时", avg("5h"))}
      {meter("7 天", avg("7d"))}
      {first && (
        <div className="sb-alert" role="status">
          <svg viewBox="0 0 16 16">
            <path d="M8 1.8 15 14H1z" />
            <path d="M8 6.2v3.4M8 11.6v.3" />
          </svg>
          <span>
            <b>{maskMail(first.label || first.email || first.id, mask)}</b>：{verdict(first, true)?.text}
            {outs.length > 1 ? `；另外 ${outs.length - 1} 个号也会在重置前用完` : ""}
          </span>
        </div>
      )}
    </div>
  );
}
