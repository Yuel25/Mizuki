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
    sync_notify: tokio::sync::Notify,
    settings: Mutex<AppSettings>,
}

impl AppState {
    fn settings(&self) -> AppSettings {
        self.settings
            .lock()
            .map(|settings| settings.clone())
            .unwrap_or_default()
    }

    /// 新任务的输出目录：自定义目录或默认“下载\Mizuki”。
    fn download_dir(&self) -> PathBuf {
        self.settings()
            .download_dir
            .and_then(|dir| {
                let path = PathBuf::from(&dir);
                path.is_absolute().then_some(path)
            })
            .unwrap_or_else(|| self.download_path.clone())
    }
}

fn torrent_session_options(settings: &AppSettings) -> SessionOptions {
    SessionOptions {
        // Session::new() leaves the peer listener disabled in librqbit 9. That makes
        // every connection outbound-only and can substantially reduce the available
        // peer pool compared with full desktop clients such as qBittorrent.
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpAndUtp,
            enable_upnp_port_forwarding: true,
            listen_addr: std::net::SocketAddr::new(
                std::net::Ipv6Addr::UNSPECIFIED.into(),
                settings.bt_listen_port,
            ),
            announce_port: (settings.bt_listen_port > 0).then_some(settings.bt_listen_port),
            ..Default::default()
        }),
        // Keep enough candidates around for swarms where only a small fraction of
        // discovered peers are fast or reachable.
        peer_limit: Some(settings.bt_peer_limit as usize),
        ratelimits: librqbit::limits::LimitsConfig {
            upload_bps: kbps_to_bps(settings.bt_upload_kbps),
            download_bps: kbps_to_bps(settings.bt_download_kbps),
        },
        client_name_and_version: Some(format!("Mizuki/{}", env!("CARGO_PKG_VERSION"))),
        ..Default::default()
    }
}

const TRACKERLIST_URL: &str = "https://cf.trackerslist.com/best.txt";

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "webm", "avi", "mov", "m4v", "ts"];

fn is_video_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

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

/// 全部设置项。以 JSON 存在 settings 表（key=app_settings），
/// serde(default) 保证旧数据缺字段时逐项回落默认值。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AppSettings {
    /// BT 监听端口，0 表示随机。重启应用后生效。
    bt_listen_port: u16,
    /// 上传/下载限速，KB/s；0 表示不限。
    bt_upload_kbps: u64,
    bt_download_kbps: u64,
    /// 每个 torrent 的最大连接 peer 数。重启后生效。
    bt_peer_limit: u32,
    /// 同时下载的任务数上限，0 表示不限；超出部分排队（paused + queued）。
    max_concurrent_downloads: u32,
    /// 下载完成后自动暂停任务（停止做种）。
    stop_seeding_on_complete: bool,
    rss_interval_minutes: u32,
    /// 自定义下载目录；None 表示系统下载目录下的 Mizuki。
    download_dir: Option<String>,
    autostart: bool,
    close_to_tray: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            bt_listen_port: 0,
            bt_upload_kbps: 0,
            bt_download_kbps: 0,
            bt_peer_limit: 256,
            max_concurrent_downloads: 0,
            stop_seeding_on_complete: true,
            rss_interval_minutes: 15,
            download_dir: None,
            autostart: false,
            close_to_tray: true,
        }
    }
}

const SETTINGS_KEY: &str = "app_settings";

