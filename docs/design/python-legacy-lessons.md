# Python 版历史教训与 Rust 骨架反推

## 1. 目的

本文档把旧 Python 版 `rss-ai-news-py` 的事实现状、痛点和结构性缺陷显式落档，作为 Rust 版骨架级约束的 rationale。

它存在的理由：

- 让未来的维护者看到 Rust 版的每一条硬约束"为什么"是硬约束，而不是随意的洁癖
- 当某个 Rust 版设计被质疑"能不能放松"时，能直接回到这份文档看"放松后会退回到 Python 版的哪个坑"
- 作为验收 Rust 首版的对照清单：每一项 Python 痛点都必须有可落地的对策

本文档不是使用手册，也不是 Python 版的架构评审。它只记录与 Rust 骨架决策直接相关的事实与反推。

## 2. Python 版事实快照

### 2.1 模块拓扑

- `news_crawler/core/` — 配置、数据库、日志、爬虫与分类策略
- `news_crawler/services/` — `crawler_service` / `ai_service` / `report_service` / `publisher_service` / `email_service`
- `news_crawler/workers/` — `crawler_worker` 负责 feed 拉取与正文回填
- `news_crawler/dtos/` — `ParsedItem` / `PseudoEntry` 两个轻 DTO
- `news_crawler/utils/` — 日志与通用工具
- `news_crawler/categories/*.toml` — 分类与 RSS 源配置
- `ingest.py` / `publish.py` — 由 cron 直接驱动的两个入口

### 2.2 数据模型

单一持久化表 `raw_news`（`news_crawler/core/database.py:27-53`），字段大致包含：

- `id` / `title` / `link (UNIQUE)` / `content_hash` / `content_text`
- `source` / `category` / `created_at`
- `summary` / `ai_tags` / `importance_score` / `is_ai_processed`

没有独立的订阅源表、任务表、快照表、发布记录表、artifact 表或 run_events 表。`is_ai_processed` 是一个布尔位，承担了"整条 item 的阶段"这一含义。

### 2.3 主流程

1. cron 每 6 小时触发 `ingest.py`
   - `run_crawler_job()`：并发抓 feed，`link` / `content_hash` 去重，批量插入
   - `process_new_summaries()`：查询 `is_ai_processed=False`，调 AI，写回同一行
2. cron 每天 09:00 触发 `publish.py`
   - `run_publishing_job()`：按 25 小时时间窗查询，按分类分组，渲染 Markdown，一次性 PyGithub tree commit 推送

### 2.4 外部集成

- `feedparser` — RSS / Atom
- 自写 JSON feed 解析
- `trafilatura` — 正文提取
- `openai` SDK — AI 调用
- `PyGithub` — GitHub tree + commit + push
- `smtplib` — 失败通知邮件
- `SQLAlchemy` — ORM + SQLite / PostgreSQL

### 2.5 观测

- `logging` 模块 + `TimedRotatingFileHandler`
- 没有 metrics、没有结构化 tracing、没有 run_events 表
- 邮件通知仅在出错时发出

## 3. Python → Rust 模块映射

| Python 实体 | 当前承载 | Rust 版目标 crate | 说明 |
|---|---|---|---|
| `core.settings` / `core.bootstrap` | env + `.env` 读取 | `config` | 加入 `schema_version` 校验 |
| `core.category_config_loader` | TOML 加载 | `config` | schema 版本化、加载失败即退出 |
| `core.database` | SQLAlchemy + 单表 | `storage` + `domain` | 单表拆成 6–9 张本体对象表 |
| `core.crawler` / `workers.crawler_worker` | feed 抓取 + trafilatura | `feed` + `extractor` | 拉取与正文提取分离为两个 crate |
| `services.crawler_service` | 抓取编排 | `runtime::ingest` | 改为状态机驱动 |
| `services.ai_service` | AI 调用 + 正则解析 | `ai` + `runtime::ai_run` | 强制 JSON，输出 schema 版本化 |
| `services.report_service` | 日报查询 + Markdown | `report` + `runtime::publish` | 拆成"选稿 + 冻结快照 + 渲染"三步 |
| `services.publisher_service` | PyGithub 直接提交 | `publish` | 通过 `PublisherTarget` trait 抽象 |
| `services.email_service` | 出错邮件 | `observability` 可选 sink | 不进入主干失败路径 |
| `ingest.py` / `publish.py` | cron 入口脚本 | `cli` + `app` | 统一 CLI 单入口 + 子命令 |
| `my-crontab` | docker 内 cron | 外置调度 | Rust 版不自带 cron，依赖宿主调度 |
| `utils.logger` | 文件日志 | `observability` | `tracing` + 结构化事件 + metrics |

## 4. 8 个骨架级痛点与 Rust 对策

### 4.1 单表 `raw_news` 承载多本体对象

**现象**：订阅源、feed 条目、文章正文、AI 结果、发布状态都塞进一张表，用布尔位 `is_ai_processed` 区分阶段。

