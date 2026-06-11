# AC-C-04: doctor 子命令

## 功能描述

汇总执行一组 `HealthCheck`：config / database / migrations / openai / github / rsshub / disk。
默认 shallow；`--deep` 启用跨表不变量扫描（I1–I6 / I8 / I9 预算耗尽的可领取行）。

exit code：含 `Fail` → 1；其余（含 `Warn`）→ 0。

面向场景：部署后启动验证、CI smoke、问题排查第一站。

## 验收标准

### 命中条件（success path）

- 全部 check 返回 `Ok` 或 `Info` → exit 0
- 仅 `Warn`（如 `--local-only` 时 GITHUB_TOKEN 缺失）不致命 → exit 0
- 默认 shallow 不跑跨表 deep scan
- `--deep` 启用 I1–I6 / I8 / I9 不变量校验
- pretty 输出列出每项 check 的 status + message
- JSON 输出含 `command`、`status`、`checks[]`
- `ai.enabled=false` 时 doctor **不视为失败**（仅 ai-run 拦）
- 远端 401 → `openai_check_reports_fail_for_unauthorized`
- DB 不可创建 → 立即报 `StorageError`

### 失败条件（failure path）

- 任一 check 返回 `Fail` → exit 1
- `--deep` 命中不变量违规 → `DoctorFailed` 错误
- DB pool 已关闭 → fail
- disk 最小空间无法满足 → fail

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `args_parsing_parses_doctor_with_deep` | `crates/cli/tests/args_parsing_tests.rs` | `--deep` 解析 |
| `doctor_cmd_shallow_non_failing_checks_return_success` | `crates/cli/tests/doctor_cmd_tests.rs` | shallow happy |
| `doctor_cmd_missing_github_token_is_not_failure` | 同上 | token 缺失非致命 |
| `doctor_cmd_uncreatable_database_path_returns_storage_error` | 同上 | DB 不可创建 |
| `doctor_cmd_deep_happy_path_returns_success` | 同上 | deep happy |
| `doctor_cmd_deep_i6_violation_returns_doctor_failed` | 同上 | deep 不变量违规 |
| `doctor_summary_pretty_snapshot_contains_status_lines` | 同上 | pretty 输出 |
| `doctor_summary_json_snapshot_has_command_status_and_checks` | 同上 | JSON 输出 |
| `i4_violation_ready_for_publish_with_non_keep_ai_row` | `crates/runtime/tests/doctor_deep_scan_tests.rs` | I4 违规 |
| `i4a_prime_violation_publish_item_bound_to_non_keep_ai_result` | 同上 | I4'a 违规 |
| `i4b_prime_violation_passthrough_publish_item_with_ai_row` | 同上 | I4'b 违规 |
| `i6_violation_successful_publish_record_with_unpublished_article` | 同上 | I6 违规 |
| `i9_feed_violation_claimable_entry_with_exhausted_budget` | 同上 | I9.feed 违规 |
| `i9_ai_violation_counts_only_exhausted_pending` | 同上 | I9.ai 违规（预算未满不计） |
| `i9_publish_violation_exhausted_stage_state` | 同上 | I9.publish 违规 |
| `config_check_reports_ok` | `crates/observability/tests/health_tests.rs` | config check |
| `database_check_reports_ok` | 同上 | database happy |
| `database_check_reports_fail_for_closed_pool` | 同上 | 关闭 pool fail |
| `migration_check_reports_ok_when_migration_table_has_version` | 同上 | migration check |
| `openai_check_reports_ok_for_chat_completion_shape` | 同上 | openai happy |
| `openai_check_reports_fail_for_unauthorized` | 同上 | openai 401 |
| `github_check_reports_warn_without_token` | 同上 | github warn |
| `disk_check_reports_ok_for_tempdir` | 同上 | disk happy |
| `disk_check_reports_fail_when_minimum_is_impossible` | 同上 | disk fail |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/07-observability.md](../../plan/07-observability.md) §6 HealthCheck
- 不变量：[../../plan/00-overview.md](../../plan/00-overview.md) §9 不变量
- 部署运维：`../../operations/troubleshooting.md`
