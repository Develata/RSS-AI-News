use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use http_body_util::{BodyExt, Limited, combinators::BoxBody};
use octocrab::Octocrab;
use rss_ai_news_domain::{SecretString, dto::publish::RenderedReport};
use serde_json::{Value, json};

use crate::error::PublishError;
use crate::target::{PublishTarget, PublishedArtifact, PublishedBatchArtifact};

/// GitHub remote-target configuration. The `token` field is a
/// [`SecretString`] so its raw value is redacted by the type's own
/// `Debug` / `Display` / `Serialize` impls — `tracing::error!("…{cfg:?}")`
/// or panic messages cannot leak credentials. The token is only exposed
/// at the actual `Octocrab::personal_token` boundary (W2-A2 follow-up to
/// the F4-2 manual-Debug fix).
#[derive(Clone, Debug)]
pub struct GitHubTargetConfig {
    pub token: SecretString,
    pub owner: String,
    pub repo: String,
    pub branch: String,
    pub path_prefix: String,
    pub commit_message_prefix: String,
}

pub struct GitHubTarget {
    cfg: GitHubTargetConfig,
    client: Arc<Octocrab>,
}

impl GitHubTarget {
    pub fn new(cfg: GitHubTargetConfig) -> Result<Self, PublishError> {
        let client = Octocrab::builder()
            .set_connect_timeout(Some(Duration::from_secs(5)))
            .set_read_timeout(Some(Duration::from_secs(30)))
            .set_write_timeout(Some(Duration::from_secs(30)))
            .personal_token(secrecy::SecretString::from(
                cfg.token.expose_secret().to_owned(),
            ))
            .build()
            .map_err(|error| PublishError::GitHubAuthFailed(format!("octocrab build: {error}")))?;
        Ok(Self {
            cfg,
            client: Arc::new(client),
        })
    }

    pub fn with_base_uri(cfg: GitHubTargetConfig, base_uri: &str) -> Result<Self, PublishError> {
        let client = Octocrab::builder()
            .set_connect_timeout(Some(Duration::from_secs(5)))
            .set_read_timeout(Some(Duration::from_secs(30)))
            .set_write_timeout(Some(Duration::from_secs(30)))
            .personal_token(secrecy::SecretString::from(
                cfg.token.expose_secret().to_owned(),
            ))
            .base_uri(base_uri)
            .map_err(|error| PublishError::GitHubAuthFailed(format!("octocrab base_uri: {error}")))?
            .build()
            .map_err(|error| PublishError::GitHubAuthFailed(format!("octocrab build: {error}")))?;
        Ok(Self {
            cfg,
            client: Arc::new(client),
        })
    }

    pub fn with_client(cfg: GitHubTargetConfig, client: Arc<Octocrab>) -> Self {
        Self { cfg, client }
    }

    fn join_path(&self, relative: &str) -> Result<String, PublishError> {
        validate_path_part(relative)?;
        let prefix = self.cfg.path_prefix.trim_matches('/');
        if prefix.is_empty() {
            Ok(relative.replace('\\', "/"))
        } else {
            validate_path_part(prefix)?;
            Ok(format!(
                "{}/{}",
                prefix.replace('\\', "/"),
                relative.replace('\\', "/")
            ))
        }
    }

    /// `owner/repo:branch`, the form recorded in `publish_records.remote_target`
    /// at init. The path prefix is not part of it (see plan/04 §4.2).
    pub fn identity(&self) -> String {
        format!("{}/{}:{}", self.cfg.owner, self.cfg.repo, self.cfg.branch)
    }

    fn remote_target_url(&self, final_path: &str) -> String {
        format!(
            "github://{}/{}/{}/{}",
            self.cfg.owner, self.cfg.repo, self.cfg.branch, final_path
        )
    }
}

#[async_trait]
impl PublishTarget for GitHubTarget {
    fn target_identity(&self) -> Option<String> {
        Some(self.identity())
    }

    async fn publish(&self, report: &RenderedReport) -> Result<PublishedArtifact, PublishError> {
        let final_path = self.join_path(&report.relative_path)?;
        let existing_sha = self.existing_sha(&final_path).await?;
        let commit_sha = self
            .put_file(report, &final_path, existing_sha.as_deref())
            .await?;

        Ok(PublishedArtifact {
            local_path: None,
            commit_sha: Some(commit_sha),
            remote_target: Some(self.remote_target_url(&final_path)),
        })
    }

