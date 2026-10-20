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
  warning?: string | null; // 后端提醒，比如凭据和 ~/.claude.json 对不上
  pending_switch?: PendingSwitch | null; // 已记录、等 claude 会话结束后由后台执行的切换
  region?: RegionInfo | null; // 后台上次检测到的出口地区，只用于提示
  risks?: ServiceCheckItem[]; // 后台缓存算出的风险提示（出口不支持 / 出口变化 / 时区 / 时钟 / 官方状态页 / 账号多地区…），只提醒不拦
  risk_score?: RiskScore | null; // 据 risks 算的环境风险评分
}

/** 环境风险评分：满分 100，≥90 低 / 70–89 中 / <70 高。只是把提醒汇总成一个数 */
export interface RiskScore {
  score: number;
  grade: "low" | "medium" | "high";
  label: string;
  advice: string;
}

export interface PendingSwitch {
  account: string;
  label: string;
  reason: "manual" | "rule";
  at: string;
  message: string;
}

/** POST /api/accounts/{id}/switch 的返回；有会话在跑且没给 mode 时 choice_required 为 true */
export interface SwitchResult {
  ok: boolean;
  message: string;
  changed?: boolean;
  deferred?: boolean;
  choice_required?: boolean;
  running_claude?: number;
  account?: string;
  label?: string;
  risks?: ServiceCheckItem[]; // 切换时的风险提醒（warn / bad），不影响切换本身
}

export interface ServiceCheckItem {
  kind?: string; // summary / dns / region / isp / ip_origin / ipv6 / dns_servers / api / web / proxy / vpn / region_change / region_mismatch / proxy_ip / tor_exit / ipv6_mismatch / dns_resolver / warp / clock / status / timezone / locale / account_regions / accounts / device / scope
  level: "ok" | "bad" | "warn" | "info";
  text: string;
  advice: string; // 这意味着什么 / 建议怎么做，可能为空
  score?: number; // 只有 summary 项有：实测后的评分和档位
  grade?: "low" | "medium" | "high";
}

export interface RegionInfo {
  loc: string | null;
  ip?: string | null;
  status: "supported" | "unsupported" | "unknown" | null; // null = 还没检测过
  checked_at: string | null;
  message: string;
  advice: string;
  warp?: boolean; // 请求经过了 Cloudflare WARP / Zero Trust
  prev_loc?: string | null; // 上一次检测到的不同出口地区（24 小时内变过）
  changed_at?: string | null;
  org?: string | null; // 出口 IP 的运营商
  city?: string | null;
  ip_type?: "residential" | "hosting" | "proxy" | "tor" | "mobile" | null; // 数据库判定的 IP 类型，null = 数据源没有标记
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
