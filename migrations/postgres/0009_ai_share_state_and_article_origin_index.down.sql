-- Roll back migration 0009.
DROP INDEX idx_articles_origin_feed_entry;
DROP TABLE ai_fresh_share_state;
