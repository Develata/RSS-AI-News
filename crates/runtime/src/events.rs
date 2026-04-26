use rss_ai_news_storage::{NewRunEvent, RunEventRepository};
use serde_json::Value as JsonValue;

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
        let context_json = context.as_ref().map(truncate_context);
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

fn truncate_context(value: &JsonValue) -> String {
    let serialized = value.to_string();
    if serialized.len() <= 4096 {
        return serialized;
    }

    let preview = serialized
        .char_indices()
        .take_while(|(idx, _)| *idx <= 3500)
        .map(|(_, ch)| ch)
        .collect::<String>();

    serde_json::json!({
        "truncated": true,
        "original_len": serialized.len(),
        "preview": preview,
    })
    .to_string()
}
