# CLI 语义设计

## 1. 定位

本文档定义 Rust 版 CLI 的命令树、参数语义、输出格式和 exit code 约定。CLI 是四层架构中的交互壳层，负责输入采集、参数转译和结果呈现。

命令列表来自 [工程蓝图 §3.1.F](../plan/full-rust-rss-ai-news-blueprint.md) 和 [宪法对齐 §3](./engineering-constitution-alignment.md)。

## 2. 命令树

```text
rss-ai-news
├── ingest          主流程：拉取 feed → 去重 → 抓正文 → 存库
├── ai-run          AI 处理：领取待处理文章 → 调 AI → 写回结果
├── publish         发布：选稿 → 冻结快照 → 渲染 → 本地存储 → 远端推送
├── doctor          健康检查：配置、数据库、外部服务连通性
├── replay          回放：从 raw_artifact 重放 feed/html/ai 解析
├── backfill        补跑：对历史数据重跑正文提取或 AI
├── rebuild-report  重建报告：从发布快照重新渲染 Markdown
├── reindex         重算：重算 link_hash / content_hash / 派生字段
├── migrate         数据库迁移
│   ├── run         执行 pending migrations
│   └── check       检查版本状态，不执行
├── validate-config 校验配置文件
└── run             一体化执行：ingest + ai-run + publish 按顺序执行
```

## 3. 全局 Flag

以下 flag 可在任何命令前使用：

| Flag | 短 | 类型 | 默认 | 说明 |
|---|---|---|---|---|
| `--config-dir` | `-c` | path | `./configs` | 配置目录（目录名固定为 `configs/`，与仓库骨架 [task T103](../task/full-rust-rss-ai-news-blueprint-tasks.md) 保持一致）|
| `--db-path` | | path | `app.toml` 中的值 | 覆盖数据库路径 |
| `--log-level` | | enum | `info` | `trace`/`debug`/`info`/`warn`/`error` |
| `--log-format` | | enum | `pretty` | `pretty`/`json` |
| `--output-format` | `-o` | enum | `pretty` | `pretty`/`json`，所有命令的最终结果输出格式（见 §5.2）|
| `--dry-run` | `-n` | bool | false | 只打印将要执行的操作，不写入。**v0.1.0**：仅 `reindex` 子命令实装；其它命令（`ingest` 等）传入会返 `DryRunNotImplemented`，留待 v0.2 |
| `--category` | `-C` | string | 全部 | 只处理指定分类 |
| `--timezone` | | string | `app.toml` 中的值 | 覆盖时区 |

`--log-format` 与 `--output-format` 的区别：

- `--log-format` 控制 `tracing` 的事件流（debug/info/warn/error 行），通常写入 stderr
- `--output-format` 控制命令"最终结果"的呈现（如 `ingest` 的统计摘要），通常写入 stdout
- JSON 模式下两者互不干扰：日志行作为独立 JSON 流写 stderr；最终结果作为单个 JSON 对象写 stdout

## 4. 命令详细语义

### 4.1 `ingest`

**用途**：拉取 feed、去重、抓正文、存库。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--source` | string | 全部 | 只处理指定源（`category_key:source_key`）。**v0.1.0**：CLI flag 暴露但传入会返 `IngestSourceFilterNotImplemented`（runtime 层的 category/max_sources 过滤已支持，但 source-level filter 尚未接通），留待 v0.2 |
| `--skip-fetch` | bool | false | 跳过正文抓取（只发现 + 去重）|
| `--batch-size` | u32 | 50 | 每批抓取任务数 |
| `--max-batches` | u32 | `runtime.max_batches_per_run`（默认 10）| `extract` 阶段 claim 循环上限（不约束 fetch 阶段，后者由 `concurrent_feeds` + 宿主超时兜底）；`0` = 不限。详见 [config-schema §4.4](./config-schema.md#44-runtime-字段语义) |

**行为**：

1. 加载配置，过滤 `--category` / `--source`
2. 遍历 active 源，拉取 feed
3. 对每条条目执行三层去重
4. 将 fresh 条目 INSERT 到 `feed_entries`
5. 领取 `pending_fetch` 任务批次，抓取正文
6. 正文提取成功 → 写入 `articles`，状态推进到 `persisted`
7. 正文提取失败 → 降级到 `summary_fallback` 或标记 `failed`

**输出**：

```text
Ingest completed:
  Sources checked:   15
  Entries discovered: 42
  Dedup skipped:     28 (uid: 20, link: 6, hash: 2)
  Articles created:  14
  Fetch failed:       0
  Duration:          12.3s
