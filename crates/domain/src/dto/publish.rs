//! Publish-stage DTOs.

use time::OffsetDateTime;

use crate::Score0To100;

/// Request from runtime to report crate: initiate publishing.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub category_key: String,
    pub report_date: String,
    pub target_timezone: String,
    pub render_version_id: i64,
    pub selection_policy_version_id: i64,
    pub max_items: u32,
    pub min_importance_score: Score0To100,
    pub include_unscored: bool,
}

/// Candidate article for publishing.
#[derive(Debug, Clone)]
pub struct PublishCandidate {
    pub article_id: i64,
    pub article_ai_result_id: Option<i64>,
    pub title: String,
    pub canonical_link: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub importance_score: Option<Score0To100>,
    pub source_display_name: String,
    pub category_key: String,
    pub published_at: Option<OffsetDateTime>,
}

impl PublishCandidate {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        article_id: i64,
        article_ai_result_id: Option<i64>,
        title: String,
        canonical_link: String,
        summary: String,
        tags: Vec<String>,
        importance_score: Option<Score0To100>,
        source_display_name: String,
        category_key: String,
        published_at: Option<OffsetDateTime>,
    ) -> Result<Self, PublishCandidateError> {
        validate_ai_binding(article_ai_result_id, importance_score)?;

        Ok(Self {
            article_id,
            article_ai_result_id,
            title,
            canonical_link,
            summary,
            tags,
            importance_score,
            source_display_name,
            category_key,
            published_at,
        })
    }
}

/// Frozen publish item, ready for storage.
#[derive(Debug, Clone)]
pub struct FrozenPublishItem {
    pub position: u32,
    pub article_id: i64,
    pub article_ai_result_id: Option<i64>,
    pub frozen_title: String,
    pub frozen_summary: String,
    pub frozen_tags_json: String,
    pub frozen_score: Option<Score0To100>,
    pub frozen_canonical_link: String,
    pub frozen_source_display_name: String,
}

impl FrozenPublishItem {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        position: u32,
        article_id: i64,
        article_ai_result_id: Option<i64>,
        frozen_title: String,
        frozen_summary: String,
        frozen_tags_json: String,
        frozen_score: Option<Score0To100>,
        frozen_canonical_link: String,
        frozen_source_display_name: String,
    ) -> Result<Self, PublishCandidateError> {
        validate_ai_binding(article_ai_result_id, frozen_score)?;

        Ok(Self {
            position,
            article_id,
            article_ai_result_id,
            frozen_title,
            frozen_summary,
            frozen_tags_json,
            frozen_score,
            frozen_canonical_link,
            frozen_source_display_name,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PublishCandidateError {
    #[error("article_ai_result_id and score must be both Some or both None")]
    MismatchedAiBinding,
}

fn validate_ai_binding(
    article_ai_result_id: Option<i64>,
    score: Option<Score0To100>,
) -> Result<(), PublishCandidateError> {
    match (article_ai_result_id, score) {
        (Some(_), Some(_)) | (None, None) => Ok(()),
        (Some(_), None) | (None, Some(_)) => Err(PublishCandidateError::MismatchedAiBinding),
    }
}

/// Rendered report ready for output.
#[derive(Debug, Clone)]
pub struct RenderedReport {
    pub publish_record_id: i64,
    pub category_key: String,
    pub report_date: String,
    pub markdown_content: String,
    pub relative_path: String,
}

/// Publishing outcome.
#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub publish_record_id: i64,
    pub local_path: Option<String>,
    pub commit_sha: Option<String>,
    pub remote_target: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(value: u8) -> Score0To100 {
        Score0To100::try_new(value).expect("test score must be in range")
    }

    #[test]
    fn publish_candidate_accepts_ai_and_direct_paths() {
        assert!(
            PublishCandidate::try_new(
                1,
                Some(10),
                "title".to_owned(),
                "https://example.com".to_owned(),
                "summary".to_owned(),
                vec!["ai".to_owned()],
                Some(score(80)),
                "source".to_owned(),
                "ai".to_owned(),
                None,
            )
            .is_ok()
        );

        assert!(
            PublishCandidate::try_new(
                1,
                None,
                "title".to_owned(),
                "https://example.com".to_owned(),
                "summary".to_owned(),
                Vec::new(),
                None,
                "source".to_owned(),
                "ai".to_owned(),
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn publish_candidate_rejects_mismatched_ai_binding() {
        let missing_score = PublishCandidate::try_new(
            1,
            Some(10),
            "title".to_owned(),
            "https://example.com".to_owned(),
            "summary".to_owned(),
            Vec::new(),
            None,
            "source".to_owned(),
            "ai".to_owned(),
            None,
        );
        assert!(matches!(
            missing_score,
            Err(PublishCandidateError::MismatchedAiBinding)
        ));

        let missing_ai_result = PublishCandidate::try_new(
            1,
            None,
            "title".to_owned(),
            "https://example.com".to_owned(),
            "summary".to_owned(),
            Vec::new(),
            Some(score(80)),
            "source".to_owned(),
            "ai".to_owned(),
            None,
        );
        assert!(matches!(
            missing_ai_result,
            Err(PublishCandidateError::MismatchedAiBinding)
        ));
    }

    #[test]
    fn frozen_publish_item_accepts_ai_and_direct_paths() {
        assert!(
            FrozenPublishItem::try_new(
                1,
                2,
                Some(10),
                "title".to_owned(),
                "summary".to_owned(),
                "[\"ai\"]".to_owned(),
                Some(score(80)),
                "https://example.com".to_owned(),
                "source".to_owned(),
            )
            .is_ok()
        );

        assert!(
            FrozenPublishItem::try_new(
                1,
                2,
                None,
                "title".to_owned(),
                "summary".to_owned(),
                "[]".to_owned(),
                None,
                "https://example.com".to_owned(),
                "source".to_owned(),
            )
            .is_ok()
        );
    }

    #[test]
    fn frozen_publish_item_rejects_mismatched_ai_binding() {
        let missing_score = FrozenPublishItem::try_new(
            1,
            2,
            Some(10),
            "title".to_owned(),
            "summary".to_owned(),
            "[\"ai\"]".to_owned(),
            None,
            "https://example.com".to_owned(),
            "source".to_owned(),
        );
        assert!(matches!(
            missing_score,
            Err(PublishCandidateError::MismatchedAiBinding)
        ));

        let missing_ai_result = FrozenPublishItem::try_new(
            1,
            2,
            None,
            "title".to_owned(),
            "summary".to_owned(),
            "[\"ai\"]".to_owned(),
            Some(score(80)),
            "https://example.com".to_owned(),
            "source".to_owned(),
        );
        assert!(matches!(
            missing_ai_result,
            Err(PublishCandidateError::MismatchedAiBinding)
        ));
    }
}
