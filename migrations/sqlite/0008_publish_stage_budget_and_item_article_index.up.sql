-- Per-stage publish retry budget (ADR 0012). Each claim increments both
-- attempt_count (cumulative, audit) and stage_attempt_count; every successful
-- stage advance resets stage_attempt_count to 0, and the claim filter, the
-- retryable-release fold and the exhausted sweep compare stage_attempt_count
-- with retry.publish_max_attempts. Existing rows start the current stage with
-- a fresh budget (0); terminal rows are unaffected.
ALTER TABLE publish_records ADD COLUMN stage_attempt_count INTEGER NOT NULL DEFAULT 0;

-- Snapshot ownership lookups (ADR 0012): candidate selection and the freeze
-- re-check ask whether an article is already in another active snapshot.
CREATE INDEX idx_publish_items_article ON publish_items (article_id);
