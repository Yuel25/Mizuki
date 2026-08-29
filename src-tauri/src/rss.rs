//! RSS 订阅：feed 管理、刷新、匹配规则、追番订阅与资源下载入口。

use std::collections::HashMap;

use crate::bt::download_source_key;
use crate::downloads::start_download;
use crate::models::{BatchDownloadResult, DownloadTask, FeedRule, RssFeed, RssItem};
use crate::state::AppState;
use crate::{feeds, matcher, subscriptions};
use tauri::State;

#[tauri::command]
pub(crate) async fn add_rss_feed(
    url: String,
    state: State<'_, AppState>,
) -> Result<RssFeed, String> {
    let (title, items) = feeds::fetch(&state.client, &url).await?;
    let feed = RssFeed {
        id: uuid::Uuid::new_v4().to_string(),
        title,
        url,
        enabled: true,
        last_checked_at: Some(chrono::Utc::now().to_rfc3339()),
        rule: FeedRule::default(),
        subject_id: None,
    };
    state.db.add_feed(&feed)?;
    state.db.insert_rss_items(&feed.id, &items)?;
    Ok(feed)
}

/// 追番订阅：按 Bangumi 条目创建 Mikan 单番 RSS，新集经规则自动下载。
/// 订阅时已有的资源只入列不自动下载（避免补全库式批量下载）。
#[tauri::command]
pub(crate) async fn subscribe_subject(
    subject_id: i64,
    name: String,
    name_cn: String,
    subtitle_group: Option<String>,
    state: State<'_, AppState>,
) -> Result<RssFeed, String> {
    if let Some(existing) = state.db.feed_for_subject(subject_id)? {
        return Ok(existing);
    }
    let url = subscriptions::build_mikan_rss_url(subject_id);
    // 用户可能早已手动添加过同一单番地址：直接收编为追番订阅，避免 UNIQUE 冲突。
    if let Some(mut existing) = state.db.feed_by_url(&url)? {
        state
            .db
            .adopt_feed_as_subscription(&existing.id, subject_id)?;
        existing.subject_id = Some(subject_id);
        existing.enabled = true;
        return Ok(existing);
    }
    // 先验证 RSS 可读再落库，避免留下永远刷不动的订阅。
    let (mut title, items) = feeds::fetch(&state.client, &url).await?;
    if title.trim().is_empty() {
        title = if name_cn.trim().is_empty() {
            name
        } else {
            name_cn
        };
    }
    let feed = RssFeed {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.trim().to_owned(),
        url,
        enabled: true,
        last_checked_at: Some(chrono::Utc::now().to_rfc3339()),
        rule: subscriptions::default_subscription_rule(subtitle_group.as_deref()),
        subject_id: Some(subject_id),
    };
    state.db.add_feed(&feed)?;
    state.db.insert_rss_items(&feed.id, &items)?;
    Ok(feed)
}

#[tauri::command]
pub(crate) fn unsubscribe_subject(
    subject_id: i64,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.db.delete_feeds_for_subject(subject_id)
}

#[tauri::command]
pub(crate) fn set_rss_feed_rules(
    id: String,
    rule: FeedRule,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if rule.includes.len() > 20 || rule.excludes.len() > 20 {
        return Err("规则关键词过多（最多 20 个）".into());
    }
    state.db.update_feed_rule(&id, &rule)
}

#[tauri::command]
pub(crate) fn list_rss_feeds(state: State<'_, AppState>) -> Result<Vec<RssFeed>, String> {
    state.db.list_feeds()
}

#[tauri::command]
pub(crate) fn list_rss_items(
    feed_id: Option<String>,
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<RssItem>, String> {
    let mut items = state
        .db
        .list_rss_items(feed_id.as_deref(), limit.unwrap_or(100).min(500))?;
    let handles = state.handles.lock().map_err(|e| e.to_string())?;
    let rules: HashMap<String, FeedRule> = state
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
pub(crate) fn set_rss_feed_enabled(
    id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.db.set_feed_enabled(&id, enabled)
}

#[tauri::command]
pub(crate) fn delete_rss_feed(id: String, state: State<'_, AppState>) -> Result<(), String> {
    state.db.delete_feed(&id)
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
            match start_download(source, item.title.clone(), "RSS 自动下载".into(), state).await
            {
                Ok(task) => {
                    let _ = state.db.mark_rss_downloaded(&item.guid, &task.id);
                }
                Err(error) => log::warn!("RSS 自动下载失败（{}）：{error}", item.title),
            }
        }
    }
    Ok(inserted)
}

#[tauri::command]
pub(crate) async fn refresh_rss_feed(
    id: String,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let feed = state.db.feed(&id)?;
    if !feed.enabled {
        return Ok(0);
    }
    Ok(refresh_feed(&feed, &state).await?.len())
}

#[tauri::command]
pub(crate) async fn refresh_all_rss_feeds(state: State<'_, AppState>) -> Result<usize, String> {
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

fn rss_download_source(item: &RssItem) -> Option<&str> {
    item.torrent.as_deref().or_else(|| {
        let link = item.link.as_str();
        (link.starts_with("magnet:") || link.to_ascii_lowercase().ends_with(".torrent"))
            .then_some(link)
    })
}

#[tauri::command]
pub(crate) async fn download_rss_item(
    guid: String,
    state: State<'_, AppState>,
) -> Result<DownloadTask, String> {
    let item = state.db.rss_item(&guid)?;
    let source = rss_download_source(&item).ok_or("该 RSS 条目没有可下载地址")?;
    let task = start_download(source, item.title.clone(), "RSS 手动下载".into(), &state).await?;
    state.db.mark_rss_downloaded(&guid, &task.id)?;
    Ok(task)
}

#[tauri::command]
pub(crate) async fn download_rss_items(
    guids: Vec<String>,
    state: State<'_, AppState>,
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
pub(crate) fn preview_rule_match(title: String, rule: matcher::MatchRule) -> bool {
    matcher::resource_matches(&title, &rule)
}
