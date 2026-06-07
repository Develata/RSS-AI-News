use std::{
    collections::{BTreeMap, HashSet},
    path::Component,
};

use url::Url;

use crate::{
    AppConfig, CategoryConfig, Diagnostic, DiagnosticReport, EnvConfig, LoadedConfig, rsshub,
    validate::CommandFlags,
};

use super::SUPPORTED_SCHEMA_VERSION;

pub(super) fn collect_general_checks(
    report: &mut DiagnosticReport,
    app: &AppConfig,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) {
    if app.schema_version != SUPPORTED_SCHEMA_VERSION {
        report.push(Diagnostic::new(
            "app.toml",
            "schema_version",
            format!(
                "expected schema_version {SUPPORTED_SCHEMA_VERSION:?}, got {:?}",
                app.schema_version
            ),
        ));
    }

    collect_category_checks(report, categories, env);
    collect_app_value_checks(report, app);
    collect_cross_category_path_collisions(report, app, categories);
}

pub(super) fn collect_env_checks(
    report: &mut DiagnosticReport,
    app: &AppConfig,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) {
    if app.ai.enabled {
        if is_blank(
            env.openai_api_key
                .as_ref()
                .map(rss_ai_news_domain::SecretString::expose_secret),
        ) {
            report.push(Diagnostic::new(
                ".env",
                "OPENAI_API_KEY",
                "required when app.ai.enabled=true",
            ));
        }
        match env
            .openai_base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            Some(value) if Url::parse(value).is_ok() => {}
            Some(value) => report.push(Diagnostic::new(
                ".env",
                "OPENAI_BASE_URL",
                format!("invalid URL {value:?}"),
            )),
            None => report.push(Diagnostic::new(
                ".env",
                "OPENAI_BASE_URL",
                "required when app.ai.enabled=true",
            )),
        }
    }

    if categories
        .iter()
        .flat_map(|category| &category.sources)
        .any(|source| rsshub::has_base_placeholder(&source.feed_url))
        && is_blank(env.rsshub_base_url.as_deref())
    {
        report.push(Diagnostic::new(
            ".env",
            "RSSHUB_BASE_URL",
            "required because at least one source uses an RSSHub base URL placeholder",
        ));
    }
}

pub(super) fn collect_publish_checks(
    report: &mut DiagnosticReport,
    config: &LoadedConfig,
    flags: &CommandFlags,
) {
    if flags.local_only || config.app.publish.github_owner.trim().is_empty() {
        return;
    }
    if is_blank(
        config
            .env
            .github_token
            .as_ref()
            .map(rss_ai_news_domain::SecretString::expose_secret),
    ) {
        report.push(Diagnostic::new(
            ".env",
            "GITHUB_TOKEN",
            "required for remote publish",
        ));
    }
    if config.app.publish.github_repo.trim().is_empty() {
        report.push(Diagnostic::new(
            "app.toml",
            "publish.github_repo",
            "required for remote publish when publish.github_owner is non-empty",
        ));
    }
}

fn collect_category_checks(
    report: &mut DiagnosticReport,
    categories: &[CategoryConfig],
    env: &EnvConfig,
) {
    let mut category_keys = HashSet::new();
    for category in categories {
        let source_file = category_source_file(category);
        if category.schema_version != SUPPORTED_SCHEMA_VERSION {
            report.push(Diagnostic::new(
                source_file.clone(),
                "schema_version",
                format!(
                    "expected schema_version {SUPPORTED_SCHEMA_VERSION:?}, got {:?}",
                    category.schema_version
                ),
            ));
        }
        if !category_keys.insert(category.category.key.clone()) {
            report.push(Diagnostic::new(
                source_file.clone(),
                "category.key",
                format!("duplicate category key {:?}", category.category.key),
            ));
        }
        if let Some(path_template) = category
            .publish_override
            .as_ref()
            .and_then(|override_| override_.path_template.as_ref())
        {
            let field_path = "category.publish_override.path_template";
            if path_template.trim().is_empty() {
                report.push(Diagnostic::new(
                    source_file.clone(),
                    field_path,
                    "must not be empty",
                ));
            }
            validate_path_template(
                report,
                source_file.clone(),
                field_path,
                path_template,
                false,
            );
            validate_template_placeholders(
                report,
                source_file.clone(),
                field_path,
                path_template,
                &[
                    "category_key",
                    "CATEGORY_KEY",
                    "date",
                    "YYYY",
                    "MM",
                    "DD",
                    "YYYYMMDD",
                ],
            );
        }

        if let Some(fallback_models) = category
            .ai_override
            .as_ref()
            .and_then(|override_| override_.fallback_models.as_ref())
        {
            for (index, model) in fallback_models.iter().enumerate() {
                if model.trim().is_empty() {
                    report.push(Diagnostic::new(
                        source_file.clone(),
                        format!("category.ai_override.fallback_models[{index}]"),
                        "must not be blank",
                    ));
                }
            }
        }

        let mut source_keys = HashSet::new();
        for (index, source) in category.sources.iter().enumerate() {
            if !source_keys.insert(source.key.clone()) {
                report.push(Diagnostic::new(
                    source_file.clone(),
                    format!("sources[{index}].key"),
                    format!("duplicate source key {:?}", source.key),
                ));
            }
            validate_feed_url(report, &source_file, index, &source.feed_url, env);
        }
    }
}

