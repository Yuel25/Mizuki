//! 下载任务生命周期：添加、排队调度、暂停/继续、播放文件与完成通知。

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::bt::{
    add_torrent_options, download_source_key, is_video_file, prepare_torrent_input, tracker_list,
};
use crate::models::{DownloadTask, StoredFileInfo};
use crate::state::AppState;
use librqbit::{ManagedTorrent, TorrentStatsState};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_notification::NotificationExt;

/// 正在占用并发下载槽位的任务数（仅统计活动 downloading 状态）。
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
        .filter(|task| handles.contains(&task.id) && task.state == "downloading")
        .count()
}

pub(crate) fn extract_stored_files(handle: &ManagedTorrent) -> Option<Vec<StoredFileInfo>> {
    handle
        .with_metadata(|metadata| {
            metadata
                .file_infos
                .iter()
                .map(|file| StoredFileInfo {
                    relative_path: file.relative_filename.to_string_lossy().into_owned(),
                    len: file.len,
                })
                .collect::<Vec<_>>()
        })
        .ok()
}

/// 有空闲槽位时把排队的任务（paused 句柄 + queued 状态）继续启动。
/// max_concurrent_downloads=0 表示不限，此时所有排队任务都直接启动。
pub(crate) async fn promote_queued_downloads_logic(state: &AppState) {
    let _guard = state.download_gate.lock().await;
    promote_queued_downloads_locked(state).await;
}

// 调用方必须持有 download_gate，覆盖读取槽位、更新状态及引擎操作。
async fn promote_queued_downloads_locked(state: &AppState) {
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
    let now = Instant::now();
    for task in tasks.iter().filter(|task| task.state == "queued") {
        if slots <= 0 {
            break;
        }
        let Some(handle) = handles.get(&task.id) else {
            continue;
        };
        // 启动失败退避策略：若近期启动失败，保持 10 秒冷却间隔，避免高频循环重试
        let cooled_down = state
            .promote_failures
            .lock()
            .map(|failures| {
                failures
                    .get(&task.id)
                    .map(|last_failed| now.duration_since(*last_failed).as_secs() >= 10)
                    .unwrap_or(true)
            })
            .unwrap_or(true);
        if !cooled_down {
            continue;
        }
        // 先占位再启动，避免轮询重入导致超额。
        if state
            .db
            .set_download_state(&task.id, "downloading")
            .is_err()
        {
            continue;
        }
        slots -= 1;
        if state.bt.unpause(handle).await.is_err() {
            // 启动失败退回排队，记录失败时间戳供退避使用
            if let Ok(mut failures) = state.promote_failures.lock() {
                failures.insert(task.id.clone(), Instant::now());
            }
            let _ = state.db.set_download_state(&task.id, "queued");
            slots += 1;
        } else if let Ok(mut failures) = state.promote_failures.lock() {
            failures.remove(&task.id);
        }
    }
}

pub(crate) fn promote_queued_downloads(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        promote_queued_downloads_logic(&state).await;
    });
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
pub(crate) async fn list_downloads(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<DownloadTask>, String> {
    let _guard = state.download_gate.lock().await;
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
                "completed".into()
            } else if matches!(stats.state, TorrentStatsState::Error) {
                "failed".into()
            } else if task.state == "queued" {
                // 如果应用层已经记录为排队状态，只要引擎没有转为活动（Live），就保持 queued，
                // 绝不能因为底层 handle 为 paused 就被覆盖为 paused！
                if matches!(stats.state, TorrentStatsState::Live) {
                    "downloading".into()
                } else {
                    "queued".into()
                }
            } else {
                match stats.state {
                    TorrentStatsState::Live => "downloading".into(),
                    TorrentStatsState::Paused => "paused".into(),
                    TorrentStatsState::Error => "failed".into(),
                    TorrentStatsState::Initializing { paused: true } => "paused".into(),
                    TorrentStatsState::Initializing { paused: false } => "downloading".into(),
                }
            };
            if let Some(files) = extract_stored_files(handle) {
                let _ = state.db.save_download_files(&task.id, &files);
            }
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
    _app: AppHandle,
    id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    pause_download_core(&state, &id).await
}

