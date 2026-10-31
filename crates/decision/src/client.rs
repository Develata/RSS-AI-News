//! `POST /v1/systemone` client (ADR 0010 §2, §5; ADR 0011 §9). No retries:
//! callers decide, and reqwest's own automatic retry is switched off.

use std::{
    fmt,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use rss_ai_news_domain::SecretString;
use sha2::{Digest, Sha256};
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
    /// One request, finished by `deadline`: the per-request timeout is the
    /// smaller of the configured timeout and the time left. A known non-2xx
    /// status is kept even when its body times out.
    async fn decide(&self, request: &DecisionRequest<'_>, deadline: Instant) -> Exchange;
}

/// The outcome of one call, with what is needed to record it (ADR 0011 §8–§9).
#[derive(Debug)]
pub struct Exchange {
    pub result: Result<DecisionResponse, DecisionError>,
    /// `None` when the request was rejected before it was built.
    pub sent: Option<SentState>,
    /// `Some` only for a 2xx response.
    pub raw: Option<RawInfo>,
}

impl Exchange {
    fn rejected(error: DecisionError) -> Self {
        Self {
            result: Err(error),
            sent: None,
            raw: None,
        }
    }
}

/// What was sent as `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentState {
    /// SHA-256 (lowercase hex) of the compact `state` JSON, keys sorted.
    pub state_sha256: String,
    /// Body length in Unicode scalar values after `max_state_chars`.
    pub body_chars: u32,
    pub body_truncated: bool,
}

/// Why a 2xx body is not handed out for storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// Not JSON: other encodings could carry the key past any check.
    NotJson,
    /// The body contains the API key.
    Credential,
    /// Oversized or broken off; `sha256` is absent.
    Incomplete,
}

impl Withheld {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotJson => "not_json",
            Self::Credential => "credential",
            Self::Incomplete => "incomplete",
        }
    }
}

/// A 2xx response body as seen by the client, which holds the key; the
/// caller never touches a body that failed the checks.
#[derive(Clone, PartialEq, Eq)]
pub struct RawInfo {
    /// Bytes read (all of them unless `withheld` is `Incomplete`).
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the complete body.
    pub sha256: Option<String>,
    /// The body when it is JSON without the key.
    pub body: Option<Vec<u8>>,
    pub withheld: Option<Withheld>,
}

/// Lengths and digest only, never bytes.
impl fmt::Debug for RawInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawInfo")
            .field("bytes", &self.bytes)
            .field("sha256", &self.sha256)
            .field("captured", &self.body.is_some())
            .field("withheld", &self.withheld)
            .finish()
    }
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
        // A key with e.g. a trailing newline makes every request fail to
        // build without being sent; reject it here, without the value.
        if reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_key.expose_secret()))
            .is_err()
        {
            return Err(DecisionError::InvalidConfig(
                "api key is not a valid HTTP header value".to_string(),
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
        // reqwest retries some protocol-level failures by default; off, so
        // one call is at most one physical send (ADR 0011 §6).
        let mut builder = reqwest::Client::builder()
            .timeout(settings.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never());
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

    /// `scheme://host[:port]` of the endpoint, recorded with each result.
    pub fn endpoint_origin(&self) -> String {
        self.endpoint.origin().ascii_serialization()
    }

    pub fn execution_key(&self) -> String {
        provider::execution_key(
            self.settings.provider,
            &self.settings.model,
            self.settings.laya_max_len,
        )
    }

    /// By stage: never built (not sent), timed out, never connected (not
    /// sent), or broken afterwards (may have been sent).
    fn map_reqwest_error(error: &reqwest::Error, timeout: Duration) -> DecisionError {
        if error.is_builder() {
            DecisionError::InvalidConfig("request could not be built".to_string())
        } else if error.is_timeout() {
            DecisionError::Timeout {
                seconds: timeout.as_secs(),
            }
        } else if error.is_connect() {
            DecisionError::Connection
        } else {
            DecisionError::Transport
        }
    }

    /// The body, or the error with the number of bytes read before it.
    async fn read_limited_body(
        mut response: reqwest::Response,
        timeout: Duration,
    ) -> Result<Vec<u8>, (DecisionError, u64)> {
        // Same shape as crates/ai/src/client.rs `read_limited_body`: reject an
        // oversized Content-Length up front, then cap the streamed bytes.
        let too_large = DecisionError::ResponseTooLarge {
            limit: MAX_RESPONSE_BODY_BYTES,
        };
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_RESPONSE_BODY_BYTES as u64)
        {
            return Err((too_large, 0));
        }
        let mut body = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => return Ok(body),
                Err(error) => {
                    return Err((Self::map_reqwest_error(&error, timeout), body.len() as u64));
                }
            };
            if chunk.len() > MAX_RESPONSE_BODY_BYTES - body.len() {
                return Err((too_large, (body.len() + chunk.len()) as u64));
            }
            body.extend_from_slice(&chunk);
        }
    }

    /// Length, digest and, when safe, the body itself (ADR 0011 §8–§9).
    fn capture(&self, body: &[u8]) -> RawInfo {
        let withheld = if serde_json::from_slice::<serde::de::IgnoredAny>(body).is_err() {
            Some(Withheld::NotJson)
        } else if contains_credential(body, self.api_key.expose_secret()) {
            Some(Withheld::Credential)
        } else {
            None
        };
        RawInfo {
            bytes: body.len() as u64,
            sha256: Some(hex::encode(Sha256::digest(body))),
            body: withheld.is_none().then(|| body.to_vec()),
            withheld,
        }
    }

    fn decode_body(
        &self,
        body: &[u8],
        params: &WireParams<'_>,
        questions: &[QuestionSpec],
    ) -> Result<DecisionResponse, DecisionError> {
        // serde messages can quote response content; report the category only.
        let wire: WireResponse =
            serde_json::from_slice(body).map_err(|error| DecisionError::InvalidResponse {
                reason: format!(
                    "response does not match the decision schema ({:?})",
                    error.classify()
                ),
            })?;
        let response = provider::decode(self.settings.provider, params, questions, wire)
            .map_err(|reason| DecisionError::InvalidResponse { reason })?;
        // The only free text that is stored; a reflected key must not land.
        let key = self.api_key.expose_secret();
        let execution = &response.execution;
        if execution.reported_model.contains(key)
            || execution
                .routing_model
                .as_deref()
                .is_some_and(|model| model.contains(key))
        {
            return Err(DecisionError::InvalidResponse {
                reason: "response reflects the credential".to_string(),
            });
        }
        Ok(response)
    }
}

