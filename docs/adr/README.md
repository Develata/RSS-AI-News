# adr/ — 架构决策记录

按时间序保存非显然的架构决策：**为什么这么决定的、是什么时候做的、有什么后果**。

## 编号规则

- 4 位数字，从 `0001` 起，**不复用**
- 文件名：`NNNN-kebab-case-slug.md`
- 同一议题的修订决策另起新 ADR，旧 ADR 状态改为 `superseded by NNNN`

## ADR 模板

```markdown
# ADR NNNN: <决策标题>

- 日期：YYYY-MM-DD
- 状态：`proposed` | `accepted` | `superseded by NNNN` | `deprecated`
- 决策者：<who>

## Context

（背景：当时面临什么问题、有哪些约束、有什么候选方案）

## Decision

（最终决定：做什么 / 不做什么 / 为什么）

## Consequences

### 正面后果
- ...

### 负面后果 / 代价
- ...

### 后续行动
- ...

## Links

- 相关 commit / PR
- 相关 plan / acceptance-case / 代码路径
- 相关讨论记录
```

## 当前 ADR 索引

| 编号 | 标题 | 状态 |
|---|---|---|
| [0001](./0001-single-shot-cli-no-builtin-cron.md) | 单次执行 CLI，不内置 cron | TBD |
| [0002](./0002-stage-driven-lease-claim.md) | 阶段驱动 + 租约领取 | TBD |
| [0003](./0003-publish-snapshot-immutable.md) | 发布快照不可变 | TBD |
| [0004](./0004-active-rule-resolver-partial-unique.md) | active_rule resolver + partial unique index | TBD |
| [0005](./0005-storage-pool-dual-dialect.md) | StoragePool 双方言 enum | TBD |
| [0006](./0006-postgres-go-real-no-shrink.md) | PostgreSQL 走实补不收缩 | TBD |
| [0007](./0007-rsshub-secret-runtime-expansion.md) | RSSHub 占位符运行时展开 | TBD |
| [0008](./0008-per-category-path-template.md) | 分类级 path_template 覆盖 | TBD |

## 与其它目录的关系

- **[../constitution.md](../constitution.md)** 是 ADR 的边界：ADR 不得突破宪法
- **[../plan/](../plan/)** 中描述的设计选择，凡是非显然的，都应有对应 ADR 留痕
- **[../handoffs/](../handoffs/)** 是 ADR 的执行流水帐
- **决策被验证为错误时**：旧 ADR 状态改 `deprecated`，新 ADR 说明撤回理由
