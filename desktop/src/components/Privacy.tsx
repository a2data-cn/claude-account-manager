import { useEffect, useState } from "react";
import { api } from "../api";
import type { PrivacyInfo } from "../types";

/** 隐私与数据源：外部 IP 查询的开关和数据源、ipapi.is / ipinfo 的 key、清除地区记录。只发出口 IP，不发账号信息 */
export function Privacy({ toast, onChanged }: { toast: (m: string, bad?: boolean) => void; onChanged: () => void }) {
  const [p, setP] = useState<PrivacyInfo | null>(null);
  const [key, setKey] = useState("");
  const [tok, setTok] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api<PrivacyInfo>("GET", "/api/privacy")
      .then(setP)
      .catch((e) => toast((e as Error).message, true));
  }, [toast]);

  const save = async (body: Record<string, string>, msg: string, method: "PUT" | "DELETE" = "PUT") => {
    setBusy(true);
    try {
      setP(await api<PrivacyInfo>(method, method === "PUT" ? "/api/privacy" : "/api/privacy/history", method === "PUT" ? body : undefined));
      toast(msg);
      onChanged();
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      setBusy(false);
    }
  };

  if (!p) return null;
  return (
    <section className="block">
      <h2>隐私与数据源</h2>
      <div className="panel">
        <p className="hint">环境检测只发送这台电脑的出口 IP，不发送任何账号、邮箱或凭据。下面可以选择查不查运营商和 IP 类型、用哪家的数据。</p>
        <div className="field">
          <label htmlFor="pv-mode">外部 IP 查询</label>
          <select
            id="pv-mode"
            value={p.ip_lookup}
            disabled={busy || p.env_override}
            onChange={(e) => save({ ip_lookup: e.target.value }, "已保存，下次检测按新设置")}
          >
            {p.modes.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
          {p.env_override && <span className="hint">被环境变量 CAM_IP_LOOKUP 覆盖，这里改不了</span>}
        </div>
        <form
          className="row2"
          onSubmit={(e) => {
            e.preventDefault();
            const body: Record<string, string> = {};
            if (key.trim()) body.ipapi_key = key.trim();
            if (tok.trim()) body.ipinfo_token = tok.trim();
            if (!Object.keys(body).length) return;
            save(body, "已保存 key").then(() => {
              setKey("");
              setTok("");
            });
          }}
        >
          <div className="field">
            <label htmlFor="pv-key">ipapi.is key（免费注册，每天 1000 次，可商用，带机房 / VPN 标记）</label>
            <input id="pv-key" type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)}
              placeholder={p.has_ipapi_key ? "已配置，留空不修改" : "没配：每天 30 次，不带类型标记"} />
          </div>
          <div className="field">
            <label htmlFor="pv-tok">ipinfo.io token（可选）</label>
            <input id="pv-tok" type="password" autoComplete="off" value={tok} onChange={(e) => setTok(e.target.value)}
              placeholder={p.has_ipinfo_token ? "已配置，留空不修改" : "没配"} />
          </div>
          <div className="pv-acts">
            <button className="btn" type="submit" disabled={busy || (!key.trim() && !tok.trim())}>
              保存 key
            </button>
            {(p.has_ipapi_key || p.has_ipinfo_token) && (
              <button className="btn quiet danger" type="button" disabled={busy}
                onClick={() => save({ ipapi_key: "", ipinfo_token: "" }, "已删除 key")}>
                删除已保存的 key
              </button>
            )}
          </div>
        </form>
        <h4 className="pv-h">当前设置下会访问</h4>
        <ul className="sources">
          {p.sources.map((s) => (
            <li key={s.name}>
              <b>{s.name}</b>
              <span>{s.what}</span>
            </li>
          ))}
        </ul>
        <p className="hint">出口 IP 没变时，运营商信息 {p.meta_ttl_hours} 小时内不重查。</p>
        <div className="alert-row">
          <span>地区记录：{p.history.accounts} 个账号记过使用地区（用来提醒"同一账号多地区使用"）</span>
          <button className="btn" disabled={busy} onClick={() => save({}, "已清除出口信息和地区记录", "DELETE")}>
            清除地区记录
          </button>
        </div>
      </div>
    </section>
  );
}