async fn pause_download_core(state: &AppState, id: &str) -> Result<(), String> {
    let _guard = state.download_gate.lock().await;
    let handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(id)
        .cloned()
        .ok_or("任务不存在")?;
    state.bt.pause(&handle).await.map_err(|e| e.to_string())?;
    state.db.set_download_state(&id, "paused")?;
    if let Ok(mut failures) = state.promote_failures.lock() {
        failures.remove(id);
    }
    // 手动暂停也会腾出下载槽位。
    promote_queued_downloads_locked(state).await;
    Ok(())
}

#[tauri::command]
pub(crate) async fn resume_download(
    _app: AppHandle,
    id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    resume_download_core(&state, &id).await
}

async fn resume_download_core(state: &AppState, id: &str) -> Result<(), String> {
    let _guard = state.download_gate.lock().await;
    if !state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .contains_key(id)
    {
        return Err("任务不存在".into());
    }
    if let Ok(mut failures) = state.promote_failures.lock() {
        failures.remove(id);
    }
    state.db.set_download_state(id, "queued")?;
    promote_queued_downloads_locked(state).await;
    Ok(())
}

fn delete_manifest_files(base: &Path, files: &[StoredFileInfo]) -> Result<(), String> {
    let root = match base.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("无法访问下载目录，任务已保留：{error}")),
    };
    let mut targets = Vec::new();
    // 删除前验证完整清单，拒绝绝对路径、.. 和指向目录外的链接。
    for file in files {
        let relative = Path::new(&file.relative_path);
        if relative.as_os_str().is_empty()
            || relative.components().any(|part| {
                !matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            return Err("文件清单含无效路径，任务已保留。".into());
        }
        let path = root.join(relative);
        match path.canonicalize() {
            Ok(resolved) if resolved.starts_with(&root) && resolved != root => targets.push(path),
            Ok(_) => return Err("文件清单路径超出下载目录，任务已保留。".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("无法验证文件路径，任务已保留：{error}")),
        }
    }
    let mut failures = Vec::new();
    let mut directories = std::collections::HashSet::new();
    for path in targets {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                let mut parent = path.parent();
                while let Some(dir) = parent {
                    if dir == root || !dir.starts_with(&root) {
                        break;
                    }
                    directories.insert(dir.to_path_buf());
                    parent = dir.parent();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "部分文件删除失败，任务和文件清单已保留以便重试：\n{}",
            failures.join("\n")
        ));
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in directories {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

pub(crate) async fn delete_download_core(
    state: &AppState,
    id: &str,
    delete_files: bool,
) -> Result<(), String> {
    let _guard = state.download_gate.lock().await;
    let mut details = state.db.download_files_and_paths(id)?.ok_or("任务不存在")?;
    let mut handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(id)
        .cloned();
    if delete_files {
        if let Some(current) = &handle {
            if let Some(files) = extract_stored_files(current) {
                state.db.save_download_files(id, &files)?;
                details.files = Some(files);
            }
        }
        if details.files.is_none() {
            handle = Some(restore_download_handle(id, &details.source, &details.output_path, state)
                .await.map_err(|error| format!("旧版任务缺少文件清单，恢复元数据失败（{error}）。已保留任务，请联网后重试或仅移除任务。"))?);
            details = state.db.download_files_and_paths(id)?.ok_or("任务不存在")?;
        }
        if details.files.as_ref().is_none_or(|files| files.is_empty()) {
            return Err("任务缺少完整文件清单，已保留任务以便重试。".into());
        }
    }
    // 只让引擎释放句柄；librqbit 的 delete(true) 不会返回逐文件删除错误。
    if let Some(handle) = handle {
        state
            .bt
            .delete(handle.id().into(), false)
            .await
            .map_err(|e| e.to_string())?;
        state.handles.lock().map_err(|e| e.to_string())?.remove(id);
    }
    if delete_files {
        // 删除失败后任务与清单仍在数据库，且不再占用下载槽位。
        state.db.set_download_state(id, "paused")?;
        if let Err(error) = delete_manifest_files(
            Path::new(&details.output_path),
            details.files.as_deref().ok_or("任务缺少文件清单")?,
        ) {
            promote_queued_downloads_locked(state).await;
            return Err(error);
        }
    }
    state.db.reset_rss_download_for_task(id)?;
    state.db.delete_download(id)?;
    if let Ok(mut samples) = state.speed_samples.lock() {
        samples.remove(id);
    }
    if let Ok(mut failures) = state.promote_failures.lock() {
        failures.remove(id);
    }
    promote_queued_downloads_locked(state).await;
    Ok(())
}

#[tauri::command]
pub(crate) async fn delete_download(
    _app: AppHandle,
    id: String,
    delete_files: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    delete_download_core(&state, &id, delete_files).await
}

pub(crate) async fn check_download_progress_and_promote_core(state: &AppState) -> Vec<String> {
    let mut completed = Vec::new();
    let _guard = state.download_gate.lock().await;
    let Ok(mut tasks) = state.db.list_downloads() else {
        return completed;
    };
    if let Ok(handles) = state.handles.lock() {
        for task in &mut tasks {
            let Some(handle) = handles.get(&task.id) else {
                continue;
            };
            let stats = handle.stats();
            if let Some(files) = extract_stored_files(handle) {
                let _ = state.db.save_download_files(&task.id, &files);
            }
            let was_completed = task.state == "completed";
            if stats.finished && !was_completed {
                task.state = "completed".into();
                task.progress = 1.0;
                task.down_speed = 0;
                task.up_speed = 0;
                if task.playback_path.is_none() {
                    task.playback_path = largest_video_file(handle);
                }
                let _ = state.db.update_download(task);

                completed.push(task.title.clone());
                if state.settings().stop_seeding_on_complete {
                    let session = state.bt.clone();
                    let handle = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = session.pause(&handle).await;
                    });
                }
            } else if matches!(stats.state, TorrentStatsState::Error) && task.state != "failed" {
                // 引擎进入错误状态时释放数据库活动槽位，速度清零
                task.state = "failed".into();
                task.down_speed = 0;
                task.up_speed = 0;
                let _ = state.db.update_download(task);
            }
        }
    }
    // 在有可用槽位时推进排队队列，不以“本轮存在新完成任务”为唯一调度条件。
    promote_queued_downloads_locked(state).await;
    completed
}

