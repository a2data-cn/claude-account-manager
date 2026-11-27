import { type ReactNode, useEffect, useLayoutEffect, useRef, useState } from "react";
import { api } from "../api";
import { dur } from "../format";
import { maskMail, usePrefs } from "../prefs";
import type { CompItem, SessionDetail, SessionRow, SessionsView } from "../types";
import { Ico } from "./Icon";
import { modelName } from "./LocalUsage";

/** 会话页：这台电脑上正在跑的 Claude Code（/api/sessions），选中一个看它的上下文窗口（/api/sessions/<id>，按记录估的组成）。
 *  和网页面板的“会话”页一样；页签开着时每 5 秒刷新一次。 */
const STATE: Record<string, [string, string]> = { busy: ["在回答", "busy"], waiting: ["等你确认", "wait"], idle: ["空闲", ""] };
const HEALTH: Record<string, [string, string, string]> = { ok: ["健康", "ss-ok", ""], high: ["快满了", "ss-warn", "warn"], full: ["很满", "ss-hot", "hot"] };
type Cat = "chat" | "files" | "tools" | "images" | "other";
const CATS: [Cat, string, string][] = [
  ["chat", "对话", "var(--c2)"],
  ["files", "读过的文件", "var(--c3)"],
  ["tools", "工具结果", "var(--c1)"],
  ["images", "图片", "var(--c6)"],
  ["other", "其他（估算）", "var(--c4)"],
];
const LOGIN: Record<string, string> = { own_token: "自带的号", api_key: "API Key", config_dir: "单独的配置目录", unknown: "看不出用的哪个号" };
const FILTERS: [string, string][] = [["all", "全部"], ["files", "读过的文件"], ["tools", "工具结果"], ["images", "图片"], ["chat", "对话"]];
const COLOR = Object.fromEntries(CATS.map(([k, , c]) => [k, c])) as Record<Cat, string>;
const CNAME = Object.fromEntries(CATS.map(([k, n]) => [k, n])) as Record<Cat, string>;

export const kTok = (n: number | null | undefined) =>
  n == null ? "–" : n >= 1e6 ? `${Math.round(n / 1e5) / 10}M` : n >= 1e3 ? `${Math.round(n / 100) / 10}K` : String(n);
const pctOf = (r: SessionRow) => (r.context_used == null || !r.context_window ? null : Math.min(100, (r.context_used / r.context_window) * 100));

function saved(): string | null {
  try {
    return localStorage.getItem("cam-session");
  } catch {
    return null;
  }
}

type Box = { v: number; x: number; y: number; w: number; h: number };
/** 方块图：面积按 token 分。squarify：一行一行地摆，每行挑能让方块最接近正方形的个数（和网页面板一样） */
function squarify<T extends { v: number }>(items: T[], x: number, y: number, w: number, h: number): (T & Box)[] {
  const out: (T & Box)[] = [];
  const total = items.reduce((n, i) => n + i.v, 0);
  if (!total || w <= 0 || h <= 0) return out;
  let rest = items.map((i) => ({ ...i, a: (i.v * w * h) / total }));
  while (rest.length) {
    const short = Math.min(w, h);
    const worst = (r: { a: number }[]) => {
      const s = r.reduce((n, q) => n + q.a, 0);
      const mx = Math.max(...r.map((q) => q.a));
      const mn = Math.min(...r.map((q) => q.a));
      return Math.max((short * short * mx) / (s * s), (s * s) / (short * short * mn));
    };
    const row = [rest[0]];
    let i = 1;
    while (i < rest.length && worst([...row, rest[i]]) <= worst(row)) row.push(rest[i++]);
    const s = row.reduce((n, q) => n + q.a, 0);
    if (w >= h) {
      const cw = s / h;
      let yy = y;
      row.forEach((q) => {
        const hh = q.a / cw;
        out.push({ ...q, x, y: yy, w: cw, h: hh });
        yy += hh;
      });
      x += cw;
      w -= cw;
    } else {
      const rh = s / w;
      let xx = x;
      row.forEach((q) => {
        const ww = q.a / rh;
        out.push({ ...q, x: xx, y, w: ww, h: rh });
        xx += ww;
      });
      y += rh;
      h -= rh;
    }
    rest = rest.slice(row.length);
  }
  return out;
}

type Cell = { v: number; it: { name: string; detail?: string; tokens: number; count?: number }; cat: Cat };

