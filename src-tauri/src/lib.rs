mod bangumi;
mod db;
mod feeds;
mod matcher;
mod models;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use db::Database;
use librqbit::{
    AddTorrent, AddTorrentOptions, ListenerMode, ListenerOptions, ManagedTorrent, Session,
    SessionOptions, TorrentStatsState,
};
use models::{BatchDownloadResult, DownloadTask, RssFeed, RssItem, Subject};
use tauri::{
    Manager,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

struct AppState {
    db: Database,
    client: reqwest::Client,
    bt: Arc<Session>,
    handles: Mutex<HashMap<String, Arc<ManagedTorrent>>>,
    speed_samples: Mutex<HashMap<String, (u64, Instant)>>,
    download_path: PathBuf,
    trackers: tokio::sync::RwLock<Option<Vec<String>>>,
    download_gate: tokio::sync::Mutex<()>,
}

fn torrent_session_options() -> SessionOptions {
    SessionOptions {
        // Session::new() leaves the peer listener disabled in librqbit 9. That makes
        // every connection outbound-only and can substantially reduce the available
        // peer pool compared with full desktop clients such as qBittorrent.
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpAndUtp,
            enable_upnp_port_forwarding: true,
            ..Default::default()
        }),
        // Keep enough candidates around for swarms where only a small fraction of
        // discovered peers are fast or reachable.
        peer_limit: Some(256),
        client_name_and_version: Some(format!("Mizuki/{}", env!("CARGO_PKG_VERSION"))),
        ..Default::default()
    }
}

const TRACKERLIST_URL: &str = "https://cf.trackerslist.com/best.txt";

static EXITING: AtomicBool = AtomicBool::new(false);

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BangumiProfile {
    username: String,
    nickname: String,
    avatar: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CalendarPayload {
    subjects: Vec<Subject>,
    refreshed_at: Option<String>,
    stale: bool,
    warning: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchPayload {
    subjects: Vec<Subject>,
    local_only: bool,
    warning: Option<String>,
}

fn token_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new("app.mizuki.desktop", "bangumi-access-token").map_err(|e| e.to_string())
}

fn stored_access_token() -> Option<String> {
    token_entry()
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|value| !value.is_empty())
}

fn profile_from_value(value: &serde_json::Value) -> BangumiProfile {
    BangumiProfile {
        username: value
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .into(),
        nickname: value
            .get("nickname")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .into(),
        avatar: value
            .pointer("/avatar/large")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    }
}

fn subject_from_cache(value: &serde_json::Value) -> Subject {
    serde_json::from_value::<Subject>(value.clone())
        .unwrap_or_else(|_| bangumi::subject_from_v0(value, None, 0))
}

/// 周表 + 本地收藏 + Bangumi 收藏缓存条目 => 今日/追番页的完整条目列表。
/// 缓存条目只有已存在本地收藏时才出现（air_weekday 为 -1，仅用于追番页）。
fn merge_calendar_with_collections(
    mut subjects: Vec<Subject>,
    cached: Vec<serde_json::Value>,
    collections: &HashMap<i64, (String, i64)>,
) -> Vec<Subject> {
    for subject in &mut subjects {
        if let Some((collection, watched)) = collections.get(&subject.id) {
            subject.collection = Some(collection.clone());
            subject.watched = *watched;
        }
    }
    let mut known: std::collections::HashSet<i64> =
        subjects.iter().map(|subject| subject.id).collect();
    for value in cached {
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_default();
        if known.insert(id)
            && let Some((collection, watched)) = collections.get(&id).cloned()
        {
            let mut subject = subject_from_cache(&value);
            subject.collection = Some(collection);
            subject.watched = watched;
            subjects.push(subject);
        }
    }
    subjects
}

#[tauri::command]
async fn get_bangumi_profile(
    state: tauri::State<'_, AppState>,
) -> Result<Option<BangumiProfile>, String> {
    let Some(token) = stored_access_token() else {
        return Ok(None);
    };
    Ok(Some(profile_from_value(
        &bangumi::profile(&state.client, &token).await?,
    )))
}

