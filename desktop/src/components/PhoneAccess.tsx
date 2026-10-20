import { useEffect, useState } from "react";
import QRCode from "qrcode";
import { command } from "../api";
import type { ServerInfo, Settings } from "../types";

/** 手机访问：开启内置 Web 面板，扫码即可在手机上查看和切换。 */
export function PhoneAccess({ toast }: { toast: (m: string, bad?: boolean) => void }) {
  const [info, setInfo] = useState<ServerInfo | null>(null);
  const [cfg, setCfg] = useState<Settings | null>(null);
  const [qr, setQr] = useState<string>("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    command<ServerInfo>("server_info")
      .then((i) => {
        setInfo(i);
        setCfg(i.settings);
      })
      .catch((e) => toast((e as Error).message, true));
  }, [toast]);

  // 优先给局域网地址生成二维码
  const phoneUrl = info?.urls.find((u) => !u.includes("127.0.0.1")) || "";
  useEffect(() => {
    if (!phoneUrl) return setQr("");
    QRCode.toDataURL(phoneUrl, { margin: 1, width: 220 }).then(setQr).catch(() => setQr(""));
  }, [phoneUrl]);

  if (!info || !cfg) return null;

  const save = async (next: Settings) => {
    setBusy(true);
    try {
      const i = await command<ServerInfo>("save_settings", { settings: next });
      setInfo(i);
      setCfg(i.settings);
      toast(next.server_enabled ? "手机访问已开启" : "设置已保存");
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      setBusy(false);
    }
  };

  // 没有剪贴板权限（或根本没有 navigator.clipboard）时给个提示，而不是悄悄失败
  const copy = (u: string) =>
    Promise.resolve()
      .then(() => navigator.clipboard.writeText(u))
      .then(() => toast("链接已复制"))
      .catch(() => toast("复制失败，请手动选中链接复制", true));

  const resetToken = async () => {
    if (!confirm("重置后，之前在手机上打开过的链接都会失效，需要重新扫码。继续？")) return;
    try {
      setInfo(await command<ServerInfo>("reset_token"));
      toast("访问令牌已重置");
    } catch (e) {
      toast((e as Error).message, true);
    }
  };

  return (
    <section className="block">
      <h2>手机和其它设备</h2>
      <div className="panel phone">
        <div className="phone-main">
          <label className="check">
            <input
              type="checkbox"
              checked={cfg.server_enabled}
              disabled={busy}
              onChange={(e) => save({ ...cfg, server_enabled: e.target.checked })}
            />
            开启 Web 面板
          </label>
          <label className="check">
            <input type="checkbox" checked={cfg.lan} disabled={busy} onChange={(e) => save({ ...cfg, lan: e.target.checked })} />
            允许同一局域网的手机访问
          </label>
          <div className="row2">
            <div className="field">
              <label htmlFor="s-port">端口</label>
              <input
                id="s-port"
                type="number"
                min={1024}
                max={65535}
                value={cfg.port}
                onChange={(e) => setCfg({ ...cfg, port: Number(e.target.value) })}
                onBlur={() => info.settings.port !== cfg.port && save(cfg)}
              />
            </div>
            <div className="field">
              <label htmlFor="s-every">自动刷新用量</label>
              <select id="s-every" value={cfg.auto_refresh_min} onChange={(e) => save({ ...cfg, auto_refresh_min: Number(e.target.value) })}>
                <option value={5}>每 5 分钟</option>
                <option value={10}>每 10 分钟</option>
                <option value={30}>每 30 分钟</option>
                <option value={0}>不自动刷新</option>
              </select>
            </div>
          </div>
          {info.running ? (
            <>
              <ul className="facts">
                {info.urls.map((u) => (
                  <li key={u} className="url">
                    <code>{u}</code>{" "}
                    <button className="btn quiet" onClick={() => copy(u)}>
                      复制
                    </button>
                  </li>
                ))}
              </ul>
              <p className="hint">
                链接里带访问令牌，等同于密码，不要发到群里。在外面用手机访问，建议在电脑和手机上都装 Tailscale 等组网工具，不要把端口直接暴露到公网。
              </p>
              <button className="btn quiet danger" onClick={resetToken}>
                重置访问令牌
              </button>
            </>
          ) : (
            <p className="hint">开启后，手机扫码就能查看所有账号的用量，也能一键切换电脑上的 Claude Code 账号。</p>
          )}
        </div>
        {info.running && qr && (
          <figure className="qr">
            <img src={qr} width={180} height={180} alt="手机扫码打开面板" />
            <figcaption>手机扫码打开</figcaption>
          </figure>
        )}
        {info.running && !qr && cfg.lan && <p className="hint">没找到局域网地址，请确认电脑已连上 Wi-Fi 或网线。</p>}
      </div>
    </section>
  );
}
