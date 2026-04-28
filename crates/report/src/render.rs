use rss_ai_news_domain::dto::publish::{FrozenPublishItem, RenderedReport};

use crate::error::ReportError;
use crate::frontmatter::build_frontmatter;

pub struct RenderConfig {
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: time::OffsetDateTime,
}

pub fn render_markdown(
    publish_record_id: i64,
    category_key: &str,
    report_date: &str,
    items: &[FrozenPublishItem],
    config: &RenderConfig,
) -> Result<RenderedReport, ReportError> {
    let mut out = String::new();
    out.push_str(&build_frontmatter(
        &config.report_title,
        report_date,
        category_key,
        config.generated_at,
    ));
    out.push_str(&format!("\n# {}\n\n", config.report_title));
    out.push_str(&format!("分类：{}\n\n", config.category_display_name));
    for item in items {
        out.push_str(&format!("## {}\n\n", item.frozen_title));
        if let Some(score) = item.frozen_score {
            out.push_str(&format!("- 重要度：{}\n", score.get()));
        }
        if !item.frozen_tags_json.is_empty() && item.frozen_tags_json != "[]" {
            out.push_str(&format!("- 标签：{}\n", item.frozen_tags_json));
        }
        out.push_str(&format!("- 来源：{}\n", item.frozen_source_display_name));
        out.push_str(&format!(
            "- 链接：[{0}]({0})\n\n",
            item.frozen_canonical_link
        ));
        out.push_str(&item.frozen_summary);
        out.push_str("\n\n---\n\n");
    }
    let relative_path = format!("archive/{category_key}/{report_date}.md");
    Ok(RenderedReport {
        publish_record_id,
        category_key: category_key.to_string(),
        report_date: report_date.to_string(),
        markdown_content: out,
        relative_path,
    })
}