#[tauri::command]
async fn save_bangumi_token(
    token: String,
    state: tauri::State<'_, AppState>,
) -> Result<BangumiProfile, String> {
    let token = token.trim();
    if token.is_empty() || token.len() > 512 || token.chars().any(char::is_whitespace) {
        return Err("Access Token 格式无效".into());
    }
    let value = bangumi::profile(&state.client, token).await?;
    token_entry()?
        .set_password(token)
        .map_err(|e| e.to_string())?;
    Ok(profile_from_value(&value))
}

#[tauri::command]
fn remove_bangumi_token() -> Result<(), String> {
    let entry = token_entry()?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
async fn sync_bangumi_collections(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let token = stored_access_token().ok_or("请先填写 Bangumi Access Token")?;
    let profile = bangumi::profile(&state.client, &token).await?;
    let username = profile
        .get("username")
        .and_then(|v| v.as_str())
        .ok_or("无法识别 Bangumi 用户")?;
    let items = bangumi::collections(&state.client, &token, username).await?;
    for item in &items {
        let id = item
            .get("subject_id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        let Some(collection) =
            bangumi::collection_slug(item.get("type").and_then(|v| v.as_i64()).unwrap_or(0))
        else {
            continue;
        };
        let watched = item
            .get("ep_status")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        state.db.set_collection_progress(id, collection, watched)?;
        if let Some(subject) = item.get("subject") {
            state.db.cache_subject(id, subject)?;
        }
    }
    Ok(items.len())
}

#[tauri::command]
async fn get_calendar(state: tauri::State<'_, AppState>) -> Result<CalendarPayload, String> {
    let (subjects, stale, warning, refreshed_at) = match bangumi::calendar(&state.client).await
    {
        Ok(subjects) => {
            state.db.save_calendar(&subjects)?;
            (subjects, false, None, Some(chrono::Utc::now().to_rfc3339()))
        }
        Err(error) => match state.db.cached_calendar() {
            Ok(subjects) if !subjects.is_empty() => (
                subjects,
                true,
                Some(format!("{error}，已显示上次缓存")),
                None,
            ),
            _ => return Err(error),
        }
    };
    let collections = state.db.collections()?;
    let subjects =
        merge_calendar_with_collections(subjects, state.db.cached_subjects()?, &collections);
    Ok(CalendarPayload {
        subjects,
        refreshed_at,
        stale,
        warning,
    })
}

#[tauri::command]
async fn search_anime(
    keyword: String,
    limit: Option<u32>,
    state: tauri::State<'_, AppState>,
) -> Result<SearchPayload, String> {
    let keyword = keyword.trim();
    if keyword.chars().count() < 2 {
        return Err("请输入至少 2 个字符".into());
    }
    if keyword.chars().count() > 80 {
        return Err("搜索关键词过长".into());
    }
    let collections = state.db.collections()?;
    let remote = bangumi::search_subjects(
        &state.client,
        stored_access_token().as_deref(),
        keyword,
        limit.unwrap_or(20),
    )
    .await;
    let (mut subjects, local_only, warning) = match remote {
        Ok(subjects) => (subjects, false, None),
        Err(error) => {
            let normalized = matcher::normalize_title(keyword);
            let values = state.db.cached_subjects().unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            let mut local: Vec<Subject> = values
                .into_iter()
                .map(|value| subject_from_cache(&value))
                .collect();
            if let Ok(calendar) = state.db.cached_calendar() {
                local.extend(calendar)
            }
            let subjects = local
                .into_iter()
                .filter(|subject| seen.insert(subject.id))
                .filter(|subject| {
                    matcher::normalize_title(&format!("{}{}", subject.name, subject.name_cn))
                        .contains(&normalized)
                })
                .take(limit.unwrap_or(20).min(30) as usize)
                .collect();
            (
                subjects,
                true,
                Some(format!("{error}，当前显示本地匹配结果")),
            )
        }
    };
    for subject in &mut subjects {
        if let Some((collection, watched)) = collections.get(&subject.id) {
            subject.collection = Some(collection.clone());
            subject.watched = *watched
        }
    }
    Ok(SearchPayload {
        subjects,
        local_only,
        warning,
    })
}

#[tauri::command]
async fn add_rss_feed(url: String, state: tauri::State<'_, AppState>) -> Result<RssFeed, String> {
    let (title, items) = feeds::fetch(&state.client, &url).await?;
    let feed = RssFeed {
        id: uuid::Uuid::new_v4().to_string(),
        title,
        url,
        enabled: true,
        last_checked_at: Some(chrono::Utc::now().to_rfc3339()),
        auto_download: false,
    };
    state.db.add_feed(&feed)?;
    state.db.insert_rss_items(&feed.id, &items)?;
    Ok(feed)
}

#[tauri::command]
fn list_rss_feeds(state: tauri::State<'_, AppState>) -> Result<Vec<RssFeed>, String> {
    state.db.list_feeds()
}

#[tauri::command]
fn list_rss_items(
    feed_id: Option<String>,
    limit: Option<u32>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<RssItem>, String> {
    let mut items = state
        .db
        .list_rss_items(feed_id.as_deref(), limit.unwrap_or(100).min(500))?;
    let handles = state.handles.lock().map_err(|e| e.to_string())?;
    for item in &mut items {
        if let Some(download) = &mut item.download {
            download.active = handles.contains_key(&download.task_id);
        }
    }
    Ok(items)
}

#[tauri::command]
fn set_rss_feed_enabled(
    id: String,
    enabled: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    state.db.set_feed_enabled(&id, enabled)
}

#[tauri::command]
fn delete_rss_feed(id: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.db.delete_feed(&id)
}

async fn tracker_list(state: &AppState) -> Vec<String> {
    if let Some(trackers) = state.trackers.read().await.as_ref() {
        return trackers.clone();
    }
    let trackers = match state
        .client
        .get(TRACKERLIST_URL)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
    {
        Ok(response) => response
            .text()
            .await
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .take(80)
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    };
    if !trackers.is_empty() {
        *state.trackers.write().await = Some(trackers.clone());
    }
    trackers
}

fn download_source_key(source: &str) -> String {
    db::canonical_download_source_key(source)
}

fn magnet_with_trackers(source: &str, trackers: &[String]) -> String {
    if !source.starts_with("magnet:") {
        return source.to_owned();
    }
    let Ok(mut url) = url::Url::parse(source) else {
        return source.to_owned();
    };
    let existing: std::collections::HashSet<String> = url
        .query_pairs()
        .filter(|(key, _)| key == "tr")
        .map(|(_, value)| value.into_owned())
        .collect();
    {
        let mut query = url.query_pairs_mut();
        for tracker in trackers
            .iter()
            .filter(|tracker| !existing.contains(*tracker))
            .take(40)
        {
            query.append_pair("tr", tracker);
        }
    }
    url.into()
}

async fn prepare_torrent_input(
    source: &str,
    trackers: &[String],
    client: &reqwest::Client,
) -> Result<(String, AddTorrent<'static>), String> {
    if source.starts_with("magnet:") {
        let prepared = magnet_with_trackers(source, trackers);
        return Ok((prepared.clone(), AddTorrent::from_url(prepared)));
    }
    let response = client
        .get(source)
        .header(
            reqwest::header::ACCEPT,
            "application/x-bittorrent,*/*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| format!("获取种子文件失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("获取种子文件失败：{e}"))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("读取种子文件失败：{e}"))?;
    if bytes.is_empty() {
        return Err("种子文件为空".into());
    }
    if bytes.len() > 20 * 1024 * 1024 {
        return Err("种子文件超过 20 MB，已拒绝加载".into());
    }
    if bytes.first() != Some(&b'd') {
        return Err("订阅地址返回的不是有效 .torrent 文件".into());
    }
    Ok((source.to_owned(), AddTorrent::from_bytes(bytes)))
}

#[cfg(test)]
mod calendar_merge_tests {
    use super::merge_calendar_with_collections;
    use crate::bangumi::subject_from_v0;
    use crate::models::Subject;
    use std::collections::HashMap;

    fn cached(value: serde_json::Value) -> serde_json::Value {
        value
    }

    fn collection(id: i64, state: &str, watched: i64) -> (i64, (String, i64)) {
        (id, (state.to_string(), watched))
    }

    #[test]
    fn calendar_subjects_pick_up_local_collection_and_watched() {
        let mut subject = subject_from_v0(&serde_json::json!({"id": 1, "name": "A"}), None, 0);
        subject.air_weekday = 2;
        let collections = HashMap::from([collection(1, "doing", 5)]);
        let merged = merge_calendar_with_collections(vec![subject], Vec::new(), &collections);
        assert_eq!(merged[0].collection.as_deref(), Some("doing"));
        assert_eq!(merged[0].watched, 5);
        assert_eq!(merged[0].air_weekday, 2, "合并不得破坏周表放送日");
    }

    #[test]
    fn cached_collection_entries_are_appended_once() {
        let cached = vec![
            cached(serde_json::json!({"id": 2, "name": "Old", "name_cn": "旧番", "eps": 24})),
            cached(serde_json::json!({"id": 2, "name": "Old", "name_cn": "旧番", "eps": 24})),
        ];
        let collections = HashMap::from([collection(2, "collect", 24)]);
        let merged = merge_calendar_with_collections(Vec::new(), cached, &collections);
        assert_eq!(merged.len(), 1, "重复缓存条目只出现一次");
        assert_eq!(merged[0].name_cn, "旧番");
        assert_eq!(merged[0].watched, 24);
    }

    #[test]
    fn cached_entries_without_local_collection_are_hidden() {
        let cached = vec![
            cached(serde_json::json!({"id": 3, "name": "X"})),
            cached(serde_json::json!({"id": 4, "name": "Y"})),
        ];
        let collections = HashMap::from([collection(4, "wish", 0)]);
        let merged = merge_calendar_with_collections(Vec::new(), cached, &collections);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, 4);
    }

    #[test]
    fn cached_payload_falls_back_to_v0_parser() {
        // cached_subjects 存的是 Bangumi 原始 JSON，字段名与 Subject 不同也能解析。
        let value = serde_json::json!({"id": 5, "name": "Raw", "name_cn": "原始", "rating": {"score": 7.1, "rank": 99}, "eps": 13});
        let collections = HashMap::from([collection(5, "doing", 1)]);
        let merged = merge_calendar_with_collections(Vec::new(), vec![value], &collections);
        assert_eq!(merged.len(), 1);
        let subject: &Subject = &merged[0];
        assert_eq!(subject.name_cn, "原始");
        assert!((subject.score - 7.1).abs() < f64::EPSILON);
        assert_eq!(subject.episodes, 13);
    }
}

#[cfg(test)]
mod download_tests {
    use super::{download_source_key, magnet_with_trackers, torrent_session_options};
    use librqbit::ListenerMode;

    #[test]
    fn desktop_session_accepts_incoming_tcp_and_utp_peers() {
        let options = torrent_session_options();
        let listener = options.listen.expect("peer listener should be enabled");
        assert!(matches!(listener.mode, ListenerMode::TcpAndUtp));
        assert!(listener.enable_upnp_port_forwarding);
        assert_eq!(options.peer_limit, Some(256));
    }

    #[test]
    fn magnet_dedup_key_ignores_name_and_trackers() {
        let first = "magnet:?xt=urn:btih:ABCDEF&dn=Episode+1&tr=udp%3A%2F%2Fold.example%3A80";
        let second = "magnet:?tr=udp%3A%2F%2Fnew.example%3A80&xt=urn:btih:abcdef&dn=Other";
        assert_eq!(download_source_key(first), download_source_key(second));
    }

    #[test]
    fn http_source_key_preserves_case_sensitive_path_and_query() {
        let first = "https://example.com/A.torrent?token=X";
        let second = "https://example.com/a.torrent?token=x";
        assert_ne!(download_source_key(first), download_source_key(second));
    }

    #[test]
    fn trackerlist_entries_are_added_without_duplicates() {
        let source = "magnet:?xt=urn:btih:abcdef&tr=udp%3A%2F%2Fexisting.example%3A80";
        let trackers = vec![
            "udp://existing.example:80".to_owned(),
            "https://new.example/announce".to_owned(),
        ];
        let prepared = magnet_with_trackers(source, &trackers);
        let url = url::Url::parse(&prepared).expect("valid magnet URL");
        let actual: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "tr")
            .map(|(_, value)| value.into_owned())
            .collect();
        assert_eq!(
            actual,
            vec![
                "udp://existing.example:80".to_owned(),
                "https://new.example/announce".to_owned()
            ]
        );
    }
}

