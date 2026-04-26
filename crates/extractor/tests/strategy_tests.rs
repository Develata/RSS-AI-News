use std::time::Duration;

use rss_ai_news_domain::state::{ContentQuality, ExtractorStrategy};
use rss_ai_news_extractor::{
    ArticleFetchTask, ContentStrategy, ExtractorError, ReadabilityStrategy, summary_fallback,
};

fn task(summary_raw: Option<String>) -> ArticleFetchTask {
    ArticleFetchTask {
        feed_entry_id: 7,
        normalized_link: "https://example.com/article".to_string(),
        title_raw: "Fallback Title".to_string(),
        summary_raw,
        timeout: Duration::from_secs(2),
    }
}

#[test]
fn readability_extracts_simple_article() {
    let html = include_bytes!("fixtures/simple_article.html");
    let article = ReadabilityStrategy
        .extract(&task(None), html, "https://example.com/final")
        .expect("simple article should extract");

    assert_eq!(article.feed_entry_id, 7);
    assert_eq!(article.canonical_link, "https://example.com/final");
    assert_eq!(article.extractor_strategy, ExtractorStrategy::Readability);
    assert!(matches!(
        article.content_quality,
        ContentQuality::High | ContentQuality::Medium
    ));
    assert!(article.word_count > 100);
    assert!(!article.title.trim().is_empty());
    assert_eq!(article.content_hash.len(), 64);
    assert!(article.body_html.is_some());
    assert!(
        article
            .body_text
            .contains("deterministic article extraction")
    );
    assert!(!article.body_text.contains("Login"));
}

#[test]
fn readability_returns_content_too_short_for_short_article() {
    let html = include_bytes!("fixtures/short_article.html");
    let err = ReadabilityStrategy
        .extract(&task(None), html, "https://example.com/final")
        .expect_err("short article should fail quality threshold");

    assert!(matches!(err, ExtractorError::ContentTooShort { .. }));
}

#[test]
fn readability_returns_parse_failed_for_no_content() {
    let html = include_bytes!("fixtures/no_content.html");
    let err = ReadabilityStrategy
        .extract(&task(None), html, "https://example.com/final")
        .expect_err("navigation-only page should fail parse");

    assert!(matches!(err, ExtractorError::ParseFailed { .. }));
}

#[test]
fn summary_fallback_uses_summary_raw_when_present() {
    let fallback = summary_fallback(&task(Some(
        "<p>Summary text with <strong>HTML</strong> tags and enough context.</p>".to_string(),
    )))
    .expect("summary should produce fallback article");

    assert_eq!(fallback.feed_entry_id, 7);
    assert_eq!(fallback.canonical_link, "https://example.com/article");
    assert_eq!(fallback.title, "Fallback Title");
    assert_eq!(fallback.content_quality, ContentQuality::Fallback);
    assert_eq!(
        fallback.body_text,
        "Summary text with HTML tags and enough context."
    );
    assert_eq!(fallback.word_count, 8);
    assert_eq!(fallback.content_hash.len(), 64);
}

#[test]
fn summary_fallback_returns_none_when_summary_empty_or_only_html_tags() {
    assert!(summary_fallback(&task(None)).is_none());
    assert!(summary_fallback(&task(Some("   ".to_string()))).is_none());
    assert!(summary_fallback(&task(Some("<p><span></span></p>".to_string()))).is_none());
}
