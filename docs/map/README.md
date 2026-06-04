# map/ — 现状导航地图

本目录是**现状索引**，不承载契约内容。它指向 [../plan/](../plan/) 章节与实际代码路径，
让读者 / agent 从"我要改 X"到"X 当前在哪、改了影响什么"三跳到位。

## 文件清单

| 文件 | 角色 | 维护方式 |
|---|---|---|
| [architecture.md](./architecture.md) | 一页架构总览：四层 + 对象层 + 主链路 | 手写，每 minor release 一次 |
| [architecture-plan.lisp](./architecture-plan.lisp) | plan 视图（"应当如此"） | 手写，与 [../plan/](../plan/) 同步 |
| [architecture-code.lisp](./architecture-code.lisp) | code 视图（"实际如此"） | 由 codegraph 半自动导出 |
| [architecture-diff.md](./architecture-diff.md) | drift registry：两个视图的差异 | 发现漂移即追加 |
| [modules.lisp](./modules.lisp) | 12 crate + 关键模块清单 | 增删 crate 时更新 |

## lisp 风格约定

参考 Deve-Notebook `docs/overview/`。S-expression keyword 形式：

```lisp
(node :id <kebab-case-id>
      :label "<人类可读名>"
      :layer <interaction-shell|instruction-interface|flow-coord|capability|object>
      :crate <crate-name>
      :path "<crates/.../src/file.rs>"
      :kind <function|struct|enum|trait|module|flow|state-machine>
      :upstream (<id1> <id2> ...)
      :downstream (<id1> <id2> ...)
      :state <active|deprecated|planned>)
```

字段语义：
- `:id` 全局唯一，不复用
- `:layer` 来自 [constitution.md §3.2](../constitution.md) 四层 + 对象层
- `:crate` 是 12 crate 之一（见 [modules.lisp](./modules.lisp)）
- `:path` 是相对仓库根的代码路径
- `:upstream` / `:downstream` 表达调用关系
- `:state` 标记成熟度：`active` 在用 / `deprecated` 将弃 / `planned` 计划中

## codegraph 联动

`architecture-code.lisp` 的初版可用以下方式半自动生成：

```text
mcp__codegraph__codegraph_files crates/    # 列出所有 crate 文件
mcp__codegraph__codegraph_explore <crate>  # 列出公开导出
mcp__codegraph__codegraph_callers <symbol> # 反查调用方
mcp__codegraph__codegraph_callees <symbol> # 正查被调
```

人工补充 `:layer` / `:state` / 高层语义 `:label`，机器导出 `:path` / `:kind` / `:upstream` / `:downstream`。

## 漂移流程

发现 plan 与 code 不一致时：

1. 在 [architecture-diff.md](./architecture-diff.md) 追加一条漂移记录（不删 plan 或 code）
2. 决定收敛方向：修代码以匹配契约 / 修契约以承认现实 / 写新 ADR
3. 收敛完成后，把漂移条目标记 `resolved` 并附 commit SHA

漂移条目格式见 [architecture-diff.md](./architecture-diff.md)。
