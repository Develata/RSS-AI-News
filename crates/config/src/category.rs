use rss_ai_news_domain::state::FeedKind;
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
    pub max_items_per_report: Option<u32>,
    pub min_importance_score: Option<u8>,
    pub include_unscored: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SourceConfig {
    pub key: String,
    pub display_name: String,
    pub feed_url: String,
    #[serde(deserialize_with = "deserialize_feed_kind")]
    pub feed_kind: FeedKind,
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
}
