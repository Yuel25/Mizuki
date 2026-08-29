//! Mizuki 桌面端入口：Tauri 构建、启动引导、托盘与命令注册。
//! 业务按域拆分在 settings/bt/downloads/rss/collection/calendar/covers 等模块。

mod bangumi;
mod bt;
mod calendar;
mod collection;
mod covers;
mod db;
mod downloads;
mod feeds;
mod matcher;
mod models;
mod rss;
mod settings;
mod state;
mod subscriptions;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use bangumi::stored_access_token;
use bt::{prepare_torrent_input, torrent_session_options, tracker_list};
use collection::{import_bangumi_collections, process_sync_queue};
use db::Database;
use downloads::promote_queued_downloads;
use librqbit::{AddTorrentOptions, Session};
use state::AppState;
use tauri::{
    Emitter, Manager, UriSchemeContext,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

static EXITING: AtomicBool = AtomicBool::new(false);

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
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("mizuki".into()),
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview),
                ])
                .build(),
        )
        // 本地封面协议：http://cover.localhost/{subject_id}.jpg，
        // 文件名严格校验纯数字防目录穿越；命中缓存后由 max-age 交给 WebView 缓存。
        .register_uri_scheme_protocol("cover", |ctx: UriSchemeContext<'_, tauri::Wry>, request| {
            let name = request.uri().path().trim_start_matches('/').to_owned();
            let cover_dir = ctx.app_handle().state::<AppState>().cover_dir.clone();
            let respond = |status: u16, body: Vec<u8>, content_type: &'static str| {
                tauri::http::Response::builder()
                    .status(status)
                    .header("Content-Type", content_type)
                    .header("Cache-Control", "max-age=604800")
                    .body(body)
                    .expect("static cover response")
            };
            if !covers::cover_file_name_is_valid(&name) {
                return respond(404, Vec::new(), "text/plain");
            }
            match std::fs::read(cover_dir.join(name)) {
                Ok(bytes) => respond(200, bytes, "image/jpeg"),
                Err(_) => respond(404, Vec::new(), "text/plain"),
            }
        })
        .setup(|app| {
            let data = app.path().app_data_dir()?;
            let download_path = app.path().download_dir()?.join("Mizuki");
            std::fs::create_dir_all(&download_path)?;
            let cover_dir = data.join("img_cache");
            std::fs::create_dir_all(&cover_dir)?;
            let db = Database::open(&data.join("mizuki.sqlite3")).map_err(std::io::Error::other)?;
            let settings = settings::load_settings(&db);
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
                cover_dir,
                bangumi_gate: tokio::sync::Semaphore::new(4),
            });
            if let Err(error) = settings::apply_autostart(app.handle(), settings.autostart) {
                log::warn!("开机启动设置未生效：{error}");
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
            // 启动自动同步：已连接 Bangumi 时每次启动拉取一次最新收藏，
            // 完成后通知前端刷新周表与追番页；未连接 Token 时静默跳过。
            let import_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if stored_access_token().is_none() {
                    return;
                }
                // 稍等启动高峰（周表请求、种子恢复）过去再同步，避免争抢网络。
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                match import_bangumi_collections(&import_app).await {
                    Ok(count) => {
                        let _ = import_app.emit("bangumi-imported", count);
                    }
                    Err(error) => log::error!("启动同步 Bangumi 收藏失败：{error}"),
                }
            });
            restore_downloads(app.handle());

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
                            // 只暂停真正进行中/排队的任务；已完成任务（如播放回退恢复的）
                            // 保持 completed，避免下次重启被当作未完成做无谓校验。
                            let pausable: std::collections::HashSet<String> = state
                                .db
                                .list_downloads()
                                .unwrap_or_default()
                                .iter()
                                .filter(|task| {
                                    task.state == "downloading" || task.state == "queued"
                                })
                                .map(|task| task.id.clone())
                                .collect();
                            for (id, handle) in handles {
                                if !pausable.contains(&id) {
                                    continue;
                                }
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
            log::info!("Mizuki {} 启动完成", env!("CARGO_PKG_VERSION"));
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
            calendar::get_calendar,
            calendar::search_anime,
            calendar::get_subject_detail,
            calendar::get_comments,
            bangumi::get_bangumi_profile,
            bangumi::save_bangumi_token,
            bangumi::remove_bangumi_token,
            collection::sync_bangumi_collections,
            collection::set_collection,
            collection::set_watch_progress,
            collection::get_sync_status,
            collection::retry_sync_now,
            rss::add_rss_feed,
            rss::subscribe_subject,
            rss::unsubscribe_subject,
            rss::list_rss_feeds,
            rss::list_rss_items,
            rss::set_rss_feed_enabled,
            rss::set_rss_feed_rules,
            rss::delete_rss_feed,
            rss::refresh_rss_feed,
            rss::refresh_all_rss_feeds,
            rss::download_rss_item,
            rss::download_rss_items,
            rss::preview_rule_match,
            downloads::list_downloads,
            downloads::add_download,
            downloads::pause_download,
            downloads::resume_download,
            downloads::delete_download,
            downloads::download_playback_files,
            downloads::open_local_path,
            settings::get_settings,
            settings::save_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Mizuki")
}

/// 重启后把未完成任务装回下载会话：全部以暂停装入，
/// 之后由 promote_queued_downloads 按“同时下载数”设置统一续跑。
fn restore_downloads(app: &tauri::AppHandle) {
    let restore_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = restore_app.state::<AppState>();
        let _guard = state.download_gate.lock().await;
        // 后台预热 TrackerList，避免第一次手动下载被网络探测阻塞。
        let trackers = tracker_list(&state).await;
        if let Ok(tasks) = state.db.restorable_downloads() {
            for (id, source, previous_state, output_path) in tasks {
                let output_folder = {
                    let dir = PathBuf::from(&output_path);
                    if output_path.is_empty() || !dir.is_absolute() {
                        state.download_dir().to_string_lossy().into_owned()
                    } else {
                        output_path.clone()
                    }
                };
                if previous_state == "downloading" {
                    // 恢复后统一走排队调度。
                    let _ = state.db.set_download_state(&id, "queued");
                }
                let input = prepare_torrent_input(&source, &trackers, &state.client).await;
                match input {
                    Ok((_, torrent_input)) => {
                        let options = AddTorrentOptions {
                            overwrite: true,
                            paused: true,
                            output_folder: Some(output_folder),
                            trackers: (!source.starts_with("magnet:") && !trackers.is_empty())
                                .then_some(trackers.clone()),
                            ..Default::default()
                        };
                        match state.bt.add_torrent(torrent_input, Some(options)).await {
                            Ok(response) => {
                                if let Some(handle) = response.into_handle() {
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
            promote_queued_downloads(&restore_app);
        }
    });
}