async fn start_download(
    source: &str,
    title: String,
    episode: String,
    state: &AppState,
) -> Result<DownloadTask, String> {
    if !(source.starts_with("magnet:")
        || source.starts_with("http://")
        || source.starts_with("https://"))
    {
        return Err("仅支持 magnet、HTTP 或 HTTPS 种子地址".into());
    }
    let _guard = state.download_gate.lock().await;
    let source_key = download_source_key(source);
    let existing = state.db.download_by_source_key(&source_key)?;
    if let Some(task) = &existing {
        let active = state
            .handles
            .lock()
            .map_err(|e| e.to_string())?
            .contains_key(&task.id);
        if active || task.state == "completed" {
            return Ok(task.clone());
        }
    }
    let trackers = tracker_list(state).await;
    let (prepared_source, torrent_input) =
        prepare_torrent_input(source, &trackers, &state.client).await?;
    let (mut task, is_new) = match existing {
        Some(task) => (task, false),
        None => (
            DownloadTask {
                id: uuid::Uuid::new_v4().to_string(),
                title,
                episode,
                progress: 0.0,
                down_speed: 0,
                up_speed: 0,
                state: "queued".into(),
                output_path: state.download_path.to_string_lossy().into_owned(),
            },
            true,
        ),
    };
    let restore_paused = task.state == "paused";
    if is_new {
        state
            .db
            .add_download(&task, &prepared_source, &source_key)?;
    } else {
        state.db.set_download_state(&task.id, "queued")?;
    }
    let options = AddTorrentOptions {
        // Existing partial/complete files are opened without truncation and hash-checked for resume.
        overwrite: true,
        trackers: (!source.starts_with("magnet:") && !trackers.is_empty()).then_some(trackers),
        ..Default::default()
    };
    match state.bt.add_torrent(torrent_input, Some(options)).await {
        Ok(response) => {
            let Some(handle) = response.into_handle() else {
                state.db.set_download_state(&task.id, "failed")?;
                return Err("下载引擎未能创建任务，请重试".into());
            };
            if restore_paused {
                if let Err(error) = state.bt.pause(&handle).await {
                    state.db.set_download_state(&task.id, "failed")?;
                    return Err(error.to_string());
                }
            }
            state
                .handles
                .lock()
                .map_err(|e| e.to_string())?
                .insert(task.id.clone(), handle);
            task.state = if restore_paused {
                "paused".into()
            } else {
                "downloading".into()
            };
            state.db.set_download_state(&task.id, &task.state)?;
            Ok(task)
        }
        Err(error) => {
            state.db.set_download_state(&task.id, "failed")?;
            Err(error.to_string())
        }
    }
}

