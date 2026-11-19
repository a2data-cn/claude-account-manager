import { useEffect, useRef, useState, type PointerEvent as RPointerEvent } from "react";
import { Ico } from "./Icon";
import { api } from "../api";
import { usd, wan } from "../format";
import { maskMail, usePrefs } from "../prefs";
import type { Account, LocalReport, UsageRec } from "../types";

// 和管理面板（web/panel.html 的“本机 Claude Code 消耗”）同一套：四格汇总、叠起来的柱子、明细、热力图
// 颜色跟着账号走（按账号列表的顺序，CSS 变量 --c1…--c6），第 7 个号起、模型第 7 个起归到灰色的“其他”，不循环用色
const CHART = ["var(--c1)", "var(--c2)", "var(--c3)", "var(--c4)", "var(--c5)", "var(--c6)"];
const RANGES: [string, string][] = [["today", "今天"], ["7", "7 天"], ["30", "30 天"], ["all", "全部"]];
const METRICS: [string, string][] = [["tokens", "Token"], ["cost", "金额"], ["messages", "消息"]];
const GROUPS: [string, string][] = [["account", "按账号"], ["model", "按模型"]];
const ROWBY: [string, string][] = [["account", "账号"], ["model", "模型"]];
const FIELDS = ["input", "output", "cache_write", "cache_read", "messages", "cost_usd"] as const;
const WD = "一二三四五六日";
const OPACITY = [0, 0.22, 0.45, 0.7, 1];

function pick(key: string, opts: [string, string][], dflt: string): string {
  try {
    const v = localStorage.getItem(key);
    if (key === "cam-local-range" && v === "1") return "today"; // 旧版存的“今天”
    return opts.some(([x]) => x === v) ? (v as string) : dflt;
  } catch {
    return dflt;
  }
}
function put(key: string, v: string) {
  try {
    localStorage.setItem(key, v);
  } catch {
    /* 存不了就只在这次打开时有效 */
  }
}

const pad2 = (n: number) => String(n).padStart(2, "0");
const empty = (): UsageRec => ({ input: 0, output: 0, cache_write: 0, cache_read: 0, messages: 0, cost_usd: 0 });
const addTo = (into: UsageRec, r: UsageRec) => {
  FIELDS.forEach((f) => (into[f] += r[f] || 0));
  return into;
};
const tokens = (r: UsageRec) => r.input + r.output + r.cache_write + r.cache_read;

/** 模型 id 变成好认的名字：claude-opus-4-1-20250805 → Opus 4.1 */
export function modelName(m: string | null | undefined): string {
  const s = String(m || "").replace(/^.*?claude-/, "").replace(/-\d{8}$/, "");
  const parts = s.split("-");
  const fam = parts.filter((p) => /^[a-z]+$/i.test(p));
  const ver = parts.filter((p) => /^\d+$/.test(p));
  if (!fam.length) return !m || m === "unknown" ? "未知模型" : m;
  return fam.map((f) => f[0].toUpperCase() + f.slice(1)).join(" ") + (ver.length ? " " + ver.join(".") : "");
}

/** 参考线的间隔：1 / 2 / 5 × 10ⁿ，大约画三条 */
function niceStep(max: number) {
  const raw = max / 3;
  const p = Math.pow(10, Math.floor(Math.log10(raw)));
  const f = raw / p;
  return (f < 1.5 ? 1 : f < 3.5 ? 2 : f < 7.5 ? 5 : 10) * p;
}

/** 模型的颜色：第一次见到时领一个空着的，记在本机，换时间范围也不变；六个都被这次出现的模型占着时，后来的归到“其他” */
function modelColors(models: string[]): Record<string, number> {
  let map: Record<string, number> = {};
  try {
    const v = JSON.parse(localStorage.getItem("cam-model-colors") || "{}");
    if (v && typeof v === "object" && !Array.isArray(v)) map = v;
  } catch {
    /* 坏了就重新分 */
  }
  const present = new Set(models);
  const slots = [0, 1, 2, 3, 4, 5];
  let changed = false;
  models
    .filter((m) => m && m !== "unknown" && !Number.isInteger(map[m]))
    .sort()
    .forEach((m) => {
      const taken = new Set(Object.values(map));
      const busy = new Set(Object.keys(map).filter((k) => present.has(k)).map((k) => map[k]));
      const free = slots.find((i) => !taken.has(i)) ?? slots.find((i) => !busy.has(i));
      if (free === undefined) return;
      Object.keys(map).forEach((k) => {
        if (map[k] === free) delete map[k];
      });
      map[m] = free;
      changed = true;
    });
  if (changed) put("cam-model-colors", JSON.stringify(map));
  return map;
}

