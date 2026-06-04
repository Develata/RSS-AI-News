# 04 — 发布段

本章详解主链路第四段：从 `articles.state=ready_for_publish` 到 `articles.state=published`。

```text
ready_for_publish (articles)
  → freeze:  INSERT publish_records (pending → snapshot_frozen) + 拷贝候选到 publish_items
  → render:  Markdown + frontmatter（无持久副作用）
  → store:   写 docs/news/<category>/<date>.md（本地 target）
  → push:    GitHub Git Data API（atomic batch + 422 retry）
  → published_local / published_remote
```

## 1. 边界

本章覆盖：
- 选稿与冻结快照（`PublishRecord` + `PublishItem`）
- Markdown 渲染（含 frontmatter）
- 本地 fs target + GitHub target
- per-category path_template 覆盖（v0.3）
- atomic batch publish（v0.3）+ 422 lost-update retry
- `rebuild-report` 子命令（byte-equal 重渲染）

**不覆盖**：
- 选稿评分逻辑（在 AI 阶段判定 `articles.state=ready_for_publish` 时已决）→ [./03-ai.md](./03-ai.md)
- 发布快照不可变契约 → [../adr/0003-publish-snapshot-immutable.md](../adr/0003-publish-snapshot-immutable.md)（建设中）

## 2. 选稿与冻结

由 `PublishFlow::freeze()` 在 [`crates/runtime/src/flows/publish.rs`](../../crates/runtime/src/flows/publish.rs) 编排。

### 2.1 候选选取

事务内：

```sql
SELECT * FROM articles
WHERE category_key = ?
  AND state = 'ready_for_publish'
  AND published_at >= ?  -- candidate_window_days
ORDER BY importance_score DESC NULLS LAST, published_at DESC
LIMIT max_items_per_report;
```

AI 关闭模式（`ai.enabled=false` + `include_unscored=true`）下还包含 `state='persisted'` 行，
同事务升格到 `ready_for_publish`。

### 2.2 冻结：PublishRecord + PublishItem

```sql
-- 1. 创建 record
INSERT INTO publish_records (id, category_key, report_date, render_version, state, ...)
VALUES (?, ?, ?, ?, 'pending', ...);

-- 2. 拷贝候选到 publish_items（冷列冻结）
INSERT INTO publish_items (publish_record_id, article_id, frozen_title, frozen_summary,
                            frozen_tags_json, frozen_score, frozen_link, ...)
SELECT ...
FROM articles JOIN article_ai_results ...
WHERE article_id IN (...);

-- 3. 推进 record 状态
UPDATE publish_records SET state='snapshot_frozen' WHERE id=?;
```

**冻结契约**：`publish_items.frozen_*` 列**只读不改**。即使 article 后续被 backfill 改了 AI 结果，
已冻结的 PublishItem 仍引用旧值。

### 2.3 幂等

唯一约束 `(category_key, report_date, render_version)`。重复 freeze 同一组合返回
`PublishInitOutcome::AlreadyExists`。

## 3. Markdown 渲染

`crates/report/src/` 实现。**无持久副作用**，纯函数式：

```rust
fn render_markdown(record: &PublishRecord, items: &[PublishItem]) -> String;
```

### 3.1 frontmatter

YAML 头：

```yaml
---
title: AI 日报 2026-06-04
date: 2026-06-04
category: ai
render_version: v1
item_count: 12
generated_at: 2026-06-04T12:34:56Z
---
```

`generated_at` 来自 `publish_records.rendered_at`（首次 render 时写入；rebuild-report 重渲染时
作为输入参数，保证 byte-equal）。

### 3.2 正文格式

每个 PublishItem 渲染为：

```markdown
## [{frozen_title}]({frozen_link})

**评分**: {frozen_score} | **标签**: {frozen_tags_json.join(", ")}

{frozen_summary}

> 原文：[{frozen_link}]({frozen_link})
```

## 4. PublishTarget

`PublishTarget` trait 在 [`crates/publish/src/target.rs`](../../crates/publish/src/target.rs) 定义。
两个生产实现：

| 实现 | 用途 | 代码 |
|---|---|---|
| `LocalFsTarget` | 本地 fs 写文件 | [`crates/publish/src/local.rs`](../../crates/publish/src/local.rs) |
| `GithubTarget` | GitHub Git Data API | [`crates/publish/src/github.rs`](../../crates/publish/src/github.rs) |

CLI `publish` 子命令同时调用两者（本地必发，GitHub 可选）。

### 4.1 LocalFsTarget

路径模板由配置决定：

```toml
[publish]
path_template = "docs/news/{category_key}/{report_date}.md"  # 全局
```

v0.3 引入**分类级覆盖**：

```toml
# categories/ai.toml
[publish]
path_template = "docs/zh-news/{category_key}/{report_date}.md"  # 覆盖全局
```

`EffectiveConfig.path_template` 合并：category override > global fallback。
详见 [./06-config.md](./06-config.md) + [../adr/0008-per-category-path-template.md](../adr/0008-per-category-path-template.md)（建设中）。

支持的占位符：`{category_key}` / `{CATEGORY_KEY}` / `{report_date}` / `{render_version}`。

### 4.1.1 跨分类碰撞检测

`validate-config` 校验：所有分类的 effective path_template 渲染后**不**应碰撞。
碰撞示例：`docs/news/{report_date}.md`（缺 category 占位符）让 ai 和 ml 分类的产物互相覆盖。

