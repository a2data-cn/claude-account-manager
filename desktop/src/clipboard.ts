/** 复制到剪贴板。webview 里 navigator.clipboard 可能没有或没权限，退到老办法（临时文本框 + execCommand）。 */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 没权限：下面用老办法
  }
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.cssText = "position:fixed;left:-9999px;top:0";
    document.body.appendChild(ta);
    ta.select();
    ta.setSelectionRange(0, text.length);
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

/** 名字拼进 shell 命令：只有字母、数字、汉字、_ . - 的原样放，其它用单引号包起来。 */
export function shellArg(s: string): string {
  return /^[A-Za-z0-9_.\-一-鿿]+$/.test(s) ? s : `'${s.replace(/'/g, "'\\''")}'`;
}
