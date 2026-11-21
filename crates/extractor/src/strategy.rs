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
        // Runtime passes bytes already decoded to UTF-8 by `decode_html` (with
        // the response's Content-Type). Valid UTF-8 is used as is: re-applying
        // the page's `<meta charset>` to decoded text would corrupt it. Other
        // callers (e.g. replay of a stored artifact) may pass the original
        // bytes; only then is the BOM / `<meta>` declaration consulted.
        let decoded = match std::str::from_utf8(html_bytes) {
            Ok(text) => std::borrow::Cow::Borrowed(text),
            Err(_) => crate::charset::decode_html(html_bytes, None)?,
        };
        let html: &str = &decoded;

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

        let word_count = word_count(&body_text);
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
                    chars: u32::try_from(body_text.chars().count()).unwrap_or(u32::MAX),
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

pub fn summary_fallback(task: &ArticleFetchTask) -> Option<FallbackArticle> {
    let body_text = task.summary_raw.as_ref()?;
    if body_text.trim().is_empty() {
        return None;
    }

    let body_text = strip_html_tags(body_text);
    if body_text.trim().is_empty() {
        return None;
    }

    let word_count = word_count(&body_text);
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

/// Word count used by the quality thresholds (100 = Medium, 300 = High).
///
/// Languages written without spaces between words (Chinese, Japanese) would
/// otherwise count a whole paragraph as one "word". Rule:
/// - a whitespace token without CJK characters counts as one word, exactly as
///   before, so the thresholds keep their meaning for space-delimited text;
/// - inside a token that contains CJK characters, every maximal non-CJK run
///   holding an alphanumeric character (`Rust` in `Rust语言`, `2024` in
///   `2024年`) counts as one word, and CJK punctuation-only runs (`，`) count
///   as none;
/// - CJK characters (Han ideographs, Hiragana, Katakana, Hangul) count
///   `total / 2` words, rounded down.
///
/// Ratio: Chinese–English parallel text runs about 1.6 Chinese characters per
/// English word; 2 is chosen conservatively (it never makes CJK text look
/// longer than its English equivalent), and rounding down keeps the
/// boundaries exact: 200 CJK characters are the first to reach Medium, 600 the
/// first to reach High. Hangul is written with spaces between words, so this
/// slightly over-counts Korean (about 1.5 words per space-delimited word).
///
/// Postcondition: the result is monotone in appended text and equals
/// `split_whitespace().count()` for text without CJK characters.
pub fn word_count(text: &str) -> u32 {
    let mut words: u64 = 0;
    let mut cjk_chars: u64 = 0;
    for token in text.split_whitespace() {
        if !token.chars().any(is_cjk) {
            words += 1;
            continue;
        }
        let mut run_has_alphanumeric = false;
        for ch in token.chars() {
            if is_cjk(ch) {
                cjk_chars += 1;
                if run_has_alphanumeric {
                    words += 1;
                    run_has_alphanumeric = false;
                }
            } else if ch.is_alphanumeric() {
                run_has_alphanumeric = true;
            }
        }
        if run_has_alphanumeric {
            words += 1;
        }
    }
    u32::try_from(words + cjk_chars / 2).unwrap_or(u32::MAX)
}

/// Han ideographs (URO, extensions A–H, compatibility), Hiragana, Katakana
/// (incl. phonetic extensions and half-width forms) and Hangul (syllables and
/// jamo).
fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{3005}'                       // 々 ideographic iteration mark
            | '\u{3040}'..='\u{309F}'   // Hiragana
            | '\u{30A0}'..='\u{30FF}'   // Katakana
            | '\u{31F0}'..='\u{31FF}'   // Katakana phonetic extensions
            | '\u{FF66}'..='\u{FF9F}'   // half-width Katakana
            | '\u{3400}'..='\u{4DBF}'   // CJK extension A
            | '\u{4E00}'..='\u{9FFF}'   // CJK unified ideographs
            | '\u{F900}'..='\u{FAFF}'   // CJK compatibility ideographs
            | '\u{20000}'..='\u{323AF}' // CJK extensions B–H + compatibility supplement
            | '\u{1100}'..='\u{11FF}'   // Hangul jamo
            | '\u{3130}'..='\u{318F}'   // Hangul compatibility jamo
            | '\u{A960}'..='\u{A97F}'   // Hangul jamo extended-A
            | '\u{AC00}'..='\u{D7AF}'   // Hangul syllables
            | '\u{D7B0}'..='\u{D7FF}'   // Hangul jamo extended-B
    )
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

    fn han(n: usize) -> String {
        "字".repeat(n)
    }

    #[test]
    fn cjk_boundaries_around_medium_and_high() {
        assert_eq!(word_count(&han(199)), 99);
        assert_eq!(assess_quality(word_count(&han(199)), true), None);
        assert_eq!(word_count(&han(200)), 100);
        assert_eq!(
            assess_quality(word_count(&han(200)), true),
            Some(ContentQuality::Medium)
        );
        assert_eq!(word_count(&han(599)), 299);
        assert_eq!(
            assess_quality(word_count(&han(599)), true),
            Some(ContentQuality::Medium)
        );
        assert_eq!(word_count(&han(600)), 300);
        assert_eq!(
            assess_quality(word_count(&han(600)), true),
            Some(ContentQuality::High)
        );
    }

    #[test]
    fn non_cjk_text_counts_like_split_whitespace() {
        let text = "Plain English — with a dash, 42 numbers and   extra   spaces.";
        assert_eq!(word_count(text), text.split_whitespace().count() as u32);
        assert_eq!(word_count(""), 0);
    }

    #[test]
    fn mixed_text_counts_latin_runs_and_halves_cjk() {
        // "Rust" + "Go" + "hello" + "world" = 4 words; 6 Han chars = 3 words;
        // full-width punctuation adds nothing.
        assert_eq!(word_count("Rust和Go都很好，大家 hello world"), 7);
        // "2024" counts as a word, 年 is one Han character (rounds down to 0).
        assert_eq!(word_count("2024年"), 1);
        // Kana and Hangul count like Han characters.
        assert_eq!(word_count("ひらがなカタカナ"), 4);
        assert_eq!(word_count("한국어 문장"), 2);
    }
}
