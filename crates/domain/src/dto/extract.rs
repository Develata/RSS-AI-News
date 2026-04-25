//! Extraction-stage DTOs.

use crate::state::{ContentQuality, ExtractorStrategy};

/// Task from runtime to extractor: fetch and extract article content.
#[derive(Debug, Clone)]
pub struct ArticleFetchTask {
    pub feed_entry_id: i64,
    pub normalized_link: String,
    pub title_raw: String,
    pub summary_raw: Option<String>,
    pub timeout: std::time::Duration,
}

/// Successful extraction result.
#[derive(Debug, Clone)]
pub struct ExtractedArticle {
    pub feed_entry_id: i64,
    pub canonical_link: String,
    pub title: String,
    pub body_text: String,
    pub body_html: Option<Vec<u8>>,
    pub extractor_strategy: ExtractorStrategy,
    pub content_quality: ContentQuality,
    pub word_count: u32,
    pub content_hash: String,
}

/// Fallback extraction using feed summary.
#[derive(Debug, Clone)]
pub struct FallbackArticle {
    pub feed_entry_id: i64,
    pub canonical_link: String,
    pub title: String,
    pub body_text: String,
    pub content_quality: ContentQuality,
    pub word_count: u32,
    pub content_hash: String,
}
