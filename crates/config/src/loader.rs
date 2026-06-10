use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    AppConfig, CategoryConfig, CliOverrides, ConfigError, EnvConfig, compute_config_sha256, env,
    rsshub, validate,
};
use rss_ai_news_domain::{SecretString, state::FeedKind};
use url::Url;

type CategoryTomlContents = Vec<(String, String)>;
type LoadedCategories = (Vec<CategoryConfig>, CategoryTomlContents);

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub env: EnvConfig,
    pub app: AppConfig,
    pub categories: Vec<CategoryConfig>,
    pub source_secrets: SourceSecrets,
    pub config_sha256: String,
    pub cli_overrides: CliOverrides,
}

impl LoadedConfig {
    pub fn categories_filtered(&self) -> impl Iterator<Item = &CategoryConfig> {
        self.categories.iter().filter(|category| {
            self.cli_overrides
                .category_filter
                .as_deref()
                .is_none_or(|filter| category.category.key == filter)
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct SourceSecrets {
    rsshub_access_keys: BTreeMap<(String, String), SecretString>,
}

impl SourceSecrets {
    pub fn insert_rsshub_access_key(
        &mut self,
        category_key: impl Into<String>,
        source_key: impl Into<String>,
        access_key: SecretString,
    ) {
        self.rsshub_access_keys
            .insert((category_key.into(), source_key.into()), access_key);
    }

    pub fn rsshub_access_key(&self, category_key: &str, source_key: &str) -> Option<&SecretString> {
        self.rsshub_access_keys
            .get(&(category_key.to_string(), source_key.to_string()))
    }
}

pub fn load(
    config_dir: &Path,
    env_file: Option<&Path>,
    cli_overrides: CliOverrides,
) -> Result<LoadedConfig, ConfigError> {
    load_inner(config_dir, env_file, cli_overrides, true)
}

/// Load config without enforcing env-credential presence checks.
///
/// Use this for infrastructure / diagnostic commands (e.g. `migrate`) that read
/// only the database section and never call OpenAI / RSSHub. Structural checks
/// (schema_version, category uniqueness, URL well-formedness, app value ranges)
/// are still applied; only the "OPENAI_API_KEY required when ai.enabled" /
/// "RSSHUB_BASE_URL required when an RSSHub base URL placeholder is used" gates are skipped.
pub fn load_skip_env_checks(
    config_dir: &Path,
    env_file: Option<&Path>,
    cli_overrides: CliOverrides,
) -> Result<LoadedConfig, ConfigError> {
    load_inner(config_dir, env_file, cli_overrides, false)
}

fn load_inner(
    config_dir: &Path,
    env_file: Option<&Path>,
    cli_overrides: CliOverrides,
    enforce_env_checks: bool,
) -> Result<LoadedConfig, ConfigError> {
    let env = env::load(env_file)?;

    let app_path = config_dir.join("app.toml");
    let app_content = read_required_file(&app_path)?;
    let mut app: AppConfig =
        toml::from_str(&app_content).map_err(|err| ConfigError::ParseFailed {
            path: app_path.display().to_string(),
            reason: err.to_string(),
        })?;

    let (mut categories, category_contents) = load_categories(&config_dir.join("categories"))?;

    cli_overrides.apply_to_app(&mut app);
    if enforce_env_checks {
        // W14-B：全局凭证 gate 按 --category filtered 范围判定继承关系。
        validate::run_general_checks(
            &app,
            &categories,
            &env,
            cli_overrides.category_filter.as_deref(),
        )?;
    } else {
        validate::run_structural_checks(&app, &categories, &env)?;
    }

    let config_sha256 = compute_config_sha256(&app_content, &category_contents);
    let source_secrets = expand_env_placeholders(&mut categories, &env);

    Ok(LoadedConfig {
        env,
        app,
        categories,
        source_secrets,
        config_sha256,
        cli_overrides,
    })
}

fn load_categories(categories_dir: &Path) -> Result<LoadedCategories, ConfigError> {
    if !categories_dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(categories_dir).map_err(|err| ConfigError::ParseFailed {
        path: categories_dir.display().to_string(),
        reason: err.to_string(),
    })? {
        let path = entry
            .map_err(|err| ConfigError::ParseFailed {
                path: categories_dir.display().to_string(),
                reason: err.to_string(),
            })?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            paths.push(path);
        }
    }
    paths.sort();

    let mut categories = Vec::with_capacity(paths.len());
    let mut contents = Vec::with_capacity(paths.len());
    for path in paths {
        let content = read_required_file(&path)?;
        let category: CategoryConfig =
            toml::from_str(&content).map_err(|err| ConfigError::ParseFailed {
                path: path.display().to_string(),
                reason: err.to_string(),
            })?;
        let filename = filename(&path)?;
        contents.push((filename, content));
        categories.push(category);
    }

    Ok((categories, contents))
}

fn expand_env_placeholders(categories: &mut [CategoryConfig], env: &EnvConfig) -> SourceSecrets {
    let mut source_secrets = SourceSecrets::default();
    let rsshub_base_url = env
        .rsshub_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_string());

    for category in categories {
        for source in &mut category.sources {
            if let Some(base_url) = rsshub_base_url.as_deref() {
                source.feed_url = rsshub::expand_base_placeholders(&source.feed_url, base_url);
            }
            if source.feed_kind == FeedKind::RssHub {
                let inline_access_key = strip_query_param(&mut source.feed_url, "key");
                if let Some(access_key) = inline_access_key
                    .filter(|value| !value.trim().is_empty())
                    .map(SecretString::new)
                    .or_else(|| env.rsshub_access_key.clone())
                {
                    source_secrets.insert_rsshub_access_key(
                        category.category.key.clone(),
                        source.key.clone(),
                        access_key,
                    );
                }
            }
        }
    }
    source_secrets
}

fn strip_query_param(raw_url: &mut String, key: &str) -> Option<String> {
    let Ok(mut url) = Url::parse(raw_url) else {
        return None;
    };

    let mut removed = None;
    let mut kept = Vec::new();
    for (name, value) in url.query_pairs() {
        if name == key {
            if removed.is_none() {
                removed = Some(value.into_owned());
            }
        } else {
            kept.push((name.into_owned(), value.into_owned()));
        }
    }

    removed.as_ref()?;

    url.set_query(None);
    if !kept.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in kept {
            pairs.append_pair(&name, &value);
        }
    }

    *raw_url = url.to_string();
    removed
}

fn read_required_file(path: &Path) -> Result<String, ConfigError> {
    fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ConfigError::FileNotFound {
                path: path.display().to_string(),
            }
        } else {
            ConfigError::ParseFailed {
                path: path.display().to_string(),
                reason: err.to_string(),
            }
        }
    })
}

