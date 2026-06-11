//! flow 启动期 maintenance（15 §5）：① `reclaim_expired_leases`（过期
//! running 类 → 可领取类）+ ② `terminalize_exhausted`（预算耗尽的可领取行
//! → 终态）。规则：每个 CLI run 在目标表首次 claim 前执行一次 ① + ②，
//! 顺序固定 ① 在前——崩溃在最后一次尝试时，行卡在 running 类，必须先被
//! reclaim 送回可领取类，sweep 的谓词才能收走它（15 §2）。
//!
//! 两步均为 best-effort：失败只记 warn 不中断 run。maintenance 不可用时
//! flow 本体仍能工作，滞留行等下一次 run 或 doctor 兜底（15 §7）。

use rss_ai_news_storage::StorageError;
use serde_json::json;

use crate::events::RunEventEmitter;

/// 把 ① reclaim / ② sweep 的结果落成 run_events（15 §7）：
///
/// - 影响行数 > 0 时各 emit 一条：`leases_reclaimed`（info）/
///   `retry_budget_swept`（warn，预算耗尽意味着上游存在持续失败）；
///   = 0 时静默，不产生事件噪音。
/// - `Err` 走 `tracing::warn`（best-effort，不产事件、不中断调用方）。
/// - `swept = None` 表示该 flow 无 sweep 语义（reindex：claim 不过滤
///   attempt_count、失败直转终态，不存在耗尽卡死，15 §5）。
pub(crate) async fn emit_maintenance_outcome(
    emitter: &RunEventEmitter<'_>,
    table: &str,
    reclaimed: Result<u64, StorageError>,
    swept: Option<Result<u64, StorageError>>,
) {
    match reclaimed {
        Ok(0) => {}
        Ok(count) => {
            emitter
                .emit(
                    "leases_reclaimed",
                    "info",
                    None,
                    None,
                    "expired leases reclaimed at run start",
                    Some(json!({ "table": table, "count": count })),
                )
                .await;
        }
        Err(error) => {
            tracing::warn!(
                table,
                "reclaim_expired_leases failed; continuing run (best-effort): {error}"
            );
        }
    }

    match swept {
        None | Some(Ok(0)) => {}
        Some(Ok(count)) => {
            emitter
                .emit(
                    "retry_budget_swept",
                    "warn",
                    None,
                    None,
                    "claimable rows with exhausted retry budget terminalized",
                    Some(json!({ "table": table, "count": count })),
                )
                .await;
        }
        Some(Err(error)) => {
            tracing::warn!(
                table,
                "terminalize_exhausted failed; continuing run (best-effort): {error}"
            );
        }
    }
}
