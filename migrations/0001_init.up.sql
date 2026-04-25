-- SQLite-first schema. PostgreSQL type/serial adaptations are deferred to a
-- later W4 migration pass; runtime PRAGMA settings are applied by storage::pool.

CREATE TABLE rule_versions (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    version_tag TEXT NOT NULL,
    description TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    retired_at TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (kind, version_tag)
);

CREATE INDEX idx_rule_versions_kind_retired_at
    ON rule_versions (kind, retired_at);

CREATE TABLE raw_artifacts (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    artifact_key TEXT NOT NULL,
    content_encoding TEXT NOT NULL,
    storage_kind TEXT NOT NULL,
    inline_body BLOB NULL,
    file_path TEXT NULL,
    byte_size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    retention_policy TEXT NOT NULL,
    expires_at TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (kind, artifact_key),
    CHECK ((storage_kind='inline' AND inline_body IS NOT NULL) OR (storage_kind='file' AND file_path IS NOT NULL))
);

CREATE INDEX idx_raw_artifacts_expires_at
    ON raw_artifacts (expires_at);

CREATE INDEX idx_raw_artifacts_kind
    ON raw_artifacts (kind);

CREATE TABLE feed_sources (
    id INTEGER PRIMARY KEY,
    category_key TEXT NOT NULL,
    source_key TEXT NOT NULL,
    display_name TEXT NOT NULL,
    feed_url TEXT NOT NULL,
    feed_kind TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    priority INTEGER NOT NULL DEFAULT 100,
    etag TEXT NULL,
    last_modified TEXT NULL,
    last_fetched_at TEXT NULL,
    last_success_at TEXT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NULL,
    last_error_kind TEXT NULL,
    config_version INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (category_key, source_key),
    FOREIGN KEY (config_version) REFERENCES rule_versions(id)
);

CREATE INDEX idx_feed_sources_status
    ON feed_sources (status);

CREATE INDEX idx_feed_sources_category_priority
    ON feed_sources (category_key, priority);

CREATE TABLE articles (
    id INTEGER PRIMARY KEY,
    content_hash TEXT NOT NULL,
    canonical_link TEXT NOT NULL,
    title TEXT NOT NULL,
    body_text TEXT NOT NULL,
    body_html_artifact_id INTEGER NULL,
    extractor_strategy TEXT NOT NULL,
    extractor_version INTEGER NOT NULL,
    content_quality TEXT NOT NULL,
    word_count INTEGER NOT NULL DEFAULT 0,
    origin_feed_entry_id INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'persisted',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (content_hash),
    FOREIGN KEY (origin_feed_entry_id) REFERENCES feed_entries(id),
    FOREIGN KEY (extractor_version) REFERENCES rule_versions(id),
    FOREIGN KEY (body_html_artifact_id) REFERENCES raw_artifacts(id) ON DELETE SET NULL
);

CREATE INDEX idx_articles_canonical_link
    ON articles (canonical_link);

CREATE INDEX idx_articles_state_created_at
    ON articles (state, created_at);

CREATE TABLE feed_entries (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL,
    feed_entry_uid TEXT NOT NULL,
    normalized_link TEXT NOT NULL,
    link_hash TEXT NOT NULL,
    title_raw TEXT NOT NULL,
    summary_raw TEXT NULL,
    published_at TEXT NULL,
    discovered_at TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'discovered',
    dedup_decision TEXT NULL,
    article_id INTEGER NULL,
    lease_owner TEXT NULL,
    lease_expires_at TEXT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NULL,
    last_error_kind TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (source_id, feed_entry_uid),
    FOREIGN KEY (source_id) REFERENCES feed_sources(id),
    FOREIGN KEY (article_id) REFERENCES articles(id) ON DELETE SET NULL
);

CREATE INDEX idx_feed_entries_link_hash
    ON feed_entries (link_hash);

CREATE INDEX idx_feed_entries_state_lease_expires_at
    ON feed_entries (state, lease_expires_at);

CREATE INDEX idx_feed_entries_source_discovered_at
    ON feed_entries (source_id, discovered_at);

