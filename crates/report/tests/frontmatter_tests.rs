use rss_ai_news_report::build_frontmatter;

#[test]
fn frontmatter_emits_yaml_with_required_fields() {
    let out = build_frontmatter("2026-04-28", "2026-04-28", "Today summary");

    assert!(out.starts_with("---\ntitle: 2026-04-28\n"));
    assert!(out.contains("date: 2026-04-28\n"));
    assert!(out.contains("excerpt: Today summary\n"));
    assert!(out.ends_with("---\n"));
}

#[test]
fn frontmatter_quotes_titles_with_special_chars() {
    let out = build_frontmatter("AI: \"News\"", "2026-04-28", "Summary: \"quoted\"");

    assert!(out.contains("title: \"AI: \\\"News\\\"\"\n"));
    assert!(out.contains("excerpt: \"Summary: \\\"quoted\\\"\"\n"));
}

#[test]
fn frontmatter_escapes_yaml_control_sequences() {
    let out = build_frontmatter("AI\\News", "2026-04-28", "line1\nline2\tend");

    assert!(out.contains("title: \"AI\\\\News\"\n"));
    assert!(out.contains("excerpt: \"line1\\nline2\\tend\"\n"));
}