interface Ent {
  key: string;
  name: string;
  /** 归到“其他”时它自己的名字 */
  own: string;
  color: string;
  rank: number;
}

function bucketLabel(k: string, unit: LocalReport["unit"], long: boolean): string {
  if (unit === "hour") {
    const h = Number(k.slice(11, 13));
    return long ? `今天 ${pad2(h)}:00–${pad2(h + 1)}:00` : `${pad2(h)}:00`;
  }
  if (unit === "month") {
    const [y, m] = k.split("-");
    return long ? `${y} 年 ${Number(m)} 月` : `${y.slice(2)}/${m}`;
  }
  const d = new Date(k + "T00:00:00");
  const md = `${d.getMonth() + 1}/${d.getDate()}`;
  if (unit === "week") return long ? `${md} 起的一周` : md;
  return long ? `${md} 周${"日一二三四五六"[d.getDay()]}` : md;
}

type Tip = { x: number; y: number; align: "right" | "left" | "above" | "above-l" | "above-r"; body: React.ReactNode };

export function LocalUsage({ accounts, stamp }: { accounts: Account[]; stamp: string }) {
  const { mask } = usePrefs();
  const [range, setRange] = useState(() => pick("cam-local-range", RANGES, "7"));
  const [metric, setMetric] = useState(() => pick("cam-local-metric", METRICS, "tokens"));
  const [group, setGroup] = useState(() => pick("cam-local-group", GROUPS, "account"));
  const [rowBy, setRowBy] = useState(() => pick("cam-local-rows", ROWBY, "account"));
  const [rep, setRep] = useState<LocalReport | null>(null);
  const [loaded, setLoaded] = useState<"" | "ok" | "fail">(""); // 用量是单独一页：没数据时给一句话，不是一片空白
  const [tip, setTip] = useState<(Tip & { id: string }) | null>(null);
  const box = useRef<HTMLElement>(null);
  const plot = useRef<HTMLDivElement>(null);
  const pointer = useRef("mouse");

  useEffect(() => {
    let stale = false; // 换了范围或数据又更新了：先问后到的那份是旧的，不要盖上去
    api<LocalReport>("GET", `/api/local-usage?range=${range}`)
      .then((r) => {
        if (!stale) {
          setRep(r);
          setLoaded("ok");
        }
      })
      .catch(() => {
        if (!stale) {
          setRep(null);
          setLoaded("fail");
        }
      });
    return () => {
      stale = true;
    };
  }, [stamp, range]);

  const heatSum = rep ? rep.heatmap.cells.flat().reduce((a, b) => a + b, 0) : 0;
  if (!rep || !rep.totals || (!rep.totals.messages && !heatSum && range !== "today"))
    return (
      <div className="panel empty">
        <p className="hint">
          {!loaded
            ? "正在读取这台电脑上的 Claude Code 记录…"
            : loaded === "fail"
              ? "没读到本机记录，稍后会自动再试。"
              : "这台电脑上还没有 Claude Code 的使用记录。用 claude 聊过之后，这里按天、按账号、按模型汇总。"}
        </p>
      </div>
    );

  const val = (r: UsageRec) => (metric === "cost" ? r.cost_usd : metric === "messages" ? r.messages : tokens(r));
  const fmt = (v: number) => (metric === "cost" ? usd(v) : metric === "messages" ? `${Math.round(v)} 条` : wan(v));
  const tick = (v: number) => (metric === "cost" ? (v >= 10 ? "$" + Math.round(v) : usd(v)) : metric === "messages" ? String(Math.round(v)) : wan(v));

  const order = accounts.map((a) => a.id);
  const names: Record<string, string> = Object.fromEntries(accounts.map((a) => [a.id, maskMail(a.label || a.email || a.id, mask) || a.id]));
  const mcol = modelColors(rep.model_totals.map((m) => m.model));
  const accEnt = (id: string): Ent => {
    const i = order.indexOf(id);
    if (i < 0) return { key: "unattributed", name: "未归属", own: "未归属", color: "var(--muted)", rank: 98 }; // 最早一次切换之前的、已经删掉的号
    if (i >= CHART.length) return { key: "_other", name: "其他账号", own: names[id], color: "var(--muted)", rank: 99 };
    return { key: id, name: names[id], own: names[id], color: CHART[i], rank: i };
  };
  const modelEnt = (m: string): Ent => {
    if (!m || m === "unknown") return { key: "unknown", name: "未知模型", own: "未知模型", color: "var(--muted)", rank: 98 };
    const i = mcol[m];
    if (!Number.isInteger(i) || i >= CHART.length) return { key: "_other", name: "其他模型", own: modelName(m), color: "var(--muted)", rank: 99 };
    return { key: m, name: modelName(m), own: modelName(m), color: CHART[i], rank: i };
  };

  // ---- 四格汇总
  const t = rep.totals;
  const split: Record<string, { name: string; rank: number; cost: number }> = {};
  rep.accounts.forEach((a) => {
    const e = accEnt(a.account_id);
    const k = e.key === "_other" ? a.account_id : e.key;
    split[k] = split[k] || { name: e.own, rank: e.rank, cost: 0 };
    split[k].cost += a.cost_usd || 0;
  });
  const splitList = Object.values(split)
    .filter((x) => x.cost > 0)
    .sort((a, b) => a.rank - b.rank);
  const splitText =
    splitList
      .slice(0, 3)
      .map((x) => `${x.name} ${usd(x.cost)}`)
      .join(" · ") + (splitList.length > 3 ? ` 等 ${splitList.length} 个` : "");
  const nAcc = rep.accounts.filter((a) => order.includes(a.account_id)).length;
  const hasLoose = rep.accounts.some((a) => !order.includes(a.account_id));
  const hit = t.cache_hit;
  const note: [string, string] =
    hit == null ? ["", "还没有记录"] : hit >= 0.9 ? ["good", "很高：大部分输入是从缓存读的"] : hit >= 0.6 ? ["", "还行：一半以上的输入从缓存读"] : ["", "偏低：大部分输入没走缓存"];

  // ---- 柱子
  const byModel = group === "model";
  const keys = rep.buckets;
  const n = keys.length;
  const idx: Record<string, number> = Object.fromEntries(keys.map((k, i) => [k, i]));
  const cols: Record<string, UsageRec>[] = keys.map(() => ({}));
  const ents: Record<string, Ent> = {};
  (byModel ? rep.model_series : rep.series).forEach((r) => {
    const i = idx[r.bucket];
    if (i === undefined) return;
    const e = "model" in r ? modelEnt(r.model) : accEnt(r.account_id);
    ents[e.key] = e;
    addTo(cols[i][e.key] || (cols[i][e.key] = empty()), r);
  });
  const list = Object.values(ents).sort((a, b) => a.rank - b.rank || a.name.localeCompare(b.name));
  const colTotals = cols.map((c) => Object.values(c).reduce((s, r) => s + val(r), 0));
  const max = Math.max(0, ...colTotals);
  const top = max > 0 ? max * 1.12 : 1;
  let step = niceStep(top);
  if (metric === "messages") step = Math.max(1, Math.round(step));
  const ticks: number[] = [];
  for (let v = step; v <= top; v += step) ticks.push(v);
  const title = { hour: "今天每小时", day: "每天", week: "每周", month: "每月" }[rep.unit];
  const marks = rep.unit === "hour" ? [0, 6, 12, 18] : n <= 8 ? keys.map((_, i) => i) : [0, 1, 2, 3, 4].map((j) => Math.round((j * (n - 1)) / 4));

  // ---- 明细
  type Row = { name: string; color: string; r: UsageRec; sub: string };
  const rows: Row[] = [];
  if (rowBy === "model") {
    rep.model_totals.forEach((r) => {
      const e = modelEnt(r.model);
      const owner = r.top_account ? accEnt(r.top_account) : null;
      rows.push({ name: e.own, color: e.color, r, sub: `主要是 ${owner ? owner.own : "未归属"}` });
    });
  } else {
    const merged: Record<string, { name: string; color: string; r: UsageRec; sessions: number; top: string | null }> = {};
    rep.accounts.forEach((r) => {
      const e = accEnt(r.account_id);
      const k = e.key === "_other" ? r.account_id : e.key;
      merged[k] = merged[k] || { name: e.own, color: e.color, r: empty(), sessions: 0, top: r.top_model };
      addTo(merged[k].r, r);
      merged[k].sessions += r.sessions || 0;
    });
    Object.values(merged).forEach((m) =>
      rows.push({ name: m.name, color: m.color, r: m.r, sub: `${m.sessions} 个会话${m.top ? " · 主要用 " + modelName(m.top) : ""}` }),
    );
  }
  rows.sort((a, b) => val(b.r) - val(a.r));
  const rowMax = Math.max(0, ...rows.map((x) => val(x.r)));

  // ---- 热力图
  const cells = rep.heatmap.cells;
  const heatMax = Math.max(0, ...cells.flat());
  const weeks = Math.round(rep.heatmap.days / 7);
  const level = (v: number) => (!v || !heatMax ? 0 : v / heatMax <= 0.25 ? 1 : v / heatMax <= 0.5 ? 2 : v / heatMax <= 0.75 ? 3 : 4);

  // ---- 悬停提示：鼠标悬停就出来；手机上点一下出来，再点一下或点别处收起
  const colTip = (i: number) => {
    const rs = list.filter((e) => cols[i][e.key]).map((e) => [e, cols[i][e.key]] as const).reverse(); // 从上到下，和柱子一样
    const sum = empty();
    rs.forEach(([, r]) => addTo(sum, r));
    return (
      <>
        <div className="tt">{bucketLabel(keys[i], rep.unit, true)}</div>
        {rs.length ? (
          <>
            {rs.map(([e, r]) => (
              <div className="tl" key={e.key}>
                <span>
                  <i style={{ background: e.color }} />
                  {e.name}
                </span>
                <b>{fmt(val(r))}</b>
              </div>
            ))}
            <div className="tl total">
              <span>合计 · {sum.messages} 条消息</span>
              <b>
                {wan(tokens(sum))} tokens · {usd(sum.cost_usd)}
              </b>
            </div>
          </>
        ) : (
          <div className="tl">
            <span>没有记录</span>
          </div>
        )}
      </>
    );
  };
  const show = (el: HTMLElement, id: string, body: React.ReactNode, isCol: boolean) => {
    const b = box.current?.getBoundingClientRect();
    if (!b) return;
    const r = el.getBoundingClientRect();
    if (isCol) {
      const top = (plot.current?.getBoundingClientRect().top ?? r.top) - b.top + 4;
      const right = r.right - b.left + 10 + 300 < b.width; // 提示最宽 290
      setTip({ id, body, y: top, x: right ? r.right - b.left + 10 : r.left - b.left - 10, align: right ? "right" : "left" });
    } else {
      const cx = r.left - b.left + r.width / 2;
      const align = cx < 130 ? "above-l" : cx > b.width - 130 ? "above-r" : "above";
      setTip({ id, body, y: r.top - b.top - 8, x: align === "above-l" ? r.left - b.left : align === "above-r" ? r.right - b.left : cx, align });
    }
  };
  const hoverIn = (e: RPointerEvent<HTMLElement>, id: string, body: () => React.ReactNode, isCol: boolean) => {
    if (e.pointerType === "mouse" && tip?.id !== id) show(e.currentTarget, id, body(), isCol);
  };
  const tap = (e: React.MouseEvent<HTMLElement>, id: string, body: () => React.ReactNode, isCol: boolean) => {
    if (pointer.current === "mouse") return;
    if (tip?.id === id) setTip(null);
    else show(e.currentTarget, id, body(), isCol);
  };
  const tipStyle = (): React.CSSProperties => {
    if (!tip) return {};
    const tf = { right: "none", left: "translateX(-100%)", above: "translate(-50%, -100%)", "above-l": "translateY(-100%)", "above-r": "translate(-100%, -100%)" }[tip.align];
    return { left: tip.x, top: tip.y, transform: tf };
  };
  const seg = (opts: [string, string][], cur: string, set: (v: string) => void, key: string, label: string) => (
    <span className="seg-mini" role="group" aria-label={label}>
      {opts.map(([v, txt]) => (
        <button
          key={v}
          type="button"
          aria-pressed={v === cur}
          onClick={() => {
            set(v);
            put(key, v);
            setTip(null);
          }}
        >
          {txt}
        </button>
      ))}
    </span>
  );

  return (
    <section
      className="block"
      id="local-block"
      ref={box}
      onPointerDown={(e) => (pointer.current = e.pointerType || "mouse")}
      onPointerLeave={(e) => e.pointerType === "mouse" && setTip(null)}
      onPointerOver={(e) => {
        if (e.pointerType === "mouse" && tip && !(e.target as HTMLElement).closest("[data-lt]")) setTip(null);
      }}
      onClick={(e) => {
        if (pointer.current !== "mouse" && !(e.target as HTMLElement).closest("[data-lt]")) setTip(null);
      }}
    >
      <div className="sec-head">
        <h2>本机 Claude Code 消耗</h2>
        <span className="hint">这台电脑上 claude 的用量 · 金额按 API 价折算，订阅不按这个收费，只用来比较用量大小</span>
        <span className="sec-acts">{seg(RANGES, range, setRange, "cam-local-range", "时间范围")}</span>
      </div>
      <div className="panel lu-tiles">
        <div>
          <div className="k">Token</div>
          <div className="v">{wan(tokens(t))}</div>
          <div className="s">
            输入 {wan(t.input)} · 输出 {wan(t.output)}
            <br />
            缓存写入 {wan(t.cache_write)} · 缓存读取 {wan(t.cache_read)}
          </div>
        </div>
        <div>
          <div className="k">消息</div>
          <div className="v">{t.messages}</div>
          <div className="s">
            {t.sessions} 个会话 · {nAcc} 个号{hasLoose ? " · 有未归属的" : ""}
          </div>
        </div>
        <div>
          <div className="k">折算金额</div>
          <div className="v">≈ {usd(t.cost_usd)}</div>
          <div className="s">{splitText || "按 API 价折算"}</div>
        </div>
        <div>
          <div className="k">缓存命中率</div>
          <div className="v">{hit == null ? "–" : (hit * 100).toFixed(1) + "%"}</div>
          <div className={`s ${note[0]}`}>{note[1]}</div>
        </div>
      </div>

      <div className="panel ecard" id="local-chart">
        <h3>
          <Ico name="bars" />
          {title}
          <span className="hint">悬停或点一下柱子看明细</span>
          <span className="spacer" />
          {seg(METRICS, metric, setMetric, "cam-local-metric", "看什么")}
          {seg(GROUPS, group, setGroup, "cam-local-group", "怎么分")}
        </h3>
        <div className="lu-plot" ref={plot} role="img" aria-label={`${title}的${METRICS.find(([v]) => v === metric)?.[1]}柱状图，最高 ${fmt(max)}`}>
          <div className="lu-grid" aria-hidden="true">
            <div className="base" style={{ bottom: 0 }}>
              <span>0</span>
            </div>
            {max > 0 &&
              ticks.map((v) => (
                <div key={v} style={{ bottom: `${(v / top) * 100}%` }}>
                  <span>{tick(v)}</span>
                </div>
              ))}
          </div>
          <div className="lu-bars" style={{ ["--lu-pad" as string]: n > 40 ? "1px" : n > 20 ? "1.5px" : "3px" }}>
            {max > 0 ? (
              cols.map((c, i) => (
                <div
                  key={keys[i]}
                  className={`lu-col${tip?.id === `c${i}` ? " on" : ""}`}
                  data-lt
                  onPointerEnter={(e) => hoverIn(e, `c${i}`, () => colTip(i), true)}
                  onClick={(e) => tap(e, `c${i}`, () => colTip(i), true)}
                >
                  {list
                    .filter((e) => c[e.key] && val(c[e.key]) > 0)
                    .map((e) => (
                      <div key={e.key} className="lu-seg" style={{ height: `${(val(c[e.key]) / top) * 100}%`, background: e.color }} />
                    ))}
                </div>
              ))
            ) : (
              <div className="lu-empty">{range === "today" ? "今天还没有用过" : "这段时间没有记录"}</div>
            )}
          </div>
        </div>
        <div className="lu-axis" aria-hidden="true">
          {[...new Set(marks)]
            .filter((i) => i < n)
            .map((i) => (
              <span key={i} style={{ left: `${((i + 0.5) / n) * 100}%` }}>
                {bucketLabel(keys[i], rep.unit, false)}
              </span>
            ))}
        </div>
        {list.length > 1 && (
          <div className="legend lu-legend">
            {list.map((e) => (
              <span key={e.key}>
                <i style={{ background: e.color }} />
                {e.name} <b>{fmt(cols.reduce((s, c) => s + (c[e.key] ? val(c[e.key]) : 0), 0))}</b>
              </span>
            ))}
          </div>
        )}
      </div>

      <div className="lu-split">
        <div className="panel ecard">
          <h3>
            <Ico name="list" />
            明细
            <span className="spacer" />
            {seg(ROWBY, rowBy, setRowBy, "cam-local-rows", "明细按什么分")}
          </h3>
          <div className="lu-rows">
            {rows.length ? (
              rows.map((x) => (
                <div className="r" key={x.name}>
                  <i className="dot" style={{ background: x.color }} />
                  <span className="nm" title={x.name}>
                    {x.name}
                  </span>
                  <span className="val">{fmt(val(x.r))}</span>
                  <span className="tr">
                    <i style={{ width: `${rowMax > 0 ? (val(x.r) / rowMax) * 100 : 0}%`, background: x.color }} />
                  </span>
                  <span className="sub">
                    {x.r.messages} 条消息 · {usd(x.r.cost_usd)} · {x.sub}
                  </span>
                </div>
              ))
            ) : (
              <p className="hint" style={{ margin: 0 }}>
                这段时间没有记录
              </p>
            )}
          </div>
        </div>
        <div className="panel ecard">
          <h3>
            <Ico name="clock" />
            什么时候用得多<span className="hint">最近 {weeks} 周 · 按消息条数</span>
            <span className="spacer" />
            <span className="lu-key" aria-hidden="true">
              少{" "}
              {OPACITY.map((o) => (
                <span className="c" key={o}>
                  {o > 0 && <i style={{ opacity: o }} />}
                </span>
              ))}{" "}
              多
            </span>
          </h3>
          <div className="lu-heat" role="img" aria-label="星期几和钟点的热力图">
            {cells.map((row, d) => [
              <span className="wd" key={`w${d}`}>
                {WD[d]}
              </span>,
              ...row.map((v, h) => {
                const id = `h${d}-${h}`;
                const text = `周${WD[d]} ${pad2(h)}:00–${pad2(h + 1)}:00 · ${v} 条消息（最近 ${weeks} 周合计）`;
                const body = () => (
                  <div className="tl">
                    <span style={{ color: "var(--ink)", whiteSpace: "normal" }}>{text}</span>
                  </div>
                );
                return (
                  <span
                    className="c"
                    key={id}
                    data-lt
                    onPointerEnter={(e) => hoverIn(e, id, body, false)}
                    onClick={(e) => tap(e, id, body, false)}
                  >
                    {level(v) > 0 && <i style={{ opacity: OPACITY[level(v)] }} />}
                  </span>
                );
              }),
            ])}
            <span />
            {[0, 6, 12, 18].map((x) => (
              <span className="hr" key={x}>
                {x}:00
              </span>
            ))}
          </div>
        </div>
      </div>
      <p className="hint" style={{ margin: "10px 0 0" }}>
        按切换记录把每条消息归到当时登录的账号；第一次切换之前的记录显示为“未归属”。“全部”是这台电脑上还留着的记录（Claude Code 默认只留 30 天）。
      </p>
      {tip && (
        <div className="lu-tip" style={tipStyle()}>
          {tip.body}
        </div>
      )}
    </section>
  );
}
