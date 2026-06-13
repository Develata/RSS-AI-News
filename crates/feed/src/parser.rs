//! feed-rs adapter to domain DTOs.

use std::io::Cursor;

use rss_ai_news_domain::dto::feed::FeedEntryMeta;
use rss_ai_news_domain::state::FeedKind;
use time::OffsetDateTime;

use crate::error::FeedError;

pub fn parse_feed(bytes: &[u8], feed_kind: FeedKind) -> Result<Vec<FeedEntryMeta>, FeedError> {
    let _ = feed_kind;
    let feed =
        feed_rs::parser::parse(Cursor::new(bytes)).map_err(|error| FeedError::ParseFailed {
            reason: error.to_string(),
        })?;

    let mut entries = Vec::with_capacity(feed.entries.len());
    for entry in feed.entries {
        let Some(first_link) = entry.links.first() else {
            tracing::debug!(feed_entry_uid = %entry.id, "skip feed entry without link");
            continue;
        };

        let link_raw = first_link.href.clone();
        let title_raw = entry
            .title
            .map(|title| title.content)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| link_raw.clone());

        entries.push(FeedEntryMeta {
            feed_entry_uid: entry.id,
            title_raw,
            link_raw,
            // atom 的正文可能放在 <summary> 或 <content>（GitHub releases.atom
            // 只有 <content>）。优先 <summary>，缺失则回退到**文本类** <content>，
            // 否则正文在 readability 失败时无从兜底而整条 failed。RSS 的
            // <description> 经 feed-rs 映射到 summary，行为不变。
            summary_raw: entry
                .summary
                .map(|summary| summary.content)
                .or_else(|| entry.content.and_then(textual_content_body)),
            published_at: entry
                .published
                .or(entry.updated)
                .and_then(|dt| OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok()),
        });
    }

    Ok(entries)
}

/// 仅当 atom `<content>` 是文本/HTML/XML 类型时返回其 body 作为摘要回退。
///
/// 非文本 content（`image/*` 的 base64、`application/json`、`application/octet-stream`
/// 等）的 body 不是可读正文，若直接写入 `summary_raw` 会被 `summary_fallback`
/// 当作 fallback 文章持久化，污染数据。按 `content_type` 过滤是必要的边界校验。
fn textual_content_body(content: feed_rs::model::Content) -> Option<String> {
    let is_textual = {
        let media_type = content.content_type.as_str();
        media_type.starts_with("text/")
            || media_type.ends_with("/xml")
            || media_type.ends_with("+xml")
    };
    is_textual.then_some(content.body).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS_MINIMAL: &[u8] = include_bytes!("../tests/fixtures/rss_2.0_minimal.xml");
    const ATOM_MINIMAL: &[u8] = include_bytes!("../tests/fixtures/atom_minimal.xml");
    const JSON_FEED_MINIMAL: &[u8] = include_bytes!("../tests/fixtures/json_feed_minimal.json");

    #[test]
    fn parses_rss_2_minimal_fixture() {
        let entries = parse_feed(RSS_MINIMAL, FeedKind::Rss).expect("RSS should parse");

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].feed_entry_uid, "rss-1");
        assert_eq!(entries[0].title_raw, "RSS item 1");
        assert_eq!(entries[0].link_raw, "https://example.com/rss/1");
        assert!(entries[0].published_at.is_some());
        assert_eq!(entries[2].feed_entry_uid, "rss-3");
    }

    #[test]
    fn parses_atom_minimal_fixture() {
        let entries = parse_feed(ATOM_MINIMAL, FeedKind::Atom).expect("Atom should parse");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].feed_entry_uid, "atom-1");
        assert_eq!(entries[0].title_raw, "Atom entry 1");
        assert_eq!(entries[0].link_raw, "https://example.com/atom/1");
        assert_eq!(entries[1].summary_raw.as_deref(), Some("Atom summary 2"));
    }

    #[test]
    fn parses_json_feed_minimal_fixture() {
        let entries =
            parse_feed(JSON_FEED_MINIMAL, FeedKind::JsonFeed).expect("JSON feed should parse");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].feed_entry_uid, "json-1");
        assert_eq!(entries[0].title_raw, "JSON item 1");
        assert_eq!(entries[0].link_raw, "https://example.com/json/1");
    }

    #[test]
    fn atom_content_falls_back_to_summary_raw_when_summary_absent() {
        // GitHub releases.atom 把 release notes 放在 <content>、没有 <summary>。
        // 解析器必须把 <content> 映射进 summary_raw，否则 readability 失败时
        // 无摘要可兜底 → 整条 failed（生产 sglang 10/10 即此症状）。
        let atom = br#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Releases</title>
  <entry>
    <id>tag:github.com,2008:Repository/1/v1.0</id>
    <title>v1.0</title>
    <link rel="alternate" type="text/html" href="https://github.com/x/y/releases/tag/v1.0"/>
    <content type="html">&lt;p&gt;Release notes body&lt;/p&gt;</content>
  </entry>
</feed>"#;

        let entries = parse_feed(atom, FeedKind::Atom).expect("atom should parse");

        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].summary_raw.as_deref(),
            Some("<p>Release notes body</p>"),
            "summary_raw must fall back to <content> when <summary> is absent"
        );
    }

    #[test]
    fn atom_non_textual_content_is_not_used_as_summary_raw() {
        // 非文本 content（这里 application/json）不是可读正文：必须拒绝，
        // 否则会被 summary_fallback 当成 fallback 文章持久化（codex P2）。
        let atom = br#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Binary</title>
  <entry>
    <id>bin-1</id>
    <title>blob</title>
    <link rel="alternate" type="text/html" href="https://example.com/blob"/>
    <content type="application/json">{"not":"an article"}</content>
  </entry>
</feed>"#;

        let entries = parse_feed(atom, FeedKind::Atom).expect("atom should parse");

        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].summary_raw, None,
            "non-textual <content> must not populate summary_raw"
        );
    }

    #[test]
    fn damaged_xml_returns_parse_failed() {
        let err = parse_feed(b"<rss><channel><item>", FeedKind::Rss)
            .expect_err("damaged XML should fail");

        assert!(matches!(err, FeedError::ParseFailed { .. }));
    }

    #[test]
    fn empty_feed_returns_empty_entries() {
        let entries = parse_feed(
            b"<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>x</title><link>https://example.com/</link><description>x</description></channel></rss>",
            FeedKind::Rss,
        )
        .expect("empty feed should parse");

        assert!(entries.is_empty());
    }
}
