# acceptance-cases/ — 功能清单 + 验收状态

本目录回答："这个项目**有哪些功能**、每个功能**当前的验收状态**如何？"

两层组织：
- `pipelines/`：按本体流水线段（feed / extract / ai / publish / storage / config）
- `commands/`：按独立 CLI 子命令（replay / backfill / reindex / doctor / ...）

`pipelines/` 与 `commands/` 内容互补，不重复：能整合到流水线段的能力放 pipelines/，独立工具型子命令放 commands/。

## 每份 case 的固定结构

```markdown
# AC-XX: <功能名>

## 功能描述
（1-3 段，说明这个功能做什么、面向什么场景）

## 验收标准

### 命中条件（success path）
- 输入 X 时，必须输出 Y
- ...

### 失败条件（failure path）
- 输入 Z 时，必须以 exit code N 退出
- ...

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `test_xxx` | `crates/yyy/tests/zzz.rs` | 命中条件 #1 |
| ... | | |

## 当前状态

`passing` | `partial` | `regression` | `deprecated`

（如非 passing，说明已知缺口与跟进任务）

## 相关文档

- 设计：`plan/0X-...md`
- 决策：`adr/000X-...md`
- 运维：`operations/...md`
```

## 状态语义

| 状态 | 含义 |
|---|---|
| `passing` | 所有验收标准均被自动化测试覆盖，最近一次 CI 通过 |
| `partial` | 部分验收标准未被测试覆盖，或测试存在 known limitation |
| `regression` | 曾通过、近期 CI 失败、未修复（必须在 [../handoffs/](../handoffs/) 有跟进记录） |
| `deprecated` | 能力已废弃或被其它能力替代，保留以便溯源 |

## 编号规则

- `pipelines/` 编号 `AC-P-NN`
- `commands/` 编号 `AC-C-NN`
- 编号一经分配不复用，废弃 case 保留编号 + `deprecated` 状态

## 索引

### pipelines/

| 编号 | 文件 | 状态 |
|---|---|---|
| AC-P-01 | [feed-ingest.md](./pipelines/01-feed-ingest.md) | TBD |
| AC-P-02 | [article-extract.md](./pipelines/02-article-extract.md) | TBD |
| AC-P-03 | [ai-analysis.md](./pipelines/03-ai-analysis.md) | TBD |
| AC-P-04 | [publish-local-and-github.md](./pipelines/04-publish-local-and-github.md) | TBD |
| AC-P-05 | [multi-dialect-storage.md](./pipelines/05-multi-dialect-storage.md) | TBD |
| AC-P-06 | [config-loading.md](./pipelines/06-config-loading.md) | TBD |

### commands/

| 编号 | 文件 | 状态 |
|---|---|---|
| AC-C-01 | [replay.md](./commands/replay.md) | TBD |
| AC-C-02 | [backfill.md](./commands/backfill.md) | TBD |
| AC-C-03 | [reindex.md](./commands/reindex.md) | TBD |
| AC-C-04 | [doctor.md](./commands/doctor.md) | TBD |
| AC-C-05 | [validate-config.md](./commands/validate-config.md) | TBD |
| AC-C-06 | [migrate.md](./commands/migrate.md) | TBD |
| AC-C-07 | [rebuild-report.md](./commands/rebuild-report.md) | TBD |
| AC-C-08 | [scheduler.md](./commands/scheduler.md) | TBD |