fn filename(path: &Path) -> Result<String, ConfigError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(String::from)
        .ok_or_else(|| ConfigError::ParseFailed {
            path: path.display().to_string(),
            reason: "category filename is not valid UTF-8".to_string(),
        })
}

#[allow(dead_code)]
fn _assert_pathbuf_send_sync(_: PathBuf) {}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::SystemTime};

    use crate::{CliOverrides, ConfigError, load, load_skip_env_checks};
    use rss_ai_news_domain::SecretString;

    const APP_TOML_AI_ENABLED_RSSHUB_PLACEHOLDER: &str = r#"
schema_version = "1"

[database]
driver = "sqlite"
sqlite_path = "data/rss-ai-news.db"
max_connections = 5
busy_timeout_ms = 5000

[http]
user_agent = "RSS-AI-News/test"
timeout_seconds = 30
max_retries = 3
retry_backoff_base_ms = 1000
concurrent_feeds = 10
concurrent_fetches = 5

[ai]
enabled = true
model = "gpt-4o-mini"
max_tokens = 4096
temperature = 0.3
request_timeout_seconds = 60
max_input_chars = 8000

[ai.rate_limit]
requests_per_minute = 60
tokens_per_minute = 0

[publish]
target_timezone = "Asia/Shanghai"
github_owner = ""
github_repo = ""
github_branch = "main"
github_path_prefix = "archive"
local_output_dir = "output"
include_unscored = false
max_items_per_report = 30
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
max_body_bytes = 1048576
min_body_chars = 100

