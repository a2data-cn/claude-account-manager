import type { Account, Window5, WindowKey } from "../types";
import { KIND, PLAN, WINDOW_KEYS, WINDOW_MS, WINDOW_NAME, ago, untilText } from "../format";

function Gauge({ k, w }: { k: WindowKey; w: Window5 }) {
  const pct = Math.max(0, Math.min(100, w.utilization));
  const cls = pct >= 90 ? "hot" : pct >= 70 ? "warn" : "";
  // 竖线：这个额度窗口已经过去的时间比例
  let elapsed: number | null = null;
  if (w.resets_at) {
    const left = new Date(w.resets_at).getTime() - Date.now();
    if (!isNaN(left)) elapsed = Math.max(0, Math.min(100, 100 - (left / WINDOW_MS[k]) * 100));
  }
  const fast = elapsed !== null && elapsed > 3 && pct / elapsed > 1.15;
  return (
    <div className="g">
      <div className="g-head">
        <span className="g-name">{WINDOW_NAME[k]}</span>
        <span className="g-pct">
          {pct.toFixed(0)}
          <small>%</small>
        </span>
        <span className="g-reset">{untilText(w.resets_at)}</span>
      </div>
      <div className="g-bar" title="竖线表示窗口时间已过去的比例">
        <div className={`g-fill ${cls}`} style={{ width: `${pct}%` }} />
        {elapsed !== null && <div className="g-tick" style={{ left: `calc(${elapsed}% - 1px)` }} />}
      </div>
      {elapsed !== null && elapsed > 3 && (
        <div className={`g-pace${fast ? " fast" : ""}`}>
          {fast ? "用得比时间快，照这个速度会提前用完" : `窗口已过 ${Math.round(elapsed)}%`}
        </div>
      )}
    </div>
  );
}

function DueLine({ a }: { a: Account }) {
  if (a.days_left === null || a.days_left === undefined) return null;
  const m = a.manual || {};
  const d = a.days_left;
  // 到期日可能来自接口返回的订阅信息而不是手动填写，没填时不显示括号
  const when = m.expires_at ? `（${m.expires_at}）` : "";
  const txt = d > 0 ? `${d} 天后到期${when}` : d === 0 ? "今天到期" : `已过期 ${-d} 天`;
  const cls = d <= 2 ? "due-now" : d <= 7 ? "due-soon" : "";
  return (
    <li className={cls}>
      {txt}
      {m.auto_renew ? "，自动续期" : ""}
      {m.price ? `，${m.price}` : ""}
    </li>
  );
}

export interface CardProps {
  a: Account;
  busy: boolean;
  onSwitch: (a: Account) => void;
  onOpen: (a: Account) => void;
  onRefresh: (a: Account) => void;
  onEdit: (a: Account) => void;
}

export function AccountCard({ a, busy, onSwitch, onOpen, onRefresh, onEdit }: CardProps) {
  const u = a.usage || {};
  const windows = WINDOW_KEYS.filter((k) => u[k]);
  const hasCost = u.cost_month_usd !== undefined && u.cost_month_usd !== null;
  let empty = "还没有用量数据，点“刷新”。";
  if (a.kind === "claude_web" && !a.has_session_key)
    empty = "网页版账号默认不抓取用量。填写 sessionKey 后可尝试自动获取，或在“编辑”里手动记录到期日。";
  else if (a.kind === "api" && !a.has_admin_key) empty = "填写 Admin Key 后可显示本月花费。";

  return (
    <article className={`acc${a.is_active ? " is-live" : ""}`} aria-label={a.label || a.email || ""}>
      <div className="who">
        <div className="name">
          {a.label || a.email || "未命名"}
          {a.plan_display && <span className="chip plan">{PLAN[a.plan_display] || a.plan_display}</span>}
          {a.is_active && <span className="chip live">Claude Code 正在用</span>}
        </div>
        {a.email && a.email !== a.label && <div className="mail">{a.email}</div>}
        <ul className="facts">
          <li>
            {KIND[a.kind]}
            {a.org_name ? `，${a.org_name}` : ""}
          </li>
          <DueLine a={a} />
          {a.kind === "api" && a.api_key_hint && (
            <li>
              Key {a.api_key_hint}
              {u.key_valid === true ? "，有效" : u.key_valid === false ? "，无效" : ""}
            </li>
          )}
          {a.manual?.note && <li>{a.manual.note}</li>}
          {u.error && <li className="err">{u.error}</li>}
          <li>用量更新于 {ago(u.fetched_at)}</li>
        </ul>
      </div>
      <div className="gauges">
        {windows.map((k) => (
          <Gauge key={k} k={k} w={u[k] as Window5} />
        ))}
        {hasCost && (
          <div className="g">
            <div className="g-head">
              <span className="g-name">本月花费</span>
              <span className="g-pct">${Number(u.cost_month_usd).toFixed(2)}</span>
            </div>
          </div>
        )}
        {!windows.length && !hasCost && <div className="g-empty">{empty}</div>}
      </div>
      <div className="acts">
        {a.kind === "claude_code" && !a.is_active && (
          <button className="btn primary" disabled={busy} onClick={() => onSwitch(a)}>
            切换到这个账号
          </button>
        )}
        {a.kind === "claude_web" && (
          <button className="btn primary" onClick={() => onOpen(a)}>
            打开浏览器窗口
          </button>
        )}
        <button className="btn" disabled={busy} onClick={() => onRefresh(a)}>
          刷新
        </button>
        <button className="btn quiet" onClick={() => onEdit(a)}>
          编辑
        </button>
      </div>
    </article>
  );
}
