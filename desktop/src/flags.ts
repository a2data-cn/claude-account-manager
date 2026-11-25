import flagFontUrl from "../../web/flags.woff2";

/**
 * 国旗：Windows 自带的表情字体里没有国旗，桌面版（WebView2）会把国旗显示成两个字母（JP）。
 * 先在画布上试一下：彩色字形不跟着填充色变，白字黑字画出来一样就是能显示；显示不了才加载自带的国旗字体
 * （web/flags.woff2，Twemoji 国旗，CC-BY 4.0），只用在国旗这几个字符上。和 web/panel.html 脚本开头“国旗”那一段一样
 */
function colorGlyph(text: string): boolean {
  const c = document.createElement("canvas");
  c.width = c.height = 1;
  const x = c.getContext("2d", { willReadFrequently: true });
  if (!x) return true;
  x.textBaseline = "top";
  x.font = '100px "Twemoji Mozilla", "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji", sans-serif';
  x.scale(0.01, 0.01);
  const draw = (fill: string) => {
    x.clearRect(0, 0, 100, 100);
    x.fillStyle = fill;
    x.fillText(text, 0, 0);
    return x.getImageData(0, 0, 1, 1).data.join(",");
  };
  const white = draw("#fff");
  const black = draw("#000");
  return white === black && !black.startsWith("0,0,0,");
}

export function loadFlagFontIfNeeded() {
  try {
    if (colorGlyph("\u{1F1E8}\u{1F1ED}")) return;
    const st = document.createElement("style");
    st.textContent = `@font-face { font-family: "CamFlags"; src: url("${flagFontUrl}") format("woff2"); unicode-range: U+1F1E6-1F1FF; font-display: swap; }`;
    document.head.appendChild(st);
    document.documentElement.classList.add("flagfont");
  } catch {
    /* 画布用不了：照旧用系统字体 */
  }
}
