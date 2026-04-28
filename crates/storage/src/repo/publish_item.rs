use async_trait::async_trait;
use rss_ai_news_domain::{Score0To100, model::PublishItem};
use sqlx::{FromRow, SqlitePool};
use time::OffsetDateTime;

use crate::StorageError;

#[derive(Debug, Clone, FromRow)]
pub struct PublishCandidateRow {
    pub article_id: i64,
    pub article_ai_result_id: Option<i64>,
    pub title: String,
    pub canonical_link: String,
    pub summary: String,
    pub tags_json: String,
    pub importance_score: Option<i32>,
    pub source_display_name: String,
    pub category_key: String,
    pub published_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone)]
pub struct FreezeSnapshotItem {
    pub position: i64,
    pub article_id: i64,
    pub article_ai_result_id: Option<i64>,
    pub frozen_title: String,
    pub frozen_summary: String,
    pub frozen_tags_json: String,
    pub frozen_score: Option<i32>,
    pub frozen_canonical_link: String,
    pub frozen_source_display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreezeSnapshotStatus {
    Frozen,
    PublishRecordConflict,
    ArticleStateConflict { article_id: i64 },
}

#[derive(Debug, Clone)]
pub struct FreezeSnapshotOutcome {
    pub status: FreezeSnapshotStatus,
    pub item_ids: Vec<i64>,
}

#[async_trait]
pub trait PublishItemRepository: Send + Sync {
    async fn select_ai_path_candidates(
        &self,
        category_key: &str,
        min_importance_score: i32,
        max_items: u32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError>;

    async fn select_ai_off_passthrough_candidates(
        &self,
        category_key: &str,
        max_items: u32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError>;

    async fn freeze_snapshot(
        &self,
        publish_record_id: i64,
        owner: &str,
        items: Vec<FreezeSnapshotItem>,
        ai_off_promote_article_ids: Vec<i64>,
        now: OffsetDateTime,
    ) -> Result<FreezeSnapshotOutcome, StorageError>;

    async fn list_by_publish_record(
        &self,
        publish_record_id: i64,
    ) -> Result<Vec<PublishItem>, StorageError>;
}

#[derive(Debug, Clone)]
pub struct SqlitePublishItemRepo {
    pool: SqlitePool,
}

impl SqlitePublishItemRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl PublishItemRepository for SqlitePublishItemRepo {
    async fn select_ai_path_candidates(
        &self,
        category_key: &str,
        min_importance_score: i32,
        max_items: u32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError> {
        sqlx::query_as::<_, PublishCandidateRow>(
            r#"
            WITH ranked AS (
                SELECT id, article_id, importance_score,
                       ROW_NUMBER() OVER (
                           PARTITION BY article_id
                           ORDER BY importance_score DESC, id ASC
                       ) AS rn
                FROM article_ai_results
                WHERE state = 'succeeded'
                  AND keep_decision = 1
                  AND importance_score IS NOT NULL
                  AND importance_score >= ?
            )
            SELECT a.id AS article_id,
                   aar.id AS article_ai_result_id,
                   a.title,
                   a.canonical_link,
                   COALESCE(aar.summary, '') AS summary,
                   COALESCE(aar.tags_json, '[]') AS tags_json,
                   aar.importance_score,
                   fs.display_name AS source_display_name,
                   fs.category_key,
                   fe.published_at
            FROM articles a
            JOIN ranked top ON top.article_id = a.id AND top.rn = 1
            JOIN article_ai_results aar ON aar.id = top.id
            JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
            JOIN feed_sources fs ON fs.id = fe.source_id
            WHERE a.state = 'ready_for_publish'
              AND fs.category_key = ?
            ORDER BY aar.importance_score DESC, a.created_at DESC, a.id ASC
            LIMIT ?
            "#,
        )
        .bind(min_importance_score)
        .bind(category_key)
        .bind(i64::from(max_items))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::from)
    }

    async fn select_ai_off_passthrough_candidates(
        &self,
        category_key: &str,
        max_items: u32,
    ) -> Result<Vec<PublishCandidateRow>, StorageError> {
        sqlx::query_as::<_, PublishCandidateRow>(
            r#"
            SELECT a.id AS article_id,
                   NULL AS article_ai_result_id,
                   a.title,
                   a.canonical_link,
                   COALESCE(fe.summary_raw, '') AS summary,
                   '[]' AS tags_json,
                   NULL AS importance_score,
                   fs.display_name AS source_display_name,
                   fs.category_key,
                   fe.published_at
            FROM articles a
            JOIN feed_entries fe ON fe.id = a.origin_feed_entry_id
            JOIN feed_sources fs ON fs.id = fe.source_id
            WHERE fs.category_key = ?
              AND a.state IN ('persisted', 'ready_for_publish')
              AND NOT EXISTS (
                  SELECT 1 FROM article_ai_results aar WHERE aar.article_id = a.id
              )
            ORDER BY a.created_at DESC, a.id ASC
            LIMIT ?
            "#,
        )
        .bind(category_key)
        .bind(i64::from(max_items))
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::from)
    }

    async fn freeze_snapshot(
        &self,
        publish_record_id: i64,
        owner: &str,
        items: Vec<FreezeSnapshotItem>,
        ai_off_promote_article_ids: Vec<i64>,
        now: OffsetDateTime,
    ) -> Result<FreezeSnapshotOutcome, StorageError> {
        let mut tx = self.pool.begin().await.map_err(StorageError::from)?;

        for article_id in ai_off_promote_article_ids {
            let result = sqlx::query(
                r#"
                UPDATE articles
                SET state = 'ready_for_publish', updated_at = ?
                WHERE id = ? AND state = 'persisted'
                "#,
            )
            .bind(now)
            .bind(article_id)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
            if result.rows_affected() == 0 {
                tx.rollback().await.map_err(StorageError::from)?;
                return Ok(FreezeSnapshotOutcome {
                    status: FreezeSnapshotStatus::ArticleStateConflict { article_id },
                    item_ids: Vec::new(),
                });
            }
        }

        let mut item_ids = Vec::with_capacity(items.len());
        for item in items {
            let id = sqlx::query_scalar::<_, i64>(
                r#"
                INSERT INTO publish_items (
                    publish_record_id, position, article_id, article_ai_result_id,
                    frozen_title, frozen_summary, frozen_tags_json, frozen_score,
                    frozen_canonical_link, frozen_source_display_name
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                RETURNING id
                "#,
            )
            .bind(publish_record_id)
            .bind(item.position)
            .bind(item.article_id)
            .bind(item.article_ai_result_id)
            .bind(item.frozen_title)
            .bind(item.frozen_summary)
            .bind(item.frozen_tags_json)
            .bind(item.frozen_score)
            .bind(item.frozen_canonical_link)
            .bind(item.frozen_source_display_name)
            .fetch_one(&mut *tx)
            .await
            .map_err(StorageError::from)?;
            item_ids.push(id);
        }

        let result = sqlx::query(
            r#"
            UPDATE publish_records
            SET state = 'snapshot_frozen',
                snapshot_frozen_at = ?,
                lease_owner = NULL,
                lease_expires_at = NULL,
                last_error = NULL,
                last_error_kind = NULL,
                updated_at = ?
            WHERE id = ? AND lease_owner = ? AND state = 'pending'
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(publish_record_id)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;

        if result.rows_affected() == 0 {
            tx.rollback().await.map_err(StorageError::from)?;
            return Ok(FreezeSnapshotOutcome {
                status: FreezeSnapshotStatus::PublishRecordConflict,
                item_ids: Vec::new(),
            });
        }

        tx.commit().await.map_err(StorageError::from)?;
        Ok(FreezeSnapshotOutcome {
            status: FreezeSnapshotStatus::Frozen,
            item_ids,
        })
    }

    async fn list_by_publish_record(
        &self,
        publish_record_id: i64,
    ) -> Result<Vec<PublishItem>, StorageError> {
        let rows = sqlx::query_as::<_, PublishItemRow>(
            r#"
            SELECT id, publish_record_id, position, article_id, article_ai_result_id,
                   frozen_title, frozen_summary, frozen_tags_json, frozen_score,
                   frozen_canonical_link, frozen_source_display_name, created_at
            FROM publish_items
            WHERE publish_record_id = ?
            ORDER BY position ASC, id ASC
            "#,
        )
        .bind(publish_record_id)
        .fetch_all(&self.pool)
        .await
        .map_err(StorageError::from)?;

        rows.into_iter().map(PublishItemRow::try_into).collect()
    }
}

#[derive(Debug, FromRow)]
struct PublishItemRow {
    id: i64,
    publish_record_id: i64,
    position: i64,
    article_id: i64,
    article_ai_result_id: Option<i64>,
    frozen_title: String,
    frozen_summary: String,
    frozen_tags_json: String,
    frozen_score: Option<i32>,
    frozen_canonical_link: String,
    frozen_source_display_name: String,
    created_at: OffsetDateTime,
}

impl TryFrom<PublishItemRow> for PublishItem {
    type Error = StorageError;

    fn try_from(row: PublishItemRow) -> Result<Self, Self::Error> {
        let frozen_score = row
            .frozen_score
            .map(score_from_i32)
            .transpose()
            .map_err(StorageError::Corruption)?;
        Ok(Self {
            id: row.id,
            publish_record_id: row.publish_record_id,
            position: row.position,
            article_id: row.article_id,
            article_ai_result_id: row.article_ai_result_id,
            frozen_title: row.frozen_title,
            frozen_summary: row.frozen_summary,
            frozen_tags_json: row.frozen_tags_json,
            frozen_score,
            frozen_canonical_link: row.frozen_canonical_link,
            frozen_source_display_name: row.frozen_source_display_name,
            created_at: row.created_at,
        })
    }
}

fn score_from_i32(value: i32) -> Result<Score0To100, String> {
    let value = u8::try_from(value).map_err(|error| error.to_string())?;
    Score0To100::try_new(value).map_err(|error| error.to_string())
}
