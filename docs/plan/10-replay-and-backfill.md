# 10 — Replay 与 Backfill

本章详解三类跨流程的"修补 / 实验"机制：`replay` / `backfill` / `rebuild-report`。

这三个子命令是 W6+ 引入的"回放与产物治理"骨架（见 W6 handoff），目的是让线上已发生的
错误、产物丢失、prompt 升级都能在不破坏既有数据的前提下重做。

## 1. 边界

本章覆盖：
- 三个子命令的语义边界、相互区别
- `raw_artifacts` 的留档策略（retention policy × stage）
- `replay --kind ∈ {feed, html, ai}` 的输入输出
- `backfill --target ∈ {extract, ai}` 的状态机影响
- `rebuild-report` 的"按 publish_record 字节相等重建"保证

**不覆盖**：
- 各 stage 的内部状态机 → [./08-state-machines.md](./08-state-machines.md)
- `reindex`（版本化规则升级，不属"重做线上"范畴）→ [./05-storage.md](./05-storage.md) §8
- raw_artifacts 表 schema → [./05-storage.md](./05-storage.md) §9
- prompt_versions 设计 → [./03-ai.md](./03-ai.md)

## 2. 三机制的分工

| 机制 | 输入 | 副作用 | 用途 |
|---|---|---|---|
| `replay` | 单个 `raw_artifacts` 行 | **无**（dry-run 形式） | 离线复现一次解析 / 抓取 / AI 调用 |
| `backfill` | 时间窗口 + 目标段 | 重置状态机让相应阶段重跑 | 重做曾失败的 extract / 重跑新 prompt |
| `rebuild-report` | 单个 `publish_records.id` | 仅重新渲染 + 写本地/远端 | 渲染模板修复后字节相等重建 |

三者**互不替代**：
- `replay` 不写入业务表，只回答"如果重来一次，会得到什么"
- `backfill` 写入业务表（重置状态、插入 ai 任务），但不绕过状态机
- `rebuild-report` 写文件，不动 `publish_records` 行本身（snapshot 仍冻结）

## 3. raw_artifacts 留档策略

入口：[`crates/runtime/src/artifact.rs::ArtifactWriter`](../../crates/runtime/src/artifact.rs)。

留档由 `[artifact].retention_policy` 决定，五档：

| policy | 写入时机 |
|---|---|
| `always` | 每次抓取/调用都留档 |
| `on_failure` | 仅当下游解析失败时回写 |
| `sampled` | 按 `sample_rate` 随机留档（用于线上抽样诊断） |
| `debug_only` | 保留兼容值，尚未实现；当前不留档，validate-config 会告警 |
| `off` | 完全不留档 |