function Tree({ d, width }: { d: NonNullable<SessionDetail["composition"]>; width: number }) {
  const r = { w: Math.max(240, width), h: window.innerWidth <= 720 ? 200 : 240 };
  const win = d.window || 0;
  const used = d.total || 0;
  const free = win > used ? win - used : 0;
  const uw = free ? Math.max(r.w * 0.3, Math.min(r.w - 60, (r.w * used) / (used + free))) : r.w;
  const byCat: Partial<Record<Cat, CompItem[]>> = {};
  d.items.forEach((it) => (byCat[it.cat] = byCat[it.cat] || []).push(it));
  const groups = CATS.map(([k]) => ({ k, v: d.categories[k] || 0 })).filter((g) => g.v > 0);
  const cells: (Cell & Box)[] = [];
  for (const g of squarify(groups, 0, 0, uw - (free ? 4 : 0), r.h)) {
    // 太小的格子（不到用掉部分的 1.5%）放不下字、看着像一堆碎片：并进这一类的“另外几项”
    const all = (byCat[g.k as Exclude<Cat, "other">] || []).filter((it) => it.tokens > 0);
    const shown = all.filter((it) => it.tokens >= used * 0.015);
    const merged = all.length - shown.length;
    const left = g.v - shown.reduce((n, it) => n + it.tokens, 0);
    const parts: Cell[] = shown.map((it) => ({ v: it.tokens, it, cat: g.k }));
    if (left > 0)
      parts.push({
        v: left,
        it: { name: g.k === "other" ? "系统提示词、工具定义等" : merged ? `另外 ${merged} 项` : "另外的", tokens: left },
        cat: g.k,
      });
    cells.push(...squarify(parts, g.x, g.y, g.w, g.h));
  }
  const pct = (v: number) => `${((v / r.w) * 100).toFixed(3)}%`;
  const label = CATS.map(([k, n]) => `${n} ${kTok(d.categories[k] || 0)}`).join("，") + (free ? `，剩余 ${kTok(free)}` : "");
  return (
    <div className="ss-tree" role="img" aria-label={`上下文组成：${label}`} style={{ height: r.h }}>
      {cells.map((q, i) => {
        const roomy = q.w > 54 && q.h > 30;
        const two = q.w > 54 && q.h > 46;
        const count = q.it.count || 0;
        const tip = `${q.it.name}${q.it.detail ? " · " + q.it.detail : ""}：约 ${kTok(q.it.tokens)} token（${CNAME[q.cat]}）${count ? ` · ×${count}` : ""}`;
        return (
          <div
            key={i}
            className="tm"
            title={tip}
            style={{ ["--tc" as string]: COLOR[q.cat], left: pct(q.x + 2), top: q.y + 2, width: pct(Math.max(0, q.w - 4)), height: Math.max(0, q.h - 4) }}
          >
            {roomy && (
              <b>
                {q.it.name}
                {count > 1 ? ` ×${count}` : ""}
              </b>
            )}
            {two && (
              <span>
                {kTok(q.it.tokens)} · {CNAME[q.cat]}
              </span>
            )}
          </div>
        );
      })}
      {free > 0 && (
        <div className="tm free" title={`还剩 ${kTok(free)} token`} style={{ left: pct(uw), top: 2, width: pct(r.w - uw - 2), height: r.h - 4 }}>
          <b>剩余空间</b>
          <span>
            {kTok(free)} · {Math.round((free / win) * 100)}%
          </span>
        </div>
      )}
    </div>
  );
}

/** 会话用的哪个号：认得的号写名字，自带登录、API Key 之类写说明 */
function whoOf(r: SessionRow, mask: boolean): string {
  return (r.account && maskMail(r.account, mask)) || LOGIN[r.login || "unknown"] || "看不出用的哪个号";
}

function Row({ r, on, onPick }: { r: SessionRow; on: boolean; onPick: () => void }) {
  const { mask } = usePrefs();
  const [stText, led] = STATE[r.state] || STATE.idle;
  const h = r.health ? HEALTH[r.health] : null;
  const pct = pctOf(r);
  const ran = r.started_at ? "已运行 " + dur(Date.now() - new Date(r.started_at).getTime()) : "运行时长看不出来";
  const who = whoOf(r, mask);
  const guess = r.matched === "newest" || r.matched === "guess" ? "（按这个目录里最新的记录对的）" : "";
  return (
    <button type="button" className="ss-row" aria-pressed={on} onClick={onPick} title={(r.login_text || "") + guess}>
      <span>
        <i className={`ss-led ${led}`} />
      </span>
      <span>
        <b>{r.project || "看不出是哪个项目"}</b>
        <span className="hint">{r.cwd || ""}</span>
      </span>
      <span className="ss-acc">
        <span title={who}>{who}</span>
        <span className="hint">{r.model ? modelName(r.model) : "还没有回复"}</span>
      </span>
      <span className="ss-st">
        <span>{stText}</span>
        <span className="hint">
          {ran}
          {r.turns != null ? ` · ${r.turns} 轮` : ""}
        </span>
      </span>
      <span className="ss-ctxc">
        <span>
          <b>{kTok(r.context_used)}</b>{" "}
          <span className="hint" style={{ display: "inline" }}>
            / {kTok(r.context_window)}
            {r.window_exact ? "" : "（约）"}
            {pct == null ? "" : ` · ${Math.round(pct)}%`}
          </span>
        </span>
        <span className="ss-bar">
          <i className={h ? h[2] : ""} style={{ width: `${pct == null ? 0 : pct.toFixed(1)}%` }} />
        </span>
      </span>
      {h ? <span className={`chip ${h[1]}`}>{h[0]}</span> : <span />}
    </button>
  );
}

