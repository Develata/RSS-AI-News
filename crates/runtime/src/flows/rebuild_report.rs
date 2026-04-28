use std::sync::Arc;

use rss_ai_news_domain::dto::publish::RenderedReport;
use rss_ai_news_report::{RenderConfig, rebuild_markdown};
use time::OffsetDateTime;

use crate::context::RunContext;
use crate::error::RuntimeError;

pub struct RebuildReportFlow {
    ctx: Arc<RunContext>,
}

#[derive(Debug, Clone)]
pub struct RebuildReportOptions {
    pub publish_record_id: i64,
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at_override: Option<OffsetDateTime>,
}

impl RebuildReportFlow {
    pub fn new(ctx: Arc<RunContext>) -> Self {
        Self { ctx }
    }

    pub async fn rebuild(
        &self,
        opts: RebuildReportOptions,
    ) -> Result<RenderedReport, RuntimeError> {
        let record = self
            .ctx
            .publish_record_repo
            .find_by_id(opts.publish_record_id)
            .await?
            .ok_or_else(|| {
                RuntimeError::Config(format!(
                    "publish_record {} not found",
                    opts.publish_record_id
                ))
            })?;
        let generated_at = opts
            .generated_at_override
            .or(record.rendered_at)
            .unwrap_or_else(OffsetDateTime::now_utc);
        let render_config = RenderConfig {
            category_display_name: opts.category_display_name,
            report_title: opts.report_title,
            generated_at,
        };
        Ok(rebuild_markdown(
            self.ctx.publish_record_repo.as_ref(),
            self.ctx.publish_item_repo.as_ref(),
            opts.publish_record_id,
            &render_config,
        )
        .await?)
    }
}
