use std::{
    fmt,
    time::{Duration, Instant},
};

use async_openai::{Client, config::OpenAIConfig};
use async_trait::async_trait;
use rss_ai_news_domain::{SecretString, dto::ai::AiTask};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::{
    error::{AiError, classify_http_status, is_quota_error},
    prompt::{PromptInput, PromptRenderConfig, render_prompt},
};

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
#[derive(Clone, Debug)]
pub struct AiClientConfig {
    pub api_base: String,
    pub api_key: SecretString,
    pub request_timeout: Duration,
}

#[derive(Clone)]
pub struct OpenAiCompatClient {
    inner: Client<OpenAIConfig>,
    http_client: reqwest::Client,
    api_key: SecretString,
    chat_completions_url: Url,
    request_timeout: Duration,
}

impl fmt::Debug for OpenAiCompatClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatClient")
            .field("inner", &"<async_openai::Client>")
            .field("api_key", &self.api_key)
            .field("chat_completions_url", &self.chat_completions_url)
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

        let openai_config = OpenAIConfig::new()
            .with_api_base(cfg.api_base)
            .with_api_key(cfg.api_key.expose_secret().to_owned());
        let inner = Client::with_config(openai_config).with_http_client(http_client.clone());

        Ok(Self {
            inner,
            http_client,
            api_key: cfg.api_key,
            chat_completions_url,
            request_timeout: cfg.request_timeout,
        })
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn async_openai_client(&self) -> &Client<OpenAIConfig> {
        &self.inner
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvokeOptions {
    /// 用于 prompt 渲染（render_prompt 内的截断 buffer）。默认沿用 task.body_text 长度。
    pub max_input_chars: Option<usize>,
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

        let response = self
            .http_client
            .post(self.chat_completions_url.clone())
            .bearer_auth(self.api_key.expose_secret())
            .json(&request_body)
            .send()
            .await
            .map_err(|err| map_reqwest_error(err, self.request_timeout))?;

        let status = response.status();
        let retry_after_seconds = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let body = response
            .text()
            .await
            .map_err(|err| map_reqwest_error(err, self.request_timeout))?;

        if !status.is_success() {
            return Err(classify_error_response(
                status.as_u16(),
                body,
                retry_after_seconds,
            ));
        }

        let envelope: ChatCompletionEnvelope =
            serde_json::from_str(&body).map_err(|err| AiError::InvalidJson(err.to_string()))?;
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

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: Option<ApiErrorBody>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    message: String,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

fn classify_error_response(code: u16, body: String, retry_after_seconds: Option<u64>) -> AiError {
    let parsed = serde_json::from_str::<ErrorEnvelope>(&body).ok();
    let Some(api_error) = parsed.and_then(|envelope| envelope.error) else {
        return classify_http_status(code, body, retry_after_seconds);
    };

    if is_quota_error(
        api_error.r#type.as_deref(),
        api_error.code.as_deref(),
        &api_error.message,
    ) {
        return AiError::QuotaExceeded {
            message: api_error.message,
        };
    }

    classify_http_status(code, api_error.message, retry_after_seconds)
}

fn map_reqwest_error(err: reqwest::Error, timeout: Duration) -> AiError {
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
}
