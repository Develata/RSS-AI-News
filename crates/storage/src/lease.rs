use time::{Duration, OffsetDateTime};

/// 首版 owner id 只保证同进程内不同调用大概率唯一。
/// TODO: 后续可按设计 §5.1 严格化为 `{hostname}-{pid}-{random_ulid}`。
pub fn build_owner_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    )
}

pub fn lease_expires_at(now: OffsetDateTime, duration: Duration) -> OffsetDateTime {
    now + duration
}

#[derive(Debug, Clone)]
pub struct ClaimRequest {
    pub owner: String,
    pub now: OffsetDateTime,
    pub lease_expires_at: OffsetDateTime,
    pub batch_size: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    Success,
    RetryableFailure { error: String, kind: String },
    PermanentFailure { error: String, kind: String },
}
