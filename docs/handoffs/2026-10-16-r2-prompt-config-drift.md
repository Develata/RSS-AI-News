# R2：AI prompt 来源收敛、prompt warning 与 AI 文档漂移

- 日期：2026-10-16
- 作者 / Agent：Claude Code（Opus）
- 分支：`maint/r2-config-prompt`（worktree，基于 `9f29a12`）
- 当前 HEAD：`9f29a12`（改动未提交）
- 相关 commit：pending
- 相关 tag / release：N/A（计划随 v0.9.1 发布，需另行授权）
- 状态：`validated`（本地 worktree，未提交）

## 工作摘要

分阶段维护第 2 轮（R2），只做用户选定的三项：(A) prompt 模板缺占位符时正文不发送给模型且无任何提示；
(B) plan/03、10、13 与 acceptance case 的 AI / backfill 描述与代码不符；(C) `dedup.link_normalizer_version`
不生效却无 warning。

## 影响范围

- crate / 模块：
  - `crates/config`（`effective.rs`、`validate/warnings.rs`、`validate.rs`）
  - `crates/cli`（`commands/ai_run.rs`、`commands/backfill.rs`、`commands/validate_config.rs`）
  - `configs/categories/ai.toml.example`
- 真相源对象：N/A（无 schema / 迁移 / DTO / 状态机变更）
- 额外影响：
  - config：`EffectiveConfig.prompt_template` 省略或空串时改为含占位符的内置默认模板
  - CLI：`validate-config` 的 `summary.warnings[]` 可能新增 `code=prompt_template` 项，带新字段 `category`（app 级 warning 不输出该字段）；ai-run 每次运行对同样条件打 warn 日志
  - docs：plan/03、06、10、13，acceptance-cases backfill / validate-config，map/architecture-diff D-006

## 关键变更

### A. prompt 来源与 warning

- 复现（修复前代码实测）：分类未写 `prompt_template` 时，mock 端点收到的 user 消息只有
  `Summarize the following article.`，标题与正文都没有发送。示例 `ai.toml.example` 的模板同样无占位符。
- `DEFAULT_PROMPT_TEMPLATE`（`crates/config/src/effective.rs`）含 `{category_key}` / `{title}` / `{body_text}`
  与 v1 输出字段说明；`EffectiveConfig.prompt_template` 省略 / trim 后为空 → 默认模板（与 `model`、
  `path_template` 继承语义一致）。
- ai-run 与 `backfill --target ai` 的模板、ai-run 的 `max_input_chars` 统一从 `EffectiveConfig` 取，删除
  CLI 内两处各自的默认值（原先 `"Summarize the following article."` 与 effective 的 `""` 不一致）。
- `prompt_template_warnings`：AI 启用时，缺 `{body_text}`、缺 `{title}`、含未支持的 `{标识符}` 各一条
  warning；message 固定，不回显模板。`config_warnings` = inert + 各分类 prompt warning，由 validate-config 输出。
- 示例 `ai.toml.example` 改为含占位符与字段说明的模板。v0.7.1 fixture 不动（其旧模板现在会触发
  prompt warning，属预期；该测试只断言 app 级 inert warning 为空）。

### B. 文档漂移（改文档承认现实，parser 不变）

- plan/03 §3–§4、§5.3、§7、§8；plan/10 §1、§4.1、§5、§5.2（含 backfill 已知限制）；plan/13 §5.3；
  plan/06 §4、§5 与 warning 表；acceptance-cases backfill、validate-config；architecture-diff D-006。

### C. `dedup.link_normalizer_version`

- 值 ≠ `"1"` 时输出 `inert_config` warning（第 16 个 inert 字段）；该值仍参与 config sha（按原始字节）。

## 验证与验收

### 自动化验证

- 回归测试先红后绿：
  - effective 默认模板 / 空串继承两条用例：修复前 FAILED，修复后通过
  - `warns_for_all_inert_fields_without_echoing_values`（16 项）：修复前 FAILED，修复后通过
  - `shipped_category_example_prompt_is_quiet`：示例修改前 FAILED，修改后通过
  - `ai_run_without_category_prompt_sends_article_title_and_body`：在原 CLI / config 代码上 FAILED
    （`title missing from prompt: Summarize the following article.`），修复后通过
- `cargo fmt --all -- --check`：通过（exit 0）
- `cargo clippy --locked --workspace --all-targets --jobs 1 -- -D warnings`：通过（exit 0）
- `cargo test --locked --workspace --no-fail-fast`：两次全量均 899 passed / 0 failed / 68 ignored（exit 0）；
  一次保留本机代理环境变量，一次去掉
