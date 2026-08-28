use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subject { pub id:i64, pub name:String, pub name_cn:String, pub summary:String, pub image:Option<String>, pub score:f64, pub rank:Option<i64>, pub air_weekday:i64, pub collection:Option<String>, pub episodes:i64, pub watched:i64, pub update_state:String }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedRule { pub includes:Vec<String>, pub excludes:Vec<String>, pub resolution:Option<String>, pub subtitle_group:Option<String>, pub auto_download:bool }

impl From<&FeedRule> for crate::matcher::MatchRule {
    fn from(rule: &FeedRule) -> Self {
        Self { includes: rule.includes.clone(), excludes: rule.excludes.clone(), resolution: rule.resolution.clone(), subtitle_group: rule.subtitle_group.clone() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssFeed { pub id:String, pub title:String, pub url:String, pub enabled:bool, pub last_checked_at:Option<String>, pub rule:FeedRule }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadTask { pub id:String, pub title:String, pub episode:String, pub progress:f64, pub down_speed:u64, pub up_speed:u64, pub state:String, pub output_path:String }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssItem { pub guid:String, pub feed_id:String, pub title:String, pub link:String, pub torrent:Option<String>, pub published_at:Option<String>, pub downloaded:bool, pub download:Option<RssDownloadStatus>, pub matches_rule:bool }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RssDownloadStatus { pub task_id:String, pub state:String, pub progress:f64, pub active:bool }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchDownloadResult { pub added:usize, pub reused:usize, pub failed:usize, pub errors:Vec<String> }
