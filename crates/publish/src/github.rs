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

use crate::error::{GitHubRequestContext, PublishError};
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
            .map_err(|error| PublishError::auth(format!("octocrab build: {error}")))?;
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
            .map_err(|error| PublishError::auth(format!("octocrab base_uri: {error}")))?
            .build()
            .map_err(|error| PublishError::auth(format!("octocrab build: {error}")))?;
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
        // R1-P-S2: the branch already holds this exact blob (an earlier
        // attempt landed, or a batch whose other receipts were recorded is
        // resumed with this one report): confirm it at a fixed HEAD commit and
        // record that commit instead of a PUT. A mismatch at HEAD (the branch
        // moved in between) falls through to the normal PUT.
        if existing_sha.as_deref()
            == Some(git_blob_sha(report.markdown_content.as_bytes()).as_str())
            && let Some(mut batch) = self.find_published(std::slice::from_ref(report)).await?
        {
            return Ok(batch.artifacts.remove(0));
        }
        match self
            .put_file(report, &final_path, existing_sha.as_deref())
            .await
        {
            Ok(commit_sha) => Ok(PublishedArtifact {
                local_path: None,
                commit_sha: Some(commit_sha),
                remote_target: Some(self.remote_target_url(&final_path)),
            }),
            Err(error) => {
                let mut batch = self
                    .recover_ambiguous_write(error, std::slice::from_ref(report))
                    .await?;
                Ok(batch.artifacts.remove(0))
            }
        }
    }

    async fn find_published(
        &self,
        reports: &[RenderedReport],
    ) -> Result<Option<PublishedBatchArtifact>, PublishError> {
        if reports.is_empty() {
            return Ok(None);
        }
        let (expected_blobs, artifacts) = self.expected_blobs(reports)?;
        let head_commit_sha = self.head_commit_sha().await?;
        if self
            .head_contains_blobs(&head_commit_sha, &expected_blobs)
            .await?
        {
            Ok(Some(receipts_at(artifacts, head_commit_sha)))
        } else {
            Ok(None)
        }
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
            ..
        } => is_non_fast_forward_message(message),
        _ => false,
    }
}

fn is_non_fast_forward_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("fast forward") || lower.contains("not a fast-forward")
}

/// A failed write request (PUT contents / PATCH ref) whose effect on GitHub is
/// unknown: a transport failure or an unreadable response (both mapped to
/// 503 by [`classify`]) or a 5xx answer may follow a write the server already
/// applied. A 4xx (including 409 and 422) means the write was refused.
fn is_ambiguous_write_failure(error: &PublishError) -> bool {
    matches!(error, PublishError::GitHubApiError { status, .. } if *status >= 500)
}

/// Stamps every artifact with the commit that holds the batch.
fn receipts_at(
    mut artifacts: Vec<PublishedArtifact>,
    commit_sha: String,
) -> PublishedBatchArtifact {
    for artifact in &mut artifacts {
        artifact.commit_sha = Some(commit_sha.clone());
    }
    PublishedBatchArtifact {
        artifacts,
        commit_sha: Some(commit_sha),
    }
}

impl GitHubTarget {
    /// `(final path, local git blob sha)` per report plus artifacts without a
    /// commit yet, both in input order.
    #[allow(clippy::type_complexity)]
    fn expected_blobs(
        &self,
        reports: &[RenderedReport],
    ) -> Result<(Vec<(String, String)>, Vec<PublishedArtifact>), PublishError> {
        let mut expected = Vec::with_capacity(reports.len());
        let mut artifacts = Vec::with_capacity(reports.len());
        for report in reports {
            let final_path = self.join_path(&report.relative_path)?;
            artifacts.push(PublishedArtifact {
                local_path: None,
                commit_sha: None,
                remote_target: Some(self.remote_target_url(&final_path)),
            });
            expected.push((final_path, git_blob_sha(report.markdown_content.as_bytes())));
        }
        Ok((expected, artifacts))
    }

