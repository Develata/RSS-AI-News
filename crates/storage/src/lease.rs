use time::{Duration, OffsetDateTime};

/// Lease 持有者标识：跨主机 / 容器 / 进程全局唯一（设计 §5.1）。
///
/// 格式 `{host}-{pid}-{ulid}`：
///   - `host`：`HOSTNAME`（Linux/Docker，容器默认注入容器 id）或 `COMPUTERNAME`
///     （Windows）；都缺失时为 `unknown`。仅作可读定位用（哪台主机/容器持有
///     卡住的 lease），不承担唯一性。
///   - `pid`：进程号。容器内常为 `1`，跨容器会碰撞，故单靠它不足以区分。
///   - `ulid`：每次调用新生成，48-bit 毫秒时间戳 + 80-bit 随机，保证全局唯一。
///
/// 唯一性由 `ulid` 段独立保证；`host`/`pid` 只增可读性。这样 lease guard
/// （`WHERE lease_owner = $owner`）不会因 Docker 共享 PID 命名空间（容器普遍
/// PID 1）而让两个 worker 生成同一 owner、互相误判持有对方刚 claim 的行。
/// 旧格式 `{pid}-{nanos}` 在跨容器场景下仅靠纳秒时间戳防碰撞，不够稳健。
pub fn build_owner_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("{}-{}-{}", host, std::process::id(), ulid::Ulid::new())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn owner_id_is_unique_across_calls() {
        // 同进程内连续调用——唯一性是 lease guard 正确性的前提。ulid 段每次
        // 重新生成（毫秒时间戳 + 80-bit 随机），即便同一毫秒内也应各不相同。
        let ids: HashSet<String> = (0..1000).map(|_| build_owner_id()).collect();
        assert_eq!(ids.len(), 1000, "owner id 必须每次调用唯一，不得碰撞");
    }

    #[test]
    fn owner_id_embeds_pid_segment() {
        // 格式 `{host}-{pid}-{ulid}`：pid 段用于人读定位，必须出现在 id 中。
        let id = build_owner_id();
        let pid = std::process::id().to_string();
        assert!(
            id.contains(&format!("-{pid}-")),
            "owner id `{id}` 应内嵌 `-{pid}-` 段"
        );
    }
}