fn collect_app_value_checks(report: &mut DiagnosticReport, app: &AppConfig) {
    if !is_supported_timezone(&app.publish.target_timezone) {
        report.push(Diagnostic::new(
            "app.toml",
            "publish.target_timezone",
            format!("invalid IANA timezone {:?}", app.publish.target_timezone),
        ));
    }

    let u64_checks = [
        ("http.timeout_seconds", app.http.timeout_seconds),
        ("http.retry_backoff_base_ms", app.http.retry_backoff_base_ms),
        (
            "lease.fetch_duration_seconds",
            app.lease.fetch_duration_seconds,
        ),
        ("lease.ai_duration_seconds", app.lease.ai_duration_seconds),
        (
            "lease.publish_duration_seconds",
            app.lease.publish_duration_seconds,
        ),
        (
            "lease.reclaim_interval_seconds",
            app.lease.reclaim_interval_seconds,
        ),
        ("extractor.max_body_bytes", app.extractor.max_body_bytes),
        ("ai.request_timeout_seconds", app.ai.request_timeout_seconds),
    ];
    for (field_path, value) in u64_checks {
        check_positive_u64(report, "app.toml", field_path, value);
    }

    let u32_checks = [
        ("http.concurrent_feeds", app.http.concurrent_feeds),
        ("http.concurrent_fetches", app.http.concurrent_fetches),
        (
            "retry.feed_entry_max_attempts",
            app.retry.feed_entry_max_attempts,
        ),
        ("retry.ai_max_attempts", app.retry.ai_max_attempts),
        ("retry.publish_max_attempts", app.retry.publish_max_attempts),
        ("extractor.min_body_chars", app.extractor.min_body_chars),
        ("ai.max_tokens", app.ai.max_tokens),
        (
            "ai.rate_limit.requests_per_minute",
            app.ai.rate_limit.requests_per_minute,
        ),
    ];
    for (field_path, value) in u32_checks {
        check_positive_u32(report, "app.toml", field_path, value);
    }

    // W14-A: fallback 链元素不得为空白（effective 层会静默 trim/去重，但空白元素
    // 多半是配置笔误，fail-fast 报出而非默默丢弃）。
    for (index, model) in app.ai.fallback_models.iter().enumerate() {
        if model.trim().is_empty() {
            report.push(Diagnostic::new(
                "app.toml",
                format!("ai.fallback_models[{index}]"),
                "must not be blank",
            ));
        }
    }

    let template_checks = [
        (
            "publish.template.path_template",
            &app.publish.template.path_template,
        ),
        (
            "publish.template.frontmatter_template",
            &app.publish.template.frontmatter_template,
        ),
        (
            "publish.template.report_template",
            &app.publish.template.report_template,
        ),
        (
            "publish.template.item_template",
            &app.publish.template.item_template,
        ),
    ];
    for (field_path, value) in template_checks {
        if value.trim().is_empty() {
            report.push(Diagnostic::new("app.toml", field_path, "must not be empty"));
        }
    }
    validate_path_template(
        report,
        "app.toml",
        "publish.template.path_template",
        &app.publish.template.path_template,
        true,
    );
    validate_template_placeholders(
        report,
        "app.toml",
        "publish.template.path_template",
        &app.publish.template.path_template,
        &[
            "category_key",
            "CATEGORY_KEY",
            "date",
            "YYYY",
            "MM",
            "DD",
            "YYYYMMDD",
        ],
    );
    validate_template_placeholders(
        report,
        "app.toml",
        "publish.template.frontmatter_template",
        &app.publish.template.frontmatter_template,
        &[
            "title",
            "title_yaml",
            "date",
            "YYYY",
            "MM",
            "DD",
            "YYYYMMDD",
            "excerpt",
            "excerpt_yaml",
        ],
    );
    validate_template_placeholders(
        report,
        "app.toml",
        "publish.template.report_template",
        &app.publish.template.report_template,
        &[
            "frontmatter",
            "title",
            "title_md",
            "date",
            "YYYY",
            "MM",
            "DD",
            "YYYYMMDD",
            "category_key",
            "CATEGORY_KEY",
            "category_display_name",
            "category_display_name_md",
            "excerpt",
            "excerpt_yaml",
            "excerpt_block",
            "items",
            "generated_at",
        ],
    );
    validate_template_placeholders(
        report,
        "app.toml",
        "publish.template.item_template",
        &app.publish.template.item_template,
        &[
            "item_title",
            "item_title_md",
            "score",
            "score_badge",
            "tags",
            "tags_block",
            "source",
            "source_md",
            "source_code",
            "url",
            "url_md",
            "summary",
            "summary_inline",
            "summary_blockquote",
        ],
    );
    validate_required_template_tokens(report, app);
}

