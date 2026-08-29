use crate::models::{DownloadTask, RssDownloadStatus, RssFeed, RssItem, SubjectRssState};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::Path, sync::Mutex};

/// 版本化迁移：MIGRATIONS[n] 把 user_version 从 n 提升到 n+1。
/// 旧库的列探测（pragma_table_info）保留为 v0 基线，新改动一律走这里。
const MIGRATIONS: &[&str] = &[
    // v1：订阅追番——rss_feeds 关联 Bangumi 条目 ID。
    "ALTER TABLE rss_feeds ADD COLUMN subject_id INTEGER",
    // v2：条目详情缓存——消除周表卡片的 N+1 详情请求，离线时也可回退。
    "CREATE TABLE IF NOT EXISTS subject_details(subject_id INTEGER PRIMARY KEY,payload TEXT NOT NULL,updated_at TEXT NOT NULL)",
];

fn apply_migrations(connection: &Connection) -> Result<(), String> {
    let current: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if current < version {
            // DDL 与版本号必须在一个事务内原子生效：
            // 否则中途崩溃重开时会重放已执行的 ALTER 并报重复列，数据库无法打开。
            connection
                .execute_batch(&format!(
                    "BEGIN; {migration}; PRAGMA user_version={version}; COMMIT;"
                ))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub fn canonical_download_source_key(source: &str) -> String {
    let source = source.trim();
    if source
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("magnet:"))
        && let Ok(url) = url::Url::parse(source)
        && let Some((_, hash)) = url
            .query_pairs()
            .find(|(key, _)| key.eq_ignore_ascii_case("xt"))
    {
        return hash.to_ascii_lowercase();
    }
    // HTTP path/query can be case-sensitive (including signed token values).
    source.to_owned()
}

pub struct Database(pub Mutex<Connection>);

impl Database {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let connection = Connection::open(path).map_err(|e| e.to_string())?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        Self::initialize(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn initialize(connection: Connection) -> Result<Self, String> {
        connection.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS rss_feeds(id TEXT PRIMARY KEY,title TEXT NOT NULL,url TEXT NOT NULL UNIQUE,enabled INTEGER NOT NULL DEFAULT 1,last_checked_at TEXT,auto_download INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS rss_items(guid TEXT PRIMARY KEY,feed_id TEXT NOT NULL,title TEXT NOT NULL,link TEXT NOT NULL,torrent TEXT,published_at TEXT,seen_at TEXT NOT NULL,downloaded INTEGER NOT NULL DEFAULT 0,download_id TEXT);
            CREATE TABLE IF NOT EXISTS downloads(id TEXT PRIMARY KEY,title TEXT NOT NULL,episode TEXT NOT NULL,source TEXT NOT NULL,source_key TEXT,progress REAL NOT NULL DEFAULT 0,down_speed INTEGER NOT NULL DEFAULT 0,up_speed INTEGER NOT NULL DEFAULT 0,state TEXT NOT NULL,output_path TEXT NOT NULL,created_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS local_collections(subject_id INTEGER PRIMARY KEY,collection TEXT NOT NULL,watched INTEGER NOT NULL DEFAULT 0,updated_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS cached_subjects(subject_id INTEGER PRIMARY KEY,payload TEXT NOT NULL,updated_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sync_queue(id INTEGER PRIMARY KEY AUTOINCREMENT,subject_id INTEGER NOT NULL UNIQUE,collection TEXT NOT NULL,ep INTEGER,attempts INTEGER NOT NULL DEFAULT 0,next_attempt_at TEXT NOT NULL,last_error TEXT,updated_at TEXT NOT NULL);")
            .map_err(|e| e.to_string())?;
        let has_rss_download: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('rss_items') WHERE name='download_id'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if has_rss_download == 0 {
            connection
                .execute("ALTER TABLE rss_items ADD COLUMN download_id TEXT", [])
                .map_err(|e| e.to_string())?;
        }
        let has_source_key: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('downloads') WHERE name='source_key'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if has_source_key == 0 {
            connection
                .execute("ALTER TABLE downloads ADD COLUMN source_key TEXT", [])
                .map_err(|e| e.to_string())?;
        }
        let has_feed_rule: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('rss_feeds') WHERE name='rule_json'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if has_feed_rule == 0 {
            connection
                .execute("ALTER TABLE rss_feeds ADD COLUMN rule_json TEXT", [])
                .map_err(|e| e.to_string())?;
        }
        let has_playback_path: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('downloads') WHERE name='playback_path'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if has_playback_path == 0 {
            connection
                .execute("ALTER TABLE downloads ADD COLUMN playback_path TEXT", [])
                .map_err(|e| e.to_string())?;
        }
        // 旧版本保存的是完整磁力链接。统一迁移为 info-hash，避免升级后重复创建任务。
        let download_sources = {
            let mut query = connection
                .prepare("SELECT id,source FROM downloads")
                .map_err(|e| e.to_string())?;
            query
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        for (id, source) in download_sources {
            connection
                .execute(
                    "UPDATE downloads SET source_key=?2 WHERE id=?1",
                    params![id, canonical_download_source_key(&source)],
                )
                .map_err(|e| e.to_string())?;
        }
        connection
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_downloads_source_key ON downloads(source_key);
                 CREATE INDEX IF NOT EXISTS idx_rss_items_feed_seen ON rss_items(feed_id,seen_at DESC);",
            )
            .map_err(|e| e.to_string())?;
        connection.execute("UPDATE rss_items SET download_id=(SELECT id FROM downloads WHERE source=COALESCE(rss_items.torrent,rss_items.link) ORDER BY created_at DESC LIMIT 1) WHERE downloaded=1 AND download_id IS NULL",[]).map_err(|e|e.to_string())?;
        // 旧版只写 downloaded=1、却没有真实任务的记录必须恢复为可下载状态。
        connection
            .execute(
                "UPDATE rss_items SET downloaded=0 WHERE download_id IS NULL",
                [],
            )
            .map_err(|e| e.to_string())?;
        // 自动下载已废弃：升级旧数据库时也统一关闭，订阅刷新只发现资源。
        connection
            .execute(
                "UPDATE rss_feeds SET auto_download=0 WHERE auto_download<>0",
                [],
            )
            .map_err(|e| e.to_string())?;
        apply_migrations(&connection)?;
        Ok(Self(Mutex::new(connection)))
    }

    pub fn add_feed(&self, feed: &RssFeed) -> Result<(), String> {
        let rule = serde_json::to_string(&feed.rule).map_err(|e| e.to_string())?;
        self.0.lock().map_err(|e| e.to_string())?.execute("INSERT INTO rss_feeds(id,title,url,enabled,last_checked_at,rule_json,subject_id) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![feed.id,feed.title,feed.url,feed.enabled,feed.last_checked_at,rule,feed.subject_id]).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn list_feeds(&self) -> Result<Vec<RssFeed>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query = connection.prepare("SELECT id,title,url,enabled,last_checked_at,rule_json,subject_id FROM rss_feeds ORDER BY title").map_err(|e| e.to_string())?;
        query
            .query_map([], |row| {
                let rule: Option<String> = row.get(5)?;
                Ok(RssFeed {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                    enabled: row.get(3)?,
                    last_checked_at: row.get(4)?,
                    rule: rule
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or_default(),
                    subject_id: row.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
    /// 某个 Bangumi 条目当前的追番订阅（没有则为 None）。
    pub fn feed_for_subject(&self, subject_id: i64) -> Result<Option<RssFeed>, String> {
        Ok(self
            .list_feeds()?
            .into_iter()
            .find(|feed| feed.subject_id == Some(subject_id)))
    }
    /// 按 URL 查找订阅源：用户可能早已手动添加过同一 Mikan 单番地址。
    pub fn feed_by_url(&self, url: &str) -> Result<Option<RssFeed>, String> {
        Ok(self
            .list_feeds()?
            .into_iter()
            .find(|feed| feed.url == url))
    }
    /// 把已有订阅源收编为某条目的追番订阅：关联 subject_id 并确保启用。
    pub fn adopt_feed_as_subscription(&self, id: &str, subject_id: i64) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE rss_feeds SET subject_id=?2,enabled=1 WHERE id=?1",
                params![id, subject_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn feed(&self, id: &str) -> Result<RssFeed, String> {
        self.list_feeds()?
            .into_iter()
            .find(|feed| feed.id == id)
            .ok_or("订阅源不存在".into())
    }
    pub fn update_feed(&self, id: &str, title: &str, checked_at: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE rss_feeds SET title=?2,last_checked_at=?3 WHERE id=?1",
                params![id, title, checked_at],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn set_feed_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE rss_feeds SET enabled=?2 WHERE id=?1",
                params![id, enabled],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn update_feed_rule(&self, id: &str, rule: &crate::models::FeedRule) -> Result<(), String> {
        let rule = serde_json::to_string(rule).map_err(|e| e.to_string())?;
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE rss_feeds SET rule_json=?2 WHERE id=?1",
                params![id, rule],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn delete_feed(&self, id: &str) -> Result<(), String> {
        let mut connection = self.0.lock().map_err(|e| e.to_string())?;
        let tx = connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM rss_items WHERE feed_id=?1", [id])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM rss_feeds WHERE id=?1", [id])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }
    /// 取消追番订阅：删除该番剧关联的订阅源与资源列表；已产生的下载任务保留。
    pub fn delete_feeds_for_subject(&self, subject_id: i64) -> Result<(), String> {
        let mut connection = self.0.lock().map_err(|e| e.to_string())?;
        let tx = connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM rss_items WHERE feed_id IN (SELECT id FROM rss_feeds WHERE subject_id=?1)",
            [subject_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM rss_feeds WHERE subject_id=?1",
            [subject_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }
    /// 按番剧聚合订阅源的资源状态：正在下载 / 已下载 / 有未下载的匹配新资源。
    /// 暂停与失败任务不计入徽章（等待用户在下载页或 RSS 页处理）。
    pub fn subject_rss_overview(&self) -> Result<HashMap<i64, SubjectRssState>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut feed_query = connection
            .prepare("SELECT id,subject_id,rule_json FROM rss_feeds WHERE subject_id IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let feeds: HashMap<String, (i64, crate::models::FeedRule)> = feed_query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(id, subject_id, rule)| {
                let rule = rule
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_default();
                (id, (subject_id, rule))
            })
            .collect();
        if feeds.is_empty() {
            return Ok(HashMap::new());
        }
        let mut item_query = connection
            .prepare(
                "SELECT r.feed_id,r.title,r.downloaded,d.state FROM rss_items r
                 LEFT JOIN downloads d ON d.id=r.download_id
                 WHERE r.feed_id IN (SELECT id FROM rss_feeds WHERE subject_id IS NOT NULL)",
            )
            .map_err(|e| e.to_string())?;
        let items = item_query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let mut overview: HashMap<i64, SubjectRssState> = feeds
            .values()
            .map(|(subject_id, _)| (*subject_id, SubjectRssState::default()))
            .collect();
        for (feed_id, title, downloaded, task_state) in items {
            let Some((subject_id, rule)) = feeds.get(&feed_id) else {
                continue;
            };
            let entry = overview.entry(*subject_id).or_default();
            match task_state.as_deref() {
                Some("completed") => entry.completed = true,
                Some("queued" | "downloading") => entry.downloading = true,
                Some(_) => {} // 已暂停/失败：不计入徽章
                None => {
                    let matches = crate::matcher::resource_matches(&title, &rule.into());
                    if downloaded == 0 && matches {
                        entry.pending = true;
                    }
                }
            }
        }
        Ok(overview)
    }

    pub fn insert_rss_items(
        &self,
        feed_id: &str,
        items: &[RssItem],
    ) -> Result<Vec<RssItem>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut inserted = Vec::new();
        for item in items {
            let changed=connection.execute("INSERT OR IGNORE INTO rss_items(guid,feed_id,title,link,torrent,published_at,seen_at,downloaded) VALUES(?1,?2,?3,?4,?5,?6,?7,0)",params![item.guid,feed_id,item.title,item.link,item.torrent,item.published_at,chrono::Utc::now().to_rfc3339()]).map_err(|e|e.to_string())?;
            if changed > 0 {
                let mut value = item.clone();
                value.feed_id = feed_id.into();
                inserted.push(value)
            }
        }
        Ok(inserted)
    }
    pub fn list_rss_items(
        &self,
        feed_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<RssItem>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let sql = if feed_id.is_some() {
            "SELECT r.guid,r.feed_id,r.title,r.link,r.torrent,r.published_at,d.id,d.state,d.progress FROM rss_items r LEFT JOIN downloads d ON d.id=r.download_id WHERE r.feed_id=?1 ORDER BY r.seen_at DESC LIMIT ?2"
        } else {
            "SELECT r.guid,r.feed_id,r.title,r.link,r.torrent,r.published_at,d.id,d.state,d.progress FROM rss_items r LEFT JOIN downloads d ON d.id=r.download_id ORDER BY r.seen_at DESC LIMIT ?2"
        };
        let mut query = connection.prepare(sql).map_err(|e| e.to_string())?;
        let map = |row: &rusqlite::Row<'_>| {
            let task_id: Option<String> = row.get(6)?;
            let task_state: Option<String> = row.get(7)?;
            let task_progress: Option<f64> = row.get(8)?;
            let download = task_id.map(|task_id| RssDownloadStatus {
                task_id,
                state: task_state.unwrap_or_else(|| "queued".into()),
                progress: task_progress.unwrap_or_default(),
                active: false,
            });
            Ok(RssItem {
                guid: row.get(0)?,
                feed_id: row.get(1)?,
                title: row.get(2)?,
                link: row.get(3)?,
                torrent: row.get(4)?,
                published_at: row.get(5)?,
                downloaded: download.is_some(),
                download,
                matches_rule: false,
            })
        };
        if let Some(id) = feed_id {
            query
                .query_map(params![id, limit], map)
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())
        } else {
            query
                .query_map(params!["", limit], map)
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())
        }
    }
    pub fn rss_item(&self, guid: &str) -> Result<RssItem, String> {
        self.list_rss_items(None, 5000)?
            .into_iter()
            .find(|item| item.guid == guid)
            .ok_or("RSS 条目不存在".into())
    }
    pub fn mark_rss_downloaded(&self, guid: &str, task_id: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE rss_items SET downloaded=1,download_id=?2 WHERE guid=?1",
                params![guid, task_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn reset_rss_download_for_task(&self, task_id: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE rss_items SET downloaded=0,download_id=NULL WHERE download_id=?1",
                [task_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_downloads(&self) -> Result<Vec<DownloadTask>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query=connection.prepare("SELECT id,title,episode,progress,down_speed,up_speed,state,output_path,playback_path FROM downloads ORDER BY created_at DESC").map_err(|e|e.to_string())?;
        query
            .query_map([], |row| {
                Ok(DownloadTask {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    episode: row.get(2)?,
                    progress: row.get(3)?,
                    down_speed: row.get(4)?,
                    up_speed: row.get(5)?,
                    state: row.get(6)?,
                    output_path: row.get(7)?,
                    playback_path: row.get(8)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
    pub fn add_download(
        &self,
        task: &DownloadTask,
        source: &str,
        source_key: &str,
    ) -> Result<(), String> {
        self.0.lock().map_err(|e|e.to_string())?.execute("INSERT INTO downloads(id,title,episode,source,source_key,progress,down_speed,up_speed,state,output_path,created_at) VALUES(?1,?2,?3,?4,?5,0,0,0,?6,?7,?8)",params![task.id,task.title,task.episode,source,source_key,task.state,task.output_path,chrono::Utc::now().to_rfc3339()]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn download_by_source_key(&self, source_key: &str) -> Result<Option<DownloadTask>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query=connection.prepare("SELECT id,title,episode,progress,down_speed,up_speed,state,output_path,playback_path FROM downloads WHERE source_key=?1 AND state<>'failed' ORDER BY created_at DESC LIMIT 1").map_err(|e|e.to_string())?;
        match query.query_row([source_key], |row| {
            Ok(DownloadTask {
                id: row.get(0)?,
                title: row.get(1)?,
                episode: row.get(2)?,
                progress: row.get(3)?,
                down_speed: row.get(4)?,
                up_speed: row.get(5)?,
                state: row.get(6)?,
                output_path: row.get(7)?,
                playback_path: row.get(8)?,
            })
        }) {
            Ok(task) => Ok(Some(task)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    pub fn update_download(&self, task: &DownloadTask) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE downloads SET progress=?2,down_speed=?3,up_speed=?4,state=?5,playback_path=?6 WHERE id=?1",
                params![
                    task.id,
                    task.progress,
                    task.down_speed,
                    task.up_speed,
                    task.state,
                    task.playback_path
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn set_download_state(&self, id: &str, state: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE downloads SET state=?2 WHERE id=?1",
                params![id, state],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn delete_download(&self, id: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute("DELETE FROM downloads WHERE id=?1", [id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn set_playback_path(&self, id: &str, path: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE downloads SET playback_path=?2 WHERE id=?1",
                params![id, path],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// (source, output_path, playback_path)：播放回退恢复会话时使用。
    pub fn download_by_id(&self, id: &str) -> Result<Option<(String, String, Option<String>)>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        match connection.query_row(
            "SELECT source,output_path,playback_path FROM downloads WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        ) {
            Ok(found) => Ok(Some(found)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    /// (id, source, state, output_path)：重启恢复下载会话时使用。
    pub fn restorable_downloads(&self) -> Result<Vec<(String, String, String, String)>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query=connection.prepare("SELECT id,source,state,output_path FROM downloads WHERE state IN ('queued','downloading','paused')").map_err(|e|e.to_string())?;
        query
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn set_collection(&self, subject_id: i64, collection: &str) -> Result<(), String> {
        self.0.lock().map_err(|e|e.to_string())?.execute("INSERT INTO local_collections(subject_id,collection,updated_at) VALUES(?1,?2,?3) ON CONFLICT(subject_id) DO UPDATE SET collection=excluded.collection,updated_at=excluded.updated_at",params![subject_id,collection,chrono::Utc::now().to_rfc3339()]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn set_collection_progress(
        &self,
        subject_id: i64,
        collection: &str,
        watched: i64,
    ) -> Result<(), String> {
        self.0.lock().map_err(|e|e.to_string())?.execute("INSERT INTO local_collections(subject_id,collection,watched,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(subject_id) DO UPDATE SET collection=excluded.collection,watched=excluded.watched,updated_at=excluded.updated_at",params![subject_id,collection,watched,chrono::Utc::now().to_rfc3339()]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn collection_state(&self, subject_id: i64) -> Result<Option<(String, i64)>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        match connection.query_row(
            "SELECT collection,watched FROM local_collections WHERE subject_id=?1",
            [subject_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        ) {
            Ok(state) => Ok(Some(state)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    pub fn collections(&self) -> Result<HashMap<i64, (String, i64)>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query = connection
            .prepare("SELECT subject_id,collection,watched FROM local_collections")
            .map_err(|e| e.to_string())?;
        query
            .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))
            .map_err(|e| e.to_string())?
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(|e| e.to_string())
    }
    pub fn cache_subject(
        &self,
        subject_id: i64,
        payload: &serde_json::Value,
    ) -> Result<(), String> {
        self.0.lock().map_err(|e|e.to_string())?.execute("INSERT INTO cached_subjects(subject_id,payload,updated_at) VALUES(?1,?2,?3) ON CONFLICT(subject_id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at",params![subject_id,payload.to_string(),chrono::Utc::now().to_rfc3339()]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn cached_subjects(&self) -> Result<Vec<serde_json::Value>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query = connection
            .prepare("SELECT payload FROM cached_subjects")
            .map_err(|e| e.to_string())?;
        query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .map(|row| {
                row.map_err(|e| e.to_string())
                    .and_then(|value| serde_json::from_str(&value).map_err(|e| e.to_string()))
            })
            .collect()
    }

    /// 条目详情缓存：(Bangumi v0 详情 JSON, 更新时间)。
    pub fn cached_detail(
        &self,
        subject_id: i64,
    ) -> Result<Option<(serde_json::Value, String)>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        match connection.query_row(
            "SELECT payload,updated_at FROM subject_details WHERE subject_id=?1",
            [subject_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ) {
            Ok((payload, updated_at)) => Ok(Some((
                serde_json::from_str(&payload).map_err(|e| e.to_string())?,
                updated_at,
            ))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    pub fn save_detail(&self, subject_id: i64, payload: &serde_json::Value) -> Result<(), String> {
        self.0.lock().map_err(|e|e.to_string())?.execute("INSERT INTO subject_details(subject_id,payload,updated_at) VALUES(?1,?2,?3) ON CONFLICT(subject_id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at",params![subject_id,payload.to_string(),chrono::Utc::now().to_rfc3339()]).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn save_calendar(&self, subjects: &[crate::models::Subject]) -> Result<(), String> {
        let payload = serde_json::to_string(subjects).map_err(|e| e.to_string())?;
        self.0.lock().map_err(|e|e.to_string())?.execute("INSERT INTO settings(key,value) VALUES('calendar_cache',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[payload]).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub fn cached_calendar(&self) -> Result<Vec<crate::models::Subject>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let payload = connection
            .query_row(
                "SELECT value FROM settings WHERE key='calendar_cache'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&payload).map_err(|e| e.to_string())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        match connection.query_row(
            "SELECT value FROM settings WHERE key=?1",
            [key],
            |row| row.get::<_, String>(0),
        ) {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 同一条目的待同步改动只保留最新一份（UNIQUE subject_id），避免乱序写回。
    pub fn enqueue_collection_sync(
        &self,
        subject_id: i64,
        collection: &str,
        ep: Option<i64>,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.0.lock().map_err(|e|e.to_string())?.execute("INSERT INTO sync_queue(subject_id,collection,ep,attempts,next_attempt_at,updated_at) VALUES(?1,?2,?3,0,?4,?4) ON CONFLICT(subject_id) DO UPDATE SET collection=excluded.collection,ep=excluded.ep,attempts=0,next_attempt_at=excluded.next_attempt_at,updated_at=excluded.updated_at",params![subject_id,collection,ep,now]).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn due_sync_entries(&self, limit: i64) -> Result<Vec<SyncEntry>, String> {
        let now = chrono::Utc::now().to_rfc3339();
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query = connection
            .prepare(
                "SELECT id,subject_id,collection,ep,attempts FROM sync_queue
                 WHERE next_attempt_at<=?1 ORDER BY updated_at LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        query
            .query_map(params![now, limit], |row| {
                Ok(SyncEntry {
                    id: row.get(0)?,
                    subject_id: row.get(1)?,
                    collection: row.get(2)?,
                    ep: row.get(3)?,
                    attempts: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn mark_sync_attempt(
        &self,
        id: i64,
        attempts: i64,
        next_attempt_at: &str,
        last_error: &str,
    ) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE sync_queue SET attempts=?2,next_attempt_at=?3,last_error=?4,updated_at=?5 WHERE id=?1",
                params![id, attempts, next_attempt_at, last_error, chrono::Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn remove_sync_entry(&self, id: i64) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute("DELETE FROM sync_queue WHERE id=?1", [id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn expire_all_sync_entries(&self) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "UPDATE sync_queue SET next_attempt_at=?1",
                [chrono::Utc::now().to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn pending_sync_count(&self) -> Result<i64, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        connection
            .query_row("SELECT COUNT(*) FROM sync_queue", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    /// (待同步数, 最近一次失败原因, 最近一次失败时间)
    pub fn sync_queue_summary(&self) -> Result<(i64, Option<String>, Option<String>), String> {
        let pending = self.pending_sync_count()?;
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let last = connection
            .query_row(
                "SELECT last_error,updated_at FROM sync_queue WHERE last_error IS NOT NULL ORDER BY updated_at DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )
            .ok();
        Ok((
            pending,
            last.as_ref().and_then(|(error, _)| error.clone()),
            last.map(|(_, at)| at),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub id: i64,
    pub subject_id: i64,
    pub collection: String,
    pub ep: Option<i64>,
    pub attempts: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bangumi::subject_from_v0;

    fn db() -> Database {
        Database::open_in_memory().expect("in-memory database")
    }

    #[test]
    fn collection_progress_upsert_keeps_latest_values() {
        let db = db();
        db.set_collection_progress(7, "doing", 3).unwrap();
        db.set_collection_progress(7, "on_hold", 5).unwrap();
        let collections = db.collections().unwrap();
        assert_eq!(
            collections.get(&7),
            Some(&("on_hold".to_string(), 5)),
            "同一条目的旧状态与进度必须被覆盖"
        );
    }

    #[test]
    fn collection_without_progress_defaults_watched_to_zero() {
        let db = db();
        db.set_collection(9, "wish").unwrap();
        assert_eq!(db.collections().unwrap().get(&9), Some(&("wish".to_string(), 0)));
    }

    #[test]
    fn cached_subject_roundtrip_preserves_payload() {
        let db = db();
        let subject = serde_json::json!({"id": 11, "name": "Old Anime", "eps": 24});
        db.cache_subject(11, &subject).unwrap();
        // 同一条目重复导入时覆盖旧缓存，与收藏同步的分页重复安全。
        db.cache_subject(11, &serde_json::json!({"id": 11, "name": "Old Anime", "eps": 25}))
            .unwrap();
        let cached = db.cached_subjects().unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0]["eps"], 25);
    }

    #[test]
    fn collections_merge_into_cached_subjects_like_get_calendar() {
        // 复现 get_calendar 的合并路径：缓存条目 + 本地收藏 => 追番页可展示旧番。
        let db = db();
        db.cache_subject(21, &serde_json::json!({"id": 21, "name": "Cached", "name_cn": "缓存番剧", "eps": 12}))
            .unwrap();
        db.cache_subject(22, &serde_json::json!({"id": 22, "name": "No Collection"}))
            .unwrap();
        db.set_collection_progress(21, "doing", 4).unwrap();
        let collections = db.collections().unwrap();
        let merged: Vec<_> = db
            .cached_subjects()
            .unwrap()
            .iter()
            .filter_map(|value| {
                let id = value.get("id")?.as_i64()?;
                let (collection, watched) = collections.get(&id)?;
                Some(subject_from_v0(value, Some(collection.clone()), *watched))
            })
            .collect();
        assert_eq!(merged.len(), 1, "没有本地收藏的缓存条目不应出现在追番页");
        assert_eq!(merged[0].id, 21);
        assert_eq!(merged[0].collection.as_deref(), Some("doing"));
        assert_eq!(merged[0].watched, 4);
        assert_eq!(merged[0].episodes, 12);
    }

    #[test]
    fn calendar_cache_roundtrip() {
        let db = db();
        assert!(db.cached_calendar().is_err(), "未缓存时应返回错误而非空数据");
        let subjects = vec![subject_from_v0(
            &serde_json::json!({"id": 31, "name": "Aired", "air_weekday": 3}),
            None,
            0,
        )];
        db.save_calendar(&subjects).unwrap();
        let cached = db.cached_calendar().unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].id, 31);
    }

    #[test]
    fn detail_cache_roundtrip_and_overwrite() {
        let db = db();
        assert!(db.cached_detail(11).unwrap().is_none());
        db.save_detail(11, &serde_json::json!({"id": 11, "eps": 12})).unwrap();
        db.save_detail(11, &serde_json::json!({"id": 11, "eps": 13})).unwrap();
        let (payload, updated_at) = db.cached_detail(11).unwrap().expect("cached detail");
        assert_eq!(payload["eps"], 13, "同条目重复刷新覆盖旧详情");
        assert!(chrono::DateTime::parse_from_rfc3339(&updated_at).is_ok());
    }

    #[test]
    fn settings_roundtrip_and_overwrite() {
        let db = db();
        assert_eq!(db.get_setting("missing").unwrap(), None);
        db.set_setting("rss_interval_minutes", "15").unwrap();
        db.set_setting("rss_interval_minutes", "30").unwrap();
        assert_eq!(
            db.get_setting("rss_interval_minutes").unwrap().as_deref(),
            Some("30")
        );
    }

    fn subject_feed(id: &str, subject_id: Option<i64>) -> RssFeed {
        RssFeed {
            id: id.into(),
            title: "测试订阅".into(),
            url: format!("https://mikanani.me/RSS/Bangumi?bangumiId={}", subject_id.unwrap_or_default()),
            enabled: true,
            last_checked_at: None,
            rule: crate::models::FeedRule::default(),
            subject_id,
        }
    }

    fn rss_item(guid: &str, title: &str) -> RssItem {
        RssItem {
            guid: guid.into(),
            feed_id: String::new(),
            title: title.into(),
            link: format!("https://example.com/{guid}"),
            torrent: None,
            published_at: None,
            downloaded: false,
            download: None,
            matches_rule: false,
        }
    }

    #[test]
    fn migrations_leave_user_version_at_latest() {
        let db = db();
        let version: i64 = db
            .0
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn subject_feed_roundtrip_and_unsubscribe() {
        let db = db();
        db.add_feed(&subject_feed("f1", Some(7))).unwrap();
        db.add_feed(&subject_feed("f2", None)).unwrap();
        db.insert_rss_items("f1", &[rss_item("g1", "[X] 番剧 01 [1080p]")])
            .unwrap();
        assert_eq!(db.feed_for_subject(7).unwrap().map(|feed| feed.id), Some("f1".into()));
        assert!(db.feed_for_subject(8).unwrap().is_none());
        db.delete_feeds_for_subject(7).unwrap();
        assert!(db.feed_for_subject(7).unwrap().is_none());
        assert_eq!(db.list_feeds().unwrap().len(), 1, "普通订阅不受影响");
        assert!(db.list_rss_items(Some("f1"), 10).unwrap().is_empty(), "取消订阅要清掉资源列表");
    }

    #[test]
    fn subscribe_adopts_existing_feed_with_same_url() {
        // 用户手动添加过同一单番地址时，订阅必须收编而不是撞 UNIQUE 约束报错。
        let db = db();
        let mut manual = subject_feed("f1", None);
        manual.url = "https://mikanani.me/RSS/Bangumi?bangumiId=9".into();
        db.add_feed(&manual).unwrap();
        db.set_feed_enabled("f1", false).unwrap();
        db.adopt_feed_as_subscription("f1", 9).unwrap();
        let adopted = db.feed_for_subject(9).unwrap().expect("adopted feed");
        assert_eq!(adopted.id, "f1");
        assert_eq!(adopted.subject_id, Some(9));
        assert!(adopted.enabled, "收编为订阅时要重新启用");
    }

    #[test]
    fn subject_rss_overview_tracks_download_states() {
        let db = db();
        let mut feed = subject_feed("f1", Some(7));
        feed.rule.includes = vec!["简中".into()];
        db.add_feed(&feed).unwrap();
        db.insert_rss_items(
            "f1",
            &[
                rss_item("g1", "[喵萌] 番剧 01 [简中]"),
                rss_item("g2", "[某组] 番剧 01 [生肉]"),
                rss_item("g3", "[喵萌] 番剧 02 [简中]"),
            ],
        )
        .unwrap();
        db.add_download(
            &DownloadTask {
                id: "t1".into(),
                title: "番剧 01".into(),
                episode: "01".into(),
                progress: 0.0,
                down_speed: 0,
                up_speed: 0,
                state: "downloading".into(),
                output_path: String::new(),
                playback_path: None,
            },
            "magnet:?xt=urn:btih:abc",
            "abc",
        )
        .unwrap();
        db.mark_rss_downloaded("g1", "t1").unwrap();
        let overview = db.subject_rss_overview().unwrap();
        let state = overview.get(&7).unwrap();
        assert!(state.downloading && state.pending && !state.completed);
        // 全部完成后徽章转为“已下载”；不匹配规则的 g2 不算“有更新”。
        db.set_download_state("t1", "completed").unwrap();
        let overview = db.subject_rss_overview().unwrap();
        let state = overview.get(&7).unwrap();
        assert!(state.completed && state.pending && !state.downloading);
    }

    #[test]
    fn sync_queue_keeps_latest_entry_per_subject_and_retries() {
        let db = db();
        db.enqueue_collection_sync(7, "doing", Some(3)).unwrap();
        db.enqueue_collection_sync(7, "collect", Some(12)).unwrap();
        db.enqueue_collection_sync(8, "wish", None).unwrap();
        assert_eq!(db.pending_sync_count().unwrap(), 2, "同一条目只保留最新改动");
        let due = db.due_sync_entries(10).unwrap();
        let entry = due.iter().find(|e| e.subject_id == 7).unwrap();
        assert_eq!(entry.collection, "collect");
        assert_eq!(entry.ep, Some(12));
        assert_eq!(entry.attempts, 0, "重新入队要重置退避计数");
        // 失败进入退避：到期时间推到远期后不再被取出。
        db.mark_sync_attempt(entry.id, 1, "2999-01-01T00:00:00+00:00", "Bangumi 同步失败：500")
            .unwrap();
        assert!(db
            .due_sync_entries(10)
            .unwrap()
            .iter()
            .all(|e| e.subject_id != 7));
        let (pending, last_error, last_attempt_at) = db.sync_queue_summary().unwrap();
        assert_eq!(pending, 2);
        assert_eq!(last_error.as_deref(), Some("Bangumi 同步失败：500"));
        assert!(last_attempt_at.is_some());
        db.expire_all_sync_entries().unwrap();
        assert_eq!(db.due_sync_entries(10).unwrap().len(), 2);
        db.remove_sync_entry(entry.id).unwrap();
        assert_eq!(db.pending_sync_count().unwrap(), 1);
    }
}
