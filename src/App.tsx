import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";
import "./Frameless.css";
import "./Detail.css";
import "./Manager.css";
import "./DownloadExtras.css";
import "./RssGroups.css";
import "./Enhancements.css";
import "./Theme.css";
import { DetailDrawer } from "./components/DetailDrawer";
import { SubjectGrid, SkeletonGrid, TodaySection } from "./components/SubjectCard";
import { WindowControls } from "./components/WindowControls";
import { collectionLabels, jstToday, nav, weekdays } from "./lib";
import { DownloadsPage } from "./pages/DownloadsPage";
import { RssPage } from "./pages/RssPage";
import { SearchPage } from "./pages/SearchPage";
import { SettingsPage } from "./pages/SettingsPage";
import type {
  AppSettings,
  BangumiProfile,
  CalendarPayload,
  Collection,
  DownloadTask,
  RssFeed,
  RssItem,
  Subject,
  SyncStatus,
  View,
} from "./types";

export default function App() {
  const [view, setView] = useState<View>("today"),
    [weekday, setWeekday] = useState(jstToday()),
    [subjects, setSubjects] = useState<Subject[]>([]),
    [downloads, setDownloads] = useState<DownloadTask[]>([]),
    [feeds, setFeeds] = useState<RssFeed[]>([]),
    [rssItems, setRssItems] = useState<RssItem[]>([]),
    [selected, setSelected] = useState<Subject | null>(null),
    [collectionFilter, setCollectionFilter] = useState<Collection>("doing"),
    [rssUrl, setRssUrl] = useState(""),
    [online, setOnline] = useState(true),
    [profile, setProfile] = useState<BangumiProfile | null>(null),
    [syncStatus, setSyncStatus] = useState<SyncStatus | null>(null),
    [rssInterval, setRssInterval] = useState(15),
    [calendarBusy, setCalendarBusy] = useState(false),
    [calendarMessage, setCalendarMessage] = useState("");
  const calendarRequest = useRef(0);
  async function refreshCalendar(force = false) {
    if (!isTauri()) {
      setCalendarBusy(false);
      setCalendarMessage("浏览器预览不可用，请在桌面端运行 Mizuki");
      return;
    }
    const request = ++calendarRequest.current;
    setCalendarBusy(true);
    setCalendarMessage(calendarBusy ? "正在重新请求最新数据…" : "正在刷新今日番剧…");
    try {
      const data = await invoke<CalendarPayload>("get_calendar", { force });
      if (request !== calendarRequest.current) return;
      setSubjects(data.subjects);
      setOnline(!data.stale);
      setCalendarMessage(
        data.warning || `已更新 ${new Date(data.refreshedAt || Date.now()).toLocaleTimeString()}`,
      );
    } catch (error) {
      if (request !== calendarRequest.current) return;
      setOnline(false);
      setCalendarMessage(String(error));
    } finally {
      if (request === calendarRequest.current) setCalendarBusy(false);
    }
  }
  const refreshDownloads = useCallback(async () => {
    try {
      setDownloads(await invoke<DownloadTask[]>("list_downloads"));
    } catch {
      /* browser preview */
    }
  }, []);
  const refreshRssData = useCallback(async () => {
    try {
      const [nextFeeds, nextItems] = await Promise.all([
        invoke<RssFeed[]>("list_rss_feeds"),
        invoke<RssItem[]>("list_rss_items", { feedId: null, limit: 500 }),
      ]);
      setFeeds(nextFeeds);
      setRssItems(nextItems);
    } catch {
      /* browser preview */
    }
  }, []);
  async function addFeed() {
    if (!rssUrl.trim()) return;
    try {
      await invoke<RssFeed>("add_rss_feed", { url: rssUrl.trim() });
      setRssUrl("");
      await refreshRssData();
    } catch (e) {
      alert(String(e));
    }
  }
  async function updateCollection(subject: Subject, collection: Collection) {
    setSubjects((items) =>
      items.some((item) => item.id === subject.id)
        ? items.map((i) => (i.id === subject.id ? { ...i, collection } : i))
        : [...items, { ...subject, collection }],
    );
    setSelected((c) => (c?.id === subject.id ? { ...c, collection } : c));
    try {
      await invoke("set_collection", {
        subjectId: subject.id,
        collection,
        subject: { ...subject, collection },
      });
    } catch {
      /* 本地界面仍保留本次选择 */
    }
  }
  async function setProgress(subject: Subject, watched: number) {
    const value = Math.max(0, watched),
      collection = subject.collection ?? "doing";
    setSubjects((items) =>
      items.some((i) => i.id === subject.id)
        ? items.map((i) => (i.id === subject.id ? { ...i, watched: value, collection } : i))
        : [...items, { ...subject, watched: value, collection }],
    );
    setSelected((c) => (c?.id === subject.id ? { ...c, watched: value, collection } : c));
    try {
      await invoke("set_watch_progress", { subjectId: subject.id, watched: value });
    } catch {
      /* 本地界面仍保留本次进度 */
    }
  }
  const subscribedIds = useMemo(
    () => new Set(feeds.map((f) => f.subjectId).filter((id): id is number => typeof id === "number")),
    [feeds],
  );
  async function subscribeSubject(subject: Subject) {
    try {
      await invoke("subscribe_subject", {
        subjectId: subject.id,
        name: subject.name,
        nameCn: subject.nameCn,
      });
      await Promise.all([refreshRssData(), refreshCalendar()]);
    } catch (e) {
      alert(`订阅失败：${String(e)}`);
    }
  }
  async function unsubscribeSubject(subject: Subject) {
    if (!confirm(`取消订阅“${subject.nameCn || subject.name}”的新集自动下载？已添加的下载任务会保留。`))
      return;
    try {
      await invoke("unsubscribe_subject", { subjectId: subject.id });
      await Promise.all([refreshRssData(), refreshCalendar()]);
    } catch (e) {
      alert(String(e));
    }
  }
  async function refreshProfile() {
    try {
      setProfile(await invoke<BangumiProfile | null>("get_bangumi_profile"));
    } catch {
      setProfile(null);
    }
  }
  async function refreshSyncStatus() {
    if (!isTauri()) return;
    try {
      setSyncStatus(await invoke<SyncStatus>("get_sync_status"));
    } catch {
      /* 浏览器预览无此命令 */
    }
  }
  useEffect(() => {
    refreshCalendar();
    refreshDownloads();
    refreshProfile();
    refreshRssData();
    refreshSyncStatus();
    const syncTimer = window.setInterval(refreshSyncStatus, 20000);
    if (!isTauri()) return () => window.clearInterval(syncTimer);
    let unlisten: (() => void) | undefined,
      disposed = false;
    listen<number>("bangumi-imported", () => {
      refreshCalendar();
    }).then((off) => {
      if (disposed) off();
      else unlisten = off;
    });
    return () => {
      disposed = true;
      unlisten?.();
      window.clearInterval(syncTimer);
    };
  }, []);
  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    (async () => {
      try {
        const settings = await invoke<AppSettings>("get_settings");
        if (!cancelled) setRssInterval(Math.max(5, settings.rssIntervalMinutes));
      } catch {
        /* 浏览器预览无此命令 */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);
  useEffect(() => {
    const timer = window.setInterval(
      async () => {
        try {
          await invoke("refresh_all_rss_feeds");
        } catch {
          /* 保留成功刷新的订阅结果 */
        }
        await Promise.all([refreshRssData(), refreshDownloads()]);
      },
      rssInterval * 60 * 1000,
    );
    return () => window.clearInterval(timer);
  }, [rssInterval, refreshRssData, refreshDownloads]);
  const shownSubjects = useMemo(
    () => subjects.filter((s) => s.collection === collectionFilter),
    [subjects, collectionFilter],
  );
  const todaySubjects = useMemo(() => subjects.filter((s) => s.airWeekday === weekday), [subjects, weekday]);
  const trackedToday = useMemo(
    () => todaySubjects.filter((s) => s.collection === "doing" || s.collection === "wish"),
    [todaySubjects],
  );
  const otherToday = useMemo(
    () => todaySubjects.filter((s) => s.collection !== "doing" && s.collection !== "wish"),
    [todaySubjects],
  );
  return (
    <div className="window-root">
      <div className="drag-strip" data-tauri-drag-region />
      <WindowControls />
      <div className="app-shell">
        <aside className="sidebar">
          <div className="brand" data-tauri-drag-region>
            <img src="/icon.png" data-tauri-drag-region />
            <span data-tauri-drag-region>Mizuki</span>
          </div>
          <nav>
            {nav.map((i) => (
              <button key={i.id} className={view === i.id ? "active" : ""} onClick={() => setView(i.id)}>
                <span>{i.icon}</span>
                {i.label}
              </button>
            ))}
          </nav>
          <button className="profile-entry" onClick={() => setView("settings")} title="Bangumi 与本地资料">
            {profile?.avatar ? <img src={profile.avatar} /> : <span className="default-avatar">●</span>}
            <div>
              <b>{profile?.nickname || profile?.username || "本地资料"}</b>
              <small>
                <i className={online ? "online" : "offline"} />
                {profile
                  ? syncStatus?.pending
                    ? `${syncStatus.pending} 条待同步`
                    : "Bangumi 已连接"
                  : online
                    ? "本地模式"
                    : "当前离线"}
              </small>
            </div>
          </button>
        </aside>
        <main>
          {view === "today" && (
            <>
              <header>
                <div>
                  <p className="eyebrow">WEEKLY CALENDAR</p>
                  <h1>今天看什么？</h1>
                  <p>按放送日发现新番，优先查看你的收藏。</p>
                </div>
                <button className="ghost" aria-busy={calendarBusy} onClick={() => refreshCalendar(true)}>
                  {calendarBusy ? "↻ 重新刷新" : "↻ 刷新"}
                </button>
              </header>
              {calendarMessage && (
                <div className={`calendar-message${online ? "" : " warning"}`}>{calendarMessage}</div>
              )}
              <div className="weekday-tabs">
                {weekdays.map((d, i) => (
                  <button className={weekday === i ? "active" : ""} onClick={() => setWeekday(i)} key={d}>
                    {d}
                    <small>{subjects.filter((s) => s.airWeekday === i).length}</small>
                  </button>
                ))}
              </div>
              {calendarBusy && subjects.length === 0 ? (
                <SkeletonGrid count={8} />
              ) : (
                <>
                  {trackedToday.length > 0 && (
                    <TodaySection
                      title="我的追番"
                      subtitle="当天放送的在看与想看"
                      count={trackedToday.length}
                      featured
                    >
                      <SubjectGrid subjects={trackedToday} onSelect={setSelected} />
                    </TodaySection>
                  )}
                  {otherToday.length > 0 && (
                    <TodaySection
                      title={trackedToday.length ? "今日全部" : "今日番剧"}
                      subtitle={trackedToday.length ? "其他当天放送番剧" : "当天放送的全部番剧"}
                      count={otherToday.length}
                    >
                      <SubjectGrid subjects={otherToday} onSelect={setSelected} />
                    </TodaySection>
                  )}
                  {todaySubjects.length === 0 && <SubjectGrid subjects={[]} onSelect={setSelected} />}
                </>
              )}
            </>
          )}
          {view === "search" && <SearchPage onSelect={setSelected} />}
          {view === "library" && (
            <>
              <header>
                <div>
                  <p className="eyebrow">MY COLLECTION</p>
                  <h1>我的追番</h1>
                  <p>收藏与章节进度保存在本机，无需登录账号。</p>
                </div>
                <span className="tag">本地管理</span>
              </header>
              <div className="filter-pills">
                {(Object.keys(collectionLabels) as Collection[]).map((k) => (
                  <button
                    key={k}
                    className={collectionFilter === k ? "active" : ""}
                    onClick={() => setCollectionFilter(k)}
                  >
                    {collectionLabels[k]}
                    <span>{subjects.filter((s) => s.collection === k).length}</span>
                  </button>
                ))}
              </div>
              {calendarBusy && subjects.length === 0 ? (
                <SkeletonGrid count={8} />
              ) : (
                <SubjectGrid subjects={shownSubjects} onSelect={setSelected} />
              )}
            </>
          )}
          {view === "rss" && (
            <RssPage
              feeds={feeds}
              items={rssItems}
              rssUrl={rssUrl}
              setRssUrl={setRssUrl}
              addFeed={addFeed}
              reload={refreshRssData}
              refreshDownloads={refreshDownloads}
              onOpenDownloads={() => setView("downloads")}
            />
          )}
          {view === "downloads" && <DownloadsPage tasks={downloads} refresh={refreshDownloads} />}
          {view === "settings" && (
            <SettingsPage
              profile={profile}
              onProfile={setProfile}
              onSynced={() => refreshCalendar()}
              syncStatus={syncStatus}
              onSyncStatus={setSyncStatus}
              onSettingsSaved={(settings) => setRssInterval(Math.max(5, settings.rssIntervalMinutes))}
            />
          )}
        </main>
        {selected && (
          <DetailDrawer
            subject={selected}
            close={() => setSelected(null)}
            updateCollection={updateCollection}
            setProgress={setProgress}
            subscribed={subscribedIds.has(selected.id)}
            subscribe={subscribeSubject}
            unsubscribe={unsubscribeSubject}
          />
        )}
      </div>
    </div>
  );
}
