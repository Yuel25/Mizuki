//! Bangumi 收藏同步：本地收藏、观看进度、可靠写回队列与收藏导入。

use crate::bangumi::{self, stored_access_token};
use crate::covers::{cover_candidate, spawn_cover_prefetch};
use crate::models::Subject;
use crate::state::AppState;
use tauri::{AppHandle, Manager, State};

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WatchProgress {
    collection: String,
    watched: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncStatus {
    pending: i64,
    last_error: Option<String>,
    last_attempt_at: Option<String>,
}

/// 从 Bangumi 分页拉取全部动画收藏并写入本地库。手动同步与启动自动同步共用。
/// 完成后后台预取缺失封面。
pub(crate) async fn import_bangumi_collections(app: &AppHandle) -> Result<usize, String> {
    let state = app.state::<AppState>();
    let token = stored_access_token().ok_or("请先填写 Bangumi Access Token")?;
    let profile = bangumi::profile(&state.client, &token).await?;
    let username = profile
        .get("username")
        .and_then(|v| v.as_str())
        .ok_or("无法识别 Bangumi 用户")?;
    let items = bangumi::collections(&state.client, &token, username).await?;
    let mut cover_jobs = Vec::new();
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
            if let Some(candidate) = cover_candidate(subject) {
                cover_jobs.push(candidate);
            }
        }
    }
    spawn_cover_prefetch(app, cover_jobs);
    Ok(items.len())
}

#[tauri::command]
pub(crate) async fn sync_bangumi_collections(app: AppHandle) -> Result<usize, String> {
    import_bangumi_collections(&app).await
}

#[tauri::command]
pub(crate) fn set_collection(
    subject_id: i64,
    collection: String,
    subject: Option<Subject>,
    state: State<'_, AppState>,
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
pub(crate) fn set_watch_progress(
    subject_id: i64,
    watched: i64,
    state: State<'_, AppState>,
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
    Ok(WatchProgress {
        collection,
        watched,
    })
}

/// 收藏改动入队待写回 Bangumi。未连接 Token 时跳过（本地模式无同步）。
fn queue_collection_sync(state: &AppState, subject_id: i64, collection: &str, ep: Option<i64>) {
    if stored_access_token().is_none() {
        return;
    }
    if let Err(error) = state.db.enqueue_collection_sync(subject_id, collection, ep) {
        log::error!("收藏改动入队失败：{error}");
        return;
    }
    state.sync_notify.notify_one();
}

/// 失败退避：1→2 分钟起步，翻倍到 60 分钟封顶。
fn sync_backoff_minutes(attempts: i64) -> i64 {
    1i64.max(60.min(1 << attempts.clamp(1, 6)))
}

/// 处理所有到期的待同步条目，返回剩余数量。Token 缺失时不动队列。
pub(crate) async fn process_sync_queue(state: &AppState) -> Result<i64, String> {
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
pub(crate) fn get_sync_status(state: State<'_, AppState>) -> Result<SyncStatus, String> {
    let (pending, last_error, last_attempt_at) = state.db.sync_queue_summary()?;
    Ok(SyncStatus {
        pending,
        last_error,
        last_attempt_at,
    })
}

/// 立即重试全部待同步条目，返回最新队列状态。
#[tauri::command]
pub(crate) async fn retry_sync_now(state: State<'_, AppState>) -> Result<SyncStatus, String> {
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
