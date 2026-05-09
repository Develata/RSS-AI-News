use std::path::{Component, Path};
use std::sync::Arc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use octocrab::Octocrab;
use rss_ai_news_domain::{SecretString, dto::publish::RenderedReport};
use serde_json::{Value, json};

use crate::error::PublishError;
use crate::target::{PublishTarget, PublishedArtifact};

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
}

impl GitHubTarget {
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
