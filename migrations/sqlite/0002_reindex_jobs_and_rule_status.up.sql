-- F15-1 W9-F3+W9-F4: 补完 W0 doc storage-schema §4.8 + §4.10 缺失的两块。
--
-- 1. rule_versions.status 列 + partial unique index (kind WHERE status='active')
--    —— W9-F4：act 化 active_rule(kind) resolver 的前提。
-- 2. reindex_jobs 表 + 复合索引 + 约束
--    —— W9-F3：W0 doc state-machine §6 reindex_job 状态轮的物理基础。
--
-- 回填策略 (F15-4)：0001 时代 rule_versions 仅 UNIQUE(kind, version_tag)，
-- 允许同 kind 多行（实际场景：backfill ai 写多个 prompt version、
-- publish --force 写多个 render tag）。0002 引入的 partial unique
-- `uq_rule_versions_kind_active` 限制同 kind 至多一行 status='active'，
-- 因此在建立 index 前必须把"超出一行"的行降为 superseded：
--
--   step 1: retired_at IS NOT NULL 的行直接映射为 superseded
--           （0001 时代用 retired_at 表示已退役）
--   step 2: 同 kind 未退役多行时，仅 max(id) 保留 active，其余 superseded
--           （选 max(id) 因 0001 表 created_at 是 TEXT 排序不稳，id 自增更可靠）
--
-- 一次性 backfill；之后由 reindex flow + active_rule_or_register 维护。

ALTER TABLE rule_versions
    ADD COLUMN status TEXT NOT NULL DEFAULT 'active';

UPDATE rule_versions
   SET status = 'superseded'
 WHERE retired_at IS NOT NULL;

UPDATE rule_versions
   SET status = 'superseded'
 WHERE retired_at IS NULL
   AND id NOT IN (
       SELECT MAX(id)
         FROM rule_versions
        WHERE retired_at IS NULL
        GROUP BY kind
   );

-- partial unique index: 同 kind 至多一行 status='active'
CREATE UNIQUE INDEX uq_rule_versions_kind_active
    ON rule_versions (kind)
    WHERE status = 'active';

CREATE INDEX idx_rule_versions_kind_status
    ON rule_versions (kind, status);

CREATE TABLE reindex_jobs (
    id INTEGER PRIMARY KEY,
    target TEXT NOT NULL,
    rule_version_id INTEGER NOT NULL,
    last_processed_id INTEGER NULL,
    total_estimated INTEGER NULL,
    state TEXT NOT NULL,
    error TEXT NULL,
    aborted_reason TEXT NULL,
    lease_owner TEXT NULL,
    lease_expires_at TEXT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    started_at TEXT NULL,
    finished_at TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (rule_version_id) REFERENCES rule_versions(id),
    CHECK (state != 'completed' OR finished_at IS NOT NULL),
    CHECK (state != 'aborted' OR aborted_reason IS NOT NULL)
);

-- partial unique index: 同 target 至多一个未完成 job
CREATE UNIQUE INDEX uq_reindex_jobs_target_active
    ON reindex_jobs (target)
    WHERE state IN ('pending', 'running');

-- reclaim 扫描索引（state + lease_expires_at）
CREATE INDEX idx_reindex_jobs_state_lease
    ON reindex_jobs (state, lease_expires_at);