    /// P-B2: after an ambiguous write failure, a read-only check of the
    /// branch HEAD decides. HEAD holding every report byte for byte means the
    /// write landed (or the content was already there): record receipts
    /// against that commit. Otherwise — or if the check itself fails — the
    /// original error stands and the normal release applies.
    async fn recover_ambiguous_write(
        &self,
        error: PublishError,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        if !is_ambiguous_write_failure(&error) {
            return Err(error);
        }
        match self.find_published(reports).await {
            Ok(Some(batch)) => {
                tracing::info!(
                    reports = reports.len(),
                    commit_sha = batch.commit_sha.as_deref(),
                    "ambiguous GitHub write verified as landed by a HEAD content check"
                );
                Ok(batch)
            }
            Ok(None) => Err(error),
            Err(check_error) => {
                tracing::warn!(
                    ?check_error,
                    "HEAD content check after an ambiguous GitHub write failed; keeping the write error"
                );
                Err(error)
            }
        }
    }

    async fn publish_many_atomic(
        &self,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        let (expected_blobs, artifacts) = self.expected_blobs(reports)?;
        let tree_entries = expected_blobs
            .iter()
            .zip(reports)
            .map(|((final_path, _), report)| {
                json!({
                    "path": final_path,
                    "mode": "100644",
                    "type": "blob",
                    "content": report.markdown_content,
                })
            })
            .collect::<Vec<_>>();

        let (head_commit_sha, base_tree_sha) = self.head_commit_and_tree().await?;
        // Idempotent retry (B9): if an earlier attempt's ref update landed but
        // its response was lost, HEAD already holds every report byte for
        // byte. Record receipts against that commit instead of stacking a
        // second commit on top of it.
        if self
            .head_contains_blobs(&head_commit_sha, &expected_blobs)
            .await?
        {
            return Ok(receipts_at(artifacts, head_commit_sha));
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
        if let Err(error) = self.update_branch_ref(&commit_sha).await {
            return self.recover_ambiguous_write(error, reports).await;
        }
        Ok(receipts_at(artifacts, commit_sha))
    }

    async fn get_json(
        &self,
        operation: &'static str,
        route: &str,
    ) -> Result<Responded, PublishError> {
        let response = self.client._get(route).await;
        self.json_response(operation, response).await
    }

    async fn post_json(
        &self,
        operation: &'static str,
        route: &str,
        body: &Value,
    ) -> Result<Responded, PublishError> {
        let response = self.client._post(route, Some(body)).await;
        self.json_response(operation, response).await
    }

    async fn patch_json(
        &self,
        operation: &'static str,
        route: &str,
        body: &Value,
    ) -> Result<Responded, PublishError> {
        let response = self.client._patch(route, Some(body)).await;
        self.json_response(operation, response).await
    }

    async fn json_response(
        &self,
        operation: &'static str,
        response: Result<GitHubResponse, octocrab::Error>,
    ) -> Result<Responded, PublishError> {
        let (body, context) = self.finish(operation, response).await?;
        let value = parse_json_value(&body, &context)?;
        Ok((value, context))
    }

    /// Reads and classifies one GitHub REST response. Every failure (transport,
    /// oversized body, non-2xx) and the returned success carry the structured
    /// request context — operation, answered HTTP status, validated
    /// `x-github-request-id` — so a persisted publish failure says which call
    /// failed and can be traced on GitHub's side (incident 2026-11-21,
    /// R1-P-S1).
    async fn finish(
        &self,
        operation: &'static str,
        response: Result<GitHubResponse, octocrab::Error>,
    ) -> Result<(Bytes, GitHubRequestContext), PublishError> {
        let response = response.map_err(|error| {
            with_request_context(
                classify::classify_octocrab_error(error),
                &GitHubRequestContext {
                    operation: Some(operation),
                    ..GitHubRequestContext::default()
                },
            )
        })?;
        let status = response.status().as_u16();
        let rate_limit = classify::RateLimitHints::from_headers(response.headers());
        let context = GitHubRequestContext {
            operation: Some(operation),
            http_status: Some(status),
            request_id: github_request_id(response.headers(), self.cfg.token.expose_secret()),
        };
        let body = read_response_body(response)
            .await
            .map_err(|error| with_request_context(error, &context))?;
        if !(200..300).contains(&status) {
            return Err(with_request_context(
                classify::classify_github_response(
                    status,
                    response_reason(status, &body),
                    &rate_limit,
                ),
                &context,
            ));
        }
        Ok((body, context))
    }

    /// Commit sha the configured branch points at now.
    async fn head_commit_sha(&self) -> Result<String, PublishError> {
        let ref_route = format!(
            "/repos/{}/{}/git/ref/heads/{}",
            self.cfg.owner,
            self.cfg.repo,
            encode_repo_path(&self.cfg.branch)
        );
        let (head, context) = self.get_json("GET ref", &ref_route).await?;
        required_str(&head, "/object/sha", "object.sha", &context)
    }

    async fn head_commit_and_tree(&self) -> Result<(String, String), PublishError> {
        let head_commit_sha = self.head_commit_sha().await?;

        let commit_route = format!(
            "/repos/{}/{}/git/commits/{}",
            self.cfg.owner, self.cfg.repo, head_commit_sha
        );
        let (commit, context) = self.get_json("GET commit", &commit_route).await?;
        let base_tree_sha = required_str(&commit, "/tree/sha", "tree.sha", &context)?;
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
        let (value, context) = self.post_json("POST tree", &route, &body).await?;
        required_str(&value, "/sha", "sha", &context)
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
        let (value, context) = self.post_json("POST commit", &route, &body).await?;
        required_str(&value, "/sha", "sha", &context)
    }

    async fn update_branch_ref(&self, commit_sha: &str) -> Result<(), PublishError> {
        let route = format!(
            "/repos/{}/{}/git/refs/heads/{}",
            self.cfg.owner,
            self.cfg.repo,
            encode_repo_path(&self.cfg.branch)
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
        let (body, context) = self.finish(OPERATION, response).await?;
        let value = parse_json_value(&body, &context)?;
        // A 200 without a string `sha` (e.g. a directory listing) is a
        // protocol error, never "not there yet" (R1-P-S1).
        required_str(&value, "/sha", "sha", &context).map(Some)
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
        let (body, context) = self.finish(OPERATION, response).await?;
        let value = parse_json_value(&body, &context)?;
        required_str(&value, "/commit/sha", "commit.sha", &context)
    }
}

type GitHubResponse = http::Response<BoxBody<Bytes, octocrab::Error>>;

/// A successful GitHub JSON response and the request context it came with.
type Responded = (Value, GitHubRequestContext);

/// The string at JSON `pointer`, or a 502 protocol error naming the missing
/// field with the request context of the (2xx) response that lacked it.
fn required_str(
    value: &Value,
    pointer: &str,
    field: &str,
    context: &GitHubRequestContext,
) -> Result<String, PublishError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| PublishError::GitHubApiError {
            status: 502,
            message: format!("missing {field} in GitHub response"),
            context: context.clone(),
        })
}

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

