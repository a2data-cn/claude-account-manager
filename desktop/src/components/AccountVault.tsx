import { useEffect, useRef, useState, type FormEvent } from "react";
import { api } from "../api";
import { copyText } from "../clipboard";
import { ago } from "../format";
import type { Account, State } from "../types";
import { ConfirmDialog, type ConfirmAsk } from "./Dialogs";
import {
  openVaultUrl, safeVaultError, vaultFrom, vaultMailDoc, vaultMailLinks, vaultRemoteImages, vaultTime, vaultUrl, VAULT_PROVIDERS,
  type VaultDetected, type VaultEntry, type VaultJob, type VaultMailDetail, type VaultMessages, type VaultProvider, type VaultState,
} from "../vault";

// 和网页面板的“账号资料与邮箱”一样：左边邮箱和表单（填“邮箱”或“邮箱----密码”，按后缀自动识别收信方式），右边邮件列表，点开看原邮件
const emptyForm = () => ({ id: "", label: "", email: "", provider: "imap" as VaultProvider, account_id: "", password: "", clear_password: false, client_id: "", client_secret: "", tenant: "common", imap_host: "", imap_port: "993", imap_username: "", notes: "" });
const LOOKBACK = 7 * 86400000;
const canRead = (entry?: VaultEntry) => !!entry && entry.connected && entry.provider !== "credential";
type Mode = "claude" | "all";
/** 账号卡上点了“重新登录”：这个号绑了能收信的邮箱时，打开这里开始等登录邮件 */
export interface VaultWaitFor { account_id: string; email: string; at: number }

const MS_GUIDE = <details className="src vguide"><summary>怎么拿到微软应用 ID（第一次要做，大约 5 分钟）</summary><ol>
  <li>打开 <code>entra.microsoft.com</code>，用任意一个微软账号登录（个人 Outlook 账号也行；第一次进去如果提示要先建目录或 Azure 免费账号，按提示建好）。</li>
  <li>左边菜单点“应用注册”（App registrations）→“新注册”（New registration）。</li>
  <li>名称随便填，比如“cam 收信”；“受支持的帐户类型”选“任何组织目录中的帐户和个人 Microsoft 帐户”。</li>
  <li>“重定向 URI”平台选“公共客户端/本机（移动和桌面）”，地址填 <code>http://localhost/oauth/callback</code>，点“注册”。</li>
  <li>进这个应用的“身份验证”（Authentication）页，最下面“允许公共客户端流”选“是”，保存。</li>
  <li>回到“概述”（Overview），复制“应用程序(客户端) ID”，粘贴到上面的框里，租户保持 <code>common</code>。</li></ol>
  <p className="hint">只要填一次：以后添加别的 Outlook 邮箱，这里留空就用同一个 ID。授权只申请“读邮件”，cam 不会发信、删信。</p></details>;
const GOOGLE_GUIDE = <details className="src vguide"><summary>怎么拿到 Google 应用 ID（嫌麻烦就改用“邮箱密码收信”，填应用专用密码）</summary><ol>
  <li>打开 <code>console.cloud.google.com</code>，新建一个项目，名字随便。</li>
  <li>“API 和服务 → 库”里搜 Gmail API，点“启用”。</li>
  <li>“API 和服务 → OAuth 同意屏幕”：用户类型选“外部”，填应用名称和你的邮箱；在“测试用户”里加上要收信的 Gmail 地址。</li>
  <li>“API 和服务 → 凭据 → 创建凭据 → OAuth 客户端 ID”，应用类型选“桌面应用”，创建。</li>
  <li>把“客户端 ID”和“客户端密钥”分别填到上面两个框，保存后点“连接邮箱并授权”。</li></ol>
  <p className="hint">应用在“测试”状态时，Google 大约每 7 天要重新授权一次；用应用专用密码收信没有这个限制。只要填一次，别的 Gmail 留空就用同一个。</p></details>;