**后果**：

- 订阅源停用无法表达（只能删行）
- AI 重跑会覆盖前一轮结果，无历史
- 发布状态没有独立记录，重发靠 GitHub commit 时间戳猜
- 无法表达"同一文章被不同 prompt 版本处理过"这一事实

**Rust 对策**：在 `domain` 层定义 6 个本体对象，在 `storage` 层拆成至少 6 张表：`feed_sources` / `feed_entries` / `articles` / `article_ai_results` / `publish_records` / `publish_items`。每张表有独立 PK、外键指向上游对象、独立状态字段。具体见 [storage-schema](./storage-schema.md)。

### 4.2 无 claim + lease 机制

**现象**：`ingest` 与 `publish` 完全依赖"cron 不重叠"的假设，加上 `UNIQUE(link)` 约束防撞。

**后果**：

- 若 `ingest` 跑超时，下一轮 cron 到点后两进程同时操作同一批 feed_entries
- AI 调用是串行 per-article，若 OOM 重启，"已处理/未处理"的边界只能靠 `is_ai_processed` 位，无法表达"正在处理中"
- 多 worker / 多机分布式运行无任何基础

**Rust 对策**：`feed_entries`、`article_ai_results`、`publish_records` 均带 `lease_owner` / `lease_expires_at` / `attempt_count` 三联字段。领取任务使用 `UPDATE ... WHERE state = ? AND (lease_expires_at IS NULL OR lease_expires_at < now) RETURNING ...` 原子抢占。租约到期自动回收。具体 SQL 模板见 [storage-schema §5](./storage-schema.md)。

### 4.3 异常被吞与默默跳过

**现象**（多处典型位置）：

- `services/crawler_service.py:195-201` — `future.result()` 异常只在 warning 级日志吞掉
- `ingest.py:48-55` — 邮件通知失败不影响 ingest 退出码
- `workers/crawler_worker.py:103-142` — 正文提取返回 `None` 时调用方直接跳过
- `core/database.py:111-125` — `_try_create_sessionmaker()` 吞所有异常返回 `None`

**后果**：运行时静默失败，操作员只能靠"日报里少了某个源"这种二次现象才察觉。

**Rust 对策**：

