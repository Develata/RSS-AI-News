# architecture-diff — plan vs code 漂移登记

本文件登记 [./architecture-plan.lisp](./architecture-plan.lisp) 与
[./architecture-code.lisp](./architecture-code.lisp) 之间的差异。任何 plan 与 code
不一致的地方先**记录**在这里，不直接修补一边，先让漂移可见。

## 状态语义

| 状态 | 含义 |
|---|---|
| `open` | 已发现，未收敛 |
| `resolved` | 已修复 plan 或 code 任一边；附 commit SHA 与修复方向 |
| `accepted` | 双方都不动，承认现状（必须附 ADR 链接） |

## 收敛流程

发现漂移时按以下顺序处理：

1. 在下方表格追加一行（不删除既有行）
2. 决定收敛方向之一：
   - **改代码** — 实现追赶契约（常见）
   - **改 plan** — 现实已合理，更新文档承认现实
   - **写 ADR** — 现实与契约偏差有充分理由，固化为决策
3. 收敛完成后把行状态改为 `resolved` 或 `accepted`，附 commit SHA / ADR id

## 漂移列表

| 编号 | 发现日期 | 节点 :id | 差异描述 | 状态 | 收敛 |
|---|---|---|---|---|---|
| D-001 | 2026-06-04 | _(baseline)_ | 初版 plan 视图与 code 视图按章节 + codegraph 协同写出，未发现需登记的具体漂移 | resolved | 基线建立；后续漂移由首先发现的人在此追加 |
| D-002 | 2026-06-11 | config-versioning | plan 06 §11 声称 `ConfigVersionStore::get_or_create_config_version` 向 reindex 关联配置快照；code 中该 trait 生产路径零调用（reindex 实际走 `active_rule_or_register`），`reindex.rs` 注释同样描述不实 | resolved | 双向收敛：P0 修订 06 §11（c6612a7）；P2 删除 trait 死代码 + 修 reindex.rs 注释（43f7443） |
| D-003 | 2026-06-11 | storage-multi-dialect-doc | `cli/src/context_factory.rs` 等多处注释引用 `docs/design/storage-multi-dialect.md`（§2.5/§5.4），该文件在仓库中不存在（docs/design/ 目录不存在） | resolved | 改注释/链接（W17）：设计契约实际存于 `docs-backup/design/storage-multi-dialect.md`（2026-05-16 落地，docs 重组迁入 backup 后引用未跟上）。代码注释/ci.yml 7 处改指 backup 真实路径；README 10 处死链改指 plan/ 活章节 + backup 存档。现行权威 = plan/05-storage.md。**例外**：migrations/postgres/0001 的注释已回滚不改——sqlx 对已应用迁移做校验和比对，文件字节不可变（见 migration_immutability_tests.rs 锁定） |
| D-004 | 2026-07-18 | cli-commands / ci-workflow | code/map 仍写 12 commands / 4 CI jobs，实际已为 13 commands（含 `recent-entries`）/ 5 jobs；`RecentEntriesFlow` 也未进入 code map | resolved | v0.7.1 candidate 同步 `plan/09`、`architecture*.md`、`modules.lisp` 与 `architecture-code.lisp`，并登记独立 acceptance tooling boundary |