/// Encodes each `/`-separated segment of a repository path or branch name,
/// keeping the separators. GitHub resolves the Contents API path and the Git
/// ref route segment by segment; a literal UTF-8 or reserved character in the
/// request target was rejected with HTTP 400 in production (incident
/// 2026-11-21), and an unencoded `#` / `?` in a branch would cut the route
/// short (R1-P-B3: `release#daily` became `release`).
fn encode_repo_path(path: &str) -> String {
    path.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// `x-github-request-id`, kept only when the whole value has GitHub's shape
/// and never when it contains the token (R1-P-B6). Observed ids are five
/// colon-separated groups of upper-case hex digits, e.g.
/// `828A:4A44E:40DA1B1:CEE6757:6B01CE59`; 2–8 groups of 1–16 hex digits are
/// accepted. Anything else (an intermediary's own id, injected text, a
/// token-shaped value) is dropped rather than filtered into shape.
fn github_request_id(headers: &http::HeaderMap, token: &str) -> Option<String> {
    let value = headers.get("x-github-request-id")?.to_str().ok()?.trim();
    let groups = value.split(':').collect::<Vec<_>>();
    let well_formed = value.len() <= 128
        && (2..=8).contains(&groups.len())
        && groups.iter().all(|group| {
            (1..=16).contains(&group.len()) && group.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    let carries_token = !token.is_empty() && value.contains(token);
    (well_formed && !carries_token).then(|| value.to_string())
}

/// Attaches the request context to a GitHub failure (fields already set are
/// kept). Classification is unchanged: the variant, status and message — and
/// with them the signals the retry logic matches on (`fast forward`, `rate
/// limit`) — were decided before.
fn with_request_context(mut error: PublishError, context: &GitHubRequestContext) -> PublishError {
    if let Some(target) = error.github_context_mut() {
        target.operation = target.operation.or(context.operation);
        target.http_status = target.http_status.or(context.http_status);
        if target.request_id.is_none() {
            target.request_id = context.request_id.clone();
        }
    }
    error
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
                context: GitHubRequestContext::default(),
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
                PublishError::api(
                    503,
                    rss_ai_news_domain::error::truncate_diagnostic(
                        format!("failed reading GitHub response: {error}"),
                        512,
                    ),
                )
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

/// The persisted reason of a GitHub error response (R1-P-B6). GitHub's
/// `message` (or an undecodable body) is free server text that may echo the
/// request — report fragments, their base64, a credential — so it is only
/// scanned, in full and in memory, for known phrases; what leaves this
/// function is a fixed vocabulary. The phrases the retry logic matches on
/// (`not a fast-forward`, `rate limit`) are part of that vocabulary, so the
/// 422 / 403 classification sees the same signal it would in the raw text,
/// wherever it sat in the body (no truncation happens before the scan).
fn response_reason(status: u16, body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => match value.get("message").and_then(Value::as_str) {
            Some(message) => summarize_github_message(message),
            // Unknown JSON keys/values may contain escaped credentials.
            None => format!("github api returned status {status} with an unrecognized JSON error"),
        },
        Err(_) => {
            let summary = summarize_github_message(&String::from_utf8_lossy(body));
            format!("unreadable error body: {summary}")
        }
    }
}

/// Known GitHub error phrases (matched case-insensitively as substrings of
/// the server text) and the fixed wording persisted for each.
const KNOWN_GITHUB_PHRASES: &[(&[&str], &str)] = &[
    (&["rate limit"], "rate limit exceeded"),
    (&["abuse"], "abuse detection"),
    (&["fast forward", "fast-forward"], "not a fast-forward"),
    (&["bad credentials"], "bad credentials"),
    (&["requires authentication"], "requires authentication"),
    (
        &["resource not accessible"],
        "resource not accessible by token",
    ),
    (&["permission"], "insufficient permission"),
    (
        &["protected branch", "branch protection"],
        "protected branch",
    ),
    (&["not found"], "not found"),
    (&["validation failed"], "validation failed"),
    (&["invalid request"], "invalid request"),
    (&["wasn't supplied", "was not supplied"], "sha not supplied"),
    (&["does not match"], "sha does not match"),
    (&["conflict"], "conflict"),
    (&["too large"], "too large"),
    (&["bad request"], "bad request"),
    (&["bad gateway"], "bad gateway"),
    (&["server error"], "server error"),
    (&["unavailable"], "service unavailable"),
    (&["timeout", "timed out"], "timeout"),
    (&["blocked"], "access blocked"),
];

/// Fixed-vocabulary summary of free GitHub text; never echoes the input.
fn summarize_github_message(message: &str) -> String {
    let lower = message.to_lowercase();
    let phrases = KNOWN_GITHUB_PHRASES
        .iter()
        .filter(|(needles, _)| needles.iter().any(|needle| lower.contains(needle)))
        .map(|(_, phrase)| *phrase)
        .collect::<Vec<_>>();
    if phrases.is_empty() {
        format!(
            "unrecognized GitHub message omitted ({} bytes)",
            message.len()
        )
    } else {
        phrases.join("; ")
    }
}

/// A 2xx body that is not JSON: 502 protocol error with the response context.
fn parse_json_value(body: &[u8], context: &GitHubRequestContext) -> Result<Value, PublishError> {
    serde_json::from_slice::<Value>(body).map_err(|error| PublishError::GitHubApiError {
        status: 502,
        message: format!("invalid GitHub JSON response: {error}"),
        context: context.clone(),
    })
}

pub mod classify {
    use octocrab::Error as OctocrabError;
    use time::{Duration, OffsetDateTime};

    use crate::error::{GitHubRequestContext, PublishError};

    pub fn classify_octocrab_error(error: OctocrabError) -> PublishError {
        match error {
            // `source.message` is free server text: persist only its summary.
            OctocrabError::GitHub { source, .. } => classify_github_status(
                source.status_code.as_u16(),
                super::summarize_github_message(&source.message),
                None,
            ),
            // Client-side errors (no server text); bounded all the same.
            OctocrabError::Http { source, .. } => network_error(format!("network error: {source}")),
            OctocrabError::Hyper { source, .. } => {
                network_error(format!("network error: {source}"))
            }
            OctocrabError::Service { source, .. } => {
                network_error(format!("network error: {source}"))
            }
            other => network_error(format!("octocrab error: {other}")),
        }
    }

    /// Transport-level failure: retryable 503 with a bounded client message.
    fn network_error(message: String) -> PublishError {
        PublishError::api(
            503,
            rss_ai_news_domain::error::truncate_diagnostic(message, 512),
        )
    }

    /// Rate-limit headers of a GitHub REST response (GitHub docs, "Rate
    /// limits for the REST API" → "Exceeding the rate limit"): a primary limit
    /// answers 403/429 with `x-ratelimit-remaining: 0` and `x-ratelimit-reset`;
    /// a secondary limit may send `retry-after`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct RateLimitHints {
        pub reset_epoch_seconds: Option<i64>,
        pub remaining: Option<i64>,
        /// `None` = no `retry-after` header at all.
        pub retry_after: Option<RetryAfter>,
    }

    /// A `retry-after` header (RFC 9110 §10.2.3): delay-seconds or an
    /// HTTP-date. Its presence alone marks a 403 as a rate limit (R1-P-S11),
    /// even when its value cannot be turned into a deadline.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RetryAfter {
        Seconds(u64),
        At(OffsetDateTime),
        Unparsable,
    }

    /// Longest server-requested wait honoured (a day). A larger or overflowing
    /// value is capped, never added unchecked to the clock.
    pub const MAX_RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;

    impl RetryAfter {
        pub fn parse(value: &str) -> Self {
            let value = value.trim();
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
                // More digits than u64 holds is still "very long".
                return Self::Seconds(value.parse().unwrap_or(u64::MAX));
            }
            parse_http_date(value).map_or(Self::Unparsable, Self::At)
        }
    }

    /// IMF-fixdate, the HTTP-date form senders must generate
    /// (`Sun, 06 Nov 1994 08:49:37 GMT`).
    fn parse_http_date(value: &str) -> Option<OffsetDateTime> {
        // Runtime-parsed description: the workspace `time` has no `macros`.
        let format = time::format_description::parse_borrowed::<2>(
            "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT",
        )
        .ok()?;
        time::PrimitiveDateTime::parse(value, &format)
            .ok()
            .map(time::PrimitiveDateTime::assume_utc)
    }

    impl RateLimitHints {
        pub fn from_headers(headers: &http::HeaderMap) -> Self {
            let text = |name: &str| headers.get(name).map(|value| value.to_str().unwrap_or(""));
            let number = |name: &str| text(name).and_then(|value| value.trim().parse::<i64>().ok());
            Self {
                reset_epoch_seconds: number("x-ratelimit-reset"),
                remaining: number("x-ratelimit-remaining"),
                retry_after: text("retry-after").map(RetryAfter::parse),
            }
        }

        /// Earliest retry time at `now`: `retry-after` (seconds capped at
        /// [`MAX_RETRY_AFTER_SECONDS`], a past date means now), then
        /// `x-ratelimit-reset`, then one minute.
        pub fn reset_at_from(&self, now: OffsetDateTime) -> OffsetDateTime {
            let from_retry_after = match self.retry_after {
                Some(RetryAfter::Seconds(seconds)) => {
                    let capped = seconds.min(MAX_RETRY_AFTER_SECONDS);
                    i64::try_from(capped)
                        .ok()
                        .and_then(|seconds| now.checked_add(Duration::seconds(seconds)))
                }
                Some(RetryAfter::At(at)) => Some(at.max(now)),
                Some(RetryAfter::Unparsable) | None => None,
            };
            from_retry_after
                .or_else(|| {
                    self.reset_epoch_seconds
                        .and_then(|timestamp| OffsetDateTime::from_unix_timestamp(timestamp).ok())
                })
                .or_else(|| now.checked_add(Duration::seconds(60)))
                .unwrap_or(now)
        }

        fn reset_at(&self) -> OffsetDateTime {
            self.reset_at_from(OffsetDateTime::now_utc())
        }
    }

    /// A 403 is a rate limit (retryable) rather than a permission failure when
    /// the quota is exhausted, a `retry-after` header is present (in either
    /// form, parsable or not), or GitHub's message names a (secondary) rate
    /// limit. `message` is the fixed-vocabulary reason, whose `rate limit`
    /// phrase was found anywhere in the full server text (R1-P-B6), so a
    /// signal past any truncation point is not lost.
    fn is_rate_limited_403(message: &str, hints: &RateLimitHints) -> bool {
        hints.remaining == Some(0)
            || hints.retry_after.is_some()
            || message.to_ascii_lowercase().contains("rate limit")
    }

    /// The answered `status` is kept as `context.http_status` on every
    /// variant (R1-P-S1): a 401 and a 403 auth failure stay distinguishable.
    pub fn classify_github_response(
        status: u16,
        message: String,
        hints: &RateLimitHints,
    ) -> PublishError {
        let context = GitHubRequestContext {
            http_status: Some(status),
            ..GitHubRequestContext::default()
        };
        match status {
            403 if is_rate_limited_403(&message, hints) => PublishError::GitHubRateLimit {
                reset_at: hints.reset_at(),
                context,
            },
            401 | 403 => PublishError::GitHubAuthFailed { message, context },
            429 => PublishError::GitHubRateLimit {
                reset_at: hints.reset_at(),
                context,
            },
            _ => PublishError::GitHubApiError {
                status,
                message,
                context,
            },
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
            for raw in [body.clone(), json!({ "message": body }).to_string()] {
                let message = response_reason(422, raw.as_bytes());
                assert!(message.len() <= 256, "{message}");
                let error = classify::classify_github_status(422, message, None);
                assert_eq!(is_branch_concurrently_updated(&error), expected);
            }
        }
    }

    /// R1-P-B6: free server text never reaches the persisted reason.
    #[test]
    fn response_reason_keeps_only_known_phrases_never_server_text() {
        let echo =
            "Bad request: path 科技新闻 body IyDnp5HmioDmlrDpl7sK REPORT-MARKER token ghp_secret";
        let reason = response_reason(400, json!({ "message": echo }).to_string().as_bytes());
        assert_eq!(reason, "bad request");
        let unknown = response_reason(
            400,
            json!({ "message": "REPORT-MARKER" }).to_string().as_bytes(),
        );
        assert!(!unknown.contains("REPORT-MARKER"), "{unknown}");
        assert!(unknown.contains("13 bytes"), "{unknown}");
        let text = response_reason(502, b"<html>REPORT-MARKER Bad Gateway</html>");
        assert_eq!(text, "unreadable error body: bad gateway");
    }

    #[test]
    fn request_id_is_kept_only_in_githubs_shape_and_never_with_the_token() {
        let id = |value: &str, token: &str| {
            let mut headers = http::HeaderMap::new();
            headers.insert("x-github-request-id", value.parse().unwrap());
            github_request_id(&headers, token)
        };
        assert_eq!(
            id("828A:4A44E:40DA1B1:CEE6757:6B01CE59", "ghp_x").as_deref(),
            Some("828A:4A44E:40DA1B1:CEE6757:6B01CE59")
        );
        for rejected in [
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "REQ-1 injected text",
            "828A",
            "828A::CEE6757",
            "0123456789ABCDEF0:1",
        ] {
            assert_eq!(id(rejected, "ghp_x"), None, "{rejected}");
        }
        // A hex token embedded in an otherwise well-formed id is dropped.
        assert_eq!(id("ABCD:1234DEADBEEF", "1234DEADBEEF"), None);
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