```

**Exit code**：0 成功（含部分 source 失败），1 全量失败。

### 4.2 `ai-run`

**用途**：领取待 AI 处理的文章，调用 AI，写回结果。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--batch-size` | u32 | 20 | 每批处理数 |
| `--max-batches` | u32 | `runtime.max_batches_per_run`（默认 10）| `process` 阶段 claim 循环上限（不约束 `task_gen` one-shot 扫描）；`0` = 不限。详见 [config-schema §4.4](./config-schema.md#44-runtime-字段语义) |
| `--model` | string | `app.toml` 中的值 | 覆盖模型 |

**行为**：

1. **Task 生成阶段**（入口）：查询 `articles.state = 'persisted'` 的文章，对每篇在同事务内
   - INSERT 一行 `article_ai_results` state='pending'，幂等四元组 `(article_id, prompt_version, output_schema_version, model_id)`
   - UPDATE `articles.state='ai_pending'`
   - UNIQUE 冲突 → 说明并发生成，回滚本次，按真相源现状继续
2. **Claim 阶段**：从 `article_ai_results.state='pending'` 领取批次，设 lease
3. **调用阶段**：组装 prompt，截断输入，调用 AI
4. **完成阶段**：解析结构化输出，写回 `article_ai_results`，同事务推进 `articles.state` 到 `ai_done` / `ready_for_publish` / `publish_skipped`
5. retryable 失败 → `running→pending` 回落（见 [state-machine §4.2](./state-machine.md)）
6. permanent 失败 → `running→permanent_failed`；若存在其他版本 succeeded 行，`articles.state` 保持不变

**输出**：

```text
AI run completed:
  Articles processed: 14
  Succeeded:         12
  Filtered:           1
  Failed (retryable): 1
  Duration:          45.2s
```

### 4.3 `publish`

**用途**：选稿、冻结快照、渲染报告、发布到本地和/或 GitHub。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--date` | string | 今天 | 目标日期（`YYYY-MM-DD`）|
| `--local-only` | bool | false | 只输出本地，不推 GitHub；该模式下跳过 `GITHUB_TOKEN` / `github_*` 配置校验 |
| `--force` | bool | false | 对已完成批次强制生成新批次：不覆盖既有 `publish_records`，而是 bump `render_version` 后用新 key 重发（见 §4.3 附注）|

**行为**：

1. 决定发布模式：`--local-only=true` 或 `config.publish.github_owner` 为空 → 本地模式；否则远端模式
2. 计算 `idempotency_key = {category}-{date}-{render_version}`（`render_version` 是当前活跃 `rule_versions.id`）
3. 检查是否已存在同 key 的 `publish_records`
   - 已存在且已完成（`published_local` / `published_remote`）→ 跳过；若带 `--force` 则 bump `render_version` 后生成新 key、新批次
   - 已存在但未完成（中间态且 lease 过期）→ 按当前 `state` 从中断处恢复；`--force` 不影响此分支
   - 已存在且处于 `failed` → 生成新 key（同 `--force` 路径）
   - 不存在 → 创建新记录
4. 选稿：按 effective `ai.enabled × include_unscored` 真值表（[config-schema §4.1](./config-schema.md#41-aienabled--publishinclude_unscored-真值表)）确定候选源；AI-off 直通时 `persisted` 候选若入选，必须在下一步 freeze 同事务内升格为 `ready_for_publish`
5. 冻结快照：写入 `publish_items`（同事务内完成 `persisted → ready_for_publish` 升格，避免破坏 I4 / I4'）
6. 渲染 Markdown
7. 写入本地文件
8. 本地模式 → 进入 `published_local` 终态；远端模式 → 推送到 GitHub，进入 `published_remote` 终态

**`--force` 语义约束**：`--force` 永不覆盖或删除已存在的 `publish_records` 行；它只通过 bump `render_version` 打开一条新的 key 路径。这是为了保持"发布快照一经冻结即不可变"的硬约束。

**输出**：

```text
Publish completed:
  Category: ai
  Date:     2025-01-15
  Items:    12
  Local:    output/ai/2025-01-15.md
  GitHub:   https://github.com/owner/repo/commit/abc1234
```

### 4.4 `doctor`

**用途**：全面健康检查。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--deep` | bool | false | 扫描 [state-machine §7.2](./state-machine.md#72-跨状态机不变量) 定义的 canonical 不变量集 I1–I8 + I4'。典型违反示例分别对应 I2、I5、I6：`ai_pending` 无任一 AI 行、`published` 无成功发布、`published_remote` 或 `published_local` 但被引用的 article 未同步为 `published` |

**行为与输出**：见 [error-and-observability §5](./error-and-observability.md)。`--deep` 会额外输出不变量扫描结果，可能显著延长执行时间（全表扫描若干）。

**`--deep` canonical SQL（节选 I4 / I4'，其它不变量同形式）**：

```sql
-- I4 违反：ready_for_publish 但既非 AI 路径（无 succeeded+keep）也非直通路径（仍存在 AI 行）
-- 即 I4.a 与 I4.b 同时不成立的脏状态（AI 路径未走完或 AI 已永久失败/过滤却被错误升格）
SELECT a.id
FROM articles a
WHERE a.state = 'ready_for_publish'
  AND NOT EXISTS (
    SELECT 1 FROM article_ai_results aar
    WHERE aar.article_id = a.id
      AND aar.state = 'succeeded'
      AND aar.keep_decision = 1
  )                                         -- 不满足 I4.a
  AND EXISTS (
    SELECT 1 FROM article_ai_results aar
    WHERE aar.article_id = a.id
  );                                        -- 不满足 I4.b（直通要求 NOT EXISTS 任何 AI 行）

-- I4'.a 违反：freeze 后 publish_items 绑定到 AI 结果，但该结果不可保留
SELECT pi.id, pi.article_id, pi.article_ai_result_id
FROM publish_items pi
WHERE pi.article_ai_result_id IS NOT NULL
  AND NOT EXISTS (
    SELECT 1 FROM article_ai_results aar
    WHERE aar.id = pi.article_ai_result_id
      AND aar.state = 'succeeded'
      AND aar.keep_decision = 1
  );

-- I4'.b 违反：freeze 后 publish_items 走直通路径，但 article 仍有任何 AI 结果行
-- （包括 filtered / permanent_failed / pending / running，全都排除直通）
SELECT pi.id, pi.article_id
FROM publish_items pi
WHERE pi.article_ai_result_id IS NULL
  AND EXISTS (
    SELECT 1 FROM article_ai_results aar
    WHERE aar.article_id = pi.article_id
  );

-- I6 违反：发布记录已成功终态，但被引用 articles 未同步为 published
SELECT pr.id, a.id AS article_id, a.state
FROM publish_records pr
JOIN publish_items pi ON pi.publish_record_id = pr.id
JOIN articles a ON a.id = pi.article_id
WHERE pr.state IN ('published_remote', 'published_local')
  AND a.state <> 'published';
```

其余 I1/I2/I3/I5/I7/I8 的 SQL 形式同上，由 `crates/runtime` 的 `doctor::deep_scan` 模块统一发布。

**Exit code**：0（全部 OK 或仅 WARN），1（存在 FAIL）。

### 4.5 `replay`

**用途**：从 `raw_artifacts` 重放解析过程，用于调试和验证。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--kind` | enum | 必填 | `feed` / `html` / `ai` |
| `--key` | string | 互斥 | artifact_key |
| `--id` | i64 | 互斥 | artifact id |
| `--diff` | bool | false | 与当前数据库状态对比 |

**行为**：

1. 从 `raw_artifacts` 读取指定 artifact
2. 根据 `kind` 进入对应解析入口
3. 输出解析结果（人类可读）
4. 若 `--diff`，对比当前数据库中的对应记录

**输出**：解析结果 + 可选 diff。脱离外网可执行。

### 4.6 `backfill`

**用途**：对历史数据重跑正文提取或 AI。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--target` | enum | 必填 | `extract` / `ai` |
| `--date-from` | string | 无 | 起始日期 |
| `--date-to` | string | 无 | 结束日期 |
| `--batch-size` | u32 | 50 | 每批数量 |

**行为**：根据 `--target` 重新创建提取或 AI 任务，携带新版本号。

### 4.7 `rebuild-report`

**用途**：从发布快照重新渲染 Markdown，不触发 AI。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--publish-id` | i64 | 互斥 | 指定 publish_record id |
| `--date` | string | 互斥 | 指定日期 + `--category` |
| `--output` | path | stdout | 输出路径 |

**行为**：读取 `publish_items`，用当前渲染模板重新生成 Markdown。

### 4.8 `reindex`

**用途**：规则升级触发的批量重算（`link_hash` / `content_hash` / `categories`）。运行模型见 [state-machine §6](./state-machine.md#6-reindex_job-独立状态轮)，持久化 schema 见 [storage-schema §4.10](./storage-schema.md#410-reindex_jobs)。

**参数**：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--target` | enum | 必填 | `link_hash` / `content_hash` / `categories` / `all`（顺序执行三类，逐类提交独立 job）|
| `--batch-size` | u32 | 100 | 每批 commit 行数；不受 `runtime.max_batches_per_run` 控制 |
| `--dry-run` | bool | false | 仅统计将更新行数与待写入 rule_versions 元数据；不 INSERT `rule_versions`、不 INSERT `reindex_jobs`、不更新任何数据行 |
| `--abort` | string | - | 取消指定 job_id（`reindex_jobs.id`），状态推进到 `aborted`；保留已更新批次（active rule 保护数据语义正确）|

**行为（两阶段语义）**：

1. **启动阶段**（同事务）：
   - INSERT `rule_versions` 新行，`status='pending'`，`payload_sha256` 取自当前规则配置
   - INSERT `reindex_jobs` 新行，`state='pending'`，`rule_version_id` 指向上行
   - `target='all'` 时按顺序生成三个独立 job（每个 target 一个 reindex_job + 一个 pending rule_version）
2. **claim 阶段**：runtime 取得 lease（同 target 已有 `pending`/`running` job 时拒绝，partial unique index 保证）；状态 `pending → running`
3. **批次执行阶段**：
   - 按 `id` 升序批量 SELECT 待更新行（`WHERE id > last_processed_id ORDER BY id LIMIT batch_size`）
   - 重算派生字段，UPDATE 数据行 + UPDATE `reindex_jobs.last_processed_id`，**每批一个事务 COMMIT**
   - 批次提交后 lease 续期；崩溃/超时由 reclaim 总则按 `running → pending` 处理，下次 claim 从 checkpoint 继续
4. **激活阶段**（终止事务）：
   - 当 `last_processed_id == max(target_table.id)`：同一事务内 UPDATE `rule_versions.status: pending → active`、同 kind 旧 active 行 → `superseded` + `retired_at`、`reindex_jobs.state='completed'` + `finished_at`
   - 此事务对外原子可见：`active_rule(kind)` resolver 在事务前后一致返回单一 active 行
5. **失败/取消**：见 [state-machine §6.6 失败路径](./state-machine.md#66-失败路径)；新 `rule_versions` 保持 `pending` 直到管理员清理

**与其它命令的并发约束**：

- 同 target 的 reindex_job 互斥（partial unique index）；不同 target 可并发（`link_hash` 与 `categories` 同时 reindex 不冲突）
- ingest / ai-run / publish / extract 与 reindex **不互斥**：所有读规则的命令通过 `active_rule(kind)` resolver 取规则，reindex 期间始终读到旧 active rule（详见 [state-machine §6.4](./state-machine.md#64-active-rule-resolver)）
- `migrate run` 与 `running` reindex_job 互斥：migrate 启动前必须无 running reindex（doctor 应验证）；规则升级走 reindex，schema 升级走 migrate，二者职责边界明确

**进度输出**：

```text
Reindex started: target=link_hash job_id=42 rule_version_id=17
  [batch 1/?] processed 100 rows, last_id=10042 (12.3 rows/s)
  [batch 2/?] processed 200 rows, last_id=10142 (15.1 rows/s)
  ...
  Activating: rule_versions 17 pending → active, 16 active → superseded
Reindex completed: 12834 rows in 142s
```

`--dry-run` 输出仅前两行（启动信息）+ 「Would update N rows for target X with new rule sha256 ...」；不写任何表。

**Exit code**：0 完成；1 运行时错误（job 进入 `failed`）；2 参数错误；其它见 [§6](#6-exit-code-约定)。

### 4.9 `migrate`

**子命令**：

- `migrate run`：执行所有 pending migrations。输出已应用的版本列表。
- `migrate check`：检查当前版本与代码内嵌版本的差异，不执行。

### 4.10 `validate-config`

**用途**：仅校验配置文件，不启动任何流程，不连接数据库或外部服务。

**与 `doctor` 的关系**：`validate-config` 等价于 `doctor` 中"配置文件存在且合法"这一项检查的独立入口，复用同一份校验实现（位于 `crates/config`）。`doctor` 内部直接调用该函数，二者结果保持一致。

**为何独立成命令**：

- CI / 预发流水线中需要在不具备数据库或外部访问条件时仅校验配置
- Docker 镜像启动前的快速 fail-fast 检查
- 用户编辑配置后的本地反馈回路（无需启动完整 doctor）

**行为**：执行 [config-schema §6](./config-schema.md) 中定义的全部校验。

**Exit code**：0（合法），78（不合法）。

### 4.11 `run`

**用途**：一体化执行 `ingest` + `ai-run` + `publish`。

**参数**：接受所有三个子命令的参数，通过前缀区分（如 `--ingest-batch-size`），或直接继承全局 flag。

**`--max-batches` 继承语义**：`run` 自身接受 `--max-batches`（覆盖 `runtime.max_batches_per_run`）；`run` 内部触发的 `ingest` / `ai-run` 阶段沿用同一个生效值（即 CLI flag > config > 默认 10），不引入 `--ingest-max-batches` / `--ai-run-max-batches` 复合参数。`publish` 阶段不受 `max_batches_per_run` 控制（见 [config-schema §4.4](./config-schema.md#44-runtime-字段语义)）。

**行为**：按顺序执行三个阶段。任一阶段失败不阻塞后续阶段（除非 `ingest` 全量失败导致无新文章）。

**`ai.enabled=false` 下的阶段编排**：当 effective `config.ai.enabled=false` 时，`run` 命令**主动跳过** `ai-run` 阶段，直接进入 `publish` 阶段（依赖 [config-schema §4.1](./config-schema.md#41-aienabled--publishinclude_unscored-真值表) 的 `(ai=false, include_unscored)` 行为）。具体：

- `run` 在 ai-run 阶段产生一行 `[INFO] AI disabled (ai.enabled=false), skipping ai-run` 并直接推进；不返回 exit 78
- `publish` 阶段按真值表行为执行：`include_unscored=true` 时正常发布直通候选；`include_unscored=false` 时本轮无候选，发布产生 0 条 publish_items 并返回 exit 0
- 整体 exit code：以最严重的阶段结果为准（ingest WARN + publish OK → 0；任一阶段 FAIL → 非 0）

**`ai-run` 单独调用与 `run` 内部跳过的差异**：`ai.enabled=false` 时显式调用 `ai-run` 仍按 [config-schema §6.2](./config-schema.md) 返回配置语义错误 exit 78（用户显式表达了与配置矛盾的意图，应失败）；而 `run` 是隐式编排，跳过更符合调用者意图。该差异由 `runtime::orchestrator` 区分调用路径实现，**不**通过新增 CLI flag 暴露。

## 5. 输出格式约定

### 5.1 人类可读（默认）

- 摘要统计表（见各命令示例）
- 错误使用红色（tty 检测）
- 警告使用黄色

### 5.2 JSON 输出

所有命令支持 `--output-format json`，输出结构化 JSON：

```json
{
  "command": "ingest",
  "status": "success",
  "summary": {
    "sources_checked": 15,
    "entries_discovered": 42,
    "dedup_skipped": 28,
    "articles_created": 14,
    "fetch_failed": 0,
    "duration_seconds": 12.3
  },
  "errors": []
}
```

### 5.3 `--dry-run` 输出（v0.2 设计稿；v0.1.0 仅 `reindex --dry-run`）

> **v0.1.0 实装边界**：仅 `reindex --dry-run` 完整实装（见 §4.8）；
> 全局 `--dry-run` 应用到 `ingest` / `ai-run` / `publish` 等子命令时返回
> `DryRunNotImplemented`（exit 1）。本节文本描述的 `[DRY RUN] Would ...`
> 输出格式属 v0.2 follow-up，作为未来实装时的契约稿保留。

`--dry-run` 模式下，所有写操作替换为"将要执行"的描述：

```text
[DRY RUN] Would insert 14 entries into feed_entries
[DRY RUN] Would fetch 14 article pages
[DRY RUN] Would create 14 articles
```

## 6. Exit Code 约定

| Code | 含义 |
|---|---|
| 0 | 成功（含部分可恢复的 source 级失败）|
| 1 | 运行时错误 |
| 2 | 用户输入/参数错误 |
| 78 | 配置错误（EX_CONFIG）|

## 7. 与宪法的对齐检查

- §3.2 replay / backfill / rebuild-report / reindex 作为正式 CLI 命令 ✓
- §3.1 doctor 是正式命令 ✓
- 使用体验：错误提示能定位到配置、网络、提取、AI、发布层次 ✓
- 幂等：`publish` 的 `idempotency_key` 保证重复执行安全 ✓
