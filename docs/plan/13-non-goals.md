# 13 — 明确不做的事

本章固化系统的**反向边界**：哪些方向被显式排除、为什么排除。这些是骨架级约束，
任何"看起来很方便"的局部需求都不能突破这些边界。

宪法 §7.1 规定骨架级变更必须经审批；本章列出的 non-goals 都属于骨架级。**先复读此章
再提出反向方案**，避免重新讨论已被排除的方向。

## 1. 调度方面

### 1.1 不内置 cron / 不常驻进程

二进制是 **single-shot CLI**：每次调用执行一遍后退出。

**禁止**：
- 在 Rust 二进制内引入 tokio interval、cron 解析器、`loop { sleep; ... }`
- 任何形式的"等待下一个周期"循环
- 后台守护线程做周期性任务

唯一例外：`ingest` / `ai-run` 内部的批次处理循环（处理当前批次直到 `pending` 清空或达到
`batch_size * max_batches_per_run`），属于单次运行内的工作分片。

**理由**：见 [../adr/0001-single-shot-cli-no-builtin-cron.md](../adr/0001-single-shot-cli-no-builtin-cron.md)。
核心论点：可观测性按 run_id 切分、可恢复性依赖宿主重新调用、宿主调度器（cron / systemd /
GHA / K8s CronJob）比自建调度器更稳定。

### 1.2 不自建 worker pool / 不实现工作队列

并发由数据库 lease 机制承担。**不**引入：
- 进程内 worker pool（Rayon / Tokio task pool 用于业务级并发）
- 消息队列（Redis / NATS / RabbitMQ）做任务分发
- 自定义 work-stealing / fan-out 调度

并行执行通过启动多个 CLI 实例并行调度实现（每个实例独立 claim lease）。

## 2. 部署方面

### 2.1 不追 scratch 镜像

终局镜像基础是 `debian:bookworm-slim`，**不**追 `scratch` / `distroless`。

**理由**：
- 必须保留 CA 证书（HTTPS 出站）
- 必须保留 tzdata（按时区生成日报）
- 必须保留 ca-certificates 更新通道
- 稳定优先于极限压缩

详见 [../adr/](../adr/) 中关于部署决策的 ADR。

### 2.2 不默认 root 用户

镜像 ENTRYPOINT 用非 root 用户（`appuser` uid/gid 10001）运行。即使容器 escape，也无 root 权限。

### 2.3 不内置自动更新机制

二进制不自动升级。版本切换由宿主（Docker pull 新 tag / 包管理器升级）执行。

## 3. 配置方面

### 3.1 不支持远程配置中心

配置只读取本地文件：
- `.env`（密钥 / 占位符值）
- `app.toml`（全局）
- `categories/*.toml`（每分类）

**不**支持 Consul / etcd / Vault 等远程配置拉取。

**理由**：
- 一致性来源单一（每次启动读固定文件）
- 简化部署（无配置中心依赖）
- 通过 K8s ConfigMap / Docker secrets 挂载文件可达到类似效果

### 3.2 不热加载配置

配置只在进程启动时加载一次。运行中改 toml 文件不生效。**理由**：single-shot 进程不需要热加载，
下次调用自动读新值。

## 4. 数据方面

### 4.1 不依赖外部 cache

不用 Redis / Memcached 做查询缓存。状态全部在 SQLite / PostgreSQL 表中。

**理由**：
- 单一真相源约束（宪法 §3.4）
- single-shot 进程缓存命中率低
- 增加部署复杂度

### 4.2 不支持多租户隔离

数据库实例 1:1 对应一个 RSS-AI-News 部署。**不**实现：
- 多 tenant 字段
- 行级安全（RLS）
- 跨 tenant 鉴权

如需多份独立部署，启动多个 CLI 实例 + 各自独立的 DB。

### 4.3 不暴露 SQL 接口

CLI 没有 `query` / `exec` 子命令让用户执行 SQL。直接连 DB 用 `sqlite3` / `psql`（运维自负）。

### 4.4 不内嵌 Web UI

无 dashboard、无 admin panel、无 HTTP server（除 `/metrics` Prometheus 端点）。**理由**：
- 偏离单进程 CLI 定位
- 增加权限 / 鉴权设计负担
- 已有 GitHub 发布产物作为输出 UI