    async fn publish_many(
        &self,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        if reports.is_empty() {
            return Ok(PublishedBatchArtifact {
                artifacts: Vec::new(),
                commit_sha: None,
            });
        }
        if reports.len() == 1 {
            let artifact = self.publish(&reports[0]).await?;
            return Ok(PublishedBatchArtifact {
                commit_sha: artifact.commit_sha.clone(),
                artifacts: vec![artifact],
            });
        }

        // PATCH refs/heads/<branch> with force=false 在另一个 push 抢先落到分支时
        // 会以 422 "Update is not a fast forward" 失败。此时之前抓的
        // base_tree_sha 已过期，必须重新走 head_commit_and_tree → create_tree →
        // create_commit → update_branch_ref 整套。重试上限 2 次（首次 + 1 retry），
        // 仍失败则透传 422 让上层 publish_record retry/lease 接管。
        //
        // 关于 unreachable! 末尾：循环里 Ok 直接 return；非 retryable Err 也直接
        // return；只有 retryable Err 且 attempt < MAX_ATTEMPTS 走 continue。最后
        // 一轮 attempt == MAX_ATTEMPTS 时 retry guard 短路为 false，retryable Err
        // 也会落到默认 `Err(error) => return Err(error)`。因此循环正常结束（无
        // return）的路径不存在。
        const MAX_ATTEMPTS: u32 = 2;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.publish_many_atomic(reports).await {
                Ok(batch) => {
                    if attempt > 1 {
                        tracing::info!(
                            attempt,
                            "publish_many succeeded after concurrent-update retry"
                        );
                    }
                    return Ok(batch);
                }
                Err(error) if is_branch_concurrently_updated(&error) && attempt < MAX_ATTEMPTS => {
                    tracing::warn!(
                        attempt,
                        ?error,
                        "branch advanced concurrently; retrying publish_many from fresh HEAD"
                    );
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!(
            "publish_many retry loop is bounded by MAX_ATTEMPTS and every arm returns on \
             the last attempt; reaching this point implies the retry guard semantics were \
             broken"
        )
    }
}

fn is_branch_concurrently_updated(error: &PublishError) -> bool {
    match error {
        PublishError::GitHubApiError {
            status: 422,
            message,
        } => is_non_fast_forward_message(message),
        _ => false,
    }
}

fn is_non_fast_forward_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("fast forward") || lower.contains("not a fast-forward")
}

impl GitHubTarget {
    async fn publish_many_atomic(
        &self,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        let mut tree_entries = Vec::with_capacity(reports.len());
        let mut artifacts = Vec::with_capacity(reports.len());
        let mut expected_blobs = Vec::with_capacity(reports.len());
        for report in reports {
            let final_path = self.join_path(&report.relative_path)?;
            expected_blobs.push((
                final_path.clone(),
                git_blob_sha(report.markdown_content.as_bytes()),
            ));
            tree_entries.push(json!({
                "path": final_path,
                "mode": "100644",
                "type": "blob",
                "content": report.markdown_content,
            }));
            artifacts.push(PublishedArtifact {
                local_path: None,
                commit_sha: None,
                remote_target: Some(self.remote_target_url(&final_path)),
            });
        }

        let (head_commit_sha, base_tree_sha) = self.head_commit_and_tree().await?;
        // Idempotent retry (B9): if an earlier attempt's ref update landed but
        // its response was lost, HEAD already holds every report byte for
        // byte. Record receipts against that commit instead of stacking a
        // second commit on top of it.
        if self
            .head_contains_blobs(&head_commit_sha, &expected_blobs)
            .await?
        {
            for artifact in &mut artifacts {
                artifact.commit_sha = Some(head_commit_sha.clone());
            }
            return Ok(PublishedBatchArtifact {
                artifacts,
                commit_sha: Some(head_commit_sha),
            });
        }
        let tree_sha = self.create_tree(&base_tree_sha, tree_entries).await?;
        let commit_message = format!(
            "{} {} reports",
            self.cfg.commit_message_prefix,
            reports.len()
        );
        let commit_sha = self
            .create_commit(&commit_message, &tree_sha, &head_commit_sha)
            .await?;
        self.update_branch_ref(&commit_sha).await?;

        for artifact in &mut artifacts {
            artifact.commit_sha = Some(commit_sha.clone());
        }
        Ok(PublishedBatchArtifact {
            artifacts,
            commit_sha: Some(commit_sha),
        })
    }

