use std::io::{self, Write};

use rss_ai_news_ai::ParsedResponse;
use rss_ai_news_runtime::{
    ReplayDiff, ReplayFlow, ReplayFlowDeps, ReplayKind as RuntimeReplayKind, ReplayOptions,
    ReplayOutcome, ReplayParsed, ReplaySelector, RuntimeError,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    args::{Cli, ReplayArgs, ReplayKind},
    context_factory::{ReplayDeps, build_replay_deps},
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
    // A missing selector is a usage error: report it before loading the config
    // or opening the database, whose failures would otherwise mask it (R3-17).
    selector(args)?;
    let deps = build_replay_deps(cli).await?;
    replay(deps, args).await
}

fn selector(args: &ReplayArgs) -> Result<ReplaySelector, CliError> {
    match (args.id, args.key.as_deref()) {
        (Some(id), _) => Ok(ReplaySelector::Id(id)),
        (None, Some(key)) => Ok(ReplaySelector::Key(key.to_string())),
        (None, None) => Err(CliError::Runtime(RuntimeError::Config(
            "replay requires either --id or --key".to_string(),
        ))),
    }
}

/// Assembles the read-only [`ReplayFlow`] from the replay dependencies, runs
/// it and renders its outcome. HTML is re-extracted with the strategies the
/// configured `extractor.strategy_order` enables, as live extraction does
/// (Readability; none when only `summary_fallback` is configured, which the
/// flow reports as an error, acceptance AC-C-01).
async fn replay(deps: ReplayDeps, args: &ReplayArgs) -> Result<ReplayCommandSummary, CliError> {
    let kind = runtime_kind(args.kind);
    let selector = selector(args)?;
    let flow = ReplayFlow::new(ReplayFlowDeps {
        projection_repo: deps.projection_repo,
        html_strategies: deps.html_strategies,
    });
    let outcome = flow
        .execute(&ReplayOptions {
            kind,
            selector,
            diff: args.diff,
        })
        .await
        .map_err(cli_error)?
        .ok_or_else(|| CliError::ReplayArtifactNotFound {
            kind: kind.storage_kind().to_string(),
            key: args
                .id
                .map(|id| id.to_string())
                .or_else(|| args.key.clone())
                .unwrap_or_default(),
        })?;
    Ok(summary(outcome))
}

/// Storage failures keep their `storage` error kind, as before the flow
/// existed; every other flow error is a runtime error.
fn cli_error(error: RuntimeError) -> CliError {
    match error {
        RuntimeError::Storage(error) => CliError::Storage(error),
        other => CliError::Runtime(other),
    }
}

fn summary(outcome: ReplayOutcome) -> ReplayCommandSummary {
    ReplayCommandSummary {
        kind: outcome.kind.storage_kind().to_string(),
        artifact_id: outcome.artifact_id,
        artifact_key: outcome.artifact_key,
        byte_size: u32::try_from(outcome.byte_size).unwrap_or(u32::MAX),
        parsed: render_parsed(&outcome.parsed),
        diff: outcome.diff.as_ref().map(render_diff),
    }
}

fn render_parsed(parsed: &ReplayParsed) -> Value {
    match parsed {
        ReplayParsed::Feed {
            entry_count,
            samples,
        } => json!({
            "entry_count": entry_count,
            "entries": samples.iter().map(|entry| json!({
                "uid": entry.feed_entry_uid,
                "title": entry.title_raw,
                "link": entry.link_raw,
            })).collect::<Vec<_>>(),
        }),
        ReplayParsed::Html { base_uri, article } => json!({
            "title": article.title,
            "word_count": article.word_count,
            "content_quality": format!("{:?}", article.content_quality),
            "body_preview": article.body_text.chars().take(200).collect::<String>(),
            "base_uri": base_uri.as_str(),
        }),
        ReplayParsed::Ai(ParsedResponse::Output(output)) => json!({
            "keep_decision": true,
            "summary": output.summary,
            "importance_score": output.importance_score.get(),
            "tags": output.tags,
        }),
        ReplayParsed::Ai(ParsedResponse::Filtered(filtered)) => json!({
            "keep_decision": false,
            "reason": filtered.reason,
        }),
    }
}

fn render_diff(diff: &ReplayDiff) -> Value {
    match diff {
        ReplayDiff::Unsupported { reason } => json!({ "supported": false, "reason": reason }),
        ReplayDiff::HtmlArticleNotFound => json!({ "found": false }),
        ReplayDiff::Html {
            article_id,
            title_db,
            word_count_db,
            word_count_replay,
            content_hash_match,
        } => json!({
            "found": true,
            "article_id": article_id,
            "title_db": title_db,
            "word_count_db": word_count_db,
            "word_count_replay": word_count_replay,
            "content_hash_match": content_hash_match,
        }),
    }
}