## 5. AI 方面

### 5.1 不强制 OpenAI

接口签名是 OpenAI Chat Completions 兼容。任何兼容 endpoint（如 vLLM / Ollama / DeepSeek /
Azure OpenAI）都可通过 `OPENAI_BASE_URL` 切换。

**不**承诺支持 OpenAI 私有扩展（function calling 之外的字段、tools 字段、stream tools 等）。

### 5.2 模型路由：仅失败回退，不做智能路由

**支持**（v0.x，见 [./14-ai-fallback.md](./14-ai-fallback.md)）：
- 调用失败时按配置的 fallback 模型链顺序重试（同凭证下换 model 名）。主模型失败
  （quota / 限流 / 5xx / 模型不存在 / 超时 / 内容解析失败等）时，在同一次执行内依次尝试
  `[ai].fallback_models`，全部失败才按可重试性回队 / 永久失败。

**仍不实现**（明确排除的"模型路由 essence"）：
- 按文章长度 / 内容特征路由不同模型
- 智能选择模型（成本 / 质量自适应、A/B、加权）
- 运行时跨 provider 动态路由

即：fallback 是"失败后的静态顺序降级"，不是"按输入挑模型"。板块（category）独立凭证
（key / base_url 自治）属第二期，见 [./14-ai-fallback.md](./14-ai-fallback.md) §B。

### 5.3 不内置 prompt 工程工具

prompt 写在 `categories/*.toml` 的 `[prompt]` 段，由用户维护。**不**提供：
- prompt 模板系统
- prompt A/B testing
- prompt 优化建议

prompt 变更通过 reindex + `rule_versions` 升级路径（见 [./03-ai.md](./03-ai.md)）。

## 6. 发布方面

### 6.1 不支持非 Markdown 输出

报告格式固定 Markdown + YAML frontmatter。**不**支持：
- HTML / PDF / Word 渲染
- RSS 反向输出
- 静态站点构建（如 Hugo / Jekyll）

如需其它格式，下游用 Pandoc / 静态站点生成器消费 Markdown。

### 6.2 不直接发文章 / 不内置 SNS 集成

只发布 Markdown 到本地 fs 和 GitHub。**不**实现：
- Twitter / Mastodon / 微博 / 微信公众号自动推送
- Discord / Slack webhook
- Email 通知

如需通知，下游消费 GitHub commit webhook。

### 6.3 不支持 git push 之外的 GitHub 操作

不创建 PR、不打 tag、不开 issue。**理由**：偏离"内容发布管线"定位。

## 7. 运维方面

### 7.1 不内置告警 / 不内置 SLO

无 Alertmanager 集成、无 PagerDuty webhook、无 SLO 计算。`/metrics` 端点暴露 Prometheus
指标，告警由宿主 Alertmanager 配置。

### 7.2 不提供 GUI 管理工具

无运维 dashboard。`doctor` 是唯一健康检查入口（输出文本 / JSON）。

### 7.3 不支持滚动升级 / 蓝绿部署

single-shot CLI 没有"运行中"的状态。版本切换 = 下次调用用新二进制。**不**需要也**不**实现
滚动升级机制。

## 8. 长期不接受的方向

以下方向无论将来需求如何，都不会进入主干：

- 多语言后端（永远 Rust）
- 把 RSS-AI-News 改造成 Web 应用 / SaaS
- 引入图数据库 / 向量数据库（与本体不匹配）
- 抽象为通用"数据管线框架"（违反单一职责）
- 商业化分发（许可证已固定）

如果上述某项被证明确有必要，应另起仓库，**不**在本仓库内演化。

## 9. 当前不变量映射

本章 non-goals 在代码中的物理约束：

| 边界 | 检查方式 |
|---|---|
| 不内置 cron | grep `tokio::time::interval` / `cron::Schedule` 在 src/ 与 crates/ 应无结果 |
| 不内嵌 Web UI | grep `axum::Router` 应只在 observability/prometheus 出现 |
| 不依赖 Redis | grep `redis` 在 Cargo.toml 应无结果 |
| 不打 PR | grep `pull` / `create_pull` 在 crates/publish/ 应无结果 |

`doctor --deep` 不会自动校验本章；这些是设计期约束，由 code-review 与本章共同守护。

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
