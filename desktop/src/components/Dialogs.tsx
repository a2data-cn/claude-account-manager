import { useEffect, useRef, useState, type FormEvent } from "react";
import type { Account, Kind } from "../types";

function useModal(open: boolean) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  return ref;
}

export interface EditValues {
  label: string;
  plan: string;
  price: string;
  expires_at: string;
  cycle: string;
  auto_renew: boolean;
  note: string;
  session_key?: string;
  api_key?: string;
  admin_key?: string;
}

export function EditDialog({
  account,
  onClose,
  onSave,
  onDelete,
}: {
  account: Account | null;
  onClose: () => void;
  onSave: (a: Account, v: EditValues) => void;
  onDelete: (a: Account) => void;
}) {
  const ref = useModal(!!account);
  const [v, setV] = useState<EditValues | null>(null);

  useEffect(() => {
    if (!account) return;
    const m = account.manual || {};
    setV({
      label: account.label || "",
      plan: m.plan || "",
      price: m.price || "",
      expires_at: m.expires_at || "",
      cycle: m.cycle || "monthly",
      auto_renew: !!m.auto_renew,
      note: m.note || "",
      session_key: "",
      api_key: "",
      admin_key: "",
    });
  }, [account]);

  if (!account || !v) return <dialog ref={ref} onClose={onClose} />;
  const set = (k: keyof EditValues) => (e: { target: { value: string } }) => setV({ ...v, [k]: e.target.value });
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const out: EditValues = { ...v, label: v.label.trim(), price: v.price.trim(), note: v.note.trim() };
    for (const k of ["session_key", "api_key", "admin_key"] as const) {
      if (!out[k]?.trim()) delete out[k];
    }
    onSave(account, out);
  };

  return (
    <dialog ref={ref} onClose={onClose}>
      <form onSubmit={submit}>
        <h3>编辑：{account.label || account.email}</h3>
        <div className="field">
          <label htmlFor="f-label">名称</label>
          <input id="f-label" value={v.label} maxLength={60} onChange={set("label")} />
        </div>
        <div className="row2">
          <div className="field">
            <label htmlFor="f-plan">套餐（自动获取失败时填）</label>
            <select id="f-plan" value={v.plan} onChange={set("plan")}>
              <option value="">自动</option>
              <option value="pro">Pro</option>
              <option value="max">Max</option>
              <option value="team">Team</option>
              <option value="enterprise">Enterprise</option>
              <option value="free">Free</option>
            </select>
          </div>
          <div className="field">
            <label htmlFor="f-price">价格</label>
            <input id="f-price" value={v.price} placeholder="例如 $100/月" onChange={set("price")} />
          </div>
        </div>
        <div className="row2">
          <div className="field">
            <label htmlFor="f-exp">到期 / 下次续费</label>
            <input id="f-exp" type="date" value={v.expires_at} onChange={set("expires_at")} />
          </div>
          <div className="field">
            <label htmlFor="f-cycle">计费周期</label>
            <select id="f-cycle" value={v.cycle} onChange={set("cycle")}>
              <option value="monthly">每月</option>
              <option value="yearly">每年</option>
            </select>
          </div>
        </div>
        <label className="check">
          <input type="checkbox" checked={v.auto_renew} onChange={(e) => setV({ ...v, auto_renew: e.target.checked })} />
          自动续期（过期后按周期顺延）
        </label>
        {account.kind === "claude_web" && (
          <div className="field">
            <label htmlFor="f-session">claude.ai sessionKey（可选，用于自动获取网页版用量）</label>
            <input id="f-session" type="password" autoComplete="off" placeholder="留空表示不修改" value={v.session_key} onChange={set("session_key")} />
          </div>
        )}
        {account.kind === "api" && (
          <>
            <div className="field">
              <label htmlFor="f-api">API Key（留空不修改）</label>
              <input id="f-api" type="password" autoComplete="off" value={v.api_key} onChange={set("api_key")} />
            </div>
            <div className="field">
              <label htmlFor="f-admin">Admin Key（可选，用于读取本月花费）</label>
              <input id="f-admin" type="password" autoComplete="off" value={v.admin_key} onChange={set("admin_key")} />
            </div>
          </>
        )}
        <div className="field">
          <label htmlFor="f-note">备注</label>
          <textarea id="f-note" rows={2} value={v.note} onChange={set("note")} />
        </div>
        <div className="dlg-acts">
          <button type="button" className="btn quiet danger" onClick={() => onDelete(account)}>
            移除账号
          </button>
          <button type="button" className="btn" onClick={onClose}>
            取消
          </button>
          <button type="submit" className="btn primary">
            保存
          </button>
        </div>
      </form>
    </dialog>
  );
}

export interface AddValues {
  kind: Kind;
  label?: string;
  email?: string;
  api_key?: string;
  admin_key?: string;
}

