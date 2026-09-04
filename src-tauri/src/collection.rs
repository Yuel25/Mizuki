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
/// 待同步队列中的本地改动不会被云端旧数据覆盖。
pub(crate) async fn import_bangumi_collections(app: &AppHandle) -> Result<usize, String> {
    let state = app.state::<AppState>();
    let (count, cover_jobs) = import_collections_with_fetch(&state, || async {
        let token = stored_access_token().ok_or("请先填写 Bangumi Access Token")?;
        let profile = bangumi::profile(&state.client, &token).await?;
        let username = profile
            .get("username")
            .and_then(|v| v.as_str())
            .ok_or("无法识别 Bangumi 用户")?;
        bangumi::collections(&state.client, &token, username).await
    })
    .await?;
    spawn_cover_prefetch(app, cover_jobs);
    Ok(count)
}

// 可注入网络传输，但导入与锁的实际执行路径在生产和测试中一致。
async fn import_collections_with_fetch<F, Fut>(
    state: &AppState,
    fetch: F,
) -> Result<(usize, Vec<(i64, String)>), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<serde_json::Value>, String>>,
{
    let _guard = state.sync_gate.lock().await;
    let items = fetch().await?;
    let mut cover_jobs = Vec::new();
    let mut parsed_items = Vec::new();
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
        parsed_items.push((id, collection.to_string(), watched));
        if let Some(subject) = item.get("subject") {
            state.db.cache_subject(id, subject)?;
            if let Some(candidate) = cover_candidate(subject) {
                cover_jobs.push(candidate);
            }
        }
    }
    state.db.import_cloud_collections(&parsed_items)?;
    Ok((items.len(), cover_jobs))
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
    let enqueue = stored_access_token().is_some();
    state
        .db
        .set_collection_and_sync(subject_id, &collection, enqueue)?;
    if let Some(subject) = subject {
        let payload = serde_json::to_value(subject).map_err(|e| e.to_string())?;
        state.db.cache_subject(subject_id, &payload)?;
    }
    if enqueue {
        state.sync_notify.notify_one();
    }
    Ok(())
}

/// 更新观看集数：先写本地，再带 `ep` 写回 Bangumi。
/// 条目还没有收藏状态时视为“在看”。
/// 本地与队列更新在同一事务内完成，保留收藏状态。
#[tauri::command]
pub(crate) fn set_watch_progress(
    subject_id: i64,
    watched: i64,
    state: State<'_, AppState>,
) -> Result<WatchProgress, String> {
    if !(0..=2000).contains(&watched) {
        return Err("观看集数无效".into());
    }
    let enqueue = stored_access_token().is_some();
    let collection = state
        .db
        .set_progress_and_sync(subject_id, watched, enqueue)?;
    if enqueue {
        state.sync_notify.notify_one();
    }
    Ok(WatchProgress {
        collection,
        watched,
    })
}

/// 失败退避：1→2 分钟起步，翻倍到 60 分钟封顶。
fn sync_backoff_minutes(attempts: i64) -> i64 {
    1i64.max(60.min(1 << attempts.clamp(1, 6)))
}

/// 处理所有到期的待同步条目，返回剩余数量。Token 缺失时不动队列。
/// 通过 sync_gate 确保后台任务与手动重试互斥执行。
/// 删除和重试状态均带版本号校验，防止覆盖并发新改动。
pub(crate) async fn process_sync_queue(state: &AppState) -> Result<i64, String> {
    let Some(token) = stored_access_token() else {
        return state.db.pending_sync_count();
    };
    process_sync_queue_with_sender(state, |id, collection, ep| {
        let token = token.clone();
        async move { bangumi::set_collection(&state.client, &token, id, &collection, ep).await }
    })
    .await
}

