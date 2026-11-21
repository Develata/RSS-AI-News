//! Charset decoding of fetched HTML (A2): BOM → Content-Type → <meta> → UTF-8.

use std::time::Duration;

use rss_ai_news_domain::state::ContentQuality;
use rss_ai_news_extractor::{
    ArticleFetchTask, ContentStrategy, ExtractorError, HtmlFetcher, ReadabilityStrategy,
    ReqwestHtmlFetcher, decode_html,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ZH_SENTENCE: &str =
    "人工智能研究在过去十年取得了显著进展，研究者正在探索更高效的训练方法与评估基准。";

fn zh_html(meta: &str) -> String {
    let body: String = (0..12).map(|_| format!("<p>{ZH_SENTENCE}</p>")).collect();
    format!(
        "<!doctype html><html><head>{meta}<title>中文标题</title></head>\
         <body><article><h1>中文标题</h1>{body}</article></body></html>"
    )
}

fn encode(encoding: &'static encoding_rs::Encoding, text: &str) -> Vec<u8> {
    let (bytes, _, had_errors) = encoding.encode(text);
    assert!(
        !had_errors,
        "sample must be representable in {}",
        encoding.name()
    );
    bytes.into_owned()
}

fn task() -> ArticleFetchTask {
    ArticleFetchTask {
        feed_entry_id: 1,
        normalized_link: "https://example.com/a".to_string(),
        title_raw: "t".to_string(),
        summary_raw: None,
        timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn gbk_page_declared_only_by_the_content_type_header_decodes() {
    let html = zh_html("");
    let gbk = encode(encoding_rs::GBK, &html);
    assert!(std::str::from_utf8(&gbk).is_err());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gbk"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html; charset=GBK")
                .set_body_bytes(gbk.clone()),
        )
        .mount(&server)
        .await;
    let fetcher = ReqwestHtmlFetcher::new(1024 * 1024).expect("fetcher should build");
    let mut fetch_task = task();
    fetch_task.normalized_link = format!("{}/gbk", server.uri());
    let raw = fetcher.fetch_html(&fetch_task).await.expect("fetch");

    assert_eq!(raw.body_bytes, gbk, "raw bytes stay untouched");
    assert_eq!(raw.content_type.as_deref(), Some("text/html; charset=GBK"));
    let decoded = decode_html(&raw.body_bytes, raw.content_type.as_deref()).expect("decode");
    assert_eq!(decoded, html);

    let article = ReadabilityStrategy
        .extract(&task(), decoded.as_bytes(), "https://example.com/a")
        .expect("decoded GBK page extracts");
    assert!(article.body_text.contains("人工智能研究"));
    assert_eq!(article.content_quality, ContentQuality::Medium);
}

#[test]
fn gbk_page_declared_only_by_meta_decodes() {
    let html = zh_html("<meta charset=\"gb2312\">");
    let gbk = encode(encoding_rs::GBK, &html);
    assert_eq!(decode_html(&gbk, None).expect("decode"), html);
    assert_eq!(
        decode_html(&gbk, Some("text/html")).expect("decode"),
        html,
        "a Content-Type without charset does not block the meta prescan"
    );
}

#[test]
fn http_equiv_big5_and_shift_jis_pages_decode() {
    let big5_html = "<html><head><meta http-equiv=\"Content-Type\" \
                     content=\"text/html; charset=big5\"><title>繁體標題</title></head>\
                     <body><p>人工智慧研究在過去十年取得了顯著進展。</p></body></html>";
    let big5 = encode(encoding_rs::BIG5, big5_html);
    assert_eq!(decode_html(&big5, None).expect("big5"), big5_html);

    let sjis_html = "<html><head><meta charset=\"Shift_JIS\"></head><body><p>日本語のテキストです。</p></body></html>";
    let sjis = encode(encoding_rs::SHIFT_JIS, sjis_html);
    assert_eq!(decode_html(&sjis, None).expect("sjis"), sjis_html);
}

#[test]
fn undeclared_invalid_utf8_still_fails_without_guessing() {
    let gbk = encode(encoding_rs::GBK, &zh_html(""));
    let err = decode_html(&gbk, None).expect_err("no declaration, not UTF-8");
    assert!(matches!(err, ExtractorError::ParseFailed { .. }));

    // The strategy keeps rejecting such bytes (no windows-1252 guess).
    let err = ReadabilityStrategy
        .extract(&task(), &gbk, "https://example.com/a")
        .expect_err("undeclared non-UTF-8 page");
    assert!(matches!(err, ExtractorError::ParseFailed { .. }));
}

#[test]
fn declared_charset_with_malformed_bytes_is_rejected_not_replaced() {
    // 0x81 0x20 is not a valid GBK sequence; strict decoding refuses it
    // instead of publishing U+FFFD.
    let mut bytes = b"<meta charset=gbk><p>".to_vec();
    bytes.extend_from_slice(&[0x81, 0x20]);
    assert!(decode_html(&bytes, None).is_err());
}

#[test]
fn utf8_pages_are_borrowed_unchanged() {
    let html = zh_html("<meta charset=\"utf-8\">");
    let decoded = decode_html(html.as_bytes(), Some("text/html; charset=utf-8")).unwrap();
    assert!(matches!(decoded, std::borrow::Cow::Borrowed(_)));
    assert_eq!(decoded, html);
}

#[test]
fn strategy_decodes_original_bytes_by_meta_when_not_utf8() {
    // Replay passes the stored original bytes: the strategy honours <meta>.
    let html = zh_html("<meta charset=\"gbk\">");
    let gbk = encode(encoding_rs::GBK, &html);
    let article = ReadabilityStrategy
        .extract(&task(), &gbk, "https://example.com/a")
        .expect("meta-declared GBK extracts");
    assert!(article.body_text.contains("人工智能研究"));
}
