use rss_ai_news_domain::dto::publish::{FrozenPublishItem, PublishCandidate};
use rss_ai_news_storage::FreezeSnapshotItem;

use crate::error::ReportError;
use crate::excerpt::generate_excerpt;

pub struct SnapshotConfig {
    pub excerpt_max_chars: usize,
}

/// 将 `PublishCandidate` 列表转为带 position 的 `FrozenPublishItem`。
/// position 从 1 开始递增，与 candidates 的传入顺序一致（已由 selection 排好）。
pub fn freeze(
    candidates: Vec<PublishCandidate>,
    config: &SnapshotConfig,
) -> Result<Vec<FrozenPublishItem>, ReportError> {
    candidates
        .into_iter()
        .enumerate()
        .map(|(idx, candidate)| {
            let position = u32::try_from(idx + 1)
                .map_err(|error| ReportError::RenderFailed(error.to_string()))?;
            let frozen_summary = generate_excerpt(&candidate.summary, config.excerpt_max_chars);
            let frozen_tags_json = serde_json::to_string(&candidate.tags)
                .map_err(|error| ReportError::InvalidTagsJson(error.to_string()))?;
            FrozenPublishItem::try_new(
                position,
                candidate.article_id,
                candidate.article_ai_result_id,
                candidate.title,
                frozen_summary,
                frozen_tags_json,
                candidate.importance_score,
                candidate.canonical_link,
                candidate.source_display_name,
            )
            .map_err(ReportError::from)
        })
        .collect()
}

/// 配套：把 frozen items 转成 storage 层接口需要的 FreezeSnapshotItem。
pub fn to_storage_items(items: &[FrozenPublishItem]) -> Vec<FreezeSnapshotItem> {
    items
        .iter()
        .map(|item| FreezeSnapshotItem {
            position: i64::from(item.position),
            article_id: item.article_id,
            article_ai_result_id: item.article_ai_result_id,
            frozen_title: item.frozen_title.clone(),
            frozen_summary: item.frozen_summary.clone(),
            frozen_tags_json: item.frozen_tags_json.clone(),
            frozen_score: item.frozen_score.map(|score| i32::from(score.get())),
            frozen_canonical_link: item.frozen_canonical_link.clone(),
            frozen_source_display_name: item.frozen_source_display_name.clone(),
        })
        .collect()
}
