use std::num::NonZeroU32;

use rss_ai_news_domain::{Score0To100, SecretString, state::FeedKind};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct CategoryConfig {
    pub schema_version: String,
    pub category: CategoryMeta,
    pub ai_override: Option<AiOverride>,
    pub publish_override: Option<PublishOverride>,
    pub sources: Vec<SourceConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CategoryMeta {
    pub key: String,
    pub display_name: String,
    pub priority: u32,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AiOverride {
    pub prompt_template: Option<String>,
    pub max_input_chars: Option<u32>,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PublishOverride {
    pub max_items_per_report: Option<NonZeroU32>,
    /// Per docs/design/config-schema.md §8 line 378:
    /// `Option<Score0To100>` — `None` 继承全局，`Some(0)` 显式无下限。
    /// 使用 `Score0To100` 而非裸 `u8`，让 TOML 反序列化阶段直接拒绝越界
    /// 值（W2-B-1：旧实现把 `try_new` 失败折叠为继承默认，掩盖配置错误）。
    pub min_importance_score: Option<Score0To100>,
    pub include_unscored: Option<bool>,
    /// Optional per-category report path template. When present, it overrides
    /// `[publish.template].path_template` for local and remote publishing.
    pub path_template: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SourceConfig {
    pub key: String,
    pub display_name: String,
    pub feed_url: String,
    #[serde(deserialize_with = "deserialize_feed_kind")]
    pub feed_kind: FeedKind,
    #[serde(skip)]
    pub rsshub_access_key: Option<SecretString>,
    pub priority: u32,
    pub enabled: bool,
}

fn deserialize_feed_kind<'de, D>(deserializer: D) -> Result<FeedKind, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    match value.as_str() {
        "rss" => Ok(FeedKind::Rss),
        "atom" => Ok(FeedKind::Atom),
        "json_feed" => Ok(FeedKind::JsonFeed),
        "rsshub" | "rss_hub" => Ok(FeedKind::RssHub),
        other => Err(serde::de::Error::custom(format!(
            "unknown feed_kind {other:?}, expected rss, atom, json_feed, or rsshub"
        ))),
    }
}

#[derive(Deserialize)]
struct RawCategoryConfig {
    schema_version: String,
    category: RawCategorySection,
    #[serde(default)]
    sources: Vec<SourceConfig>,
}

#[derive(Deserialize)]
struct RawCategorySection {
    key: String,
    display_name: String,
    priority: u32,
    ai_override: Option<AiOverride>,
    publish_override: Option<PublishOverride>,
}

impl<'de> Deserialize<'de> for CategoryConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawCategoryConfig::deserialize(deserializer)?;
        Ok(Self {
            schema_version: raw.schema_version,
            category: CategoryMeta {
                key: raw.category.key,
                display_name: raw.category.display_name,
                priority: raw.category.priority,
            },
            ai_override: raw.category.ai_override,
            publish_override: raw.category.publish_override,
            sources: raw.sources,
        })
    }
}

impl CategoryConfig {
    pub fn key(&self) -> &str {
        &self.category.key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_category_config() {
        let content = include_str!("../../../configs/categories/ai.toml.example");
        let config: CategoryConfig = toml::from_str(content).expect("example category parses");

        assert_eq!(config.schema_version, "1");
        assert_eq!(config.category.key, "ai");
        assert!(config.ai_override.is_some());
        assert!(config.publish_override.is_some());
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[1].feed_kind, FeedKind::RssHub);
    }

    #[test]
    fn rejects_min_importance_score_above_100() {
        // W2-B-1 regression guard: out-of-range `min_importance_score` must
        // fail at TOML deserialization (Score0To100 contract), not be
        // silently folded into the global default by `effective_for_category`.
        // See docs/design/config-schema.md §8 line 378.
        let content = r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[category.publish_override]
min_importance_score = 200
"#;
        let err = toml::from_str::<CategoryConfig>(content)
            .expect_err("min_importance_score = 200 must be rejected");
        let lowered = err.to_string().to_lowercase();
        assert!(
            lowered.contains("0") && lowered.contains("100")
                || lowered.contains("range")
                || lowered.contains("invalid"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn accepts_min_importance_score_zero_as_explicit_no_floor() {
        // §4.5: `0` is "explicit no floor", not "use default" — must round-trip.
        let content = r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[category.publish_override]
min_importance_score = 0
"#;
        let config: CategoryConfig =
            toml::from_str(content).expect("min_importance_score = 0 must parse");
        let override_ = config
            .publish_override
            .expect("publish_override should be present");
        assert_eq!(
            override_
                .min_importance_score
                .expect("Some(0), not None")
                .get(),
            0
        );
    }

    #[test]
    fn rejects_zero_max_items_per_report() {
        // max_items_per_report = 0 must fail at toml deserialization (NonZeroU32 contract).
        // See docs/design/config-schema.md §234.
        let content = r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[category.publish_override]
max_items_per_report = 0
"#;
        let err = toml::from_str::<CategoryConfig>(content)
            .expect_err("max_items_per_report = 0 must be rejected");
        assert!(
            err.to_string().to_lowercase().contains("zero")
                || err.to_string().contains("nonzero")
                || err.to_string().contains("non-zero")
                || err.to_string().contains("invalid value"),
            "unexpected error message: {err}"
        );
    }
}
