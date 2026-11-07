# 摘要字面量标签导致 VitePress 构建失败的修复

- 日期：2026-11-07
- 基线：main `5cf1511470bdc554ae0747fab92a7aad8b4a43e9`
- 相关提交：待审核，未发布
- 状态：`validated`（本地，未发布）

## 工作摘要

主页 2026-11-07 科技新闻的 Servo 摘要含字面量 `<progress>`，原渲染器直接写入
Markdown 引用块，VitePress/Vue 将其当成未闭合标签。相同内容若进入前三条摘要，
也会经 `{excerpt_block}` 到达正文。本次复用现有 Markdown 文本转义器修复这两个出口。

## 影响范围

- `crates/report/src/render.rs`：只在正文输出边界转义摘要/摘录，然后添加引用标记。
- 冻结快照、YAML 摘录、Badge、原始模板占位符不变；无数据库迁移、配置结构或 CLI 变更。
- 新增标签、反斜杠、多行/空行、原始模板兼容与完整 Markdown 快照回归。
- 文档已明确默认引用占位符按纯文本输出。摘要中的 Markdown 标点将显示为字面量。
- 无公开导出或符号移动，无架构地图更新需求。

## 验证与验收

- 原始主页 `f3a65f5` 的完整 VitePress 构建：复现 159:24 缺闭合标签错误。
- 主页仅转义该标签后的完整构建：通过。
- 两个新增 Rust 回归：原实现失败，修复后通过。
- 带完整 Markdown 回归快照的主页构建及 HTML 检查：通过，字面量保留、没有 progress 元素、Badge 正常。
- `cargo clippy --locked --workspace --all-targets --jobs 1 -- -D warnings`：通过。
- `cargo acceptance run --lane static --lane workspace`：通过，7 项静态检查与工作区构建/测试共 9 项均通过。

## 发布边界与后续

生成器补丁尚未提交、推送、创建 PR、tag 或镜像，未访问生产配置。主页文章经单独批准
发布为 `3a02c04caa7d7a5a5fe83ef3ee23e5d0d9030af3`，触发 Pages 工作流。
生成器 main 推送只运行 CI；release 工作流由 v* tag 触发。
生产 scheduler 仍需升级到包含修复的版本，不能以源码合并代替部署验证。

不自动重渲染/重发旧报告，不改冻结快照。旧快照在新渲染器下的字节可能变化；
相同构建与输入下的重建等价性仍由原有测试覆盖。
未运行 PostgreSQL、Docker、release lanes 或真实 feed/AI/GitHub 发布探针。

## 证据入口

- [原始主页失败运行](https://github.com/Develata/Develata.github.io/actions/runs/42318603777)
- `crates/report/tests/render_tests.rs`
- `crates/report/tests/fixtures/literal-summary.md`
- `docs/plan/04-publish.md` 与 `docs/acceptance-cases/pipelines/04-publish-local-and-github.md`
