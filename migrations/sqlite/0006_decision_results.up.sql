-- typed-decision Phase B (ADR 0011): policy snapshots and shadow results.
-- Removal is a later forward migration that drops both tables (ADR 0011 §13).
CREATE TABLE decision_policies (
    policy_version TEXT NOT NULL PRIMARY KEY,
    canonical_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (length(policy_version) = 64)
);

CREATE TABLE decision_results (
    id INTEGER PRIMARY KEY,
    article_id INTEGER NOT NULL,
    category_key TEXT NOT NULL,
    policy_version TEXT NOT NULL,
    execution_key TEXT NOT NULL,
    provider TEXT NOT NULL,
    requested_model TEXT NOT NULL,
    laya_max_len INTEGER NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    lease_owner TEXT NULL,
    lease_expires_at TEXT NULL,
    answers_json TEXT NULL,
    reported_model TEXT NULL,
    routing_model TEXT NULL,
    endpoint_origin TEXT NULL,
    state_sha256 TEXT NULL,
    body_chars_sent INTEGER NULL,
    body_truncated INTEGER NULL,
    provider_truncated INTEGER NULL,
    state_tokens_dropped INTEGER NULL,
    input_tokens INTEGER NULL,
    latency_ms INTEGER NULL,
    raw_response TEXT NULL,
    raw_response_expires_at TEXT NULL,
    raw_response_bytes INTEGER NULL,
    raw_response_sha256 TEXT NULL,
    raw_response_withheld TEXT NULL,
    last_error_kind TEXT NULL,
    last_error TEXT NULL,
    started_at TEXT NULL,
    completed_at TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (article_id, policy_version, execution_key),
    FOREIGN KEY (article_id) REFERENCES articles(id),
    FOREIGN KEY (policy_version) REFERENCES decision_policies(policy_version),
    CHECK (provider IN ('jev', 'laya')),
    CHECK ((provider = 'laya' AND laya_max_len IS NOT NULL AND laya_max_len BETWEEN 512 AND 8192)
        OR (provider = 'jev' AND laya_max_len IS NULL)),
    CHECK (state IN ('pending', 'running', 'succeeded', 'failed')),
    CHECK (attempt_count >= 0),
    CHECK (state <> 'succeeded' OR answers_json IS NOT NULL),
    CHECK (body_truncated IS NULL OR body_truncated IN (0, 1)),
    CHECK (provider_truncated IS NULL OR provider_truncated IN (0, 1)),
    CHECK (raw_response_withheld IS NULL
        OR raw_response_withheld IN ('not_json', 'credential', 'incomplete'))
);

CREATE INDEX idx_decision_results_claim
    ON decision_results (category_key, policy_version, execution_key, state, id);
CREATE INDEX idx_decision_results_state_lease_expires_at
    ON decision_results (state, lease_expires_at);
CREATE INDEX idx_decision_results_article_id
    ON decision_results (article_id);
CREATE INDEX idx_decision_results_raw_response_expires_at
    ON decision_results (raw_response_expires_at);
