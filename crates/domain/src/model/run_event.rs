use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct RunEvent {
    pub id: i64,
    pub run_id: String,
    pub trace_id: Option<String>,
    pub stage: String,
    pub severity: String,
    pub event_kind: String,
    pub target_kind: Option<String>,
    pub target_id: Option<i64>,
    pub message: String,
    pub context_json: Option<String>,
    pub created_at: OffsetDateTime,
}
