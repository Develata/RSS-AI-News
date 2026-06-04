# AC-C-03: reindex 子命令

## 功能描述

版本化规则升级：升级 link_hash 规范化 / 正文规范化 / 分类规则后，重算 `feed_entries` /
`articles` / `feed_sources` 的字段，并把对应 `rule_versions` 行从 pending 升 active、旧 active 降 superseded。

三种 target：`link_hash` / `content_hash` / `categories`；外加 `all` 展开为三者顺序执行；
`--abort` 中止指定的 running 任务。

面向场景：算法升级（含 minor schema 演进）。与 backfill 互补，详见 [./backfill.md](./backfill.md)。

## 验收标准

### 命中条件（success path）

- `start_tx`：原子写入 `rule_versions(pending)` + `reindex_jobs(pending)`
- partial unique index 保证同 target 只能存在一个 pending/running 任务
- claim → state=`running`，lease 写入；checkpoint 按批 commit `last_processed_id`
- lease 过期 reclaim → 保留 checkpoint + started_at，可 resume
- 完成时事务内：rule_versions pending→active、旧 active→superseded、reindex_jobs→completed
- `link_hash`：扫描所有 entry，重算 hash，未变 / 变化分别计数
- `content_hash`：扫描所有 article 重算；命中 unique conflict 时计入 conflict 计数（不写入）
- `categories`：扫描 sources 增量插入新行、归档已不在 config 的旧行；幂等（第二次执行归档=0）
- categories reindex 把 active `config` kind rule_version_id 写入新插入 feed_sources 的 `config_rule_version_id`
- `--dry-run` 与 real run 计数完全一致；dry-run 不写任何业务表
- `--abort <job_id>` 中止 running 任务，**保留** 已 checkpoint 的数据；幂等
- `all`：解析为 link_hash → content_hash → categories 顺序执行

### 失败条件（failure path）

- target 已有 active job → `start_tx` 回滚（partial unique）
- rule_version_tag 冲突 → `start_tx` 回滚
- claim 时 lease 已被其它 worker 持有 → 不抢占
- mark_failed 时保留 pending 新 rule_version + 不降级旧 active（避免读路径丢 active）
- abort 终态任务 → idempotent noop
- abort 缺失任务 → `NotFound` outcome（exit 3）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `args_parsing_parses_reindex_with_target_link_hash` | `crates/cli/tests/args_parsing_tests.rs` | 单 target |
| `args_parsing_reindex_target_all_parses` | 同上 | `all` 解析 |
| `args_parsing_reindex_target_all_expands_to_three_domain_targets_in_order` | 同上 | `all` 展开顺序 |
| `args_parsing_reindex_abort_parses_without_target` | 同上 | `--abort` 单飞 |
| `args_parsing_reindex_abort_and_target_are_mutually_exclusive` | 同上 | abort/target 互斥 |
| `args_parsing_reindex_without_target_or_abort_is_rejected` | 同上 | 必须二选一 |
| `writes_both_rows_atomically_with_pending_status` | `crates/storage/tests/reindex_job_start_tx_tests.rs` | start_tx 原子 |
| `distinct_targets_can_coexist_in_pending` | 同上 | 跨 target 共存 |
| `rolls_back_when_target_already_has_active_job` | 同上 | partial unique 拦截 |
| `rolls_back_when_rule_version_tag_collides` | 同上 | tag 冲突 |
| `allows_restart_after_previous_terminal` | 同上 | 终态后可重启 |
| `reindex_link_hash_recomputes_changed_rows` | `crates/runtime/tests/w9c_runtime_tests.rs` | link_hash 重算 |
| `reindex_link_hash_unchanged_rows_counted` | 同上 | 未变行计数 |
| `reindex_link_hash_invalid_url_counted_errors` | 同上 | 非法 URL 错误计数 |
| `reindex_content_hash_updates_when_body_text_diff` | 同上 | content_hash 重算 |
| `reindex_content_hash_skips_unique_conflict` | 同上 | unique conflict 跳过 |
| `reindex_content_hash_unchanged_when_hash_matches` | 同上 | 未变不写 |
| `reindex_categories_inserts_new_sources` | 同上 | 增量插入 |
| `reindex_categories_archives_obsolete_sources` | 同上 | 归档旧 source |
| `reindex_categories_second_run_archives_nothing` | 同上 | 幂等 |
| `reindex_categories_writes_config_kind_id_into_feed_sources_config_version` | 同上 | config rule_version 绑定 |
| `reindex_link_hash_finalizes_reindex_jobs_row` | 同上 | jobs 终态 |
| `reindex_categories_finalizes_reindex_jobs_row_without_checkpoint` | 同上 | 无 checkpoint 终态 |
| `reindex_promotes_rule_version_to_active_on_completion` | 同上 | active 切换 |
| `reindex_demotes_previous_active_rule_version_on_second_run` | 同上 | 旧 active 降级 |
| `abort_running_job_transitions_to_aborted_and_preserves_data` | 同上 | abort 保留数据 |
| `abort_already_terminal_job_is_idempotent_noop` | 同上 | abort 幂等 |
| `abort_missing_job_returns_not_found_outcome` | 同上 | abort 缺失 |
| `dry_run_link_hash_matches_real_run_numbers_and_writes_nothing` | 同上 | dry-run 一致 |
| `dry_run_content_hash_distinguishes_unchanged_updated_conflict` | 同上 | dry-run 三态 |
| `dry_run_categories_counts_would_archive_without_writing` | 同上 | dry-run 不写 |
| `reindex_lease_reclaim_preserves_checkpoint_and_started_at_for_resume` | 同上 | reclaim 续跑 |
| `reindex_link_hash_batch_size_one_processes_all_rows_and_checkpoints_last_id` | 同上 | checkpoint 推进 |
| `reindex_second_start_for_same_target_rejected_by_partial_unique` | 同上 | partial unique 端到端 |
| `reindex_mark_failed_keeps_old_active_and_pending_new_rule_version` | 同上 | 失败保旧 active |
| `reindex_link_hash_partial_failure_continues_processing_remaining_rows` | 同上 | 部分失败续跑 |
| `reindex_dry_run_then_real_run_promotes_without_polluting_rule_versions_chain` | 同上 | dry-run 不污染 |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/05-storage.md](../../plan/05-storage.md) §8 reindex
- 决策（建设中）：`../../adr/0004-active-rule-resolver-partial-unique.md`
- 与 backfill 边界：[./backfill.md](./backfill.md)
