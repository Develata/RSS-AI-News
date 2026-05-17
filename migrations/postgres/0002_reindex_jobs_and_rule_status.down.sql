-- PG 反向：DROP COLUMN 原生支持，无需重建表（与 SQLite down 的形态差异）。

DROP TABLE IF EXISTS reindex_jobs;

DROP INDEX IF EXISTS uq_rule_versions_kind_active;
DROP INDEX IF EXISTS idx_rule_versions_kind_status;

ALTER TABLE rule_versions DROP COLUMN status;