    async fn get_json(&self, operation: &'static str, route: &str) -> Result<Value, PublishError> {
        let response = self.client._get(route).await;
        self.json_response(operation, response).await
    }

    async fn post_json(
        &self,
        operation: &'static str,
        route: &str,
        body: &Value,
    ) -> Result<Value, PublishError> {
        let response = self.client._post(route, Some(body)).await;
        self.json_response(operation, response).await
    }

    async fn patch_json(
        &self,
        operation: &'static str,
        route: &str,
        body: &Value,
    ) -> Result<Value, PublishError> {
        let response = self.client._patch(route, Some(body)).await;
        self.json_response(operation, response).await
    }

    async fn json_response(
        &self,
        operation: &'static str,
        response: Result<GitHubResponse, octocrab::Error>,
    ) -> Result<Value, PublishError> {
        let (status, body, request_id) = self.finish(operation, response).await?;
        parse_json_value(status, &body)
            .map_err(|error| with_request_context(error, operation, request_id.as_deref()))
    }

    /// Reads and classifies one GitHub REST response. Every failure (transport,
    /// oversized body, non-2xx) carries `operation` and the response's
    /// `x-github-request-id`, so a persisted publish failure says which call
    /// failed and can be traced on GitHub's side (incident 2026-11-21).
    async fn finish(
        &self,
        operation: &'static str,
        response: Result<GitHubResponse, octocrab::Error>,
    ) -> Result<(u16, Bytes, Option<String>), PublishError> {
        let response = response.map_err(|error| {
            with_request_context(classify::classify_octocrab_error(error), operation, None)
        })?;
        let status = response.status().as_u16();
        let rate_limit = classify::RateLimitHints::from_headers(response.headers());
        let request_id = github_request_id(response.headers());
        let context = |error| with_request_context(error, operation, request_id.as_deref());
        let body = read_response_body(response).await.map_err(context)?;
        if !(200..300).contains(&status) {
            return Err(context(classify::classify_github_response(
                status,
                response_message(status, &body, self.cfg.token.expose_secret()),
                &rate_limit,
            )));
        }
        Ok((status, body, request_id))
    }

    async fn head_commit_and_tree(&self) -> Result<(String, String), PublishError> {
        let ref_route = format!(
            "/repos/{}/{}/git/ref/heads/{}",
            self.cfg.owner, self.cfg.repo, self.cfg.branch
        );
        let head = self.get_json("GET ref", &ref_route).await?;
        let head_commit_sha = head
            .get("object")
            .and_then(|object| object.get("sha"))
            .and_then(|sha| sha.as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| PublishError::GitHubApiError {
                status: 502,
                message: "missing object.sha in GitHub ref response".to_string(),
            })?;

        let commit_route = format!(
            "/repos/{}/{}/git/commits/{}",
            self.cfg.owner, self.cfg.repo, head_commit_sha
        );
        let commit = self.get_json("GET commit", &commit_route).await?;
        let base_tree_sha = commit
            .get("tree")
            .and_then(|tree| tree.get("sha"))
            .and_then(|sha| sha.as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| PublishError::GitHubApiError {
                status: 502,
                message: "missing tree.sha in GitHub commit response".to_string(),
            })?;
        Ok((head_commit_sha, base_tree_sha))
    }

    async fn create_tree(
        &self,
        base_tree_sha: &str,
        tree_entries: Vec<Value>,
    ) -> Result<String, PublishError> {
        let route = format!("/repos/{}/{}/git/trees", self.cfg.owner, self.cfg.repo);
        let mut body = json!({ "base_tree": base_tree_sha });
        body["tree"] = Value::Array(tree_entries);
        let value = self.post_json("POST tree", &route, &body).await?;
        value
            .get("sha")
            .and_then(|sha| sha.as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| PublishError::GitHubApiError {
                status: 502,
                message: "missing sha in GitHub tree response".to_string(),
            })
    }

