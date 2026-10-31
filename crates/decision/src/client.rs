//! `POST /v1/systemone` client (ADR 0010 §2, §5). No retries: callers decide.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use rss_ai_news_domain::SecretString;
use url::Url;

use crate::{
    error::{DecisionError, classify_status, error_code},
    provider::{self, Provider, WireParams},
    question::{QuestionSpec, normalize},
    response::{DecisionResponse, WireResponse},
};

/// Answers for at most 50 questions are a few KiB; 1 MiB is a generous fixed
/// cap independent of any Content-Length the provider sends.
pub const MAX_RESPONSE_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct DecisionRequest<'a> {
    pub title: &'a str,
    /// Truncated to `max_state_chars` before sending; independent of the
    /// generative model's `max_input_chars`.
    pub body: &'a str,
    pub questions: &'a [QuestionSpec],
}

#[async_trait]
pub trait DecisionClient: Send + Sync {
    async fn decide(
        &self,
        request: &DecisionRequest<'_>,
    ) -> Result<DecisionResponse, DecisionError>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct ClientSettings {
    pub provider: Provider,
    /// Server root; the client appends `v1/systemone`. No credentials, query
    /// or fragment.
    pub base_url: Url,
    pub model: String,
    pub request_timeout: Duration,
    pub max_state_chars: u32,
    /// Sent on every Laya request; ignored for Jev.
    pub laya_max_len: u32,
    /// Honour `HTTP(S)_PROXY` / `NO_PROXY`. Configured clients do; loopback
    /// mock tests turn it off, because reqwest does not match a `127.*`
    /// `NO_PROXY` entry.
    pub use_env_proxy: bool,
}

/// Origin only: a configured path is not printed.
impl fmt::Debug for ClientSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientSettings")
            .field("provider", &self.provider)
            .field("origin", &self.base_url.origin().ascii_serialization())
            .field("model", &self.model)
            .field("request_timeout", &self.request_timeout)
            .field("max_state_chars", &self.max_state_chars)
            .field("laya_max_len", &self.laya_max_len)
            .field("use_env_proxy", &self.use_env_proxy)
            .finish_non_exhaustive()
    }
}

/// `base_url` rule shared with config validation: http(s) with a host, and
/// nothing that could carry a secret into logs (userinfo, query, fragment).
pub(crate) fn check_base_url(url: &Url) -> Result<(), &'static str> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("must be an http(s) URL with a host");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("must not contain credentials, a query or a fragment");
    }
    Ok(())
}

pub struct SystemOneClient {
    http: reqwest::Client,
    endpoint: Url,
    api_key: SecretString,
    settings: ClientSettings,
}

impl fmt::Debug for SystemOneClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemOneClient")
            .field("provider", &self.settings.provider)
            .field("origin", &self.endpoint.origin().ascii_serialization())
            .field("model", &self.settings.model)
            .field("api_key", &self.api_key)
            .finish()
    }
}

