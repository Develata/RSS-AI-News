//! Run-event emission, with mandatory secret redaction on every payload.
//!
//! Per `docs/design/error-and-observability.md` §4.2, anything that ends
//! up in the `run_events.context_json` column must pass through the same
//! redaction filter so that callers cannot accidentally persist a raw
//! API key, GitHub token, Authorization header, or URL userinfo simply
//! by stuffing it into a `serde_json::Value` they hand to `emit`.
//!
//! Redaction is applied **before** truncation so that even truncated
//! values keep their secrets masked.

use rss_ai_news_observability::redact::redact_event_context;
use rss_ai_news_storage::{NewRunEvent, RunEventRepository};
use serde_json::Value as JsonValue;

const CONTEXT_JSON_MAX_BYTES: usize = 4096;
const CONTEXT_JSON_PREVIEW_BYTES: usize = 3500;

pub struct RunEventEmitter<'a> {
    pub run_id: &'a str,
    pub stage: &'a str,
    pub repo: &'a dyn RunEventRepository,
}

impl<'a> RunEventEmitter<'a> {
    pub async fn emit(
        &self,
        event_kind: &str,
        severity: &str,
        target_kind: Option<&str>,
        target_id: Option<i64>,
        message: &str,
        context: Option<JsonValue>,
    ) {
        let context_json = context.map(|value| sanitize_and_serialize(value));
        let event = NewRunEvent {
            run_id: self.run_id.to_string(),
            trace_id: None,
            stage: self.stage.to_string(),
            severity: severity.to_string(),
            event_kind: event_kind.to_string(),
            target_kind: target_kind.map(str::to_string),
            target_id,
            message: message.to_string(),
            context_json,
        };

        if let Err(error) = self.repo.insert(&event).await {
            tracing::error!(
                run_id = %self.run_id,
                stage = %self.stage,
                event_kind,
                "failed to persist run_event: {error}"
            );
        }
    }
}

/// Apply the §4.2 redaction filter to `value`, then serialize and
/// truncate. The function takes ownership so the redacted form never
/// leaks back to the caller.
fn sanitize_and_serialize(mut value: JsonValue) -> String {
    redact_event_context(&mut value);
    truncate_serialized(&value)
}

fn truncate_serialized(value: &JsonValue) -> String {
    let serialized = value.to_string();
    if serialized.len() <= CONTEXT_JSON_MAX_BYTES {
        return serialized;
    }

    let preview = serialized
        .char_indices()
        .take_while(|(idx, _)| *idx <= CONTEXT_JSON_PREVIEW_BYTES)
        .map(|(_, ch)| ch)
        .collect::<String>();

    serde_json::json!({
        "truncated": true,
        "original_len": serialized.len(),
        "preview": preview,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_redacts_known_secret_keys_at_any_depth() {
        let secret = "sk-extremely-secret-token-1234567890";
        let value = json!({
            "outer": {
                "openai_api_key": secret,
                "nested": [{"github_token": secret}],
                "note": "fine"
            },
            "url": format!("https://user:{secret}@example.test/path")
        });
        let serialized = sanitize_and_serialize(value);
        assert!(
            !serialized.contains(secret),
            "context_json must not leak raw secrets: {serialized}"
        );
        assert!(serialized.contains("***"));
        assert!(serialized.contains("\"note\":\"fine\""));
    }

    #[test]
    fn sanitize_redacts_authorization_header_embedded_in_string_value() {
        let secret = "ghp-xxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let value = json!({
            "request_dump": format!("GET /\nAuthorization: Bearer {secret}\nHost: api.test"),
        });
        let serialized = sanitize_and_serialize(value);
        assert!(!serialized.contains(secret));
        assert!(serialized.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_redacts_url_userinfo_embedded_in_string_value() {
        let value = json!({
            "callback": "https://operator:hunter2@hooks.test/notify"
        });
        let serialized = sanitize_and_serialize(value);
        assert!(!serialized.contains("hunter2"));
        assert!(serialized.contains("***"));
    }

    #[test]
    fn sanitize_truncates_oversized_payload_after_redaction() {
        let secret = "openai-key-must-not-leak-after-truncation";
        let mut large = String::with_capacity(CONTEXT_JSON_MAX_BYTES + 200);
        for _ in 0..(CONTEXT_JSON_MAX_BYTES / 4) {
            large.push_str("aaa ");
        }
        let value = json!({
            "openai_api_key": secret,
            "padding": large,
        });
        let serialized = sanitize_and_serialize(value);
        // The redaction step ran before truncation, so the secret is
        // already masked even though the payload then went through the
        // preview path.
        assert!(!serialized.contains(secret));
        assert!(serialized.contains("\"truncated\":true"));
    }
}
