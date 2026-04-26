//! Feed fetching and parsing.

pub mod error;
pub mod fetcher;
pub mod parser;

pub use error::FeedError;
pub use fetcher::{FeedFetcher, ReqwestFeedFetcher};
pub use parser::parse_feed;

pub use rss_ai_news_domain::dto::feed::{
    DedupResult, FeedEntryMeta, FeedFetchRequest, FeedFetchResponse,
};
