# typed-decision Phase B：decision-run 影子运行

- 日期：2026-10-31
- 作者 / Agent：Claude Code（Opus）
- 分支：`feat/decision-phase-b`（主 checkout，基于 main `e0ffdd0`）
- 相关 commit：ADR `75c27ac`、`af0d547`、`13d2640`、`51e3896`、`f449ef2`、`efaae37`；D-009 漂移登记 `abbd323`；
  实现 `697c845`、`8b73e59`、`dcd8e69`、`4f0d902`、`e460575`；文档 `92d2f20`
- 相关 tag / release：N/A
- 状态：`validated`（本地；PostgreSQL 测试待 CI postgres lane）

## 工作摘要

按 ADR 0011 实现独立的 `decision-run` 子命令：对近期文章按板块问题向 Jev / laya-server 提问，只把校验后的答案
写入新表 `decision_results`（另有策略快照表 `decision_policies`）。不写 `articles.state`、AI 结果或 `raw_artifacts`，
不在 `run` 中。ADR 经 Codex 四轮审查（BLOCKER 4 → 1 → 1 → 0），定稿把正确性放在与计时无关的性质上。

## 影响范围

- crate / 模块：
  - decision：`Exchange` / `RawInfo` / `SentState`、调用截止时间、凭据反射检查、按阶段的错误映射（新增 `Transport`）、
    关闭 reqwest 自动重试、`answers_document`、`revalidate`、策略 JSON 解析、`[decision]` 新字段、两家统一的 50 题 / 2000 字符上限
  - storage：迁移 `0006_decision_results`（两方言，已 pin）、`repo/decision_result{,_sql,_impl}.rs`
  - runtime：`flows/decision_run/{mod,task}.rs`，新增依赖 `runtime → decision`
  - cli：`commands/decision_run.rs`、`args.rs`、`commands/mod.rs`、`context_factory.rs`；测试夹具移到 `tests/support/`
- 真相源对象：新增 `decision_policies` / `decision_results` 与 `DecisionResultState`（定义在 storage，不进 domain，ADR 0011 §2）
- 额外影响：config schema `[decision]` 新增 7 个可选字段；新子命令；validate-config 新增 `decision_budget_below_round_one` warning

## 关键变更

- 每一步只领取一个窗口（≤ `concurrency` 且 ≤ 剩余预算），全部派发、全部收取后再进入下一步；没有可领取的行才生成。
- 请求额度在调用前扣减、退还不恢复；不带 `--category` 时要求预算覆盖每个板块一个窗口。
- 失败四类：单篇可重试（计次、暂停板块）、单篇永久、全局与文章无关（退还、中止）、全局疑似配置（计次、中止）；
  存储错误 / 超时 / owner 不符 / panic / 收尾超时中止运行且不推断行状态。
- 原始响应：`raw_response = on_failure | always | never`，只保存合法 JSON 且不含 key 的 2xx；长度、摘要、未保存原因总记录；
  TTL 清理跨身份、每次运行上限 `max(1000, 2 × 预算)`。
- 与 ADR 的一处偏离（已记入 plan/18 §8.4）：生成是单条 `INSERT … SELECT`，没有分页游标。

## 验证与验收

### 自动化验证（`92d2f20`）

- `cargo fmt --all --check`：通过
- `cargo clippy --workspace --all-targets --locked -- -D warnings`：通过
- `cargo test --workspace --no-fail-fast --locked`：1021 passed / 0 failed / 72 ignored
- `cargo acceptance run --lane static`：通过（7 项）
- PostgreSQL：`crates/storage/tests/decision_result_pg_tests.rs` 本地无 docker，**未执行**；由 CI postgres lane
  （`--include-ignored`）执行

### 手工验收

- 未执行：对真实 provider 的 `decision-run`（Phase A 的两家探针仍有效，记录见 Phase A handoff）。

## 结果

- 默认关闭；不配置 `[decision]` 或不调度 `decision-run` 时，生产行为不变。
- 合并前待：Codex 代码审查、CI（含 postgres lane）。

## 风险与后续事项

- doctor 不认识这两张表（ADR 0011 §11 有意保留的缺口）。
- 回放只有库入口（`revalidate`），没有 `replay --kind=decision`；非 JSON / 含 key 的 2xx 不可回放（已批准的两项例外）。
- Phase C：人工标注评估、阈值拟合、是否让判断参与过滤。

## 给下一位 Agent 的备注

- 入口：ADR 0011、plan/18 §8、`crates/runtime/src/flows/decision_run/mod.rs`。
- 卸载按 ADR 0011 §13：先停调度并确认进程退出，再发布带删表迁移的版本；0006 与删表迁移文件永久保留。
