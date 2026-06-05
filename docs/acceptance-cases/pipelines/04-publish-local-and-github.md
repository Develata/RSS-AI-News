# AC-P-04: Publish 本地 + GitHub 发布流水线

## 功能描述

按 `(category, report_date)` 维度组装一份发布报告：init → snapshot freeze → render → store-local → publish-remote 五阶段。
每阶段独立 lease 推进 `PublishState` 状态机，snapshot 冻结后保证 `rebuild-report` 可字节相等重建。

面向场景：每日定时触发 `publish` 子命令，或 `run` 的最终段；可 `publish-all` 一次跑全部分类。

## 验收标准

### 命中条件（success path）

- **init**：基于 `(category_key, report_date)` 幂等键创建/复用 `publish_records.Pending` 行
- **freeze**：将命中 `[publish].min_importance_score` × `include_unscored` × `candidate_window_hours` 的 article 冻结为 `publish_items`；
  - AI 路径：选 `ArticleState=ReadyForPublish`
  - AI-off 直通：`Persisted` article 在同 tx 内提升为候选（无需 ai_result）
  - 候选为空 → 返回 `SnapshotEmpty`（不算失败）
- **render**：snapshot_frozen → rendered；空 items 失败
- **store-local**：写本地文件 + 推进 article 到 `Published`（无远端目标时）/ 保持文章不动（有远端目标待 push）；
  - 分类 `path_template` override 生效
- **publish-remote**：批量提交一个 commit 覆盖多 publish_records；422 lost-update 自动重试一次；429 保持状态可重试
- **path 防穿越**：含 `..` / 反斜杠 / 无日期 token 的模板在 validate 阶段就报错（详见 [./06-config-loading.md](./06-config-loading.md)）
- 远端 publish 成功后 article → `Published`；批次成功后 record → `PublishedRemote`

### 失败条件（failure path）

- store-local 目录不可写 → record → `Failed`，errors 表登记
- 远端 401 → `GithubAuthFailed`（终态，不重试）
- 远端 429 → `GithubRateLimit`，state 保持、article 不晋升
- 远端 422 重试达上限 → 仍报错，record 保持 `StoredLocal`
- promote target article 已被其它流转推进 → `ArticleConflict`

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `init_creates_publish_record_returns_created_outcome` | `crates/runtime/tests/publish_freeze_tests.rs` | init happy |
| `init_returns_already_exists_on_idempotency_key_conflict` | 同上 | 幂等键 |
| `freeze_with_ai_path_inserts_publish_items_and_advances_record_to_snapshot_frozen` | 同上 | AI 路径 freeze |
| `freeze_record_claims_requested_pending_record_not_older_one` | 同上 | claim 指定 record |
| `freeze_record_isolates_two_concurrent_pending_records_by_id` | 同上 | 并发隔离 |
| `freeze_with_ai_off_passthrough_promotes_persisted_articles_in_same_tx` | 同上 | AI-off 直通 |
| `freeze_returns_snapshot_empty_when_no_candidates_match` | 同上 | SnapshotEmpty |
| `freeze_returns_nothing_to_claim_when_no_pending_records` | 同上 | 空闲 |
| `freeze_skips_articles_without_correct_category_key` | 同上 | category 隔离 |
| `render_advances_snapshot_frozen_to_rendered_when_items_exist` | `crates/runtime/tests/publish_render_tests.rs` | render happy |
| `render_returns_failed_when_publish_record_has_no_items` | 同上 | 空 items 失败 |
| `store_local_with_no_remote_target_publishes_locally_and_promotes_articles` | `crates/runtime/tests/publish_store_local_tests.rs` | 本地 + promote |
| `store_local_uses_category_path_template_override` | 同上 | 分类 path override |
| `store_local_with_remote_target_advances_to_stored_local_without_promoting_articles` | 同上 | 有远端不 promote |
| `store_local_returns_article_conflict_when_promote_target_already_advanced` | 同上 | ArticleConflict |
| `store_local_returns_failed_with_local_io_error_when_target_dir_unwritable` | 同上 | 不可写 |
| `store_local_retryable_failure_keeps_rendered_state_and_reclaim_succeeds` | 同上 | 可重试保态 |
| `publish_remote_succeeds_promotes_articles` | `crates/runtime/tests/publish_remote_tests.rs` | 远端 happy + promote |
| `publish_remote_batch_succeeds_with_one_commit_for_multiple_records` | 同上 | 多 record 单 commit |
| `publish_remote_rate_limit_keeps_state_and_articles` | 同上 | 429 |
| `publish_remote_auth_failed_is_terminal_without_promoting_articles` | 同上 | 401 终态 |
| `publish_many_creates_one_commit_for_multiple_reports` | `crates/publish/tests/github_target_tests.rs` | target 层 batch |
| `publish_many_retries_once_after_non_fast_forward_then_succeeds` | 同上 | 422 重试 |
| `publish_many_surfaces_422_after_max_retries` | 同上 | 422 超限 |
| `auth_failure_maps_to_github_auth_failed` | 同上 | 401 映射 |
| `rate_limit_maps_to_github_rate_limit` | 同上 | 429 映射 |
| `local_fs_target_rejects_path_with_parent_traversal` | `crates/publish/tests/local_target_tests.rs` | 路径穿越拒绝 |
| `local_fs_target_creates_parent_directories` | 同上 | 父目录自建 |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/04-publish.md](../../plan/04-publish.md)
- 状态机：[../../plan/08-state-machines.md](../../plan/08-state-machines.md)（PublishState）
- 模板与 path：[../../plan/06-config.md](../../plan/06-config.md) §[publish.template]
- 决策：`../../adr/0003-publish-snapshot-immutable.md`、`../../adr/0008-per-category-path-template.md`
