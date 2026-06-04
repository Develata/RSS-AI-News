# reports/ — 时间戳快照

本目录保存**时间戳事实记录**，非真相源。包括：

- `releases/`：每次 release 的完成态快照（如 v0.1.0 / v0.2.0 / v0.3.0）
- 后续可新增：`audits/`（code review / security audit）/ `incidents/`（故障复盘）等

## 子目录

| 子目录 | 内容 | 当前文件 |
|---|---|---|
| [releases/](./releases/) | 每个 release tag 一份快照 | TBD: v0.1.0 / v0.2.0 / v0.3.0 |

## release 快照固定结构

```markdown
# Release vX.Y.Z

- tag：vX.Y.Z
- 日期：YYYY-MM-DD
- 范围：上一个 tag .. vX.Y.Z

## 核心交付
- ...

## 文档同步
- 更新了 plan/0X-...md（具体差异）
- 新增 ADR-NNNN
- 新增 / 更新 acceptance-case AC-...

## CI / 镜像
- GHCR 镜像：`ghcr.io/<owner>/rss-ai-news:vX.Y.Z`
- workflow：`.github/workflows/release.yml`

## 后续 follow-up
- 未结清的小修：链接到 handoffs/ 或下个版本的 plan 章节
```

## 与其它目录的关系

- **不改写 [../plan/](../plan/) 或 [../adr/](../adr/)** — 这里是事后记录，不是真相源
- 与 [../handoffs/](../handoffs/) 的区别：handoffs 是每次工作交接（高频，append-only），
  reports 是 release / 重大事件维度（低频，每件一份）