fn load_settings(db: &Database) -> AppSettings {
    db.get_setting(SETTINGS_KEY)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn store_settings(db: &Database, settings: &AppSettings) -> Result<(), String> {
    let json = serde_json::to_string(settings).map_err(|e| e.to_string())?;
    db.set_setting(SETTINGS_KEY, &json)
}

fn kbps_to_bps(kbps: u64) -> Option<std::num::NonZeroU32> {
    if kbps == 0 || kbps > u32::MAX as u64 / 1024 {
        return None;
    }
    std::num::NonZeroU32::new((kbps * 1024) as u32)
}

fn validate_settings(settings: &AppSettings) -> Result<(), String> {
    if settings.bt_upload_kbps > 1_000_000 || settings.bt_download_kbps > 1_000_000 {
        return Err("限速上限为 1000000 KB/s".into());
    }
    if !(1..=2000).contains(&settings.bt_peer_limit) {
        return Err("每任务连接数需在 1-2000 之间".into());
    }
    if settings.max_concurrent_downloads > 20 {
        return Err("并发任务数最多 20".into());
    }
    if !(5..=1440).contains(&settings.rss_interval_minutes) {
        return Err("RSS 刷新间隔需在 5-1440 分钟之间".into());
    }
    Ok(())
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
    let mut subject = serde_json::from_value::<Subject>(value.clone())
        .unwrap_or_else(|_| bangumi::subject_from_v0(value, None, 0));
    // 缓存里存的旧 JSON 可能还是 http 图片地址，读取时统一升级。
    let needs_upgrade = subject
        .image
        .as_deref()
        .is_some_and(|image| image.starts_with("http://lain.bgm.tv/") || image.starts_with("http://bgm.tv/"));
    if needs_upgrade {
        subject.image = subject
            .image
            .map(|image| format!("https://{}", &image["http://".len()..]));
    }
    subject
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
        rule: models::FeedRule::default(),
    };
    state.db.add_feed(&feed)?;
    state.db.insert_rss_items(&feed.id, &items)?;
    Ok(feed)
}

