# 04 — 发布段

本章详解主链路第四段：从 `articles.state=ready_for_publish` 到 `articles.state=published`。

```text
ready_for_publish (articles)
  → freeze:  INSERT publish_records (pending → snapshot_frozen) + 拷贝候选到 publish_items
  → render:  Markdown + frontmatter（无持久副作用）
  → store:   写 <local_output_dir>/<path_template 渲染结果>（本地 target）
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
- 发布快照不可变契约 → [../adr/0003-publish-snapshot-immutable.md](../adr/0003-publish-snapshot-immutable.md)

## 2. 选稿与冻结

由 `PublishFlow::freeze()` 在 [`crates/runtime/src/flows/publish/freeze.rs`](../../crates/runtime/src/flows/publish/freeze.rs) 编排。

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

**快照归属（2026-11-21，[ADR 0012](../adr/0012-per-stage-publish-budget-and-snapshot-ownership.md)）**：
一篇文章同一时刻至多属于一份**活动快照**——即 `publish_items` 中出现该文章、且所属 `publish_records.state <> 'failed'`
的记录（`published_*` 记录的文章已是 `published`，自然不再入选）。三条选稿 SQL（AI 路径；AI-off 的 SQLite / PG 两版）
都排除已被活动快照持有的文章；记录转 `failed` 即释放其文章，下一份报告可重新选入。
AI-off 下，未被持有的 `ready_for_publish` 遗留文章（先前快照失败后留下）照常入选、保持原状态冻结，
与待升格的 `persisted` 文章区分处理，不会因此整批回滚。

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

实际事务顺序（`freeze_snapshot`，两方言一致）：① 带 owner/state 守卫的 record → `snapshot_frozen`（首条语句即写，
SQLite 由此持有写锁、串行化并发 freeze）；② PG 按 id 顺序 `SELECT … FOR UPDATE` 锁住快照内的文章（并发 freeze 同一文章时后者等待，
随后在 READ COMMITTED 下看见前者已提交的 items；固定顺序避免死锁；PG freeze 事务首条语句显式
`SET TRANSACTION ISOLATION LEVEL READ COMMITTED`，不依赖数据库 / 角色默认隔离级别）；③ 事务内复查归属，被其它活动快照持有 →
回滚，`ArticleOwnershipConflict`；④ AI-off 文章：`persisted` → `ready_for_publish`，已是 `ready_for_publish` 则保持，
其它状态 → 回滚，`ArticleStateConflict`；⑤ 写 publish_items、提交。
冲突时 runtime 立即释放 lease（`release_freeze_conflict`：记录回 `pending`、两个计数都退还、写 `last_error_kind`
= `article_owned_by_active_snapshot` / `article_state_conflict`），不发生任何外部写入；本次 freeze 阶段结果为
`ArticleConflict`（exit 1），下一次运行重新选稿时不再包含该文章。

存量数据（0008 之前已有两份未失败快照持有同一文章）由写前拦截处理：store_local 与 remote（单条 / 批量）在任何外部写入前
查询本快照是否与**胜出**的未失败记录（已 `published_*` 者优先，否则 id 较小者）共享文章；败方不写、释放 lease、退还 claim、
保持阶段态，`ArticleConflict`（`last_error_kind = article_owned_by_active_snapshot`），批量中只剔除败方。见 ADR 0012 §2。

**冻结契约**：`publish_items.frozen_*` 列**只读不改**。即使 article 后续被 backfill 改了 AI 结果，
已冻结的 PublishItem 仍引用旧值。

### 2.3 幂等

报告日期：`--date` 显式给定时按给定值；省略时 `publish` / `publish-all` 取本次运行时刻（每次运行采样一次）在
**有效目标时区**（`--timezone`，否则 `[publish].target_timezone`）下的日历日期，而不是 UTC 日期
（`runtime::flows::publish::default_report_date`）。例：`Asia/Shanghai` 下 2026-11-21 00:30 运行 → `2026-11-21`。
时区须为 IANA 时区库中存在的名称（jiff 查询：有系统 zoneinfo 用系统库，否则用内置库；validate-config、doctor 与运行时同一查询）；
未知时区在任何写入之前报错，绝不回退到 UTC。

唯一约束 `(category_key, report_date, render_version)`。重复 freeze 同一组合返回
`PublishInitOutcome::AlreadyExists`。

`--force`（`publish` / `publish-all`）每次调用都新建一条 `kind='render'` 的 trace 行（tag 含本次 run id，
同一秒内重复 force 也互不复用），状态恒为 `pending`（ADR 0004 的 partial unique 之外），因此产生新的
render_version / 新记录，且**不会**成为 active——新库上首次 force 之后，普通发布仍使用 `default` render 规则。

非 force 时，publish / publish-all 用的 render 与 selection_policy 版本必须是该 kind 当前 **active** 的行（无任何行时登记
`default` 为 active）。若 `default` 行已 `superseded` 且没有 active 行，引导在建任何记录之前报错（`RuntimeError::Config`），
不再按 tag 回读并盖上一个已 superseded 的版本（R1-P-S6；否则按日期 rebuild-report 随后会失败）。

## 3. Markdown 渲染

`crates/report/src/render.rs` 的 `render_markdown(publish_record_id, category_key, report_date, frozen_items,
&RenderConfig)` 实现：**无持久副作用**的纯函数，只读冻结快照与 `[publish.template]` 四个模板
（`path_template` / `frontmatter_template` / `report_template` / `item_template`），按 `{name}` 占位符替换。
各模板允许的占位符由 `validate-config` 白名单校验（`crates/config/src/validate/checks.rs`），未知占位符 → 配置错误：

| 模板 | 允许的占位符 |
|---|---|
| `path_template` | `{category_key}` `{CATEGORY_KEY}` `{date}` `{YYYY}` `{MM}` `{DD}` `{YYYYMMDD}` |
| `frontmatter_template` | `{title}` `{title_yaml}` `{date}` `{YYYY}` `{MM}` `{DD}` `{YYYYMMDD}` `{excerpt}` `{excerpt_yaml}` |
| `report_template` | `{frontmatter}` `{title}` `{title_md}` `{date}` `{YYYY}` `{MM}` `{DD}` `{YYYYMMDD}` `{category_key}` `{CATEGORY_KEY}` `{category_display_name}` `{category_display_name_md}` `{excerpt}` `{excerpt_yaml}` `{excerpt_block}` `{items}` `{generated_at}` |
| `item_template` | `{item_title}` `{item_title_md}` `{score}` `{score_badge}` `{tags}` `{tags_block}` `{source}` `{source_md}` `{source_code}` `{url}` `{url_md}` `{summary}` `{summary_inline}` `{summary_blockquote}` |

`{date}` 是报告日期 `YYYY-MM-DD`；`{CATEGORY_KEY}` 为分类 key 的大写。`report_template` 必须含 `{items}`；
`item_template` 必须含 `{item_title_md}` 或 `{item_title}`，以及一个摘要占位符。

### 3.1 frontmatter

frontmatter 由 `frontmatter_template` 渲染后填入 `report_template` 的 `{frontmatter}`。默认模板：

```yaml
---
title: {date}
date: {date}
excerpt: {excerpt_yaml}
---
```

`{generated_at}` 只在 `report_template` 中可用（ISO 8601，例如 `2026-06-04T12:34:56.123456000Z`）。
`generated_at` 来自 `publish_records.rendered_at`：render 阶段取**一个**时刻（UTC，截断到整微秒——PostgreSQL
`timestamptz` 与 SQLite 都能原样往返的精度），既渲染进报告、又写入 `rendered_at`；store_local、remote 重试
（单条与批量）与 rebuild-report 一律从记录读回该值重渲染（调用方传入的 `generated_at` 只在记录尚无
`rendered_at` 时兜底；rebuild 的显式 override 优先），保证含 `{generated_at}` 的模板也 byte-equal。

### 3.2 正文格式

每个冻结的 PublishItem 用 `item_template` 渲染后按顺序拼入 `{items}`。默认模板：

```markdown
## {item_title_md}{score_badge}

