import type { Account, LocalWindow, Window5, WindowKey } from "../types";
import { BREAKDOWN, KIND, PLAN, WINDOW_KEYS, WINDOW_MS, WINDOW_NAME, ago, dur, planTier, resetText, usd, wan } from "../format";
import { maskMail, usePrefs } from "../prefs";

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

const ESTIMATE_TIP = "按本机 Claude Code 记录和当前百分比推算整个窗口，按 API 价折算（订阅不按这个收费）；这个号在别的电脑或网页上也用的话会偏高";

interface GaugeWindow {
  name: string;
  w: Window5;
  span: number; // 窗口长度（毫秒）
  local?: LocalWindow;
}

/** 要显示的额度窗口，按模型的周额度排在 7 天后面 */
function windowsOf(a: Account): GaugeWindow[] {
  const u = a.usage || {};
  const local = u.local?.windows || {};
  const out: GaugeWindow[] = [];
  for (const k of WINDOW_KEYS) {
    const w = u[k];
    if (w && (a.kind !== "claude_desktop" || (desktopUsage(a) && !desktopWindowExpired(a, k, w))))
      out.push({ name: WINDOW_NAME[k], w, span: WINDOW_MS[k], local: local[k] });
    if (k === "seven_day")
      (u.models || []).forEach((m) => out.push({ name: `7 天 · ${m.name}`, w: m, span: 7 * 86400e3, local: local[m.name] }));
  }
  return out;
}

