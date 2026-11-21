//! Read-only replay of one `raw_artifacts` row (plan/10 §4).
//!
//! The flow looks the artifact up, re-parses its bytes in memory with the
//! current parser for its kind and, for `html --diff`, compares the result
//! with the article the feed entry produced. It writes nothing: its
//! dependencies are the artifact lookup, a read-only projection and the
//! extraction strategy, with no writer repository or network client.

use std::{sync::Arc, time::Duration};

use rss_ai_news_ai::{ParsedResponse, parse_response};
use rss_ai_news_domain::{
    dto::{
        extract::{ArticleFetchTask, ExtractedArticle},
        feed::FeedEntryMeta,
    },
    model::RawArtifact,
    state::{ArtifactKind, FeedKind},
};
use rss_ai_news_extractor::{ContentStrategy, decode_html};
use rss_ai_news_feed::parse_feed;
use rss_ai_news_storage::{HtmlReplaySource, ReplayProjectionRepository};

use crate::error::RuntimeError;

/// Base URI for an HTML replay whose artifact key does not name an existing
/// feed entry (a hand-imported row, or an entry that no longer exists).
/// `about:blank` is a valid URL that cannot be a base, so no relative
/// reference resolves against it: nothing is invented about the page's host.
pub const REPLAY_PLACEHOLDER_BASE_URI: &str = "about:blank";

/// Reported by an HTML replay when the configured `extractor.strategy_order`
/// enables no HTML strategy (only `summary_fallback`): live extraction then
/// never parses the HTML, so a replay result parsed with some other strategy
/// would not describe what the pipeline does with these bytes (R3-6).
pub const REPLAY_HTML_NO_STRATEGY: &str = "html replay needs an HTML extraction strategy, but \
     extractor.strategy_order enables none (only summary_fallback, which stores the feed summary \
     without parsing the HTML); add \"readability\" to replay the HTML";

/// Title handed to the strategy when the artifact names no feed entry (with
/// an entry, its `title_raw` is used, as in live extraction).
pub const REPLAY_PLACEHOLDER_TITLE: &str = "(replay)";

/// Number of parsed feed entries a feed replay returns as samples.
pub const REPLAY_FEED_SAMPLE_LIMIT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayKind {
    Feed,
    Html,
    Ai,
}

impl ReplayKind {
    /// `raw_artifacts.kind` value of this replay kind.
    pub fn storage_kind(self) -> &'static str {
        match self {
            Self::Feed => "feed_payload",
            Self::Html => "html_payload",
            Self::Ai => "ai_raw_response",
        }
    }

    fn artifact_kind(self) -> ArtifactKind {
        match self {
            Self::Feed => ArtifactKind::FeedPayload,
            Self::Html => ArtifactKind::HtmlPayload,
            Self::Ai => ArtifactKind::AiRawResponse,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaySelector {
    /// `raw_artifacts.id`; the kind is checked after the lookup.
    Id(i64),
    /// `(kind, artifact_key)`.
    Key(String),
}

#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub kind: ReplayKind,
    pub selector: ReplaySelector,
    pub diff: bool,
}

/// Where the base URI handed to the extraction strategy came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayBaseUri {
    /// `feed_entries.normalized_link` of the entry the artifact is keyed by.
    FeedEntryLink(String),
    /// [`REPLAY_PLACEHOLDER_BASE_URI`].
    Placeholder,
}

