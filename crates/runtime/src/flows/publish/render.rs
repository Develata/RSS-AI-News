//! [`PublishFlow`] 的 render 阶段：claim → 渲染报告 → 推进 publish_record。

use rss_ai_news_domain::error::ClassifiedError;
use rss_ai_news_report::RenderConfig;
use rss_ai_news_storage::{
    ClaimRequest, PublishAdvanceExtras, PublishState, PublishTimestampField, build_owner_id,
    lease_expires_at,
};
use time::{Duration, OffsetDateTime};

use super::PublishFlow;
use super::dto::{PublishRenderOptions, PublishRenderOutcome, PublishRenderStatus};
use super::render_templates_from_ctx;
use crate::events::RunEventEmitter;

impl PublishFlow {
    pub async fn render(&self, opts: PublishRenderOptions) -> PublishRenderOutcome {
        self.render_record_inner(None, opts).await
    }

    pub async fn render_record(
        &self,
        publish_record_id: i64,
        opts: PublishRenderOptions,
    ) -> PublishRenderOutcome {
        self.render_record_inner(Some(publish_record_id), opts)
            .await
    }

    async fn render_record_inner(
        &self,
        publish_record_id: Option<i64>,
        opts: PublishRenderOptions,
    ) -> PublishRenderOutcome {
        let emitter = RunEventEmitter {
            run_id: &self.ctx.run_id,
            stage: "publish",
            repo: self.ctx.event_repo.as_ref(),
        };
        let now = OffsetDateTime::now_utc();
        let owner = build_owner_id();
        let claim = ClaimRequest {
            owner: owner.clone(),
            now,
            lease_expires_at: lease_expires_at(
                now,
                Duration::seconds(self.ctx.app.lease.publish_duration_seconds as i64),
            ),
            batch_size: 1,
            max_attempts: self.ctx.app.retry.publish_max_attempts,
        };
        let claimed_result = if let Some(publish_record_id) = publish_record_id {
            self.ctx
                .publish_record_repo
                .claim_publish_by_ids(&claim, PublishState::SnapshotFrozen, &[publish_record_id])
                .await
        } else {
            self.ctx
                .publish_record_repo
                .claim_frozen_for_render(&claim)
                .await
        };
        let claimed = match claimed_result {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!("claim frozen for render failed: {error}");
                return PublishRenderOutcome {
                    publish_record_id: 0,
                    status: PublishRenderStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                };
            }
        };
        let claimed = match claimed.into_iter().next() {
            Some(claimed) => claimed,
            None => {
                return PublishRenderOutcome {
                    publish_record_id: 0,
                    status: PublishRenderStatus::NothingToClaim,
                };
            }
        };

        let render_config = RenderConfig {
            category_display_name: opts.category_display_name,
            report_title: opts.report_title,
            generated_at: opts.generated_at,
            templates: render_templates_from_ctx(&self.ctx, opts.path_template.as_deref()),
        };
        if let Err(error) = rss_ai_news_report::rebuild_markdown(
            self.ctx.publish_record_repo.as_ref(),
            self.ctx.publish_item_repo.as_ref(),
            claimed.id,
            &render_config,
        )
        .await
        {
            self.fail_claimed(claimed.id, &owner, &error, now, &emitter)
                .await;
            return PublishRenderOutcome {
                publish_record_id: claimed.id,
                status: PublishRenderStatus::Failed {
                    error_kind: error.error_kind().to_string(),
                },
            };
        }

        match self
            .ctx
            .publish_record_repo
            .release_advance(
                claimed.id,
                &owner,
                PublishState::SnapshotFrozen,
                PublishState::Rendered,
                PublishTimestampField::RenderedAt,
                now,
                PublishAdvanceExtras::default(),
            )
            .await
        {
            Ok(true) => PublishRenderOutcome {
                publish_record_id: claimed.id,
                status: PublishRenderStatus::Rendered,
            },
            Ok(false) => PublishRenderOutcome {
                publish_record_id: claimed.id,
                status: PublishRenderStatus::Conflicted,
            },
            Err(error) => {
                tracing::error!("release_advance to rendered failed: {error}");
                PublishRenderOutcome {
                    publish_record_id: claimed.id,
                    status: PublishRenderStatus::Failed {
                        error_kind: error.error_kind().to_string(),
                    },
                }
            }
        }
    }
}
