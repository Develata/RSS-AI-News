-- 反向：按 FK 拓扑反序 drop。PG CASCADE 会自动清理依赖索引/约束，无需逐个 DROP INDEX。
DROP TABLE IF EXISTS run_events;
DROP TABLE IF EXISTS publish_items;
DROP TABLE IF EXISTS publish_records;
DROP TABLE IF EXISTS article_ai_results;
DROP TABLE IF EXISTS feed_entries CASCADE;
DROP TABLE IF EXISTS articles CASCADE;
DROP TABLE IF EXISTS feed_sources;
DROP TABLE IF EXISTS raw_artifacts;
DROP TABLE IF EXISTS rule_versions;
