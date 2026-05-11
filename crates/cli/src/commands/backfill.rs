use std::io::{self, Write};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{BackfillAiOptions, BackfillExtractOptions, BackfillFlow, RuntimeError};
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::{
    args::{BackfillArgs, BackfillTarget, Cli},
    context_factory::build_run_context,
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct BackfillCommandSummary {
    pub target: String,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub feed_entries_examined: u32,
    pub feed_entries_reset: u32,
    /// 仅 `--target ai` 填充：runtime 写入的新 prompt_version 行 id。
    pub new_prompt_version_id: Option<i64>,
    /// 仅 `--target ai` 填充：用户给定的 `--prompt-version-tag`，或
    /// 回落 `backfill-<unix-ts>`。让 JSON 消费者无需查 DB 即可知道这次
    /// 实验落到了哪个版本标签。
    pub new_prompt_version_tag: Option<String>,
    /// 仅 `--target ai` 填充：实际生效的 model id（args.model 或 config 默认）。
    pub model_id: Option<String>,
    pub articles_scanned: u32,
    pub ai_tasks_inserted: u32,
    pub ai_tasks_conflict: u32,
}

impl CommandSummary for BackfillCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Backfill completed:")?;
        writeln!(writer, "  Target:               {}", self.target)?;
        writeln!(
            writer,
            "  Feed entries examined: {}",
            self.feed_entries_examined
        )?;
        writeln!(
            writer,
            "  Feed entries reset:    {}",
            self.feed_entries_reset
        )?;
        if let Some(id) = self.new_prompt_version_id {
            writeln!(writer, "  New prompt version:    {id}")?;
        }
        if let Some(tag) = &self.new_prompt_version_tag {
            writeln!(writer, "  Prompt version tag:    {tag}")?;
        }
        if let Some(model) = &self.model_id {
            writeln!(writer, "  Model:                 {model}")?;
        }
        writeln!(writer, "  Articles scanned:      {}", self.articles_scanned)?;
        writeln!(
            writer,
            "  AI tasks inserted:     {}",
            self.ai_tasks_inserted
        )?;
        writeln!(
            writer,
            "  AI task conflicts:     {}",
            self.ai_tasks_conflict
        )
    }
}

pub async fn run(cli: &Cli, args: &BackfillArgs) -> Result<BackfillCommandSummary, CliError> {
    let loaded = config::load(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let date_from = parse_date_start(args.date_from.as_deref())?;
    let date_to = parse_date_start(args.date_to.as_deref())?;
    let (_pool, ctx) = build_run_context("backfill", &loaded).await?;
    let flow = BackfillFlow::new(ctx.clone());

    match args.target {
        BackfillTarget::Extract => {
            let summary = flow
                .extract(BackfillExtractOptions { date_from, date_to })
                .await?;
            Ok(BackfillCommandSummary {
                target: "extract".to_string(),
                date_from: args.date_from.clone(),
                date_to: args.date_to.clone(),
                feed_entries_examined: summary.examined,
                feed_entries_reset: summary.reset,
                new_prompt_version_id: None,
                new_prompt_version_tag: None,
                model_id: None,
                articles_scanned: 0,
                ai_tasks_inserted: 0,
                ai_tasks_conflict: 0,
            })
        }
        BackfillTarget::Ai => {
            let category = super::ai_run::select_category(cli, &categories)?;
            let prompt_template = category
                .ai_override
                .as_ref()
                .and_then(|override_| override_.prompt_template.clone())
                .unwrap_or_else(|| "Summarize the following article.".to_string());
            let output_schema_version = ctx
                .rule_version_repo
                .get_or_create("ai_output_schema", "v1", "AI v1 schema", "v1")
                .await?;
            let options = build_backfill_ai_options(
                args,
                &loaded.app.ai.model,
                &prompt_template,
                output_schema_version,
                date_from,
                date_to,
                OffsetDateTime::now_utc(),
            );
            let tag = options.new_prompt_version_tag.clone();
            let model_id = options.model_id.clone();
            let summary = flow.ai(options).await?;
            Ok(BackfillCommandSummary {
                target: "ai".to_string(),
                date_from: args.date_from.clone(),
                date_to: args.date_to.clone(),
                feed_entries_examined: 0,
                feed_entries_reset: 0,
                new_prompt_version_id: Some(summary.new_prompt_version_id),
                new_prompt_version_tag: Some(tag),
                model_id: Some(model_id),
                articles_scanned: summary.articles_scanned,
                ai_tasks_inserted: summary.ai_tasks_inserted,
                ai_tasks_conflict: summary.ai_tasks_conflict,
            })
        }
    }
}

/// 把 `BackfillAiOptions` 构造抽成纯函数，方便单测覆盖 override 字段的
/// 回落路径（避免在 cargo test 里启 DB / runtime）。
///
/// 回落规则（参考 W2-A-6 audit）：
///   - tag    : args.prompt_version_tag.unwrap_or("backfill-<unix-ts>")
///   - desc   : args.prompt_version_description.unwrap_or("manual backfill via CLI")
///   - model  : args.model.unwrap_or(loaded_model)
///   - sha256 : 永远从 prompt_template 派生（同一 prompt 内容 → 同一 sha；
///              在不同 tag 下创建新版本但 sha 一致，保留 W3 后续做内容
///              指纹对比的能力）
pub fn build_backfill_ai_options(
    args: &BackfillArgs,
    loaded_model: &str,
    prompt_template: &str,
    output_schema_version: i64,
    date_from: Option<OffsetDateTime>,
    date_to: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> BackfillAiOptions {
    let tag = args
        .prompt_version_tag
        .clone()
        .unwrap_or_else(|| format!("backfill-{}", now.unix_timestamp()));
    let description = args
        .prompt_version_description
        .clone()
        .unwrap_or_else(|| "manual backfill via CLI".to_string());
    let model_id = args
        .model
        .clone()
        .unwrap_or_else(|| loaded_model.to_string());
    BackfillAiOptions {
        date_from,
        date_to,
        batch_size: args.batch_size,
        new_prompt_version_tag: tag,
        new_prompt_version_sha256: sha256_hex(prompt_template.as_bytes()),
        new_prompt_version_description: description,
        model_id,
        output_schema_version,
    }
}

pub fn parse_date_start(value: Option<&str>) -> Result<Option<OffsetDateTime>, CliError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let mut parts = value.split('-');
    let year = parts.next().and_then(|v| v.parse::<i32>().ok());
    let month = parts.next().and_then(|v| v.parse::<u8>().ok());
    let day = parts.next().and_then(|v| v.parse::<u8>().ok());
    if parts.next().is_some() {
        return Err(invalid_date(value));
    }
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Err(invalid_date(value));
    };
    let month = Month::try_from(month).map_err(|_| invalid_date(value))?;
    let date = Date::from_calendar_date(year, month, day).map_err(|_| invalid_date(value))?;
    Ok(Some(
        PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc(),
    ))
}

