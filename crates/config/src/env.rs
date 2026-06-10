use std::{
    env, fmt,
    path::{Path, PathBuf},
};

use rss_ai_news_domain::SecretString;

use crate::ConfigError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvConfig {
    pub openai_api_key: Option<SecretString>,
    pub openai_base_url: Option<String>,
    pub github_token: Option<SecretString>,
    pub rsshub_base_url: Option<String>,
    pub rsshub_access_key: Option<SecretString>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub database_url: Option<String>,
    /// W14-B：`.env` 文件全量键值（私有），供 `resolve_secret` 按板块
    /// `api_key_env` 动态解析。值在 `Debug` 中固定 redact（见 [`EnvFileValues`]）。
    file_values: EnvFileValues,
}

impl EnvConfig {
    /// W14-B：按名字动态解析 env 变量（板块 `api_key_env` 引用）。
    /// 优先级与固定字段一致（`value` 同语义）：进程 env > `.env` 文件
    /// （同 key 取最后一次出现）；trim 后空白视为未设置。
    pub fn resolve_secret(&self, name: &str) -> Option<SecretString> {
        env::var(name)
            .ok()
            .or_else(|| self.file_values.get(name).map(str::to_owned))
            .filter(|value| !value.trim().is_empty())
            .map(SecretString::new)
    }
}

/// `.env` 文件原始键值的私有载体。值可能含任意密钥，`Debug` 只输出键名
/// 列表（键名非密钥，便于诊断"配置了哪些变量"），值一律不打印。
#[derive(Clone, Default, PartialEq, Eq)]
struct EnvFileValues(Vec<(String, String)>);

impl EnvFileValues {
    /// 同 key 多次出现取最后一次（与 `value` 的 `.rev().find()` 一致）。
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for EnvFileValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

pub fn load(env_file: Option<&Path>) -> Result<EnvConfig, ConfigError> {
    let file_values = load_file_values(env_file)?;

    Ok(EnvConfig {
        openai_api_key: secret("OPENAI_API_KEY", &file_values),
        openai_base_url: value("OPENAI_BASE_URL", &file_values),
        github_token: secret("GITHUB_TOKEN", &file_values),
        rsshub_base_url: value("RSSHUB_BASE_URL", &file_values),
        rsshub_access_key: secret("RSSHUB_ACCESS_KEY", &file_values),
        http_proxy: value("HTTP_PROXY", &file_values),
        https_proxy: value("HTTPS_PROXY", &file_values),
        database_url: value("DATABASE_URL", &file_values),
        file_values: EnvFileValues(file_values),
    })
}

fn load_file_values(env_file: Option<&Path>) -> Result<Vec<(String, String)>, ConfigError> {
    let default_path;
    let Some(path) = env_file else {
        default_path = PathBuf::from(".env");
        if !default_path.exists() {
            return Ok(Vec::new());
        }
        return load_existing_file(&default_path);
    };

    if !path.exists() {
        return Err(ConfigError::FileNotFound {
            path: path.display().to_string(),
        });
    }

    load_existing_file(path)
}

fn load_existing_file(path: &Path) -> Result<Vec<(String, String)>, ConfigError> {
    let mut values = Vec::new();
    for item in dotenvy::from_path_iter(path).map_err(|err| ConfigError::ParseFailed {
        path: path.display().to_string(),
        reason: err.to_string(),
    })? {
        let (key, value) = item.map_err(|err| ConfigError::ParseFailed {
            path: path.display().to_string(),
            reason: err.to_string(),
        })?;
        values.push((key, value));
    }
    Ok(values)
}

fn value(key: &str, file_values: &[(String, String)]) -> Option<String> {
    env::var(key)
        .ok()
        .or_else(|| {
            file_values
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        })
        .filter(|value| !value.trim().is_empty())
}

fn secret(key: &str, file_values: &[(String, String)]) -> Option<SecretString> {
    value(key, file_values).map(SecretString::new)
}

#[cfg(test)]
mod tests {
    use std::{fs, time::SystemTime};

    use super::*;

    #[test]
    fn env_file_loads_non_empty_values() {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        path.push(format!("rss-ai-news-config-env-{unique}.env"));
        fs::write(
            &path,
            "OPENAI_API_KEY=sk-test\nOPENAI_BASE_URL=https://api.example.test/v1\nRSSHUB_ACCESS_KEY=rsshub-secret\nHTTP_PROXY=\n",
        )
        .expect("write temp env file");

        let config = load(Some(&path)).expect("env file loads");
        fs::remove_file(&path).expect("remove temp env file");

        assert_eq!(
            config
                .openai_api_key
                .as_ref()
                .map(SecretString::expose_secret),
            Some("sk-test")
        );
        assert_eq!(
            config.openai_base_url.as_deref(),
            Some("https://api.example.test/v1")
        );
        assert_eq!(config.http_proxy, None);
        assert_eq!(
            config
                .rsshub_access_key
                .as_ref()
                .map(SecretString::expose_secret),
            Some("rsshub-secret")
        );
    }

    #[test]
    fn env_secret_fields_redact_in_debug_output() {
        // Smoke test that EnvConfig's Debug never leaks secret values, even when
        // tracing/log statements format the whole struct (regression guard for
        // W0 codex 二审 Issue 4 / docs/handoffs/2026-05-07-w0-doc-freeze-e2-decisions.md).
        let secret = "sk-extremely-secret-value-9999";
        let config = EnvConfig {
            openai_api_key: Some(SecretString::new(secret)),
            github_token: Some(SecretString::new(secret)),
            rsshub_access_key: Some(SecretString::new(secret)),
            ..EnvConfig::default()
        };
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(secret),
            "Debug must not leak secret: {rendered}"
        );
        assert!(rendered.contains("***"));
    }

    /// W14-B：动态解析覆盖三态——文件命中 / 空白视为未设置 / 不存在。
    #[test]
    fn resolve_secret_reads_arbitrary_env_file_key() {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        path.push(format!("rss-ai-news-config-resolve-{unique}.env"));
        fs::write(
            &path,
            "DEEPSEEK_API_KEY=sk-deepseek\nDEEPSEEK_API_KEY=sk-deepseek-last\nBLANK_KEY=   \n",
        )
        .expect("write temp env file");

        let config = load(Some(&path)).expect("env file loads");
        fs::remove_file(&path).expect("remove temp env file");

        // 同 key 多次出现取最后一次（与固定字段的 value() 语义一致）。
        assert_eq!(
            config
                .resolve_secret("DEEPSEEK_API_KEY")
                .as_ref()
                .map(SecretString::expose_secret),
            Some("sk-deepseek-last")
        );
        assert_eq!(config.resolve_secret("BLANK_KEY"), None);
        assert_eq!(config.resolve_secret("NO_SUCH_KEY_W14B"), None);
    }

    /// W14-B：`.env` 保留的全量键值在 Debug 中只露键名、绝不露值。
    #[test]
    fn env_file_values_redact_in_debug_output() {
        let secret = "sk-w14b-file-secret-1234";
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        path.push(format!("rss-ai-news-config-redact-{unique}.env"));
        fs::write(&path, format!("CUSTOM_PROVIDER_KEY={secret}\n")).expect("write temp env file");

        let config = load(Some(&path)).expect("env file loads");
        fs::remove_file(&path).expect("remove temp env file");

        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(secret),
            "Debug must not leak .env file value: {rendered}"
        );
        assert!(
            rendered.contains("CUSTOM_PROVIDER_KEY"),
            "Debug should list key names for diagnostics: {rendered}"
        );
    }
}
