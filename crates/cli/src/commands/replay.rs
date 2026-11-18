use std::{
    io::{self, Write},
    time::Duration,
};

use rss_ai_news_ai::{ParsedResponse, parse_response};
use rss_ai_news_domain::{
    dto::extract::ArticleFetchTask,
    model::RawArtifact,
    state::{ArtifactKind, FeedKind},
};
use rss_ai_news_extractor::{ContentStrategy, ReadabilityStrategy};
use rss_ai_news_feed::parse_feed;
use rss_ai_news_runtime::RuntimeError;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    args::{Cli, ReplayArgs, ReplayKind},
    context_factory::build_replay_deps,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct ReplayCommandSummary {
    pub kind: String,
    pub artifact_id: i64,
    pub artifact_key: String,
    pub byte_size: u32,
    pub parsed: Value,
    pub diff: Option<Value>,
}

impl CommandSummary for ReplayCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Replay completed:")?;
        writeln!(writer, "  Kind:       {}", self.kind)?;
        writeln!(
            writer,
            "  Artifact:   {} ({})",
            self.artifact_id, self.artifact_key
        )?;
        writeln!(writer, "  Byte size:  {}", self.byte_size)?;
        writeln!(writer, "  Parsed:     {}", self.parsed)?;
        if let Some(diff) = &self.diff {
            writeln!(writer, "  Diff:       {diff}")?;
        }
        Ok(())
    }
}

pub async fn run(cli: &Cli, args: &ReplayArgs) -> Result<ReplayCommandSummary, CliError> {
    let deps = build_replay_deps(cli).await?;
    let kind = kind_storage(args.kind);
    let artifact = if let Some(id) = args.id {
        deps.artifact_repo.find_by_id(id).await?
    } else {
        let key = args.key.as_deref().ok_or_else(|| {
            CliError::Runtime(RuntimeError::Config(
                "replay requires either --id or --key".to_string(),
            ))
        })?;
        deps.artifact_repo.find_by_key(kind, key).await?
    };
    let artifact =
        of_kind(artifact, args.kind).ok_or_else(|| CliError::ReplayArtifactNotFound {
            kind: kind.to_string(),
            key: args
                .id
                .map(|id| id.to_string())
                .or_else(|| args.key.clone())
                .unwrap_or_default(),
        })?;
    let bytes = artifact.inline_body.as_deref().ok_or_else(|| {
        CliError::Runtime(RuntimeError::Config(
            "file-backed artifacts not supported in W9c replay".to_string(),
        ))
    })?;

    let (parsed, diff) = match args.kind {
        ReplayKind::Feed => {
            let entries = parse_feed(bytes, FeedKind::Rss)?;
            let parsed = json!({
                "entry_count": entries.len(),
                "entries": entries.iter().take(5).map(|entry| json!({
                    "uid": entry.feed_entry_uid,
                    "title": entry.title_raw,
                    "link": entry.link_raw,
                })).collect::<Vec<_>>(),
            });
            let diff = args
                .diff
                .then(|| json!({ "supported": false, "reason": "feed kind diff not implemented" }));
            (parsed, diff)
        }
        ReplayKind::Html => {
            let task = ArticleFetchTask {
                feed_entry_id: 0,
                normalized_link: artifact.artifact_key.clone(),
                title_raw: "(replay)".to_string(),
                summary_raw: None,
                timeout: Duration::from_secs(0),
            };
            let extracted = ReadabilityStrategy.extract(&task, bytes, &artifact.artifact_key)?;
            let replay_hash = extracted.content_hash.clone();
            let parsed = json!({
                "title": extracted.title,
                "word_count": extracted.word_count,
                "content_quality": format!("{:?}", extracted.content_quality),
                "body_preview": extracted.body_text.chars().take(200).collect::<String>(),
            });
            let diff = if args.diff {
                Some(
                    html_diff(
                        &deps.pool,
                        &artifact.artifact_key,
                        &replay_hash,
                        extracted.word_count,
                    )
                    .await?,
                )
            } else {
                None
            };
            (parsed, diff)
        }
        ReplayKind::Ai => {
            let raw = std::str::from_utf8(bytes)
                .map_err(|err| CliError::Runtime(RuntimeError::Config(err.to_string())))?;
            let parsed = match parse_response(0, raw)? {
                ParsedResponse::Output(output) => json!({
                    "keep_decision": true,
                    "summary": output.summary,
                    "importance_score": output.importance_score.get(),
                    "tags": output.tags,
                }),
                ParsedResponse::Filtered(filtered) => json!({
                    "keep_decision": false,
                    "reason": filtered.reason,
                }),
            };
            let diff = args
                .diff
                .then(|| json!({ "supported": false, "reason": "ai kind diff not implemented" }));
            (parsed, diff)
        }
    };

    Ok(ReplayCommandSummary {
        kind: kind.to_string(),
        artifact_id: artifact.id,
        artifact_key: artifact.artifact_key,
        byte_size: u32::try_from(artifact.byte_size).unwrap_or(u32::MAX),
        parsed,
        diff,
    })
}