- `domain` 层定义分层错误 enum：`FeedError` / `ExtractError` / `AiError` / `PublishError` / `StorageError`，每种带 `retryable: bool` 与 `kind` 枚举
- 能力层不得吞错误，必须向 `runtime` 返回 `Result`
- `runtime` 负责把失败写入 `run_events` 表和 `feed_entries.last_error` / `articles.last_error` 等字段
- `observability` 把每个错误作为结构化事件推入 `tracing`
- 代码风格检查禁止 `let _ = some_fallible_op();` 这种静默吞错写法（具体三层 enforcement——workspace lints / CI ripgrep / allowlist——见 [error-and-observability §3.3](./error-and-observability.md#33-绝不静默吞掉错误)）

详见 [error-and-observability](./error-and-observability.md)（Phase C 产出）。

### 4.4 hardcoded 时间窗、超时、批量大小

**现象**：

- `services/report_service.py:104` — 日报时间窗硬编码 25 小时
- `services/crawler_service.py:178` — `max_workers=4`、`batch_size=100`、`timeout=300s` 全硬编码
- `services/ai_service.py` — `temperature=0.3` 硬编码

**后果**：任何运行环境调整都必须改代码，违背"配置模型早定"原则。

**Rust 对策**：所有阈值进入 `config::app::*` 或 `config::category::*`，schema 见 [config-schema](./config-schema.md)（Phase C 产出）。硬编码只允许出现在 domain 不变量（如状态机 transition）中。

### 4.5 无 replay 能力

**现象**：feed payload 与 HTML payload 一次性消费完即丢；AI raw response 不保留；`article.content_text` 会被清理脚本清掉。

**后果**：

- 当某篇文章被 AI 误分类时，无法重现当时的 prompt 输入与 raw response
- 当正文提取规则 bug 被修复后，历史文章无法重跑（输入已经不在）
- 线上问题无法脱离外网重现

**Rust 对策**：

- 引入 `raw_artifacts` 表，保留 `feed_payload` / `html_payload` / `ai_raw_response` 三类
- 保留策略可配置：关、仅失败、采样、全保留
- `replay` 作为主 CLI 命令存在，按 artifact key 重入 feed / extractor / ai 对应路径
- 详见 [replay-and-artifacts](./replay-and-artifacts.md)（Phase C 产出）

### 4.6 无发布快照，发布非幂等

**现象**：`run_publishing_job()` 查库 → 渲染 Markdown → `GitHubPublisher.publish_changes()` 一次性推。没有"我刚才发布了什么"这一事实的持久化。

**后果**：

- 重发同一天的日报会得到不同内容（因为新文章可能在两次运行之间入库）
- 如果 GitHub push 失败，本地文件已写，无法精确判断"还需要补推哪些"
- `rebuild-report` 不可能，因为没有快照可重建

**Rust 对策**：

- `publish_records` 记录发布批次，带 `idempotency_key`（`{category}-{report_date}-{render_version}`）且 UNIQUE
- `publish_items` 冻结当次发布引用的 article 与渲染所需字段
- 发布强制"冻结快照 → 渲染 → 本地落盘 → 远程推送"四步，任一步失败都可从快照恢复
- `rebuild-report` 从 `publish_items` 完全重建 Markdown，不依赖活库
- 详见 [state-machine §5](./state-machine.md)

### 4.7 AI 输出用正则解析

**现象**：`services/ai_service.py:169-184` 用 `|SCORE|` / `|TAGS|` 分隔符加正则解析 AI 文本输出。任何模型输出格式漂移都会静默失败并 fallback 到默认分数。

**后果**：

- 换模型时，解析成功率不可预测
- 解析失败与"AI 判定过滤"难以区分
- 无法对输出协议做版本演进

**Rust 对策**：

- `ai` crate 强制要求 JSON 输出（prompt 内明确约束 + 解析层严格模式）
- `article_ai_results` 保存 `output_schema_version` 字段
- 解析失败作为 `AiError::OutputParseFailed { raw }` 显式存在（见 [error-and-observability §2.3](./error-and-observability.md)），不静默 fallback
- `prompt_version` 与 `output_schema_version` 双版本号绑定

### 4.8 观测性只靠 `logging`

**现象**：除了 `TimedRotatingFileHandler` 文件日志 + 控制台，没有 metrics、没有结构化 tracing、没有 run history 表。运维只能靠 grep log。

**后果**：无法回答"上周哪个源抓取成功率最低"、"哪类文章 AI 成功率最低"、"日报连续几天 publish 失败"这类聚合问题。

**Rust 对策**：

- `observability` crate 内建 `tracing` + `metrics` + 结构化事件
- `run_events` 表作为关键事件的持久化镜像
- `doctor` 命令作为主动健康检查入口
- 详见 [error-and-observability](./error-and-observability.md)（Phase C 产出）

## 5. 反推出的 Rust 硬骨架清单

下列条目由上述痛点直接反推，属于"不允许被首版妥协掉"的骨架级决策：

1. **本体对象拆表**：6 张本体表必须首版就位，不接受"先用单表，后续再拆"
2. **claim + lease + attempt_count 三联字段**：每张带任务语义的表都要有，不接受"单 worker 场景不需要"
3. **分层错误 enum + 禁止吞错误**：`domain::error` 首版定义，能力层必须返回 `Result`
4. **所有阈值配置化**：时间窗、超时、批量、并发、retry budget 都进 `app.toml`
5. **raw_artifacts 存在 + 保留策略可配置**：即使默认关闭也必须从 schema 上支持
6. **publish_records + publish_items + idempotency_key 三件套**：发布即事实，必须可回查
7. **AI 输出强制 JSON + output_schema_version**：不接受文本协议 fallback
8. **tracing + metrics + run_events 三位一体**：不允许"先写日志，后加 metrics"
9. **`replay` / `backfill` / `rebuild-report` / `reindex` 是主命令**：不允许作为调试脚本进入 `scripts/`

## 6. 显式不保留的 Python 决策

为了避免 Rust 版误继承 Python 版的局部便利而把自己坑了，下列 Python 做法在 Rust 版是显式放弃的：

- **cron 双入口 `ingest.py` / `publish.py`** — 改为 `cli` 单入口 + 子命令
- **`is_ai_processed` 单布尔位** — 改为多阶段状态机
- **AI 输出 `|SCORE|` / `|TAGS|` 文本协议** — 强制 JSON
- **单表 `raw_news`** — 本体对象拆表
- **PyGithub 直接提交** — 经 `PublisherTarget` trait 层
- **邮件作为主通知通道** — 邮件只作为 observability 的可选 sink，不进入主干失败路径
- **SQLAlchemy ORM** — 改用 `sqlx` 显式 SQL，以便 migration 严格版本化、claim SQL 可直接手写
- **`TimedRotatingFileHandler` 文件日志** — 改用 `tracing` + 结构化事件 + 外部 sink
- **`feedparser` + `trafilatura` 隐式解析** — 用 `feed-rs` + 可替换 `ExtractorStrategy` trait 明确边界

## 7. 结论

Python 版在其自身约束下是能跑的，但它没有达到本仓库工程宪法要求的"长期运行、可恢复、可审计、可回放、可发布、可演化"六项基本属性。Rust 版的骨架级硬约束不是洁癖，而是对 Python 版已经发生过的每一类失效模式的直接反推。

任何未来的 Rust 实现若试图放松这里列出的 9 项硬骨架，都必须先回到本文件对应的痛点条目回答："放松后，Python 版的这个坑我们准备怎么避开？"
