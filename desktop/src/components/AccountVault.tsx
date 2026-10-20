import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../api";
import { ago } from "../format";
import type { Account, State } from "../types";
import { openVaultUrl, safeVaultError, vaultUrl, VAULT_PROVIDERS, type VaultEntry, type VaultJob, type VaultMessages, type VaultProvider, type VaultState } from "../vault";

const emptyForm = () => ({ id: "", label: "", email: "", provider: "credential" as VaultProvider, account_id: "", password: "", clear_password: false, client_id: "", client_secret: "", tenant: "common", imap_host: "", imap_port: "993", imap_username: "", notes: "" });

export function AccountVault({ accounts, onAccountsChanged, toast }: { accounts: Account[]; onAccountsChanged: (state: State) => void; toast: (message: string, bad?: boolean) => void }) {
  const [state, setState] = useState<VaultState | null>(null);
  const [error, setError] = useState("");
  const [master, setMaster] = useState("");
  const [confirm, setConfirm] = useState("");
  const [form, setForm] = useState(emptyForm);
  const [selected, setSelected] = useState("");
  const [busy, setBusy] = useState(false);
  const working = useRef(false);
  const [revealed, setRevealed] = useState<string | null>(null);
  const [messages, setMessages] = useState<VaultMessages | null>(null);
  const [job, setJob] = useState<VaultJob | null>(null);
  const [jobMessage, setJobMessage] = useState("");
  const [autoRead, setAutoRead] = useState(true);
  const [waiting, setWaiting] = useState<{ entry_id: string; since: string; until: number } | null>(null);
  const mailEpoch = useRef(0);
  const selectedRef = useRef("");
  const unlockedRef = useRef(false);
  const active = state?.entries.find((entry) => entry.id === selected);
  const oauth = form.provider === "gmail" || form.provider === "outlook";
  const set = (key: keyof ReturnType<typeof emptyForm>, value: string | boolean) => setForm((current) => ({ ...current, [key]: value }));

  const clearContext = () => {
    mailEpoch.current += 1;
    setWaiting(null); setRevealed(null); setMessages(null); setJob(null); setJobMessage("");
  };
  const clearPrivate = () => {
    clearContext(); selectedRef.current = "";
    setMaster(""); setConfirm(""); setSelected(""); setForm(emptyForm());
  };
  const apply = (next: VaultState) => {
    unlockedRef.current = next.unlocked;
    setState(next);
    if (!next.unlocked) clearPrivate();
  };
  const run = async (action: () => Promise<void>, quiet = false) => {
    if (working.current) return;
    working.current = true; setBusy(true);
    try { await action(); setError(""); }
    catch (cause) {
      const message = safeVaultError(cause, [master, form.password, form.client_secret, revealed || ""]);
      setError(message);
      if (!quiet) toast(message, true);
      try { const latest = await api<VaultState>("GET", "/api/vault"); if (!latest.unlocked) apply(latest); } catch { /* 原始错误已显示 */ }
    } finally { working.current = false; setBusy(false); }
  };
  const reload = async () => apply(await api<VaultState>("GET", "/api/vault"));

  useEffect(() => { void run(reload); }, []);
  useEffect(() => {
    if (revealed === null) return;
    const timer = window.setTimeout(() => setRevealed(null), 30000);
    return () => window.clearTimeout(timer);
  }, [revealed]);
  useEffect(() => {
    if (!job) return;
    let cancelled = false;
    let polling = false;
    const until = Date.now() + 5 * 60 * 1000;
    const epoch = mailEpoch.current;
    const poll = async () => {
      if (polling || cancelled) return;
      if (Date.now() > until) { setJob(null); setJobMessage("授权等待已结束，请重新连接邮箱"); return; }
      polling = true;
      try {
        const result = await api<{ status: "pending" | "connected" | "error"; message: string }>("GET", `/api/vault/oauth/${encodeURIComponent(job.job_id)}`);
        if (cancelled || epoch !== mailEpoch.current || !unlockedRef.current) return;
        setJobMessage(safeVaultError(new Error(result.message || (result.status === "pending" ? "等待在电脑浏览器中完成授权…" : "授权已完成"))));
        if (result.status !== "pending") {
          setJob(null);
          if (result.status === "connected") { await reload(); toast("邮箱已连接，可以读取 Claude 验证邮件"); }
        }
      } catch (cause) { if (!cancelled && epoch === mailEpoch.current) { setJob(null); setJobMessage(safeVaultError(cause)); try { const latest = await api<VaultState>("GET", "/api/vault"); if (!latest.unlocked) apply(latest); } catch { /* 保留授权错误 */ } } }
      finally { polling = false; }
    };
    const timer = window.setInterval(() => void poll(), 2000);
    void poll();
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [job?.job_id]);

  useEffect(() => () => { mailEpoch.current += 1; unlockedRef.current = false; }, []);
  const checkMail = (quiet = false, since?: string) => run(async () => {
    if (!active || active.provider === "credential") return;
    const epoch = mailEpoch.current;
    const result = await api<VaultMessages>("POST", `/api/vault/entries/${encodeURIComponent(active.id)}/messages`, since ? { since } : {});
    if (epoch !== mailEpoch.current || active.id !== selectedRef.current || !unlockedRef.current) return;
    setMessages(result);
    if (since && result.status === "found") setWaiting(null);
  }, quiet);
  useEffect(() => {
    if (!autoRead || waiting || !state?.unlocked || !active?.connected || active.provider === "credential") return;
    void checkMail(true);
    const timer = window.setInterval(() => void checkMail(true), 30000);
    return () => window.clearInterval(timer);
  }, [autoRead, waiting, state?.unlocked, active?.id, active?.connected]);
  useEffect(() => {
    if (!waiting || !state?.unlocked || !active?.connected || waiting.entry_id !== active.id) return;
    const poll = () => {
      if (Date.now() >= waiting.until) { setWaiting(null); toast("等待已结束，暂未收到新的 Claude 验证邮件"); return; }
      void checkMail(true, waiting.since);
    };
    const timer = window.setInterval(poll, 5000);
    poll();
    return () => window.clearInterval(timer);
  }, [waiting, state?.unlocked, active?.id, active?.connected]);

  const choose = (entry?: VaultEntry) => {
    clearContext(); selectedRef.current = entry?.id || "";
    setSelected(entry?.id || "");
    setForm(entry ? { ...emptyForm(), id: entry.id, label: entry.label, email: entry.email, provider: entry.provider, account_id: entry.account_id || "", ...entry.config, imap_port: String(entry.config.imap_port || 993) } : emptyForm());
    setError("");
  };
  const authenticate = (event: FormEvent) => {
    event.preventDefault();
    if (!state?.initialized && master !== confirm) { setError("两次输入的主密码不一致"); return; }
    void run(async () => {
      apply(await api<VaultState>("POST", state?.initialized ? "/api/vault/unlock" : "/api/vault/setup", { master_password: master }));
      setMaster(""); setConfirm(""); toast("账号库已解锁");
    });
  };
  const save = (event: FormEvent) => {
    event.preventDefault();
    void run(async () => {
      const next = await api<VaultState>("POST", "/api/vault/entries", {
        id: form.id || undefined, label: form.label.trim(), email: form.email.trim(), provider: form.provider, account_id: form.account_id || null,
        config: { client_id: form.client_id.trim(), tenant: form.tenant.trim() || "common", imap_host: form.imap_host.trim(), imap_port: Number(form.imap_port) || 993, imap_username: form.imap_username.trim(), notes: form.notes.trim() },
        ...(form.password ? { password: form.password } : {}), ...(form.client_secret ? { client_secret: form.client_secret } : {}), clear_password: form.clear_password,
      });
      apply(next);
      const entry = next.entries.find((item) => item.id === form.id) || next.entries.find((item) => item.email.toLowerCase() === form.email.trim().toLowerCase() && item.provider === form.provider);
      choose(entry); toast("账号资料已保存");
    });
  };
  const identify = () => run(async () => {
    if (!form.account_id) throw new Error("先选择要绑定的 Claude 账号");
    const boundId = form.account_id;
    const next = await api<State & { identity_message?: string }>("POST", `/api/accounts/${encodeURIComponent(boundId)}/identify`);
    onAccountsChanged(next);
    const account = next.accounts.find((item) => item.id === form.account_id);
    if (!account?.email) throw new Error(next.identity_message || "未识别到这个账号的已登录邮箱，请先在对应 Claude 配置中登录");
    setForm((current) => current.account_id === boundId ? { ...current, email: account.email || "", label: current.label || account.label || account.email || "" } : current);
    toast(next.identity_message || "已读取登录邮箱");
  });
  const connect = () => run(async () => {
    if (!active) return;
    const epoch = mailEpoch.current;
    const next = await api<VaultJob>("POST", `/api/vault/entries/${encodeURIComponent(active.id)}/connect`);
    if (epoch !== mailEpoch.current || active.id !== selectedRef.current || !unlockedRef.current) return;
    if (!vaultUrl(next.authorization_url, true)) throw new Error("授权地址无法识别，请检查邮箱配置");
    setJob(next); setJobMessage("请在电脑浏览器中完成邮箱授权…");
    try { await openVaultUrl(next.authorization_url, true); }
    catch { toast("授权已准备好，请点击“在电脑上打开授权页”继续", true); }
  });
  const copy = (value: string) => run(async () => {
    if (!navigator.clipboard) throw new Error("当前环境无法直接复制，请显式显示后手动复制");
    await navigator.clipboard.writeText(value); toast("已复制");
  });

  return <section className="block vault" aria-label="账号资料与邮箱">
    <div className="sec-head"><h2>账号资料与邮箱</h2><span className="spacer" />
      <button className="btn" disabled={busy} onClick={() => void run(reload)}>重新读取</button>
      {state?.unlocked && <button className="btn" disabled={busy} onClick={() => void run(async () => { apply(await api<VaultState>("POST", "/api/vault/lock")); toast("账号库已锁定"); })}>锁定账号库</button>}
    </div>
    <p className="hint">账号资料保存在本机 SQLite 账号库，密码和邮箱授权加密保存。首次授权邮箱后，本页解锁并打开时可自动读取 Claude 登录验证码和链接；Claude 登录、回跳和多因素验证仍由你完成。</p>
    {error && <div className="notice" role="alert">{error}</div>}
    {!state ? <div className="panel"><p className="hint">正在读取账号库…</p></div> : !state.unlocked ? <form className="panel vault-unlock" onSubmit={authenticate}>
      <h3>{state.initialized ? "解锁本地账号库" : "设置账号库主密码"}</h3>
      <p className="hint">主密码用于本地加密，至少 8 个字符。请妥善保存；忘记后无法解密已存的密码与授权。每个后端进程需单独解锁。</p>
      <div className="field"><label htmlFor="vault-master">主密码</label><input id="vault-master" type="password" minLength={8} required autoComplete={state.initialized ? "current-password" : "new-password"} value={master} onChange={(event) => setMaster(event.target.value)} /></div>
      {!state.initialized && <div className="field"><label htmlFor="vault-confirm">再次输入主密码</label><input id="vault-confirm" type="password" minLength={8} required autoComplete="new-password" value={confirm} onChange={(event) => setConfirm(event.target.value)} /></div>}
      <button className="btn primary" disabled={busy} type="submit">{busy ? "处理中…" : state.initialized ? "解锁" : "创建加密账号库"}</button>
    </form> : <>
      <div className="vault-list">
        {state.entries.map((entry) => <button type="button" key={entry.id} className={`vault-entry${selected === entry.id ? " sel" : ""}`} aria-pressed={selected === entry.id} disabled={busy} onClick={() => choose(entry)}><b>{entry.label || entry.email}</b><span>{entry.email}</span><small>{VAULT_PROVIDERS[entry.provider]}{entry.connected ? " · 已保存邮箱授权" : ""}</small></button>)}
        <button className="btn" type="button" disabled={busy} onClick={() => choose()}>＋ 新增账号资料</button>
      </div>
      <form className="panel vault-form" onSubmit={save}>
        <h3>{form.id ? "编辑账号资料" : "新增账号资料"}</h3>
        <div className="row2"><div className="field"><label htmlFor="vault-label">名称</label><input id="vault-label" maxLength={60} value={form.label} onChange={(event) => set("label", event.target.value)} /></div><div className="field"><label htmlFor="vault-email">邮箱</label><input id="vault-email" type="email" required value={form.email} onChange={(event) => set("email", event.target.value)} /></div></div>
        <div className="field"><label htmlFor="vault-account">绑定 Claude 账号（可选）</label><div className="vault-inline"><select id="vault-account" value={form.account_id} onChange={(event) => {
          const account = accounts.find((item) => item.id === event.target.value);
          setForm((current) => ({ ...current, account_id: event.target.value, email: account?.email || current.email, label: current.label || account?.label || "" }));
        }}><option value="">不绑定</option>{accounts.filter((account) => account.kind !== "api").map((account) => <option key={account.id} value={account.id}>{account.label || account.email || account.id}</option>)}</select><button className="btn" type="button" disabled={busy || !form.account_id} onClick={() => void identify()}>读取已登录邮箱</button></div></div>
        <div className="field"><label htmlFor="vault-provider">邮箱接入方式</label><select id="vault-provider" value={form.provider} onChange={(event) => { set("provider", event.target.value); setMessages(null); }}>{Object.entries(VAULT_PROVIDERS).map(([value, label]) => <option value={value} key={value}>{label}</option>)}</select></div>
        <div className="field"><label htmlFor="vault-password">{form.provider === "imap" ? "邮箱密码 / 应用专用密码" : "账号密码（可选）"}{active?.has_password ? " · 已保存，留空保留" : ""}</label><input id="vault-password" type="password" autoComplete="new-password" value={form.password} onChange={(event) => setForm((current) => ({ ...current, password: event.target.value, clear_password: false }))} /></div>
        {active?.has_password && <label className="check"><input type="checkbox" checked={form.clear_password} onChange={(event) => setForm((current) => ({ ...current, clear_password: event.target.checked, password: event.target.checked ? "" : current.password }))} />清除已保存的密码</label>}
        {oauth && <>
          <p className="hint">使用你自己注册的桌面 OAuth 应用，修改后先保存再连接。Gmail 包含 Google Workspace；Outlook 包含 Hotmail 和 Microsoft 365。邮箱授权会申请只读邮件权限，企业账号可能需要管理员批准。</p>
          <div className="field"><label htmlFor="vault-client">OAuth Client ID / 应用程序 ID</label><input id="vault-client" value={form.client_id} autoComplete="off" onChange={(event) => set("client_id", event.target.value)} /></div>
          <div className="field"><label htmlFor="vault-client-secret">Client Secret（可选）{active?.has_client_secret ? " · 已保存，留空保留" : ""}</label><input id="vault-client-secret" type="password" autoComplete="new-password" value={form.client_secret} onChange={(event) => set("client_secret", event.target.value)} /></div>
          {form.provider === "outlook" && <div className="field"><label htmlFor="vault-tenant">租户（默认 common，可填租户 ID 或域名）</label><input id="vault-tenant" value={form.tenant} onChange={(event) => set("tenant", event.target.value)} /></div>}
          <details className="src"><summary>首次配置 OAuth 应用</summary><p className="hint">Google Cloud：启用 Gmail API，创建“桌面应用”OAuth 客户端，填写客户端 ID 和提供的 Client Secret；应用处于测试状态时加入自己的测试邮箱。Microsoft Entra：选择所需个人 / 组织账号类型，在“移动和桌面应用”添加 <code>http://localhost/oauth/callback</code>，启用公用客户端，填写应用程序 ID；租户默认 common。首次授权后可自动读取邮件，不会自动完成 Claude 登录。</p></details>
        </>}
        {form.provider === "imap" && <><div className="row2"><div className="field"><label htmlFor="vault-host">IMAP 服务器</label><input id="vault-host" required value={form.imap_host} placeholder="imap.example.com" onChange={(event) => set("imap_host", event.target.value)} /></div><div className="field"><label htmlFor="vault-port">TLS 端口</label><input id="vault-port" type="number" min={1} max={65535} required value={form.imap_port} onChange={(event) => set("imap_port", event.target.value)} /></div></div><div className="field"><label htmlFor="vault-username">IMAP 用户名</label><input id="vault-username" value={form.imap_username} placeholder="通常为完整邮箱地址" onChange={(event) => set("imap_username", event.target.value)} /></div><p className="hint">使用服务器提供的 TLS 端口（通常 993）和应用专用密码。只读收件箱，不标记已读，不内置其它邮箱服务商适配。</p></>}
        <div className="field"><label htmlFor="vault-notes">备注</label><textarea id="vault-notes" rows={2} value={form.notes} onChange={(event) => set("notes", event.target.value)} /></div>
        <div className="vault-actions"><button className="btn primary" type="submit" disabled={busy}>保存资料</button>
          {active?.has_password && <button className="btn" type="button" disabled={busy} onClick={() => void run(async () => { const epoch = mailEpoch.current; const value = await api<{ password: string }>("POST", `/api/vault/entries/${encodeURIComponent(active.id)}/reveal`); if (epoch === mailEpoch.current && active.id === selectedRef.current && unlockedRef.current) setRevealed(value.password); })}>显示已保存密码</button>}
          {active && <button className="btn quiet danger" type="button" disabled={busy} onClick={() => {
            if (window.confirm(`移除“${active.label || active.email}”的账号库资料和邮箱授权？Claude 账号记录会保留。`)) void run(async () => { apply(await api<VaultState>("DELETE", `/api/vault/entries/${encodeURIComponent(active.id)}`)); choose(); toast("账号库条目已移除"); });
          }}>移除资料</button>}
        </div>
        {revealed !== null && <div className="vault-revealed"><code>{revealed || "未保存密码"}</code><button className="btn" type="button" disabled={busy || !revealed} onClick={() => void copy(revealed)}>复制密码</button><button className="btn" type="button" onClick={() => setRevealed(null)}>隐藏</button><small>30 秒后自动隐藏；剪贴板由系统管理。</small></div>}
      </form>
      {active && active.provider !== "credential" && <div className="panel vault-mail">
        <div className="sec-head"><h3>Claude 验证邮件</h3><span className="chip">{active.connected ? "已保存授权" : "尚未连接"}</span></div>
        <p className="hint">仅提取最近的 Claude / Anthropic 登录邮件，最多回溯一小时。验证码和链接不会自动提交；点击验证链接会按绑定的 Claude 账号打开。</p>
        <div className="vault-actions">{(active.provider === "gmail" || active.provider === "outlook") && <button className="btn" type="button" disabled={busy || !!job} onClick={() => void connect()}>{active.connected ? "重新授权邮箱" : "连接邮箱并授权"}</button>}<button className="btn" type="button" disabled={busy || !active.connected || !!waiting} onClick={() => void checkMail()}>读取最近登录邮件</button><button className="btn primary" type="button" disabled={busy || !active.connected || !!waiting} onClick={() => { setMessages(null); setWaiting({ entry_id: active.id, since: new Date(Date.now() - 30000).toISOString(), until: Date.now() + 120000 }); }}>等待新验证码</button>{waiting && <button className="btn" type="button" onClick={() => { mailEpoch.current += 1; setWaiting(null); }}>取消等待</button>}<label className="check"><input type="checkbox" checked={autoRead} onChange={(event) => setAutoRead(event.target.checked)} />本页打开时每 30 秒自动读取</label></div>
        {waiting && <p className="hint" role="status">正在等待新验证码，每 5 秒检查一次，最多等待 2 分钟。你可继续在 Claude 中请求登录邮件。</p>}
        {job && <div className="notice"><span>{jobMessage}</span><button className="btn" type="button" onClick={() => void run(async () => openVaultUrl(job.authorization_url, true))}>在电脑上打开授权页</button><a href={vaultUrl(job.authorization_url, true) || undefined} target="_blank" rel="noopener noreferrer">自行打开（需在同一台电脑）</a></div>}
        {!job && jobMessage && <p className="hint" role="status">{jobMessage}</p>}
        {messages && <><p className="hint">最近检查：{ago(messages.checked_at)}{messages.status === "empty" ? " · 暂无匹配的验证邮件" : ""}</p><div className="vault-messages">{messages.messages.map((message) => <article className="vault-message" key={message.id}><b>{message.subject}</b><small>{message.sender} · {ago(message.received_at)}</small><div className="vault-actions">{message.codes.map((code) => <button className="btn vault-code" type="button" key={code} onClick={() => void copy(code)} title="点击复制验证码">{code}</button>)}{message.verification_links.filter((link) => vaultUrl(link)).map((link, index) => <button className="btn" type="button" key={link} onClick={() => void run(async () => openVaultUrl(link, false, active.id))}>打开验证链接 {index + 1}</button>)}</div></article>)}</div></>}
      </div>}
    </>}
  </section>;
}
