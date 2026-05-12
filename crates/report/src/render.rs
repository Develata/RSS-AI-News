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
    out.push_str(&format!(
        "\n# {}\n\n",
        escape_markdown_text(&config.report_title)
    ));
    out.push_str(&format!(
        "分类：{}\n\n",
        escape_markdown_text(&config.category_display_name)
    ));
    for item in items {
        out.push_str(&format!(
            "## {}\n\n",
            escape_markdown_text(&item.frozen_title)
        ));
        if let Some(score) = item.frozen_score {
            out.push_str(&format!("- 重要度：{}\n", score.get()));
        }
        if !item.frozen_tags_json.is_empty() && item.frozen_tags_json != "[]" {
            out.push_str(&format!("- 标签：{}\n", item.frozen_tags_json));
        }
        out.push_str(&format!(
            "- 来源：{}\n",
            escape_markdown_text(&item.frozen_source_display_name)
        ));
        out.push_str(&format!(
            "- 链接：<{0}>\n\n",
            escape_markdown_angle_url(&item.frozen_canonical_link)
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

/// 转义 CommonMark 行内文本中的 ASCII 标点，避免 frozen_title / source 等
/// 用户/上游内容触发意外的 Markdown 解释（虚假标题 / 斜体 / 链接破坏等）。
///
/// 仅转义 CommonMark §6 Inlines 列出的 ASCII 标点子集；非 ASCII 字符（含
/// 中文标点、emoji）原样保留——它们没有 Markdown 语义。
fn escape_markdown_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(
            ch,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '!'
                | '<'
                | '>'
                | '|'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// 处理放入 `<...>` autolink 形式的 URL。`<` 和 `>` 若出现在 URL 内会
/// 破坏 autolink 边界——按 RFC 3986 它们本不该出现，但作为防御 percent-
/// encode 一下。其余字符原样保留（不破坏 valid URL 的 percent encoding）。
fn escape_markdown_angle_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        match ch {
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            _ => out.push(ch),
        }
    }
    out
}
