# AC-P-04: Publish 本地 + GitHub 发布流水线

## 功能描述

按 `(category, report_date)` 维度组装一份发布报告：init → snapshot freeze → render → store-local → publish-remote 五阶段。
每阶段独立 lease 推进 `PublishState` 状态机，snapshot 冻结后保证 `rebuild-report` 可字节相等重建。

面向场景：每日定时触发 `publish` 子命令，或 `run` 的最终段；可 `publish-all` 一次跑全部分类。

## 验收标准

### 命中条件（success path）

- **init**：基于 `(category_key, report_date)` 幂等键创建/复用 `publish_records.Pending` 行
- **默认报告日期**：未给 `--date` 时取运行时刻在有效目标时区（`--timezone` / `[publish].target_timezone`）下的日期，非 UTC；未知时区 → 配置错误（exit 78），不写库
- **freeze**：将命中 `[publish].min_importance_score` × `include_unscored` × `candidate_window_hours` 的 article 冻结为 `publish_items`；
  - AI 路径：选 `ArticleState=ReadyForPublish`
  - AI-off 直通：`Persisted` article 在同 tx 内提升为候选（无需 ai_result）
  - 候选为空 → 返回 `SnapshotEmpty`（不算失败）；记录回到 `pending`、退还 attempt，同日后续运行有新候选时照常发布
- **render**：snapshot_frozen → rendered；空 items 失败
- **store-local**：写本地文件 + 推进 article 到 `Published`（无远端目标时）/ 保持文章不动（有远端目标待 push）；
  - 分类 `path_template` override 生效
- **publish-remote**：批量提交一个 commit 覆盖多 publish_records；422 lost-update 自动重试一次；429 保持状态可重试
- **本地原子写**：临时文件 fsync 后 rename 覆盖；越出输出根的符号链接（父目录或目标）被拒绝
- **跨分类路径碰撞（运行时）**：`publish-all` 在首次写入前按本次报告日期渲染全部分类真实路径并查重；碰撞 → 报错退出，不打开存储（不建库、不迁移、不轮换 config）、不建 record、不登记规则版本、不落盘。
  比较忽略大小写，且一个路径是另一路径的整段父目录也算碰撞（validate 同规则）
- **--force**：每次调用新建唯一的 `pending` render trace 行与新记录；force 行永不成为 active
- **path 防穿越**：含 `..` / 反斜杠 / 无日期 token 的模板在 validate 阶段就报错（详见 [./06-config-loading.md](./06-config-loading.md)）
- 远端 publish 成功后 article → `Published`；批次成功后 record → `PublishedRemote`
- **lease 按时刻判定**：SQLite 上 publish_records 的 claim / reclaim / sweep 用 `julianday()` 比较（lease 写入向上、now 向下取整到毫秒），lease 绝不提前过期；PG 用 `timestamptz`
- **重试预算按阶段计**：每阶段最多 `publish_max_attempts` 次 claim（`stage_attempt_count`，推进即清零）；`attempt_count` 为累计审计值
- **快照归属**：选稿排除已在其它未失败记录（活动快照）中的文章；freeze 事务内复查，输掉竞争 → 回滚、立即释放 lease、退还 claim，下轮不含该文章；记录 `failed` 释放归属；AI-off 下未被持有的 `ready_for_publish` 遗留文章可直接冻结
- **record 隔离**：`publish` 与 `publish-all` 的每个阶段（freeze / render / store-local / publish-remote）都只 claim init 得到的 `publish_record_id`；
  单分类 `publish` 不得领取或推进其他分类残留的记录。无 ID 的 `PublishFlow::freeze` 只领取 `category_key` 与选稿分类相同的
  pending 记录；按 ID 的 `freeze_record` 遇分类不符 → `Failed { category_mismatch }`、退还 claim、不写快照（R1-P-S15）。例外：每阶段 claim 前的全局 maintenance
  （lease 过期 reclaim + 预算耗尽 sweep → `failed`）按设计作用于全表，不区分分类

