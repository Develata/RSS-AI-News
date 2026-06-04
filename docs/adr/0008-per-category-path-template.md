# ADR 0008: 分类级 path_template 覆盖

- 日期：2026-04（W5–W6 期间）
- 状态：`accepted`
- 决策者：项目主作者

## Context

发布段需要把渲染好的 Markdown 写到本地 / GitHub 路径，例如
`AI/2026/20260603.md`。路径模板由 `[publish.template].path_template` 提供全局默认
（如 `{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md`）。

实际生产场景里，不同分类的发布路径**结构差异**可能很大：
- AI 分类沿用全局结构
- 某些分类希望走自定义路径 `custom/foo/{YYYY}/{YYYYMMDD}.md`
- 子站结构如 `subdomain/{YYYYMMDD}.md`

如果只能用全局模板，要么放弃这种分化、要么给每个分类的 publish_record 单独编排 ——
都不优雅。

候选方案：
- (a) 全局模板单一 → 灵活性不足
- (b) **`[publish_override].path_template` 分类级覆盖** + validate 时跨分类冲突检测
- (c) 用 hook / callback 让用户写代码自定义路径 → 引入运行时插件复杂度，与"少依赖"取向矛盾

## Decision

采用 **(b) 分类级 path_template 覆盖**：

- `CategoryConfig.publish_override.path_template: Option<String>`
- 当 `Some(...)` 时**覆盖**全局 `[publish.template].path_template`
- 全局或分类级 path 模板都必须满足：
  - 含日期 token（`{YYYY}` / `{YYYYMMDD}` 等）—— 否则两个不同日期的报告会落到同一路径
  - 不含 `..`（防穿越）
  - 不含反斜杠（强制 `/` 分隔）
- 分类级 override **可以省略** `{category_key}` token（分类内已自带语境）
- 跨分类**路径冲突检测**：渲染样本路径完全相同的两个分类会在 validate 阶段被拦下
  （即使一个用全局模板、一个用 override）

## Consequences

### 正面后果

- 分类可独立演化发布结构而不影响其它分类
- 冲突检测在 validate-config 阶段就拦下，不会到运行时才发现两个分类互相覆盖文件
- 全局 + 局部双层 fallback 模型与配置其它字段一致（如 `min_importance_score`、`include_unscored`）

### 负面后果 / 代价

- 冲突检测算法要枚举所有分类、用样本日期渲染、做集合比较 —— validate 时间略增
- 用户可能误以为"分类 override 完全独立" —— 但 placeholder 仍然受全局 `report_template` /
  `item_template` 约束（这些不开放分类级覆盖）
- 若分类 override 同时跨越多个 subdirectory 层级（如 `archive/{YYYY}/{MM}/sub/{YYYYMMDD}.md`），
  store-local 目录创建逻辑需要递归 `create_dir_all` —— 已实现，但调试时容易忘

### 后续行动

- 当前**不**开放 `report_template` / `frontmatter_template` / `item_template` 的分类级 override
  —— 这些是"渲染契约"层，跨分类应保持统一；只有路径层允许分类异构
- 若未来需要分类级 rendering 差异，单独 ADR 评估

## Links

- 设计：[../plan/06-config.md](../plan/06-config.md) §[publish.template] 与 §[publish_override]、[../plan/04-publish.md](../plan/04-publish.md) §per-category path
- 实现：[`crates/config/src/category.rs`](../../crates/config/src/category.rs) `PublishOverride`、[`crates/config/src/validate.rs`](../../crates/config/src/validate.rs) path 冲突检测
- 验收：[../acceptance-cases/pipelines/06-config-loading.md](../acceptance-cases/pipelines/06-config-loading.md) 跨分类冲突场景、[../acceptance-cases/pipelines/04-publish-local-and-github.md](../acceptance-cases/pipelines/04-publish-local-and-github.md)
- 关键测试：`cross_category_path_collision_fails_when_overrides_collapse_to_same_path`、
  `cross_category_path_with_category_token_does_not_collide`、
  `cross_category_collision_detected_when_only_one_category_has_override`、
  `category_path_template_can_omit_category_token`、
  `category_path_template_still_requires_date_token`、
  `store_local_uses_category_path_template_override`
