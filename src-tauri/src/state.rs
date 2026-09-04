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
    pub(crate) sync_gate: tokio::sync::Mutex<()>,
    pub(crate) sync_notify: tokio::sync::Notify,
    pub(crate) promote_failures: Mutex<HashMap<String, Instant>>,
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

    #[cfg(test)]
    pub(crate) async fn test_state(temp_dir: &std::path::Path) -> Self {
        let db = Database::open_in_memory().unwrap();
        let bt_opts = librqbit::SessionOptions {
            dht: None,
            listen: None,
            ..Default::default()
        };
        let bt = librqbit::Session::new_with_opts(temp_dir.to_path_buf(), bt_opts)
            .await
            .expect("bt session");
        Self {
            db,
            client: reqwest::Client::new(),
            bt,
            handles: Mutex::new(HashMap::new()),
            speed_samples: Mutex::new(HashMap::new()),
            download_path: temp_dir.to_path_buf(),
            trackers: tokio::sync::RwLock::new(None),
            download_gate: tokio::sync::Mutex::new(()),
            sync_gate: tokio::sync::Mutex::new(()),
            sync_notify: tokio::sync::Notify::new(),
            promote_failures: Mutex::new(HashMap::new()),
            settings: Mutex::new(AppSettings::default()),
            cover_dir: temp_dir.join("covers"),
            bangumi_gate: tokio::sync::Semaphore::new(4),
        }
    }
}