CREATE TABLE article_ai_results (
    id INTEGER PRIMARY KEY,
    article_id INTEGER NOT NULL,
    prompt_version INTEGER NOT NULL,
    output_schema_version INTEGER NOT NULL,
    model_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    summary TEXT NULL,
    tags_json TEXT NULL,
    importance_score INTEGER NULL,
    keep_decision INTEGER NULL,
    raw_response_artifact_id INTEGER NULL,
    tokens_in INTEGER NULL,
    tokens_out INTEGER NULL,
    cost_micro_usd INTEGER NULL,
    latency_ms INTEGER NULL,
    lease_owner TEXT NULL,
    lease_expires_at TEXT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NULL,
    last_error_kind TEXT NULL,
    started_at TEXT NULL,
    completed_at TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (article_id, prompt_version, output_schema_version, model_id),
    FOREIGN KEY (article_id) REFERENCES articles(id),
    FOREIGN KEY (prompt_version) REFERENCES rule_versions(id),
    FOREIGN KEY (output_schema_version) REFERENCES rule_versions(id),
    FOREIGN KEY (raw_response_artifact_id) REFERENCES raw_artifacts(id) ON DELETE SET NULL,
    CHECK (importance_score IS NULL OR (importance_score BETWEEN 0 AND 100)),
    CHECK (keep_decision IS NULL OR keep_decision IN (0, 1))
);

CREATE INDEX idx_article_ai_results_state_lease_expires_at
    ON article_ai_results (state, lease_expires_at);

CREATE INDEX idx_article_ai_results_article_id
    ON article_ai_results (article_id);

CREATE TABLE publish_records (
    id INTEGER PRIMARY KEY,
    idempotency_key TEXT NOT NULL,
    category_key TEXT NOT NULL,
    report_date TEXT NOT NULL,
    target_timezone TEXT NOT NULL,
    render_version INTEGER NOT NULL,
    selection_policy_version INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    snapshot_frozen_at TEXT NULL,
    rendered_at TEXT NULL,
    local_stored_at TEXT NULL,
    remote_published_at TEXT NULL,
    local_path TEXT NULL,
    remote_target TEXT NULL,
    commit_sha TEXT NULL,
    lease_owner TEXT NULL,
    lease_expires_at TEXT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NULL,
    last_error_kind TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (idempotency_key),
    FOREIGN KEY (render_version) REFERENCES rule_versions(id),
    FOREIGN KEY (selection_policy_version) REFERENCES rule_versions(id)
);

CREATE INDEX idx_publish_records_category_report_date
    ON publish_records (category_key, report_date);

CREATE INDEX idx_publish_records_state_lease_expires_at
    ON publish_records (state, lease_expires_at);

CREATE TABLE publish_items (
    id INTEGER PRIMARY KEY,
    publish_record_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    article_id INTEGER NOT NULL,
    article_ai_result_id INTEGER NULL,
    frozen_title TEXT NOT NULL,
    frozen_summary TEXT NOT NULL,
    frozen_tags_json TEXT NOT NULL,
    frozen_score INTEGER NULL,
    frozen_canonical_link TEXT NOT NULL,
    frozen_source_display_name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (publish_record_id, article_id),
    FOREIGN KEY (publish_record_id) REFERENCES publish_records(id) ON DELETE CASCADE,
    FOREIGN KEY (article_id) REFERENCES articles(id),
    FOREIGN KEY (article_ai_result_id) REFERENCES article_ai_results(id),
    CHECK ((article_ai_result_id IS NOT NULL AND frozen_score IS NOT NULL) OR (article_ai_result_id IS NULL AND frozen_score IS NULL)),
    CHECK (frozen_score IS NULL OR (frozen_score BETWEEN 0 AND 100))
);

CREATE INDEX idx_publish_items_record_position
    ON publish_items (publish_record_id, position);

CREATE TABLE run_events (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL,
    trace_id TEXT NULL,
    stage TEXT NOT NULL,
    severity TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    target_kind TEXT NULL,
    target_id INTEGER NULL,
    message TEXT NOT NULL,
    context_json TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_run_events_run_id
    ON run_events (run_id);

CREATE INDEX idx_run_events_stage_severity_created_at
    ON run_events (stage, severity, created_at);

CREATE INDEX idx_run_events_target_kind_target_id
    ON run_events (target_kind, target_id);
