use rss_ai_news_storage::RuleVersionRepository;
use std::{
    io::{self, Write},
    path::PathBuf,
};

use rss_ai_news_config::{self as config, CategoryConfig};
use rss_ai_news_runtime::{PublishFlow, RebuildReportFlow, RebuildReportOptions, RuntimeError};
use serde::Serialize;

use crate::{
    args::{Cli, RebuildReportArgs},
    commands::backfill::parse_date_start,
    context_factory::{build_rebuild_report_deps, open_read_storage},
    error::CliError,
    output::CommandSummary,
};

#[derive(Debug, Clone, Serialize)]
pub struct RebuildReportCommandSummary {
    pub publish_record_id: i64,
    pub category: String,
    pub date: String,
    pub output_path: Option<String>,
    pub markdown_bytes: u32,
    pub items: u32,
}

impl CommandSummary for RebuildReportCommandSummary {
    fn render_pretty(&self, writer: &mut dyn Write) -> io::Result<()> {
        writeln!(writer, "Rebuild report completed:")?;
        writeln!(writer, "  Publish record: {}", self.publish_record_id)?;
        writeln!(writer, "  Category:       {}", self.category)?;
        writeln!(writer, "  Date:           {}", self.date)?;
        writeln!(writer, "  Markdown bytes: {}", self.markdown_bytes)?;
        if let Some(path) = &self.output_path {
            writeln!(writer, "  Output:         {path}")?;
        }
        Ok(())
    }
}

pub async fn run(
    cli: &Cli,
    args: &RebuildReportArgs,
) -> Result<RebuildReportCommandSummary, CliError> {
    let loaded = config::load_skip_env_checks(&cli.config_dir, None, cli.to_cli_overrides())?;
    let categories: Vec<CategoryConfig> = loaded.categories_filtered().cloned().collect();
    let pool = open_read_storage(&loaded).await?;
    let ctx = build_rebuild_report_deps(&loaded, &pool)?;
    let rule_version_repo = rss_ai_news_storage::RuleVersionRepo::new_with_storage(pool.clone());

    let record = if let Some(id) = args.publish_id {
        ctx.publish_record_repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| CliError::PublishRecordNotFound {
                idempotency_key: format!("id:{id}"),
            })?
    } else {
        let date = args.date.as_deref().ok_or_else(|| {
            CliError::Runtime(RuntimeError::Config(
                "rebuild-report requires --publish-id or --date with --category".to_string(),
            ))
        })?;
        let _ = parse_date_start(Some(date))?;
        let category = super::ai_run::select_category(cli, &categories)?;
        // F15-3: rebuild-report 仅用 render_version 重建 idempotency key，
        // 只读 active_rule；缺失时明确失败，不自动 seed。force 模式由 publish 命令
        // 单独负责，不在 rebuild-report 复制语义。
        let render_version = rule_version_repo
            .active_rule("render")
            .await?
            .ok_or_else(|| CliError::Runtime(RuntimeError::Config("no active render rule".into())))?
            .id;
        let key = PublishFlow::build_idempotency_key(&category.category.key, date, render_version);
        ctx.publish_record_repo
            .find_by_idempotency_key(&key)
            .await?
            .ok_or_else(|| CliError::PublishRecordNotFound {
                idempotency_key: key,
            })?
    };

    let category = categories
        .iter()
        .find(|cat| cat.category.key == record.category_key)
        .ok_or_else(|| {
            CliError::Runtime(RuntimeError::Config(format!(
                "category {} not found",
                record.category_key
            )))
        })?;
    let report = RebuildReportFlow::new(ctx.clone())
        .rebuild(RebuildReportOptions {
            publish_record_id: record.id,
            category_display_name: category.category.display_name.clone(),
            report_title: format!("{} {}", category.category.display_name, record.report_date),
            generated_at_override: None,
        })
        .await?;

    let items = ctx
        .publish_item_repo
        .list_by_publish_record(record.id)
        .await?
        .len();
    let output_path = write_or_stdout(args.output.clone(), &report.markdown_content)?;
    Ok(RebuildReportCommandSummary {
        publish_record_id: record.id,
        category: record.category_key,
        date: record.report_date,
        output_path,
        markdown_bytes: u32::try_from(report.markdown_content.len()).unwrap_or(u32::MAX),
        items: u32::try_from(items).unwrap_or(u32::MAX),
    })
}

fn write_or_stdout(path: Option<PathBuf>, markdown: &str) -> Result<Option<String>, CliError> {
    if let Some(path) = path {
        std::fs::write(&path, markdown)?;
        Ok(Some(path.display().to_string()))
    } else {
        println!("{markdown}");
        Ok(None)
    }
}