fn invalid_date(value: &str) -> CliError {
    CliError::Runtime(RuntimeError::Config(format!(
        "invalid date {value:?}; expected YYYY-MM-DD"
    )))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn args_with_target(target: BackfillTarget) -> BackfillArgs {
        BackfillArgs {
            target,
            date_from: None,
            date_to: None,
            batch_size: 50,
            prompt_version_tag: None,
            prompt_version_description: None,
            model: None,
        }
    }

    #[test]
    fn ai_options_fall_back_to_generated_tag_when_args_omit_it() {
        // 缺省路径：tag = "backfill-<unix-ts>"（与历史行为一致）。
        let args = args_with_target(BackfillTarget::Ai);
        let now = datetime!(2026-05-11 0:00 UTC);
        let options = build_backfill_ai_options(
            &args,
            "config-model",
            "prompt template body",
            42,
            None,
            None,
            now,
        );
        assert_eq!(
            options.new_prompt_version_tag,
            format!("backfill-{}", now.unix_timestamp())
        );
        assert_eq!(
            options.new_prompt_version_description,
            "manual backfill via CLI"
        );
        assert_eq!(options.model_id, "config-model");
    }

    #[test]
    fn ai_options_use_args_overrides_when_present() {
        // F5-7 主路径：三个 override 字段全部由 CLI 提供，应原样落到
        // BackfillAiOptions（state-machine §4.4 line 262 多版本并存的入口）。
        let args = BackfillArgs {
            target: BackfillTarget::Ai,
            date_from: None,
            date_to: None,
            batch_size: 50,
            prompt_version_tag: Some("exp-A".to_string()),
            prompt_version_description: Some("prompt v2 rerun".to_string()),
            model: Some("gpt-4o".to_string()),
        };
        let options = build_backfill_ai_options(
            &args,
            "config-model",
            "template",
            42,
            None,
            None,
            datetime!(2026-05-11 0:00 UTC),
        );
        assert_eq!(options.new_prompt_version_tag, "exp-A");
        assert_eq!(options.new_prompt_version_description, "prompt v2 rerun");
        assert_eq!(options.model_id, "gpt-4o");
    }

    #[test]
    fn ai_options_sha256_derived_from_prompt_template_regardless_of_tag_override() {
        // 同一 prompt body → 同一 sha256，即使 tag 不同。保留 W3 后续做
        // 「同内容不同标签」对比的能力。
        let body = "stable prompt body";
        let now = datetime!(2026-05-11 0:00 UTC);
        let with_tag = BackfillArgs {
            prompt_version_tag: Some("custom-tag".to_string()),
            ..args_with_target(BackfillTarget::Ai)
        };
        let without_tag = args_with_target(BackfillTarget::Ai);
        let a = build_backfill_ai_options(&with_tag, "m", body, 1, None, None, now);
        let b = build_backfill_ai_options(&without_tag, "m", body, 1, None, None, now);
        assert_eq!(a.new_prompt_version_sha256, b.new_prompt_version_sha256);
        assert_ne!(a.new_prompt_version_tag, b.new_prompt_version_tag);
    }

    #[test]
    fn summary_pretty_shows_tag_and_model_when_present() {
        // pretty 输出应让 operator 一眼看到这次实验落到了哪个 tag、哪个 model。
        let summary = BackfillCommandSummary {
            target: "ai".to_string(),
            date_from: None,
            date_to: None,
            feed_entries_examined: 0,
            feed_entries_reset: 0,
            new_prompt_version_id: Some(7),
            new_prompt_version_tag: Some("exp-A".to_string()),
            model_id: Some("gpt-4o".to_string()),
            articles_scanned: 5,
            ai_tasks_inserted: 5,
            ai_tasks_conflict: 0,
        };
        let mut buf = Vec::new();
        summary.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Prompt version tag:    exp-A"));
        assert!(text.contains("Model:                 gpt-4o"));
    }

    #[test]
    fn summary_pretty_omits_tag_and_model_for_extract_target() {
        // --target extract 不创建新版本；这两行不应出现。
        let summary = BackfillCommandSummary {
            target: "extract".to_string(),
            date_from: None,
            date_to: None,
            feed_entries_examined: 3,
            feed_entries_reset: 2,
            new_prompt_version_id: None,
            new_prompt_version_tag: None,
            model_id: None,
            articles_scanned: 0,
            ai_tasks_inserted: 0,
            ai_tasks_conflict: 0,
        };
        let mut buf = Vec::new();
        summary.render_pretty(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(!text.contains("Prompt version tag"));
        assert!(!text.contains("Model:"));
    }
}
