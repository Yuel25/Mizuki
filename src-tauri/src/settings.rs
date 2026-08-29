//! 应用设置：持久化、校验与即时生效。

use crate::db::Database;
use crate::state::AppState;
use tauri::State;

/// 全部设置项。以 JSON 存在 settings 表（key=app_settings），
/// serde(default) 保证旧数据缺字段时逐项回落默认值。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct AppSettings {
    /// BT 监听端口，0 表示随机。重启应用后生效。
    pub(crate) bt_listen_port: u16,
    /// 上传/下载限速，KB/s；0 表示不限。
    pub(crate) bt_upload_kbps: u64,
    pub(crate) bt_download_kbps: u64,
    /// 每个 torrent 的最大连接 peer 数。重启后生效。
    pub(crate) bt_peer_limit: u32,
    /// 同时下载的任务数上限，0 表示不限；超出部分排队（paused + queued）。
    pub(crate) max_concurrent_downloads: u32,
    /// 下载完成后自动暂停任务（停止做种）。
    pub(crate) stop_seeding_on_complete: bool,
    pub(crate) rss_interval_minutes: u32,
    /// 自定义下载目录；None 表示系统下载目录下的 Mizuki。
    pub(crate) download_dir: Option<String>,
    pub(crate) autostart: bool,
    pub(crate) close_to_tray: bool,
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

pub(crate) fn load_settings(db: &Database) -> AppSettings {
    db.get_setting(SETTINGS_KEY)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub(crate) fn store_settings(db: &Database, settings: &AppSettings) -> Result<(), String> {
    let json = serde_json::to_string(settings).map_err(|e| e.to_string())?;
    db.set_setting(SETTINGS_KEY, &json)
}

pub(crate) fn kbps_to_bps(kbps: u64) -> Option<std::num::NonZeroU32> {
    if kbps == 0 || kbps > u32::MAX as u64 / 1024 {
        return None;
    }
    std::num::NonZeroU32::new((kbps * 1024) as u32)
}

pub(crate) fn validate_settings(settings: &AppSettings) -> Result<(), String> {
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

pub(crate) fn apply_autostart(app: &tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    let result = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    result.map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn get_settings(state: State<'_, AppState>) -> Result<AppSettings, String> {
    Ok(state.settings())
}

/// 保存设置并让可即时生效的部分（限速、开机启动、下载目录）立即应用。
/// 端口与连接数在会话创建时读取，重启后生效。
#[tauri::command]
pub(crate) fn save_settings(
    settings: AppSettings,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<AppSettings, String> {
    validate_settings(&settings)?;
    if let Some(dir) = &settings.download_dir {
        let path = std::path::PathBuf::from(dir);
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
        log::warn!("开机启动设置未生效：{error}");
    }
    if let Ok(mut current) = state.settings.lock() {
        *current = settings.clone();
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
