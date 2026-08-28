use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchRule {
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
    pub resolution: Option<String>,
    pub subtitle_group: Option<String>,
}

pub fn normalize_title(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn resource_matches(title: &str, rule: &MatchRule) -> bool {
    let normalized = normalize_title(title);
    let contains = |value: &str| normalized.contains(&normalize_title(value));
    rule.includes.iter().all(|value| contains(value))
        && rule.excludes.iter().all(|value| !contains(value))
        && rule.resolution.as_deref().is_none_or(contains)
        && rule.subtitle_group.as_deref().is_none_or(contains)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_release_punctuation() {
        assert_eq!(
            normalize_title("[Nekomoe kissaten] Frieren - 08"),
            "nekomoekissatenfrieren08"
        );
    }

    #[test]
    fn applies_include_exclude_and_quality_rules() {
        let rule = MatchRule {
            includes: vec!["简中".into()],
            excludes: vec!["720p".into()],
            resolution: Some("1080p".into()),
            subtitle_group: Some("喵萌".into()),
        };
        assert!(resource_matches(
            "[喵萌奶茶屋] 芙莉莲 08 [1080P][简中]",
            &rule
        ));
        assert!(!resource_matches(
            "[喵萌奶茶屋] 芙莉莲 08 [720P][简中]",
            &rule
        ));
    }
}
