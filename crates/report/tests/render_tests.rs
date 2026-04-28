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