function Detail({ d }: { d: SessionDetail }) {
  const { mask } = usePrefs();
  const [filter, setFilter] = useState("all");
  const [all, setAll] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(900);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () => setWidth(el.clientWidth - 32);
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, []);
  const r = d.session;
  const c = d.composition;
  const h = r.health ? HEALTH[r.health] : null;
  const pct = pctOf(r);
  const head = (
    <h3>
      <span className="eico">
        <Ico name="bars" />
      </span>
      上下文窗口<span className="hint">{r.project || ""}</span>
      <span className="spacer" />
      <span className="hint">
        {whoOf(r, mask)} › {r.model ? modelName(r.model) : "还没有回复"}
      </span>
      <span className="chip c-ok">每 5 秒刷新</span>
    </h3>
  );
  if (!c || r.context_used == null)
    return (
      <div className="panel ecard ss-ctx" ref={ref}>
        {head}
        <p className="hint">这个会话还没有回复，等它回了第一条再看。</p>
      </div>
    );
  const hit = r.cache_hit == null ? null : r.cache_hit * 100;
  const win = r.context_window || 0;
  const items = c.items.filter((it) => filter === "all" || it.cat === filter);
  const shown = all ? items : items.slice(0, 8);
  const max = Math.max(1, ...shown.map((it) => it.tokens));
  return (
    <div className="panel ecard ss-ctx" ref={ref}>
      {head}
      <div className="ss-top">
        <div>
          <div className="hint">
            已用上下文{h && <span className={`chip ${h[1]}`}>{h[0]}</span>}
          </div>
          <div className="ss-big">
            {kTok(r.context_used)}{" "}
            <small>
              / {kTok(win)} token{r.window_exact ? "" : "（约）"}
            </small>
          </div>
          <div className="hint">
            已用 {pct == null ? "–" : Math.round(pct) + "%"}
            {win > r.context_used ? ` · 还剩 ${kTok(win - r.context_used)}` : ""}
            {c.since_compact ? " · 压缩过，只算压缩之后的" : ""}
          </div>
        </div>
        <div>
          <div className="hint">提示词缓存</div>
          <div className="ss-big cache">
            {hit == null ? "–" : hit.toFixed(1) + "%"} <small>命中</small>
          </div>
          <div className="ss-bar" style={{ height: 7 }}>
            <i style={{ width: `${hit || 0}%`, opacity: 0.75 }} />
          </div>
          <div className="hint">上一次请求的输入里有多少是从缓存读的</div>
        </div>
      </div>
      <Tree d={{ ...c, window: win }} width={width} />
      <div className="legend ss-key">
        {CATS.map(([k, n, col]) => (
          <span key={k}>
            <i style={{ background: col }} />
            {n} <b>{kTok(c.categories[k] || 0)}</b>
          </span>
        ))}
        {win > r.context_used && (
          <span>
            <i style={{ background: "var(--track)", border: "1px solid var(--line)" }} />
            剩余空间 <b>{kTok(win - r.context_used)}</b>
          </span>
        )}
      </div>
      <div className="ss-filter">
        <span className="hint">占得最多的</span>
        <span className="spacer" />
        <span className="seg-mini" role="group" aria-label="看哪一类">
          {FILTERS.map(([v, t]) => (
            <button
              key={v}
              type="button"
              aria-pressed={v === filter}
              onClick={() => {
                setFilter(v);
                setAll(false);
              }}
            >
              {t}
            </button>
          ))}
        </span>
      </div>
      <div className="ss-items">
        {shown.length ? (
          shown.map((it) => (
            <div className="li" key={it.key}>
              <i className="dot" style={{ background: COLOR[it.cat] }} />
              <span className="nm">
                {it.name}
                {it.detail && <span className="path">{it.detail}</span>}
              </span>
              <span className="tag">{it.count > 1 || (it.cat !== "chat" && it.count) ? "×" + it.count : ""}</span>
              <span className="ss-bar">
                <i style={{ width: `${((it.tokens / max) * 100).toFixed(1)}%`, background: COLOR[it.cat] }} />
              </span>
              <span className="v">{kTok(it.tokens)}</span>
            </div>
          ))
        ) : (
          <p className="hint">这一类没有内容</p>
        )}
      </div>
      {items.length > 8 && (
        <button type="button" className="btn quiet ss-more" onClick={() => setAll(!all)}>
          {all ? "收起" : `显示全部 ${items.length} 项`}
        </button>
      )}
      <p className="hint">
        按 Claude Code 的会话记录估的（文字按字节数除以 4，一张图按 1600 token），加起来按实际用量缩放。系统提示词、工具定义、记忆文件、只留了签名的思考过程不在记录里，算在“其他（估算）”里。
        {c.item_count > c.items.length ? `只列了占得最多的 ${c.items.length} 项。` : ""}只在本机读，不上传。
      </p>
    </div>
  );
}

