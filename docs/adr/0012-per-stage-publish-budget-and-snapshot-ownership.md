# ADR 0012: 发布按阶段计重试预算 + 文章的快照归属

- 日期：2026-11-21
- 状态：`accepted`（2026-11-21）
- 决策者：Develata（2026-11-21 按宪法 §7.1 批准本组设计：按阶段预算经 migration 0008、跨快照文章排他、
  多报告重试幂等、默认报告日期按目标时区）
- 来源：RSS-AI-News 审计报告 2026-11-21（main `f420d95`），§2 P1 第 5、6、7 项与 §2 表格"多报告重试会重复建 commit"；
  Codex 盲审与对抗核查均判 BLOCKER / SHOULD_FIX，复核后机制全部成立

## Context

发布是阶段驱动状态机（ADR 0002）：`pending → snapshot_frozen → rendered → stored_local → published_*`，
每个阶段各自 claim、各自释放 lease。审计确认了两类跨记录不变量缺失：

1. **重试预算四阶段共用。** 每次 claim `attempt_count + 1` 并过滤 `attempt_count < publish_max_attempts`，成功不清零。
   一份无故障的远端发布本身就要 4 次 claim，因此 `publish_max_attempts ≤ 3`（校验只要求 `> 0`）时远端阶段永远领不到；
   默认 5 时整条链只容忍 1 次失败，而且这 1 次额度被最先失败的阶段用掉。
2. **同一文章可进入两份活动快照。** 选稿只看 `articles.state = 'ready_for_publish'` 和时间窗；文章要到发布终态才升
   `published`。前一份记录卡在 freeze 之后时，下一份 freeze 会再选中同一批文章；两份都可能写出文件或推到 GitHub，
   之后后一份的数据库事务才因文章状态冲突回滚。AI-off 模式下还有 S9：先前快照失败后遗留的 `ready_for_publish`
   文章要求从 `persisted` 晋升，0 行即整批回滚，且 freeze 不释放 lease，每轮重复。
3. **多报告 GitHub 发布重试不幂等。** ref 更新成功但响应丢失时，重试以新 HEAD 为父再建一次提交。

约束：正确性优先于性能（宪法 §2.4）；双方言（ADR 0005）；迁移不可变、需 up + down；不引入新的常驻组件。

## Decision

### 1. 按阶段计预算（migration 0008）

- `publish_records` 增加 `stage_attempt_count`（`NOT NULL DEFAULT 0`）。
- 每次 claim 同时 `attempt_count + 1`、`stage_attempt_count + 1`；claim 过滤、retryable release 折叠
  （`stage_attempt_count >= max` → `failed`）与耗尽 sweep 一律用 `stage_attempt_count` 对比 `retry.publish_max_attempts`。
- 每次阶段推进成功（freeze 写快照、render / store_local / remote 的 advance，含终态 advance）把 `stage_attempt_count` 清零。
- `attempt_count` 保持累计语义（plan/11 §4.1），只作审计。
- 空快照（`SnapshotEmpty`）与 freeze 冲突释放（见 2）两个计数都退还。
- **存量行**：0008 之前的行 `stage_attempt_count = 0`，即当前阶段重新获得完整预算；终态行不受影响（`failed`
  不会因迁移复活）。这是有意的保守选择：旧的累计值无法拆分到阶段，给满预算最多多重试几次，不会把可恢复的行误判为耗尽。
- `publish_max_attempts = N` 的含义变为"每阶段最多 N 次尝试"。

### 2. 文章的快照归属

- **活动快照**：`publish_items` 中出现该文章、且所属 `publish_records.state <> 'failed'` 的记录。
  一篇文章同一时刻至多属于一份活动快照；记录转 `failed` 即释放归属。
- 三条选稿 SQL（AI 路径共用一条；AI-off 的 SQLite / PG 各一条）排除已被活动快照持有的文章。
  0008 同时增加 `idx_publish_items_article` 支撑这一查询。
