-- Roll back the per-stage publish budget column and the ownership index.
DROP INDEX idx_publish_items_article;
ALTER TABLE publish_records DROP COLUMN stage_attempt_count;