[lease]
fetch_duration_seconds = 300
ai_duration_seconds = 600
publish_duration_seconds = 600
reclaim_interval_seconds = 120

[retry]
feed_entry_max_attempts = 5
ai_max_attempts = 3
publish_max_attempts = 5

[artifact]
retention_policy = "on_failure"
sample_rate = 0.1
inline_threshold_bytes = 65536
file_storage_dir = "data/artifacts"
ttl_days = 30

[observability]
log_level = "info"
log_format = "pretty"
log_file = ""
enable_metrics = false
metrics_bind = "127.0.0.1:9090"
"#;

    const CATEGORY_TOML_RSSHUB_PLACEHOLDER: &str = r#"
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[[sources]]
key = "rsshub-source"
display_name = "RSSHub Source"
feed_url = "{RSSHUB}/example"
feed_kind = "rsshub"
priority = 10
enabled = true
"#;

    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos();
            let mut root = std::env::temp_dir();
            root.push(format!("rss-ai-news-loader-{label}-{unique}"));
            fs::create_dir_all(root.join("configs/categories")).expect("create temp config tree");
            Self { root }
        }

        fn config_dir(&self) -> PathBuf {
            self.root.join("configs")
        }

        fn write_app(&self, body: &str) {
            fs::write(self.config_dir().join("app.toml"), body).expect("write app.toml");
        }

        fn write_category(&self, name: &str, body: &str) {
            fs::write(
                self.config_dir().join(format!("categories/{name}.toml")),
                body,
            )
            .expect("write category toml");
        }

        fn empty_env_file(&self) -> PathBuf {
            let path = self.root.join("empty.env");
            fs::write(&path, "").expect("write empty env file");
            path
        }

        fn env_file(&self, body: &str) -> PathBuf {
            let path = self.root.join("test.env");
            fs::write(&path, body).expect("write env file");
            path
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            // W11-P4-fix2.H2 lint：测试 fixture cleanup，Drop 内 best-effort。
            fs::remove_dir_all(&self.root).ok();
        }
    }

    #[test]
    fn load_skip_env_checks_succeeds_without_openai_or_rsshub_env() {
        let ws = Workspace::new("skip-env-success");
        ws.write_app(APP_TOML_AI_ENABLED_RSSHUB_PLACEHOLDER);
        ws.write_category("ai", CATEGORY_TOML_RSSHUB_PLACEHOLDER);
        let env_file = ws.empty_env_file();

        load_skip_env_checks(&ws.config_dir(), Some(&env_file), CliOverrides::default())
            .expect("infrastructure command path tolerates missing env credentials");
    }

    #[test]
    fn load_full_fails_without_openai_when_ai_enabled() {
        let ws = Workspace::new("full-failure");
        ws.write_app(APP_TOML_AI_ENABLED_RSSHUB_PLACEHOLDER);
        ws.write_category("ai", CATEGORY_TOML_RSSHUB_PLACEHOLDER);
        let env_file = ws.empty_env_file();

        let err = load(&ws.config_dir(), Some(&env_file), CliOverrides::default())
            .expect_err("full load still gates on env credentials");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }

    #[test]
    fn load_expands_rsshub_placeholder_and_appends_access_key() {
        let ws = Workspace::new("rsshub-expand");
        ws.write_app(APP_TOML_AI_ENABLED_RSSHUB_PLACEHOLDER);
        ws.write_category("ai", CATEGORY_TOML_RSSHUB_PLACEHOLDER);
        let env_file = ws.env_file(
            "OPENAI_API_KEY=sk-test\nOPENAI_BASE_URL=https://api.example.test/v1\nRSSHUB_BASE_URL=http://rsshub:1200/\nRSSHUB_ACCESS_KEY=test-key\n",
        );

        let loaded = load(&ws.config_dir(), Some(&env_file), CliOverrides::default())
            .expect("config with RSSHub env loads");

        assert_eq!(
            loaded.categories[0].sources[0].feed_url,
            "http://rsshub:1200/example"
        );
        assert_eq!(
            loaded
                .source_secrets
                .rsshub_access_key("ai", "rsshub-source")
                .map(SecretString::expose_secret),
            Some("test-key")
        );
    }

    #[test]
    fn load_strips_existing_rsshub_access_key_from_feed_url() {
        let ws = Workspace::new("rsshub-existing-key");
        ws.write_app(APP_TOML_AI_ENABLED_RSSHUB_PLACEHOLDER);
        ws.write_category(
            "ai",
            &CATEGORY_TOML_RSSHUB_PLACEHOLDER.replace(
                r#"feed_url = "{RSSHUB}/example""#,
                r#"feed_url = "{RSSHUB}/example?foo=1&key=inline-key#section""#,
            ),
        );
        let env_file = ws.env_file(
            "OPENAI_API_KEY=sk-test\nOPENAI_BASE_URL=https://api.example.test/v1\nRSSHUB_BASE_URL=http://rsshub:1200\nRSSHUB_ACCESS_KEY=env-key\n",
        );

        let loaded = load(&ws.config_dir(), Some(&env_file), CliOverrides::default())
            .expect("config with inline RSSHub key loads");

        assert_eq!(
            loaded.categories[0].sources[0].feed_url,
            "http://rsshub:1200/example?foo=1#section"
        );
        assert_eq!(
            loaded
                .source_secrets
                .rsshub_access_key("ai", "rsshub-source")
                .map(SecretString::expose_secret),
            Some("inline-key")
        );
    }

    #[test]
    fn load_expands_rsshub_base_url_placeholder_alias() {
        let ws = Workspace::new("rsshub-base-url-alias");
        ws.write_app(APP_TOML_AI_ENABLED_RSSHUB_PLACEHOLDER);
        ws.write_category(
            "ai",
            &CATEGORY_TOML_RSSHUB_PLACEHOLDER.replace(
                r#"feed_url = "{RSSHUB}/example""#,
                r#"feed_url = "{RSSHUB_BASE_URL}/example?key=inline-key""#,
            ),
        );
        let env_file = ws.env_file(
            "OPENAI_API_KEY=sk-test\nOPENAI_BASE_URL=https://api.example.test/v1\nRSSHUB_BASE_URL=http://rsshub:1200/\n",
        );

        let loaded = load(&ws.config_dir(), Some(&env_file), CliOverrides::default())
            .expect("RSSHUB_BASE_URL placeholder alias loads");

        assert_eq!(
            loaded.categories[0].sources[0].feed_url,
            "http://rsshub:1200/example"
        );
        assert_eq!(
            loaded
                .source_secrets
                .rsshub_access_key("ai", "rsshub-source")
                .map(SecretString::expose_secret),
            Some("inline-key")
        );
    }

    #[test]
    fn load_skip_env_checks_still_fails_on_bad_schema_version() {
        let ws = Workspace::new("skip-env-bad-schema");
        let bad_app = APP_TOML_AI_ENABLED_RSSHUB_PLACEHOLDER.replacen(
            r#"schema_version = "1""#,
            r#"schema_version = "2""#,
            1,
        );
        ws.write_app(&bad_app);
        ws.write_category("ai", CATEGORY_TOML_RSSHUB_PLACEHOLDER);
        let env_file = ws.empty_env_file();

        let err = load_skip_env_checks(&ws.config_dir(), Some(&env_file), CliOverrides::default())
            .expect_err("structural checks still enforce schema_version");
        assert!(matches!(err, ConfigError::ValidationFailed { .. }));
    }
}
