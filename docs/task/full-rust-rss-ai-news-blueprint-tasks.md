# RSS-AI-News 全 Rust 蓝图任务分解

## 1. 说明

本任务文档从 [工程蓝图](../plan/full-rust-rss-ai-news-blueprint.md) 拆解而来，参考 Spec Kit 的任务风格，但按本仓库工程宪法做了更强的骨架约束。

规则：

- 先骨架，后模块，最后实现
- 先定义真相源、状态机、配置和 DTO，再写业务代码
- 所有任务默认面向 `E:\gitclone\RSS-AI-News`

任务状态建议：

- `[ ]` 未开始
- `[~]` 进行中
- `[x]` 完成

### 1.1 Workstream 与 Phase 的区分

本文档的 `Workstream W0–W10` 是**工作流分组**，按"骨架→模块→闭环"的颗粒度组织任务。
[蓝图 §15](../plan/full-rust-rss-ai-news-blueprint.md) 的 `Phase 0–5` 是**里程碑节点**，按"可交付的端到端能力"组织。

二者并非一一对应，对应关系如下：

| Blueprint Phase | 对应 Workstream | 交付物 |
|---|---|---|
| Phase 0 文档冻结 | W0 | 全套设计文档 |
| Phase 1 最小闭环 | W1 + W2 + W3 + W4 + W5 | ingest 端到端可跑 |
| Phase 2 正文提取 | W6 | extract 闭环 |
| Phase 3 AI 闭环 | W7 | ai-run 闭环 |
| Phase 4 发布闭环 | W8 | publish 闭环 |
| Phase 5 硬化 | W9 + W10 | doctor / metrics / Docker / CI |

实施时按 Workstream 推进，按 Phase 验收。

## 2. Workstream W0：文档与骨架冻结

### T001 文档目录标准化

- [x] 建立并检查 `docs/constitution.md`
- [x] 建立并检查 `docs/design/`
- [x] 建立并检查 `docs/plan/`
- [x] 建立并检查 `docs/task/`
- [x] 建立并检查 `docs/handoffs/`
- [x] 补充 `docs/README.md`

### T002 宪法冻结

- [x] 审阅 [工程宪法](../constitution.md)
- [x] 确认优先级顺序无冲突
- [x] 确认四层架构 + 对象层已进入正式骨架

### T003 设计哲学冻结

- [x] 审阅 [核心设计哲学](../design/design-philosophy.md)
- [x] 确认"阶段驱动、快照驱动、回放驱动、租约驱动、版本化驱动"已成为终局约束

### T004 蓝图冻结

- [x] 审阅 [工程蓝图](../plan/full-rust-rss-ai-news-blueprint.md)
- [x] 确认模块、状态机、调用关系、失败路径、扩展点完整
- [x] 完成 A–T 一致性审阅并回写蓝图与对应设计文档

### T005 后续文档补齐

- [x] 新增 `docs/design/config-schema.md`
- [x] 新增 `docs/design/internal-dto-contracts.md`
- [x] 新增 `docs/design/cli-semantics.md`
- [x] 新增 `docs/design/error-and-observability.md`
- [x] 新增 `docs/design/replay-and-artifacts.md`
- [x] 新增 `docs/design/dependency-choices.md`
- [x] 新增 `docs/design/state-machine.md`
- [x] 新增 `docs/design/storage-schema.md`

## 3. Workstream W1：新仓库初始化

### T101 初始化 Rust workspace

