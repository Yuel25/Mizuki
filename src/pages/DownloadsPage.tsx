import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ConfirmDialog } from "../components/ConfirmDialog";
import { formatSpeed } from "../lib";
import type { DownloadTask, PlaybackFile } from "../types";

export function DownloadsPage({ tasks, refresh }: { tasks: DownloadTask[]; refresh: () => Promise<void> }) {
  const [busy, setBusy] = useState(""),
    [source, setSource] = useState(""),
    [title, setTitle] = useState(""),
    [deleting, setDeleting] = useState<{ task: DownloadTask; files: boolean } | null>(null),
    [playing, setPlaying] = useState<PlaybackFile[] | null>(null);
  useEffect(() => {
    const timer = window.setInterval(refresh, 2000);
    return () => window.clearInterval(timer);
  }, [refresh]);
  async function openLocal(path: string) {
    try {
      await invoke("open_local_path", { path });
    } catch (error) {
      alert(`打开失败：${String(error)}`);
    }
  }
  async function action(id: string, command: string, args: Record<string, unknown> = {}) {
    setBusy(id);
    try {
      await invoke(command, { id, ...args });
      await refresh();
    } catch (error) {
      alert(String(error));
    } finally {
      setBusy("");
    }
  }
  async function play(id: string) {
    setBusy(id);
    try {
      const files = await invoke<PlaybackFile[]>("download_playback_files", { id });
      if (files.length === 1) await openLocal(files[0].path);
      else setPlaying(files);
    } catch (error) {
      const fallback = tasks.find((task) => task.id === id)?.playbackPath;
      if (fallback) await openLocal(fallback);
      else alert(`无法播放：${String(error)}`);
    } finally {
      setBusy("");
    }
  }
  async function addManual() {
    if (!source.trim()) return;
    setBusy("manual");
    try {
      await invoke("add_download", {
        source: source.trim(),
        title: title.trim() || "手动下载",
        episode: "手动添加",
      });
      setSource("");
      setTitle("");
      await refresh();
    } catch (error) {
      alert(String(error));
    } finally {
      setBusy("");
    }
  }
  return (
    <>
      <header>
        <div>
          <p className="eyebrow">DOWNLOAD MANAGER</p>
          <h1>下载管理</h1>
          <p>暂停、继续和管理本地下载任务。</p>
        </div>
        <button className="ghost" onClick={refresh}>
          ↻ 刷新
        </button>
      </header>
      <div className="manual-download">
        <input
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          placeholder="任务名称（可选）"
        />
        <input
          value={source}
          onChange={(event) => setSource(event.target.value)}
          placeholder="magnet: 或 .torrent URL"
          onKeyDown={(event) => event.key === "Enter" && addManual()}
        />
        <button className="primary" disabled={!source.trim() || busy === "manual"} onClick={addManual}>
          {busy === "manual" ? "添加中…" : "添加任务"}
        </button>
      </div>
      <div className="download-summary">
        <div>
          <span>⇩</span>
          <strong>{formatSpeed(tasks.reduce((a, b) => a + b.downSpeed, 0))}</strong>
          <small>下载速度</small>
        </div>
        <div>
          <span>◉</span>
          <strong>{tasks.filter((t) => t.state === "downloading").length}</strong>
          <small>活动任务</small>
        </div>
        <div>
          <span>✓</span>
          <strong>{tasks.filter((t) => t.state === "completed").length}</strong>
          <small>已完成</small>
        </div>
      </div>
      {tasks.length ? (
        <div className="download-list">
          {tasks.map((t) => (
            <article key={t.id}>
              <div className="file-icon">
                {t.state === "completed" ? "✓" : t.state === "paused" ? "Ⅱ" : "⇩"}
              </div>
              <div className="download-info">
                <div>
                  <h3>{t.title}</h3>
                  <span>
                    {t.state === "downloading"
                      ? `${formatSpeed(t.downSpeed)} · 下载中`
                      : t.state === "completed"
                        ? "已完成"
                        : t.state === "paused"
                          ? "已暂停"
                          : t.state === "failed"
                            ? "下载失败"
                            : t.state === "queued"
                              ? "排队中"
                              : "等待中"}
                  </span>
                </div>
                <p>{t.episode}</p>
                <div className="progress">
                  <i style={{ width: `${t.progress * 100}%` }} />
                </div>
              </div>
              <strong>{Math.round(t.progress * 100)}%</strong>
              <div className="download-actions">
                {t.state === "downloading" && (
                  <button disabled={busy === t.id} onClick={() => action(t.id, "pause_download")}>
                    暂停
                  </button>
                )}
                {t.state === "paused" && (
                  <button disabled={busy === t.id} onClick={() => action(t.id, "resume_download")}>
                    继续
                  </button>
                )}
                {t.state === "completed" && (
                  <button className="play" disabled={busy === t.id} onClick={() => play(t.id)}>
                    {busy === t.id ? "打开中…" : "▶ 播放"}
                  </button>
                )}
                <button disabled={!t.outputPath} onClick={() => openLocal(t.outputPath)}>
                  目录
                </button>
                <button onClick={() => setDeleting({ task: t, files: false })}>移除</button>
                <button className="danger" onClick={() => setDeleting({ task: t, files: true })}>
                  删除文件
                </button>
              </div>
            </article>
          ))}
        </div>
      ) : (
        <div className="empty">
          <span>⇩</span>
          <h3>暂无下载任务</h3>
          <p>从 RSS 资源列表中选择条目，或在上方粘贴磁力链接。</p>
        </div>
      )}
      {deleting && (
        <ConfirmDialog
          title={deleting.files ? "删除任务和文件？" : "移除下载任务？"}
          description={
            deleting.files
              ? "本地已下载的数据将一并删除，此操作无法撤销。"
              : "任务将从列表中移除，本地已下载文件会保留。"
          }
          item={deleting.task.title}
          danger={deleting.files}
          confirmText={deleting.files ? "删除文件" : "仅移除任务"}
          onCancel={() => setDeleting(null)}
          onConfirm={async () => {
            const current = deleting;
            setDeleting(null);
            await action(current.task.id, "delete_download", { deleteFiles: current.files });
          }}
        />
      )}
      {playing && (
        <div className="confirm-backdrop" onMouseDown={() => setPlaying(null)}>
          <section
            className="confirm-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="play-title"
            onMouseDown={(event) => event.stopPropagation()}
          >
            <div className="confirm-icon">▶</div>
            <div>
              <p className="eyebrow">CHOOSE EPISODE</p>
              <h2 id="play-title">选择要播放的文件</h2>
              <div className="play-files">
                {playing.map((file) => (
                  <button
                    key={file.index}
                    onClick={() => {
                      setPlaying(null);
                      openLocal(file.path);
                    }}
                  >
                    {file.name}
                    <small>
                      {file.size >= 1073741824
                        ? `${(file.size / 1073741824).toFixed(2)} GB`
                        : `${(file.size / 1048576).toFixed(0)} MB`}
                    </small>
                  </button>
                ))}
              </div>
            </div>
            <div className="confirm-actions">
              <button className="ghost" autoFocus onClick={() => setPlaying(null)}>
                取消
              </button>
            </div>
          </section>
        </div>
      )}
    </>
  );
}