async fn process_sync_queue_with_sender<F, Fut>(
    state: &AppState,
    mut send: F,
) -> Result<i64, String>
where
    F: FnMut(i64, String, Option<i64>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let _guard = state.sync_gate.lock().await;
    for entry in state.db.due_sync_entries(50)? {
        match send(entry.subject_id, entry.collection.clone(), entry.ep).await {
            Ok(()) => {
                let _ = state.db.remove_sync_entry(entry.id, entry.version)?;
            }
            Err(error) => {
                let attempts = entry.attempts + 1;
                let next = (chrono::Utc::now()
                    + chrono::Duration::minutes(sync_backoff_minutes(attempts)))
                .to_rfc3339();
                let _ =
                    state
                        .db
                        .mark_sync_attempt(entry.id, entry.version, attempts, &next, &error)?;
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
    process_sync_queue(&state).await?;
    let (pending, last_error, last_attempt_at) = state.db.sync_queue_summary()?;
    Ok(SyncStatus {
        pending,
        last_error,
        last_attempt_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mizuki_coll_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_import_protection_under_concurrent_edits_and_sync_queue() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;

        // 1. 本地初始化条目 501: 在看 3 集，并入队
        let _ = state
            .db
            .set_collection_and_sync(501, "doing", true)
            .unwrap();
        let _ = state.db.set_progress_and_sync(501, 3, true).unwrap();

        // 2. 模拟云端导入开始，持有 sync_gate（模拟从 Bangumi 发起拉取）
        let sync_guard = state.sync_gate.lock().await;

        // 3. 在导入拉取期间，用户在本地继续观看了第 4 集与第 5 集
        let _ = state.db.set_progress_and_sync(501, 4, true).unwrap();
        let _ = state.db.set_progress_and_sync(501, 5, true).unwrap();

        // 4. 后台或重试尝试执行 process_sync_queue，由于 sync_gate 被导入持有，写回被互斥保护（无法清空队列）
        assert!(
            state.sync_gate.try_lock().is_err(),
            "sync_gate 必须正在锁定，队列不能在导入期间被删除"
        );

        // 5. 模拟导入从云端拉取到了旧数据快照（云端当时还是 0 集、想看 wish）
        let cloud_snapshot = vec![(501, "wish".to_string(), 0)];
        let imported = state.db.import_cloud_collections(&cloud_snapshot).unwrap();
        assert_eq!(imported, 0, "存在本地待同步修改时，导入事务必须跳过该条目");

        // 6. 验证本地状态依然是最新修改的第 5 集与 doing，决不被云端旧快照覆盖！
        let (coll, watched) = state.db.collection_state(501).unwrap().unwrap();
        assert_eq!(coll, "doing");
        assert_eq!(watched, 5);

        // 7. 导入完成，释放锁
        drop(sync_guard);

        // 8. 此时 sync_gate 恢复空闲，队列中的最新记录等待写回
        assert!(state.sync_gate.try_lock().is_ok());
        let entries = state.db.due_sync_entries(10).unwrap();
        let entry = entries.into_iter().find(|e| e.subject_id == 501).unwrap();
        assert_eq!(entry.collection, "doing");
        assert_eq!(entry.ep, Some(5));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_import_preserves_retrying_entry() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;

        // 本地条目 502：已有失败重试记录（attempts=2, next_attempt_at 在未来）
        let _ = state.db.set_progress_and_sync(502, 8, true).unwrap();
        let entry = state
            .db
            .due_sync_entries(10)
            .unwrap()
            .into_iter()
            .find(|e| e.subject_id == 502)
            .unwrap();
        let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        state
            .db
            .mark_sync_attempt(entry.id, entry.version, 2, &future, "network timeout")
            .unwrap();

        // 触发云端导入，云端带回旧数据 0 集
        let cloud_snapshot = vec![(502, "doing".to_string(), 0)];
        let imported = state.db.import_cloud_collections(&cloud_snapshot).unwrap();
        assert_eq!(
            imported, 0,
            "即便条目正处于重试退避期，也绝不被云端旧数据覆盖"
        );

        let (_, watched) = state.db.collection_state(502).unwrap().unwrap();
        assert_eq!(watched, 8, "本地重试进度完整保留");

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[tokio::test]
    async fn real_import_and_worker_paths_serialize_controlled_transport() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        state.db.set_progress_and_sync(701, 3, true).unwrap();
        let fetched = tokio::sync::Notify::new();
        let release = tokio::sync::Notify::new();
        let cloud_ep = std::sync::atomic::AtomicI64::new(0);
        let import = import_collections_with_fetch(&state, || async {
            fetched.notify_one();
            release.notified().await;
            Ok(vec![
                serde_json::json!({"subject_id":701,"type":1,"ep_status":0}),
            ])
        });
        let worker = async {
            fetched.notified().await;
            state.db.set_progress_and_sync(701, 5, true).unwrap();
            // 直接运行生产 worker 的控制路径；拉取未释放时发送回调不能执行。
            let send = process_sync_queue_with_sender(&state, |_, _, ep| {
                cloud_ep.store(ep.unwrap(), std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok(()))
            });
            tokio::pin!(send);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), &mut send)
                    .await
                    .is_err()
            );
            assert_eq!(cloud_ep.load(std::sync::atomic::Ordering::SeqCst), 0);
            release.notify_one();
            send.await.unwrap();
        };
        let (result, ()) = tokio::join!(import, worker);
        result.unwrap();
        assert_eq!(state.db.collection_state(701).unwrap().unwrap().1, 5);
        assert_eq!(cloud_ep.load(std::sync::atomic::Ordering::SeqCst), 5);
        assert_eq!(state.db.pending_sync_count().unwrap(), 0);
        // 网络失败必须释放互斥锁，后续写回仍可执行。
        assert!(
            import_collections_with_fetch(&state, || async { Err("network failure".into()) })
                .await
                .is_err()
        );
        assert!(state.sync_gate.try_lock().is_ok());
        state.bt.stop().await;
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }
}