#[tauri::command]
fn set_rss_feed_rules(
    id: String,
    rule: models::FeedRule,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if rule.includes.len() > 20 || rule.excludes.len() > 20 {
        return Err("规则关键词过多（最多 20 个）".into());
    }
    state.db.update_feed_rule(&id, &rule)
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
    let rules: HashMap<String, models::FeedRule> = state
        .db
        .list_feeds()?
        .into_iter()
        .map(|feed| (feed.id.clone(), feed.rule))
        .collect();
    for item in &mut items {
        if let Some(download) = &mut item.download {
            download.active = handles.contains_key(&download.task_id);
        }
        if let Some(rule) = rules.get(&item.feed_id) {
            item.matches_rule = matcher::resource_matches(&item.title, &rule.into());
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
    use super::{AppSettings, download_source_key, magnet_with_trackers, torrent_session_options, validate_settings};
    use librqbit::ListenerMode;

    #[test]
    fn desktop_session_accepts_incoming_tcp_and_utp_peers() {
        let options = torrent_session_options(&AppSettings::default());
        let listener = options.listen.expect("peer listener should be enabled");
        assert!(matches!(listener.mode, ListenerMode::TcpAndUtp));
        assert!(listener.enable_upnp_port_forwarding);
        assert_eq!(options.peer_limit, Some(256));
        assert_eq!(listener.listen_addr.port(), 0, "默认使用随机端口");
    }

    #[test]
    fn session_options_apply_bt_settings() {
        let settings = AppSettings {
            bt_listen_port: 4242,
            bt_upload_kbps: 512,
            bt_download_kbps: 0,
            bt_peer_limit: 120,
            ..Default::default()
        };
        let options = torrent_session_options(&settings);
        let listener = options.listen.expect("peer listener should be enabled");
        assert_eq!(listener.listen_addr.port(), 4242);
        assert_eq!(listener.announce_port, Some(4242));
        assert_eq!(options.peer_limit, Some(120));
        assert_eq!(
            options.ratelimits.upload_bps.map(|v| v.get()),
            Some(512 * 1024)
        );
        assert_eq!(options.ratelimits.download_bps, None, "0 表示不限速");
    }

    #[test]
    fn settings_validation_bounds() {
        let mut settings = AppSettings::default();
        assert!(validate_settings(&settings).is_ok());
        settings.bt_peer_limit = 0;
        assert!(validate_settings(&settings).is_err());
        settings.bt_peer_limit = 256;
        settings.bt_download_kbps = 2_000_000;
        assert!(validate_settings(&settings).is_err());
        settings.bt_download_kbps = 0;
        settings.rss_interval_minutes = 1;
        assert!(validate_settings(&settings).is_err());
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

/// 正在占用下载槽位的任务数（排队中的也算，防止超卖）。
fn active_download_count(state: &AppState) -> usize {
    let Ok(tasks) = state.db.list_downloads() else {
        return 0;
    };
    let handles = state
        .handles
        .lock()
        .map(|handles| handles.keys().cloned().collect::<std::collections::HashSet<_>>())
        .unwrap_or_default();
    tasks
        .iter()
        .filter(|task| {
            handles.contains(&task.id) && (task.state == "downloading" || task.state == "queued")
        })
        .count()
}

/// 有空闲槽位时把排队的任务（paused 句柄 + queued 状态）继续启动。
fn promote_queued_downloads(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings();
    if settings.max_concurrent_downloads == 0 {
        return;
    }
    let Ok(tasks) = state.db.list_downloads() else {
        return;
    };
    let handles = state
        .handles
        .lock()
        .map(|handles| handles.clone())
        .unwrap_or_default();
    let active = tasks
        .iter()
        .filter(|task| handles.contains_key(&task.id) && task.state == "downloading")
        .count() as i64;
    let mut slots = settings.max_concurrent_downloads as i64 - active;
    for task in tasks.iter().filter(|task| task.state == "queued") {
        if slots <= 0 {
            break;
        }
        let Some(handle) = handles.get(&task.id) else {
            continue;
        };
        // 先占位再启动，避免轮询重入导致超额。
        if state.db.set_download_state(&task.id, "downloading").is_err() {
            continue;
        }
        slots -= 1;
        let app = app.clone();
        let handle = handle.clone();
        let id = task.id.clone();
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            if state.bt.unpause(&handle).await.is_err() {
                // 启动失败退回排队，等下一次轮询重试。
                let _ = state.db.set_download_state(&id, "queued");
            }
        });
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
                output_path: String::new(),
                playback_path: None,
            },
            true,
        ),
    };
    // 已有任务沿用原目录，避免改设置后旧任务找不到数据。
    let output_dir = if is_new {
        state.download_dir()
    } else {
        PathBuf::from(&task.output_path)
    };
    task.output_path = output_dir.to_string_lossy().into_owned();
    let restore_paused = task.state == "paused";
    let over_limit = {
        let settings = state.settings();
        settings.max_concurrent_downloads > 0
            && active_download_count(state) >= settings.max_concurrent_downloads as usize
    };
    // 排队任务以 paused 句柄加入会话，等有空位再 unpause。
    let add_paused = restore_paused || over_limit;
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
        paused: add_paused,
        output_folder: Some(output_dir.to_string_lossy().into_owned()),
        trackers: (!source.starts_with("magnet:") && !trackers.is_empty()).then_some(trackers),
        ..Default::default()
    };
    match state.bt.add_torrent(torrent_input, Some(options)).await {
        Ok(response) => {
            let Some(handle) = response.into_handle() else {
                state.db.set_download_state(&task.id, "failed")?;
                return Err("下载引擎未能创建任务，请重试".into());
            };
            state
                .handles
                .lock()
                .map_err(|e| e.to_string())?
                .insert(task.id.clone(), handle);
            task.state = if add_paused { "queued" } else { "downloading" }.into();
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
    if feed.rule.auto_download {
        let rule = (&feed.rule).into();
        for item in &inserted {
            if !matcher::resource_matches(&item.title, &rule) {
                continue;
            }
            let Some(source) = rss_download_source(item) else {
                continue;
            };
            // 单条失败不影响其余资源；失败任务会留在 RSS 列表里可手动重试。
            match start_download(source, item.title.clone(), "RSS 自动下载".into(), state).await {
                Ok(task) => {
                    let _ = state.db.mark_rss_downloaded(&item.guid, &task.id);
                }
                Err(error) => eprintln!("RSS 自动下载失败（{}）：{error}", item.title),
            }
        }
    }
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
fn list_downloads(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<DownloadTask>, String> {
    let mut tasks = state.db.list_downloads()?;
    let handles = state.handles.lock().map_err(|e| e.to_string())?;
    let mut speed_samples = state.speed_samples.lock().map_err(|e| e.to_string())?;
    let mut completed_now = false;
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
                completed_now = true;
                // 完成后可选自动暂停（停止做种），由设置控制。
                if state.settings().stop_seeding_on_complete {
                    let session = state.bt.clone();
                    let handle = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = session.pause(&handle).await;
                    });
                }
                // 完成时记住主视频路径：重启后任务句柄不再恢复，播放要靠这条记录。
                if task.playback_path.is_none() {
                    task.playback_path = largest_video_file(handle);
                }
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
    drop(speed_samples);
    drop(handles);
    if completed_now {
        promote_queued_downloads(&app);
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
async fn pause_download(
    app: tauri::AppHandle,
    id: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(&id)
        .cloned()
        .ok_or("任务不存在")?;
    state.bt.pause(&handle).await.map_err(|e| e.to_string())?;
    state.db.set_download_state(&id, "paused")?;
    // 手动暂停也会腾出下载槽位。
    promote_queued_downloads(&app);
    Ok(())
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
async fn delete_download(
    app: tauri::AppHandle,
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
    state.db.delete_download(&id)?;
    promote_queued_downloads(&app);
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaybackFile {
    index: usize,
    name: String,
    size: u64,
    path: String,
}

/// 从任务元数据里挑体积最大的视频文件（播放默认策略）。
fn largest_video_file(handle: &ManagedTorrent) -> Option<String> {
    handle
        .with_metadata(|metadata| {
            metadata
                .file_infos
                .iter()
                .filter(|file| is_video_file(&file.relative_filename))
                .max_by_key(|file| file.len)
                .map(|file| {
                    handle
                        .output_folder()
                        .join(&file.relative_filename)
                        .to_string_lossy()
                        .into_owned()
                })
        })
        .ok()
        .flatten()
}

#[tauri::command]
async fn download_playback_files(
    id: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PlaybackFile>, String> {
    let handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(&id)
        .cloned();
    if let Some(handle) = handle {
        return playback_files_from_handle(&handle);
    }
    let (source, output_path, stored) =
        state
            .db
            .download_by_id(&id)?
            .ok_or("任务不存在")?;
    // 完成时落库的主视频路径是最快的回退。
    if let Some(stored) = stored {
        let path = PathBuf::from(&stored);
        if path.is_file() {
            let size = path.metadata().map(|meta| meta.len()).unwrap_or(0);
            return Ok(vec![PlaybackFile {
                index: 0,
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| stored.clone()),
                size,
                path: stored,
            }]);
        }
        // 落库路径已失效（文件被移动），继续尝试恢复会话。
    }
    restore_playback_files(&id, &source, &output_path, &state).await
}

/// 重启后完成任务不会回到下载会话。这里以暂停状态把种子重新装回会话读取文件清单：
/// 需要联网获取元数据，并做一次磁盘校验（耗时与文件体积成正比）。
/// 成功后句柄留在会话内，本次及后续播放不再重复恢复；主视频路径同时落库。
async fn restore_playback_files(
    id: &str,
    source: &str,
    output_path: &str,
    state: &AppState,
) -> Result<Vec<PlaybackFile>, String> {
    let trackers = tracker_list(state).await;
    let (_, torrent_input) =
        prepare_torrent_input(source, &trackers, &state.client).await?;
    let options = AddTorrentOptions {
        overwrite: true,
        paused: true,
        output_folder: Some(output_path.to_owned()),
        ..Default::default()
    };
    let response = state
        .bt
        .add_torrent(torrent_input, Some(options))
        .await
        .map_err(|e| format!("读取种子信息失败：{e}"))?;
    let Some(handle) = response.into_handle() else {
        return Err("下载引擎未能读取该种子".into());
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        handle.wait_until_initialized(),
    )
    .await
    .map_err(|_| "读取种子信息超时，请稍后重试".to_string())?
    .map_err(|e| format!("读取种子信息失败：{e}"))?;
    state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .insert(id.to_owned(), handle.clone());
    let files = playback_files_from_handle(&handle)?;
    if let Some(largest) = largest_video_file(&handle) {
        let _ = state.db.set_playback_path(id, &largest);
    }
    Ok(files)
}

fn playback_files_from_handle(handle: &ManagedTorrent) -> Result<Vec<PlaybackFile>, String> {
    if !handle.stats().finished {
        return Err("下载尚未完成".into());
    }
    let mut files = handle
        .with_metadata(|metadata| {
            metadata
                .file_infos
                .iter()
                .enumerate()
                .filter(|(_, file)| is_video_file(&file.relative_filename))
                .map(|(index, file)| {
                    let path = handle.output_folder().join(&file.relative_filename);
                    PlaybackFile {
                        index,
                        name: file
                            .relative_filename
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| {
                                file.relative_filename.to_string_lossy().into_owned()
                            }),
                        size: file.len,
                        path: path.to_string_lossy().into_owned(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .map_err(|e| e.to_string())?;
    if files.is_empty() {
        return Err("该任务中没有可播放的视频文件".into());
    }
    // 多视频合集按文件名排序，选集时更像剧集列表。
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
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
    queue_collection_sync(&state, subject_id, &collection, None);
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
    queue_collection_sync(&state, subject_id, &collection, Some(watched));
    Ok(WatchProgress { collection, watched })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncStatus {
    pending: i64,
    last_error: Option<String>,
    last_attempt_at: Option<String>,
}

/// 收藏改动入队待写回 Bangumi。未连接 Token 时跳过（本地模式无同步）。
fn queue_collection_sync(state: &AppState, subject_id: i64, collection: &str, ep: Option<i64>) {
    if stored_access_token().is_none() {
        return;
    }
    if let Err(error) = state
        .db
        .enqueue_collection_sync(subject_id, collection, ep)
    {
        eprintln!("收藏改动入队失败：{error}");
        return;
    }
    state.sync_notify.notify_one();
}

/// 失败退避：1→2 分钟起步，翻倍到 60 分钟封顶。
fn sync_backoff_minutes(attempts: i64) -> i64 {
    1i64.max(60.min(1 << attempts.clamp(1, 6)))
}

/// 处理所有到期的待同步条目，返回剩余数量。Token 缺失时不动队列。
async fn process_sync_queue(state: &AppState) -> Result<i64, String> {
    let Some(token) = stored_access_token() else {
        return state.db.pending_sync_count();
    };
    for entry in state.db.due_sync_entries(50)? {
        match bangumi::set_collection(
            &state.client,
            &token,
            entry.subject_id,
            &entry.collection,
            entry.ep,
        )
        .await
        {
            Ok(()) => state.db.remove_sync_entry(entry.id)?,
            Err(error) => {
                let attempts = entry.attempts + 1;
                let next = (chrono::Utc::now()
                    + chrono::Duration::minutes(sync_backoff_minutes(attempts)))
                .to_rfc3339();
                state
                    .db
                    .mark_sync_attempt(entry.id, attempts, &next, &error)?;
            }
        }
    }
    state.db.pending_sync_count()
}

#[tauri::command]
fn get_sync_status(state: tauri::State<'_, AppState>) -> Result<SyncStatus, String> {
    let (pending, last_error, last_attempt_at) = state.db.sync_queue_summary()?;
    Ok(SyncStatus {
        pending,
        last_error,
        last_attempt_at,
    })
}

/// 立即重试全部待同步条目，返回最新队列状态。
#[tauri::command]
async fn retry_sync_now(state: tauri::State<'_, AppState>) -> Result<SyncStatus, String> {
    state.db.expire_all_sync_entries()?;
    state.sync_notify.notify_one();
    process_sync_queue(&state).await?;
    let (pending, last_error, last_attempt_at) = state.db.sync_queue_summary()?;
    Ok(SyncStatus {
        pending,
        last_error,
        last_attempt_at,
    })
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Result<AppSettings, String> {
    Ok(state.settings())
}

/// 保存设置并让可即时生效的部分（限速、开机启动、下载目录）立即应用。
/// 端口与连接数在会话创建时读取，重启后生效。
#[tauri::command]
fn save_settings(
    settings: AppSettings,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<AppSettings, String> {
    validate_settings(&settings)?;
    if let Some(dir) = &settings.download_dir {
        let path = PathBuf::from(dir);
        if !path.is_absolute() {
            return Err("下载目录必须是绝对路径".into());
        }
        std::fs::create_dir_all(&path).map_err(|e| format!("无法创建下载目录：{e}"))?;
    }
    store_settings(&state.db, &settings)?;
    state
        .bt
        .ratelimits
        .set_upload_bps(kbps_to_bps(settings.bt_upload_kbps));
    state
        .bt
        .ratelimits
        .set_download_bps(kbps_to_bps(settings.bt_download_kbps));
    if let Err(error) = apply_autostart(&app, settings.autostart) {
        eprintln!("开机启动设置未生效：{error}");
    }
    if let Ok(mut current) = state.settings.lock() {
        *current = settings.clone();
    }
    Ok(settings)
}

fn apply_autostart(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    let result = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    result.map_err(|e| e.to_string())
}

/// 打开本地文件/目录（下载目录与播放）。绕过前端 opener ACL，
/// 因为自定义下载目录不受 $DOWNLOAD/Mizuki/** 范围限制。
#[tauri::command]
fn open_local_path(path: String) -> Result<(), String> {
    let path = PathBuf::from(&path);
    if !path.exists() {
        return Err("文件或目录不存在，可能已被移动或删除".into());
    }
    tauri_plugin_opener::open_path(&path, None::<&str>).map_err(|e| e.to_string())
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
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            let download_path = app.path().download_dir()?.join("Mizuki");
            std::fs::create_dir_all(&download_path)?;
            let db = Database::open(&data.join("mizuki.sqlite3")).map_err(std::io::Error::other)?;
            let settings = load_settings(&db);
            if let Some(dir) = &settings.download_dir {
                // 自定义目录可能被用户移除，启动时静默重建。
                let _ = std::fs::create_dir_all(dir);
            }
            let bt = tauri::async_runtime::block_on(Session::new_with_opts(
                download_path.clone(),
                torrent_session_options(&settings),
            ))?;
            let client = reqwest::Client::builder()
                .user_agent(format!("Mizuki/{}", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(15))
                .build()?;
            app.manage(AppState {
                db,
                client,
                bt,
                handles: Mutex::new(HashMap::new()),
                speed_samples: Mutex::new(HashMap::new()),
                download_path,
                trackers: tokio::sync::RwLock::new(None),
                download_gate: tokio::sync::Mutex::new(()),
                sync_notify: tokio::sync::Notify::new(),
                settings: Mutex::new(settings.clone()),
            });
            if let Err(error) = apply_autostart(app.handle(), settings.autostart) {
                eprintln!("开机启动设置未生效：{error}");
            }

            // 同步队列 worker：处理到期的 Bangumi 写回，空闲时等通知或 45 秒轮询。
            let sync_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = sync_app.state::<AppState>();
                loop {
                    let _ = process_sync_queue(&state).await;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(45)) => {}
                        _ = state.sync_notify.notified() => {}
                    }
                }
            });
            let restore_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = restore_app.state::<AppState>();
                let restore_output_folder = state.download_dir().to_string_lossy().into_owned();
                let _guard = state.download_gate.lock().await;
                // 后台预热 TrackerList，避免第一次手动下载被网络探测阻塞。
                let trackers = tracker_list(&state).await;
                if let Ok(tasks) = state.db.restorable_downloads() {
                    for (id, source, previous_state) in tasks {
                        let options = AddTorrentOptions {
                            overwrite: true,
                            output_folder: Some(restore_output_folder.clone()),
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
                // 设置允许时关闭窗口仅隐藏到托盘；否则真正退出。
                let close_to_tray = window
                    .app_handle()
                    .try_state::<AppState>()
                    .map(|state| state.settings().close_to_tray)
                    .unwrap_or(true);
                if close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
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
            set_rss_feed_rules,
            delete_rss_feed,
            refresh_rss_feed,
            refresh_all_rss_feeds,
            download_rss_item,
            download_rss_items,
            list_downloads,
            add_download,
            pause_download,
            resume_download,
            delete_download,
            download_playback_files,
            open_local_path,
            get_settings,
            save_settings,
            set_collection,
            set_watch_progress,
            get_sync_status,
            retry_sync_now,
            get_comments,
            preview_rule_match
        ])
        .run(tauri::generate_context!())
        .expect("error while running Mizuki")
}