async fn refresh_feed(feed: &RssFeed, state: &AppState) -> Result<Vec<RssItem>, String> {
    let (title, items) = feeds::fetch(&state.client, &feed.url).await?;
    let now = chrono::Utc::now().to_rfc3339();
    let inserted = state.db.insert_rss_items(&feed.id, &items)?;
    state.db.update_feed(&feed.id, &title, &now)?;
    Ok(inserted)
}

#[tauri::command]
async fn refresh_rss_feed(id: String, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let feed = state.db.feed(&id)?;
    if !feed.enabled {
        return Ok(0);
    }
    Ok(refresh_feed(&feed, &state).await?.len())
}

#[tauri::command]
async fn refresh_all_rss_feeds(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let feeds = state.db.list_feeds()?;
    let mut count = 0;
    let mut errors = Vec::new();
    for feed in feeds.into_iter().filter(|feed| feed.enabled) {
        match refresh_feed(&feed, &state).await {
            Ok(items) => count += items.len(),
            Err(error) => errors.push(format!("{}：{}", feed.title, error)),
        }
    }
    if errors.is_empty() {
        Ok(count)
    } else {
        Err(format!(
            "刷新完成，新增 {count} 条；失败：{}",
            errors.join("；")
        ))
    }
}

#[tauri::command]
async fn download_rss_item(
    guid: String,
    state: tauri::State<'_, AppState>,
) -> Result<DownloadTask, String> {
    let item = state.db.rss_item(&guid)?;
    let source = rss_download_source(&item).ok_or("该 RSS 条目没有可下载地址")?;
    let task = start_download(source, item.title.clone(), "RSS 手动下载".into(), &state).await?;
    state.db.mark_rss_downloaded(&guid, &task.id)?;
    Ok(task)
}

