//! publish flow：把 ready_for_publish 的文章 freeze→render→store_local→remote
//! 多阶段编排落地。
//!
//! 按阶段拆分为子模块，各自 `impl PublishFlow`：
//! [`freeze`] / [`render`] / [`store_local`] / [`remote`]；DTO 见 [`dto`]，
//! 经 `pub use dto::*` 保持对外 API 路径不变。本文件保留入口
//! （`new` / `build_idempotency_key` / `init`）、`PublishFlow` 本体，以及跨阶段
//! 共享的 `fail_claimed`（失败回收）与 `render_templates_from_ctx`（模板装配）
//! ——后两者为父模块私有项，子模块据 Rust 可见性规则可直接访问。

use std::sync::Arc;

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_report::{RenderTemplates, ReportError};
use rss_ai_news_storage::NewPublishRecord;
use serde_json::json;
use time::OffsetDateTime;

use crate::context::RunContext;
use crate::events::RunEventEmitter;
use crate::flows::maintenance::emit_maintenance_outcome;

mod dto;
mod freeze;
mod remote;
mod render;
mod store_local;

pub use dto::*;

pub struct PublishFlow {
    ctx: Arc<RunContext>,
}

fn render_templates_from_ctx(ctx: &RunContext, path_template: Option<&str>) -> RenderTemplates {
    let template = &ctx.app.publish.template;
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

impl PublishFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
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
            .terminalize_exhausted(self.ctx.app.retry.publish_max_attempts, now)
            .await;
        emit_maintenance_outcome(emitter, "publish_records", reclaimed, Some(swept)).await;
    }

    async fn fail_claimed(
        &self,
        publish_record_id: i64,
        owner: &str,
        error: &ReportError,
        now: OffsetDateTime,
        emitter: &RunEventEmitter<'_>,
    ) {
        if let Err(persist_err) = self
            .ctx
            .publish_record_repo
            .release_permanent_failure(
                publish_record_id,
                owner,
                &error.display_user(),
                error.error_kind(),
                now,
            )
            .await
        {
            tracing::warn!(
                publish_record_id,
                phase = "render",
                ?persist_err,
                "release_permanent_failure 持久化失败；保留上游错误向上抛（F15-fix4）"
            );
        }
        emitter
            .emit(
                "publish_failed",
                if matches!(error, ReportError::SnapshotEmpty) {
                    "warn"
                } else {
                    "error"
                },
                Some("publish_record"),
                Some(publish_record_id),
                &error.display_user(),
                Some(json!({ "phase": "freeze", "error_kind": error.error_kind() })),
            )
            .await;
    }
}
