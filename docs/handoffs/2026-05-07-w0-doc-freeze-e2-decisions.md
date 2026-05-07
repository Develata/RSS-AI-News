# W0 文档冻结收口：E2 决策轮（6 个 issue）

- 日期：2026-05-07
- 作者 / Agent：Claude Code (claude-opus-4-7)
- 分支：main
- 当前 HEAD：`8e63c44`
- 相关 commit：
  - `c40627f` docs(W0): E2 Issue 4 — define runtime.max_batches_per_run
  - `ab03cd9` docs(W0): E2 Issue 9 — three-layer swallowed-error enforcement
  - `652243e` docs(W0): E2 Issue 15 — reindex 一致性模型 + 实现任务拆解
  - `5b604a0` docs(W0): E2 Issue 20 — articles 不承载阶段错误（修正文档泛化）
  - `3e3a979` docs(W0): E2 Issue B-3 — on_failure artifact 时序悖论修订
  - `8e63c44` docs(W0): E2 Issue B-5 — publish 阈值全局默认 + override 按字段覆盖
- 上游批次（参考）：`cae743b` docs(W0): apply DeepSeek+codex 找茬 E1/E3 batch revisions（2026-05-05）
- 相关 tag / release：N/A（v0.1.0 待打）
- 状态：`validated`

## 工作摘要

W0 文档冻结的最后一轮：把 DeepSeek "找茬" 报告中归到 E2 桶（**需独立决策再修**）的 6 个 issue 逐个分析、codex 二审、独立 commit 落地。E1（事实错 + schema/命名一致性）+ E3（措辞优化）共 26 条已在 `cae743b` 批量落地；本轮收口意味着 W0 的全部"找茬"反馈消化完毕，文档真相源进入冻结态，后续可放心进入 W1 仓库初始化阶段（实际 W1–W10 已先行实现至 v0.1.0 候选，现在文档与代码进入一致冻结）。

> **术语澄清**：本 handoff 里的 E1 / E2 / E3 是 commit message 沿用的"决策轮次"分桶，**不是严重度等级**。W0 找茬实际使用的等级是 codex 二审给出的 severity_revised 字段：`critical / high / medium / low / non_issue`（详见 `.codex-tmp/w0_codex_verdicts.md`）。

## 影响范围

- crate / 模块：N/A（纯文档修订）
- 真相源对象：通过文档间接覆盖
  - `FeedSource` / `FeedEntry` / `ArticleAiResult` / `PublishRecord`：错误归属（Issue 20）
  - `RawArtifact`：捕获门控 / 保留策略 / on_failure 时序（Issue B-3）
  - `Article`：负面声明（不承载阶段错误，无 `last_error*` 列）（Issue 20）
- 额外影响：
  - `docs/design/config-schema.md` — Issue 4（[runtime]）+ Issue B-5（[publish] effective）
  - `docs/design/cli-semantics.md` — Issue 4（`--max-batches`）
  - `docs/design/internal-dto-contracts.md` — Issue B-5（PublishRequest 来源注解）
  - `docs/design/error-and-observability.md` — Issue 9（§3.3 三层 enforcement）+ Issue 20（§3.1 错误归属枚举）
  - `docs/design/state-machine.md` — Issue 20（§4.1.2 派生原则补 articles 负面声明）
  - `docs/design/storage-schema.md` — Issue 20（§4.3 articles 设计注）+ Issue B-3（§4.13 retention/expires 字段交叉引用）
  - `docs/design/replay-and-artifacts.md` — Issue B-3（§3.1/§3.2 拆为 3.2.1/3.2.2/3.2.3；§6 写入时序与失败粒度）
  - `docs/design/python-legacy-lessons.md` — Issue 20（§120 错误归属泛化修正）
  - `docs/plan/full-rust-rss-ai-news-blueprint.md` — Issue 4（宪法不变量 reword）+ Issue B-3（不变量 7 reword）
  - `docs/task/full-rust-rss-ai-news-blueprint-tasks.md` — Issue 9 / Issue 15（实现任务拆解）

## 关键变更

### Issue 4 — `runtime.max_batches_per_run` 落位（commit `c40627f`）

DeepSeek 指出蓝图多次提及"单次 run 工作量上限由配置控制"，但 `[runtime]` 段在 config-schema.md 中缺失。

- `config-schema.md` §4 新增 `[runtime] max_batches_per_run = 10`（`0` = 不限）；§4.4 加完整字段语义、CLI 覆盖、触达上限的退出语义（exit 0 + INFO 日志）、与 `[lease]` / 宿主超时的三层兜底关系，以及与 `publish` 命令的边界声明。
- `cli-semantics.md` 新增 `--max-batches <n>` flag（仅 `ingest` / `ai-run` / `run`）。
- `internal-dto-contracts.md` 类型映射补 `RuntimeConfig`。
- `plan/full-rust-rss-ai-news-blueprint.md` 宪法不变量 6 reword："宿主负责调度，进程负责单次执行"，把工作量上限的真相源指向 `runtime.max_batches_per_run`。

### Issue 9 — 三层吞错误 enforcement（commit `ab03cd9`）