impl SystemOneClient {
    pub fn new(settings: ClientSettings, api_key: SecretString) -> Result<Self, DecisionError> {
        if api_key.expose_secret().trim().is_empty() {
            return Err(DecisionError::InvalidConfig(
                "api key must not be empty".to_string(),
            ));
        }
        provider::check_model(settings.provider, &settings.model)
            .map_err(DecisionError::InvalidConfig)?;
        if settings.provider == Provider::Laya
            && !provider::LAYA_MAX_LEN.contains(&settings.laya_max_len)
        {
            return Err(DecisionError::InvalidConfig(
                "laya_max_len must be in 512..=8192".to_string(),
            ));
        }
        if settings.max_state_chars == 0 || settings.request_timeout.is_zero() {
            return Err(DecisionError::InvalidConfig(
                "max_state_chars and request_timeout must be positive".to_string(),
            ));
        }
        let endpoint = systemone_url(&settings.base_url)?;
        // A redirect would resend the bearer key to wherever `Location`
        // points (reqwest only strips it across hosts), so 3xx is an error.
        let mut builder = reqwest::Client::builder()
            .timeout(settings.request_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if !settings.use_env_proxy {
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|_| DecisionError::InvalidConfig("cannot build HTTP client".to_string()))?;
        Ok(Self {
            http,
            endpoint,
            api_key,
            settings,
        })
    }

    pub fn settings(&self) -> &ClientSettings {
        &self.settings
    }

    fn map_send_error(&self, error: &reqwest::Error) -> DecisionError {
        if error.is_timeout() {
            DecisionError::Timeout {
                seconds: self.settings.request_timeout.as_secs(),
            }
        } else {
            DecisionError::Connection
        }
    }

    async fn read_limited_body(
        &self,
        mut response: reqwest::Response,
    ) -> Result<Vec<u8>, DecisionError> {
        // Same shape as crates/ai/src/client.rs `read_limited_body`: reject an
        // oversized Content-Length up front, then cap the streamed bytes.
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_RESPONSE_BODY_BYTES as u64)
        {
            return Err(DecisionError::ResponseTooLarge {
                limit: MAX_RESPONSE_BODY_BYTES,
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.map_send_error(&error))?
        {
            if chunk.len() > MAX_RESPONSE_BODY_BYTES - body.len() {
                return Err(DecisionError::ResponseTooLarge {
                    limit: MAX_RESPONSE_BODY_BYTES,
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[async_trait]
impl DecisionClient for SystemOneClient {
    async fn decide(
        &self,
        request: &DecisionRequest<'_>,
    ) -> Result<DecisionResponse, DecisionError> {
        let settings = &self.settings;
        if request.questions.is_empty() {
            return Err(DecisionError::InvalidRequest(
                "no questions to ask".to_string(),
            ));
        }
        let questions = normalize(request.questions);
        if let Some(issue) =
            provider::validate_questions(settings.provider, &questions, "questions")
                .into_iter()
                .next()
        {
            return Err(DecisionError::InvalidRequest(format!(
                "{}: {}",
                issue.field, issue.message
            )));
        }
        let params = WireParams {
            model: &settings.model,
            laya_max_len: settings.laya_max_len,
        };
        let body = truncate_chars(request.body, settings.max_state_chars as usize);
        let payload = provider::encode(settings.provider, &params, request.title, body, &questions);

        let response = self
            .http
            .post(self.endpoint.clone())
            .bearer_auth(self.api_key.expose_secret())
            .json(&payload)
            .send()
            .await
            .map_err(|error| self.map_send_error(&error))?;
        let status = response.status();
        let retry_after_seconds = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(retry_after_seconds);
        let body = match self.read_limited_body(response).await {
            Ok(body) => body,
            // An error page that is oversized, stalls or breaks off still
            // keeps its status semantics; only the code is lost.
            Err(_) if !status.is_success() => {
                return Err(classify_status(status.as_u16(), None, retry_after_seconds));
            }
            Err(error) => return Err(error),
        };
        if !status.is_success() {
            return Err(classify_status(
                status.as_u16(),
                error_code(&body, self.api_key.expose_secret()),
                retry_after_seconds,
            ));
        }
        // serde messages can quote response content; report the category only.
        let wire: WireResponse =
            serde_json::from_slice(&body).map_err(|error| DecisionError::InvalidResponse {
                reason: format!(
                    "response does not match the decision schema ({:?})",
                    error.classify()
                ),
            })?;
        provider::decode(settings.provider, &params, &questions, wire)
            .map_err(|reason| DecisionError::InvalidResponse { reason })
    }
}

/// `Retry-After` in delay-seconds form only (RFC 9110 §10.2.3 `1*DIGIT`).
/// The HTTP-date form is not parsed: the value is a hint, and Phase A never
/// retries, so a date is reported as no hint.
fn retry_after_seconds(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // All digits, so only overflow fails; an absurd delay is no hint either.
    value.parse().ok()
}

fn systemone_url(base: &Url) -> Result<Url, DecisionError> {
    check_base_url(base)
        .map_err(|message| DecisionError::InvalidConfig(format!("base_url {message}")))?;
    let mut root = base.clone();
    if !root.path().ends_with('/') {
        let path = format!("{}/", root.path());
        root.set_path(&path);
    }
    root.join("v1/systemone")
        .map_err(|_| DecisionError::InvalidConfig("invalid base_url".to_string()))
}

/// At most `max_chars` Unicode scalar values, cut on a char boundary.
pub(crate) fn truncate_chars(input: &str, max_chars: usize) -> &str {
    match input.char_indices().nth(max_chars) {
        Some((end, _)) => &input[..end],
        None => input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_keeps_a_path_prefix() {
        let url = |base: &str| {
            systemone_url(&Url::parse(base).unwrap())
                .unwrap()
                .to_string()
        };
        assert_eq!(
            url("https://api.typesafe.ai"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            url("http://host:8080/laya"),
            "http://host:8080/laya/v1/systemone"
        );
    }

    #[test]
    fn base_url_must_not_carry_secrets() {
        for bad in [
            "file:///tmp",
            "http://user:pass@host",
            "http://user@host",
            "http://host/?token=x",
            "http://host/#x",
        ] {
            assert!(systemone_url(&Url::parse(bad).unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn retry_after_is_delay_seconds_only() {
        assert_eq!(retry_after_seconds(" 7 "), Some(7));
        assert_eq!(retry_after_seconds("0"), Some(0));
        for hint in [
            "+7",
            "-1",
            "1.5",
            "",
            "99999999999999999999999",
            "Wed, 21 Oct 2026 07:28:00 GMT",
        ] {
            assert_eq!(retry_after_seconds(hint), None, "{hint:?}");
        }
    }

    #[test]
    fn settings_debug_prints_the_origin_only() {
        let settings = ClientSettings {
            provider: Provider::Laya,
            base_url: Url::parse("http://laya.internal:8080/secret-path").unwrap(),
            model: "multilingual".to_string(),
            request_timeout: Duration::from_secs(1),
            max_state_chars: 10,
            laya_max_len: 2048,
            use_env_proxy: true,
        };
        let debug = format!("{settings:?}");
        assert!(debug.contains("http://laya.internal:8080"));
        assert!(!debug.contains("secret-path"));
    }

    #[test]
    fn truncation_counts_chars_not_bytes() {
        assert_eq!(truncate_chars("中文正文", 2), "中文");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abc", 0), "");
    }
}
