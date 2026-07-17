-- v0.7.0：关闭跨源 link_hash dedup 的 SELECT→INSERT TOCTOU 窗口。
--
-- 保留历史重复行，不删除 audit/state/article 关系：每个 link_hash 按最小 id
-- 选择一条 canonical row，其余标记为 shadow。partial unique index 只约束
-- canonical rows；新写入默认 canonical，由数据库在 INSERT 时原子裁决。
ALTER TABLE feed_entries
    ADD COLUMN link_dedup_shadow INTEGER NOT NULL DEFAULT 0
    CHECK (link_dedup_shadow IN (0, 1));

WITH ranked AS (
    SELECT id,
           ROW_NUMBER() OVER (PARTITION BY link_hash ORDER BY id ASC) AS position
    FROM feed_entries
)
UPDATE feed_entries
   SET link_dedup_shadow = 1
 WHERE id IN (SELECT id FROM ranked WHERE position > 1);

CREATE UNIQUE INDEX uq_feed_entries_link_hash_canonical
    ON feed_entries(link_hash)
    WHERE link_dedup_shadow = 0;
