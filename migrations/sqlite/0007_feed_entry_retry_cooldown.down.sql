-- Roll back the extract retry cooldown column.
ALTER TABLE feed_entries DROP COLUMN next_attempt_at;
