import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, command, inTauri, onEvent } from "./api";
import { AccountCard } from "./components/AccountCard";
import { AddDialog, EditDialog, SwitchChoiceDialog, type AddValues, type EditValues, type SwitchChoice } from "./components/Dialogs";
import { LocalUsage } from "./components/LocalUsage";
import { PhoneAccess } from "./components/PhoneAccess";
import { Rules } from "./components/Rules";
import type { Account, ServiceCheckItem, State, SwitchResult } from "./types";

type Tab = "accounts" | "settings";
const KIND_ORDER = ["claude_code", "claude_web", "api"];
const CHECK_MARK: Record<ServiceCheckItem["level"], string> = { ok: "✓", bad: "✗", warn: "!", info: "·" };

/** Claude 服务检测：后端做 DNS、出口地区、两个域名、代理 / VPN 的探测，再列出环境里的风险点，要等几秒。只提醒，不拦 */
function ServiceCheck({ toast }: { toast: (m: string, bad?: boolean) => void }) {
  const [items, setItems] = useState<ServiceCheckItem[] | null>(null);
  const [checking, setChecking] = useState(false);
  const check = async () => {
    setChecking(true);
    try {
      setItems((await api<{ items: ServiceCheckItem[] }>("GET", "/api/service-check")).items);
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      setChecking(false);
    }
  };
  return (
    <section className="block">
      <h2>Claude 服务检测</h2>
      <div className="panel">
        <p className="hint">检查 DNS、出口地区（对照 Anthropic 官方名单）、api.anthropic.com、claude.ai、代理 / VPN、时区、时钟、官方状态页和账号使用地区，要等几秒钟。只提醒，不拦任何操作。</p>
        <button className="btn" disabled={checking} onClick={check}>
          {checking ? "检测中…" : "开始检测"}
        </button>
        {items && (
          <ul className="checks">
            {items.map((it, i) => (
              <li key={i} className={it.level}>
                <span className="mark">{CHECK_MARK[it.level] || "·"}</span>
                <span>
                  {it.text}
                  {it.advice && <span className="advice">→ {it.advice}</span>}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}

export default function App() {
  const [state, setState] = useState<State | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [editing, setEditing] = useState<Account | null>(null);
  const [adding, setAdding] = useState(false);
  const [choice, setChoice] = useState<SwitchChoice | null>(null); // 有会话在跑时的三选一
  const [tab, setTab] = useState<Tab>("accounts");
  const [toastMsg, setToastMsg] = useState<{ text: string; bad: boolean } | null>(null);
  const toastTimer = useRef<number>();
  const [, tick] = useState(0);

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

  const run = async (key: string, fn: () => Promise<void>) => {
    setBusy(key);
    try {
      await fn();
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      setBusy(null);
    }
  };

  const refresh = (id?: string) =>
    run(id || "all", async () => {
      const s = await api<State>("POST", "/api/refresh", id ? { id } : {});
      setState(s);
      const errs = s.accounts.filter((a) => (!id || a.id === id) && a.usage?.error).length;
      toast(errs ? `已刷新，${errs} 个账号没拿到用量` : "用量已更新");
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
    });

  const saveEdit = (a: Account, v: EditValues) =>
    run(a.id, async () => {
      setState(await api<State>("PATCH", `/api/accounts/${a.id}`, v));
      setEditing(null);
      toast("已保存");
    });

  const remove = (a: Account) => {
    if (!confirm(`删除“${a.label || a.email}”？凭据会移到备份目录，不会直接销毁。`)) return;
    run(a.id, async () => {
      setState(await api<State>("DELETE", `/api/accounts/${a.id}`));
      setEditing(null);
      toast("已删除");
    });
  };

  const add = (v: AddValues) =>
    run("add", async () => {
      setState(await api<State>("POST", "/api/accounts", v));
      setAdding(false);
      toast(v.kind === "claude_code" ? "已导入当前登录的账号" : "已添加");
    });

  return (
    <div className="wrap">
      <header className="top">
        <h1>Claude 账号</h1>
        {state && (
          <span className="host">
            {state.host}
            {state.region?.status === "supported" && <span className="reg-ok"> · 出口 {state.region.loc} ✓</span>}
            {state.region?.status === "unsupported" && (
              <span className="reg-bad"> · 出口 {state.region.loc} ⚠ 不在支持名单</span>
            )}
            {state.region?.status === "unknown" && <span> · 出口地区未知</span>}
          </span>
        )}
        <span className="spacer" />
        {inTauri && (
          <nav className="tabs" aria-label="页面">
            <button className={`tab${tab === "accounts" ? " on" : ""}`} onClick={() => setTab("accounts")}>
              账号
            </button>
            <button className={`tab${tab === "settings" ? " on" : ""}`} onClick={() => setTab("settings")}>
              手机与设置
            </button>
          </nav>
        )}
        {tab === "accounts" && (
          <>
            <button className="btn quiet" onClick={() => setAdding(true)}>
              添加账号
            </button>
            <button className="btn" disabled={busy === "all"} onClick={() => refresh()}>
              {busy === "all" ? "刷新中…" : "刷新用量"}
            </button>
          </>
        )}
      </header>

      {tab === "settings" ? (
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
          <ServiceCheck toast={toast} />
        </>
      ) : (
        <main aria-live="polite">
          {state?.warning && (
            <div className="notice" role="alert">
              ⚠ {state.warning}
            </div>
          )}
          {(state?.risks ?? [])
            .filter((r) => r.level === "warn" || r.level === "bad")
            .map((r) => (
              <div key={r.kind ?? r.text} className={`notice${r.level === "bad" || r.kind === "region" ? " bad" : ""}`} role="alert">
                ⚠ {r.text}
                {r.advice && <span className="n-advice">{r.advice}</span>}
              </div>
            ))}
          {state?.pending_switch && (
            <div className="notice pending" role="status">
              <span className="n-text">{state.pending_switch.message}</span>
              <button className="btn" disabled={busy === "pending"} onClick={cancelPending}>
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
          {state && !state.accounts.length && (
            <div className="panel empty">
              <h2>还没有账号</h2>
              <p className="hint">先把这台电脑上当前登录的 Claude Code 账号存进来。以后你在 Claude Code 里登录新账号，这里会自动发现并保存。</p>
              <p>
                <button className="btn primary" onClick={() => add({ kind: "claude_code" })}>
                  导入当前登录
                </button>
              </p>
            </div>
          )}
          {sorted.map((a) => (
            <AccountCard
              key={a.id}
              a={a}
              busy={busy === a.id}
              onSwitch={switchTo}
              onOpen={openWeb}
              onRefresh={(x) => refresh(x.id)}
              onEdit={setEditing}
            />
          ))}
          {state && <Rules accounts={state.accounts} stamp={state.generated_at} toast={toast} />}
          {state && <LocalUsage accounts={state.accounts} stamp={state.generated_at} />}
          {state && (
            <p className="hint">
              {state.backend} {state.version}，数据更新于 {new Date(state.generated_at).toLocaleString()}
            </p>
          )}
        </main>
      )}

      <EditDialog account={editing} onClose={() => setEditing(null)} onSave={saveEdit} onDelete={remove} />
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
