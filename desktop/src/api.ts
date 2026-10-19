import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** 在 Tauri 窗口里走 IPC；在普通浏览器里（vite 调试）走 HTTP，接口完全一致。 */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

function token(): string {
  try {
    return localStorage.getItem("cam-token") || "";
  } catch {
    return "";
  }
}

export async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
  if (inTauri) {
    try {
      return await invoke<T>("api_call", { method, path, body: body ?? null });
    } catch (e) {
      throw new Error(typeof e === "string" ? e : String(e));
    }
  }
  const res = await fetch(path, {
    method,
    headers: { Authorization: `Bearer ${token()}`, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await res.json().catch(() => null);
  if (!res.ok) throw new Error((data && data.error) || `HTTP ${res.status}`);
  return data as T;
}

export async function command<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  if (!inTauri) throw new Error("这个功能只在桌面版里可用");
  try {
    return await invoke<T>(name, args);
  } catch (e) {
    throw new Error(typeof e === "string" ? e : String(e));
  }
}

export async function onEvent<T>(name: string, fn: (payload: T) => void): Promise<UnlistenFn> {
  if (!inTauri) return () => {};
  return listen<T>(name, (e) => fn(e.payload));
}