export function AddDialog({ open, onClose, onAdd }: { open: boolean; onClose: () => void; onAdd: (v: AddValues) => void }) {
  const ref = useModal(open);
  const [kind, setKind] = useState<Kind>("claude_code");
  const [label, setLabel] = useState("");
  const [email, setEmail] = useState("");
  const [key, setKey] = useState("");
  const [admin, setAdmin] = useState("");

  useEffect(() => {
    if (open) {
      setKind("claude_code");
      setLabel("");
      setEmail("");
      setKey("");
      setAdmin("");
    }
  }, [open]);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (kind === "claude_code") return onAdd({ kind, label: label.trim() || undefined });
    onAdd({
      kind,
      label: label.trim(),
      email: kind === "claude_web" ? email.trim() : undefined,
      api_key: kind === "api" ? key.trim() : undefined,
      admin_key: kind === "api" ? admin.trim() : undefined,
    });
  };

  return (
    <dialog ref={ref} onClose={onClose}>
      <form onSubmit={submit}>
        <h3>添加账号</h3>
        <div className="field">
          <label htmlFor="a-kind">类型</label>
          <select id="a-kind" value={kind} onChange={(e) => setKind(e.target.value as Kind)}>
            <option value="claude_code">Claude Code（命令行）</option>
            <option value="claude_web">claude.ai 网页版</option>
            <option value="api">API Key</option>
          </select>
        </div>
        {kind === "claude_code" ? (
          <div className="hint">
            <p>Claude Code 账号需要先在这台电脑上登录，这样切换时才拿得到完整凭据：</p>
            <ol>
              <li>
                在终端运行 <code>claude</code>，输入 <code>/login</code> 登录要添加的账号
              </li>
              <li>回到这里点“导入当前登录”</li>
            </ol>
            <p>导入前会先保存上一个账号的最新凭据，不会丢。</p>
            <div className="field">
              <label htmlFor="a-label0">名称（可选）</label>
              <input id="a-label0" value={label} maxLength={60} onChange={(e) => setLabel(e.target.value)} />
            </div>
          </div>
        ) : (
          <>
            <div className="field">
              <label htmlFor="a-label">名称</label>
              <input id="a-label" value={label} maxLength={60} onChange={(e) => setLabel(e.target.value)} />
            </div>
            {kind === "claude_web" && (
              <div className="field">
                <label htmlFor="a-email">邮箱（可选，只用于显示）</label>
                <input id="a-email" type="email" value={email} onChange={(e) => setEmail(e.target.value)} />
              </div>
            )}
            {kind === "api" && (
              <>
                <div className="field">
                  <label htmlFor="a-key">API Key</label>
                  <input id="a-key" type="password" autoComplete="off" value={key} onChange={(e) => setKey(e.target.value)} />
                </div>
                <div className="field">
                  <label htmlFor="a-admin">Admin Key（可选）</label>
                  <input id="a-admin" type="password" autoComplete="off" value={admin} onChange={(e) => setAdmin(e.target.value)} />
                </div>
              </>
            )}
          </>
        )}
        <div className="dlg-acts">
          <button type="button" className="btn" onClick={onClose}>
            取消
          </button>
          <button type="submit" className="btn primary">
            {kind === "claude_code" ? "导入当前登录" : "添加"}
          </button>
        </div>
      </form>
    </dialog>
  );
}

export interface SwitchChoice {
  account: Account;
  message: string;
}

/** 有 claude 会话在跑时让用户选：等它们结束再切，还是现在就切 */
export function SwitchChoiceDialog({
  choice,
  onClose,
  onPick,
}: {
  choice: SwitchChoice | null;
  onClose: () => void;
  onPick: (a: Account, mode: "wait" | "now") => void;
}) {
  const ref = useModal(!!choice);
  return (
    <dialog ref={ref} onClose={onClose}>
      {choice && (
        <form method="dialog">
          <h3>切换账号</h3>
          <p className="dlg-text">{choice.message}</p>
          <div className="dlg-acts wrap">
            <button type="button" className="btn" onClick={onClose}>
              取消
            </button>
            <button type="button" className="btn" onClick={() => onPick(choice.account, "now")}>
              现在就切换
            </button>
            <button type="button" className="btn primary" onClick={() => onPick(choice.account, "wait")}>
              等会话结束后自动切换
            </button>
          </div>
        </form>
      )}
    </dialog>
  );
}

/** 移除账号：只从这个工具里移除，Claude Code 的项目、会话、设置都保留；正在用的 Claude Code 账号要先切走 */
export function RemoveDialog({
  account,
  onClose,
  onConfirm,
}: {
  account: Account | null;
  onClose: () => void;
  onConfirm: (a: Account) => void;
}) {
  const ref = useModal(!!account);
  const blocked = !!account && account.kind === "claude_code" && account.is_active;
  const what =
    account?.kind === "api" ? " API Key" : account?.kind === "claude_web" ? " sessionKey 和独立浏览器窗口的登录状态" : "登录凭据";
  return (
    <dialog ref={ref} onClose={onClose}>
      {account && (
        <form method="dialog" onSubmit={(e) => e.preventDefault()}>
          <h3>移除“{account.label || account.email || account.id}”？</h3>
          <div className="dlg-text">
            只从这个工具里移除，不会动 Claude 本身：
            <ul className="rm-list">
              <li className="keep">保留：Claude Code 的项目、会话记录、设置和 MCP 配置（~/.claude 里的都不动）</li>
              <li className="keep">保留：这个账号在 Anthropic 那边的订阅、对话和数据</li>
              <li className="drop">移除：这里保存的{what}、它的切号规则（都移到备份目录，可以找回）</li>
            </ul>
            {blocked && <div className="rm-block">这是 Claude Code 正在用的账号，先切换到其它账号再移除。</div>}
          </div>
          <div className="dlg-acts">
            <button type="button" className="btn" onClick={onClose}>
              取消
            </button>
            <button type="button" className="btn danger-fill" disabled={blocked} onClick={() => onConfirm(account)}>
              移除
            </button>
          </div>
        </form>
      )}
    </dialog>
  );
}