| D-005 | 2026-09-10 | run-meta / ingest-deps / extract-deps / ai-deps / publish-deps / backfill-deps / reindex-deps / rebuild-report-deps / health-check / doctor-health | 用户授权 Boundary & Resource Hardening 后，旧地图仍将 RunContext/RunContextDeps 标为全局接缝，并把具体 health checks 与 config/storage 依赖放在 observability；与本轮代码及 plan/05、07、09 的边界不一致 | resolved | 当前工作树同步 plan/code/crate maps：按 flow 列出具体依赖 struct，RunMeta 仅保留运行标识；health 实现归 runtime，observability/domain 无 infra 反向依赖。见 [ADR-0009](../adr/0009-boundary-resource-hardening.md)；提交 SHA 待本轮提交后记录 |
| D-006 | 2026-10-16 | ai-prompt / ai-parser / backfill-ai | plan/03 §1–§4、§7–§8，plan/05 §8，plan/10 §4.1、§5、§6，plan/13 §5.3，plan/14 §B 与代码不符（另含"一次 ai-run 严格单 category"、prompt 经 reindex 升级、rebuild-report 旧参数）：`[prompt]` 段与 `{{title}}`/`{{body}}` 双花括号、`keep`/`score` 字段、head+tail 截断、`kind='ai_prompt'` 按分类、parser 签名与围栏提取、`--prompt-template-path`、`prompt_versions` 表、backfill 走 reindex 并激活版本、ai replay diff。照旧文档配置会使正文不发送给模型；代码与示例的默认模板同样不含占位符 | resolved | 改 plan 承认现实（parser 不变）；代码侧只收敛 prompt 来源：`EffectiveConfig` 统一模板（省略 / 空串 = 含占位符的内置默认），新增 `prompt_template` warning 与 `dedup.link_normalizer_version` inert warning。backfill 分类 / 模型限制登记于 plan/10 §5.2，暂不改实现。提交：568471e、152caed（v0.9.1） |
| D-007 | 2026-10-16 | artifact-retention | 原 plan/10 §3 定义 `on_failure` = 下游解析失败时回写，且五档策略作用于全部 stage；代码中 ingest / extract 只调用 `should_write(false)`（`on_failure` 实际从不写 feed / html），AI 路径 `process.rs::write_ai_raw_response_artifact` 不检查策略、每次尝试都写。`debug_only` 的 warning 文案"currently writes no artifacts"同样不含 AI 例外 | open | 文档已改为描述现状（plan/10 §3、plan/03 §5.3、plan/06、README）；实现是否对齐策略（失败回写、AI 受控）待用户决定，涉及 replay 与 `raw_response_artifact_id` 依赖 |
| D-008 | 2026-10-16 | backfill-window | `backfill --date-to` 的 CLI help 写"结束日期（含）"，SQL 为 `created_at < date_to 00:00 UTC`（不含当天），extract 与 ai 两个 target 一致 | resolved | 改代码：`backfill` 上界改为 `--date-to` 次日 00:00 UTC（`parse_date_to`），与 help 及原设计 `[date_from, date_to]` 一致；plan/10 §5.1 / §5.2、acceptance case、cli-reference 同步 |

## 已知**可能**的近期漂移点（监视）

以下不是确认漂移，只是设计/实现演进路径上较可能产生差异的位置，列出便于日后核对：

- ~~`config_sha256` 与 bootstrap rule 升 active 后真实 sha 替换路径~~ → 已升级为
  W16 设计议题（[../plan/16-config-versioning.md](../plan/16-config-versioning.md)），
  相关文档漂移登记为 D-002
- `replay --kind` 对文件后端 artifact 的支持（commands/replay.md 标注 partial）
- `scheduler` 镜像缺自动化 e2e（commands/scheduler.md 标注 partial）
- `runtime/flows/backfill.rs` 与 `runtime/flows/rebuild_report.rs` 的 plan 节点尚未在 plan/ 章节内单列入口段落（10 章是整段叙述）

监视项**不**算 open 漂移；只有在 plan 和 code 实质不一致时才追加为 D-NNN 行。

## 修复 / 接受样例

仅作格式参考，不是真实记录：

```markdown
| D-002 | 2026-07-15 | flow-ai-run | plan 节点说 task_gen 仅扫 Persisted，code 实测同时扫 AiDone | resolved | 改 plan，code 行为已固化；commit a1b2c3d |
| D-003 | 2026-08-02 | publish-state | code 内部多了一个 `Retrying` 中间态，plan 状态机未列出 | accepted | 见 adr/0009-publish-retry-intermediate-state.md |
```