fn rss_download_source(item: &RssItem) -> Option<&str> {
    item.torrent.as_deref().or_else(|| {
        let link = item.link.as_str();
        (link.starts_with("magnet:") || link.to_ascii_lowercase().ends_with(".torrent"))
            .then_some(link)
    })
}

#[tauri::command]
async fn download_rss_items(
    guids: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<BatchDownloadResult, String> {
    if guids.is_empty() {
        return Ok(BatchDownloadResult {
            added: 0,
            reused: 0,
            failed: 0,
            errors: Vec::new(),
        });
    }
    if guids.len() > 100 {
        return Err("单次最多选择 100 个资源".into());
    }
    let mut added = 0;
    let mut reused = 0;
    let mut errors = Vec::new();
    for guid in guids {
        let item = match state.db.rss_item(&guid) {
            Ok(item) => item,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if item.download.as_ref().is_some_and(|download| {
            download.state != "failed" && (download.active || download.state == "completed")
        }) {
            reused += 1;
            continue;
        }
        let Some(source) = rss_download_source(&item) else {
            errors.push(format!("{}：没有可下载地址", item.title));
            continue;
        };
        let existed = match state
            .db
            .download_by_source_key(&download_source_key(source))
        {
            Ok(task) => task.is_some(),
            Err(error) => {
                errors.push(format!("{}：检查重复任务失败：{}", item.title, error));
                continue;
            }
        };
        match start_download(
            source,
            item.title.clone(),
            "RSS 批量手动下载".into(),
            &state,
        )
        .await
        {
            Ok(task) => {
                if existed {
                    reused += 1
                } else {
                    added += 1
                }
                if let Err(error) = state.db.mark_rss_downloaded(&guid, &task.id) {
                    errors.push(format!(
                        "{}：任务已添加，但关联 RSS 失败：{}",
                        item.title, error
                    ));
                }
            }
            Err(error) => errors.push(format!("{}：{}", item.title, error)),
        }
    }
    Ok(BatchDownloadResult {
        added,
        reused,
        failed: errors.len(),
        errors,
    })
}

#[tauri::command]
fn list_downloads(state: tauri::State<'_, AppState>) -> Result<Vec<DownloadTask>, String> {
    let mut tasks = state.db.list_downloads()?;
    let handles = state.handles.lock().map_err(|e| e.to_string())?;
    let mut speed_samples = state.speed_samples.lock().map_err(|e| e.to_string())?;
    for task in &mut tasks {
        if let Some(handle) = handles.get(&task.id) {
            let stats = handle.stats();
            let now = Instant::now();
            let observed_speed = speed_samples
                .insert(task.id.clone(), (stats.progress_bytes, now))
                .and_then(|(previous_bytes, previous_at)| {
                    let elapsed = now.duration_since(previous_at).as_secs_f64();
                    (elapsed > 0.25 && stats.progress_bytes >= previous_bytes)
                        .then(|| ((stats.progress_bytes - previous_bytes) as f64 / elapsed) as u64)
                })
                .unwrap_or(0);
            let was_completed = task.state == "completed";
            task.progress = if stats.total_bytes > 0 {
                stats.progress_bytes as f64 / stats.total_bytes as f64
            } else {
                0.0
            };
            task.state = if stats.finished {
                "completed"
            } else {
                match stats.state {
                    TorrentStatsState::Live => "downloading",
                    TorrentStatsState::Paused => "paused",
                    TorrentStatsState::Error => "failed",
                    TorrentStatsState::Initializing { paused: true } => "paused",
                    TorrentStatsState::Initializing { paused: false } => "queued",
                }
            }
            .into();
            if stats.finished && !was_completed {
                let session = state.bt.clone();
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = session.pause(&handle).await;
                });
            }
            if task.state == "downloading"
                && let Some(live) = stats.live
            {
                // librqbit's instantaneous estimator can briefly report zero while
                // pieces are still arriving. Byte deltas keep the UI truthful.
                task.down_speed = live.download_speed.as_bytes().max(observed_speed);
                task.up_speed = live.upload_speed.as_bytes();
            } else {
                task.down_speed = 0;
                task.up_speed = 0;
            }
        }
        state.db.update_download(task)?;
    }
    Ok(tasks)
}