- [x] 初始化根 `Cargo.toml`（`resolver = "3"`, `edition = "2024"`, `[workspace.dependencies]` 统一版本）
- [x] 初始化 `Cargo.lock`
- [x] 新增 `.gitignore`
- [ ] 新增 `rustfmt.toml`
- [ ] 新增 `clippy.toml`
- [ ] 在根 `Cargo.toml` 启用 `[workspace.lints]` 禁止吞错误 lint：`unused_must_use=deny`、`clippy::let_underscore_must_use=deny`、`clippy::let_underscore_future=deny`、`clippy::ok_expect=warn`、`clippy::ignored_unit_patterns=warn`（依据 [error-and-observability §3.3 Enforcement 第 1 层](../design/error-and-observability.md#33-绝不静默吞掉错误)）

### T102 初始化 crate 骨架

**最终决议**：根目录为二进制 crate `rss-ai-news`（对应旧方案的 `app` crate），其余全部下沉到 `crates/`。

- [x] 根 `src/` 作为 `rss-ai-news` binary（替代 `crates/app`）
- [x] 创建 `crates/cli`
- [x] 创建 `crates/domain`
- [x] 创建 `crates/config`
- [x] 创建 `crates/runtime`
- [x] 创建 `crates/storage`
- [x] 创建 `crates/feed`
- [x] 创建 `crates/extractor`
- [x] 创建 `crates/ai`
- [x] 创建 `crates/report`
- [x] 创建 `crates/publish`
- [x] 创建 `crates/observability`

### T103 建立目录与静态资源骨架

- [ ] 创建 `configs/`
- [ ] 创建 `configs/categories/`
- [ ] 创建 `migrations/`
- [ ] 创建 `docker/`
- [ ] 创建 `tests/fixtures/`
- [ ] 创建 `tests/integration/`

## 4. Workstream W2：领域对象与 DTO 契约

### T201 定义核心对象

- [ ] 在 `crates/domain` 中定义 `FeedSource`
- [ ] 定义 `FeedEntry`
- [ ] 定义 `Article`
- [ ] 定义 `ArticleAiResult`
- [ ] 定义 `PublishRecord`
- [ ] 定义 `PublishItem`
- [ ] 定义 `RawArtifact`
- [ ] 定义 `ReindexJob`（见 [storage-schema §4.10](../design/storage-schema.md#410-reindex_jobs)）

### T202 定义状态机

- [ ] 定义 `FeedEntryState`
- [ ] 定义 `ArticleState`
- [ ] 定义 `AiResultState`（不含 `RetryableFailed`；retryable 失败回落到 `Pending`）
- [ ] 定义 `PublishState`
- [ ] 定义 `ReindexJobState`（pending / running / completed / failed / aborted；见 [state-machine §6.2](../design/state-machine.md#62-状态集合)）
- [ ] 定义 `RuleVersionStatus`（pending / active / superseded；见 [storage-schema §4.8](../design/storage-schema.md#48-rule_versions)）

### T203 定义内部 DTO

- [ ] 定义 `FeedEntryMeta`（见 internal-dto-contracts §2.3）
- [ ] 定义 `ArticleFetchTask`（§3.1）
- [ ] 定义 `ExtractedArticle` 与 `FallbackArticle`（§3.2/§3.3）
- [ ] 定义 `AiTask` / `AiOutput` / `AiFilteredOutput`（§4.1–§4.3）
- [ ] 定义 `FrozenPublishItem` / `PublishCandidate` / `RenderedReport` / `PublishOutcome`（§5.2–§5.5）
- [ ] 定义 `ReplayRequest/Result` 与 `BackfillRequest`（§6）

### T204 定义稳定契约测试

- [ ] 为状态机转换写单元测试
- [ ] 为 link 规范化写单元测试
- [ ] 为 DTO 序列化 / 反序列化写单元测试

## 5. Workstream W3：配置系统

### T301 定义配置 schema 文档

- [ ] 完成 `docs/design/config-schema.md`
- [ ] 明确 `schema_version`
- [ ] 明确 `app.toml` 字段
- [ ] 明确 `categories/*.toml` 字段
- [ ] 明确 `.env` 字段

### T302 实现 config crate

- [ ] 加载 `.env`
- [ ] 加载 `app.toml`
- [ ] 加载 `categories/*.toml`
- [ ] 实现 schema version 校验
- [ ] 实现非法配置即退出

### T303 配置测试

- [ ] 合法配置通过测试
- [ ] 缺失字段失败测试
- [ ] 重复分类 key 失败测试
- [ ] 非法 URL 失败测试

### T304 实现 `validate-config` 子命令

参见 [cli-semantics §4.10](../design/cli-semantics.md)。命令在 W3 阶段完成端到端落地，避免 W9 doctor 重复包装配置校验逻辑。

- [ ] 在 `cli` crate 注册 `validate-config` 子命令（clap derive）
- [ ] 调用 `config::load_all` 完整加载 `.env` + `app.toml` + `categories/*.toml`
- [ ] 输出每个被加载文件路径与 schema_version
- [ ] 输出 effective `[ai].enabled × [publish].include_unscored` 真值表行（见 [config-schema §4.1](../design/config-schema.md#41-aienabled--publishinclude_unscored-真值表)）
- [ ] 退出码：合法 → 0；schema 不匹配 / 缺必填 / 非法 URL → exit 78；I/O 错误 → exit 74
- [ ] CLI 集成测试：合法配置返回 0；故意篡改的非法 toml 返回 78；缺失 OPENAI_API_KEY（且 `ai.enabled=true`）返回 78

## 6. Workstream W4：存储模型与 migration

### T401 设计并冻结数据库 schema

首版 migration **一次性**建齐所有表（见 storage-schema §3）；`raw_artifacts` 不是可选。

- [ ] 在 `migrations/` 中创建初始 migration
- [ ] 建 `feed_sources`
- [ ] 建 `feed_entries`
- [ ] 建 `articles`
- [ ] 建 `article_ai_results`
- [ ] 建 `publish_records`
- [ ] 建 `publish_items`
- [ ] 建 `raw_artifacts`
- [ ] 建 `rule_versions`（含 `status` 列 + partial unique index `UNIQUE (kind) WHERE status='active'`；首版数据 INSERT 时直接 `status='active'`）
- [ ] 建 `run_events`
- [ ] 建 `reindex_jobs`（含 `UNIQUE (target) WHERE state IN ('pending','running')` partial unique index；见 [storage-schema §4.10](../design/storage-schema.md#410-reindex_jobs)）

### T402 实现 storage crate

- [ ] 初始化连接池
- [ ] 实现 migration 执行入口
- [ ] 实现 repository trait
- [ ] 实现 SQLite 适配
- [ ] 预留 PostgreSQL 适配

### T403 实现 claim + lease

- [ ] 设计任务领取 SQL
- [ ] 设计 lease 过期回收 SQL
- [ ] 设计 attempt_count 更新逻辑
- [ ] 编写并发领取测试

### T404 幂等测试

- [ ] 重复 feed entry 插入幂等测试
- [ ] 重复 article 写入防重测试
- [ ] 重复 publish 创建防重测试

## 7. Workstream W5：Feed 抓取闭环

### T501 实现 feed crate

- [ ] 实现 HTTP client
- [ ] 实现 RSS parser
- [ ] 实现 Atom parser
- [ ] 实现 JSON feed parser
- [ ] 实现 `FeedEntryMeta` 规范化

### T502 实现条件请求

- [ ] 支持 ETag
- [ ] 支持 Last-Modified
- [ ] source 表更新条件请求字段

### T503 实现 ingest use-case

- [ ] 枚举可用 source
- [ ] 拉取 feed
- [ ] 解析 entry
- [ ] 第一层去重
- [ ] 第二层 `normalized_link` 去重
- [ ] 生成正文抓取任务

### T504 可观测性

- [ ] 记录 source 抓取成功 / 失败事件
- [ ] 输出新增 entry 数、跳过数、错误数

## 8. Workstream W6：正文提取闭环

### T601 初始化 extractor crate

- [ ] 实现详情页 HTML 抓取
- [ ] 实现内容大小限制
- [ ] 实现媒体类型过滤

### T602 实现多策略正文提取

- [ ] 规则型提取入口
- [ ] 通用 readability / 密度提取入口
- [ ] summary fallback
- [ ] content_quality 分级

### T603 正文去重

- [ ] 生成 `content_hash`
- [ ] 第三层内容去重
- [ ] 回退或失败状态推进

### T604 replay(html)

- [x] 保存 HTML artifact 的策略开关（已实现：`extractor::artifact::ArtifactWriter` + `RetentionPolicy` 五策略；在 strategy chain 前写入，配合 §4.5 `[artifact]` 配置）
- [x] 实现 HTML payload 重放入口（CLI `replay --kind html` 已实现：`crates/cli/src/commands/replay.rs::ReplayKind::Html` 走 `ReadabilityStrategy.extract`，支持 `--diff` 与 articles 表对比；W9c 注释 file-backed artifact 仍待支持）

## 9. Workstream W7：AI 闭环

### T701 初始化 ai crate

- [ ] 实现 `AiClient`
- [ ] 实现 prompt 组装
- [ ] 实现输入裁剪
- [ ] 实现 JSON 优先输出解析
- [ ] 实现 JSON schema 漂移处理（向后兼容字段解析测试，不接受文本协议 fallback）

### T702 实现 ai-run use-case

- [ ] 为 `articles.state='persisted'` 生成 `article_ai_results` 任务行（同事务推进 `articles.state='ai_pending'`）
- [ ] claim `article_ai_results.state='pending'` 批次
- [ ] 执行 AI 调用
- [ ] 更新 `article_ai_results`
- [ ] 推进 `articles.state` 至 `ai_done` / `ready_for_publish` / `publish_skipped`

### T703 失败与版本化

- [ ] 记录 `prompt_version`
- [ ] 记录 `output_schema_version`
- [ ] 区分可重试与永久失败

### T704 replay(ai) 与 backfill(ai)

- [x] 保存 AI raw response artifact（已实现：`runtime::flows::ai_run::write_ai_raw_response_artifact`，parse 前独立事务 commit）
- [x] 实现 AI response replay（CLI `replay --kind ai` 已实现：`crates/cli/src/commands/replay.rs::ReplayKind::Ai` 走 `parse_response` 还原 `keep_decision` / `summary` / `importance_score` / `tags`；W9c 注释 file-backed artifact 仍待支持）
- [x] 实现历史 article 的 AI backfill（已实现：`runtime::flows::backfill::BackfillFlow::ai` + CLI `backfill --target=ai`，通过 BackfillAiOptions 生成新 prompt_version 后扫描 articles 重新插入 pending）

## 10. Workstream W8：发布闭环

### T801 初始化 report crate

- [ ] 选稿逻辑
- [ ] excerpt 逻辑
- [ ] Markdown renderer
- [ ] frontmatter builder

### T802 实现发布快照

- [ ] 设计 `PublishSnapshot`
- [ ] 冻结 `publish_records`
- [ ] 冻结 `publish_items`

### T803 初始化 publish crate

- [ ] 实现 local fs target
- [ ] 实现 GitHub target
- [ ] 实现本地 + GitHub 双目标协调

### T804 实现 publish use-case

- [ ] 领取待发布任务
- [ ] 渲染 Markdown
- [ ] 本地落盘
- [ ] GitHub 提交
- [ ] 更新发布状态

### T805 rebuild-report

- [x] 根据 `publish_record_id` 重建 Markdown（已实现：`runtime::flows::rebuild_report::RebuildReportFlow` + CLI `rebuild-report`；`report::rebuild::rebuild_markdown` 严格读 `publish_items.frozen_*` 列重新走 `FrozenPublishItem::try_new` + `render_markdown`）
- [x] 对比原始快照输出（已实现：`crates/runtime/tests/rebuild_report_tests.rs` 含两个 byte-equal 锁定测试——`rebuild_returns_byte_equal_markdown_to_original_render` 覆盖显式 `generated_at_override` 路径；`rebuild_without_generated_at_override_falls_back_to_record_rendered_at_and_matches_original` (F13-4) 覆盖默认 fallback 行为）

## 11. Workstream W9：运维与可靠性

### T901 observability crate

- [x] tracing 初始化（F15-13 tracing-appender::rolling::daily + F15-16 try_init 显式 warn；详见 [crates/observability/src/tracing_init.rs](../../crates/observability/src/tracing_init.rs)）
- [x] metrics 注册（F15-14 prometheus exporter + `/metrics` HTTP 端点；详见 [crates/observability/src/prometheus.rs](../../crates/observability/src/prometheus.rs)。业务侧 counter/histogram 调用点接入是另一桩 instrumentation 工作，独立追踪）
- [x] health probe（W7-T705 实装；详见 [crates/observability/src/health.rs](../../crates/observability/src/health.rs)）
- [x] 关键事件结构化日志（W3-T303 起，`SqliteRunEventRepo` + `RunEventEmitter` 在 ingest / extract / ai / publish / reindex 五个 stage 全部接入，事件持久化进 `run_events` 表）

### T902 doctor 命令

- [x] 检查配置（[crates/cli/src/commands/doctor.rs](../../crates/cli/src/commands/doctor.rs) ConfigCheck）
- [x] 检查数据库（DatabaseCheck）
- [x] 检查 AI endpoint（AiEndpointCheck）
- [x] 检查 GitHub token / repo（GithubCheck）
- [x] 检查 RSSHub base URL（RsshubCheck）
- [x] 检查时区（TimezoneCheck）

### T903 reindex

参见 [cli-semantics §4.8](../design/cli-semantics.md#48-reindex)、[state-machine §6](../design/state-machine.md#6-reindex_job-独立状态轮)、[storage-schema §4.10](../design/storage-schema.md#410-reindex_jobs)。

#### CLI 与 runtime 入口

- [x] `crates/cli` 注册 `reindex` 子命令（clap derive）：`--target` / `--batch-size` / `--dry-run` / `--abort`（F15-10）
- [x] `runtime::reindex` use-case：按 `--target` 分派；`target='all'` 时顺序生成三个独立 job（F15-10 ReindexFlow + CLI ReindexTarget::All expand）

#### active rule resolver

- [x] `storage` 实现 `active_rule(kind) -> RuleVersion` resolver；所有读取规则的命令（ingest / extract / ai-run / publish）改用此 resolver 取规则，禁止直接 `SELECT FROM rule_versions WHERE id = ?`（F15-2 active_rule resolver + F15-3 把 6 个生产读路径切到 active_rule_or_register）
- [x] `active_rule` resolver 单元测试：partial unique index 保证返回 0 或 1 行；migration 后所有 kind 各有 1 行 active（F15-2 + F15-4 partial unique 锁定测试）

#### 三类 target 实现

- [x] `storage`: `link_hash` 重算（F15-7..F15-8 reindex flow link_hash 分支）
- [x] `storage`: `content_hash` 重算（F15-8 reindex flow content_hash 分支 + F15-10 peek_content_hash_outcome）
- [x] `storage`: `categories` 重算（F15-8 reindex flow categories 分支）

#### 两阶段激活

- [x] `runtime`: 启动事务 INSERT 新 `rule_versions` (`status='pending'`) + INSERT `reindex_jobs` (`state='pending'`)（F15-7 start_reindex_tx）
- [x] `runtime`: claim/lease 推进 `pending → running`，按 batch-size 分批 commit，每批更新 `last_processed_id` + 数据行 `*_rule_version_id` 指向 pending 行（F15-8 claim_by_id + advance_checkpoint）
- [x] `runtime`: 终止事务 `pending → active`、旧 active → `superseded` + `retired_at`、`reindex_jobs` → `completed`，对外原子可见（F15-9 finish_reindex_tx）

#### checkpoint 与失败恢复

- [x] `runtime`: `last_processed_id` checkpoint 持久化（每批 commit 一并写入）（F15-8 advance_checkpoint + F15-12 batch boundary test）
- [x] `runtime`: lease 过期 reclaim 时保留 checkpoint，下次 claim 从 `last_processed_id` 继续（F15-5 reclaim_expired_leases + F15-12 `reindex_lease_reclaim_preserves_checkpoint_and_started_at_for_resume`）
- [x] `runtime`: crash-after-batch 恢复（已 commit 批次保留，未 commit 批次丢失，重启从 checkpoint 重做）（F15-12 同上测试锁定 reclaim → resume 路径）
- [ ] `runtime`: 批次内部重试上限（`[retry] reindex_max_attempts`，待 W3/T301 加入 config-schema）；超限 → `failed`

#### 并发与 abort

- [x] `runtime`: 同 target 启动 reindex 时 partial unique index 冲突 → 返回 exit 1 + 友好错误（"target X 已有 pending/running job"）（F15-7 partial unique + F15-12 `reindex_second_start_for_same_target_rejected_by_partial_unique`）
- [x] `runtime`: `--abort <job_id>` 实现：仅允许 `running` / `pending` → `aborted`；写 `aborted_reason`（F15-10 ReindexFlow::abort + storage abort）
- [x] `runtime`: migrate 启动前检查无 `running` reindex_job，否则拒绝 migrate（F15-11 assert_no_running_reindex）

#### 进度输出

- [ ] CLI 默认输出每批进度（target / batch_index / processed / last_id / 速率）（F15-10 决策：summary-only 模型；流式 stderr 留待后续 UX commit）
- [x] 终态行输出激活信息（rule_versions pending → active / 旧 active → superseded）（F15-10 pretty 模板 "Job id" 行 + finalize tracing::info）
- [x] `--dry-run` 仅输出启动信息 + "Would update N rows"（F15-10 dry_run pretty 模板）

#### 测试

- [x] 幂等：同 target 重跑生成新 `rule_versions` 行，`payload_sha256` 一致 → 完成后激活无差异（F15-12 `reindex_link_hash_second_run_with_all_unchanged_still_rotates_rule_versions`）
- [x] 批处理：`--batch-size` 边界（1 / max(id)+1 / 单批跨完整表）（F15-12 `reindex_link_hash_batch_size_one_processes_all_rows_and_checkpoints_last_id`）
- [x] dry-run：不写入任何表，输出预估行数（F15-10 三例 dry_run_* + F15-12 `reindex_dry_run_then_real_run_promotes_without_polluting_rule_versions_chain`）
- [x] crash-after-batch 恢复（F15-12 lease reclaim resume 测试覆盖该路径）
- [x] 隔离：reindex `running` 期间 active_rule 仍返回旧 active 行（F15-12 `reindex_link_hash_does_not_modify_articles_or_content_hashes` + F15-6 active_rule resolver 在 pending 行存在时跳过的单测）
- [x] 并发拒绝：同 target 第二个 `reindex --target X` 启动失败（F15-12 `reindex_second_start_for_same_target_rejected_by_partial_unique`）
- [ ] target='all' 部分失败：第一个 target completed、第二个 target failed 时，第三个 target 不启动；前一个的 active 状态保持（CLI 入口已支持 target='all' 顺序执行；中途失败 short-circuit 行为仍需补端到端 CLI 集成测试）
- [x] active rule 不被 pending 污染：`active_rule(kind)` 在 reindex 全程返回旧 active 行（F15-12 `reindex_mark_failed_keeps_old_active_and_pending_new_rule_version` + F15-3 path 不变量）

#### migrate 边界（文档）

- [x] 在 [cli-semantics §4.9 migrate](../design/cli-semantics.md#49-migrate) 增补：migrate 启动前 doctor preflight 检查无 `running` reindex_job；规则升级走 reindex，schema 升级走 migrate（F15-11 实装阻塞门 + doc §4.8 line 312 cross-link）

## 12. Workstream W10：交付

### T1001 Docker 交付

- [ ] 新增 multi-stage `docker/Dockerfile`
- [ ] builder/runtime 分离
- [ ] runtime 保留 CA 证书与 tzdata
- [ ] 默认非 root 用户

### T1002 镜像策略

- [ ] 生产镜像
- [ ] 调试镜像

### T1003 CI

- [ ] `cargo fmt --check`
- [ ] `cargo clippy`
- [ ] `cargo test`
- [ ] migration smoke test
- [ ] docker build smoke test
- [x] 禁止吞错误的 ripgrep 扫描步骤：模式 A `if\s+let\s+Ok\([^)]*\)\s*=`、模式 B `\.ok\(\)\s*;\s*$`；任一非空匹配 fail（F15-15 `.ci/check_swallowed_errors.sh`，依据 [error-and-observability §3.3 Enforcement 第 2 层](../design/error-and-observability.md#33-绝不静默吞掉错误)）
- [x] 维护 `.ci/swallowed-error-allowlist.txt`（F15-15 落地时盘点 4 条 pre-existing 豁免，每条记录 reason；新增条目须在 PR 中说明并经设计 owner 批准）

## 13. 阶段验收点

### A1 最小 ingest 闭环

验收标准：

- 能加载配置
- 能抓 RSS / Atom / JSON
- 能去重
- 能入 SQLite

### A2 正文提取闭环

验收标准：

- 能抓详情页
- 能提正文
- 能 fallback
- 能 replay HTML

### A3 AI 闭环

验收标准：

- 能跑分类 prompt
- 能写 AI 结果
- 能 replay AI
- 能 backfill

### A4 发布闭环

验收标准：

- 能冻结快照
- 能渲染 Markdown
- 能本地落盘
- 能推送 GitHub
- 能 rebuild-report

## 14. 任务执行原则

- 任何任务若触及骨架级变更，必须先暂停并提交变更分析
- 任何任务若需要跨层写入，必须先回到工程宪法核对边界
- 任何任务若不能说明真相源、状态变化、失败路径，则不得进入正式实现