/// 跨分类 path collision pass。
///
/// 全局 `[publish.template].path_template` 强制含 `{category_key}` /
/// `{CATEGORY_KEY}` 占位符，所以走全局模板的分类天然不会互相覆盖。
/// 分类级 `[category.publish_override].path_template` 放松了这一约束
/// （`validate_path_template(require_category_token=false)`），允许写死路径，
/// 但前提是 **调用方保证不同分类不会渲染出同一路径**。本 pass 用样本日期
/// 渲染每个分类的 effective path_template，对结果做 `\` → `/` 归一化后查
/// 重，把"配置合法但运行时会互相覆盖"的情况在 validate 阶段拦下。
fn collect_cross_category_path_collisions(
    report: &mut DiagnosticReport,
    app: &AppConfig,
    categories: &[CategoryConfig],
) {
    let global = app.publish.template.path_template.as_str();
    // BTreeMap 让 Diagnostic 顺序与 category_key 字典序一致，便于回归测试。
    let mut bucket: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for category in categories {
        let template = category
            .publish_override
            .as_ref()
            .and_then(|override_| override_.path_template.as_deref())
            .filter(|template| !template.trim().is_empty())
            .unwrap_or(global);
        let rendered = render_path_template_sample(template, &category.category.key);
        bucket
            .entry(rendered)
            .or_default()
            .push(category.category.key.clone());
    }
    for (path, mut keys) in bucket {
        if keys.len() > 1 {
            keys.sort();
            report.push(Diagnostic::new(
                "categories/*.toml",
                "category.publish_override.path_template",
                format!(
                    "rendered path {path:?} collides across categories {keys:?}: \
                    include {{category_key}} or {{CATEGORY_KEY}} in path_template, \
                    or give each affected category a distinct prefix"
                ),
            ));
        }
    }
}

/// 用样本日期与真实 category_key 渲染 path_template，复用与 `validate_path_template`
/// 内一致的占位符集。这里使用真实 category_key（不是 fixed "ai_ml"），因为
/// collision 检查要看不同分类渲染出来是否分得开。
fn render_path_template_sample(template: &str, category_key: &str) -> String {
    template
        .replace("{category_key}", category_key)
        .replace("{CATEGORY_KEY}", &category_key.to_ascii_uppercase())
        .replace("{date}", "2026-01-03")
        .replace("{YYYY}", "2026")
        .replace("{MM}", "01")
        .replace("{DD}", "03")
        .replace("{YYYYMMDD}", "20260103")
        .replace('\\', "/")
}

fn validate_path_template(
    report: &mut DiagnosticReport,
    source: impl Into<String> + Clone,
    field_path: &str,
    template: &str,
    require_category_token: bool,
) {
    if template.contains('\\') {
        report.push(Diagnostic::new(
            source.clone(),
            field_path,
            "must use '/' separators, not '\\'",
        ));
    }
    if template.contains("..") {
        report.push(Diagnostic::new(
            source.clone(),
            field_path,
            "must not contain '..'",
        ));
    }

    let sample = template
        .replace("{category_key}", "ai_ml")
        .replace("{CATEGORY_KEY}", "AI_ML")
        .replace("{date}", "2026-01-03")
        .replace("{YYYY}", "2026")
        .replace("{MM}", "01")
        .replace("{DD}", "03")
        .replace("{YYYYMMDD}", "20260103");
    let sample_path = std::path::Path::new(&sample);
    if sample_path.is_absolute()
        || sample_path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        report.push(Diagnostic::new(
            source.clone(),
            field_path,
            "must render to a relative path inside the publish root",
        ));
    }
    let has_date_token = template.contains("{date}")
        || template.contains("{YYYYMMDD}")
        || (template.contains("{YYYY}") && template.contains("{MM}") && template.contains("{DD}"));
    if !has_date_token {
        report.push(Diagnostic::new(
            source.clone(),
            field_path,
            "must include {date}, {YYYYMMDD}, or {YYYY}+{MM}+{DD} to avoid overwriting reports from different days",
        ));
    }
    if require_category_token
        && !template.contains("{category_key}")
        && !template.contains("{CATEGORY_KEY}")
    {
        report.push(Diagnostic::new(
            source,
            field_path,
            "must include {category_key} or {CATEGORY_KEY} to avoid cross-category overwrites",
        ));
    }
}

