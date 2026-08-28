use crate::models::{DownloadTask, RssDownloadStatus, RssFeed, RssItem};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::Path, sync::Mutex};

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
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);")
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
        Ok(Self(Mutex::new(connection)))
    }

    pub fn add_feed(&self, feed: &RssFeed) -> Result<(), String> {
        self.0.lock().map_err(|e| e.to_string())?.execute("INSERT INTO rss_feeds(id,title,url,enabled,last_checked_at,auto_download) VALUES(?1,?2,?3,?4,?5,?6)", params![feed.id,feed.title,feed.url,feed.enabled,feed.last_checked_at,feed.auto_download]).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn list_feeds(&self) -> Result<Vec<RssFeed>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query = connection.prepare("SELECT id,title,url,enabled,last_checked_at,auto_download FROM rss_feeds ORDER BY title").map_err(|e| e.to_string())?;
        query
            .query_map([], |row| {
                Ok(RssFeed {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                    enabled: row.get(3)?,
                    last_checked_at: row.get(4)?,
                    auto_download: row.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
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
    pub fn delete_feed(&self, id: &str) -> Result<(), String> {
        let mut connection = self.0.lock().map_err(|e| e.to_string())?;
        let tx = connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM rss_items WHERE feed_id=?1", [id])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM rss_feeds WHERE id=?1", [id])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
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
        let mut query=connection.prepare("SELECT id,title,episode,progress,down_speed,up_speed,state,output_path FROM downloads ORDER BY created_at DESC").map_err(|e|e.to_string())?;
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
        let mut query=connection.prepare("SELECT id,title,episode,progress,down_speed,up_speed,state,output_path FROM downloads WHERE source_key=?1 AND state<>'failed' ORDER BY created_at DESC LIMIT 1").map_err(|e|e.to_string())?;
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
                "UPDATE downloads SET progress=?2,down_speed=?3,up_speed=?4,state=?5 WHERE id=?1",
                params![
                    task.id,
                    task.progress,
                    task.down_speed,
                    task.up_speed,
                    task.state
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
    pub fn restorable_downloads(&self) -> Result<Vec<(String, String, String)>, String> {
        let connection = self.0.lock().map_err(|e| e.to_string())?;
        let mut query=connection.prepare("SELECT id,source,state FROM downloads WHERE state IN ('queued','downloading','paused')").map_err(|e|e.to_string())?;
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
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
}
