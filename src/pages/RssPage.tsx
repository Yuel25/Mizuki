import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { parseKeywords, releaseAnimeName } from "../lib";
import type { BatchDownloadResult, RssFeed, RssItem } from "../types";

export function RssPage({
  feeds,
  items,
  rssUrl,
  setRssUrl,
  addFeed,
  reload,
  refreshDownloads,
  onOpenDownloads,
}: {
  feeds: RssFeed[];
  items: RssItem[];
  rssUrl: string;
  setRssUrl: (v: string) => void;
  addFeed: () => void;
  reload: () => Promise<void>;
  refreshDownloads: () => Promise<void>;
  onOpenDownloads: () => void;
}) {
  const [busy, setBusy] = useState(""),
    [pendingItems, setPendingItems] = useState<Set<string>>(new Set()),
    [message, setMessage] = useState(""),
    [selectedItems, setSelectedItems] = useState<Set<string>>(new Set()),
    [filter, setFilter] = useState<"all" | "pending" | "active" | "completed">("all"),
    [resourceQuery, setResourceQuery] = useState(""),
    [ruleDraft, setRuleDraft] = useState<{
      feedId: string;
      includes: string;
      excludes: string;
      resolution: string;
      subtitleGroup: string;
      autoDownload: boolean;
    } | null>(null);
  const visibleItems = useMemo(
    () =>
      items.filter((item) => {
        const matches =
          !resourceQuery.trim() ||
          item.title.toLocaleLowerCase().includes(resourceQuery.trim().toLocaleLowerCase());
        if (!matches) return false;
        const stalled =
          !!item.download &&
          !item.download.active &&
          ["queued", "downloading", "paused"].includes(item.download.state);
        if (filter === "pending") return !item.download || item.download.state === "failed" || stalled;
        if (filter === "active")
          return !!item.download?.active && ["queued", "downloading", "paused"].includes(item.download.state);
        if (filter === "completed") return item.download?.state === "completed";
        return true;
      }),
    [items, filter, resourceQuery],
  );
  const selectableVisibleGuids = useMemo(
    () =>
      new Set(
        visibleItems
          .filter(
            (item) =>
              !item.download ||
              item.download.state === "failed" ||
              (!item.download.active && ["queued", "downloading", "paused"].includes(item.download.state)),
          )
          .map((item) => item.guid),
      ),
    [visibleItems],
  );
  const selectedVisibleCount = useMemo(
    () => [...selectedItems].filter((guid) => selectableVisibleGuids.has(guid)).length,
    [selectedItems, selectableVisibleGuids],
  );
  const groups = useMemo(() => {
    const result = new Map<string, RssItem[]>();
    visibleItems.forEach((item) => {
      const name = releaseAnimeName(item.title);
      result.set(name, [...(result.get(name) || []), item]);
    });
    return [...result].sort((a, b) => a[0].localeCompare(b[0], "zh-CN"));
  }, [visibleItems]);
  useEffect(() => {
    setSelectedItems((current) => {
      const next = new Set([...current].filter((guid) => selectableVisibleGuids.has(guid)));
      return next.size === current.size ? current : next;
    });
  }, [selectableVisibleGuids]);
  useEffect(() => {
    const timer = window.setInterval(async () => {
      await refreshDownloads();
      await reload();
    }, 3000);
    return () => window.clearInterval(timer);
  }, [reload, refreshDownloads]);
  async function run(label: string, action: () => Promise<unknown>) {
    setBusy(label);
    setMessage("");
    try {
      await action();
      await reload();
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy("");
    }
  }
  async function refreshAll() {
    setBusy("all");
    setMessage("");
    try {
      const count = await invoke<number>("refresh_all_rss_feeds");
      setMessage(`刷新完成，发现 ${count} 条新资源`);
      await reload();
      await refreshDownloads();
    } catch (error) {
      setMessage(String(error));
      await reload();
    } finally {
      setBusy("");
    }
  }
  async function downloadOne(item: RssItem) {
    setPendingItems((current) => new Set(current).add(item.guid));
    setMessage("");
    try {
      await invoke("download_rss_item", { guid: item.guid });
      await Promise.all([refreshDownloads(), reload()]);
    } catch (error) {
      setMessage(String(error));
      await reload();
    } finally {
      setPendingItems((current) => {
        const next = new Set(current);
        next.delete(item.guid);
        return next;
      });
    }
  }
  function toggle(guid: string) {
    setSelectedItems((current) => {
      const next = new Set(current);
      next.has(guid) ? next.delete(guid) : next.add(guid);
      return next;
    });
  }
  async function batchDownload() {
    const guids = [...selectedItems].filter((guid) => selectableVisibleGuids.has(guid));
    if (!guids.length) return;
    setBusy("selected");
    setMessage("");
    try {
      const result = await invoke<BatchDownloadResult>("download_rss_items", { guids });
      setMessage(
        `新增 ${result.added} 个任务${result.reused ? `，复用 ${result.reused} 个已有任务` : ""}${result.failed ? `，失败 ${result.failed} 个：${result.errors.join("；")}` : ""}`,
      );
      setSelectedItems(new Set());
      await refreshDownloads();
      await reload();
    } catch (error) {
      setMessage(String(error));
      await reload();
    } finally {
      setBusy("");
    }
  }
  function selectGroup(group: RssItem[]) {
    setSelectedItems((current) => {
      const next = new Set(current);
      group
        .filter(
          (item) =>
            !item.download ||
            item.download.state === "failed" ||
            (!item.download.active && ["queued", "downloading", "paused"].includes(item.download.state)),
        )
        .forEach((item) => next.add(item.guid));
      return next;
    });
  }
  function openRules(feed: RssFeed) {
    setRuleDraft({
      feedId: feed.id,
      includes: feed.rule.includes.join("，"),
      excludes: feed.rule.excludes.join("，"),
      resolution: feed.rule.resolution ?? "",
      subtitleGroup: feed.rule.subtitleGroup ?? "",
      autoDownload: feed.rule.autoDownload,
    });
  }
  async function saveRules() {
    if (!ruleDraft) return;
    const draft = ruleDraft;
    setBusy("rules");
    setMessage("");
    try {
      await invoke("set_rss_feed_rules", {
        id: draft.feedId,
        rule: {
          includes: parseKeywords(draft.includes),
          excludes: parseKeywords(draft.excludes),
          resolution: draft.resolution.trim() || null,
          subtitleGroup: draft.subtitleGroup.trim() || null,
          autoDownload: draft.autoDownload,
        },
      });
      setRuleDraft(null);
      await reload();
    } catch (error) {
      setMessage(String(error));
    } finally {
      setBusy("");
    }
  }
  const stateLabel = (item: RssItem) =>
    item.download?.state === "completed"
      ? "已下载"
      : item.download &&
          !item.download.active &&
          ["queued", "downloading", "paused"].includes(item.download.state)
        ? "连接已中断"
        : item.download?.state === "downloading"
          ? `下载中 ${Math.round(item.download.progress * 100)}%`
          : item.download?.state === "paused"
            ? "已暂停"
            : item.download?.state === "queued"
              ? "下载中"
              : item.download?.state === "failed"
                ? "下载失败"
                : "待下载";
  const actionLabel = (item: RssItem) =>
    item.download?.state === "completed"
      ? "已下载"
      : item.download?.active && ["queued", "downloading"].includes(item.download.state)
        ? "下载中"
        : item.download &&
            !item.download.active &&
            ["queued", "downloading", "paused"].includes(item.download.state)
          ? "重新连接"
          : item.download?.state === "failed"
            ? "重试下载"
            : "下载";
  return (
    <>
      <header>
        <div>
          <p className="eyebrow">MIKAN RSS + DOWNLOADS</p>
          <h1>订阅资源</h1>
          <p>按番剧整理新资源，手动选择并跟踪真实下载状态。</p>
        </div>
        <div className="header-actions">
          <button className="ghost" onClick={onOpenDownloads}>
            下载管理 ↗
          </button>
          <button className="ghost" disabled={!!busy} onClick={refreshAll}>
            {busy === "all" ? "刷新中…" : "↻ 全部刷新"}
          </button>
        </div>
      </header>
      <div className="rss-add">
        <div>
          <label>添加 Mikan RSS 地址</label>
          <div>
            <input
              value={rssUrl}
              onChange={(e) => setRssUrl(e.target.value)}
              placeholder="https://mikanani.me/RSS/..."
              onKeyDown={(event) => event.key === "Enter" && addFeed()}
            />
            <button className="primary" onClick={addFeed}>
              验证并添加
            </button>
          </div>
          <small>每 15 分钟只刷新资源列表，不会自动下载。</small>
        </div>
      </div>
      {message && <div className="manager-message">{message}</div>}
      <div className="section-title manager-heading">
        <h2>
          订阅源 <small>{feeds.length}</small>
        </h2>
      </div>
      <div className="feed-list">
        {feeds.map((f) => (
          <div className="feed-block" key={f.id}>
            <div className={`feed-row${f.enabled ? "" : " disabled"}`}>
              <span className="feed-icon">◒</span>
              <div>
                <h3>{f.title}</h3>
                <p>{f.url}</p>
                <small>
                  {f.lastCheckedAt ? new Date(f.lastCheckedAt).toLocaleString() : "尚未刷新"}
                  {f.subjectId ? " · 追番订阅" : ""}
                  {f.rule.autoDownload && " · 自动下载已开启"}
                  {f.rule.includes.length ||
                  f.rule.excludes.length ||
                  f.rule.resolution ||
                  f.rule.subtitleGroup
                    ? " · 已配置规则"
                    : ""}
                </small>
              </div>
              <div className="feed-actions">
                <button disabled={!!busy} onClick={() => openRules(f)}>
                  {ruleDraft?.feedId === f.id ? "收起规则" : "规则"}
                </button>
                <button
                  disabled={!!busy}
                  onClick={() => run(`refresh-${f.id}`, () => invoke("refresh_rss_feed", { id: f.id }))}
                >
                  {busy === `refresh-${f.id}` ? "刷新中…" : "刷新"}
                </button>
                <button
                  onClick={() =>
                    run(`enable-${f.id}`, () =>
                      invoke("set_rss_feed_enabled", { id: f.id, enabled: !f.enabled }),
                    )
                  }
                >
                  {f.enabled ? "停用" : "启用"}
                </button>
                <button
                  className="danger"
                  onClick={() =>
                    confirm(`删除订阅“${f.title}”？`) &&
                    run(`delete-${f.id}`, () => invoke("delete_rss_feed", { id: f.id }))
                  }
                >
                  删除
                </button>
              </div>
            </div>
            {ruleDraft?.feedId === f.id && (
              <div className="rule-editor">
                <div className="rule-fields">
                  <label>
                    必须包含（逗号分隔）
                    <input
                      value={ruleDraft.includes}
                      onChange={(e) => setRuleDraft({ ...ruleDraft, includes: e.target.value })}
                      placeholder="简中，简体"
                    />
                  </label>
                  <label>
                    必须排除（逗号分隔）
                    <input
                      value={ruleDraft.excludes}
                      onChange={(e) => setRuleDraft({ ...ruleDraft, excludes: e.target.value })}
                      placeholder="720p，预告"
                    />
                  </label>
                  <label>
                    分辨率
                    <input
                      value={ruleDraft.resolution}
                      onChange={(e) => setRuleDraft({ ...ruleDraft, resolution: e.target.value })}
                      placeholder="1080p"
                    />
                  </label>
                  <label>
                    字幕组
                    <input
                      value={ruleDraft.subtitleGroup}
                      onChange={(e) => setRuleDraft({ ...ruleDraft, subtitleGroup: e.target.value })}
                      placeholder="喵萌奶茶屋"
                    />
                  </label>
                </div>
                <div className="rule-footer">
                  <label className="rule-toggle">
                    <input
                      type="checkbox"
                      checked={ruleDraft.autoDownload}
                      onChange={(e) => setRuleDraft({ ...ruleDraft, autoDownload: e.target.checked })}
                    />
                    自动下载匹配的新资源
                  </label>
                  <div>
                    <button className="ghost" onClick={() => setRuleDraft(null)}>
                      取消
                    </button>
                    <button className="primary" disabled={busy === "rules"} onClick={saveRules}>
                      {busy === "rules" ? "保存中…" : "保存规则"}
                    </button>
                  </div>
                </div>
                <small>规则在刷新发现新资源时生效：标记“符合规则”并按需自动下载；条件留空表示不限制。</small>
              </div>
            )}
          </div>
        ))}
      </div>
      <div className="rss-tools">
        <input
          value={resourceQuery}
          onChange={(event) => setResourceQuery(event.target.value)}
          placeholder="筛选番剧或资源名称"
        />
        <div>
          {(["all", "pending", "active", "completed"] as const).map((value) => (
            <button className={filter === value ? "active" : ""} onClick={() => setFilter(value)} key={value}>
              {value === "all"
                ? "全部"
                : value === "pending"
                  ? "待下载"
                  : value === "active"
                    ? "进行中"
                    : "已完成"}
            </button>
          ))}
        </div>
      </div>
      <div className="section-title manager-heading rss-resource-heading">
        <h2>
          按番剧分类{" "}
          <small>
            {groups.length} 部 · {visibleItems.length}/{items.length} 个资源
          </small>
        </h2>
        <button className="primary" disabled={!selectedVisibleCount || !!busy} onClick={batchDownload}>
          {busy === "selected" ? "正在添加…" : `下载所选 (${selectedVisibleCount})`}
        </button>
      </div>
      <div className="rss-groups">
        {groups.map(([name, group]) => (
          <details className="rss-group" key={name} open>
            <summary>
              <span>{name}</span>
              <small>{group.length} 个资源</small>
            </summary>
            <div className="rss-group-actions">
              <button onClick={() => selectGroup(group)}>选择待下载</button>
            </div>
            <div className="rss-item-list">
              {group.map((item) => {
                const retryable =
                  !item.download ||
                  item.download.state === "failed" ||
                  (!item.download.active &&
                    ["queued", "downloading", "paused"].includes(item.download.state));
                const blocked = !retryable;
                const itemBusy = pendingItems.has(item.guid);
                return (
                  <article className={item.download ? `has-task ${item.download.state}` : ""} key={item.guid}>
                    <input
                      type="checkbox"
                      checked={selectedItems.has(item.guid)}
                      disabled={blocked || itemBusy}
                      onChange={() => toggle(item.guid)}
                    />
                    <div className="rss-item-main">
                      <h3>{item.title}</h3>
                      <p>
                        {feeds.find((feed) => feed.id === item.feedId)?.title || "RSS"} ·{" "}
                        {item.publishedAt ? new Date(item.publishedAt).toLocaleString() : "时间未知"}
                        {item.matchesRule && <i className="rule-hit">符合规则</i>}
                      </p>
                    </div>
                    <span className={`rss-download-state ${item.download?.state || "pending"}`}>
                      {itemBusy ? "下载中" : stateLabel(item)}
                    </span>
                    <button className="ghost" onClick={() => openUrl(item.link)}>
                      来源 ↗
                    </button>
                    <button
                      className={blocked ? "ghost" : "primary"}
                      disabled={blocked || itemBusy || busy === "all" || busy === "selected"}
                      onClick={() => downloadOne(item)}
                    >
                      {itemBusy ? "下载中" : actionLabel(item)}
                    </button>
                  </article>
                );
              })}
            </div>
          </details>
        ))}
      </div>
      {!groups.length && (
        <div className="empty compact">
          <span>◒</span>
          <h3>没有匹配的资源</h3>
          <p>刷新订阅或调整筛选条件。</p>
        </div>
      )}
    </>
  );
}
