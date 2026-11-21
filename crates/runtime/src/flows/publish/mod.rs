//! publish flow：把 ready_for_publish 的文章 freeze→render→store_local→remote
//! 多阶段编排落地。
//!
//! 按阶段拆分为子模块，各自 `impl PublishFlow`：
//! `freeze` / `render` / `store_local` / `remote`；DTO 见 `dto`，
//! 经 `pub use dto::*` 保持对外 API 路径不变。本文件保留入口
//! （`new` / `build_idempotency_key` / `init`）、`PublishFlow` 本体，以及跨阶段
//! 共享的 `fail_claimed`（失败回收）与 `render_templates_from_ctx`（模板装配）
//! ——后两者为父模块私有项，子模块据 Rust 可见性规则可直接访问。

use std::sync::Arc;

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_report::{RenderTemplates, ReportError};
use rss_ai_news_storage::NewPublishRecord;
use serde_json::json;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::context::PublishDeps;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

mod dto;
mod freeze;
pub(crate) use freeze::published_since;
mod paths;
mod remote;
mod render;
mod report_date;
mod rule_versions;
mod store_local;

pub use dto::*;
pub use paths::{ReportPathSpec, check_report_path_collisions};
pub use report_date::default_report_date;
pub use rule_versions::{ForceRenderTrace, PublishRuleVersions, bootstrap_publish_rule_versions};

pub struct PublishFlow {
    ctx: Arc<PublishDeps>,
}

fn render_templates_from_ctx(ctx: &PublishDeps, path_template: Option<&str>) -> RenderTemplates {
    let template = &ctx.template;
    RenderTemplates {
        path_template: path_template
            .filter(|path_template| !path_template.trim().is_empty())
            .unwrap_or(&template.path_template)
            .to_string(),
        frontmatter_template: template.frontmatter_template.clone(),
        report_template: template.report_template.clone(),
        item_template: template.item_template.clone(),
    }
}

/// The report's `generated_at`, fixed once by the first render and persisted
/// as `publish_records.rendered_at`: UTC, truncated to whole microseconds, the
/// finest precision both backends return unchanged (PostgreSQL `timestamptz`
/// is microseconds; SQLite keeps the text as written). A value that survives
/// the round trip is what lets store_local, remote retries and rebuild-report
/// re-render byte-identical Markdown.
fn report_timestamp(at: OffsetDateTime) -> OffsetDateTime {
    let at = at.to_offset(UtcOffset::UTC);
    at - Duration::nanoseconds(i64::from(at.nanosecond() % 1_000))
}

/// Timestamp for re-rendering an already rendered record: the persisted
/// `rendered_at`; the caller's value only for a record that has none.
fn rendered_timestamp(
    rendered_at: Option<OffsetDateTime>,
    caller: OffsetDateTime,
) -> OffsetDateTime {
    rendered_at.unwrap_or(caller)
}

impl PublishFlow {
    pub fn new(ctx: Arc<PublishDeps>) -> Self {
        Self { ctx }
    }

    pub fn build_idempotency_key(
        category_key: &str,
        report_date: &str,
        render_version: i64,
    ) -> String {
        format!("{category_key}-{report_date}-v{render_version}")
    }

    pub async fn init(
        &self,
        opts: PublishInitOptions,
    ) -> Result<PublishInitOutcome, crate::RuntimeError> {
        let key =
            Self::build_idempotency_key(&opts.category_key, &opts.report_date, opts.render_version);
        let new_record = NewPublishRecord {
            idempotency_key: key.clone(),
            category_key: opts.category_key,
            report_date: opts.report_date,
            target_timezone: opts.target_timezone,
            render_version: opts.render_version,
            selection_policy_version: opts.selection_policy_version,
            remote_target: opts.remote_target,
        };
        match self
            .ctx
            .publish_record_repo
            .create_if_new(&new_record)
            .await?
        {
            Some(id) => Ok(PublishInitOutcome::Created {
                publish_record_id: id,
            }),
            None => {
                let existing = self
                    .ctx
                    .publish_record_repo
                    .find_by_idempotency_key(&key)
                    .await?
                    .ok_or_else(|| {
                        crate::RuntimeError::Config(format!(
                            "idempotency_key {key} disappeared after conflict"
                        ))
                    })?;
                Ok(PublishInitOutcome::AlreadyExists {
                    publish_record_id: existing.id,
                    state: existing.state,
                })
            }
        }
    }

