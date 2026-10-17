# R3：门禁加固、留档策略与日期窗口修正、证据驱动清理

- 日期：2026-10-17
- 作者 / Agent：Claude Code（Opus）
- 分支：`maint/r3`（worktree，基于 `2cb5b8f`）
- 相关 commit：`ba443dc`、`665c468`、`1df8511`、`06d5403`、`9f5c28a`、`fb6e48a`、`29401d2`；审查修复
  `a060a9c`、`bb80108`、`af3efd8`、`204ff6c`、`4473b2b`、`811c00e`、`8c627a4`、`c2af1fe`
- 相关 tag / release：N/A
- 状态：`validated`（本地 worktree，未 push）

## 工作摘要

分阶段维护第 3 轮。用户决定：D-007 补齐 feed / HTML 的 `on_failure` 语义、AI 原始响应保持无条件留档；
D-008 改代码使 `--date-to` 含当天；rustdoc `-D warnings` 进 static lane。另做 v0.9.1 审查遗留的两项 CI
修正、`docs/design/*` 旧引用与有证据的死代码清理。

## 影响范围

- crate / 模块：runtime（artifact、ingest、extract）、cli（backfill、args 的 lint 豁免）、config（error、
  warnings 文案）、storage（lease、repo 文档）、report / observability（依赖、文档链接）、tools/acceptance（static lane）
- CI：`.ci/check_swallowed_errors.sh`、`.github/workflows/{ci,release,maintenance}.yml`
- 真相源对象：RawArtifact（写入时机），无 schema / 迁移变更
- 行为变化：`on_failure`（默认）下解析 / 提取失败的 feed 与 HTML 原文开始留档；`backfill --date-to` 当天
  纳入窗口；static lane 多一步 rustdoc

## 关键变更

### CI 门禁（`ba443dc`）

- swallowed-error 扫描：`rg` 退出码 ≥ 2 时门禁以 2 失败（此前 `2>/dev/null || true` 当作无命中放行）。
- Docker `cache-to` 仅 `main` 写；release 的 tag run 不再导出缓存，仍读 `main` 的 `scope=runtime`；
  maintenance perf job 的 rust-cache `save-if: main`。

### rustdoc（`665c468`）

- 修复 32 处 lint（21 公开文档链接私有项、8 无法解析、1 冗余目标、2 HTML 样占位符），纯注释改动；
  `cli/src/args.rs` 的 `///` 是 clap help，文字不动，模块级豁免两个 rustdoc lint。
- static lane 新增 `rustdoc` step（`RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked`）。

### D-008（`1df8511`、`06d5403`）

- `parse_date_to`：上界 = `--date-to` 次日 00:00 UTC；SQL 仍为 `created_at < $to`；extract / ai 两个 target。

### D-007（`9f5c28a`）

- `ArtifactWriter::waits_for_outcome`：`on_failure` 在结果出来后决定（feed 解析失败；正文无策略成功——
  retryable、fallback、永久失败）；`always` / `sampled` 仍在解析前决定并写入（parser panic 时原文仍在，
  sampled 只抽样一次）。AI 原始响应不受策略控制为有意例外。

### 旧引用（`fb6e48a`）与清理（`29401d2`）

- 36 处代码注释、门禁消息与 `app.toml.example` 的 `docs/design/*` → `docs-backup/design/*`（同行替换，
  allowlist 行号键不变）；v0.7.1 fixture、migrations、ADR / 漂移日志历史保留。
- 删除：6 个零引用直接依赖（config async-trait / tracing，storage tracing，report async-trait / serde /
  tracing）、`ConfigError::VersionMismatch` / `MissingGithubCredentials`、`storage::ReleaseOutcome`。

## 验证与验收

### 自动化验证

- 先红后绿：`on_failure_retention_keeps_feed_payload_only_when_parsing_fails`、
  `on_failure_retention_keeps_html_only_when_extraction_fails`（修复前分别 0 ≠ 1）；门禁故障注入（修复前
  exit 0，修复后 exit 2）。
- 最终验证（HEAD `c2af1fe`，运行期间未改文件）：
  - `cargo fmt --all -- --check`：exit 0
  - `cargo test --locked --workspace --no-fail-fast`：911 passed / 0 failed / 68 ignored
  - `cargo acceptance --format json run --profile local --expected-version 0.9.1`：34/34 steps（含新增
    rustdoc），514.9 s，report SHA-256 `2bad0423e5d85016ba6b15f4c41f0591b62529eeafb8fbfb88e132387f25e8cf`
  - `DATABASE_URL=… cargo acceptance run --lane postgres`（一次性 `postgres:16-alpine` 容器）：8/8 steps，249.5 s
  - release 二进制 `--help`（顶层 + 13 个子命令，14 份）与 R3 前构建逐字节相同
