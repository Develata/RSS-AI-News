use rss_ai_news_domain::Score0To100;
use rss_ai_news_domain::dto::publish::PublishCandidate;
use rss_ai_news_report::{SnapshotConfig, freeze};

#[test]
fn freeze_assigns_positions_starting_from_one_in_input_order() {
    let items = freeze(
        vec![
            candidate(10, "first", vec![]),
            candidate(11, "second", vec![]),
        ],
        &SnapshotConfig {
            excerpt_max_chars: 100,
        },
    )
    .unwrap();

    assert_eq!(items[0].position, 1);
    assert_eq!(items[0].article_id, 10);
    assert_eq!(items[1].position, 2);
    assert_eq!(items[1].article_id, 11);
}

#[test]
fn freeze_serializes_tags_to_json() {
    let items = freeze(
        vec![candidate(
            10,
            "summary",
            vec!["ai".to_string(), "rust".to_string()],
        )],
        &SnapshotConfig {
            excerpt_max_chars: 100,
        },
    )
    .unwrap();

    assert_eq!(items[0].frozen_tags_json, "[\"ai\",\"rust\"]");
}

#[test]
fn freeze_truncates_summary_to_excerpt_max_chars() {
    let items = freeze(
        vec![candidate(10, "abcdef", vec![])],
        &SnapshotConfig {
            excerpt_max_chars: 4,
        },
    )
    .unwrap();

    assert_eq!(items[0].frozen_summary, "abc…");
}

fn candidate(article_id: i64, summary: &str, tags: Vec<String>) -> PublishCandidate {
    PublishCandidate::try_new(
        article_id,
        Some(article_id + 1000),
        format!("title {article_id}"),
        format!("https://example.com/{article_id}"),
        summary.to_string(),
        tags,
        Some(Score0To100::try_new(80).unwrap()),
        "Source".to_string(),
        "ai".to_string(),
        None,
    )
    .unwrap()
}