- freeze 事务内复查（选稿在事务外）：
  1. 首条语句是带 owner/state 守卫的 record 推进（SQLite 由此持有写锁，串行化并发 freeze，避免读后升级写的 busy 失败）；
  2. PostgreSQL 按 id 顺序 `SELECT … FOR UPDATE` 锁住快照内文章——READ COMMITTED 下仅靠 `NOT EXISTS` 复查无法串行化，
     两个 freeze 都会通过检查；加锁后后到者等待，再看见先到者已提交的 items；固定顺序避免死锁；
  3. 被其它活动快照持有 → 回滚，`ArticleOwnershipConflict`；
  4. AI-off 文章按事务内读到的状态区分：`persisted` 晋升；已是 `ready_for_publish`（未被持有的遗留）保持原状冻结；
     其它状态 → 回滚，`ArticleStateConflict`；
  5. 写 publish_items，提交。
- 冲突时 runtime 立即释放 lease（`release_freeze_conflict`：记录回 `pending`、退还本次 claim、写 `last_error_kind`），
  不发生外部写入；下一次运行重新选稿时自然排除该文章。输掉竞争不是一次发布尝试，因此不消耗阶段预算——
  否则 `publish_max_attempts = 1` 时一次竞争就会让记录 `failed`。
- **存量重复归属的写前拦截（2026-11-22，R1-P-B1）**：0008 之前的库里可能已有两份未失败快照持有同一文章（迁移只建索引，
  不改数据；之后的 freeze 都会事务内复查，不会再产生新的重复）。因此 store_local 与 remote（单条与批量）在 claim 之后、
  任何外部写入（本地文件 / GitHub）之前，查询本记录快照中是否有文章同时属于另一份**胜出**的未失败记录：
  已 `published_*` 的记录无论 id 大小都胜出（其文章已是 `published`），其余情况 id 较小者胜出。
  败方不写任何外部产物：立即释放 lease、两个计数都退还、**保持当前阶段态**、`last_error_kind =
  article_owned_by_active_snapshot`、`last_error` 列出文章与胜方记录 id，阶段结果为 `ArticleConflict`（exit 1）；
  批量发布中只剔除败方，其余照常提交。胜方转 `failed` 后归属释放，败方下次运行即可继续；胜方已发布时败方无法完成，
  由运维把败方置为 `failed`（冻结快照保留）。

### 3. 多报告重试幂等

建 tree / commit 之前，按刚取到的 HEAD commit sha 查询批内每个路径的 blob sha，与本地按 git 规则计算的 blob sha
（SHA-1 over `blob <len>\0` + 内容）比对。**全部**一致才认定上一次尝试已落地，直接以该 HEAD commit 记回执（每份报告一条，
按输入顺序）；任一缺失 / 不一致照常提交；查询出错按原分类失败，绝不当作"已发布"。保留 422 non-fast-forward 重试。

**同一次尝试内的歧义写入核对（2026-11-22，R1-P-B2）**：写请求（批量的 `PATCH ref`、单报告的 `PUT contents`）已发出、
但结果未知——传输失败 / 响应不可读（分类为 503）或 5xx 应答——时，释放前先做只读核对：`GET ref` 取分支 HEAD commit，
再按该 commit 查每个路径的 blob（`find_published`，与上文同一比对）。全部一致 → 以 HEAD commit 记成功回执；不一致或核对本身
出错 → 原写入错误不变，按原分类释放。4xx（含 409、422）表示写入被拒，不核对。runtime 层的远端 deadline（lease 的 80%）
切断调用时同样核对，核对自己的时限为 deadline 之后再加 lease 的 10%（其余 10% 留给数据库收尾），lease 已失时由带 owner
守卫的推进拒绝。因此 `publish_max_attempts = 1` 或默认预算的最后一次尝试里"PATCH 已落地、响应丢失"也会记为
`published_remote`，而不是 `failed` 并释放文章归属。限流（`GitHubRateLimit`）不核对，避免在限流窗口内追加请求。
**残留限制**：进程在写请求之后、核对之前崩溃，且该阶段预算已耗尽时，过期清扫仍会把记录转 `failed`（未核对、归属释放）；
预算未耗尽时，下一次运行的多报告幂等比对 / 单报告幂等（§3 下文）会补回执。

### 4. 默认报告日期按目标时区

未给 `--date` 时，`publish` / `publish-all` 取运行时刻在有效目标时区（`--timezone` / `[publish].target_timezone`）下的日期。
时区经 jiff 按 IANA 库解析（有系统 zoneinfo 用系统库，否则用内置库），validate-config、doctor 与运行时用同一查询；
未知时区在任何写入之前报错，不回退 UTC。

