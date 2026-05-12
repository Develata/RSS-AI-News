-- F15-1 W9-F3+W9-F4 反向：先删 reindex_jobs，再回滚 rule_versions.status。
--
-- SQLite 不支持 DROP COLUMN，回滚时通过重建表实现（保留 0001 时 schema）。

DROP TABLE IF EXISTS reindex_jobs;

DROP INDEX IF EXISTS uq_rule_versions_kind_active;
DROP INDEX IF EXISTS idx_rule_versions_kind_status;

-- SQLite 3.35+ 才支持 DROP COLUMN；为兼容历史发行版用重建表。
-- 保留所有现有数据，仅去掉 status 列。
CREATE TABLE rule_versions__rollback_tmp (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    version_tag TEXT NOT NULL,
    description TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    retired_at TEXT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (kind, version_tag)
);

INSERT INTO rule_versions__rollback_tmp
    (id, kind, version_tag, description, payload_sha256, retired_at, created_at)
SELECT id, kind, version_tag, description, payload_sha256, retired_at, created_at
FROM rule_versions;

DROP TABLE rule_versions;
ALTER TABLE rule_versions__rollback_tmp RENAME TO rule_versions;

CREATE INDEX idx_rule_versions_kind_retired_at
    ON rule_versions (kind, retired_at);
