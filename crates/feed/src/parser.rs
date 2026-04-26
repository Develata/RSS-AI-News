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
            summary_raw: entry.summary.map(|summary| summary.content),
            published_at: entry
                .published
                .or(entry.updated)
                .and_then(|dt| OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok()),
        });
    }

    Ok(entries)
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
