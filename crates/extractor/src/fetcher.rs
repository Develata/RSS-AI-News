//! HTTP-only HTML fetcher.

use async_trait::async_trait;
use bytes::BytesMut;
use reqwest::header::{ACCEPT, USER_AGENT};
use reqwest::{Client, Url};
use rss_ai_news_domain::dto::extract::ArticleFetchTask;

use crate::error::ExtractorError;

const USER_AGENT_VALUE: &str = "rss-ai-news/0.1 (+https://github.com/Develata/RSS-AI-News)";
const ACCEPT_VALUE: &str = "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5";

#[derive(Debug, Clone)]
pub struct RawHtmlFetch {
    pub feed_entry_id: i64,
    pub final_url: String,
    pub http_status: u16,
    pub body_bytes: Vec<u8>,
}

#[async_trait]
pub trait HtmlFetcher: Send + Sync {
    /// HTTP-only fetch. Caller (runtime) is responsible for persisting
    /// `body_bytes` to `raw_artifacts` (kind="html_payload",
    /// key={feed_entry_id}) before calling any extract method.
    async fn fetch_html(&self, task: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError>;
}

pub struct ReqwestHtmlFetcher {
    client: Client,
    max_body_bytes: u64,
}

impl ReqwestHtmlFetcher {
    pub fn new(max_body_bytes: u64) -> Result<Self, ExtractorError> {
        let client = Client::builder()
            .user_agent(USER_AGENT_VALUE)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(ExtractorError::from)?;
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
impl HtmlFetcher for ReqwestHtmlFetcher {
    async fn fetch_html(&self, task: &ArticleFetchTask) -> Result<RawHtmlFetch, ExtractorError> {
        let url = Url::parse(&task.normalized_link).map_err(|_| ExtractorError::InvalidUrl {
            url: task.normalized_link.clone(),
        })?;

        let response = self
            .client
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, ACCEPT_VALUE)
            .timeout(task.timeout)
            .send()
            .await
            .map_err(ExtractorError::from)?;

        let status = response.status();
        let final_url = response.url().to_string();
        if !status.is_success() {
            return Err(ExtractorError::HttpStatus {
                code: status.as_u16(),
            });
        }

        let body = read_limited_body(response, self.max_body_bytes).await?;

        Ok(RawHtmlFetch {
            feed_entry_id: task.feed_entry_id,
            final_url,
            http_status: status.as_u16(),
            body_bytes: body.to_vec(),
        })
    }
}

async fn read_limited_body(
    mut response: reqwest::Response,
    max_body_bytes: u64,
) -> Result<BytesMut, ExtractorError> {
    let mut body = BytesMut::new();
    let mut total = 0_u64;

    while let Some(chunk) = response.chunk().await.map_err(ExtractorError::from)? {
        total = total.saturating_add(chunk.len() as u64);
        if total > max_body_bytes {
            return Err(ExtractorError::TooLarge { bytes: total });
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}
