use time::OffsetDateTime;

use crate::state::{ArticleState, ContentQuality, ExtractorStrategy};

#[derive(Debug, Clone)]
pub struct Article {
    pub id: i64,
    pub content_hash: String,
    pub canonical_link: String,
    pub title: String,
    pub body_text: String,
    pub body_html_artifact_id: Option<i64>,
    pub extractor_strategy: ExtractorStrategy,
    pub extractor_version: i64,
    pub content_quality: ContentQuality,
    pub word_count: i64,
    pub origin_feed_entry_id: i64,
    pub state: ArticleState,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
