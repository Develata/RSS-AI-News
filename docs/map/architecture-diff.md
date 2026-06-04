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

## 已知**可能**的近期漂移点（监视）

以下不是确认漂移，只是设计/实现演进路径上较可能产生差异的位置，列出便于日后核对：

- `config_sha256` 与 bootstrap rule 升 active 后真实 sha 替换路径（plan 06 §11 标注的"已知缺口"）
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
