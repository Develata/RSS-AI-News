-- Extract retry cooldown: retryable extract failures record when the entry becomes claimable
-- again. NULL means no cooldown (fresh rows, rows reset by backfill). The
-- extract claim skips pending_fetch rows whose deadline is after the run's
-- start, so a failed entry is not reclaimed by the run that released it.
ALTER TABLE feed_entries ADD COLUMN next_attempt_at TIMESTAMPTZ NULL;
