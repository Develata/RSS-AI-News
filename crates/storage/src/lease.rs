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

/// W15 §3：retryable 失败 release 的折叠结果。
///
/// release SQL 内按 `attempt_count >= max_attempts` 折叠（CASE）：耗尽 → 终态，
/// 否则回可领取态。状态转移规则收口在 repo 层，flow 只据此发准确事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseFailureOutcome {
    /// false = lease guard 冲突（行已被 reclaim / 他人持有），未写任何变更。
    pub released: bool,
    /// true = 预算耗尽，本次已折叠进终态（feed/publish `failed`，
    /// ai `permanent_failed`）。
    pub exhausted: bool,
}
