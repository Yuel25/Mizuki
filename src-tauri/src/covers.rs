//! 封面缓存：下载、校验与本地协议地址改写。

use crate::state::AppState;
use tauri::Manager;

const MAX_COVER_BYTES: usize = 5 * 1024 * 1024;

/// 本地封面走自定义协议（Windows 下为 http://cover.localhost/*）。
pub(crate) fn cover_local_url(subject_id: i64, cached: bool) -> Option<String> {
    cached.then(|| format!("http://cover.localhost/{}", cover_file_name(subject_id)))
}

/// 封面缓存文件名只允许纯数字 id + .jpg，协议处理器据此拒绝目录穿越。
pub(crate) fn cover_file_name(subject_id: i64) -> String {
    format!("{subject_id}.jpg")
}

pub(crate) fn cover_file_name_is_valid(name: &str) -> bool {
    let stem = name.strip_suffix(".jpg").unwrap_or_default();
    !stem.is_empty() && stem.len() <= 20 && stem.bytes().all(|byte| byte.is_ascii_digit())
}

/// 从 Bangumi JSON（v0 条目或收藏内嵌 subject）提取 (subject_id, 封面 URL)。
pub(crate) fn cover_candidate(value: &serde_json::Value) -> Option<(i64, String)> {
    let id = value.get("id").and_then(serde_json::Value::as_i64)?;
    let url = ["large", "common", "medium"].iter().find_map(|size| {
        value
            .pointer(&format!("/images/{size}"))
            .and_then(serde_json::Value::as_str)
    })?;
    (url.starts_with("http")).then_some((id, url.to_owned()))
}

/// 下载单个封面到缓存目录。best-effort：已存在跳过，非 JPEG 或超 5MB 丢弃。
async fn download_cover(state: &AppState, subject_id: i64, url: &str) {
    let path = state.cover_dir.join(cover_file_name(subject_id));
    if path.is_file() {
        return;
    }
    let Ok(_permit) = state.bangumi_gate.acquire().await else {
        return;
    };
    let Ok(mut response) = state.client.get(url).send().await else {
        return;
    };
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_COVER_BYTES as u64)
    {
        return;
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_COVER_BYTES as u64) as usize,
    );
    loop {
        let Ok(chunk) = response.chunk().await else {
            return;
        };
        let Some(chunk) = chunk else {
            break;
        };
        let Some(next_len) = bytes.len().checked_add(chunk.len()) else {
            return;
        };
        if next_len > MAX_COVER_BYTES {
            return;
        }
        bytes.extend_from_slice(&chunk);
    }
    let is_jpeg = bytes.first() == Some(&0xFF) && bytes.get(1) == Some(&0xD8);
    if !is_jpeg {
        return;
    }
    let _ = std::fs::write(path, bytes);
}

/// 后台批量预取缺失封面（best-effort，失败静默）。
pub(crate) fn spawn_cover_prefetch(app: &tauri::AppHandle, candidates: Vec<(i64, String)>) {
    if candidates.is_empty() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        for (subject_id, url) in candidates {
            download_cover(&state, subject_id, &url).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_file_name_rejects_traversal_and_non_numeric() {
        assert!(cover_file_name_is_valid("3354.jpg"));
        assert!(!cover_file_name_is_valid(".jpg"), "空 stem 拒绝");
        assert!(!cover_file_name_is_valid("abc.jpg"));
        assert!(
            !cover_file_name_is_valid("../etc/passwd.jpg"),
            "目录穿越拒绝"
        );
        assert!(!cover_file_name_is_valid("3354.gif"));
        assert!(
            !cover_file_name_is_valid("123456789012345678901.jpg"),
            "超长文件名拒绝"
        );
    }

    #[test]
    fn cover_candidate_reads_images_and_requires_http() {
        let value = serde_json::json!({"id": 7, "images": {"large": "https://lain.bgm.tv/pic/cover/l/a.jpg"}});
        assert_eq!(
            cover_candidate(&value),
            Some((7, "https://lain.bgm.tv/pic/cover/l/a.jpg".into()))
        );
        assert_eq!(cover_candidate(&serde_json::json!({"id": 7})), None);
        assert_eq!(
            cover_candidate(&serde_json::json!({"id": 7, "images": {}})),
            None
        );
    }
}
