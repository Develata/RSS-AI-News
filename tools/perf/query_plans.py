#!/usr/bin/env python3
"""Print SQLite plans using production SQL and 10k local fixture rows."""

import json
from pathlib import Path
import re
import sqlite3

ROOT = Path(__file__).resolve().parents[2]
NOW = "2026-01-02T00:00:00Z"
LEASE_END = "2026-01-02T00:00:30Z"
SINCE = "2025-12-31T00:00:00Z"
# Parameter order follows each production constant's numbered placeholders.
QUERIES = (
    ("feed_entry_sql.rs", "CLAIM_PENDING_FETCH_SQLITE_SQL",
     ["owner", LEASE_END, NOW, NOW, 5, 50]),
    ("article_ai_result_sql.rs", "CLAIM_AI_PENDING_SQLITE_SQL",
     ["owner", LEASE_END, NOW, NOW, NOW, 3, "ai", 20]),
    ("feed_entry_sql.rs", "LIST_RECENT_FEED_ENTRIES_SQLITE_SQL",
     ["ai", SINCE, 1767225600, 0, None, 0, 80]),
    ("article.rs", "LIST_ARTICLES_PERSISTED_FOR_AI_TASK_GEN_SQL", [0, "ai", 50]),
    ("publish_item.rs", "SELECT_AI_PATH_CANDIDATES_SQL", [30, "ai", SINCE, NOW, 30]),
    ("raw_artifact.rs", "SELECT_RAW_ARTIFACT_BY_KEY_SQL", ["html_payload", "1"]),
    ("publish_record_sql.rs", "CLAIM_PUBLISH_SQLITE_SQL",
     ["owner", LEASE_END, NOW, "rendered", NOW, 5, 1]),
)


def seed_fixture(db):
    for path in sorted((ROOT / "migrations/sqlite").glob("*.up.sql")):
        db.executescript(path.read_text())
    db.execute("""
        INSERT INTO rule_versions(kind, version_tag, description, payload_sha256)
        VALUES('config', 'fixture', 'fixture', 'fixture')
    """)
    for source_id in range(1, 11):
        db.execute("""
            INSERT INTO feed_sources(id, category_key, source_key, display_name,
                                     feed_url, feed_kind, config_version)
            VALUES(?, 'ai', ?, 'source', 'https://example.invalid', 'rss', 1)
        """, (source_id, str(source_id)))
    for row_id in range(1, 10001):
        pending = row_id > 9900
        link = f"https://example.invalid/{row_id}"
        db.execute("""
            INSERT INTO feed_entries(id, source_id, feed_entry_uid, normalized_link,
                                     link_hash, title_raw, discovered_at, state)
            VALUES(?, ?, ?, ?, ?, 'fixture', '2026-01-01T00:00:00Z', ?)
        """, (row_id, row_id % 10 + 1, str(row_id), link, str(row_id),
              "pending_fetch" if pending else "persisted"))
        db.execute("""
            INSERT INTO articles(id, content_hash, canonical_link, title, body_text,
                                 extractor_strategy, extractor_version,
                                 content_quality, origin_feed_entry_id, state)
            VALUES(?, ?, ?, 'fixture', 'body', 'readability', 1, 'high', ?, ?)
        """, (row_id, str(row_id), link, row_id,
              "ai_pending" if pending else "published"))
        db.execute("""
            INSERT INTO article_ai_results(article_id, prompt_version,
                                           output_schema_version, model_id, state)
            VALUES(?, 1, 1, 'fixture', ?)
        """, (row_id, "pending" if pending else "succeeded"))
    for row_id in range(1, 1001):
        db.execute("""
            INSERT INTO publish_records(idempotency_key, category_key, report_date,
                                        target_timezone, render_version,
                                        selection_policy_version, state)
            VALUES(?, 'ai', '2026-01-01', 'UTC', 1, 1, ?)
        """, (str(row_id), "rendered" if row_id > 990 else "published"))
    db.execute("ANALYZE")


def explain(db, filename, name, values):
    source = (ROOT / "crates/storage/src/repo" / filename).read_text()
    match = re.search(r"const " + re.escape(name) + r': &str = r#"(.*?)"#;',
                      source, re.DOTALL)
    if match is None:
        return {"query": name, "error": "constant not found"}
    params = {str(index): value for index, value in enumerate(values, start=1)}
    try:
        rows = db.execute("EXPLAIN QUERY PLAN " + match[1], params)
        return {"query": name, "plan": [row[3] for row in rows]}
    except sqlite3.Error as error:
        return {"query": name, "error": str(error)}


def main():
    db = sqlite3.connect(":memory:")
    try:
        seed_fixture(db)
        results = [explain(db, *query) for query in QUERIES]
    finally:
        db.close()
    print(json.dumps({
        "sqlite_version": sqlite3.sqlite_version,
        "fixture_rows": 10000,
        "pending_rows": 100,
        "queries": results,
    }, indent=2))
    return int(any("error" in row for row in results))


if __name__ == "__main__":
    raise SystemExit(main())