{tags_block}- **Source:** `{source_code}` | [阅读原文]({url_md})

> [摘要]
{summary_blockquote}

---

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

路径模板由配置决定（相对 `[publish].local_output_dir`；GitHub target 另加 `github_path_prefix`）：

```toml
# app.toml
[publish.template]
path_template = "{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md"  # 全局默认，必须含 {category_key} 或 {CATEGORY_KEY}
```

v0.3 引入**分类级覆盖**：

```toml
# categories/ai.toml
[category.publish_override]
path_template = "zh-news/{category_key}/{YYYY}/{YYYYMMDD}.md"  # 覆盖全局；可省略分类占位符
```

`EffectiveConfig.path_template` 合并：category override > global fallback。
详见 [./06-config.md](./06-config.md) + [../adr/0008-per-category-path-template.md](../adr/0008-per-category-path-template.md)。

支持的占位符：`{category_key}` / `{CATEGORY_KEY}` / `{date}` / `{YYYY}` / `{MM}` / `{DD}` / `{YYYYMMDD}`；
必须含日期占位符（`{date}`、`{YYYYMMDD}`，或 `{YYYY}` + `{MM}` + `{DD}`），不得含 `..` / `\` / 绝对路径。

写入语义（2026-11-21）：先在目标目录内写隐藏临时文件并 fsync，再 `rename` 覆盖目标（Unix 上随后 fsync 目录），
中断不会留下截断或半份报告。临时文件名（2026-11-22，R1-P-S8）与报告文件名无关、长度固定且短
（`.rss-ai-news.<pid>.<纳秒>.<计数>.tmp`，十六进制），接近 255 字节上限的报告名也能发布；`create_new` 遇到已存在的同名文件
视为他人所有，不写、不删，换名重试（最多 8 次，仍冲突则 `LocalIoError`）；只有本次调用创建成功的临时文件才会在写入失败时被删除。路径逐级在输出根（canonical）之下解析：既有符号链接仅在其 canonical 目标仍位于
输出根内时才跟随；越界的父目录链接或目标链接 → `PublishError::InvalidPath`，根外不创建任何目录或文件。

### 4.1.1 跨分类碰撞检测

`validate-config` 校验：所有分类的 effective path_template 渲染后**不**应碰撞。
碰撞示例：两个分类的 override 都写 `docs/news/{date}.md`（缺 category 占位符），产物互相覆盖。

碰撞 → `Diagnostic`，`validate-config` exit 78。

validate 只用一个样例日期（`2026-01-03`，ADR 0008）渲染，无法发现只在部分日期碰撞的组合
（如 `{YYYY}/{MM}/{DD}.md` 与 `{YYYY}/{DD}/{MM}.md` 在月 = 日时同路径）。因此 `publish-all`（含 `run` 的发布段）
在 preflight 里、即独立 `publish-all` **打开存储之前**（打开存储会跑迁移并轮换 active config 行，R1-P-S7），
也就在任何写入（迁移、config 轮换、rule_version 登记、init、落盘）之前，确定一次报告日期（同一值传给后续执行），
用发布渲染器按**本次报告日期**渲染全部所选分类的真实路径，
归一化（`\` → `/`、去掉空段与 `.` 段）后查重；有碰撞 → `RuntimeError::Config`（列出路径与分类），exit 1。
实现：[`crates/runtime/src/flows/publish/paths.rs`](../../crates/runtime/src/flows/publish/paths.rs)。

碰撞规则（2026-11-22，R1-P-B5；validate 与 publish-all 共用 `config::validate::report_path_collisions`）：
按段比较，且**忽略大小写**（Unicode 小写折叠）——`News/{date}.md` 与 `news/{date}.md` 在 macOS / Windows 默认的
大小写不敏感卷上是同一文件；一个路径是另一个路径的**整段前缀**（`news/2026-01-03` 与 `news/2026-01-03/ml.md`）也算碰撞，
一个分类要写文件的位置另一个分类需要目录。Git tree 路径本身区分大小写，但每份报告都先落本地，因此统一用更严格的规则。
根内目录符号链接造成的别名不在此检查之列。

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

ref 路由（`GET git/ref/heads/{branch}`、`PATCH git/refs/heads/{branch}`）中的分支名与 Contents API 路径同规则编码：按 `/` 分段、
每段一次 RFC 3986 百分号编码，保留层级 `/`（`release#daily/50%` → `release%23daily/50%25`；`release/2026` 不变）。
未编码时 `#` 之后会被 URI 解析截掉，批量发布会读写另一个分支（R1-P-B3）。回执里的 `remote_target` 仍用逻辑分支名。