/// Compares a replayed HTML payload with the article its feed entry produced.
///
/// An `html_payload` artifact is keyed by the feed entry id (see
/// `flows/extract.rs::write_html_artifact`), so the article is found through
/// `feed_entries.article_id`. That also covers a content-dedup hit, whose entry
/// points at the earlier article. Looking the key up as a `canonical_link`
/// never matched, so `--diff` always reported `found: false`.
async fn html_diff(
    pool: &rss_ai_news_storage::StoragePool,
    artifact_key: &str,
    replay_hash: &str,
    replay_word_count: u32,
) -> Result<Value, CliError> {
    let Ok(feed_entry_id) = artifact_key.parse::<i64>() else {
        return Ok(json!({ "found": false }));
    };
    let sql = "SELECT a.id, a.title, a.word_count, a.content_hash FROM feed_entries fe \
               JOIN articles a ON a.id = fe.article_id WHERE fe.id = $1 LIMIT 1";
    let row = match pool {
        rss_ai_news_storage::StoragePool::Sqlite(p) => {
            sqlx::query_as::<_, (i64, String, i64, String)>(sql)
                .bind(feed_entry_id)
                .fetch_optional(p)
                .await
        }
        rss_ai_news_storage::StoragePool::Postgres(p) => {
            sqlx::query_as::<_, (i64, String, i64, String)>(sql)
                .bind(feed_entry_id)
                .fetch_optional(p)
                .await
        }
    }
    .map_err(rss_ai_news_storage::StorageError::from)?;
    Ok(match row {
        Some((article_id, title, word_count, content_hash)) => json!({
            "found": true,
            "article_id": article_id,
            "title_db": title,
            "word_count_db": word_count,
            "word_count_replay": replay_word_count,
            "content_hash_match": content_hash == replay_hash,
        }),
        None => json!({ "found": false }),
    })
}

/// `--id` looks an artifact up without its kind, and the keys of different
/// kinds live in different id spaces (a feed payload is keyed by source id,
/// an HTML payload by feed entry id). An artifact of another kind is treated
/// as not found rather than parsed, and diffed, as the requested kind.
fn of_kind(artifact: Option<RawArtifact>, kind: ReplayKind) -> Option<RawArtifact> {
    let expected = match kind {
        ReplayKind::Feed => ArtifactKind::FeedPayload,
        ReplayKind::Html => ArtifactKind::HtmlPayload,
        ReplayKind::Ai => ArtifactKind::AiRawResponse,
    };
    artifact.filter(|artifact| artifact.kind == expected)
}

fn kind_storage(kind: ReplayKind) -> &'static str {
    match kind {
        ReplayKind::Feed => "feed_payload",
        ReplayKind::Html => "html_payload",
        ReplayKind::Ai => "ai_raw_response",
    }
}

#[cfg(test)]
mod tests {
    use rss_ai_news_storage::{StoragePool, build_sqlite_pool, run_migrations};
    use sqlx::SqlitePool;

    use rss_ai_news_domain::{model::RawArtifact, state::ArtifactKind};
    use time::OffsetDateTime;

    use super::{html_diff, of_kind};
    use crate::args::ReplayKind;