DeepSeek 指出"绝不静默吞错误"原则只有口头声明、缺乏 enforcement。

- `error-and-observability.md` §3.3 重写为三层：
  1. **Lint 层（W1 root Cargo.toml `[workspace.lints]`）**：`unused_must_use=deny` / `clippy::let_underscore_must_use=deny` / `clippy::let_underscore_future=deny` 等
  2. **运行时检查（runtime 内 swallow_test）**：每条流程的 fallible 路径在退出前必须命中 emit / propagate / persist 三选一，否则 panic
  3. **测试层 panic-on-result-drop**：测试 helper 强制要求 `Result` 不允许 `let _`
- `task/full-rust-rss-ai-news-blueprint-tasks.md` T101 / T201 注入对应 lint 规则；T501（runtime 框架）注入 swallow_test 实现职责。

### Issue 15 — reindex 一致性模型 + 实现任务拆解（commit `652243e`）

DeepSeek 指出 `reindex` / `backfill --target ai` 在跨真相源（`articles` / `article_ai_results` / `feed_entries`）情景下没有定义一致性边界。

- `replay-and-artifacts.md` §6.5 新增"reindex 一致性模型"：明确 reindex 不修改 `articles.id` 与 `article_ai_results.id`，只重建衍生索引（FTS5 / vector）。原子性边界 = 单个 article + 其衍生行；跨 article 不要求事务原子。
- `state-machine.md` §4.1.3 / §5 涉及的状态保持稳定；任务表把 reindex 实现拆为 storage（FTS5 重建）+ runtime（按批扫描 + 进度回写）两个任务。

### Issue 20 — `articles` 不承载阶段错误（commit `5b604a0`）

DeepSeek 把 `python-legacy-lessons.md` 中的"`articles.last_error` 写入路径"当作 schema 错误。实际并非 schema 错（`articles` 表本就没有 `last_error*` 列），而是文档泛化措辞误导。

- `python-legacy-lessons.md`：`articles.last_error` → 按真相源行枚举写入：`feed_sources` / `feed_entries` / `article_ai_results` / `publish_records`，并显式声明"`articles` 表本身不承载阶段错误"
- `error-and-observability.md` §3.1：将"对应行"展开为枚举来源 + articles 负面声明
- `storage-schema.md` §4.3：articles 索引/约束块下补"**设计注（无 `last_error*` 列）**"段
- `state-machine.md` §4.1.2：派生原则第三条补 articles 不承载阶段错误的负面声明，错误传播规则交叉引用 error-and-observability §3.1

### Issue B-3 — `on_failure` artifact 时序悖论（commit `3e3a979`）

DeepSeek 指出 `retention_policy = "on_failure"` 与"解析前必须捕获 artifact"存在语义冲突——失败结果未知时如何决定是否捕获。

- `replay-and-artifacts.md`：
  - §3 加术语段：`retention_policy` **同时承担"捕获门控"与"保留策略"两个职责**
  - §3.1 表头重做为「捕获门控 / 保留策略 / 适用场景」三列
  - §3.2 拆为 §3.2.1 捕获门控 / §3.2.2 保留与清理 / §3.2.3 on_failure 失败粒度（feed_payload→单 feed；html_payload→单 entry；ai_raw_response→单 article_ai_result）
  - §3.3 TTL 计算扩写 on_failure 同步 DELETE 路径
  - §6.1 / §6.2 / §6.3 每个写入点："条件：retention_policy 允许" → "捕获门控通过（见 §3.2.1）+ 独立短事务 commit"
  - §6.4 重写为"写入时序与持久化边界"：写入时序、事务隔离、on_failure 同步清理（写后清）、清理补偿（expires_at 扫描 fallback）
- `storage-schema.md` 行 344-345 retention_policy + expires_at 字段交叉引用至 replay-and-artifacts
- `config-schema.md` `[artifact]` 默认 `on_failure` 注释扩展为"解析前总是捕获并独立事务 commit，关联操作成功后同步清理"
- `plan/full-rust-rss-ai-news-blueprint.md` 不变量 7 reword

### Issue B-5 — publish 阈值全局默认 + override 按字段覆盖（commit `8e63c44`）

DeepSeek 指出 `PublishRequest.max_items` / `min_importance_score` 是必填非 Option，但 `[publish]` 全局段没有这两个字段，仅 `[category.publish_override]` 中出现。当某分类未声明 `publish_override` 时，runtime 找不到全局默认。

- `config-schema.md` §4 `[publish]` 补 `max_items_per_report = 30` / `min_importance_score = 30`（保留默认值不变更产品策略）
- §4.5 新增 effective publish 配置合并规则：**按字段覆盖**（不是整表覆盖）；零值与 `false` 是显式覆盖、缺省由 TOML 字段缺席表达；`max_items_per_report` 用 `NonZeroU32` 表达；AI 关闭时 `min_importance_score` 不参与过滤、`max_items_per_report` 仍生效
- §5 `[category.publish_override]` 注释明确"按字段覆盖、省略即继承"
- §8 类型映射展开 `PublishConfig` / `PublishOverride`（内部字段全部 `Option<...>` 以区别"未设置"与"显式 0/false"）
- `internal-dto-contracts.md` §5.1 PublishRequest 注明 `max_items` / `min_importance_score` / `include_unscored` 是 `runtime::publish::freeze` 在构造前消解 effective 后的最终值

