//! bgmlist 放送数据：补全 Bangumi 旧 `/calendar` 偶尔遗漏的当季条目。

use chrono::Datelike;
use serde::Deserialize;
use std::collections::HashMap;

use crate::models::Subject;

const ONAIR_URL: &str = "https://bgmlist.com/api/v1/bangumi/onair";

#[derive(Deserialize)]
struct Payload {
    items: Vec<Item>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    title: String,
    #[serde(default)]
    title_translate: serde_json::Value,
    #[serde(default)]
    broadcast: String,
    #[serde(default)]
    sites: Vec<Site>,
}

#[derive(Deserialize)]
struct Site {
    site: String,
    id: String,
}

fn broadcast_datetime(item: &Item) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    // 每周表只接收明确的 P7D 循环；一次性 OVA/剧场版不能按首播日永久重复展示。
    let value = item.broadcast.strip_prefix("R/")?;
    let (value, period) = value.split_once('/')?;
    if period != "P7D" {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(value).ok()
}

fn subject_from_item(item: Item) -> Option<Subject> {
    let id = item
        .sites
        .iter()
        .find(|site| site.site == "bangumi")?
        .id
        .parse::<i64>()
        .ok()?;
    let air = broadcast_datetime(&item)?;
    let jst = chrono::FixedOffset::east_opt(9 * 3600)?;
    let air = air.with_timezone(&jst);
    let name_cn = item
        .title_translate
        .get("zh-Hans")
        .and_then(serde_json::Value::as_array)
        .and_then(|titles| titles.first())
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Some(Subject {
        id,
        name: item.title,
        name_cn,
        summary: String::new(),
        image: None,
        score: 0.0,
        rank: None,
        air_weekday: air.weekday().num_days_from_monday() as i64,
        collection: None,
        episodes: 0,
        watched: 0,
        update_state: "none".into(),
    })
}

pub(crate) struct CalendarData {
    pub(crate) subjects: Vec<Subject>,
    pub(crate) mikan_ids: HashMap<i64, i64>,
}

fn site_id(item: &Item, name: &str) -> Option<i64> {
    item.sites
        .iter()
        .find(|site| site.site == name)?
        .id
        .parse()
        .ok()
}

async fn fetch(client: &reqwest::Client) -> Result<Payload, String> {
    let payload: Payload = client
        .get(ONAIR_URL)
        .send()
        .await
        .map_err(|error| format!("刷新 bgmlist 放送数据失败：{error}"))?
        .error_for_status()
        .map_err(|error| format!("刷新 bgmlist 放送数据失败：{error}"))?
        .json()
        .await
        .map_err(|error| format!("解析 bgmlist 放送数据失败：{error}"))?;
    Ok(payload)
}

pub(crate) async fn calendar(client: &reqwest::Client) -> Result<CalendarData, String> {
    let payload = fetch(client).await?;
    let mikan_ids = payload
        .items
        .iter()
        .filter_map(|item| Some((site_id(item, "bangumi")?, site_id(item, "mikan")?)))
        .collect();
    let subjects = payload
        .items
        .into_iter()
        .filter_map(subject_from_item)
        .collect();
    Ok(CalendarData {
        subjects,
        mikan_ids,
    })
}

pub(crate) async fn mikan_id_for_bangumi(
    client: &reqwest::Client,
    subject_id: i64,
) -> Result<i64, String> {
    fetch(client)
        .await?
        .items
        .iter()
        .find_map(|item| {
            if site_id(item, "bangumi") == Some(subject_id) {
                site_id(item, "mikan")
            } else {
                None
            }
        })
        .ok_or_else(|| "bgmlist 暂无这部番的 Mikan ID，无法创建单番订阅".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_re_zero_by_bangumi_id_and_jst_weekday() {
        let item: Item = serde_json::from_value(serde_json::json!({
            "title": "Re:ゼロから始める異世界生活 4th season 奪還編",
            "titleTranslate": {"zh-Hans": ["Re：从零开始的异世界生活 第四季 夺还篇"]},
            "begin": "2026-08-12T14:00:00.000Z",
            "broadcast": "R/2026-08-12T14:00:00.000Z/P7D",
            "sites": [
                {"site": "bangumi", "id": "633836"},
                {"site": "mikan", "id": "4052"}
            ]
        }))
        .unwrap();
        let subject = subject_from_item(item).unwrap();
        assert_eq!(subject.id, 633836);
        assert_eq!(subject.air_weekday, 2);
        assert!(subject.name_cn.contains("夺还篇"));
    }

    #[test]
    fn ignores_items_without_weekly_broadcast_rule() {
        let item: Item = serde_json::from_value(serde_json::json!({
            "title": "Movie",
            "begin": "2026-08-12T14:00:00.000Z",
            "broadcast": "",
            "sites": [{"site": "bangumi", "id": "1"}]
        }))
        .unwrap();
        assert!(subject_from_item(item).is_none());
    }
}
