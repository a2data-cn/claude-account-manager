import { Fragment, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Ico, IconSvg } from "./components/Icon";
import { api, command, inTauri, onEvent } from "./api";
import { copyText } from "./clipboard";
import { ago, fmtTime } from "./format";
import { AccountDetail, AccountTile } from "./components/AccountCard";
import { SummaryBar, type Points } from "./components/Pace";
import {
  AddDialog,
  EditDialog,
  RemoveDialog,
  SwitchChoiceDialog,
  type AddValues,
  type EditValues,
  type SwitchChoice,
} from "./components/Dialogs";
import { LocalUsage } from "./components/LocalUsage";
import { Sessions } from "./components/Sessions";
import { setPref, usePrefs } from "./prefs";
import { PhoneAccess } from "./components/PhoneAccess";
import { Privacy } from "./components/Privacy";
import { ThemeToggle } from "./components/ThemeToggle";
import { AccountVault, type VaultWaitFor } from "./components/AccountVault";
import type { VaultState } from "./vault";
import { Rules } from "./components/Rules";
import { StatusLight } from "./components/StatusLight";
import type { Account, CheckProgress, CheckStep, RegionInfo, RegionsInfo, RiskChange, ServiceCheckItem, State, SwitchResult, UsageSamples } from "./types";

/** 页签：账号 / 用量 / 会话 / 环境 / 设置，和网页面板一样；“账号资料与邮箱”是标题栏右边的按钮，打开后占整页 */
type Tab = "accounts" | "usage" | "sessions" | "env" | "settings";
const TABS: [Tab, string][] = [["accounts", "账号"], ["usage", "用量"], ["sessions", "会话"], ["env", "环境"], ["settings", "设置"]];
function savedTab(): Tab {
  try {
    const t = localStorage.getItem("cam-tab");
    return TABS.some(([v]) => v === t) ? (t as Tab) : "accounts";
  } catch {
    return "accounts";
  }
}
/** 这几项变了要整份换状态：等着的切换、后端的提醒、当前用的是哪些号 */
const headOf = (s: State) => JSON.stringify([s.pending_switch ?? null, s.warning ?? null, s.accounts.filter((a) => a.is_active).map((a) => a.id)]);
/** 红绿灯跟着走的两项：claude 会话（几个、在不在执行）、是不是弹着确认在等你 */
const lightOf = (s: State) => JSON.stringify([s.sessions ?? null, s.attention ?? null]);
const KIND_ORDER = ["claude_code", "claude_desktop", "claude_web", "api"];
const CHECK_MARK: Record<ServiceCheckItem["level"], string> = { ok: "正常", bad: "异常", warn: "注意", info: "说明" };
/** 检测结果分组（和手机面板同样的分组） */
const CHECK_GROUPS: [string, string[]][] = [
  ["网络", ["dns", "region", "domain_region", "isp", "ip_origin", "ip_origin_unknown", "ipv6", "dns_servers", "proxy", "vpn", "proxy_down", "proxy_up", "region_change", "region_mismatch", "proxy_ip", "tor_exit", "ipv6_mismatch", "dns_resolver", "hosts", "warp", "wifi", "http_tamper"]],
  ["本机", ["clock", "timezone", "locale", "device"]],
  ["安全", ["creds_perm", "history_key", "perm_bypass", "panel_lan", "proxy_lan", "config_perm", "remote_app", "auto_run"]],
  ["服务", ["api", "web", "auth", "tls_chain", "tls_intercept", "status"]],
  ["账号", ["account_regions", "accounts"]],
];
const IP_TYPE: Record<string, string> = { residential: "家庭 / 公司宽带", hosting: "机房 / 云服务器", proxy: "代理 / VPN 出口", tor: "Tor 出口节点", mobile: "手机网络" };

function CheckRow({ it, cls }: { it: ServiceCheckItem; cls?: string }) {
  return (
    <li className={`${it.level}${cls ? " " + cls : ""}`}>
      <span className="mark">{CHECK_MARK[it.level] || "说明"}</span>
      <span className="v">
        {it.text}
        {it.notice && <> <span className="chip">只提醒，不扣分</span></>}
        {it.advice && <span className="advice">→ {it.advice}</span>}
      </span>
    </li>
  );
}

