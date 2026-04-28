use rss_ai_news_domain::dto::publish::{FrozenPublishItem, RenderedReport};
use rss_ai_news_domain::model::PublishItem;
use rss_ai_news_storage::{PublishItemRepository, PublishRecordRepository};

use crate::error::ReportError;
use crate::render::{RenderConfig, render_markdown};

pub async fn rebuild_markdown(
    record_repo: &dyn PublishRecordRepository,
    item_repo: &dyn PublishItemRepository,
    publish_record_id: i64,
    render_config: &RenderConfig,
) -> Result<RenderedReport, ReportError> {
    let record = record_repo
        .find_by_id(publish_record_id)
        .await
        .map_err(|error| ReportError::RenderFailed(error.to_string()))?
        .ok_or_else(|| {
            ReportError::RenderFailed(format!("publish_record {publish_record_id} not found"))
        })?;
    let items = item_repo
        .list_by_publish_record(publish_record_id)
        .await
        .map_err(|error| ReportError::RenderFailed(error.to_string()))?;
    let frozen = items
        .into_iter()
        .map(item_to_frozen)
        .collect::<Result<Vec<_>, _>>()?;

    render_markdown(
        record.id,
        &record.category_key,
        &record.report_date,
        &frozen,
        render_config,
    )
}

fn item_to_frozen(item: PublishItem) -> Result<FrozenPublishItem, ReportError> {
    let position = u32::try_from(item.position)
        .map_err(|error| ReportError::RenderFailed(format!("position overflow: {error}")))?;
    FrozenPublishItem::try_new(
        position,
        item.article_id,
        item.article_ai_result_id,
        item.frozen_title,
        item.frozen_summary,
        item.frozen_tags_json,
        item.frozen_score,
        item.frozen_canonical_link,
        item.frozen_source_display_name,
    )
    .map_err(ReportError::from)
}
