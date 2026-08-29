//! BT 会话与种子源：librqbit 会话参数、tracker 列表与 magnet/.torrent 输入准备。

use crate::settings::AppSettings;
use crate::state::AppState;
use librqbit::{AddTorrent, AddTorrentOptions, ListenerMode, ListenerOptions, SessionOptions};

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "webm", "avi", "mov", "m4v", "ts"];

pub(crate) fn is_video_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

pub(crate) fn torrent_session_options(settings: &AppSettings) -> SessionOptions {
    SessionOptions {
        // Session::new() leaves the peer listener disabled in librqbit 9. That makes
        // every connection outbound-only and can substantially reduce the available
        // peer pool compared with full desktop clients such as qBittorrent.
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpAndUtp,
            enable_upnp_port_forwarding: true,
            listen_addr: std::net::SocketAddr::new(
                std::net::Ipv6Addr::UNSPECIFIED.into(),
                settings.bt_listen_port,
            ),
            announce_port: (settings.bt_listen_port > 0).then_some(settings.bt_listen_port),
            ..Default::default()
        }),
        // Keep enough candidates around for swarms where only a small fraction of
        // discovered peers are fast or reachable.
        peer_limit: Some(settings.bt_peer_limit as usize),
        ratelimits: librqbit::limits::LimitsConfig {
            upload_bps: crate::settings::kbps_to_bps(settings.bt_upload_kbps),
            download_bps: crate::settings::kbps_to_bps(settings.bt_download_kbps),
        },
        client_name_and_version: Some(format!("Mizuki/{}", env!("CARGO_PKG_VERSION"))),
        ..Default::default()
    }
}

const TRACKERLIST_URL: &str = "https://cf.trackerslist.com/best.txt";

/// 后台拉取公共 tracker 列表（带缓存），下载与恢复会话共用。
pub(crate) async fn tracker_list(state: &AppState) -> Vec<String> {
    if let Some(trackers) = state.trackers.read().await.as_ref() {
        return trackers.clone();
    }
    let trackers = match state
        .client
        .get(TRACKERLIST_URL)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
    {
        Ok(response) => response
            .text()
            .await
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .take(80)
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    };
    if !trackers.is_empty() {
        *state.trackers.write().await = Some(trackers.clone());
    }
    trackers
}

/// 下载任务的查重键：magnet 取 info-hash，HTTP 保留大小写敏感的路径与查询。
pub(crate) fn download_source_key(source: &str) -> String {
    crate::db::canonical_download_source_key(source)
}

fn magnet_with_trackers(source: &str, trackers: &[String]) -> String {
    if !source.starts_with("magnet:") {
        return source.to_owned();
    }
    let Ok(mut url) = url::Url::parse(source) else {
        return source.to_owned();
    };
    let existing: std::collections::HashSet<String> = url
        .query_pairs()
        .filter(|(key, _)| key == "tr")
        .map(|(_, value)| value.into_owned())
        .collect();
    {
        let mut query = url.query_pairs_mut();
        for tracker in trackers
            .iter()
            .filter(|tracker| !existing.contains(*tracker))
            .take(40)
        {
            query.append_pair("tr", tracker);
        }
    }
    url.into()
}

/// 把 magnet/.torrent URL 转成 librqbit 可接受的输入。
pub(crate) async fn prepare_torrent_input(
    source: &str,
    trackers: &[String],
    client: &reqwest::Client,
) -> Result<(String, AddTorrent<'static>), String> {
    if source.starts_with("magnet:") {
        let prepared = magnet_with_trackers(source, trackers);
        return Ok((prepared.clone(), AddTorrent::from_url(prepared)));
    }
    let response = client
        .get(source)
        .header(
            reqwest::header::ACCEPT,
            "application/x-bittorrent,*/*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| format!("获取种子文件失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("获取种子文件失败：{e}"))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("读取种子文件失败：{e}"))?;
    if bytes.is_empty() {
        return Err("种子文件为空".into());
    }
    if bytes.len() > 20 * 1024 * 1024 {
        return Err("种子文件超过 20 MB，已拒绝加载".into());
    }
    if bytes.first() != Some(&b'd') {
        return Err("订阅地址返回的不是有效 .torrent 文件".into());
    }
    Ok((source.to_owned(), AddTorrent::from_bytes(bytes)))
}

pub(crate) fn add_torrent_options(
    paused: bool,
    output_dir: &std::path::Path,
    trackers: Option<Vec<String>>,
) -> AddTorrentOptions {
    AddTorrentOptions {
        // Existing partial/complete files are opened without truncation and hash-checked for resume.
        overwrite: true,
        paused,
        output_folder: Some(output_dir.to_string_lossy().into_owned()),
        trackers,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::AppSettings;
    use librqbit::ListenerMode;

    #[test]
    fn desktop_session_accepts_incoming_tcp_and_utp_peers() {
        let options = torrent_session_options(&AppSettings::default());
        let listener = options.listen.expect("peer listener should be enabled");
        assert!(matches!(listener.mode, ListenerMode::TcpAndUtp));
        assert!(listener.enable_upnp_port_forwarding);
        assert_eq!(options.peer_limit, Some(256));
        assert_eq!(listener.listen_addr.port(), 0, "默认使用随机端口");
    }

    #[test]
    fn session_options_apply_bt_settings() {
        let settings = AppSettings {
            bt_listen_port: 4242,
            bt_upload_kbps: 512,
            bt_download_kbps: 0,
            bt_peer_limit: 120,
            ..Default::default()
        };
        let options = torrent_session_options(&settings);
        let listener = options.listen.expect("peer listener should be enabled");
        assert_eq!(listener.listen_addr.port(), 4242);
        assert_eq!(listener.announce_port, Some(4242));
        assert_eq!(options.peer_limit, Some(120));
        assert_eq!(
            options.ratelimits.upload_bps.map(|v| v.get()),
            Some(512 * 1024)
        );
        assert_eq!(options.ratelimits.download_bps, None, "0 表示不限速");
    }

    #[test]
    fn magnet_dedup_key_ignores_name_and_trackers() {
        let first = "magnet:?xt=urn:btih:ABCDEF&dn=Episode+1&tr=udp%3A%2F%2Fold.example%3A80";
        let second = "magnet:?tr=udp%3A%2F%2Fnew.example%3A80&xt=urn:btih:abcdef&dn=Other";
        assert_eq!(download_source_key(first), download_source_key(second));
    }

    #[test]
    fn http_source_key_preserves_case_sensitive_path_and_query() {
        let first = "https://example.com/A.torrent?token=X";
        let second = "https://example.com/a.torrent?token=x";
        assert_ne!(download_source_key(first), download_source_key(second));
    }

    #[test]
    fn trackerlist_entries_are_added_without_duplicates() {
        let source = "magnet:?xt=urn:btih:abcdef&tr=udp%3A%2F%2Fexisting.example%3A80";
        let trackers = vec![
            "udp://existing.example:80".to_owned(),
            "https://new.example/announce".to_owned(),
        ];
        let prepared = magnet_with_trackers(source, &trackers);
        let url = url::Url::parse(&prepared).expect("valid magnet URL");
        let actual: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "tr")
            .map(|(_, value)| value.into_owned())
            .collect();
        assert_eq!(
            actual,
            vec![
                "udp://existing.example:80".to_owned(),
                "https://new.example/announce".to_owned()
            ]
        );
    }
}