    fn artifact(kind: ArtifactKind) -> RawArtifact {
        RawArtifact {
            id: 7,
            kind,
            artifact_key: "42".to_string(),
            content_encoding: "identity".to_string(),
            storage_kind: "inline".to_string(),
            inline_body: Some(b"<html></html>".to_vec()),
            file_path: None,
            byte_size: 13,
            sha256: "sha".to_string(),
            retention_policy: "always".to_string(),
            expires_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// `--id` ignores the kind, so a feed payload keyed by source 42 must not
    /// be replayed and diffed as the HTML of feed entry 42.
    #[test]
    fn an_artifact_of_another_kind_is_not_replayed() {
        assert!(of_kind(Some(artifact(ArtifactKind::FeedPayload)), ReplayKind::Html).is_none());
        assert!(of_kind(Some(artifact(ArtifactKind::HtmlPayload)), ReplayKind::Ai).is_none());
        assert!(of_kind(Some(artifact(ArtifactKind::HtmlPayload)), ReplayKind::Html).is_some());
        assert!(of_kind(Some(artifact(ArtifactKind::FeedPayload)), ReplayKind::Feed).is_some());
        assert!(of_kind(None, ReplayKind::Html).is_none());
    }

    async fn insert_id(pool: &SqlitePool, sql: &'static str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .expect("fixture row should insert")
    }

    async fn insert_entry(pool: &SqlitePool, source: i64, uid: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
             title_raw, discovered_at, state, dedup_decision) VALUES (?, ?, ?, ?, ?, \
             CURRENT_TIMESTAMP, 'pending_fetch', 'fresh') RETURNING id",
        )
        .bind(source)
        .bind(uid)
        .bind(format!("https://example.com/{uid}"))
        .bind(format!("hash-{uid}"))
        .bind(format!("title {uid}"))
        .fetch_one(pool)
        .await
        .expect("fixture entry should insert")
    }

    /// The artifact key is a feed entry id; the diff must reach the article
    /// through that entry, including a dedup entry pointing at an earlier one.
    #[tokio::test]
    async fn html_diff_finds_the_article_through_the_feed_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let sqlite = build_sqlite_pool(&dir.path().join("replay.sqlite"), 2, 5_000)
            .await
            .expect("pool");
        let pool = StoragePool::Sqlite(sqlite.clone());
        run_migrations(&pool).await.expect("migrations");

        let config = insert_id(
            &sqlite,
            "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) \
             VALUES ('config_fixture', 'cfg', 'test', 'sha-cfg') RETURNING id",
        )
        .await;
        let extractor = insert_id(
            &sqlite,
            "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) \
             VALUES ('extractor_fixture', 'ext', 'test', 'sha-ext') RETURNING id",
        )
        .await;
        let source = sqlx::query_scalar::<_, i64>(
            "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
             feed_kind, config_version) VALUES ('ai', 's', 'S', 'https://example.com/f.xml', \
             'rss', ?) RETURNING id",
        )
        .bind(config)
        .fetch_one(&sqlite)
        .await
        .expect("fixture source should insert");
        let first = insert_entry(&sqlite, source, "first").await;
        let dedup = insert_entry(&sqlite, source, "dedup").await;
        let orphan = insert_entry(&sqlite, source, "orphan").await;
        let article = sqlx::query_scalar::<_, i64>(
            "INSERT INTO articles (content_hash, canonical_link, title, body_text, \
             extractor_strategy, extractor_version, content_quality, word_count, \
             origin_feed_entry_id, state) VALUES ('hash-a', 'https://example.com/first', \
             'Title A', 'body', 'readability', ?, 'high', 42, ?, 'persisted') RETURNING id",
        )
        .bind(extractor)
        .bind(first)
        .fetch_one(&sqlite)
        .await
        .expect("fixture article should insert");
        sqlx::query("UPDATE feed_entries SET article_id = ? WHERE id IN (?, ?)")
            .bind(article)
            .bind(first)
            .bind(dedup)
            .execute(&sqlite)
            .await
            .expect("link entries");

        for entry in [first, dedup] {
            let diff = html_diff(&pool, &entry.to_string(), "hash-a", 40)
                .await
                .expect("diff");
            assert_eq!(diff["found"], true, "entry {entry}");
            assert_eq!(diff["article_id"], article);
            assert_eq!(diff["title_db"], "Title A");
            assert_eq!(diff["word_count_db"], 42);
            assert_eq!(diff["word_count_replay"], 40);
            assert_eq!(diff["content_hash_match"], true);
        }
        let changed = html_diff(&pool, &first.to_string(), "hash-b", 42)
            .await
            .expect("diff");
        assert_eq!(changed["content_hash_match"], false);
        for key in [orphan.to_string(), "https://example.com/first".to_string()] {
            let diff = html_diff(&pool, &key, "hash-a", 42).await.expect("diff");
            assert_eq!(diff["found"], false, "key {key}");
        }
    }
}