#[async_trait]
impl DecisionClient for SystemOneClient {
    async fn decide(&self, request: &DecisionRequest<'_>, deadline: Instant) -> Exchange {
        let settings = &self.settings;
        if request.questions.is_empty() {
            return Exchange::rejected(DecisionError::InvalidRequest(
                "no questions to ask".to_string(),
            ));
        }
        let questions = normalize(request.questions);
        if let Some(issue) =
            provider::validate_questions(settings.provider, &questions, "questions")
                .into_iter()
                .next()
        {
            return Exchange::rejected(DecisionError::InvalidRequest(format!(
                "{}: {}",
                issue.field, issue.message
            )));
        }
        let params = WireParams {
            model: &settings.model,
            laya_max_len: settings.laya_max_len,
        };
        let body = truncate_chars(request.body, settings.max_state_chars as usize);
        let state = provider::state_value(request.title, body);
        let sent = SentState {
            state_sha256: hex::encode(Sha256::digest(state.to_string().as_bytes())),
            body_chars: u32::try_from(body.chars().count()).unwrap_or(u32::MAX),
            body_truncated: body.len() < request.body.len(),
        };
        let payload = provider::encode(settings.provider, &params, state, &questions);
        let timeout = settings
            .request_timeout
            .min(deadline.saturating_duration_since(Instant::now()));
        if timeout.is_zero() {
            return Exchange::rejected(DecisionError::Timeout { seconds: 0 });
        }
        let failed = |error| Exchange {
            result: Err(error),
            sent: Some(sent.clone()),
            raw: None,
        };

        let response = match self
            .http
            .post(self.endpoint.clone())
            .timeout(timeout)
            .bearer_auth(self.api_key.expose_secret())
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => return failed(Self::map_reqwest_error(&error, timeout)),
        };
        let status = response.status();
        let retry_after_seconds = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(retry_after_seconds);
        let read = Self::read_limited_body(response, timeout).await;
        if !status.is_success() {
            // An error page that is oversized, stalls or breaks off still
            // keeps its status semantics; only the code is lost.
            let code = read
                .ok()
                .and_then(|body| error_code(&body, self.api_key.expose_secret()));
            return failed(classify_status(status.as_u16(), code, retry_after_seconds));
        }
        let body = match read {
            Ok(body) => body,
            Err((error, bytes)) => {
                return Exchange {
                    result: Err(error),
                    sent: Some(sent),
                    raw: Some(RawInfo {
                        bytes,
                        sha256: None,
                        body: None,
                        withheld: Some(Withheld::Incomplete),
                    }),
                };
            }
        };
        Exchange {
            result: self.decode_body(&body, &params, &questions),
            sent: Some(sent),
            raw: Some(self.capture(&body)),
        }
    }
}

/// Whether `body` carries `key`, as raw bytes or inside any JSON string
/// literal once its escapes (`\uXXXX`, `\/`, ...) are decoded. The scan is
/// lexical, so duplicate object keys are all seen, not just the last one.
pub(crate) fn contains_credential(body: &[u8], key: &str) -> bool {
    let needle = key.as_bytes();
    if needle.is_empty() {
        return false;
    }
    if body.windows(needle.len()).any(|window| window == needle) {
        return true;
    }
    let mut index = 0;
    while index < body.len() {
        if body[index] != b'"' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < body.len() && body[index] != b'"' {
            index += if body[index] == b'\\' { 2 } else { 1 };
        }
        let end = (index + 1).min(body.len());
        if serde_json::from_slice::<String>(&body[start..end]).is_ok_and(|text| text.contains(key))
        {
            return true;
        }
        index = end;
    }
    false
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
    fn credential_scan_sees_escapes_and_every_duplicate_key() {
        let key = "sk-abc";
        assert!(contains_credential(br#"{"a":"x sk-abc y"}"#, key));
        assert!(contains_credential(br#"{"a":"sk\u002dabc"}"#, key));
        assert!(contains_credential(
            br#"{"m":"sk-ab\u0063","m":"clean"}"#,
            key
        ));
        assert!(contains_credential(br#"{"sk\u002dabc":1}"#, key));
        assert!(!contains_credential(br#"{"a":"sk-ab","b":"c"}"#, key));
        assert!(!contains_credential(br#"{"a":"unterminated"#, key));
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
