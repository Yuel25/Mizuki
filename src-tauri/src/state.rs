//! 应用共享状态：所有命令与后台任务的依赖中枢。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use crate::db::Database;
use crate::settings::AppSettings;
use librqbit::{ManagedTorrent, Session};

pub(crate) struct AppState {
    pub(crate) db: Database,
    pub(crate) client: reqwest::Client,
    pub(crate) bt: Arc<Session>,
    pub(crate) handles: Mutex<HashMap<String, Arc<ManagedTorrent>>>,
    pub(crate) speed_samples: Mutex<HashMap<String, (u64, Instant)>>,
    pub(crate) download_path: PathBuf,
    pub(crate) trackers: tokio::sync::RwLock<Option<Vec<String>>>,
    pub(crate) download_gate: tokio::sync::Mutex<()>,
    pub(crate) sync_notify: tokio::sync::Notify,
    pub(crate) settings: Mutex<AppSettings>,
    /// 封面缓存目录（img_cache/{subject_id}.jpg）。
    pub(crate) cover_dir: PathBuf,
    /// Bangumi 详情拉取与封面下载共用的并发闸（避免卡片风暴打爆接口）。
    pub(crate) bangumi_gate: tokio::sync::Semaphore,
}

impl AppState {
    pub(crate) fn settings(&self) -> AppSettings {
        self.settings
            .lock()
            .map(|settings| settings.clone())
            .unwrap_or_default()
    }

    /// 新任务的输出目录：自定义目录或默认“下载\Mizuki”。
    pub(crate) fn download_dir(&self) -> PathBuf {
        self.settings()
            .download_dir
            .and_then(|dir| {
                let path = PathBuf::from(&dir);
                path.is_absolute().then_some(path)
            })
            .unwrap_or_else(|| self.download_path.clone())
    }
}
