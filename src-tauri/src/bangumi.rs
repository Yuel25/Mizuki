use crate::models::Subject;
use serde_json::Value;

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or_default()
}

/// Bangumi 旧接口（calendar 等）会返回 http:// 图片地址；统一升级为 https，
/// 否则会被应用 CSP 的 img-src 白名单拦截，表现为封面加载不出来。
fn https_image(url: Option<String>) -> Option<String> {
    url.map(|url| {
        if url.starts_with("http://lain.bgm.tv/") || url.starts_with("http://bgm.tv/") {
            format!("https://{}", &url["http://".len()..])
        } else {
            url
        }
    })
}

pub fn subject_from_v0(value: &Value, collection: Option<String>, watched: i64) -> Subject {
    Subject {
        id: value.get("id").and_then(Value::as_i64).unwrap_or_default(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        name_cn: value
            .get("name_cn")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        summary: value
            .get("summary")
            .or_else(|| value.get("short_summary"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        image: https_image(
            value
                .pointer("/images/large")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        score: number(value.pointer("/rating/score")).max(number(value.get("score"))),
        rank: value
            .pointer("/rating/rank")
            .and_then(Value::as_i64)
            .or_else(|| value.get("rank").and_then(Value::as_i64))
            .filter(|rank| *rank > 0),
        air_weekday: -1,
        collection,
        episodes: value
            .get("total_episodes")
            .and_then(Value::as_i64)
            .or_else(|| value.get("eps").and_then(Value::as_i64))
            .unwrap_or_default(),
        watched,
        update_state: "none".into(),
    }
}

pub async fn calendar(client: &reqwest::Client) -> Result<Vec<Subject>, String> {
    let days: Vec<Value> = client
        .get("https://api.bgm.tv/calendar")
        .send()
        .await
        .map_err(|e| format!("刷新每日放送失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("刷新每日放送失败：{e}"))?
        .json()
        .await
        .map_err(|e| format!("解析每日放送失败：{e}"))?;
    let mut out = Vec::new();
    for day in days {
        let id = day
            .pointer("/weekday/id")
            .and_then(Value::as_i64)
            .unwrap_or(1);
        let weekday = if id == 0 { 6 } else { id - 1 };
        for value in day
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let mut subject = subject_from_v0(value, None, 0);
            let item_day = value
                .get("air_weekday")
                .and_then(Value::as_i64)
                .filter(|day| (1..=7).contains(day));
            subject.air_weekday = item_day.map(|day| day - 1).unwrap_or(weekday);
            out.push(subject);
        }
    }
    Ok(out)
}

pub async fn search_subjects(
    client: &reqwest::Client,
    token: Option<&str>,
    keyword: &str,
    limit: u32,
) -> Result<Vec<Subject>, String> {
    let request = client.post("https://api.bgm.tv/v0/search/subjects")
        .query(&[("limit", limit.min(30)), ("offset", 0)])
        .json(&serde_json::json!({"keyword":keyword,"sort":"match","filter":{"type":[2],"nsfw":false}}));
    let request = if let Some(token) = token {
        request.bearer_auth(token)
    } else {
        request
    };
    let page: Value = request
        .send()
        .await
        .map_err(|e| format!("搜索番剧失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("搜索番剧失败：{e}"))?
        .json()
        .await
        .map_err(|e| format!("解析搜索结果失败：{e}"))?;
    Ok(page
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| subject_from_v0(value, None, 0))
        .collect())
}

pub async fn profile(client: &reqwest::Client, token: &str) -> Result<Value, String> {
    client
        .get("https://api.bgm.tv/v0/me")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|_| "Access Token 无效或已过期".to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}
/// Bangumi 收藏 type（1-5）与本地状态名互转，是前后端字段约定的唯一来源。
pub fn collection_slug(kind: i64) -> Option<&'static str> {
    match kind {
        1 => Some("wish"),
        2 => Some("collect"),
        3 => Some("doing"),
        4 => Some("on_hold"),
        5 => Some("dropped"),
        _ => None,
    }
}

pub fn collection_kind(slug: &str) -> Option<i64> {
    match slug {
        "wish" => Some(1),
        "collect" => Some(2),
        "doing" => Some(3),
        "on_hold" => Some(4),
        "dropped" => Some(5),
        _ => None,
    }
}

/// 收藏接口返回分页对象 `{ data, total, limit, offset }`。
/// 历史上曾误按数组解析导致导入为空，这里固定只认对象形状。
pub fn collection_page_data(page: &Value) -> Vec<Value> {
    page.get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `ep` 传 `Some` 时同步观看进度到 Bangumi 的 `ep_status`；`None` 表示不改集数。
pub async fn set_collection(
    client: &reqwest::Client,
    token: &str,
    subject_id: i64,
    collection: &str,
    ep: Option<i64>,
) -> Result<(), String> {
    let Some(kind) = collection_kind(collection) else {
        return Err("收藏状态无效".into());
    };
    let mut body = serde_json::json!({"type": kind});
    if let Some(ep) = ep {
        body["ep"] = ep.into();
    }
    client
        .post(format!(
            "https://api.bgm.tv/v0/users/-/collections/{subject_id}"
        ))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| format!("Bangumi 同步失败：{e}"))?;
    Ok(())
}
pub async fn collections(
    client: &reqwest::Client,
    token: &str,
    username: &str,
) -> Result<Vec<Value>, String> {
    let mut all = Vec::new();
    let mut offset = 0;
    loop {
        let page: Value = client
            .get(format!(
                "https://api.bgm.tv/v0/users/{username}/collections"
            ))
            .bearer_auth(token)
            .query(&[("subject_type", 2), ("limit", 50), ("offset", offset)])
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| format!("读取 Bangumi 收藏失败：{e}"))?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let data = collection_page_data(&page);
        let count = data.len();
        all.extend(data);
        if count < 50 {
            break;
        }
        offset += 50;
    }
    Ok(all)
}
pub async fn comments(
    client: &reqwest::Client,
    subject_id: i64,
    offset: u32,
) -> Result<Value, String> {
    client
        .get(format!(
            "https://next.bgm.tv/p1/subjects/{subject_id}/comments"
        ))
        .query(&[("limit", 20), ("offset", offset)])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| format!("读取 Bangumi 短评失败：{e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())
}
pub async fn subject(
    client: &reqwest::Client,
    token: Option<&str>,
    subject_id: i64,
) -> Result<Value, String> {
    let request = client.get(format!("https://api.bgm.tv/v0/subjects/{subject_id}"));
    let request = if let Some(token) = token {
        request.bearer_auth(token)
    } else {
        request
    };
    request
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| format!("读取条目详情失败：{e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_full_subject_rating() {
        let value = serde_json::json!({"id":1,"name":"A","rating":{"score":8.2,"rank":123},"total_episodes":12});
        let subject = subject_from_v0(&value, None, 0);
        assert_eq!(subject.score, 8.2);
        assert_eq!(subject.rank, Some(123));
        assert_eq!(subject.episodes, 12)
    }
    #[test]
    fn reads_slim_subject_fields() {
        let value = serde_json::json!({"id":2,"name":"B","score":7.6,"rank":456,"eps":24,"short_summary":"简介"});
        let subject = subject_from_v0(&value, Some("doing".into()), 3);
        assert_eq!(subject.score, 7.6);
        assert_eq!(subject.rank, Some(456));
        assert_eq!(subject.summary, "简介");
        assert_eq!(subject.watched, 3)
    }
    #[test]
    fn reads_paginated_collection_object() {
        let page = serde_json::json!({
            "data": [
                {"subject_id": 7, "type": 3, "ep_status": 4},
                {"subject_id": 9, "type": 1, "ep_status": 0}
            ],
            "total": 12,
            "limit": 50,
            "offset": 0
        });
        let data = collection_page_data(&page);
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["subject_id"], 7);
        assert_eq!(page["total"], 12);
    }
    #[test]
    fn ignores_legacy_array_shaped_collection_response() {
        // 旧解析按数组读取分页响应导致导入恒为空；数组形状必须得到空结果而不是 panic。
        let page = serde_json::json!([{"subject_id": 7, "type": 3}]);
        assert!(collection_page_data(&page).is_empty());
    }
    #[test]
    fn short_page_marks_the_final_offset() {
        // 每页 50 条：不足一页说明已到末尾，循环必须停止。
        let full_page = collection_page_data(&serde_json::json!({
            "data": vec![serde_json::Value::Null; 50]
        }));
        let last_page = collection_page_data(&serde_json::json!({
            "data": vec![serde_json::Value::Null; 17]
        }));
        assert_eq!(full_page.len(), 50);
        assert_eq!(last_page.len(), 17);
        assert!(full_page.len() >= 50);
        assert!(last_page.len() < 50);
    }
    #[test]
    fn collection_kind_and_slug_are_inverses() {
        for kind in 1..=5 {
            let slug = collection_slug(kind).expect("known kind");
            assert_eq!(collection_kind(slug), Some(kind));
        }
        assert_eq!(collection_slug(0), None);
        assert_eq!(collection_slug(9), None);
        assert_eq!(collection_kind("wish"), Some(1));
        assert_eq!(collection_kind("doing"), Some(3));
        assert_eq!(collection_kind("unknown"), None);
    }
    #[test]
    fn calendar_http_image_urls_are_upgraded_to_https() {
        // CSP 只放行 https 图片；旧 calendar 接口返回 http 地址必须被规范化。
        let value = serde_json::json!({"id":1,"name":"A","images":{"large":"http://lain.bgm.tv/pic/cover/l/ce/e2/456080_C4q4C.jpg"}});
        let subject = subject_from_v0(&value, None, 0);
        assert_eq!(
            subject.image.as_deref(),
            Some("https://lain.bgm.tv/pic/cover/l/ce/e2/456080_C4q4C.jpg")
        );
        let already_https = serde_json::json!({"id":2,"name":"B","images":{"large":"https://lain.bgm.tv/pic/cover/x.jpg"}});
        assert_eq!(
            subject_from_v0(&already_https, None, 0).image.as_deref(),
            Some("https://lain.bgm.tv/pic/cover/x.jpg")
        );
        assert_eq!(subject_from_v0(&serde_json::json!({"id":3,"name":"C"}), None, 0).image, None);
    }
}