fn validate_required_template_tokens(report: &mut DiagnosticReport, app: &AppConfig) {
    if !app.publish.template.report_template.contains("{items}") {
        report.push(Diagnostic::new(
            "app.toml",
            "publish.template.report_template",
            "must include {items}",
        ));
    }
    if !app
        .publish
        .template
        .item_template
        .contains("{item_title_md}")
        && !app.publish.template.item_template.contains("{item_title}")
    {
        report.push(Diagnostic::new(
            "app.toml",
            "publish.template.item_template",
            "must include {item_title_md} or {item_title}",
        ));
    }
    if !app
        .publish
        .template
        .item_template
        .contains("{summary_blockquote}")
        && !app.publish.template.item_template.contains("{summary}")
        && !app
            .publish
            .template
            .item_template
            .contains("{summary_inline}")
    {
        report.push(Diagnostic::new(
            "app.toml",
            "publish.template.item_template",
            "must include a summary placeholder",
        ));
    }
}

fn validate_template_placeholders(
    report: &mut DiagnosticReport,
    source: impl Into<String> + Clone,
    field_path: &str,
    template: &str,
    allowed: &[&str],
) {
    let chars = template.char_indices().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let (start_byte, ch) = chars[index];
        if ch != '{' {
            if ch == '}' {
                report.push(Diagnostic::new(
                    source.clone(),
                    field_path,
                    "unmatched '}' in template",
                ));
            }
            index += 1;
            continue;
        }

        let Some(end_index) = chars[index + 1..]
            .iter()
            .position(|(_, candidate)| *candidate == '}')
            .map(|offset| index + 1 + offset)
        else {
            report.push(Diagnostic::new(
                source.clone(),
                field_path,
                "unmatched '{' in template",
            ));
            break;
        };
        let end_byte = chars[end_index].0;
        let name = &template[start_byte + 1..end_byte];
        if name.is_empty() || !allowed.contains(&name) {
            report.push(Diagnostic::new(
                source.clone(),
                field_path,
                format!("unknown template placeholder {{{name}}}"),
            ));
        }
        index = end_index + 1;
    }
}

fn validate_feed_url(
    report: &mut DiagnosticReport,
    source_file: &str,
    index: usize,
    feed_url: &str,
    env: &EnvConfig,
) {
    if feed_url.trim().is_empty() {
        report.push(Diagnostic::new(
            source_file,
            format!("sources[{index}].feed_url"),
            "feed_url must not be empty",
        ));
        return;
    }

    let candidate = if rsshub::has_base_placeholder(feed_url) {
        match env
            .rsshub_base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            Some(base_url) => rsshub::expand_base_placeholders(feed_url, base_url),
            None => return,
        }
    } else {
        feed_url.to_string()
    };

    if Url::parse(&candidate).is_err() {
        report.push(Diagnostic::new(
            source_file,
            format!("sources[{index}].feed_url"),
            format!("invalid URL {feed_url:?}"),
        ));
    }
}

fn check_positive_u32(
    report: &mut DiagnosticReport,
    source_file: &str,
    field_path: &str,
    value: u32,
) {
    if value == 0 {
        report.push(Diagnostic::new(source_file, field_path, "must be > 0"));
    }
}

fn check_positive_u64(
    report: &mut DiagnosticReport,
    source_file: &str,
    field_path: &str,
    value: u64,
) {
    if value == 0 {
        report.push(Diagnostic::new(source_file, field_path, "must be > 0"));
    }
}

fn is_blank(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.trim().is_empty())
}

fn category_source_file(category: &CategoryConfig) -> String {
    format!("categories/{}.toml", category.category.key)
}

// Strict IANA parsing is intentionally deferred until the project adds a timezone
// database dependency. This whitelist covers shipped fixtures and common targets,
// then rejects blank or malformed strings with whitespace.
fn is_supported_timezone(timezone: &str) -> bool {
    matches!(
        timezone,
        "UTC"
            | "Asia/Shanghai"
            | "Asia/Tokyo"
            | "Asia/Singapore"
            | "Europe/London"
            | "Europe/Berlin"
            | "America/New_York"
            | "America/Los_Angeles"
    ) || (timezone.contains('/')
        && !timezone.trim().is_empty()
        && !timezone.chars().any(char::is_whitespace))
}
