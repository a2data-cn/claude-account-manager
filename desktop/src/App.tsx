import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, command, inTauri, onEvent } from "./api";
import { ago } from "./format";
import { AccountDetail, AccountTile } from "./components/AccountCard";
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
import { setPref, usePrefs } from "./prefs";
import { PhoneAccess } from "./components/PhoneAccess";
import { Privacy } from "./components/Privacy";
import { ThemeToggle } from "./components/ThemeToggle";
import { AccountVault } from "./components/AccountVault";
import { Rules } from "./components/Rules";
import type { Account, ServiceCheckItem, State, SwitchResult } from "./types";

type Tab = "accounts" | "vault" | "settings";
const KIND_ORDER = ["claude_code", "claude_desktop", "claude_web", "api"];
const CHECK_MARK: Record<ServiceCheckItem["level"], string> = { ok: "✓", bad: "✗", warn: "!", info: "·" };
/** 检测结果分组（和手机面板同样的分组） */
const CHECK_GROUPS: [string, string[]][] = [
  ["网络", ["dns", "region", "isp", "ip_origin", "ip_origin_unknown", "ipv6", "dns_servers", "proxy", "vpn", "region_change", "region_mismatch", "proxy_ip", "tor_exit", "ipv6_mismatch", "dns_resolver", "warp"]],
  ["本机", ["clock", "timezone", "locale", "device"]],
  ["服务", ["api", "web", "status"]],
  ["账号", ["account_regions", "accounts"]],
  ["说明", ["scope"]],
];
const IP_TYPE: Record<string, string> = { residential: "家庭 / 公司宽带", hosting: "机房 / 云服务器", proxy: "代理 / VPN 出口", tor: "Tor 出口节点", mobile: "手机网络" };

function CheckRow({ it, cls }: { it: ServiceCheckItem; cls?: string }) {
  return (
    <li className={`${it.level}${cls ? " " + cls : ""}`}>
      <span className="mark">{CHECK_MARK[it.level] || "·"}</span>
      <span>
        {it.text}
        {it.advice && <span className="advice">→ {it.advice}</span>}
      </span>
    </li>
  );
}

/** 服务检测结果：总体结论在最上面，其余按网络 / 本机 / 服务 / 账号分组 */
function CheckList({ items }: { items: ServiceCheckItem[] }) {
  const rest = items.filter((i) => i.kind !== "summary");
  const used = new Set<ServiceCheckItem>();
  const groups = CHECK_GROUPS.map(([name, kinds]) => {
    const rows = rest.filter((i) => kinds.includes(i.kind || ""));
    rows.forEach((i) => used.add(i));
    return [name, rows] as [string, ServiceCheckItem[]];
  }).filter(([, rows]) => rows.length);
  const other = rest.filter((i) => !used.has(i));
  const summary = items.filter((i) => i.kind === "summary");
  return (
    <div className="check-groups">
      {summary.length > 0 && (
        <ul className="checks">
          {summary.map((it, i) => <CheckRow key={`s${i}`} it={it} cls="summary" />)}
        </ul>
      )}
      {[...groups, ...(other.length ? [["其它", other] as [string, ServiceCheckItem[]]] : [])].map(([name, rows]) => (
        <Fragment key={name}>
          <h4>{name}</h4>
          <ul className="checks">
            {rows.map((it, i) => <CheckRow key={`${name}${i}`} it={it} />)}
          </ul>
        </Fragment>
      ))}
    </div>
  );
}

function useServiceCheck(toast: (m: string, bad?: boolean) => void, after?: () => void) {
  const [items, setItems] = useState<ServiceCheckItem[] | null>(null);
  const [checking, setChecking] = useState(false);
  const check = async () => {
    setChecking(true);
    try {
      setItems((await api<{ items: ServiceCheckItem[] }>("GET", "/api/service-check")).items);
      after?.(); // 检测会刷新后台缓存，顶部的分数跟着更新
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      setChecking(false);
    }
  };
  return { items, checking, check, reset: () => setItems(null) };
}

type ServiceCheckState = ReturnType<typeof useServiceCheck>;

