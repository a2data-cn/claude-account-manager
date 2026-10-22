import { useSyncExternalStore } from "react";

/** 显示偏好（只记在这台设备上）：额度看已用还是剩余、邮箱打码。和手机面板用同一组 localStorage 键 */
export interface Prefs {
  remain: boolean;
  mask: boolean;
}

const KEYS: Record<keyof Prefs, string> = { remain: "cam-remain", mask: "cam-mask" };

function read(): Prefs {
  try {
    return { remain: localStorage.getItem(KEYS.remain) === "1", mask: localStorage.getItem(KEYS.mask) === "1" };
  } catch {
    return { remain: false, mask: false };
  }
}

let current = read();
const listeners = new Set<() => void>();

export function setPref(k: keyof Prefs, v: boolean) {
  current = { ...current, [k]: v };
  try {
    localStorage.setItem(KEYS[k], v ? "1" : "0");
  } catch {
    /* 存不了就只在这次打开时有效 */
  }
  listeners.forEach((l) => l());
}

export function usePrefs(): Prefs {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => current,
  );
}

// 打码后是 ch***@y***.com，截图分享时用
const MAIL_RE = /([^\s@<>"']{1,2})[^\s@<>"']*@([^\s@<>"'.])[^\s@<>"']*?(\.[A-Za-z]{2,})(?![\w.])/g;
export const maskMail = (s: string | null | undefined, on: boolean) => (on && s ? s.replace(MAIL_RE, "$1***@$2***$3") : s);
