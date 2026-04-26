//! HTML-to-article extraction strategies.

use readability_rust::{Readability, ReadabilityOptions};
use rss_ai_news_domain::dto::extract::{ArticleFetchTask, ExtractedArticle, FallbackArticle};
use rss_ai_news_domain::state::{ContentQuality, ExtractorStrategy};
use scraper::{Html, Selector};
use sha2::{Digest, Sha256};

use crate::error::ExtractorError;

pub trait ContentStrategy: Send + Sync {
    fn strategy(&self) -> ExtractorStrategy;

    /// Convert already-fetched HTML bytes into an article. This method performs
    /// no HTTP so runtime can persist the raw HTML artifact before parsing.
    fn extract(
        &self,
        task: &ArticleFetchTask,
        html_bytes: &[u8],
        final_url: &str,
    ) -> Result<ExtractedArticle, ExtractorError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReadabilityStrategy;

impl ContentStrategy for ReadabilityStrategy {
    fn strategy(&self) -> ExtractorStrategy {
        ExtractorStrategy::Readability
    }

    fn extract(
        &self,
        task: &ArticleFetchTask,
        html_bytes: &[u8],
        final_url: &str,
    ) -> Result<ExtractedArticle, ExtractorError> {
        let html = std::str::from_utf8(html_bytes).map_err(|err| ExtractorError::ParseFailed {
            reason: format!("HTML is not valid UTF-8: {err}"),
        })?;

        let mut parser = Readability::new_with_base_uri(
            html,
            final_url,
            Some(ReadabilityOptions {
                char_threshold: 25,
                ..Default::default()
            }),
        )
        .map_err(|err| ExtractorError::ParseFailed {
            reason: err.to_string(),
        })?;

        let article = parser.parse().ok_or_else(|| ExtractorError::ParseFailed {
            reason: "no readable content found".to_string(),
        })?;

        let body_html = article.content.filter(|content| !content.trim().is_empty());
        let body_text = match (article.text_content, body_html.as_deref()) {
            (Some(text), _) if !text.trim().is_empty() => normalize_text(&text),
            (_, Some(content)) => html_to_paragraph_text(content),
            _ => String::new(),
        };

        if body_text.trim().is_empty() {
            return Err(ExtractorError::ParseFailed {
                reason: "readability returned empty body".to_string(),
            });
        }

        let word_count = body_text.split_whitespace().count() as u32;
        let title = article
            .title
            .map(|title| normalize_text(&title))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| task.title_raw.clone());
        let has_title = !title.trim().is_empty();
        let content_quality = assess_quality(word_count, has_title).ok_or_else(|| {
            if !has_content_candidate(html) {
                ExtractorError::ParseFailed {
                    reason: "no article content candidate found".to_string(),
                }
            } else {
                ExtractorError::ContentTooShort {
                    chars: body_text.len() as u32,
                }
            }
        })?;
        let content_hash = sha256_hex(body_text.as_bytes());

        Ok(ExtractedArticle {
            feed_entry_id: task.feed_entry_id,
            canonical_link: final_url.to_string(),
            title,
            body_text,
            body_html: body_html.map(String::into_bytes),
            extractor_strategy: ExtractorStrategy::Readability,
            content_quality,
            word_count,
            content_hash,
        })
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RuleStrategy;

impl ContentStrategy for RuleStrategy {
    fn strategy(&self) -> ExtractorStrategy {
        ExtractorStrategy::Rule
    }

    fn extract(
        &self,
        _task: &ArticleFetchTask,
        _html_bytes: &[u8],
        _final_url: &str,
    ) -> Result<ExtractedArticle, ExtractorError> {
        Err(ExtractorError::ParseFailed {
            reason: "rule strategy not implemented".to_string(),
        })
    }
}

pub fn summary_fallback(task: &ArticleFetchTask) -> Option<FallbackArticle> {
    let body_text = task.summary_raw.as_ref()?;
    if body_text.trim().is_empty() {
        return None;
    }

    let body_text = strip_html_tags(body_text);
    if body_text.trim().is_empty() {
        return None;
    }

    let word_count = body_text.split_whitespace().count() as u32;
    let content_hash = sha256_hex(body_text.as_bytes());

    Some(FallbackArticle {
        feed_entry_id: task.feed_entry_id,
        canonical_link: task.normalized_link.clone(),
        title: task.title_raw.clone(),
        body_text,
        content_quality: ContentQuality::Fallback,
        word_count,
        content_hash,
    })
}

fn assess_quality(word_count: u32, has_title: bool) -> Option<ContentQuality> {
    if word_count >= 300 && has_title {
        Some(ContentQuality::High)
    } else if word_count >= 100 {
        Some(ContentQuality::Medium)
    } else {
        None
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

fn strip_html_tags(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    normalize_text(&fragment.root_element().text().collect::<Vec<_>>().join(" "))
}

fn html_to_paragraph_text(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    let selector = Selector::parse("p").expect("static paragraph selector should parse");
    let paragraphs = fragment
        .select(&selector)
        .map(|element| normalize_text(&element.text().collect::<Vec<_>>().join(" ")))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();

    if paragraphs.is_empty() {
        strip_html_tags(html)
    } else {
        paragraphs.join("\n\n")
    }
}

fn has_content_candidate(html: &str) -> bool {
    let document = Html::parse_document(html);
    let selector =
        Selector::parse("article, main, [role=\"main\"], p").expect("static selector should parse");
    document.select(&selector).next().is_some()
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_thresholds_match_w6a_contract() {
        assert_eq!(assess_quality(299, true), Some(ContentQuality::Medium));
        assert_eq!(assess_quality(300, true), Some(ContentQuality::High));
        assert_eq!(assess_quality(300, false), Some(ContentQuality::Medium));
        assert_eq!(assess_quality(99, true), None);
    }
}