### 失败条件（failure path）

- 任一阶段读库（候选读取、快照读取、冻结写入）遇可重试存储错误（pool timeout、SQLite busy、PG 40001/40P01 等）
  → 保持当前阶段态、释放 lease、计入重试预算（耗尽才转 `failed`），`last_error_kind` 为存储错误类别；
  下一次运行可继续推进。不可重试的存储错误 → `failed`
- store-local 目录不可写 → record → `Failed`，errors 表登记；命令 summary 该阶段 `verdict=failed`、`status=fail`、exit 1
- 远端 401，或无限流信号的 403 → `GithubAuthFailed`（终态，不重试）
- 单报告 Contents API：路径按段百分号编码、`ref` 编码为查询参数（中文 / 空格 / 保留字符路径可发布）；任何 GitHub 失败的消息含操作、状态、message 的固定词表摘要与 `x-github-request-id`（整值符合 GitHub 形状且不含 token 时）；服务端回显的自由文本不落库、不进事件
- 远端 403 且 `x-ratelimit-remaining: 0` / 带 `retry-after` / message 提到 rate limit → `GithubRateLimit`（可重试）
- 远端 429 → `GithubRateLimit`，state 保持、article 不晋升；GitHub 要求的重试时刻（钳制 1 h）写入 `next_attempt_at`，到点前的运行 `NothingToClaim`
- 远端 422 non-fast-forward：同一次尝试内重试一次；第二次 422 为永久错误 → record `failed`（`github_api_error`），文章不晋升
- 远端 5xx / 传输失败 / 远端超时 / 409 → retryable：保持 `StoredLocal`、计本阶段预算（耗尽 → `failed`）
- 多报告批次重试：HEAD 已逐字节包含整批内容（上次 ref 更新落地、响应丢失）→ 不新建 commit，回执指向该 HEAD
- 单报告重试：分支上既有 blob 与本地内容一致且在固定 HEAD commit 复核一致 → 不发 PUT，回执指向该 HEAD
- 歧义写入（PATCH ref / PUT contents 后传输失败、5xx，或远端 deadline 切断）：释放前只读核对 HEAD，已含全部内容 → 记成功；
  否则按原错误释放（最后一次尝试也不会因已落地的写入而 `failed`）
- promote target article 已被其它流转推进 → `ArticleConflict`
- 存量重复归属（0008 前两份未失败快照共享文章）：store_local / remote 写前拦截，已发布者或低 id 者胜出；败方不写外部产物、
  释放 lease、退还 claim、保持阶段态 → `ArticleConflict`（`article_owned_by_active_snapshot`）