单报告（`publish` 单分类，或 `publish_many` 只有一份报告时）走 Contents API：
`GET /repos/{owner}/{repo}/contents/{path}?ref={branch}` 取既有 sha，再 `PUT /repos/{owner}/{repo}/contents/{path}`
（更新时带 sha）。`{path}` 按 `/` 分段、每段以 UTF-8 做一次 RFC 3986 百分号编码（仅 unreserved
`A-Z a-z 0-9 - . _ ~` 保留字面），分隔符 `/` 保留；`ref` 作为编码后的查询参数（`release/2026` → `release%2F2026`）。
记录与产物里的路径（`remote_target`、Git tree JSON 的 `path`、输出文件名）仍是逻辑路径，不编码。
来由：2026-11-21 中文路径 `科技新闻/…` 的单报告发布以 HTTP 400 失败（请求行含字面 UTF-8）。

单报告幂等（2026-11-22，R1-P-S2）：`GET contents` 取到的既有 blob sha 等于本地内容的 git blob sha 时，先 `GET ref` 取 HEAD
commit、按该 commit 复核同一路径；一致 → 以 HEAD commit 记回执，不发 PUT。批量发布中途中断（第一条回执已落库）后重跑只剩
一条报告、走单报告路径时，因此不再重复提交、也不需要写权限。