    async fn create_commit(
        &self,
        message: &str,
        tree_sha: &str,
        parent_commit_sha: &str,
    ) -> Result<String, PublishError> {
        let route = format!("/repos/{}/{}/git/commits", self.cfg.owner, self.cfg.repo);
        let body = json!({
            "message": message,
            "tree": tree_sha,
            "parents": [parent_commit_sha],
        });
        let value = self.post_json("POST commit", &route, &body).await?;
        value
            .get("sha")
            .and_then(|sha| sha.as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| PublishError::GitHubApiError {
                status: 502,
                message: "missing sha in GitHub commit response".to_string(),
            })
    }

    async fn update_branch_ref(&self, commit_sha: &str) -> Result<(), PublishError> {
        let route = format!(
            "/repos/{}/{}/git/refs/heads/{}",
            self.cfg.owner, self.cfg.repo, self.cfg.branch
        );
        let body = json!({
            "sha": commit_sha,
            "force": false,
        });
        self.patch_json("PATCH ref", &route, &body).await?;
        Ok(())
    }

    /// `/repos/{owner}/{repo}/contents/{path}` with each path segment
    /// percent-encoded once; `final_path` stays the logical repository path.
    fn contents_route(&self, final_path: &str) -> String {
        format!(
            "/repos/{}/{}/contents/{}",
            self.cfg.owner,
            self.cfg.repo,
            encode_repo_path(final_path)
        )
    }