const CHECK_TIP =
  "完整检测 DNS、出口地区（对照 Anthropic 官方名单）、IP 类型、IPv4 / IPv6、api.anthropic.com、claude.ai、代理 / VPN、时区、时钟、官方状态页和账号使用地区，要等几秒钟。只提醒，不拦任何操作。";

function EnvActs({ label, sc: { items, checking, check, reset } }: { label: string; sc: ServiceCheckState }) {
  return (
    <div className="env-acts">
      {items && !checking && (
        <button className="btn quiet" onClick={reset}>
          收起
        </button>
      )}
      <button className="btn" disabled={checking} title={CHECK_TIP} onClick={check}>
        {checking ? "检测中，要等几秒…" : label}
      </button>
    </div>
  );
}

/** 环境状态卡：分数、出口、网络类型、上次检测时间、风险提醒，点"重新检测"在卡下展开完整服务检测结果。只提醒，不拦 */
function EnvCard({ state, sc }: { state: State; sc: ServiceCheckState }) {
  const { items } = sc;
  const reg = state.region;
  const score = state.risk_score;
  if (!reg?.status || !score) {
    return (
      <section className="env" aria-label="环境状态">
        <div className="env-score">
          <div className="env-num">–</div>
          <div className="env-lab">还没检测</div>
        </div>
        <div className="env-empty">后台每隔几分钟自动检测一次出口地区和环境风险；也可以现在就测。只提醒，不拦任何操作。</div>
        <EnvActs label="立即检测" sc={sc} />
        {items && (
          <div className="env-checks">
            <CheckList items={items} />
          </div>
        )}
      </section>
    );
  }
  const risks = (state.risks ?? []).filter((r) => r.level === "warn" || r.level === "bad");
  return (
    <section className={`env ${score.grade}`} aria-label="环境状态">
      <div className="env-score">
        <div className="env-num">
          {score.score}
          <small>/100</small>
        </div>
        <div className="env-lab">{score.label.split("，")[0]}</div>
      </div>
      <div className="env-facts">
        <div>
          <span className="k">出口</span>
          <span className={reg.status === "unsupported" ? "bad" : ""}>
            {reg.status === "unknown" ? "查不到" : `${reg.loc}${reg.city ? " · " + reg.city : ""}`}
            {reg.status === "unsupported" ? " · 不在支持名单" : reg.status === "supported" ? " · 支持名单内" : ""}
          </span>
        </div>
        <div>
          <span className="k">网络</span>
          {reg.lookup_mode === "off"
            ? "外部 IP 查询已关闭"
            : reg.org
              ? `${reg.org}${reg.ip_type ? " · " + IP_TYPE[reg.ip_type] : ""}${reg.meta_source ? " · 数据：" + reg.meta_source : ""}`
              : "查不到"}
        </div>
        <div>
          <span className="k">检测</span>
          {ago(reg.checked_at)} · 后台每隔几分钟自动检测
        </div>
      </div>
      <EnvActs label="重新检测" sc={sc} />
      {items ? (
        <div className="env-checks">
          <CheckList items={items} />
        </div>
      ) : (
        risks.length > 0 && (
          <ul className="env-risks">
            {risks.map((r, i) => (
              <li key={`${r.kind}-${i}`} className={r.level}>
                {r.text}
                {r.advice && <span className="advice">{r.advice}</span>}
              </li>
            ))}
          </ul>
        )
      )}
    </section>
  );
}