function Gauge({ g: { name, w, span, local }, snapshot = false }: { g: GaugeWindow; snapshot?: boolean }) {
  const { remain } = usePrefs();
  const pct = Math.max(0, Math.min(100, w.utilization));
  const cls = pct >= 90 ? "hot" : pct >= 70 ? "warn" : "";
  // 窗口已经过去了多少时间；看剩余时进度条和竖线都换成“还剩多少”
  let elapsed: number | null = null;
  if (w.resets_at && !snapshot) {
    const left = new Date(w.resets_at).getTime() - Date.now();
    if (!isNaN(left)) elapsed = Math.max(0, Math.min(100, 100 - (left / span) * 100));
  }
  const shown = remain ? 100 - pct : pct;
  const tick = elapsed === null ? null : remain ? 100 - elapsed : elapsed;
  // 配速：已用百分比比时间多出多少；照现在的速度什么时候用完，或者到重置时大约用到多少
  let pace: { text: string; fast: boolean } | null = null;
  if (elapsed !== null && elapsed > 3 && pct >= 1) {
    const p = Math.round(pct - elapsed);
    const head = p > 0 ? `配速 +${p}%` : p < 0 ? `配速 ${p}%` : "配速持平";
    const tail =
      pct > elapsed
        ? `照这个速度约 ${dur((100 - pct) / (pct / ((span * elapsed) / 100)))}后用完`
        : `照这个速度到重置约用到 ${Math.round((pct * 100) / elapsed)}%`;
    pace = { text: `${head} · ${tail}`, fast: pct / elapsed > 1.15 };
  }
  let est = "";
  if (local && local.cost_usd > 0 && !snapshot) {
    const full = local.cost_usd * (100 / pct);
    est =
      pct < 5
        ? "整窗估算：用量还少，数据不足"
        : remain
          ? `还能用约 ${usd(full - local.cost_usd)} · 整窗约 ${usd(full)}`
          : `整窗约 ${usd(full)}（${wan(local.tokens * (100 / pct))} tokens）· 本机已用 ${usd(local.cost_usd)}`;
  }
  return (
    <div className="g">
      <div className="g-head">
        <span className="g-name">{name}</span>
        <span className="g-pct">
          {shown.toFixed(0)}
          <small>%{remain ? " 剩余" : ""}</small>
        </span>
        <span className="g-reset">{snapshot && !w.resets_at ? "未提供重置时间" : resetText(w.resets_at)}</span>
      </div>
      <div className="g-bar" title={snapshot ? "Claude 本地额度快照" : remain ? "竖线表示窗口还剩的时间" : "竖线表示窗口时间已过去的比例"}>
        <div className={`g-fill ${cls}`} style={{ width: `${shown}%` }} />
        {tick !== null && <div className="g-tick" style={{ left: `calc(${tick}% - 1px)` }} />}
      </div>
      {pace && <div className={`g-pace${pace.fast ? " fast" : ""}`}>{pace.text}</div>}
      {est && (
        <div className="g-est" title={ESTIMATE_TIP}>
          {est}
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
  return (
    <span className={`chip ${PLAN_CLASS[p] || "plan"}`}>
      {PLAN[p] || p}
      {planTier(a.rate_limit_tier, p)}
    </span>
  );
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
  const { remain } = usePrefs();
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
  const shown = remain ? 100 - pct : pct;
  return (
    <span className="t-meter">
      5 小时{remain ? "剩余" : ""}
      {a.kind === "claude_desktop" ? (desktopStale(a) ? " · 旧快照" : " · 本地") : ""}
      <span className="g-bar">
        <span className={`g-fill ${cls}`} style={{ width: `${shown}%` }} />
      </span>
      <b>{shown.toFixed(0)}%</b>
    </span>
  );
}

/** 账号条里的一张卡：名称、套餐、状态、5 小时用量。点一下在下面显示详情 */
export function AccountTile({ a, selected, onSelect }: { a: Account; selected: boolean; onSelect: (a: Account) => void }) {
  const { mask } = usePrefs();
  const sub = a.email && a.email !== a.label ? maskMail(a.email, mask) : KIND[a.kind];
  const cls = ["tile", selected && "sel", a.is_active && "live", a.health?.level === "error" && "bad"].filter(Boolean).join(" ");
  return (
    <button type="button" className={cls} aria-pressed={selected} onClick={() => onSelect(a)}>
      <span className="t-top">
        <span className="t-name">{maskMail(a.label || a.email, mask) || "未命名"}</span>
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
  const { mask } = usePrefs();
  const u = a.usage || {};
  const m = a.manual || {};
  const windows = windowsOf(a);
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
    <section className="detail" aria-label={`${maskMail(nameOf(a), mask)} 的详情`}>
      <div className="d-head">
        <div className="d-title">
          <div className="name">
            {maskMail(a.label || a.email, mask) || "未命名"}
            <PlanChip a={a} />
            <StatusChips a={a} />
          </div>
          {a.email && a.email !== a.label && <div className="mail">{maskMail(a.email, mask)}</div>}
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
      <div className="cards d-body">
        <div className="ecard sub">
        <h3>
          <span className="eico">📊</span>额度
        </h3>
        <div className="gauges">
          {windows.map((g) => (
            <Gauge key={g.name} g={g} snapshot={a.kind === "claude_desktop"} />
          ))}
          {!!u.breakdown?.length && (
            <div className="g-mini g-parts" title="这个号 7 天额度花在了哪里">
              7 天构成：
              {u.breakdown.map((b) => (
                <span key={b.key}>
                  {BREAKDOWN[b.key] || b.name} <b>{Math.round(b.percent)}%</b>
                </span>
              ))}
            </div>
          )}
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
        </div>
        <div className="ecard sub">
        <h3>
          <span className="eico">👤</span>账号信息
        </h3>
        <dl className="kv d-info">
          <dt>类型</dt>
          <dd><span className="v">
            {KIND[a.kind]}
            {a.org_name ? ` · ${a.org_name}` : ""}
          </span></dd>
          {due && (
            <>
              <dt>到期</dt>
              <dd><span className="v">
                <span className={due.cls}>
                  {due.short}
                  {m.expires_at ? `（${m.expires_at}）` : ""}
                </span>
                {m.auto_renew ? " · 自动续期" : ""}
              </span></dd>
            </>
          )}
          {m.price && (
            <>
              <dt>价格</dt>
              <dd><span className="v">{m.price}</span></dd>
            </>
          )}
          {a.kind === "api" && a.api_key_hint && (
            <>
              <dt>Key</dt>
              <dd><span className="v">
                {a.api_key_hint}
                {u.key_valid === true ? " · 有效" : u.key_valid === false ? " · 无效" : ""}
              </span></dd>
            </>
          )}
          {(a.kind === "claude_code" || a.kind === "claude_desktop") && (
            <>
              <dt>{a.kind === "claude_desktop" ? "最近打开" : "最近切换"}</dt>
              <dd><span className="v">{a.last_switched_at ? ago(a.last_switched_at) : a.kind === "claude_desktop" ? "还没打开过" : "还没切换过"}</span></dd>
            </>
          )}
          {a.kind === "claude_desktop" ? (
            <>
              <dt>额度来源</dt>
              <dd><span className="v">{u.source === "desktop_history" ? "Claude 本地用量历史" : u.source === "desktop_cache" ? "Claude 本地缓存" : "等待本地记录"}</span></dd>
              <dt>记录时间</dt>
              <dd><span className="v">{ago(u.fetched_at)}</span></dd>
              <dt>最近读取</dt>
              <dd><span className="v">{ago(u.checked_at)}</span></dd>
            </>
          ) : (
            <>
              <dt>用量更新</dt>
              <dd><span className="v">{ago(u.fetched_at)}</span></dd>
            </>
          )}
          {m.note && (
            <>
              <dt>备注</dt>
              <dd><span className="v">{m.note}</span></dd>
            </>
          )}
        </dl>
        </div>
      </div>
    </section>
  );
}
