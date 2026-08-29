// 前后端共享的数据形状（与 Rust 模型 camelCase 序列化一一对应）。

export type View = "today" | "search" | "library" | "rss" | "downloads" | "settings";
export type Collection = "wish" | "doing" | "collect" | "on_hold" | "dropped";
export type Theme = "dark" | "light";

export interface Subject {
  id: number;
  name: string;
  nameCn: string;
  summary: string;
  image: string | null;
  score: number;
  rank: number | null;
  airWeekday: number;
  collection: Collection | null;
  episodes: number;
  watched: number;
  updateState: "none" | "published" | "downloading" | "completed";
}

export interface DownloadTask {
  id: string;
  title: string;
  episode: string;
  progress: number;
  downSpeed: number;
  upSpeed: number;
  state: "queued" | "downloading" | "paused" | "completed" | "failed";
  outputPath: string;
  playbackPath?: string | null;
}

export interface RssFeed {
  id: string;
  title: string;
  url: string;
  enabled: boolean;
  lastCheckedAt: string | null;
  rule: {
    includes: string[];
    excludes: string[];
    resolution: string | null;
    subtitleGroup: string | null;
    autoDownload: boolean;
  };
  subjectId?: number | null;
}

export interface RssItem {
  guid: string;
  feedId: string;
  title: string;
  link: string;
  torrent: string | null;
  publishedAt: string | null;
  downloaded: boolean;
  download: { taskId: string; state: DownloadTask["state"]; progress: number; active: boolean } | null;
  matchesRule: boolean;
}

export interface BangumiProfile {
  username: string;
  nickname: string;
  avatar: string | null;
}

export interface SyncStatus {
  pending: number;
  lastError: string | null;
  lastAttemptAt: string | null;
}

export interface SubjectDetail {
  summary?: string;
  total_episodes?: number;
  eps?: number;
  rating?: { score?: number; rank?: number };
  images?: { large?: string; common?: string };
}

export interface BangumiComment {
  id: number;
  comment: string;
  rate?: number;
  spoiler?: boolean;
  user?: { username?: string; nickname?: string; avatar?: { small?: string; medium?: string } };
}

export interface CommentPage {
  data?: BangumiComment[];
  total?: number;
}

export interface CalendarPayload {
  subjects: Subject[];
  refreshedAt: string | null;
  stale: boolean;
  warning: string | null;
}

export interface SearchPayload {
  subjects: Subject[];
  localOnly: boolean;
  warning: string | null;
}

export interface BatchDownloadResult {
  added: number;
  reused: number;
  failed: number;
  errors: string[];
}

export interface AppSettings {
  btListenPort: number;
  btUploadKbps: number;
  btDownloadKbps: number;
  btPeerLimit: number;
  maxConcurrentDownloads: number;
  stopSeedingOnComplete: boolean;
  rssIntervalMinutes: number;
  downloadDir: string | null;
  autostart: boolean;
  closeToTray: boolean;
}

export interface PlaybackFile {
  index: number;
  name: string;
  size: number;
  path: string;
}
