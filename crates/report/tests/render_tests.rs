use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::publish::FrozenPublishItem;
use rss_ai_news_report::{RenderConfig, RenderTemplates, render_markdown};
use time::OffsetDateTime;

#[test]
fn render_markdown_emits_frontmatter_then_heading_then_items() {
    let report = render_markdown(1, "ai", "2026-04-28", &[ai_item()], &config()).unwrap();

    assert!(
        report
            .markdown_content
            .starts_with("---\ntitle: 2026-04-28\n")
    );
    assert!(report.markdown_content.contains("\n# Daily AI\n"));
    assert!(
        report
            .markdown_content
            .contains("## Title <Badge type=\"tip\" text=\"88\" />\n\n")
    );
    assert!(report.markdown_content.contains("- **Tags:** `ai` \n\n"));
    assert!(
        report
            .markdown_content
            .contains("- **Source:** `Source` | [阅读原文](https://example.com)\n")
    );
    assert!(report.markdown_content.contains("> [摘要]  \n>Summary\n"));
}

#[test]
fn render_markdown_with_ai_off_items_omits_score_and_tags() {
    let report = render_markdown(1, "ai", "2026-04-28", &[direct_item()], &config()).unwrap();

    assert!(!report.markdown_content.contains("<Badge"));
    assert!(!report.markdown_content.contains("**Tags:**"));
    assert!(
        report
            .markdown_content
            .contains("- **Source:** `Source` | [阅读原文](https://example.com)\n")
    );
}

#[test]
fn render_markdown_uses_category_path_in_relative_path() {
    let report = render_markdown(1, "ai_ml", "2026-04-28", &[ai_item()], &config()).unwrap();

    assert_eq!(report.relative_path, "AI_ML/2026/20260428.md");
}

#[test]
fn render_markdown_rejects_non_numeric_report_date() {
    let error = render_markdown(1, "ai_ml", "2026-aa-28", &[ai_item()], &config())
        .expect_err("non-numeric date must fail");

    assert!(error.to_string().contains("invalid report date"));
}

#[test]
fn render_markdown_rejects_invalid_calendar_date() {
    let error = render_markdown(1, "ai_ml", "2026-02-30", &[ai_item()], &config())
        .expect_err("invalid calendar date must fail");

    assert!(error.to_string().contains("invalid report date"));
}

#[test]
fn render_markdown_escapes_markdown_special_chars_in_title_and_source() {
    let item = FrozenPublishItem::try_new(
        1,
        10,
        Some(100),
        "# Fake heading * with _markdown_ [chars]".to_string(),
        "Summary".to_string(),
        "[]".to_string(),
        Some(Score0To100::try_new(50).unwrap()),
        "https://example.com".to_string(),
        "Source *with* #hash".to_string(),
    )
    .unwrap();
    let report = render_markdown(1, "ai", "2026-04-28", &[item], &config()).unwrap();

    assert!(
        report
            .markdown_content
            .contains("## \\# Fake heading \\* with \\_markdown\\_ \\[chars\\] <Badge type=\"tip\" text=\"50\" />\n"),
        "title must escape Markdown control chars; got:\n{}",
        report.markdown_content
    );
    assert!(
        report
            .markdown_content
            .contains("- **Source:** `Source *with* #hash` | [阅读原文](https://example.com)\n"),
        "source must be rendered as code span; got:\n{}",
        report.markdown_content
    );
}

#[test]
fn render_markdown_emits_autolink_for_canonical_link_with_parens() {
    let item = FrozenPublishItem::try_new(
        1,
        10,
        Some(100),
        "Title".to_string(),
        "Summary".to_string(),
        "[]".to_string(),
        Some(Score0To100::try_new(50).unwrap()),
        "https://example.com/path(v2)".to_string(),
        "Source".to_string(),
    )
    .unwrap();
    let report = render_markdown(1, "ai", "2026-04-28", &[item], &config()).unwrap();

    assert!(
        report
            .markdown_content
            .contains("- **Source:** `Source` | [阅读原文](https://example.com/path%28v2%29)\n"),
        "URL with parens must escape parens in inline-link target; got:\n{}",
        report.markdown_content
    );
}

#[test]
fn render_markdown_does_not_expand_placeholders_from_article_content() {
    let item = FrozenPublishItem::try_new(
        1,
        10,
        Some(100),
        "Literal {date} token".to_string(),
        "Summary mentions {items} and {score_badge} literally.".to_string(),
        "[\"ai\"]".to_string(),
        Some(Score0To100::try_new(50).unwrap()),
        "https://example.com".to_string(),
        "Source".to_string(),
    )
    .unwrap();
    let report = render_markdown(1, "ai", "2026-04-28", &[item], &config()).unwrap();

    assert!(
        report
            .markdown_content
            .contains("## Literal \\{date\\} token <Badge type=\"tip\" text=\"50\" />"),
        "article title placeholders must remain literal; got:\n{}",
        report.markdown_content
    );
    assert!(
        report
            .markdown_content
            .contains(">Summary mentions {items} and {score_badge} literally."),
        "article summary placeholders must remain literal; got:\n{}",
        report.markdown_content
    );
}

fn config() -> RenderConfig {
    RenderConfig {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        templates: RenderTemplates::default(),
    }
}

fn ai_item() -> FrozenPublishItem {
    FrozenPublishItem::try_new(
        1,
        10,
        Some(100),
        "Title".to_string(),
        "Summary".to_string(),
        "[\"ai\"]".to_string(),
        Some(Score0To100::try_new(88).unwrap()),
        "https://example.com".to_string(),
        "Source".to_string(),
    )
    .unwrap()
}

fn direct_item() -> FrozenPublishItem {
    FrozenPublishItem::try_new(
        1,
        10,
        None,
        "Title".to_string(),
        "Summary".to_string(),
        "[]".to_string(),
        None,
        "https://example.com".to_string(),
        "Source".to_string(),
    )
    .unwrap()
}
