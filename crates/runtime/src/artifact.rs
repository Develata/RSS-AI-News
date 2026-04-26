use rss_ai_news_config::{ArtifactConfig, RetentionPolicy};
use rss_ai_news_storage::{NewRawArtifact, RawArtifactRepository, StorageError};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

pub struct ArtifactWriter<'a> {
    pub config: &'a ArtifactConfig,
    pub repo: &'a dyn RawArtifactRepository,
}

impl<'a> ArtifactWriter<'a> {
    pub fn should_write(&self, on_failure: bool) -> bool {
        match self.config.retention_policy {
            RetentionPolicy::Always => true,
            RetentionPolicy::OnFailure => on_failure,
            RetentionPolicy::Sampled => fastrand::f32() < self.config.sample_rate,
            RetentionPolicy::DebugOnly | RetentionPolicy::Off => false,
        }
    }

    pub async fn write_inline(
        &self,
        kind: &str,
        artifact_key: &str,
        body: &[u8],
    ) -> Result<i64, StorageError> {
        let mut hasher = Sha256::new();
        hasher.update(body);
        let sha256 = hex::encode(hasher.finalize());
        let now = OffsetDateTime::now_utc();
        let expires_at = if self.config.ttl_days > 0 {
            Some(now + Duration::days(i64::from(self.config.ttl_days)))
        } else {
            None
        };

        self.repo
            .upsert_inline(&NewRawArtifact {
                kind: kind.to_string(),
                artifact_key: artifact_key.to_string(),
                content_encoding: "utf-8".to_string(),
                inline_body: body.to_vec(),
                byte_size: body.len() as i64,
                sha256,
                retention_policy: retention_policy_str(self.config.retention_policy).to_string(),
                expires_at,
            })
            .await
    }
}

fn retention_policy_str(policy: RetentionPolicy) -> &'static str {
    match policy {
        RetentionPolicy::Always => "always",
        RetentionPolicy::OnFailure => "on_failure",
        RetentionPolicy::Sampled => "sampled",
        RetentionPolicy::DebugOnly => "debug_only",
        RetentionPolicy::Off => "off",
    }
}
