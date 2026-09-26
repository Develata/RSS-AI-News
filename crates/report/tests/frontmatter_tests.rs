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

#[test]
fn frontmatter_quotes_values_yaml_would_misread() {
    for (value, expected) in [
        ("[AI]", "\"[AI]\""),
        ("*missing", "\"*missing\""),
        ("true", "\"true\""),
        ("No", "\"No\""),
        ("null", "\"null\""),
        ("123", "\"123\""),
        ("-1.5", "\"-1.5\""),
        ("0x1F", "\"0x1F\""),
        ("", "\"\""),
        (" padded", "\" padded\""),
        ("{a}", "\"{a}\""),
        ("1_000", "\"1_000\""),
        ("0b101", "\"0b101\""),
        ("-", "\"-\""),
        ("- item", "\"- item\""),
        ("nul\u{0}byte", "\"nul\\u0000byte\""),
    ] {
        let out = build_frontmatter("T", "2026-04-28", value);
        assert!(
            out.contains(&format!("excerpt: {expected}\n")),
            "{value:?} -> {out}"
        );
    }
}

#[test]
fn frontmatter_keeps_already_valid_plain_values_byte_identical() {
    let out = build_frontmatter("AI 日报 2026-04-28", "2026-04-28", "Today - summary 1");
    assert!(out.contains("title: AI 日报 2026-04-28\n"));
    assert!(out.contains("excerpt: Today - summary 1\n"));
    // Valid plain scalars starting with `-` / `?` / `.` and plain words that
    // Rust (not YAML) would parse as floats.
    for value in [
        "-update",
        "?help",
        ".net",
        "inf",
        "nan",
        "2026-04-28",
        "v1.2",
    ] {
        let out = build_frontmatter("T", "2026-04-28", value);
        assert!(
            out.contains(&format!("excerpt: {value}\n")),
            "{value:?} -> {out}"
        );
    }
}
