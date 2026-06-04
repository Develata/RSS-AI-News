# 一页架构总览

这是 [../plan/00-overview.md](../plan/00-overview.md) 的浓缩"导航地图"。
完整定义、不变量、设计依据请回 plan。**本页只回答**："系统在哪、谁调谁、改一处影响什么"。

## 整体形态

- 一只 **single-shot CLI**（`rss-ai-news`），每次 run 完成一段流水线后退出
- 11 个 library crate + 1 个根 binary = workspace 共 12 crate
- 主链路 5 段：`ingest → extract → ai-run → publish → publish-remote`
- 调度外挂（外挂 cron / supercronic 镜像），见 [../plan/13-non-goals.md](../plan/13-non-goals.md)

## 四层 + 对象层 + 横向

```text
┌──────────────────────────────────────────────────────────────────────┐
│  L1 交互壳层    main.rs                                              │
│  L2 指令接口层  cli/commands/* (clap derive)                          │
├──────────────────────────────────────────────────────────────────────┤
│  L3 流程协调层  runtime/flows/*  (RunContext + Flow 编排)             │
├──────────────────────────────────────────────────────────────────────┤
│  L4 能力执行层  feed | extractor | ai | storage | publish | report    │
├──────────────────────────────────────────────────────────────────────┤
│  对象层         domain | config                                       │
├──────────────────────────────────────────────────────────────────────┤
│  横向能力       observability (tracing / metrics / health / events)   │
└──────────────────────────────────────────────────────────────────────┘
```

跨层调用规则：上层调下层、同层调同层、**禁止下层反向调上层**。`observability`
是显式横向，可被任一层使用。

## 主链路一图

```text
FeedSource ──ingest──▶ FeedEntry ──extract──▶ Article ──ai-run──▶ ArticleAiResult
                          │                      │                    │
                          └─ raw_artifacts ◀─────┴────────────────────┘
                          (feed_payload / html_payload / ai_raw_response)
                                                                       │
                                                                       ▼
                                                     publish_records ◀ candidate
                                                          │
                                              freeze ──▶ publish_items (frozen)
                                                          │
                                              render ──▶ Markdown
                                                          │
                                        store-local ──▶ filesystem
                                                          │
                                       publish-remote ──▶ GitHub commit
```

每段对应一个 Flow，入口集中在 [`crates/runtime/src/flows/`](../../crates/runtime/src/flows/)。
状态机共 4 个，集中在 [`crates/domain/src/state.rs`](../../crates/domain/src/state.rs)，
详见 [../plan/08-state-machines.md](../plan/08-state-machines.md)。

## crate 依赖（手工对照参考）

依赖箭头读法：`A → B` 表示 `A` 的 Cargo.toml 在 `[dependencies]` 列了 `B`。
权威版本以 `Cargo.toml` + codegraph 为准；本图仅作导航。

```text
              ┌─────────── observability ◀──── (所有 runtime/cli)
              │
   cli ──▶ runtime ──▶ storage ──┐
              │  │       │        │
              │  ├──▶ feed         │
              │  ├──▶ extractor    │
              │  ├──▶ ai           │
              │  ├──▶ publish ─────┤
              │  └──▶ report ──────┤
              │                    │
              └──▶ config ──▶ domain ◀── (几乎所有 crate)
```

机器可读的 crate 依赖见 [./modules.lisp](./modules.lisp) 与
可选脚本 [`../../scripts/map-deps.sh`](../../scripts/map-deps.sh)（如已添加）。

## 9 大不变量速记

| # | 速记 | 实施位置 |
|---|---|---|
| 1 | 去重前不抓正文 | feed + extract 第三层 hash 拦截 |
| 2 | 所有队列固定容量 | `[http].concurrent_*` + lease |
| 3 | 入库后状态自描述 | 4 状态机表 |
| 4 | AI 任务只从数据库领取 | `article_ai_results` Pending |
| 5 | 发布先冻结快照 | `publish_items.frozen_*` |
| 6 | 所有并发任务都 claim + lease | repo 层 SQL 模式 |
| 7 | 所有外部输入可回放 | `raw_artifacts` × retention |
| 8 | 核心状态与配置单一真相源 | 无内存影子表 |
| 9 | 失败路径与观测点同步设计 | run_events × tracing × ClassifiedError |

定义见 [../plan/00-overview.md](../plan/00-overview.md) §5。

## 路由：常见任务 → 入口

| 想做的事 | 起点 |
|---|---|
| 改某段流水线 | [../plan/](../plan/) 对应 01-04 |
| 改状态机 | [../plan/08-state-machines.md](../plan/08-state-machines.md) → `crates/domain/src/state.rs` |
| 改配置 | [../plan/06-config.md](../plan/06-config.md) → `crates/config/` |
| 加日志/指标 | [../plan/07-observability.md](../plan/07-observability.md) → `crates/observability/` |
| 加错误分类 | [../plan/11-error-and-recovery.md](../plan/11-error-and-recovery.md) → `crates/runtime/src/error.rs` + `crates/cli/src/error.rs` |
| 加迁移 | `migrations/sqlite/NNNN-name.sql` + `migrations/postgres/NNNN-name.sql`，详见 [../acceptance-cases/commands/migrate.md](../acceptance-cases/commands/migrate.md) |
| 加子命令 | `crates/cli/src/commands/*.rs` + `crates/cli/src/args.rs` |
| 加测试 | 对应 crate `tests/`，参考 [../acceptance-cases/](../acceptance-cases/) 引用的测试名 |

## 漂移登记

发现 plan 与 code 不一致时在 [./architecture-diff.md](./architecture-diff.md) 追加一条；
**不直接修改** plan 或 code 中任一方，先让漂移可见。