/// 后台常驻检查下载完成与错误状态并推进队列（脱离前端 Downloads 页面仍正常推进）。
pub(crate) async fn check_download_progress_and_promote(app: &AppHandle) {
    let state = app.state::<AppState>();
    for title in check_download_progress_and_promote_core(&state).await {
        if let Err(error) = app
            .notification()
            .builder()
            .title("下载完成")
            .body(title)
            .show()
        {
            log::warn!("系统通知发送失败：{error}");
        }
    }
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
    let _guard = state.download_gate.lock().await;
    let handle = state
        .handles
        .lock()
        .map_err(|e| e.to_string())?
        .get(&id)
        .cloned();
    if let Some(handle) = handle {
        if let Some(files) = extract_stored_files(&handle) {
            let _ = state.db.save_download_files(&id, &files);
        }
        return playback_files_from_handle(&handle);
    }
    let details = state
        .db
        .download_files_and_paths(&id)?
        .ok_or("任务不存在")?;
    let source = details.source;
    let output_path = details.output_path;
    let stored = details.playback_path;
    let stored_files = details.files;
    // 优先使用已持久化的文件清单，支持完全离线多视频合集选集。
    if let Some(stored_files) = stored_files {
        let base = Path::new(&output_path);
        let mut files: Vec<PlaybackFile> = stored_files
            .iter()
            .enumerate()
            .filter(|(_, file)| is_video_file(Path::new(&file.relative_path)))
            .map(|(index, file)| {
                let path = base.join(&file.relative_path);
                let name = Path::new(&file.relative_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.relative_path.clone());
                PlaybackFile {
                    index,
                    name,
                    size: file.len,
                    path: path.to_string_lossy().into_owned(),
                }
            })
            .collect();
        if !files.is_empty() {
            files.sort_by(|a, b| a.name.cmp(&b.name));
            return Ok(files);
        }
    }
    // 旧版未持久化清单的任务：尝试重新把种子装回会话读取元数据。
    match restore_playback_files(&id, &source, &output_path, &state).await {
        Ok(files) => Ok(files),
        Err(error) => {
            // 恢复会话失败（如离线/做种已断），回退到完成时记住的单视频主路径。
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
            }
            Err(format!(
                "读取合集文件列表失败（{error}），且未找到本地可播放文件"
            ))
        }
    }
}

