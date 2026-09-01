//! 追番订阅：把 Bangumi 条目与 Mikan 单番 RSS 关联起来，
//! 新集资源经现有 RSS 刷新 + 规则管线自动下载。

use crate::models::{FeedRule, SubjectRssState};

/// Mikan 单番 RSS 地址：一部番剧一个订阅源，字幕组/画质偏好交给规则过滤。
pub fn build_mikan_rss_url(mikan_id: i64) -> String {
    format!("https://mikanani.me/RSS/Bangumi?bangumiId={mikan_id}")
}

/// 订阅默认规则：不筛内容、新资源直接自动下载；字幕组偏好可选。
/// 订阅时已存在的资源只入列不自动下载，避免“补全库”式批量下载。
pub fn default_subscription_rule(subtitle_group: Option<&str>) -> FeedRule {
    FeedRule {
        includes: Vec::new(),
        excludes: Vec::new(),
        resolution: None,
        subtitle_group: subtitle_group
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        auto_download: true,
    }
}

/// 卡片更新徽章判定。订阅驱动的状态优先（来自真实资源），
/// 有未下载的匹配资源时“有更新”要让用户看见，不能被“已下载”挡住；
/// 无订阅时仅对“在看 + 今天放送 + 未看完”给“有更新”提示。
pub fn compute_update_state(
    collection: Option<&str>,
    watched: i64,
    episodes: i64,
    air_weekday: i64,
    today_weekday: i64,
    rss: Option<&SubjectRssState>,
) -> &'static str {
    if let Some(rss) = rss {
        if rss.downloading {
            return "downloading";
        }
        if rss.pending {
            return "published";
        }
        if rss.completed {
            return "completed";
        }
    }
    if collection == Some("doing")
        && episodes > 0
        && watched < episodes
        && air_weekday == today_weekday
    {
        return "published";
    }
    "none"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_mikan_per_subject_rss_url() {
        assert_eq!(
            build_mikan_rss_url(4052),
            "https://mikanani.me/RSS/Bangumi?bangumiId=4052"
        );
    }

    #[test]
    fn default_rule_auto_downloads_and_keeps_subtitle_preference() {
        let rule = default_subscription_rule(Some(" 喵萌奶茶屋 "));
        assert!(rule.auto_download);
        assert_eq!(rule.subtitle_group.as_deref(), Some("喵萌奶茶屋"));
        assert!(rule.includes.is_empty() && rule.excludes.is_empty());
        let without = default_subscription_rule(None);
        assert!(without.subtitle_group.is_none());
        let blank = default_subscription_rule(Some("  "));
        assert!(blank.subtitle_group.is_none(), "空白字幕组偏好视为未设置");
    }

    #[test]
    fn rss_states_take_priority_over_heuristics() {
        let rss = SubjectRssState {
            downloading: true,
            completed: true,
            pending: true,
        };
        assert_eq!(
            compute_update_state(Some("collect"), 12, 12, 1, 1, Some(&rss)),
            "downloading",
            "下载中优先于其他状态"
        );
        let rss = SubjectRssState {
            downloading: false,
            completed: true,
            pending: true,
        };
        assert_eq!(
            compute_update_state(Some("doing"), 3, 12, 1, 1, Some(&rss)),
            "published",
            "已有下载但存在未下载的新资源时，有更新不能被已下载挡住"
        );
        let rss = SubjectRssState {
            downloading: false,
            completed: true,
            pending: false,
        };
        assert_eq!(
            compute_update_state(Some("collect"), 12, 12, 1, 1, Some(&rss)),
            "completed",
            "匹配资源全部下载完才显示已下载"
        );
        let rss = SubjectRssState {
            downloading: false,
            completed: false,
            pending: true,
        };
        assert_eq!(
            compute_update_state(Some("collect"), 12, 12, 3, 3, Some(&rss)),
            "published"
        );
    }

    #[test]
    fn airing_today_heuristic_only_for_unfinished_doing() {
        // 在看 + 今天放送 + 没看完 => 有更新。
        assert_eq!(
            compute_update_state(Some("doing"), 3, 12, 2, 2, None),
            "published"
        );
        // 看完、不在看、非今天放送、未知集数都不提示。
        assert_eq!(
            compute_update_state(Some("doing"), 12, 12, 2, 2, None),
            "none"
        );
        assert_eq!(
            compute_update_state(Some("wish"), 0, 12, 2, 2, None),
            "none"
        );
        assert_eq!(
            compute_update_state(Some("doing"), 3, 12, 3, 2, None),
            "none"
        );
        assert_eq!(
            compute_update_state(Some("doing"), 3, 0, 2, 2, None),
            "none"
        );
    }
}
