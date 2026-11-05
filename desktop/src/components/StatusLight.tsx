import type { State } from "../types";

type Item = { lv: "ok" | "busy" | "bad"; text: string; tip?: string | null; pulse?: boolean; blink?: boolean };

/** 标题旁的红绿灯要说的话：规则和网页面板、卡片小面板、菜单栏图标一致，按严重程度排。
 *  bad = 要处理（后台提醒、当前号登录失效、额度用完）；busy = 等会话结束再切、有 claude 会话在跑、额度快满、
 *  当前号要注意、别的号有问题；都没有就是 ok */
export function lightItems(state: State): Item[] {
  const items: Item[] = [];
  const cur = state.accounts.find((a) => a.is_active && a.kind === "claude_code");
  const h = cur?.health;
  const issue = h?.issues?.[0];
  const p5 = cur?.usage?.five_hour?.utilization ?? null;
  const p7 = cur?.usage?.seven_day?.utilization ?? null;
  const n = state.sessions?.count ?? null;
  const working = !!state.sessions?.busy; // 开着但在等你输入的不算工作中
  if (state.warning) items.push({ lv: "bad", text: state.warning });
  if (h?.level === "error") items.push({ lv: "bad", text: `当前号${issue?.text ? "：" + issue.text : "有问题"}`, tip: issue?.advice });
  if (p5 !== null && p5 >= 100) items.push({ lv: "bad", text: "5 小时额度用完了" });
  if (state.pending_switch) items.push({ lv: "busy", blink: true, text: state.pending_switch.message });
  if (n !== null && n > 0 && working) items.push({ lv: "busy", pulse: true, text: `工作中 · ${n} 个 Claude Code 会话在跑` });
  if (p5 !== null && p5 >= 90) items.push({ lv: "busy", text: `5 小时额度 ${Math.round(p5)}%，快满了` });
  else if (p7 !== null && p7 >= 90) items.push({ lv: "busy", text: `7 天额度 ${Math.round(p7)}%，快满了` });
  if (h?.level === "warn") items.push({ lv: "busy", text: `当前号：${issue?.text || h.label || "要注意"}`, tip: issue?.advice });
  for (const a of state.accounts) {
    if (a.id !== cur?.id && a.health?.level === "error") {
      items.push({ lv: "busy", text: `${a.label || a.email || "未命名"}：${a.health.issues?.[0]?.text || "登录有问题"}` });
    }
  }
  if (!items.length) items.push({ lv: "ok", text: n !== null && n > 0 ? `空闲 · ${n} 个 Claude Code 会话开着` : "空闲 · 一切正常" });
  return items;
}

/** 标题旁的红绿灯：只显示最要紧的一条，其余在悬停提示里；有会话在跑时一呼一吸，等会话结束再切时一亮一暗 */
export function StatusLight({ state }: { state: State }) {
  const items = lightItems(state);
  const top = items[0];
  const title = items.map((i) => i.text).join("\n") + (top.tip ? "\n" + top.tip : "");
  return (
    <span className={`light ${top.lv}${top.pulse ? " pulse" : ""}${top.blink ? " blink" : ""}`} role="status" title={title}>
      <i className="led" />
      <span className="txt">{top.text}</span>
      {items.length > 1 && <span className="more">+{items.length - 1}</span>}
    </span>
  );
}