export function Sessions() {
  const [view, setView] = useState<SessionsView | { error: string } | null>(null);
  const [sel, setSel] = useState<string | null>(saved);
  const [detail, setDetail] = useState<SessionDetail | { error: string } | null>(null);
  const selRef = useRef(sel);
  selRef.current = sel;

  useEffect(() => {
    let stop = false;
    const tick = async () => {
      if (document.hidden) return;
      let v: SessionsView | { error: string };
      try {
        v = await api<SessionsView>("GET", "/api/sessions");
      } catch (e) {
        v = { error: (e as Error).message };
      }
      if (stop) return;
      setView(v);
      const rows = "sessions" in v ? v.sessions : [];
      let want = selRef.current;
      if (!rows.some((r) => r.id === want)) want = rows.length ? rows[0].id : null;
      if (want !== selRef.current) setSel(want);
      if (!want) return setDetail(null);
      try {
        const d = await api<SessionDetail>("GET", `/api/sessions/${encodeURIComponent(want)}`);
        if (!stop && want === selRef.current) setDetail(d);
      } catch (e) {
        if (!stop && want === selRef.current) setDetail({ error: (e as Error).message });
      }
    };
    tick();
    const t = setInterval(tick, 5000);
    return () => {
      stop = true;
      clearInterval(t);
    };
  }, [sel]);

  const pick = (id: string) => {
    if (id === sel) return;
    try {
      localStorage.setItem("cam-session", id);
    } catch {
      /* 存不了就只在这次打开时有效 */
    }
    setDetail(null);
    setSel(id);
  };

  let list: ReactNode;
  if (!view) list = <p className="hint ss-note">正在读取…</p>;
  else if ("error" in view) list = <p className="hint ss-note">读不到会话：{view.error}</p>;
  else if (!view.known) list = <p className="hint ss-note">这次没查到正在运行的进程，过几秒会再试。</p>;
  else if (!view.sessions.length)
    list = (
      <div className="ss-empty">
        <b>这台电脑上现在没有在运行的 Claude Code</b>
        <span className="hint">在终端里运行 claude 以后，这里会列出每个会话在干什么、上下文用了多少。</span>
      </div>
    );
  else
    list = (
      <>
        {view.sessions.map((r) => (
          <Row key={r.id} r={r} on={r.id === sel} onPick={() => pick(r.id)} />
        ))}
        {!view.hooks && view.sessions.some((r) => r.state_by === "guess") && (
          <p className="hint ss-note">
            有的会话“在回答 / 空闲 / 等你确认”是按记录文件和 CPU 猜的：Claude Code 版本旧，自己没记状态，也没装 cam 的钩子。升级 Claude Code（<code>claude update</code>），或者终端运行 <code>cam hooks install</code> 后就准了（正在跑的会话要重开一次）。
          </p>
        )}
      </>
    );
  const v = view && "sessions" in view ? view : null;
  return (
    <section className="block" id="sessions-block">
      <div className="sec-head">
        <h2>正在运行的 Claude Code</h2>
        <span className="hint">
          {v && v.sessions.length
            ? `${v.sessions.length} 个会话${v.answering ? ` · ${v.answering} 个在回答` : ""}${v.waiting ? ` · ${v.waiting} 个等你确认` : ""} · `
            : ""}
          只读本机的记录，不上传
        </span>
      </div>
      <div className="panel ss-list" aria-live="polite">
        {list}
      </div>
      {detail && "session" in detail && v && v.sessions.length > 0 && <Detail d={detail} key={detail.session.id} />}
      {detail && "error" in detail && <p className="hint">{detail.error}</p>}
    </section>
  );
}
