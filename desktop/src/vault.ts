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
  subject: string;
  received_at: string;
  codes: string[];
  verification_links: string[];
}
export interface VaultMessages { checked_at: string; status: "found" | "empty"; messages: VaultMessage[] }
export interface VaultJob { job_id: string; authorization_url: string; expires_at: string | number }

export const VAULT_PROVIDERS: Record<VaultProvider, string> = {
  credential: "仅保存账号资料", gmail: "Gmail / Google Workspace", outlook: "Outlook / Hotmail / Microsoft 365", imap: "其它企业邮箱（IMAP）",
};

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