    /// `true` only if every `(path, blob sha)` is present at `commit_sha` with
    /// exactly that blob. A missing path, a non-file entry or a differing blob
    /// means "not yet published"; a lookup error propagates (never read as
    /// "already there"). Queried at the fetched commit, not the branch name,
    /// so the answer refers to the same commit the receipts will name.
    async fn head_contains_blobs(
        &self,
        commit_sha: &str,
        expected: &[(String, String)],
    ) -> Result<bool, PublishError> {
        for (final_path, blob_sha) in expected {
            let found = self.existing_sha_at(final_path, commit_sha).await?;
            if found.as_deref() != Some(blob_sha.as_str()) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn existing_sha(&self, final_path: &str) -> Result<Option<String>, PublishError> {
        self.existing_sha_at(final_path, &self.cfg.branch).await
    }

    /// Blob sha of `final_path` at `git_ref` (branch or commit), `None` if the
    /// path does not exist there.
    async fn existing_sha_at(
        &self,
        final_path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, PublishError> {
        const OPERATION: &str = "GET contents";
        let route = format!(
            "{}?ref={}",
            self.contents_route(final_path),
            encode_component(git_ref)
        );
        let response = self.client._get(route.as_str()).await;
        if matches!(&response, Ok(response) if response.status().as_u16() == 404) {
            return Ok(None);
        }
        let (status, body, request_id) = self.finish(OPERATION, response).await?;

        let value = parse_json_value(status, &body)
            .map_err(|error| with_request_context(error, OPERATION, request_id.as_deref()))?;
        Ok(value
            .get("sha")
            .and_then(|sha| sha.as_str())
            .map(ToOwned::to_owned))
    }

    async fn put_file(
        &self,
        report: &RenderedReport,
        final_path: &str,
        existing_sha: Option<&str>,
    ) -> Result<String, PublishError> {
        let mut body = json!({
            "message": format!("{} {}", self.cfg.commit_message_prefix, report.relative_path),
            "branch": self.cfg.branch,
        });
        body["content"] = Value::String(STANDARD.encode(report.markdown_content.as_bytes()));
        if let Some(sha) = existing_sha {
            body["sha"] = Value::String(sha.to_owned());
        }

        const OPERATION: &str = "PUT contents";
        let route = self.contents_route(final_path);
        let response = self.client._put(route.as_str(), Some(&body)).await;
        let (status, body, request_id) = self.finish(OPERATION, response).await?;

        let value = parse_json_value(status, &body)
            .map_err(|error| with_request_context(error, OPERATION, request_id.as_deref()))?;
        value
            .get("commit")
            .and_then(|commit| commit.get("sha"))
            .and_then(|sha| sha.as_str())
            .map(ToOwned::to_owned)
            .ok_or_else(|| PublishError::GitHubApiError {
                status: 502,
                message: "missing commit.sha in GitHub response".to_string(),
            })
    }
}

type GitHubResponse = http::Response<BoxBody<Bytes, octocrab::Error>>;

/// Git's blob object id: SHA-1 over `"blob {len}\0"` followed by the bytes
/// (what `git hash-object` prints and what GitHub reports as a file's `sha`).
fn git_blob_sha(content: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", content.len()).as_bytes());
    hasher.update(content);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// RFC 3986 `unreserved` bytes stay literal; every other byte of the UTF-8
/// text (including `/`, space, `%`, `#`, `?`) becomes `%XX`. Applied exactly
/// once, to a logical (never pre-encoded) value.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Encodes each `/`-separated segment of a repository path, keeping the
/// separators. GitHub resolves the Contents API path segment by segment; a
/// literal UTF-8 or reserved character in the request target was rejected
/// with HTTP 400 in production (incident 2026-11-21).
fn encode_repo_path(path: &str) -> String {
    path.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// `x-github-request-id`, restricted to the characters GitHub uses so a
/// hostile intermediary cannot inject text into persisted diagnostics.
fn github_request_id(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get("x-github-request-id")?.to_str().ok()?;
    let id: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '-' | '_' | '.'))
        .take(128)
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Prefixes the (already sanitized) diagnostic with the failed operation and
/// appends GitHub's request id. Classification is unchanged: the variant and
/// status are kept, and the signals the retry logic matches on (`fast forward`,
/// `rate limit`) were evaluated before and survive the prefix.
fn with_request_context(
    error: PublishError,
    operation: &str,
    request_id: Option<&str>,
) -> PublishError {
    let annotate = |message: String| match request_id {
        Some(id) => format!("{operation}: {message} (x-github-request-id: {id})"),
        None => format!("{operation}: {message}"),
    };
    match error {
        PublishError::GitHubApiError { status, message } => PublishError::GitHubApiError {
            status,
            message: annotate(message),
        },
        PublishError::GitHubAuthFailed(message) => {
            PublishError::GitHubAuthFailed(annotate(message))
        }
        other => other,
    }
}

/// Includes base64 content returned by GitHub's Contents API. The cap is on
/// received body bytes and applies even without a Content-Length header.
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;

async fn read_response_body(response: GitHubResponse) -> Result<Bytes, PublishError> {
    let status = response.status().as_u16();
    let rate_limit = classify::RateLimitHints::from_headers(response.headers());
    let too_large = || {
        if (200..300).contains(&status) {
            PublishError::ResponseTooLarge {
                limit: MAX_RESPONSE_BODY_BYTES,
            }
        } else {
            // The oversized body is never read, so a 403 rate limit can only
            // be recognised from its headers here.
            classify::classify_github_response(
                status,
                format!("response body exceeded {MAX_RESPONSE_BODY_BYTES} bytes"),
                &rate_limit,
            )
        }
    };
    if response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        return Err(too_large());
    }
    Limited::new(response.into_body(), MAX_RESPONSE_BODY_BYTES)
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|error| {
            if error.is::<http_body_util::LengthLimitError>() {
                too_large()
            } else {
                PublishError::GitHubApiError {
                    status: 503,
                    message: format!("failed reading GitHub response: {error}"),
                }
            }
        })
}

fn validate_path_part(path: &str) -> Result<(), PublishError> {
    if path.is_empty() || path.contains("..") {
        return Err(PublishError::InvalidPath(path.to_string()));
    }
    let as_path = Path::new(path);
    if as_path.is_absolute()
        || as_path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(PublishError::InvalidPath(path.to_string()));
    }
    Ok(())
}

fn response_message(status: u16, body: &[u8], token: &str) -> String {
    let message = if let Ok(value) = serde_json::from_slice::<Value>(body) {
        value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                // Unknown JSON keys/values may contain escaped credentials.
                format!("github api returned status {status} with an unrecognized JSON error")
            })
    } else {
        let body = String::from_utf8_lossy(body);
        if status == 422 && is_non_fast_forward_message(&body.replace(token, "***")) {
            // Preserve only the retry signal, never the undecodable body.
            "not a fast-forward".to_string()
        } else {
            format!("github api returned status {status}; unreadable error body omitted")
        }
    };
    // Error JSON can encode the token with escapes. Scrub after decoding, before
    // this message reaches errors, tracing or persisted publish diagnostics.
    let mut safe = if token.is_empty() {
        message
    } else {
        message.replace(token, "***")
    };
    // The atomic publish retry path classifies 422 by message. Preserve its
    // semantic signal even when the original wording falls outside the prefix.
    if safe.len() > 8 * 1024 && status == 422 && is_non_fast_forward_message(&safe) {
        safe.insert_str(0, "not a fast-forward: ");
    }
    rss_ai_news_domain::error::truncate_diagnostic(safe, 8 * 1024)
}

