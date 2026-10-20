import type { Account, Window5, WindowKey } from "../types";
import { KIND, PLAN, WINDOW_KEYS, WINDOW_MS, WINDOW_NAME, ago, untilText } from "../format";

const PLAN_CLASS: Record<string, string> = {
  free: "p-free", pro: "p-pro", max: "p-max", team: "p-team", enterprise: "p-enterprise", api: "p-api",
};

export const nameOf = (a: Account) => a.label || a.email || a.id;

function desktopWindowExpired(a: Account, key: WindowKey, w?: Window5 | null) {
  if (!w) return true;
  if (w.resets_at) return new Date(w.resets_at).getTime() <= Date.now();
  const sampled = new Date(a.usage?.fetched_at || "").getTime();
  return !Number.isFinite(sampled) || Date.now() - sampled >= WINDOW_MS[key];
}
const desktopStale = (a: Account) => a.usage?.cache_stale || !a.usage?.fetched_at || Date.now() - new Date(a.usage.fetched_at).getTime() > 30 * 60 * 1000;
const desktopUsage = (a: Account) => a.usage?.source === "desktop_cache" || a.usage?.source === "desktop_history";

function Gauge({ k, w, snapshot = false }: { k: WindowKey; w: Window5; snapshot?: boolean }) {
  const pct = Math.max(0, Math.min(100, w.utilization));
  const cls = pct >= 90 ? "hot" : pct >= 70 ? "warn" : "";
  // 竖线：这个额度窗口已经过去的时间比例
  let elapsed: number | null = null;
  if (w.resets_at && !snapshot) {
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
        <span className="g-reset">{snapshot && !w.resets_at ? "未提供重置时间" : untilText(w.resets_at)}</span>
      </div>
      <div className="g-bar" title={snapshot ? "Claude 本地额度快照" : "竖线表示窗口时间已过去的比例"}>
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

export function PlanChip({ a }: { a: Account }) {
  const p = a.plan_display || (a.kind === "api" ? "api" : null);
  if (!p)
    return (
      <span className="chip p-unknown" title={a.kind === "claude_desktop" ? "可以在“编辑”里手动记录套餐" : "还没拿到套餐信息，可以在“编辑”里手动选"}>
        套餐未知
      </span>
    );
  return <span className={`chip ${PLAN_CLASS[p] || "plan"}`}>{PLAN[p] || p}</span>;
}

function dueInfo(a: Account) {
  if (a.days_left === null || a.days_left === undefined) return null;
  const d = a.days_left;
  return {
    cls: d < 0 || d <= 2 ? "due-now" : d <= 7 ? "due-soon" : "",
    short: d > 0 ? `${d} 天后到期` : d === 0 ? "今天到期" : `已过期 ${-d} 天`,
  };
}

/** 正在用 / 异常 / 注意 / 快到期 */
export function StatusChips({ a }: { a: Account }) {
  const h = a.health;
  const due = dueInfo(a);
  const expiredIssue = h?.issues.some((i) => i.kind === "expired");
  return (
    <>
      {a.is_active && <span className="chip live">{a.kind === "claude_desktop" ? "上次打开" : "正在用"}</span>}
      {h?.level === "error" && <span className="chip st-error">异常</span>}
      {h?.level === "warn" && <span className="chip st-warn">注意</span>}
      {due && due.cls && !expiredIssue && <span className={`chip ${due.cls}`}>{due.short}</span>}
    </>
  );
}

function TileMeter({ a }: { a: Account }) {
  const u = a.usage || {};
  if (a.kind !== "claude_desktop" && u.cost_month_usd !== undefined && u.cost_month_usd !== null)
    return (
      <span className="t-meter">
        本月花费 <b style={{ marginLeft: "auto" }}>${Number(u.cost_month_usd).toFixed(2)}</b>
      </span>
    );
  const w = u.five_hour;
  if (a.kind === "claude_desktop" && (!desktopUsage(a) || desktopWindowExpired(a, "five_hour", w))) return <span className="t-meter">暂无当前窗口的本地额度</span>;
  if (!w) return <span className="t-meter">{a.kind === "claude_web" && !a.has_session_key ? "不自动读取用量" : "还没有用量"}</span>;
  const pct = Math.max(0, Math.min(100, w.utilization));
  const cls = pct >= 90 ? "hot" : pct >= 70 ? "warn" : "";
  return (
    <span className="t-meter">
      5 小时{a.kind === "claude_desktop" ? (desktopStale(a) ? " · 旧快照" : " · 本地") : ""}
      <span className="g-bar">
        <span className={`g-fill ${cls}`} style={{ width: `${pct}%` }} />
      </span>
      <b>{pct.toFixed(0)}%</b>
    </span>
  );
}

/** 账号条里的一张卡：名称、套餐、状态、5 小时用量。点一下在下面显示详情 */
export function AccountTile({ a, selected, onSelect }: { a: Account; selected: boolean; onSelect: (a: Account) => void }) {
  const sub = a.email && a.email !== a.label ? a.email : KIND[a.kind];
  const cls = ["tile", selected && "sel", a.is_active && "live", a.health?.level === "error" && "bad"].filter(Boolean).join(" ");
  return (
    <button type="button" className={cls} aria-pressed={selected} onClick={() => onSelect(a)}>
      <span className="t-top">
        <span className="t-name">{a.label || a.email || "未命名"}</span>
        <PlanChip a={a} />
      </span>
      <span className="t-sub">{sub}</span>
      <span className="t-chips">
        <StatusChips a={a} />
      </span>
      <TileMeter a={a} />
    </button>
  );
}

export interface DetailProps {
  a: Account;
  busy: boolean;
  onSwitch: (a: Account) => void;
  onOpen: (a: Account) => void;
  onRefresh: (a: Account) => void;
  onEdit: (a: Account) => void;
  onRemove: (a: Account) => void;
}

/** 选中账号的详情：问题和修复建议、额度、信息、操作 */
export function AccountDetail({ a, busy, onSwitch, onOpen, onRefresh, onEdit, onRemove }: DetailProps) {
  const u = a.usage || {};
  const m = a.manual || {};
  const windows = WINDOW_KEYS.filter((k) => u[k] && (a.kind !== "claude_desktop" || (desktopUsage(a) && !desktopWindowExpired(a, k, u[k]))));
  const hasCost = a.kind !== "claude_desktop" && u.cost_month_usd !== undefined && u.cost_month_usd !== null;
  let empty = "还没有用量数据，点“刷新”。";
  if (a.kind === "claude_desktop")
    empty = "暂无当前窗口的本地额度。请在这个配置的 Claude 中打开“设置 → 用量”，再点“读取本地额度”。";
  else if (a.kind === "claude_web" && !a.has_session_key)
    empty = "网页版账号默认不读取用量。填写 sessionKey 后可以自动获取，或者在“编辑”里手动记录到期日。";
  else if (a.kind === "api" && !a.has_admin_key) empty = "填写 Admin Key 后可以显示本月花费。";
  const due = dueInfo(a);
  const issues = a.health?.issues ?? [];
  return (
    <section className="detail" aria-label={`${nameOf(a)} 的详情`}>
      <div className="d-head">
        <div className="d-title">
          <div className="name">
            {a.label || a.email || "未命名"}
            <PlanChip a={a} />
            <StatusChips a={a} />
          </div>
          {a.email && a.email !== a.label && <div className="mail">{a.email}</div>}
        </div>
        <div className="d-acts">
          {a.kind === "claude_code" && !a.is_active && (
            <button className="btn primary" disabled={busy} onClick={() => onSwitch(a)}>
              切换到这个账号
            </button>
          )}
          {a.kind === "claude_web" && (
            <button className="btn primary" disabled={busy} onClick={() => onOpen(a)}>
              在电脑上打开
            </button>
          )}
          {a.kind === "claude_desktop" && (
            <button className="btn primary" disabled={busy} onClick={() => onOpen(a)}>
              {a.is_active ? "重新打开" : "切换桌面版"}
            </button>
          )}
          <button className="btn" disabled={busy} onClick={() => onRefresh(a)}>
            {busy ? "处理中…" : a.kind === "claude_desktop" ? "读取本地额度" : "刷新"}
          </button>
          <button className="btn" disabled={busy} onClick={() => onEdit(a)}>
            编辑
          </button>
          <button className="btn quiet danger" disabled={busy} onClick={() => onRemove(a)}>
            移除
          </button>
        </div>
      </div>
      {issues.length > 0 && (
        <ul className="issues">
          {issues.map((i) => (
            <li key={i.kind} className={`issue ${i.level}`}>
              <b>{i.text}</b>
              <span className="advice">{i.advice}</span>
              {i.detail && <span className="raw">{i.detail}</span>}
            </li>
          ))}
        </ul>
      )}
      {a.kind === "claude_desktop" && (
        <p className="hint">
          {u.notice || "额度来自这个配置的 Claude 本地记录；打开客户端用量页后可读取更新。"}
          {u.source === "desktop_history" && " 历史记录未提供重置时间。"}
          {windows.length > 0 && desktopStale(a) && " 这份快照已超过 30 分钟，请结合记录时间查看。"}
          {" 切换或重新打开会退出并重启 Claude 桌面版，请先处理好其中正在运行的任务。首次打开和会话过期后需要登录；Claude Code 命令行账号单独切换。"}
        </p>
      )}
      <div className="d-body">
        <div className="gauges">
          {windows.map((k) => (
            <Gauge key={k} k={k} w={u[k] as Window5} snapshot={a.kind === "claude_desktop"} />
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
        <dl className="d-info">
          <dt>类型</dt>
          <dd>
            {KIND[a.kind]}
            {a.org_name ? ` · ${a.org_name}` : ""}
          </dd>
          {due && (
            <>
              <dt>到期</dt>
              <dd>
                <span className={due.cls}>
                  {due.short}
                  {m.expires_at ? `（${m.expires_at}）` : ""}
                </span>
                {m.auto_renew ? " · 自动续期" : ""}
              </dd>
            </>
          )}
          {m.price && (
            <>
              <dt>价格</dt>
              <dd>{m.price}</dd>
            </>
          )}
          {a.kind === "api" && a.api_key_hint && (
            <>
              <dt>Key</dt>
              <dd>
                {a.api_key_hint}
                {u.key_valid === true ? " · 有效" : u.key_valid === false ? " · 无效" : ""}
              </dd>
            </>
          )}
          {(a.kind === "claude_code" || a.kind === "claude_desktop") && (
            <>
              <dt>{a.kind === "claude_desktop" ? "最近打开" : "最近切换"}</dt>
              <dd>{a.last_switched_at ? ago(a.last_switched_at) : a.kind === "claude_desktop" ? "还没打开过" : "还没切换过"}</dd>
            </>
          )}
          {a.kind === "claude_desktop" ? (
            <>
              <dt>额度来源</dt>
              <dd>{u.source === "desktop_history" ? "Claude 本地用量历史" : u.source === "desktop_cache" ? "Claude 本地缓存" : "等待本地记录"}</dd>
              <dt>记录时间</dt>
              <dd>{ago(u.fetched_at)}</dd>
              <dt>最近读取</dt>
              <dd>{ago(u.checked_at)}</dd>
            </>
          ) : (
            <>
              <dt>用量更新</dt>
              <dd>{ago(u.fetched_at)}</dd>
            </>
          )}
          {m.note && (
            <>
              <dt>备注</dt>
              <dd>{m.note}</dd>
            </>
          )}
        </dl>
      </div>
    </section>
  );
}
