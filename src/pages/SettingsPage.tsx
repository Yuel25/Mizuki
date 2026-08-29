import { useEffect, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { check as checkForUpdate, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import type { AppSettings, BangumiProfile, SyncStatus } from "../types";

function Setting({ label, value, button }: { label: string; value: string; button?: string }) {
  return (
    <div className="setting-row">
      <span>{label}</span>
      <div>
        <b>{value}</b>
        {button && <button className="ghost">{button}</button>}
      </div>
    </div>
  );
}

export function SettingsPage({
  profile,
  onProfile,
  onSynced,
  syncStatus,
  onSyncStatus,
  onSettingsSaved,
}: {
  profile: BangumiProfile | null;
  onProfile: (value: BangumiProfile | null) => void;
  onSynced: () => void;
  syncStatus: SyncStatus | null;
  onSyncStatus: (value: SyncStatus | null) => void;
  onSettingsSaved: (settings: AppSettings) => void;
}) {
  const [token, setToken] = useState(""),
    [busy, setBusy] = useState(false),
    [message, setMessage] = useState(""),
    [settings, setSettings] = useState<AppSettings | null>(null),
    [appVersion, setAppVersion] = useState(""),
    [update, setUpdate] = useState<Update | null>(null),
    [updateBusy, setUpdateBusy] = useState(false),
    [updateMessage, setUpdateMessage] = useState("");
  useEffect(() => {
    if (!isTauri()) return;
    invoke<AppSettings>("get_settings")
      .then(setSettings)
      .catch(() => {
        /* 浏览器预览无此命令 */
      });
    getVersion()
      .then(setAppVersion)
      .catch(() => {});
  }, []);
  function patch(update: Partial<AppSettings>) {
    setSettings((current) => (current ? { ...current, ...update } : current));
  }
  async function save() {
    if (!settings) return;
    setBusy(true);
    setMessage("");
    try {
      const saved = await invoke<AppSettings>("save_settings", { settings });
      setSettings(saved);
      onSettingsSaved(saved);
      setMessage("设置已保存并生效");
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
    }
  }
  async function checkUpdate() {
    setUpdateBusy(true);
    setUpdateMessage("");
    try {
      const found = await checkForUpdate();
      setUpdate(found ?? null);
      setUpdateMessage(found ? `发现新版本 ${found.version}` : "已是最新版本");
    } catch (error) {
      setUpdateMessage(`检查更新失败：${String(error)}`);
    } finally {
      setUpdateBusy(false);
    }
  }
  async function installUpdate() {
    if (!update) return;
    setUpdateBusy(true);
    try {
      await update.downloadAndInstall((event) => {
        if (event.event === "Started" && event.data.contentLength)
          setUpdateMessage(`开始下载，共 ${(event.data.contentLength / 1048576).toFixed(1)} MB`);
        else if (event.event === "Progress")
          setUpdateMessage(`下载中 ${(event.data.chunkLength / 1048576).toFixed(1)} MB`);
        else if (event.event === "Finished") setUpdateMessage("下载完成，正在重启…");
      });
      await relaunch();
    } catch (error) {
      setUpdateMessage(`安装失败：${String(error)}`);
    } finally {
      setUpdateBusy(false);
    }
  }
  async function connect() {
    if (!token.trim()) return;
    setBusy(true);
    setMessage("");
    try {
      const value = await invoke<BangumiProfile>("save_bangumi_token", { token: token.trim() });
      onProfile(value);
      setToken("");
      const count = await invoke<number>("sync_bangumi_collections");
      onSynced();
      refreshSync();
      setMessage(`已连接并导入 ${count} 条收藏`);
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
    }
  }
  async function refreshSync() {
    try {
      onSyncStatus(await invoke<SyncStatus>("get_sync_status"));
    } catch {
      /* 忽略状态查询失败 */
    }
  }
  async function sync() {
    setBusy(true);
    setMessage("");
    try {
      const count = await invoke<number>("sync_bangumi_collections");
      onSynced();
      await refreshSync();
      setMessage(`已同步 ${count} 条收藏`);
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
    }
  }
  async function retry() {
    setBusy(true);
    setMessage("");
    try {
      const status = await invoke<SyncStatus>("retry_sync_now");
      onSyncStatus(status);
      setMessage(status.pending ? "仍有条目在退避等待，稍后会自动重试" : "待同步改动已全部写回 Bangumi");
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy(false);
    }
  }
  async function disconnect() {
    await invoke("remove_bangumi_token");
    onProfile(null);
    onSyncStatus(null);
    setMessage("已断开 Bangumi，本地收藏仍会保留");
  }
  return (
    <>
      <header>
        <div>
          <p className="eyebrow">PREFERENCES</p>
          <h1>设置</h1>
          <p>管理 Bangumi 连接、本地数据、下载路径与后台行为。</p>
        </div>
      </header>
      <div className="settings-grid">
        <section>
          <h2>Bangumi 同步</h2>
          <div className="account">
            {profile?.avatar ? (
              <img className="avatar-image" src={profile.avatar} />
            ) : (
              <div className="avatar">本</div>
            )}
            <div>
              <h3>{profile?.nickname || profile?.username || "本地模式"}</h3>
              <p>
                {profile
                  ? `@${profile.username} · 收藏改动将同步到 Bangumi`
                  : "不填写 Token 也可正常使用本地追番"}
              </p>
            </div>
            {profile ? (
              <button className="ghost" onClick={disconnect}>
                断开
              </button>
            ) : (
              <span className="tag">可选</span>
            )}
          </div>
          <div className="local-data">
            <div className="section-title">
              <h3>Personal Access Token</h3>
              <span className={profile ? "configured" : ""}>{profile ? "已安全保存" : "无需 OAuth"}</span>
            </div>
            <p>
              在 Bangumi 生成 Access Token 后粘贴到这里。Token 只保存在 Windows 凭据管理器，不写入数据库。
            </p>
            {!profile && (
              <>
                <label>
                  Access Token
                  <input
                    type="password"
                    value={token}
                    onChange={(e) => setToken(e.target.value)}
                    placeholder="粘贴 Bangumi Access Token"
                    onKeyDown={(e) => e.key === "Enter" && connect()}
                  />
                </label>
                <div className="token-actions">
                  <button className="ghost" onClick={() => openUrl("https://next.bgm.tv/demo/access-token")}>
                    打开 Token 生成页 ↗
                  </button>
                  <button className="primary" disabled={busy || !token.trim()} onClick={connect}>
                    {busy ? "验证中…" : "验证并连接"}
                  </button>
                </div>
              </>
            )}
            {profile && (
              <button className="primary" disabled={busy} onClick={sync}>
                {busy ? "同步中…" : "立即同步收藏"}
              </button>
            )}
            {profile && syncStatus && (
              <div className={`sync-status${syncStatus.pending ? " pending" : ""}`}>
                {syncStatus.pending ? (
                  <>
                    <span>
                      {syncStatus.pending} 条收藏改动待同步
                      {syncStatus.lastError ? ` · 上次失败：${syncStatus.lastError}` : ""}
                    </span>
                    <button className="ghost" disabled={busy} onClick={retry}>
                      立即重试
                    </button>
                  </>
                ) : (
                  <span className="ok">✓ 收藏改动已全部同步</span>
                )}
              </div>
            )}
            {message && <small className="auth-message">{message}</small>}
          </div>
        </section>
        <section>
          <h2>下载</h2>
          <label className="setting-row field-row">
            <span>下载目录</span>
            <input
              value={settings?.downloadDir ?? ""}
              placeholder="默认：系统下载目录\Mizuki"
              onChange={(e) => patch({ downloadDir: e.target.value.trim() || null })}
            />
          </label>
          <label className="setting-row field-row">
            <span>下载限速 KB/s（0 不限）</span>
            <input
              type="number"
              min={0}
              max={1000000}
              value={settings?.btDownloadKbps ?? 0}
              onChange={(e) =>
                patch({ btDownloadKbps: Math.max(0, Math.round(Number(e.target.value) || 0)) })
              }
            />
          </label>
          <label className="setting-row field-row">
            <span>上传限速 KB/s（0 不限）</span>
            <input
              type="number"
              min={0}
              max={1000000}
              value={settings?.btUploadKbps ?? 0}
              onChange={(e) => patch({ btUploadKbps: Math.max(0, Math.round(Number(e.target.value) || 0)) })}
            />
          </label>
          <label className="setting-row field-row">
            <span>BT 端口（0 随机，重启生效）</span>
            <input
              type="number"
              min={0}
              max={65535}
              value={settings?.btListenPort ?? 0}
              onChange={(e) =>
                patch({ btListenPort: Math.min(65535, Math.max(0, Math.round(Number(e.target.value) || 0))) })
              }
            />
          </label>
          <label className="setting-row field-row">
            <span>每任务连接数（重启生效）</span>
            <input
              type="number"
              min={1}
              max={2000}
              value={settings?.btPeerLimit ?? 256}
              onChange={(e) =>
                patch({ btPeerLimit: Math.min(2000, Math.max(1, Math.round(Number(e.target.value) || 256))) })
              }
            />
          </label>
          <label className="setting-row field-row">
            <span>同时下载数（0 不限）</span>
            <input
              type="number"
              min={0}
              max={20}
              value={settings?.maxConcurrentDownloads ?? 0}
              onChange={(e) =>
                patch({
                  maxConcurrentDownloads: Math.min(20, Math.max(0, Math.round(Number(e.target.value) || 0))),
                })
              }
            />
          </label>
          <label className="setting-row field-row checkbox-row">
            <span>下载完成后停止做种</span>
            <input
              type="checkbox"
              checked={settings?.stopSeedingOnComplete ?? true}
              onChange={(e) => patch({ stopSeedingOnComplete: e.target.checked })}
            />
          </label>
        </section>
        <section>
          <h2>后台与数据</h2>
          <label className="setting-row field-row">
            <span>RSS 刷新间隔（分钟）</span>
            <input
              type="number"
              min={5}
              max={1440}
              value={settings?.rssIntervalMinutes ?? 15}
              onChange={(e) =>
                patch({
                  rssIntervalMinutes: Math.min(1440, Math.max(5, Math.round(Number(e.target.value) || 15))),
                })
              }
            />
          </label>
          <label className="setting-row field-row checkbox-row">
            <span>开机自动启动</span>
            <input
              type="checkbox"
              checked={settings?.autostart ?? false}
              onChange={(e) => patch({ autostart: e.target.checked })}
            />
          </label>
          <label className="setting-row field-row checkbox-row">
            <span>关闭窗口时最小化到托盘</span>
            <input
              type="checkbox"
              checked={settings?.closeToTray ?? true}
              onChange={(e) => patch({ closeToTray: e.target.checked })}
            />
          </label>
          <Setting label="收藏数据" value={profile ? "本地 + Bangumi" : "本地 SQLite"} />
          <div className="settings-save">
            <button className="primary" disabled={busy || !settings} onClick={save}>
              {busy ? "保存中…" : "保存设置"}
            </button>
          </div>
        </section>
        {isTauri() && (
          <section>
            <h2>应用更新</h2>
            <Setting label="当前版本" value={appVersion || "…"} />
            {update && (
              <div className="update-available">
                <div>
                  <b>Mizuki {update.version} 可用</b>
                  {update.body && <p>{update.body}</p>}
                </div>
                <button className="primary" disabled={updateBusy} onClick={installUpdate}>
                  {updateBusy ? "安装中…" : "下载并重启"}
                </button>
              </div>
            )}
            <div className="settings-save">
              <button className="ghost" disabled={updateBusy} onClick={checkUpdate}>
                {updateBusy ? "检查中…" : "检查更新"}
              </button>
            </div>
            {updateMessage && <small className="auth-message">{updateMessage}</small>}
          </section>
        )}
      </div>
    </>
  );
}
