//! Configuration loading and validation.
//!
//! Three-layer: env (secrets) / app.toml (global) / categories/*.toml (per-category).
//! See docs/design/config-schema.md.

pub mod app;
pub mod category;
pub mod effective;
pub mod env;
pub mod error;
pub mod loader;
pub mod overrides;
pub mod validate;
pub mod version;

mod rsshub;

pub use app::{
    AiConfig, AiRateLimitConfig, AppConfig, ArtifactConfig, DatabaseConfig, DatabaseDriver,
    DedupConfig, ExtractorConfig, HttpConfig, LeaseConfig, ObservabilityConfig, PublishConfig,
    PublishTemplateConfig, RetentionPolicy, RetryConfig, RuntimeConfig,
};
pub use category::{AiOverride, CategoryConfig, CategoryMeta, PublishOverride, SourceConfig};
pub use effective::EffectiveConfig;
pub use env::EnvConfig;
pub use error::{ConfigError, Diagnostic, DiagnosticReport};
pub use loader::{LoadedConfig, SourceSecrets, load, load_skip_env_checks};
pub use overrides::CliOverrides;
pub use validate::{CommandFlags, CommandKind};
pub use version::{ConfigVersionStore, ConfigVersionStoreError, compute_config_sha256};
