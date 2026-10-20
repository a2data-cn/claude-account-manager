import { useEffect, useState, type FormEvent } from "react";
import { api } from "../api";
import type { Account } from "../types";

interface Rule {
  id: string;
  type: "dir" | "time";
  account: string;
  path?: string;
  days?: number[];
  start?: string;
  end?: string;
}
interface RulesView {
  rules: Rule[];
  alert_threshold: number;
}

const DAY_TXT: Record<string, string> = { "1,2,3,4,5": "工作日", "6,7": "周末", "1,2,3,4,5,6,7": "每天" };
const dayText = (d: number[]) => DAY_TXT[d.join(",")] || "周" + d.map((x) => "一二三四五六日"[x - 1]).join("、");

function inWindow(r: Rule, now = new Date()): boolean {
  if (r.type !== "time" || !r.start || !r.end || !r.days) return false;
  const wd = now.getDay() === 0 ? 7 : now.getDay();
  const mins = now.getHours() * 60 + now.getMinutes();
  const toM = (t: string) => {
    const [h, m] = t.split(":").map(Number);
    return h * 60 + m;
  };
  const st = toM(r.start);
  const en = toM(r.end);
  if (st < en) return r.days.includes(wd) && mins >= st && mins < en;
  const prev = ((wd + 5) % 7) + 1;
  return (r.days.includes(wd) && mins >= st) || (r.days.includes(prev) && mins < en);
}

export function Rules({
  accounts,
  stamp,
  toast,
}: {
  accounts: Account[];
  stamp: string;
  toast: (m: string, bad?: boolean) => void;
}) {
  const [view, setView] = useState<RulesView | null>(null);
  const [type, setType] = useState<"time" | "dir">("time");
  const [path, setPath] = useState("");
  const [days, setDays] = useState("1-5");
  const [start, setStart] = useState("09:00");
  const [end, setEnd] = useState("18:00");
  const [acc, setAcc] = useState("");
  const [alert, setAlert] = useState("90");
  const code = accounts.filter((a) => a.kind === "claude_code");

  useEffect(() => {
    api<RulesView>("GET", "/api/rules")
      .then((v) => {
        setView(v);
        setAlert(String(v.alert_threshold));
      })
      .catch(() => setView(null));
  }, [stamp]);

  useEffect(() => {
    if (!code.some((a) => a.id === acc) && code.length) setAcc(code[0].id);
  }, [code, acc]);

  if (!view || !code.length) return null;
  const names = Object.fromEntries(accounts.map((a) => [a.id, a.label || a.email || a.id]));
  const live = view.rules.find((r) => inWindow(r));

  const save = async (body: Partial<{ rules: unknown[]; alert_threshold: number }>, msg: string) => {
    try {
      setView(await api<RulesView>("PUT", "/api/rules", body));
      toast(msg);
    } catch (e) {
      toast((e as Error).message, true);
    }
  };
  const add = (e: FormEvent) => {
    e.preventDefault();
    const r = type === "dir" ? { type, path: path.trim(), account: acc } : { type, days, start, end, account: acc };
    save({ rules: [...view.rules, r] }, "规则已添加");
  };

  return (
    <section className="block">
      <h2>切号规则</h2>
      <div className="panel">
        <p className="hint">
          时间规则在进入时间段时自动切换，时间段内你手动换号不会被改回去。目录规则在终端里用 <code>cam claude</code> 代替 <code>claude</code> 启动时生效。
        </p>
        <ul className="rules">
          {view.rules.length ? (
            view.rules.map((r) => (
              <li key={r.id}>
                <span className="r-kind">{r.type === "dir" ? "目录" : "时间"}</span>
                <span className="r-what">
                  {r.type === "dir" ? r.path : `${dayText(r.days || [])} ${r.start}–${r.end}`} → <b>{names[r.account] || "已删除的账号"}</b>
                  {live?.id === r.id && <span className="now">当前时间段</span>}
                </span>
                <button className="btn quiet" onClick={() => save({ rules: view.rules.filter((x) => x.id !== r.id) }, "规则已删除")}>
                  删除
                </button>
              </li>
            ))
          ) : (
            <li className="hint">还没有规则。比如：工作日 9:00–18:00 自动用工作号。</li>
          )}
        </ul>
        <form className="rule-form" onSubmit={add}>
          <div className="field">
            <label htmlFor="r-type">规则</label>
            <select id="r-type" value={type} onChange={(e) => setType(e.target.value as "time" | "dir")}>
              <option value="time">按时间段</option>
              <option value="dir">按项目目录</option>
            </select>
          </div>
          {type === "dir" ? (
            <div className="field grow">
              <label htmlFor="r-path">目录（含子目录）</label>
              <input id="r-path" placeholder="~/work" value={path} onChange={(e) => setPath(e.target.value)} />
            </div>
          ) : (
            <>
              <div className="field">
                <label htmlFor="r-days">星期</label>
                <select id="r-days" value={days} onChange={(e) => setDays(e.target.value)}>
                  <option value="1-5">工作日</option>
                  <option value="6,7">周末</option>
                  <option value="1-7">每天</option>
                </select>
              </div>
              <div className="field">
                <label htmlFor="r-start">从</label>
                <input id="r-start" type="time" value={start} onChange={(e) => setStart(e.target.value)} />
              </div>
              <div className="field">
                <label htmlFor="r-end">到</label>
                <input id="r-end" type="time" value={end} onChange={(e) => setEnd(e.target.value)} />
              </div>
            </>
          )}
          <div className="field">
            <label htmlFor="r-acc">切到</label>
            <select id="r-acc" value={acc} onChange={(e) => setAcc(e.target.value)}>
              {code.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.label || a.email}
                </option>
              ))}
            </select>
          </div>
          <button className="btn primary" type="submit">
            添加规则
          </button>
        </form>
        <form
          className="alert-row"
          onSubmit={(e) => {
            e.preventDefault();
            save({ alert_threshold: Number(alert) }, "提醒阈值已保存");
          }}
        >
          <label htmlFor="alert-pct">当前账号 5 小时额度超过</label>
          <input id="alert-pct" type="number" min={1} max={100} value={alert} onChange={(e) => setAlert(e.target.value)} />
          <span>% 时弹通知，并告诉你多久后重置</span>
          <button className="btn" type="submit">
            保存
          </button>
        </form>
      </div>
    </section>
  );
}