const IMAP_GUIDE = <details className="src vguide"><summary>常见邮箱的密码怎么填</summary><ul>
  <li><b>Gmail</b>：不收登录密码。Google 账号打开两步验证，在 <code>myaccount.google.com/apppasswords</code> 生成 16 位应用专用密码填这里。</li>
  <li><b>Outlook / Hotmail</b>：先直接填登录密码试试；微软不让这个号用密码收信时，收信方式改成“Outlook 授权”。</li>
  <li><b>QQ 邮箱</b>：网页版 设置 → 账号，开启 IMAP / SMTP 服务，按提示生成授权码，填授权码。</li>
  <li><b>163 / 126</b>：网页版 设置 → POP3 / SMTP / IMAP，开启 IMAP 后生成客户端授权码，填授权码。</li>
  <li><b>GMX / mail.com / Onet</b>：网页版设置里先打开 POP3 / IMAP，再填登录密码。</li>
  <li><b>iCloud</b>：在 <code>account.apple.com</code> 生成 App 专用密码填这里。</li></ul></details>;

export function AccountVault({ accounts, onAccountsChanged, toast, waitFor }: { accounts: Account[]; onAccountsChanged: (state: State) => void; toast: (message: string, bad?: boolean) => void; waitFor?: VaultWaitFor | null }) {
  const [state, setState] = useState<VaultState | null>(null);
  const [error, setError] = useState("");
  const [master, setMaster] = useState("");
  const [confirm, setConfirm] = useState("");
  const [form, setForm] = useState(emptyForm);
  const [selected, setSelected] = useState("");
  const [busy, setBusy] = useState(false);
  const [ask, setAsk] = useState<ConfirmAsk | null>(null);
  const working = useRef(false);
  const [revealed, setRevealed] = useState<string | null>(null);
  const [messages, setMessages] = useState<VaultMessages | null>(null);
  const [reading, setReading] = useState(false);
  const [mailError, setMailError] = useState("");
  const [mode, setMode] = useState<Mode>("claude");
  const [filter, setFilter] = useState("");
  const [detail, setDetail] = useState<VaultMailDetail | null>(null);
  const [images, setImages] = useState(false);
  const [fresh, setFresh] = useState<Set<string>>(new Set());
  const [job, setJob] = useState<VaultJob | null>(null);
  const [jobMessage, setJobMessage] = useState("");
  const [autoRead, setAutoRead] = useState(true);
  const [waiting, setWaiting] = useState<{ entry_id: string; since: string; until: number } | null>(null);
  const [detected, setDetected] = useState<VaultDetected | null>(null);
  const [dropped, setDropped] = useState(false);
  const providerTouched = useRef(false);
  const detectFor = useRef("");
  const mailEpoch = useRef(0);
  const selectedRef = useRef("");
  const unlockedRef = useRef(false);
  const modeRef = useRef<Mode>("claude");
  const lastCheck = useRef(0);
  const active = state?.entries.find((entry) => entry.id === selected);
  // 5 秒一次的检查读这些（不用等重新渲染）
  const live = useRef({ state, messages, detail, waiting, mailError, autoRead });
  live.current = { state, messages, detail, waiting, mailError, autoRead };
  const oauth = form.provider === "gmail" || form.provider === "outlook";
  const set = (key: keyof ReturnType<typeof emptyForm>, value: string | boolean) => setForm((current) => ({ ...current, [key]: value }));

  const clearMail = () => {
    mailEpoch.current += 1; lastCheck.current = 0;
    setWaiting(null); setMessages(null); setDetail(null); setImages(false); setMailError(""); setFresh(new Set()); setReading(false);
  };
  const clearContext = () => { clearMail(); setRevealed(null); setJob(null); setJobMessage(""); };
  const clearPrivate = () => {
    clearContext(); selectedRef.current = "";
    setMaster(""); setConfirm(""); setSelected(""); setForm(emptyForm()); setDetected(null); setDropped(false);
  };
  const apply = (next: VaultState) => {
    const wasLocked = !unlockedRef.current;
    unlockedRef.current = next.unlocked;
    setState(next);
    if (!next.unlocked) clearPrivate();
    else if (wasLocked && !selectedRef.current) {  // 刚解锁：直接选第一个能收信的邮箱，右边马上开始获取
      const first = next.entries.find(canRead);
      if (first) choose(first);
    }
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
          if (result.status === "connected") { clearMail(); await reload(); toast("邮箱已连接，开始获取邮件"); }
        }
      } catch (cause) { if (!cancelled && epoch === mailEpoch.current) { setJob(null); setJobMessage(safeVaultError(cause)); try { const latest = await api<VaultState>("GET", "/api/vault"); if (!latest.unlocked) apply(latest); } catch { /* 保留授权错误 */ } } }
      finally { polling = false; }
    };
    const timer = window.setInterval(() => void poll(), 2000);
    void poll();
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [job?.job_id]);

  useEffect(() => () => { mailEpoch.current += 1; unlockedRef.current = false; }, []);
  /** 读邮件列表；since 有值时是“等新验证码”，新来的放到最上面标“新” */
  const checkMail = async (quiet = true, since?: string) => {
    const entry = live.current.state?.entries.find((item) => item.id === selectedRef.current);
    if (!canRead(entry) || working.current) return;
    const epoch = mailEpoch.current, all = !since && modeRef.current === "all";
    lastCheck.current = Date.now();
    setReading(true);
    let failed = "";
    await run(async () => {
      try {
        const body: Record<string, unknown> = { since: since || new Date(Date.now() - LOOKBACK + 60000).toISOString() };
        if (all) body.all = true;
        const result = await api<VaultMessages>("POST", `/api/vault/entries/${encodeURIComponent(entry!.id)}/messages`, body);
        if (epoch !== mailEpoch.current || entry!.id !== selectedRef.current || !unlockedRef.current) return;
        if (since) {
          if (result.status !== "found") return;
          const ids = new Set(result.messages.map((m) => m.id));
          setFresh(ids);
          setMessages((old) => ({ ...result, all: false, messages: result.messages.concat((old && !old.all ? old.messages : []).filter((m) => !ids.has(m.id))).slice(0, 40) }));
          modeRef.current = "claude"; setMode("claude"); setWaiting(null); toast("收到新的 Claude 邮件");
          return;
        }
        if (modeRef.current !== (result.all ? "all" : "claude")) return;
        setMessages(result); setMailError("");
      } catch (cause) { failed = safeVaultError(cause); throw cause; }
    }, quiet);
    if (epoch !== mailEpoch.current) return;
    setReading(false);
    if (failed) setMailError(failed);
  };
  // 每 5 秒看一眼：列表还没拿到就先拿（出错了隔 5 分钟再试，免得密码不对时一直登录被邮箱锁号）；在等新验证码就查新邮件；否则每 30 秒自动获取
  const tick = () => {
    const now = live.current;
    if (document.hidden || !unlockedRef.current || working.current) return;  // 窗口在后台时不读邮箱
    const entry = now.state?.entries.find((item) => item.id === selectedRef.current);
    if (!canRead(entry)) return;
    const gap = now.mailError ? 300000 : 30000;
    if (!now.messages && !now.detail) { if (Date.now() - lastCheck.current >= (now.mailError ? gap : 0)) void checkMail(true); return; }
    const wait = now.waiting;
    if (wait && wait.entry_id === entry!.id) {
      if (Date.now() >= wait.until) { setWaiting(null); toast("等了 2 分钟，还没收到新的 Claude 邮件"); }
      else void checkMail(true, wait.since);
    } else if (now.autoRead && !now.detail && modeRef.current === "claude" && Date.now() - lastCheck.current >= gap) void checkMail(true);
  };
  const tickRef = useRef(tick);
  tickRef.current = tick;
  useEffect(() => {
    const timer = window.setInterval(() => tickRef.current(), 5000);
    return () => window.clearInterval(timer);
  }, []);
  useEffect(() => { tickRef.current(); }, [state?.unlocked, active?.id, active?.connected, mode, waiting]);  // 换了邮箱、模式或开始等：马上动一下
  // 从账号卡的“重新登录”过来：选中绑定这个号的邮箱，开始等登录邮件
  useEffect(() => {
    if (!waitFor || !state?.unlocked) return;
    const email = waitFor.email.toLowerCase();
    const entry = state.entries.find((e) => e.account_id === waitFor.account_id && canRead(e)) || state.entries.find((e) => email && e.email.toLowerCase() === email && canRead(e));
    if (!entry) return;
    if (entry.id !== selectedRef.current) choose(entry);
    modeRef.current = "claude"; setMode("claude"); setDetail(null);
    setWaiting({ entry_id: entry.id, since: new Date(Date.now() - 30000).toISOString(), until: Date.now() + 120000 });
  }, [waitFor?.at, state?.unlocked]);

  // 填邮箱时按后缀认出收信方式；粘贴“邮箱----密码（----Client ID----Refresh Token）”时拆开填好
  const onEmail = (raw: string) => {
    let email = raw, password: string | null = null, extra = false;
    if (raw.includes("----")) {
      const parts = raw.split("----").map((part) => part.trim());
      email = parts[0]; password = parts[1] || null; extra = parts.length > 2;
    }
    setDropped(extra);
    setForm((current) => ({ ...current, email, ...(password ? { password, clear_password: false } : {}) }));
  };
  useEffect(() => {
    const email = form.email.trim();
    if (!/^[^@\s<>]+@[^@\s<>]+\.[^@\s<>]+$/.test(email)) { setDetected(null); detectFor.current = ""; return; }
    if (email === detectFor.current) return;
    providerTouched.current = false;
    const timer = window.setTimeout(async () => {
      detectFor.current = email;
      try {
        const d = await api<VaultDetected>("POST", "/api/vault/detect", { email });
        setDetected(d);
        if (!selectedRef.current && !providerTouched.current)  // 只在“添加邮箱”时自动改；编辑已有的、或用户自己选过的不动
          setForm((current) => current.email.trim() === email ? { ...current, provider: d.provider, imap_host: d.imap_host || "", imap_port: String(d.imap_port || 993) } : current);
      } catch { detectFor.current = ""; }
    }, 250);
    return () => window.clearTimeout(timer);
  }, [form.email]);

  function choose(entry?: VaultEntry) {
    clearContext(); selectedRef.current = entry?.id || "";
    setSelected(entry?.id || "");
    setForm(entry ? { ...emptyForm(), id: entry.id, label: entry.label !== entry.email ? entry.label : "", email: entry.email, provider: entry.provider, account_id: entry.account_id || "", ...entry.config, imap_port: String(entry.config.imap_port || 993) } as ReturnType<typeof emptyForm> : emptyForm());
    detectFor.current = entry?.email || ""; setDetected(null); setDropped(false); providerTouched.current = false;
    setError("");
  }
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
      choose(entry);
      toast(canRead(entry) ? "已保存，开始获取邮件" : entry && (entry.provider === "gmail" || entry.provider === "outlook") ? "已保存，点“连接邮箱并授权”登录一次就能收信" : "已保存");
    });
  };
  const identify = () => run(async () => {
    if (!form.account_id) throw new Error("先选择要绑定的 Claude 账号");
    const boundId = form.account_id;
    const next = await api<State & { identity_message?: string }>("POST", `/api/accounts/${encodeURIComponent(boundId)}/identify`);
    onAccountsChanged(next);
    const account = next.accounts.find((item) => item.id === form.account_id);
    if (!account?.email) throw new Error(next.identity_message || "未识别到这个账号的已登录邮箱，请先在对应 Claude 配置中登录");
    setForm((current) => current.account_id === boundId ? { ...current, email: account.email || "", label: current.label || account.label || "" } : current);
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
  const copy = async (value: string, what = "已复制") => { const ok = await copyText(value); toast(ok ? what : "复制不了，请手动选中复制", !ok); };
  const openMail = (id: string) => run(async () => {
    if (!active) return;
    const epoch = mailEpoch.current;
    const found = await api<VaultMailDetail>("POST", `/api/vault/entries/${encodeURIComponent(active.id)}/message`, { id });
    if (epoch !== mailEpoch.current || active.id !== selectedRef.current) return;
    setDetail(found); setImages(false);
  });
  const switchMode = (next: Mode) => { if (next === modeRef.current) return; clearMail(); modeRef.current = next; setMode(next); };
  const startWait = () => { if (!active) return; setDetail(null); setWaiting({ entry_id: active.id, since: new Date(Date.now() - 30000).toISOString(), until: Date.now() + 120000 }); };

  const actions = (m: { codes: string[]; verification_links: string[] }) => <>
    {m.codes.map((code) => <button className="btn vault-code" type="button" key={code} onClick={() => void copy(code, "验证码已复制")} title="点一下复制验证码">{code}</button>)}
    {m.verification_links.filter((link) => vaultUrl(link)).map((link, index, all) => <button className="btn primary" type="button" key={link} disabled={busy} onClick={() => void run(async () => { await openVaultUrl(link, false, active!.id); toast("已打开登录链接"); })}>打开登录链接{all.length > 1 ? ` ${index + 1}` : ""}</button>)}
  </>;
  const status = !active ? null : active.provider === "credential" ? <span className="chip">只存资料</span>
    : !active.connected ? <span className="chip st-warn">{active.provider === "imap" ? "先填密码" : "先授权"}</span>
    : reading ? <span className="chip live">获取中…</span>
    : mailError ? <span className="chip st-error" title={mailError}>没获取到</span>
    : messages ? <span className="chip c-ok">{messages.messages.length} 封邮件</span> : <span className="chip">等待中</span>;
  const empty = (title: string, text: string, bad = false) => <div className={`vm-empty${bad ? " bad" : ""}`}><b>{title}</b><span>{text}</span></div>;
  const mailBody = () => {
    if (!active) return empty("还没选邮箱", "左边选一个邮箱，或者填邮箱和密码添加一个，这里就会列出邮件。");
    if (active.provider === "credential") return empty("这个邮箱只保存了账号资料", "要收信，把左边的“收信方式”改成“邮箱密码收信”或授权，保存后就会获取。");
    if (!active.connected) return empty(active.provider === "imap" ? "还没填密码" : "还没授权", active.provider === "imap" ? "在左边填好密码（或授权码）保存，就开始获取。" : "点上面的“连接邮箱并授权”，在弹出的页面登录一次。");
    if (detail) {
      const remote = vaultRemoteImages(detail.html), show = !!detail.official || images, links = vaultMailLinks(detail.html);
      return <div className="vm-detail">
        <div className="vm-dhead"><button className="btn quiet" type="button" onClick={() => { setDetail(null); setImages(false); }}>‹ 返回列表</button><span className="spacer" />{remote && !show && <button className="btn" type="button" onClick={() => setImages(true)}>显示图片</button>}</div>
        <h4>{detail.subject || "（没有标题）"}</h4>
        <div className="vm-meta"><span className="chip">{vaultFrom(detail)}</span>{detail.received_at && <span className="chip">{vaultTime(detail.received_at)}</span>}{detail.official && <span className="chip c-ok">Claude 官方</span>}</div>
        {(detail.codes.length > 0 || detail.verification_links.length > 0) && <div className="vault-actions">{actions(detail)}</div>}
        {detail.html ? <iframe className="vm-frame" title="邮件正文" sandbox="allow-popups allow-popups-to-escape-sandbox" referrerPolicy="no-referrer" srcDoc={vaultMailDoc(detail.html, show)} /> : <pre className="vm-text">{detail.text || "（这封信没有正文）"}</pre>}
        {remote && !show && <p className="hint">为了不被追踪，别人发的邮件先不加载远程图片。</p>}
        {detail.truncated && <p className="hint">邮件太大，只显示了前面一部分。</p>}
        {links.length > 0 && <details className="src"><summary>邮件里的链接（{links.length} 个，在这里点不开时复制到浏览器）</summary><div className="vault-messages">{links.map((link) => <div className="vault-actions" key={link}><code className="vm-link">{link}</code><button className="btn" type="button" onClick={() => void copy(link, "链接已复制")}>复制</button></div>)}</div></details>}
      </div>;
    }
    if (mailError && !messages) return empty("没获取到邮件", mailError, true);
    if (!messages) return empty(reading ? "正在获取…" : "等待中", reading ? "第一次连接邮箱要几秒钟。" : "点“刷新”获取最近 7 天的邮件。");
    const words = filter.trim().toLowerCase();
    const rows = messages.messages.filter((m) => !words || [m.subject, m.sender, m.sender_name, m.preview].join(" ").toLowerCase().includes(words));
    return <>
      <p className="hint vm-scope">{messages.all ? "收件箱最近 7 天" : "最近 7 天 Claude / Anthropic 发来的邮件"} · {ago(messages.checked_at)}获取{mailError && <> · <span className="bad">刚才那次没成功：{mailError}</span></>}</p>
      {rows.length ? <div className="vault-messages">{rows.map((m) => <article className={`vm-row${m.official ? " official" : ""}${fresh.has(m.id) ? " new" : ""}`} key={m.id}>
        <div className="vm-main">
          <button className="vm-subj" type="button" disabled={busy} onClick={() => void openMail(m.id)}><svg className="vm-ico" viewBox="0 0 24 24" aria-hidden="true"><rect x="3.5" y="5.5" width="17" height="13" rx="2.2" /><path d="M4.5 7l7.5 6 7.5-6" /></svg><span>{m.subject || "（没有标题）"}</span></button>
          <div className="vm-meta"><span className="chip" title={m.sender}>{vaultFrom(m)}</span><span className="chip">{vaultTime(m.received_at)}</span>{fresh.has(m.id) && <span className="chip live">新</span>}</div>
          {m.preview && <p className="vm-prev">{m.preview}</p>}
          {(m.codes.length > 0 || m.verification_links.length > 0) && <div className="vault-actions">{actions(m)}</div>}
        </div>
        <button className="vm-more" type="button" disabled={busy} onClick={() => void openMail(m.id)}>查看详情 ›</button>
      </article>)}</div>
        : empty(words ? "没有符合筛选的邮件" : "没有邮件", words ? "换个词试试。" : messages.all ? "收件箱最近 7 天是空的。" : "最近 7 天没有 Claude 发来的邮件；要看别的邮件，切到“全部邮件”。")}
    </>;
  };
  const kindChip = (entry: VaultEntry) => entry.provider === "credential" ? <span className="chip">只存资料</span>
    : entry.connected ? <span className="chip c-ok">能收信</span> : <span className="chip st-warn">{entry.provider === "imap" ? "没填密码" : "还没授权"}</span>;
  const imapHost = form.imap_host.trim();

  return <section className="block vault" aria-label="账号资料与邮箱">
    <div className="sec-head"><h2>账号资料与邮箱</h2><span className="spacer" />
      <button className="btn" disabled={busy} onClick={() => void run(reload)}>重新读取</button>
    </div>
    <p className="hint">邮箱和密码用主密码加密，存在这台电脑的账号库里，不经过别人的服务器。cam 只读邮件，不发信、不删信、不标成已读；Claude 的登录链接和验证码还是你自己点。</p>
    {error && <div className="notice" role="alert">{error}</div>}
    {!state ? <div className="panel"><p className="hint">正在读取账号库…</p></div> : !state.unlocked ? <form className="panel vault-unlock" onSubmit={authenticate}>
      <h3>{state.initialized ? "解锁本地账号库" : "设置账号库主密码"}</h3>
      <p className="hint">主密码用于本地加密，至少 8 个字符。请妥善保存；忘记后无法解密已存的密码与授权。每个后端进程需单独解锁。</p>
      <div className="field"><label htmlFor="vault-master">主密码</label><input id="vault-master" type="password" minLength={8} required autoComplete={state.initialized ? "current-password" : "new-password"} value={master} onChange={(event) => setMaster(event.target.value)} /></div>
      {!state.initialized && <div className="field"><label htmlFor="vault-confirm">再次输入主密码</label><input id="vault-confirm" type="password" minLength={8} required autoComplete="new-password" value={confirm} onChange={(event) => setConfirm(event.target.value)} /></div>}
      <button className="btn primary" disabled={busy} type="submit">{busy ? "处理中…" : state.initialized ? "解锁" : "创建加密账号库"}</button>
    </form> : <div className="vault-grid"><div className="vault-side">
      <div className="panel vault-boxes"><div className="vm-head"><h3>邮箱账户</h3><span className="spacer" /><button className="btn quiet" type="button" disabled={busy} onClick={() => void run(async () => { apply(await api<VaultState>("POST", "/api/vault/lock")); toast("账号库已锁定"); })}>锁定</button></div>
        <div className="vault-list">{state.entries.length ? state.entries.map((entry) => <button type="button" key={entry.id} className={`vault-entry${selected === entry.id ? " sel" : ""}`} aria-pressed={selected === entry.id} disabled={busy} onClick={() => choose(entry)}><b>{entry.label || entry.email}</b><span>{entry.email}</span>{kindChip(entry)}</button>)
          : <p className="hint">还没有邮箱。在下面填邮箱和密码，保存后右边就开始收信。</p>}</div>
        {active && <button className="btn" type="button" disabled={busy} onClick={() => choose()}>＋ 添加邮箱</button>}
      </div>
      <form className="panel vault-form" onSubmit={save} autoComplete="off">
        <h3>{active ? `编辑 ${active.label || active.email}` : "添加邮箱"}</h3>
        <div className="field"><label htmlFor="vault-email">邮箱账户（自动识别类型）</label><input id="vault-email" required placeholder="邮箱地址，或 邮箱----密码" spellCheck={false} autoCapitalize="off" value={form.email} onChange={(event) => onEmail(event.target.value)} /></div>
        {(detected || dropped) && <p className="hint vdetect">{detected && <>{detected.name && <>认出是 <b>{detected.name}</b>：</>}{detected.note}</>}{dropped && <><br />后面的 Client ID / Refresh Token 用不上，已经去掉；Outlook 要授权时用下面的“Outlook 授权”。</>}</p>}
        <div className="field"><label htmlFor="vault-password">{form.provider === "imap" ? "密码 / 授权码" : oauth ? "账号密码（可选，只保存，收信用授权）" : "账号密码（可选）"}{active?.has_password ? " · 已保存，留空保留" : ""}</label><input id="vault-password" type="password" autoComplete="new-password" placeholder={form.provider === "imap" ? "邮箱密码；QQ、163 填授权码，Gmail、iCloud 填应用专用密码" : ""} value={form.password} onChange={(event) => setForm((current) => ({ ...current, password: event.target.value, clear_password: false }))} /></div>
        {active?.has_password && <label className="check"><input type="checkbox" checked={form.clear_password} onChange={(event) => setForm((current) => ({ ...current, clear_password: event.target.checked, password: event.target.checked ? "" : current.password }))} />清除已保存的密码</label>}
        <div className="field"><label htmlFor="vault-provider">收信方式</label><select id="vault-provider" value={form.provider} onChange={(event) => { providerTouched.current = true; set("provider", event.target.value); }}>{Object.entries(VAULT_PROVIDERS).map(([value, label]) => <option value={value} key={value}>{label}</option>)}</select></div>
        {oauth && <>
          <div className="field"><label htmlFor="vault-client">应用 ID（Client ID）</label><input id="vault-client" value={form.client_id} autoComplete="off" spellCheck={false} placeholder="留空就用同一种邮箱里填过的那个" onChange={(event) => set("client_id", event.target.value)} /></div>
          {form.provider === "gmail" && <div className="field"><label htmlFor="vault-client-secret">客户端密钥（Client Secret，Google 需要）{active?.has_client_secret ? " · 已保存，留空保留" : ""}</label><input id="vault-client-secret" type="password" autoComplete="new-password" value={form.client_secret} onChange={(event) => set("client_secret", event.target.value)} /></div>}
          {form.provider === "outlook" && <div className="field"><label htmlFor="vault-tenant">租户（一般不用改）</label><input id="vault-tenant" value={form.tenant} onChange={(event) => set("tenant", event.target.value)} /></div>}
          {form.provider === "outlook" ? MS_GUIDE : GOOGLE_GUIDE}
        </>}
        {form.provider === "imap" && <><details className="src" open={!imapHost || undefined} key={imapHost ? "filled" : "empty"}><summary>{imapHost ? `收信服务器：${imapHost}:${form.imap_port || 993}` : "收信服务器（没认出这个邮箱，要自己填）"}</summary>
          <div className="row2"><div className="field"><label htmlFor="vault-host">IMAP 服务器</label><input id="vault-host" required value={form.imap_host} placeholder="imap.example.com" spellCheck={false} onChange={(event) => set("imap_host", event.target.value)} /></div><div className="field"><label htmlFor="vault-port">TLS 端口</label><input id="vault-port" type="number" min={1} max={65535} required value={form.imap_port} onChange={(event) => set("imap_port", event.target.value)} /></div></div>
          <div className="field"><label htmlFor="vault-username">登录用户名</label><input id="vault-username" value={form.imap_username} placeholder="留空就用邮箱地址" onChange={(event) => set("imap_username", event.target.value)} /></div><p className="hint">连接都走 TLS，正常校验证书；只读收件箱，不标记已读。</p></details>{IMAP_GUIDE}</>}
        <details className="src" open={!!(active && (active.account_id || active.label !== active.email || active.config.notes)) || undefined}><summary>名称、绑定 Claude 账号、备注</summary>
          <div className="field"><label htmlFor="vault-label">名称（可选）</label><input id="vault-label" maxLength={60} placeholder="不填就用邮箱" value={form.label} onChange={(event) => set("label", event.target.value)} /></div>
          <div className="field"><label htmlFor="vault-account">绑定 Claude 账号（可选，点登录链接时用这个号的浏览器打开）</label><div className="vault-inline"><select id="vault-account" value={form.account_id} onChange={(event) => {
            const account = accounts.find((item) => item.id === event.target.value);
            setForm((current) => ({ ...current, account_id: event.target.value, email: current.email.trim() ? current.email : account?.email || "", label: current.label || account?.label || "" }));
          }}><option value="">不绑定</option>{accounts.filter((account) => account.kind !== "api").map((account) => <option key={account.id} value={account.id}>{account.label || account.email || account.id}</option>)}</select><button className="btn" type="button" disabled={busy || !form.account_id} onClick={() => void identify()}>读取已登录邮箱</button></div></div>
          <div className="field"><label htmlFor="vault-notes">备注</label><textarea id="vault-notes" rows={2} value={form.notes} onChange={(event) => set("notes", event.target.value)} /></div>
        </details>
        <div className="vault-actions"><button className="btn primary" type="submit" disabled={busy}>{active ? "保存" : "保存并开始获取"}</button>
          {active?.has_password && <button className="btn" type="button" disabled={busy} onClick={() => void run(async () => { const epoch = mailEpoch.current; const value = await api<{ password: string }>("POST", `/api/vault/entries/${encodeURIComponent(active.id)}/reveal`); if (epoch === mailEpoch.current && active.id === selectedRef.current && unlockedRef.current) setRevealed(value.password); })}>显示已保存密码</button>}
          {active && <button className="btn quiet danger" type="button" disabled={busy} onClick={() => setAsk({
            title: "移除邮箱", ok: "移除", danger: true,
            body: `移除“${active.label || active.email}”保存的密码和邮箱授权？Claude 账号记录会保留。`,
            onOk: () => void run(async () => { apply(await api<VaultState>("DELETE", `/api/vault/entries/${encodeURIComponent(active.id)}`)); choose(); toast("已移除"); }),
          })}>移除</button>}
        </div>
        {revealed !== null && <div className="vault-revealed"><code>{revealed || "未保存密码"}</code><button className="btn" type="button" disabled={!revealed} onClick={() => void copy(revealed)}>复制密码</button><button className="btn" type="button" onClick={() => setRevealed(null)}>隐藏</button><small>30 秒后自动隐藏；剪贴板由系统管理。</small></div>}
      </form></div>
      <section className="panel vault-mail" aria-label="邮件列表">
        <div className="vm-head"><h3><svg className="vm-ico" viewBox="0 0 24 24" aria-hidden="true"><rect x="3.5" y="5.5" width="17" height="13" rx="2.2" /><path d="M4.5 7l7.5 6 7.5-6" /></svg>邮件列表</h3>
          <span className="seg-mini" role="group" aria-label="看哪些邮件"><button type="button" aria-pressed={mode === "claude"} onClick={() => switchMode("claude")}>Claude 邮件</button><button type="button" aria-pressed={mode === "all"} onClick={() => switchMode("all")}>全部邮件</button></span>
          <span className="spacer" />{status}<button className="btn" type="button" disabled={busy || !canRead(active)} onClick={() => { setDetail(null); void checkMail(false); }}>刷新</button></div>
        <div className="vault-actions vm-tools">
          {active && active.provider !== "credential" && <>
            {(active.provider === "gmail" || active.provider === "outlook") && <button className={`btn${active.connected ? "" : " primary"}`} type="button" disabled={busy || !!job} onClick={() => void connect()}>{active.connected ? "重新授权邮箱" : "连接邮箱并授权"}</button>}
            <button className={`btn${active.connected ? " primary" : ""}`} type="button" disabled={busy || !active.connected || !!waiting} onClick={startWait}>等新验证码</button>
            {waiting && <button className="btn" type="button" onClick={() => { mailEpoch.current += 1; setWaiting(null); setReading(false); }}>取消等待</button>}
            <label className="check"><input type="checkbox" checked={autoRead} onChange={(event) => setAutoRead(event.target.checked)} />每 30 秒自动获取</label>
          </>}
          <span className="spacer" /><input type="search" className="vm-filter" placeholder="筛选邮件" aria-label="筛选邮件" value={filter} onChange={(event) => setFilter(event.target.value)} />
        </div>
        {waiting && <p className="hint" role="status">正在等新的 Claude 邮件，每 5 秒查一次，最多等 2 分钟。现在去让 Claude 发登录邮件就行。</p>}
        {job && <div className="notice"><span>{jobMessage}</span><button className="btn" type="button" onClick={() => void run(async () => openVaultUrl(job.authorization_url, true))}>在电脑上打开授权页</button><a href={vaultUrl(job.authorization_url, true) || undefined} target="_blank" rel="noopener noreferrer">自行打开（需在同一台电脑）</a></div>}
        {!job && jobMessage && <p className="hint" role="status">{jobMessage}</p>}
        <div className="vm-body">{mailBody()}</div>
      </section>
    </div>}
    <ConfirmDialog ask={ask} onClose={() => setAsk(null)} />
  </section>;
}
