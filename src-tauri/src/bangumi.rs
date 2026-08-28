use crate::models::Subject;
use serde_json::Value;

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or_default()
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
        image: value
            .pointer("/images/large")
            .and_then(Value::as_str)
            .map(str::to_owned),
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
pub async fn set_collection(
    client: &reqwest::Client,
    token: &str,
    subject_id: i64,
    collection: &str,
) -> Result<(), String> {
    let kind = match collection {
        "wish" => 1,
        "collect" => 2,
        "doing" => 3,
        "on_hold" => 4,
        "dropped" => 5,
        _ => return Err("收藏状态无效".into()),
    };
    client
        .post(format!(
            "https://api.bgm.tv/v0/users/-/collections/{subject_id}"
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({"type":kind}))
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
        let data = page
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
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
}
