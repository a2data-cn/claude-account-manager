import { useEffect, useState } from "react";
import { api } from "../api";
import type { PrivacyInfo } from "../types";

type KeyField = "ipapi_key" | "ipinfo_token";
const KEYS: [KeyField, string, string, "has_ipapi_key" | "has_ipinfo_token", string][] = [
  ["ipapi_key", "ipapi.is key", "免费注册 · 每天 1000 次 · 可商用 · 带机房 / VPN 标记", "has_ipapi_key", "没配：每天 30 次，不带类型标记"],
  ["ipinfo_token", "ipinfo.io token", "可选 · 上一家查不到时用", "has_ipinfo_token", "没配也能用"],
];
const GROUPS: ["anthropic" | "third", string][] = [
  ["anthropic", "Anthropic（访问 Claude 时本来就会连）"],
  ["third", "第三方（只发出口 IP）"],
];

/** 隐私与数据源：外部 IP 查询三选一、每个 key 一行、会访问的服务（默认折叠）、清除地区记录。和手机面板同一套布局 */
export function Privacy({
  toast,
  onChanged,
  onCleared,
}: {
  toast: (m: string, bad?: boolean) => void;
  onChanged: () => void;
  onCleared?: () => void;
}) {
  const [p, setP] = useState<PrivacyInfo | null>(null);
  const [vals, setVals] = useState<Record<KeyField, string>>({ ipapi_key: "", ipinfo_token: "" });
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api<PrivacyInfo>("GET", "/api/privacy")
      .then(setP)
      .catch((e) => toast((e as Error).message, true));
  }, [toast]);

  const call = async (method: "PUT" | "DELETE", body: Record<string, string> | undefined, msg: string) => {
    setBusy(true);
    try {
      setP(await api<PrivacyInfo>(method, method === "PUT" ? "/api/privacy" : "/api/privacy/history", body));
      toast(msg);
      onChanged();
      return true;
    } catch (e) {
      toast((e as Error).message, true);
      return false;
    } finally {
      setBusy(false);
    }
  };

  if (!p) return null;
  const third = p.sources.filter((s) => s.group === "third").length;
  return (
    <section className="block">
      <div className="sec-head">
        <h2>隐私与数据源</h2>
        <span className="hint">环境检测用哪些服务、要不要填 key，以及账号的使用地区记录</span>
      </div>
      <div className="cards">
        <div className="panel ecard wide">
        <h3>
          <span className="eico">🔍</span>外部 IP 查询<span className="hint">环境检测只发送电脑的出口 IP，不发送任何账号、邮箱或凭据</span>
        </h3>
        {p.env_override && (
          <p className="hint" style={{ margin: "0 0 8px" }}>
            被环境变量 CAM_IP_LOOKUP 覆盖，这里改不了
          </p>
        )}
        <div
          className="pv-modes"
          role="radiogroup"
          aria-label="外部 IP 查询"
          onKeyDown={(e) => {
            // 单选组：方向键切换，只有选中的那个能 Tab 到
            if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(e.key)) return;
            const i = p.modes.findIndex((m) => m.value === p.ip_lookup);
            if (i < 0 || busy || p.env_override) return;
            e.preventDefault();
            const back = e.key === "ArrowLeft" || e.key === "ArrowUp";
            const next = p.modes[(i + (back ? p.modes.length - 1 : 1)) % p.modes.length];
            call("PUT", { ip_lookup: next.value }, "已保存，下次检测按新设置").then(() =>
              (e.currentTarget.querySelector(`[data-mode="${next.value}"]`) as HTMLElement | null)?.focus(),
            );
          }}
        >
          {p.modes.map((m) => (
            <button
              key={m.value}
              type="button"
              role="radio"
              data-mode={m.value}
              tabIndex={m.value === p.ip_lookup ? 0 : -1}
              aria-checked={m.value === p.ip_lookup}
              className={`seg-opt${m.value === p.ip_lookup ? " on" : ""}`}
              disabled={busy || p.env_override}
              onClick={() => m.value !== p.ip_lookup && call("PUT", { ip_lookup: m.value }, "已保存，下次检测按新设置")}
            >
              <b>{m.title || m.label}</b>
              <span>{m.desc}</span>
            </button>
          ))}
        </div>
        {p.ip_lookup !== "off" && (
          <div className="keys">
            {KEYS.map(([k, name, desc, has, empty]) => (
              <div className="key-row" key={k}>
                <div className="key-name">
                  <b>
                    {name} <span className={`chip${p[has] ? " c-ok" : ""}`}>{p[has] ? "已配置" : "未配置"}</span>
                  </b>
                  <small>{desc}</small>
                </div>
                <input
                  type="password"
                  autoComplete="off"
                  aria-label={name}
                  value={vals[k]}
                  placeholder={p[has] ? "已配置，输入新的可替换" : empty}
                  onChange={(e) => setVals({ ...vals, [k]: e.target.value })}
                />
                <div className="key-acts">
                  <button
                    type="button"
                    className="btn"
                    disabled={busy}
                    onClick={() => {
                      const v = vals[k].trim();
                      if (!v) return toast("先填上 key 再保存", true);
                      call("PUT", { [k]: v }, "已保存").then((ok) => ok && setVals({ ...vals, [k]: "" }));
                    }}
                  >
                    保存
                  </button>
                  {p[has] && (
                    <button type="button" className="btn quiet danger" disabled={busy} onClick={() => call("PUT", { [k]: "" }, "已删除")}>
                      删除
                    </button>
                  )}
                </div>
              </div>
            ))}
          </div>
        )}
        </div>
        <div className="panel ecard">
        <h3>
          <span className="eico">🔗</span>会访问的服务
        </h3>
        <details className="src">
          <summary>
            {third
              ? `会访问 ${p.sources.length} 个服务，其中第三方 ${third} 个 · 出口 IP 不变时运营商信息 ${p.meta_ttl_hours} 小时内不重查`
              : `只访问 ${p.sources.length} 个 Anthropic 服务，不访问任何第三方`}
          </summary>
          {GROUPS.map(([g, title]) => {
            const rows = p.sources.filter((s) => (s.group || "third") === g);
            return rows.length ? (
              <div className="src-group" key={g}>
                <h5>{title}</h5>
                <ul className="sources">
                  {rows.map((s) => (
                    <li key={s.name}>
                      <b>
                        {s.name}
                        {s.warn && <span className="chip st-warn"> 仅限非商业</span>}
                      </b>
                      <span>{s.what}</span>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null;
          })}
        </details>
        </div>
        <div className="panel ecard">
        <h3>
          <span className="eico">📍</span>地区记录
        </h3>
        <div className="pv-foot">
          <span>地区记录：{p.history.accounts} 个账号记过使用地区，用来提醒"同一账号多地区使用"</span>
          <button
            className="btn"
            disabled={busy}
            onClick={() => call("DELETE", undefined, "已清除出口信息和地区记录").then((ok) => ok && onCleared?.())}
          >
            清除地区记录
          </button>
        </div>
        </div>
      </div>
    </section>
  );
}
