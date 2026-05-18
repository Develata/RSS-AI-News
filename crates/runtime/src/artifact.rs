use rss_ai_news_config::{ArtifactConfig, RetentionPolicy};
use rss_ai_news_storage::{NewRawArtifact, RawArtifactRepository, StorageError};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

/// Artifact writer：把已经 sanitize 的字节负载落到 `raw_artifact` 表
/// （inline 路径）。
///
/// **W3-4 / F7-2 不变量**（详见 docs/design/error-and-observability.md §4.2
/// "日志/artifact 不变量"）：
///
/// - `write_inline` 的 `body` 参数**只能是响应体或已 sanitize 的内容**，
///   不得是 HTTP 请求体或任何含 `Authorization` / `Cookie` 等头部的原始
///   字节流。
/// - 当前三处生产调用均符合该约束：
///   - `flows/ingest.rs` 写 `feed_payload`（feed XML response body）
///   - `flows/extract.rs` 写 `html_payload`（article HTML response body）
///   - `flows/ai_run.rs` 写 `ai_raw_response`（LLM JSON response body）
/// - 若未来扩展写入请求体（如调试场景下保留 outgoing request 用于
///   replay），调用方必须先剥离 secret-bearing headers。
///
/// `ArtifactConfig.file_storage_dir` / `inline_threshold_bytes` 配置项当前
/// 未被任何代码消费——v0.1.0 仅实装 inline 路径（artifact 全部走数据库，
/// SQLite BLOB / PG BYTEA）。该字段为 v0.2 large-payload 外置存储预留；
/// schema CHECK 约束已支持 `storage_kind='file'`，未来切换不需要 migration。
/// 详见 [`docs/design/replay-and-artifacts.md`] §2.3。
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

    /// 写入 inline artifact。详见 [`ArtifactWriter`] 文档中关于 W3-4 / F7-2
    /// 不变量的说明：`body` 必须是已 sanitize 的内容。
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
