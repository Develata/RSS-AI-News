//! HTTP feed fetcher with conditional request support.

use async_trait::async_trait;
use bytes::BytesMut;
use reqwest::header::{
    ACCEPT, ETAG, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, USER_AGENT,
};
use reqwest::{Client, StatusCode, Url};
use rss_ai_news_domain::dto::feed::{FeedFetchRequest, FeedFetchResponse};

use crate::error::FeedError;
use crate::parser::parse_feed;

const USER_AGENT_VALUE: &str = "rss-ai-news/0.1 (+https://github.com/Develata/RSS-AI-News)";
const ACCEPT_VALUE: &str = "application/rss+xml, application/atom+xml, application/feed+json, application/xml;q=0.9, */*;q=0.5";

/// Raw HTTP fetch result — bytes only, no parsing.
///
/// Use this when you need to persist `raw_payload_bytes` to `raw_artifacts`
/// **before** parsing, per `docs/design/replay-and-artifacts.md` §5.1: if
/// parsing later panics or fails, the artifact is already on disk and can
/// be replayed.
#[derive(Debug, Clone)]
pub struct RawFeedFetch {
    pub source_id: i64,
    pub http_status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub not_modified: bool,
    pub raw_payload_bytes: Option<Vec<u8>>,
}

#[async_trait]
pub trait FeedFetcher: Send + Sync {
    /// HTTP fetch only. Does not parse. Caller is responsible for calling
    /// `parse_feed` (and persisting `raw_payload_bytes` first if `raw_artifact`
    /// retention is enabled).
    async fn fetch_raw(&self, request: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError>;

    /// Convenience: fetch + parse in one call. Suitable for tests or callers
    /// that do not need raw_artifact persistence.
    async fn fetch(&self, request: &FeedFetchRequest) -> Result<FeedFetchResponse, FeedError> {
        let raw = self.fetch_raw(request).await?;
        let entries = match (raw.not_modified, raw.raw_payload_bytes.as_deref()) {
            (true, _) | (_, None) => Vec::new(),
            (false, Some(body)) => parse_feed(body, request.feed_kind)?,
        };
        Ok(FeedFetchResponse {
            source_id: raw.source_id,
            http_status: raw.http_status,
            etag: raw.etag,
            last_modified: raw.last_modified,
            not_modified: raw.not_modified,
            entries,
            raw_payload_bytes: raw.raw_payload_bytes,
        })
    }
}

pub struct ReqwestFeedFetcher {
    client: Client,
    max_body_bytes: u64,
}

impl ReqwestFeedFetcher {
    pub fn new(max_body_bytes: u64) -> Result<Self, FeedError> {
        let client = Client::builder()
            .user_agent(USER_AGENT_VALUE)
            .build()
            .map_err(FeedError::from)?;

        Ok(Self {
            client,
            max_body_bytes,
        })
    }

    pub fn with_client(client: Client, max_body_bytes: u64) -> Self {
        Self {
            client,
            max_body_bytes,
        }
    }
}

#[async_trait]
impl FeedFetcher for ReqwestFeedFetcher {
    async fn fetch_raw(&self, request: &FeedFetchRequest) -> Result<RawFeedFetch, FeedError> {
        let mut feed_url = Url::parse(&request.feed_url).map_err(|_| FeedError::InvalidUrl {
            url: request.feed_url.clone(),
        })?;
        if let Some(access_key) = &request.rsshub_access_key {
            append_query_param_if_missing(&mut feed_url, "key", access_key.expose_secret().trim());
        }

        let mut builder = self
            .client
            .get(feed_url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, ACCEPT_VALUE)
            .timeout(request.timeout);

        if let Some(etag) = &request.etag {
            builder = builder.header(IF_NONE_MATCH, etag);
        }

        if let Some(last_modified) = &request.last_modified {
            builder = builder.header(IF_MODIFIED_SINCE, last_modified);
        }

        let response = builder.send().await.map_err(FeedError::from)?;
        let status = response.status();
        let headers = response.headers().clone();
        let etag = header_to_string(&headers, ETAG);
        let last_modified = header_to_string(&headers, LAST_MODIFIED);

        if status == StatusCode::NOT_MODIFIED {
            return Ok(RawFeedFetch {
                source_id: request.source_id,
                http_status: status.as_u16(),
                etag,
                last_modified,
                not_modified: true,
                raw_payload_bytes: None,
            });
        }

        if !status.is_success() {
            return Err(FeedError::HttpStatus {
                code: status.as_u16(),
            });
        }

        let body = read_limited_body(response, self.max_body_bytes).await?;

        Ok(RawFeedFetch {
            source_id: request.source_id,
            http_status: status.as_u16(),
            etag,
            last_modified,
            not_modified: false,
            raw_payload_bytes: Some(body.to_vec()),
        })
    }
}

fn append_query_param_if_missing(url: &mut Url, key: &str, value: &str) {
    if value.is_empty() || url.query_pairs().any(|(name, _)| name == key) {
        return;
    }
    url.query_pairs_mut().append_pair(key, value);
}

async fn read_limited_body(
    mut response: reqwest::Response,
    max_body_bytes: u64,
) -> Result<BytesMut, FeedError> {
    let mut body = BytesMut::new();
    let mut total = 0_u64;

    while let Some(chunk) = response.chunk().await.map_err(FeedError::from)? {
        total = total.saturating_add(chunk.len() as u64);
        if total > max_body_bytes {
            return Err(FeedError::TooLarge { bytes: total });
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn header_to_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}