GitHub 调用失败时，`PublishError::GitHubApiError` / `GitHubAuthFailed` 的消息带上失败的操作
（`GET contents` / `PUT contents` / `GET ref` / `GET commit` / `POST tree` / `POST commit` / `PATCH ref`）、
HTTP 状态、GitHub message 的**固定词表摘要**与 `x-github-request-id`（若有），并进入 `last_error` 与
`publish_failed` 事件；不含凭据与报告正文。分类（auth / rate limit / 422 fast-forward）在附加上下文前完成，不受影响。

持久化诊断的保密规则（2026-11-22，R1-P-B6）：GitHub 的 `message`（以及无法解码的错误正文）是服务端自由文本，可能回显请求里的
报告片段、其 Base64 或凭据，因此只在内存中整段扫描已知短语（`rate limit`、`fast forward`、`bad credentials`、`not found`、
`validation failed`、`bad request` 等，大小写不敏感），持久化的只是对应的固定措辞，多个以 `; ` 连接；一个都不认识时写
`unrecognized GitHub message omitted (<N> bytes)`；无法解码的正文前缀 `unreadable error body: `。重试分类依赖的信号
（`not a fast-forward`、`rate limit`）属于该词表，且在任何截断之前从完整文本中提取。`x-github-request-id` 只有整值符合
GitHub 形状（2–8 组、每组 1–16 位十六进制、以 `:` 分隔，实测形如 `828A:4A44E:40DA1B1:CEE6757:6B01CE59`）且不含 token
时才保留，否则整值丢弃（不再"过滤成合法字符"）。客户端侧的传输错误文本（无服务端内容）最多 512 字节。

结构化请求上下文（2026-11-22，R1-P-S1）：`GitHubAuthFailed` / `GitHubApiError` / `GitHubRateLimit` / `ResponseTooLarge`
都带 `GitHubRequestContext { operation, http_status, request_id }`（`PublishError::github_context()`）：401 与无限流信号的
403 由 `http_status` 区分；403 / 429 限流、超过 16 MiB 的成功响应、缺必需字段（`object.sha` / `tree.sha` / `sha` /
`commit.sha`）的 2xx 也保留操作、实际状态与 request id（缺字段仍按 502 协议错误分类，显示 `(http 201)` 之类的实际状态）。
Contents API 200 但无字符串 `sha`（如目录列表）是协议错误，不当作"文件不存在"。runtime 的 `publish_failed` 事件 payload
增加 `operation` / `http_status` / `request_id` 三个字段。

