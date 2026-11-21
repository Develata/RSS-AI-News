//! HTML fetching and content extraction.

pub mod charset;
pub mod error;
pub mod fetcher;
pub mod strategy;

/// `rule_versions` identity (`kind = 'extractor'`) of this crate's extraction
/// rules, stamped into `articles.extractor_version`. Bump the tag when the
/// rules that shape article text change (strategy, word count, decoding).
pub const EXTRACTOR_RULE_KIND: &str = "extractor";
pub const EXTRACTOR_RULE_TAG: &str = "readability-2026-11-cjk-charset";
pub const EXTRACTOR_RULE_DESCRIPTION: &str =
    "readability + summary fallback; CJK-aware word count; BOM/Content-Type/meta charset decoding";

pub use charset::decode_html;
pub use error::ExtractorError;
pub use fetcher::{HtmlFetcher, RawHtmlFetch, ReqwestHtmlFetcher};
pub use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle, FallbackArticle};
pub use strategy::{ContentStrategy, ReadabilityStrategy, summary_fallback, word_count};