fn runtime_kind(kind: ReplayKind) -> RuntimeReplayKind {
    match kind {
        ReplayKind::Feed => RuntimeReplayKind::Feed,
        ReplayKind::Html => RuntimeReplayKind::Html,
        ReplayKind::Ai => RuntimeReplayKind::Ai,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rss_ai_news_runtime::RuntimeError;
    use rss_ai_news_storage::{
        NewRawArtifact, RawArtifactRepo, RawArtifactRepository, ReplayProjectionRepo, StorageError,
        StoragePool, build_sqlite_pool, run_migrations,
    };
    use sqlx::SqlitePool;

    use super::{cli_error, replay};
    use crate::{
        args::{ReplayArgs, ReplayKind},
        context_factory::ReplayDeps,
        error::CliError,
    };

    /// The pool (for fixtures), a writer artifact repo (fixtures only: the
    /// replay deps themselves hold no writer) and the replay deps.
    async fn deps(dir: &tempfile::TempDir) -> (SqlitePool, RawArtifactRepo, ReplayDeps) {
        let sqlite = build_sqlite_pool(&dir.path().join("replay.sqlite"), 2, 5_000)
            .await
            .expect("pool");
        let pool = StoragePool::Sqlite(sqlite.clone());
        run_migrations(&pool).await.expect("migrations");
        let deps = ReplayDeps {
            projection_repo: Arc::new(ReplayProjectionRepo::new_with_storage(pool.clone())),
            html_strategies: vec![Arc::new(rss_ai_news_extractor::ReadabilityStrategy)],
        };
        (sqlite, RawArtifactRepo::new_with_storage(pool), deps)
    }

    async fn feed_entry(sqlite: &SqlitePool, link: &str) -> i64 {
        let config = sqlx::query_scalar::<_, i64>(
            "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) \
             VALUES ('config_fixture', 'cfg', 'test', 'sha-cfg') RETURNING id",
        )
        .fetch_one(sqlite)
        .await
        .expect("config rule");
        let source = sqlx::query_scalar::<_, i64>(
            "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
             feed_kind, config_version) VALUES ('ai', 's', 'S', 'https://example.com/f.xml', \
             'rss', ?) RETURNING id",
        )
        .bind(config)
        .fetch_one(sqlite)
        .await
        .expect("feed source");
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
             title_raw, discovered_at, state, dedup_decision) VALUES (?, 'uid', ?, 'hash', \
             'title', CURRENT_TIMESTAMP, 'pending_fetch', 'fresh') RETURNING id",
        )
        .bind(source)
        .bind(link)
        .fetch_one(sqlite)
        .await
        .expect("feed entry")
    }

    fn args(kind: ReplayKind, id: Option<i64>, key: Option<&str>, diff: bool) -> ReplayArgs {
        ReplayArgs {
            kind,
            key: key.map(str::to_string),
            id,
            diff,
        }
    }

    /// The whole command path below config loading: flow assembly with
    /// Readability, the read-only projection and JSON rendering, on an
    /// artifact whose links are relative.
    #[tokio::test]
    async fn html_replay_renders_with_the_feed_entry_link_as_base_uri() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (sqlite, artifacts, deps) = deps(&dir).await;
        let link = "https://example.com/posts/relative/";
        let entry = feed_entry(&sqlite, link).await;
        let paragraph =
            "Replay keeps the original bytes so the parse can be re-run offline. ".repeat(20);
        let body = format!(
            "<html><head><title>Relative</title></head><body><article><h1>Relative</h1>\
             <p>{paragraph} <a href=\"/about\">about</a></p>\
             <p>{paragraph} <a href=\"../next.html\">next</a></p></article></body></html>"
        )
        .into_bytes();
        let id = artifacts
            .upsert_inline(&NewRawArtifact {
                kind: "html_payload".to_string(),
                artifact_key: entry.to_string(),
                content_encoding: "identity".to_string(),
                byte_size: i64::try_from(body.len()).expect("size"),
                inline_body: body,
                sha256: "sha".to_string(),
                retention_policy: "always".to_string(),
                expires_at: None,
            })
            .await
            .expect("artifact");

        let summary = replay(deps, &args(ReplayKind::Html, Some(id), None, true))
            .await
            .expect("replay");
        let value = serde_json::to_value(&summary).expect("json");
        assert_eq!(value["kind"], "html_payload");
        assert_eq!(value["artifact_id"], id);
        assert_eq!(value["artifact_key"], entry.to_string());
        assert_eq!(value["parsed"]["base_uri"], link);
        assert_eq!(value["parsed"]["title"], "Relative");
        assert!(value["parsed"]["word_count"].as_u64().expect("count") > 0);
        assert!(value["parsed"]["content_quality"].is_string());
        assert!(value["parsed"]["body_preview"].is_string());
        // The entry has no article yet.
        assert_eq!(value["diff"], serde_json::json!({ "found": false }));
    }

    #[tokio::test]
    async fn replay_failures_keep_their_error_kinds() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (_sqlite, _artifacts, replay_deps) = deps(&dir).await;
        let error = replay(
            replay_deps,
            &args(ReplayKind::Feed, None, Some("missing"), false),
        )
        .await
        .expect_err("missing artifact");
        assert!(
            matches!(&error, CliError::ReplayArtifactNotFound { kind, key }
                if kind == "feed_payload" && key == "missing"),
            "{error:?}"
        );

        let (_sqlite, _artifacts, replay_deps) = deps(&dir).await;
        let error = replay(replay_deps, &args(ReplayKind::Ai, None, None, false))
            .await
            .expect_err("no selector");
        assert_eq!(
            error.to_string(),
            "config: replay requires either --id or --key"
        );

        let storage = RuntimeError::Storage(StorageError::UnsupportedBackend("x".into()));
        assert_eq!(cli_error(storage).error_kind(), "storage");
        let config = RuntimeError::Config("x".into());
        assert_eq!(cli_error(config).error_kind(), "runtime");
    }
}
