use std::{
    fmt,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use rss_ai_news_domain::{SecretString, dto::ai::AiTask};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::{
    error::{AiError, classify_http_status, is_model_unavailable_error, is_quota_error},
    prompt::{PromptInput, PromptRenderConfig, render_prompt},
};

/// Maximum decoded HTTP response body, including provider error responses.
/// News analysis returns compact JSON; 4 MiB is a generous fixed safety cap
/// independent of an upstream Content-Length or max_tokens promise.
pub const MAX_RESPONSE_BODY_BYTES: usize = 4 * 1024 * 1024;

pub const SYSTEM_MESSAGE: &str =
    "你是新闻分析助手。严格返回 JSON 格式，不要添加 markdown 围栏或额外说明。";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiResponse {
    pub article_ai_result_id: i64,
    pub raw_response: String,
    pub usage: Option<TokenUsage>,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_micro_usd: Option<i64>,
}

#[async_trait]
pub trait AiClient: Send + Sync {
    async fn invoke(&self, task: &AiTask) -> Result<AiResponse, AiError>;
}

/// AI client configuration. `api_key` is wrapped in [`SecretString`] so the
/// raw value is redacted by the type's own `Debug` / `Display` /
/// `Serialize` impls; callers should only `expose_secret()` at the actual
/// HTTP authentication boundary.
#[derive(Clone)]
pub struct AiClientConfig {
    pub api_base: String,
    pub api_key: SecretString,
    pub request_timeout: Duration,
}

impl fmt::Debug for AiClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiClientConfig")
            .field(
                "api_base_origin",
                &Url::parse(&self.api_base)
                    .ok()
                    .map(|url| url.origin().ascii_serialization()),
            )
            .field("api_key", &self.api_key)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

#[derive(Clone)]
pub struct OpenAiCompatClient {
    http_client: reqwest::Client,
    api_key: SecretString,
    chat_completions_url: Url,
    request_timeout: Duration,
}

impl fmt::Debug for OpenAiCompatClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatClient")
            .field("api_key", &self.api_key)
            .field(
                "api_origin",
                &self.chat_completions_url.origin().ascii_serialization(),
            )
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl OpenAiCompatClient {
    pub fn new(cfg: AiClientConfig) -> Result<Self, AiError> {
        if cfg.api_key.expose_secret().trim().is_empty() {
            return Err(AiError::InvalidConfig(
                "api_key must not be empty".to_string(),
            ));
        }

        let chat_completions_url = chat_completions_url(&cfg.api_base)?;
        let http_client = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .build()
            .map_err(|err| AiError::InvalidConfig(err.to_string()))?;

        Ok(Self {
            http_client,
            api_key: cfg.api_key,
            chat_completions_url,
            request_timeout: cfg.request_timeout,
        })
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}

#[async_trait]
impl AiClient for OpenAiCompatClient {
    async fn invoke(&self, task: &AiTask) -> Result<AiResponse, AiError> {
        let start = Instant::now();
        let prompt = render_prompt(
            &task.prompt_template,
            &PromptInput {
                title: &task.title,
                body_text: &task.body_text,
                category_key: &task.category_key,
            },
            &PromptRenderConfig {
                max_input_chars: task.body_text.chars().count(),
            },
        );

        let request_body = json!({
            "model": task.model_id,
            "messages": [
                { "role": "system", "content": SYSTEM_MESSAGE },
                { "role": "user", "content": prompt }
            ],
            "max_tokens": task.max_tokens,
            "temperature": task.temperature,
        });

        let request = self
            .http_client
            .post(self.chat_completions_url.clone())
            .bearer_auth(self.api_key.expose_secret())
            .json(&request_body);
        // RequestBuilder owns the serialized bytes; release intermediate copies
        // before waiting for the network response.
        drop(request_body);
        drop(prompt);
        let response = request
            .send()
            .await
            .map_err(|err| map_reqwest_error(err, self.request_timeout))?;

        let status = response.status();
        let retry_after_seconds = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let body = match read_limited_body(response, self.request_timeout).await {
            Ok(body) => body,
            Err(AiError::ResponseTooLarge { .. }) if !status.is_success() => {
                // Preserve retry semantics for a 429/5xx even if its error page
                // exceeds the cap; never persist that page as an error message.
                return Err(classify_http_status(
                    status.as_u16(),
                    format!("response body exceeded {MAX_RESPONSE_BODY_BYTES} bytes"),
                    retry_after_seconds,
                ));
            }
            Err(err) => return Err(err),
        };

        if !status.is_success() {
            return Err(classify_error_response(
                status.as_u16(),
                String::from_utf8_lossy(&body).into_owned(),
                retry_after_seconds,
                self.api_key.expose_secret(),
            ));
        }

        let envelope: ChatCompletionEnvelope =
            serde_json::from_slice(&body).map_err(|err| AiError::InvalidJson(err.to_string()))?;
        let choice = envelope
            .choices
            .into_iter()
            .next()
            .ok_or(AiError::EmptyResponse)?;
        let raw_response = choice.message.content.ok_or(AiError::EmptyResponse)?;
        let usage = envelope.usage.map(|usage| TokenUsage {
            tokens_in: u64::from(usage.prompt_tokens),
            tokens_out: u64::from(usage.completion_tokens),
            cost_micro_usd: None,
        });

        Ok(AiResponse {
            article_ai_result_id: task.article_ai_result_id,
            raw_response,
            usage,
            latency_ms: millis_u64(start.elapsed()),
        })
    }
}

#[derive(Deserialize)]
struct ChatCompletionEnvelope {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<CompletionUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct CompletionUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

fn classify_error_response(
    code: u16,
    body: String,
    retry_after_seconds: Option<u64>,
    api_key: &str,
) -> AiError {
    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
    let Some(parsed) = parsed else {
        let mut error =
            classify_http_status(code, body.replace(api_key, "***"), retry_after_seconds);
        // Preserve status/body-based classification, but never retain an error
        // body whose JSON escapes could not be decoded safely.
        if let AiError::HttpStatus { message, .. }
        | AiError::RateLimited { message, .. }
        | AiError::QuotaExceeded { message }
        | AiError::ModelUnavailable { message } = &mut error
        {
            *message = format!("provider returned status {code}; unreadable error body omitted");
        }
        return error;
    };
    let api_error = parsed.get("error");
    let field = |name| {
        api_error
            .and_then(|error| error.get(name))
            .and_then(serde_json::Value::as_str)
    };
    let message = field("message").map(str::to_owned).unwrap_or_else(|| {
        // Never echo unrecognized JSON: keys and nested values can contain
        // escaped credentials that a wire-text replacement cannot remove.
        format!("provider returned status {code} with an unrecognized JSON error")
    });
    // Decode JSON escapes before matching the credential. Scrubbing the wire
    // text alone allows e.g. "\u0073k-..." to reappear in persisted diagnostics.
    let message = message.replace(api_key, "***");

    if is_quota_error(field("type"), field("code"), &message) {
        return AiError::QuotaExceeded {
            message: rss_ai_news_domain::error::truncate_diagnostic(message, 8 * 1024),
        };
    }

    if is_model_unavailable_error(field("type"), field("code"), &message) {
        return AiError::ModelUnavailable {
            message: rss_ai_news_domain::error::truncate_diagnostic(message, 8 * 1024),
        };
    }

    classify_http_status(code, message, retry_after_seconds)
}

async fn read_limited_body(
    mut response: reqwest::Response,
    timeout: Duration,
) -> Result<Vec<u8>, AiError> {
    if response
        .content_length()
        .is_some_and(|bytes| bytes > MAX_RESPONSE_BODY_BYTES as u64)
    {
        return Err(AiError::ResponseTooLarge {
            limit: MAX_RESPONSE_BODY_BYTES,
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| map_reqwest_error(err, timeout))?
    {
        if chunk.len() > MAX_RESPONSE_BODY_BYTES - body.len() {
            return Err(AiError::ResponseTooLarge {
                limit: MAX_RESPONSE_BODY_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn map_reqwest_error(err: reqwest::Error, timeout: Duration) -> AiError {
    let err = err.without_url();
    if err.is_timeout() {
        return AiError::HttpTimeout {
            seconds: timeout.as_secs(),
        };
    }

    if let Some(status) = err.status() {
        return classify_http_status(status.as_u16(), err.to_string(), None);
    }

    AiError::ConnectionFailed(err.to_string())
}

fn chat_completions_url(api_base: &str) -> Result<Url, AiError> {
    let mut url = Url::parse(api_base)
        .map_err(|err| AiError::InvalidConfig(format!("api_base invalid: {err}")))?;
    let base_path = url.path().trim_end_matches('/');
    let path = if base_path.ends_with("/v1") {
        format!("{base_path}/chat/completions")
    } else {
        format!("{base_path}/v1/chat/completions")
    };
    url.set_path(&path);
    url.set_query(None);
    Ok(url)
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_client_config_debug_redacts_api_key() {
        // W2-A2 regression guard: AiClientConfig once held api_key as a
        // raw String with a manual Debug impl that printed "<redacted>";
        // SecretString now provides redaction at the type level so the
        // derived Debug suffices and downstream typo-fixes can't reintroduce
        // a leak.
        let secret = "sk-extremely-secret-token-1234567890";
        let cfg = AiClientConfig {
            api_base: "https://example.test/v1".to_string(),
            api_key: SecretString::from(secret),
            request_timeout: Duration::from_secs(5),
        };
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains(secret),
            "Debug must not leak api_key: {rendered}"
        );
        assert!(rendered.contains("***"));
        assert!(rendered.contains("https://example.test"));
    }

    #[test]
    fn open_ai_compat_client_debug_redacts_api_key() {
        let secret = "sk-extremely-secret-token-1234567890";
        let client = OpenAiCompatClient::new(AiClientConfig {
            api_base: "https://example.test/v1".to_string(),
            api_key: SecretString::from(secret),
            request_timeout: Duration::from_secs(5),
        })
        .expect("build client");
        let rendered = format!("{client:?}");
        assert!(
            !rendered.contains(secret),
            "Debug must not leak api_key: {rendered}"
        );
        assert!(rendered.contains("***"));
    }

    #[test]
    fn classify_error_response_detects_model_unavailable_from_json_code() {
        // W14-A：reqwest 路径——JSON error 体里 code=model_not_found → ModelUnavailable。
        let body =
            r#"{"error":{"message":"The model `gpt-x` does not exist","code":"model_not_found"}}"#;
        let err = classify_error_response(404, body.to_string(), None, "sk-test");
        assert!(matches!(err, AiError::ModelUnavailable { .. }));
    }

    #[test]
    fn classify_error_response_detects_model_unavailable_from_plain_text() {
        // W14-A：reqwest 路径——非 JSON 纯文本体含 "model not found" → ModelUnavailable。
        let err = classify_error_response(
            404,
            "404 page not found: model not found".to_string(),
            None,
            "sk-test",
        );
        assert!(matches!(err, AiError::ModelUnavailable { .. }));
    }

    #[test]
    fn reqwest_errors_drop_sensitive_urls() {
        let error = reqwest::Client::new()
            .get("not a URL")
            .build()
            .expect_err("invalid URL")
            .with_url(
                Url::parse("https://user:password@example.test/private?key=secret").expect("URL"),
            );
        let error = map_reqwest_error(error, Duration::from_secs(2));
        let rendered = format!("{error:?} {error}");
        for secret in ["password", "private", "secret"] {
            assert!(!rendered.contains(secret));
        }
    }
}
