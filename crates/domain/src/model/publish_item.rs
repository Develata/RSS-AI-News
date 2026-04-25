use time::OffsetDateTime;

use crate::Score0To100;

#[derive(Debug, Clone)]
pub struct PublishItem {
    pub id: i64,
    pub publish_record_id: i64,
    pub position: i64,
    pub article_id: i64,
    pub article_ai_result_id: Option<i64>,
    pub frozen_title: String,
    pub frozen_summary: String,
    pub frozen_tags_json: String,
    pub frozen_score: Option<Score0To100>,
    pub frozen_canonical_link: String,
    pub frozen_source_display_name: String,
    pub created_at: OffsetDateTime,
}