    /// W15 §5：publish_records 的启动期 maintenance（① reclaim + ② sweep，
    /// 顺序固定，best-effort）。publish CLI 支持断点续跑，freeze / render /
    /// store_local / publish_remote（单条与 batch）任一阶段都可能是本次 run
    /// 对 publish_records 的首次 claim，故每个阶段入口各执行一次（codex
    /// W15-P4 复审）。幂等、count=0 静默，同 run 内重复无害。
    async fn run_publish_maintenance(&self, emitter: &RunEventEmitter<'_>) {
        let now = OffsetDateTime::now_utc();
        let reclaimed = self
            .ctx
            .publish_record_repo
            .reclaim_expired_leases(now)
            .await;
        let swept = self
            .ctx
            .publish_record_repo
            .terminalize_exhausted(self.ctx.retry.publish_max_attempts, now)
            .await;
        emit_maintenance_outcome(emitter, "publish_records", reclaimed, Some(swept)).await;
    }

    /// Releases a claimed record after a stage failure, by classification:
    /// retryable errors (e.g. a transient [`rss_ai_news_storage::StorageError`]
    /// wrapped in [`ReportError::Storage`]) keep the stage state and are charged
    /// to the current stage's retry budget (ADR 0012, folded to `failed` in SQL once
    /// exhausted); everything else is a permanent `failed`. A transient DB blip
    /// must never become the day's terminal verdict, because the idempotency
    /// key would then return the failed record on every later run.
    ///
    /// `&dyn ClassifiedError` lets report, storage and publish errors share one
    /// release path without a generic per call site.
    async fn release_classified_failure(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &dyn ClassifiedError,
        phase: &'static str,
        now: OffsetDateTime,
        emitter: &RunEventEmitter<'_>,
    ) {
        let message = error.display_user();
        let retryable = error.is_retryable();
        let mut budget_exhausted = false;
        let release_result = if retryable {
            self.ctx
                .publish_record_repo
                .release_retryable_failure(
                    publish_record_id,
                    owner,
                    &message,
                    error.error_kind(),
                    self.ctx.retry.publish_max_attempts,
                    now,
                )
                .await
                .map(|outcome| {
                    budget_exhausted = outcome.exhausted;
                    outcome.released
                })
        } else {
            self.ctx
                .publish_record_repo
                .release_permanent_failure(
                    publish_record_id,
                    owner,
                    &message,
                    error.error_kind(),
                    now,
                )
                .await
        };
        if let Err(persist_err) = release_result {
            tracing::warn!(
                publish_record_id,
                phase,
                retryable,
                ?persist_err,
                "release_*_failure 持久化失败；lease 回收仍可兜底（F15-fix4）"
            );
        }
        emitter
            .emit(
                "publish_failed",
                "error",
                Some("publish_record"),
                Some(publish_record_id),
                &message,
                Some(json!({
                    "phase": phase,
                    "error_kind": error.error_kind(),
                    "retryable": retryable,
                    "budget_exhausted": budget_exhausted,
                })),
            )
            .await;
    }

    async fn fail_claimed(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &ReportError,
        phase: &'static str,
        now: OffsetDateTime,
        emitter: &RunEventEmitter<'_>,
    ) {
        self.release_classified_failure(publish_record_id, owner, error, phase, now, emitter)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_timestamp_is_utc_and_truncated_to_whole_microseconds() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_763_700_000_123_456_789)
            .unwrap()
            .to_offset(UtcOffset::from_hms(8, 0, 0).unwrap());

        let normalized = report_timestamp(at);

        assert_eq!(normalized.offset(), UtcOffset::UTC);
        assert_eq!(normalized.unix_timestamp_nanos(), 1_763_700_000_123_456_000);
        assert_eq!(report_timestamp(normalized), normalized, "idempotent");
    }
}
