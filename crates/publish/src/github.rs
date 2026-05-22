use std::path::{Component, Path};
use std::sync::Arc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
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

    fn remote_target_url(&self, final_path: &str) -> String {
        format!(
            "github://{}/{}/{}/{}",
            self.cfg.owner, self.cfg.repo, self.cfg.branch, final_path
        )
    }
}

#[async_trait]
impl PublishTarget for GitHubTarget {
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
        } => {
            let lower = message.to_lowercase();
            lower.contains("fast forward") || lower.contains("not a fast-forward")
        }
        _ => false,
    }
}

impl GitHubTarget {
    async fn publish_many_atomic(
        &self,
        reports: &[RenderedReport],
    ) -> Result<PublishedBatchArtifact, PublishError> {
        let mut tree_entries = Vec::with_capacity(reports.len());
        let mut artifacts = Vec::with_capacity(reports.len());
        for report in reports {
            let final_path = self.join_path(&report.relative_path)?;
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

    async fn get_json(&self, route: &str) -> Result<Value, PublishError> {
        let response = self
            .client
            ._get(route)
            .await
            .map_err(classify::classify_octocrab_error)?;
        let status = response.status().as_u16();
        let reset_epoch = response
            .headers()
            .get("X-RateLimit-Reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok());
        let body = self
            .client
            .body_to_string(response)
            .await
            .map_err(classify::classify_octocrab_error)?;

        if !(200..300).contains(&status) {
            return Err(classify::classify_github_status(
                status,
                response_message(status, &body),
                reset_epoch,
            ));
        }

        parse_json_value(status, &body)
    }

    async fn post_json(&self, route: &str, body: &Value) -> Result<Value, PublishError> {
        let response = self
            .client
            ._post(route, Some(body))
            .await
            .map_err(classify::classify_octocrab_error)?;
        let status = response.status().as_u16();
        let reset_epoch = response
            .headers()
            .get("X-RateLimit-Reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok());
        let body = self
            .client
            .body_to_string(response)
            .await
            .map_err(classify::classify_octocrab_error)?;

        if !(200..300).contains(&status) {
            return Err(classify::classify_github_status(
                status,
                response_message(status, &body),
                reset_epoch,
            ));
        }

        parse_json_value(status, &body)
    }

    async fn patch_json(&self, route: &str, body: &Value) -> Result<Value, PublishError> {
        let response = self
            .client
            ._patch(route, Some(body))
            .await
            .map_err(classify::classify_octocrab_error)?;
        let status = response.status().as_u16();
        let reset_epoch = response
            .headers()
            .get("X-RateLimit-Reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok());
        let body = self
            .client
            .body_to_string(response)
            .await
            .map_err(classify::classify_octocrab_error)?;

        if !(200..300).contains(&status) {
            return Err(classify::classify_github_status(
                status,
                response_message(status, &body),
                reset_epoch,
            ));
        }

        parse_json_value(status, &body)
    }

    async fn head_commit_and_tree(&self) -> Result<(String, String), PublishError> {
        let ref_route = format!(
            "/repos/{}/{}/git/ref/heads/{}",
            self.cfg.owner, self.cfg.repo, self.cfg.branch
        );
        let head = self.get_json(&ref_route).await?;
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
        let commit = self.get_json(&commit_route).await?;
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
        let body = json!({
            "base_tree": base_tree_sha,
            "tree": tree_entries,
        });
        let value = self.post_json(&route, &body).await?;
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
        let value = self.post_json(&route, &body).await?;
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
        self.patch_json(&route, &body).await?;
        Ok(())
    }

    async fn existing_sha(&self, final_path: &str) -> Result<Option<String>, PublishError> {
        let route = format!(
            "/repos/{}/{}/contents/{}?ref={}",
            self.cfg.owner, self.cfg.repo, final_path, self.cfg.branch
        );
        let response = self
            .client
            ._get(route.as_str())
            .await
            .map_err(classify::classify_octocrab_error)?;
        let status = response.status().as_u16();
        let reset_epoch = response
            .headers()
            .get("X-RateLimit-Reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok());
        let body = self
            .client
            .body_to_string(response)
            .await
            .map_err(classify::classify_octocrab_error)?;

        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(classify::classify_github_status(
                status,
                response_message(status, &body),
                reset_epoch,
            ));
        }

        let value = parse_json_value(status, &body)?;
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
            "content": STANDARD.encode(report.markdown_content.as_bytes()),
            "branch": self.cfg.branch,
        });
        if let Some(sha) = existing_sha {
            body["sha"] = Value::String(sha.to_owned());
        }

        let route = format!(
            "/repos/{}/{}/contents/{}",
            self.cfg.owner, self.cfg.repo, final_path
        );
        let response = self
            .client
            ._put(route.as_str(), Some(&body))
            .await
            .map_err(classify::classify_octocrab_error)?;
        let status = response.status().as_u16();
        let reset_epoch = response
            .headers()
            .get("X-RateLimit-Reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok());
        let body = self
            .client
            .body_to_string(response)
            .await
            .map_err(classify::classify_octocrab_error)?;

        if !(200..300).contains(&status) {
            return Err(classify::classify_github_status(
                status,
                response_message(status, &body),
                reset_epoch,
            ));
        }

        let value = parse_json_value(status, &body)?;
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

fn response_message(status: u16, body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && let Some(message) = value.get("message").and_then(|message| message.as_str())
    {
        return message.to_string();
    }
    if body.trim().is_empty() {
        format!("github api returned status {status}")
    } else {
        body.to_string()
    }
}

fn parse_json_value(status: u16, body: &str) -> Result<Value, PublishError> {
    serde_json::from_str::<Value>(body).map_err(|error| PublishError::GitHubApiError {
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

    pub fn classify_github_status(
        status: u16,
        message: String,
        reset_epoch_seconds: Option<i64>,
    ) -> PublishError {
        match status {
            401 | 403 => PublishError::GitHubAuthFailed(message),
            429 => PublishError::GitHubRateLimit {
                reset_at: reset_epoch_seconds
                    .and_then(|timestamp| OffsetDateTime::from_unix_timestamp(timestamp).ok())
                    .unwrap_or_else(|| OffsetDateTime::now_utc() + Duration::seconds(60)),
            },
            _ => PublishError::GitHubApiError { status, message },
        }
    }
}

pub use classify::classify_octocrab_error;

#[cfg(test)]
mod tests {
    use super::*;

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
}
