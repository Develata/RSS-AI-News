-- C9 / R2-B1: the side (fresh = inside the publish window, backlog = the rest)
-- that the last batch-size-1 task_gen pass / claim batch of a category actually
-- served. The next batch-size-1 pass leads with the other side, so the
-- alternation survives a moving window and retries (a derived signal re-read
-- against the current window does not). Written in the same transaction as the
-- advance / claim it records.
CREATE TABLE ai_fresh_share_state (
    category_key TEXT NOT NULL,
    stage TEXT NOT NULL,
    last_side TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (category_key, stage),
    CHECK (stage IN ('task_gen', 'claim')),
    CHECK (last_side IN ('fresh', 'backlog'))
);