- `cargo acceptance --format json run --lane static --lane sqlite`：passed，23 步，349.5 s
- 未执行：postgres lane（无 SQL / 迁移变更）、docker lane、真实 AI provider 调用

### 本机环境导致的间歇失败（非本次改动引入，未修改）

- 现象：前两次 `cargo test --workspace`（无 `--no-fail-fast`）在 `crates/cli/tests/publish_cmd_tests.rs` 的
  `ai_run_processes_the_healthy_category_when_another_lacks_credentials`（既有）与新增 e2e 用例上
  失败，结果为 `process_permanent_failed=1`，耗时约 1.3 s；单独运行该 binary 15/15 通过。
- 已核实：本机 `NO_PROXY` 只含 `127.*` / `localhost`；hyper-util 0.1.20 `NoProxy::from_string` 把 `127.*` 当域名，
  `127.0.0.1` 作为 IP 只与 IP 列表比对，因此 reqwest 发往 wiremock 的请求经 `HTTP_PROXY` 转发。
- 推断：该测试夹具 `request_timeout_seconds = 1`、`ai_max_attempts = 1`，在机器负载约 20–25（另一会话
  并行构建）时，代理链路超过 1 s → 超时 → 唯一一次尝试耗尽 → permanent_failed。人为 CPU 负载下
  带代理 1/10 失败、去掉代理 0/10；样本小，仅作倾向性证据。
- 未做：按约定不加重试、不放宽超时、不改既有夹具。可选处理：本机 `NO_PROXY` 增加 `127.0.0.1`
  或 `127.0.0.0/8`；或另行评估测试夹具超时。

### 手工验收

- release 二进制 `validate-config`（示例 app + ai 分类、伪造 env）：无 warning，exit 0
- 将示例模板改为旧文档写法 `{{title}}` / `{{body}}`：JSON 输出 2 条 `prompt_template` warning（缺
  `{body_text}`、未支持占位符），`status=success`，exit 0

### 独立跨模型审查（2026-10-16）

- `codex-reviewer`（gpt-6-astra）两个视角：代码 / 测试（effort max）与文档准确性（effort high），
  结论均为 `PASS / no blocker`；15 条发现，每条由两个独立核实 agent 对照源码尝试证伪。
- 采纳并修复：占位符扫描最坏二次复杂度 → 每个 `{` 只扫其后的标识符段（语义不变，补边界用例）；
  filtered 分支字段"被忽略"措辞过强（补 parser 用例：类型不符仍 `InvalidJson`，null 可接受）；
  plan/03 残留"prompt 经 reindex 升级""一次 ai-run 严格单 category"，plan/05 §8、plan/14 §B 同类旧述；
  plan/03 §5.3 新写的"按 retention policy 留档"不实；backfill extract 仅重置 `failed`、窗口左闭右开；
  acceptance case 测试文件路径；rebuild-report 示例参数；本节下方生产影响措辞；ASCII 标识符说明。
- 登记为 open（只改文档描述现状，不改实现）：D-007 留档策略与实现不一致（`on_failure` 不写
  feed/html，AI 原始响应始终写）；D-008 `--date-to` help 写"含"而 SQL 不含当天。
- 未采纳：测试代理隔离（两票均判定为既有、超出 R2，见上文环境问题）；backfill CLI 层 sha 与
  ai-run warning 日志的直接断言（对应函数已有单元测试，CLI 胶水各一行，记为可选补测）。

## 结果

- 可提交审查。
- 生产影响：用户已确认生产各分类模板含 `{title}` / `{body_text}`，ai-run 请求内容不变。仅在模板
  另含未支持的 `{标识符}`、或 `dedup.link_normalizer_version` 不为 `"1"` 时才会出现新 warning；
  这两项生产值未经核查（未读取生产配置）。JSON `summary.warnings[]` 新增可选 `category` 字段为增量变更。

## 风险与后续事项

- 默认模板变化只影响未写 `prompt_template`（或写空串）的分类；用户确认生产各分类模板均含
  `{title}` / `{body_text}`，生产行为不变。
- `backfill --target ai` 不按分类过滤、模型取全局：生产未使用，仅登记于 plan/10 §5.2，未修实现。
- 代码注释中约 36 处 `docs/design/*` 引用指向已移入 `docs-backup/` 的文件，留给 R3。
- D-007（留档策略）与 D-008（backfill 结束日期）待用户决定实现方向。

## 给下一位 Agent 的备注

- 入口：`crates/config/src/effective.rs`、`crates/config/src/validate/warnings.rs`、
  `crates/cli/src/commands/ai_run.rs::run_category`。
- 契约文档：`docs/plan/03-ai.md` §3–§4、`docs/plan/06-config.md`「独立能力与保留字段」。