## 验证与验收

### 自动化验证

- 文档修订无代码变更，跳过 cargo / clippy / test。
- 每个 commit 在执行前由 codex（或在 codex Windows 沙箱失效时由本人）做反向 grep 确认目标位置 + 无未列出的串扰。
- 每个 commit 提交前 `git diff --stat` 检查影响文件清单与预期一致。

### codex 二审

每个 issue 在执行前以独立 prompt 提交 codex 审查，prompt 与（部分）输出落在 `.codex-tmp/`：
- `e2_issue15_review_prompt.md` / `e2_issue20_review_prompt.md` / `e2_issueB3_review_prompt.md` / `e2_issueB5_review_prompt.md`
- Issue 4 / Issue 9 在 E2 启动前经 DeepSeek + codex 联合审，prompt 留存在 `.codex-tmp/w0_codex_*` 系列与 `w0_deepseek_issues.md`

codex 普遍给出 `yes_with_revisions` 并贡献了若干关键修订（典型如 Issue B-5 把 `PublishOverride` 字段全部 `Option<...>`、Issue B-3 把"清理补偿"路径写明 expires_at 扫描兜底）。Windows 沙箱出现 `CreateProcessAsUserW failed: 1920` 时，由本人改用 Grep / Read 工具直接验证仓内事实。

### 手工验收

- 每个 commit `git show --stat` 与本 handoff 列出的影响文件一致：通过
- `MEMORY.md` / 设计文档结构完好（无悬空交叉引用）：通过（Grep 反查）
- `docs/handoffs/README.md` 命名与格式：通过（本文件遵循 `YYYY-MM-DD-<slug>.md` + TEMPLATE.md 结构）

## 结果

- W0 文档冻结的找茬-决策-修订环节全部消化完毕：33 条 issue（Part A 27 + Part B 6）全数有去向。
    - **`cae743b` 批量（26 条）**：Issues 1, 2, 3, 5, 6, 8, 10, 11, 12, 13, 14, 16, 17, 18, 19, 21, 22, 23, 24, 25, 27, 28 + B-1, B-2, B-4, B-6
    - **本轮 E2 6 commits**：Issues 4, 9, 15, 20, B-3, B-5
    - **codex `refuted` 为 `non_issue`，不需修（1 条）**：Issue 26（蓝图 §3.2 非功能需求"退出路径"——DeepSeek 漏读了同一蓝图后续专门的"生命周期与退出路径"章节）
- 详细判决见 `.codex-tmp/w0_codex_verdicts.md`，原始 issue 列表见 `.codex-tmp/w0_deepseek_issues.md`。
- 文档真相源与现有 W1–W10 实现保持一致：本轮修订均为措辞校准 / 字段补全 / 语义澄清，**未产生需要回追代码的 schema 或 DTO 字段变更**（注：B-5 提到 `PublishOverride` 字段应为 `Option<...>` —— 现有代码已是该形态，无需迁移；详见 `crates/domain/src/dto/publish.rs` 与 `crates/config/src/category.rs`）。
- 与 `docs/handoffs/2026-05-04-...-migrate-decoupling.md` 之间无依赖；本 handoff 之后可直接进入下一阶段（v0.1.0 tag / 第二批功能 / 后续 workstream）。

## 风险与后续事项

- **`PublishOverride` 内部字段为 Option** 的语义在 §4.5 已明确，但 [config §6.1 校验规则](../design/config-schema.md#6-校验规则) 中没有对应的"零值不视为缺省"的 unit test 覆盖；W1 在补 `[workspace.lints]` 时如顺手补 config crate test，应把这个 invariant 写进去。
- **Issue 15 reindex 一致性**：现在的实现（`crates/cli/.../backfill.rs`）已经存在但未严格按新文档定义的"按 article 原子"语义校验；后续如要为 reindex 加性能指标或观测点，应回看 §6.5 的边界。

## 给下一位 Agent 的备注

- 进入下一项工作前先读 `.codex-tmp/w0_deepseek_issues.md` 与 `w0_codex_verdicts.md` —— 这两份是本次找茬的真实 issue 池，其中 D 等级的判决理由对维护设计文档一致性仍有参考价值。
- 想还原 E2 任一 issue 的决策上下文：`git show <sha>` 看 commit message + 对应的 `.codex-tmp/e2_issueXX_review_prompt.md` 看 codex 审查请求。
- 涉及 publish / artifact 的代码修订，优先以 `docs/design/config-schema.md` §4.5、`docs/design/replay-and-artifacts.md` §3.1–§3.2 为契约真相源，而非 `python-legacy-lessons.md`（后者只是历史教训记录）。
- W0 此后不再开新 commit；如需进一步澄清某条规则，请改为开 W1+ 的相应任务，避免 W0 commit 历史无限扩张。