- 未执行：docker lane（无 Dockerfile 改动）、真实 AI provider；CI 对 workflow 改动的实跑待 push 后观察
  （cache-to 仅 main 写入、release 不导出缓存）。
- 本机环境：中途一次被并发改动污染的 local profile 中，`ai/tests/client_tests.rs::invoke_limits_chunked_response_without_content_length`
  报 `HttpTimeout { seconds: 2 }`（经本机代理流式 4 MiB、高负载）；与 R3 改动无关，干净重跑通过。

### 独立跨模型审查（Codex，effort max，范围 `2cb5b8f..29401d2`）

- BLOCKER（已修，`a060a9c`）：SQLite 的 backfill 窗口按文本比较——生产行 `created_at` 为
  `CURRENT_TIMESTAMP`（`YYYY-MM-DD HH:MM:SS`），绑定值为 RFC3339，`' ' < 'T'` 使 `--date-from` 当天被排除、
  `--date-to` 次日被纳入；`date_from` 一侧自 backfill 上线即存在。SQLite 的计数、重置、文章窗口查询改为
  `replace(substr(created_at, 1, 19), 'T', ' ')` 与 Rust 侧格式化的 UTC 整秒边界比较，PG 不变。第一版用
  `datetime()` 的修复经复核（Codex，effort high）证伪：本机实测 SQLite 3.46.0
  `datetime('2026-04-30T23:59:59.999999+00:00')` = `2026-05-01 23:59:59`，已在 `4473b2b` 替换；测试覆盖
  生产格式与亚秒 RFC3339 行在两个午夜两侧的取舍（两版旧实现下都失败）。EXPLAIN QUERY PLAN 与原 SQL 相同。
- SHOULD_FIX（已修）：rustdoc 门禁可被继承的 `CARGO_ENCODED_RUSTDOCFLAGS` 覆盖（`af3efd8`，坏链接探针：
  旧写法 exit 0，新写法 exit 101）；日期解析接受 `2026-5-1` / `+05`（`bb80108`）。
- NEEDS_VERIFICATION（已补测，`204ff6c`）：fallback 路径的 artifact 关联、retryable 留档、`always` 在策略
  panic 时仍留档。
- SHOULD_FIX（未改，既有）：内容去重命中已有文章时（`ON CONFLICT DO NOTHING`），新写入的 html artifact
  不补到已有文章的 `body_html_artifact_id`；属去重存储语义，按 feed-entry key 仍可 replay。

## 暂缓的清理候选（未改，需另行决定）

- domain：`model::{ArticleAiResult, FeedEntry, PublishRecord, ReindexJob, RuleVersion, RunEvent}`、
  `state::{FeedEntryState, AiResultState, PublishState, DedupDecision, RuleVersionStatus, ReindexJobState, BackfillTarget}`、
  `dto::{PublishOutcome, ReplayRequest, ReplayArtifactSelector, ReplayResult, BackfillRequest, DedupResult}`
  在生产代码中无引用；属于对象层（宪法 §3.1–3.2），删除前需设计决定。
- `ClassifiedError::display_debug`：8 个实现、零调用，但 plan/11 承诺了它。
- storage 仅测试使用的 trait 方法（`insert_if_new`、`exists_by_link_hash`、`list_by_category`、`mark_archived`、
  `insert_pending_rule`、reindex 若干）：双方言实现，删除需改写测试。
- `PublishRequest.render_version_id` / `selection_policy_version_id` 只写不读（预留的选择策略钩子）。
- `report::build_frontmatter`（生产未用，但其测试锁定 YAML 引号规则）；两个未用 `with_client` 测试注入点。
- CI 的 scheduler 镜像写 `scope=scheduler` 缓存，但 CI 与 release 都只读 `scope=runtime`，该缓存从未被读取。

## 给下一位 Agent 的备注

- 入口：`crates/runtime/src/artifact.rs::waits_for_outcome`、`flows/ingest.rs`、`flows/extract.rs`、
  `crates/cli/src/commands/backfill.rs::parse_date_to`、`tools/acceptance/src/runner/lanes/static_checks.rs`。
- 契约：plan/01 §7、plan/02 §5–§7、plan/10 §3 / §5、architecture-diff D-007 / D-008。