fn parse_json_value(status: u16, body: &[u8]) -> Result<Value, PublishError> {
    serde_json::from_slice::<Value>(body).map_err(|error| PublishError::GitHubApiError {
        status: 502,
        message: format!("invalid GitHub response for status {status}: {error}"),
    })
}

pub mod classify {
    use octocrab::Error as OctocrabError;
    use time::{Duration, OffsetDateTime};

    use crate::error::PublishError;

    pub fn classify_octocrab_error(error: OctocrabError) -> PublishError {
        match error {
            OctocrabError::GitHub { source, .. } => {
                classify_github_status(source.status_code.as_u16(), source.message.clone(), None)
            }
            OctocrabError::Http { source, .. } => PublishError::GitHubApiError {
                status: 503,
                message: format!("network error: {source}"),
            },
            OctocrabError::Hyper { source, .. } => PublishError::GitHubApiError {
                status: 503,
                message: format!("network error: {source}"),
            },
            OctocrabError::Service { source, .. } => PublishError::GitHubApiError {
                status: 503,
                message: format!("network error: {source}"),
            },
            other => PublishError::GitHubApiError {
                status: 503,
                message: format!("octocrab error: {other}"),
            },
        }
    }

    /// Rate-limit headers of a GitHub REST response (GitHub docs, "Rate
    /// limits for the REST API" → "Exceeding the rate limit"): a primary limit
    /// answers 403/429 with `x-ratelimit-remaining: 0` and `x-ratelimit-reset`;
    /// a secondary limit may send `retry-after` (seconds).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct RateLimitHints {
        pub reset_epoch_seconds: Option<i64>,
        pub remaining: Option<i64>,
        pub retry_after_seconds: Option<i64>,
    }

    impl RateLimitHints {
        pub fn from_headers(headers: &http::HeaderMap) -> Self {
            let number = |name: &str| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<i64>().ok())
            };
            Self {
                reset_epoch_seconds: number("x-ratelimit-reset"),
                remaining: number("x-ratelimit-remaining"),
                retry_after_seconds: number("retry-after"),
            }
        }

        fn reset_at(&self) -> OffsetDateTime {
            let now = OffsetDateTime::now_utc();
            self.retry_after_seconds
                .filter(|seconds| *seconds >= 0)
                .map(|seconds| now + Duration::seconds(seconds))
                .or_else(|| {
                    self.reset_epoch_seconds
                        .and_then(|timestamp| OffsetDateTime::from_unix_timestamp(timestamp).ok())
                })
                .unwrap_or_else(|| now + Duration::seconds(60))
        }
    }

    /// A 403 is a rate limit (retryable) rather than a permission failure when
    /// the quota is exhausted, a `retry-after` is present, or GitHub's message
    /// names a (secondary) rate limit.
    fn is_rate_limited_403(message: &str, hints: &RateLimitHints) -> bool {
        hints.remaining == Some(0)
            || hints.retry_after_seconds.is_some()
            || message.to_ascii_lowercase().contains("rate limit")
    }

    pub fn classify_github_response(
        status: u16,
        message: String,
        hints: &RateLimitHints,
    ) -> PublishError {
        match status {
            403 if is_rate_limited_403(&message, hints) => PublishError::GitHubRateLimit {
                reset_at: hints.reset_at(),
            },
            401 | 403 => PublishError::GitHubAuthFailed(message),
            429 => PublishError::GitHubRateLimit {
                reset_at: hints.reset_at(),
            },
            _ => PublishError::GitHubApiError { status, message },
        }
    }

    /// Status + message + `x-ratelimit-reset` only; see
    /// [`classify_github_response`] for the header-aware variant.
    pub fn classify_github_status(
        status: u16,
        message: String,
        reset_epoch_seconds: Option<i64>,
    ) -> PublishError {
        classify_github_response(
            status,
            message,
            &RateLimitHints {
                reset_epoch_seconds,
                ..RateLimitHints::default()
            },
        )
    }
}

