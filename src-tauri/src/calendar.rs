//! 周表、搜索与条目详情：Bangumi 展示数据的获取、缓存与合并。

use std::collections::HashMap;

use crate::bangumi::{self, stored_access_token};
use crate::covers::{cover_candidate, cover_file_name, cover_local_url, spawn_cover_prefetch};
use crate::matcher;
use crate::models::Subject;
use crate::state::AppState;
use tauri::{AppHandle, State};

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CalendarPayload {
    subjects: Vec<Subject>,
    refreshed_at: Option<String>,
    stale: bool,
    warning: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchPayload {
    subjects: Vec<Subject>,
    local_only: bool,
    warning: Option<String>,
}

/// 条目详情缓存有效期。详情（评分/集数/简介）变化缓慢，一天足够新鲜。
const DETAIL_TTL: chrono::Duration = chrono::Duration::hours(24);

/// 周表缓存有效期：6 小时内不重复请求 Bangumi，启动更快、对源站更友好。
const CALENDAR_TTL: chrono::Duration = chrono::Duration::hours(6);
const CALENDAR_REFRESHED_KEY: &str = "calendar_refreshed_at";

/// Bangumi 周表的放送日是 JST（UTC+9）语义：徽章的“今天”与前端默认 Tab 统一按此计算。
pub(crate) fn jst_weekday_today(now: chrono::DateTime<chrono::Utc>) -> i64 {
    use chrono::Datelike;
    let jst = chrono::FixedOffset::east_opt(9 * 3600).expect("JST offset is valid");
    now.with_timezone(&jst).weekday().num_days_from_monday() as i64
}

/// 周表缓存是否仍新鲜：缺时间戳或无法解析都视为过期。
fn calendar_cache_fresh(refreshed_at: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    refreshed_at
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|stamp| now.signed_duration_since(stamp) < CALENDAR_TTL)
}