碰撞 → `Diagnostic`，`validate-config` exit 78。

### 4.2 GithubTarget

走 GitHub Git Data API（Tree / Blob / Commit / Ref）：

```text
1. GET refs/heads/main → base SHA
2. GET commits/<base> → base tree SHA
3. POST blobs（每个新文件）→ blob SHA
4. POST trees（with base_tree=<base tree SHA> + 新 blobs）→ new tree SHA
5. POST commits（with tree=<new tree>, parents=[<base>]）→ new commit SHA
6. PATCH refs/heads/main → new commit SHA
```

这是一次提交多个文件的 atomic 操作。优于"循环 PUT contents API"。

### 4.2.1 Atomic batch publish

v0.3 引入：`publish_many(reports: &[RenderedReport])` 在一次 commit 内推送多个 report。
减少 commit 噪音 + 节省 API 配额。

### 4.2.2 422 lost-update retry

PATCH refs/heads 时如果 base SHA 不再是 HEAD（并发 push 了别的 commit），GitHub 返 422
`"Update is not a fast forward"`。

处理：
- 单次 retry：重新获取 base SHA，重新构造 tree/commit/ref
- MAX_ATTEMPTS=2，第二次仍失败 → 返 `PublishError::GitHubApiError`，状态转 `Failed`

`is_branch_concurrently_updated()` 用正则匹配 message lowercase 含 `"fast forward"` / `"not a fast-forward"`。

## 5. Publish Flow 完整路径

```text
1. freeze(category) → publish_record_id（state='snapshot_frozen'）
2. render(record_id) → markdown string（state='rendered'）
3. store_local(markdown) → 写文件（state='stored_local'）
   - 失败 → state='failed'
4. publish_remote(markdown) → GitHub atomic commit
   - 422 → retry 1 次
   - 成功 → state='published_remote'
   - 最终失败 → state='failed'（本地文件保留）
5. UPDATE articles SET state='published' WHERE id IN (...) AND state='ready_for_publish'
```

### 5.1 record-scoped claim（v0.3）

旧版本：claim 用 state-scan，多 record 并行时容易抢资源。
v0.3 改为 record-id-scoped claim（`claim_publish_by_ids(state, ids)`），把同一 record 的所有
items 一次性圈走，避免跨 record 互抢。

详见 commit `9080223` "Fix publish-all record-scoped claims"。

## 6. rebuild-report

```bash
rss-ai-news rebuild-report --publish-record-id <id>
```

读 `publish_records` + `publish_items` 冷列，重新走 `render_markdown` + `frontmatter_builder`，
输出与原始 commit 内的 Markdown **byte-equal**。

测试锁定：[`crates/runtime/tests/rebuild_report_tests.rs`](../../crates/runtime/tests/rebuild_report_tests.rs)
中的 `rebuild_returns_byte_equal_markdown_to_original_render` 与
`rebuild_without_generated_at_override_falls_back_to_record_rendered_at_and_matches_original`。

这是发布快照不可变契约的最强验证：只要冷列没动，重渲染产物必字节一致。

## 7. 失败路径速查

| 失败点 | 错误变体 | 处理 |
|---|---|---|
| freeze 时 UNIQUE 冲突 | `PublishError::AlreadyExists` | 返 idempotent outcome，不视为失败 |
| 本地落盘失败 | `PublishError::LocalStoreFailed` | state='failed'，无 retry |
| GitHub 401/403 | `PublishError::GitHubAuthFailed` | state='failed' |
| GitHub 422 lost-update | `is_branch_concurrently_updated` | retry 1 次，仍失败 → state='failed' |
| GitHub 5xx | `PublishError::GitHubApiError {5xx}` | state='failed'，本地文件保留 |
| GitHub timeout | `PublishError::GitHubApiError` | state='failed' |

**本地落盘成功 + GitHub 失败**：record 转 `failed`，但本地文件保留。下次 publish 触发同
`(category, date, version)` 走 idempotent 路径。

## 8. 当前实现入口

| 内容 | 路径 |
|---|---|
| Publish Flow | [`crates/runtime/src/flows/publish.rs`](../../crates/runtime/src/flows/publish.rs) |
| publish_all CLI（atomic batch） | [`crates/cli/src/commands/publish_all.rs`](../../crates/cli/src/commands/publish_all.rs) |
| PublishTarget trait | [`crates/publish/src/target.rs`](../../crates/publish/src/target.rs) |
| LocalFsTarget | [`crates/publish/src/local.rs`](../../crates/publish/src/local.rs) |
| GithubTarget + 422 retry | [`crates/publish/src/github.rs`](../../crates/publish/src/github.rs) |
| Markdown renderer | [`crates/report/src/`](../../crates/report/src/) |
| PublishItemRepository | [`crates/storage/src/repo/publish_item.rs`](../../crates/storage/src/repo/publish_item.rs) |
| rebuild-report 入口 | [`crates/runtime/src/flows/rebuild_report.rs`](../../crates/runtime/src/flows/rebuild_report.rs) |
| 集成测试 | [`crates/runtime/tests/publish_freeze_tests.rs`](../../crates/runtime/tests/publish_freeze_tests.rs) + [`crates/runtime/tests/rebuild_report_tests.rs`](../../crates/runtime/tests/rebuild_report_tests.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md)（建设中）登记漂移。