pub use classify::classify_octocrab_error;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_cap_preserves_branch_conflict_classification() {
        for (tail, expected) in [("not a fast-forward", true), ("validation failed", false)] {
            let body = format!("{} {tail}", "x".repeat(20_000));
            let message = response_message(422, body.as_bytes(), "test-token");
            assert!(message.len() <= 8 * 1024);
            let error = classify::classify_github_status(422, message, None);
            assert_eq!(is_branch_concurrently_updated(&error), expected);
        }
    }

    #[test]
    fn git_blob_sha_matches_git_hash_object() {
        // `printf '<content>' | git hash-object --stdin`
        for (content, expected) in [
            ("", "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"),
            ("# hello\n", "8954bb97349bfe2a7799e6a7a64c6f747c635d6c"),
            ("# 科技新闻\n", "e4762c1311ca20160f4ae15f8732880b5162e5b5"),
        ] {
            assert_eq!(git_blob_sha(content.as_bytes()), expected, "{content:?}");
        }
    }

    #[tokio::test]
    async fn target_identity_uses_the_publish_record_remote_target_format() {
        let target = GitHubTarget::new(GitHubTargetConfig {
            token: SecretString::from("token"),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: "main".to_string(),
            path_prefix: "archive".to_string(),
            commit_message_prefix: "rss-ai-news".to_string(),
        })
        .expect("target");
        // Must equal what `publish` init stores in `publish_records.remote_target`.
        assert_eq!(target.target_identity().as_deref(), Some("owner/repo:main"));
    }

    #[test]
    fn debug_redacts_token_to_prevent_log_leakage() {
        // Regression guard for F4-2 (manual Debug redact) and W2-A2
        // (token typed as SecretString end-to-end). SecretString's own
        // Debug impl prints `SecretString("***")`, so the derived Debug
        // on GitHubTargetConfig already redacts the PAT — without
        // SecretString this struct used to be `#[derive(Debug)]` over a
        // raw `String`, leaking the token into tracing events / panic
        // messages.
        let secret = "ghp-extremely-secret-personal-access-token-1234567890";
        let cfg = GitHubTargetConfig {
            token: SecretString::from(secret),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            branch: "main".to_string(),
            path_prefix: "archive".to_string(),
            commit_message_prefix: "rss-ai-news".to_string(),
        };
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains(secret),
            "Debug must not leak GitHub token: {rendered}"
        );
        assert!(rendered.contains("***"));
        assert!(rendered.contains("owner"));
        assert!(rendered.contains("repo"));
    }

    #[tokio::test]
    async fn body_limit_without_content_length_preserves_status_semantics() {
        use rss_ai_news_domain::error::ClassifiedError;
        let data = Bytes::from(vec![b'x'; MAX_RESPONSE_BODY_BYTES + 1]);
        for (status, kind, retryable) in [
            (200, "response_too_large", false),
            (429, "github_rate_limit", true),
            (503, "github_api_error", true),
        ] {
            let body = http_body_util::Full::new(data.clone())
                .map_err(|never| match never {})
                .boxed();
            let response = http::Response::builder()
                .status(status)
                .body(body)
                .expect("response");
            assert!(
                !response
                    .headers()
                    .contains_key(http::header::CONTENT_LENGTH)
            );
            let error = match read_response_body(response).await {
                Err(error) => error,
                Ok(_) => panic!("oversized body must be rejected"),
            };
            assert_eq!(error.error_kind(), kind);
            assert_eq!(error.is_retryable(), retryable);
            assert!(format!("{error:?}").len() < 200);
        }
    }

    #[tokio::test]
    async fn body_at_exact_limit_is_accepted_without_copying_to_string() {
        let body = http_body_util::Full::new(Bytes::from(vec![b' '; MAX_RESPONSE_BODY_BYTES]))
            .map_err(|never| match never {})
            .boxed();
        let response = http::Response::new(body);
        assert_eq!(
            read_response_body(response)
                .await
                .expect("exact limit")
                .len(),
            MAX_RESPONSE_BODY_BYTES
        );
    }
}
