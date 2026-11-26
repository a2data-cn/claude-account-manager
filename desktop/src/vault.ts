import { api } from "./api";

export type VaultProvider = "credential" | "gmail" | "outlook" | "imap";
export interface VaultEntry {
  id: string;
  label: string;
  email: string;
  provider: VaultProvider;
  account_id: string | null;
  config: { client_id?: string; tenant?: string; imap_host?: string; imap_port?: number; imap_username?: string; notes?: string };
  has_password: boolean;
  has_client_secret: boolean;
  connected: boolean;
  updated_at: string;
}
export interface VaultState { initialized: boolean; unlocked: boolean; entries: VaultEntry[] }
export interface VaultMessage {
  id: string;
  sender: string;
  sender_name?: string;
  subject: string;
  received_at: string;
  preview?: string;
  official?: boolean;
  login?: boolean;
  codes: string[];
  verification_links: string[];
}
export interface VaultMessages { checked_at: string; status: "found" | "empty"; all?: boolean; messages: VaultMessage[] }
/** 点开一封信：原邮件 HTML（后端已去掉脚本）和纯文本 */
export interface VaultMailDetail extends Omit<VaultMessage, "received_at" | "preview"> { received_at: string | null; html: string; text: string; truncated: boolean }
/** 按邮箱后缀认出来的收信方式（web/mail-providers.json） */
export interface VaultDetected { provider: VaultProvider; imap_host: string; imap_port: number; name: string; note: string; oauth: "gmail" | "outlook" | null }
export interface VaultJob { job_id: string; authorization_url: string; expires_at: string | number }

export const VAULT_PROVIDERS: Record<VaultProvider, string> = {
  imap: "邮箱密码收信（IMAP）", outlook: "Outlook 授权（在微软页面登录一次）", gmail: "Gmail 授权（在 Google 页面登录一次）", credential: "只保存账号资料，不收信",
};

/** 详情页的 iframe 内容：不许脚本，图片按需加载（别人发的先不加载远程图片，防追踪） */
export function vaultMailDoc(html: string, images: boolean): string {
  const remote = images ? "https: data:" : "data:";
  const csp = `default-src 'none'; style-src 'unsafe-inline'; img-src ${remote}; font-src ${remote}`;
  return `<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${csp}"><meta name="referrer" content="no-referrer"><base target="_blank">`
    + `<style>html{color-scheme:light}body{margin:14px;font:14px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif;color:#222;background:#fff;overflow-wrap:anywhere}img{max-width:100%;height:auto}table{max-width:100%}</style></head><body>${html}</body></html>`;
}
export const vaultRemoteImages = (html: string) => /<img\b[^>]*\bsrc\s*=\s*["']?\s*https?:|url\(\s*["']?\s*https?:/i.test(html || "");
/** 邮件里的 https 链接（桌面 App 的窗口里点 iframe 里的链接不一定能打开，列出来可以复制）；DOMParser 不运行脚本 */
export function vaultMailLinks(html: string): string[] {
  try {
    const doc = new DOMParser().parseFromString(html || "", "text/html");
    const out: string[] = [];
    doc.querySelectorAll("a[href]").forEach((a) => {
      const href = a.getAttribute("href") || "";
      if (/^https:\/\//i.test(href) && !out.includes(href) && out.length < 30) out.push(href);
    });
    return out;
  } catch { return []; }
}
export const vaultTime = (iso: string | null | undefined) => {
  const d = new Date(iso || "");
  const p = (n: number) => String(n).padStart(2, "0");
  return isNaN(d.getTime()) ? "" : `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
};
export const vaultFrom = (m: { sender: string; sender_name?: string }) => (m.sender_name && m.sender_name !== m.sender ? `${m.sender_name} <${m.sender}>` : m.sender);

export function safeVaultError(error: unknown, secrets: string[] = []) {
  let message = error instanceof Error ? error.message : "操作失败，请重试";
  for (const secret of secrets.filter(Boolean)) message = message.split(secret).join("••••");
  return message.replace(/(access_token|refresh_token|client_secret|password|authorization_code)\s*[=:]\s*["']?[^\s,"'&}]+/gi, "$1=••••").slice(0, 400);
}

export function vaultUrl(value: string, authorization = false): string | null {
  try {
    const url = new URL(value);
    if (url.protocol !== "https:" || url.username || url.password) return null;
    const host = url.hostname.toLowerCase();
    const allowed = authorization
      ? host === "accounts.google.com" || host === "login.microsoftonline.com"
      : ["claude.ai", "claude.com", "anthropic.com"].some((domain) => host === domain || host.endsWith("." + domain));
    return allowed ? url.href : null;
  } catch { return null; }
}

export async function openVaultUrl(value: string, authorization = false, entryId?: string) {
  const url = vaultUrl(value, authorization);
  if (!url) throw new Error("这个链接不是受支持的官方 HTTPS 地址");
  await api("POST", !authorization && entryId ? `/api/vault/entries/${encodeURIComponent(entryId)}/open-verification` : "/api/open-external", { url });
}