写入位置：当前全部存入 `raw_artifacts.inline_body`（SQLite BLOB / PostgreSQL BYTEA）。
`inline_threshold_bytes` / `file_storage_dir` 是保留字段，没有 threshold 分流或文件写入实现。
`replay` 仅支持 inline；手工导入的 file-backed 行仍明确拒绝，不由 TTL 清理删除文件或元数据。
配置警告的权威列表见 [06-config.md](./06-config.md#独立能力与保留字段2026-09-10)。

生命周期：

- `ttl_days > 0` 在写入/upsert 时计算新的 `expires_at`；`ttl_days=0` 写 `NULL`，永久保留。
  修改配置不会追溯改写已有行的期限；`off` 只停止新增留档，已有到期记录仍会清理。
- ingest 与 AI process 启动、领取任务前各调用一次 `purge_expired(now, batch_size)`，固定一批 500 条。
  无后台 GC，无循环排空；没有相关运行时不会推进清理，积压可能需要多次运行。
- 只删除到期的 inline 行，按 `expires_at` 最旧优先（相同期限不保证顺序，直接使用既有到期索引）；期限严格早于当前 UTC 秒，
  SQLite 数值时间复核防止不同小数精度造成提前删除，临界记录允许延后。
- SQLite 单语句原子删除；PG 锁定候选并 `SKIP LOCKED`，并发 upsert/cleanup 不复用陈旧候选集。
  `NULL`、未来期限及 file-backed 行保留；续期后的行不删除。
  PG 清理事务局部设置 1 秒 lock timeout、5 秒 statement timeout，覆盖外键置空的引用行锁等待；
  超时回滚本批，后续运行可重试，不修改连接池的长期设置。
- 现有 `ON DELETE SET NULL` 只清空 `articles.body_html_artifact_id` 和
  `article_ai_results.raw_response_artifact_id`；业务行、已解析正文/摘要和报告快照不删除。
  过期原文将无法 replay，这是 TTL 的预期结果。
- 删除条数有界不等于耗时严格 `O(batch)`：还包括索引访问、payload 页回收与受影响引用数 R。
  `0005_artifact_reference_indexes` 为两个引用列添加索引，避免每删一条 artifact 都扫描整张业务表。
  新旧迁移的 up/down 文件保持双方言配对；旧 0001–0004 不修改。升级显式 `migrate run/check`；
  降级到仅识别 0001–0004 的二进制前，先用 SQLx migration undo 撤销 0005 及记录。
- 清理失败只记 warning，后续流程继续；删除非零时写 `artifacts_purged` 事件，count 为实际删除行数。
  零删除静默。清理不会自动执行 VACUUM，也不保证 SQLite 文件立刻缩小。

## 4. `replay` 子命令

入口：[`crates/cli/src/commands/replay.rs`](../../crates/cli/src/commands/replay.rs)。

```bash
rss-ai-news replay --kind ai     --id 12345
rss-ai-news replay --kind html   --key entry-key-...
rss-ai-news replay --kind feed   --id 678
```

三种 kind 行为：

| kind | 解析路径 | 输出 |
|---|---|---|
| `feed` | `rss_ai_news_feed::parse_feed(bytes, FeedKind::Rss)` | `{ "entry_count": N, "samples": [...] }` |
| `html` | `ReadabilityStrategy::extract(html_bytes)` | 抽取出的 title / body 长度 / 是否触发 fallback |
| `ai` | `rss_ai_news_ai::parse_response(raw)` | 解析后的 `ParsedResponse`（含 schema 校验） |

副作用：**无**。`replay` 不写任何业务表，仅查询 `raw_artifacts` 并在内存中重做，
打印 `ReplayCommandSummary`（pretty 或 JSON）。

`--id` 直查 `raw_artifacts.id`，`--key` 走 `(kind, key)` 联合索引。两者必给其一，
否则 CLI 报 `replay requires either --id or --key`。

### 4.1 diff 字段

当 artifact 行已关联到下游业务记录（如 ai_raw_response 对应已 succeeded 的 `article_ai_results`），
replay 会同时取出当时的解析结果与本次解析结果做 diff，输出在 `ReplayCommandSummary.diff`。
这是"线上结果跟当下代码是否一致"的诊断点。

## 5. `backfill` 子命令

入口：[`crates/cli/src/commands/backfill.rs`](../../crates/cli/src/commands/backfill.rs) →
`BackfillFlow`（[`crates/runtime/src/flows/backfill.rs`](../../crates/runtime/src/flows/backfill.rs)）。

```bash
rss-ai-news backfill --target extract --date-from 2026-05-01 --date-to 2026-05-10
rss-ai-news backfill --target ai      --date-from 2026-05-01 \
    --prompt-version-tag exp-v2 --prompt-template-path prompts/exp.md \
    --model gpt-4o --batch-size 50
```

### 5.1 `--target extract`

调用 `feed_entry_repo.reset_failed_in_window(...)`：

- 范围：`created_at ∈ [date_from, date_to]` 且当前 state ∈ `{Failed, FallbackPersisted}`
- 动作：state 重置回 `PendingFetch`，清理 `lease_*` 与 `attempt_count`
- 不动 `Persisted` 行（已成功的不重做）
- 不写新 `articles`/`raw_artifacts`

`BackfillExtractSummary { examined, reset }` 返回审计计数。
后续 `ingest` 或 `extract` run 会把这些 entry 重新走一遍状态机。

### 5.2 `--target ai`

步骤：

1. **新建 prompt_version 行**：根据 `--prompt-version-tag` / `--prompt-template-path` /
   `--model` 算 SHA-256，写入 `prompt_versions`（state=`active`，旧版本保留 `superseded`）
2. **扫描候选 articles**：时间窗内、当前无 ai_result 或 ai_result 不属于新 prompt_version 的行
3. **批量插入 `article_ai_results` 行**：state=`Pending`、关联新 prompt_version_id
4. **ON CONFLICT 不重复**：通过 `(article_id, prompt_version_id)` 唯一键避免重复入队

返回 `BackfillAiSummary { new_prompt_version_id, articles_scanned, ai_tasks_inserted, ai_tasks_conflict }`。

后续 `ai-run` 会消费这些 Pending 任务。这是 prompt 实验的标准入口：**不**直接覆盖已有
`article_ai_results`，而是新建 prompt_version 并产生平行结果，发布侧由 active rule resolver
决定使用哪个版本。

### 5.3 与 reindex 的边界

`reindex` 处理**升级算法导致的字段重算**（link_hash / content_hash / categories）；
`backfill` 处理**业务状态的重做**。两者底层都用 rule_versions，但 reindex 走 reindex_jobs
+ partial unique index 的 claim/lease 机制，backfill 不走。详见 [./05-storage.md](./05-storage.md) §8。

`--target categories` 还承担 `feed_sources` 的**整集对账**：配置里改名 / 删除 source 后，
ingest（per-source UPSERT）只会新建新行、留下旧 key 的孤儿行；归档孤儿由 categories
reindex 完成（`--dry-run` 先报 `archived: N`）。该生命周期约定见
[./06-config.md](./06-config.md) §7.1。

## 6. `rebuild-report` 子命令

入口：[`crates/cli/src/commands/rebuild_report.rs`](../../crates/cli/src/commands/rebuild_report.rs) →
`RebuildReportFlow`（[`crates/runtime/src/flows/rebuild_report.rs`](../../crates/runtime/src/flows/rebuild_report.rs)）。

```bash
rss-ai-news rebuild-report --publish-record-id 42
rss-ai-news rebuild-report --publish-record-id 42 --local-only
```

行为：
1. 读 `publish_records` 行（含冻结的 `snapshot_*` 字段）
2. 用当前模板（`[publish.template]` + 分类 override）+ snapshot 重新渲染 Markdown
3. 写入 `local_output_dir/<path>` + 可选 GitHub
4. **不**修改 `publish_records` 行本身（snapshot 仍冻结）

字节相等保证：
- 若模板未变 → 重建结果与原文件 byte-for-byte 一致
- 若模板已变 → 字节差异即为模板变更影响范围，可作为模板修复的回归依据

详见 [./04-publish.md](./04-publish.md) §冻结快照 与
[../adr/0003-publish-snapshot-immutable.md](../adr/0003-publish-snapshot-immutable.md)。

## 7. 失败语义

| 场景 | exit code | 行为 |
|---|---|---|
| `replay` artifact 找不到 | 3 | `CliError::ReplayArtifactNotFound`，stderr 一行 |
| `replay` 找到但 inline_body 为空 | 3 | "file-backed artifacts not supported in W9c" |
| `backfill --target ai` 范围内无候选 | 0 | `articles_scanned=0`，正常退出 |
| `backfill` DB 写失败 | 3 | 透传 `RuntimeError::Storage` |
| `rebuild-report` publish_record 不存在 | 3 | `CliError::PublishRecordNotFound` |
| `rebuild-report` GitHub 推送 422 lost-update | 3（默认） / 0（带 retry） | 详见 [./04-publish.md](./04-publish.md) §422 |

exit code 详表见 [./11-error-and-recovery.md](./11-error-and-recovery.md)。

## 8. 当前实现入口

| 内容 | 路径 |
|---|---|
| replay CLI | [`crates/cli/src/commands/replay.rs`](../../crates/cli/src/commands/replay.rs) |
| backfill CLI | [`crates/cli/src/commands/backfill.rs`](../../crates/cli/src/commands/backfill.rs) |
| backfill Flow | [`crates/runtime/src/flows/backfill.rs`](../../crates/runtime/src/flows/backfill.rs) |
| rebuild-report CLI | [`crates/cli/src/commands/rebuild_report.rs`](../../crates/cli/src/commands/rebuild_report.rs) |
| rebuild-report Flow | [`crates/runtime/src/flows/rebuild_report.rs`](../../crates/runtime/src/flows/rebuild_report.rs) |
| ArtifactWriter | [`crates/runtime/src/artifact.rs`](../../crates/runtime/src/artifact.rs) |
| raw_artifacts repo | [`crates/storage/src/repo/raw_artifact.rs`](../../crates/storage/src/repo/raw_artifact.rs) |
| Feed parser | [`crates/feed/src/lib.rs`](../../crates/feed/src/lib.rs) |
| AI response parser | [`crates/ai/src/lib.rs`](../../crates/ai/src/lib.rs) |
| Readability / Fallback 策略链 | [`crates/extractor/src/strategy.rs`](../../crates/extractor/src/strategy.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
