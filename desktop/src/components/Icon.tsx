/** 卡片标题用的细线图标。路径和 web/panel.html 里的 <symbol id="i-…"> 一一对应，改一处要同步另一处。 */
const PATHS = {
  globe: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c-3.2 3-3.2 15 0 18M12 3c3.2 3 3.2 15 0 18"/>',
  shield: '<path d="M12 3l7 2.8v5.4c0 4.4-3 8.2-7 9.8-4-1.6-7-5.4-7-9.8V5.8L12 3z"/><path d="M9 12l2 2 4-4"/>',
  gauge: '<path d="M5 17.5a8.5 8.5 0 1 1 14 0"/><path d="M12 12.7l3.3-3.8"/><circle cx="12" cy="12.7" r="1.1"/>',
  user: '<circle cx="12" cy="8.5" r="3.8"/><path d="M4.5 20.5c.5-4 3.6-6.3 7.5-6.3s7 2.3 7.5 6.3"/>',
  receipt: '<path d="M6.5 3.5h11v17l-2.75-1.8L12 20.5l-2.75-1.8-2.75 1.8v-17z"/><path d="M9.5 8.5h5M9.5 12h5M9.5 15.5h3"/>',
  calendar: '<rect x="3.5" y="5" width="17" height="15.5" rx="2.5"/><path d="M3.5 10h17M8 3v4M16 3v4M8 14h2.5M13.5 14H16M8 17.5h2.5"/>',
  pie: '<path d="M12 12V3.5a8.5 8.5 0 1 0 8.5 8.5H12z"/><path d="M13.5 10.5V2a8.5 8.5 0 0 1 8.5 8.5h-8.5z"/>',
  clock: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>',
  plus: '<circle cx="12" cy="12" r="8.5"/><path d="M12 8.5v7M8.5 12h7"/>',
  bell: '<path d="M6.5 16.5V11a5.5 5.5 0 0 1 11 0v5.5l1.5 2h-14l1.5-2z"/><path d="M10 20.5a2 2 0 0 0 4 0"/>',
  search: '<circle cx="11" cy="11" r="6"/><path d="M15.5 15.5L20 20"/>',
  link: '<path d="M10.5 13.5a3.5 3.5 0 0 0 5 0l3-3a3.5 3.5 0 0 0-5-5l-1 1"/><path d="M13.5 10.5a3.5 3.5 0 0 0-5 0l-3 3a3.5 3.5 0 0 0 5 5l1-1"/>',
  pin: '<path d="M12 20.5s-6-5.5-6-10.3a6 6 0 0 1 12 0c0 4.8-6 10.3-6 10.3z"/><circle cx="12" cy="10.2" r="2.2"/>',
  refresh: '<path d="M20 12a8 8 0 1 1-8-8c2.2 0 4.3.9 5.9 2.4L20 8.5"/><path d="M20 4v4.5h-4.5"/>',
  tag: '<path d="M3.5 11.5V4.5a1 1 0 0 1 1-1h7l9 9-8 8-9-9z"/><circle cx="8" cy="8" r="1.3"/>',
  bars: '<path d="M5.5 19.5v-6M12 19.5V5.5M18.5 19.5v-9M3 19.5h18"/>',
  signal: '<path d="M4.5 18.5v-3M9.5 18.5v-7M14.5 18.5V7.5M19.5 18.5v-14"/>',
  list: '<path d="M4 7h2M4 12h2M4 17h2M9.5 7H20M9.5 12H20M9.5 17H20"/>',
} as const;

export type IconName = keyof typeof PATHS;

/** 卡片标题前的图标小方块（样式见 styles.css 的 .eico） */
export function Ico({ name }: { name: IconName }) {
  return (
    <span className="eico">
      <svg viewBox="0 0 24 24" aria-hidden="true" dangerouslySetInnerHTML={{ __html: PATHS[name] }} />
    </span>
  );
}
