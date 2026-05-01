use std::{
    io::{self, Write},
    time::Duration,
};

use rss_ai_news_ai::{ParsedResponse, parse_response};
use rss_ai_news_domain::{dto::extract::ArticleFetchTask, state::FeedKind};
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
    let artifact = artifact.ok_or_else(|| CliError::ReplayArtifactNotFound {
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

async fn html_diff(
    pool: &sqlx::SqlitePool,
    canonical_link: &str,
    replay_hash: &str,
    replay_word_count: u32,
) -> Result<Value, CliError> {
    let row = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT title, word_count, content_hash FROM articles WHERE canonical_link = ? LIMIT 1",
    )
    .bind(canonical_link)
    .fetch_optional(pool)
    .await
    .map_err(rss_ai_news_storage::StorageError::from)?;
    Ok(match row {
        Some((title, word_count, content_hash)) => json!({
            "found": true,
            "title_db": title,
            "word_count_db": word_count,
            "word_count_replay": replay_word_count,
            "content_hash_match": content_hash == replay_hash,
        }),
        None => json!({ "found": false }),
    })
}

fn kind_storage(kind: ReplayKind) -> &'static str {
    match kind {
        ReplayKind::Feed => "feed_payload",
        ReplayKind::Html => "html_payload",
        ReplayKind::Ai => "ai_raw_response",
    }
}