- 目标漂移：`stored_local` 记录的 `remote_target`（`owner/repo:branch`）与当前配置的远端目标不同 →
  不发任何请求，`TargetMismatch { stored, current }`，record → `failed`（`last_error_kind=remote_target_mismatch`）；
  批量发布中只跳过该记录，其余照常提交

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `init_creates_publish_record_returns_created_outcome` | `crates/runtime/tests/publish_freeze_tests.rs` | init happy |
| `init_returns_already_exists_on_idempotency_key_conflict` | 同上 | 幂等键 |
| `freeze_with_ai_path_inserts_publish_items_and_advances_record_to_snapshot_frozen` | 同上 | AI 路径 freeze |
| `freeze_record_claims_requested_pending_record_not_older_one` | 同上 | claim 指定 record |
| `freeze_record_isolates_two_concurrent_pending_records_by_id` | 同上 | 并发隔离 |
| `freeze_without_id_claims_only_its_own_categorys_record` | 同上 | 无 ID freeze 只领本分类记录 |
| `freeze_record_rejects_a_record_of_another_category` | 同上 | 按 ID freeze 拒绝分类不符（`category_mismatch`，退还 claim） |
| `pg_pending_claim_in_category_skips_other_categories` | `crates/storage/tests/publish_record_pg_tests.rs` | PG 分类限定 claim |
| `freeze_with_ai_off_passthrough_promotes_persisted_articles_in_same_tx` | 同上 | AI-off 直通 |
| `freeze_returns_snapshot_empty_when_no_candidates_match` | 同上 | SnapshotEmpty |
| `freeze_after_empty_snapshot_publishes_articles_that_arrive_later` | 同上 | 空跑不耗预算、后到文章可发布 |
| `freeze_returns_nothing_to_claim_when_no_pending_records` | 同上 | 空闲 |
| `freeze_skips_articles_without_correct_category_key` | 同上 | category 隔离 |
| `small_budget_completes_all_four_stages_without_failures` | `crates/runtime/tests/publish_stage_budget_tests.rs` | 按阶段预算：max=2 无故障走完四阶段 |
| `one_retryable_failure_per_stage_is_absorbed_by_that_stage_budget` | 同上 | 单阶段 1 次失败可重试 |
| `exhausting_one_stage_budget_fails_the_record` | 同上 | 单阶段 2 次失败 → failed |
| `sqlite_0008_gives_existing_rows_a_fresh_stage_budget_and_rolls_back` | `crates/storage/tests/publish_stage_budget_migration_tests.rs` | 存量行迁移 + down（PG 同名 `pg_…`） |
| `pg_publish_budget_is_per_stage` | `crates/storage/tests/publish_record_pg_tests.rs` | PG 按阶段预算 |
| `selection_excludes_articles_owned_by_an_active_snapshot_until_it_fails` | `crates/storage/tests/publish_freeze_tests.rs` | 选稿排除活动快照文章，failed 释放 |
| `freeze_snapshot_rejects_article_owned_by_another_active_snapshot` | 同上 | freeze 事务内复查归属 |
| `freeze_snapshot_keeps_unowned_ready_article_and_promotes_persisted_one` | 同上 | AI-off 遗留 ready 文章不整批回滚（S9） |
| `freeze_losing_an_article_releases_lease_without_charging_budget_and_retries_without_it` | `crates/runtime/tests/publish_freeze_tests.rs` | 冲突立即释放 lease、不耗预算、下轮排除 |
| `pg_concurrent_freeze_of_the_same_article_serialises_on_the_article_lock` | `crates/storage/tests/publish_record_pg_tests.rs` | PG 并发 freeze 由文章行锁串行化 |
| `pg_freeze_ownership_holds_when_the_session_defaults_to_repeatable_read` | 同上 | 会话默认 REPEATABLE READ 时 freeze 仍显式用 READ COMMITTED，归属不重复 |
| `pg_ai_off_selection_excludes_article_owned_by_active_snapshot` | 同上 | PG AI-off 选稿排除 |
| `pg_snapshot_ownership_conflict_prefers_published_then_lowest_id` | 同上 | PG 存量重复归属：已发布者优先、否则低 id 胜出；败方释放退还、保态 |
| `publish_remote_stops_the_higher_record_sharing_an_article_before_sending` | `crates/runtime/tests/publish_remote_tests.rs` | 存量重复归属：败方不发请求、保态、退还 claim |
| `publish_remote_stops_a_lower_record_whose_article_a_published_record_holds` | 同上 | 已发布的持有者无论 id 都胜出 |
| `publish_remote_batch_drops_only_the_record_losing_a_shared_article` | 同上 | 批量只剔除败方 |
| `publish_remote_sends_the_other_record_after_the_keeper_failed` | 同上 | 胜方 failed 后归属释放 |
| `store_local_stops_the_higher_record_sharing_an_article_before_writing` | `crates/runtime/tests/publish_store_local_tests.rs` | 败方不落盘 |
| `freeze_transient_candidate_load_error_keeps_record_pending_and_next_run_freezes` | 同上 | 瞬时存储错误保态、下轮继续 |
| `freeze_permanent_candidate_load_error_fails_record` | 同上 | 不可重试存储错误 → failed |
| `render_advances_snapshot_frozen_to_rendered_when_items_exist` | `crates/runtime/tests/publish_render_tests.rs` | render happy |
| `render_returns_failed_when_publish_record_has_no_items` | 同上 | 空 items 失败 |
| `store_local_with_no_remote_target_publishes_locally_and_promotes_articles` | `crates/runtime/tests/publish_store_local_tests.rs` | 本地 + promote |
| `store_local_uses_category_path_template_override` | 同上 | 分类 path override |
| `store_local_with_remote_target_advances_to_stored_local_without_promoting_articles` | 同上 | 有远端不 promote |
| `store_local_returns_article_conflict_when_promote_target_already_advanced` | 同上 | ArticleConflict |
| `store_local_returns_failed_with_local_io_error_when_target_dir_unwritable` | 同上 | 不可写 |
| `store_local_retryable_failure_keeps_rendered_state_and_reclaim_succeeds` | 同上 | 可重试保态 |
| `publish_remote_succeeds_promotes_articles` | `crates/runtime/tests/publish_remote_tests.rs` | 远端 happy + promote |
| `publish_remote_record_claims_only_the_requested_record` | 同上 | 远端 claim 指定 record |
| `publish_remote_batch_succeeds_with_one_commit_for_multiple_records` | 同上 | 多 record 单 commit |
| `publish_remote_rate_limit_keeps_state_and_articles` | 同上 | 429 |
| `publish_remote_rate_limit_persists_the_reset_and_skips_claims_until_then` | 同上 | 限流 reset 写入 `next_attempt_at`；期内重跑不领取、不请求；到点后发布 |
| `publish_remote_rate_limit_wait_is_clamped_to_one_hour` | 同上 | 远期 reset 钳制到 1 h |
| `sqlite_rate_limit_wait_is_honoured_by_every_claim_and_cleared_on_advance` | `crates/storage/tests/publish_stage_recovery_tests.rs` | SQLite 两类 claim 按时刻遵守等待；推进 / 普通 release 清空 |
| `pg_rate_limit_wait_is_honoured_by_every_claim_and_cleared_on_advance` | `crates/storage/tests/publish_record_pg_tests.rs` | PG 同语义 |
| `publish_remote_auth_failed_is_terminal_without_promoting_articles` | 同上 | 401 终态 |
| `publish_remote_rejects_record_frozen_for_another_target_without_sending` | 同上 | 目标漂移拦截 |
| `publish_remote_sends_record_whose_stored_target_matches` | 同上 | 目标一致照常发布 |
| `publish_remote_batch_skips_only_the_mismatched_record` | 同上 | 批量中只跳过漂移记录 |
| `target_identity_uses_the_publish_record_remote_target_format` | `crates/publish/src/github.rs` | 目标身份格式 |
| `publish_many_creates_one_commit_for_multiple_reports` | `crates/publish/tests/github_target_tests.rs` | target 层 batch |
| `publish_many_retries_once_after_non_fast_forward_then_succeeds` | 同上 | 422 重试 |
| `publish_many_surfaces_422_after_max_retries` | 同上 | 422 超限 |
| `publish_remote_batch_second_non_fast_forward_fails_the_records` | `crates/runtime/tests/publish_remote_tests.rs` | 第二次 422 → 记录 `failed`（持久状态断言），PATCH 恰 2 次 |
| `retry_after_lost_ref_update_response_records_existing_head_without_new_commit` | `crates/publish/tests/github_batch_idempotency_tests.rs` | ref 更新已落地但响应丢失：重试不新建 commit，按 HEAD 记回执 |
| `one_differing_blob_still_creates_a_commit` | 同上 | 任一 blob 不一致照常提交 |
| `blob_lookup_error_is_not_treated_as_already_published` | 同上 | 查询出错不当作已发布 |
| `ambiguous_ref_update_not_on_head_keeps_the_write_error` | 同上 | 歧义 PATCH 未落地：保留原 5xx 错误 |
| `ambiguous_contents_put_is_verified_against_head` | 同上 | 单报告歧义 PUT：HEAD 已含内容 → 成功回执 |
| `ambiguous_contents_put_not_on_head_keeps_the_write_error` | 同上 | 单报告歧义 PUT 未落地：保留原错误 |
| `refused_contents_put_is_not_verified` | 同上 | 4xx 写入被拒不核对 |
| `remaining_single_report_already_on_head_records_receipt_without_put` | 同上 | 批量部分回执后重跑剩一条：HEAD 已含 → 零 PUT |
| `matching_branch_blob_not_confirmed_at_head_still_puts` | 同上 | HEAD 已移动未含该 blob → 照常 PUT |
| `publish_remote_timeout_on_last_attempt_records_success_when_head_holds_the_report` | `crates/runtime/tests/publish_remote_tests.rs` | 最后一次尝试超时但已落地 → `published_remote` |
| `publish_remote_timeout_without_the_report_at_head_keeps_the_timeout` | 同上 | 未落地 → 维持超时语义 |
| `publish_remote_batch_timeout_records_success_when_head_holds_the_batch` | 同上 | 批量超时核对 |
| `git_blob_sha_matches_git_hash_object` | `crates/publish/src/github.rs` | blob sha 与 `git hash-object` 一致（含中文） |
| `auth_failure_maps_to_github_auth_failed` | 同上 | 401 映射 |
| `chinese_path_leaves_the_client_percent_encoded_on_get_and_put` | `crates/publish/tests/github_contents_path_tests.rs` | 中文路径请求行按段编码（原始字节） |
| `create_chinese_path_sends_encoded_get_then_put_without_sha` | 同上 | 新建：GET 404 → PUT 无 sha；ref 编码 |
| `update_chinese_path_sends_encoded_get_then_put_with_existing_sha` | 同上 | 更新：GET 200 → PUT 带 sha |
| `space_and_reserved_characters_are_encoded_exactly_once_per_segment` | 同上 | 空格 / `%#?` 只编码一次 |
| `ascii_path_request_target_is_unchanged` | 同上 | ASCII 路径回归 |
| `batch_ref_routes_encode_hash_percent_and_keep_slashes_in_the_branch` | `crates/publish/tests/github_ref_route_tests.rs` | ref GET / PATCH 分支名按段编码（原始请求行，`#` `%` `/`） |
| `plain_hierarchical_branch_ref_route_is_unchanged` | 同上 | 含 `/` 的普通分支名不变 |
| `transport_failure_after_the_ref_patch_is_verified_against_head` | 同上 | PATCH 发出后连接断开（传输失败）→ 只读核对 HEAD → 记成功 |
| `contents_get_failure_keeps_operation_status_message_and_request_id` | 同上 | 失败保留操作、状态、固定词表摘要、request id；服务端回显的正文标记与 token 不出现 |
| `malformed_or_token_bearing_request_id_is_dropped` | 同上 | 形状不符或含 token 的 request id 整值丢弃 |
| `auth_failures_keep_the_actual_status_and_request_id` | 同上 | 401 / 403 认证失败保留实际状态、操作、request id |
| `rate_limits_keep_operation_status_and_request_id` | 同上 | 两类 403 限流与 429 保留上下文 |
| `oversized_success_keeps_its_request_context` | 同上 | 过大成功响应保留上下文 |
| `success_missing_a_required_field_keeps_its_request_context` | 同上 | 缺字段 2xx：502 + 实际状态 + request id |
| `contents_200_without_sha_is_a_protocol_error` | 同上 | Contents 200 无 sha → 协议错误 |
| `response_reason_keeps_only_known_phrases_never_server_text` | `crates/publish/src/github.rs` | 持久化原因只来自固定词表 |
| `request_id_is_kept_only_in_githubs_shape_and_never_with_the_token` | 同上 | request id 整值校验 |
| `github_failure_persists_only_sanitized_reason_and_valid_request_id` | `crates/runtime/tests/publish_remote_tests.rs` | `last_error` 与 `publish_failed` 事件均不含回显标记与 token；事件 payload 含 `operation` / `http_status` / `request_id` |
| `contents_put_failure_keeps_operation_and_request_id` | 同上 | PUT 失败上下文 |
| `contents_rate_limited_403_is_still_retryable_with_request_context` | 同上 | 403 限流分类不变 |
| `rate_limit_maps_to_github_rate_limit` | 同上 | 429 映射 |
| `forbidden_with_exhausted_quota_maps_to_retryable_rate_limit` | 同上 | 403 + remaining=0 → 限流 |
| `forbidden_with_retry_after_maps_to_retryable_rate_limit` | 同上 | 403 + retry-after → 限流 |
| `forbidden_without_rate_limit_signals_maps_to_auth_failed` | 同上 | 普通 403 → 认证失败 |
| `status_403_naming_a_rate_limit_maps_to_rate_limit` | `crates/publish/tests/github_classify_tests.rs` | message 含 (secondary) rate limit |
| `status_403_without_rate_limit_signal_maps_to_auth_failed` | 同上 | 普通 403 |
| `forbidden_with_an_http_date_retry_after_is_a_rate_limit_until_that_date` | 同上 | HTTP 日期形式 `retry-after` |
| `unparsable_retry_after_still_marks_a_403_as_rate_limited` | 同上 | 头存在即限流信号 |
| `oversized_retry_after_seconds_are_capped_without_overflow` | 同上 | 超大秒数封顶、无溢出 |
| `past_http_date_retry_after_means_now` | 同上 | 过去日期 = now |
| `rate_limit_phrase_past_the_diagnostic_cap_is_still_a_rate_limit` | `crates/publish/tests/github_contents_path_tests.rs` | 8 KiB 之后的 "rate limit" 仍识别 |
| `http_date_retry_after_on_a_403_is_a_rate_limit` | 同上 | 线上 403 + HTTP 日期 |
| `local_fs_target_rejects_path_with_parent_traversal` | `crates/publish/tests/local_target_tests.rs` | 路径穿越拒绝 |
| `local_fs_target_creates_parent_directories` | 同上 | 父目录自建 |
| `local_fs_target_replaces_existing_report_without_truncating_it_in_place` | 同上 | 临时文件 + rename，不原地截断 |
| `local_fs_target_rejects_parent_symlink_escaping_the_root` | 同上 | 越界父目录链接拒绝 |
| `local_fs_target_rejects_destination_symlink_escaping_the_root` | 同上 | 越界目标链接拒绝 |
| `local_fs_target_follows_symlinks_that_stay_inside_the_root` | 同上 | 根内链接照常 |
| `interrupted_write_before_commit_leaves_old_report_intact` | `crates/publish/src/local.rs` | 中断写保留旧报告 |
| `taken_temp_name_is_neither_written_nor_removed` | 同上 | 临时名被占用：不碰他人文件，换名 |
| `every_temp_name_taken_is_an_error_that_removes_nothing` | 同上 | 重试耗尽报错且不删任何文件 |
| `temp_names_are_short_and_unique` | 同上 | 临时名短且唯一 |
| `owner_only_temp_file_is_created_0600` | 同上 | 替换用临时文件 0600 创建 |
| `local_fs_target_keeps_the_permissions_of_the_report_it_replaces` | `crates/publish/tests/local_target_tests.rs` | 替换保留 0600 / 0640 |
| `local_fs_target_new_report_gets_the_default_create_permissions` | 同上 | 新报告为默认创建权限 |
| `local_fs_target_publishes_a_report_whose_name_is_near_the_name_limit` | `crates/publish/tests/local_target_tests.rs` | 240 字节文件名可发布、无残留临时文件 |
| `publish_force_creates_unique_non_active_render_versions_on_a_fresh_db` | `crates/cli/tests/publish_cmd_tests.rs` | --force 每次唯一且非 active |
| `bootstrap_refuses_a_superseded_default_rule_when_none_is_active` | `crates/runtime/tests/publish_rule_bootstrap_tests.rs` | 无 active render 规则（default 已 superseded）→ 建记录前报错 |
| `bootstrap_refuses_a_superseded_default_selection_policy` | 同上 | selection_policy 同规则 |
| `bootstrap_returns_the_active_rules_and_seeds_them_on_a_fresh_db` | 同上 | 新库登记 active；已有非 default active 行照用 |
| `publish_all_refuses_categories_whose_paths_collide_on_the_report_date` | `crates/cli/tests/publish_cmd_tests.rs` | 真实日期路径碰撞在写入前拦截 |
| `publish_all_collision_check_runs_before_any_database_write` | 同上 | 碰撞检查早于打开存储：空库不建库，已有库的 rule_versions（含 config）不变 |
| `month_day_and_day_month_templates_collide_on_eleven_eleven` | `crates/runtime/src/flows/publish/paths.rs` | 11-11 碰撞 / 其它日期不碰撞 |
| `paths_differing_only_in_case_collide` | 同上 | 仅大小写不同的路径碰撞 |
| `a_report_path_that_is_another_reports_directory_collides` | 同上 | 文件路径是另一路径的父目录 → 碰撞 |
| `paths_differing_only_in_letter_case_collide` | `crates/config/src/validate/checks.rs` | 共享碰撞规则：大小写折叠（含非 ASCII）、整段父子、整段前缀才算 |
| `publish_default_date_uses_the_effective_timezone_and_rejects_unknown_zones` | `crates/cli/tests/publish_cmd_tests.rs` | 默认日期按目标时区；未知时区写库前报错 |
| `shanghai_after_midnight_is_the_local_date_not_the_utc_date` | `crates/runtime/src/flows/publish/report_date.rs` | 2026-11-21T00:30+08:00 → 2026-11-21（另含负偏移、夏令时、未知时区用例） |
| `real_iana_zones_are_accepted_and_made_up_ones_rejected` | `crates/config/src/validate/checks.rs` | validate 按 IANA 库校验 |
| `etc_unknown_is_rejected_in_any_case` | 同上 | 共享解析拒绝 `Etc/Unknown`（任意大小写） |
| `surrounding_whitespace_is_not_trimmed_away` | 同上 | 不 trim，三处同规则 |
| `etc_unknown_is_an_error_not_a_utc_fallback` | `crates/runtime/src/flows/publish/report_date.rs` | 运行时默认日期拒绝 `Etc/Unknown` |
| `etc_unknown_and_padded_names_fail_like_in_validate` | `crates/runtime/src/doctor/health.rs` | doctor 与 validate 一致 |
| `publish_category_never_claims_another_categorys_pending_record` | `crates/cli/tests/publish_cmd_tests.rs` | 命令层跨分类 record 隔离 |
| `sqlite_lease_predicates_compare_instants_not_rfc3339_text` | `crates/storage/tests/publish_stage_recovery_tests.rs` | SQLite lease 谓词按时刻（.5Z vs 整秒 now） |
| `sqlite_claim_lease_is_not_expired_before_its_instant` | 同上 | claim 写入的 lease 不提前过期 |
| `pg_lease_predicates_treat_subsecond_lease_as_live` | `crates/storage/tests/publish_record_pg_tests.rs` | PG 同语义 |
| `pg_claim_by_ids_skips_row_locked_by_another_transaction` | `crates/storage/tests/publish_record_pg_tests.rs` | PG 按 ID claim `SKIP LOCKED` 不阻塞 |
| `publish_store_local_failure_is_reported_as_failure` | 同上 | 本地写失败 → exit 1 |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/04-publish.md](../../plan/04-publish.md)
- 状态机：[../../plan/08-state-machines.md](../../plan/08-state-machines.md)（PublishState）
- 模板与 path：[../../plan/06-config.md](../../plan/06-config.md) §[publish.template]
- 决策：`../../adr/0003-publish-snapshot-immutable.md`、`../../adr/0008-per-category-path-template.md`