#[tauri::command]
async fn add_download(
    source: String,
    title: String,
    episode: String,
    state: tauri::State<'_, AppState>,
) -> Result<DownloadTask, String> {
    start_download(&source, title, episode, &state).await
}

#[tauri::command]
async fn pause_download(id: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(&id)
        .cloned()
        .ok_or("任务不存在")?;
    state.bt.pause(&handle).await.map_err(|e| e.to_string())?;
    state.db.set_download_state(&id, "paused")
}

#[tauri::command]
async fn resume_download(id: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(&id)
        .cloned()
        .ok_or("任务不存在")?;
    state.bt.unpause(&handle).await.map_err(|e| e.to_string())?;
    state.db.set_download_state(&id, "downloading")
}

#[tauri::command]
fn download_playback_path(
    id: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "webm", "avi", "mov", "m4v", "ts"];
    let handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(&id)
        .cloned()
        .ok_or("任务不存在或尚未恢复")?;
    if !handle.stats().finished {
        return Err("下载尚未完成".into());
    }
    let relative_path = handle
        .with_metadata(|metadata| {
            metadata
                .file_infos
                .iter()
                .filter(|file| {
                    file.relative_filename
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|extension| {
                            VIDEO_EXTENSIONS
                                .iter()
                                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                        })
                })
                .max_by_key(|file| file.len)
                .map(|file| file.relative_filename.clone())
        })
        .map_err(|e| e.to_string())?
        .ok_or("该任务中没有可播放的视频文件")?;
    let path = handle.output_folder().join(relative_path);
    if !path.is_file() {
        return Err("视频文件不存在，可能已被移动或删除".into());
    }
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
async fn delete_download(
    id: String,
    delete_files: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let handle = { state.handles.lock().map_err(|e| e.to_string())?.remove(&id) };
    if let Some(handle) = handle {
        state
            .bt
            .delete(handle.id().into(), delete_files)
            .await
            .map_err(|e| e.to_string())?
    }
    state.db.reset_rss_download_for_task(&id)?;
    state.db.delete_download(&id)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchProgress {
    collection: String,
    watched: i64,
}

#[tauri::command]
fn set_collection(
    subject_id: i64,
    collection: String,
    subject: Option<Subject>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if !["wish", "doing", "collect", "on_hold", "dropped"].contains(&collection.as_str()) {
        return Err("收藏状态无效".into());
    }
    state.db.set_collection(subject_id, &collection)?;
    if let Some(subject) = subject {
        let payload = serde_json::to_value(subject).map_err(|e| e.to_string())?;
        state.db.cache_subject(subject_id, &payload)?;
    }
    // 写回失败进入同步队列重试（见 enqueue_collection_sync）。
    if let Some(token) = stored_access_token() {
        let client = state.client.clone();
        tauri::async_runtime::spawn(async move {
            let _ = bangumi::set_collection(&client, &token, subject_id, &collection, None).await;
        });
    }
    Ok(())
}

/// 更新观看集数：先写本地，再带 `ep` 写回 Bangumi。
/// 条目还没有收藏状态时视为“在看”。
#[tauri::command]
fn set_watch_progress(
    subject_id: i64,
    watched: i64,
    state: tauri::State<'_, AppState>,
) -> Result<WatchProgress, String> {
    if !(0..=2000).contains(&watched) {
        return Err("观看集数无效".into());
    }
    let collection = match state.db.collection_state(subject_id)? {
        Some((collection, _)) => collection,
        None => "doing".to_string(),
    };
    state
        .db
        .set_collection_progress(subject_id, &collection, watched)?;
    if let Some(token) = stored_access_token() {
        let client = state.client.clone();
        let collection = collection.clone();
        tauri::async_runtime::spawn(async move {
            let _ = bangumi::set_collection(&client, &token, subject_id, &collection, Some(watched))
                .await;
        });
    }
    Ok(WatchProgress { collection, watched })
}

#[tauri::command]
async fn get_comments(
    subject_id: i64,
    offset: u32,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    bangumi::comments(&state.client, subject_id, offset).await
}

#[tauri::command]
async fn get_subject_detail(
    subject_id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let token = stored_access_token();
    bangumi::subject(&state.client, token.as_deref(), subject_id).await
}

#[tauri::command]
fn preview_rule_match(title: String, rule: matcher::MatchRule) -> bool {
    matcher::resource_matches(&title, &rule)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            let download_path = app.path().download_dir()?.join("Mizuki");
            std::fs::create_dir_all(&download_path)?;
            let bt = tauri::async_runtime::block_on(Session::new_with_opts(
                download_path.clone(),
                torrent_session_options(),
            ))?;
            let client = reqwest::Client::builder()
                .user_agent(format!("Mizuki/{}", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(15))
                .build()?;
            app.manage(AppState {
                db: Database::open(&data.join("mizuki.sqlite3")).map_err(std::io::Error::other)?,
                client,
                bt,
                handles: Mutex::new(HashMap::new()),
                speed_samples: Mutex::new(HashMap::new()),
                download_path,
                trackers: tokio::sync::RwLock::new(None),
                download_gate: tokio::sync::Mutex::new(()),
            });
            let restore_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = restore_app.state::<AppState>();
                let _guard = state.download_gate.lock().await;
                // 后台预热 TrackerList，避免第一次手动下载被网络探测阻塞。
                let trackers = tracker_list(&state).await;
                if let Ok(tasks) = state.db.restorable_downloads() {
                    for (id, source, previous_state) in tasks {
                        let options = AddTorrentOptions {
                            overwrite: true,
                            trackers: (!source.starts_with("magnet:") && !trackers.is_empty())
                                .then_some(trackers.clone()),
                            ..Default::default()
                        };
                        let input = prepare_torrent_input(&source, &trackers, &state.client).await;
                        match input {
                            Ok((_, torrent_input)) => {
                                match state.bt.add_torrent(torrent_input, Some(options)).await {
                                    Ok(response) => {
                                        if let Some(handle) = response.into_handle() {
                                            if previous_state == "paused" {
                                                let _ = state.bt.pause(&handle).await;
                                            }
                                            if let Ok(mut handles) = state.handles.lock() {
                                                handles.insert(id, handle);
                                            }
                                        } else {
                                            let _ = state.db.set_download_state(&id, "failed");
                                        }
                                    }
                                    Err(_) => {
                                        let _ = state.db.set_download_state(&id, "failed");
                                    }
                                }
                            }
                            Err(_) => {
                                let _ = state.db.set_download_state(&id, "failed");
                            }
                        }
                    }
                }
            });

            let show = MenuItem::with_id(app, "show", "显示 Mizuki", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "暂停全部下载", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &pause, &quit])?;
            let tray_icon = app
                .default_window_icon()
                .cloned()
                .ok_or("应用图标资源缺失")?;
            TrayIconBuilder::new()
                .icon(tray_icon)
                .tooltip("Mizuki")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "pause" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<AppState>();
                            let handles = state
                                .handles
                                .lock()
                                .ok()
                                .map(|items| {
                                    items
                                        .iter()
                                        .map(|(id, handle)| (id.clone(), handle.clone()))
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            for (id, handle) in handles {
                                if state.bt.pause(&handle).await.is_ok() {
                                    let _ = state.db.set_download_state(&id, "paused");
                                }
                            }
                        });
                    }
                    "quit" => {
                        EXITING.store(true, Ordering::SeqCst);
                        app.exit(0)
                    }
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && !EXITING.load(Ordering::SeqCst)
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_calendar,
            search_anime,
            get_subject_detail,
            get_bangumi_profile,
            save_bangumi_token,
            remove_bangumi_token,
            sync_bangumi_collections,
            add_rss_feed,
            list_rss_feeds,
            list_rss_items,
            set_rss_feed_enabled,
            delete_rss_feed,
            refresh_rss_feed,
            refresh_all_rss_feeds,
            download_rss_item,
            download_rss_items,
            list_downloads,
            add_download,
            pause_download,
            resume_download,
            download_playback_path,
            delete_download,
            set_collection,
            set_watch_progress,
            get_comments,
            preview_rule_match
        ])
        .run(tauri::generate_context!())
        .expect("error while running Mizuki")
}