## Alternatives considered

- **保留共享预算，校验 `publish_max_attempts ≥ 4`。** 改动最小，但只修掉"小预算必然失败"这一表象：每阶段可用的失败次数
  仍取决于前面阶段花掉多少，默认 5 时整链仍只容忍 1 次失败；而且把一个阶段机的内部细节（阶段数）泄漏成配置下限，
  阶段数一变配置就要跟着改。未采用。
- **只在冲突时检测（conflict-only），不在选稿时排除。** 即沿用现状：重复入选，等到发布终态推进文章时再以
  `ArticleStateConflict` 回滚。问题是冲突发现得太晚——两份快照都可能已经写出本地文件或推到 GitHub，回滚只回滚数据库。
  归属必须在任何外部写入之前确定，因此选稿排除 + freeze 事务内复查。未采用。
- **用唯一约束表达归属。** 归属依赖另一张表的状态（`state <> 'failed'`），无法用 `publish_items` 上的普通或部分唯一索引表达；
  另建归属表会引入第二个真相源。未采用。
- **多报告幂等改为查整棵 tree（`git/trees?recursive=1`）。** 大仓库响应可能超过 16 MiB 上限且被截断，截断的列表不能作为
  "全部存在"的证据；按路径查询 N 次（N = 本批分类数，通常个位数）更稳。未采用。

## Consequences

### 正面后果
- `publish_max_attempts` 的语义与阶段数解耦；小预算的无故障运行可以完成。
- 同一文章不会同时出现在两份活动快照里，也就不会先外部写入、后数据库回滚。
- AI-off 遗留文章不再卡住整批 freeze；freeze 冲突不再占着 lease 等到过期。
- 多报告重试不会在 ref 已更新时再叠一个提交。
- 默认报告日期与目标时区一致（如上海早上 8 点前不再落到前一天）。

### 负面后果 / 代价
- 选稿多一个 `NOT EXISTS` 子查询（有索引）；多报告批次在建 commit 前多 N 次 `GET contents`。
- 迁移 0008 改变 `publish_max_attempts` 的含义：同一配置值下可用的总尝试次数变多（最多 4 × N）。
- **已知限制（未在本 ADR 解决）**：各阶段都按本次运行 `init` 返回的 `publish_record_id` 领取，没有任何入口会自动续跑或
  终结一份停在中间阶段、且 lease 已清、阶段预算未耗尽的**更早日期**的记录。归属规则下这样的记录会一直持有其文章，
  使它们进不了之后的报告，直到运维对该日期重跑 `publish --date <那天>`（续跑或失败后释放）或人工把记录置为 `failed`。
  此前同样的记录也不会被续跑，但文章仍可能被重复选入；现在改为"不重复、但需要人工收尾"。
- 新依赖：`jiff`（IANA 时区；内置 tzdb）、`sha1`（git blob id，lockfile 中已有同版本）。

### 后续行动
- 评估为"更早日期的中间态记录"增加有界的自动收尾（例如 maintenance 中按 report_date 过期终结），需另行设计与审批。
- 单报告批次走 Contents API，不在本 ADR 的幂等范围内。

## Links

- 代码：`migrations/{sqlite,postgres}/0008_publish_stage_budget_and_item_article_index.*.sql`；
  `crates/storage/src/repo/publish_record_sql.rs`、`publish_item.rs`；`crates/runtime/src/flows/publish/freeze.rs`、
  `report_date.rs`；`crates/publish/src/github.rs`（`head_contains_blobs`、`git_blob_sha`）
- 计划：[plan/04 §2、§4.2](../plan/04-publish.md)、[plan/08 §6.1](../plan/08-state-machines.md)、
  [plan/11 §4.1](../plan/11-error-and-recovery.md)、[plan/15](../plan/15-retry-exhaustion-and-reclaim.md)、
  [plan/05 迁移表](../plan/05-storage.md)
- 验收：[acceptance-cases/pipelines/04-publish-local-and-github.md](../acceptance-cases/pipelines/04-publish-local-and-github.md)
- 相关 ADR：[0002](./0002-stage-driven-lease-claim.md)、[0003](./0003-publish-snapshot-immutable.md)、[0005](./0005-storage-pool-dual-dialect.md)
