use std::sync::Arc;

use rss_ai_news_storage::{NewAiResult, ResetFailedFilter};
use serde_json::json;
use time::OffsetDateTime;

use crate::context::RunContext;
use crate::error::RuntimeError;
use crate::events::RunEventEmitter;

#[derive(Debug, Clone)]
pub struct BackfillExtractOptions {
    pub date_from: Option<OffsetDateTime>,
    pub date_to: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default)]
pub struct BackfillExtractSummary {
    pub examined: u32,
    pub reset: u32,
}

#[derive(Debug, Clone)]
pub struct BackfillAiOptions {
    pub date_from: Option<OffsetDateTime>,
    pub date_to: Option<OffsetDateTime>,
    pub batch_size: u32,
    pub new_prompt_version_tag: String,
    pub new_prompt_version_sha256: String,
    pub new_prompt_version_description: String,
    pub model_id: String,
    pub output_schema_version: i64,
}

#[derive(Debug, Clone, Default)]
pub struct BackfillAiSummary {
    pub new_prompt_version_id: i64,
    pub articles_scanned: u32,
    pub ai_tasks_inserted: u32,
    pub ai_tasks_conflict: u32,
}

pub struct BackfillFlow {
    ctx: Arc<RunContext>,
}

impl BackfillFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
        Self { ctx }
    }

    pub async fn extract(
        &self,
        opts: BackfillExtractOptions,
    ) -> Result<BackfillExtractSummary, RuntimeError> {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "backfill",
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "backfill extract started",
                Some(json!({ "target": "extract" })),
            )
            .await;

        let outcome = self
            .ctx
            .feed_entry_repo
            .reset_failed_in_window(&ResetFailedFilter {
                date_from: opts.date_from,
                date_to: opts.date_to,
            })
            .await?;

        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "backfill extract completed",
                Some(json!({ "examined": outcome.examined, "reset": outcome.reset })),
            )
            .await;
        Ok(BackfillExtractSummary {
            examined: outcome.examined,
            reset: outcome.reset,
        })
    }

    pub async fn ai(&self, opts: BackfillAiOptions) -> Result<BackfillAiSummary, RuntimeError> {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "backfill",
            repo: self.ctx.event_repo.as_ref(),
        };
        emitter
            .emit(
                "run_started",
                "info",
                None,
                None,
                "backfill ai started",
                Some(json!({ "target": "ai" })),
            )
            .await;

        let prompt_version_id = self
            .ctx
            .rule_version_repo
            .get_or_create(
                "prompt",
                &opts.new_prompt_version_tag,
                &opts.new_prompt_version_description,
                &opts.new_prompt_version_sha256,
            )
            .await?;

        let mut summary = BackfillAiSummary {
            new_prompt_version_id: prompt_version_id,
            ..BackfillAiSummary::default()
        };
        let mut after_id = 0;
        let batch_size = opts.batch_size.max(1);

        loop {
            let rows = self
                .ctx
                .article_repo
                .list_in_window_for_backfill(opts.date_from, opts.date_to, batch_size, after_id)
                .await?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                after_id = row.article_id;
                summary.articles_scanned += 1;
                let item = NewAiResult {
                    article_id: row.article_id,
                    prompt_version: prompt_version_id,
                    output_schema_version: opts.output_schema_version,
                    model_id: opts.model_id.clone(),
                };
                if self
                    .ctx
                    .ai_result_repo
                    .insert_pending(&item)
                    .await?
                    .is_some()
                {
                    summary.ai_tasks_inserted += 1;
                } else {
                    summary.ai_tasks_conflict += 1;
                }
            }
        }

        emitter
            .emit(
                "run_completed",
                "info",
                None,
                None,
                "backfill ai completed",
                Some(json!({
                    "articles_scanned": summary.articles_scanned,
                    "ai_tasks_inserted": summary.ai_tasks_inserted,
                    "ai_tasks_conflict": summary.ai_tasks_conflict,
                })),
            )
            .await;
        Ok(summary)
    }
}