export default function App() {
  const [state, setState] = useState<State | null>(null);
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
  const [tab, setTab] = useState<Tab>("accounts");
  const [toastMsg, setToastMsg] = useState<{ text: string; bad: boolean } | null>(null);
  const toastTimer = useRef<number>();
  const [, tick] = useState(0);
  const prefs = usePrefs();

  const toast = useCallback((text: string, bad = false) => {
    setToastMsg({ text, bad });
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToastMsg(null), bad ? 5000 : 2600);
  }, []);

  const load = useCallback(async () => {
    try {
      setState(await api<State>("GET", "/api/state"));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  useEffect(() => {
    load();
    const unsubs = [
      onEvent("state-changed", () => load()),
      onEvent<{ message: string; bad?: boolean }>("toast", (p) => toast(p.message, !!p.bad)),
    ];
    const t = window.setInterval(() => tick((n) => n + 1), 60000); // 倒计时每分钟更新
    const vis = () => !document.hidden && load();
    document.addEventListener("visibilitychange", vis);
    return () => {
      unsubs.forEach((p) => p.then((f) => f()));
      window.clearInterval(t);
      document.removeEventListener("visibilitychange", vis);
    };
  }, [load, toast]);

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
  // 环境有风险时把环境状态卡放到最上面，没风险时放到账号详情下面
  const risky = !!(state?.risk_score && state.region?.status && state.risk_score.grade !== "low");
  // 选中的卡不在可见范围时，把账号条滑到它
  useEffect(() => {
    const strip = document.querySelector<HTMLElement>(".strip");
    const cur = strip?.querySelector<HTMLElement>(".tile.sel");
    if (!strip || !cur) return;
    const l = cur.getBoundingClientRect().left - strip.getBoundingClientRect().left + strip.scrollLeft;
    if (l < strip.scrollLeft || l + cur.offsetWidth > strip.scrollLeft + strip.clientWidth) strip.scrollLeft = Math.max(0, l - 3);
  }, [selected?.id, tab, sorted.map((a) => a.id).join(",")]); // 回到账号页、列表重新排序后也要滑到选中的卡

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
      setState(s);
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
      setState(await api<State>("DELETE", "/api/pending-switch"));
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
      setState(await api<State>("PATCH", `/api/accounts/${a.id}`, v));
      setEditing(null);
      toast("已保存");
    });

  const remove = (a: Account) =>
    run(a.id, async () => {
      setState(await api<State>("DELETE", `/api/accounts/${a.id}`));
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
      setState(s);
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
      setState(await api<State>("POST", "/api/accounts", v));
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
          <nav className="tabs" aria-label="页面">
            <button className={`tab${tab === "accounts" ? " on" : ""}`} onClick={() => setTab("accounts")}>
              账号
            </button>
            <button className={`tab${tab === "vault" ? " on" : ""}`} onClick={() => setTab("vault")}>
              账号资料与邮箱
            </button>
            {inTauri && <button className={`tab${tab === "settings" ? " on" : ""}`} onClick={() => setTab("settings")}>
              手机与设置
            </button>}
          </nav>
        <ThemeToggle toast={toast} />
        {tab === "accounts" && (
          <button className="btn" disabled={!!busy} onClick={() => refresh()}>
            {busy === "all" ? "刷新中…" : "刷新用量"}
          </button>
        )}
      </header>

      {tab === "vault" ? <AccountVault accounts={state?.accounts || []} onAccountsChanged={setState} toast={toast} /> : tab === "settings" ? (
        <>
          <PhoneAccess toast={toast} />
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
          <Privacy toast={toast} onChanged={load} onCleared={envSc.reset} />
        </>
      ) : (
        <main aria-live="polite">
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
          {state && risky && <EnvCard state={state} sc={envSc} />}
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
                  <AccountTile key={a.id} a={a} selected={a.id === selected.id} onSelect={select} />
                ))}
                <button type="button" className="tile add" disabled={!!busy} onClick={() => setAdding(true)}>
                  <span className="plus">＋</span>添加账号
                </button>
              </div>
              <AccountDetail
                a={selected}
                busy={!!busy}
                onSwitch={switchTo}
                onOpen={openWeb}
                onRefresh={(x) => refresh(x.id)}
                onEdit={setEditing}
                onRemove={setRemoving}
              />
            </>
          )}
          {state && !risky && <EnvCard state={state} sc={envSc} />}
          {state && <LocalUsage accounts={state.accounts} stamp={state.generated_at} />}
          {state && <Rules accounts={state.accounts} stamp={state.generated_at} toast={toast} />}
          {state && (
            <p className="hint foot">
              后端：{state.backend} {state.version}，数据更新于 {new Date(state.generated_at).toLocaleString()}
            </p>
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
