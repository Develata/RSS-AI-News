use sqlx::{Row, SqlitePool};

use rss_ai_news_storage::StorageError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvariantId {
    I1,
    I2,
    I3,
    I4,
    I4APrime,
    I4BPrime,
    I5,
    I6,
    I8,
}

impl InvariantId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::I1 => "I1",
            Self::I2 => "I2",
            Self::I3 => "I3",
            Self::I4 => "I4",
            Self::I4APrime => "I4'.a",
            Self::I4BPrime => "I4'.b",
            Self::I5 => "I5",
            Self::I6 => "I6",
            Self::I8 => "I8",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::I1 => "feed_entries.persisted => articles",
            Self::I2 => "articles.ai_pending => ai_result row exists",
            Self::I3 => "articles.ai_done => succeeded ai_result row exists",
            Self::I4 => "articles.ready_for_publish => AI keep or passthrough",
            Self::I4APrime => "publish_items.ai_result binding => keepable result",
            Self::I4BPrime => "publish_items.passthrough binding => no AI rows",
            Self::I5 => "articles.published => successful publish record",
            Self::I6 => "publish_records.published_* => articles.published",
            Self::I8 => "no expired running AI leases",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViolationRow {
    pub primary_id: i64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantResult {
    pub id: InvariantId,
    pub violations: Vec<ViolationRow>,
    pub total_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepScanReport {
    pub results: Vec<InvariantResult>,
}

pub async fn run(pool: &SqlitePool) -> Result<DeepScanReport, StorageError> {
    let specs = [
        Spec {
            id: InvariantId::I1,
            select: r#"
                SELECT fe.id AS primary_id,
                       'feed_entry_id=' || fe.id || ' article_id=' || COALESCE(CAST(fe.article_id AS TEXT), 'NULL') AS message
                FROM feed_entries fe
                WHERE fe.state = 'persisted'
                  AND (fe.article_id IS NULL
                       OR NOT EXISTS (SELECT 1 FROM articles a WHERE a.id = fe.article_id))
            "#,
        },
        Spec {
            id: InvariantId::I2,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || a.id || ' state=' || a.state AS message
                FROM articles a
                WHERE a.state = 'ai_pending'
                  AND NOT EXISTS (SELECT 1 FROM article_ai_results aar WHERE aar.article_id = a.id)
            "#,
        },
        Spec {
            id: InvariantId::I3,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || a.id || ' state=' || a.state AS message
                FROM articles a
                WHERE a.state = 'ai_done'
                  AND NOT EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.article_id = a.id AND aar.state = 'succeeded'
                  )
            "#,
        },
        Spec {
            id: InvariantId::I4,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || a.id || ' state=' || a.state AS message
                FROM articles a
                WHERE a.state = 'ready_for_publish'
                  AND NOT EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.article_id = a.id
                      AND aar.state = 'succeeded'
                      AND aar.keep_decision = 1
                  )
                  AND EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.article_id = a.id
                  )
            "#,
        },
        Spec {
            id: InvariantId::I4APrime,
            select: r#"
                SELECT pi.id AS primary_id,
                       'publish_item_id=' || pi.id || ' article_id=' || pi.article_id ||
                       ' article_ai_result_id=' || pi.article_ai_result_id AS message
                FROM publish_items pi
                WHERE pi.article_ai_result_id IS NOT NULL
                  AND NOT EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.id = pi.article_ai_result_id
                      AND aar.state = 'succeeded'
                      AND aar.keep_decision = 1
                  )
            "#,
        },
        Spec {
            id: InvariantId::I4BPrime,
            select: r#"
                SELECT pi.id AS primary_id,
                       'publish_item_id=' || pi.id || ' article_id=' || pi.article_id AS message
                FROM publish_items pi
                WHERE pi.article_ai_result_id IS NULL
                  AND EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.article_id = pi.article_id
                  )
            "#,
        },
        Spec {
            id: InvariantId::I5,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || a.id || ' state=' || a.state AS message
                FROM articles a
                WHERE a.state = 'published'
                  AND NOT EXISTS (
                    SELECT 1 FROM publish_items pi
                    JOIN publish_records pr ON pr.id = pi.publish_record_id
                    WHERE pi.article_id = a.id
                      AND pr.state IN ('published_remote', 'published_local')
                  )
            "#,
        },
        Spec {
            id: InvariantId::I6,
            select: r#"
                SELECT pr.id AS primary_id,
                       'publish_record_id=' || pr.id || ' article_id=' || a.id ||
                       ' article.state=' || a.state AS message
                FROM publish_records pr
                JOIN publish_items pi ON pi.publish_record_id = pr.id
                JOIN articles a ON a.id = pi.article_id
                WHERE pr.state IN ('published_remote', 'published_local')
                  AND a.state <> 'published'
            "#,
        },
        Spec {
            id: InvariantId::I8,
            select: r#"
                SELECT aar.id AS primary_id,
                       'article_ai_result_id=' || aar.id || ' article_id=' || aar.article_id ||
                       ' lease_expires_at=' || aar.lease_expires_at AS message
                FROM article_ai_results aar
                WHERE aar.state = 'running'
                  AND aar.lease_expires_at IS NOT NULL
                  AND aar.lease_expires_at < datetime('now')
            "#,
        },
    ];

    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        results.push(run_spec(pool, spec).await?);
    }
    Ok(DeepScanReport { results })
}

struct Spec {
    id: InvariantId,
    select: &'static str,
}

async fn run_spec(pool: &SqlitePool, spec: Spec) -> Result<InvariantResult, StorageError> {
    let count_sql = format!("SELECT COUNT(*) FROM ({}) AS violations", spec.select);
    let total_count = sqlx::query_scalar::<_, i64>(&count_sql)
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;
    let select_sql = format!("{} LIMIT 50", spec.select);
    let rows = sqlx::query(&select_sql)
        .fetch_all(pool)
        .await
        .map_err(StorageError::from)?;
    let violations = rows
        .into_iter()
        .map(|row| ViolationRow {
            primary_id: row.get::<i64, _>("primary_id"),
            message: row.get::<String, _>("message"),
        })
        .collect();
    Ok(InvariantResult {
        id: spec.id,
        violations,
        total_count: u64::try_from(total_count).unwrap_or(0),
    })
}