### 4.2.1 Atomic batch publish

v0.3 引入：`publish_many(reports: &[RenderedReport])` 在一次 commit 内推送多个 report。
减少 commit 噪音 + 节省 API 配额。

**重试幂等（2026-11-21，ADR 0012）**：取到 HEAD commit 后、建 tree/commit 之前，按该 commit sha（不是分支名）
对批内每个路径 `GET contents?ref=<head commit>`，与本地按 git 规则（SHA-1 over `blob <len>\0` + 内容）算出的 blob sha 比对。
**全部**路径都存在且 blob 一致 → 视为上一次尝试的 ref 更新已落地（响应丢失），直接以该 HEAD commit 记回执（每份报告一条、按输入顺序），
不再新建 commit；任何一个路径缺失 / 不一致 → 照常建 commit；查询出错 → 本次失败（按原分类重试），绝不当作"已发布"。
单报告走 Contents API，其幂等见 §4.2 上文"单报告幂等"。

**歧义写入的只读核对（2026-11-22，ADR 0012 §3）**：`PATCH ref`（批量）或 `PUT contents`（单报告）发出后遇到传输失败 /
响应不可读 / 5xx，或 runtime 的远端 deadline 切断了调用时，释放前先 `GET ref` 取 HEAD commit、按该 commit 比对全部 blob；
一致 → 以 HEAD commit 记成功；否则保留原错误按分类释放。4xx 与限流不核对。

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
rss-ai-news rebuild-report --publish-id <id> [--output <PATH>]
rss-ai-news --category <KEY> rebuild-report --date <YYYY-MM-DD> [--output <PATH>]
```

读 `publish_records` + `publish_items` 冷列，重新走 `render_markdown` + `frontmatter_builder`；
`--output` 写文件，省略时 pretty 模式 stdout 为同样的 Markdown 字节（不补末尾换行；JSON 模式放入
`summary.markdown`）。在**相同 snapshot、模板、render config、generated_at 与渲染器构建**下与原始发布的
Markdown **byte-equal**；跨渲染器版本（例如 YAML 引号规则调整）不作字节保证，只保证解析后的值不变或被修正。

测试锁定：[`crates/runtime/tests/rebuild_report_tests.rs`](../../crates/runtime/tests/rebuild_report_tests.rs)
中的 `rebuild_returns_byte_equal_markdown_to_original_render` 与
`rebuild_without_generated_at_override_falls_back_to_record_rendered_at_and_matches_original`。

这是发布快照不可变契约的最强验证：上述输入都不变时，重渲染产物必字节一致。

## 7. 失败路径速查

| 失败点 | 错误变体 | 处理 |
|---|---|---|
| freeze 时 UNIQUE 冲突 | `PublishError::AlreadyExists` | 返 idempotent outcome，不视为失败 |
| 阶段内读库 / 冻结写入失败 | `ReportError::Storage` / `StorageError`（保留类型） | 按 `is_retryable()`：可重试 → 维持阶段态 + 计预算（耗尽 → `failed`）；否则 `failed` |
| 本地落盘失败 | `PublishError::LocalStoreFailed` | state='failed'，无 retry |
| GitHub 401；无限流信号的 403 | `PublishError::GitHubAuthFailed` | state='failed' |
| GitHub 429；带限流信号的 403（`x-ratelimit-remaining: 0`、存在 `retry-after`，或 message 含 "rate limit" / "secondary rate limit"，见 GitHub REST 文档 "Exceeding the rate limit"） | `PublishError::GitHubRateLimit { reset_at }`（`retry-after` > `x-ratelimit-reset` > now+60s） | retryable：维持阶段态 + 计预算 |
| GitHub 422 lost-update | `is_branch_concurrently_updated` | retry 1 次，仍失败 → state='failed' |
| GitHub 5xx | `PublishError::GitHubApiError {5xx}` | state='failed'，本地文件保留 |
| GitHub timeout | `PublishError::GitHubApiError` | state='failed' |
| 目标漂移：record 的 `remote_target`（init 时写入的 `owner/repo:branch`）≠ 当前配置目标 | `PublishError::RemoteTargetMismatch`（`PublishRemoteStatus::TargetMismatch { stored, current }`） | 发请求前拦截，不重试，state='failed'，`last_error` 含两个目标；恢复配置或 `--force` 由运维决定 |

**本地落盘成功 + GitHub 失败**：record 转 `failed`，但本地文件保留。下次 publish 触发同
`(category, date, version)` 走 idempotent 路径。

## 8. 当前实现入口

| 内容 | 路径 |
|---|---|
| Publish Flow | [`crates/runtime/src/flows/publish/mod.rs`](../../crates/runtime/src/flows/publish/mod.rs) |
| 发布规则版本登记（render / selection_policy，publish 与 publish-all 共用） | [`crates/runtime/src/flows/publish/rule_versions.rs`](../../crates/runtime/src/flows/publish/rule_versions.rs) |
| publish_all CLI（atomic batch） | [`crates/cli/src/commands/publish_all.rs`](../../crates/cli/src/commands/publish_all.rs) |
| PublishTarget trait | [`crates/publish/src/target.rs`](../../crates/publish/src/target.rs) |
| LocalFsTarget | [`crates/publish/src/local.rs`](../../crates/publish/src/local.rs) |
| GithubTarget + 422 retry | [`crates/publish/src/github.rs`](../../crates/publish/src/github.rs) |
| Markdown renderer | [`crates/report/src/`](../../crates/report/src/) |
| PublishItemRepository | [`crates/storage/src/repo/publish_item.rs`](../../crates/storage/src/repo/publish_item.rs) |
| rebuild-report 入口 | [`crates/runtime/src/flows/rebuild_report.rs`](../../crates/runtime/src/flows/rebuild_report.rs) |
| 集成测试 | [`crates/runtime/tests/publish_freeze_tests.rs`](../../crates/runtime/tests/publish_freeze_tests.rs) + [`crates/runtime/tests/rebuild_report_tests.rs`](../../crates/runtime/tests/rebuild_report_tests.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。

## 远端发布资源边界（2026-09-10）

remote 使用 claim 返回的记录元信息，只加载一次 frozen items，同时用于渲染与 article id 推进，
不回读当前文章/AI正文，不破坏冻结快照不可变性。批量报告和推进元信息分开持有，不再复制整批 Markdown。
从 claim 前起计算共享 deadline = publish lease 的80%；准备阶段（started事件与快照读取）和远端调用均受此限制，
超时按 retryable 释放，预留20%用于数据库收尾。claim与收尾写入沿用数据库自身的失败/恢复机制，
不因deadline主动取消；数据库不可用时仍须依赖lease回收，不能保证整个函数必在lease内返回。
同步渲染不能被 Tokio 抢占；preparation 完成后重新检查 deadline，超时返回 retryable timeout。
单条/批量调用远端前也各自检查 deadline，已超预算不发起远端调用。
网络取消不能撤销服务端已经完成的提交，仍依靠既有幂等发布与重试恢复。
Octocrab connect/read/write timeout 为5/30/30秒，所有 GitHub 响应最多16 MiB（含 base64 Contents 响应）。
成功响应超限不可重试；429/5xx 错误页超限仍保留对应 retry 分类。
GitHub错误消息先解析JSON，再只输出固定词表摘要（见 §4.2，R1-P-B6，取代此前"遮蔽 token + 8 KiB 截断"的做法），
因此 JSON 转义、回显的报告片段与凭据都不会进入诊断；422 的 fast-forward 冲突分类信号来自完整文本。
合法 JSON 缺少字符串 `message` 时仅输出状态说明，不回显未知结构中的键或嵌套值。
JSON 解析失败（含深度超限、截断及纯文本）时同样只输出固定词表摘要，避免未解码转义泄漏凭据。
