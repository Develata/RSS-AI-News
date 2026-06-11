# 故障定位与排障

## 首选工具：`doctor`

任何"线上不对劲"先跑 doctor：

```bash
rss-ai-news doctor              # 浅层（config + DB + migrations + openai + github + rsshub + disk）
rss-ai-news doctor --deep       # 深层（跨表不变量 I1–I6 / I8 / I9）
rss-ai-news -o json doctor      # JSON 输出便于 grep / 管道
```

退出码：含 `Fail` → `1`（RuntimeError）；仅 `Warn` / `Ok` / `Info` → `0`。
详见 [../acceptance-cases/commands/doctor.md](../acceptance-cases/commands/doctor.md) 与
[../plan/07-observability.md](../plan/07-observability.md) §6。

## 常见故障

### 1. ingest 全失败 / 大量 source 5xx

排查：
1. `doctor` 看 `rsshub` / 网络 check
2. `grep` tracing 日志：`stage=ingest` + `severity=error`
3. 查 `feed_sources` 表 `last_status` / `last_error`
4. 查 `run_events` 表：`SELECT * FROM run_events WHERE run_id=? AND stage='ingest' AND severity='error'`

可能原因：
- RSSHub 实例挂了 → 切实例 / 临时禁用相关 source
- 代理 / 防火墙变化 → 检查 `HTTP_PROXY` / `HTTPS_PROXY` env
- User-Agent 被 ban → 改 `[http].user_agent`

### 2. extract 大量 fallback 或 failed

排查：
1. 找一个 article id，跑 `replay --kind html --key <link>` 离线复现
2. 若 Readability 抽不出，看页面结构是否新版（站点改版）
3. 看 `[extractor].min_body_chars` 是否过严

可能原因：
- 目标站点 JS 渲染（Readability 拿不到正文）→ 暂无方案（不内置浏览器）
- 站点改版 → 调整 strategy_order 或加自定义策略（需改代码）

### 3. ai-run 大量 PermanentFailed

排查：
1. `replay --kind ai --id <raw_artifact_id>` 看原始响应
2. 看 `article_ai_results.last_error` 字段
3. 看 prompt 是否兼容新模型（temperature / max_tokens 边界）

可能原因：
- 模型返回非 JSON / 格式偏移 → 调 prompt / 加 schema 校验后转 PermanentFailed
- `max_input_chars` 截断破坏关键上下文 → 调大或改 prompt 让模型在前文给结论
- 模型本身被替换且 API schema 变化 → 切回旧模型或加适配

### 4. publish 远端 422 lost-update

`publish_many` 有内置一次重试（详见 [../adr/0003-publish-snapshot-immutable.md](../adr/0003-publish-snapshot-immutable.md)
与 `crates/publish/tests/github_target_tests.rs`）。多次 422 通常是：

- 另一个 worker / 手工提交在写同一路径 → 检查 publish_records 是否并发
- GitHub branch protection rule 阻拦 → 检查 repo 设置

### 5. 启动报 `ConfigError`（exit 78）

```bash
rss-ai-news validate-config          # 一次性列出所有问题
```

常见：
- `OPENAI_API_KEY` 未设但 `[ai].enabled=true`
- `GITHUB_TOKEN` 未设但 `github_owner`+`repo` 非空（且未传 `--local-only`）
- `{RSSHUB}` 占位符存在但 `RSSHUB_BASE_URL` 未设
- `path_template` 含 `..` / 反斜杠 / 缺日期 token
- `report_template` 缺 `{items}` placeholder

### 6. 启动报 `UserError`（exit 2）

CLI 参数错（clap 解析失败）。看 stderr 的 clap 提示，按 `--help` 修。

### 7. `migrate check` 报版本不一致

```bash
rss-ai-news migrate check
```

可能原因：
- DB 是低版本 schema（旧部署没升级）→ `migrate run` 升
- 代码是旧 commit、DB 已新（回滚场景）→ 检查 git checkout 是否正确
- 双方言 migration 文件不对齐 → 看 CI 是否近期改过 migrations/

### 8. doctor `--deep` 报 I4 / I4'a / I6 违规

定义见 [../plan/00-overview.md](../plan/00-overview.md) §5 不变量。
违规通常源于：
- 手工改 DB（绕过状态机）
- 旧 bug 留下的脏数据
- backfill / reindex 中途崩溃

修复：通常需手工 SQL 修正 + 再 backfill / reindex。**先备份 DB**。

### 8a. doctor `--deep` 报 I9（预算耗尽的可领取行）

W15 起各 flow 启动期会自动 sweep 这类行转终态（事件 `retry_budget_swept`），
该检查常态应为绿；报违规说明对应 flow 自上次滞留后没再跑过。处置：跑一次对应
flow（ingest / ai-run / publish）让 sweep 收走，或等终态化后按恢复路径处理——
feed `failed` → `backfill --target extract`；AI `permanent_failed` →
`backfill --target ai`（新版本行）；publish `failed` → bump `render_version`。
详见 [../plan/15-retry-exhaustion-and-reclaim.md](../plan/15-retry-exhaustion-and-reclaim.md) §6。

### 9. 日志缺失 / 末尾被截断

- `--log-file` 模式下 `WorkerGuard` 没活到进程结束 → CLI 应已正确处理；若你 fork 了 cli/lib.rs 注意持有 guard
- supercronic 容器 OOM → 看宿主资源、调 `--memory` 限制
- 日志被红action 误杀 → 不会，红action 只动 URL userinfo / Bearer / JSON 键

## 查 run_events

`run_events` 是按 `run_id` × `stage` 索引的业务里程碑表（详见 [../plan/07-observability.md](../plan/07-observability.md) §5）。
没有内置查询子命令，直接 SQL：

```sql
SELECT created_at, severity, event_kind, message, context_json
FROM run_events
WHERE run_id = ?
ORDER BY id;
```

特定 stage：

```sql
WHERE run_id = ? AND stage = 'ai_run';
```

特定 target：

```sql
WHERE target_kind = 'article' AND target_id = ?;
```

## metrics 查询

`--metrics-bind 127.0.0.1:9090` 启动后 `curl http://127.0.0.1:9090/metrics`。

关注指标（命名 prefix `rss_ai_news_`）：
- `*_total{stage=...,category=...}` — 各段的成功 / 失败 / dedup 计数
- `*_duration_seconds_bucket{...}` — histogram
- `*_lease_held{...}` — gauge：当前持有的 lease 数

详见 [../plan/07-observability.md](../plan/07-observability.md) §4 metrics。

## 升级 / 回滚清单

升级：
1. `migrate check` —— 确认目标版本与当前是否兼容
2. 关 scheduler（避免升级窗口 cron 触发）
3. `migrate run`（如有变更）
4. 启新版镜像

回滚（仅 minor patch 间）：
1. 关 scheduler
2. 用旧版镜像启
3. 若 schema 变更**不**支持 down，先备份 DB 再 `migrate` down（注意 down 文件存在性）

> 当前不保证跨 minor 的向下兼容。回滚跨 minor 视为重大事件，写 handoff。

## 相关文档

- 设计：[../plan/11-error-and-recovery.md](../plan/11-error-and-recovery.md) 错误模型
- 设计：[../plan/07-observability.md](../plan/07-observability.md) 可观测性
- 子命令验收：[../acceptance-cases/commands/doctor.md](../acceptance-cases/commands/doctor.md)
- PG 切换：[./postgres-deployment.md](./postgres-deployment.md)
- 本地开发：[./local-dev.md](./local-dev.md)