impl ReplayBaseUri {
    pub fn as_str(&self) -> &str {
        match self {
            Self::FeedEntryLink(link) => link,
            Self::Placeholder => REPLAY_PLACEHOLDER_BASE_URI,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ReplayParsed {
    Feed {
        entry_count: usize,
        /// The first [`REPLAY_FEED_SAMPLE_LIMIT`] entries.
        samples: Vec<FeedEntryMeta>,
    },
    Html {
        base_uri: ReplayBaseUri,
        article: ExtractedArticle,
    },
    Ai(ParsedResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDiff {
    /// `--diff` is not implemented for this kind; no business table is read.
    Unsupported { reason: &'static str },
    /// The artifact's feed entry has no article (or no such entry exists).
    HtmlArticleNotFound,
    Html {
        article_id: i64,
        title_db: String,
        word_count_db: i64,
        word_count_replay: u32,
        content_hash_match: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    pub kind: ReplayKind,
    pub artifact_id: i64,
    pub artifact_key: String,
    pub byte_size: i64,
    pub parsed: ReplayParsed,
    pub diff: Option<ReplayDiff>,
}

pub struct ReplayFlowDeps {
    /// The flow's only storage access: read-only artifact lookups and the
    /// HTML projection (no writer repository, R3-20).
    pub projection_repo: Arc<dyn ReplayProjectionRepository>,
    /// The HTML strategies live extraction runs, in `extractor.strategy_order`
    /// (the CLI builds both from one function). An HTML replay uses the first
    /// that extracts, else reports the last strategy's error. Empty when only
    /// `summary_fallback` is configured: see [`REPLAY_HTML_NO_STRATEGY`].
    pub html_strategies: Vec<Arc<dyn ContentStrategy>>,
}

pub struct ReplayFlow {
    deps: ReplayFlowDeps,
}

impl ReplayFlow {
    pub fn new(deps: ReplayFlowDeps) -> Self {
        Self { deps }
    }

    /// `Ok(None)` when no artifact of the requested kind matches the selector.
    pub async fn execute(
        &self,
        options: &ReplayOptions,
    ) -> Result<Option<ReplayOutcome>, RuntimeError> {
        // A configuration contradiction, reported before any lookup.
        if options.kind == ReplayKind::Html && self.deps.html_strategies.is_empty() {
            return Err(RuntimeError::Config(REPLAY_HTML_NO_STRATEGY.to_string()));
        }
        let artifact = match &options.selector {
            ReplaySelector::Id(id) => self.deps.projection_repo.artifact_by_id(*id).await?,
            ReplaySelector::Key(key) => {
                self.deps
                    .projection_repo
                    .artifact_by_key(options.kind.storage_kind(), key)
                    .await?
            }
        };
        let Some(artifact) = of_kind(artifact, options.kind) else {
            return Ok(None);
        };
        let bytes = artifact.inline_body.as_deref().ok_or_else(|| {
            RuntimeError::Config("file-backed artifacts not supported in W9c replay".to_string())
        })?;

        let (parsed, diff) = match options.kind {
            ReplayKind::Feed => {
                let entries = parse_feed(bytes, FeedKind::Rss)?;
                let entry_count = entries.len();
                let mut samples = entries;
                samples.truncate(REPLAY_FEED_SAMPLE_LIMIT);
                let diff = options.diff.then_some(ReplayDiff::Unsupported {
                    reason: "feed kind diff not implemented",
                });
                (
                    ReplayParsed::Feed {
                        entry_count,
                        samples,
                    },
                    diff,
                )
            }
            ReplayKind::Html => self.replay_html(&artifact, bytes, options.diff).await?,
            ReplayKind::Ai => {
                let raw = std::str::from_utf8(bytes)
                    .map_err(|err| RuntimeError::Config(err.to_string()))?;
                let parsed = parse_response(0, raw)?;
                let diff = options.diff.then_some(ReplayDiff::Unsupported {
                    reason: "ai kind diff not implemented",
                });
                (ReplayParsed::Ai(parsed), diff)
            }
        };

        Ok(Some(ReplayOutcome {
            kind: options.kind,
            artifact_id: artifact.id,
            artifact_key: artifact.artifact_key,
            byte_size: artifact.byte_size,
            parsed,
            diff,
        }))
    }

    /// An `html_payload` artifact is keyed by its feed entry id, so the entry
    /// supplies the base URI (the link the live extraction fetched) and, for
    /// `--diff`, the article it produced.
    async fn replay_html(
        &self,
        artifact: &RawArtifact,
        bytes: &[u8],
        diff: bool,
    ) -> Result<(ReplayParsed, Option<ReplayDiff>), RuntimeError> {
        let source: Option<HtmlReplaySource> = match artifact.artifact_key.parse::<i64>() {
            Ok(feed_entry_id) => {
                self.deps
                    .projection_repo
                    .html_replay_source(feed_entry_id)
                    .await?
            }
            Err(_) => None,
        };
        // The entry's feed title is the strategy's title fallback, exactly as
        // in live extraction (`claimed.title_raw`), so a page without its own
        // title replays to the same title (R3-18).
        let (feed_entry_id, base_uri, title_raw) = match &source {
            Some(source) => (
                source.feed_entry_id,
                ReplayBaseUri::FeedEntryLink(source.normalized_link.clone()),
                source.title_raw.clone(),
            ),
            None => (
                0,
                ReplayBaseUri::Placeholder,
                REPLAY_PLACEHOLDER_TITLE.to_string(),
            ),
        };
        let task = ArticleFetchTask {
            feed_entry_id,
            normalized_link: base_uri.as_str().to_string(),
            title_raw,
            summary_raw: None,
            timeout: Duration::from_secs(0),
        };
        let extracted = self.extract_html(&task, bytes, base_uri.as_str())?;
        let diff = diff.then(|| match source.and_then(|source| source.article) {
            Some(article) => ReplayDiff::Html {
                article_id: article.id,
                title_db: article.title,
                word_count_db: article.word_count,
                word_count_replay: extracted.word_count,
                content_hash_match: article.content_hash == extracted.content_hash,
            },
            None => ReplayDiff::HtmlArticleNotFound,
        });
        Ok((
            ReplayParsed::Html {
                base_uri,
                article: extracted,
            },
            diff,
        ))
    }
}

impl ReplayFlow {
    /// The configured chain, in order: the first strategy that extracts wins,
    /// else the last strategy's error (with Readability alone, exactly its
    /// result). Callers ensure the chain is not empty.
    ///
    /// Like live extraction (`run_strategy_chain`), the stored original bytes
    /// are decoded to UTF-8 first. The response's Content-Type is not kept
    /// with the artifact, so the charset comes from the BOM or the page's
    /// `<meta>` declaration, else UTF-8; a page whose charset only its HTTP
    /// header declared can replay differently (documented in plan/10).
    fn extract_html(
        &self,
        task: &ArticleFetchTask,
        bytes: &[u8],
        base_uri: &str,
    ) -> Result<ExtractedArticle, RuntimeError> {
        let html = decode_html(bytes, None)?;
        let mut last_error = None;
        for strategy in &self.deps.html_strategies {
            match strategy.extract(task, html.as_bytes(), base_uri) {
                Ok(article) => return Ok(article),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.map_or_else(
            || RuntimeError::Config(REPLAY_HTML_NO_STRATEGY.to_string()),
            RuntimeError::from,
        ))
    }
}

/// `--id` looks an artifact up without its kind, and the keys of different
/// kinds live in different id spaces (a feed payload is keyed by source id,
/// an HTML payload by feed entry id). An artifact of another kind is treated
/// as not found rather than parsed, and diffed, as the requested kind.
fn of_kind(artifact: Option<RawArtifact>, kind: ReplayKind) -> Option<RawArtifact> {
    let expected = kind.artifact_kind();
    artifact.filter(|artifact| artifact.kind == expected)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rss_ai_news_domain::{
        dto::extract::{ArticleFetchTask, ExtractedArticle},
        model::RawArtifact,
        state::{ArtifactKind, ExtractorStrategy},
    };
    use rss_ai_news_extractor::{ContentStrategy, ExtractorError, ReadabilityStrategy};
    use rss_ai_news_storage::{
        NewRawArtifact, RawArtifactRepo, RawArtifactRepository, ReplayProjectionRepo, StoragePool,
        build_sqlite_pool, run_migrations,
    };
    use sqlx::SqlitePool;
    use time::OffsetDateTime;

    use super::{
        REPLAY_HTML_NO_STRATEGY, REPLAY_PLACEHOLDER_BASE_URI, ReplayBaseUri, ReplayDiff,
        ReplayFlow, ReplayFlowDeps, ReplayKind, ReplayOptions, ReplayParsed, ReplaySelector,
        of_kind,
    };

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

    /// Records the link and final URL the flow hands to the strategy, then
    /// extracts with Readability as the CLI does.
    #[derive(Default)]
    struct RecordingStrategy {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl ContentStrategy for RecordingStrategy {
        fn strategy(&self) -> ExtractorStrategy {
            ExtractorStrategy::Readability
        }

        fn extract(
            &self,
            task: &ArticleFetchTask,
            html_bytes: &[u8],
            final_url: &str,
        ) -> Result<ExtractedArticle, ExtractorError> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((task.normalized_link.clone(), final_url.to_string()));
            ReadabilityStrategy.extract(task, html_bytes, final_url)
        }
    }

    /// An article page whose links are all relative, so a wrong base URI is
    /// what the strategy (and any later link resolution) would build on.
    fn relative_link_html() -> Vec<u8> {
        let paragraph =
            "Replay keeps the original bytes so the parse can be re-run offline. ".repeat(20);
        format!(
            "<html><head><title>Relative Links</title></head><body><article>\
             <h1>Relative Links</h1><p>{paragraph} <a href=\"/about\">about</a></p>\
             <p>{paragraph} <a href=\"../next.html\">next</a> <img src=\"img/a.png\"></p>\
             </article></body></html>"
        )
        .into_bytes()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        pool: StoragePool,
        sqlite: SqlitePool,
        artifacts: Arc<RawArtifactRepo>,
        source: i64,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let sqlite = build_sqlite_pool(&dir.path().join("replay.sqlite"), 2, 5_000)
                .await
                .expect("pool");
            let pool = StoragePool::Sqlite(sqlite.clone());
            run_migrations(&pool).await.expect("migrations");
            let config = sqlx::query_scalar::<_, i64>(
                "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256) \
                 VALUES ('config_fixture', 'cfg', 'test', 'sha-cfg') RETURNING id",
            )
            .fetch_one(&sqlite)
            .await
            .expect("config rule");
            let source = sqlx::query_scalar::<_, i64>(
                "INSERT INTO feed_sources (category_key, source_key, display_name, feed_url, \
                 feed_kind, config_version) VALUES ('ai', 's', 'S', 'https://example.com/f.xml', \
                 'rss', ?) RETURNING id",
            )
            .bind(config)
            .fetch_one(&sqlite)
            .await
            .expect("feed source");
            Self {
                artifacts: Arc::new(RawArtifactRepo::new_with_storage(pool.clone())),
                _dir: dir,
                pool,
                sqlite,
                source,
            }
        }

        async fn entry(&self, uid: &str) -> i64 {
            sqlx::query_scalar::<_, i64>(
                "INSERT INTO feed_entries (source_id, feed_entry_uid, normalized_link, link_hash, \
                 title_raw, discovered_at, state, dedup_decision) VALUES (?, ?, ?, ?, ?, \
                 CURRENT_TIMESTAMP, 'pending_fetch', 'fresh') RETURNING id",
            )
            .bind(self.source)
            .bind(uid)
            .bind(format!("https://example.com/posts/{uid}/"))
            .bind(format!("hash-{uid}"))
            .bind(format!("title {uid}"))
            .fetch_one(&self.sqlite)
            .await
            .expect("feed entry")
        }

        async fn article(&self, origin: i64, content_hash: &str, entries: &[i64]) -> i64 {
            let extractor = sqlx::query_scalar::<_, i64>(
                "INSERT INTO rule_versions (kind, version_tag, description, payload_sha256, \
                 status) VALUES ('extractor_fixture', ?, 'test', 'sha-ext', 'superseded') \
                 RETURNING id",
            )
            .bind(content_hash)
            .fetch_one(&self.sqlite)
            .await
            .expect("extractor rule");
            let article = sqlx::query_scalar::<_, i64>(
                "INSERT INTO articles (content_hash, canonical_link, title, body_text, \
                 extractor_strategy, extractor_version, content_quality, word_count, \
                 origin_feed_entry_id, state) VALUES (?, 'https://example.com/canonical', \
                 'Title A', 'body', 'readability', ?, 'high', 42, ?, 'persisted') RETURNING id",
            )
            .bind(content_hash)
            .bind(extractor)
            .bind(origin)
            .fetch_one(&self.sqlite)
            .await
            .expect("article");
            for entry in entries {
                sqlx::query("UPDATE feed_entries SET article_id = ? WHERE id = ?")
                    .bind(article)
                    .bind(entry)
                    .execute(&self.sqlite)
                    .await
                    .expect("link entry");
            }
            article
        }

        async fn html_artifact(&self, key: &str, body: Vec<u8>) -> i64 {
            self.artifacts
                .upsert_inline(&NewRawArtifact {
                    kind: "html_payload".to_string(),
                    artifact_key: key.to_string(),
                    content_encoding: "identity".to_string(),
                    byte_size: i64::try_from(body.len()).expect("size"),
                    inline_body: body,
                    sha256: "sha".to_string(),
                    retention_policy: "always".to_string(),
                    expires_at: None,
                })
                .await
                .expect("artifact")
        }

        fn flow(&self, strategy: Arc<RecordingStrategy>) -> ReplayFlow {
            ReplayFlow::new(ReplayFlowDeps {
                projection_repo: Arc::new(ReplayProjectionRepo::new_with_storage(
                    self.pool.clone(),
                )),
                html_strategies: vec![strategy],
            })
        }
    }

    fn html(selector: ReplaySelector, diff: bool) -> ReplayOptions {
        ReplayOptions {
            kind: ReplayKind::Html,
            selector,
            diff,
        }
    }

    /// The artifact key is the feed entry id. The base URI must be that
    /// entry's link, never the numeric key itself.
    #[tokio::test]
    async fn html_replay_uses_the_feed_entry_link_as_base_uri() {
        let fixture = Fixture::new().await;
        let entry = fixture.entry("first").await;
        let id = fixture
            .html_artifact(&entry.to_string(), relative_link_html())
            .await;
        let strategy = Arc::new(RecordingStrategy::default());
        let outcome = fixture
            .flow(strategy.clone())
            .execute(&html(ReplaySelector::Id(id), false))
            .await
            .expect("replay")
            .expect("artifact found");

        let link = "https://example.com/posts/first/".to_string();
        assert_eq!(
            *strategy.calls.lock().expect("calls"),
            vec![(link.clone(), link.clone())]
        );
        let ReplayParsed::Html { base_uri, article } = outcome.parsed else {
            panic!("html replay must parse as html");
        };
        assert_eq!(base_uri, ReplayBaseUri::FeedEntryLink(link.clone()));
        assert_eq!(article.canonical_link, link);
        assert_eq!(article.feed_entry_id, entry);
        assert_eq!(article.title, "Relative Links");
        assert_eq!(outcome.artifact_key, entry.to_string());
        assert!(outcome.diff.is_none());
    }

    /// A key that names no feed entry falls back to the documented
    /// placeholder instead of passing the raw key on as a URL.
    #[tokio::test]
    async fn html_replay_without_a_feed_entry_uses_the_placeholder_base_uri() {
        let fixture = Fixture::new().await;
        for key in ["999999", "not-an-entry-id"] {
            fixture.html_artifact(key, relative_link_html()).await;
            let strategy = Arc::new(RecordingStrategy::default());
            let outcome = fixture
                .flow(strategy.clone())
                .execute(&html(ReplaySelector::Key(key.to_string()), true))
                .await
                .expect("replay")
                .expect("artifact found");
            let placeholder = REPLAY_PLACEHOLDER_BASE_URI.to_string();
            assert_eq!(
                *strategy.calls.lock().expect("calls"),
                vec![(placeholder.clone(), placeholder)],
                "key {key}"
            );
            assert!(matches!(
                outcome.parsed,
                ReplayParsed::Html {
                    base_uri: ReplayBaseUri::Placeholder,
                    ..
                }
            ));
            assert_eq!(outcome.diff, Some(ReplayDiff::HtmlArticleNotFound));
        }
    }

    /// R3-18: a page without a title of its own falls back to the entry's
    /// feed title, as live extraction does; without an entry the placeholder
    /// title is used.
    #[tokio::test]
    async fn html_replay_falls_back_to_the_feed_entry_title() {
        let paragraph =
            "Replay keeps the original bytes so the parse can be re-run offline. ".repeat(20);
        let untitled =
            format!("<html><body><div><p>{paragraph}</p><p>{paragraph}</p></div></body></html>")
                .into_bytes();
        let fixture = Fixture::new().await;
        let entry = fixture.entry("untitled").await;
        fixture
            .html_artifact(&entry.to_string(), untitled.clone())
            .await;
        fixture.html_artifact("no-entry", untitled).await;
        let flow = fixture.flow(Arc::new(RecordingStrategy::default()));
        for (key, title) in [
            (entry.to_string(), "title untitled".to_string()),
            (
                "no-entry".to_string(),
                super::REPLAY_PLACEHOLDER_TITLE.to_string(),
            ),
        ] {
            let outcome = flow
                .execute(&html(ReplaySelector::Key(key.clone()), false))
                .await
                .expect("replay")
                .expect("artifact found");
            let ReplayParsed::Html { article, .. } = outcome.parsed else {
                panic!("html replay must parse as html");
            };
            assert_eq!(article.title, title, "key {key}");
        }
    }

    /// A GBK page declared by `<meta charset>` replays to readable text, as
    /// live extraction decodes it.
    #[tokio::test]
    async fn html_replay_decodes_a_meta_declared_gbk_page() {
        // "中文标题" in GBK.
        const GBK_TITLE: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4, 0xB1, 0xEA, 0xCC, 0xE2];
        let paragraph =
            "Replay keeps the original bytes so the parse can be re-run offline. ".repeat(20);
        let mut page = b"<html><head><meta charset=\"gbk\"><title>".to_vec();
        page.extend_from_slice(GBK_TITLE);
        page.extend_from_slice(b"</title></head><body><article><h1>");
        page.extend_from_slice(GBK_TITLE);
        page.extend_from_slice(
            format!("</h1><p>{paragraph}</p><p>{paragraph}</p></article></body></html>").as_bytes(),
        );
        assert!(
            std::str::from_utf8(&page).is_err(),
            "fixture must not be UTF-8"
        );

        let fixture = Fixture::new().await;
        let entry = fixture.entry("gbk").await;
        fixture.html_artifact(&entry.to_string(), page).await;
        let outcome = fixture
            .flow(Arc::new(RecordingStrategy::default()))
            .execute(&html(ReplaySelector::Key(entry.to_string()), false))
            .await
            .expect("replay")
            .expect("artifact found");
        let ReplayParsed::Html { article, .. } = outcome.parsed else {
            panic!("html replay must parse as html");
        };
        assert_eq!(article.title, "中文标题");
    }

    /// `--diff` reaches the article through the entry, including a dedup
    /// entry pointing at an earlier article; an entry without an article is
    /// reported as not found.
    #[tokio::test]
    async fn html_diff_finds_the_article_through_the_feed_entry() {
        let fixture = Fixture::new().await;
        let first = fixture.entry("first").await;
        let dedup = fixture.entry("dedup").await;
        let orphan = fixture.entry("orphan").await;
        for entry in [first, dedup, orphan] {
            fixture
                .html_artifact(&entry.to_string(), relative_link_html())
                .await;
        }
        let flow = fixture.flow(Arc::new(RecordingStrategy::default()));
        let replay = async |entry: i64| {
            flow.execute(&html(ReplaySelector::Key(entry.to_string()), true))
                .await
                .expect("replay")
                .expect("artifact found")
        };

        // The stored hash differs from the replay's: a mismatch.
        let article = fixture.article(first, "hash-old", &[first, dedup]).await;
        for entry in [first, dedup] {
            let outcome = replay(entry).await;
            let ReplayParsed::Html {
                article: extracted, ..
            } = &outcome.parsed
            else {
                panic!("html replay must parse as html");
            };
            assert_eq!(
                outcome.diff,
                Some(ReplayDiff::Html {
                    article_id: article,
                    title_db: "Title A".to_string(),
                    word_count_db: 42,
                    word_count_replay: extracted.word_count,
                    content_hash_match: false,
                }),
                "entry {entry}"
            );
        }
        let orphan_outcome = replay(orphan).await;
        assert_eq!(orphan_outcome.diff, Some(ReplayDiff::HtmlArticleNotFound));

        // Same bytes, same parser: the hashes match.
        let ReplayParsed::Html {
            article: extracted, ..
        } = orphan_outcome.parsed
        else {
            panic!("html replay must parse as html");
        };
        fixture
            .article(orphan, &extracted.content_hash, &[orphan])
            .await;
        assert!(matches!(
            replay(orphan).await.diff,
            Some(ReplayDiff::Html {
                content_hash_match: true,
                ..
            })
        ));
    }

    /// R3-6: with only `summary_fallback` configured, live extraction never
    /// parses the HTML, so an HTML replay reports the contract error instead
    /// of parsing with a strategy the pipeline does not run. Other kinds are
    /// unaffected.
    #[tokio::test]
    async fn html_replay_without_an_html_strategy_reports_the_contract_error() {
        let fixture = Fixture::new().await;
        let entry = fixture.entry("first").await;
        fixture
            .html_artifact(&entry.to_string(), relative_link_html())
            .await;
        let flow = ReplayFlow::new(ReplayFlowDeps {
            projection_repo: Arc::new(ReplayProjectionRepo::new_with_storage(fixture.pool.clone())),
            html_strategies: Vec::new(),
        });
        let error = flow
            .execute(&html(ReplaySelector::Key(entry.to_string()), true))
            .await
            .expect_err("no HTML strategy configured");
        assert_eq!(
            error.to_string(),
            format!("config: {REPLAY_HTML_NO_STRATEGY}")
        );

        let feed = flow
            .execute(&ReplayOptions {
                kind: ReplayKind::Feed,
                selector: ReplaySelector::Key("missing".to_string()),
                diff: false,
            })
            .await
            .expect("feed replay does not need an HTML strategy");
        assert!(feed.is_none());
    }

    /// A strategy that fails is followed by the next one, as in the live
    /// chain; with every strategy failing the last error is reported.
    #[tokio::test]
    async fn html_replay_runs_the_configured_chain_in_order() {
        struct Failing;
        impl ContentStrategy for Failing {
            fn strategy(&self) -> ExtractorStrategy {
                ExtractorStrategy::Readability
            }
            fn extract(
                &self,
                _task: &ArticleFetchTask,
                _html_bytes: &[u8],
                _final_url: &str,
            ) -> Result<ExtractedArticle, ExtractorError> {
                Err(ExtractorError::ParseFailed {
                    reason: "first strategy fails".to_string(),
                })
            }
        }
        let fixture = Fixture::new().await;
        let entry = fixture.entry("first").await;
        fixture
            .html_artifact(&entry.to_string(), relative_link_html())
            .await;
        let flow = |html_strategies| {
            ReplayFlow::new(ReplayFlowDeps {
                projection_repo: Arc::new(ReplayProjectionRepo::new_with_storage(
                    fixture.pool.clone(),
                )),
                html_strategies,
            })
        };
        let recording = Arc::new(RecordingStrategy::default());
        let chain: Vec<Arc<dyn ContentStrategy>> = vec![Arc::new(Failing), recording.clone()];
        let outcome = flow(chain)
            .execute(&html(ReplaySelector::Key(entry.to_string()), false))
            .await
            .expect("second strategy extracts")
            .expect("artifact found");
        assert!(matches!(outcome.parsed, ReplayParsed::Html { .. }));
        assert_eq!(recording.calls.lock().expect("calls").len(), 1);

        let error = flow(vec![Arc::new(Failing)])
            .execute(&html(ReplaySelector::Key(entry.to_string()), false))
            .await
            .expect_err("every strategy fails");
        assert!(
            error.to_string().contains("first strategy fails"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn replay_reports_missing_and_file_backed_artifacts() {
        let fixture = Fixture::new().await;
        let flow = fixture.flow(Arc::new(RecordingStrategy::default()));
        for selector in [
            ReplaySelector::Id(424_242),
            ReplaySelector::Key("1".to_string()),
        ] {
            let found = flow
                .execute(&html(selector.clone(), false))
                .await
                .expect("lookup");
            assert!(found.is_none(), "{selector:?}");
        }

        let file_backed = sqlx::query_scalar::<_, i64>(
            "INSERT INTO raw_artifacts (kind, artifact_key, content_encoding, storage_kind, \
             file_path, byte_size, sha256, retention_policy) VALUES ('html_payload', '5', \
             'identity', 'file', '/tmp/a.html', 1, 'sha', 'always') RETURNING id",
        )
        .fetch_one(&fixture.sqlite)
        .await
        .expect("file-backed artifact");
        let error = flow
            .execute(&html(ReplaySelector::Id(file_backed), false))
            .await
            .expect_err("file-backed artifacts are rejected");
        assert_eq!(
            error.to_string(),
            "config: file-backed artifacts not supported in W9c replay"
        );
    }
}