fn subject_from_cache(value: &serde_json::Value) -> Subject {
    let mut subject = serde_json::from_value::<Subject>(value.clone())
        .unwrap_or_else(|_| bangumi::subject_from_v0(value, None, 0));
    // 缓存里存的旧 JSON 可能还是 http 图片地址，读取时统一升级。
    let needs_upgrade = subject.image.as_deref().is_some_and(|image| {
        image.starts_with("http://lain.bgm.tv/") || image.starts_with("http://bgm.tv/")
    });
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
pub(crate) async fn get_calendar(
    app: AppHandle,
    force: Option<bool>,
    state: State<'_, AppState>,
) -> Result<CalendarPayload, String> {
    let force = force.unwrap_or(false);
    let mut refreshed_at = state.db.get_setting(CALENDAR_REFRESHED_KEY).ok().flatten();
    let (subjects, stale, warning) = if !force
        && calendar_cache_fresh(refreshed_at.as_deref(), chrono::Utc::now())
        && let Ok(cached) = state.db.cached_calendar()
        && !cached.is_empty()
    {
        (cached, false, None)
    } else {
        match bangumi::calendar(&state.client).await {
            Ok(fresh) => {
                state.db.save_calendar(&fresh)?;
                let now = chrono::Utc::now().to_rfc3339();
                let _ = state.db.set_setting(CALENDAR_REFRESHED_KEY, &now);
                refreshed_at = Some(now);
                (fresh, false, None)
            }
            Err(error) => match state.db.cached_calendar() {
                Ok(cached) if !cached.is_empty() => {
                    (cached, true, Some(format!("{error}，已显示上次缓存")))
                }
                _ => return Err(error),
            },
        }
    };
    let collections = state.db.collections()?;
    let mut subjects =
        merge_calendar_with_collections(subjects, state.db.cached_subjects()?, &collections);
    // 封面：在把 image 改写为本地协议地址之前收集远程地址，后台预取缺失文件。
    let cover_jobs: Vec<(i64, String)> = subjects
        .iter()
        .filter_map(|subject| {
            let url = subject.image.as_deref()?;
            url.starts_with("http")
                .then(|| (subject.id, url.to_owned()))
        })
        .collect();
    // 卡片更新徽章：订阅资源状态优先，无订阅时退回“今天放送”启发。
    let rss_overview = state.db.subject_rss_overview().unwrap_or_default();
    let today = jst_weekday_today(chrono::Utc::now());
    for subject in &mut subjects {
        subject.update_state = crate::subscriptions::compute_update_state(
            subject.collection.as_deref(),
            subject.watched,
            subject.episodes,
            subject.air_weekday,
            today,
            rss_overview.get(&subject.id),
        )
        .into();
        // 已缓存的封面直接走本地协议，网格不再逐张请求 Bangumi 图床。
        if let Some(url) = cover_local_url(
            subject.id,
            state.cover_dir.join(cover_file_name(subject.id)).is_file(),
        ) {
            subject.image = Some(url);
        }
    }
    spawn_cover_prefetch(&app, cover_jobs);
    Ok(CalendarPayload {
        subjects,
        refreshed_at,
        stale,
        warning,
    })
}

#[tauri::command]
pub(crate) async fn search_anime(
    keyword: String,
    limit: Option<u32>,
    state: State<'_, AppState>,
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
pub(crate) async fn get_comments(
    subject_id: i64,
    offset: u32,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    bangumi::comments(&state.client, subject_id, offset).await
}

/// 条目详情：24h 内直接用本地缓存（消除周表卡片 N+1）；
/// 网络失败时回退过期缓存，离线/限流时详情页与卡片补水仍可用。
#[tauri::command]
pub(crate) async fn get_subject_detail(
    app: AppHandle,
    subject_id: i64,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    if let Ok(Some((value, updated_at))) = state.db.cached_detail(subject_id) {
        let fresh = chrono::DateTime::parse_from_rfc3339(&updated_at)
            .ok()
            .is_some_and(|stamp| chrono::Utc::now().signed_duration_since(stamp) < DETAIL_TTL);
        if fresh {
            return Ok(value);
        }
    }
    let token = stored_access_token();
    let result = {
        let _permit = state
            .bangumi_gate
            .acquire()
            .await
            .map_err(|e| e.to_string())?;
        bangumi::subject(&state.client, token.as_deref(), subject_id).await
    };
    match result {
        Ok(value) => {
            let _ = state.db.save_detail(subject_id, &value);
            if let Some((id, url)) = cover_candidate(&value) {
                spawn_cover_prefetch(&app, vec![(id, url)]);
            }
            Ok(value)
        }
        Err(error) => match state.db.cached_detail(subject_id)? {
            Some((value, _)) => Ok(value),
            None => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CALENDAR_TTL, calendar_cache_fresh, jst_weekday_today, merge_calendar_with_collections,
    };
    use crate::bangumi::subject_from_v0;
    use crate::models::Subject;
    use std::collections::HashMap;

    #[test]
    fn calendar_cache_respects_ttl() {
        let now = chrono::Utc::now();
        let fresh = (now - chrono::Duration::hours(5)).to_rfc3339();
        let stale = (now - CALENDAR_TTL - chrono::Duration::minutes(1)).to_rfc3339();
        assert!(calendar_cache_fresh(Some(&fresh), now));
        assert!(
            !calendar_cache_fresh(Some(&stale), now),
            "超过 TTL 必须重新请求"
        );
        assert!(!calendar_cache_fresh(None, now), "从未刷新过不能走缓存");
        assert!(!calendar_cache_fresh(Some("not-a-date"), now));
    }

    #[test]
    fn jst_weekday_follows_utc_plus_nine() {
        // 2026-08-29 12:00 UTC = 周六 21:00 JST；15:00 UTC 已跨入 JST 周日。
        let sat = chrono::DateTime::parse_from_rfc3339("2026-08-29T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(jst_weekday_today(sat), 5);
        let sun = chrono::DateTime::parse_from_rfc3339("2026-08-29T15:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(jst_weekday_today(sun), 6);
    }

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
