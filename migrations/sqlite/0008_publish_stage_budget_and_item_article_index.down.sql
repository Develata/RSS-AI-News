-- Roll back the per-stage publish budget column, the ownership index and the
-- rate-limit retry time.
ALTER TABLE publish_records DROP COLUMN next_attempt_at;
DROP INDEX idx_publish_items_article;
ALTER TABLE publish_records DROP COLUMN stage_attempt_count;
