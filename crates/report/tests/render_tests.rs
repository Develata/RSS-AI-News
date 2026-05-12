use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::publish::FrozenPublishItem;
use rss_ai_news_report::{RenderConfig, render_markdown};
use time::OffsetDateTime;

#[test]
fn render_markdown_emits_frontmatter_then_heading_then_items() {
    let report = render_markdown(1, "ai", "2026-04-28", &[ai_item()], &config()).unwrap();

    assert!(
        report
            .markdown_content
            .starts_with("---\ntitle: Daily AI\n")
    );
    assert!(report.markdown_content.contains("\n# Daily AI\n\n"));
    assert!(report.markdown_content.contains("## Title\n\n"));
    assert!(report.markdown_content.contains("- 重要度：88\n"));
}

#[test]
fn render_markdown_with_ai_off_items_omits_score_and_tags() {
    let report = render_markdown(1, "ai", "2026-04-28", &[direct_item()], &config()).unwrap();

    assert!(!report.markdown_content.contains("重要度"));
    assert!(!report.markdown_content.contains("标签"));
    assert!(report.markdown_content.contains("- 来源：Source\n"));
}

#[test]
fn render_markdown_uses_category_path_in_relative_path() {
    let report = render_markdown(1, "ai", "2026-04-28", &[ai_item()], &config()).unwrap();

    assert_eq!(report.relative_path, "archive/ai/2026-04-28.md");
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
            .contains("## \\# Fake heading \\* with \\_markdown\\_ \\[chars\\]\n"),
        "title must escape Markdown control chars; got:\n{}",
        report.markdown_content
    );
    assert!(
        report
            .markdown_content
            .contains("- 来源：Source \\*with\\* \\#hash\n"),
        "source must escape Markdown control chars; got:\n{}",
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
            .contains("- 链接：<https://example.com/path(v2)>\n"),
        "URL with parens must be wrapped as autolink not as inline-link target; got:\n{}",
        report.markdown_content
    );
}

fn config() -> RenderConfig {
    RenderConfig {
        category_display_name: "AI".to_string(),
        report_title: "Daily AI".to_string(),
        generated_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
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
