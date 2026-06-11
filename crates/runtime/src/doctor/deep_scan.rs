use rss_ai_news_config::RetryConfig;
use rss_ai_news_storage::{StorageError, StoragePool};
use sqlx::Row;
use time::OffsetDateTime;

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
    I9Feed,
    I9Ai,
    I9Publish,
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
            Self::I9Feed => "I9.feed",
            Self::I9Ai => "I9.ai",
            Self::I9Publish => "I9.publish",
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
            Self::I9Feed => "no claimable feed_entries with exhausted retry budget",
            Self::I9Ai => "no claimable article_ai_results with exhausted retry budget",
            Self::I9Publish => "no claimable publish_records with exhausted retry budget",
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

/// W11-P4-C2：deep_scan 双轨化。
///
/// 关键 SQL 改造：所有 `'literal' || some_bigint_col` 在 PG 上需 `CAST(... AS TEXT)`
/// 显式转换（PG 严格类型，`text || bigint` 报 "operator does not exist"），
/// SQLite 动态类型本来无需 cast 但能接受 `CAST(... AS TEXT)`。两边统一用
/// `CAST(... AS TEXT)` 跨方言等价。
///
/// 占位符 `$1` 跨方言（SQLite 也支持）。
///
/// W15 I9：`retry` 提供三表的预算上限（15 §7）。注意 extract 的运行期有效
/// 预算是 `max(opts.max_attempts, config)`——doctor 只能用 config 值，CLI 传
/// 入更大 `--max-attempts` 期间该检查可能短暂误报（sweep 接线后常态应为绿）。
pub async fn run(pool: &StoragePool, retry: &RetryConfig) -> Result<DeepScanReport, StorageError> {
    let specs = [
        Spec {
            id: InvariantId::I1,
            select: r#"
                SELECT fe.id AS primary_id,
                       'feed_entry_id=' || CAST(fe.id AS TEXT) || ' article_id=' ||
                       COALESCE(CAST(fe.article_id AS TEXT), 'NULL') AS message
                FROM feed_entries fe
                WHERE fe.state = 'persisted'
                  AND (fe.article_id IS NULL
                       OR NOT EXISTS (SELECT 1 FROM articles a WHERE a.id = fe.article_id))
            "#,
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I2,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || CAST(a.id AS TEXT) || ' state=' || a.state AS message
                FROM articles a
                WHERE a.state = 'ai_pending'
                  AND NOT EXISTS (SELECT 1 FROM article_ai_results aar WHERE aar.article_id = a.id)
            "#,
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I3,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || CAST(a.id AS TEXT) || ' state=' || a.state AS message
                FROM articles a
                WHERE a.state = 'ai_done'
                  AND NOT EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.article_id = a.id AND aar.state = 'succeeded'
                  )
            "#,
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I4,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || CAST(a.id AS TEXT) || ' state=' || a.state AS message
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
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I4APrime,
            select: r#"
                SELECT pi.id AS primary_id,
                       'publish_item_id=' || CAST(pi.id AS TEXT) ||
                       ' article_id=' || CAST(pi.article_id AS TEXT) ||
                       ' article_ai_result_id=' || CAST(pi.article_ai_result_id AS TEXT) AS message
                FROM publish_items pi
                WHERE pi.article_ai_result_id IS NOT NULL
                  AND NOT EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.id = pi.article_ai_result_id
                      AND aar.state = 'succeeded'
                      AND aar.keep_decision = 1
                  )
            "#,
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I4BPrime,
            select: r#"
                SELECT pi.id AS primary_id,
                       'publish_item_id=' || CAST(pi.id AS TEXT) ||
                       ' article_id=' || CAST(pi.article_id AS TEXT) AS message
                FROM publish_items pi
                WHERE pi.article_ai_result_id IS NULL
                  AND EXISTS (
                    SELECT 1 FROM article_ai_results aar
                    WHERE aar.article_id = pi.article_id
                  )
            "#,
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I5,
            select: r#"
                SELECT a.id AS primary_id,
                       'article_id=' || CAST(a.id AS TEXT) || ' state=' || a.state AS message
                FROM articles a
                WHERE a.state = 'published'
                  AND NOT EXISTS (
                    SELECT 1 FROM publish_items pi
                    JOIN publish_records pr ON pr.id = pi.publish_record_id
                    WHERE pi.article_id = a.id
                      AND pr.state IN ('published_remote', 'published_local')
                  )
            "#,
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I6,
            select: r#"
                SELECT pr.id AS primary_id,
                       'publish_record_id=' || CAST(pr.id AS TEXT) ||
                       ' article_id=' || CAST(a.id AS TEXT) ||
                       ' article.state=' || a.state AS message
                FROM publish_records pr
                JOIN publish_items pi ON pi.publish_record_id = pr.id
                JOIN articles a ON a.id = pi.article_id
                WHERE pr.state IN ('published_remote', 'published_local')
                  AND a.state <> 'published'
            "#,
            now_binds: 0,
            max_attempts: None,
        },
        Spec {
            id: InvariantId::I8,
            select: r#"
                SELECT aar.id AS primary_id,
                       'article_ai_result_id=' || CAST(aar.id AS TEXT) ||
                       ' article_id=' || CAST(aar.article_id AS TEXT) ||
                       ' lease_expires_at=' || COALESCE(CAST(aar.lease_expires_at AS TEXT), 'NULL') AS message
                FROM article_ai_results aar
                WHERE aar.state = 'running'
                  AND aar.lease_expires_at IS NOT NULL
                  AND aar.lease_expires_at < $1
            "#,
            now_binds: 1,
            max_attempts: None,
        },
        // W15 I9：与 storage 的 TERMINALIZE_EXHAUSTED_*_SQL 谓词逐字段一致
        // （可领取态 + attempt_count >= 预算 + lease 空/过期）。sweep 在各
        // flow 启动期执行，doctor 让运行间隙的滞留行可见（15 §7）。
        Spec {
            id: InvariantId::I9Feed,
            select: r#"
                SELECT fe.id AS primary_id,
                       'feed_entry_id=' || CAST(fe.id AS TEXT) ||
                       ' attempt_count=' || CAST(fe.attempt_count AS TEXT) AS message
                FROM feed_entries fe
                WHERE fe.state = 'pending_fetch'
                  AND fe.attempt_count >= $1
                  AND (fe.lease_expires_at IS NULL OR fe.lease_expires_at < $2)
            "#,
            now_binds: 1,
            max_attempts: Some(retry.feed_entry_max_attempts),
        },
        Spec {
            id: InvariantId::I9Ai,
            select: r#"
                SELECT aar.id AS primary_id,
                       'article_ai_result_id=' || CAST(aar.id AS TEXT) ||
                       ' attempt_count=' || CAST(aar.attempt_count AS TEXT) AS message
                FROM article_ai_results aar
                WHERE aar.state = 'pending'
                  AND aar.attempt_count >= $1
                  AND (aar.lease_expires_at IS NULL OR aar.lease_expires_at < $2)
            "#,
            now_binds: 1,
            max_attempts: Some(retry.ai_max_attempts),
        },
        Spec {
            id: InvariantId::I9Publish,
            select: r#"
                SELECT pr.id AS primary_id,
                       'publish_record_id=' || CAST(pr.id AS TEXT) ||
                       ' state=' || pr.state ||
                       ' attempt_count=' || CAST(pr.attempt_count AS TEXT) AS message
                FROM publish_records pr
                WHERE pr.state IN ('pending', 'snapshot_frozen', 'rendered', 'stored_local')
                  AND pr.attempt_count >= $1
                  AND (pr.lease_expires_at IS NULL OR pr.lease_expires_at < $2)
            "#,
            now_binds: 1,
            max_attempts: Some(retry.publish_max_attempts),
        },
    ];

    let now = OffsetDateTime::now_utc();
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        results.push(run_spec(pool, spec, now).await?);
    }
    Ok(DeepScanReport { results })
}

