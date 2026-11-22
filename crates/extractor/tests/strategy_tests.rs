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

/// 117 chars per paragraph (108 Han ideographs + full-width punctuation).
const ZH_PARAGRAPH: &str = "人工智能研究在过去十年取得了显著进展，大型语言模型能够完成翻译、摘要、问答等多种任务，同时也带来了关于安全性、可解释性与能源消耗的广泛讨论。研究者正在探索更高效的训练方法与评估基准，以便更准确地衡量模型能力的边界与风险。以及更多讨论。";

fn zh_page(paragraphs: usize) -> String {
    let body: String = (0..paragraphs)
        .map(|_| format!("<p>{ZH_PARAGRAPH}</p>"))
        .collect();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>测试文章标题</title></head>\
         <body><nav>导航</nav><article><h1>测试文章标题</h1>{body}</article><footer>页脚</footer></body></html>"
    )
}

#[test]
fn readability_grades_a_2340_char_chinese_article_high() {
    assert_eq!(ZH_PARAGRAPH.chars().count() * 20, 2340);
    let html = zh_page(20);
    let article = ReadabilityStrategy
        .extract(&task(None), html.as_bytes(), "https://example.com/final")
        .expect("a 2340-char Chinese article must not be ContentTooShort");

    assert_eq!(article.content_quality, ContentQuality::High);
    // (20 paragraphs × 108 + the 6-character <h1>) Han characters, two per word.
    assert_eq!(article.word_count, 1083);
}

#[test]
fn content_too_short_reports_characters_not_bytes() {
    // One paragraph: 54 words, below the Medium threshold.
    let html = zh_page(1);
    let err = ReadabilityStrategy
        .extract(&task(None), html.as_bytes(), "https://example.com/final")
        .expect_err("one Chinese paragraph is too short");

    let ExtractorError::ContentTooShort { chars } = err else {
        panic!("expected ContentTooShort, got {err:?}");
    };
    // UTF-8 spends 3 bytes per Han character; the field counts characters.
    assert!(
        (117..200).contains(&chars),
        "chars should count characters, got {chars}"
    );
}

#[test]
fn summary_fallback_counts_cjk_words() {
    let fallback = summary_fallback(&task(Some(ZH_PARAGRAPH.to_string())))
        .expect("Chinese summary should produce a fallback article");
    assert_eq!(fallback.word_count, 54);
}
