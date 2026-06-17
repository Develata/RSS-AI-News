use std::{num::NonZeroU32, path::PathBuf};

use rss_ai_news_domain::Score0To100;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    pub schema_version: String,
    pub database: DatabaseConfig,
    pub http: HttpConfig,
    pub ai: AiConfig,
    pub publish: PublishConfig,
    pub dedup: DedupConfig,
    pub extractor: ExtractorConfig,
    pub lease: LeaseConfig,
    pub retry: RetryConfig,
    /// 单次 run 工作量边界。`#[serde(default)]` 使旧 `app.toml`
    /// （无 `[runtime]` 段）仍可解析；缺省值见 [config-schema §4.4](docs/design/config-schema.md#44-runtime-字段语义)。
    #[serde(default)]
    pub runtime: RuntimeConfig,
    pub artifact: ArtifactConfig,
    pub observability: ObservabilityConfig,
    /// `[doctor]` 段：诊断检查阈值（卡住的 reindex job / 静默 source /
    /// 待处理积压）。`#[serde(default)]` 使旧 `app.toml`（无 `[doctor]` 段）
    /// 仍可解析，缺省值见 [`DoctorConfig::default`]。
    #[serde(default)]
    pub doctor: DoctorConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseConfig {
    pub driver: DatabaseDriver,
    pub sqlite_path: PathBuf,
    pub max_connections: u32,
    pub busy_timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseDriver {
    Sqlite,
    Postgres,
}

#[derive(Clone, Debug, Deserialize)]
pub struct HttpConfig {
    pub user_agent: String,
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub retry_backoff_base_ms: u64,
    pub concurrent_feeds: u32,
    pub concurrent_fetches: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AiConfig {
    pub enabled: bool,
    pub model: String,
    /// 失败回退模型链（W14-A）。主模型调用失败且错误"换模型可能有救"时，在同一次
    /// 执行内按此顺序换 model 名重试。空 = 不回退（行为同历史）。元素经 effective 层
    /// trim / 去空白 / 与主模型去重。见 docs/plan/14-ai-fallback.md。
    #[serde(default)]
    pub fallback_models: Vec<String>,
    pub max_tokens: u32,
    pub temperature: f32,
    pub request_timeout_seconds: u64,
    pub max_input_chars: u32,
    pub rate_limit: AiRateLimitConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AiRateLimitConfig {
    pub requests_per_minute: u32,
    pub tokens_per_minute: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PublishConfig {
    pub target_timezone: String,
    pub github_owner: String,
    pub github_repo: String,
    pub github_branch: String,
    pub github_path_prefix: String,
    pub local_output_dir: PathBuf,
    pub template: PublishTemplateConfig,
    pub include_unscored: bool,
    /// Global default for `[publish] max_items_per_report`. Categories may
    /// override per-field via `[category.publish_override]`. NonZeroU32 makes
    /// `0` (which would silently mean SQL `LIMIT 0`) a toml deserialization
    /// error. See `docs/design/config-schema.md` §4.5 / §234.
    pub max_items_per_report: NonZeroU32,
    /// Global default for `[publish] min_importance_score`. AI path: articles
    /// with score < this are filtered before report selection. AI-off direct
    /// path: not applied (see §4.5). Score0To100 enforces the 0-100 invariant
    /// at toml deserialization. See `docs/design/config-schema.md` §357-358.
    pub min_importance_score: Score0To100,
    /// Sliding candidate window for publish selection, in hours. `0` disables
    /// the window for manual backfills.
    #[serde(default = "PublishConfig::default_candidate_window_hours")]
    pub candidate_window_hours: u32,
}

impl PublishConfig {
    const fn default_candidate_window_hours() -> u32 {
        48
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PublishTemplateConfig {
    pub path_template: String,
    pub frontmatter_template: String,
    pub report_template: String,
    pub item_template: String,
}

impl PublishTemplateConfig {
    pub fn default_path_template() -> String {
        "{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md".to_string()
    }

    pub fn default_frontmatter_template() -> String {
        "---\ntitle: {date}\ndate: {date}\nexcerpt: {excerpt_yaml}\n---\n".to_string()
    }

    pub fn default_report_template() -> String {
        "{frontmatter}\n# {title_md}\n{excerpt_block}\n{items}".to_string()
    }

    pub fn default_item_template() -> String {
        "## {item_title_md}{score_badge}\n\n{tags_block}- **Source:** `{source_code}` | [阅读原文]({url_md})\n\n> [摘要]  \n{summary_blockquote}\n\n---\n\n".to_string()
    }
}

impl Default for PublishTemplateConfig {
    fn default() -> Self {
        Self {
            path_template: Self::default_path_template(),
            frontmatter_template: Self::default_frontmatter_template(),
            report_template: Self::default_report_template(),
            item_template: Self::default_item_template(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DedupConfig {
    pub enable_link_dedup: bool,
    pub enable_content_dedup: bool,
    pub link_normalizer_version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExtractorConfig {
    pub strategy_order: Vec<String>,
    /// HTML 文章正文抓取的响应体上限（字节）。extract 阶段抓原文页时用，
    /// 用于把单篇文章占用的内存 / 带宽限定在合理范围。
    pub max_body_bytes: u64,
    /// Feed 下载的响应体上限（字节）。`None` = 沿用 [`Self::max_body_bytes`]
    /// （向后兼容：旧配置无此字段时行为完全不变）。
    ///
    /// 解耦动机：GitHub `releases.atom` 等把全文 release notes 嵌进 feed 的源，
    /// 体积远大于普通文章页，需要更高的 feed 上限；而该上限若与
    /// `max_body_bytes` 共用，会连带放大**每篇 HTML 抓取**的内存 / 带宽天花板。
    /// 拆开后可单独抬高 feed 上限而不波及 HTML 路径。
    #[serde(default)]
    pub feed_max_body_bytes: Option<u64>,
    pub min_body_chars: u32,
}

impl ExtractorConfig {
    /// Feed 下载体上限的有效值：显式配置优先，缺省回退到 `max_body_bytes`。
    /// 回退保证旧配置（无 `feed_max_body_bytes`）的 feed 抓取行为零变化。
    pub fn effective_feed_max_body_bytes(&self) -> u64 {
        self.feed_max_body_bytes.unwrap_or(self.max_body_bytes)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct LeaseConfig {
    pub fetch_duration_seconds: u64,
    pub ai_duration_seconds: u64,
    pub publish_duration_seconds: u64,
    pub reclaim_interval_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RetryConfig {
    pub feed_entry_max_attempts: u32,
    pub ai_max_attempts: u32,
    pub publish_max_attempts: u32,
}

/// `[runtime]` 段：单次 run 内部批次循环上限。
///
/// `max_batches_per_run` 在 ingest / ai-run 内部生效（每批 `--batch-size`
/// 行 × 上限批次 = 单次 run 处理上限）；`0` 表示不限，由 lease + 宿主超时兜底。
/// CLI `--max-batches` 覆盖此值。详见
/// [config-schema §4.4](docs/design/config-schema.md#44-runtime-字段语义)。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// 单次 run 内部批次循环上限。默认 10；`0` = 不限。
    #[serde(default = "RuntimeConfig::default_max_batches_per_run")]
    pub max_batches_per_run: u32,
}

impl RuntimeConfig {
    const fn default_max_batches_per_run() -> u32 {
        10
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_batches_per_run: Self::default_max_batches_per_run(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ArtifactConfig {
    pub retention_policy: RetentionPolicy,
    pub sample_rate: f32,
    pub inline_threshold_bytes: u64,
    pub file_storage_dir: PathBuf,
    pub ttl_days: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPolicy {
    Always,
    OnFailure,
    Sampled,
    DebugOnly,
    Off,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ObservabilityConfig {
    pub log_level: String,
    pub log_format: String,
    pub log_file: String,
    pub enable_metrics: bool,
    pub metrics_bind: String,
}

/// `[doctor]` 段：`doctor` 子命令的诊断阈值。这些是**运维调参**（非业务规则），
/// 全部 `#[serde(default)]`，缺省给出对一个日级新闻管线合理的保守值；按部署
/// 节奏在 `app.toml` 覆盖。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DoctorConfig {
    /// reindex job 停在 `pending` 超过此秒数仍未被 claim → 视为卡住（Warn）。
    /// 默认 3600（1h）：reindex 由调度/运维触发，pending 滞留意味着创建后
    /// 没有任何 reindex run 来认领（多半 run 在建 job 后崩溃）。
    pub stuck_reindex_pending_secs: u64,
    /// 活跃 source 距上次成功抓取超过此秒数 → 视为静默（Warn）。默认
    /// 86400（24h）：日级源超过一天没有任何成功抓取即可疑。
    pub silent_source_max_age_secs: u64,
    /// 活跃 source 连续失败次数达到此值 → 视为静默（Warn）。默认 10。
    pub silent_source_max_consecutive_failures: u32,
    /// `pending_fetch` 抓取队列或 `pending` AI 队列深度超过此值 → 视为积压
    /// （Warn）。默认 1000：worker 跟不上 / 调度未运行 / 流量尖峰的早期信号。
    pub pending_backlog_warn_threshold: u64,
}

impl Default for DoctorConfig {
    fn default() -> Self {
        Self {
            stuck_reindex_pending_secs: 3600,
            silent_source_max_age_secs: 86_400,
            silent_source_max_consecutive_failures: 10,
            pending_backlog_warn_threshold: 1000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_app_config() {
        let content = include_str!("../../../configs/app.toml.example");
        let config: AppConfig = toml::from_str(content).expect("example app config parses");

        assert_eq!(config.schema_version, "1");
        assert_eq!(config.database.driver, DatabaseDriver::Sqlite);
        assert_eq!(config.ai.model, "gpt-4o-mini");
        assert_eq!(config.ai.rate_limit.tokens_per_minute, 0);
        assert!(!config.publish.include_unscored);
        assert_eq!(config.runtime.max_batches_per_run, 10);
    }

    #[test]
    fn missing_runtime_block_falls_back_to_default() {
        // §4.4 line 196: 默认 10。`[runtime]` 段缺失时 serde default 必须
        // 还原默认值，不应反序列化失败 —— 旧 toml 应平滑兼容。
        let content = include_str!("../../../configs/app.toml.example");
        // 删掉 [runtime] 段，模拟旧配置
        let stripped: String = content
            .lines()
            .filter(|line| {
                !line.starts_with("[runtime]") && !line.starts_with("max_batches_per_run")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config: AppConfig =
            toml::from_str(&stripped).expect("legacy app config without [runtime] parses");
        assert_eq!(config.runtime.max_batches_per_run, 10);
    }

    #[test]
    fn runtime_max_batches_zero_round_trips_as_unlimited() {
        // §4.4 line 196: `0` 表示不限。反序列化必须保留 0，不可被 default 顶替。
        let toml = r#"
schema_version = "1"
[database]
driver = "sqlite"
sqlite_path = "data.db"
max_connections = 5
busy_timeout_ms = 5000
[http]
user_agent = "x"
timeout_seconds = 30
max_retries = 3
retry_backoff_base_ms = 1000
concurrent_feeds = 10
concurrent_fetches = 5
[ai]
enabled = true
model = "m"
max_tokens = 100
temperature = 0.1
request_timeout_seconds = 60
max_input_chars = 1000
[ai.rate_limit]
requests_per_minute = 60
tokens_per_minute = 0
[publish]
target_timezone = "UTC"
github_owner = ""
github_repo = ""
github_branch = "main"
github_path_prefix = "x"
local_output_dir = "out"
include_unscored = false
max_items_per_report = 1
min_importance_score = 30
[publish.template]
path_template = "{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md"
frontmatter_template = "---\ntitle: {date}\ndate: {date}\nexcerpt: {excerpt_yaml}\n---\n"
report_template = "{frontmatter}\n# {title_md}\n{excerpt_block}\n{items}"
item_template = '''
## {item_title_md}{score_badge}

{tags_block}- **Source:** `{source_code}` | [阅读原文]({url_md})

> [摘要]  
{summary_blockquote}

---

'''
[dedup]
enable_link_dedup = true
enable_content_dedup = true
link_normalizer_version = "1"
[extractor]
strategy_order = ["readability"]
max_body_bytes = 1024
min_body_chars = 1
[lease]
fetch_duration_seconds = 1
ai_duration_seconds = 1
publish_duration_seconds = 1
reclaim_interval_seconds = 1
[retry]
feed_entry_max_attempts = 1
ai_max_attempts = 1
publish_max_attempts = 1
[runtime]
max_batches_per_run = 0
[artifact]
retention_policy = "off"
sample_rate = 0.1
inline_threshold_bytes = 1024
file_storage_dir = "x"
ttl_days = 30
[observability]
log_level = "info"
log_format = "pretty"
log_file = ""
enable_metrics = false
metrics_bind = "127.0.0.1:9090"
"#;
        let config: AppConfig = toml::from_str(toml).expect("zero parses");
        assert_eq!(config.runtime.max_batches_per_run, 0);
    }

    #[test]
    fn feed_max_body_bytes_falls_back_to_max_body_bytes_when_absent() {
        // 旧配置（无 feed_max_body_bytes）：feed 抓取上限沿用 max_body_bytes，
        // 行为零变化。这是解耦该 knob 时向后兼容的核心保证。
        let extractor = ExtractorConfig {
            strategy_order: vec!["readability".to_string()],
            max_body_bytes: 1_048_576,
            feed_max_body_bytes: None,
            min_body_chars: 100,
        };
        assert_eq!(extractor.effective_feed_max_body_bytes(), 1_048_576);
    }

    #[test]
    fn feed_max_body_bytes_overrides_max_body_bytes_when_present() {
        // 显式设置时，feed 上限与 HTML 上限解耦：抬高 feed 不波及 HTML。
        let extractor = ExtractorConfig {
            strategy_order: vec!["readability".to_string()],
            max_body_bytes: 1_048_576,
            feed_max_body_bytes: Some(5_242_880),
            min_body_chars: 100,
        };
        assert_eq!(extractor.effective_feed_max_body_bytes(), 5_242_880);
        assert_eq!(extractor.max_body_bytes, 1_048_576, "HTML 上限不受影响");
    }
}
