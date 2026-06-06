# ADR 0003: 发布快照不可变

- 日期：2026-03（W4 期间确立）
- 状态：`accepted`
- 决策者：项目主作者

## Context

发布段的核心动作是把一组 article 渲染为一份 Markdown 报告并写到本地 / GitHub。
设计上存在两个矛盾的诉求：

1. **重渲染**应当幂等可重做（模板修复、字节相等回归、报告补发）
2. **正在被渲染的 article 集合**可能在 ai-run / extract / backfill 等并发流程中变化

如果"渲染"直接 SQL 查 articles 表，每次重建结果可能不同（因为新 article 已入库 /
ai-result 已更新），失去字节相等保证；如果"渲染"用进程内副本，副本生命周期与
[[ADR-0001]] 的 single-shot 模型不匹配。

候选方案：
- (a) 不冻结，render 每次直接查实时数据 → rebuild 不可保证字节相等
- (b) **冻结快照**：freeze 阶段把入选的 article 字段复制进 `publish_items.frozen_*`，
     之后 render 与 rebuild 只读 frozen 列
- (c) 用 PG 的 `SERIALIZABLE` / 时间旅行特性 → 强依赖方言，与 [[ADR-0005]] 抵触

## Decision

采用 **(b) 冻结快照**：

- `publish_records` + `publish_items` 表组成发布快照
- freeze 阶段（`PublishFlow::freeze`）把 article 的 `title` / `summary` / `link` / `score` 等
  **复制**到 `publish_items.frozen_*` 列；同时把 `articles.state` 推到 `ReadyForPublish` /
  `PublishSkipped` 等终态候选状态
- render / store-local / publish-remote 阶段**只读 frozen 列**，不再查 articles
- `rebuild-report` 子命令读 `publish_records` snapshot + 当前模板重新渲染，**不**修改
  `publish_records` 行 —— snapshot 永远冻结
- 模板未变 + snapshot 一致 → 重建字节相等（见 [../acceptance-cases/commands/rebuild-report.md](../acceptance-cases/commands/rebuild-report.md)）

## Consequences

### 正面后果

- 模板修复后可批量重建历史报告：字节级回归确认改动范围
- 报告字节相等可作为"模板事实"的回归依据
- 状态机简洁：`publish_records` 跨 5 阶段推进时不会被外部 article 变化干扰
- AI-off 直通路径（`include_unscored=true` + `ai.enabled=false`）也走同一 snapshot 接缝，
  避免双路径分叉

### 负面后果 / 代价

- 数据冗余：article 字段在 articles 与 publish_items 两处存储
- 不能"修订已发布报告内容"：必须新建 publish_record + 新 snapshot
- freeze 阶段事务复杂：涉及 article 状态跃迁 + items 插入 + record 推进，单 SQL 事务里完成
- backfill / ai 实验产生的新 ai_result 不会自动重渲染已发布报告 —— 需显式 `rebuild-report` 或新 publish run

### 后续行动

- `publish_items.frozen_*` 列由 schema 定义，对应 plan/04-publish.md §冻结快照
- rebuild-report 子命令实现字节相等保证（`rebuild_returns_byte_equal_markdown_to_original_render` 测试）

## Links

- 设计：[../plan/04-publish.md](../plan/04-publish.md) §冻结快照
- 实现：[`crates/runtime/src/flows/publish/freeze.rs`](../../crates/runtime/src/flows/publish/freeze.rs) 的 freeze 方法
- 重建：[`crates/runtime/src/flows/rebuild_report.rs`](../../crates/runtime/src/flows/rebuild_report.rs) + [`crates/report/src/`](../../crates/report/src/)
- 验收：[../acceptance-cases/commands/rebuild-report.md](../acceptance-cases/commands/rebuild-report.md)、[../acceptance-cases/pipelines/04-publish-local-and-github.md](../acceptance-cases/pipelines/04-publish-local-and-github.md)
- 关键测试：`rebuild_returns_byte_equal_markdown_to_original_render`（[`crates/runtime/tests/rebuild_report_tests.rs`](../../crates/runtime/tests/rebuild_report_tests.rs)）
