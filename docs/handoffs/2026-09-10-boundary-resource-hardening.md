# Boundary & Resource Hardening

- 日期：2026-09-10
- 作者 / Agent：Codex
- 分支：main
- 当前 HEAD：95ae450
- 相关 commit：pending（未提交）
- 相关 tag / release：基于 v0.7.1；未发布新版本
- 状态：`validated`（本地可用环境；PG/Docker未验证）

后续审查修复及最新验证见[review handoff](2026-09-10-boundary-hardening-review.md)。本记录保留第一轮交付快照。

## 工作摘要

按用户授权收窄flow/domain/observability边界，限制任务、响应和结果累计，减少复制与重复查询，并修复能力初始化、正文抽取、诊断和dry-run的实际错误。

## 影响范围

- crate：domain、config、storage、observability、feed、extractor、ai、report、publish、runtime、cli及根binary。
- 真相源：持久化对象、schema、迁移文件保持兼容；Rust RunContext/DTO/summary公开接口有源代码级变更，workspace调用方已迁移。
- CLI：单命令按能力构造；只读维护不隐式迁移；配置策略真正生效；通用错误和doctor输出脱敏。
- 构建：Docker普通locked build/cache mounts；Supercronic0.2.49/SHA-256；定期audit与月度兼容依赖更新。

## 关键变更

7个窄依赖struct与RunMeta替代全量RunContext；domain移除SQLx，具体health进入runtime。ingest引用配置并限制存活任务；extract lazy fallback；AI模板/options共享；extract/AI仅保留32个失败样例。AI/GitHub/health响应有上限，远端发布整体deadline留出lease释放时间。metrics连接/时间/头部有上限。

Readability在CLI真正装配并执行最小正文长度。DB URL拒绝未知scheme。reindex dry-run只读、有100k行上限，content-hash按顺序模拟碰撞链。remote报告一次加载冻结items，避免重复SELECT与整批Markdown复制。

契约和地图已同步：[ADR0009](../adr/0009-boundary-resource-hardening.md)、[plan09](../plan/09-cli-and-runtime.md)、[plan02](../plan/02-extract.md)、[plan03](../plan/03-ai.md)、[plan04](../plan/04-publish.md)、[plan05](../plan/05-storage.md)、[plan06](../plan/06-config.md)、[plan07](../plan/07-observability.md)、[plan14](../plan/14-ai-fallback.md)、[reindex验收](../acceptance-cases/commands/reindex.md)。

## 验证与验收

- PASS：fmt；workspace all-targets/all-features Clippy `-D warnings`；workspace all-features test/build；locked release build。
- PASS：816测试、0失败、62 ignored；原始日志另含隔离子进程重复执行1项。6个ignored perf fixture已分别运行3次，56项原有ignored集成测试未执行。
- PASS：SQLite/release acceptance（migration/query/版本）；吞错策略；inactive-rsa gate；遵循既有rsa例外的cargo audit。
- FAIL（已解释）：完整cargo audit仍报告锁文件inactive rsa。保留fxhash维护状态与spin/chacha20 yanked警告。
- PASS：7条SQLite生产SQL的10k行EXPLAIN；Python语法；shell语法；diff空白检查。
- PASS：两个架构Supercronic摘要与ELF检查；原生amd64合法/非法cron、mounted/generated启动、SIGTERM退出。
- NOT RUN：PG integration/migration/perf/query plan、Docker runtime/debug/scheduler构建与容器smoke、arm64执行。当前无PG，Linux Docker入口无效。

## 结果

代码与文档可供提交审查；没有commit、push、镜像发布或部署生效。1000源配置场景三次中位耗时664.520→434.105ms，RSS199336→11976KiB；其他场景不声称普遍加速，binary体积+0.48%。

完整修改表、候选问题分类、性能原始数据、依赖和PASS/FAIL/NOT RUN记录见[工程报告](../reports/boundary-resource-hardening/README.md)。

## 风险与后续事项

发布前补齐PG与Docker矩阵。继续跟踪上游yanked/维护状态；部分配置字段是已记录的兼容保留项。Readability仍同步解析；repository trait尚混合读写方法；dry-run预测要求静态数据，不提供跨页一致性快照。Docker cache mounts在新GitHub runner上不自动持久化。docs-backup仍有引用，保留为只读历史。
