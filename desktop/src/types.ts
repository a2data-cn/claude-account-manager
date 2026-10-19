export type Kind = "claude_code" | "claude_web" | "api";

export interface Window5 {
  utilization: number;
  resets_at: string | null;
}

export interface Usage {
  source?: string;
  fetched_at?: string;
  error?: string | null;
  five_hour?: Window5 | null;
  seven_day?: Window5 | null;
  seven_day_opus?: Window5 | null;
  seven_day_sonnet?: Window5 | null;
  cost_month_usd?: number | null;
  key_valid?: boolean;
  key_error?: string;
}

export interface Manual {
  plan?: string | null;
  expires_at?: string | null;
  auto_renew?: boolean | null;
  cycle?: "monthly" | "yearly" | null;
  price?: string | null;
  note?: string | null;
  app_mode?: boolean | null;
}

export interface Account {
  id: string;
  kind: Kind;
  label: string | null;
  email: string | null;
  plan: string | null;
  plan_display: string | null;
  org_name: string | null;
  manual: Manual | null;
  usage: Usage | null;
  is_active: boolean;
  days_left: number | null;
  api_key_hint?: string | null;
  has_admin_key?: boolean;
  has_session_key?: boolean;
}

export interface State {
  version: string;
  backend: string;
  host: string;
  platform: string;
  accounts: Account[];
  generated_at: string;
}

export interface LocalRow {
  date: string;
  account_id: string;
  input: number;
  output: number;
  cache_write: number;
  cache_read: number;
  messages: number;
}

export interface Settings {
  server_enabled: boolean;
  lan: boolean;
  port: number;
  auto_refresh_min: number;
}

export interface ServerInfo {
  running: boolean;
  port?: number;
  lan?: boolean;
  urls: string[];
  settings: Settings;
}

export type WindowKey = "five_hour" | "seven_day" | "seven_day_opus" | "seven_day_sonnet";