/// 重启后完成任务不会回到下载会话。这里以暂停状态把种子重新装回会话读取文件清单：
/// 需要联网获取元数据，并做一次磁盘校验（耗时与文件体积成正比）。
/// 成功后句柄留在会话内，本次及后续播放不再重复恢复；主视频路径与完整文件清单同时落库。
async fn restore_playback_files(
    id: &str,
    source: &str,
    output_path: &str,
    state: &AppState,
) -> Result<Vec<PlaybackFile>, String> {
    let handle = restore_download_handle(id, source, output_path, state).await?;
    let files = playback_files_from_handle(&handle)?;
    if let Some(largest) = largest_video_file(&handle) {
        state.db.set_playback_path(id, &largest)?;
    }
    Ok(files)
}

async fn restore_download_handle(
    id: &str,
    source: &str,
    output_path: &str,
    state: &AppState,
) -> Result<std::sync::Arc<ManagedTorrent>, String> {
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
        std::time::Duration::from_secs(15),
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
    let stored_files = extract_stored_files(&handle).ok_or("无法读取完整文件清单")?;
    state.db.save_download_files(id, &stored_files)?;
    Ok(handle)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mizuki_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_delete_legacy_task_without_files_fails_when_metadata_cannot_restore() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        let dummy_video = dir.join("ep01.mp4");
        fs::write(&dummy_video, b"dummy video content").unwrap();

        let task = DownloadTask {
            id: "legacy_task_1".into(),
            title: "旧版合集测试".into(),
            episode: "01".into(),
            progress: 1.0,
            down_speed: 0,
            up_speed: 0,
            state: "completed".into(),
            output_path: dir.to_string_lossy().into_owned(),
            playback_path: Some(dummy_video.to_string_lossy().into_owned()),
        };
        // 插入旧版任务（无 files_json，且使用不可访问的种子源）
        state
            .db
            .add_download(
                &task,
                "http://127.0.0.1:9/unreachable.torrent",
                "http://127.0.0.1:9/unreachable.torrent",
            )
            .unwrap();

        // 尝试删除并勾选 delete_files=true
        let res = delete_download_core(&state, "legacy_task_1", true).await;
        assert!(res.is_err(), "无法恢复元数据时必须返回明确错误");
        let err = res.unwrap_err();
        assert!(
            err.contains("缺少文件清单") || err.contains("失败"),
            "错误信息必须提示无法恢复元数据与保留任务：{err}"
        );

        // 核心断言：绝不能只删除主视频就移除任务记录！
        assert!(dummy_video.exists(), "主视频文件必须保留");
        assert!(
            state.db.download_by_id("legacy_task_1").unwrap().is_some(),
            "任务记录在数据库中必须保留以便重试"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_delete_task_with_files_cleans_only_target_files_and_preserves_other_tasks() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        let shared_output = dir.join("shared_anime");
        let task1_subdir = shared_output.join("task1_dir");
        let task2_subdir = shared_output.join("task2_dir");
        fs::create_dir_all(&task1_subdir).unwrap();
        fs::create_dir_all(&task2_subdir).unwrap();

        let task1_ep1 = task1_subdir.join("ep01.mp4");
        let task1_ep2 = task1_subdir.join("ep02.mp4");
        let task2_ep1 = task2_subdir.join("ep01.mp4");
        let common_file = shared_output.join("common.txt");

        fs::write(&task1_ep1, b"t1e1").unwrap();
        fs::write(&task1_ep2, b"t1e2").unwrap();
        fs::write(&task2_ep1, b"t2e1").unwrap();
        fs::write(&common_file, b"keep me").unwrap();

        let task1 = DownloadTask {
            id: "task_1".into(),
            title: "Task 1".into(),
            episode: "01-02".into(),
            progress: 1.0,
            down_speed: 0,
            up_speed: 0,
            state: "completed".into(),
            output_path: shared_output.to_string_lossy().into_owned(),
            playback_path: Some(task1_ep1.to_string_lossy().into_owned()),
        };
        let task2 = DownloadTask {
            id: "task_2".into(),
            title: "Task 2".into(),
            episode: "01".into(),
            progress: 1.0,
            down_speed: 0,
            up_speed: 0,
            state: "completed".into(),
            output_path: shared_output.to_string_lossy().into_owned(),
            playback_path: Some(task2_ep1.to_string_lossy().into_owned()),
        };
        state
            .db
            .add_download(&task1, "magnet:?xt=urn:btih:111", "111")
            .unwrap();
        state
            .db
            .add_download(&task2, "magnet:?xt=urn:btih:222", "222")
            .unwrap();

        state
            .db
            .save_download_files(
                "task_1",
                &[
                    StoredFileInfo {
                        relative_path: "task1_dir/ep01.mp4".into(),
                        len: 4,
                    },
                    StoredFileInfo {
                        relative_path: "task1_dir/ep02.mp4".into(),
                        len: 4,
                    },
                ],
            )
            .unwrap();

        state
            .db
            .save_download_files(
                "task_2",
                &[StoredFileInfo {
                    relative_path: "task2_dir/ep01.mp4".into(),
                    len: 4,
                }],
            )
            .unwrap();

        // 执行删除 task1
        let res = delete_download_core(&state, "task_1", true).await;
        assert!(res.is_ok(), "删除 task1 应当成功");

        // 验证 task1 文件被清理，且其专属子目录被清理
        assert!(!task1_ep1.exists(), "task1 ep1 必须被删除");
        assert!(!task1_ep2.exists(), "task1 ep2 必须被删除");
        assert!(!task1_subdir.exists(), "task1 空子目录必须被删除");

        // 验证 task2 文件与共享目录完全不受影响！
        assert!(task2_ep1.exists(), "task2 文件绝不能被误删");
        assert!(task2_subdir.exists(), "task2 子目录绝不能被删除");
        assert!(common_file.exists(), "共享目录中的其他文件绝不能被误删");
        assert!(shared_output.exists(), "共享根目录绝不能被删除");

        // 验证数据库任务记录
        assert!(state.db.download_by_id("task_1").unwrap().is_none());
        assert!(state.db.download_by_id("task_2").unwrap().is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_delete_task_with_files_tolerates_already_deleted_files() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        let ep2 = dir.join("ep02.mp4");
        fs::write(&ep2, b"ep2 only").unwrap();
        // ep01.mp4 故意不创建（模拟用户先前手动删了或者重试重入）

        let task = DownloadTask {
            id: "task_tol".into(),
            title: "Tolerant Task".into(),
            episode: "01-02".into(),
            progress: 1.0,
            down_speed: 0,
            up_speed: 0,
            state: "completed".into(),
            output_path: dir.to_string_lossy().into_owned(),
            playback_path: Some(ep2.to_string_lossy().into_owned()),
        };
        state
            .db
            .add_download(&task, "magnet:?xt=urn:btih:333", "333")
            .unwrap();
        state
            .db
            .save_download_files(
                "task_tol",
                &[
                    StoredFileInfo {
                        relative_path: "ep01.mp4".into(),
                        len: 10,
                    },
                    StoredFileInfo {
                        relative_path: "ep02.mp4".into(),
                        len: 8,
                    },
                ],
            )
            .unwrap();

        let res = delete_download_core(&state, "task_tol", true).await;
        assert!(res.is_ok(), "重试或部分已删除文件应被容忍并成功完成删除");
        assert!(!ep2.exists(), "现存文件已被清除");
        assert!(state.db.download_by_id("task_tol").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_delete_task_without_delete_files_keeps_all_files_on_disk() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        let ep1 = dir.join("keep_ep01.mp4");
        fs::write(&ep1, b"keep content").unwrap();

        let task = DownloadTask {
            id: "task_keep".into(),
            title: "Keep Files Task".into(),
            episode: "01".into(),
            progress: 1.0,
            down_speed: 0,
            up_speed: 0,
            state: "completed".into(),
            output_path: dir.to_string_lossy().into_owned(),
            playback_path: Some(ep1.to_string_lossy().into_owned()),
        };
        state
            .db
            .add_download(&task, "magnet:?xt=urn:btih:444", "444")
            .unwrap();
        state
            .db
            .save_download_files(
                "task_keep",
                &[StoredFileInfo {
                    relative_path: "keep_ep01.mp4".into(),
                    len: 12,
                }],
            )
            .unwrap();

        let res = delete_download_core(&state, "task_keep", false).await;
        assert!(res.is_ok());
        assert!(ep1.exists(), "未勾选删除文件时，文件必须完整保留在磁盘");
        assert!(state.db.download_by_id("task_keep").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_promote_failure_backoff_and_manual_pause_protection() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;

        let task_queued = DownloadTask {
            id: "queued_1".into(),
            title: "Queued 1".into(),
            episode: "01".into(),
            progress: 0.0,
            down_speed: 0,
            up_speed: 0,
            state: "queued".into(),
            output_path: dir.to_string_lossy().into_owned(),
            playback_path: None,
        };
        let task_paused = DownloadTask {
            id: "paused_1".into(),
            title: "Manual Paused".into(),
            episode: "01".into(),
            progress: 0.5,
            down_speed: 0,
            up_speed: 0,
            state: "paused".into(),
            output_path: dir.to_string_lossy().into_owned(),
            playback_path: None,
        };
        state
            .db
            .add_download(&task_queued, "magnet:?xt=urn:btih:555", "555")
            .unwrap();
        state
            .db
            .add_download(&task_paused, "magnet:?xt=urn:btih:666", "666")
            .unwrap();

        // 模拟 queued_1 此前启动失败并记录退避冷却时间
        state
            .promote_failures
            .lock()
            .unwrap()
            .insert("queued_1".into(), Instant::now());

        // 触发调度
        promote_queued_downloads_logic(&state).await;

        let tasks = state.db.list_downloads().unwrap();
        let q1 = tasks.iter().find(|t| t.id == "queued_1").unwrap();
        let p1 = tasks.iter().find(|t| t.id == "paused_1").unwrap();
        assert_eq!(q1.state, "queued", "冷却中的排队任务不被调度");
        assert_eq!(p1.state, "paused", "手动暂停的任务绝不被自动调度拉起");

        // 模拟用户手动恢复操作清除失败记录
        state.promote_failures.lock().unwrap().remove("queued_1");
        assert!(
            state
                .promote_failures
                .lock()
                .unwrap()
                .get("queued_1")
                .is_none(),
            "恢复操作已清除退避记录"
        );
        let _ = fs::remove_dir_all(&dir);
    }
    async fn add_test_handle(
        state: &AppState,
        id: &str,
        status: &str,
    ) -> std::sync::Arc<ManagedTorrent> {
        let name = format!("{id}.mp4");
        let mut torrent = format!(
            "d4:infod6:lengthi4e4:name{}:{}12:piece lengthi16384e6:pieces20:",
            name.len(),
            name
        )
        .into_bytes();
        torrent.extend_from_slice(&[0; 20]);
        torrent.extend_from_slice(b"ee");
        let handle = state
            .bt
            .add_torrent(
                librqbit::AddTorrent::from_bytes(torrent),
                Some(add_torrent_options(true, &state.download_path, None)),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle.wait_until_initialized(),
        )
        .await
        .unwrap()
        .unwrap();
        state
            .db
            .add_download(
                &DownloadTask {
                    id: id.into(),
                    title: id.into(),
                    episode: "test".into(),
                    progress: 0.0,
                    down_speed: 0,
                    up_speed: 0,
                    state: status.into(),
                    output_path: state.download_path.to_string_lossy().into_owned(),
                    playback_path: None,
                },
                id,
                id,
            )
            .unwrap();
        state
            .handles
            .lock()
            .unwrap()
            .insert(id.into(), handle.clone());
        handle
    }

    #[tokio::test]
    async fn real_handles_serialize_resume_and_background_promotion() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        state.settings.lock().unwrap().max_concurrent_downloads = 1;
        let first = add_test_handle(&state, "first", "paused").await;
        let second = add_test_handle(&state, "second", "paused").await;
        let (a, b, ()) = tokio::join!(
            resume_download_core(&state, "first"),
            resume_download_core(&state, "second"),
            promote_queued_downloads_logic(&state)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(active_download_count(&state), 1);
        assert_eq!(
            [first.stats(), second.stats()]
                .iter()
                .filter(|s| matches!(s.state, TorrentStatsState::Live))
                .count(),
            1
        );
        tokio::join!(
            pause_download_core(&state, "first"),
            promote_queued_downloads_logic(&state)
        )
        .0
        .unwrap();
        assert_eq!(
            state
                .db
                .list_downloads()
                .unwrap()
                .iter()
                .find(|t| t.id == "first")
                .unwrap()
                .state,
            "paused"
        );
        assert!(matches!(first.stats().state, TorrentStatsState::Paused));
        state.bt.stop().await;
        drop(state);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn real_handle_cooldown_expires_and_background_starts_task() {
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        let handle = add_test_handle(&state, "cooldown", "queued").await;
        state
            .promote_failures
            .lock()
            .unwrap()
            .insert("cooldown".into(), Instant::now());
        check_download_progress_and_promote_core(&state).await;
        assert!(matches!(handle.stats().state, TorrentStatsState::Paused));
        state.promote_failures.lock().unwrap().insert(
            "cooldown".into(),
            Instant::now() - std::time::Duration::from_secs(11),
        );
        check_download_progress_and_promote_core(&state).await;
        assert_eq!(active_download_count(&state), 1);
        assert!(matches!(handle.stats().state, TorrentStatsState::Live));
        assert!(
            !state
                .promote_failures
                .lock()
                .unwrap()
                .contains_key("cooldown")
        );
        state.bt.stop().await;
        drop(state);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn real_handle_delete_failure_preserves_manifest_and_retry_succeeds() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_test_dir();
        let state = AppState::test_state(&dir).await;
        add_test_handle(&state, "locked", "paused").await;
        let path = dir.join("locked.mp4");
        // 允许引擎读写，但禁止 Windows 删除共享，确定性触发文件占用错误。
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        let result = delete_download_core(&state, "locked", true).await;
        assert!(result.is_err(), "文件占用必须报告失败");
        assert!(path.exists());
        assert!(
            state
                .db
                .download_files_and_paths("locked")
                .unwrap()
                .unwrap()
                .files
                .is_some()
        );
        assert!(!state.handles.lock().unwrap().contains_key("locked"));
        drop(locked);
        delete_download_core(&state, "locked", true).await.unwrap();
        assert!(!path.exists());
        assert!(
            state
                .db
                .download_files_and_paths("locked")
                .unwrap()
                .is_none()
        );
        state.bt.stop().await;
        drop(state);
        let _ = fs::remove_dir_all(dir);
    }
}
