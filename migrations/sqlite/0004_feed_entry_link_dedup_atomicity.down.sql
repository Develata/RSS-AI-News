-- 回滚 v0.7.0 link_hash canonical uniqueness。
DROP INDEX IF EXISTS uq_feed_entries_link_hash_canonical;
ALTER TABLE feed_entries DROP COLUMN link_dedup_shadow;
