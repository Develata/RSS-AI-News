# ADR 0009 — Boundary & Resource Hardening

- 日期：2026-09-10
- 状态：已接受，工作区实施；尚未发布
- 授权：本轮用户明确要求收窄 application/domain/observability 边界并直接实施。

原 RunContext 把所有 client、repository 和 AppConfig 一并暴露给每条流程，造成无关凭据 gate、占位 client 与复杂测试装配。结果汇总、源配置复制、未限制的网络响应又放大了长期运行的资源成本。

采用普通 struct composition：IngestDeps、ExtractDeps、AiDeps、PublishDeps、RebuildReportDeps、BackfillDeps、ReindexDeps；RunMeta 仅携带运行身份。CLI composition root 负责按能力初始化客户端及区分读/写存储。删除原全量上下文和 NullAiClient，不增加容器、注册表、trait 转发层或新 crate。

Domain 不依赖 SQLx；数据库原值到 domain invariant 的转换由 storage 显式完成。observability 只包含日志、metrics、脱敏、通用 health 类型；具体检查迁到 runtime::doctor。SQLite/PG 继续显式分支，保持原有 atomic claim、dedup、事务和 lease 恢复机制。

extract/AI 的历史 outcome 集合改为 counters + 最多32条失败样例；源配置引用迭代，ingest任务数受 concurrent_feeds 约束；AI模板和配置共享 Arc。摘要只在降级时查询。AI/health/GitHub 响应各有固定上限，remote publish 截止时间从 claim 前开始计算，保留 lease 的20%用于释放。

只读维护路径不隐式迁移、轮换配置或构造外部客户端；reindex dry-run 不写业务表或事件。dry-run 全量碰撞模拟需要历史占用集合，因此采用显式行数上限，超限失败而非返回截断“成功”。这是有边界的管理操作，不声称可在常量内存中精确模拟任意大数据库。

代价与兼容：runtime/domain 的 Rust DTO/API 发生源代码级变化，workspace 内调用方已同步；公开 TOML 格式与持久化 schema 保持兼容。需要迁移的只读命令现在报错，用户须显式 migrate run。若配置已指定远端仓库但缺凭据，publish 明确失败；--local-only 可独立运行。

Docker 采用普通 locked Cargo build + cache mounts，放弃 stub/fingerprint 技巧。新建 GitHub runner 的 cache mounts 不自动跨 runner 持久化，冷构建耗时需由真实 CI 持续测量。保留单次 CLI 与外部 scheduler，不改 runtime/ORM，不引入 batch insert 或未经测量的新索引。
