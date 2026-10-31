# AC-C-10: decision-run 子命令

## 功能描述

typed-decision 影子运行（[ADR 0011](../../adr/0011-decision-run-shadow-runner.md)）：对近期文章按板块问题向
Jev 或自部署 laya-server 提问，只把校验后的答案写入 `decision_results`。不写 `articles.state`、AI 结果或
`raw_artifacts`，不在 `run` 中，需要单独调度。

## 验收标准

### 命中条件（success path）

- 启用且配置合法时，对窗口内、未判断过的文章各发一次请求，成功行为 `succeeded`，`answers_json` 为 `schema = 1` 的自描述文档
- 单次命令的请求数不超过 `max_requests_per_run`；重复运行不重复判断同一文章、同一策略与执行身份
- 不带 `--category` 时，预算满足 `concurrency × 分类数` 则每个积压分类都在首轮得到一个窗口
- 换问题或 provider 后，旧身份的 pending 行不被执行，摘要报告其数量
- 留档策略：`on_failure` 只保存未通过校验的 2xx；`always` 保存每次可安全保存的 2xx；`never` 不保存；长度与摘要总是记录
- 事件 stage 为 `decision_run`，不含文章正文、答案与响应体

### 失败条件（failure path）

- `[decision]` 缺失或 `enabled = false`、凭据缺失、问题不合法、首轮预算不足 → exit 78，且不打开数据库
- `--dry-run` → exit 2
- 凭据被拒、连接失败、限流：本行退还尝试次数并中止运行（凭据类 exit 78，连接 exit 1，限流 exit 0）
- 疑似配置错误（其它 400 / 422、415、未归类状态码）：本行计次并中止，exit 1
- 单篇可重试失败：本行计次，该分类本次暂停；单篇永久失败：本行 `failed`；二者都不影响退出码
- 存储错误、存储超时、owner 不符、任务 panic、收尾超时：中止运行，exit 1，不推断行状态

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `a_missing_or_disabled_section_exits_78_without_storage` | `crates/cli/tests/decision_run_cmd_tests.rs` | 段缺失 / 关闭 |
| `a_missing_key_exits_78_without_naming_its_value` | 同上 | 凭据缺失 |
| `a_budget_below_the_first_round_exits_78_unless_one_category_is_selected` | 同上 | 首轮预算 |
| `dry_run_is_rejected` | 同上 | `--dry-run` |
| `an_enabled_run_records_the_answer` | 同上 | 端到端（mock laya-server） |
| `successful_rows_store_answers_and_events_without_content` | `crates/runtime/tests/decision_run_tests.rs` | 成功路径与事件 |
| `the_request_budget_caps_calls` | 同上 | 请求预算 |
| `provider_wide_failures_refund_and_stop_with_exit_78` | 同上 | 全局失败退还 |
| `rate_limits_stop_the_run_but_exit_zero` | 同上 | 限流 |
| `item_retryable_failures_charge_and_pause_the_category` | 同上 | 单篇可重试 |
| `suspected_configuration_errors_charge_the_trigger_and_exit_1` | 同上 | 疑似配置 |
| `raw_bodies_follow_the_policy` | 同上 | 留档策略 |
| `every_backlogged_category_gets_a_first_round_window` | 同上 | 首轮公平 |
| `a_changed_policy_never_executes_rows_of_the_old_one` | 同上 | 身份隔离 |
| `too_little_lease_left_refunds_without_calling` | 同上 | lease 不足 |
| `a_panicking_task_stops_the_run_and_leaves_the_row_to_reclaim` | 同上 | panic |
| `a_hanging_call_is_cancelled_at_the_drain_deadline` | 同上 | 收尾超时 |

存储与客户端层的测试见 [../../plan/18-typed-decision.md](../../plan/18-typed-decision.md) §8.5。

## 当前状态

`passing`

## 相关文档

- 设计：[../../adr/0011-decision-run-shadow-runner.md](../../adr/0011-decision-run-shadow-runner.md)、[../../plan/18-typed-decision.md](../../plan/18-typed-decision.md) §8
- 配置：[../../plan/06-config.md](../../plan/06-config.md) §typed-decision
- CLI：[../../operations/cli-reference.md](../../operations/cli-reference.md)
