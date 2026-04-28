use rss_ai_news_report::build_frontmatter;
use time::OffsetDateTime;

#[test]
fn frontmatter_emits_yaml_with_required_fields() {
    let out = build_frontmatter("AI News", "2026-04-28", "ai", fixed_time());

    assert!(out.starts_with("---\ntitle: AI News\n"));
    assert!(out.contains("date: 2026-04-28\n"));
    assert!(out.contains("category: ai\n"));
    assert!(out.contains("generated_at: 1970-01-01T00:00:00.000000000Z\n"));
    assert!(out.ends_with("---\n"));
}

#[test]
fn frontmatter_quotes_titles_with_special_chars() {
    let out = build_frontmatter("AI: \"News\"", "2026-04-28", "ai", fixed_time());

    assert!(out.contains("title: \"AI: \\\"News\\\"\"\n"));
}

fn fixed_time() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(0).unwrap()
}
