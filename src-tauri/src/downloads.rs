//! 下载任务生命周期：添加、排队调度、暂停/继续、播放文件与完成通知。

use std::path::PathBuf;
use std::time::Instant;

use crate::bt::{
    add_torrent_options, download_source_key, is_video_file, prepare_torrent_input, tracker_list,
};
use crate::models::DownloadTask;
use crate::state::AppState;
use librqbit::{ManagedTorrent, TorrentStatsState};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_notification::NotificationExt;

/// 正在占用下载槽位的任务数（排队中的也算，防止超卖）。
fn active_download_count(state: &AppState) -> usize {
    let Ok(tasks) = state.db.list_downloads() else {
        return 0;
    };
    let handles = state
        .handles
        .lock()
        .map(|handles| {
            handles
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    tasks
        .iter()
        .filter(|task| {
            handles.contains(&task.id) && (task.state == "downloading" || task.state == "queued")
        })
        .count()
}

/// 有空闲槽位时把排队的任务（paused 句柄 + queued 状态）继续启动。
/// max_concurrent_downloads=0 表示不限，此时所有排队任务都直接启动。
pub(crate) fn promote_queued_downloads(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings();
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
    let mut slots = if settings.max_concurrent_downloads == 0 {
        i64::MAX
    } else {
        settings.max_concurrent_downloads as i64 - active
    };
    for task in tasks.iter().filter(|task| task.state == "queued") {
        if slots <= 0 {
            break;
        }
        let Some(handle) = handles.get(&task.id) else {
            continue;
        };
        // 先占位再启动，避免轮询重入导致超额。
        if state
            .db
            .set_download_state(&task.id, "downloading")
            .is_err()
        {
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

pub(crate) async fn start_download(
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
    let options = add_torrent_options(
        add_paused,
        &output_dir,
        (!source.starts_with("magnet:") && !trackers.is_empty()).then_some(trackers),
    );
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
            // 用户手动暂停的任务保持 paused（不被排队逻辑吞掉）；
            // 超出并发限制的新任务以 queued 等待 promote 续跑。
            task.state = if restore_paused {
                "paused".into()
            } else if add_paused {
                "queued".into()
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

#[tauri::command]
pub(crate) fn list_downloads(
    app: AppHandle,
    state: State<'_, AppState>,
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
                // 只在任务真正完成的那一刻通知一次（was_completed 防止轮询重复）。
                if let Err(error) = app
                    .notification()
                    .builder()
                    .title("下载完成")
                    .body(&task.title)
                    .show()
                {
                    log::warn!("系统通知发送失败：{error}");
                }
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
pub(crate) async fn add_download(
    source: String,
    title: String,
    episode: String,
    state: State<'_, AppState>,
) -> Result<DownloadTask, String> {
    start_download(&source, title, episode, &state).await
}

#[tauri::command]
pub(crate) async fn pause_download(
    app: AppHandle,
    id: String,
    state: State<'_, AppState>,
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
pub(crate) async fn resume_download(id: String, state: State<'_, AppState>) -> Result<(), String> {
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
pub(crate) async fn delete_download(
    app: AppHandle,
    id: String,
    delete_files: bool,
    state: State<'_, AppState>,
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
    if let Ok(mut samples) = state.speed_samples.lock() {
        samples.remove(&id);
    }
    promote_queued_downloads(&app);
    Ok(())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaybackFile {
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
pub(crate) async fn download_playback_files(
    id: String,
    state: State<'_, AppState>,
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
    let (source, output_path, stored) = state.db.download_by_id(&id)?.ok_or("任务不存在")?;
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
    let (_, torrent_input) = prepare_torrent_input(source, &trackers, &state.client).await?;
    let options = add_torrent_options(true, std::path::Path::new(output_path), None);
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

/// 打开本地文件/目录（下载目录与播放）。绕过前端 opener ACL，
/// 因为自定义下载目录不受 $DOWNLOAD/Mizuki/** 范围限制。
#[tauri::command]
pub(crate) fn open_local_path(path: String) -> Result<(), String> {
    let path = PathBuf::from(&path);
    if !path.exists() {
        return Err("文件或目录不存在，可能已被移动或删除".into());
    }
    tauri_plugin_opener::open_path(&path, None::<&str>).map_err(|e| e.to_string())
}
