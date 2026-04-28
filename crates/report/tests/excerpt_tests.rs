use rss_ai_news_report::generate_excerpt;

#[test]
fn excerpt_returns_input_when_under_max() {
    assert_eq!(generate_excerpt("short", 10), "short");
}

#[test]
fn excerpt_truncates_with_ellipsis_when_over_max() {
    assert_eq!(generate_excerpt("abcdef", 4), "abc…");
}

#[test]
fn excerpt_handles_unicode_chars_safely() {
    assert_eq!(generate_excerpt("中文测试文本", 4), "中文测…");
}
