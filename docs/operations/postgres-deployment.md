# PostgreSQL 部署

## 何时需要

默认 SQLite 已可满足"单机日活几百条 feed"的场景。切到 PostgreSQL 通常是：

- 多 worker 并发跑相同子命令需要更强的锁语义
- 数据量增长（>百万行 / 数 GB），SQLite 写并发瓶颈出现
- 与既有 PG 基础设施统一运维

切换的设计取舍详见 [../adr/0005-storage-pool-dual-dialect.md](../adr/0005-storage-pool-dual-dialect.md)
与 [../adr/0006-postgres-go-real-no-shrink.md](../adr/0006-postgres-go-real-no-shrink.md)。

## 切换步骤

### 1. 准备 PG 实例

最小可用：

```bash
docker run -d --name rss-pg \
  -e POSTGRES_USER=rss_ai \
  -e POSTGRES_PASSWORD=<set-strong-password> \
  -e POSTGRES_DB=rss_ai_news \
  -p 5432:5432 \
  -v rss-pg-data:/var/lib/postgresql/data \
  postgres:16
```

生产建议：托管 PG（RDS / Cloud SQL / Supabase 等）+ TLS + 备份策略。

### 2. 设置 env

```env
# .env
DATABASE_URL=postgres://rss_ai:<password>@db-host:5432/rss_ai_news
```

`DATABASE_URL` 优先于 `app.toml [database].sqlite_path`，由 `crates/cli/src/db_url.rs` 解析。

### 3. 改 app.toml

```toml
[database]
driver = "postgres"           # 原值 "sqlite" 改 "postgres"
sqlite_path = "data.db"       # 字段保留，PG 模式下不读
max_connections = 10
busy_timeout_ms = 5000
```

### 4. 跑 migrate

```bash
cargo run -- migrate run                    # 本地
docker run --rm --env-file .env \           # 容器
  ghcr.io/develata/rss-ai-news:0.4.0 \
  --config-dir /app/configs migrate run
```

migrate 会自动按 `is_postgres_url` 识别走 `migrations/postgres/*.up.sql` 全量。

### 5. 验证

```bash
cargo run -- doctor
# 关注 database / migrations / openai / github 几项应全部 ok
```

## 数据迁移

**当前没有自动 SQLite → PG 数据搬运工具**（详见 [../adr/0006-postgres-go-real-no-shrink.md](../adr/0006-postgres-go-real-no-shrink.md)）。
切换路径只有两种：

### 路径 A：fresh start

新 PG 实例从空 schema 开始，让 ingest / ai-run 重新跑一遍。
- 优点：简单、无数据完整性风险
- 缺点：丢失历史 articles / publish_records；rebuild-report 无法回溯老报告

### 路径 B：手动 dump/restore

按表逐张 dump（SQLite `.dump` → 转 PG-friendly SQL → `psql -f`）。
- `feed_sources` / `feed_entries` / `articles` / `publish_records` 等主要表 schema 在两方言下基本对齐
- 时间字段 SQLite 是 `TEXT (ISO8601)`、PG 是 `TIMESTAMPTZ`，需要在 dump 时转换
- `rule_versions` / `reindex_jobs` 等含 partial unique 的表注意先把 status 收敛到合法集合
- 仓库**未提供**自动转换脚本

## CI 中的 PG 验证

`.github/workflows/ci.yml` 的 `migrate` job 用 PG service container 端到端跑：

```yaml
services:
  postgres:
    image: postgres:16
    env: { POSTGRES_USER: ..., POSTGRES_PASSWORD: ..., POSTGRES_DB: ... }
    ports: [5432:5432]
    options: --health-cmd="pg_isready"
```

CI 会跑 `migrate run` + 部分集成测试，证明 PG 路径不退化。

## 已知差异

两方言下行为**严格对齐**的清单（任一项失配视为 bug）：

| 行为 | SQLite | PostgreSQL | 测试 |
|---|---|---|---|
| `claim` 行级互斥 | `UPDATE ... RETURNING` 单 tx | 同上 + `FOR UPDATE SKIP LOCKED`（性能更好） | `parallel_claim_returns_disjoint_rows`（`crates/storage/tests/concurrency_tests.rs`） |
| partial unique `WHERE state='active'` | ✓ | ✓ | `partial_unique_index_holds_after_backfill`（`crates/storage/tests/migration_0002_backfill_tests.rs`） |
| `release` owner 误写返 `rows_affected = 0` | ✓ | ✓ | `release_with_wrong_owner_returns_false`（`crates/storage/tests/concurrency_tests.rs`） |
| reindex 完成时同 tx 切换 active | ✓ | ✓ | `reindex_promotes_rule_version_to_active_on_completion`（`crates/runtime/tests/w9c_runtime_tests.rs`） |
| migration 编号 + basename 一对一 | ✓ | ✓ | `migrations_sqlite_and_postgres_have_matching_numbers_and_basenames`（`crates/storage/tests/migration_pair_parity_tests.rs`） |

详见 [../acceptance-cases/pipelines/05-multi-dialect-storage.md](../acceptance-cases/pipelines/05-multi-dialect-storage.md)。

## 排障

| 现象 | 排查 |
|---|---|
| `connection refused` | PG 端口 / 防火墙 / `pg_hba.conf` listen_addresses |
| `password authentication failed` | URL 中的密码 URL-encode（含特殊字符时） |
| `relation ... does not exist` | 没跑 `migrate run` 或跑错 driver |
| `duplicate key value violates unique constraint "..._active_idx"` | partial unique 冲突 —— 通常意味着 reindex 流程出错；查 `reindex_jobs` 表 |

## 相关文档

- 设计：[../plan/05-storage.md](../plan/05-storage.md) §多方言
- 部署：[../plan/12-deployment.md](../plan/12-deployment.md) §7
- ADR：[../adr/0005-storage-pool-dual-dialect.md](../adr/0005-storage-pool-dual-dialect.md)、[../adr/0006-postgres-go-real-no-shrink.md](../adr/0006-postgres-go-real-no-shrink.md)
- 验收：[../acceptance-cases/pipelines/05-multi-dialect-storage.md](../acceptance-cases/pipelines/05-multi-dialect-storage.md)
- 实现：[`crates/storage/src/pool.rs`](../../crates/storage/src/pool.rs)、[`crates/cli/src/db_url.rs`](../../crates/cli/src/db_url.rs)
