//! HTML fetching and content extraction.

pub mod error;
pub mod fetcher;
pub mod strategy;

pub use error::ExtractorError;
pub use fetcher::{HtmlFetcher, RawHtmlFetch, ReqwestHtmlFetcher};
pub use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle, FallbackArticle};
pub use strategy::{ContentStrategy, ReadabilityStrategy, summary_fallback};
