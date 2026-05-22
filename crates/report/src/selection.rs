use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::publish::{PublishCandidate, PublishRequest};
use rss_ai_news_storage::{PublishCandidateRow, PublishItemRepository};

use crate::error::ReportError;

pub struct SelectionConfig {
    pub ai_enabled: bool,
}

pub async fn load_candidates(
    repo: &dyn PublishItemRepository,
    request: &PublishRequest,
    config: &SelectionConfig,
) -> Result<Vec<PublishCandidate>, ReportError> {
    let rows = if config.ai_enabled {
        repo.select_ai_path_candidates(
            &request.category_key,
            i32::from(request.min_importance_score.get()),
            request.published_since,
            request.published_until,
            request.max_items,
        )
        .await
        .map_err(|error| ReportError::RenderFailed(error.to_string()))?
    } else if request.include_unscored {
        repo.select_ai_off_passthrough_candidates(
            &request.category_key,
            request.published_since,
            request.published_until,
            request.max_items,
        )
        .await
        .map_err(|error| ReportError::RenderFailed(error.to_string()))?
    } else {
        return Ok(Vec::new());
    };

    rows.into_iter().map(row_to_candidate).collect()
}

fn row_to_candidate(row: PublishCandidateRow) -> Result<PublishCandidate, ReportError> {
    let tags: Vec<String> = serde_json::from_str(&row.tags_json)
        .map_err(|error| ReportError::InvalidTagsJson(error.to_string()))?;
    let score = match row.importance_score {
        Some(value) => Some(
            Score0To100::try_new(
                u8::try_from(value)
                    .map_err(|error| ReportError::InvalidScore(error.to_string()))?,
            )
            .map_err(|error| ReportError::InvalidScore(error.to_string()))?,
        ),
        None => None,
    };

    PublishCandidate::try_new(
        row.article_id,
        row.article_ai_result_id,
        row.title,
        row.canonical_link,
        row.summary,
        tags,
        score,
        row.source_display_name,
        row.category_key,
        row.published_at,
    )
    .map_err(ReportError::from)
}
