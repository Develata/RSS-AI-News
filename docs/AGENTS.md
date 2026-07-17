# Agent 工作指南

本文件指引在 RSS-AI-News 仓库内工作的所有 agent（含 Claude Code、Codex CLI、各类 subagent）。

## 角色定位

RSS-AI-News 当前处于**细节优化期**：
- 主骨架已稳，W0–W11 全部交付；当前稳定版本与候选版本以 `git tag` / release report 为准
- 工作重心：局部性能 / 可观测性 / 可维护性 / 缺陷修复 / 小范围能力扩展
- **不接受**未经审批的骨架级变更（见 [constitution.md §7.1](./constitution.md)）

## 工作入口

任何改动开始前，**先读这四件套**，避免重复探索：

1. **[constitution.md](./constitution.md)** — 不可破坏的最高约束
2. **[plan/](./plan/)** 对应能力章节 — 当前契约
3. **[map/architecture-code.lisp](./map/architecture-code.lisp)** — 目标符号当前位置
4. **[acceptance-cases/](./acceptance-cases/)** 对应 case — 验收边界

跳过这四步直接动代码 = 大概率撞契约或回归测试。

## 工具优先级

| 任务类型 | 首选 | 原因 |
|---|---|---|
| 找符号定义 / 调用关系 / 影响面 | `codegraph_*` MCP | 亚毫秒级，AST 解析准确 |
| 跨目录文本搜索 | Grep | 比 codegraph 弱但覆盖注释 / log message |
| 文件遍历 | Glob / `codegraph_files` | 不要用 `find` |
| 库 API 查询 | `context7` MCP | 训练数据可能过时 |
| 浏览器验证（罕见，本项目无 UI） | 不需要 | CLI 项目 |

## codegraph 联动

本仓库的 `map/architecture-code.lisp` 由 codegraph 数据导出。当你新增 / 移动 / 删除关键符号
（特别是 `crates/*/src/lib.rs` 的公开导出、`crates/runtime/src/flows/*` 的 Flow 入口、
`crates/cli/src/commands/*` 的子命令），请同步检查地图是否需要更新。

校对方式：
```
mcp__codegraph__codegraph_files crates/
# 与 map/modules.lisp 列表比对
```

漂移记录在 [map/architecture-diff.md](./map/architecture-diff.md)。

## 改动后的硬门槛

所有 Rust 改动在 commit 前必须通过：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --jobs 1 -- -D warnings
```

本仓库已配置 `.githooks/pre-commit` 自动执行。启用方式：[.githooks/README.md](../.githooks/README.md)。

CI 同步校验，本地跳过 = 远端必炸。

## 测试纪律

按 [`~/.claude/rules/common/testing.md`](file:///C:/Users/QQ/.claude/rules/common/testing.md)
分级。对本仓库：
- **修 bug**：先写复现测试（红 → 绿）
- **加能力**：覆盖正常路径 + 至少一条失败路径
- **重构**：必须有等价回归测试锁定行为
- **改契约**：测试与文档同步更新

每个 acceptance-case 在 [acceptance-cases/](./acceptance-cases/) 里都标注了引用的测试名，
改前先看。

## 子代理使用

参考 [`~/.claude/rules/common/agents.md`](file:///C:/Users/QQ/.claude/rules/common/agents.md)：
**默认不 spawn**。本项目可直接用 codegraph + Read + Grep 完成绝大多数探索。

明确该用 sub-agent 的场景：
- 多文件并行重构（用 `refactor-cleaner`）
- 显式 TDD 流程（用 `tdd-guide`）
- Release 前安全审计（用 `security-reviewer`）

## 工作交接

完成一次有意义的改动后，在 [handoffs/](./handoffs/) 追加一份记录。
TEMPLATE 见 [handoffs/TEMPLATE.md](./handoffs/TEMPLATE.md)。

交接的核心要求：
- 事实型，不空泛
- 验证项写明：跑了什么、结果如何、未跑的原因
- 跨 crate / 跨平台影响显式列出
- 如果改了真相源（契约 / 状态机 / DTO），handoff 必须指出对应 plan/ 与 adr/ 已同步

## 与 codex 协作

参考 `~/.claude/skills/codex-orchestration/`（统一走 `codex exec` CLI，不走 MCP）。
RSS-AI-News 上 codex 已多次承担实施类任务，Claude 做评审与决策。两边并行时：
- Claude 不动 codex 工作中文件
- 提交前互相 review 对方变更
- 关键决策写入 adr/，不留在私聊
