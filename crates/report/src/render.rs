use rss_ai_news_domain::dto::publish::{FrozenPublishItem, RenderedReport};
use time::{Date, Month};

use crate::error::ReportError;
use crate::frontmatter::yaml_escape;

pub struct RenderConfig {
    pub category_display_name: String,
    pub report_title: String,
    pub generated_at: time::OffsetDateTime,
    pub templates: RenderTemplates,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderTemplates {
    pub path_template: String,
    pub frontmatter_template: String,
    pub report_template: String,
    pub item_template: String,
}

impl Default for RenderTemplates {
    fn default() -> Self {
        Self {
            path_template: "{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md".to_string(),
            frontmatter_template: "---\ntitle: {date}\ndate: {date}\nexcerpt: {excerpt_yaml}\n---\n"
                .to_string(),
            report_template: "{frontmatter}\n# {title_md}\n{excerpt_block}\n{items}".to_string(),
            item_template: "## {item_title_md}{score_badge}\n\n{tags_block}- **Source:** `{source_code}` | [阅读原文]({url_md})\n\n> [摘要]  \n{summary_blockquote}\n\n---\n\n".to_string(),
        }
    }
}

pub fn render_markdown(
    publish_record_id: i64,
    category_key: &str,
    report_date: &str,
    items: &[FrozenPublishItem],
    config: &RenderConfig,
) -> Result<RenderedReport, ReportError> {
    let excerpt = report_excerpt(items);
    let parts = DateParts::parse(report_date)?;
    let frontmatter = render_frontmatter(
        &config.templates.frontmatter_template,
        report_date,
        &excerpt,
        &parts,
    );
    let items_markdown = items
        .iter()
        .map(|item| render_item(&config.templates.item_template, item))
        .collect::<Result<Vec<_>, _>>()?
        .join("");
    let excerpt_block = if excerpt.is_empty() {
        String::new()
    } else {
        format!("> {excerpt}\n")
    };
    let generated_at = config
        .generated_at
        .format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_default();
    let out = apply_template(
        &config.templates.report_template,
        &[
            ("frontmatter", frontmatter),
            ("title", config.report_title.clone()),
            ("title_md", escape_markdown_text(&config.report_title)),
            ("date", report_date.to_string()),
            ("YYYY", parts.year.clone()),
            ("MM", parts.month.clone()),
            ("DD", parts.day.clone()),
            ("YYYYMMDD", parts.compact.clone()),
            ("category_key", category_key.to_string()),
            ("CATEGORY_KEY", category_key.to_ascii_uppercase()),
            (
                "category_display_name",
                config.category_display_name.clone(),
            ),
            (
                "category_display_name_md",
                escape_markdown_text(&config.category_display_name),
            ),
            ("excerpt", excerpt.clone()),
            ("excerpt_yaml", yaml_escape(&excerpt)),
            ("excerpt_block", excerpt_block),
            ("items", items_markdown),
            ("generated_at", generated_at),
        ],
    );
    let category_dir = category_key.to_ascii_uppercase();
    let relative_path = apply_template(
        &config.templates.path_template,
        &[
            ("category_key", category_key.to_string()),
            ("CATEGORY_KEY", category_dir),
            ("date", report_date.to_string()),
            ("YYYY", parts.year.to_string()),
            ("MM", parts.month.to_string()),
            ("DD", parts.day.to_string()),
            ("YYYYMMDD", parts.compact),
        ],
    );
    Ok(RenderedReport {
        publish_record_id,
        category_key: category_key.to_string(),
        report_date: report_date.to_string(),
        markdown_content: out,
        relative_path,
    })
}

fn render_frontmatter(
    template: &str,
    report_date: &str,
    excerpt: &str,
    parts: &DateParts,
) -> String {
    apply_template(
        template,
        &[
            ("title", report_date.to_string()),
            ("title_yaml", yaml_escape(report_date)),
            ("date", report_date.to_string()),
            ("YYYY", parts.year.clone()),
            ("MM", parts.month.clone()),
            ("DD", parts.day.clone()),
            ("YYYYMMDD", parts.compact.clone()),
            ("excerpt", excerpt.to_string()),
            ("excerpt_yaml", yaml_escape(excerpt)),
        ],
    )
}

fn render_item(template: &str, item: &FrozenPublishItem) -> Result<String, ReportError> {
    let score = item
        .frozen_score
        .map(|score| score.get().to_string())
        .unwrap_or_default();
    let score_badge = if score.is_empty() {
        String::new()
    } else {
        format!(" <Badge type=\"tip\" text=\"{score}\" />")
    };
    let tags = render_tags(&item.frozen_tags_json)?;
    let tags_block = if tags.is_empty() {
        String::new()
    } else {
        format!("- **Tags:** {tags} \n\n")
    };
    Ok(apply_template(
        template,
        &[
            ("item_title", item.frozen_title.clone()),
            ("item_title_md", escape_markdown_text(&item.frozen_title)),
            ("score", score),
            ("score_badge", score_badge),
            ("tags", tags),
            ("tags_block", tags_block),
            ("source", item.frozen_source_display_name.clone()),
            (
                "source_md",
                escape_markdown_text(&item.frozen_source_display_name),
            ),
            (
                "source_code",
                escape_code_span(&item.frozen_source_display_name),
            ),
            ("url", item.frozen_canonical_link.clone()),
            (
                "url_md",
                escape_markdown_link_url(&item.frozen_canonical_link),
            ),
            ("summary", item.frozen_summary.clone()),
            ("summary_inline", normalize_inline(&item.frozen_summary)),
            (
                "summary_blockquote",
                blockquote_summary(&item.frozen_summary),
            ),
        ],
    ))
}

fn apply_template(template: &str, vars: &[(&str, String)]) -> String {
    let chars = template.char_indices().collect::<Vec<_>>();
    let mut out = String::with_capacity(template.len());
    let mut index = 0;
    while index < chars.len() {
        let (start_byte, ch) = chars[index];
        if ch != '{' {
            out.push(ch);
            index += 1;
            continue;
        }

        let Some(end_index) = chars[index + 1..]
            .iter()
            .position(|(_, candidate)| *candidate == '}')
            .map(|offset| index + 1 + offset)
        else {
            out.push_str(&template[start_byte..]);
            break;
        };
        let end_byte = chars[end_index].0;
        let name = &template[start_byte + 1..end_byte];
        if let Some((_, value)) = vars.iter().find(|(key, _)| *key == name) {
            out.push_str(value);
        } else {
            let after_end = end_byte + '}'.len_utf8();
            out.push_str(&template[start_byte..after_end]);
        }
        index = end_index + 1;
    }
    out
}

fn report_excerpt(items: &[FrozenPublishItem]) -> String {
    let mut excerpt = items
        .iter()
        .take(3)
        .map(|item| normalize_inline(&item.frozen_summary))
        .filter(|summary| !summary.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    const MAX_CHARS: usize = 180;
    if excerpt.chars().count() > MAX_CHARS {
        excerpt = excerpt.chars().take(MAX_CHARS).collect::<String>();
        excerpt.push('…');
    }
    excerpt
}

fn normalize_inline(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_tags(tags_json: &str) -> Result<String, ReportError> {
    let tags: Vec<String> = serde_json::from_str(tags_json)
        .map_err(|error| ReportError::InvalidTagsJson(error.to_string()))?;
    Ok(tags
        .into_iter()
        .filter(|tag| !tag.trim().is_empty())
        .map(|tag| format!("`{}`", escape_code_span(&tag)))
        .collect::<Vec<_>>()
        .join(" "))
}

fn blockquote_summary(summary: &str) -> String {
    summary
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                ">".to_string()
            } else {
                format!(">{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct DateParts {
    year: String,
    month: String,
    day: String,
    compact: String,
}

impl DateParts {
    fn parse(report_date: &str) -> Result<Self, ReportError> {
        let mut parts = report_date.split('-');
        let (Some(year), Some(month), Some(day), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ReportError::RenderFailed(format!(
                "invalid report date {report_date:?}"
            )));
        };
        if year.len() != 4
            || month.len() != 2
            || day.len() != 2
            || !year.chars().all(|ch| ch.is_ascii_digit())
            || !month.chars().all(|ch| ch.is_ascii_digit())
            || !day.chars().all(|ch| ch.is_ascii_digit())
        {
            return Err(ReportError::RenderFailed(format!(
                "invalid report date {report_date:?}"
            )));
        }
        let month_number = month
            .parse::<u8>()
            .map_err(|_| invalid_report_date(report_date))?;
        let day_number = day
            .parse::<u8>()
            .map_err(|_| invalid_report_date(report_date))?;
        let year_number = year
            .parse::<i32>()
            .map_err(|_| invalid_report_date(report_date))?;
        let month_value =
            Month::try_from(month_number).map_err(|_| invalid_report_date(report_date))?;
        Date::from_calendar_date(year_number, month_value, day_number)
            .map_err(|_| invalid_report_date(report_date))?;
        Ok(Self {
            year: year.to_string(),
            month: month.to_string(),
            day: day.to_string(),
            compact: format!("{year}{month}{day}"),
        })
    }
}

fn invalid_report_date(report_date: &str) -> ReportError {
    ReportError::RenderFailed(format!("invalid report date {report_date:?}"))
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

fn escape_markdown_link_url(input: &str) -> String {
    input
        .replace('\\', "%5C")
        .replace(' ', "%20")
        .replace('<', "%3C")
        .replace('>', "%3E")
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('\n', "")
}

fn escape_code_span(input: &str) -> String {
    input.replace('`', "\\`").replace('\n', " ")
}