struct Spec {
    id: InvariantId,
    select: &'static str,
    /// 该 spec SQL 中绑定 `now` 的 `$N` 占位符数量。显式声明而非数 SQL
    /// 文本中的 `?` —— W11-P1-F 把占位符改成 `$N` 后字面量计数法不再可用。
    now_binds: u8,
    /// W15 I9：若 `Some`，作为 **首个** 占位符（`$1`）绑定预算上限；
    /// `now` 的占位符编号顺延（I9 系列即 `$2`）。
    max_attempts: Option<u32>,
}

async fn run_spec(
    pool: &StoragePool,
    spec: Spec,
    now: OffsetDateTime,
) -> Result<InvariantResult, StorageError> {
    let count_sql = format!("SELECT COUNT(*) FROM ({}) AS violations", spec.select);
    let select_sql = format!("{} LIMIT 50", spec.select);

    let (total_count, violations) = match pool {
        StoragePool::Sqlite(p) => {
            let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
            if let Some(max) = spec.max_attempts {
                count_q = count_q.bind(i64::from(max));
            }
            for _ in 0..spec.now_binds {
                count_q = count_q.bind(now);
            }
            let total_count = count_q.fetch_one(p).await.map_err(StorageError::from)?;
            let mut select_q = sqlx::query(&select_sql);
            if let Some(max) = spec.max_attempts {
                select_q = select_q.bind(i64::from(max));
            }
            for _ in 0..spec.now_binds {
                select_q = select_q.bind(now);
            }
            let rows = select_q.fetch_all(p).await.map_err(StorageError::from)?;
            let violations: Vec<ViolationRow> = rows
                .into_iter()
                .map(|row| ViolationRow {
                    primary_id: row.get::<i64, _>("primary_id"),
                    message: row.get::<String, _>("message"),
                })
                .collect();
            (total_count, violations)
        }
        StoragePool::Postgres(p) => {
            let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
            if let Some(max) = spec.max_attempts {
                count_q = count_q.bind(i64::from(max));
            }
            for _ in 0..spec.now_binds {
                count_q = count_q.bind(now);
            }
            let total_count = count_q.fetch_one(p).await.map_err(StorageError::from)?;
            let mut select_q = sqlx::query(&select_sql);
            if let Some(max) = spec.max_attempts {
                select_q = select_q.bind(i64::from(max));
            }
            for _ in 0..spec.now_binds {
                select_q = select_q.bind(now);
            }
            let rows = select_q.fetch_all(p).await.map_err(StorageError::from)?;
            let violations: Vec<ViolationRow> = rows
                .into_iter()
                .map(|row| ViolationRow {
                    primary_id: row.get::<i64, _>("primary_id"),
                    message: row.get::<String, _>("message"),
                })
                .collect();
            (total_count, violations)
        }
    };

    Ok(InvariantResult {
        id: spec.id,
        violations,
        total_count: u64::try_from(total_count).unwrap_or(0),
    })
}