/** 服务检测结果：总体结论一张通栏的卡（检测范围接在它下面），其余分两列：“网络”项最多，单独占左边；别的几组叠在右边 */
function CheckList({ items }: { items: ServiceCheckItem[] }) {
  const summary = items.filter((i) => i.kind === "summary");
  const rest = items.filter((i) => i.kind !== "summary");
  const scope = summary.length ? rest.find((i) => i.kind === "scope") : undefined;
  const used = new Set<ServiceCheckItem>(scope ? [scope] : []);
  const groups = CHECK_GROUPS.map(([name, kinds]) => {
    const rows = rest.filter((i) => kinds.includes(i.kind || ""));
    rows.forEach((i) => used.add(i));
    return [name, rows] as [string, ServiceCheckItem[]];
  }).filter(([, rows]) => rows.length);
  const other = rest.filter((i) => !used.has(i));
  if (other.length) groups.push(["其它", other]);
  const cols = [groups.filter(([name]) => name === "网络"), groups.filter(([name]) => name !== "网络")].filter((c) => c.length);
  return (
    <div className="cards check-groups">
      {summary.length > 0 && (
        <div className="panel ecard wide">
          <ul className="checks">
            {summary.map((it, i) => <CheckRow key={`s${i}`} it={it} cls="summary" />)}
          </ul>
          {scope && (
            <p className="hint">
              {scope.text}。{scope.advice}
            </p>
          )}
        </div>
      )}
      {cols.map((col, n) => (
        <div className="check-col" key={n}>
          {col.map(([name, rows]) => (
            <div className="panel ecard" key={name}>
              <h3>{name}</h3>
              <ul className="checks">
                {rows.map((it, i) => <CheckRow key={`${name}${i}`} it={it} />)}
              </ul>
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

// ---- 检测进度：“重新检测”进行时，拿自己生成的 run 每 300 毫秒问一次 GET /api/progress?run=…（只有步骤名和状态），
// 画成环境页顶上的进度卡；做完后留一份收在结果上面（“检测过程”），点开还能看哪一步慢。和网页面板一致
const newRun = () => {
  const a = new Uint8Array(12);
  crypto.getRandomValues(a);
  return Array.from(a, (b) => (b % 36).toString(36)).join("");
};
const fmtMs = (ms: number) => (ms < 1 ? "不到 1 毫秒" : ms < 1000 ? `${ms} 毫秒` : `${(ms / 1000).toFixed(1)} 秒`);
const stepOpen = (s: CheckStep) => s.state === "wait" || s.state === "run";
/** 一步右边的那几个字 */
function stepStatus(s: CheckStep) {
  if (s.state === "ok") return fmtMs(s.ms || 0);
  if (s.state === "fail") return `${s.note || "没成"} · ${fmtMs(s.ms || 0)}`;
  if (s.state === "timeout") return `等了 ${fmtMs(s.ms || 0)}，没回应`;
  if (s.state === "skip") return s.note || "这次不用做";
  return s.state === "run" ? "正在测…" : "排队中";
}
function ProgressSteps({ p }: { p: CheckProgress }) {
  return (
    <ul className="pg-steps">
      {p.steps.map((s) => (
        <li key={s.key} className={`pg-step ${s.state}`}>
          <span className="pg-ico" />
          <span className="pg-name" title={s.label}>
            {s.label}
          </span>
          <span className="pg-st">{stepStatus(s)}</span>
        </li>
      ))}
    </ul>
  );
}
/** 检测进行时的进度卡 */
function ProgressCard({ title, p }: { title: string; p: CheckProgress }) {
  const fin = p.steps.filter((s) => !stepOpen(s)).length;
  return (
    <div className="panel ecard pg" role="status" aria-live="polite">
      <h3>
        <Ico name="signal" />
        {title}
        <span className="chip pg-count">{p.steps.length ? `${fin} / ${p.steps.length} 步 · ${(p.ms / 1000).toFixed(1)} 秒` : "正在开始…"}</span>
      </h3>
      <div className="pg-bar">
        <i style={{ width: `${p.steps.length ? Math.round((fin / p.steps.length) * 100) : 0}%` }} />
      </div>
      <ProgressSteps p={p} />
    </div>
  );
}
/** 做完以后收成一行，放在结果上面 */
function ProgressKept({ title, p }: { title: string; p: CheckProgress }) {
  const odd = p.steps.filter((s) => s.state === "fail" || s.state === "timeout").length;
  return (
    <details className="panel ecard pg pg-kept">
      <summary>
        {title}过程：{p.steps.length} 步，用了 {(p.ms / 1000).toFixed(1)} 秒{odd ? `，${odd} 步没连上或没等到` : ""}（点开看每一步）
      </summary>
      <ProgressSteps p={p} />
    </details>
  );
}

function useServiceCheck(toast: (m: string, bad?: boolean) => void, after?: () => void) {
  const [items, setItems] = useState<ServiceCheckItem[] | null>(null);
  const [checking, setChecking] = useState(false);
  const [progress, setProgress] = useState<CheckProgress | null>(null); // 正在检测时的进度
  const [kept, setKept] = useState<CheckProgress | null>(null); // 上一次检测的过程
  const check = async () => {
    const run = newRun();
    const ask = () => api<CheckProgress>("GET", `/api/progress?run=${run}`);
    setChecking(true);
    setKept(null);
    setProgress({ steps: [], done: false, ms: 0 });
    let live = true;
    const tick = () => {
      // 问不到就算了，检测照常进行
      ask()
        .then((p) => {
          if (live && p.steps.length) setProgress(p);
        })
        .catch(() => {});
    };
    const timer = window.setInterval(tick, 300);
    window.setTimeout(tick, 60);
    let ok = false;
    try {
      setItems((await api<{ items: ServiceCheckItem[] }>("GET", `/api/service-check?run=${run}`)).items);
      ok = true;
      after?.(); // 检测会刷新后台缓存，顶部的分数跟着更新
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      live = false;
      window.clearInterval(timer);
      const last = ok ? await ask().catch(() => null) : null;
      setKept(last && last.steps.length ? last : null);
      setProgress(null);
      setChecking(false);
    }
  };
  return {
    items,
    checking,
    progress,
    kept,
    check,
    reset: () => {
      setItems(null);
      setKept(null);
    },
  };
}

type ServiceCheckState = ReturnType<typeof useServiceCheck>;

const CHECK_TIP =
  "完整检测 DNS、出口地区（对照 Anthropic 官方名单）、IP 类型、IPv4 / IPv6、api.anthropic.com、claude.ai、续登录的地址、HTTPS 证书、代理 / VPN（设了代理的话看它在不在运行）、时区、时钟、官方状态页和账号使用地区，要等几秒钟。只提醒，不拦任何操作。";

/** 两个官方链接：claude.ai 和官方状态页（服务出问题时先看这里），用系统浏览器打开（只认 Claude / Anthropic 的 HTTPS 地址） */
const OFFICIAL_LINKS: [string, string, string][] = [
  ["https://claude.ai", "打开 claude.ai", "Claude 官网"],
  ["https://status.claude.com", "官方状态页", "Anthropic 官方状态页：Claude、API、Claude Code 现在有没有故障"],
];
function openOfficial(url: string) {
  api("POST", "/api/open-external", { url }).catch(() => window.open(url, "_blank", "noopener"));
}

function EnvActs({ label, sc: { items, checking, check, reset } }: { label: string; sc: ServiceCheckState }) {
  return (
    <span className="sec-acts env-acts">
      {OFFICIAL_LINKS.map(([url, text, tip]) => (
        <button key={url} className="btn quiet" title={tip} onClick={() => openOfficial(url)}>
          {text}
        </button>
      ))}
      {items && !checking && (
        <button className="btn quiet" onClick={reset}>
          收起
        </button>
      )}
      <button className="btn" disabled={checking} title={CHECK_TIP} onClick={check}>
        {checking ? "检测中，要等几秒…" : label}
      </button>
    </span>
  );
}

/** 国家代码变成国旗 emoji；不是两个字母的（比如 Tor 的 T1）不显示 */
const flagOf = (cc?: string | null) =>
  /^[A-Z]{2}$/.test(cc || "") ? String.fromCodePoint(...[...(cc as string)].map((ch) => 0x1f1e6 + ch.charCodeAt(0) - 65)) : "";
const regName = (code?: string | null, name?: string | null) => name || code || "未知";
function Flag({ cc }: { cc?: string | null }) {
  const f = flagOf(cc);
  return f ? <span className="flag">{f}</span> : null;
}

/** 评分卡上的行：每行归并哪些风险项；没有风险项时显示“正常”的那句 */
const ENV_ROWS: [string, string, string[]][] = [
  ["region", "出口地区", ["region", "region_unknown", "region_mismatch", "domain_region", "ipv6_mismatch", "warp"]],
  ["iptype", "IP 类型", ["tor_exit", "proxy_ip", "isp", "ip_origin", "ip_origin_unknown"]],
  ["proxy", "代理 / VPN", ["proxy", "vpn", "proxy_down"]],
  ["local", "时区 / 语言", ["timezone", "locale", "clock"]],
  ["dns", "DNS / hosts", ["dns", "dns_resolver", "dns_servers", "hosts"]],
  ["netsafe", "这张网", ["wifi", "http_tamper"]], // Wi-Fi 加没加密、没加密的网页有没有被中途改动；都没查到时没有这一行
  ["change", "出口变化", ["region_change"]],
  // 安全：凭据和配置文件的权限、命令历史里的密钥、不问就执行、对局域网开着的面板和代理、钩子和 MCP、远程控制软件
  ["security", "安全", ["creds_perm", "history_key", "perm_bypass", "panel_lan", "proxy_lan", "config_perm", "remote_app", "auto_run"]],
  ["account", "账号", ["account_regions", "accounts"]],
  ["service", "Claude 服务", ["api", "web", "tls", "status", "auth", "tls_intercept"]],
  ["stale", "检测时间", ["env_stale", "env_failed"]], // 结果是一小时前的、或者最近一次没连上才有这一行
];
const LEVEL_RANK: Record<string, number> = { bad: 3, warn: 2, info: 1, ok: 0 };

/** 出口 IP 卡：国旗 + IP、地区 / 城市 / 运营商、IP 类型、ASN、注册地、上次检测 */
function ExitCard({ reg }: { reg: RegionInfo | null | undefined }) {
  const head = (extra?: ReactNode) => (
    <h3>
      <Ico name="globe" />出口 IP{extra}
    </h3>
  );
  if (!reg?.status) {
    return (
      <div className="panel ecard c-exit">
        {head()}
        <p className="hint" style={{ margin: 0 }}>
          还没有检测过。后台每隔几分钟自动检测一次出口地区和环境风险，也可以点“立即检测”现在就测。
        </p>
      </div>
    );
  }
  const off = reg.lookup_mode === "off";
  // 最近一次检测没连上：上面留着的是以前查到的出口（从没查到过就是“查不到”），在这里说清楚是哪一次的
  const failed = !!reg.failed_at;
  const kept = failed && !!reg.loc;
  const status = (
    <>
      {reg.status === "supported" ? (
        <span className="chip c-ok">支持名单内</span>
      ) : reg.status === "unsupported" ? (
        <span className="chip st-error">不在支持名单</span>
      ) : (
        <span className="chip st-warn">查不到</span>
      )}
      {kept && <span className="chip st-warn">最近一次没连上</span>}
    </>
  );
  const source = reg.meta_source ? " · 数据来自 " + reg.meta_source : "";
  const when: ReactNode = failed ? (
    <>
      <span className="bad">{ago(reg.failed_at)}没连上</span>
      {(reg.fail_count ?? 0) > 1 ? `，连续 ${reg.fail_count} 次` : ""}
      {kept ? ` · 上面是上一次查到的（${ago(reg.checked_at)}）` : ""}
      <span className="sub">后台过一会儿自动重试，换了网络也会自动重测；也可以点“重新检测”</span>
    </>
  ) : (
    `${ago(reg.checked_at)} · 后台每隔几分钟自动检测，换了网络也会自动重测${source}`
  );
  const where = [reg.loc ? regName(reg.loc, reg.loc_name) : "", reg.city || "", reg.org || ""].filter(Boolean).join(" · ");
  const native =
    reg.reg_loc && reg.loc ? (reg.reg_loc === reg.loc ? <span className="chip c-ok">原生 IP</span> : <span className="chip st-warn">广播 IP</span>) : null;
  const rows: ([string, ReactNode] | null)[] = [
    [
      "出口",
      reg.ip ? (
        <>
          <b className="ip">
            <Flag cc={reg.loc} />
            {reg.ip}
          </b>
          {where && <span className="sub">{where}</span>}
        </>
      ) : reg.status === "unknown" ? (
        <>
          <span className="bad">查不到</span>
          <span className="sub">上次检测连不上 claude.ai 和 Cloudflare</span>
        </>
      ) : (
        <>
          <Flag cc={reg.loc} />
          {regName(reg.loc, reg.loc_name)}
        </>
      ),
    ],
    [
      "IP 类型",
      off ? (
        "外部 IP 查询已关闭"
      ) : (
        <>
          {reg.ip_type ? IP_TYPE[reg.ip_type] : "数据源没有标记"}
          {native}
        </>
      ),
    ],
    off || !reg.org ? null : ["运营商", `${reg.asn ? "AS" + reg.asn + " · " : ""}${reg.org}`],
    off || !reg.reg_loc
      ? null
      : [
          "注册地",
          <>
            <Flag cc={reg.reg_loc} />
            {regName(reg.reg_loc, reg.reg_loc_name)}
            {reg.loc ? (reg.reg_loc === reg.loc ? " · 和定位一致" : ` · 定位在 ${regName(reg.loc, reg.loc_name)}`) : ""}
          </>,
        ],
    reg.warp ? ["WARP", "出口经过 Cloudflare WARP / Zero Trust，上面是 WARP 的出口，不是本机网络的真实出口"] : null,
    ["检测", when],
  ];
  return (
    <div className="panel ecard c-exit">
      {head(status)}
      <dl className="kv">
        {rows
          .filter((r): r is [string, ReactNode] => !!r)
          .map(([k, v]) => (
            <Fragment key={k}>
              <dt>{k}</dt>
              <dd>
                <span className="v">{v}</span>
              </dd>
            </Fragment>
          ))}
      </dl>
    </div>
  );
}

/** 环境评分卡：大圆环 + 每个信号一行（有风险项就列风险项和建议，没有就一句“正常”） */
function ScoreCard({ state }: { state: State }) {
  const sc = state.risk_score;
  const reg = state.region;
  const has = !!(reg?.status && sc);
  const r = 40;
  const c = 2 * Math.PI * r;
  const pct = has && sc ? sc.score : 0;
  const risks = state.risks ?? [];
  const okText: Record<string, string> = {
    region: `${regName(reg?.loc, reg?.loc_name)} · 在 Anthropic 支持名单内${reg?.failed_at ? `（${ago(reg.checked_at)}查到的）` : ""}`,
    iptype: reg?.lookup_mode === "off" ? "外部 IP 查询已关闭，没有类型信息" : reg?.ip_type ? IP_TYPE[reg.ip_type] : "数据源没有标记",
    proxy: "未检出显式代理或默认隧道；TUN 分流仍可能生效",
    local: "没发现和出口地区不一致的设置",
    dns: "没发现问题，hosts 文件没动过 Claude 的域名",
    netsafe: [reg?.wifi?.security === "wpa" ? "Wi-Fi 已加密" : "", reg?.plain_http === true ? "没加密的网页没被中途改动" : ""].filter(Boolean).join(" · "),
    change: reg?.prev_loc && reg.changed_at ? `${ago(reg.changed_at)}从 ${reg.prev_loc} 换到 ${reg.loc}，已过了观察期` : "最近没变",
    account: "当前账号最近没在多个地区用过",
    security: reg?.status ? "命令历史里没有密钥，没设成不问就执行，面板和代理没对局域网开着" : "",
    service: "正常",
  };
  const used = new Set<ServiceCheckItem>();
  const rows = ENV_ROWS.map(([key, title, kinds]) => {
    const hits = risks.filter((x) => kinds.includes(x.kind || ""));
    hits.forEach((x) => used.add(x));
    return { title, hits, ok: okText[key] };
  });
  const other = risks.filter((x) => !used.has(x));
  if (other.length) rows.push({ title: "其它", hits: other, ok: "" });
  // 圆环下面一句合计：共扣几分、几项
  const lost = risks.filter((x) => (x.points ?? 0) > 0);
  const total = lost.reduce((n, x) => n + (x.points ?? 0), 0);
  return (
    <div className="panel ecard c-score">
      <h3>
        <Ico name="shield" />环境评分 <span className="chip">只提醒，不拦任何操作</span>
      </h3>
      <div className="score-row">
        <div className="score-side">
        <div className={`score-ring ${has && sc ? sc.grade : "none"}`}>
          <svg viewBox="0 0 92 92" aria-hidden="true">
            <circle className="rt" cx="46" cy="46" r={r} />
            <circle className="rf" cx="46" cy="46" r={r} strokeDasharray={c.toFixed(2)} strokeDashoffset={(c * (1 - pct / 100)).toFixed(2)} />
            <text x="46" y="41" textAnchor="middle" dominantBaseline="central">
              {has && sc ? sc.score : "–"}
            </text>
            <text className="lab" x="46" y="61" textAnchor="middle" dominantBaseline="central">
              {has && sc ? sc.label.split("，")[0] : "还没检测"}
            </text>
          </svg>
        </div>
        {has && sc && state.risk_log && (
          <div className="score-sum">
            <b>{total ? `共扣 ${total} 分` : "没有扣分项"}</b>
            {total ? `${lost.length} 项` : ""}
            <span className="lbl">{sc.label}</span>
          </div>
        )}
        </div>
        {has ? (
          <dl className="kv">
            {rows.filter((row) => row.hits.length || row.ok).map(({ title, hits, ok }) => {
              const level = hits.reduce<string>((m, x) => (LEVEL_RANK[x.level] > LEVEL_RANK[m] ? x.level : m), "ok");
              return (
                <Fragment key={title}>
                  <dt>{title}</dt>
                  <dd>
                    <i className={`dot ${level}`} />
                    <span className="v">
                      {hits.length
                        ? hits.map((x, i) => (
                            <div key={i}>
                              {x.text}
                              {!!x.points && <span className={`chip ${x.points >= 20 ? "st-error" : "st-warn"}`}>−{x.points} 分</span>}
                              {!x.points && x.notice && <span className="chip">只提醒，不扣分</span>}
                              {x.advice && <span className="advice">{x.advice}</span>}
                            </div>
                          ))
                        : ok}
                    </span>
                  </dd>
                </Fragment>
              );
            })}
          </dl>
        ) : (
          <p className="hint" style={{ margin: 0 }}>
            检测过一次后这里按项列出：出口地区、IP 类型、代理 / VPN、时区语言、DNS、出口变化、账号地区、Claude 服务。
          </p>
        )}
      </div>
      <p className="hint">
        100 分起扣，每条后面标着扣几分；90 以上低风险，70–89 有风险，70 以下高风险。{has && sc ? sc.advice : ""}
      </p>
      {!!state.risk_rules?.rules?.length && (
        <details className="score-rules">
          <summary>扣分规则（{state.risk_rules.rules.length} 条）</summary>
          <ul>
            {state.risk_rules.rules.map((r, i) => (
              <li key={i}>
                <b>−{r.points}</b>
                <span>{r.text}</span>
              </li>
            ))}
          </ul>
          {!!state.risk_rules.notices?.length && <p className="hint">只提醒、不扣分的：{state.risk_rules.notices.join("；")}。</p>}
        </details>
      )}
    </div>
  );
}

/** 最近几次直接列出来，更早的收起来 */
const SCORE_LOG_OPEN = 3;
/** 一次分数变化：什么时候从几分变到几分，多了 / 没有了哪一条扣分，那两次之间出口换没换 */
function ScoreChange({ c }: { c: RiskChange }) {
  const cls = c.score < c.prev ? "down" : c.score > c.prev ? "up" : "";
  return (
    <li>
      {c.at ? (
        <span className="when" title={fmtTime(c.at)}>
          {ago(c.at)}
        </span>
      ) : (
        <span className="when">现在</span>
      )}
      <span className={`delta ${cls}`}>{c.score === c.prev ? `还是 ${c.score} 分，扣分的换了一条` : `${c.prev} → ${c.score}`}</span>
      {c.added.map((i, n) => (
        <div className="why" key={`a${n}`}>
          多了：<b>{i.text}</b>（−{i.points}）
        </div>
      ))}
      {c.removed.map((i, n) => (
        <div className="why" key={`r${n}`}>
          没有了：<b>{i.text}</b>（+{i.points}）
        </div>
      ))}
      {c.exit_note && <div className="why">{c.exit_note}</div>}
    </li>
  );
}

/** “分数变化”卡：后台每次环境检测后分数变了就记一笔（state.risk_log） */
function ScoreLogCard({ state }: { state: State }) {
  const log = state.risk_log;
  if (!log) return null;
  const ch = log.changes || [];
  const head = ch.slice(0, SCORE_LOG_OPEN);
  const more = ch.slice(SCORE_LOG_OPEN);
  const how = "分数一变，这里就列出从几分变到几分、多了或少了哪一条。";
  const note = !log.since
    ? "还没开始记录：下一次环境检测之后开始记。" + how
    : ch.length
      ? `从 ${fmtTime(log.since)} 开始记录，最多留 30 次。`
      : `从 ${fmtTime(log.since)} 开始记录，之后分数没变过。${how}`;
  return (
    <div className="panel ecard c-log">
      <h3>
        <Ico name="clock" />分数变化
      </h3>
      <div className="score-log">
        {head.length > 0 && <ol>{head.map((c, i) => <ScoreChange key={i} c={c} />)}</ol>}
        {more.length > 0 && (
          <details className="sl-more">
            <summary>更早的 {more.length} 次</summary>
            <ol>{more.map((c, i) => <ScoreChange key={i} c={c} />)}</ol>
          </details>
        )}
        <p className="hint">{note}</p>
      </div>
    </div>
  );
}

function Loading() {
  return (
    <div className="panel empty">
      <p className="hint">正在读取…</p>
    </div>
  );
}

/** 支持地区卡的地区中文名：浏览器自带的 Intl.DisplayNames，几个常说的按 cam 的叫法 */
const REGION_ZH: Record<string, string> = { CN: "中国大陆", HK: "香港", MO: "澳门", TW: "台湾", XK: "科索沃" };
const REGION_EXTRA = ["AQ", "EH"]; // 名单里都没有的另外两个（南极洲、西撒哈拉）：搜得到，结果是不在名单里
const REGION_WHERE: Record<string, string> = { "api.anthropic.com": "Claude Code 的请求发到这里", "platform.claude.com": "续登录走这里" };
let regionDN: { zh?: Intl.DisplayNames; en?: Intl.DisplayNames } | null = null;
function regionNames() {
  if (!regionDN) {
    regionDN = {};
    try {
      regionDN.zh = new Intl.DisplayNames(["zh-CN"], { type: "region" });
    } catch {
      /* 老浏览器没有：用代码 */
    }
    try {
      regionDN.en = new Intl.DisplayNames(["en"], { type: "region" });
    } catch {
      /* 同上 */
    }
  }
  return regionDN;
}
function regionZh(code: string): string {
  if (REGION_ZH[code]) return REGION_ZH[code];
  try {
    return regionNames().zh?.of(code) || code;
  } catch {
    return code;
  }
}
function regionEn(code: string): string {
  try {
    return regionNames().en?.of(code) || "";
  } catch {
    return "";
  }
}
function regionVerdict(info: RegionsInfo, code: string): [string, string] {
  if (info.supported.includes(code)) return ["c-ok", code === "UA" ? "支持（被占领地区除外）" : "支持"];
  if (info.dependencies.includes(code)) return ["c-ok", "支持：海外属地，按所属国家算"];
  return ["st-error", "不在支持名单"];
}
function regionMatches(info: RegionsInfo, q: string): string[] {
  q = q.trim();
  if (!q) return [];
  const all = [...new Set([...info.supported, ...info.dependencies, ...info.unsupported, ...REGION_EXTRA])];
  const up = q.toUpperCase();
  const low = q.toLowerCase();
  const score = (c: string) => {
    const zh = regionZh(c);
    const en = regionEn(c).toLowerCase();
    if (c === up) return 0;
    if (zh === q) return 1;
    if (zh.startsWith(q)) return 2;
    if (zh.includes(q)) return 3;
    if (en.startsWith(low)) return 4;
    if (low.length >= 3 && en.includes(low)) return 5;
    return 9;
  };
  return all
    .map((c) => [score(c), c] as [number, string])
    .filter(([n]) => n < 9)
    .sort((a, b) => a[0] - b[0] || a[1].localeCompare(b[1]))
    .slice(0, 8)
    .map(([, c]) => c);
}

/** 支持地区卡：当前出口在不在名单里（claude.ai、Claude Code 用的 API、续登录的地址各自看到的）、不支持的地区、查一个地区、几条说明 */
function RegionsCard({ state }: { state: State }) {
  const [q, setQ] = useState("");
  const info = state.regions;
  if (!info?.supported?.length) return null; // 旧版后台没有
  const reg = state.region;
  const doms = reg?.domains ?? [];
  const seen = (loc: string, status?: string | null) => (
    <>
      <Flag cc={loc} />
      {regionZh(loc)}（{loc}）<span className={`chip ${status === "supported" ? "c-ok" : "st-error"}`}>{status === "supported" ? "支持" : "不在名单"}</span>
    </>
  );
  const hits = regionMatches(info, q);
  return (
    <div className="cards one">
      <div className="panel ecard c-regions">
        <h3>
          <Ico name="pin" />
          支持地区 <span className="chip">{info.supported.length} 个国家和地区</span>
        </h3>
        <dl className="kv">
          <dt>claude.ai</dt>
          <dd>
            <span className="v">
              {reg?.loc ? (
                <>
                  {seen(reg.loc, reg.status)}
                  <span className="sub">网页版，也是“出口 IP”卡上的那个出口</span>
                </>
              ) : (
                "还没检测过"
              )}
            </span>
          </dd>
          {doms.map((d) => (
            <Fragment key={d.host}>
              <dt>{d.host}</dt>
              <dd>
                <span className="v">
                  {seen(d.loc, d.status)}
                  {REGION_WHERE[d.host] && <span className="sub">{REGION_WHERE[d.host]}</span>}
                </span>
              </dd>
            </Fragment>
          ))}
          {!!reg?.loc && !doms.length && (
            <>
              <dt>API、续登录</dt>
              <dd>
                <span className="v">下次检测时一起查</span>
              </dd>
            </>
          )}
          <dt>不支持</dt>
          <dd>
            <span className="v">
              <span className="rg-chips">
                {info.unsupported.map((c) => (
                  <span className="chip" key={c}>
                    <Flag cc={c} />
                    {regionZh(c)}
                  </span>
                ))}
              </span>
            </span>
          </dd>
        </dl>
        <label className="rg-search">
          <IconSvg name="search" />
          <input
            type="search"
            autoComplete="off"
            spellCheck={false}
            placeholder="查一个国家或地区：日本、Japan、JP"
            aria-label="查一个国家或地区在不在支持名单里"
            value={q}
            onChange={(e) => setQ(e.target.value)}
          />
        </label>
        <div className="rg-result" aria-live="polite">
          {!q.trim() ? (
            <p className="hint">输入国家或地区的中文名、英文名或两位代码，马上告诉你在不在名单里</p>
          ) : !hits.length ? (
            <p className="hint">没找到“{q.trim()}”：换个写法试试，比如“日本”“Japan”“JP”</p>
          ) : (
            <ul className="rg-hits">
              {hits.map((c) => {
                const [cls, txt] = regionVerdict(info, c);
                return (
                  <li key={c}>
                    <Flag cc={c} />
                    <b>{regionZh(c)}</b>
                    <small>
                      {c}
                      {regionEn(c) ? " · " + regionEn(c) : ""}
                    </small>
                    <span className={`chip ${cls}`}>{txt}</span>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
        {!!info.notes?.length && (
          <details className="rg-notes">
            <summary>几条说明（{info.notes.length} 条）</summary>
            <ul>
              {info.notes.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          </details>
        )}
        <p className="hint">
          名单 {info.checked} 对着{" "}
          <a href={info.url} target="_blank" rel="noopener">
            Anthropic 官方名单
          </a>
          核对；官方改了名单，更新 cam 就能拿到。只提醒，不拦任何操作。
        </p>
      </div>
    </div>
  );
}

/** 环境区块：出口 IP 一张卡、环境评分一张卡（每个信号一行），点“重新检测”在下面展开完整检测结果。只提醒，不拦 */
function EnvCard({ state, sc }: { state: State; sc: ServiceCheckState }) {
  const ready = !!(state.region?.status && state.risk_score);
  const hasLog = ready && !!state.risk_log; // 检测过才有分数可记
  // 点了“重新检测”之后结果出来时滚到它那里：结果在三张卡下面，停在页面顶上的话看不出来它出来了
  const result = useRef<HTMLDivElement>(null);
  const shown = !!sc.items;
  const wasChecking = useRef(false);
  useEffect(() => {
    if (wasChecking.current && !sc.checking && shown) {
      const el = result.current;
      const top = el ? el.getBoundingClientRect().top : 0;
      if (el && (top < 0 || top > window.innerHeight * 0.6)) el.scrollIntoView({ block: "start", behavior: "smooth" });
    }
    wasChecking.current = sc.checking;
  }, [sc.checking, shown]);
  return (
    <section className="block" id="env" aria-label="环境状态">
      <div className="sec-head">
        <h2>环境</h2>
        <span className="hint">本机能看到的信号：出口 IP、IP 类型、代理 / VPN、时区语言、出口变化、账号地区、安全项。只提醒，不拦任何操作</span>
        <EnvActs label={ready ? "重新检测" : "立即检测"} sc={sc} />
      </div>
      {sc.progress && <ProgressCard title="环境检测" p={sc.progress} />}
      <div className={`cards env${hasLog ? "" : " nolog"}`}>
        <ExitCard reg={state.region} />
        <ScoreCard state={state} />
        {hasLog && <ScoreLogCard state={state} />}
      </div>
      <RegionsCard state={state} />
      <div id="env-result" ref={result}>
        {sc.items && sc.kept && <ProgressKept title="环境检测" p={sc.kept} />}
        {sc.items && <CheckList items={sc.items} />}
      </div>
    </section>
  );
}

export default function App() {
  const [state, setState] = useState<State | null>(null);
  // 7 天额度节奏线的点（/api/usage-samples）：账号数据更新了就重新要一次，问不到就不画线（节奏的结论在 state 里）
  const [paceSeries, setPaceSeries] = useState<Record<string, Points>>({});
  const stamp = state?.generated_at;
  useEffect(() => {
    if (!stamp) return;
    let stale = false;
    api<UsageSamples>("GET", "/api/usage-samples")
      .then((r) => {
        if (!stale) setPaceSeries(Object.fromEntries(Object.entries(r.accounts || {}).map(([id, v]) => [id, v.points || []])));
      })
      .catch(() => {
        if (!stale) setPaceSeries({});
      });
    return () => {
      stale = true;
    };
  }, [stamp]);
  const stateRef = useRef<State | null>(null);
  stateRef.current = state;
  const stateSeq = useRef(0); // 每次整份换状态加一：在这之前问的 /api/state 回来了也不要
  const lightPolling = useRef(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const busyRef = useRef(false);
  const [editing, setEditing] = useState<Account | null>(null);
  const [adding, setAdding] = useState(false);
  const [choice, setChoice] = useState<SwitchChoice | null>(null); // 有会话在跑时的三选一
  const [removing, setRemoving] = useState<Account | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(() => {
    try {
      return localStorage.getItem("cam-selected");
    } catch {
      return null;
    }
  });
  const select = (a: Account) => {
    setSelectedId(a.id);
    try {
      localStorage.setItem("cam-selected", a.id);
    } catch {
      /* 存不了就只在本次会话里记住 */
    }
  };
  const [tab, setTabRaw] = useState<Tab>(savedTab);
  const [vault, setVault] = useState(false); // 账号资料与邮箱：开着时占整页，页签都不选中
  const setTab = (t: Tab) => {
    setTabRaw(t);
    setVault(false);
    try {
      localStorage.setItem("cam-tab", t);
    } catch {
      /* 存不了就只在本次打开时记住 */
    }
  };
  const onTabKey = (e: React.KeyboardEvent<HTMLDivElement>) => {
    // 方向键在页签间移动并切过去，Home / End 到头尾
    const i = TABS.findIndex(([v]) => v === tab);
    const j = ({ ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: TABS.length - 1 } as Record<string, number>)[e.key];
    if (j === undefined) return;
    e.preventDefault();
    const next = TABS[(j + TABS.length) % TABS.length][0];
    setTab(next);
    e.currentTarget.querySelector<HTMLButtonElement>(`[data-tab="${next}"]`)?.focus();
  };
  const [toastMsg, setToastMsg] = useState<{ text: string; bad: boolean } | null>(null);
  const toastTimer = useRef<number>();
  const [, tick] = useState(0);
  const prefs = usePrefs();

  const toast = useCallback((text: string, bad = false) => {
    setToastMsg({ text, bad });
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToastMsg(null), bad ? 5000 : 2600);
  }, []);
  // 账号卡上点了“重新登录”：这个号绑了能收信的邮箱、账号库也解锁着，就打开“账号资料与邮箱”开始等登录邮件
  const [vaultWait, setVaultWait] = useState<VaultWaitFor | null>(null);
  useEffect(() => {
    const onRelogin = async (event: Event) => {
      const a = (event as CustomEvent<{ id: string; email: string }>).detail;
      try {
        const vs = await api<VaultState>("GET", "/api/vault");
        if (!vs.unlocked) return;
        const email = (a.email || "").toLowerCase();
        const hit = vs.entries.find((e) => (e.account_id === a.id || (!!email && e.email.toLowerCase() === email)) && e.connected && e.provider !== "credential");
        if (!hit) return;
        setVault(true);
        setVaultWait({ account_id: a.id, email: a.email || "", at: Date.now() });
        toast(`邮箱 ${hit.email} 已经开始等登录邮件，来了直接在这里点`);
      } catch {
        /* 账号库没装或读不了：只复制命令 */
      }
    };
    window.addEventListener("cam-relogin", onRelogin);
    return () => window.removeEventListener("cam-relogin", onRelogin);
  }, [toast]);

  /** 操作接口直接给回来的新状态 */
  const putState = useCallback((s: State) => {
    stateSeq.current += 1;
    setState(s);
  }, []);

  const load = useCallback(async () => {
    const seq = ++stateSeq.current;
    try {
      const s = await api<State>("GET", "/api/state");
      if (seq !== stateSeq.current) return; // 后问的那次（或某个操作的结果）已经用上了：先问后到的这份是旧的
      setState(s);
      setError(null);
    } catch (e) {
      if (seq === stateSeq.current) setError((e as Error).message);
    }
  }, []);

  // 红绿灯要跟上 Claude Code 的状态（在不在执行、是不是弹着确认等你）：窗口开着时每 15 秒问一次，变了才换，而且只换这两项，
  // generated_at 不动，本机用量和规则不会跟着重新取。等着的切换有变化、当前号换了、后端的提醒变了，才整份换掉（和手机面板一样）
  const pollLight = useCallback(async () => {
    if (document.hidden || lightPolling.current || busyRef.current) return;
    if (!stateRef.current) return void load(); // 一开始就没问到：隔一会儿再试
    lightPolling.current = true;
    const seq = stateSeq.current;
    try {
      const s = await api<State>("GET", "/api/state");
      const cur = stateRef.current;
      if (seq !== stateSeq.current || busyRef.current || !cur) return; // 这期间状态整份换过，或者正在做别的操作：这份不用了
      if (headOf(s) !== headOf(cur)) {
        putState(s);
        setError(null);
      } else if (lightOf(s) !== lightOf(cur)) {
        setState((p) => p && { ...p, sessions: s.sessions, attention: s.attention });
      }
    } catch {
      /* 这一轮没问到：灯先不动，下一轮再问 */
    } finally {
      lightPolling.current = false;
    }
  }, [load, putState]);

  useEffect(() => {
    load();
    const unsubs = [
      onEvent("state-changed", () => load()),
      onEvent<{ message: string; bad?: boolean }>("toast", (p) => toast(p.message, !!p.bad)),
    ];
    const t = window.setInterval(() => tick((n) => n + 1), 60000); // 倒计时每分钟更新
    const light = window.setInterval(() => void pollLight(), 15000);
    const vis = () => !document.hidden && load();
    document.addEventListener("visibilitychange", vis);
    // 页面上任何一段命令（<code>），点一下就复制；账号库里显示出来的密码有自己的“复制密码”按钮，不走这里
    const copyCode = (ev: MouseEvent) => {
      const c = (ev.target as HTMLElement | null)?.closest?.("code");
      if (!c || c.closest(".vault-revealed") || c.closest("button, a")) return;
      const text = c.textContent?.trim();
      if (text) void copyText(text).then((ok) => toast(ok ? `已复制：${text}` : "这里复制不了，手动选中复制吧", !ok));
    };
    document.addEventListener("click", copyCode);
    return () => {
      unsubs.forEach((p) => p.then((f) => f()));
      window.clearInterval(t);
      window.clearInterval(light);
      document.removeEventListener("visibilitychange", vis);
      document.removeEventListener("click", copyCode);
    };
  }, [load, toast, pollLight]);

  // 当前账号最前；同类账号按 5 小时余量从多到少
  const sorted = useMemo(() => {
    if (!state) return [];
    return [...state.accounts].sort((x, y) => {
      if (x.is_active !== y.is_active) return x.is_active ? -1 : 1;
      if (x.kind !== y.kind) return KIND_ORDER.indexOf(x.kind) - KIND_ORDER.indexOf(y.kind);
      const ux = x.usage?.five_hour?.utilization ?? 999;
      const uy = y.usage?.five_hour?.utilization ?? 999;
      return ux - uy;
    });
  }, [state]);

  const selected = sorted.find((a) => a.id === selectedId) || sorted.find((a) => a.is_active) || sorted[0] || null;
  const badCount = sorted.filter((a) => a.health?.level === "error").length;
  // 环境卡的检测结果放在这里：卡片在顶部和底部之间移动时结果不丢
  const envSc = useServiceCheck(toast, load);
  const lastFetch = (state?.accounts ?? []).map((a) => a.usage?.fetched_at).filter((x): x is string => !!x).sort().pop();
  // 环境有风险时把环境状态卡放到最上面，没风险时放到账号详情下面
  const risky = !!(state?.risk_score && state.region?.status && state.risk_score.grade !== "low");
  const firstRisk = (state?.risks ?? []).find((x) => x.level === "bad" || x.level === "warn")?.text;
  // Anthropic 那边的严重状况（官方状态页的故障、服务端错误）不扣分，分数看不出来：单独提醒一句
  const outage = (state?.risks ?? []).find((x) => x.notice && x.level === "bad");
  // 一天之内刚降下来的：说一声是从几分降的，原因在环境页的“分数变化”里
  const lastCh = state?.risk_log?.changes?.[0];
  const scoreDrop =
    lastCh && lastCh.score < lastCh.prev && (!lastCh.at || Date.now() - new Date(lastCh.at).getTime() < 86400e3)
      ? `（${lastCh.at ? ago(lastCh.at) : "刚刚"}从 ${lastCh.prev} 分降下来）`
      : "";
  // 选中的卡不在可见范围时，把账号条滑到它
  useEffect(() => {
    const strip = document.querySelector<HTMLElement>(".strip");
    const cur = strip?.querySelector<HTMLElement>(".tile.sel");
    if (!strip || !cur) return;
    const l = cur.getBoundingClientRect().left - strip.getBoundingClientRect().left + strip.scrollLeft;
    if (l < strip.scrollLeft || l + cur.offsetWidth > strip.scrollLeft + strip.clientWidth) strip.scrollLeft = Math.max(0, l - 3);
  }, [selected?.id, tab, vault, sorted.map((a) => a.id).join(",")]); // 回到账号页、列表重新排序后也要滑到选中的卡

  const run = async (key: string, fn: () => Promise<void>) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(key);
    try {
      await fn();
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      busyRef.current = false;
      setBusy(null);
    }
  };

  const refresh = (id?: string) =>
    run(id || "all", async () => {
      const s = await api<State>("POST", "/api/refresh", id ? { id } : {});
      putState(s);
      const errs = s.accounts.filter((a) => (!id || a.id === id) && a.usage?.error).length;
      toast(errs ? `已刷新，${errs} 个账号没拿到用量` : id && s.accounts.find((a) => a.id === id)?.kind === "claude_desktop" ? "已重新读取本地额度记录" : "用量已更新");
    });

  const switchTo = (a: Account, mode?: "wait" | "now") =>
    run(a.id, async () => {
      const r = await api<SwitchResult>("POST", `/api/accounts/${a.id}/switch`, mode ? { mode } : undefined);
      // 有 claude 会话在跑：让用户选等会话结束再切，还是现在就切
      if (r.choice_required) {
        setChoice({ account: a, message: r.message });
        return;
      }
      toast(r.message);
      await load();
    });

  const cancelPending = () =>
    run("pending", async () => {
      putState(await api<State>("DELETE", "/api/pending-switch"));
      toast("已取消等待");
    });

  const openWeb = (a: Account) =>
    run(a.id, async () => {
      const r = await api<{ message: string }>("POST", `/api/accounts/${a.id}/open`);
      toast(r.message);
      await load();
    });

  const saveEdit = (a: Account, v: EditValues) =>
    run(a.id, async () => {
      putState(await api<State>("PATCH", `/api/accounts/${a.id}`, v));
      setEditing(null);
      toast("已保存");
    });

  const remove = (a: Account) =>
    run(a.id, async () => {
      putState(await api<State>("DELETE", `/api/accounts/${a.id}`));
      setRemoving(null);
      setEditing(null);
      if (selectedId === a.id) setSelectedId(null);
      toast("已移除。Claude Code 的项目和会话都还在，凭据在备份目录里");
    });

  // 检测新登录：Claude Code 里 /login 换了号之后点一下，是新账号就保存，不是就说明已经存过（和手机面板一致）
  const detectLogin = () =>
    run("detect", async () => {
      const before = new Set((state?.accounts ?? []).map((a) => a.id));
      const s = await api<State>("POST", "/api/accounts", { kind: "claude_code" });
      putState(s);
      const added = s.accounts.find((a) => !before.has(a.id));
      const cur = s.accounts.find((a) => a.kind === "claude_code" && a.is_active);
      if (added) {
        select(added);
        toast(`发现新账号，已保存：${added.label || added.email}`);
      } else {
        toast(cur ? `Claude Code 现在登录的是"${cur.label || cur.email}"，已经保存过了` : "没有发现新登录");
      }
    });

  const add = (v: AddValues) =>
    run("add", async () => {
      putState(await api<State>("POST", "/api/accounts", v));
      setAdding(false);
      toast(
        v.kind === "claude_code"
          ? "已导入当前登录的账号"
          : v.kind === "claude_desktop"
            ? v.use_current
              ? "已绑定原有 Claude 桌面版配置。打开时会退出并重启客户端"
              : "已添加。点「切换桌面版」，首次在 Claude 窗口里登录"
            : "已添加",
      );
    });

  return (
    <div className="wrap">
      <header className="top">
        <h1>Claude 账号</h1>
        {state && <StatusLight state={state} />}
        {state && (
          <span className="host">
            {state.host}（{state.platform}）
            {state.region?.status === "supported" && <span className="reg-ok"> · 出口 {state.region.loc} ✓</span>}
            {state.region?.status === "unsupported" && (
              <span className="reg-bad"> · 出口 {state.region.loc} ⚠ 不在支持名单</span>
            )}
            {state.region?.status === "unknown" && <span> · 出口地区未知</span>}
            {state.risk_score && state.region?.status && (
              <span
                className={`risk ${state.risk_score.grade}`}
                title={`环境评分（越高越安全）：${state.risk_score.label}。${state.risk_score.advice}`}
              >
                环境 {state.risk_score.score}
              </span>
            )}
          </span>
        )}
        <span className="spacer" />
        <ThemeToggle toast={toast} />
        <button className="btn quiet" aria-pressed={vault} onClick={() => setVault(!vault)}>
          {vault ? "收起账号资料" : "账号资料与邮箱"}
        </button>
        <button className="btn" disabled={!!busy} onClick={() => refresh()}>
          {busy === "all" ? "刷新中…" : "刷新用量"}
        </button>
      </header>
      <div className="tabrow">
        <div className="tabs" role="tablist" aria-label="页面" onKeyDown={onTabKey}>
          {TABS.map(([v, t]) => (
            <button
              key={v}
              type="button"
              role="tab"
              data-tab={v}
              id={`tab-${v}`}
              aria-controls={`pane-${v}`}
              aria-selected={!vault && tab === v}
              tabIndex={tab === v ? 0 : -1}
              onClick={() => setTab(v)}
            >
              {t}
              {v === "sessions" && !!state?.sessions?.count && <span className="n">{state.sessions.count}</span>}
            </button>
          ))}
        </div>
        {lastFetch && <span className="hint">{ago(lastFetch) === "刚刚" ? "用量刚刚更新" : `用量 ${ago(lastFetch)}更新`}</span>}
      </div>

      {vault ? (
        <AccountVault accounts={state?.accounts || []} onAccountsChanged={putState} toast={toast} waitFor={vaultWait} />
      ) : tab === "usage" ? (
        <div className="pane" role="tabpanel" id="pane-usage" aria-labelledby="tab-usage">
          {state ? <LocalUsage accounts={state.accounts} stamp={state.generated_at} /> : <Loading />}
        </div>
      ) : tab === "sessions" ? (
        <div className="pane" role="tabpanel" id="pane-sessions" aria-labelledby="tab-sessions">
          <Sessions />
        </div>
      ) : tab === "env" ? (
        <div className="pane" role="tabpanel" id="pane-env" aria-labelledby="tab-env">
          {state ? <EnvCard state={state} sc={envSc} /> : <Loading />}
        </div>
      ) : tab === "settings" ? (
        <div className="pane" role="tabpanel" id="pane-settings" aria-labelledby="tab-settings">
          {state && <Rules accounts={state.accounts} stamp={state.generated_at} toast={toast} />}
          {inTauri && <PhoneAccess toast={toast} />}
          {inTauri && (
            <section className="block">
              <h2>数据</h2>
              <div className="panel">
                <p className="hint">
                  账号和凭据保存在 <code>~/.claude-accounts</code>，与脚本版 <code>cam</code> 共用。每次切换前的登录状态都会备份在 backups 目录，保留最近 30 份。
                </p>
                <button className="btn" onClick={() => command("open_data_dir").catch((e) => toast((e as Error).message, true))}>
                  打开数据目录
                </button>
              </div>
            </section>
          )}
          <Privacy toast={toast} onChanged={load} onCleared={envSc.reset} />
          {state && (
            <p className="hint foot">
              后端：{state.backend} {state.version}，数据更新于 {new Date(state.generated_at).toLocaleString()}
            </p>
          )}
        </div>
      ) : (
        <main className="pane" role="tabpanel" id="pane-accounts" aria-labelledby="tab-accounts" aria-live="polite">
          {state?.warning && (
            <div className="notice" role="alert">
              ⚠ {state.warning}
            </div>
          )}
          {state?.pending_switch && (
            <div className="notice pending" role="status">
              <span className="n-text">{state.pending_switch.message}</span>
              <button className="btn" disabled={!!busy} onClick={cancelPending}>
                取消等待
              </button>
            </div>
          )}
          {error && (
            <div className="panel empty">
              <h2>读取账号失败</h2>
              <p className="hint">{error}</p>
            </div>
          )}
          {state && risky && state.risk_score && (
            <div className={`notice env-brief ${state.risk_score.grade}`} role="status">
              <span className="n-text">
                环境评分 <b>{state.risk_score.score}/100</b>：{state.risk_score.label}
                {scoreDrop}
                {firstRisk ? ` · ${firstRisk}` : ""}
              </span>
              <button type="button" className="btn quiet" onClick={() => setTab("env")}>
                查看详情
              </button>
            </div>
          )}
          {outage && (
            <div className="notice env-brief medium" role="status">
              <span className="n-text">{outage.text} · 这是 Anthropic 那边的状况，不是本机环境的问题，不扣环境评分</span>
              <button type="button" className="btn quiet" onClick={() => setTab("env")}>
                查看详情
              </button>
            </div>
          )}
          {state && !state.accounts.length && (
            <div className="panel empty">
              <h2>还没有账号</h2>
              <p className="hint">先把这台电脑上当前登录的 Claude Code 账号存进来。以后你在 Claude Code 里登录新账号，这里会自动发现并保存。</p>
              <p>
                <button className="btn primary" disabled={!!busy} onClick={() => add({ kind: "claude_code" })}>
                  导入当前登录
                </button>
              </p>
              <p className="hint">
                用 Claude 桌面版、网页版 claude.ai 或 API Key：
                <button className="btn" disabled={!!busy} onClick={() => setAdding(true)}>
                  添加账号
                </button>
              </p>
            </div>
          )}
          {selected && (
            <>
              <SummaryBar accounts={sorted} />
              <div className="sec-head">
                <h2>账号 · {sorted.length} 个</h2>
                <span className="hint">
                  {badCount > 0 && <span className="bad-count">{badCount} 个异常</span>}
                  {badCount > 0 && " · "}
                  点账号卡看详情
                </span>
                <span className="sec-acts">
                  <span className="seg-mini" role="group" aria-label="额度显示">
                    <button type="button" aria-pressed={!prefs.remain} onClick={() => setPref("remain", false)}>
                      已用
                    </button>
                    <button type="button" aria-pressed={prefs.remain} onClick={() => setPref("remain", true)}>
                      剩余
                    </button>
                  </span>
                  <button
                    type="button"
                    className="btn quiet"
                    aria-pressed={prefs.mask}
                    title="截图分享时把邮箱显示成 ch***@y***.com"
                    onClick={() => setPref("mask", !prefs.mask)}
                  >
                    {prefs.mask ? "显示邮箱" : "邮箱打码"}
                  </button>
                  <button
                    className="btn quiet"
                    disabled={!!busy}
                    title="检查 Claude Code 现在登录的是不是新账号，是就保存"
                    onClick={detectLogin}
                  >
                    {busy === "detect" ? "检测中…" : "检测新登录"}
                  </button>
                  <button className="btn quiet" disabled={!!busy} onClick={() => setAdding(true)}>
                    ＋ 添加账号
                  </button>
                </span>
              </div>
              <div className="strip">
                {sorted.map((a) => (
                  <AccountTile key={a.id} a={a} selected={a.id === selected.id} onSelect={select} points={paceSeries[a.id]} sessions={state?.sessions} />
                ))}
                <button type="button" className="tile add" disabled={!!busy} onClick={() => setAdding(true)}>
                  <span className="plus">＋</span>添加账号
                </button>
              </div>
              <AccountDetail
                a={selected}
                points={paceSeries[selected.id]}
                desktopApp={state?.desktop_app}
                busy={!!busy}
                onSwitch={switchTo}
                onOpen={openWeb}
                onRefresh={(x) => refresh(x.id)}
                onEdit={setEditing}
                onRemove={setRemoving}
              />
            </>
          )}
        </main>
      )}

      <EditDialog
        account={editing}
        onClose={() => setEditing(null)}
        onSave={saveEdit}
        onDelete={(a) => {
          setEditing(null);
          setRemoving(a);
        }}
      />
      <RemoveDialog account={removing} onClose={() => setRemoving(null)} onConfirm={remove} />
      <AddDialog open={adding} onClose={() => setAdding(false)} onAdd={add} />
      <SwitchChoiceDialog
        choice={choice}
        onClose={() => setChoice(null)}
        onPick={(a, mode) => {
          setChoice(null);
          switchTo(a, mode);
        }}
      />
      <div className={`toast${toastMsg ? " show" : ""}${toastMsg?.bad ? " bad" : ""}`} role="status">
        {toastMsg?.text}
      </div>
    </div>
  );
}
